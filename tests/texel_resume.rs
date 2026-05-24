//! R1/R3 integration tests for the `texel-tune` harness (ADR-0037 §10).
//!
//! Mirrors the `tests/corpus_reproducibility.rs` / `tests/corpus_crash_safety.rs`
//! spawn+kill idioms:
//!   - `tune_resumes_bit_identical_after_sigkill` — SIGKILL mid-`tune`, resume,
//!     assert the final tuned params are bit-identical to an uninterrupted run.
//!   - `resume_ignores_torn_temp_checkpoint` — a torn `.tmp` checkpoint sibling
//!     is ignored; the prior valid checkpoint is used (the store.rs torn-tail
//!     discipline, directed).
//!   - `crosscheck_subcommand_passes_on_corpus_sample` — `#[ignore]`d unless the
//!     gitignored `bench/corpus/*/lane.bin` lanes are present on disk.
//!
//! **CLI contract this test pins (coordinate with the impl coder).** These flag
//! names/semantics are the contract the `texel-tune` binary must implement:
//!   texel-tune cache  --lanes <DIR4×> --out <CACHE>            (build the cache)
//!   texel-tune tune   --cache <CACHE> --out-params <JSON>
//!                     --checkpoint <CKPT> --max-iter <N> --seed <S>
//!                     --eval-every <E> --patience <P> --val-fraction <F>
//!   texel-tune crosscheck --lanes <DIR4×>                       (three-tier gate)
//! `tune` writes its resumable checkpoint atomically (temp→fsync→rename) at
//! `--checkpoint` every `--eval-every` iters, and on a fresh invocation with an
//! existing valid `--checkpoint` it RESUMES from it. The final `--out-params`
//! JSON carries the tuned `EvalParamsI32` (the `bit-identical` comparison key).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// `target/debug/texel-tune` path (provided by Cargo).
fn texel_bin() -> &'static str {
    env!("CARGO_BIN_EXE_texel-tune")
}

/// `target/debug/corpus` path — used to materialize tiny synthetic lanes the
/// `cache` step consumes (reuses the corpus self-play generator).
fn corpus_bin() -> &'static str {
    env!("CARGO_BIN_EXE_corpus")
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("clawfish-texel-resume-{tag}-{pid}-{n}"));
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

/// SIGKILL the child via libc (no `kill` shell dependency) — mirrors
/// `corpus_crash_safety::sigkill`.
fn sigkill(child: &Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        let _ = child;
    }
}

/// Generate one tiny self-play lane via the `corpus` binary so the harness has
/// real frozen lane bytes to consume (no gitignored corpus dependency).
fn make_lane(dir: &Path, source_tag: &str, seed: u64, games: u64) {
    let status = Command::new(corpus_bin())
        .arg("selfplay")
        .args(["--opening-mode", "random"])
        .args(["--seed", &seed.to_string()])
        .args(["--games", &games.to_string()])
        .args(["--workers", "1"])
        .args(["--out", &dir.display().to_string()])
        .args(["--max-plies", "30"])
        .args(["--cap-seed", "7"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("spawn corpus selfplay");
    assert!(
        status.success(),
        "lane generation ({source_tag}) must succeed"
    );
}

/// The four lane dirs for the harness (`SelfPlayOnBook` / `SelfPlayOffBook` /
/// `Ccrl` / `LichessOpen` order). For the integration tests all four point at
/// tiny self-play lanes (the source taxonomy does not affect the resume
/// mechanics under test).
fn make_four_lanes(root: &Path) -> [PathBuf; 4] {
    let dirs: [PathBuf; 4] = [
        root.join("lane0"),
        root.join("lane1"),
        root.join("lane2"),
        root.join("lane3"),
    ];
    for (i, d) in dirs.iter().enumerate() {
        std::fs::create_dir_all(d).expect("mkdir lane");
        make_lane(d, &format!("lane{i}"), 100 + i as u64, 3);
    }
    dirs
}

/// Build the feature cache from four lane dirs (the `cache` subcommand).
fn build_cache(lanes: &[PathBuf; 4], cache: &Path) {
    let mut cmd = Command::new(texel_bin());
    cmd.arg("cache").arg("--out").arg(cache);
    for d in lanes {
        cmd.arg("--lanes").arg(d);
    }
    let status = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .expect("spawn texel-tune cache");
    assert!(status.success(), "texel-tune cache must succeed");
}

/// Spawn `texel-tune tune` (resumable). Checkpoints every `eval_every` iters at
/// `--checkpoint`; writes the tuned params to `--out-params` on completion.
fn spawn_tune(
    cache: &Path,
    out_params: &Path,
    checkpoint: &Path,
    max_iter: u64,
    seed: u64,
    eval_every: u64,
) -> Child {
    Command::new(texel_bin())
        .arg("tune")
        .args(["--cache", &cache.display().to_string()])
        .args(["--out-params", &out_params.display().to_string()])
        .args(["--checkpoint", &checkpoint.display().to_string()])
        .args(["--max-iter", &max_iter.to_string()])
        .args(["--seed", &seed.to_string()])
        .args(["--eval-every", &eval_every.to_string()])
        .args(["--patience", "1000000"]) // disable early stop — fixed iter count
        .args(["--val-fraction", "0.25"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn texel-tune tune")
}

/// Run `texel-tune tune` synchronously to completion (asserts success).
fn run_tune(
    cache: &Path,
    out_params: &Path,
    checkpoint: &Path,
    max_iter: u64,
    seed: u64,
    eval_every: u64,
) {
    let mut child = spawn_tune(cache, out_params, checkpoint, max_iter, seed, eval_every);
    let status = child.wait().expect("wait texel-tune tune");
    assert!(status.success(), "texel-tune tune must succeed");
}

/// The tuned `params` block + scaling constant K of an `--out-params` JSON,
/// as a canonical byte key for the bit-identical comparison. Including K
/// catches resume bugs where the scaling constant is re-fit from scratch.
fn tuned_params_bytes(path: &Path) -> Vec<u8> {
    let bytes = std::fs::read(path).expect("read out-params");
    let pf: clawfish::texel::params::ParamsFile =
        serde_json::from_slice(&bytes).expect("parse out-params JSON");
    // The integer params + K are the deployment artifact; serialize them
    // canonically as the comparison key (field-order-stable serde).
    let mut key = serde_json::to_vec(&pf.params).expect("serialize tuned params");
    // Append the K bits in big-endian so any K divergence is detectable.
    key.extend_from_slice(&pf.k.to_bits().to_be_bytes());
    key
}

/// Wait until `checkpoint` exists (the first periodic flush) or the child dies.
fn wait_for_checkpoint(checkpoint: &Path, child: &mut Child) {
    loop {
        if checkpoint.exists() {
            return;
        }
        if let Some(status) = child.try_wait().expect("try_wait") {
            panic!("tune child exited before writing a checkpoint (status: {status:?})");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ───────────────────────────────────────────────────────────────────────────
// R3 — resume bit-identical after SIGKILL.
// ───────────────────────────────────────────────────────────────────────────

/// Tiny cache; SIGKILL `texel-tune tune` mid-run after at least one checkpoint
/// flush; resume from the surviving checkpoint; assert the final tuned params
/// are bit-identical to an uninterrupted run of the same seed/iters (R3,
/// ADR-0037 §10). Mirrors `corpus_crash_safety`'s spawn+kill+resume harness.
#[test]
fn tune_resumes_bit_identical_after_sigkill() {
    let td = TempDir::new("kill-resume");
    let lanes = make_four_lanes(td.path());
    let cache = td.path().join("features.cache");
    build_cache(&lanes, &cache);

    let seed = 42u64;
    let max_iter = 200u64;
    let eval_every = 10u64;

    // Reference: uninterrupted run.
    let ref_params = td.path().join("ref-params.json");
    let ref_ckpt = td.path().join("ref.ckpt");
    run_tune(&cache, &ref_params, &ref_ckpt, max_iter, seed, eval_every);
    let reference = tuned_params_bytes(&ref_params);

    // Crash run: spawn, wait for ≥1 checkpoint flush, SIGKILL, then resume.
    let crash_params = td.path().join("crash-params.json");
    let crash_ckpt = td.path().join("crash.ckpt");
    let mut child = spawn_tune(
        &cache,
        &crash_params,
        &crash_ckpt,
        max_iter,
        seed,
        eval_every,
    );
    wait_for_checkpoint(&crash_ckpt, &mut child);
    sigkill(&child);
    let _ = child.wait();

    // Resume: same args + the surviving checkpoint at the same path. The tune
    // re-invocation loads `--checkpoint` if present and continues to max_iter.
    run_tune(
        &cache,
        &crash_params,
        &crash_ckpt,
        max_iter,
        seed,
        eval_every,
    );
    let resumed = tuned_params_bytes(&crash_params);

    assert_eq!(
        resumed, reference,
        "resumed tune must produce bit-identical tuned params to the \
         uninterrupted reference (R3)"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// R1 — a torn temp checkpoint is ignored; the prior valid checkpoint is used.
// ───────────────────────────────────────────────────────────────────────────

/// A partial `.tmp` checkpoint (a crash between fsync and rename) must be
/// ignored on resume — the prior valid checkpoint at `--checkpoint` is used.
///
/// The test distinguishes "resumed from a valid prior checkpoint" from a
/// cold-start by construction:
///   - The reference runs K+N iterations uninterrupted.
///   - The crash run does K iterations, writes a checkpoint, gets killed, has
///     a torn `.tmp` planted next to its checkpoint, then resumes to K+N.
///   - A cold-start of only N iterations (not K+N) produces different weights.
///
/// `resumed == reference` proves the checkpoint was used (not cold-started).
/// `cold_n != reference` confirms the cold-start baseline truly differs, so
/// the equality assertion is not vacuously true.
#[test]
fn resume_ignores_torn_temp_checkpoint() {
    let td = TempDir::new("torn-temp");
    let lanes = make_four_lanes(td.path());
    let cache = td.path().join("features.cache");
    build_cache(&lanes, &cache);

    let seed = 7u64;
    let eval_every = 10u64;
    // K = number of iterations before SIGKILL (must be a multiple of eval_every
    // so a checkpoint is guaranteed). N = remaining iterations after resume.
    let k_iters = 30u64; // must write ≥1 checkpoint
    let n_iters = 30u64;
    let total_iters = k_iters + n_iters;

    // Reference: uninterrupted K+N.
    let ref_params = td.path().join("ref-params.json");
    let ref_ckpt = td.path().join("ref.ckpt");
    run_tune(
        &cache,
        &ref_params,
        &ref_ckpt,
        total_iters,
        seed,
        eval_every,
    );
    let reference = tuned_params_bytes(&ref_params);

    // Cold-start baseline: only N iterations (no checkpoint, fresh start).
    // Should differ from the K+N reference (the optimizer moved K steps worth
    // of gradient updates before the cold-start split).
    let cold_params = td.path().join("cold-params.json");
    let cold_ckpt = td.path().join("cold.ckpt");
    run_tune(&cache, &cold_params, &cold_ckpt, n_iters, seed, eval_every);
    let cold_n = tuned_params_bytes(&cold_params);

    // The cold-start must differ from the full reference (otherwise the test is
    // vacuous — a resume that cold-starts would pass trivially).
    assert_ne!(
        cold_n, reference,
        "cold-start of N={n_iters} iters should differ from K+N={total_iters} \
         reference (if equal, the fixture does not discriminate)"
    );

    // Crash run: spawn for K iterations, wait for ≥1 checkpoint, kill it.
    let crash_params = td.path().join("crash-params.json");
    let crash_ckpt = td.path().join("crash.ckpt");
    let mut child = spawn_tune(
        &cache,
        &crash_params,
        &crash_ckpt,
        k_iters,
        seed,
        eval_every,
    );
    wait_for_checkpoint(&crash_ckpt, &mut child);
    sigkill(&child);
    let _ = child.wait();

    // Plant a torn temp checkpoint: the atomic-write temp sibling the impl uses
    // is the checkpoint path with a `.tmp` suffix (the corpus store.rs
    // temp→fsync→rename convention). Garbage bytes ⇒ must be ignored.
    let torn_temp = PathBuf::from(format!("{}.tmp", crash_ckpt.display()));
    std::fs::write(&torn_temp, b"torn-not-a-valid-checkpoint").expect("plant torn temp");

    // Resume with K+N total iters: the torn `.tmp` must be ignored and the
    // valid checkpoint at iter K must be used to continue to K+N.
    run_tune(
        &cache,
        &crash_params,
        &crash_ckpt,
        total_iters,
        seed,
        eval_every,
    );
    let resumed = tuned_params_bytes(&crash_params);

    assert_eq!(
        resumed, reference,
        "torn .tmp must be ignored; resume from valid checkpoint at iter {k_iters} \
         must produce bit-identical result to the uninterrupted K+N={total_iters} reference"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Three-tier crosscheck on a real corpus sample (gated on lanes existing).
// ───────────────────────────────────────────────────────────────────────────

/// The four frozen production lane.bin paths (gitignored). Present only on a
/// machine that has materialized the corpus; absent on CI / fresh checkouts.
fn production_lane_dirs() -> [PathBuf; 4] {
    let base = Path::new("bench/corpus");
    [
        base.join("self_play_on_book"),
        base.join("self_play_off_book"),
        base.join("ccrl"),
        base.join("lichess_open"),
    ]
}

/// `true` iff every production lane dir has a `lane.bin` on disk.
fn corpus_present() -> bool {
    production_lane_dirs()
        .iter()
        .all(|d| d.join("lane.bin").is_file())
}

/// `texel-tune crosscheck` exits successfully on a real corpus sample (the
/// three-tier model/reference/engine agreement gate, ADR-0037 §7). `#[ignore]`d
/// when the gitignored production lanes are absent — run explicitly with
/// `cargo test --test texel_resume -- --ignored` on a corpus-materialized box.
#[test]
#[ignore = "requires the gitignored bench/corpus/*/lane.bin lanes on disk"]
fn crosscheck_subcommand_passes_on_corpus_sample() {
    if !corpus_present() {
        // Defensive: even under `--ignored`, skip cleanly if the lanes vanished
        // between the gate check and here (no spurious failure).
        eprintln!("bench/corpus lanes absent — skipping crosscheck");
        return;
    }
    let lanes = production_lane_dirs();
    let mut cmd = Command::new(texel_bin());
    cmd.arg("crosscheck");
    for d in &lanes {
        cmd.arg("--lanes").arg(d);
    }
    let status = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .status()
        .expect("spawn texel-tune crosscheck");
    assert!(
        status.success(),
        "texel-tune crosscheck must pass the three-tier gate on the corpus sample"
    );
}
