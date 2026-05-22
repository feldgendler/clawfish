//! M6.H — robust on-demand Lichess/CCRL ingestion.
//!
//! A network-layer extension of the M6.G corpus infra: streams a compressed
//! PGN dump over HTTPS, decompresses on the fly, runs the full per-position
//! inline pipeline (skip8 ∧ `!in_check` ∧ `|static_eval| ≤ HIGH_SCORE_CP` ∧
//! `is_quiet`) over each game's positions, and routes the surviving
//! quiet-certified records through the shared [`LaneCommitter`] (per-lane FEN
//! dedup → per-game cap → exact target truncation) into `lane.bin`. No
//! `evaluate`/search BEHAVIOR change — the qsearch seam (`QSearcher::eval_white`)
//! and `evaluate` are existing read-only calls — so bench stays byte-identical;
//! the gate is functional, not SPRT. Robust for unattended overnight runs:
//! in-attempt HTTP `Range` resume (zero re-decompression), a games-parsed stall
//! watchdog, infinite outer backoff, and seven robustness gates. The
//! synchronous `stream_to_ingest` primitive is what the M6.I bi-level driver
//! calls. See `docs/plans/m6.h2-corpus-lanes.md` §1.4, `docs/plans/m6.h.md`,
//! ADR-0036, and `docs/data-catalog.md`.

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
use crate::corpus::objective::strata_for;
use crate::corpus::pgn::{GamePositions, PgnStats, stream_pgn};
use crate::corpus::pipeline::LaneCommitter;
use crate::corpus::quiet::{is_quiet, static_eval_white};
use crate::corpus::store::scan_valid_blocks;
use crate::corpus::{
    CorpusError, CorpusRecord, DEPTH_RUNG_EXTERNAL, HIGH_SCORE_CP, OPENING_SKIP_PLIES, Source,
};
use crate::movegen::in_check;
use crate::search::QSearcher;

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
    /// Per-game reservoir-cap seed for the [`LaneCommitter`] (Knuth/Vitter
    /// Algorithm R, per-game sub-stream `substream_seed(cap_seed, game_id)`).
    /// Pinned in the manifest so the lane's per-game cap survivor set is
    /// deterministic for a fresh uninterrupted fetch (the cross-process resume
    /// carve-out is content/label-stable; see plan §1.6).
    pub cap_seed: u64,
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
            cap_seed: 0,
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
    /// New USABLE positions committed this call (post quiet-filter → dedup →
    /// cap → exact target; the committer's `usable_committed` summed — NOT raw
    /// parsed positions).
    pub positions_ingested: u64,
    /// New games that committed ≥ 1 record this call.
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

/// Stream `target_positions` NEW usable positions from `url` (a `source`-typed
/// CCRL/Lichess dump) into `<out_dir>/lane.bin`, applying `filter` (game-level
/// admission) AND the full per-position inline pipeline (skip8 ∧ `!in_check` ∧
/// `|static_eval| ≤ HIGH_SCORE_CP` ∧ `is_quiet`) before routing the surviving
/// quiet-certified records through the shared [`LaneCommitter`] (per-lane FEN
/// dedup → per-game cap → exact target truncation). `target_positions` counts
/// **usable** (post-pipeline) positions; the committer's exact truncation lands
/// the lane on the target exactly. Blocks under infinite outer backoff (unless
/// `cfg.max_attempts` caps it) until the target is reached / the stream drains
/// / `stop` is set; returns `Err` only on a permanent failure (4xx, disk).
/// `target_positions == 0` means **unbounded** (no early-stop — drain the whole
/// stream), matching the CLI's `u64::MAX` default.
///
/// Idempotence (plan §1.4): the `LaneCommitter` is created ONCE per call (via
/// `resume`, which gives the committed-game-id set + the dedup `fen_set`), and
/// `base_game_id = max_existing_game_id + 1` is pinned once. In-process byte-0
/// restarts skip already-appended game_ids via `is_already_appended` BEFORE the
/// committer (no double-insert into `fen_set`, no double-count); a cross-process
/// resume rebuilds `fen_set` from `lane.bin` so a re-ingested early game's FENs
/// are dropped as dups. See `docs/plans/m6.h2-corpus-lanes.md` §1.4.
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
    let lane = out_dir.join("lane.bin");
    // Pinned ONCE for the whole call (across all byte-0 restarts): a re-parse
    // re-derives the same id per physical game, so the skip-re-seen logic makes
    // an in-process restart a true no-op (§5A).
    let base_game_id = max_existing_game_id(out_dir) + 1;
    let call = CallState::new(target_positions, stop.clone());
    let agent = build_agent(cfg.connect_timeout, cfg.stall_timeout, cfg.max_redirects);

    // The LaneCommitter is created ONCE per call (NOT per attempt), so it
    // persists across byte-0 restarts: its `fen_set` + `committed` count
    // survive a RangeIgnored-forced restart so the re-parsed prefix dedups
    // against what is already on disk + in-memory. `resume` does the single
    // scan of lane.bin (rebuilding fen_set + the committed count). `target=0`
    // (unbounded) maps to `None`.
    let target = (target_positions != 0).then_some(target_positions);
    let (mut committer, _committed_ids) = LaneCommitter::resume(&lane, cfg.cap_seed, target)?;
    // One QSearcher per call (fetch is single-threaded; reused across games —
    // lives in the Attempt bundle as &mut, NOT in the Rc-shared CallState).
    let mut qsearcher = QSearcher::new();

    let mut backoff = cfg.backoff_initial;
    let mut attempt: u32 = 0;
    let mut last_bytes: u64 = 0;

    let terminated = loop {
        if call.stop.load(Ordering::Relaxed) {
            break Termination::Stopped;
        }
        attempt += 1;
        let att = AttemptState::new();
        let mut ctx = Attempt {
            agent: &agent,
            url,
            lane: &lane,
            source,
            base_game_id,
            filter,
            call: &call,
            att: &att,
            cfg,
            committer: &mut committer,
            qsearcher: &mut qsearcher,
        };
        let result = run_attempt(&mut ctx);
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
/// per-attempt state; the rest are per-call. `committer` and `qsearcher` are
/// borrowed `&mut` from the per-call state (NOT in the `Rc`-shared `CallState`,
/// which would force a `RefCell`); the ingest path is single-threaded and
/// `&mut`/`FnMut`, so this composes (plan §1.4 placement note).
struct Attempt<'a> {
    agent: &'a ureq::Agent,
    url: &'a str,
    lane: &'a Path,
    source: Source,
    base_game_id: u64,
    filter: &'a GameFilter,
    call: &'a std::rc::Rc<CallState>,
    att: &'a std::rc::Rc<AttemptState>,
    cfg: &'a FetchConfig,
    committer: &'a mut LaneCommitter,
    qsearcher: &'a mut QSearcher,
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
/// inline pipeline → commit. `.pgn.zst` streams (in-RAM resume); `.zip`/`.7z`
/// download to a temp file then parse the first `.pgn` entry locally (archive
/// metadata needs the whole file; the temp download is itself resumable,
/// research §4).
fn run_attempt(ctx: &mut Attempt) -> AttemptResult {
    // Gate 7: a full disk is operator-fixable, not a transient — don't loop.
    if let Some(dir) = ctx.lane.parent()
        && let Err(e) = gates::disk_precheck(dir, ctx.cfg.disk_floor_bytes)
    {
        return AttemptResult::Permanent(e.to_string());
    }
    if matches!(ctx.source, Source::SelfPlayOnBook | Source::SelfPlayOffBook) {
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
        Codec::Zstd => run_attempt_zstd_stream(ctx),
        Codec::Zip => run_attempt_archive(ctx, Codec::Zip),
        Codec::SevenZ => run_attempt_archive(ctx, Codec::SevenZ),
    }
}

/// `.pgn.zst`: streaming zstd over the resumable reader (in-RAM resume,
/// games-parsed watchdog, counter-driven early termination — no temp file).
fn run_attempt_zstd_stream(ctx: &mut Attempt) -> AttemptResult {
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
    run_decompressed_pipeline(decoder, ctx)
}

/// `.zip`/`.7z`: download the archive to a resumable temp file, then parse its
/// first `.pgn` entry locally. The temp file is unlinked on every exit path.
fn run_attempt_archive(ctx: &mut Attempt, codec: Codec) -> AttemptResult {
    let dir = ctx
        .lane
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| Path::new(".").to_path_buf());
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
        Codec::Zip => parse_zip_pgn(&tmp, ctx),
        Codec::SevenZ => parse_7z_pgn(&tmp, ctx),
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
fn parse_zip_pgn(tmp: &Path, ctx: &mut Attempt) -> AttemptResult {
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
    let guarded = TargetGuard::new(entry, ctx.call.clone());
    run_decompressed_pipeline(guarded, ctx)
}

/// Parse the first `.pgn` entry of a downloaded `.7z` (CCRL). `for_each_entries`
/// yields the entry as a streaming `Read`; the target-guard returns EOF once
/// `target` is reached, so iteration stops without decompressing the rest of a
/// multi-GB archive.
fn parse_7z_pgn(tmp: &Path, ctx: &mut Attempt) -> AttemptResult {
    let mut archive = match sevenz_rust2::ArchiveReader::open(tmp, sevenz_rust2::Password::empty())
    {
        Ok(a) => a,
        Err(_) => return AttemptResult::Retry, // corrupt / truncated → re-download
    };
    let mut result: Option<AttemptResult> = None;
    let call = ctx.call.clone();
    let walk = archive.for_each_entries(|entry, rd| {
        if entry.name().to_ascii_lowercase().ends_with(".pgn") {
            let guarded = TargetGuard::new(rd, call.clone());
            result = Some(run_decompressed_pipeline(guarded, ctx));
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
        // A fatal commit-abort (disk full / I/O append error) or a reached
        // target both halt the local-archive parse; the abort is surfaced by
        // `classify_attempt` as a permanent error.
        if self.call.commit_aborted() || self.call.target_reached() {
            return Ok(0);
        }
        self.inner.read(buf)
    }
}

/// Shared tail: gates 5+6 pre-flight on a decompressed prefix, then the real
/// `stream_pgn` over (prefix ‖ rest) with the inline-pipeline ingest closure.
fn run_decompressed_pipeline<R: Read>(mut decompressed: R, ctx: &mut Attempt) -> AttemptResult {
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
        // Reborrow the per-call fields out of `ctx` so the `FnMut` closure
        // borrows only what it needs (not the whole `&mut Attempt`).
        let lane = ctx.lane;
        let source = ctx.source;
        let filter = ctx.filter;
        let call = ctx.call;
        let att = ctx.att;
        let committer = &mut *ctx.committer;
        let qsearcher = &mut *ctx.qsearcher;
        let base_game_id = ctx.base_game_id;
        let mut on_game = |gp: GamePositions| {
            ingest_game(gp, source, lane, filter, call, att, committer, qsearcher);
        };
        let _ = stream_pgn(
            BufReader::new(chained),
            base_game_id,
            &mut on_game,
            &mut stats,
        );
    }
    classify_attempt(ctx.call, ctx.att)
}

/// The ingest closure body: liveness + skip-re-seen + game-filter + per-position
/// inline pipeline (skip8 ∧ `!in_check` ∧ `|static_eval| ≤ HIGH_SCORE_CP` ∧
/// `is_quiet`) → `LaneCommitter::commit_game` (dedup → cap → exact target).
///
/// `positions_ingested` is bumped by the committer's `usable_committed`, NOT
/// the raw quiet-certified `recs.len()` — the lane lands on `target` exactly
/// (plan §1.4).
#[allow(clippy::too_many_arguments)] // per-call refs reborrowed out of Attempt
fn ingest_game(
    gp: GamePositions,
    source: Source,
    lane: &Path,
    filter: &GameFilter,
    call: &std::rc::Rc<CallState>,
    att: &std::rc::Rc<AttemptState>,
    committer: &mut LaneCommitter,
    qsearcher: &mut QSearcher,
) {
    // Liveness: EVERY yielded game (even re-seen / filtered) proves the stream
    // is healthy and refreshes the watchdog clock.
    att.note_progress();
    // Skip games already appended this call (byte-0-restart idempotence). This
    // MUST run before the committer so a re-parsed early game never re-enters
    // the dedup set or the committed count (§1.4).
    if is_already_appended(gp.game_id, call.max_appended_game_id.get()) {
        return;
    }
    // Stop committing once the usable target is met.
    if committer.target_reached() {
        return;
    }
    // Game-level admission (Termination / non-standard-start / length / TC /
    // Elo). `positions.len()` = mainline plies + 1.
    if !game_admitted(&gp.tags, gp.positions.len(), filter) {
        return;
    }
    let Some(label) = gp.tags.result else {
        return;
    };
    // Per-position inline pipeline (the build-ready certificate every lane
    // shares): ply ≥ OPENING_SKIP_PLIES ∧ !in_check ∧ |static_eval| ≤
    // HIGH_SCORE_CP ∧ is_quiet. The committer then handles dedup/cap/target.
    let recs: Vec<CorpusRecord> = gp
        .positions
        .into_iter()
        .filter_map(|(pos, ply)| {
            if ply < OPENING_SKIP_PLIES || in_check(&pos) {
                return None;
            }
            let se = static_eval_white(&pos);
            if se.abs() > HIGH_SCORE_CP {
                return None;
            }
            let qs = qsearcher.eval_white(&pos);
            if !is_quiet(&pos, se, qs) {
                return None;
            }
            Some(CorpusRecord {
                fen: pos.to_fen(),
                label,
                source,
                game_id: gp.game_id,
                ply,
                depth_rung: DEPTH_RUNG_EXTERNAL,
                strata: strata_for(&pos),
            })
        })
        .collect();
    if recs.is_empty() {
        return;
    }
    // The committer applies per-lane dedup → per-game cap → exact target
    // truncation and appends one CRC block to lane.bin. An empty-post-dedup
    // game writes no block and leaves `max_appended_game_id` unchanged (so a
    // byte-0 restart re-runs it; it stays empty because its FENs are dups).
    //
    // A `commit_game` Err is a fatal append failure (disk full / I/O error),
    // NOT an empty game: record it as a commit-abort so the parse halts and
    // `stream_to_ingest` returns an error instead of silently under-counting.
    let outcome = match committer.commit_game(lane, gp.game_id, recs) {
        Ok(outcome) => outcome,
        Err(e) => {
            call.set_commit_abort(format!("commit_game failed for game {}: {e}", gp.game_id));
            return;
        }
    };
    if outcome.usable_committed > 0 {
        call.positions_ingested
            .set(call.positions_ingested.get() + outcome.usable_committed);
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
    // A fatal commit-game failure aborts the whole fetch loudly (no infinite
    // backoff): checked BEFORE `stop` so a disk-full append error surfaces as
    // an error rather than being masked as a clean operator stop.
    if let Some(msg) = call.commit_abort.borrow_mut().take() {
        return AttemptResult::Permanent(msg);
    }
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

/// Highest existing `game_id` in this lane's `lane.bin`; 0 if none (M6.H2: each
/// lane lives in its own dir, so its `game_id` space is private — only `lane.bin`
/// is scanned). The fetch reserves ids strictly above this.
fn max_existing_game_id(dir: &Path) -> u64 {
    let mut max = 0u64;
    let p = dir.join("lane.bin");
    if p.exists()
        && let Ok((blocks, _)) = scan_valid_blocks(&p)
    {
        for b in &blocks {
            max = max.max(b.game_id);
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
    fn commit_abort_classifies_as_permanent_before_stop() {
        // A fatal commit_game failure must surface as a Permanent abort, even
        // when `stop` is also set — the abort is checked first so a disk-full
        // append error is never masked as a clean operator stop.
        let stop = Arc::new(AtomicBool::new(true));
        let call = CallState::new(0, stop);
        let att = AttemptState::new();
        call.set_commit_abort("commit_game failed for game 7: disk full".into());
        // First-wins: a second abort does not overwrite the message.
        call.set_commit_abort("later error".into());
        assert!(call.commit_aborted());

        match classify_attempt(&call, &att) {
            AttemptResult::Permanent(msg) => {
                assert!(msg.contains("disk full"), "first abort wins: {msg}");
            }
            AttemptResult::Done(_) => panic!("commit-abort must not classify as Done"),
            AttemptResult::Retry => panic!("commit-abort must not classify as Retry"),
        }
    }

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
