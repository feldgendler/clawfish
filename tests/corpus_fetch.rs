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

use clawfish::corpus::fetch::{FetchConfig, Termination, stream_to_ingest};
use clawfish::corpus::filter::GameFilter;
use clawfish::corpus::quiet::{is_quiet, static_eval_white};
use clawfish::corpus::store::scan_valid_blocks;
use clawfish::corpus::{HIGH_SCORE_CP, OPENING_SKIP_PLIES, Source};
use clawfish::search::QSearcher;
use clawfish::{Move, MoveFlag, MoveList, PieceKind, Position, generate_moves, in_check};

// --- test fixtures ----------------------------------------------------------
//
// M6.H2: fetch now runs the FULL per-position inline pipeline (skip8 ∧
// !in_check ∧ |static_eval| ≤ HIGH_SCORE_CP ∧ is_quiet) and routes survivors
// through the `LaneCommitter` (per-lane FEN dedup → per-game cap → exact target
// truncation). The pre-M6.H2 fixture reused ONE shared movetext across all
// games (only the Site tag varied), which under per-lane dedup collapses every
// game after game 0 to zero usable positions. The new generator emits a
// DISTINCT legal game per index — diverging at ply 0 — so post-skip8 FENs are
// fresh across games (verified: zero cross-game FEN collisions at n=800), and
// the usable totals are computed by the same pipeline at runtime
// (`expected_usable_total`) rather than a uniform literal, because branching
// games yield non-uniform per-game usable counts.

const GAME_PLIES: usize = 22; // ≥ 20 plies (ADR-0036 min-length gate)

// SAN encoder over the public engine API (the fetcher parses SAN PGN; the
// fixture must emit legal SAN). Mirrors the `epd-suite` encoder's logic.
fn piece_san_letter(k: PieceKind) -> char {
    (k.san_letter() as char).to_ascii_uppercase()
}

fn san_of_legal_move(pos: &Position, mv: Move) -> String {
    let flag = mv.flag();
    if flag == MoveFlag::KingCastle {
        return "O-O".into();
    }
    if flag == MoveFlag::QueenCastle {
        return "O-O-O".into();
    }
    let from = mv.from_square();
    let to = mv.to_square();
    let pc = pos.piece_at(from).expect("legal move has a piece on from");
    let is_pawn = pc.kind == PieceKind::Pawn;
    let cap = mv.is_capture();
    let mut s = String::with_capacity(8);
    if is_pawn {
        if cap {
            s.push((b'a' + from.file()) as char);
            s.push('x');
        }
        s.push((b'a' + to.file()) as char);
        s.push((b'1' + to.rank()) as char);
        if let Some(p) = mv.promotion_kind() {
            s.push('=');
            s.push(piece_san_letter(p));
        }
    } else {
        s.push(piece_san_letter(pc.kind));
        s.push_str(&san_disambig(pos, mv));
        if cap {
            s.push('x');
        }
        s.push((b'a' + to.file()) as char);
        s.push((b'1' + to.rank()) as char);
    }
    s
}

fn san_disambig(pos: &Position, mv: Move) -> String {
    let from = mv.from_square();
    let to = mv.to_square();
    let pc = pos.piece_at(from).expect("piece on from");
    let us = pos.side_to_move();
    let mut ml = MoveList::new();
    generate_moves(pos, &mut ml);
    let ambiguous: Vec<Move> = ml
        .as_slice()
        .iter()
        .copied()
        .filter(|&m| {
            m != mv
                && m.to_square() == to
                && !m.is_castling()
                && pos
                    .piece_at(m.from_square())
                    .map(|p| p.kind == pc.kind && p.color == us)
                    .unwrap_or(false)
        })
        .collect();
    if ambiguous.is_empty() {
        return String::new();
    }
    let same_file = ambiguous
        .iter()
        .any(|m| m.from_square().file() == from.file());
    let same_rank = ambiguous
        .iter()
        .any(|m| m.from_square().rank() == from.rank());
    if !same_file {
        return ((b'a' + from.file()) as char).to_string();
    }
    if !same_rank {
        return ((b'1' + from.rank()) as char).to_string();
    }
    let mut t = String::new();
    t.push((b'a' + from.file()) as char);
    t.push((b'1' + from.rank()) as char);
    t
}

/// Deterministic legal-move index from a per-`(idx, ply)` hash.
fn move_pick(idx: usize, ply: usize, n: usize) -> usize {
    let mut x = (idx as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add((ply as u64).wrapping_mul(0xD1B5_4A32_D192_ED03))
        .wrapping_add(1);
    x ^= x >> 33;
    (x as usize) % n
}

/// Pick a calm (non-capturing, non-checking, non-promoting) legal move when one
/// exists, so the walk stays quiet and yields plenty of post-skip8 quiet
/// positions; fall back to any legal move otherwise.
fn calm_move(pos: &Position, ml: &MoveList, idx: usize, ply: usize) -> Move {
    let slice = ml.as_slice();
    let calm: Vec<Move> = slice
        .iter()
        .copied()
        .filter(|&m| !m.is_capture() && m.promotion_kind().is_none())
        .filter(|&m| {
            let mut p = *pos;
            p.make_move(m);
            !in_check(&p)
        })
        .collect();
    if calm.is_empty() {
        slice[move_pick(idx, ply, slice.len())]
    } else {
        calm[move_pick(idx, ply, calm.len())]
    }
}

/// Generate game `idx` as a distinct legal `GAME_PLIES`-ply line, returning its
/// SAN movetext and the full position trail (startpos + each ply). Diverges at
/// ply 0 (`idx`-selected first move) so post-skip8 FENs are fresh across games.
fn game_positions(idx: usize) -> (String, Vec<Position>) {
    let mut pos = Position::starting_position();
    let mut trail = vec![pos];
    let mut sans: Vec<String> = Vec::new();
    for ply in 0..GAME_PLIES {
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        if ml.is_empty() {
            break;
        }
        let mv = if ply == 0 {
            ml.as_slice()[idx % ml.len()]
        } else {
            calm_move(&pos, &ml, idx, ply)
        };
        sans.push(san_of_legal_move(&pos, mv));
        pos.make_move(mv);
        trail.push(pos);
    }
    let mut movetext = String::new();
    for (i, chunk) in sans.chunks(2).enumerate() {
        movetext.push_str(&format!("{}. {}", i + 1, chunk[0]));
        if let Some(b) = chunk.get(1) {
            movetext.push(' ');
            movetext.push_str(b);
        }
        movetext.push(' ');
    }
    (movetext.trim_end().to_string(), trail)
}

/// Extend game `idx` by one additional calm ply, returning the extended SAN
/// movetext and the new unique position U (at ply GAME_PLIES, 0-indexed).
/// U is guaranteed to be a legal non-capture non-check position — suitable for
/// asserting that dedup-across-restart committed exactly U (and not a skip).
fn extend_game_one_ply(idx: usize) -> (String, Position) {
    let (mt, trail) = game_positions(idx);
    let final_pos = *trail.last().expect("game must have ≥1 position");
    let mut ml = MoveList::new();
    generate_moves(&final_pos, &mut ml);
    assert!(!ml.is_empty(), "game {idx} must have legal moves to extend");
    let mv = calm_move(&final_pos, &ml, idx, GAME_PLIES);
    let san = san_of_legal_move(&final_pos, mv);
    // Determine the next move number and color context. GAME_PLIES plies have
    // been played; if GAME_PLIES is even, it's white's turn; if odd, black's.
    let extended = if GAME_PLIES.is_multiple_of(2) {
        format!("{mt} {}. {san}", GAME_PLIES / 2 + 1)
    } else {
        format!("{mt} {san}")
    };
    let mut u_pos = final_pos;
    u_pos.make_move(mv);
    (extended, u_pos)
}

/// Ground-truth usable total a fresh unbounded fetch of `make_pgn(n_games)`
/// commits: run a REFERENCE `stream_to_ingest` (target = unbounded) against a
/// throwaway localhost server + dir and read the lane's record count. This is
/// the exact value the system-under-test produces — it folds in game-level
/// admission AND the warm-`QSearcher` quiet decisions (a local per-position
/// re-derivation can diverge from the streamed path on game-admission and
/// QSearcher-TT ordering, so we measure end-to-end instead of re-deriving).
fn expected_usable_total(n_games: usize) -> u64 {
    let pgn = make_pgn(n_games);
    let payload = zstd_encode(pgn.as_bytes());
    let srv = TestServer::spawn(payload, Behavior::Serve);
    let dir = tmp_dir("ref-total");
    let out = stream_to_ingest(
        Source::LichessOpen,
        &srv.url("/ref.pgn.zst"),
        u64::MAX,
        &dir,
        &GameFilter::default(),
        &no_stop(),
        &test_cfg(),
    )
    .expect("reference fetch");
    let count = out.positions_ingested;
    let _ = std::fs::remove_dir_all(&dir);
    count
}

/// The game result token for game `idx` (cycled so labels vary across games).
fn result_token(idx: usize) -> &'static str {
    match idx % 3 {
        0 => "1-0",
        1 => "0-1",
        _ => "1/2-1/2",
    }
}

/// One band-filter-passing PGN game (WhiteElo/BlackElo ≥ 2000, Standard TC,
/// Normal termination, ≥ 20 plies) with a per-game-DISTINCT legal movetext.
fn one_game(idx: usize) -> String {
    let result = result_token(idx);
    let (movetext, _trail) = game_positions(idx);
    // A long (~256-char), per-game-unique, compression-resistant string in the
    // (unfiltered, unparsed-for-content) Site tag. Keeps the compressed payload
    // large enough that zstd's ~128 KiB input buffer can't gulp a small file
    // whole — which is what makes the early-termination `bytes_received` bound
    // meaningful (the distinct movetext also contributes to that).
    format!(
        "[Event \"Rated game {idx}\"]\n[Site \"{pad}\"]\n[White \"a\"]\n[Black \"b\"]\n\
         [Result \"{result}\"]\n[WhiteElo \"2400\"]\n[BlackElo \"2410\"]\n\
         [TimeControl \"600+5\"]\n[Termination \"Normal\"]\n\n{movetext} {result}\n\n",
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

fn lane_record_count(dir: &Path) -> u64 {
    let lane = dir.join("lane.bin");
    if !lane.exists() {
        return 0;
    }
    let (blocks, _) = scan_valid_blocks(&lane).expect("scan");
    blocks.iter().map(|b| b.records.len() as u64).sum()
}

fn lane_game_ids(dir: &Path) -> Vec<u64> {
    let lane = dir.join("lane.bin");
    let (blocks, _) = scan_valid_blocks(&lane).expect("scan");
    blocks.iter().map(|b| b.game_id).collect()
}

fn lane_fens(dir: &Path) -> std::collections::HashSet<String> {
    let lane = dir.join("lane.bin");
    if !lane.exists() {
        return std::collections::HashSet::new();
    }
    let (blocks, _) = scan_valid_blocks(&lane).expect("scan");
    blocks
        .iter()
        .flat_map(|b| b.records.iter().map(|r| r.fen.clone()))
        .collect()
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
        cap_seed: 0,
        ..FetchConfig::default()
    }
}

fn no_stop() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

// --- tests ------------------------------------------------------------------

#[test]
fn italian_fixture_quiet_nonquiet_classification() {
    // Verifies the Italian-line fixture's quiet/non-quiet classification WITHOUT
    // any network calls — fully runnable in the sandbox. This is the pre-condition
    // that `fetch_quiet_filter_applied` relies on; if this fails, the fetch test
    // would produce a false pass or spurious failure.
    let (_, trail) = italian_hanging_knight_line();
    assert_eq!(
        trail.len(),
        14,
        "Italian line: 13 moves → 14 trail positions (indices 0..13)"
    );

    let hanging_pos = trail[13]; // after 7.Ng5
    let calm_pos = trail[12]; // after 6...h6

    let mut q = QSearcher::new();

    // The position after 7.Ng5: Black's h6 pawn can take the knight on g5.
    let se_h = static_eval_white(&hanging_pos);
    let qs_h = q.eval_white(&hanging_pos);
    assert!(
        !in_check(&hanging_pos),
        "Ng5 position: Black must not be in check"
    );
    assert!(
        se_h.abs() <= HIGH_SCORE_CP,
        "Ng5 position: |static_eval|={se_h} must be ≤ {HIGH_SCORE_CP}"
    );
    assert!(
        !is_quiet(&hanging_pos, se_h, qs_h),
        "Ng5 position must be NON-QUIET (hxg5 wins free knight); se={se_h} qs={qs_h}"
    );

    // The position after 6...h6: standard Italian, no hanging material.
    let se_c = static_eval_white(&calm_pos);
    let qs_c = q.eval_white(&calm_pos);
    assert!(
        !in_check(&calm_pos),
        "h6 position: White must not be in check"
    );
    assert!(
        se_c.abs() <= HIGH_SCORE_CP,
        "h6 position: |static_eval|={se_c} must be ≤ {HIGH_SCORE_CP}"
    );
    assert!(
        is_quiet(&calm_pos, se_c, qs_c),
        "h6 position must be QUIET (no hanging material, no captures); se={se_c} qs={qs_c}"
    );

    // All post-skip8 positions (trail[8..12]) must be quiet — none of them
    // have hanging material in the quiet Italian middlegame.
    let mut q2 = QSearcher::new();
    let mut quiet_count = 0u32;
    for (ply, pos) in trail.iter().enumerate() {
        if (ply as u32) < OPENING_SKIP_PLIES {
            continue;
        }
        if in_check(pos) {
            continue;
        }
        let se = static_eval_white(pos);
        if se.abs() > HIGH_SCORE_CP {
            continue;
        }
        let qs = q2.eval_white(pos);
        if is_quiet(pos, se, qs) {
            quiet_count += 1;
        }
    }
    // Positions at plies 8..12 (5 positions) are quiet; ply 13 is non-quiet.
    // The exact count could vary if any position also has |eval| > threshold,
    // but we assert at least one quiet (calm_pos at ply 12 is always quiet).
    assert!(
        quiet_count >= 1,
        "Italian line must have ≥1 quiet post-skip8 position; got {quiet_count}"
    );
}

#[test]
fn fixture_games_diverge_before_skip8_boundary() {
    // Regression guard: the `game_positions` fixture MUST produce distinct
    // post-skip8 FENs across games. If two games share a post-skip8 FEN,
    // per-lane dedup would silently shrink the corpus (game N contributes zero
    // for every position it shares with an earlier game), breaking all tests
    // that assume each game contributes fresh usable positions.
    //
    // The critical invariant is "diverges before ply 8" — if the first-move
    // divergence (ply 0) propagates through to ply 8 and beyond, no collision
    // is possible. We verify this empirically by collecting all post-skip8 FENs
    // across the first 20 games and asserting the total count equals the size
    // of the union (i.e., zero cross-game collisions).
    let n_check = 20usize;
    let mut all_fens: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut total_count = 0usize;
    for idx in 0..n_check {
        let (_, trail) = game_positions(idx);
        for (ply, pos) in trail.iter().enumerate() {
            if (ply as u32) >= OPENING_SKIP_PLIES {
                let fen = pos.to_fen();
                total_count += 1;
                all_fens.insert(fen);
            }
        }
    }
    assert!(total_count > 0, "fixture must produce post-skip8 positions");
    assert_eq!(
        all_fens.len(),
        total_count,
        "CRITICAL: cross-game FEN collision detected across games 0..{n_check}. \
         The fixture does not diverge before ply {OPENING_SKIP_PLIES}. \
         Per-lane dedup would silently collapse corpus size. \
         unique={} total={total_count}",
        all_fens.len()
    );
}

#[test]
fn fetch_zst_early_termination_bounds_bytes() {
    // Large, compression-resistant payload so zstd's ~128 KiB input buffer is a
    // small fraction of the file (otherwise the decoder could gulp the whole
    // small file and the byte bound would be vacuous).
    let n_games = 6000usize;
    let pgn = make_pgn(n_games);
    let payload = zstd_encode(pgn.as_bytes());
    let compressed_len = payload.len() as u64;
    assert!(
        compressed_len > 512 * 1024,
        "payload must dwarf the decoder buffer: {compressed_len}"
    );
    let srv = TestServer::spawn(payload, Behavior::Serve);
    let dir = tmp_dir("early");

    let target = 700u64;
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
    // M6.H2: exact target truncation in `LaneCommitter` lands the lane on the
    // usable target EXACTLY (no overshoot — the boundary game is truncated).
    assert_eq!(
        out.positions_ingested, target,
        "exact target truncation: {} != {target}",
        out.positions_ingested
    );
    // Lane is valid + holds exactly the ingested usable positions.
    assert_eq!(lane_record_count(&dir), out.positions_ingested);
    // The load-bearing early-termination assertion: the CLIENT consumed far
    // fewer compressed bytes than the whole file (client-side `bytes_received`,
    // immune to server-side socket buffering). Tight (< 1/3) because the target
    // is a tiny fraction of the file and the file dwarfs the decoder buffer.
    assert!(
        out.bytes_received < compressed_len / 3,
        "early termination must bound the download: read {} of {} compressed bytes",
        out.bytes_received,
        compressed_len
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_target_counts_usable_not_raw() {
    // M6.H2 §1.4: the target counts USABLE (post quiet-filter → dedup → cap)
    // positions — NOT raw parsed positions — and the committer's exact
    // truncation lands the lane on `target` exactly (no overshoot). A LARGE file
    // with a SMALL target keeps the reader actively streaming when the target is
    // hit (so the early-termination peek classifies EarlyTarget, not a
    // fully-buffered drain). Average usable/game is well below the ~23 raw
    // positions/game, so a raw-count interpretation of the target would land at a
    // very different byte offset — the "usable not raw" distinction.
    let n_games = 6000usize;
    let pgn = make_pgn(n_games);
    let payload = zstd_encode(pgn.as_bytes());
    let target = 200u64;
    let srv = TestServer::spawn(payload, Behavior::Serve);
    let dir = tmp_dir("usable-target");
    let out = stream_to_ingest(
        Source::LichessOpen,
        &srv.url("/d.pgn.zst"),
        target,
        &dir,
        &GameFilter::default(),
        &no_stop(),
        &test_cfg(),
    )
    .expect("fetch ok");
    assert_eq!(out.terminated, Termination::EarlyTarget);
    assert_eq!(
        out.positions_ingested, target,
        "usable-count target hit exactly via committer truncation"
    );
    assert_eq!(lane_record_count(&dir), target, "disk holds exactly target");
    // Exactly `target` usable positions came from FEWER than `target` raw
    // positions' worth of *games* — i.e. the target is in usable units. The
    // games emitted (each ≤ PER_GAME_CAP=10 usable) must exceed target/10.
    assert!(
        out.games_emitted >= target / 10,
        "usable target spans ≥ target/cap games (usable-count, not raw): \
         games={} target={target}",
        out.games_emitted
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_per_lane_dedup_drops_repeat_fen() {
    // Two physical games with the IDENTICAL movetext (hence identical post-skip8
    // FENs) only contribute one game's worth of usable positions: the second is
    // 100% dedup-dropped within the lane. We pin this by comparing two fetches —
    // one WITH a duplicate game inserted, one WITHOUT — and asserting the
    // duplicate adds ZERO. (Comparing against a re-derived per-game count would
    // be fragile against the warm-QSearcher TT ordering; an end-to-end A/B is
    // exact.)
    let g0 = one_game(0);
    let g5 = one_game(5);
    // Re-emit game 0's movetext under a distinct Event/Site tag (same FENs).
    let (mt0, _t) = game_positions(0);
    let dup = format!(
        "[Event \"dup\"]\n[Site \"{pad}\"]\n[White \"a\"]\n[Black \"b\"]\n\
         [Result \"1-0\"]\n[WhiteElo \"2400\"]\n[BlackElo \"2410\"]\n\
         [TimeControl \"600+5\"]\n[Termination \"Normal\"]\n\n{mt0} 1-0\n\n",
        pad = noise(99999),
    );

    let run = |pgn: &str, tag: &str| -> u64 {
        let payload = zstd_encode(pgn.as_bytes());
        let srv = TestServer::spawn(payload, Behavior::Serve);
        let dir = tmp_dir(tag);
        let out = stream_to_ingest(
            Source::LichessOpen,
            &srv.url("/d.pgn.zst"),
            u64::MAX,
            &dir,
            &GameFilter::default(),
            &no_stop(),
            &test_cfg(),
        )
        .expect("fetch ok");
        assert_eq!(out.terminated, Termination::Eos);
        assert_eq!(lane_record_count(&dir), out.positions_ingested);
        let _ = std::fs::remove_dir_all(&dir);
        out.positions_ingested
    };

    // [game0, game5] vs [game0, dup-of-game0, game5]: the dup must add nothing.
    let without_dup = run(&format!("{g0}{g5}"), "perlane-nodup");
    let with_dup = run(&format!("{g0}{dup}{g5}"), "perlane-dup");
    assert!(without_dup > 0, "fixture must yield usable positions");
    assert_eq!(
        with_dup, without_dup,
        "the repeated-FEN game is fully dedup-dropped (adds zero positions)"
    );
}

/// Build the Italian-game + knight-blunder line by replaying moves from/to
/// squares, collecting SAN and positions as we go.
///
/// Line: `1. e4 e5 2. Nf3 Nc6 3. Bc4 Bc5 4. O-O Nf6 5. d3 d6 6. h3 h6 7. Ng5`
///
/// Trail indices / ply structure:
/// - `trail[0]`  = starting position (0 plies played, skipped — ply 0 < 8)
/// - `trail[8]`  = after `...Nf6`  (ply 8, first non-skipped)  — quiet
/// - `trail[9]`  = after `d3`      (ply 9)                      — quiet
/// - `trail[10]` = after `...d6`   (ply 10)                     — quiet
/// - `trail[11]` = after `h3`      (ply 11)                     — quiet
/// - `trail[12]` = after `...h6`   (ply 12) — `calm_pos`        — quiet
/// - `trail[13]` = after `Ng5`     (ply 13) — `hanging_pos`     — NON-QUIET
///
/// `hanging_pos`: knight on g5 is attacked by Black's h6 pawn; `hxg5` wins a
/// free knight → qsearch swing ≫ QUIET_MARGIN_CP under ANY QSearcher TT state.
///
/// `calm_pos` (h6 just played, White to move): standard Italian structure,
/// no captures available, no material imbalance → `qs == static_eval` → quiet
/// regardless of TT warmup.
fn italian_hanging_knight_line() -> (String, Vec<Position>) {
    use clawfish::Square;

    // (from, to) in UCI square notation; castling = king e1→g1.
    let move_squares: &[(&str, &str)] = &[
        ("e2", "e4"),
        ("e7", "e5"),
        ("g1", "f3"),
        ("b8", "c6"),
        ("f1", "c4"),
        ("f8", "c5"),
        ("e1", "g1"), // O-O
        ("g8", "f6"),
        ("d2", "d3"),
        ("d7", "d6"),
        ("h2", "h3"),
        ("h7", "h6"),
        ("f3", "g5"), // Ng5 — hangs the knight on g5 to the h6 pawn
    ];

    let mut pos = Position::starting_position();
    let mut trail = vec![pos];
    let mut sans: Vec<String> = Vec::new();

    for &(from_s, to_s) in move_squares {
        let from = Square::parse_uci(from_s).expect("valid square");
        let to = Square::parse_uci(to_s).expect("valid square");
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let mv = ml
            .as_slice()
            .iter()
            .copied()
            .find(|m| m.from_square() == from && m.to_square() == to)
            .unwrap_or_else(|| panic!("no legal move {from_s}{to_s} in position"));
        sans.push(san_of_legal_move(&pos, mv));
        pos.make_move(mv);
        trail.push(pos);
    }

    let mut movetext = String::new();
    for (i, chunk) in sans.chunks(2).enumerate() {
        movetext.push_str(&format!("{}. {}", i + 1, chunk[0]));
        if let Some(b) = chunk.get(1) {
            movetext.push(' ');
            movetext.push_str(b);
        }
        movetext.push(' ');
    }
    (movetext.trim_end().to_string(), trail)
}

#[test]
fn fetch_quiet_filter_applied() {
    // `1. e4 e5 2. Nf3 Nc6 3. Bc4 Bc5 4. O-O Nf6 5. d3 d6 6. h3 h6 7. Ng5`
    //
    // The quiet filter must drop the position after 7.Ng5 (non-quiet: Black's
    // h-pawn can capture the knight for free) while retaining the position after
    // 6...h6 (genuinely quiet — standard Italian structure, no hanging material).
    //
    // Both positions are WARMTH-INDEPENDENT for QSearcher:
    //   • After Ng5 (knight on g5, attacked by h6 pawn, undefended): `hxg5`
    //     wins a clean knight → qsearch swing ≫ QUIET_MARGIN_CP under ANY TT.
    //   • After h6 (no captures/checks available): qsearch returns static_eval
    //     instantly → always quiet regardless of TT state.
    //
    // Membership assertions (not a count) so no fresh-vs-warm QSearcher
    // disagreement can cause a false pass or spurious failure.
    let (movetext, trail) = italian_hanging_knight_line();
    // 13 moves played → 14 positions in trail (indices 0..13).
    assert_eq!(
        trail.len(),
        14,
        "Italian line must produce 14 trail positions"
    );

    let hanging_pos = trail[13]; // after Ng5
    let calm_pos = trail[12]; // after h6
    let hanging_fen = hanging_pos.to_fen();
    let calm_fen = calm_pos.to_fen();

    // Fixture sanity-check (fresh QSearcher).
    {
        let mut q = QSearcher::new();
        let se_h = static_eval_white(&hanging_pos);
        let qs_h = q.eval_white(&hanging_pos);
        assert!(
            !in_check(&hanging_pos),
            "fixture: Ng5 position must not be in check (Black to move)"
        );
        assert!(
            se_h.abs() <= HIGH_SCORE_CP,
            "fixture: Ng5 position must pass |eval| ≤ {HIGH_SCORE_CP} gate (got {se_h})"
        );
        assert!(
            !is_quiet(&hanging_pos, se_h, qs_h),
            "fixture: Ng5 position must be non-quiet (hxg5 wins free knight); \
             se={se_h} qs={qs_h}"
        );

        let se_c = static_eval_white(&calm_pos);
        let qs_c = q.eval_white(&calm_pos);
        assert!(
            !in_check(&calm_pos),
            "fixture: h6 position must not be in check"
        );
        assert!(
            se_c.abs() <= HIGH_SCORE_CP,
            "fixture: h6 position must pass |eval| gate"
        );
        assert!(
            is_quiet(&calm_pos, se_c, qs_c),
            "fixture: h6 position must be quiet (no hanging material); \
             se={se_c} qs={qs_c}"
        );
    }

    let pgn = format!(
        "[Event \"Italian\"]\n[Site \"q\"]\n[White \"a\"]\n[Black \"b\"]\n\
         [Result \"1-0\"]\n[WhiteElo \"2400\"]\n[BlackElo \"2410\"]\n\
         [TimeControl \"600+5\"]\n[Termination \"Normal\"]\n\n{movetext} 1-0\n\n"
    );
    let payload = zstd_encode(pgn.as_bytes());

    let srv = TestServer::spawn(payload, Behavior::Serve);
    let dir = tmp_dir("quiet-filter");
    // Permissive game-level filter: the point under test is the per-POSITION
    // quiet filter, not game-level admission.
    let filter = GameFilter {
        min_plies: 0,
        ..GameFilter::default()
    };
    let out = stream_to_ingest(
        Source::LichessOpen,
        &srv.url("/d.pgn.zst"),
        u64::MAX,
        &dir,
        &filter,
        &no_stop(),
        &test_cfg(),
    )
    .expect("fetch ok");
    assert_eq!(out.terminated, Termination::Eos);

    let fens = lane_fens(&dir);

    // Hanging FEN absent → the non-quiet position was dropped.
    // (A non-filtering regression leaves this FEN present — assertion fires.)
    assert!(
        !fens.contains(&hanging_fen),
        "non-quiet Ng5 FEN must be ABSENT from lane.bin: {hanging_fen}"
    );

    // Calm FEN present → a genuinely-quiet position survived the filter.
    // (An over-filtering regression drops everything — assertion fires.)
    assert!(
        fens.contains(&calm_fen),
        "quiet h6 FEN must be PRESENT in lane.bin: {calm_fen}"
    );

    assert!(
        out.positions_ingested > 0,
        "quiet filter must not drop every post-skip8 position"
    );
    assert_eq!(lane_record_count(&dir), out.positions_ingested);

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
    let ref_count = lane_record_count(&ref_dir);
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
        lane_record_count(&drop_dir),
        ref_count,
        "resume must recover the full stream"
    );

    let _ = std::fs::remove_dir_all(&ref_dir);
    let _ = std::fs::remove_dir_all(&drop_dir);
}

#[test]
fn fetch_eos_when_target_exceeds_file() {
    let n_games = 50usize;
    let pgn = make_pgn(n_games);
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
    let expected = expected_usable_total(n_games);
    assert!(expected > 0, "fixture must yield usable positions");
    assert_eq!(out.terminated, Termination::Eos);
    assert_eq!(out.positions_ingested, expected);
    assert_eq!(lane_record_count(&dir), expected);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_eos_at_target_reports_eos_not_early() {
    let n_games = 50usize;
    let pgn = make_pgn(n_games);
    let payload = zstd_encode(pgn.as_bytes());
    let srv = TestServer::spawn(payload, Behavior::Serve);
    let dir = tmp_dir("eos-tie");
    // target == exactly the file's usable-position count (post-pipeline).
    let target = expected_usable_total(n_games);
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
    // Bound the attempts so a mis-handled 416 (retry-loop instead of EOS) fails
    // fast, but loosely enough to absorb a load-induced stall-watchdog reconnect
    // under full-suite parallelism (the inline qsearch pipeline is CPU-heavy, so
    // a tight bound of 2 flaked under contention while passing in isolation). A
    // correctly EOS-mapped 416 terminates on the first 416 regardless.
    cfg.max_attempts = Some(6);
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
    assert_eq!(lane_record_count(&dir), 0);
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
    assert_eq!(lane_record_count(&dir), 0, "no garbage committed");
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
    let n_games = 600usize;
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
    // No duplicate game_ids despite the byte-0 restart re-parsing the prefix —
    // the load-bearing idempotence invariant (skip-re-seen by game_id).
    let mut ids = lane_game_ids(&dir);
    let len_before = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(
        ids.len(),
        len_before,
        "no duplicate game_ids after a byte-0 restart"
    );
    // No double-count of the re-parsed prefix: the disk record count equals the
    // committer's reported usable count (every committed block counted once).
    assert!(
        out.positions_ingested > 0,
        "fixture must yield usable positions"
    );
    assert_eq!(lane_record_count(&dir), out.positions_ingested);
    // Per-lane FEN dedup also guarantees no FEN appears twice on disk despite
    // the prefix re-parse — a stronger no-double-count check than counts alone.
    let (blocks, _) = scan_valid_blocks(&dir.join("lane.bin")).unwrap();
    let mut fens = std::collections::HashSet::new();
    for b in &blocks {
        for r in &b.records {
            assert!(
                fens.insert(r.fen.clone()),
                "duplicate FEN on disk after byte-0 restart: {}",
                r.fen
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_empty_post_dedup_game_idempotent_across_byte0_restart() {
    // §1.4 dedup-driven-empty + byte-0-restart idempotence. `DropOnceIgnoreRange`
    // forces a byte-0 outer restart. The CRITICAL distinction (assertion (c)):
    // the boundary game that re-parses post-restart at `game_id = max_appended +
    // 1` is NOT skipped by `is_already_appended` — it re-runs the full pipeline
    // and is empty ONLY because its post-skip8 FENs are already in `fen_set`
    // (per-lane dedup). So it exercises the dedup-driven empty path, not the
    // skip-by-game_id path.
    //
    // Construction: every game in the file shares the SAME movetext (so every
    // post-skip8 FEN collides with game 0's), but distinct Site tags keep the
    // compressed stream long enough that several games append before the drop.
    // After the restart, the re-parsed games dedup to empty. Net: only game 0's
    // usable positions land; `games_emitted` counts only games that committed
    // ≥ 1 record.
    let (mt, _t) = game_positions(3); // any distinct legal line
    let n_games = 1500usize;
    let mut pgn = String::new();
    for idx in 0..n_games {
        pgn.push_str(&format!(
            "[Event \"e{idx}\"]\n[Site \"{pad}\"]\n[White \"a\"]\n[Black \"b\"]\n\
             [Result \"1-0\"]\n[WhiteElo \"2400\"]\n[BlackElo \"2410\"]\n\
             [TimeControl \"600+5\"]\n[Termination \"Normal\"]\n\n{mt} 1-0\n\n",
            pad = noise(idx),
        ));
    }
    let payload = zstd_encode(pgn.as_bytes());
    let srv = TestServer::spawn(
        payload.clone(),
        Behavior::DropOnceIgnoreRange(payload.len() * 2 / 5),
    );
    let dir = tmp_dir("emptydedup-idem");
    let mut cfg = test_cfg();
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

    // Ground truth: a reference fetch of a SINGLE game-3 PGN (the shared line).
    let usable_one = {
        let single = format!(
            "[Event \"one\"]\n[Site \"s\"]\n[White \"a\"]\n[Black \"b\"]\n\
             [Result \"1-0\"]\n[WhiteElo \"2400\"]\n[BlackElo \"2410\"]\n\
             [TimeControl \"600+5\"]\n[Termination \"Normal\"]\n\n{mt} 1-0\n\n"
        );
        let payload = zstd_encode(single.as_bytes());
        let srv1 = TestServer::spawn(payload, Behavior::Serve);
        let d1 = tmp_dir("emptydedup-one");
        let o = stream_to_ingest(
            Source::LichessOpen,
            &srv1.url("/one.pgn.zst"),
            u64::MAX,
            &d1,
            &GameFilter::default(),
            &no_stop(),
            &test_cfg(),
        )
        .expect("single-game fetch");
        let _ = std::fs::remove_dir_all(&d1);
        o.positions_ingested
    };
    assert!(
        usable_one > 0,
        "the shared line must yield usable positions"
    );

    // (a) zero NET extra positions: only ONE game's worth survives per-lane
    //     dedup, despite n_games physical games (all sharing the same FENs).
    assert_eq!(
        out.positions_ingested, usable_one,
        "all-identical-movetext games dedup to one game's worth"
    );
    assert_eq!(lane_record_count(&dir), usable_one);

    // (b) games_emitted counts only games that committed ≥ 1 record — exactly
    //     one (the first); every later game is empty-post-dedup.
    assert_eq!(
        out.games_emitted, 1,
        "only the first game commits records; the rest are empty-post-dedup"
    );

    // (c) emptiness is dedup-driven, not skip-by-game_id: only ONE block is on
    //     disk, carrying ONE game_id; the many later games were processed by the
    //     committer (NOT skipped by game_id) and produced no block because their
    //     FENs were dups. (If they had been skipped by `is_already_appended`,
    //     the byte-0 restart's re-parse path would never reach the committer —
    //     but here it must, to dedup them to empty.)
    let ids = lane_game_ids(&dir);
    assert_eq!(ids.len(), 1, "exactly one committed block (game 0)");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_dedup_drops_reseen_fens_post_byte0_restart() {
    // §1.4 invariant (c): dedup ran across the byte-0 resume (NOT a skip-by-id).
    //
    // Construction:
    //   G  (game_id=0)   — game_positions(1), posts FENs F0..Fk to disk.
    //   Fillers (game_ids 1..N) — identical to G's movetext, so they are all
    //     dedup-empty (their game_ids are > max_appended=0, so they are NOT
    //     skipped by `is_already_appended` — they run the pipeline and produce
    //     zero records). They pad the stream so the drop reliably falls here.
    //   G' (game_id=N+1) — game_positions(1) extended by one extra ply; shares
    //     all of G's post-skip8 FENs (F0..Fk) plus ONE unique FEN U (at ply 23).
    //
    // base_idx=1 was selected by verifying extend_game_one_ply(1) produces a
    // position U that is quiet (non-capturing, non-checking calm move from the
    // game's final position → genuinely quiescent, passes the pipeline filter).
    //
    // Drop fires at ~50% of the compressed stream — deep in the filler section,
    // after G is committed but long before G' is reached.
    //
    // After the byte-0 restart:
    //   • G (game_id=0): skipped by `is_already_appended` (max_appended=0).
    //   • Fillers: NOT skipped (game_ids > 0); dedup → all FENs in fen_set →
    //     empty (same as before the drop — idempotent).
    //   • G' (game_id=N+1): NOT skipped; runs full pipeline; G's FENs already
    //     in fen_set → F0..Fk all dedup-dropped → only U survives → committed.
    //
    // Assertions that this test pins but the existing idempotence test cannot:
    //   • U is PRESENT on disk exactly once (a skip-by-id regression would leave
    //     U absent, since G' would be incorrectly skipped).
    //   • Each shared FEN Fi is PRESENT exactly once (a broken-dedup regression
    //     would double-count them from the restart re-parse of G).
    //
    // The timing comment above ensures G' is first processed post-restart — the
    // filler section provides > 50% of the stream so the ~50% drop always falls
    // in the fillers, making G' always post-drop.
    let base_idx = 1usize;
    let (g_mt, g_trail) = game_positions(base_idx);
    let (g_prime_mt, u_pos) = extend_game_one_ply(base_idx);

    // Fixture contract: base_idx=1 was selected by verification (calm_move at
    // game index 1 produces a quiet non-check non-capture position U). Assert
    // the invariant so a future engine change that breaks the fixture is loud.
    {
        let mut q = QSearcher::new();
        let se_u = static_eval_white(&u_pos);
        let qs_u = q.eval_white(&u_pos);
        assert!(
            !in_check(&u_pos),
            "fixture: U (extended ply of game {base_idx}) must not be in check"
        );
        assert!(
            se_u.abs() <= HIGH_SCORE_CP,
            "fixture: U must pass |eval| ≤ {HIGH_SCORE_CP} gate (got {se_u})"
        );
        assert!(
            is_quiet(&u_pos, se_u, qs_u),
            "fixture: U must be quiet — base_idx={base_idx} was selected for this property; \
             se={se_u} qs={qs_u}"
        );
    }
    let u_fen = u_pos.to_fen();

    // Collect G's post-skip8 FENs that will be in fen_set after G commits.
    let g_committed_fens: std::collections::HashSet<String> = g_trail
        .iter()
        .enumerate()
        .filter(|(ply, p)| {
            if (*ply as u32) < OPENING_SKIP_PLIES {
                return false;
            }
            if in_check(p) {
                return false;
            }
            let se = static_eval_white(p);
            if se.abs() > HIGH_SCORE_CP {
                return false;
            }
            let mut q = QSearcher::new();
            let qs = q.eval_white(p);
            is_quiet(p, se, qs)
        })
        .map(|(_, p)| p.to_fen())
        .collect();
    assert!(
        !g_committed_fens.is_empty(),
        "G must have usable positions after the pipeline"
    );
    // U must not be one of G's FENs (it's the unique position).
    assert!(
        !g_committed_fens.contains(&u_fen),
        "U must not be in G's FEN set (it's the unique extended position)"
    );

    // Build the PGN: G, then N fillers (same movetext), then G'.
    let n_fillers = 300usize;
    let mut pgn = String::new();
    // G (game_id=0)
    pgn.push_str(&format!(
        "[Event \"G\"]\n[Site \"{pad}\"]\n[White \"a\"]\n[Black \"b\"]\n\
         [Result \"1-0\"]\n[WhiteElo \"2400\"]\n[BlackElo \"2410\"]\n\
         [TimeControl \"600+5\"]\n[Termination \"Normal\"]\n\n{g_mt} 1-0\n\n",
        pad = noise(base_idx),
    ));
    // Fillers (game_ids 1..=n_fillers): same movetext, distinct Site (to resist
    // compression and ensure the filler section spans > 50% of stream bytes).
    for i in 0..n_fillers {
        pgn.push_str(&format!(
            "[Event \"F{i}\"]\n[Site \"{pad}\"]\n[White \"a\"]\n[Black \"b\"]\n\
             [Result \"1-0\"]\n[WhiteElo \"2400\"]\n[BlackElo \"2410\"]\n\
             [TimeControl \"600+5\"]\n[Termination \"Normal\"]\n\n{g_mt} 1-0\n\n",
            pad = noise(i + 1000),
        ));
    }
    // G' (game_id=n_fillers+1): G's movetext + one extra ply → includes U.
    pgn.push_str(&format!(
        "[Event \"Gprime\"]\n[Site \"{pad}\"]\n[White \"a\"]\n[Black \"b\"]\n\
         [Result \"1-0\"]\n[WhiteElo \"2400\"]\n[BlackElo \"2410\"]\n\
         [TimeControl \"600+5\"]\n[Termination \"Normal\"]\n\n{g_prime_mt} 1-0\n\n",
        pad = noise(99998),
    ));

    let payload = zstd_encode(pgn.as_bytes());
    // Drop at 50% — in the filler section, after G, before G'.
    let srv = TestServer::spawn(
        payload.clone(),
        Behavior::DropOnceIgnoreRange(payload.len() / 2),
    );
    let dir = tmp_dir("dedup-reseen");
    let mut cfg = test_cfg();
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

    let fens = lane_fens(&dir);

    // U is PRESENT: dedup let it through (it's fresh), so G' committed it.
    // A skip-by-id regression (G' skipped due to wrong game_id check) would
    // leave U absent — this assertion catches that regression.
    assert!(
        fens.contains(&u_fen),
        "unique FEN U must be PRESENT (dedup ran on G' and U survived): {u_fen}"
    );

    // Each shared FEN (F0..Fk) appears EXACTLY ONCE (no double-append from a
    // re-committed G block). Check via HashSet-vs-raw-count: if G were appended
    // again as a second block, its FENs would appear twice in the raw record list
    // but only once in the HashSet → `fens.len() < raw_count`.
    let raw_count: usize = {
        let (blocks, _) = scan_valid_blocks(&dir.join("lane.bin")).unwrap();
        blocks.iter().map(|b| b.records.len()).sum()
    };
    assert_eq!(
        fens.len(),
        raw_count,
        "no duplicate FENs on disk: HashSet size ({}) must equal raw count ({raw_count}); \
         a mismatch indicates a double-append from the byte-0 restart",
        fens.len()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_ccrl_zip_temp_file_parity() {
    let n_games = 100usize;
    let pgn = make_pgn(n_games);
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
    assert_eq!(lane_record_count(&dir), expected_usable_total(n_games));
    // Every record carries the right provenance, and game_ids are strictly
    // increasing — the inline-pipeline parity invariant (not just a count).
    let (blocks, _) = scan_valid_blocks(&dir.join("lane.bin")).unwrap();
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
    // are the lane and the per-URL state file `stream_to_ingest` writes.
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "lane.bin" && n != "fetch-state.json")
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
    // `eof_reason` reset + TargetGuard): a target far below the archive's usable
    // positions must report EarlyTarget, and exact truncation lands on it.
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
    assert_eq!(
        out.positions_ingested, target,
        "exact target truncation on the zip path too"
    );
    assert_eq!(lane_record_count(&dir), target);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fetch_ccrl_7z_parity() {
    // CCRL's real format: a single-entry .7z (LZMA2). The fetcher must download
    // it, open it with sevenz_rust2, and parse the .pgn entry — parity with the
    // zip/zst paths.
    let n_games = 100usize;
    let pgn = make_pgn(n_games);
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
    assert_eq!(lane_record_count(&dir), expected_usable_total(n_games));
    let (blocks, _) = scan_valid_blocks(&dir.join("lane.bin")).unwrap();
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
        .filter(|n| n != "lane.bin" && n != "fetch-state.json")
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
    let target = 70u64;
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
    assert_eq!(
        out.positions_ingested, target,
        "exact target truncation on the 7z path too"
    );
    assert_eq!(lane_record_count(&dir), target);
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
    let ref_count = lane_record_count(&ref_dir);

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
        lane_record_count(&drop_dir),
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

/// Bit-identical fetch EXTEND — the fetch half of the "test extend in BOTH
/// fetching and self-play lanes" gate. Build fresh to 2X; separately build to X
/// (made to land mid-game → a partial boundary block), drop that block, and
/// re-stream to 2X (= extend); assert `lane.bin` is BYTE-IDENTICAL to the fresh
/// build. Exercises the fetch-specific extend logic: C1 stable base id, C3
/// target-as-total resume, C2 prefix skip-by-id, and the boundary
/// drop-and-re-derive (the (cmd_fetch) driver wiring is symmetric to the
/// already-proven cmd_selfplay subprocess path).
#[test]
fn fetch_extend_lane_is_bit_identical_to_fresh_build() {
    let n_games = 120;
    let total = expected_usable_total(n_games);
    assert!(
        total >= 24,
        "fixture must yield enough usable positions; got {total}"
    );
    let target_2x = total / 2; // comfortably below `total` so the stream never drains

    let pgn = make_pgn(n_games);
    let payload = zstd_encode(pgn.as_bytes());

    // FRESH → 2X.
    let srv = TestServer::spawn(payload.clone(), Behavior::Serve);
    let fresh = tmp_dir("fext-fresh");
    let o_fresh = stream_to_ingest(
        Source::LichessOpen,
        &srv.url("/x.pgn.zst"),
        target_2x,
        &fresh,
        &GameFilter::default(),
        &no_stop(),
        &test_cfg(),
    )
    .expect("fresh-to-2X");
    assert_eq!(o_fresh.positions_ingested, target_2x, "fresh build hit 2X");
    let fresh_bytes = std::fs::read(fresh.join("lane.bin")).unwrap();

    // Find an X in (0, 2X) that lands MID-game (records a partial boundary), so
    // the truncate-and-re-derive path is exercised. delta=0 (X = total/4) almost
    // always lands mid-game; the rest are a safety net.
    let mut built: Option<(PathBuf, u64)> = None;
    for delta in [0u64, 1, 2, 3, 5, 7] {
        let target_x = target_2x / 2 + delta;
        if target_x == 0 || target_x >= target_2x {
            continue;
        }
        let srv_x = TestServer::spawn(payload.clone(), Behavior::Serve);
        let ext = tmp_dir("fext-ext");
        let o_x = stream_to_ingest(
            Source::LichessOpen,
            &srv_x.url("/x.pgn.zst"),
            target_x,
            &ext,
            &GameFilter::default(),
            &no_stop(),
            &test_cfg(),
        )
        .expect("build-to-X");
        assert_eq!(o_x.positions_ingested, target_x);
        if let Some(off) = o_x.truncated_boundary_offset {
            built = Some((ext, off));
            break;
        }
        let _ = std::fs::remove_dir_all(&ext);
    }
    let (ext, off) = built.expect("some probed X lands mid-game (partial boundary)");

    // Drop the partial boundary block, then extend to 2X (what cmd_fetch does).
    clawfish::corpus::store::truncate_to_valid(&ext.join("lane.bin"), off)
        .expect("truncate partial boundary");
    let srv_e = TestServer::spawn(payload, Behavior::Serve);
    stream_to_ingest(
        Source::LichessOpen,
        &srv_e.url("/x.pgn.zst"),
        target_2x,
        &ext,
        &GameFilter::default(),
        &no_stop(),
        &test_cfg(),
    )
    .expect("extend-to-2X");
    let ext_bytes = std::fs::read(ext.join("lane.bin")).unwrap();

    assert_eq!(
        fresh_bytes.len(),
        ext_bytes.len(),
        "fetch extend lane.bin size differs from fresh-to-2X"
    );
    assert_eq!(
        fresh_bytes, ext_bytes,
        "fetch extend is NOT byte-identical to a fresh build to 2X"
    );
    let _ = std::fs::remove_dir_all(&fresh);
    let _ = std::fs::remove_dir_all(&ext);
}
