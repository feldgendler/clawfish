# Implementation Plan: SPSA Parameter-Tuning Harness

**Status:** plan (revised after plan-review round 1).
**Scope:** shared-infrastructure prerequisite for the top tuning-backlog lead
("Delta-baseline aspiration, TC/depth-gated + SPSA") and the broader
"ML-tuned aspiration window sizing" item. Two independently-shippable units.

**References:** `docs/research/spsa-tuning.md` (algorithm, Spall schedule,
CRN, integer rounding, stopping, starting values); `docs/tuning-backlog.md`
(delta-baseline item, ML-aspiration item); `bench/sprt/patches/item5-delta-baseline-aspiration.patch`
(the hand-picked mechanism this generalizes); ADR-0028 (aspiration/qsearch),
ADR-0037 (texel-tune external-tuner conventions + `apply` codegen precedent);
`docs/workflow.md` (TDD, blind-review, bench gate, SPRT-for-strength).

**Production HEAD:** `M6.J` + `M5.F.1`. **Bench invariant (hard gate):**
d4 = `112020`, d7 = `1354640`. Every engine-side change in Unit 1 MUST leave
these byte-identical when the new feature is OFF by default.

---

## 0. Architecture overview and unit split

- **Unit 1 (engine):** make the delta-baseline adaptive aspiration width a
  *runtime-tunable, default-OFF* feature exposed via four new UCI options. When
  OFF the code path is byte-identical to today's fixed-±50 path (bench
  unchanged). When ON it realizes the item-5 patch mechanism with the constants
  replaced by engine fields. This is what the SPSA loop perturbs.
- **Unit 2 (harness):** a new SPSA driver inside `clawfish::elo_iterate` that
  holds θ as floats, perturbs/rounds/configures two `setoption` sets against the
  *same* clawfish binary, plays a CRN paired mini-match per iteration, and steps
  θ. Generic over a named-parameter set so future tunes reuse it.

Unit 1 has no dependency on Unit 2 and can land first (it is also independently
useful: it un-hardcodes the item-5 patch behind a flag). Unit 2 depends on
Unit 1 only at *runtime* (it sets the options Unit 1 advertises); the harness
code compiles and tests independently with a mock engine.

---

## Unit 1 — engine-side runtime-tunable adaptive aspiration

### 1.1 What exists today (anchors)

- `src/search.rs:411-426` — consts `ASPIRATION_MIN_DEPTH = 6`,
  `ASPIRATION_HALF_WIDTH = 50`.
- `src/search.rs:591-599` — pure fn `aspiration_window(prior_score, depth)`.
- `src/search.rs:1258-1259` — ID loop reads `prior_score =
  last_complete.map(|(_,_,s)| s)` and calls `aspiration_window`.
- `src/search.rs:1335` — `last_complete = Some((depth, bestmove, returned))`.
  **score(d-2) is NOT currently retained.**
- `src/engine.rs:188-200` `handle_uci` (option advertisements);
  `src/engine.rs:341` `handle_setoption`; `Engine` struct fields ~`src/engine.rs:87-126`;
  consts `MAX_RANDOM_SEED`/`MAX_MOVE_OVERHEAD` at module scope ~`src/engine.rs:28-37`.
- Options thread into the worker via `self.search.lock()...` (Random_Seed
  precedent, `src/engine.rs:356`) OR via an `Engine` field read at the top of
  `handle_go` (MoveOverhead/VirtualClock precedent, `src/engine.rs:285,299`).
- `bench/sprt/patches/item5-delta-baseline-aspiration.patch` already implements
  the *mechanism* (helper `aspiration_half_width`, the second-prior-score thread,
  the `aspiration_window` signature change, and unit tests) — but with hardcoded
  consts `ASPIRATION_DELTA_K=2 / MIN=25 / MAX=250` and *always-on*. Unit 1
  reconciles with it: **keep the helper shape and tests, replace the consts with
  runtime fields, and gate the whole thing behind a default-false flag.**

### 1.2 Integer encoding of K (decision)

K is a real multiplier (~2.0, plausibly 0.1–3.0). UCI `spin` options are
integers. **Decision: fixed-point ×100 (centi-K).**

- New option `Aspiration_K` is a `spin` in **hundredths**: advertised
  `default 200 min 0 max 1000` (i.e. K ∈ [0.00, 10.00], default 2.00).
- Engine stores it as an integer field `aspiration_k_centi: i32` and computes
  the half-width with a single integer expression that rounds half-away-from-zero
  without floats:
  `half = clamp((aspiration_k_centi * |d1 - d2| + 50) / 100, MIN, MAX)`.
  The `+ 50` before integer-dividing by 100 is round-to-nearest on a
  non-negative numerator (|d1-d2| ≥ 0, k_centi ≥ 0), so the result is
  deterministic and platform-independent (no float in the hot path — preserves
  the existing all-integer aspiration arithmetic and avoids any FP-nondeterminism
  risk in bench).
- **Justification vs integer-only K:** integer-only K (1, 2, 3) cannot express
  the research's recommended c_end for K of 0.1–0.3 — SPSA needs sub-unit
  resolution on K or θ⁺/θ⁻ round to the same integer and the K-gradient is
  always zero (research §7 "c must exceed the integer grid"). ×100 gives the
  harness a grid of 0.01 per K-unit, so a c_end of ~0.2 (= 20 centi-K) clears
  the grid comfortably. The harness's UCI-encoding for K is therefore "centi-K
  integer"; the harness holds K as a float and emits `round(K*100)`.

`Aspiration_Min` and `Aspiration_Max` are plain integer centipawns
(`spin default 25 min 0 max 1000` and `spin default 250 min 0 max 2000`
respectively — wide bounds per research §7 "wide bounds preferred").

### 1.3 Engine changes — exact targets

**a. Module-scope consts (`src/search.rs`, near line 426).** Keep
`ASPIRATION_HALF_WIDTH = 50` (the OFF-path fallback and the default). Add
defaults that reproduce the item-5 hand-pick *as the ON-path defaults*:
`ASPIRATION_K_CENTI_DEFAULT: i32 = 200`, `ASPIRATION_MIN_DEFAULT: i32 = 25`,
`ASPIRATION_MAX_DEFAULT: i32 = 250`, and `ASPIRATION_ADAPTIVE_DEFAULT: bool = false`.
Do NOT keep the item-5 patch's hardcoded `ASPIRATION_DELTA_K/MIN/MAX` consts —
they become the runtime fields' defaults instead.

**b. Plumb params into the search.** The aspiration helpers are free functions
called from the ID loop inside `impl Search for AlphaBetaMover`. **Decision (not
an option): use the `set_seed`/lock precedent.** Add an `AspirationParams {
adaptive: bool, k_centi: i32, min: i32, max: i32 }` struct, store it as a field on
the search mover (alongside the existing seed state reachable via
`self.search.lock()`), and have the engine set it via the **Random_Seed
precedent** (`self.search.lock().unwrap().set_aspiration_params(...)` with a
worker-join first, since it mutates shared search state). Thread `&self.<aspiration
field>` (or copy the `AspirationParams` by value) into the ID loop so
`aspiration_window` can read it.
  - Rationale (settled, do not re-litigate at code time): the aspiration params
    are consumed *inside* the running search (not just at `go`-setup time like
    MoveOverhead), so they belong with the search state and must be set under the
    same worker-join discipline as `set_seed`/`Hash`. The `handle_go`-field
    precedent (MoveOverhead/VirtualClock) would be a latent bug here because those
    are read once at go-setup, whereas the aspiration params are read every ID
    iteration mid-search.

**c. Signature changes.**
  - Change `aspiration_window` to the item-5 signature plus the params:
    `aspiration_window(prior_score: Option<i32>, prior_prior_score: Option<i32>,
    params: &AspirationParams, depth: u32) -> (i32, i32)`.
  - **`aspiration_window` retains BOTH legacy `(-INF, INF)` early returns
    verbatim** (verified at `search.rs:592-598`): `if depth < ASPIRATION_MIN_DEPTH
    { return (-INF, INF); }` and `let Some(prior) = prior_score else { return
    (-INF, INF); };`. Only the *final* line `(prior - half, prior + half)` changes
    — it now reads `half = aspiration_half_width(prior, prior_prior_score, params)`
    instead of the const. This preservation is what makes the OFF path
    byte-identical at the spec level.
  - Add the helper (item-5's `aspiration_half_width`, generalized):
    `aspiration_half_width(score_d1: i32, score_d2: Option<i32>, params:
    &AspirationParams) -> i32`. **Gate on `params.adaptive`:** when
    `\!params.adaptive` OR `score_d2` is `None`, return `ASPIRATION_HALF_WIDTH`
    (the fixed-50 fallback — identical to today). Otherwise return
    `clamp((params.k_centi * (score_d1 - d2).abs() + 50) / 100, params.min,
    params.max)`.
  - This is the *one* place the OFF/ON branch lives. When `adaptive == false`
    the helper returns exactly `ASPIRATION_HALF_WIDTH` for every input, and the
    two early returns above are unchanged ⇒ `aspiration_window` is bit-identical
    to today for every (prior, prior_prior, depth) ⇒ **bench unchanged.**

**d. Thread score(d-2) in the ID loop (`src/search.rs` ~1258, ~1335).** Adopt
the item-5 patch verbatim: add `let mut prev_prev_score: Option<i32> = None;`
before the `for depth` loop; pass `prev_prev_score` into `aspiration_window`;
and at the completion point (just before `last_complete = Some(...)` at ~1335)
set `prev_prev_score = last_complete.map(|(_,_,s)| s);`. Preserved across
mid-iteration aborts exactly like `last_complete` (the shift happens only on
completion). This thread is *unconditional* (cheap, and harmless when OFF since
the gated helper ignores it).

**e. Engine UCI surface (`src/engine.rs`).**
  - Module-scope consts: `ASPIRATION_K_MAX = 1000`, `ASPIRATION_MIN_MAX = 1000`,
    `ASPIRATION_MAX_MAX = 2000`, plus the defaults mirroring search-side defaults.
  - `Engine` struct fields: `aspiration_adaptive: bool`, `aspiration_k_centi: i32`,
    `aspiration_min: i32`, `aspiration_max: i32`, initialized to the defaults in
    the constructor (~`src/engine.rs:123`). These are the engine's record of the
    options; on each relevant `setoption` (and once at construction / before
    `go`) they are pushed into the search mover's `AspirationParams`.
  - `handle_uci` (~line 199, after VirtualClock): advertise all four:
    `option name Aspiration_Adaptive type check default false`
    `option name Aspiration_K type spin default 200 min 0 max 1000`
    `option name Aspiration_Min type spin default 25 min 0 max 1000`
    `option name Aspiration_Max type spin default 250 min 0 max 2000`.
  - `handle_setoption` (~line 341): add four case-insensitive branches following
    the exact `random_seed`/`hash` pattern (parse → bounds-check via
    `.filter(|&n| ...)` → on success join in-flight worker + push params into
    search under the lock + clear nothing else; on failure `info_string_debug`
    rejection, state unchanged). The `Aspiration_Adaptive` branch parses
    `true`/`false` like `VirtualClock` (case-insensitive). After updating any of
    the four fields, recompute and push the full `AspirationParams` to the search
    mover so a partial update can't leave the search reading a stale combination.

**f. Cross-cut: bench command.** The `bench` UCI command must run with adaptive
OFF (the default). No code change needed — the default-false field guarantees
it — but the test plan asserts it explicitly.

### 1.4 Reconciliation with the item-5 patch

The item-5 patch (`bench/sprt/patches/item5-delta-baseline-aspiration.patch`)
is the *always-on, hardcoded* prototype. Unit 1 supersedes it: the patch's
`aspiration_half_width` helper, the `prev_prev_score` thread, and the
`aspiration_window` signature change are adopted; the patch's hardcoded
`ASPIRATION_DELTA_{K,MIN,MAX}` consts become the runtime fields' *defaults*; and
a `params.adaptive` gate is added so the default build is OFF. The patch's unit
tests (`aspiration_half_width_*`, `aspiration_window_uses_delta_width`) are
adopted with the `params` argument threaded through; add the explicit
adaptive-OFF test (1.5.a); and the project's pre-existing AS1–AS5b tests are
migrated to the new signature (see test 1.5.d2). After Unit 1 lands, the patch
file is obsolete and is deleted **in the same commit that updates the
tuning-backlog item referencing it** (so the backlog never points at a deleted
path).

### 1.5 Unit 1 test list

(TDD: tests first, blind-reviewed, then implement. Bench assertion is the gate.)

- **a. Adaptive-OFF is the fixed path (the bench-equivalence guard).**
  Unit test: for `params.adaptive == false`, `aspiration_half_width` returns
  `ASPIRATION_HALF_WIDTH` for *all* (d1, d2) inputs including
  `(Some, Some)`; and `aspiration_window(prior, prior_prior, &OFF_PARAMS, depth)`
  equals the **legacy** `aspiration_window(prior, depth)` across **all three
  legacy branches** — `depth < ASPIRATION_MIN_DEPTH (6)` ⇒ `(-INF, INF)`;
  `prior == None` ⇒ `(-INF, INF)`; else ⇒ `(prior - 50, prior + 50)`. (Oracle is
  the legacy output, NOT a blanket `prior±50` — the below-threshold and
  None-prior cases return the full window.) This is the unit-level proof that the
  OFF path is byte-identical.
- **b. Bench byte-identical (the hard gate).** Integration test (or explicit
  bench-step assertion in the plan's benchmark phase): `cargo run --release --
  bench` with no options set yields d4 = `112020`, d7 = `1354640`. Adopt the
  existing bench-regression mechanism; this is the load-bearing gate per
  `docs/workflow.md` §benchmarking.
- **c. K fixed-point arithmetic.** Unit tests on the integer expression:
  `k_centi=200, |Δ|=25 → (200*25+50)/100 = 50` (matches the legacy ±50 at the
  median Δ≈25 calibration point — same property the item-5 patch asserted);
  `k_centi=200, |Δ|=40 → 80`; clamp-to-MIN on small Δ; clamp-to-MAX on huge Δ;
  rounding boundary (`k_centi=100, |Δ|=1 → 1`; verify half-away rounding on an
  off-grid numerator).
- **d. score(d-2) threading.** Adopt item-5's `aspiration_window_uses_delta_width`;
  add: after an aborted iteration the prev_prev shift is not corrupted (the
  shift only happens on completion). A search-level test driving ≥3 ID iterations
  with adaptive ON observes a window that differs from ±50 once d-2 is available.
- **d2. Migrate existing aspiration tests to the new signature.** The signature
  change breaks compilation of the ~7 existing callers of the 2-arg
  `aspiration_window(prior, depth)`: AS1–AS5b at `search.rs:9249-9322`
  (`aspiration_window_below_threshold_is_full_window_*`,
  `..._at_threshold_with_no_prior_is_full_window`, `..._above_threshold_*`,
  `..._with_mate_score_prior_does_not_special_case`) and the search-level test at
  `~9661/9707`. Thread `&OFF_PARAMS` and `None` for `prior_prior_score` through
  each; assert **unchanged** results (this doubles as additional OFF-path
  byte-identity coverage). These are migrated, not deleted (anchor tests).
- **e. UCI option round-trip.** `handle_setoption` tests mirroring the
  `Random_Seed`/`MoveOverhead` test style (`src/engine.rs` test module ~2848):
  set each option, assert the `Engine` field updated; out-of-bounds value
  rejected (field unchanged); `Aspiration_Adaptive` true/false parse;
  case-insensitive names; `handle_uci` advertises all four with correct
  default/min/max and in a stable order.
- **f. Adaptive-ON behaves.** With `Aspiration_Adaptive=true` and defaults,
  drive a search and assert (via the existing `info string aspiration_re_search`
  / window instrumentation, `src/search.rs:1318`) that the first-try window can
  narrow below ±50 on a stable line and widen above on a volatile one.
- **g. No SPRT required to *land* Unit 1** because the default-OFF path is
  bench-proven neutral (refactor-class per `docs/workflow.md:384`). A strength
  SPRT is only run later, by the *operator*, on the ON path with SPSA-tuned
  values (and is Unit 2's confirmatory step). State this explicitly so a
  chess-coder doesn't gate Unit 1 on an SPRT.

---

## Unit 2 — the SPSA loop

### 2.1 Subcommand vs sibling binary (decision)

**Decision: a new flag-activated mode inside the existing `elo-iterate` binary**
(`clawfish::elo_iterate`), activated by `--spsa` (analogous to how `--sprt-elo0`
activates SPRT mode at the post-loop validation — `src/elo_iterate/cli.rs:483`).
The CLI is already a flat flag parser with a single `pub fn main()` dispatch
(`src/elo_iterate.rs:54`), not a clap-subcommand tree, so "subcommand" here means
a mode branch, not a new `[[bin]]`.

Rationale:
- Maximal reuse: `controller::spawn_workers` (the worker pool),
  `WorkerCmd::PlayPair` (the CRN color-swap pair loop, `controller.rs:368-455`),
  `driver::{spawn_engine, EngineSpec}`, `match_loop::play_one_game`,
  `compute_clawfish_score`, `prng::Prng` (SplitMix64), and the output
  conventions all live in this crate as `pub(crate)`. A sibling binary would
  have to re-export or duplicate them.
- The SPRT path stays untouched: `--spsa` is mutually exclusive with
  `--sprt-elo0` and the K-update/σ flags (add to the existing mutex check at
  `cli.rs:556`). The post-loop SPRT validation that Unit 2 recommends is a
  *separate* run of the harness in its existing SPRT mode, not a code path
  inside the SPSA loop.

### 2.2 The central integration constraint — per-iteration reconfiguration

**Critical finding:** today `WorkerConfig::engine_options` /`opponent_options`
are applied **once at worker-handshake time** (`controller.rs:304-311`, and the
test `production_worker_fn_applies_engine_options_during_handshake_not_per_pair`
at `controller.rs:4078` pins this). SPSA needs *different* options per iteration
(θ⁺/θ⁻ change every iteration). The existing `PlayPair` command carries no
per-pair options.

**Design: extend the per-pair command, do not reuse the SPRT controller loop.**
Add a new worker command variant (or a field on a new SPSA-specific pair command)
that carries `engine_options: Vec<(String,String)>` and
`opponent_options: Vec<(String,String)>` to be sent as a `setoption` block
*before the pair's `ucinewgame`* (the same ordering discipline already used for
`UCI_Elo` at `controller.rs:337-359`, which explicitly sends setoption before
ucinewgame within a pair). Both `EngineSpec`s point at the **same clawfish
binary** (`args.engine`), so the worker spawns two clawfish processes and feeds
θ⁺ to "engine", θ⁻ to "opponent".

To keep the SPRT path byte-for-byte unchanged, **do not modify the existing
`WorkerCmd::PlayPair` semantics or `run_iteration`.** Instead add:
- `WorkerCmd::PlaySpsaPair { pair_index, engine_options, opponent_options, tc }`
  (or extend with an `Option<PerPairOptions>` field defaulting to `None` so the
  SPRT path is unaffected and the test at `controller.rs:4078` still holds).
- A worker arm that, on `PlaySpsaPair`, sends the per-pair `setoption` block to
  each engine, `isready`-syncs, then runs the identical color-swap 2-game loop
  (`controller.rs:370-436`) and reports `GameComplete{clawfish_score}` ×2 +
  `PairComplete`. **No `UCI_Elo`/`UCI_LimitStrength` is sent** — full-strength
  self-play (this is the "clean self-play-full-strength path" the risks section
  calls out; the SPRT path's UCI_Elo block at `controller.rs:337-366` is skipped
  entirely in the SPSA arm).
- A dedicated `run_spsa(&mut pool, &args, &out_dir)` orchestration function
  (sibling to `run_iteration`, `controller.rs:503`) that owns the SPSA loop and
  dispatches `PlaySpsaPair` commands. It reuses the same
  `spawn_workers`/`WorkerReport` plumbing.

### 2.3 The SPSA core (new module `src/elo_iterate/spsa.rs`)

Pure, engine-agnostic, fully unit-testable. Holds no I/O.

- **Named parameter set (generic, reusable).**
  ```
  struct SpsaParam {
      name: String,          // UCI option name, e.g. "Aspiration_K"
      theta: f64,            // continuous internal state
      lo: f64, hi: f64,      // box constraints (in encoded units)
      c_end: f64,            // per-param perturbation scale at last iter
      encode: Encoding,      // how float θ → integer UCI value
  }
  enum Encoding { IntCp, CentiK }   // IntCp: round(θ); CentiK: round(θ) too —
                                    // θ is *already* in encoded units (centi-K
                                    // or cp), so encode = round-and-clip.
  ```
  Keeping θ in *encoded units* (centi-K for K, cp for MIN/MAX) means the
  rounding and the box-clip are uniform across params (`round(clip(θ))`), and
  c_end is specified in the same encoded units. This sidesteps the per-param
  unit mismatch cleanly and matches research §2 "per-parameter scaling" (each
  param's c_i carries its own scale).
- **Schedule (research §1–§2).** Canonical Spall gain sequences, carried via the
  two base constants `a` (global) and `c_i` (per-param) — **NOT** a per-iteration
  `R_k` shorthand. Per Fishtest convention specify the *final* values and back out
  the constants:
  - `alpha = 0.602`, `gamma = 0.101`, `A = 0.1 * N_total`.
  - `c_i = c_end_i * N^gamma` (so `c_k_i = c_i/(k+1)^gamma` equals `c_end_i` at
    `k = N-1`).
  - `a = R_end * c_end^2 * (N+A)^alpha` (so the relative factor
    `R_{N-1} = a_{N-1}/c_{N-1}^2 = R_end` at the last iteration; `c_end` here is
    the global reference scale used to anchor `a`).
  - **Each iteration computes `a_k` and `c_k_i` directly from the canonical
    closed forms** `a_k = a / (k+1+A)^alpha` and `c_k_i = c_i / (k+1)^gamma`.
    **Do NOT carry a standalone `R_k`:** the true relative factor
    `R_k = a_k/c_k^2 = (a/c^2)·(k+1)^{2gamma}/(k+1+A)^alpha` varies in `k` through
    *both* a `(k+1)^{2gamma}` numerator and the `(k+1+A)^alpha` denominator; any
    `R_k = R_end·(…)^alpha`-only shorthand silently drops the `((k+1)/N)^{2gamma}`
    term and mis-scales every non-final step. Carrying `a` and `c_i` and
    recomputing `a_k`/`c_k_i` sidesteps this entirely. (This is the one numerics
    spot the plan-review flagged — pinned by test 2.6.a at k=0, N/2, N−1.)
- **Iteration step (pure fn `spsa_step`) — the ONE canonical equation (sign
  nailed).** Inputs: current θ vector, the gain `a_k`, the per-param perturbation
  `c_k_i`, a `Δ` vector (±1 Rademacher), and the scalar `match` (defined in
  "Objective from the pair" below). For each component i:
  ```
  match = (pair_sum - 1.0) * 2.0        // ∈ [-2,+2], POSITIVE when θ⁺ wins
  theta_i += a_k * match * Delta_i / (2 * c_k_i)   // moves θ TOWARD θ⁺ (the +Δ side) when match>0
  theta_i  = clip(theta_i, lo_i, hi_i)
  ```
  This is gradient *ascent* on θ⁺'s advantage — algebraically `θ_{k+1} = θ_k −
  a_k·ĝ` with `ĝ_i = [J(θ⁻)−J(θ⁺)]/(2 c_k_i Δ_i)` and `J = −match`. The same
  equation is referenced by test 2.6.c (sign guard) and test 2.6.a (schedule).
  This is the *only* arithmetic the stochastic-loop test mocks (2.6.b).
- **Rademacher draw.** `delta_i = if (prng.next_u64() & 1) == 0 { -1.0 } else { 1.0 }`
  using the existing `Prng` (SplitMix64). Deterministic from `--seed`.
- **Perturbed engine values.** For each param: `plus = round(clip(theta_i + c_k_i*Delta_i))`,
  `minus = round(clip(theta_i - c_k_i*Delta_i))`, both also clipped to the box,
  then formatted as the UCI option string. `Aspiration_Adaptive=true` is always
  in both option sets (the tune only makes sense with the feature ON).
- **Objective from the pair.** Per research §3: aggregate the pair's two
  `clawfish_score` values (θ⁺ side). With CRN color-swap, the pair yields θ⁺'s
  total in {0.0, 0.5, 1.0, 1.5, 2.0}; map to the centered match score
  `match = (sum - 1.0) * 2.0 ∈ [−2, +2]` so a θ⁺ sweep = +2, split = 0, θ⁺
  swept = −2. **Sign convention is load-bearing** (research §8) — the test
  2.6.c validates it with a known-good direction.
- **Multiple pairs per iteration (optional `--games-per-iter`).** If
  `games_per_iter = 2*m`, play `m` CRN pairs with the *same* θ⁺/θ⁻ and *same* Δ,
  average the per-pair match scores → less-noisy objective. (Distinct from
  perturbation averaging; see parallelization §2.7.)

### 2.4 Trajectory logging and final-θ output

- **Per-iteration trajectory line** appended to `<out_dir>/spsa-trajectory.tsv`:
  `k  <theta_i...>  <plus_i...>  <minus_i...>  match  a_k  c_k_i...  pair_score`.
  (Emits the scalar step gain `a_k`, not a relative `R_k` — see §2.3's
  dropped-`(k+1)^{2γ}`-factor note.)
  Machine-parseable; this is the operator's plateau-monitoring input
  (research §5 "visual trajectory monitoring").
- **Tail-averaged final θ** (research §3/§5): output both the last θ and the
  mean of the last `T` iterates (`--tail-average T`, default e.g. 10% of N) to
  `<out_dir>/spsa-final.txt`, rounded to the integer UCI encoding, as a
  ready-to-paste `--engine-option Aspiration_K=… --engine-option Aspiration_Min=…
  --engine-option Aspiration_Max=… --engine-option Aspiration_Adaptive=true`
  block.
- **How the operator "applies" the result.** Because these are UCI options, the
  immediate "apply" is *recording the winning option values* and running a
  confirmatory SPRT (the next bullet). The *durable* apply — baking the tuned
  values into the engine defaults — is a later, optional follow-up modeled on
  ADR-0037 / `texel-tune apply` (`src/bin/texel-tune.rs:312` `cmd_apply` rewrites
  marked regions in a `data.rs`): a future `spsa apply`-style codegen could
  rewrite the `ASPIRATION_*_DEFAULT` consts in `src/search.rs` and flip
  `ASPIRATION_ADAPTIVE_DEFAULT` to true. **Out of scope for this plan** — noted
  as the realization path. For now, the deliverable is the recorded option block
  + the confirmatory SPRT.
- **Confirmatory SPRT (research §5/§9, mandatory before any strength claim).**
  The SPSA run is NOT SPRT evidence. After tail-averaging, the operator runs the
  existing harness in SPRT mode: baseline = `M5.F.1` (production) with
  `Aspiration_Adaptive=false` (or omitted), candidate = same binary with the
  tuned `--engine-option Aspiration_*` block, mixed-TC + virtual-clock, per the
  delta-baseline backlog item's validation methodology. This reuses the
  *unmodified* SPRT path — no new code.

### 2.5 CLI surface (`src/elo_iterate/cli.rs`, `Args` + `parse_args`)

New flags (added to `Args` struct ~`cli.rs:17` and `parse_args` ~`cli.rs:214`,
following the existing `--k0`/`--tau`/`--seed` parsing idioms):
- `--spsa` — activate SPSA mode (mutually exclusive with `--sprt-*` and the
  σ-convergence flags; extend the mutex at `cli.rs:556`).
- `--spsa-param NAME:THETA0:LO:HI:CEND:ENC` (repeatable) — the generic
  parameter spec. `ENC ∈ {cp, centik}`. For the aspiration tune the operator
  passes three: `Aspiration_K:200:0:1000:20:centik`,
  `Aspiration_Min:25:0:1000:4:cp`, `Aspiration_Max:250:0:2000:12:cp` (θ0/lo/hi/
  c_end values per research §9 starting config). Parsed into `Vec<SpsaParam>`.
- `--spsa-iters N` — iteration budget (each iteration = `games_per_iter` games).
- `--spsa-games-per-iter G` (default 2; even; small per research §3) — CRN pairs
  per iteration = G/2.
- `--spsa-r-end R` (default 0.002 per research §9) — global apply factor at the
  last iteration.
- `--spsa-A A` (default `0.1*N`) — Spall stability constant (override allowed).
- `--seed` — already exists (`cli.rs:478`); reused to drive Δ and color/opening
  assignment deterministically.
- `--tc` / `--tc-sample` — already exist; the SPSA loop uses the *same TC the
  confirmatory SPRT will use* (research §8 time-control dependence: tune at the
  SPRT-standard TC). Recommend a single mid-TC (e.g. 20+0.2) for the aspiration
  tune since the backlog signal is mid-TC-specific.
- `--tail-average T` — tail window for final-θ.
- Extra-options pass-through: `Aspiration_Adaptive=true` is injected
  automatically into both perturbation option sets by the SPSA driver (the
  operator does not pass it).

### 2.6 Unit 2 test list

- **a. Schedule math.** Unit-test `a_k` and `c_k_i` against hand-computed values
  for `alpha=0.602, gamma=0.101, A, N` at **three points: k=0, k=N/2, and
  k=N−1** (endpoints alone miss an `(k+1)^{2γ}` off-by-power error). Assert
  `c_k_i(N−1) ≈ c_end_i` and the `a = R_end·c_end²·(N+A)^α` back-out (i.e.
  `a_{N−1}/c_{N−1}² ≈ R_end`), plus the full `spsa_step` arithmetic
  (`a_k·match·Δ/(2·c_k_i)`) at each of the three k-points against hand-computed θ
  deltas. Assert `c_k_i ≥ 1` (integer-grid collision-safety margin,
  research §7/§8) for the K param throughout the run for the recommended config —
  a regression here silently zeroes the K gradient.
- **b. `spsa_step` arithmetic on a mocked objective (the determinism gate).**
  With a fixed `Prng` seed, the Δ sequence is reproducible; feed a *mocked*
  scalar objective (e.g. a quadratic `J(θ)` with a known minimum encoded as a
  deterministic `match_score(θ⁺, θ⁻)`), run M iterations, and assert θ moves
  monotonically toward the known optimum and the exact θ trajectory is
  bit-reproducible across two runs with the same seed. This is the "assert the
  gradient-step arithmetic on a mocked objective" requirement — **no engine
  spawned.**
- **c. Sign convention.** Mock an objective where increasing one param strictly
  helps θ⁺; assert θ for that param *increases* over iterations (catches the
  flipped-sign divergence, research §8). A toy "one param known to improve"
  validation, per research §8 mitigation. Reference the canonical
  `theta_i += a_k * match * Delta_i / (2*c_k_i)` from §2.3: with `match > 0` for
  the improving param, θ must move in the `+Δ` direction (toward θ⁺).
- **d. Rademacher draw determinism.** Same seed → identical ±1 Δ sequence;
  distinct seeds → distinct sequences (mirrors the existing
  `prng_*` golden/determinism tests, `prng.rs:44`).
- **e. Perturb/round/clip.** θ near a box boundary clips before rounding; centi-K
  encoding round-trips (`θ=200 → "200"`, `θ=215 → "215"`); MIN/MAX cp encoding;
  θ⁺ and θ⁻ never equal when `c_k_i ≥ 1` (grid guard).
- **f. Objective mapping.** Pair-score sum {0,0.5,1,1.5,2} → match {−2,−1,0,1,2};
  CRN color-swap correctness (reuse `compute_clawfish_score` truth-table style,
  `controller.rs:1819`).
- **g. CLI parse.** `--spsa-param` round-trips into `SpsaParam`; bad ENC / bad
  numeric field rejected; `--spsa` ⊥ `--sprt-elo0` mutex; `--spsa` ⊥ σ-flags.
- **h. Per-pair setoption wiring (worker arm).** Following the existing
  `production_worker_sends_setoption_when_advertised_and_flag_on` style
  (`driver.rs:1026`) and the per-pair `UCI_Elo`-ordering tests
  (`controller.rs:337`): assert the SPSA worker arm sends the per-pair
  `Aspiration_*` `setoption` block *before* `ucinewgame`, sends it to *both*
  engines with the correct θ⁺/θ⁻ split, sends **no** `UCI_Elo`/`UCI_LimitStrength`,
  and that the existing `PlayPair` (SPRT) path is unchanged (the handshake-time
  options test at `controller.rs:4078` still passes).
- **i. End-to-end SPSA smoke (`#[ignore]`-gated, spawns clawfish).** Mirror the
  existing e2e smokes (`src/elo_iterate.rs:402+`): `--spsa --spsa-iters 3
  --spsa-games-per-iter 2 --tc 1+0.05 --seed 42` against the real clawfish
  binary; assert `spsa-trajectory.tsv` has 3 rows and `spsa-final.txt` exists
  with a parseable option block. Same-seed reproducibility of the trajectory
  across two e2e runs (the Δ/opening determinism contract end-to-end).

### 2.7 Parallelization

- **Within an iteration:** the existing worker pool (`spawn_workers`,
  `controller.rs:466`) already plays a pair's two games — and, with
  `--spsa-games-per-iter > 2`, multiple CRN pairs — *concurrently* across
  workers. The SPSA driver dispatches all of an iteration's pairs, then *barriers*
  on their `GameComplete`/`PairComplete` reports before computing the gradient
  and stepping θ. So intra-iteration games are parallel; the θ-update is the
  serialization point.
- **Across iterations:** **sequential by construction** — θ_{k+1} depends on
  J(θ_k), so iteration k+1 cannot start until k's objective is in. This is the
  **wall-clock bottleneck**: with `games_per_iter=2`, only 2 games run in
  parallel regardless of `--concurrency`, so high core counts are wasted.
  Mitigations, in increasing order of complexity (the plan recommends the first):
  1. **Raise `--spsa-games-per-iter`** (e.g. 4–8) so each iteration saturates
     more workers AND reduces objective variance (research §3 signal-to-noise).
     Best simple lever; recommended default 2, operator raises to fill cores.
  2. **Multiple perturbation pairs per iteration (mini-batch SPSA):** draw P
     independent Δ's, evaluate all P θ±-pairs concurrently, average the P
     gradient estimates before one θ step. Reduces gradient variance and fills
     P×(games-per-batch) workers. More code (P-fold Δ draw + gradient average)
     but the natural way to use a big machine. **Recommend as a follow-up knob**
     `--spsa-perturbations P` (default 1), not in the first landing, to keep the
     core loop simple and reviewable.
  3. Antithetic variates (research §4): pair Δ with −Δ across two iterations.
     Variance reduction only; defer.
- **Determinism under parallelism:** the per-pair Δ draw, color assignment,
  per-pair TC sample (when `--tc-sample` is used — matching the existing
  sequential TC draw at `controller.rs:562`), and (future) opening selection are
  ALL drawn from the single seeded `Prng` *in the sequential driver*, NOT in the
  workers — so wall-clock interleaving of concurrent games does not affect
  reproducibility. **Fixed per-pair draw order: Δ, then colors, then TC** (applied
  identically for every dispatched pair so the stream is consumed in a
  `--concurrency`-independent order). The driver assigns each dispatched pair its
  Δ/colors/TC deterministically before dispatch. (Tests 2.6.b/2.6.i pin this.)

### 2.8 Opening book / CRN note

There is **no opening book today — all games start from startpos**
(`controller.rs` sets `starting_fen: None`, `GameContext` at ~430). CRN in the
current harness is realized purely by the *color-swap* within a pair (θ⁺ white /
θ⁻ black, then swap) from the *same* startpos. This already gives the
research-§4 paired-opening variance reduction *minus* the opening-diversity axis:
every pair starts from the same position, so successive iterations replay
correlated games. **Recommendation:** land Unit 2 with startpos CRN (color-swap
only) to match the existing harness and keep scope tight; note opening-book
diversification (drawing a varied start FEN per iteration, shared within the
pair) as a **follow-up** that needs a book-plumbing change in `GameContext`/
`PlayPair` and is shared with any future SPRT-book work. Record this as an
open item, not a blocker — self-play-from-startpos draw-rate inflation is a known
variance risk (see §3 risks).

---

## 3. Risks and open questions

1. **c_end calibration for real-valued K (research §9 open question).** The
   Fishtest 4cp/0.002 convention is for eval params; the right c_end for a
   centi-K multiplier is undocumented. **Mitigation (operator pre-flight, not
   code):** before the full run, a sanity SPRT of (K+c_end) vs (K−c_end) with
   MIN/MAX fixed, ~200 games, targeting a 2–5 Elo gap (research §9 calibration).
   The harness supports this directly (it's just a 2-config SPRT in existing
   SPRT mode). Plan surfaces this as a required pre-flight step in the operator
   runbook, with `c_end(K)=20 centi-K` as the starting guess.
2. **Self-play draw-rate inflating objective variance.** Closely-matched θ⁺/θ⁻
   self-play from a single startpos draws heavily; high draw rate ⇒ most pair
   match-scores are 0 ⇒ weak gradient signal (research §3). Mitigations:
   (a) c large enough for a measurable Elo gap (risk 1); (b) higher
   `--spsa-games-per-iter`; (c) the deferred opening-book diversification (§2.8);
   (d) the deferred mini-batch (§2.7.2). The first run should over-budget
   iterations (research §9: 10k–20k) to average through the draw noise.
3. **UCI_Elo machinery needs a clean full-strength self-play path.** The SPRT
   worker loop hard-sends `UCI_Elo`/`UCI_LimitStrength` per pair
   (`controller.rs:337-366`). The SPSA worker arm MUST skip this entirely
   (full-strength). This is why §2.2 adds a *separate* `PlaySpsaPair` arm rather
   than reusing `PlayPair` — verified by test 2.6.h (no UCI_Elo emitted). Open
   check for plan-review: confirm clawfish does not *require* UCI_LimitStrength
   to be explicitly set false (it defaults full-strength — the existing
   match-mode e2e at `elo_iterate.rs:739` runs full-strength self-play with only
   Random_Seed, so this is established).
4. **Sign-convention bug (research §8) = silent divergence.** Hardest failure
   to detect at the engine level (θ just drifts to a box boundary). Test 2.6.c
   (mocked monotone-improvement objective) is the guard; the trajectory log lets
   the operator catch a drift-to-boundary early (research §5 poisoned-params).
5. **Schedule numerics back-out (`a`/`R_end`/`c_end` algebra).** The one spot
   where an off-by-power error silently mis-scales every step. **Resolved in §2.3
   (plan-review round 1):** carry the base constants `a` and `c_i` and recompute
   `a_k = a/(k+1+A)^α`, `c_k_i = c_i/(k+1)^γ` canonically each iteration — do NOT
   evolve a relative `R_k` (the dropped-`(k+1)^{2γ}`-factor trap). Pinned by
   test 2.6.a at k=0, N/2, N−1.
6. **Bench gate is on Unit 1 only.** Unit 2 touches no engine search code ⇒
   bench is structurally unaffected; the gate is satisfied by Unit 1's
   default-OFF proof (test 1.5.a/1.5.b). State this so the harness work isn't
   incorrectly gated on a bench re-run.
7. **Stopping is manual (research §5).** No convergence test is implemented;
   the first landing uses a fixed `--spsa-iters` budget + tail-averaging +
   trajectory monitoring + the mandatory confirmatory SPRT. A periodic
   hold-out-SPRT auto-stop (research §5) is a deferred enhancement.
8. **Local optima / param interaction (research §8).** The 3 aspiration params
   are a small bounded space; the confirmatory SPRT (final θ vs `M5.F.1`) is the
   composition check. A few grid-probe SPRTs near the SPSA optimum are an
   optional operator sanity step (research §8), not harness code.

---

## 4. Sequencing and parallelization of the implementation work

- **Unit 1 and Unit 2 can be built in parallel by separate chess-coder agents**
  (Unit 1 = engine; Unit 2 = harness). Unit 2's only Unit-1 dependency is the
  *option names/encoding* (§1.2), which this plan fixes, so Unit 2's code and
  mock-engine tests do not block on Unit 1's implementation. The e2e smoke
  (2.6.i) is the only cross-unit gate and runs last.
- **Within Unit 2**, `spsa.rs` (pure core + tests 2.6.a–g) is independent of the
  controller worker-arm change (§2.2, tests 2.6.h) and can be a third parallel
  workstream; they meet at `run_spsa` (§2.2) and the e2e smoke.
- Per `docs/workflow.md`: each unit's plan → test-suite → implementation each
  goes through its own blind-review loop; this document is the input to the
  plan-review loop for both units.

---

## Critical Files for Implementation

- `src/search.rs` (aspiration consts/fields/helpers + ID-loop score(d-2) thread; Unit 1)
- `src/engine.rs` (four UCI options: advertise + parse + thread into search; Unit 1)
- `src/elo_iterate/controller.rs` (`PlaySpsaPair` worker arm + `run_spsa`; per-pair setoption; Unit 2)
- `src/elo_iterate/spsa.rs` (NEW — pure SPSA core: schedule, Rademacher, step, encoding; Unit 2)
- `src/elo_iterate/cli.rs` (`--spsa*` flags + mutex with `--sprt-*`; Unit 2)
