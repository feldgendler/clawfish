//! M6.H — robust on-demand Lichess/CCRL ingestion.
//!
//! A network-layer extension of the M6.G corpus infra: streams a compressed
//! PGN dump over HTTPS, decompresses on the fly, and feeds the *existing*
//! ingest path (`pgn::stream_pgn` → `filter::game_admitted` →
//! `store::append_block` into `pgn-shard.bin`). No `evaluate`/`Position`/search
//! touch → bench byte-identical to `M6.F`/`M6.G` by construction; the gate is
//! functional, not SPRT. Robust for unattended overnight runs: in-attempt
//! HTTP `Range` resume (zero re-decompression), a games-parsed stall watchdog,
//! infinite outer backoff, and seven robustness gates. The synchronous
//! `stream_to_ingest` primitive is what the M6.I bi-level driver calls. See
//! `docs/plans/m6.h.md`, ADR-0036, and `docs/data-catalog.md`.

pub mod catalog;
pub mod gates;
pub mod reader;

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::corpus::filter::{GameFilter, game_admitted};
use crate::corpus::pgn::{GamePositions, PgnStats, stream_pgn};
use crate::corpus::store::{append_block, scan_valid_blocks};
use crate::corpus::{CorpusError, CorpusRecord, DEPTH_RUNG_EXTERNAL, Source};

use catalog::{FetchState, TermKind};
use reader::{
    AttemptFailure, AttemptState, CallState, EofReason, ResumableHttpReader, build_agent,
    http_opener,
};

/// Tunables for a fetch campaign. `Default` carries the roadmap constants.
#[derive(Clone, Debug)]
pub struct FetchConfig {
    /// Gate 1: TCP connect timeout.
    pub connect_timeout: Duration,
    /// `T_stall` — also the ureq `timeout_recv_body` heartbeat (§5B).
    pub stall_timeout: Duration,
    /// Gate 4: redirect cap.
    pub max_redirects: u32,
    /// Outer-backoff initial sleep.
    pub backoff_initial: Duration,
    /// Outer-backoff max sleep (cap).
    pub backoff_max: Duration,
    /// In-attempt escalation bound (consecutive no-progress resumes).
    pub max_noprogress_resumes: u32,
    /// Gate 7 floor (the streaming path needs only this).
    pub disk_floor_bytes: u64,
    /// Gates 5+6 pre-flight buffer size (decompressed bytes).
    pub preflight_bytes: usize,
    /// Gate 6 max parse-failure ratio over the pre-flight prefix.
    pub parse_sanity_max_fail_ratio: f64,
    /// `None` = infinite outer backoff (production); `Some(k)` = bounded (tests,
    /// so the permanent/garbage/escalation paths can't hang CI).
    pub max_attempts: Option<u32>,
}

impl Default for FetchConfig {
    fn default() -> Self {
        FetchConfig {
            connect_timeout: Duration::from_secs(10),
            stall_timeout: Duration::from_secs(75),
            max_redirects: 5,
            backoff_initial: Duration::from_secs(1),
            backoff_max: Duration::from_secs(300),
            max_noprogress_resumes: 3,
            disk_floor_bytes: 2 * 1024 * 1024 * 1024,
            preflight_bytes: 256 * 1024,
            parse_sanity_max_fail_ratio: 0.10,
            max_attempts: None,
        }
    }
}

/// How a `stream_to_ingest` call terminated.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Termination {
    /// Reached `target_positions` (the URL likely has more to give).
    EarlyTarget,
    /// Stream drained to EOF before the target.
    Eos,
    /// SIGINT / operator stop.
    Stopped,
}

/// Result of a `stream_to_ingest` call.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FetchOutcome {
    /// New positions appended this call.
    pub positions_ingested: u64,
    /// New games appended this call.
    pub games_emitted: u64,
    /// Compressed bytes received (last attempt; for the early-term bound).
    pub bytes_received: u64,
    /// How it ended.
    pub terminated: Termination,
    /// One past the highest appended `game_id` (the next free id a follow-on
    /// ingest uses) — accounts for filter gaps; equals `base_game_id` if nothing
    /// was appended.
    pub next_game_id: u64,
}

/// Stream `target_positions` NEW labeled positions from `url` (a `source`-typed
/// CCRL/Lichess dump) into `<out_dir>/pgn-shard.bin`, applying `filter`
/// (game-level admission only — quiet/opening/cap filtering is the later
/// `corpus build`). Blocks under infinite outer backoff (unless
/// `cfg.max_attempts` caps it) until the target is reached / the stream drains
/// / `stop` is set; returns `Err` only on a permanent failure (4xx, disk).
/// `target_positions == 0` means **unbounded** (no early-stop — drain the whole
/// stream), matching the CLI's `u64::MAX` default.
///
/// Idempotence: `base_game_id` is pinned once; in-process byte-0 restarts skip
/// already-appended game_ids (true no-op); cross-process re-ingest is
/// FEN-deduped by `corpus build` (ADR-0035 §8). See `docs/plans/m6.h.md` §5.
pub fn stream_to_ingest(
    source: Source,
    url: &str,
    target_positions: u64,
    out_dir: &Path,
    filter: &GameFilter,
    stop: &Arc<AtomicBool>,
    cfg: &FetchConfig,
) -> Result<FetchOutcome, CorpusError> {
    std::fs::create_dir_all(out_dir)?;
    let shard = out_dir.join("pgn-shard.bin");
    // Pinned ONCE for the whole call (across all byte-0 restarts): a re-parse
    // re-derives the same id per physical game, so the skip-re-seen logic makes
    // an in-process restart a true no-op (§5A).
    let base_game_id = max_existing_game_id(out_dir) + 1;
    let call = CallState::new(target_positions, stop.clone());
    let agent = build_agent(cfg.connect_timeout, cfg.stall_timeout, cfg.max_redirects);

    let mut backoff = cfg.backoff_initial;
    let mut attempt: u32 = 0;
    let mut last_bytes: u64 = 0;

    let terminated = loop {
        if call.stop.load(Ordering::Relaxed) {
            break Termination::Stopped;
        }
        attempt += 1;
        let att = AttemptState::new();
        let ctx = Attempt {
            agent: &agent,
            url,
            shard: &shard,
            base_game_id,
            filter,
            call: &call,
            att: &att,
            cfg,
        };
        let result = run_attempt(source, &ctx);
        last_bytes = att.consumed.get();
        match result {
            AttemptResult::Done(t) => break t,
            AttemptResult::Permanent(msg) => return Err(CorpusError::Pgn(msg)),
            AttemptResult::Retry => {
                if let Some(max) = cfg.max_attempts
                    && attempt >= max
                {
                    return Err(CorpusError::Pgn(format!(
                        "fetch gave up after {attempt} attempts on {url}"
                    )));
                }
                let resume_at = chrono_local_plus(backoff);
                eprintln!(
                    "corpus: fetch attempt {attempt} failed; backing off until {resume_at} local"
                );
                sleep_interruptible(backoff, &call.stop);
                backoff = (backoff * 2).min(cfg.backoff_max);
            }
        }
    };

    let outcome = FetchOutcome {
        positions_ingested: call.positions_ingested.get(),
        games_emitted: call.games_emitted.get(),
        bytes_received: last_bytes,
        terminated,
        // The next free id is one past the HIGHEST appended id, NOT
        // `base + games_emitted` — `stream_pgn` assigns ids to every emitted
        // game (pre-filter), so the appended block ids have gaps wherever a
        // game was filtered out. A follow-on ingest recomputes the same value
        // from `max_existing_game_id`.
        next_game_id: call
            .max_appended_game_id
            .get()
            .map_or(base_game_id, |m| m + 1),
    };
    update_fetch_state(out_dir, url, &outcome);
    Ok(outcome)
}

/// Per-attempt result the outer backoff loop dispatches on.
enum AttemptResult {
    /// Terminal — the call is done with this disposition.
    Done(Termination),
    /// Recoverable — back off and restart from byte 0.
    Retry,
    /// Unrecoverable (4xx / disk) — abort with a message.
    Permanent(String),
}

/// The recurring per-attempt argument bundle (keeps the helpers under clippy's
/// argument-count limit and documents the shared context). `att` is the fresh
/// per-attempt state; the rest are per-call.
struct Attempt<'a> {
    agent: &'a ureq::Agent,
    url: &'a str,
    shard: &'a Path,
    base_game_id: u64,
    filter: &'a GameFilter,
    call: &'a std::rc::Rc<CallState>,
    att: &'a std::rc::Rc<AttemptState>,
    cfg: &'a FetchConfig,
}

/// Decompression codec, selected by the URL extension (not the provenance
/// `Source`, which is just the record tag).
enum Codec {
    /// `.pgn.zst` — streaming zstd over the resumable reader (Lichess).
    Zstd,
    /// `.zip` — download-to-temp + `ZipArchive`.
    Zip,
    /// `.7z` — download-to-temp + `sevenz_rust2::ArchiveReader` (CCRL).
    SevenZ,
}

/// One fetch attempt: open → decompress → (gates 5+6 pre-flight) → stream_pgn →
/// ingest. `.pgn.zst` streams (in-RAM resume); `.zip`/`.7z` download to a temp
/// file then parse the first `.pgn` entry locally (archive metadata needs the
/// whole file; the temp download is itself resumable, research §4).
fn run_attempt(source: Source, ctx: &Attempt) -> AttemptResult {
    // Gate 7: a full disk is operator-fixable, not a transient — don't loop.
    if let Some(dir) = ctx.shard.parent()
        && let Err(e) = gates::disk_precheck(dir, ctx.cfg.disk_floor_bytes)
    {
        return AttemptResult::Permanent(e.to_string());
    }
    if matches!(source, Source::SelfPlayOnBook | Source::SelfPlayOffBook) {
        return AttemptResult::Permanent("self-play sources are not network-fetched".into());
    }
    let url = ctx.url.to_ascii_lowercase();
    let codec = if url.ends_with(".zst") {
        Codec::Zstd
    } else if url.ends_with(".zip") {
        Codec::Zip
    } else if url.ends_with(".7z") {
        Codec::SevenZ
    } else {
        return AttemptResult::Permanent(format!(
            "unsupported URL extension (need .pgn.zst / .zip / .7z): {}",
            ctx.url
        ));
    };
    match codec {
        Codec::Zstd => run_attempt_zstd_stream(source, ctx),
        Codec::Zip => run_attempt_archive(source, ctx, Codec::Zip),
        Codec::SevenZ => run_attempt_archive(source, ctx, Codec::SevenZ),
    }
}

/// `.pgn.zst`: streaming zstd over the resumable reader (in-RAM resume,
/// games-parsed watchdog, counter-driven early termination — no temp file).
fn run_attempt_zstd_stream(source: Source, ctx: &Attempt) -> AttemptResult {
    let mut opener = http_opener(ctx.agent.clone(), ctx.url.to_string());
    let initial = match opener(0) {
        Ok(r) => r,
        Err(reader::ReconnectError::Permanent(m)) => return AttemptResult::Permanent(m),
        Err(_) => return AttemptResult::Retry,
    };
    let reader = ResumableHttpReader::new(
        initial,
        opener,
        ctx.call.clone(),
        ctx.att.clone(),
        ctx.cfg.stall_timeout,
        ctx.cfg.max_noprogress_resumes,
        /* bytes_are_progress */ false,
    );
    let decoder = match zstd::stream::read::Decoder::new(reader) {
        Ok(d) => d,
        Err(_) => return AttemptResult::Retry,
    };
    run_decompressed_pipeline(decoder, source, ctx)
}

/// `.zip`/`.7z`: download the archive to a resumable temp file, then parse its
/// first `.pgn` entry locally. The temp file is unlinked on every exit path.
fn run_attempt_archive(source: Source, ctx: &Attempt, codec: Codec) -> AttemptResult {
    let dir = ctx.shard.parent().unwrap_or_else(|| Path::new("."));
    let ext = match codec {
        Codec::Zip => "zip",
        Codec::SevenZ => "7z",
        Codec::Zstd => unreachable!("zstd is streamed, not downloaded to temp"),
    };
    let tmp = dir.join(format!(".fetch-{}.{ext}.part", std::process::id()));
    let _guard = TmpGuard(&tmp);

    if let Err(r) = download_to_temp(ctx, &tmp) {
        return r;
    }
    match codec {
        Codec::Zip => parse_zip_pgn(&tmp, source, ctx),
        Codec::SevenZ => parse_7z_pgn(&tmp, source, ctx),
        Codec::Zstd => unreachable!(),
    }
}

/// Stream the archive body into `tmp` (resumable, bytes-as-progress). On
/// failure, returns the classified `AttemptResult` to bubble.
fn download_to_temp(ctx: &Attempt, tmp: &Path) -> Result<(), AttemptResult> {
    let mut opener = http_opener(ctx.agent.clone(), ctx.url.to_string());
    let initial = match opener(0) {
        Ok(r) => r,
        Err(reader::ReconnectError::Permanent(m)) => return Err(AttemptResult::Permanent(m)),
        Err(_) => return Err(AttemptResult::Retry),
    };
    let mut reader = ResumableHttpReader::new(
        initial,
        opener,
        ctx.call.clone(),
        ctx.att.clone(),
        ctx.cfg.stall_timeout,
        ctx.cfg.max_noprogress_resumes,
        /* bytes_are_progress */ true,
    );
    let mut f = match File::create(tmp) {
        Ok(f) => f,
        Err(e) => return Err(AttemptResult::Permanent(format!("temp create: {e}"))),
    };
    if std::io::copy(&mut reader, &mut f).is_err() {
        return Err(classify_attempt(ctx.call, ctx.att));
    }
    // The download's own EOF set `eof_reason = InnerEof`, but that is the END OF
    // THE ARCHIVE, not the end of the parse. Clear it so the subsequent
    // local-parse classification (target-reached → EarlyTarget vs drained →
    // Eos) isn't pre-empted by the InnerEof branch in `classify_attempt`.
    ctx.att.eof_reason.set(None);
    Ok(())
}

/// Parse the first `.pgn` entry of a downloaded `.zip` (target-guarded so the
/// parse stops once `target` is reached, not at the entry's EOF).
fn parse_zip_pgn(tmp: &Path, source: Source, ctx: &Attempt) -> AttemptResult {
    let zf = match File::open(tmp) {
        Ok(f) => f,
        Err(_) => return AttemptResult::Retry,
    };
    let mut archive = match zip::ZipArchive::new(zf) {
        Ok(a) => a,
        Err(_) => return AttemptResult::Retry, // corrupt / truncated → re-download
    };
    let pgn_idx = (0..archive.len()).find(|&i| {
        archive
            .by_index(i)
            .map(|e| e.name().to_ascii_lowercase().ends_with(".pgn"))
            .unwrap_or(false)
    });
    let Some(idx) = pgn_idx else {
        return AttemptResult::Permanent("no .pgn entry in zip archive".into());
    };
    let entry = match archive.by_index(idx) {
        Ok(e) => e,
        Err(_) => return AttemptResult::Retry,
    };
    run_decompressed_pipeline(TargetGuard::new(entry, ctx.call.clone()), source, ctx)
}

/// Parse the first `.pgn` entry of a downloaded `.7z` (CCRL). `for_each_entries`
/// yields the entry as a streaming `Read`; the target-guard returns EOF once
/// `target` is reached, so iteration stops without decompressing the rest of a
/// multi-GB archive.
fn parse_7z_pgn(tmp: &Path, source: Source, ctx: &Attempt) -> AttemptResult {
    let mut archive = match sevenz_rust2::ArchiveReader::open(tmp, sevenz_rust2::Password::empty())
    {
        Ok(a) => a,
        Err(_) => return AttemptResult::Retry, // corrupt / truncated → re-download
    };
    let mut result: Option<AttemptResult> = None;
    let walk = archive.for_each_entries(|entry, rd| {
        if entry.name().to_ascii_lowercase().ends_with(".pgn") {
            let guarded = TargetGuard::new(rd, ctx.call.clone());
            result = Some(run_decompressed_pipeline(guarded, source, ctx));
            return Ok(false); // stop after the first .pgn entry
        }
        Ok(true)
    });
    match result {
        Some(r) => r,
        None if walk.is_err() => AttemptResult::Retry,
        None => AttemptResult::Permanent("no .pgn entry in 7z archive".into()),
    }
}

/// A `Read` adapter that returns EOF (`Ok(0)`) once `target` is reached, so a
/// local archive parse stops early instead of decompressing the whole entry.
/// Used only for the `.zip`/`.7z` local-parse paths (the `.zst` streaming path
/// early-terminates inside `ResumableHttpReader` instead).
struct TargetGuard<R: Read> {
    inner: R,
    call: std::rc::Rc<CallState>,
}

impl<R: Read> TargetGuard<R> {
    fn new(inner: R, call: std::rc::Rc<CallState>) -> Self {
        TargetGuard { inner, call }
    }
}

impl<R: Read> Read for TargetGuard<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.call.target_reached() {
            return Ok(0);
        }
        self.inner.read(buf)
    }
}

/// Shared tail: gates 5+6 pre-flight on a decompressed prefix, then the real
/// `stream_pgn` over (prefix ‖ rest) with the ingest closure.
fn run_decompressed_pipeline<R: Read>(
    mut decompressed: R,
    source: Source,
    ctx: &Attempt,
) -> AttemptResult {
    // --- Gates 5+6 pre-flight (nothing appended before the verdict) ---------
    let mut prefix = vec![0u8; ctx.cfg.preflight_bytes];
    let n = match read_up_to(&mut decompressed, &mut prefix) {
        Ok(n) => n,
        Err(_) => return classify_attempt(ctx.call, ctx.att),
    };
    prefix.truncate(n);
    if !gates::sniff_is_pgn(&prefix) {
        return AttemptResult::Retry; // gate 5: not PGN (HTML / garbage)
    }
    let mut pf_stats = PgnStats::default();
    let mut pf_noop = |_gp: GamePositions| {};
    let _ = stream_pgn(
        std::io::Cursor::new(&prefix),
        0,
        &mut pf_noop,
        &mut pf_stats,
    );
    if !gates::parse_sanity_ok(&pf_stats, ctx.cfg.parse_sanity_max_fail_ratio) {
        return AttemptResult::Retry; // gate 6: too many parse failures
    }

    // --- Real run: re-feed the prefix, then the rest of the stream ----------
    let chained = std::io::Cursor::new(prefix).chain(decompressed);
    let mut stats = PgnStats::default();
    {
        let mut on_game = |gp: GamePositions| {
            ingest_game(gp, source, ctx.shard, ctx.filter, ctx.call, ctx.att);
        };
        let _ = stream_pgn(
            BufReader::new(chained),
            ctx.base_game_id,
            &mut on_game,
            &mut stats,
        );
    }
    classify_attempt(ctx.call, ctx.att)
}

/// The ingest closure body: liveness + skip-re-seen + game-filter + append.
fn ingest_game(
    gp: GamePositions,
    source: Source,
    shard: &Path,
    filter: &GameFilter,
    call: &std::rc::Rc<CallState>,
    att: &std::rc::Rc<AttemptState>,
) {
    // Liveness: EVERY yielded game (even re-seen / filtered) proves the stream
    // is healthy and refreshes the watchdog clock.
    att.note_progress();
    // Skip games already appended this call (byte-0-restart idempotence).
    if is_already_appended(gp.game_id, call.max_appended_game_id.get()) {
        return;
    }
    // Stop appending once the target is met (bounds overshoot to ≤ one game).
    if call.target_reached() {
        return;
    }
    if !game_admitted(&gp.tags, filter) {
        return;
    }
    let Some(label) = gp.tags.result else {
        return;
    };
    let recs: Vec<CorpusRecord> = gp
        .positions
        .into_iter()
        .map(|(pos, ply)| CorpusRecord {
            fen: pos.to_fen(),
            label,
            source,
            game_id: gp.game_id,
            ply,
            depth_rung: DEPTH_RUNG_EXTERNAL,
            strata: 0,
        })
        .collect();
    if recs.is_empty() {
        return;
    }
    if append_block(shard, gp.game_id, &recs).is_ok() {
        call.positions_ingested
            .set(call.positions_ingested.get() + recs.len() as u64);
        call.games_emitted.set(call.games_emitted.get() + 1);
        call.max_appended_game_id.set(Some(gp.game_id));
    }
}

/// Classify the attempt's disposition from the shared state (after a stream
/// read/parse returned), per §5(C): stop → Stopped; InnerEof → Eos; target →
/// EarlyTarget; else the recorded failure.
fn classify_attempt(
    call: &std::rc::Rc<CallState>,
    att: &std::rc::Rc<AttemptState>,
) -> AttemptResult {
    if call.stop.load(Ordering::Relaxed) {
        return AttemptResult::Done(Termination::Stopped);
    }
    if att.eof_reason.get() == Some(EofReason::InnerEof) {
        return AttemptResult::Done(Termination::Eos);
    }
    if call.target_reached() {
        return AttemptResult::Done(Termination::EarlyTarget);
    }
    match att.failure.take() {
        Some(AttemptFailure::Permanent(m)) => AttemptResult::Permanent(m),
        Some(_) => AttemptResult::Retry, // Stall / Transient / RangeIgnored
        // No EOF, no target, no failure: the stream simply ended (e.g. CCRL
        // local parse to EOF without the reader's eof_reason). Treat as drained.
        None => AttemptResult::Done(Termination::Eos),
    }
}

/// Read up to `buf.len()` bytes, looping until full or EOF (unlike a single
/// `read`, which may return early). Surfaces the first error.
fn read_up_to<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// Highest existing `game_id` across BOTH shards in `dir` (self-play
/// `shard.bin` + ingest `pgn-shard.bin`); 0 if none. The fetch reserves ids
/// strictly above this.
fn max_existing_game_id(dir: &Path) -> u64 {
    let mut max = 0u64;
    for name in ["shard.bin", "pgn-shard.bin"] {
        let p = dir.join(name);
        if p.exists()
            && let Ok((blocks, _)) = scan_valid_blocks(&p)
        {
            for b in &blocks {
                max = max.max(b.game_id);
            }
        }
    }
    max
}

/// Merge this call's result into `<dir>/fetch-state.json` (best-effort).
fn update_fetch_state(dir: &Path, url: &str, outcome: &FetchOutcome) {
    let path = dir.join("fetch-state.json");
    let mut state = FetchState::load(&path);
    let term = match outcome.terminated {
        Termination::EarlyTarget => TermKind::EarlyTarget,
        Termination::Eos => TermKind::Eos,
        Termination::Stopped => TermKind::Stopped,
    };
    state.record(
        url,
        outcome.positions_ingested,
        outcome.bytes_received,
        term,
    );
    let _ = state.save(&path);
}

/// Sleep `dur`, waking early (every ≤250 ms) to honor a `stop` flip.
fn sleep_interruptible(dur: Duration, stop: &Arc<AtomicBool>) {
    let deadline = Instant::now() + dur;
    while Instant::now() < deadline {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(remaining.min(Duration::from_millis(250)));
    }
}

/// Format `now + dur` as a local wall-clock `HH:MM:SS` (CLAUDE.md: ETAs in
/// absolute local time, not "in N seconds"). Best-effort via `libc::localtime_r`.
fn chrono_local_plus(dur: Duration) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        + dur.as_secs();
    // SAFETY: `localtime_r` writes a valid `tm` into the zeroed out-param from a
    // valid `time_t` pointer; both are stack locals valid for the call.
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// Unlink a temp file on drop (CCRL download cleanup, every exit path).
struct TmpGuard<'a>(&'a Path);
impl Drop for TmpGuard<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0);
    }
}

/// The byte-0-restart idempotence predicate: a re-parsed game whose id was
/// already appended this call is skipped (no re-append, no re-count). Boundary
/// is `<=` (the `max_appended` id itself was appended). `None` ⇒ nothing
/// appended yet ⇒ never skip. See §5(A).
pub(crate) fn is_already_appended(game_id: u64, max_appended: Option<u64>) -> bool {
    matches!(max_appended, Some(m) if game_id <= m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_already_appended_boundary() {
        // Nothing appended yet ⇒ never skip.
        assert!(!is_already_appended(0, None));
        assert!(!is_already_appended(100, None));
        // `<=` boundary: the max id and below are re-seen (skip); above is new.
        assert!(
            is_already_appended(5, Some(5)),
            "the max id itself was appended"
        );
        assert!(is_already_appended(4, Some(5)));
        assert!(!is_already_appended(6, Some(5)), "ids above max are new");
    }
}
