# ELOH.D — Mixed-TC sampling

The harness's mixed-TC layer. Adds per-pair time-control sampling so a single run can play games drawn from a discrete weighted TC distribution, enabling true mixed-TC SPRT under the redefined game and per-(TC, W/L/D) data emission for downstream Δ(TC) regression. Closes the ELOH milestone.

Spec source: `docs/tooling/elo-iteration-harness.md` ELOH.D section (lines 152-195) and ELOH.D row at line 55. Validation precedent: ELOH.A's color-paired match loop; ELOH.B's `sigma::tests`-private `Xorshift64` (which we replace with a production-grade SplitMix64). No new ADR — the TC-spec grammar and per-pair sampling cadence are parameter-level decisions defended in the spec body. `docs/architecture.md` carries no engine-side change.

## 0. Sizing note

Estimated total: ~120 prod LOC + ~80 test LOC = ~200, dropping from v1's ~245 after the v1-review must-fix to drop the speculative `opening` sub-stream and `Prng::fork` (CLAUDE.md ground rule: don't design for hypothetical future requirements). Well under the workflow's 300-800-LOC typical band.

## 1. Goals

- New CLI flag `--tc-sample <SPEC>` accepting a discrete weighted distribution `<TC>:<weight>(,<TC>:<weight>)*`. Mutually exclusive with `--tc` at parse time.
- Per-pair TC sampling: harness draws one TC per color-pair; both color-swapped games of the pair use the same sampled TC (preserves ELOH.A's "fair experiment at one TC" invariant).
- Single seeded master PRNG `--seed N` controlling TC sampling (introduced in ELOH.D — spec line 74 referenced it for ELOH.A but it was never wired). Default seed = `0xC1AB_F15A_E10D_D000_u64` (runs without `--seed` are bit-deterministic). Single-purpose: only TC sampling consumes it. **Spec line 160 said `--seed` controls both opening selection AND TC sampling via sub-stream derivation; we drop opening selection from this scope** — opening positions are still always startpos in ELOH.D, so opening selection has no consumer. When a future opening-book consumer lands, it will introduce labelled-stream forking as part of its own plan; an ELOH.D-only run with the same `--seed` will produce identical TC-sample sequences regardless of how a future opening-randomization layer derives its own randomness (e.g. via an additional flag like `--opening-seed` or a labelled fork from `--seed`).
- `--seed N` accepts both decimal (`--seed 42`) and `0x`-prefixed hex (`--seed 0xDEADBEEF`) — explicit parser path detailed in §4.3 since the existing `s.parse::<u64>()` is decimal-only.
- Stockfish-side TC compatibility: when `--opponent-tc-override <TC>` is set with `--tc-sample`, opponent uses the override regardless of the sampled TC; only clawfish's TC varies per pair. When override absent, both engines use the same sampled TC.
- PGN per-game annotation: existing `PgnHeader.time_control` field already exists (line 3546); the controller emits the *sampled* TC string, not the static `args.tc`, when `--tc-sample` is active.
- Per-game summary line gains a `tc=<base>+<inc>` field; aggregate emission at run end adds a `summary-by-tc:` line with per-TC W/L/D, in input-distribution order.
- K-update path is unchanged — game outcomes remain i.i.d. under the mixed game; only the data-emitter side changes. `--tc-sample` is compatible with both rating-estimate (`--k0 0 --target-sigma 0`) and online-σ modes. **Mixed-mode rating semantics:** under `--tc-sample`, the resulting Elo number is the rating of the mixed game; for per-TC ratings, run separate fixed-TC sessions. Documented in `--help` for `--tc-sample` to prevent users misreading the aggregate as a single-TC rating.
- Back-validation gate: Part 1 chi-squared sampler test (in-tree, fully reproducible); Part 2 degenerate single-TC mix self-back-test (deferred to post-merge manual run, ELOH.B/ELOH.C precedent).

## 2. Out of scope

- Continuous TC distributions (`uniform:`, `loguniform:`). Defer until a real consumer asks.
- Per-game TC asymmetry (different white/black TC). Methodologically suspect for SPRT; defer indefinitely.
- Live curve-fitting / scatterplot output. Harness emits data; analysis is downstream.
- TC-adaptive aspiration negotiation (engine-side; tracked separately under ML/parametric aspiration).
- Mixed-game SPRT verdict computation, Δ(TC) regression fit, confidence-band visualisation. Downstream tooling consumes the per-(TC, W/L/D) data the harness emits.
- `--tc-sample-per-game` (color-pair-broken sampling cadence). Spec open question 3 — defer; revisit if a consumer asks.

## 3. Files modified

| File | Change | LOC est |
|---|---|---|
| `src/bin/elo-iterate.rs` | New sub-modules `prng` (SplitMix64 + sub-stream derivation) and `tc_sample` (`TcDistribution` parser + sampler). Existing modules extended: `cli` (new `--tc-sample` + `--seed` flags + post-loop mutex with `--tc`), `summary` (`SummaryLine.tc` field, new `format_summary_by_tc`), controller (per-pair sample in bootstrap + drain-loop redispatch; PGN/summary use sampled TC; per-TC bucket aggregation). `WorkerCmd::PlayPair` gains `engine_tc` + `opponent_tc`; `WorkerReport::GameComplete` gains `tc: TimeControl`; `WorkerConfig` loses static `tc` and `opponent_tc` fields (`opponent_tc_override` retained as optional fallback for `--tc` mode). | +150 prod / +95 tests |
| `docs/tooling/elo-iteration-harness.md` | ELOH.D row → done; scope detail → "Done" prose with actual landing size; cross-link Part 1+2 results; ELOH milestone closed banner. | re-state |
| `docs/tooling-backlog.md` | Sub-bullet 8 ("Per-game TC sampling for mixed-TC SPRT") of the harness entry → "Done." | re-state |
| `docs/workflow.md` | New short subsection "Mixed-TC SPRT" under "Online Elo iteration," documenting `--tc-sample`, the redefined-game framing, joint `--seed` semantics. | +25 |
| `docs/research/tooling-elo-harness-validation.md` | Append "ELOH.D Part 1 — sampler chi-squared" and (post-manual-run) "ELOH.D Part 2 — degenerate single-TC self-back-test" sections. Part 1 lands atomic; Part 2 is follow-up. | +40 (Part 1) |
| `docs/roadmap.md` | ELOH milestone → closed (all four sub-phases pass). | +3 |
| `CLAUDE.md` | Status table: ELOH milestone closed row added. | +2 |
| `bench/eloh-d.md` | New milestone bench file. ELOH.D adds no engine-side code path, so `bench` numbers are unchanged from ELOH.C; the file documents the no-regression observation and links to `bench/eloh-c.md` as the load-bearing baseline. | new ~15 |
| `.cargo/mutants.toml` | Anticipated: `tc_sample::sample`'s cumulative-bucket scan, `prng::next_split` mixer constants, `format_summary_by_tc`'s join order. Survivor-driven; default zero new entries. | +0..10 |

## 4. Type definitions and key signatures

### 4.1 `mod prng` (new, `src/bin/elo-iterate.rs`)

```rust
//! SplitMix64 PRNG for `--seed`-driven TC-sampling reproducibility.
//!
//! ELOH.D uses a single u64 seed → single SplitMix64 stream consumed by
//! `tc_sample::TcDistribution::sample`. Hand-rolled (~20 LOC); no `rand`
//! crate dep. Mixer constants are pinned by a golden-fixture test
//! (`prng_seed_zero_first_three_words_golden`) so a transcription typo
//! fails at compile-time-of-test.

#[derive(Debug, Clone, Copy)]
pub(crate) struct Prng(u64);

impl Prng {
    /// Construct from a u64 seed. Runs one SplitMix64 mix step so a seed
    /// of 0 doesn't yield a 0-state pathology.
    pub(crate) fn new(seed: u64) -> Self;

    /// SplitMix64 next. Standard algorithm: state += GOLDEN_GAMMA;
    /// z = state; z = (z ^ (z >> 30)) * MIX_C1;
    /// z = (z ^ (z >> 27)) * MIX_C2; z ^ (z >> 31).
    pub(crate) fn next_u64(&mut self) -> u64;
}

/// Default seed when `--seed` is absent. Intentionally non-zero. Documented
/// in `--help` so users know no-`--seed` runs are still bit-deterministic.
pub(crate) const DEFAULT_SEED: u64 = 0xC1AB_F15A_E10D_D000;
```

### 4.2 `mod tc_sample` (new, `src/bin/elo-iterate.rs`)

```rust
//! `--tc-sample <SPEC>` parsing + cumulative-bucket sampling.

#[derive(Debug, Clone)]
pub(crate) struct TcDistribution {
    /// Parsed (TC, weight) entries in input order. Weights are positive.
    pub entries: Vec<(super::cli::TimeControl, u32)>,
    /// Prefix sums of weights; len == entries.len(); strictly increasing;
    /// last element == total.
    cumulative: Vec<u32>,
    /// Sum of all weights. Sampling draws u64 then mods into `total`
    /// (rejection-free: the spec guarantees u32 weights, so total fits u32
    /// and the modulo bias from u64 is below 1 part in 2^32).
    total: u32,
}

impl TcDistribution {
    /// Sample one TC. Draw `r = (rng.next_u64() % total)`, find first
    /// cumulative bucket strictly greater than `r`, return its TC.
    /// Linear scan — entries.len() expected ≤ ~10 in practice.
    pub(crate) fn sample(&self, rng: &mut super::prng::Prng) -> super::cli::TimeControl;

    /// Iterate (TC, weight) pairs in input-spec order. Used by the
    /// `summary-by-tc:` aggregator to preserve user-visible ordering.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &(super::cli::TimeControl, u32)>;
}

/// Parse `<TC>:<weight>(,<TC>:<weight>)*`. Each `<TC>` via `cli::parse_tc`;
/// `<weight>` is a u32 in `1..=u32::MAX`. At least one entry required.
/// Empty input, zero weight, weight overflow on summing, or repeated TC
/// keys all yield Err.
///
/// Repeated TC keys are an error (not silently merged): the spec writes
/// `--tc-sample 10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1` with distinct TCs;
/// a typo like `10+0.1:1,10+0.1:2` likely indicates user confusion and
/// should fail loudly.
pub(crate) fn parse_tc_sample(s: &str) -> Result<TcDistribution, super::cli::CliError>;
```

### 4.3 CLI extensions (`mod cli`)

```rust
pub(crate) struct Args {
    // existing ELOH.A/B/C fields...
    /// `--tc-sample` distribution. Mutually exclusive with `--tc`; exactly
    /// one of `args.tc` / `args.tc_sample` is `Some` after parse_args.
    pub tc_sample: Option<super::tc_sample::TcDistribution>,
    /// `--seed N`. Optional; when `None`, the harness uses
    /// `prng::DEFAULT_SEED`. Currently consumed only by `--tc-sample`'s
    /// per-pair sampler. When a future opening-randomization consumer
    /// lands, the docs will pin whether `--seed` extends to it or whether
    /// a separate flag is added.
    pub seed: Option<u64>,
}

// `args.tc` becomes `Option<TimeControl>` (was non-optional). Post-parse
// validation enforces "exactly one of --tc / --tc-sample is set."
pub tc: Option<TimeControl>,
```

**Hex-prefix parsing for `--seed` (load-bearing).** Existing `s.parse::<u64>()` is decimal-only. Add a small helper:

```rust
fn parse_u64_seed(s: &str) -> Result<u64, CliError> {
    let v = if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(rest, 16)
    } else {
        s.parse::<u64>()
    };
    v.map_err(|_| CliError::InvalidValue(format!("--seed: not a valid u64: {s}")))
}
```

Used in the `"--seed" => { seed = Some(parse_u64_seed(next_val!())?); }` arm.

**Post-loop validation**, immediately before the existing `--k0 0 requires --target-sigma 0` check at line 457:

```rust
match (tc.is_some(), tc_sample.is_some()) {
    (true,  true)  => return Err(CliError::InvalidValue(
        "--tc and --tc-sample are mutually exclusive".into())),
    (false, false) => return Err(CliError::MissingFlag(
        "one of --tc or --tc-sample".into())),
    _ => {}
}
```

**No-equals convention** (per ELOH.C `--virtual-clock=` precedent at line 435): `--tc-sample=foo` and `--seed=42` rejected. Both flags use the existing `next_val!()` macro pattern from line 220. The plan does NOT add new `--*=value` rejection arms — they're caught by the trailing `other => UnknownArg(...)` in the existing match (the equals form would fall through unrecognised, since neither `--tc-sample=` nor `--seed=` is matched). Documented in `--help` for both flags.

### 4.4 WorkerCmd / WorkerReport / WorkerConfig (`mod controller`)

```rust
pub(crate) enum WorkerCmd {
    PlayPair {
        pair_index: u32,
        opponent_uci_elo: u32,
        /// Sampled per-pair TC for clawfish. When `--tc` is set (no
        /// sampling), the controller passes `args.tc` here for both games
        /// of the pair. The worker uses the value verbatim — sampling
        /// ownership lives in the controller.
        engine_tc: super::cli::TimeControl,
        /// Per-pair TC for the opponent. Equal to `engine_tc` when
        /// `--opponent-tc-override` is absent; equal to the override
        /// when it is set.
        opponent_tc: super::cli::TimeControl,
    },
    Quit,
}

pub(crate) enum WorkerReport {
    GameComplete {
        // existing fields...
        /// The TC the *engine* (clawfish) played at. Used by the
        /// controller's per-game persistence (PGN TimeControl tag,
        /// summary line, per-TC W/L/D bucket lookup).
        tc: super::cli::TimeControl,
    },
    PairComplete { worker_id: u32 },
    Failure(String),
}

pub(crate) struct WorkerConfig {
    // existing fields... but `tc` and `opponent_tc` REMOVED.
    // The per-pair TC arrives via WorkerCmd::PlayPair. WorkerConfig
    // retains static config (engine_spec, opponent_spec, options, mode,
    // harness_overhead_ms, watchdog, max_plies, thresholds, virtual_clock).
}
```

Removing the `tc`/`opponent_tc` fields from `WorkerConfig` (rather than keeping them as fallback) is the cleaner shape: the worker now has a single source-of-truth per pair (the `WorkerCmd::PlayPair` payload). `--tc` mode just passes `args.tc.unwrap()` for every pair from the controller — bookkeeping symmetry with `--tc-sample` mode at zero per-pair cost.

**Prerequisite: `TimeControl` derives `PartialEq, Eq`.** Currently `src/bin/elo-iterate.rs` line 116-122 derives only `Debug, Clone, Copy`. The §4.6 per-TC bucket lookup (`buckets.iter().position(|b| b.tc == report.tc)`) and the §6.5 reproducibility test (Vec equality) both require structural equality. Add `PartialEq, Eq` to the derive list as part of Slice A.

**Worker-side dataflow change (production_worker_fn, lines 4805-4922).** Concretely, the per-pair `(white_tc, black_tc)` build at lines 4866-4870 changes from:
```rust
let (white_tc, black_tc) = if clawfish_white {
    (cfg.tc, cfg.opponent_tc)            // before: read from WorkerConfig
} else {
    (cfg.opponent_tc, cfg.tc)
};
```
to read from the destructured `WorkerCmd::PlayPair` payload:
```rust
let (white_tc, black_tc) = if clawfish_white {
    (engine_tc, opponent_tc)             // after: per-pair from cmd
} else {
    (opponent_tc, engine_tc)
};
```
And the `match_loop::GameContext` builder at lines 4894-4909 sets `engine_tc: engine_tc` and `opponent_tc: opponent_tc` (per-pair) instead of `engine_tc: cfg.tc, opponent_tc: cfg.opponent_tc`. The `GameContext` struct itself (line 4117) already takes the values as fields — no struct change needed; only the source of the values shifts.

**`WorkerReport::GameComplete` emission** at line 4914 gains `tc: engine_tc` so the controller's per-game persistence can pull it.

**Exhaustive-destructure sites (Slice B sub-step — verified against current source).** The following three sites destructure `WorkerCmd::PlayPair` exhaustively and must update their patterns to bind (or `..`-skip) the new `engine_tc` + `opponent_tc` fields:
- **Line 4808-4811** (production worker's recv loop, `WorkerCmd::PlayPair { pair_index, opponent_uci_elo } => { ... }`): MUST bind `engine_tc` and `opponent_tc` — these are the values the per-pair build at line 4866 reads. This is the load-bearing site.
- **Line 5339-5346** (`mod controller::tests` synthetic-pool fixture A's recording closure, exhaustive destructure for log replay): `..`-skip the new fields if the test doesn't inspect them, or bind them if it does.
- **Line 5663-5667** (synthetic-pool fixture B's recording closure): same shape as 5339-5346.

Sites that already use `..` continue to compile unchanged and need no edit: lines 5268, 5391, 5423, 5732, 5834. (v1 of this plan incorrectly listed 5423 and 5732 as exhaustive — they were already wildcarded; v2 reviewer caught this.) Slice B's prompt MUST list the three exhaustive-destructure sites above (4808, 5339, 5663) so the implementer doesn't miss the worker's own binding site at 4808 — without that bind, the dataflow change at line 4866 doesn't compile.

### 4.5 Sampler ownership in the controller — pre-materialised TC sequence

**Pre-materialise all TCs at run start, indexed by pair_index.** v1's plan sampled at dispatch time from a single shared stream, and v2's plan-review caught that under N>1 concurrency the *call order* to `sample_pair_tcs` depends on which worker reports PairComplete first — so the `pair_index → engine_tc` mapping wasn't actually deterministic. Pre-materialisation fixes this structurally.

```rust
// In run_iteration, before the bootstrap loop:
let mut tc_rng = prng::Prng::new(args.seed.unwrap_or(prng::DEFAULT_SEED));

// Pre-materialise all per-pair TCs indexed by pair_index. This is the single
// place sampler RNG state advances; below, dispatch reads from the vector
// rather than calling the sampler. Memory cost: 8 bytes per pair × total_pairs;
// at 5000 pairs that's 40 KB — negligible.
let pair_tcs: Vec<(cli::TimeControl, cli::TimeControl)> = (0..total_pairs)
    .map(|_| {
        let engine_tc = match &args.tc_sample {
            Some(dist) => dist.sample(&mut tc_rng),
            None       => args.tc.expect("post-parse: exactly one of tc/tc_sample set"),
        };
        let opponent_tc = args.opponent_tc_override.unwrap_or(engine_tc);
        (engine_tc, opponent_tc)
    })
    .collect();
```

Both bootstrap (line 5008) and drain-loop redispatch (line 5145) read from `pair_tcs[pair_index as usize]` instead of calling a sampler closure. **Result:** `pair_index → engine_tc` is now genuinely a function (deterministic given args + seed), independent of subprocess scheduling or worker completion order under any concurrency level. The §6.5 `seed_reproducibility` test asserts equality of `engine_tc` sequences (recorded by the synthetic-pool fixture's command log, ordered by `pair_index`) across two runs.

**Trade-off articulated.** Under σ-stopping (the online-σ rating-estimate's common case), the controller terminates before all `total_pairs` games play; the tail of `pair_tcs` past the termination index is materialised but unused. The over-sampling is the cost paid for sampler advance being independent of subprocess scheduling — and is also why the up-front loop computes `Vec<(TimeControl, TimeControl)>` rather than reading from a streaming sampler at dispatch time.

### 4.6 Per-TC bucket aggregation path

**Buckets exist only under `--tc-sample`.** Under `--tc` mode (no sampling), the controller skips bucket maintenance entirely and `summary-by-tc:` is not emitted — no extra output for users who didn't opt into mixed-TC.

Under `--tc-sample`, the controller builds the parallel `Vec<TcBucket>` once at run start (right after the §4.5 `pair_tcs` materialisation), with one entry per `dist.iter()` entry, ordered by input-spec order. In the `WorkerReport::GameComplete` arm (line 5042-5110), after the existing global `wins/losses/draws` counter update, look up the bucket index by `buckets.iter().position(|b| b.tc == report.tc)` (linear scan on ≤ ~10 entries — fine) and increment the appropriate `wins/losses/draws` field on `&mut buckets[idx]`. **Do NOT re-scan summary.txt at run end** — that path is fragile across format changes and confuses the data-emit invariant.

At run end (right after the existing `converged:` line emission at line 5193-5204), emit `summary-by-tc:` only when `args.tc_sample.is_some()`. Format via `format_summary_by_tc(&buckets)`; print to stdout; append to summary.txt.

### 4.7 Summary extension

```rust
pub(crate) struct SummaryLine {
    pub game_index: u32,
    pub white: String,
    pub black: String,
    pub result: String,
    pub plies: u32,
    pub termination: String,
    /// NEW. Format `<base>+<inc>` matching `format_tc`. Always Some in
    /// ELOH.D; `Option<String>` lets ELOH.A/B fixtures stay valid in tests.
    pub tc: Option<String>,
}

/// Per-TC W/L/D bucket; ordered by input spec. Built incrementally in
/// the controller's drain loop alongside the global wins/losses/draws
/// counters.
pub(crate) struct TcBucket {
    pub tc: super::cli::TimeControl,
    pub wins: u32,
    pub losses: u32,
    pub draws: u32,
}

pub(crate) fn format_summary_by_tc(buckets: &[TcBucket]) -> String;
// Output: `summary-by-tc: 10+0.1: W=110 L=95 D=45 (250)  20+0.2: W=...`
//                                                       ^^ two spaces between bucket entries.
// When buckets.len() == 1 the line is still emitted (degenerate single-TC
// mix) — preserves the invariant that summary-by-tc is present iff
// --tc-sample was active. --tc mode emits no summary-by-tc line.
```

The aggregate is appended to `summary.txt` after the existing `converged:` line and printed to stdout (open question (b): default both — it's two writes for terser-vs-richer output, the cost is negligible). Bucket ordering follows `TcDistribution::iter()` (input-spec order — preserves user intent and is deterministic; spec didn't say but defaults to this).

## 5. Module boundaries

```
src/bin/elo-iterate.rs
    mod cli                      (--tc-sample + --seed flags; tc → Option;
                                  post-loop mutex; CliError variants)
    mod prng                     (NEW; SplitMix64 + Prng newtype + DEFAULT_SEED)
    mod tc_sample                (NEW; TcDistribution + parse_tc_sample)
    mod controller               (per-pair sampler at bootstrap + drain;
                                  per-TC bucket build; format_summary_by_tc emit)
    mod summary                  (SummaryLine.tc field; format_summary_by_tc)
    mod pgn                      (untouched; existing time_control field carries the sampled TC)
    WorkerCmd / WorkerReport / WorkerConfig (controller-internal API shape change)
```

No new top-level `src/` files. ELOH.D's surface fits cleanly inside the existing `elo-iterate.rs` submodule layout.

## 6. Test coverage strategy

### 6.1 `mod prng::tests` (~12 LOC)

| Test | Asserts |
|---|---|
| `prng_zero_seed_yields_nonzero_first_word` | `Prng::new(0).next_u64()` is non-zero (the constructor's mix step ensures a 0 seed isn't a 0-state). |
| `prng_deterministic_across_constructions` | Two `Prng::new(42)` produce identical first 100 u64s. |
| `prng_distinct_seeds_yield_distinct_streams` | `Prng::new(42)` and `Prng::new(43)` first 100 u64s differ. |
| `prng_seed_zero_first_three_words_golden` | Hardcoded golden fixture: `Prng::new(0)`'s first three `next_u64()` outputs match pre-computed expected values. Catches accidental mixer-constant typos at compile-time-of-test. |

### 6.2 `mod tc_sample::tests` (~25 LOC + the chi-squared back-test gate, ~25 LOC)

| Test | Asserts |
|---|---|
| `parse_single_entry` | `"10+0.1:1"` ⇒ entries `[(10s+0.1s, 1)]`, total 1. |
| `parse_four_entries_uniform` | `"10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1"` ⇒ four entries, total 4, cumulative `[1,2,3,4]`. |
| `parse_three_to_one_skewed` | `"10+0.1:3,60+0.6:1"` ⇒ entries `[(10s+0.1s, 3), (60s+0.6s, 1)]`, cumulative `[3,4]`, total 4. |
| `parse_rejects_empty` | `""` ⇒ Err. |
| `parse_rejects_zero_weight` | `"10+0.1:0"` ⇒ Err. |
| `parse_rejects_repeated_tc` | `"10+0.1:1,10+0.1:2"` ⇒ Err with "duplicate TC" message substring. |
| `parse_rejects_malformed_weight` | `"10+0.1:abc"` ⇒ Err. |
| `parse_rejects_missing_colon` | `"10+0.1"` (no weight) ⇒ Err. |
| `parse_rejects_weight_overflow` | total exceeds u32::MAX ⇒ Err. |
| `sample_single_entry_always_returns_it` | 1-entry distribution + 1000 draws ⇒ all draws return the single entry. |
| `sample_skewed_3to1_at_seed_xfeed_yields_known_counts` | **Back-validation gate Part 1.** Distribution `[(A, 3), (B, 1)]`; seed `0xC1AB_FEED`; 1000 draws; assert exact counts (e.g. `(744, 256)` — pre-computed at test-write time after the mixer constants are pinned). The test is *fully deterministic* given the fixed seed + mixer constants — pinning the exact counts beats a chi-squared CI bound (which has a per-test false-positive rate). A bug that shifts the distribution will fail the exact-count assertion regardless of whether it's still inside the chi-squared CI. Counterpart test `sample_uniform_4_bucket_at_seed_xfeed_yields_known_counts` for the 4-bucket uniform shape, also pinning exact counts. Both tests must compute χ² against expected and `eprintln!` the value as a side observable, so a future seed-or-constant change can verify the new counts are still chi-squared-plausible (χ² < 11.345 for 3 dof; χ² < 6.635 for 1 dof) before pinning new exact-count fixtures. |
| `sample_uniform_4_bucket_input_order_preserved_in_iter` | After parsing `A:1,B:1,C:1,D:1`, `dist.iter()` yields `(A,1),(B,1),(C,1),(D,1)` in that order. |

### 6.3 `mod cli::tests` (~15 LOC)

| Test | Asserts |
|---|---|
| `parse_args_tc_sample_only_accepted` | `--tc-sample 10+0.1:1,20+0.2:1` (no `--tc`) parses; `args.tc.is_none() && args.tc_sample.is_some()`. |
| `parse_args_tc_only_accepted` | `--tc 10+0.1` (no `--tc-sample`) parses; `args.tc.is_some() && args.tc_sample.is_none()`. (Pins backwards compatibility.) |
| `parse_args_both_tc_and_tc_sample_rejected` | Both set ⇒ `Err(InvalidValue("--tc and --tc-sample are mutually exclusive"))`. |
| `parse_args_neither_tc_nor_tc_sample_rejected` | Neither set ⇒ `Err(MissingFlag(_))`. |
| `parse_args_seed_default_none` | Omitted ⇒ `args.seed.is_none()`. |
| `parse_args_seed_parses_decimal` | `--seed 42` ⇒ `Some(42)`. |
| `parse_args_seed_parses_hex_with_0x` | `--seed 0xDEADBEEF` ⇒ `Some(0xDEADBEEF)`. (Convention follows existing parsers; if this is rejected by the existing u64 parser, the test guards the chosen behaviour either way — accept-hex is the pin.) |
| `parse_args_seed_rejects_negative_number` | `["--seed", "-1", ...]` ⇒ Err with InvalidValue and message containing "not a valid u64". (Argv arrives via `next_val!()` which returns the literal `"-1"` token; `parse_u64_seed("-1")` fails through both branches — no `0x` prefix and `s.parse::<u64>()` rejects the leading minus. Distinguishes the negative-number rejection from the "next-token-is-a-flag-shape" case, which is a separate hazard not under test here.) |
| `parse_args_tc_sample_invalid_grammar_rejected` | `--tc-sample foo` ⇒ Err with parser message surfaced. |

### 6.4 `mod summary::tests` (~10 LOC)

| Test | Asserts |
|---|---|
| `summary_line_with_tc_appends_tab_separated` | Append a `SummaryLine { tc: Some("10+0.1".into()), .. }` to a tempfile; resulting line ends with `\t10+0.1\n`. |
| `summary_line_without_tc_appends_dash` | `tc: None` ⇒ trailing `\t-\n` (sentinel for ELOH.A/B fixtures). |
| `format_summary_by_tc_two_buckets` | Buckets `[(10+0.1, W=110, L=95, D=45), (20+0.2, W=105, L=90, D=55)]` ⇒ exact string `"summary-by-tc: 10+0.1: W=110 L=95 D=45 (250)  20+0.2: W=105 L=90 D=55 (250)"`. |
| `format_summary_by_tc_single_bucket_emitted` | One-bucket input ⇒ `"summary-by-tc: 10+0.1: W=... (...)"` (still emitted; degenerate single-TC mix). |
| `format_summary_by_tc_zero_games_in_bucket` | A bucket with W=L=D=0 emits `"... 30+0.3: W=0 L=0 D=0 (0)"`. (Zero-pair edge case if `--max-games` is small relative to bucket count.) |

### 6.5 `mod controller::tests` (~20 LOC, extends the synthetic-pool fixtures)

| Test | Asserts |
|---|---|
| `bootstrap_dispatches_per_pair_sampled_tc_under_tc_sample` | `args.tc_sample = Some(dist)`; controller's bootstrap sends `WorkerCmd::PlayPair { engine_tc, .. }` where `engine_tc` is the sampler's first draw under the fixed seed. Captured via the synthetic_pool's command-recorder. |
| `drain_loop_redispatch_resamples_per_pair` | After 2 PairComplete reports, the third dispatched PlayPair carries the sampler's third draw. (Pins per-pair-not-per-game cadence.) |
| `tc_sample_pair_color_swap_uses_same_tc` | Worker receives one PlayPair; both GameComplete reports for that pair carry the same `tc` value. (Spec invariant: color-pair plays one TC.) |
| `opponent_tc_override_dominates_under_tc_sample` | `args.tc_sample = Some(dist)`, `args.opponent_tc_override = Some(60+0.6)`; PlayPair's `opponent_tc == 60+0.6` regardless of which TC the sampler drew for `engine_tc`. |
| `tc_mode_passes_static_tc_in_play_pair` | `args.tc = Some(10+0.1)`, `args.tc_sample = None`; every PlayPair's `engine_tc == 10+0.1` and `opponent_tc == args.opponent_tc_override.unwrap_or(10+0.1)`. (Backwards-compatibility test.) |
| `per_tc_buckets_aggregate_in_input_order` | 4-bucket uniform dist, 8 games (4 pairs); after run, the captured buckets are in input-spec order with W+L+D summing to 2 per bucket. |
| `summary_by_tc_line_appended_under_tc_sample` | `args.tc_sample = Some(dist)`; summary.txt's last line matches `^summary-by-tc: ...` regex. |
| `summary_by_tc_line_absent_under_tc_only` | `args.tc = Some(...)`, `args.tc_sample = None`; summary.txt has no `summary-by-tc:` line — only `summary:` and `converged:`. |
<!-- v1's seed_independence_opening_substream test removed: dropped along with the Prng::fork API per v1 reviewer should-fix on speculative future-proofing. -->
| `seed_reproducibility_pair_tc_mapping_deterministic` | Two synthetic runs with identical args + identical `--seed` ⇒ identical `pair_tcs` Vecs (the pre-materialised sampler output from §4.5). Assertion is `Vec` equality, indexed by pair_index; this is now a hard function-from-pair_index guarantee since sampler advances happen exclusively in the up-front `(0..total_pairs).map(...).collect()` loop, before any concurrency. Pins the §4.5 pre-materialisation contract. |

### 6.6 PGN golden-file (no new test file; extends existing `mod pgn::tests`, ~5 LOC)

| Test | Asserts |
|---|---|
| `pgn_time_control_tag_reflects_sampled_tc` | Construct `PgnHeader { time_control: Some("20+0.2".into()), .. }`; format; assert the produced PGN contains exactly one `[TimeControl "20+0.2"]` line. (Pins that the existing PGN emitter Just Works with the sampled value — no PGN-side changes needed.) |

### 6.7 `#[ignore]`-gated end-to-end (~10 LOC, extends ELOH.A/B/C smoke tests)

| Test | Asserts |
|---|---|
| `end_to_end_self_play_tc_sample_runs` | `--engine clawfish --opponent clawfish --tc-sample 2+0.5:1,3+0.5:1 --concurrency 1 --max-games 4 --target-sigma 0 --initial-elo 2000 --k0 0 --seed 42`. 4 PGNs written, each with a `TimeControl` tag from the configured set; summary has 4 entries with `tc=` field; `summary-by-tc:` line appears; exit 0. (TCs are 2-3s base / 0.5s inc — generous enough that clawfish-vs-clawfish doesn't time-forfeit on a hot CI runner; the test only validates the code path doesn't crash, not that fast clocks work.) |

## 7. Order of operations

1. **Slice A — `mod prng` + `mod tc_sample` + `--tc-sample` / `--seed` CLI parsing + post-loop mutex.** ~70 prod LOC + ~60 test LOC. Pure new code in `src/bin/elo-iterate.rs`'s submodule layout; touches `mod cli` only at the field-addition + post-loop-validation level. Sonnet — bounded surface, no cross-cutting API change.
2. **Slice B — controller plumbing + WorkerCmd/Report/Config migration + per-TC bucket aggregation.** ~60 prod LOC + ~25 test LOC. Touches `WorkerCmd::PlayPair`, `WorkerReport::GameComplete`, `WorkerConfig` (drops static `tc`/`opponent_tc`), bootstrap dispatch (line 5008), drain-loop redispatch (line 5145), worker's PlayPair handler (line 4862-ish, building `(white_tc, black_tc)`), the per-game persistence block (lines 5050-5077), and the run-end aggregate emission (after the `converged:` line at 5196). Sonnet, but sequential after Slice A — `WorkerCmd::PlayPair` needs the `TimeControl` type already imported and Slice A's `tc_sample::TcDistribution` is referenced from the new args field. **Cross-cutting API churn lives here**; estimating ~25 mechanical edits across the worker / controller test fixtures (`synthetic_pool`'s canned-report builders).
3. **Slice C — summary extension + per-TC aggregate formatter + integration tests + doc-delta.** ~20 prod LOC + ~15 test LOC. `SummaryLine.tc` field, `format_summary_by_tc`, `mod summary::tests` (§6.4) extensions. Sonnet, sequential after Slice B (the controller invokes `format_summary_by_tc`).
4. **Pre-review mechanical checks** (workflow.md step 9).
5. **Final review loop** (workflow.md step 10).
6. **Benchmark.**
   - Pre-impl: standard `bench` from current `tooling/elo-harness` HEAD = `6b9cbac` (ELOH.C landing). Same numbers as `bench/eloh-c.md`.
   - Post-impl: `bench` again. **Expected: byte-identical to pre-impl** — ELOH.D adds zero engine-side code paths; node count and NPS are unchanged. Append a one-paragraph note to `bench/eloh-d.md` confirming the no-regression observation.
7. **Commit + push** — atomic doc-delta per §11.
8. **Manual back-test (Part 2, post-commit, ~30 min wallclock).**
   - One degenerate-mix self-play run replicating M3.F's saturating-anchor framing: `--engine target/release/clawfish --opponent /opt/homebrew/bin/stockfish --tc-sample 10+0.1:1 --opponent-option UCI_LimitStrength=true --opponent-option UCI_Elo=2114 --max-games 200 --target-sigma 0 --initial-elo 2114 --concurrency 4 --k0 0 --seed 0xC1AB_F15A_E10D_D001 --engine-launch-prefix 'taskpolicy -c utility' --opponent-launch-prefix 'taskpolicy -c utility'`. (Seed `…D001` differs from the no-`--seed` default `…D000` so the explicit-vs-default behaviour is genuinely distinguished.)
   - Pass: result reproduces M3.F's ~2114 Elo within ±2σ ≈ ±70 Elo, identical to ELOH.B's Part 1 gate. Validates that the mixed-TC code path is a strict superset of the fixed-TC path — a degenerate single-TC mix observationally matches `--tc 10+0.1`.
   - Diagnostic ladder on Part 2 failure:
     - If estimate diverges from M3.F by >2σ: investigate whether the per-pair sampling is leaking across pairs or whether the worker's `engine_tc` plumbing dropped a value (regression test in §6.5 should catch this; if it doesn't, add one).
     - If `summary-by-tc:` line is malformed: format-check failure; the §6.4 `format_summary_by_tc_single_bucket_emitted` test pins this.
   - Archive verdict to `docs/research/tooling-elo-harness-validation.md` Part 2 section.

## 8. Dependencies

- **ELOH.A** for color-pair invariant + match-loop time-source seam. Already landed.
- **ELOH.B** for the controller drain-loop, K-update path, σ-stopping (the K-update accepts mixed-TC outcomes unmodified). Already landed.
- **ELOH.C** for `--virtual-clock` flag (orthogonal to ELOH.D; ELOH.D's TC sampling sets base+inc before clock initialisation, ELOH.C swaps the time-source primitive under those clocks). Already landed.
- **No new external crate dependency.** SplitMix64 is hand-rolled (~20 LOC) — adding `rand`/`rand_chacha` for one PRNG is over-engineering; the existing `Xorshift64` in `mod sigma::tests` (line 3262) is private to tests and uses `xorshift`-not-`splitmix`, so it's the wrong primitive and not promotable.
- **No M4 dependency.** ELOH.D is independent; M4.D is its first downstream consumer but M4.D can fall back to discrete-approximation against fastchess if ELOH.D misses the window.

## 9. Parallelization map

After this plan converges through review:
- **Slice A and Slice C are independent in source surface** (Slice A: new `mod prng`, new `mod tc_sample`, `mod cli` field additions and post-loop mutex; Slice C: `mod summary` field + new formatter + tests). Could in principle run in parallel via two coder-agents.
- **Slice B touches the cross-cutting WorkerCmd/Report API + controller** and depends on Slice A's `tc_sample::TcDistribution` type (it goes into the new `Args` field that Slice B threads through the controller's up-front `pair_tcs` materialisation per §4.5). Slice B sequential after Slice A.
- **Slice C is sequential after Slice B** because the controller invokes `format_summary_by_tc` (defined in Slice C) — though the function itself can be implemented standalone with its own tests in parallel; only the *integration call site* in the controller must wait.
- **Honest dependency shape:** A → (B, C in parallel). Slice C-the-formatter can land before Slice B-the-controller-integration; Slice C-the-integration follows Slice B. In practice the Slice C surface is small enough (~35 LOC total) that doing it sequential after B in one coder-agent shot is simpler than the two-agent fan-out for marginal wallclock gain.
- **Recommended in-practice shape:** single coder-agent runs Slice A → B → C sequentially (~245 LOC total, well within one Sonnet session); plan-mode and review-mode are the only forks. No Opus override needed — no novel domain-type contracts (PRNG and bucketed sampling are routine), no per-thread invariants (single controller-thread sampler), no ~16-test-site refactor (the WorkerCmd/Report changes are localized to a handful of synthetic-pool fixture builders).

## 10. Risk register

- **`--seed` single-purpose vs joint with sub-streams.** Spec open question 1. **Resolved (post-v1-review): single-purpose. `--seed` controls TC sampling only.** v1's plan introduced a labelled-fork API (`Prng::fork("tc-sample")`, `Prng::fork("opening")`) to "future-proof against opening-book consumers" — flagged by reviewer per CLAUDE.md "don't design for hypothetical future requirements." Opening positions are always startpos in ELOH.D; no consumer for an opening sub-stream exists. When a future opening-randomization layer lands (likely as part of M5+ or an opening-book milestone), it can introduce its own seed flag *or* a labelled-fork API as part of *that* unit's plan — the change is local. Reverting to single-purpose drops `Prng::fork`, `seed_independence_opening_substream_unperturbed_by_tc_sample`, the labelled-fork doc comments, and ~30 LOC of speculative complexity. Net simplification.

- **Per-TC table in `summary.txt` vs. stdout-only.** Spec open question 2. Resolved: both. Defended: stdout is for live monitoring (humans reading the harness output); `summary.txt` is for post-hoc analysis (downstream tooling reading the artifact). Two writes for two consumers; cost is a single println + a single appended line. Pinned by §6.5 `summary_by_tc_line_appended_under_tc_sample`.

- **Color-pair invariance configurable.** Spec open question 3. Resolved: not configurable in ELOH.D. Defended: per-pair sampling preserves the color-pair fairness invariant from ELOH.A (a pair is one experiment at one TC); per-game sampling would break that without a clear consumer asking. Defer; the change if a consumer materialises is to move the per-pair `pair_tcs[pair_index]` lookup to a per-game `dist.sample(&mut tc_rng)` call at the worker's per-game point — but doing so under the §4.5 pre-materialisation invariant would require re-shaping the materialisation to a per-game `Vec<TimeControl>` of length `2 * total_pairs`. Mechanical, not architectural.

- **PRNG correctness — SplitMix64 hand-rolled.** Risk: a transcription error in the mixer constants (`0x9E3779B97F4A7C15`, `0xBF58476D1CE4E5B9`, `0x94D049BB133111EB`) corrupts the sample distribution, masking as "looks random in tests but biased in production." Mitigation: §6.2's chi-squared test at N=1000 catches systematic bias for two distribution shapes (4-bucket uniform, 2-bucket 3:1 skewed); the constants are documented inline against the published reference (Vigna 2014 / Steele-Lea-Flood 2014); a hardcoded golden-fixture test (`prng_zero_seed_first_three_words_golden`) pins the first three u64 outputs from `Prng::new(0)` against pre-computed expected values, catching any constant typo at compile-time-of-test.

- **`WorkerConfig` field removal — backwards compatibility.** Removing `tc`/`opponent_tc` from `WorkerConfig` is a struct-shape change; the synthetic-pool fixtures in `mod controller::tests` reference these fields. Mechanical fan-out captured in Slice B's destructure-site enumeration at §4.4 (three exhaustive sites: 4808, 5339, 5663). Not a tail risk.

- **Pre-materialised TC sequence vs streaming sampler.** v2 plan-review caught a determinism bug — sampling at dispatch time is non-deterministic under N>1 concurrency because `sample_pair_tcs` call order tracks worker completion order, which depends on subprocess scheduling. v3 fixes this by pre-materialising `Vec<(TimeControl, TimeControl)>` of length `total_pairs` at run start (single-threaded; deterministic by construction). Memory cost: 8 bytes per pair × `total_pairs`; at 5000 pairs that's 40 KB — negligible vs. PGN file output budgets. Trade-off worth it for the structural-determinism guarantee.

- **Mutual-exclusion timing — parse-time vs run-time.** Spec line 159: "harness errors at parse time." We honor this (post-loop validation in `parse_args`). Alternative considered: lazy validation deferred to controller setup — rejected because parse-time errors are user-friendlier (they fire before any subprocess spawn).

- **Repeated TC keys in `--tc-sample`.** `parse_tc_sample` rejects `"10+0.1:1,10+0.1:2"` rather than silently merging. Defended in §4.2's doc comment: a typo like this likely indicates user confusion (did they mean two distinct TCs that happen to look similar? or did they want weight 3?), so failing loudly beats silent merging. Pinned by §6.2 `parse_rejects_repeated_tc`.

- **Modulo bias in sample().** Spec mentions u32 weights, so `total: u32`. We draw `rng.next_u64()` and mod by `total`. The standard worst-case per-bucket bias bound for `u64 % m` when `2^64` isn't a multiple of `m` is `(2^64 mod m) / 2^64 ≤ m / 2^64`. With `m = total ≤ u32::MAX = 2^32 - 1`, bias ≤ `2^32 / 2^64` = `2^-32` per bucket — well below the chi-squared detection threshold at N=1000 in §6.2's gate. Documented inline at the `sample` impl site.

- **Bench expected byte-identical post-impl.** ELOH.D adds zero engine-side code paths. If the post-impl bench differs from pre-impl, that's a structural bug (something on the engine path got perturbed). Treat as a hard error in the bench step; do not commit.

- **`WorkerCmd::PlayPair` channel-message size.** Adding two `TimeControl` fields (each `{ initial_ms: u32, increment_ms: u32 }` = 8 bytes) grows the enum from 12 bytes to 28 bytes. mpsc channel cost is negligible at the harness's pair-per-thread cadence; no perf risk.

- **Default seed `0xC1AB_F15A_E10D_D000`.** Hard-coded; intentionally non-zero (zero seed often surfaces as a default-state pathology). Documented in `--help` and the `mod prng` doc comment. If a future contributor changes the constant, the §6.5 `seed_reproducibility` regression test will fail because its hardcoded sample fixture pins the current seed's draw sequence — the test acts as a tripwire on accidental constant churn.

## 11. Doc-delta — atomic with landing

- `docs/tooling/elo-iteration-harness.md` — ELOH.D row → done; scope detail → "Done" prose with actual landing size; cross-link Part 1 result; ELOH milestone closed banner at the top of the doc (or the milestone-status row, wherever the existing closure pattern from prior milestones lives). **Atomic doc consistency fix:** ELOH.B doc-delta line 115 currently says "harness is *not* deterministic across runs ... `--seed` controls only opening-position selection." This is now stale (ELOH.D wires `--seed` for the first time, controlling TC sampling). Update the line to reflect ELOH.D's actual semantics — `--seed` controls TC sampling deterministically; opening-position selection is still always startpos in ELOH.D and has no seed consumer; cross-run determinism remains imperfect under N>1 concurrency due to subprocess-scheduling-dependent K-update arrival order, even with a fixed seed.
- `docs/tooling-backlog.md` — sub-bullet 8 of the harness entry → "Done."
- `docs/workflow.md` — new "Mixed-TC SPRT" subsection (~25 lines) under "Online Elo iteration":
   - `--tc-sample` syntax, the redefined-game framing ("draw TC from D, then play standard chess at that TC"), and worked example.
   - Mutual exclusion with `--tc`.
   - Joint `--seed` semantics + sub-stream derivation note.
   - One-line forward pointer to downstream tooling (M4.D mixed-TC width-tune is the first consumer; analysis tooling for SPRT verdict + Δ(TC) regression lives outside this scope).
- `docs/research/tooling-elo-harness-validation.md` — Part 1 (chi-squared sampler, atomic with landing) and Part 2 (degenerate single-TC self-back-test, follow-up commit).
- `docs/roadmap.md` — ELOH milestone → closed (all four sub-phases pass).
- `CLAUDE.md` Status table — ELOH milestone closed row added.
- `bench/eloh-d.md` — new file; no-regression note + link to `bench/eloh-c.md` as load-bearing baseline.
- `docs/architecture.md` — none expected (no engine-side change).

## 12. Verification checklist

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --release` (full suite — ELOH.A's 51 + ELOH.B's 88 + ELOH.C's ~85 + ELOH.D's ~30 new tests)
- `cargo llvm-cov --summary-only --lib --release`
- `git add -N` on any new `.rs` files (none — all submodules are inside `src/bin/elo-iterate.rs`); `cargo mutants --in-diff` on the unit's diff
- `cargo deny check` — no new dependency, but the file changed, so re-run.
- Bench: `cargo run --release --bin clawfish bench` matches pre-impl byte-for-byte (node count + NPS), confirming zero engine-side perturbation.
- The §6.2 sampler gate tests pass (`sample_skewed_3to1_at_seed_xfeed_yields_known_counts` + `sample_uniform_4_bucket_at_seed_xfeed_yields_known_counts`); pre-merge gate. (v2's earlier "chi-squared 99% CI" framing was replaced with exact-count fixtures + an `eprintln!`'d chi-squared as a side observable.)
- The §6.5 `seed_reproducibility_pair_tc_mapping_deterministic` test passes; pre-merge gate. (v1's `seed_independence_opening_substream_*` test was dropped along with `Prng::fork`.)

## Appendix — branches and worktrees

ELOH.D lands on the existing `tooling/elo-harness` branch in `/Users/alex/clawfish-elo-harness`, on top of ELOH.C's `6b9cbac`. **Deviation from spec line 200**, which originally said ELOH.D should branch off `main` with default name `tooling/eloh-d-mixed-tc` once ELOH.C had merged. Per the user's standing directive ("Work in the ~/clawfish-elo-harness worktree") and the precedent set by ELOH.C (which itself deviated from the spec's separate-branch suggestion for the same reason), ELOH.A/B/C/D land as one chain on the same branch and the ELOH milestone merges to main as a single bundle. The spec's branch suggestion is updated retrospectively to record the deviation.

## Appendix — review history

- **Plan v1 (2026-04-30)** — written; spawned blind plan-reviewer. Reviewer returned 3 must-fix + 9 should-fix + 4 nits + verdict "revisions required." Most consequential must-fix: worker-side dataflow change after `WorkerConfig.tc`/`opponent_tc` removal wasn't enumerated; `--seed 0xDEADBEEF` hex parsing not pinned (existing `s.parse::<u64>()` is decimal-only); synthetic_pool fixture exhaustive destructures (lines 5339-5346, 5423, 5663-5667, 5732) needed enumeration. Most consequential should-fix: speculative `opening` sub-stream + `Prng::fork` API was future-proofing without a consumer (CLAUDE.md ground rule).
- **Plan v2 (2026-04-30)** — addressed all v1 must-fix + should-fix + nits. Single-purpose `--seed` (no `Prng::fork`); §4.3 `parse_u64_seed` helper; §4.4 enumerates worker dataflow change + destructure-site churn; §4.5 dispatch-order determinism *claim* (later flagged as wrong); §4.6 per-TC bucket build path; §6.1/§6.2/§6.3/§6.5/§6.7 reshaped; §7 step 8 manual seed differs from default; §10 modulo bias wording; §11 doc-delta atomic fix to ELOH.B determinism line. Reviewer pass 2 returned 2 must-fix + 2 should-fix + 4 nits + verdict "revisions required."
- **Plan v3 (2026-04-30, this revision)** — addresses all v2 must-fix + should-fix + nits. **Most consequential v2 must-fix:** §4.5 claimed `pair_index → engine_tc` was deterministic via dispatch-time sampling, but under N>1 concurrency the call order to `sample_pair_tcs` depends on worker completion order which is non-deterministic. v3 pre-materialises all TCs into `Vec<(TimeControl, TimeControl)>` indexed by pair_index *before* the bootstrap loop — sampler advances happen exclusively in a single-threaded up-front loop, then dispatch reads `pair_tcs[pair_index]`. Now genuinely deterministic regardless of concurrency. Other v2 fixes: §4.4 destructure-site list corrected (lines 5423 and 5732 already use `..` and don't need editing; line 4808 worker recv loop *does* need binding the new fields and is now explicitly listed); §4.6/§4.7 renumbered (was duplicate §4.6); §4.6 cleanly bifurcates `--tc-sample` (build buckets) vs `--tc` (skip entirely); §5 module sketch drops "fork"; §6.5 test renamed `seed_reproducibility_pair_tc_mapping_deterministic` (shorter; references Vec equality not multiset); §12 verification checklist updated to drop stale test names; §10 risk register entry on §4.5 ownership refactored to record the pre-materialisation choice. Re-review pending.
