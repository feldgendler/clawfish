//! End-to-end reproducibility integration test for `corpus selfplay`.
//!
//! Asserts MF4: given the pinned reproducibility knobs (seed, cap-seed, games,
//! workers, max-plies, opening-random-plies), re-running `corpus selfplay`
//! produces a BYTE-IDENTICAL `lane.bin` (M6.H2: flat per-lane corpus, no
//! train/val split — that moves to M6.I, so there is no `val.bin`). This is the
//! strong R5 / reproducibility-mandate guarantee for the self-play slice (no
//! network needed; deterministic from seed + binary + the knobs).
//!
//! Heavy by nature (it runs the full self-play campaign); not `#[ignore]`d
//! because the campaign is small and the test is the most direct check of the
//! reproducibility contract. Runs in <60s on the dev machine.

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

#[test]
fn rerun_byte_identical() {
    // Build a lane from scratch in a temp dir using a fixed seed + workers=1
    // (so the on-disk byte order is deterministic). Then run the same campaign
    // a second time into a sibling dir and assert the produced `lane.bin` is
    // byte-identical between the two runs.
    //
    // M6.H2: the `corpus build` step is gone (dedup/cap moved inline into the
    // consumer's `LaneCommitter`); `corpus selfplay` alone emits the build-ready
    // `lane.bin`, then `corpus finalize` (re)writes the manifest / stats over it.
    // There is no train/val split here (→ M6.I), so no `val.bin`. The
    // byte-identity contract is on `lane.bin`, which `finalize` does NOT
    // transform — so comparing `lane.bin` directly is correct after finalize.
    let seed = "12648430"; // 0xC0FFEE
    let games = "4"; // small — fast
    let workers = "1";
    let max_plies = "40";
    let opening_random_plies = "4";
    let cap_seed = "7";

    let mk_corpus = |label: &str| -> PathBuf {
        let td = TempDir::new(label);
        let out = td.0.clone();
        let mut sp = Command::new(corpus_bin());
        sp.arg("selfplay")
            .args(["--opening-mode", "random"])
            .args(["--seed", seed])
            .args(["--games", games])
            .args(["--workers", workers])
            .args(["--max-plies", max_plies])
            .args(["--opening-random-plies", opening_random_plies])
            .args(["--cap-seed", cap_seed])
            .args(["--out", &out.display().to_string()]);
        run_step("selfplay", sp);
        // finalize: writes the manifest/stats/re-run.sh over lane.bin without
        // transforming the bytes (the M6.H2 build→finalize change).
        let mut fin = Command::new(corpus_bin());
        fin.arg("finalize")
            .args(["--in", &out.display().to_string()]);
        run_step("finalize", fin);
        // Keep the dir alive for the duration of the test; clean up manually.
        std::mem::forget(td);
        out
    };

    let a = mk_corpus("a");
    let b = mk_corpus("b");

    let a_lane = sha256_file(&a.join("lane.bin")).expect("a/lane.bin");
    let b_lane = sha256_file(&b.join("lane.bin")).expect("b/lane.bin");

    assert_eq!(
        a_lane, b_lane,
        "lane.bin must be byte-identical across re-runs \
         (workers=1 + deterministic on-disk order ⇒ pure function of input multiset)"
    );

    // Manual cleanup (we leaked the TempDir guards).
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}

// ──────────────────────────────────────────────────────────────────────────
// Per-game-file architecture: K-invariance test (NEW, §8.2).
// #[ignore]d until the implementation phase lands.
// ──────────────────────────────────────────────────────────────────────────

/// K-invariance: K ∈ {1, 4, 10} all produce a byte-identical `lane.bin` for the
/// same seed + cap-seed.
///
/// Driven by the `selfplay` command alone (the per-game-file consumer applies
/// the full inline pipeline + the shared `LaneCommitter`). The primary
/// acceptance gate for the architecture. M6.H2: no train/val split → no
/// `val.bin`; compare `lane.bin` only.
#[test]
fn rerun_byte_identical_across_k() {
    let seed = "12648430"; // 0xC0FFEE
    let games = "6";
    let max_plies = "40";
    let cap_seed = "7";

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
            .args(["--cap-seed", cap_seed])
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
        // finalize: manifest/stats over lane.bin (no byte transform).
        let mut fin = Command::new(corpus_bin());
        fin.arg("finalize")
            .args(["--in", &out.display().to_string()]);
        run_step("finalize", fin);
        std::mem::forget(td);
        out
    };

    let ks: &[usize] = &[1, 4, 10];
    let dirs: Vec<PathBuf> = ks.iter().map(|&k| run_for_k(k)).collect();

    let ref_lane = sha256_file(&dirs[0].join("lane.bin")).expect("k1/lane.bin");

    for (dir, &k) in dirs[1..].iter().zip(ks[1..].iter()) {
        let lane =
            sha256_file(&dir.join("lane.bin")).unwrap_or_else(|| panic!("k={k}/lane.bin missing"));
        assert_eq!(
            lane, ref_lane,
            "K={k}: lane.bin must be byte-identical to K=1 \
             (per-game-file reorder protocol ⇒ K-independent)"
        );
    }

    // Manual cleanup.
    for dir in &dirs {
        let _ = std::fs::remove_dir_all(dir);
    }
}
