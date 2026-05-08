# Tooling and QA backlog

Industry-best-practice items surfaced during the 2026-04-27 workflow review but not yet adopted. **Listed in recommended implementation order** — pick from the top when the next slot opens for tooling work.

### ~~Custom in-process Elo-iteration harness~~ — Done (ELOH.B/C/D/E, 2026-04-30 / 2026-05-01)

Landed as `src/bin/elo-iterate.rs` on branches `tooling/elo-harness` (A/B/C), `tooling/eloh-d-mixed-tc` (D), `tooling/eloh-e-sprt` (E).

- **ELOH.A** (harness foundation): persistent subprocesses, UCI driver, native adjudication, per-side clock management, color-paired match loop, PGN/summary output. ~2300 LOC + 139 tests.
- **ELOH.B** (statistical layer): Robbins-Monro K-update at single-game cadence, σ-based stopping, N-parallel-pair concurrency via `std::thread` + `mpsc`, threshold adjudication (resign/draw/max-moves), convergence-progress output. Replaced `scripts/elo-iterate.sh` (now a thin wrapper) and `scripts/sprt.sh rating-estimate` arm (now routes through the binary).
- **ELOH.C** (hardware-invariant TC): `VirtualClock` UCI option + harness `--virtual-clock` flag with handshake negotiation. ADR-0021.
- **ELOH.D** (mixed-TC sampling): `--tc-sample <SPEC>` + `--seed N` for mixed-TC SPRT and Δ(TC) regression.
- **ELOH.E** (in-process pentanomial-GSPRT): `mod sprt` (LLR + Wald bounds + pentanomial CI), `--sprt-elo0/elo1/alpha/beta` flags, per-worker pair-score buffers, run-end `sprt:` and `ci:` summary lines, `match.pgn` concatenation. `scripts/sprt.sh sprt|match` and `scripts/match.sh self-play|vs-stockfish` rewritten as harness wrappers; `scripts/match.sh compliance` keeps fastchess. ADR-0022.

Usage: `cargo run --release --bin elo-iterate -- --engine <path> --opponent <path> --tc <TC> --max-games <N> --initial-elo <E> [...]`. See `--help` for all flags. `scripts/elo-iterate.sh` preserves the prior bash CLI surface as a wrapper.

### EPD diagnostic suites — WAC and STS regression harness

**Purpose.** Deterministic per-position best-move scoring against canonical public diagnostic suites: **WAC** (Win at Chess; 300 tactical positions, single `bm` solution) and **STS** (Strategic Test Suite; 1500 themed positions across 15 strategy themes, weighted multi-move `c0` scoring). Complements SPRT — SPRT measures relative game-playing strength stochastically over a game distribution; WAC/STS measure absolute correctness on a fixed corpus deterministically. WAC for tactical health (catches pruning regressions that fast-TC SPRT might miss); STS for positional / eval health (per-theme breakdown attributes regressions to specific eval components).

**Surfaced.** During the M5.D `/my-explain` walkthrough on 2026-05-06, when the user asked how frontier futility handles tactical quiet moves (direct checks, discovered checks, mate-in-1, fork setups). Futility pruning accepts statistical risk for speedup; WAC catches tactical mis-prunes that fast-TC SPRT might pass but slow-TC games would reveal. STS becomes load-bearing in M6 (eval improvements), where each new eval term should defend itself via per-theme score change on the relevant group.

**Scope.** EPD parser handling `bm` + `c0` opcodes; per-position runner that issues `position fen <FEN>` → `go movetime <T>` → captures `bestmove` → scores against the annotation. Aggregate output: total score + per-theme breakdown for STS. Both suites are public-domain EPD; vendor under `bench/data/`. Separate `src/bin/epd-suite.rs` binary; not in CI by default (multi-hour wallclock at meaningful per-position time).

**When to land.**
- **M5.D-class plan-review trigger.** If a futility / pruning phase's plan-reviewer pushes on tactical-soundness, land alongside or before the phase as the empirical answer beyond "SPRT was positive."
- **Mandatory before M6.A.** M6 per-phase plans should reference per-theme STS deltas, not just aggregate Elo.

**Implementation notes.**
- WAC: 300 × 5–30s/position ≈ 25min–2.5h wallclock; trivially parallelisable across positions.
- STS: 1500 × 5–30s/position ≈ 2h–12h; STS-Elo regression `Elo ≈ 44.523 · (points/100) − 242.85` maps raw points to an estimated CCRL Elo (calibrated for the ~2000–2800 band; extrapolation outside degrades).
- Time-per-position is the main lever; both suites' scores plateau around ~5s/pos at current engine strength.
- Bratko-Kopec (24 positions) deferred — too small for statistical significance; covered functionally by WAC + STS overlap.

**Estimated size.** ~150–250 LOC harness code + vendored EPD data files.

### Split `src/bin/elo-iterate.rs` into a library + thin binary

**Surfaced.** ELOH.E plan §11 file-size growth observation. After ELOH.E lands, `src/bin/elo-iterate.rs` is past 9000 lines (`mod cli` + `mod prng` + `mod tc_sample` + `mod driver` + `mod adjudicate` + `mod estimator` + `mod sigma` + `mod sprt` + `mod pgn` + `mod summary` + `mod progress` + `mod match_loop` + `mod controller` + `mod root_tests` + `mod e2e_smoke` all in one file). This is approaching the upper bound of comfortable single-file editing.

**Scope.** Move the modules into `src/elo_iterate/<modname>.rs` files behind a `src/elo_iterate.rs` library crate; `src/bin/elo-iterate.rs` becomes a ~10-line entry point that calls `clawfish::elo_iterate::main()`. Test-site migrations across all of ELOH.A/B/C/D/E's existing tests would happen inside the same diff.

**Why deferred.** Splitting it as part of ELOH.E would have inflated the unit's diff and forced churn across all the existing test files at the same time. A standalone refactor is reviewable on its own terms.

**When to land.** Whenever the next contributor finds themselves frustrated by the file size. Not gating any milestone.

### ~~Per-game TC sampling for mixed-TC SPRT~~ — Done (ELOH.D, 2026-04-30)

Landed as `--tc-sample <SPEC>` and `--seed N` flags on `src/bin/elo-iterate.rs`. Closes the ELOH milestone.

**Mechanism.** Discrete weighted distribution `<TC>:<weight>(,<TC>:<weight>)*` (e.g. `10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1`); harness pre-materialises all per-pair TCs into `Vec<(TimeControl, TimeControl)>` indexed by pair_index *before* the bootstrap loop, so the SplitMix64 sampler advance order is deterministic regardless of subprocess scheduling under N>1 concurrency. Both color-swapped games of a pair use the same sampled TC (preserves the "fair experiment at one TC" invariant).

**Output.** PGN's `[TimeControl "<base>+<inc>"]` tag and per-game `summary.txt` `tc=<base>+<inc>` field record the sampled TC; `summary-by-tc:` aggregate (input-spec order) appended at run end only under `--tc-sample`.

**Compatibility.** `--tc` and `--tc-sample` mutually exclusive at parse time. Compatible with both rating-estimate (`--k0 0 --target-sigma 0`) and online-σ modes — game outcomes remain i.i.d. under the redefined mixed game; K-update math unchanged. Under `--tc-sample`, the resulting Elo number is "the rating of the mixed game"; for per-TC ratings, run separate fixed-TC sessions.

**Decision-rule scope.** Mixed-game SPRT verdict computation, Δ(TC) regression fit, confidence-band visualisation are downstream tooling. The harness emits per-game (TC, W/L/D) data; analysis tooling consumes it. M4.D's mixed-TC width-tune campaign is the first consumer.

### ~~Hardware-invariant TC: `go nodes` mode + `VirtualClock` UCI extension~~ — Done (ELOH.C, 2026-04-30)

Landed as `VirtualClock` UCI option + `--virtual-clock` harness flag on branch `tooling/elo-harness`. ADR-0021 (`docs/decisions/0021-virtual-clock-uci-option.md`). Bench: `bench/eloh-c.md`.

**Time source used:** `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` per thread. Decouples search time budget from background load and OS scheduler preemption; not fully thermal-invariant (throttled CPU accumulates CPU-time faster per unit of work), but substantially better than wallclock under the typical load conditions of a rating-estimation run. Combine with P-core pinning and external cooling for tightest results.

**Cycle / instruction counters explicitly not used.** Confirmed inaccessible without root or a non-grantable private entitlement (`com.apple.private.kernel.kpc`) on Apple Silicon macOS. Inaccessible inside any common Linux ARM64 VM on Apple Silicon (Apple does not expose the proprietary PMU through any hypervisor interface). Deferred indefinitely unless the project moves primary development to bare-metal Asahi Linux or non-Apple ARM64/x86 Linux. See `docs/research/tooling-cpu-cycle-counters.md` for the full evidence trail.

**`go nodes N` dropped.** Per user decision 2026-04-30: nodes-per-move is implementation-coupled — the figure shifts even within a single binary across runtime settings (Hash size, eval weights, etc.), making it unsuitable for cross-version SPRT. The `MatchTimeMode::Nodes(u64)` seam from ELOH.A remains as unconstructible-from-CLI dead code; future work may revive it for non-gate diagnostic use.

**Usage:** `cargo run --release --bin elo-iterate -- ... --virtual-clock`. The harness sends `setoption name VirtualClock value true` only to engines that advertise the option; Stockfish self-play falls back to wallclock silently. Engine must probe `CLOCK_THREAD_CPUTIME_ID` correctness on new machines before relying on the metric (probe procedure in ADR-0021 Operator's checklist).

### ML-tuned aspiration window sizing

**Purpose.** Replace M4.D's fixed `±25 → ±100 → full` widening schedule with a learned policy that predicts the optimal first-try window from cheap features, maximizing expected node savings.

**Motivation.** Aspiration is a bet on score continuity across ID iterations: a successful narrow-window search saves nodes (up to ~2× under perfect ordering), a failed one costs a re-search. The optimal window width depends on position-specific volatility, which is observable from cheap features but not captured by a fixed schedule. Surfaced during the M4.D walkthrough (2026-04-30 chat) as a deferred follow-up if the fixed schedule under-performs or saturates after SPSA tuning.

**Approach.**

1. **Feature set** (all cheap, all available without extra search work):
   - Iteration depth and depth threshold delta.
   - Prior score and prior score delta (`score(d-1) − score(d-2)`); the delta is the single strongest feature.
   - EWMA of past `K` score deltas.
   - Game-tree ply number.
   - Total material on each side (proxy for game phase; once M6's tapered eval lands, use the proper phase tag instead).
   - Branching factor at the root (move-list size).
   - First-move-stability flag from the prior iteration.
   - TT hit rate at root or near-root.
2. **Architecture.** Single hidden layer MLP, ~16-32 units, ReLU. Total params low thousands. Output: continuous window width (regression) or softmax over a bucket grid (`{25, 50, 100, 200, full}`). Inference ~ns; called once per ID iteration.
3. **Training data.** Per-position label generated by running the search with a grid of candidate windows and taking the argmin node count. Corpus: ~100K positions sampled from CCRL games + tactics suites (the latter to cover failure modes that self-play under-represents). Cost estimate: ~50-200 CPU-hours at d≤12.

**Cheap baseline first.** Before training a model, validate that the dominant signal isn't already captured by a one-line heuristic:

```
window = clamp(k * |score(d-1) − score(d-2)|, MIN, MAX)
```

Three SPSA-tunable params. Empirically this baseline captures 60-80% of the ML model's gain in published work on similar problems. If the baseline + SPSA SPRTs neutral or marginal vs M4.D's fixed schedule, the ML approach is unlikely to clear `elo1=5`.

**TC-adaptive (or depth-adaptive) intermediate tier.** Between the cheap delta-baseline and a full MLP, a parameterized adaptive width that scales with search depth or estimated time-per-move is a natural stepping stone (surfaced during the M4.D walkthrough on 2026-04-30 alongside the mixed-TC SPRT discussion). Functional shape: `half_width(depth) = base · max(min_factor, 1 - α·(depth - threshold))` — two parameters (`base`, `α`) plus the existing `threshold`. Depth-adaptive is cleaner than time-adaptive (depth is a discrete observable in the ID outer loop; no coupling to `compute_caps` time-management state). Validation: mixed-TC SPRT against M4.D's fixed-width baseline (the same campaign methodology M4.D establishes), with adaptive variants entering the per-TC regression curve to surface where adaptation helps. Expected gain: small (+2-4 Elo over fixed-width) but cheap to implement (~30 LOC + parameter tune); validates whether the depth/TC interaction motif is real before investing in delta-EWMA or MLP variants.

**Why not now.**

- M4.D's gate is the fixed schedule passing mixed-TC SPRT; building ML training infra is a multi-week scope expansion to a phase that should land in days.
- Prerequisites missing: tapered eval for proper phase signal (M6), SPSA / CLOP harness for the cheap baseline, position corpus, offline label-generation pipeline.
- Elo headroom small at this layer: ~+3-5 Elo realistic ceiling for ML over hand-tuned, vs +50-80 Elo for NMP, +80-100 Elo for LMR, +400+ Elo for NNUE. Opportunity cost is poor.
- NNUE (M9) re-trains the eval from scratch; an ML aspiration model trained on PeSTO eval would need re-training post-NNUE.

**When to land.** Four-tier escalation, each tier proceeding only if the previous shows ≥ +5 Elo of remaining headroom worth chasing:

1. **Fixed schedule (M4.D)** — ±50 default, width-tuned via mixed-TC SPRT. Ships at M4.D close.
2. **TC- or depth-adaptive parametric (post-M5, pre-M9)** — `base · max(min_factor, 1 - α·(depth - threshold))`. SPSA-tuned. Cheapest validation of the depth/TC interaction motif.
3. **Cheap delta-baseline (post-M9 or earlier if (2) saturates)** — `window = clamp(k · |score(d-1) − score(d-2)|, MIN, MAX)`. Three SPSA-tunable params.
4. **MLP (post-M9, only if (2)+(3) saturate)** — full feature set + hidden layer.

**Estimated size.** Adaptive parametric: ~30-50 LOC + SPSA harness reuse. Cheap delta-baseline: ~50 LOC + SPSA harness reuse. ML model: ~500-800 LOC (feature extraction + inference + offline-training pipeline as a separate Python or Rust binary), plus the corpus + label-generation infrastructure (potentially shared with Texel tuning or NNUE data prep).

---

*Active queue ends here. New items append above this line.*

---

## Deferred — gated on a specific later trigger, not on prioritization

- **SPRT infrastructure** (`cutechess-cli` / `fastchess`) — premature until M3; nothing has strength to test yet.
- **`unsafe` audit policy** — defer until the engine first uses `unsafe` (likely a hot-path `get_unchecked` in magic-bitboard lookups, possibly M1.C or later). At that point write an ADR for when `unsafe` is allowed and how it's reviewed.

---

## Done since the 2026-04-27 review

- **Mock-engine fixture for `production_worker_fn` setoption ordering** — completed 2026-05-09. New test fixture binary at `src/bin/mock_engine.rs` (~106 LOC, declared as `[[bin]]` in `Cargo.toml`) records every UCI command received via `MOCK_ENGINE_RECORD_PATH` and replies with a minimal subset (`uci`+`uciok` with optional `VirtualClock` advertisement, `isready`+`readyok`, `bestmove 0000`, `quit`). Ten subprocess-driven tests at `mod controller::tests::production_worker_tests` (T1–T9 plus T7 scenarios A+B) pin the per-pair UCI sequence: `setoption UCI_Elo` precedes `UCI_LimitStrength` precedes `isready` precedes `ucinewgame`; per-game `ucinewgame`+`isready` to both engines; `position startpos`+`go` routed to side-to-move; handshake-time engine_options/opponent_options applied without cross-contamination; `VirtualClock` negotiation gated on `cfg.virtual_clock && engine_caps.supports_virtual_clock`; `quit` sent on shutdown; `game_index` arithmetic and `clawfish_white = game_in_pair == 0` color-routing under `pair_index=3`. Three pure-bool helpers were extracted from `production_worker_fn` (`handshake_caps_missing`, `post_setoption_readyok_succeeded`, `either_per_game_readyok_failed`) so the otherwise-equivalent operator mutants on subprocess-failure paths become directly unit-testable via 4-row truth-table tests — minor deviation from the plan's "zero production-code changes" goal, justified per `.cargo/mutants.toml`'s preferred-approach #1 (REFACTOR over fragile line-anchored exclusions). The two function-anchored `production_worker_fn` exclusions in `.cargo/mutants.toml` are now commented out (not removed; preserved as audit trail). `cargo mutants -F 'controller::production_worker_fn'`: 16/16 caught (was 16/16 missed). `cargo mutants --in-diff` on the unit's diff: 11/11 caught. The launch_prefix-via-`/usr/bin/env` mechanism conveys per-instance env vars without any `EngineSpec` change. Plan: `docs/plans/tooling-mock-engine-fixture.md`. Unique-temp-dir generator combines PID + atomic counter + nanosecond timestamp to avoid collisions under `cargo test`'s parallel-test runner.
- **ELOH controller boundary tests + `format_pgn` sweep** — completed 2026-05-08. Test-only unit closing the deferred mutation-coverage gap at `.cargo/mutants.toml`'s `in controller::run_iteration` and `in pgn::format_pgn` exclusion blocks. Added a `run_iteration_with_watchdog` helper (ownership-transfer `std::thread::spawn` + `mpsc::recv_timeout`; `std::thread::scope` was rejected because it forces join), four hang-class watchdog tests (`bootstrap_break`, `in_flight_decrement`, `redispatch_pd_eq_tp_boundary`, `terminating_gate`), three non-hang controller boundary tests (`bootstrap_in_flight_increment_pins_drain_done`, `redispatch_pair_indices_form_strictly_ascending_sequence`, `redispatch_dispatches_exactly_total_pairs_no_overshoot`), and a parametric `format_pgn_pins_separator_and_result_spacing` sweep over n ∈ {0..=5} with strict body-shape assertions. The `mock EngineHandle` framing the original backlog item used was retired — the documented boundary mutants live in pure dispatch logic above `production_worker_fn` and close cleanly with synthetic-worker channel-recording fixtures + the watchdog. The `production_worker_fn`-side mock-engine fixture is a follow-up backlog item. Plan: `docs/plans/tooling-eloh-controller-boundary-tests.md`. Per-mutant verification: each test was confirmed by manual mutation against its target operator with revert-after-confirm cycle.
- **Doc-coverage lint (`#![deny(missing_docs)]`)** — completed 2026-04-28. Added at `src/lib.rs` crate root; ~235 doc comments backfilled across the public surface (`bitboard`, `engine`, `eval`, `fen`, `magic`, `mov`, `movegen`, `perft`, `piece`, `position`, `search`, `slow_attacks`, `square`, `uci`, `zobrist`). Module-level `//!` headers added where missing (`bitboard.rs`, `square.rs`). Originally deferred as "low ROI for an engine the user isn't reading" — revisited once the project went public on GitHub and external readers became plausible. Five chess-coder agents in parallel did the bulk of the additions; final-reviewer caught one must-fix (`Position::ep_target` doc contradicted phantom-EP sanitization) plus three should-fix and several nits, all addressed before commit.
- **LICENSE files** — completed 2026-04-28 (commit `3c6fd8e`). Dual MIT + Apache-2.0 (`LICENSE-MIT` + `LICENSE-APACHE` at repo root) — standard Rust ecosystem convention. `README.md`'s License section points to both. `Cargo.toml` is still `publish = false`.
- **CI (GitHub Actions)** — completed 2026-04-28. `.github/workflows/ci.yml` runs on push to `main`, on PRs, and on manual dispatch. Jobs: `test` matrix on `macos-14` (primary, Apple Silicon) + `ubuntu-latest` (portability) running `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test --all-targets` + `cargo test --doc`; `fuzz-check` on the separate fuzz workspace; `cargo deny` via the EmbarkStudios action (covers RustSec advisories + license + source-allowlist policy from `deny.toml` — `cargo audit` skipped as redundant); `coverage` via `cargo-llvm-cov --summary-only` (informational; no enforced threshold). All jobs cached via `Swatinem/rust-cache@v2`. Closes backlog item #1.
- **Fuzzing (`cargo-fuzz`)** — completed 2026-04-28 per ADR-0013 (`docs/decisions/0013-fuzzing-strategy.md`). `fuzz/` workspace (independent — `libfuzzer-sys` stays out of root `cargo audit` / `cargo deny` scope). Two `Arbitrary<String>` harnesses: `fuzz_fen` and `fuzz_uci`. Hand-curated seed corpora (35 fen, 30 uci) under `fuzz/corpus/<target>/`. `--sanitizer none` (no fuzzed parser path reaches `unsafe`). Cadence + commands documented under `docs/workflow.md` "Fuzzing". The first 2-hour campaigns per target are deferred to a later session — operator runs `cd fuzz && cargo fuzz run --sanitizer none <target> -- -max_len=200 -max_total_time=7200` once nightly is installed; campaign-result tables go to `docs/research/tooling-fuzzing-results.md`.

## Done in the 2026-04-27 review

- **Benchmark baseline format** — ratified at M1.G's plan-mode pass; ADR-0010 (`docs/decisions/0010-benchmark-baseline-format.md`) committed alongside the M1.G `bench/m1.g.md` first baseline. `criterion 0.7` + `--save-baseline` on per-machine `target/criterion/` (gitignored) + committed human-readable table.
- `cargo fmt --check` enforcement (commits `5ca6c86` style + `eaf9d37` workflow).
- Pre-commit hook at `.claude/hooks/pre-commit-check.sh`, wired via `.claude/settings.json`.
- `cargo audit` + `cargo deny` with policy in `deny.toml`; `Cargo.toml` marked `publish = false`.
- Documentation in `docs/workflow.md` under "Static analysis and dependency hygiene".
- **Mutation testing (`cargo-mutants`)** — backfilled across M1.A + M1.B + M1.C. Configuration in `.cargo/mutants.toml` (with `exclude_re` rules documenting the equivalent mutants). Integrated into the final-review loop per `docs/workflow.md`. Baseline run against committed code: 333 caught + 7 timeout (caught) + 47 unviable + 0 unaddressed survivors, after adding seven targeted tests (idempotency for `Bitboard::with` / `CastlingRights::with`, `Square` Debug-format, `Position::debug_assert_consistent` panic-on-broken-state, `slow_attacks::ray_attack` per-axis step pinning) and excluding eleven equivalent-mutant patterns.
- **Property-based testing (`proptest`)** — `proptest = "1.11"` wired as a dev-dependency. Eight property tests backfill M1.A/M1.B primitives: `Square` index/file/rank round-trip + out-of-range rejection (`src/square.rs`), `Bitboard` set algebra (commutativity, associativity, identity, idempotence, De Morgan, absorption) + membership/`pop_lsb`-ordering invariants (`src/bitboard.rs`), `CastlingRights` bit packing + multi-flag `has` semantics (`src/position.rs`), and `Position` ↔ FEN round-trip across randomly generated valid positions (`src/fen.rs`). Existing anchor unit tests are kept (proptest samples rather than enumerates; anchors document specific mutants killed). Test-suite review loop converged after one revision (stronger idempotence form on `with`/`without`, LSB-ascending order pin on `pop_lsb`, positive-only scope made explicit on the FEN generator).
