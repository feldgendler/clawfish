//! Integration tests for the per-game-file architecture
//! (`docs/plans/m6.g-per-game-files.md` §8.2 — `tests/corpus_per_game_file.rs`).
//!
//! **Primary acceptance gate:** `k_independent_byte_identical` — K=1 vs K=10
//! produce a byte-identical flat `lane.bin`. This is the property delivered by
//! the per-game-file reorder protocol (every step — per-lane dedup, per-game
//! cap — is a deterministic function of `(seed, cap_seed, game_id)`; K affects
//! only completion timing, not commit order). M6.H2: one flat `lane.bin` per
//! lane, no train/val split (→ M6.I), so no `val.bin`.

use clawfish::corpus::store::scan_valid_blocks;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

fn corpus_bin() -> &'static str {
    env!("CARGO_BIN_EXE_corpus")
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("clawfish-corpus-pgf-{tag}-{pid}-{n}"));
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

fn sha256_file(path: &Path) -> Option<[u8; 32]> {
    let bytes = std::fs::read(path).ok()?;
    Some(clawfish::corpus::manifest::sha256_bytes(&bytes))
}

fn run_selfplay(out: &Path, seed: u64, games: u64, workers: usize, max_plies: u32) {
    let status = Command::new(corpus_bin())
        .arg("selfplay")
        .args(["--seed", &seed.to_string()])
        .args(["--games", &games.to_string()])
        .args(["--workers", &workers.to_string()])
        .args(["--out", &out.display().to_string()])
        .args(["--max-plies", &max_plies.to_string()])
        .args(["--opening-mode", "random"])
        .args(["--cap-seed", "7"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn corpus selfplay");
    assert!(status.success(), "selfplay must succeed");
}

fn lane_records_sorted(dir: &Path) -> Vec<(u64, u32, String, u8)> {
    let (blocks, _) = scan_valid_blocks(&dir.join("lane.bin")).unwrap_or_default();
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

// ──────────────────────────────────────────────────────────────────────────────
// §8.2 Integration tests
// ──────────────────────────────────────────────────────────────────────────────

/// Out-of-order worker completions must commit in game_id order.
///
/// Runs with K=4 workers where games inherently complete out of order
/// (different depths per game) and asserts that `lane.bin` blocks appear
/// in strict game_id order.
#[test]
fn out_of_order_completion_commits_in_game_id_order() {
    let td = TempDir::new("ooo-order");
    let seed = 0xDEAD_BEEFu64;
    let games = 8;
    let workers = 4;
    run_selfplay(td.path(), seed, games, workers, 40);

    let (blocks, _) = scan_valid_blocks(&td.path().join("lane.bin")).unwrap();
    let ids: Vec<u64> = blocks.iter().map(|b| b.game_id).collect();
    assert!(!ids.is_empty(), "lane must have at least one block");
    // Blocks must appear in strictly-increasing game_id order.
    for w in ids.windows(2) {
        assert!(
            w[0] < w[1],
            "lane blocks must be in strict game_id order; got {:?}",
            ids
        );
    }
}

/// **Primary acceptance gate.** K=1 and K=10 produce a byte-identical
/// `lane.bin`.
///
/// This is the central property of the per-game-file architecture: because
/// every step (per-lane dedup, per-game cap) is a deterministic function of
/// `(seed, cap_seed, game_id)`, the byte content of the lane is K-independent.
#[test]
fn k_independent_byte_identical() {
    let seed = 0xC0FFEE_u64;
    let games = 8;
    let max_plies = 40;

    let td1 = TempDir::new("k1");
    let td10 = TempDir::new("k10");
    run_selfplay(td1.path(), seed, games, 1, max_plies);
    run_selfplay(td10.path(), seed, games, 10, max_plies);

    let k1_lane = sha256_file(&td1.path().join("lane.bin")).expect("k1/lane.bin");
    let k10_lane = sha256_file(&td10.path().join("lane.bin")).expect("k10/lane.bin");
    assert_eq!(
        k1_lane, k10_lane,
        "K=1 and K=10 lane.bin must be byte-identical \
         (per-game-file reorder protocol ⇒ K-independent)"
    );
}

/// Inline dedup is deterministic regardless of which K workers complete
/// which games first. Tests K ∈ {1, 2, 4, 8, 16} all produce the same
/// multiset of committed FENs.
#[test]
fn inline_dedup_determinism_under_completion_permutation() {
    let seed = 0xABCD_1234_u64;
    let games = 10;
    let max_plies = 40;
    let ks: &[usize] = &[1, 2, 4, 8, 16];

    let dirs: Vec<TempDir> = ks
        .iter()
        .map(|&k| {
            let td = TempDir::new(&format!("dedup-k{k}"));
            run_selfplay(td.path(), seed, games, k, max_plies);
            td
        })
        .collect();

    let reference = lane_records_sorted(dirs[0].path());
    assert!(!reference.is_empty(), "reference lane must be non-empty");

    for (td, &k) in dirs[1..].iter().zip(ks[1..].iter()) {
        let candidate = lane_records_sorted(td.path());
        assert_eq!(
            candidate, reference,
            "K={k}: inline dedup must produce identical committed FEN set as K=1"
        );
    }
}
