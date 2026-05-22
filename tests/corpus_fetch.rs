//! M6.H integration tests — `corpus::fetch::stream_to_ingest` end-to-end
//! against an in-process localhost HTTP/1.1 server (NO real network).
//!
//! The server speaks just enough HTTP to exercise the fetcher: `Content-Length`
//! and `Accept-Ranges: bytes` headers, `Range: bytes=N-` → `206 Partial
//! Content`, a once-only mid-stream connection drop (resume), `404`, and a
//! garbage body.
//! Payloads are built at test time with the `zstd` encoder (Lichess `.pgn.zst`)
//! and `zip` writer (CCRL `.zip`). The fetcher is pointed at `http://` so no
//! TLS is needed. Tests use a bounded `FetchConfig.max_attempts` + short
//! timeouts so the permanent/garbage/escalation paths can never hang CI.
//!
//! Gated on the `corpus-fetch` feature (the whole file compiles away without
//! it, like the `fetch` module).
#![cfg(feature = "corpus-fetch")]

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use clawfish::corpus::Source;
use clawfish::corpus::fetch::{FetchConfig, Termination, stream_to_ingest};
use clawfish::corpus::filter::GameFilter;
use clawfish::corpus::store::scan_valid_blocks;

// --- test fixtures ----------------------------------------------------------

/// A fixed 22-ply legal line (symmetric fianchetto) — long enough to pass the
/// ADR-0036 min-length gate (≥ 20 plies); the movetext is identical across
/// games (compression resistance comes from the per-game `noise()` Site tag,
/// not the moves). 22 plies ⇒ 23 positions (startpos + 22).
const MOVETEXT_22PLY: &str = "1. Nf3 Nf6 2. g3 g6 3. Bg2 Bg7 4. O-O O-O 5. d3 d6 \
6. c4 c5 7. Nc3 Nc6 8. Rb1 Rb8 9. a3 a6 10. b4 b5 11. cxb5 axb5";

/// One band-filter-passing PGN game (WhiteElo/BlackElo ≥ 2000, Standard TC,
/// Normal termination, ≥ 20 plies). Each game yields `POSITIONS_PER_GAME`
/// positions.
fn one_game(idx: usize) -> String {
    let result = match idx % 3 {
        0 => "1-0",
        1 => "0-1",
        _ => "1/2-1/2",
    };
    // A long (~256-char), per-game-unique, compression-resistant string in the
    // (unfiltered, unparsed-for-content) Site tag. Keeps the compressed payload
    // large enough that zstd's ~128 KiB input buffer can't gulp a small file
    // whole — which is what makes the early-termination `bytes_received` bound
    // meaningful.
    format!(
        "[Event \"Rated game {idx}\"]\n[Site \"{pad}\"]\n[White \"a\"]\n[Black \"b\"]\n\
         [Result \"{result}\"]\n[WhiteElo \"2400\"]\n[BlackElo \"2410\"]\n\
         [TimeControl \"600+5\"]\n[Termination \"Normal\"]\n\n{MOVETEXT_22PLY} {result}\n\n",
        pad = noise(idx),
    )
}

/// 256 chars of per-`idx` pseudo-random hex (a tiny xorshift expansion) —
/// deterministic, but distinct enough across games to resist zstd compression.
fn noise(idx: usize) -> String {
    let mut s = String::with_capacity(256);
    let mut x = (idx as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(1);
    for _ in 0..16 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.push_str(&format!("{x:016x}"));
    }
    s
}

fn make_pgn(n_games: usize) -> String {
    (0..n_games).map(one_game).collect()
}

const POSITIONS_PER_GAME: u64 = 23; // startpos + 22 plies (MOVETEXT_22PLY)

fn zstd_encode(bytes: &[u8]) -> Vec<u8> {
    zstd::stream::encode_all(bytes, 3).expect("zstd encode")
}

fn zip_encode_pgn(pgn: &[u8]) -> Vec<u8> {
    use std::io::Cursor;
    use zip::write::SimpleFileOptions;
    let mut buf = Vec::new();
    {
        let mut zw = zip::ZipWriter::new(Cursor::new(&mut buf));
        let opts =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zw.start_file("games.pgn", opts).unwrap();
        zw.write_all(pgn).unwrap();
        zw.finish().unwrap();
    }
    buf
}

/// Build a `.7z` (LZMA2) containing a single `games.pgn` entry — the CCRL
/// archive shape. `sevenz_rust2` compresses from a path, so the PGN is written
/// to a scratch file first.
fn sevenz_encode_pgn(pgn: &[u8], tag: &str) -> Vec<u8> {
    let dir = tmp_dir(&format!("7zsrc-{tag}"));
    let pgn_path = dir.join("games.pgn");
    std::fs::write(&pgn_path, pgn).unwrap();
    let z_path = dir.join("a.7z");
    sevenz_rust2::compress_to_path(&pgn_path, &z_path).expect("7z compress");
    let bytes = std::fs::read(&z_path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    bytes
}

// --- localhost HTTP/1.1 test server -----------------------------------------

#[derive(Clone, Copy)]
enum Behavior {
    /// Serve the payload (200, or 206 on Range).
    Serve,
    /// On the first full-body request, send only `n` body bytes then close;
    /// honor the subsequent Range resume (206) → in-attempt resume.
    DropOnce(usize),
    /// On the first request, send only `n` body bytes then close; on EVERY
    /// subsequent request ignore `Range` and answer `200` from byte 0. This
    /// makes the in-attempt resume see a `200` (→ `RangeIgnored`) and forces a
    /// byte-0 outer restart — the path that exercises the skip-re-seen-id
    /// idempotence logic.
    DropOnceIgnoreRange(usize),
    /// On the first request, send only `n` body bytes then close; on the resume
    /// (a `Range` request) answer `416 Range Not Satisfiable` (as if the offset
    /// is past EOF). Exercises `http_opener`'s 416 → clean-EOS path.
    DropThen416(usize),
    /// Always answer with this status (e.g. 404).
    Status(u16),
}

struct TestServer {
    addr: String,
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl TestServer {
    fn spawn(payload: Vec<u8>, behavior: Behavior) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(false).unwrap();
        let addr = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let shutdown = Arc::new(AtomicBool::new(false));
        // The once-only-drop latch lives entirely in the server thread.
        let dp = Arc::new(AtomicBool::new(false));
        let sd = shutdown.clone();
        let payload = Arc::new(payload);
        listener.set_nonblocking(true).unwrap();
        let handle = std::thread::spawn(move || {
            while !sd.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = handle_conn(stream, &payload, behavior, &dp);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        TestServer {
            addr,
            shutdown,
            handle: Some(handle),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Parse the request, extract a `Range: bytes=N-` start (if any), respond.
fn handle_conn(
    stream: TcpStream,
    payload: &[u8],
    behavior: Behavior,
    dropped: &AtomicBool,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut range_start: u64 = 0;
    let mut line = String::new();
    // Request line.
    reader.read_line(&mut line)?;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 {
            break;
        }
        let t = h.trim_end();
        if t.is_empty() {
            break;
        }
        if let Some(v) = t.to_ascii_lowercase().strip_prefix("range:") {
            // "range: bytes=N-"
            if let Some(eq) = v.find("bytes=") {
                let rest = &v[eq + 6..];
                let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                range_start = num.parse().unwrap_or(0);
            }
        }
    }

    let mut out = stream;
    if let Behavior::Status(code) = behavior {
        let body = b"not found";
        let resp = format!(
            "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        out.write_all(resp.as_bytes())?;
        out.write_all(body)?;
        return Ok(());
    }

    let total = payload.len() as u64;

    // DropThen416: first request sends `n` bytes then closes; the resume
    // (Range present) gets a 416 (as if past EOF) → http_opener's clean-EOS arm.
    if let Behavior::DropThen416(n) = behavior {
        let first = !dropped.swap(true, Ordering::Relaxed);
        if first {
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
            );
            out.write_all(resp.as_bytes())?;
            let n = (n as u64).min(total) as usize;
            out.write_all(&payload[..n])?;
            let _ = out.flush();
            return Ok(()); // truncated close
        }
        let resp =
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        out.write_all(resp.as_bytes())?;
        return Ok(());
    }

    // DropOnceIgnoreRange: always serve 200 from byte 0 (ignore Range), but on
    // the very first request truncate to `n` bytes.
    if let Behavior::DropOnceIgnoreRange(n) = behavior {
        let first = !dropped.swap(true, Ordering::Relaxed);
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
        );
        out.write_all(resp.as_bytes())?;
        if first {
            let n = (n as u64).min(total) as usize;
            out.write_all(&payload[..n])?;
            let _ = out.flush();
            return Ok(()); // truncated close → client resume sees a 200
        }
        out.write_all(payload)?;
        return Ok(());
    }

    let start = range_start.min(total);
    let slice = &payload[start as usize..];
    if start > 0 {
        // 206 Partial Content.
        let resp = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\
             Content-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
            slice.len(),
            start,
            total - 1,
            total
        );
        out.write_all(resp.as_bytes())?;
        out.write_all(slice)?;
        return Ok(());
    }

    // Fresh (start == 0): 200 OK.
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
        total
    );
    out.write_all(resp.as_bytes())?;

    if let Behavior::DropOnce(n) = behavior
        && !dropped.swap(true, Ordering::Relaxed)
    {
        // Send only the first `n` bytes, then close the connection abruptly so
        // the client must Range-resume.
        let n = (n as u64).min(total) as usize;
        out.write_all(&payload[..n])?;
        let _ = out.flush();
        return Ok(()); // drop closes the socket mid-body
    }
    out.write_all(payload)?;
    Ok(())
}

// --- helpers ----------------------------------------------------------------

fn tmp_dir(tag: &str) -> PathBuf {
    static CTR: AtomicUsize = AtomicUsize::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("clawfish-fetch-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn shard_record_count(dir: &Path) -> u64 {
    let shard = dir.join("pgn-shard.bin");
    if !shard.exists() {
        return 0;
    }
    let (blocks, _) = scan_valid_blocks(&shard).expect("scan");
    blocks.iter().map(|b| b.records.len() as u64).sum()
}

fn shard_game_ids(dir: &Path) -> Vec<u64> {
    let shard = dir.join("pgn-shard.bin");
    let (blocks, _) = scan_valid_blocks(&shard).expect("scan");
    blocks.iter().map(|b| b.game_id).collect()
}

fn test_cfg() -> FetchConfig {
    FetchConfig {
        connect_timeout: Duration::from_secs(5),
        // Generous: the integration tests don't exercise stalling (that's
        // unit-tested), and a tight window false-positives under the heavy CPU
        // contention of the parallel test run.
        stall_timeout: Duration::from_secs(60),
        backoff_initial: Duration::from_millis(10),
        backoff_max: Duration::from_millis(50),
        max_noprogress_resumes: 3,
        disk_floor_bytes: 1, // don't reject on the dev machine
        preflight_bytes: 64 * 1024,
        parse_sanity_max_fail_ratio: 0.10,
        max_attempts: Some(8),
        ..FetchConfig::default()
    }
}

fn no_stop() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

// --- tests ------------------------------------------------------------------

#[test]
fn fetch_zst_early_termination_bounds_bytes() {
    // Large, compression-resistant payload so zstd's ~128 KiB input buffer is a
    // small fraction of the file (otherwise the decoder could gulp the whole
    // small file and the byte bound would be vacuous).
    let n_games = 6000usize; // 42000 positions
    let pgn = make_pgn(n_games);
    let payload = zstd_encode(pgn.as_bytes());
    let compressed_len = payload.len() as u64;
    assert!(
        compressed_len > 512 * 1024,
        "payload must dwarf the decoder buffer: {compressed_len}"
    );
    let srv = TestServer::spawn(payload, Behavior::Serve);
    let dir = tmp_dir("early");

    let target = 700u64; // ~1.7% of 42000 positions
    let out = stream_to_ingest(
        Source::LichessOpen,
        &srv.url("/dump.pgn.zst"),
        target,
        &dir,
        &GameFilter::default(),
        &no_stop(),
        &test_cfg(),
    )
    .expect("fetch ok");

    assert_eq!(out.terminated, Termination::EarlyTarget);
    assert!(
        out.positions_ingested >= target,
        "reached target: {}",
        out.positions_ingested
    );
    // Overshoot at most one game.
    assert!(out.positions_ingested < target + POSITIONS_PER_GAME);
    // Shard is valid + holds the ingested positions.
    assert_eq!(shard_record_count(&dir), out.positions_ingested);
    // The load-bearing early-termination assertion: the CLIENT consumed far
    // fewer compressed bytes than the whole file (client-side `bytes_received`,
    // immune to server-side socket buffering). Tight (< 1/3) because the target
    // is ~1.7% and the file dwarfs the decoder buffer.
    assert!(
        out.bytes_received < compressed_len / 3,
        "early termination must bound the download: read {} of {} compressed bytes",
        out.bytes_received,
        compressed_len
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_resumes_after_injected_connection_drop_zst() {
    let n_games = 800usize;
    let pgn = make_pgn(n_games);
    let payload = zstd_encode(pgn.as_bytes());

    // Reference: no drop, target huge ⇒ full ingest.
    let ref_dir = tmp_dir("ref");
    {
        let srv = TestServer::spawn(payload.clone(), Behavior::Serve);
        let out = stream_to_ingest(
            Source::LichessOpen,
            &srv.url("/d.pgn.zst"),
            u64::MAX,
            &ref_dir,
            &GameFilter::default(),
            &no_stop(),
            &test_cfg(),
        )
        .expect("ref fetch");
        assert_eq!(out.terminated, Termination::Eos);
    }
    let ref_count = shard_record_count(&ref_dir);
    assert!(ref_count > 0);

    // Drop once at the midpoint of the compressed stream.
    let drop_dir = tmp_dir("drop");
    let srv = TestServer::spawn(payload.clone(), Behavior::DropOnce(payload.len() / 2));
    let out = stream_to_ingest(
        Source::LichessOpen,
        &srv.url("/d.pgn.zst"),
        u64::MAX,
        &drop_dir,
        &GameFilter::default(),
        &no_stop(),
        &test_cfg(),
    )
    .expect("drop fetch");
    assert_eq!(out.terminated, Termination::Eos);
    assert_eq!(
        shard_record_count(&drop_dir),
        ref_count,
        "resume must recover the full stream"
    );

    let _ = std::fs::remove_dir_all(&ref_dir);
    let _ = std::fs::remove_dir_all(&drop_dir);
}

#[test]
fn fetch_eos_when_target_exceeds_file() {
    let pgn = make_pgn(50);
    let payload = zstd_encode(pgn.as_bytes());
    let srv = TestServer::spawn(payload, Behavior::Serve);
    let dir = tmp_dir("eos");
    let out = stream_to_ingest(
        Source::LichessOpen,
        &srv.url("/d.pgn.zst"),
        u64::MAX,
        &dir,
        &GameFilter::default(),
        &no_stop(),
        &test_cfg(),
    )
    .expect("fetch");
    assert_eq!(out.terminated, Termination::Eos);
    assert_eq!(out.positions_ingested, 50 * POSITIONS_PER_GAME);
    assert_eq!(shard_record_count(&dir), 50 * POSITIONS_PER_GAME);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_eos_at_target_reports_eos_not_early() {
    let pgn = make_pgn(50);
    let payload = zstd_encode(pgn.as_bytes());
    let srv = TestServer::spawn(payload, Behavior::Serve);
    let dir = tmp_dir("eos-tie");
    // target == exactly the file's admitted-position count.
    let target = 50 * POSITIONS_PER_GAME;
    let out = stream_to_ingest(
        Source::LichessOpen,
        &srv.url("/d.pgn.zst"),
        target,
        &dir,
        &GameFilter::default(),
        &no_stop(),
        &test_cfg(),
    )
    .expect("fetch");
    assert_eq!(
        out.terminated,
        Termination::Eos,
        "drained-at-target is Eos, not EarlyTarget"
    );
    // Confirm the tie actually occurred: the target WAS met (not an Eos for the
    // unrelated reason of the file being smaller than the target).
    assert_eq!(out.positions_ingested, target, "target met exactly at EOS");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_416_on_resume_is_clean_eos() {
    // A 416 on a resume (offset past EOF) is treated as a clean end-of-stream by
    // http_opener (→ empty reader → InnerEof), not a retryable error. Here the
    // server truncates then 416s the resume; the fetch must terminate `Eos`
    // (with the pre-drop games ingested), not hang or error-retry.
    let pgn = make_pgn(400);
    let payload = zstd_encode(pgn.as_bytes());
    let srv = TestServer::spawn(payload.clone(), Behavior::DropThen416(payload.len() / 2));
    let dir = tmp_dir("e416");
    let mut cfg = test_cfg();
    cfg.max_attempts = Some(2); // bound just in case the arm misbehaves
    let out = stream_to_ingest(
        Source::LichessOpen,
        &srv.url("/d.pgn.zst"),
        u64::MAX,
        &dir,
        &GameFilter::default(),
        &no_stop(),
        &cfg,
    )
    .expect("416-as-EOS terminates cleanly");
    assert_eq!(
        out.terminated,
        Termination::Eos,
        "416 on resume ⇒ clean EOS"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_permanent_404_returns_err_no_infinite_loop() {
    let srv = TestServer::spawn(Vec::new(), Behavior::Status(404));
    let dir = tmp_dir("404");
    let mut cfg = test_cfg();
    cfg.max_attempts = Some(1);
    let r = stream_to_ingest(
        Source::LichessOpen,
        &srv.url("/missing.pgn.zst"),
        100,
        &dir,
        &GameFilter::default(),
        &no_stop(),
        &cfg,
    );
    assert!(r.is_err(), "permanent 404 ⇒ Err, not a hang");
    assert_eq!(shard_record_count(&dir), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_rejects_html_body_no_ingest() {
    // A zst-wrapped HTML page (soft-404 / CDN interstitial): decompresses fine
    // but gates 5/6 reject it. Nothing must be ingested.
    let html = "<!DOCTYPE html><html><head><title>Not Found</title></head><body>\
                <h1>404</h1><p>[Event lookalike] but actually html</p></body></html>";
    let payload = zstd_encode(html.as_bytes());
    let srv = TestServer::spawn(payload, Behavior::Serve);
    let dir = tmp_dir("html");
    let mut cfg = test_cfg();
    cfg.max_attempts = Some(2);
    let r = stream_to_ingest(
        Source::LichessOpen,
        &srv.url("/d.pgn.zst"),
        100,
        &dir,
        &GameFilter::default(),
        &no_stop(),
        &cfg,
    );
    // Either Err (exhausted attempts) — the key invariant is zero ingest.
    assert!(r.is_err() || matches!(r, Ok(o) if o.positions_ingested == 0));
    assert_eq!(shard_record_count(&dir), 0, "no garbage committed");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_idempotent_byte0_restart_no_duplicate() {
    // DropOnceIgnoreRange forces a genuine BYTE-0 OUTER RESTART (the resume sees
    // a 200 → RangeIgnored → fresh decoder from byte 0), which is the only path
    // that exercises the skip-re-seen-game_id idempotence logic. Games appended
    // before the drop must NOT be re-appended on the restart. A large payload
    // (resisting compression) ensures real ingest appends several games before
    // the drop point, so the skip path is actually traversed.
    let n_games = 4000usize;
    let pgn = make_pgn(n_games);
    let payload = zstd_encode(pgn.as_bytes());
    // Drop ~40% of the way in — well past the pre-flight, so games are already
    // appended when the byte-0 restart fires.
    let srv = TestServer::spawn(
        payload.clone(),
        Behavior::DropOnceIgnoreRange(payload.len() * 2 / 5),
    );
    let dir = tmp_dir("idem");
    let mut cfg = test_cfg();
    // Small-ish pre-flight so the real ingest starts well before the ~40% drop
    // point (games append → the byte-0 restart exercises skip-re-seen), but
    // large enough that the prefix's one truncated last game stays under the
    // gate-6 10% parse-failure threshold (~30 games at the ~520-byte fixture).
    cfg.preflight_bytes = 16 * 1024;
    let out = stream_to_ingest(
        Source::LichessOpen,
        &srv.url("/d.pgn.zst"),
        u64::MAX,
        &dir,
        &GameFilter::default(),
        &no_stop(),
        &cfg,
    )
    .expect("fetch");
    assert_eq!(out.terminated, Termination::Eos);
    // No duplicate game_ids despite the byte-0 restart re-parsing the prefix.
    let mut ids = shard_game_ids(&dir);
    let len_before = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(
        ids.len(),
        len_before,
        "no duplicate game_ids after a byte-0 restart"
    );
    // Exactly the file's positions — no double-count of the re-parsed prefix.
    assert_eq!(
        shard_record_count(&dir),
        n_games as u64 * POSITIONS_PER_GAME
    );
    assert_eq!(out.positions_ingested, n_games as u64 * POSITIONS_PER_GAME);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_ccrl_zip_temp_file_parity() {
    let pgn = make_pgn(100);
    let payload = zip_encode_pgn(pgn.as_bytes());
    let srv = TestServer::spawn(payload, Behavior::Serve);
    let dir = tmp_dir("zip");
    let out = stream_to_ingest(
        Source::Ccrl,
        &srv.url("/snapshot.zip"),
        u64::MAX,
        &dir,
        &GameFilter::default(),
        &no_stop(),
        &test_cfg(),
    )
    .expect("zip fetch");
    assert_eq!(out.terminated, Termination::Eos);
    assert_eq!(shard_record_count(&dir), 100 * POSITIONS_PER_GAME);
    // Every record carries the right provenance, and game_ids are the expected
    // contiguous range — the `ingest-pgn`-parity invariant (not just a count).
    let (blocks, _) = scan_valid_blocks(&dir.join("pgn-shard.bin")).unwrap();
    assert!(
        blocks
            .iter()
            .all(|b| b.records.iter().all(|r| r.source == Source::Ccrl))
    );
    let ids: Vec<u64> = blocks.iter().map(|b| b.game_id).collect();
    assert!(
        ids.windows(2).all(|w| w[0] < w[1]),
        "game_ids strictly increasing"
    );
    // The temp download file must be cleaned up. The only legitimate residents
    // are the shard and the per-URL state file `stream_to_ingest` writes.
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "pgn-shard.bin" && n != "fetch-state.json")
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp/extra files not cleaned: {leftovers:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_ccrl_zip_early_termination() {
    // Directly anchors the .zip early-term classification (the shared
    // `eof_reason` reset + TargetGuard): a target far below the archive's games
    // must report EarlyTarget, not Eos, with overshoot ≤ one game.
    let pgn = make_pgn(500);
    let payload = zip_encode_pgn(pgn.as_bytes());
    let srv = TestServer::spawn(payload, Behavior::Serve);
    let dir = tmp_dir("zipe");
    let target = 70u64;
    let out = stream_to_ingest(
        Source::Ccrl,
        &srv.url("/snapshot.zip"),
        target,
        &dir,
        &GameFilter::default(),
        &no_stop(),
        &test_cfg(),
    )
    .expect("zip fetch");
    assert_eq!(out.terminated, Termination::EarlyTarget);
    assert!(out.positions_ingested >= target);
    assert!(out.positions_ingested < target + POSITIONS_PER_GAME);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_ccrl_7z_parity() {
    // CCRL's real format: a single-entry .7z (LZMA2). The fetcher must download
    // it, open it with sevenz_rust2, and parse the .pgn entry — parity with the
    // zip/zst paths.
    let pgn = make_pgn(100);
    let payload = sevenz_encode_pgn(pgn.as_bytes(), "parity");
    let srv = TestServer::spawn(payload, Behavior::Serve);
    let dir = tmp_dir("7z");
    let out = stream_to_ingest(
        Source::Ccrl,
        &srv.url("/CCRL-4040.[100].pgn.7z"),
        u64::MAX,
        &dir,
        &GameFilter::default(),
        &no_stop(),
        &test_cfg(),
    )
    .expect("7z fetch");
    assert_eq!(out.terminated, Termination::Eos);
    assert_eq!(shard_record_count(&dir), 100 * POSITIONS_PER_GAME);
    let (blocks, _) = scan_valid_blocks(&dir.join("pgn-shard.bin")).unwrap();
    assert!(
        blocks
            .iter()
            .all(|b| b.records.iter().all(|r| r.source == Source::Ccrl))
    );
    // Temp .7z cleaned up.
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "pgn-shard.bin" && n != "fetch-state.json")
        .collect();
    assert!(leftovers.is_empty(), "temp .7z not cleaned: {leftovers:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_ccrl_7z_early_termination() {
    // The TargetGuard must stop the local .7z parse once the target is met
    // (so a multi-GB CCRL archive isn't fully decompressed for a small slice).
    let pgn = make_pgn(500);
    let payload = sevenz_encode_pgn(pgn.as_bytes(), "early");
    let srv = TestServer::spawn(payload, Behavior::Serve);
    let dir = tmp_dir("7ze");
    let target = 70u64; // ~10 games of 3500 positions
    let out = stream_to_ingest(
        Source::Ccrl,
        &srv.url("/CCRL-4040.[500].pgn.7z"),
        target,
        &dir,
        &GameFilter::default(),
        &no_stop(),
        &test_cfg(),
    )
    .expect("7z fetch");
    assert_eq!(out.terminated, Termination::EarlyTarget);
    assert!(out.positions_ingested >= target);
    assert!(out.positions_ingested < target + POSITIONS_PER_GAME);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_ccrl_zip_resumes_after_drop() {
    let pgn = make_pgn(150);
    let payload = zip_encode_pgn(pgn.as_bytes());

    let ref_dir = tmp_dir("zipref");
    {
        let srv = TestServer::spawn(payload.clone(), Behavior::Serve);
        stream_to_ingest(
            Source::Ccrl,
            &srv.url("/s.zip"),
            u64::MAX,
            &ref_dir,
            &GameFilter::default(),
            &no_stop(),
            &test_cfg(),
        )
        .expect("ref");
    }
    let ref_count = shard_record_count(&ref_dir);

    let drop_dir = tmp_dir("zipdrop");
    let srv = TestServer::spawn(payload.clone(), Behavior::DropOnce(payload.len() / 2));
    stream_to_ingest(
        Source::Ccrl,
        &srv.url("/s.zip"),
        u64::MAX,
        &drop_dir,
        &GameFilter::default(),
        &no_stop(),
        &test_cfg(),
    )
    .expect("drop");
    assert_eq!(
        shard_record_count(&drop_dir),
        ref_count,
        "zip temp-download resume recovers all"
    );

    let _ = std::fs::remove_dir_all(&ref_dir);
    let _ = std::fs::remove_dir_all(&drop_dir);
}

#[test]
fn fetch_stop_flag_clean_exit() {
    let pgn = make_pgn(500);
    let payload = zstd_encode(pgn.as_bytes());
    let srv = TestServer::spawn(payload, Behavior::Serve);
    let dir = tmp_dir("stop");
    // Covers stop-set-on-entry. Mid-stream async stop (stop flipped by another
    // thread partway through) is honored by the same `stop` check in the reader
    // loop but is not deterministically triggerable here without a timing race,
    // so it is intentionally not unit-tested.
    let stop = Arc::new(AtomicBool::new(true)); // already stopped
    let out = stream_to_ingest(
        Source::LichessOpen,
        &srv.url("/d.pgn.zst"),
        u64::MAX,
        &dir,
        &GameFilter::default(),
        &stop,
        &test_cfg(),
    )
    .expect("fetch");
    assert_eq!(out.terminated, Termination::Stopped);
    let _ = std::fs::remove_dir_all(&dir);
}
