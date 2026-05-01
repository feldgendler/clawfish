# ELOH.E — Migrate fastchess SPRT/match/smoke flows to the in-process harness

The harness's SPRT layer. Adds LLR-based pentanomial-GSPRT stopping inside `src/bin/elo-iterate.rs`, a fixed-games match mode, per-engine option plumbing for the candidate side, and rewrites `scripts/sprt.sh sprt|match` plus `scripts/match.sh self-play|vs-stockfish` as thin wrappers around the harness binary. Retains fastchess only for `scripts/match.sh compliance` (the `--compliance` UCI shake-out has no in-house substitute and is the load-bearing reason the binary stays on disk). Closes the fastchess-as-default-runner era for clawfish.

Spec source: orchestrator decision 2026-05-01 ("replace fastchess everywhere except `--compliance`"). Math source: `docs/research/eloh.e-pentanomial-sprt.md` (just-written research; load-bearing for §4 and §6). Validation precedent: ELOH.B's σ-stopping back-test gate (synthetic Bernoulli stream → stopping verdict at expected sample size) and ELOH.D's chi-squared sampler gate (deterministic in-tree gate plus deferred manual replay).

## 0. Sizing note

Estimated total: ~165 prod LOC (`mod sprt` ~80 + CLI extensions ~20 + controller hook ~35 + summary formatters ~30) + ~145 test LOC + ~60 LOC of script rewrites + ~190 LOC of doc-delta and ADR = ~560 LOC; +30% contingency → ~730 LOC. Comfortably under the workflow's 800-LOC ceiling and roughly the same shape as ELOH.B's ~650-LOC landing. The §0 and §13 totals agree (cross-checked at v2 revision after the `--engine-option`-already-exists discovery struck ~10 LOC of speculative work).

The math is genuinely small (one closed-form LLR formula, one Wald-bound pair, one pentanomial CI computation — all in `mod sprt`); the integration cost lives in the controller hook (per-pair LLR check after each `PairComplete`), the script rewrites, and the new ADR. The fixed-games match mode is essentially "harness with `--k0 0 --target-sigma 0` and no SPRT flag set" — `scripts/sprt.sh rating-estimate` already proves this path works.

**`--engine-option` is already implemented end-to-end** — CLI parse at `src/bin/elo-iterate.rs:328`, `Args` field at line 68, `WorkerConfig` field at line 5490, `setoption` plumbing at line 5668, repeatability test at line 689. ELOH.E reuses it as-is for `scripts/match.sh self-play`'s `Random_Seed=1`/`Random_Seed=2` plumbing; no new code or tests for that surface.

## 1. Goals

- **New SPRT mode in the harness.** `--sprt-elo0 N --sprt-elo1 N --sprt-alpha F --sprt-beta F` flags (separate, not parameterized; see §10). Presence of `--sprt-elo0` activates SPRT mode (the other three flags become required when it is set; mutually exclusive with rating-estimate's `--k0 0 --target-sigma 0` fixed-anchor mode and with σ-stopping). The harness checks LLR after every completed pair and emits an accept/reject verdict when LLR crosses the Wald bound `B = log(β/(1−α))` or `A = log((1−β)/α)`, or `MaxGamesReached` if `--max-games` exhausts before either bound. Pentanomial-GSPRT only — logistic Elo, normal approximation, no trinomial fallback.
- **Per-engine option plumbing — already implemented.** `--engine-option K=V` exists at `src/bin/elo-iterate.rs:328` (symmetric to `--opponent-option`), with full plumbing through `Args` → `WorkerConfig` → setoption. ELOH.E consumes it from the rewritten scripts (`scripts/match.sh self-play` passes `Random_Seed=1`/`=2`); no new harness code.
- **Fixed-games match mode driven through the harness.** Same SPRT-mode driver but `--max-games N` is the only termination criterion (no LLR check). `scripts/sprt.sh match` and `scripts/match.sh self-play|vs-stockfish` use this path. Reuses the existing controller plumbing — fixed-games is structurally what `scripts/sprt.sh rating-estimate` already does, just without the `--initial-elo`-anchored Stockfish opponent.
- **Pentanomial CI for the report line.** Final summary at run end emits both: (a) the SPRT verdict (or `MaxGamesReached`), (b) the post-hoc 95% CI on Δ Elo from the accumulated pair counts (per research report §5: pair-variance estimator, normal-approximation CI, inverse-logistic transformation). Both are reported in distinct fields. The CI applies in fixed-games match mode too — same formula, same data path, just no LLR check gating termination.
- **`scripts/sprt.sh sprt|match` and `scripts/match.sh self-play|vs-stockfish` rewritten as thin wrappers.** Each invokes `cargo run --release --bin elo-iterate -- ...` with the appropriate flag set. `scripts/sprt.sh rating-estimate` keeps its current shape (already a harness wrapper since ELOH.B). `scripts/match.sh compliance` keeps fastchess (`--compliance` is fastchess-internal; no substitute). `scripts/install-fastchess.sh` retained because compliance still depends on it.
- **Opening-position handling: startpos-only.** M4.D mixed-TC SPRT was startpos-only; the current `scripts/sprt.sh sprt|match` invocation does not pass `-openings` (fastchess defaults to startpos). Startpos-only is the explicit ELOH.E choice. Opening-book PGN/EPD ingestion is deferred to a follow-up if a consumer asks (M4.D-class mixed-TC SPRT and M5+ search-tuning SPRT campaigns have not asked).
- **Back-validation gate: Part 1 in-tree (synthetic Bernoulli-pair stream → SPRT converges to expected verdict at expected sample size); Part 2 deferred manual replay of M4.D mixed-TC SPRT (statistical-equivalence gate, not bit-equivalence).**

## 2. Out of scope

- **BayesElo and nElo conventions.** Logistic-Elo only — matches fastchess default per research report §3 ("`elo0=0 elo1=10` with no explicit `model=` uses logistic Elo"). Out of scope: a `--sprt-model` flag with `bayesian|normalized` variants. Logistic is the field-standard for chess-engine SPRT and is what every reference implementation we surveyed uses.
- **Opening books beyond startpos.** No `--openings PGN_FILE`, no `--openings EPD_FILE`, no Polyglot book ingestion. Defer until a real consumer asks (M4.D's first run will be startpos-only; M5+ tuning campaigns inherit the same default).
- **Resume-from-checkpoint.** Same as ELOH.B — YAGNI for M4-cadence runs. Crash recovery requires re-running the SPRT.
- **Non-pentanomial reporting.** No trinomial fallback, no per-game W/D/L variance estimator. Pentanomial is strictly more efficient (research report §1) and is what fastchess reports as `Ptnml(0-2)` under `-report penta=true`. Rejected alternative: emit both pentanomial and trinomial counts for cross-validation — rejected because the only consumer (the SPRT verdict) uses pentanomial, and trinomial lines in the output would invite confusion about which is the canonical statistic.
- **`model=` CLI flag.** Logistic is hardcoded. Documented choice. If a future contributor wants `--sprt-model normalized`, that's a separate plan with its own back-test gate.
- **Mid-LLR diagnostic output.** The harness emits LLR only at run end (verdict line), not per-pair. The progress-line output (`progress: t=…`) is for K-update mode; SPRT mode's interim observable is the pair-count vector, which the §4 summary already carries via `wld=W-L-D` cumulative. Rejected alternative: per-pair LLR in the progress line — rejected because mid-run LLR invites premature stopping by humans reading the output, which invalidates the α/β guarantees (research report §6 pitfall row 4).
- **`-pgnout` parity as a single combined PGN file.** Per-game PGN files in `<out-dir>/games/<N>.pgn` already exist (ELOH.A); ELOH.E adds a *post-processing concatenation* into `<out-dir>/match.pgn` at run end, see §10 open question (c). Rejected alternative: emit a single combined PGN inline (replacing per-game files) — rejected because per-game files are useful for triage of a single bad game and are already what every existing harness consumer expects.

## 3. Files modified

| File | Change | LOC est |
|---|---|---|
| `src/bin/elo-iterate.rs` | New `mod sprt` (LLR math + Wald bounds + pair classification + pentanomial CI; ~80 LOC pure functions). Existing modules extended: `cli` (new `--sprt-elo0/elo1/alpha/beta` flags + post-loop validation rejecting any non-default K-update flag combined with `--sprt-*` and the rating-estimate mode mutex); `controller` (per-pair LLR check after each `PairComplete`, with **per-worker `pair_score_buffers: HashMap<u32, Vec<f64>>`** to support `concurrency > 1` correctly — see §4.3); `summary` (extend the run-end emit with `sprt:` and `ci:` keys; preserve the existing `summary:` and `summary-by-tc:` lines); also a small per-game `match.pgn` concatenation step (§10(c)). New `StopReason::SprtAcceptH0` and `StopReason::SprtAcceptH1` variants. **One-field extension to `WorkerReport::GameComplete`: adds `worker_id: u32` (the worker producer already has it in scope at line 5804; consumer destructures at line 5970).** | +165 prod / +145 tests |
| `scripts/sprt.sh` | Rewrite `sprt` and `match` subcommand bodies to invoke the harness binary instead of fastchess. Keep `rating-estimate` arm as-is (already a harness wrapper since ELOH.B). Remove the fastchess locator + version-check block from this script's top-of-file (no longer needed — `rating-estimate` doesn't use it; `sprt` and `match` will be harness-only after this). Keep the historical-commit baseline worktree-build flow (worktree at `target/sprt-baselines/<slug>` plus build-or-cache logic). | -90 / +60 |
| `scripts/match.sh` | Rewrite `self-play` and `vs-stockfish` arms to invoke the harness with `--engine-option Random_Seed=1 --opponent-option Random_Seed=2` (self-play) or `--opponent-option UCI_LimitStrength=true --opponent-option UCI_Elo=1320` (vs-stockfish). Keep `compliance` arm as-is (fastchess `--compliance`). Keep the fastchess locator + version-check block (still load-bearing for the compliance arm); narrow its precondition so it only fires when `compliance` is the chosen subcommand (move the locator into the `compliance` case body). | -50 / +40 |
| `scripts/install-fastchess.sh` | No code change. The script remains the canonical bootstrap step for the compliance check; the doc-delta updates the rationale comment in the file's header to read "Required for `scripts/match.sh compliance` only — all other harness flows use the in-process binary." | header re-state |
| `docs/architecture.md` | Tournament-harness row updated: "fastchess: `scripts/match.sh compliance` only" (was "fastchess: smoke + SPRT"). New SPRT-runner row: "in-process harness: SPRT (LLR + pentanomial), fixed-games match, smoke flows, rating estimation." | +5 / -3 |
| `docs/workflow.md` | SPRT section (~lines 347–397) replaced. Drop the "Run via `fastchess`" line. Add a worked example invocation through `scripts/sprt.sh sprt`. Document the `--compliance`-stays-on-fastchess split. The historical-commit baseline methodology (lines ~360–397) stays as-is — only the runner inside the methodology changes. | +30 / -15 |
| `docs/tooling/elo-iteration-harness.md` | New ELOH.E sub-phase row in the §"Sub-phases" table (after the ELOH.D row). New "ELOH.E scope detail" section between ELOH.D's section and "Branches and worktrees." | +60 |
| `docs/tooling-backlog.md` | Close any sub-bullets that ELOH.E retires. Likely candidates: any "harness-side SPRT" item (none currently exist as a discrete bullet — the SPRT flow has lived in `scripts/sprt.sh` since M2); a "fastchess deprecation" follow-up if such a bullet exists. Worth a grep at landing. | +5 / -0..10 |
| `docs/roadmap.md` | Mark ELOH.E in the in-flight column under the Tooling/fuzzing row. Add a one-line entry. | +2 |
| `CLAUDE.md` | Status row — "ELOH.E in flight" or "ELOH.E done" depending on landing posture. | +2 |
| `bench/eloh-e.md` | New milestone bench file. ELOH.E adds zero engine-side code paths (no change to `src/search.rs`, `src/eval/`, or any other engine surface). The file documents the no-regression observation — node count + NPS byte-identical to `bench/eloh-d.md`'s baseline — and links to `bench/eloh-d.md` as load-bearing. | new ~15 |
| `docs/decisions/0012-tournament-harness.md` | **Amend** with a "Status: Superseded for non-compliance flows by ADR-0022 (ELOH.E)" header note + a new "## 2026-05-01 amendment" section. The amendment retains §1 (fastchess as the compliance runner) and §3 (engine registry) but supersedes §4 (output paths — harness writes elsewhere), §5 (adjudication — harness already has its own thresholds since ELOH.B), and §6 (smoke contract — now harness-side). The "0022" number is reserved at landing time; if 0022 is taken by a parallel landing, both this amendment and the new ADR file are renumbered atomically before commit. | +30 |
| `docs/decisions/0022-eloh-sprt-mechanics.md` | **New ADR** consolidating the load-bearing SPRT-mechanics decisions: pentanomial-only, logistic Elo (no `model=` flag), normal-approximation GSPRT (not exact MLE), pair-cadence LLR check (never per-game), startpos-only opening, separate flags (not parameterized string). Number 0022 reserved at landing time; if 0022 is taken in the meantime, the next available number. | new ~80 |
| `.cargo/mutants.toml` | Anticipated: `sprt::compute_llr`'s closed-form arithmetic (each multiplicative term is a likely surviving mutant), `sprt::wald_bounds` (log-arithmetic), `sprt::classify_pair_score` (5-bin truth table), `sprt::pentanomial_ci` (the inverse-logistic at the CI endpoints). Survivor-driven; default zero new entries. | +0..15 |

## 4. Type definitions and key signatures

### 4.1 `mod sprt` (new, `src/bin/elo-iterate.rs`)

Pure functions; no I/O; fully unit-testable in isolation. Same precedent as `mod estimator` and `mod sigma` from ELOH.B.

```rust
//! Pentanomial-GSPRT machinery for the in-process harness.
//!
//! Math reference: docs/research/eloh.e-pentanomial-sprt.md §2.3 (per-pair
//! GSPRT formula), §5 (post-hoc Δ Elo CI), §6 (pitfalls — load-bearing for
//! the per-pair-not-per-game cadence + discard-incomplete-pair invariants).
//!
//! Logistic Elo only. Normal-approximation GSPRT (not the exact MLE form
//! used by vdbergh/pentanomial). The approximation is what cutechess-cli
//! and fastchess use under `model=logistic` and is well-calibrated for
//! pool sizes ≥ 100 pairs — well within ELOH.E's working range.

#[derive(Debug, Clone, Copy)]
pub(crate) struct SprtConfig {
    /// H0 Elo gap. Standard chess SPRT uses 0.
    pub elo0: f64,
    /// H1 Elo gap. Standard chess SPRT uses 5–10.
    pub elo1: f64,
    /// False-positive rate. Standard 0.05.
    pub alpha: f64,
    /// False-negative rate. Standard 0.05.
    pub beta: f64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SprtState {
    /// Pair counts indexed by pair-score bin (0..=4 → 0.0/0.5/1.0/1.5/2.0).
    /// See research report §4 for the W/D/L → pentanomial classification.
    pub pair_counts: [u32; 5],
    /// Last computed LLR. Updated by `update_pair` after each completed pair.
    pub llr: f64,
    /// Singleton games discarded because their partner game in a pair did
    /// not complete (e.g. `--max-games` boundary). Audit-only; not used in
    /// LLR computation. Pinned by research report §6 pitfall row 2.
    pub discarded_singletons: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SprtVerdict {
    /// LLR is in the indifference zone (B, A); keep playing.
    Continue,
    /// LLR ≤ B. Patch fails (H0: zero or negative Elo).
    AcceptH0,
    /// LLR ≥ A. Patch passes (H1: positive Elo).
    AcceptH1,
}

/// Wald bounds for the test. Pure function of (alpha, beta).
///
/// `B = log(β / (1 - α))` (lower bound — accept H0).
/// `A = log((1 - β) / α)` (upper bound — accept H1).
///
/// At α = β = 0.05: `B ≈ -2.944`, `A ≈ 2.944`. Pinned by unit test
/// `wald_bounds_at_alpha_beta_05`.
pub(crate) fn wald_bounds(alpha: f64, beta: f64) -> (f64, f64);

/// Compute LLR from current state and config. Pure function. Per research
/// report §2.3:
///
/// ```text
/// LL(elo) = 1 / (1 + 10^(-elo / 400))                # base-10 logistic
/// s_i_pair = 2 * LL(elo_i)                           # expected pair score
/// mu = sum(n[i] * pair_score[i]) / N                  # observed mean
/// var = sum(n[i] * pair_score[i]^2) / N - mu^2        # per-pair variance
/// LLR = (s1 - s0) * (2*mu - s0 - s1) / (var/N) / 2.0  # GSPRT normal approx
/// ```
///
/// Returns 0.0 when `N == 0` or `var == 0` (degenerate; the indifference
/// zone holds and the caller will see `Continue`). Pinned by
/// `compute_llr_zero_at_indifference_midpoint` and
/// `compute_llr_positive_when_sample_favors_h1`.
pub(crate) fn compute_llr(state: &SprtState, cfg: &SprtConfig) -> f64;

/// Classify a (game-A outcome, game-B outcome) pair into a pair-score bin
/// (0..=4 → 0.0/0.5/1.0/1.5/2.0). Per research report §4 truth table.
/// Pure function; takes per-side scores in candidate-POV (1.0/0.5/0.0).
/// Pinned by `classify_pair_score_truth_table`.
pub(crate) fn classify_pair_score(game_a: f64, game_b: f64) -> usize;

/// Append a new pair to the state, recompute LLR, and return the verdict.
/// `pair_score` is the candidate's *total* score across the pair (0.0–2.0).
/// Caller is responsible for never calling this with a singleton (use
/// `discard_singleton` for the audit-only count).
pub(crate) fn update_pair(
    state: &mut SprtState,
    cfg: &SprtConfig,
    pair_score: f64,
) -> SprtVerdict;

/// Increment the audit-only singleton counter. Used at run end when
/// `--max-games` interrupts an in-flight pair.
pub(crate) fn discard_singleton(state: &mut SprtState);

/// Post-hoc 95% CI on Δ Elo from accumulated pair counts. Per research
/// report §5: pair-variance SE, normal-approximation CI on the mean pair
/// score, inverse-logistic transformation to Elo. Returns
/// `(elo_lo, elo_est, elo_hi)`. NaN-safe at `N < 2` (returns
/// `(NaN, NaN, NaN)`); the caller must guard the print path. Pinned by
/// `pentanomial_ci_hand_computed_example` matching M4.D's
/// `Ptnml(0-2) = [5, 40, 78, 56, 21]` → `[+18.18, +65.61]` shape.
pub(crate) fn pentanomial_ci(state: &SprtState) -> (f64, f64, f64);
```

**Why pure functions in their own module.** ELOH.B's `mod estimator` and `mod sigma` have the same shape: the mathematically interesting part is a closed-form computation, isolated for unit testing, called from the controller's drain loop. ELOH.E follows that precedent — the controller hook is a one-line call to `update_pair` after each `PairComplete`, and the math correctness lives in `mod sprt::tests`. Rejected alternative: integrate the LLR check directly into `mod controller` — rejected because it couples math correctness to the controller's mpsc-channel state, which is non-trivial to test in isolation (synthetic-pool fixture would have to know about LLR semantics).

### 4.2 CLI extensions (`mod cli`)

```rust
pub(crate) struct Args {
    // ELOH.A/B/C/D fields unchanged.

    /// `--sprt-elo0 N`. When `Some`, SPRT mode is active and the other three
    /// `--sprt-*` flags are required. Mutually exclusive with `--k0 0
    /// --target-sigma 0` rating-estimate mode at parse time.
    pub sprt_elo0: Option<f64>,
    /// `--sprt-elo1 N`. Required iff `sprt_elo0.is_some()`.
    pub sprt_elo1: Option<f64>,
    /// `--sprt-alpha F`. Required iff `sprt_elo0.is_some()`. Must be in (0, 1).
    pub sprt_alpha: Option<f64>,
    /// `--sprt-beta F`. Required iff `sprt_elo0.is_some()`. Must be in (0, 1).
    pub sprt_beta: Option<f64>,
}
```

**`--engine-option K=V`.** Already implemented (`src/bin/elo-iterate.rs:328-334` + test at line 689). No code change.

**Post-loop validation** (immediately after the existing `--k0 0 requires --target-sigma 0` check at line 457). The plan rejects *all* three combinations of "SPRT plus any non-default K-update / σ-stopping flag" rather than silently overriding any of them — the loud-rejection precedent from ELOH.B's `--k0 0 requires --target-sigma 0` mutex applies (see plan-review v2 must-fix #5: silent overrides invite operator confusion):

```rust
let sprt_active = sprt_elo0.is_some();
if sprt_active {
    let all_set = sprt_elo0.is_some() && sprt_elo1.is_some()
        && sprt_alpha.is_some() && sprt_beta.is_some();
    if !all_set {
        return Err(CliError::InvalidValue(
            "--sprt-elo0 requires all of --sprt-elo1, --sprt-alpha, --sprt-beta".into()));
    }
    // SPRT mode is fundamentally a different statistical regime from
    // K-update rating estimation: SPRT compares two binaries at fixed
    // (unknown) Elo via LLR-bound stopping; K-update tracks a moving
    // estimate of one binary's Elo against an anchor. Combining them
    // is methodologically incoherent. Reject any non-default K-update,
    // σ-stopping, or anchor-tracking flag explicitly.
    let k_update_default = (k0 == K0_DEFAULT) && (tau == TAU_DEFAULT);
    let sigma_stopping_default = target_sigma == TARGET_SIGMA_DEFAULT
        && stop_window == STOP_WINDOW_DEFAULT
        && stop_window_confirm == STOP_WINDOW_CONFIRM_DEFAULT;
    let anchor_default = initial_elo == INITIAL_ELO_DEFAULT;  // sentinel "0.0"
    if !(k_update_default && sigma_stopping_default && anchor_default) {
        return Err(CliError::InvalidValue(
            "--sprt-* is incompatible with K-update flags (--k0, --tau, \
             --target-sigma, --stop-window, --stop-window-confirm, \
             --initial-elo). Remove the offending flags and re-run.".into()));
    }
}
let alpha_in_range = sprt_alpha.map(|a| a > 0.0 && a < 1.0).unwrap_or(true);
let beta_in_range = sprt_beta.map(|b| b > 0.0 && b < 1.0).unwrap_or(true);
if !alpha_in_range || !beta_in_range {
    return Err(CliError::InvalidValue("--sprt-alpha and --sprt-beta must be in (0, 1)".into()));
}
```

The `*_DEFAULT` sentinels are extracted as `const` items at module scope so the mutex check can compare against them without re-encoding the literal values. (If they're not already extracted, that's a +5 LOC extraction; this is a minor refactor that the controller will need anyway for clarity.)

Rejected alternatives, both rejected by plan-review v2:
- *Silently override K-update flags when `--sprt-*` is set.* Rejected because it makes a `--target-sigma 30` value silently disappear; the caller's command line stops being self-documenting.
- *Reject only the rating-estimate frozen-anchor (`--k0 0 --target-sigma 0`) combination, allow K-update + SPRT to coexist.* Rejected because K-update + SPRT has no consistent statistical interpretation — SPRT's LLR formula assumes both engines are at fixed Elo, and a moving estimate breaks that assumption silently.

The fixed-games-match mode (no `--sprt-*` and no rating-estimate frozen-anchor) is its own legal mode and is unaffected by this mutex.

### 4.3 Controller integration (`mod controller`)

The drain loop at `src/bin/elo-iterate.rs:5867` (run_iteration) gains a per-`PairComplete` LLR check. **The score buffer is per-worker (keyed by `worker_id`), not a single shared `Vec`** — this is load-bearing for `concurrency > 1` correctness; see the rejected-alternative note at the end of this subsection.

```rust
use std::collections::HashMap;

// At top of run_iteration: build the SPRT state.
let mut sprt_state = sprt::SprtState::default();
let sprt_cfg: Option<sprt::SprtConfig> = if let (Some(e0), Some(e1), Some(a), Some(b)) =
    (args.sprt_elo0, args.sprt_elo1, args.sprt_alpha, args.sprt_beta)
{
    Some(sprt::SprtConfig { elo0: e0, elo1: e1, alpha: a, beta: b })
} else {
    None
};

// Per-worker score buffer. The existing WorkerReport::GameComplete carries
// `game_index` but NOT `pair_index`; WorkerReport::PairComplete carries
// only `worker_id`. Each worker plays at most one pair at a time, so
// `worker_id` is a valid in-flight-pair key. The HashMap is sized at most
// `concurrency` entries (the in-flight-pair set) and its memory cost is
// trivial. Rejected alternative: add `pair_index` to both report variants
// — rejected because it inflates the WorkerReport enum surface for an
// invariant the controller already tracks via worker_id.
//
// Routing GameComplete back to its worker_id requires the controller to
// know which worker emitted the report. The current `WorkerReport::GameComplete`
// (verified at `src/bin/elo-iterate.rs:5460-5472`) does NOT carry `worker_id`;
// `WorkerReport::PairComplete` (line 5474) does. ELOH.E adds a `worker_id: u32`
// field to `GameComplete` as a one-field enum-variant extension. The worker
// code at line 5804 already has `worker_id` in scope (passed as a parameter
// to the `worker_thread_fn` signature at line 5539), so the producer side is a
// 1-line change. The consumer side at line 5970 destructures the new field
// and routes it into `pair_score_buffers`.
let mut pair_score_buffers: HashMap<u32, Vec<f64>> = HashMap::new();
```

**`WorkerReport::GameComplete` arm** (existing) gains:

```rust
if sprt_cfg.is_some() {
    pair_score_buffers
        .entry(worker_id)  // from the GameComplete report
        .or_default()
        .push(clawfish_score);
}
```

**`WorkerReport::PairComplete { worker_id } ` arm** drains the per-worker bucket and runs the LLR check:

```rust
if let Some(cfg) = sprt_cfg.as_ref() {
    let scores = pair_score_buffers.remove(&worker_id)
        .expect("PairComplete for worker without prior GameComplete entries");
    debug_assert_eq!(scores.len(), 2,
        "PairComplete worker_id={} drained {} scores, expected 2",
        worker_id, scores.len());
    let pair_score: f64 = scores.iter().sum();
    let verdict = sprt::update_pair(&mut sprt_state, cfg, pair_score);
    match verdict {
        sprt::SprtVerdict::Continue => { /* fall through, dispatch next */ }
        sprt::SprtVerdict::AcceptH0 => {
            terminating = true;
            stop_reason_override = Some(StopReason::SprtAcceptH0);
        }
        sprt::SprtVerdict::AcceptH1 => {
            terminating = true;
            stop_reason_override = Some(StopReason::SprtAcceptH1);
        }
    }
}
```

**Drain-on-shutdown: any worker_ids with residual buffers in `pair_score_buffers` after the run-end loop has converged are workers that emitted game A's `GameComplete` for the in-flight pair but where game B never completed (no `PairComplete` follow-up).** Each such residual buffer (size 1) is reported as a singleton via `sprt::discard_singleton(&mut sprt_state)` once per residual entry, then the buffer is dropped. This handles both the legitimate path (worker crash mid-pair → `WorkerReport::PairFailed` is sent and the buffer is left orphaned; controller's drain phase mops up) and the `--max-games` boundary case where an in-flight pair was never completed. Pinned by `singleton_counter_increments_on_orphaned_pair_buffer`.

**`MaxGamesReached` end-of-loop tail.** When the loop exits with `pairs_dispatched == total_pairs && all_workers_idle && !terminating`, and `sprt_cfg.is_some()`, the verdict carried into the run-end summary is `MaxGamesReached` (no SPRT bound crossed). Same `StopReason::MaxGames` enum variant as ELOH.B; the SPRT-vs-non-SPRT distinction is communicated by the presence of an `sprt:` line in the summary, not by a new `StopReason` variant.

**Why the `StopReason::SprtAcceptH0/H1` are new enum variants.** The existing `StopReason` (ELOH.B) is `{ Sigma, MaxGames }`; SPRT termination is structurally different (different α/β math, different downstream interpretation), so giving it dedicated variants keeps the formatter (`format_converged`) and downstream tooling unambiguous. Rejected alternative: reuse `StopReason::Sigma` for SPRT termination — rejected because σ-stopping and LLR-stopping are different statistical decisions and conflating them in the output would be a forensics hazard.

**Rejected alternative: a single shared `pair_buffer: Vec<f64>` across all workers.** Rejected by plan-review v2 must-fix #4: with `concurrency > 1`, two workers each appending a `clawfish_score` to the same `Vec` interleaved before either `PairComplete` arrives produces nonsense pair scores and silently corrupts the SPRT state. The mixed-TC SPRT campaign in the M4.D back-test runs at `concurrency=4` or higher; the bug would not be caught by the §6.3 single-threaded synthetic gate but would surface at the §7 Part 2 manual replay (probably as a wildly wrong pentanomial bin distribution). Per-worker keying (via `worker_id`) is the correct scope.

### 4.4 POV / candidate-vs-baseline mapping

The harness names two engines: `--engine` (the candidate / patch under test) and `--opponent` (the baseline). Per `src/bin/elo-iterate.rs:5867` and the existing rating-estimate flow, `clawfish_score` (or, more accurately, the *candidate-side score* — the field is named `clawfish_score` for historical reasons but holds the score of the engine identified by `--engine`) is the score from the `--engine` side's POV: 1.0 = candidate won, 0.5 = draw, 0.0 = candidate lost. SPRT's `+Δ Elo` therefore means *the candidate is stronger than the baseline*, matching fastchess's convention with `-engine` as the patch and `-engine` listed second as the baseline.

Script-side mapping (atomic with §3 `scripts/sprt.sh sprt|match` rewrite):
- HEAD (current-tree binary at `target/release/clawfish`) → `--engine`
- baseline-tag (cached at `target/sprt-baselines/<slug>/`) → `--opponent`
- For `match.sh self-play`: both sides are HEAD; assigning either to `--engine` is fine (the `Random_Seed` differentiates them). Convention: `Random_Seed=1` → `--engine`, `Random_Seed=2` → `--opponent`.
- For `match.sh vs-stockfish`: clawfish HEAD → `--engine`, Stockfish → `--opponent`.

Pinned by `pov_engine_is_candidate_in_sprt_verdict_sign` (a single integration test that asserts `summary.txt`'s SPRT verdict reports the candidate as the patch).

### 4.5 Summary extension

```rust
pub(crate) fn format_sprt_verdict(
    state: &sprt::SprtState,
    cfg: &sprt::SprtConfig,
    verdict: SprtVerdict,
) -> String;
// Format pinned to 3-decimal alpha/beta, 2-decimal LLR, exact integer pair
// counts. All three example outputs share the same field set so downstream
// awk parses are uniform; only the `verdict=` token varies.
//
// Output examples (Elo to 1 decimal — `0.0` not `0.000` — matching fastchess
// conventions; alpha/beta to 3 decimals; LLR to 2 decimals):
//   "sprt: verdict=H1 llr=2.95 elo0=0.0 elo1=10.0 alpha=0.050 beta=0.050 pairs=187 ptnml=[3,28,71,52,33]"
//   "sprt: verdict=H0 llr=-2.95 elo0=0.0 elo1=10.0 alpha=0.050 beta=0.050 pairs=187 ptnml=[1,55,78,42,11]"
//   "sprt: verdict=continue llr=1.23 elo0=0.0 elo1=10.0 alpha=0.050 beta=0.050 pairs=199 ptnml=[5,40,78,56,21]"

pub(crate) fn format_pentanomial_ci(state: &sprt::SprtState) -> String;
// Format pinned to 2-decimal Elo. CI bounds are always lower < upper;
// signs are explicit on all three Elo numbers (negative bounds get a `-`,
// positive bounds and the estimate get a `+`).
//
// Output:
//   "ci: elo=+41.85 [+18.33, +65.85] pairs=200"
// At pairs < 2 or degenerate variance: "ci: undefined (n=1)"
```

The SPRT verdict line is always emitted when `sprt_cfg.is_some()`, regardless of whether termination was via Wald-bound crossing or `--max-games`. The CI line is emitted whenever ≥2 pairs completed, regardless of mode (rating-estimate, fixed-games match, SPRT). Two-line emission preserves the ELOH.B `summary:` and `converged:` lines verbatim — only adds new lines.

## 5. Module boundaries

```
src/bin/elo-iterate.rs
    mod cli                      (--sprt-elo0/1/alpha/beta + --engine-option flags;
                                  post-loop SPRT mutex; CliError variants)
    mod sprt                     (NEW; SprtConfig, SprtState, SprtVerdict;
                                  wald_bounds, compute_llr, classify_pair_score,
                                  update_pair, pentanomial_ci, discard_singleton)
    mod controller               (per-PairComplete sprt::update_pair hook;
                                  pair_score_buffers HashMap keyed by worker_id;
                                  SprtAcceptH0/H1 termination paths)
    mod summary                  (format_sprt_verdict + format_pentanomial_ci)
    enum StopReason              (extended with SprtAcceptH0, SprtAcceptH1)
```

No new top-level `src/` files. ELOH.E's surface fits cleanly inside the existing submodule layout, same shape as ELOH.D's `mod prng` + `mod tc_sample` additions.

## 6. Test coverage strategy

### 6.1 `mod sprt::tests` (~70 LOC)

| Test | Asserts |
|---|---|
| `wald_bounds_at_alpha_beta_05` | `wald_bounds(0.05, 0.05)` returns `(B, A)` with `B ≈ -2.944` and `A ≈ 2.944`, both within 1e-9 of the analytic `log(0.05 / 0.95)` and `log(0.95 / 0.05)`. |
| `wald_bounds_asymmetric_alpha_beta` | `wald_bounds(0.01, 0.05)` returns the analytic `(log(0.05/0.99), log(0.95/0.01))`. Pins that the formula isn't accidentally symmetric. |
| `compute_llr_zero_at_indifference_midpoint` | Empty `pair_counts = [0,0,0,0,0]` returns 0.0 (degenerate guard). State with `mu = (s0_pair + s1_pair) / 2` (exactly between H0 and H1) yields LLR = 0.0 within 1e-9. Constructed analytically: pair-score that gives the midpoint. |
| `compute_llr_positive_when_sample_favors_h1` | State with `mu` significantly above `s1_pair / 2` (a winning streak) yields LLR > 0. Specifically `pair_counts = [0, 0, 0, 50, 50]` (all 1.5 or 2.0 outcomes) at H0=0 H1=10. Pin sign and rough magnitude. |
| `compute_llr_negative_when_sample_favors_h0` | State with `pair_counts = [50, 50, 0, 0, 0]` (all 0.0 or 0.5 outcomes) at H0=0 H1=10. Pin sign. |
| `compute_llr_zero_variance_returns_zero` | `pair_counts = [0, 0, 100, 0, 0]` (all draws): variance is 0; the function must return 0.0 without dividing by zero. |
| `pentanomial_ci_hand_computed_example` | M4.D-shape input `pair_counts = [5, 40, 78, 56, 21]` (200 pairs). Hand-computed: `sum(n·s) = 0+20+78+84+42 = 224`, `mu = 1.12`. `sum(n·s²) = 0+10+78+126+84 = 298`, `m2 = 1.49`. `sigma2 = 1.49 - 1.12² = 0.2356`. `SE = sqrt(0.2356/200) ≈ 0.03432`. `CI_pair ≈ [1.0527, 1.1873]`. Inverse-logistic with the `s_game = mu/2` step: `Elo_est = 400·log10(0.56/0.44) ≈ +41.85`, `Elo_lo ≈ +18.33`, `Elo_hi ≈ +65.85`. Assert all three Elo values within **±0.5 Elo absolute** of these targets — this is a deterministic computation, so the tolerance is tight (the only slack is f64 rounding through `log10`). The recomputed values match M4.D's historical fastchess output `+41.89 [+18.18, +65.61]` to ~0.3 Elo, which separately validates that fastchess and this implementation use the same logistic convention. |
| `compute_llr_pinned_value` | A non-degenerate state pinning the LLR formula end-to-end (catches a consistent factor-of-2 scale error in `s_i_pair = 2·LL(elo_i)` that the midpoint test cannot catch — see §13 tail-risk note). State: `pair_counts = [10, 10, 30, 25, 25]` at `elo0=0, elo1=10`. Hand-computed step-by-step: `mu = (0 + 5 + 30 + 37.5 + 50)/100 = 1.225`; `m2 = (0 + 2.5 + 30 + 56.25 + 100)/100 = 1.8875`; `sigma2 = 1.8875 - 1.225² = 0.386875`; `s0_pair = 2·LL(0) = 1.0`; `s1_pair = 2/(1 + 10^(-0.025)) ≈ 1.028766` (using `10^(-0.025) ≈ 0.944061`); `LLR = (s1−s0)·(2·mu − s0 − s1)/(sigma2/N)/2 = 0.028766 · 0.421234 / 0.00386875 / 2 ≈ 1.566`. Assert within **±0.005** of 1.566 (the tolerance absorbs f64 rounding through `powf` and `log10`). **A factor-of-2 bug in `s_i_pair` (dropping the leading `2·` so `s0_pair = 0.5, s1_pair ≈ 0.5144`) would shift LLR to ≈2.67 — far outside the ±0.005 band, so the test catches it. The midpoint test cannot catch this class of bug because its `LLR = 0` property is invariant under any consistent rescaling of `s_i_pair` and `mu`.** |
| `pentanomial_ci_minimum_sample_returns_nan` | `pair_counts = [0,0,1,0,0]` (1 pair): returns `(NaN, NaN, NaN)` — caller must guard. |
| `classify_pair_score_truth_table` | 9-row table covering all (game-A, game-B) score combinations from {0.0, 0.5, 1.0}², asserting the bin index per research report §4. Specifically: (0,0)→0, (0,0.5)→1, (0,1)→2, (0.5,0)→1, (0.5,0.5)→2, (0.5,1)→3, (1,0)→2, (1,0.5)→3, (1,1)→4. |
| `update_pair_continue_then_h1` | Build a stream of pair scores favoring H1; assert `Continue` until LLR crosses A, then `AcceptH1`. Uses synthetic data generated to deterministically cross — see §6.3 back-test gate for the seeded-Bernoulli version. |
| `update_pair_h0_path` | Same shape, stream favoring H0. |
| `discard_singleton_increments_only_audit_counter` | After 5 `discard_singleton` calls, `state.discarded_singletons == 5` and `state.pair_counts` and `state.llr` are unchanged. |

### 6.2 `mod cli::tests` (~15 LOC additions — leaner after `--engine-option` strike)

| Test | Asserts |
|---|---|
| `parse_args_sprt_all_four_flags_accepted` | `--sprt-elo0 0 --sprt-elo1 10 --sprt-alpha 0.05 --sprt-beta 0.05` parses; all four `args.sprt_*` fields are `Some` with the specified values. |
| `parse_args_sprt_partial_set_rejected` | `--sprt-elo0 0 --sprt-elo1 10` (alpha + beta missing) → `Err(InvalidValue("--sprt-elo0 requires all of …"))`. |
| `parse_args_sprt_alpha_out_of_range_rejected` | `--sprt-alpha 0.0` → Err; `--sprt-alpha 1.5` → Err. |
| `parse_args_sprt_with_k_update_rejected` | `--sprt-elo0 0 ... --sprt-beta 0.05 --k0 1.0` → Err. Pins the tightened §4.2 mutex (rejects *any* non-default K-update flag, not just `--k0 0 --target-sigma 0`). Variants: `--target-sigma 50` → Err (50 differs from the 30.0 default at `src/bin/elo-iterate.rs:233`, which the §4.2 mutex extracts as `TARGET_SIGMA_DEFAULT`); `--initial-elo 2000` → Err; `--stop-window 50` → Err (the actual ELOH.B default is 30, verify at extraction time). Each variant is one-liner-asserted. **Important:** every variant must use a value that genuinely differs from the extracted default const; otherwise the mutex's "is-default" check returns true and the test passes vacuously. The implementer extracts the default consts and threads them into both the mutex and the tests in the same commit. |
| `parse_args_sprt_with_rating_estimate_frozen_anchor_rejected` | `--sprt-elo0 0 ... --sprt-beta 0.05 --k0 0 --target-sigma 0 --initial-elo 1320` → Err (the rating-estimate frozen-anchor mode requires `--initial-elo` to be set non-default; combined with `--sprt-*` the mutex still fires). |
| `parse_args_no_sprt_flags_means_classic_mode` | Default invocation (no `--sprt-*`) parses; all four `sprt_*` fields are `None`. |

**`--engine-option` parsing already has `parse_args_engine_option_repeatable` at line 689; no additions for that surface.** Opportunistic addition: a `parse_args_engine_option_malformed_rejected` test (`--engine-option NoEqualsHere` → Err) tightens the existing test gap but is not strictly ELOH.E scope; included as a +5 LOC quality-of-life addition rather than a load-bearing gate.

### 6.3 Back-validation gate Part 1 — synthetic Bernoulli-pair stream (~30 LOC, in `mod sprt::tests`)

```rust
#[test]
fn sprt_back_test_h1_accept_at_known_elo_gap() {
    // Synthetic stream: pair scores drawn from a fixed Bernoulli-like
    // generator with known Elo gap. At H0=0 H1=10 alpha=beta=0.05 and
    // a true Elo gap of +20 (well inside the H1 acceptance region),
    // SPRT must converge to AcceptH1 in a bounded sample size.
    //
    // Generator: each pair is two independent Bernoulli draws with
    // p = LL(20.0) ≈ 0.5288. Pair score = sum of {0, 0.5, 1} per game
    // — but for a clean synthetic with known law we use the trinomial
    // extension: each game outcome is W with prob s, L with prob (1-s),
    // no draws. (This biases pairs toward the {0.0, 1.0, 2.0} bins,
    // skipping {0.5, 1.5}, which is mathematically valid for the GSPRT
    // formulation — pair-score variance is just smaller. The test pins
    // the verdict, not the sample size; tolerance on sample size is wide.)
    let mut prng = TestPrng::new(0xC1AB_F15A_E10D_5757);
    let cfg = SprtConfig { elo0: 0.0, elo1: 10.0, alpha: 0.05, beta: 0.05 };
    let mut state = SprtState::default();
    let p = 1.0 / (1.0 + 10f64.powf(-20.0 / 400.0)); // ≈ 0.5288
    for _ in 0..2000 {
        let g_a = if prng.next_f64() < p { 1.0 } else { 0.0 };
        let g_b = if prng.next_f64() < p { 1.0 } else { 0.0 };
        let pair_score = g_a + g_b;
        let verdict = update_pair(&mut state, &cfg, pair_score);
        if verdict == SprtVerdict::AcceptH1 {
            // Pass: converged to the correct verdict before the cap.
            return;
        }
        assert_ne!(verdict, SprtVerdict::AcceptH0,
            "SPRT must not falsely reject at +20 Elo true gap");
    }
    panic!("SPRT did not accept H1 within 2000 pairs at +20 Elo true gap");
}
```

Companion test `sprt_back_test_h0_reject_at_zero_elo_gap` for the negative case (true Elo gap = 0, expect AcceptH0). Both tests use the same `TestPrng` (SplitMix64 — already in `mod prng` from ELOH.D) seeded with a fixed value. Deterministic, fully in-tree, no subprocesses.

**Third synthetic stream — draw-heavy, pentanomial-vs-trinomial discriminator (~20 LOC).** Per plan-review v2 should-fix #4: the no-draws Bernoulli streams above under-test the pentanomial-specific math (in pure W/L the {0.5, 1.5} bins stay empty so the pair-correlation effect is degenerate, and a buggy trinomial-over-games implementation would also pass the gate). Add a third generator `sprt_back_test_drawheavy_h1_accept_at_known_elo_gap` that draws each game from a `{W: 0.30, D: 0.50, L: 0.20}` law (a +35 Elo gap with realistic 50% draw rate) and verifies AcceptH1 within the same 2000-pair cap. The middle pair-score bins ({0.5, 1.0, 1.5}) are well-populated under this law, so a trinomial-over-games implementation would mis-estimate the variance and either fail to converge or converge to the wrong verdict. Companion `sprt_back_test_drawheavy_h0_reject_at_zero_elo_gap` with `{W: 0.25, D: 0.50, L: 0.25}` (true 0 Elo, 50% draws).

The 2000-pair cap is wide enough to absorb sampling noise — the field-typical SPRT at α=β=0.05 against +20 vs 0 Elo converges in 200–800 pairs depending on noise. Pinning the sample size narrowly is the wrong gate (it would fail spuriously on a correct implementation that happened to take 850 pairs); pinning the verdict is the right gate.

### 6.4 `mod summary::tests` (~15 LOC additions)

| Test | Asserts |
|---|---|
| `format_sprt_verdict_h1_canonical_string` | State with bounds-crossed LLR and `Ptnml = [3,28,71,52,33]` → exact `"sprt: verdict=H1 llr=2.95 elo0=0.0 elo1=10.0 alpha=0.050 beta=0.050 pairs=187 ptnml=[3,28,71,52,33]"`. (LLR and pairs values are the test inputs, not derived; we're pinning the format.) |
| `format_sprt_verdict_h0_canonical_string` | Same, with verdict=H0 and a negative LLR. |
| `format_sprt_verdict_continue_at_max_games` | Verdict=continue (LLR didn't cross), terminated by max-games. The `verdict=continue` token communicates this; downstream tooling distinguishes by absence of verdict crossing. |
| `format_pentanomial_ci_two_decimal_elo` | `pentanomial_ci((5,40,78,56,21))` → format `"ci: elo=+41.85 [+18.33, +65.85] pairs=200"` (Elo to 2 decimal places; matches the §6.1 hand-computed values and the §4.5 example output). |
| `format_pentanomial_ci_undefined_at_n_lt_2` | `pair_counts = [0,0,1,0,0]` → `"ci: undefined (n=1)"`. |

### 6.5 `mod controller::tests` (~20 LOC additions, extends synthetic-pool fixtures)

| Test | Asserts |
|---|---|
| `sprt_mode_h1_synthetic_stream_accepts` | Synthetic `WorkerReport` feed of pair-by-pair `GameComplete` + `PairComplete` reports drawn from the §6.3 Bernoulli-pair generator. `args.sprt_elo0 = Some(0.0)` etc.; `args.max_games = 4000`. Run to completion via `run_iteration`; assert `outcome.stop_reason == StopReason::SprtAcceptH1`. |
| `sprt_mode_h0_synthetic_stream_rejects` | Same with H0 generator (true Elo gap = 0); assert `StopReason::SprtAcceptH0`. |
| `sprt_mode_max_games_no_verdict` | Synthetic feed of *exactly* +2 Elo gap (tightly inside the indifference zone); short `--max-games` cap forces `StopReason::MaxGames` before either bound; assert summary contains `verdict=continue`. |
| `sprt_with_rating_estimate_flags_rejected_at_parse` | (CLI test, but assert via the controller's contract) — `args.sprt_elo0 = Some(0.0)` AND `args.k0 = 0.0` AND `args.target_sigma = 0.0` → `parse_args` returns `Err`; the controller test never sees this combination. (Redundant with §6.2 but pins the contract end-to-end.) |
| `singleton_counter_remains_zero_in_normal_termination` | A complete SPRT run via synthetic feed (no abrupt interrupt) → `state.discarded_singletons == 0` at run end. Pins that the in-flight-pair-completion discipline from ELOH.B carries through. |
| `singleton_counter_increments_on_orphaned_pair_buffer` | Synthetic feed where a worker emits `WorkerReport::PairFailed` mid-pair (one `GameComplete` then a failure, no `PairComplete`). The drain phase observes a residual entry in `pair_score_buffers` and increments `discarded_singletons` by 1 per orphan. Pins the §4.3 drain-on-shutdown logic. |
| `pair_score_buffers_per_worker_under_concurrency` | Synthetic feed with two workers' interleaved pairs: GameA(worker=0), GameA(worker=1), GameB(worker=1)+PairComplete(worker=1), GameB(worker=0)+PairComplete(worker=0). The worker_id keying must keep the two pairs' scores separate; the SPRT-state pair_count vector after the feed must be the two correctly-classified pair scores, not a confused mix. Pins the §4.3 must-fix #4 design. |
| `pov_engine_is_candidate_in_sprt_verdict_sign` | Synthetic feed where the candidate-side score consistently exceeds 0.5 (pair scores favoring `--engine`); assert the resulting LLR is positive (per §4.4 the `--engine` side is the candidate, +Δ Elo means candidate stronger). Catches a sign-error in the candidate-vs-baseline POV mapping. |
| `match_pgn_concat_orders_by_game_index` | Synthetic feed of 4 games (2 pairs); concatenation step writes `<out-dir>/match.pgn` containing each game's `<out-dir>/games/<N>.pgn` byte-content separated by `\n\n`, in ascending `game_index` order. Pinned by golden-fixture comparison. |
| `match_pgn_concat_handles_zero_games` | Synthetic feed where the run terminates before any `GameComplete` (e.g. immediate engine-spawn failure); `match.pgn` is created and is empty (zero bytes). |

### 6.6 `#[ignore]`-gated end-to-end (~15 LOC, extends ELOH.A/B/D smoke tests)

| Test | Asserts |
|---|---|
| `end_to_end_sprt_clawfish_self_play_max_games_short` | `--engine clawfish --opponent clawfish --tc 1+0.05 --max-games 20 --concurrency 1 --sprt-elo0 0 --sprt-elo1 10 --sprt-alpha 0.05 --sprt-beta 0.05`. 20 games / 10 pairs run; summary contains `sprt: verdict=…` line. Verdict is almost certainly `continue` at 10 pairs (insufficient data to cross the bounds at α=β=0.05); the test only validates the code path doesn't crash. |
| `end_to_end_match_mode_with_engine_option` | `--engine clawfish --opponent clawfish --tc 1+0.05 --max-games 4 --concurrency 1 --engine-option Random_Seed=1 --opponent-option Random_Seed=2`. PGNs written, summary has `ci:` line, no `sprt:` line (mode is fixed-games match, not SPRT). Pins the `--engine-option` end-to-end plumbing. |

## 7. Back-validation gate

Two parts. Part 1 lands atomic with the unit; Part 2 is deferred to a post-merge manual run per ELOH.B/C/D precedent.

### Part 1 — synthetic Bernoulli-pair stream (in-tree, deterministic)

§6.3 above. The SPRT converges to the correct verdict at the correct sample size to within tolerance, given a synthetic stream with known Elo gap. Analogous to ELOH.B's `bernoulli_back_test_gate` for σ-stopping. Lands atomic with the unit; failure is a hard merge-blocker.

### Part 2a — math-deterministic gate (in-tree, atomic with the unit)

Per plan-review v2 should-fix #2, the original Part 2 conflated two different gates with very different tolerances. Splitting them:

The math gate is a unit test (not a subprocess run). Feed `pair_counts = [5, 40, 78, 56, 21]` (the historical M4.D bin counts) through `pentanomial_ci` directly and assert the result matches the historical fastchess CI `+41.89 [+18.18, +65.61]` to within **±0.5 Elo absolute on each of the three Elo numbers** (the only slack is f64 rounding through `log10`/`powf`). This is `pentanomial_ci_matches_m4d_historical_within_rounding` in §6.1 (folded into the existing `pentanomial_ci_hand_computed_example`'s assertion list — they're the same computation, both pinned at ±0.5 Elo). A failure here indicates a logistic-vs-bayesian convention mismatch or a factor-of-2 bug, not statistical noise. Lands atomic with the unit.

### Part 2b — replay gate (deferred manual run)

Re-run the M4.D mixed-TC SPRT through the new in-process SPRT mode and confirm statistical equivalence to the historical fastchess outcome.

**Configuration.** `baseline/alpha-beta-tt-killer-history` vs HEAD at the same TC distribution `10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1`, α=β=0.05, elo0=0 elo1=5 (M4.D's actual SPRT bounds), startpos-only, taskpolicy P-core pinning, `--virtual-clock`. Through `scripts/sprt.sh sprt baseline/alpha-beta-tt-killer-history` post-rewrite (i.e. via the harness, not via fastchess).

**Pass criteria.** This is a *fresh* run — different seeds, different scheduling, different RNG sources — so the gate is statistical, not deterministic.
- (a) Verdict is H1 accepted (matches the historical fastchess result).
- (b) The pentanomial counts and post-hoc Δ Elo CI lie in a credible band of the historical values `Ptnml(0-2) = [5, 40, 78, 56, 21]`, Δ Elo `+41.89 [+18.18, +65.61]`: within ±30% relative deviation on each bin count and within ±15 Elo on the CI endpoints. The math is exact for any given bin distribution (Part 2a pins that); Part 2b's ±15 Elo budget is for legitimate run-to-run variance from different sampling and scheduling at n=200 pairs.

**Why not bit-equivalence.** The harness uses its own SplitMix64 PRNG (ELOH.D); fastchess uses its own. Pair scheduling under N>1 concurrency is subprocess-scheduling-dependent. The TC sampler in ELOH.D advances in pair_index order pre-materialised at run start; fastchess's TC handling is structurally different (fastchess does not natively support TC distributions; M4.D used 4 separate fixed-TC matches with the union taken post-hoc, see `docs/plans/m4.d.md` and `docs/plans/eloh.d.md` for the migration history). Statistical equivalence is the strongest gate available; bit-equivalence is unattainable.

**Diagnostic ladder on Part 2b failure.**
- If verdict is H0 or `continue` (false reject or didn't converge): suspect LLR sign error in `compute_llr` or Wald-bound sign error in `wald_bounds`; cross-check Part 1 (synthetic Bernoulli) and Part 2a (math gate) verdicts. Pre-merge gates should have caught this; if Part 2b fails after Parts 1 and 2a passed, suspect the controller-level pair_index keying (§4.3) under concurrency.
- If verdict is H1 but pentanomial counts diverge by >30% on any bin: investigate `classify_pair_score` truth-table mismatch, color-pair ordering, or game-A/game-B convention drift. The §6.5 `pair_score_buffers_per_worker_under_concurrency` and `pov_engine_is_candidate_in_sprt_verdict_sign` tests should have caught most variants of this; if they didn't, suspect a TC-sampling / opening-book / scheduling interaction.
- If verdict is H1 but Δ Elo CI is far from the historical band (>±15 Elo): check whether the harness's pentanomial CI bin counts diverged from the historical bin counts (the math is deterministic given counts; if counts agree but CI diverges, that's a math gate failure that Part 2a should have caught pre-merge).

Archive verdict to `docs/research/tooling-elo-harness-validation.md` Part 1 + Part 2a (atomic) and Part 2b (post-manual-run) sections, extending ELOH.D's existing structure.

## 8. Sequencing

ELOH.E lands after ELOH.D on its own branch (default name `tooling/eloh-e-sprt`), branched off `main` after ELOH.D has merged. Doesn't share source surface with M5 (search-advanced) — `mod sprt` is binary-internal at `src/bin/elo-iterate.rs`, no engine surface change — so can run parallel with M5 plan-mode if the milestone clock asks for it.

**No M4 dependency.** ELOH.E is independent of M4.* milestones. Its first real consumer is the next mixed-TC SPRT campaign (likely M4.D Part 2 or M5.* search-tuning), at which point fastchess is no longer in the SPRT path. M4 phases that have already completed their SPRT campaigns against fastchess do not need re-running through ELOH.E.

**ADR sequencing.** ADR-0022 (or next available number) lands atomic with the unit. ADR-0012 amendment lands atomic with the unit (single commit; the amendment is pure prose addition + a status-line update).

## 9. Parallelization map

After this plan converges through review:

- **Slice A (math, pure):** `mod sprt` + §6.1 unit tests + §6.3 back-test gate. Pure new code; no I/O. Sonnet — bounded surface, well-defined math reference, no cross-cutting API change.
- **Slice B (CLI + integration):** `--sprt-*` flag parsing (`--engine-option` already shipped — no work needed) + `mod summary` formatters + controller hook (per-`PairComplete` LLR check + `pair_score_buffers: HashMap<u32, Vec<f64>>` keyed by `worker_id` + `WorkerReport::GameComplete` `worker_id` field-extension + new `StopReason` variants + `match.pgn` concatenation). Depends on A. Sonnet — the controller-hook surface is mechanical, the new `StopReason` variants are routine, the formatters mirror ELOH.B `format_progress`/`format_converged`. **No Opus override** — no novel domain-type contracts (the math is in Slice A), the controller's drain loop already has the per-pair-cadence pattern from ELOH.D's TC-sampling consumer.
- **Slice C (script rewrites):** `scripts/sprt.sh sprt|match` and `scripts/match.sh self-play|vs-stockfish` rewrites; remove fastchess locator from `sprt.sh`; narrow `match.sh`'s fastchess locator to the `compliance` arm. Depends on B (the CLI flags must exist before the script can pass them). Bash; Sonnet.
- **Slice D (docs + ADR):** doc-delta (architecture.md row, workflow.md SPRT section, tooling/elo-iteration-harness.md ELOH.E row + scope detail, tooling-backlog.md, roadmap.md, CLAUDE.md, bench/eloh-e.md), ADR-0012 amendment, new ADR-0022 (or next available number). Can run in parallel with A/B/C up until the ADR's number assignment (which happens at landing). Sonnet.

**Honest dependency shape:** A → (B, C, D — B precedes C; D parallel with A and B). Recommended in-practice shape: single coder-agent runs A → B → C → D sequentially (~450 LOC total, well within one Sonnet session); plan-mode and review-mode are the only forks. Two-agent fan-out is available but not recommended for the size — the coordination cost exceeds the wallclock savings.

## 10. Open questions resolved at landing

### (a) One binary or two?

**Resolution: keep `elo-iterate` as the single harness binary; add `--sprt-*` flags to select SPRT mode.** Rejected alternative: rename to `harness` and split modes into subcommands. Rejected because the rename has no strong rationale (existing `scripts/sprt.sh rating-estimate` invokes `elo-iterate` by name; renaming would break that contract for zero structural benefit). The flag-additive approach matches ELOH.B/C/D precedent (each phase added flags, not subcommands) and keeps the binary's invocation surface uniform across all run modes.

### (b) What flag spelling?

**Resolution: separate flags `--sprt-elo0 N --sprt-elo1 N --sprt-alpha F --sprt-beta F`.** Rejected alternative: parameterized `--sprt elo0=N,elo1=N,alpha=F,beta=F`. Rejected because the harness's existing convention (`--k0`, `--tau`, `--target-sigma`, `--initial-elo` — all separate flags) leans toward separate flags, not parameterized strings. The `--tc-sample` parameterized-string convention from ELOH.D is for a *list* (variable-length, repeating structure); the SPRT config is fixed-arity (always exactly four scalars), so the rationale for parameterization doesn't apply. Separate flags also play better with shell tab completion and with the `next_val!()` macro that the existing CLI parser uses. Naming closely mirrors fastchess's `-sprt elo0=… elo1=… alpha=… beta=…` form so operators familiar with the fastchess convention transfer cleanly.

### (c) `-pgnout` and `-log` parity

**Resolution: emit a combined `<out-dir>/match.pgn` at run end via concatenation as a small post-processing step in the harness.** Rejected alternative: document the format change (per-game files only, no combined file) and let downstream tooling adapt. Rejected because preserving downstream tooling that consumes a single PGN file is cheap (~10 LOC concatenation step, runs once at run end) and the cost of breaking that contract is dispersed across unknown consumers. Per-game files at `<out-dir>/games/<N>.pgn` continue to exist verbatim — the combined file is additive. No `-log` parity needed: the harness already emits its own structured `summary.txt`, which is what the downstream tooling actually consumes; fastchess's `-log` was for fastchess-internal debugging and has no clawfish consumer.

**Concatenation contract** (per plan-review v2 should-fix #3 — making the §10(c) commitment unambiguous):
- **Ordering:** ascending by `game_index` (the same ordering used in `<out-dir>/games/<N>.pgn` filenames). The PGN `Round` tag (one-indexed) is `(game_index / 2) + 1` per ELOH.A's existing convention, so this matches the natural pair-order interpretation.
- **Separator:** a single blank line (one `\n` after each game's terminating `*` or game-result-string token, then one extra `\n` before the next game's tag pair). PGN spec §3 requires "one or more blank lines" between games; one is the minimum and matches fastchess's output.
- **Termination handling:** the concatenation step runs in the run-end summary code path (after `format_summary` but before stdout flush), so it fires for *all* termination reasons — `Sigma`, `MaxGames`, `SprtAcceptH0`, `SprtAcceptH1`, panic-time controller cleanup. If the run terminates before any game completes (e.g. immediate engine crash), `match.pgn` is created empty. Pinned by `match_pgn_concat_orders_by_game_index` and `match_pgn_concat_handles_zero_games` (~10 LOC) in `mod controller::tests`.
- **LOC budget:** ~15 LOC of concatenation logic in `mod controller::write_match_pgn` plus 2 unit tests. Rolled into the §3 `src/bin/elo-iterate.rs` LOC line item; not separately broken out.

### (d) Pentanomial CI vs SPRT verdict

**Resolution: both reported in the final summary, distinct fields.** No rejected alternative — these are two separate statistics with different consumers (SPRT verdict for the gate decision; CI for documentation and follow-up regression analysis). Research report §5 is explicit that the CI applies post-hoc independent of the SPRT verdict; emitting both is the only sensible choice. The CI also applies in fixed-games match mode (no SPRT), so it's not redundant with the verdict.

### (e) K-update + fixed-games match mode coexistence

**Resolution: fixed-games match mode never uses K-update.** When `--max-games` is the only termination criterion (no `--sprt-*`, and rating-estimate frozen-anchor-mode-defaults of `--k0 0 --target-sigma 0`), the harness emits the post-hoc Δ Elo CI from the pair counts at run end, but does *not* track a streaming Elo estimate during the run. Callers wanting a streaming estimate use the rating-estimate mode (`--initial-elo X --k0 K --target-sigma 0` for fixed-anchor + streaming K-update against an anchor; or `--initial-elo X --k0 K --target-sigma S` for full online iteration with σ-stopping).

Rejected alternative (per plan-review v2 should-fix): allow K-update + fixed-games concurrently (no SPRT bound, just CI + streaming progress lines). Rejected because the existing rating-estimate mode (already shipped in ELOH.B) covers the streaming-estimate use case cleanly, and conflating it with fixed-games match mode adds CLI surface area for marginal value. The resulting four-mode taxonomy is:

1. **K-update / σ-stopping (rating-estimate, online):** `--k0 > 0`, `--target-sigma > 0`, `--initial-elo X`. Streaming Elo estimate, σ-stopping or `--max-games` termination.
2. **Frozen-anchor rating estimate:** `--k0 0 --target-sigma 0 --initial-elo X`. Fixed Elo anchor for the opponent (Stockfish UCI_Elo); CI at run end. `--max-games` termination only.
3. **Fixed-games match (no anchor, no K-update, no SPRT):** all K-update / σ-stopping flags at default. CI at run end. `--max-games` termination only. Used by `scripts/match.sh self-play|vs-stockfish` and `scripts/sprt.sh match`.
4. **SPRT:** `--sprt-elo0 ... --sprt-beta`, all K-update / σ-stopping flags forced default by mutex. CI + verdict at run end. LLR-bound or `--max-games` termination.

The §4.2 mutex covers all four modes' mutual exclusivity at parse time.

## 11. Doc-delta — atomic with landing

- `docs/architecture.md` — tournament-harness row updated; new SPRT-runner row.
- `docs/workflow.md` — SPRT section reworked: replace the "Run via `fastchess`" line with a `scripts/sprt.sh sprt baseline/<tag>` invocation through the harness; document the `--compliance`-stays-on-fastchess split as a one-paragraph subsection. The historical-commit-baseline methodology (lines ~360–397) stays as-is — only the *runner* inside the methodology changes, and `scripts/sprt.sh` is already the canonical entry point.
- `docs/tooling/elo-iteration-harness.md` — new ELOH.E sub-phase row in the §"Sub-phases" table after ELOH.D; new "ELOH.E scope detail" section between ELOH.D's section and "Branches and worktrees." The new section should follow the ELOH.D structure: In scope / Out of scope / Open questions / Back-validation gate / Doc-delta. Approximately 60 LOC.
- `docs/tooling-backlog.md` — close any sub-bullets that ELOH.E retires. Conservative grep at landing time; default zero retirements.
- `docs/roadmap.md` — mark ELOH.E in the in-flight column or done column depending on landing posture; add to the Tooling/fuzzing row.
- `CLAUDE.md` — status row updated.
- `docs/research/tooling-elo-harness-validation.md` — Part 1 result (chi-squared SPRT back-test) appended atomic with the unit; Part 2 (M4.D replay) appended post-manual-run.
- `bench/eloh-e.md` — new file; no-regression note + link to `bench/eloh-d.md` as load-bearing baseline. ELOH.E adds zero engine-side code paths; node count and NPS are byte-identical to ELOH.D.
- `docs/decisions/0012-tournament-harness.md` — amendment section "## 2026-05-01 amendment" + status header note. Retains §1 (fastchess as compliance runner), §3 (engine registry shape), supersedes §4 / §5 / §6 (output paths, adjudication, smoke contract — now harness-side).
- `docs/decisions/0022-eloh-sprt-mechanics.md` — new ADR. Consolidates: pentanomial-only (no trinomial fallback), logistic Elo (no `model=` flag, no BayesElo, no nElo), normal-approximation GSPRT (not exact MLE — research report §2.3 defends this for pool sizes ≥ 100), pair-cadence LLR check (research report §6 pitfall row 6), startpos-only opening (deferred-with-consumer-trigger), separate flags (not parameterized string). Number 0022 reserved at landing time; if 0022 is taken, next available.

**File-size growth observation (per plan-review v2 should-fix #6).** `src/bin/elo-iterate.rs` is currently 8682 lines after ELOH.D; ELOH.E adds ~165 prod LOC and ~145 test LOC, pushing it past 9000 lines. This is approaching the upper bound of comfortable single-file editing. Splitting the binary into a lib + bin (`src/elo_iterate/` library with submodules + `src/bin/elo-iterate.rs` thin entry point) is **deferred** to a future tooling iteration (ELOH.F or M5+ tooling work) — ELOH.E does not undertake the refactor because it would inflate the unit's diff and force test-site migrations across all of ELOH.A/B/C/D's existing tests. Tracked as a one-bullet item on `docs/tooling-backlog.md` at landing.

## 12. Verification checklist

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --release` (full suite — ELOH.A/B/C/D existing tests + ELOH.E's ~30 new unit + integration tests).
- `cargo llvm-cov --summary-only --lib --release`
- `cargo mutants --in-diff` on the unit's diff.
- `cargo deny check` — no new dependency, but the file changed; re-run.
- Bench: `cargo run --release --bin clawfish bench` matches `bench/eloh-d.md` byte-for-byte (node count + NPS), confirming zero engine-side perturbation. If the post-impl bench differs, that's a structural bug — treat as a hard merge-blocker.
- §6.3 SPRT back-test gate (`sprt_back_test_h1_accept_at_known_elo_gap` + `sprt_back_test_h0_reject_at_zero_elo_gap`) passes; pre-merge gate.
- §6.5 controller integration tests pass; pre-merge gate.
- Smoke run: `scripts/match.sh self-play` and `scripts/match.sh vs-stockfish` execute through the rewritten harness path and produce per-game PGNs + summary; `scripts/match.sh compliance` still passes through the unchanged fastchess path.

## 13. Sizing breakdown

- **`mod sprt`**: ~80 prod LOC (config struct + state struct + verdict enum + 6 pure functions, each 5–20 LOC).
- **CLI extensions**: ~20 prod LOC (4 SPRT flag arms + `*_DEFAULT` const extractions + tightened post-loop validation block; `--engine-option` reused unchanged).
- **Controller hook**: ~35 prod LOC (per-worker `pair_score_buffers: HashMap<u32, Vec<f64>>` keyed by `worker_id`, sprt_state init, per-`PairComplete` block, drain-on-shutdown singleton handling, new `StopReason` variants, summary emission, `match.pgn` concatenation step).
- **Summary formatters**: ~30 prod LOC (`format_sprt_verdict` + `format_pentanomial_ci` + their thread-through into the run-end emit).
- **Script rewrites**: ~60 LOC of bash (harness invocation blocks for `sprt`, `match`, `self-play`, `vs-stockfish`; removal of fastchess locator from `sprt.sh`; narrowing of `match.sh`'s locator to `compliance` arm).
- **Doc-delta**: ~80 LOC across the doc files listed in §11, plus ~80 LOC for the new ADR-0022 and ~30 LOC for the ADR-0012 amendment.
- **Tests**: ~75 LOC `mod sprt::tests` (12 unit tests including the new `compute_llr_pinned_value` and the §6.3 draw-heavy variants) + ~15 LOC `mod cli::tests` extensions (leaner after `--engine-option` strike) + ~15 LOC `mod summary::tests` extensions + ~25 LOC `mod controller::tests` extensions (including the new `singleton_counter_increments_on_orphaned_pair_buffer`, `pair_score_buffers_per_worker_under_concurrency`, `pov_engine_is_candidate_in_sprt_verdict_sign`, and the `match_pgn_concat_*` pair) + ~15 LOC `#[ignore]`-gated end-to-end = ~145 LOC total.

**Totals.** Prod: ~165 (binary) + ~60 (scripts) = ~225. Tests: ~145. Docs/ADR: ~190. Grand total ~560 LOC. Apply +30% contingency → ~728 LOC. Well under the workflow's 800-LOC ceiling. **§0 and §13 totals reconciled at v2 revision.**

**Tail-risk note.** The largest ELOH.E tail risk is LLR math correctness — a sign error in `compute_llr` or a factor-of-2 error in the pair-score-vs-game-score conversion would silently produce wrong SPRT verdicts. Per plan-review v2 must-fix #2, the layered defense is **not** symmetric across all bug classes; document which layer catches what:

| Bug class | Caught by |
|---|---|
| Sign error in `compute_llr` (LLR has wrong sign) | §6.1 `compute_llr_positive_when_sample_favors_h1` + `compute_llr_negative_when_sample_favors_h0` (immediate); §6.3 synthetic gate (immediate). |
| Sign error in `wald_bounds` (B and A swapped) | §6.1 `wald_bounds_at_alpha_beta_05` (immediate). |
| Factor-of-2 in `s_i_pair = 2·LL(elo_i)` (drop or duplicate the `2·`) | §6.1 `compute_llr_pinned_value` (immediate; LLR shifts to ≈2.67 outside the ±0.005 band). The midpoint test `compute_llr_zero_at_indifference_midpoint` does **not** catch this — its LLR=0 property is invariant under any consistent rescaling. The synthetic Bernoulli gate at +20 Elo would also fail (verdict converges to wrong sample size or wrong verdict), but slower than the pinned-value unit test. |
| `pentanomial_ci` factor-of-2 or inverse-logistic bug | §6.1 `pentanomial_ci_hand_computed_example` at ±0.5 Elo (immediate; pinned against the M4.D-historical CI to ~0.3 Elo agreement). |
| `classify_pair_score` truth-table mismatch | §6.1 `classify_pair_score_truth_table` (immediate, all 9 combinations). |
| Off-by-one in `pair_counts` indexing | §6.1 truth-table test catches index-off-by-one in classify; §6.5 `pair_score_buffers_per_worker_under_concurrency` catches per-pair routing errors. |
| LLR check fired per-game instead of per-pair (research §6 pitfall row 6) | §6.5 `sprt_mode_h1_synthetic_stream_accepts` would still pass (the test is verdict-only); the bug surfaces as wrong α/β realised rates, which the §7 Part 2b manual replay would expose via wider-than-expected verdict variance over multiple runs. **This is a layered-defense gap; mitigate by code-review of the controller's `WorkerReport::PairComplete` arm placement of the `update_pair` call.** |
| Per-worker pair_buffer aliasing under concurrency | §6.5 `pair_score_buffers_per_worker_under_concurrency` catches the obvious case; §7 Part 2b at `concurrency=4` is the integration back-stop. |
| POV / candidate-vs-baseline sign flip | §6.5 `pov_engine_is_candidate_in_sprt_verdict_sign` (immediate). |

A single bug from the rows above should not pass all listed layers; layered defense is intentional. The one acknowledged gap (LLR per-game vs per-pair cadence) is mitigated by code review and by §7 Part 2b's wider-than-expected verdict-variance signal.

## Appendix — branches and worktrees

ELOH.E lands on a new branch `tooling/eloh-e-sprt`, branched off `main` after ELOH.D has merged. Worktree path: `/Users/alex/clawfish-eloh-e` (parallel to the existing `/Users/alex/clawfish-elo-harness` worktree which holds the merged ELOH.A/B/C/D chain). Per the user's standing directive on worktree management, the existing `clawfish-elo-harness` worktree is for the merged state; new work goes in its own worktree.

ADR numbers (0022 for the new SPRT-mechanics ADR) are reserved at landing time, not pre-allocated. If 0022 is taken by a parallel landing (unlikely but possible), the next available number is used and the ADR file is renamed before commit. The ADR-0012 amendment is in-place (no new number).
