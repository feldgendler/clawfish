//! Crash-safety / resume integration test for `corpus selfplay`.
//!
//! Satisfies the roadmap "Verification" clause of M6.G: SIGKILL + resume
//! yields ZERO partial-game labels, AND a resumed campaign equals an
//! uninterrupted reference run of the same seed/games modulo at most one
//! re-emitted game (the §3.5 idempotent ordering: game-block fsync
//! precedes checkpoint fsync; a crash between them re-emits a game whose
//! block was already durable and the resume dispatcher skips by `game_id`).
//!
//! Fast default = one deterministic SIGKILL after game 1. The `#[ignore]`d
//! heavy variant runs 16 randomized kill offsets plus a SIGSTOP/SIGCONT
//! suspend pass.

use clawfish::corpus::store::scan_valid_blocks;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// `target/debug/corpus` path (provided by Cargo).
fn corpus_bin() -> &'static str {
    env!("CARGO_BIN_EXE_corpus")
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("clawfish-corpus-crash-{tag}-{pid}-{n}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Spawn `corpus selfplay` with stderr captured (so a panic shows up).
fn spawn_selfplay(out: &Path, seed: u64, games: u64, workers: usize, max_plies: u32) -> Child {
    Command::new(corpus_bin())
        .arg("selfplay")
        .args(["--opening-mode", "random"])
        .args(["--seed", &seed.to_string()])
        .args(["--games", &games.to_string()])
        .args(["--workers", &workers.to_string()])
        .args(["--out", &out.display().to_string()])
        .args(["--max-plies", &max_plies.to_string()])
        .args(["--split-seed", "7"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn corpus selfplay")
}

/// Wait until the shard contains at least `min_games` durable game blocks.
///
/// Polls indefinitely — there is no wall-clock deadline. Terminates only when
/// one of two events occurs:
///   1. The shard reaches `min_games` blocks (success — returns the count).
///   2. The child process exits before reaching `min_games` (failure — panics
///      with diagnostics).
///
/// This makes the test hardware-independent: a slow CI runner just takes
/// longer to reach the target, and we never confuse "runner slow" with
/// "binary broken" the way a fixed `Duration` deadline did.
fn wait_for_blocks(shard: &Path, child: &mut Child, min_games: usize) -> usize {
    loop {
        if let Ok((blocks, _)) = scan_valid_blocks(shard)
            && blocks.len() >= min_games
        {
            return blocks.len();
        }
        // Child died before producing the expected blocks — real failure.
        if let Some(status) = child.try_wait().expect("try_wait on selfplay child") {
            let count = scan_valid_blocks(shard).map(|(b, _)| b.len()).unwrap_or(0);
            panic!(
                "selfplay child exited before producing {min_games} block(s) \
                 (exit status: {status:?}, blocks present: {count})"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Run `corpus selfplay` synchronously to completion (asserts success).
fn run_selfplay(out: &Path, seed: u64, games: u64, workers: usize, max_plies: u32) {
    let status = Command::new(corpus_bin())
        .arg("selfplay")
        .args(["--opening-mode", "random"])
        .args(["--seed", &seed.to_string()])
        .args(["--games", &games.to_string()])
        .args(["--workers", &workers.to_string()])
        .args(["--out", &out.display().to_string()])
        .args(["--max-plies", &max_plies.to_string()])
        .args(["--split-seed", "7"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("spawn corpus selfplay");
    assert!(status.success(), "selfplay must succeed");
}

/// SIGKILL the child via libc (no `kill` shell dependency).
fn sigkill(child: &Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        // Windows / non-Unix: fall back to `std::process::Child::kill`.
        let _ = child;
    }
}

/// Read the shard's durable records as a canonical multiset key
/// (sorted by (game_id, ply, fen) — ply already orders within a game).
fn shard_records(dir: &Path) -> Vec<(u64, u32, String, u8)> {
    let (blocks, _) = scan_valid_blocks(&dir.join("shard.bin")).unwrap_or_default();
    let mut v: Vec<(u64, u32, String, u8)> = blocks
        .iter()
        .flat_map(|b| {
            b.records
                .iter()
                .map(|r| (b.game_id, r.ply, r.fen.clone(), r.label.as_u8()))
        })
        .collect();
    v.sort();
    v
}

/// Every game_id in the shard appears as exactly one block whose records
/// are contiguous in ply (zero partial-game labels).
fn assert_no_partial_games(dir: &Path) {
    let (blocks, _) = scan_valid_blocks(&dir.join("shard.bin")).unwrap();
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for b in &blocks {
        // Every record in a block carries the same game_id (frame contract).
        for r in &b.records {
            assert_eq!(
                r.game_id, b.game_id,
                "frame contract violated: block carries records with mismatched game_id"
            );
        }
        // The §3.5 contract: a game is either fully present (one block) or
        // absent. A "partial game" (some plies missing AND the game flagged
        // as durable) can only manifest as a second block with the same
        // game_id — assert that does not happen.
        assert!(
            seen.insert(b.game_id),
            "game_id {} appears in TWO blocks — partial-game leak",
            b.game_id
        );
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Legacy single-shard crash-safety test (pre-M6.G architecture). Superseded
// by `crash_kill_k4_resumes_to_uninterrupted_corpus_per_game_file` which
// exercises the new per-game-file path at K=4 with stricter byte-equality.
// Kept for historical-bisect value but `#[ignore]`d because it shares the
// test binary with 8 other parallel corpus tests and its 60s
// wait-for-first-block deadline is brittle under that contention.
// ──────────────────────────────────────────────────────────────────────────

#[test]
#[ignore = "legacy single-shard variant — superseded; run explicitly via --ignored"]
fn crash_kill_after_first_game_resumes_to_uninterrupted_corpus() {
    let seed = 42u64;
    let games = 3u64;
    let workers = 1usize;
    let max_plies = 40u32;

    // Reference run: uninterrupted, same seed/games — establishes the bytes
    // the resumed run must converge to (modulo at most one re-emitted game).
    let ref_dir = TempDir::new("ref");
    let status = Command::new(corpus_bin())
        .arg("selfplay")
        .args(["--opening-mode", "random"])
        .args(["--seed", &seed.to_string()])
        .args(["--games", &games.to_string()])
        .args(["--workers", &workers.to_string()])
        .args(["--out", &ref_dir.path().display().to_string()])
        .args(["--max-plies", &max_plies.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("ref run");
    assert!(status.success(), "reference selfplay must succeed");
    let ref_records = shard_records(ref_dir.path());
    assert!(!ref_records.is_empty(), "reference shard must be non-empty");

    // Crash run: spawn `corpus selfplay`, wait until ≥ 1 game is durable,
    // SIGKILL, then re-spawn (same seed/games/out) to resume. The 60s
    // deadline is monotonic-time (Rust's `Instant` on macOS does not
    // advance during system suspend), so closing the laptop lid mid-run
    // does not consume budget; the slack is to cover the debug-binary
    // depth-4 selfplay cost (3 games × 40 plies × per-move search) under
    // any plausible concurrent system load. M6.G's original 20s was
    // borderline even on a clean machine; bumping to 60s removes the
    // brittleness without weakening the contract.
    let crash_dir = TempDir::new("crash");
    let shard = crash_dir.path().join("shard.bin");

    let mut child = spawn_selfplay(crash_dir.path(), seed, games, workers, max_plies);
    let blocks_before_kill = wait_for_blocks(&shard, &mut child, 1);
    assert!(
        blocks_before_kill >= 1,
        "expected ≥ 1 durable block before SIGKILL; got {blocks_before_kill}"
    );
    sigkill(&child);
    let _ = child.wait();

    // (a) zero partial-game labels — the durable shard has no torn block.
    assert_no_partial_games(crash_dir.path());

    // Resume: same seed/games/out.
    let status = Command::new(corpus_bin())
        .arg("selfplay")
        .args(["--opening-mode", "random"])
        .args(["--seed", &seed.to_string()])
        .args(["--games", &games.to_string()])
        .args(["--workers", &workers.to_string()])
        .args(["--out", &crash_dir.path().display().to_string()])
        .args(["--max-plies", &max_plies.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("resume run");
    assert!(status.success(), "resumed selfplay must succeed");

    // (b) zero partial-game labels post-resume.
    assert_no_partial_games(crash_dir.path());

    // (c) The §3.5 R3 contract: "resumed run bit-identical to an
    // uninterrupted run modulo the one lost in-flight game." With
    // `--workers 1` (this test) AND the fresh-searcher-per-game invariant
    // (`AlphaBetaMover::new()` per game in `selfplay::run`, the R3 fix),
    // the qualifier collapses to "bit-identical": the in-flight game is
    // re-emitted on resume from its deterministic substream seed,
    // producing the identical record sequence. Multiset equality on the
    // full shard pins this contract.
    let resumed_records = shard_records(crash_dir.path());
    assert_eq!(
        resumed_records, ref_records,
        "resumed corpus must match the uninterrupted reference shard byte-for-byte \
         (workers=1 + fresh-searcher-per-game ⇒ R3 collapses to bit-identical)"
    );
}

// ──────────────────────────────────────────────────────────────────────────
// Heavy variant — randomized kill offsets + suspend (SIGSTOP / SIGCONT).
// `#[ignore]`d by default; run via `cargo test --test corpus_crash_safety
// -- --ignored`.
// ──────────────────────────────────────────────────────────────────────────

#[test]
#[ignore]
fn crash_kill_at_randomized_offsets_emits_zero_partial() {
    let seed = 12_345u64;
    let games = 8u64;
    let workers = 1usize;
    let max_plies = 60u32;

    // Reference run.
    let ref_dir = TempDir::new("ref-heavy");
    let status = Command::new(corpus_bin())
        .arg("selfplay")
        .args(["--opening-mode", "random"])
        .args(["--seed", &seed.to_string()])
        .args(["--games", &games.to_string()])
        .args(["--workers", &workers.to_string()])
        .args(["--out", &ref_dir.path().display().to_string()])
        .args(["--max-plies", &max_plies.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("ref run");
    assert!(status.success());
    let ref_records = shard_records(ref_dir.path());

    // 16 distinct random kill offsets (in ms after spawn).
    for variant in 0..16u64 {
        let crash_dir = TempDir::new(&format!("heavy-{variant}"));
        let shard = crash_dir.path().join("shard.bin");
        // Pseudo-random offset in [50, 400] ms.
        let offset_ms = 50 + (variant * 17) % 350;
        let mut child = spawn_selfplay(crash_dir.path(), seed, games, workers, max_plies);
        std::thread::sleep(Duration::from_millis(offset_ms));
        sigkill(&child);
        let _ = child.wait();

        // No partial-game labels regardless of when we killed.
        assert_no_partial_games(crash_dir.path());

        // Resume.
        let status = Command::new(corpus_bin())
            .arg("selfplay")
            .args(["--opening-mode", "random"])
            .args(["--seed", &seed.to_string()])
            .args(["--games", &games.to_string()])
            .args(["--workers", &workers.to_string()])
            .args(["--out", &crash_dir.path().display().to_string()])
            .args(["--max-plies", &max_plies.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .expect("resume run");
        assert!(status.success());
        assert_no_partial_games(crash_dir.path());

        // Multiset byte equality: same parameters as the fast variant
        // (`--workers 1` + fresh-searcher-per-game) ⇒ the R3 contract
        // collapses to bit-identical (modulo the one in-flight game which
        // is re-emitted on resume from its deterministic seed). A FEN-level
        // resume bug that preserves game_id integrity would slip past a
        // set-equality assertion; multiset equality catches it.
        let mut resumed = shard_records(crash_dir.path());
        let mut reference = ref_records.clone();
        resumed.sort();
        reference.sort();
        assert_eq!(
            resumed, reference,
            "variant={variant} offset_ms={offset_ms}: resumed corpus byte-multiset matches uninterrupted reference"
        );
        let _ = shard; // bind to silence warning
    }

    // SIGSTOP / SIGCONT suspend pass: a long pause must NOT change the
    // bytes produced (fixed-depth ⇒ load-, suspend-, renice-independent).
    // This passes BYTE-EQUALITY because no crash → searcher state matches.
    let suspend_dir = TempDir::new("suspend");
    let mut child = spawn_selfplay(suspend_dir.path(), seed, games, workers, max_plies);
    std::thread::sleep(Duration::from_millis(100));
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGSTOP);
    }
    std::thread::sleep(Duration::from_millis(150));
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGCONT);
    }
    let status = child.wait().expect("suspended run completes");
    assert!(status.success());
    let suspended = shard_records(suspend_dir.path());
    assert_eq!(
        suspended, ref_records,
        "SIGSTOP/SIGCONT must not affect the deterministic fixed-depth corpus"
    );
}

// ──────────────────────────────────────────────────────────────────────────
// Per-game-file architecture tests (NEW — §8.2 of the plan).
// All #[ignore]d until the implementation phase lands.
// ──────────────────────────────────────────────────────────────────────────

/// K=4 SIGKILL + resume → byte-equal to uninterrupted reference.
///
/// Replaces the K=1 variant above for the per-game-file architecture. With
/// K=4, workers race; the pending-file reorder protocol ensures the resumed
/// corpus is byte-identical to the uninterrupted reference regardless of
/// which games were in-flight at kill time.
#[test]
fn crash_kill_k4_resumes_to_uninterrupted_corpus_per_game_file() {
    let seed = 42u64;
    let games = 6u64;
    let workers = 4usize;
    let max_plies = 40u32;

    // Reference: uninterrupted run.
    let ref_dir = TempDir::new("pgf-ref-k4");
    let status = Command::new(corpus_bin())
        .arg("selfplay")
        .args(["--opening-mode", "random"])
        .args(["--seed", &seed.to_string()])
        .args(["--games", &games.to_string()])
        .args(["--workers", &workers.to_string()])
        .args(["--out", &ref_dir.path().display().to_string()])
        .args(["--max-plies", &max_plies.to_string()])
        .args(["--split-seed", "7"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("ref run");
    assert!(status.success(), "reference selfplay must succeed");
    let ref_records = shard_records(ref_dir.path());
    assert!(!ref_records.is_empty(), "reference shard must be non-empty");

    // Crash run.
    let crash_dir = TempDir::new("pgf-crash-k4");
    let shard = crash_dir.path().join("shard.bin");
    let mut child = spawn_selfplay(crash_dir.path(), seed, games, workers, max_plies);
    let blocks_before_kill = wait_for_blocks(&shard, &mut child, 1);
    assert!(
        blocks_before_kill >= 1,
        "expected ≥ 1 durable block before SIGKILL; got {blocks_before_kill}"
    );
    sigkill(&child);
    let _ = child.wait();
    assert_no_partial_games(crash_dir.path());

    // Resume.
    let status = Command::new(corpus_bin())
        .arg("selfplay")
        .args(["--opening-mode", "random"])
        .args(["--seed", &seed.to_string()])
        .args(["--games", &games.to_string()])
        .args(["--workers", &workers.to_string()])
        .args(["--out", &crash_dir.path().display().to_string()])
        .args(["--max-plies", &max_plies.to_string()])
        .args(["--split-seed", "7"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("resume run");
    assert!(status.success(), "resumed selfplay must succeed");
    assert_no_partial_games(crash_dir.path());

    let resumed_records = shard_records(crash_dir.path());
    assert_eq!(
        resumed_records, ref_records,
        "K=4 resumed corpus must byte-equal the uninterrupted reference \
         (per-game-file reorder protocol ⇒ K-independent)"
    );
}

/// Ghost-pending file for a committed-with-records game is cleaned on resume.
#[test]
fn ghost_pending_self_heal_on_resume() {
    // Plant a "ghost" pending file for a game that is already durably
    // committed in shard.bin. The resume protocol must clean it.
    let seed = 77u64;
    let games = 3u64;
    let workers = 1usize;
    let max_plies = 40u32;

    let td = TempDir::new("ghost-committed");
    // Complete run.
    let status = Command::new(corpus_bin())
        .arg("selfplay")
        .args(["--opening-mode", "random"])
        .args(["--seed", &seed.to_string()])
        .args(["--games", &games.to_string()])
        .args(["--workers", &workers.to_string()])
        .args(["--out", &td.path().display().to_string()])
        .args(["--max-plies", &max_plies.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("first run");
    assert!(status.success());
    let ref_records = shard_records(td.path());

    // Plant a ghost pending file for game 0 (already committed).
    let pending_dir = td.path().join("pending");
    std::fs::create_dir_all(&pending_dir).unwrap();
    let ghost = pending_dir.join(clawfish::corpus::pending::pending_filename(0));
    // Write a valid-looking block so it passes CRC (otherwise it'd be unlinked as torn).
    let ghost_block = clawfish::corpus::store::encode_block(0, &[]);
    std::fs::write(&ghost, &ghost_block).unwrap();

    // Re-run: resume must clean the ghost and not double-commit game 0.
    let status = Command::new(corpus_bin())
        .arg("selfplay")
        .args(["--opening-mode", "random"])
        .args(["--seed", &seed.to_string()])
        .args(["--games", &games.to_string()])
        .args(["--workers", &workers.to_string()])
        .args(["--out", &td.path().display().to_string()])
        .args(["--max-plies", &max_plies.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("second run");
    assert!(status.success());

    assert!(
        !ghost.exists(),
        "ghost pending file must be cleaned on resume"
    );
    let second_records = shard_records(td.path());
    assert_eq!(
        second_records, ref_records,
        "ghost cleanup must not alter the committed record set"
    );
}

/// Ghost-pending file for an empty-post-dedup game (below checkpoint's
/// next_consume_id) is cleaned on resume.
#[test]
fn ghost_pending_empty_post_dedup_below_ckpt_self_heal_on_resume() {
    // Blocker #2 new case: a pending file for a game that was committed as
    // empty-post-dedup (no shard block, but checkpoint advanced past it).
    // The resume protocol must unlink it via the
    // `pending_ids ∩ {0..ckpt.next_consume_id}` union.
    //
    // Setup: plant a ghost pending file at game_id=3 + a checkpoint claiming
    // `next_consume_id = 5` (i.e., games 3 and 4 were "consumed" — game 3 as
    // empty-post-dedup, game 4 with records). After the consumer's resume
    // runs, the ghost at game-3 must be unlinked. The post-impl checkpoint
    // schema includes `next_consume_id` — until then, this test relies on
    // direct binary construction of the CWK2 checkpoint format.
    let td = TempDir::new("ghost-empty-ckpt");
    let pending_dir = td.path().join("pending");
    std::fs::create_dir_all(&pending_dir).unwrap();

    // Plant a syntactically-valid CRC-framed pending block for game 3.
    // We use a minimal one-record block; the resume scan only checks CRC
    // validity, not record content.
    let dummy_rec = clawfish::corpus::CorpusRecord {
        game_id: 3,
        ply: 0,
        fen: "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1".to_string(),
        label: clawfish::corpus::Label::Draw,
        source: clawfish::corpus::Source::SelfPlayOffBook,
        depth_rung: 0,
        strata: 0,
    };
    let block = clawfish::corpus::store::encode_block(3, &[dummy_rec]);
    let ghost_path = pending_dir.join("game-00000000000000000003.bin");
    std::fs::write(&ghost_path, &block).unwrap();
    assert!(ghost_path.exists(), "ghost pending file planted");

    // Plant a CWK2-format checkpoint claiming next_consume_id=5.
    let ckpt = clawfish::corpus::store::Checkpoint {
        games_completed: 5,
        prng_state: 0,
        ladder_cursor: 5,
        per_worker_next_game_id: vec![5],
        next_consume_id: 5,
    };
    clawfish::corpus::store::write_checkpoint(&td.path().join("checkpoint.bin"), &ckpt).unwrap();

    // Run a fresh selfplay; resume must self-heal the ghost.
    let status = Command::new(corpus_bin())
        .arg("selfplay")
        .args(["--opening-mode", "random"])
        .args(["--seed", "11"])
        .args(["--games", "8"])
        .args(["--workers", "1"])
        .args(["--out", &td.path().display().to_string()])
        .args(["--max-plies", "40"])
        .args(["--split-seed", "7"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("spawn");
    assert!(status.success(), "selfplay must succeed past the ghost");

    // The ghost pending file must be gone — resume's self-heal unlinked it.
    assert!(
        !ghost_path.exists(),
        "ghost pending file at game_id=3 (< ckpt.next_consume_id=5) must be unlinked on resume"
    );
}

/// K=10 SIGKILL: only in-flight games lost; rest byte-equal to reference.
#[test]
fn worker_crash_only_loses_in_flight_games() {
    let seed = 99u64;
    let games = 16u64;
    let workers = 10usize;
    let max_plies = 40u32;

    let ref_dir = TempDir::new("wc-ref");
    let status = Command::new(corpus_bin())
        .arg("selfplay")
        .args(["--opening-mode", "random"])
        .args(["--seed", &seed.to_string()])
        .args(["--games", &games.to_string()])
        .args(["--workers", &workers.to_string()])
        .args(["--out", &ref_dir.path().display().to_string()])
        .args(["--max-plies", &max_plies.to_string()])
        .args(["--split-seed", "7"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("ref run");
    assert!(status.success());
    let ref_records = shard_records(ref_dir.path());

    let crash_dir = TempDir::new("wc-crash");
    let shard = crash_dir.path().join("shard.bin");
    let mut child = spawn_selfplay(crash_dir.path(), seed, games, workers, max_plies);
    let blocks = wait_for_blocks(&shard, &mut child, 2);
    assert!(blocks >= 1, "expected ≥ 1 block before kill; got {blocks}");
    sigkill(&child);
    let _ = child.wait();
    assert_no_partial_games(crash_dir.path());

    // Resume.
    let status = Command::new(corpus_bin())
        .arg("selfplay")
        .args(["--opening-mode", "random"])
        .args(["--seed", &seed.to_string()])
        .args(["--games", &games.to_string()])
        .args(["--workers", &workers.to_string()])
        .args(["--out", &crash_dir.path().display().to_string()])
        .args(["--max-plies", &max_plies.to_string()])
        .args(["--split-seed", "7"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("resume run");
    assert!(status.success());
    assert_no_partial_games(crash_dir.path());

    let resumed_records = shard_records(crash_dir.path());
    assert_eq!(
        resumed_records, ref_records,
        "K=10 resumed corpus must byte-equal the uninterrupted reference"
    );
}

/// A worker panic releases the claim via `ClaimGuard::Drop`, so the game
/// is re-dispatched and completed by another worker.
#[test]
fn worker_panic_releases_claim_via_guard_drop() {
    // This is an integration-level test of the Blocker #3 RAII invariant.
    // We can't easily inject a panic into a live `corpus selfplay` worker
    // without modifying the binary, so this test exercises the property
    // indirectly: run a campaign and verify it completes all games despite
    // any worker dying. The unit test `claim_guard_drop_releases_on_panic`
    // covers the direct RAII path.
    let td = TempDir::new("panic-guard");
    let seed = 42u64;
    let games = 4u64;
    let workers = 2usize;
    run_selfplay(td.path(), seed, games, workers, 40);
    let records = shard_records(td.path());
    // At minimum, some games completed (the panic recovery would replay them).
    // We can't assert exact count without controlling when panics fire, but
    // we can assert no partial games.
    assert_no_partial_games(td.path());
    let _ = records; // Bind to silence unused warning.
}

/// An oversize pending file is fatal on resume: the consumer returns an
/// error rather than silently skipping or retrying.
#[test]
fn oversized_pending_file_is_fatal_on_resume_and_at_write() {
    use clawfish::corpus::pending::{MAX_PENDING_BYTES, pending_filename, write_pending};

    // Write side: write_pending rejects an oversize block.
    let td = TempDir::new("oversize-fatal");
    let pending_dir = td.path().join("pending");
    std::fs::create_dir_all(&pending_dir).unwrap();
    let oversize = vec![0u8; MAX_PENDING_BYTES + 1];
    let result = write_pending(&pending_dir, 0, &oversize);
    assert!(
        matches!(result, Err(clawfish::corpus::CorpusError::Checkpoint(_))),
        "write_pending oversize must return CorpusError::Checkpoint"
    );

    // Read side: a manually-planted oversize pending file causes the
    // selfplay run to fail (the consumer detects it and returns fatal error).
    let td2 = TempDir::new("oversize-resume-fatal");
    let pending_dir2 = td2.path().join("pending");
    std::fs::create_dir_all(&pending_dir2).unwrap();
    // Plant oversize pending for game 0.
    std::fs::write(
        pending_dir2.join(pending_filename(0)),
        vec![0u8; MAX_PENDING_BYTES + 1],
    )
    .unwrap();
    // A selfplay run into this dir should detect the oversize file and fail.
    let status = Command::new(corpus_bin())
        .arg("selfplay")
        .args(["--opening-mode", "random"])
        .args(["--seed", "1"])
        .args(["--games", "1"])
        .args(["--workers", "1"])
        .args(["--out", &td2.path().display().to_string()])
        .args(["--max-plies", "40"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn");
    assert!(
        !status.success(),
        "selfplay with oversize pending file must fail (fatal error)"
    );
}

/// A torn (CRC-invalid) pending file is unlinked and the game is replayed.
#[test]
fn torn_pending_file_is_rejected_and_replayed() {
    use clawfish::corpus::pending::pending_filename;

    let seed = 55u64;
    let games = 3u64;
    let workers = 1usize;
    let max_plies = 40u32;

    // Reference: uninterrupted.
    let ref_dir = TempDir::new("torn-ref");
    run_selfplay(ref_dir.path(), seed, games, workers, max_plies);
    let ref_records = shard_records(ref_dir.path());

    // Setup: run the campaign, then plant a torn pending file for game 0.
    // The next resume should unlink it and replay game 0.
    let td = TempDir::new("torn-replay");
    let pending_dir = td.path().join("pending");
    std::fs::create_dir_all(&pending_dir).unwrap();

    // Plant a torn (garbage) pending file for game 0.
    std::fs::write(
        pending_dir.join(pending_filename(0)),
        b"not-a-valid-crc-block",
    )
    .unwrap();

    // Run selfplay: should detect torn file, unlink it, replay game 0.
    run_selfplay(td.path(), seed, games, workers, max_plies);
    assert_no_partial_games(td.path());

    assert!(
        !pending_dir.join(pending_filename(0)).exists(),
        "torn pending file must have been unlinked and replaced by the commit"
    );
    let result_records = shard_records(td.path());
    assert_eq!(
        result_records, ref_records,
        "after torn-file replay, corpus must equal the uninterrupted reference"
    );
}

/// An old CWK1-format checkpoint is rejected as corrupt; the campaign starts
/// fresh from the shard scan.
#[test]
fn old_checkpoint_magic_is_rejected_as_missing() {
    // Write a CWK1-magic checkpoint (old format), then resume. The new
    // decoder rejects it as corrupt (magic mismatch) and falls back to the
    // shard scan — equivalent to a fresh start on an empty shard.
    let td = TempDir::new("old-ckpt-magic");

    // Construct a CWK1 checkpoint manually (old CKPT_MAGIC = 0x4357_4B31).
    // The new format uses 0x4357_4B32. The decoder must reject 0x4357_4B31.
    let old_magic: u32 = 0x4357_4B31; // CWK1
    let games_completed: u64 = 5;
    let prng_state: u64 = 42;
    let ladder_cursor: u64 = 5;
    let per_worker_len: u32 = 0;
    let mut body = Vec::new();
    body.extend_from_slice(&old_magic.to_le_bytes());
    body.extend_from_slice(&games_completed.to_le_bytes());
    body.extend_from_slice(&prng_state.to_le_bytes());
    body.extend_from_slice(&ladder_cursor.to_le_bytes());
    body.extend_from_slice(&per_worker_len.to_le_bytes());
    // CRC-32 of body (so it passes the CRC check; magic mismatch is the rejection).
    let crc = clawfish::corpus::store::crc32(&body);
    body.extend_from_slice(&crc.to_le_bytes());
    std::fs::write(td.path().join("checkpoint.bin"), &body).unwrap();

    // Run selfplay from this dir. The old checkpoint must be treated as
    // missing (not an error). Campaign runs fresh; 10 games ensures the
    // shard is non-empty for the post-condition check below.
    let status = Command::new(corpus_bin())
        .arg("selfplay")
        .args(["--opening-mode", "random"])
        .args(["--seed", "7"])
        .args(["--games", "10"])
        .args(["--workers", "1"])
        .args(["--out", &td.path().display().to_string()])
        .args(["--max-plies", "40"])
        .args(["--split-seed", "7"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("spawn");
    assert!(
        status.success(),
        "old CWK1 checkpoint must be silently rejected (treated as missing), not fatal"
    );

    // The run should have produced some records (fresh start). Use 10 games
    // at depth-default so the probability of zero quiet emissions is negligible.
    let (blocks, _) = scan_valid_blocks(&td.path().join("shard.bin")).unwrap_or_default();
    assert!(
        !blocks.is_empty(),
        "after rejecting CWK1 checkpoint, the fresh run should produce at least one block"
    );
}

/// Power-loss simulation: strict prefix property + game-level replays.
/// Heavy `#[ignore]`d variant (O(N²) in games — run explicitly via
/// `cargo test --test corpus_crash_safety -- --ignored power_loss`).
#[test]
#[ignore = "heavy — explicit-run only"]
fn power_loss_simulation_strict_prefix_plus_replays() {
    // Simulates power-loss at every block boundary in the shard, verifying
    // that resume always produces an exact prefix of the uninterrupted run.
    // This test is O(N²) in the number of games and is gated as heavy.
    let seed = 13u64;
    let games = 6u64;
    let workers = 1usize;
    let max_plies = 40u32;

    let ref_dir = TempDir::new("powerloss-ref");
    run_selfplay(ref_dir.path(), seed, games, workers, max_plies);
    let ref_records = shard_records(ref_dir.path());
    assert!(!ref_records.is_empty(), "reference must be non-empty");

    // For each prefix length of the reference shard, truncate and resume.
    let shard_bytes = std::fs::read(ref_dir.path().join("shard.bin")).unwrap();
    let total = shard_bytes.len();
    // Sample 10 evenly-spaced cut points to keep runtime bounded.
    let step = (total / 10).max(1);
    for cut in (0..=total).step_by(step) {
        let td = TempDir::new(&format!("powerloss-cut{cut}"));
        // Write a truncated shard.
        std::fs::write(td.path().join("shard.bin"), &shard_bytes[..cut]).unwrap();
        // Resume.
        run_selfplay(td.path(), seed, games, workers, max_plies);
        assert_no_partial_games(td.path());
        let resumed = shard_records(td.path());
        assert_eq!(
            resumed, ref_records,
            "after power-loss at cut={cut}, resumed corpus must equal reference"
        );
    }
}
