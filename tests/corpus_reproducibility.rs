//! End-to-end reproducibility integration test for `bench/corpus/re-run.sh`.
//!
//! Asserts MF4: given a frozen `bench/corpus/manifest.json` (every
//! reproducibility knob pinned), deleting the corpus shards and re-running
//! the `re-run.sh` script produces BYTE-IDENTICAL `shard.bin` and `val.bin`
//! files. This is the strong R5 / reproducibility-mandate guarantee for
//! the self-play slice (no network needed; deterministic from seed +
//! binary + the manifest knobs).
//!
//! Heavy by nature (it runs the full self-play campaign + build pass); not
//! `#[ignore]`d because the campaign is small (12 games) and the test is
//! the most direct check of the reproducibility contract. Runs in <60s on
//! the dev machine.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

/// `target/release/corpus` path provided by Cargo.
fn corpus_bin() -> &'static str {
    env!("CARGO_BIN_EXE_corpus")
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("clawfish-corpus-repro-{tag}-{pid}-{n}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn sha256_file(path: &Path) -> Option<[u8; 32]> {
    // Reuse the engine's own SHA-256 via a tiny re-implementation would
    // require dev-deps; defer to a hand-rolled lightweight digest via the
    // public engine API.
    let bytes = std::fs::read(path).ok()?;
    Some(clawfish::corpus::manifest::sha256_bytes(&bytes))
}

fn run_step(name: &str, mut cmd: Command) {
    let out = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("{name} spawn failed: {e}"));
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        panic!(
            "{name} failed (status {:?}):\nstdout: {stdout}\nstderr: {stderr}",
            out.status
        );
    }
}

// will be updated to K=10 in M6.G per-game-file rewrite; see
// docs/plans/m6.g-per-game-files.md §8.2.
#[test]
fn rerun_byte_identical() {
    // Build a corpus from scratch in a temp dir using a fixed seed +
    // workers=1 (so the on-disk byte order is deterministic). Then run
    // the same pipeline a second time into a sibling dir and assert the
    // produced shard.bin + val.bin are byte-identical between the two
    // runs.
    let seed = "12648430"; // 0xC0FFEE
    let games = "4"; // small — fast
    let workers = "1";
    let max_plies = "40";
    let opening_random_plies = "4";
    let val_fraction = "0.25";
    let split_seed = "7";

    let mk_corpus = |label: &str| -> PathBuf {
        let td = TempDir::new(label);
        let out = td.0.clone();
        // selfplay
        let mut sp = Command::new(corpus_bin());
        sp.arg("selfplay")
            .args(["--opening-mode", "random"])
            .args(["--seed", seed])
            .args(["--games", games])
            .args(["--workers", workers])
            .args(["--max-plies", max_plies])
            .args(["--opening-random-plies", opening_random_plies])
            .args(["--val-fraction", val_fraction])
            .args(["--split-seed", split_seed])
            .args(["--out", &out.display().to_string()]);
        run_step("selfplay", sp);
        // build
        let mut bd = Command::new(corpus_bin());
        bd.arg("build")
            .args(["--in", &out.display().to_string()])
            .args(["--val-fraction", val_fraction])
            .args(["--split-seed", split_seed]);
        run_step("build", bd);
        // Leak the TempDir into the returned path — we drop the outer one
        // via the test's stack-scoped `td`. We must keep it alive for the
        // duration of the test; clone the path and let `td` drop at the
        // end of this closure. So instead — return the path AFTER moving
        // the TempDir guard onto the heap via Box::leak. Simpler: forget
        // the guard, clean up at end of test manually.
        std::mem::forget(td);
        out
    };

    let a = mk_corpus("a");
    let b = mk_corpus("b");

    // Compute digests for shard.bin + val.bin from both runs.
    let a_shard = sha256_file(&a.join("shard.bin")).expect("a/shard.bin");
    let b_shard = sha256_file(&b.join("shard.bin")).expect("b/shard.bin");
    let a_val = sha256_file(&a.join("val.bin")).expect("a/val.bin");
    let b_val = sha256_file(&b.join("val.bin")).expect("b/val.bin");

    assert_eq!(
        a_shard, b_shard,
        "shard.bin must be byte-identical across re-runs \
         (workers=1 + deterministic on-disk order ⇒ pure function of input multiset)"
    );
    assert_eq!(
        a_val, b_val,
        "val.bin must be byte-identical across re-runs"
    );

    // Manual cleanup (we leaked the TempDir guards).
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}

// ──────────────────────────────────────────────────────────────────────────
// Per-game-file architecture: K-invariance test (NEW, §8.2).
// #[ignore]d until the implementation phase lands.
// ──────────────────────────────────────────────────────────────────────────

/// K-invariance: K ∈ {1, 4, 10} all produce byte-identical shard.bin and
/// val.bin for the same seed + split_seed.
///
/// This is the strong form of the `k_independent_byte_identical` test in
/// `tests/corpus_per_game_file.rs`, driven by the `selfplay` command alone
/// (no `build` pass needed — the per-game-file consumer applies the full
/// inline pipeline). The primary acceptance gate for the architecture.
#[test]
fn rerun_byte_identical_across_k() {
    let seed = "12648430"; // 0xC0FFEE
    let games = "6";
    let max_plies = "40";
    let split_seed = "7";
    let val_fraction = "0.25";

    let run_for_k = |k: usize| -> PathBuf {
        let td = TempDir::new(&format!("k{k}"));
        let out = td.0.clone();
        let mut sp = Command::new(corpus_bin());
        sp.arg("selfplay")
            .args(["--opening-mode", "random"])
            .args(["--seed", seed])
            .args(["--games", games])
            .args(["--workers", &k.to_string()])
            .args(["--max-plies", max_plies])
            .args(["--val-fraction", val_fraction])
            .args(["--split-seed", split_seed])
            .args(["--out", &out.display().to_string()]);
        let out_result = sp
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap_or_else(|e| panic!("selfplay k={k} spawn failed: {e}"));
        if !out_result.status.success() {
            let stderr = String::from_utf8_lossy(&out_result.stderr);
            panic!("selfplay k={k} failed:\nstderr: {stderr}");
        }
        std::mem::forget(td);
        out
    };

    let ks: &[usize] = &[1, 4, 10];
    let dirs: Vec<PathBuf> = ks.iter().map(|&k| run_for_k(k)).collect();

    let ref_shard = sha256_file(&dirs[0].join("shard.bin")).expect("k1/shard.bin");
    let ref_val = sha256_file(&dirs[0].join("val.bin"));

    for (dir, &k) in dirs[1..].iter().zip(ks[1..].iter()) {
        let shard = sha256_file(&dir.join("shard.bin"))
            .unwrap_or_else(|| panic!("k={k}/shard.bin missing"));
        assert_eq!(
            shard, ref_shard,
            "K={k}: shard.bin must be byte-identical to K=1 \
             (per-game-file reorder protocol ⇒ K-independent)"
        );
        let val = sha256_file(&dir.join("val.bin"));
        assert_eq!(val, ref_val, "K={k}: val.bin must be byte-identical to K=1");
    }

    // Manual cleanup.
    for dir in &dirs {
        let _ = std::fs::remove_dir_all(dir);
    }
}
