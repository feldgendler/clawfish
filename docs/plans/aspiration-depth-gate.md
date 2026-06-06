# Plan: depth-gated adaptive aspiration (delta-baseline lever 1)

**Status:** drafting → plan-review.
**Parent item:** `docs/tuning-backlog.md` §"Delta-baseline aspiration, TC/depth-gated + SPSA", **lever 1** (TC-/depth-gate the volatility width). Lever 2 (SPSA-tune K/MIN/MAX) was ruled out empirically low-signal on 2026-06-06 (harness shakedown + calibration; see backlog "Update 2026-06-06 (later)").
**Baseline:** `M5.F.1` (production HEAD). **Builds on:** Unit 1 (`fd1be1a`, runtime-tunable adaptive aspiration via `Aspiration_*` UCI options).

## 0. Outcome — CLOSED NEGATIVE (2026-06-06)

The `[8,12]`-banded candidate SPRT'd **≈ −34.9 Elo over 800 games (2 seeds, both
CIs fully negative)** vs `M5.F.1`. The band hypothesis is **refuted**: the
regression concentrates in the 20+0.2 bucket the band was designed to protect.
Lever 1 closed, no band re-pick (M5.I lesson). Code `89e4dad` kept as a
default-OFF runtime feature (bench byte-identical). Full writeup:
`bench/sprt/2026-06-06-depth-gate-aspiration-vs-m5f1.md`; backlog updated. The
sections below are the as-executed plan, retained for the record.

## 1. Motivation

The 2026-06-03 delta-baseline candidate (`half = clamp(K·|score(d-1)−score(d-2)|, MIN, MAX)`, K=2/MIN=25/MAX=250) SPRT'd **combined +13.03 [−3.78, +29.91]** vs `M5.F.1` — rung-2 ship-with-note, no-ship by strict CI-lower>0. The net was dragged borderline by the **depth extremes**: robustly positive at 20+0.2 (+23/+20 across two seeds, median depth ~11.5) but **flat-to-negative at 10+0.1 (too shallow for a stable d-1/d-2 delta) and 60+0.6 (deep enough that fixed-±50 already suffices)**.

**Lever 1 hypothesis:** confining the adaptive width to a mid-depth band — fixed-±50 at shallow and deep — keeps the 20+0.2 win while removing the extreme-bucket drag, potentially clearing CI-lower>0.

## 2. Mechanism

Add a tunable **adaptive-depth band** `[adaptive_min_depth, adaptive_max_depth]` to `AspirationParams`. The adaptive half-width formula applies **only when `adaptive_min_depth ≤ depth ≤ adaptive_max_depth`**; outside the band (but still at `depth ≥ ASPIRATION_MIN_DEPTH`, where aspiration is active at all), fall back to the fixed `ASPIRATION_HALF_WIDTH` (±50).

This is the minimal generalization of Unit 1: Unit 1's adaptive path is the special case `band = [ASPIRATION_MIN_DEPTH, ∞)`.

### 2.1 Threading `depth` into the half-width decision

`aspiration_half_width(score_d1, score_d2, params)` currently has no `depth` argument; the band check needs it. Two options:
- **(A) Thread `depth` into `aspiration_half_width`** — add a `depth: u32` parameter; the band gate lives inside, alongside the existing `!adaptive` / `score_d2.is_none()` early-returns.
- **(B) Gate at `aspiration_window`** — `aspiration_window` already has `depth`; compute `half` as adaptive only when in-band, else `ASPIRATION_HALF_WIDTH`.

**Choose (A).** Keeps all "what half-width?" logic in one pure function with one test surface; `aspiration_window` stays a thin window-former. The band gate becomes a third early-return in `aspiration_half_width`.

### 2.2 New `AspirationParams` fields + defaults

```
pub adaptive_min_depth: u32,   // UCI Aspiration_AdaptiveMinDepth, default = ASPIRATION_MIN_DEPTH (6)
pub adaptive_max_depth: u32,   // UCI Aspiration_AdaptiveMaxDepth, default = ASPIRATION_ADAPTIVE_MAX_DEPTH_DEFAULT
```

Defaults must preserve two invariants:
1. **adaptive=false** ⇒ byte-identical to pre-Unit-1 baseline (band fields never consulted; the `!adaptive` early-return precedes the band check).
2. **adaptive=true, default band** ⇒ byte-identical to Unit 1's ungated adaptive (so the existing +13.03 candidate is still reproducible from the binary). ⇒ default band must be a no-op gate.

`ASPIRATION_ADAPTIVE_MAX_DEPTH_DEFAULT = MAX_PLY as u32` (= 64); `adaptive_min_depth` default = `ASPIRATION_MIN_DEPTH = 6` (coincides with the existing aspiration floor, so no shallow nodes are newly gated). Together: default band = `[6, 64]` = Unit 1 behavior.

**Why `MAX_PLY` (64), not `u32::MAX`:** `max_depth_from_limits` (search.rs) hard-clamps the ID-loop depth to `MAX_PLY - 1 = 63` on **every** path (explicit `go depth`, infinite, movetime, mate, time-based) — `depth > 64` is *structurally impossible*, not merely improbable (pinned by `id_explicit_depth_100_does_not_panic_on_pv_indexing`). So `[6, 64]` covers the entire reachable depth domain `6..=63`, a guaranteed no-op gate, while keeping the UCI spin honest (advertised `max` == stored max, no `u32::MAX` sentinel to map in `handle_setoption`). **Tie the const to the symbol — `ASPIRATION_ADAPTIVE_MAX_DEPTH_DEFAULT = MAX_PLY as u32`** — so a future `MAX_PLY` bump keeps the default band a no-gate by construction (otherwise iterations 65+ would silently revert to fixed-50 even with the "default" band). ✓

### 2.3 `aspiration_half_width` new form

```
fn aspiration_half_width(score_d1: i32, score_d2: Option<i32>, params: &AspirationParams, depth: u32) -> i32 {
    let Some(d2) = score_d2 else { return ASPIRATION_HALF_WIDTH; };
    if !params.adaptive { return ASPIRATION_HALF_WIDTH; }
    if depth < params.adaptive_min_depth || depth > params.adaptive_max_depth {
        return ASPIRATION_HALF_WIDTH;   // out of band → fixed
    }
    ((params.k_centi * (score_d1 - d2).abs() + 50) / 100).clamp(params.min, params.max)
}
```

Early-return order is load-bearing: `score_d2.is_none()` and `!adaptive` come **before** the band check so the OFF-path is untouched and the no-d2 first-adaptive-iteration still returns fixed.

**Inverted-band degenerate** (`adaptive_min_depth > adaptive_max_depth`): the `depth < min || depth > max` predicate is then always true ⇒ adaptive permanently off ⇒ fixed-50 everywhere. This is **benign** (falls back to baseline behavior). Accepted by construction ("inverted band ⇒ adaptive off"), documented with a one-line comment + DG8 test rather than rejected at setoption (the SPRT is driven by the known-good `[8,12]`, so no operator footgun in practice).

### 2.4 SPRT candidate config (the hand-pick)

Per the per-TC analysis (20+0.2 wins at median depth ~11.5; 60+0.6 loses at deeper; 10+0.1 flat at shallower), **v1 candidate band = `[8, 12]`**:
- `Aspiration_AdaptiveMinDepth = 8` — revert depths 6–7 to fixed-±50 (shallow, the 10+0.1 region's low end).
- `Aspiration_AdaptiveMaxDepth = 12` — revert depth 13+ to fixed-±50 (deep, the 60+0.6 region).
- K/MIN/MAX unchanged at the SPRT'd 200/25/250.

Rationale documented as a hand-pick (SPSA can't tune the band cheaply — same low-signal finding). The band is a single SPRT candidate, not a sweep; if flat, the item closes (search layer confirmed at local optimum).

## 3. Engine plumbing (mirror Unit 1)

`src/engine.rs`:
- Two new `Engine` fields `aspiration_adaptive_min_depth` / `_max_depth` (u32), defaulted to the search consts.
- Two new UCI options advertised in `handle_uci`: `Aspiration_AdaptiveMinDepth` (spin, `default 6 min 6 max 64`) and `Aspiration_AdaptiveMaxDepth` (spin, `default 64 min 6 max 64`). The UCI value is stored literally as `adaptive_{min,max}_depth` — no sentinel. The `max 64` literal tracks `MAX_PLY`; `ASPIRATION_ADAPTIVE_MAX_DEPTH_DEFAULT = MAX_PLY as u32` (= 64) is a true no-gate over the reachable domain (see §2.2 for the structural-clamp guarantee).
- Two new `handle_setoption` branches that **reject** out-of-range values (`.filter(|&n| (ASPIRATION_MIN_DEPTH..=MAX_PLY as u32).contains(&n))` → on miss, `info string ...: rejected value '{v}'`, field unchanged) — mirroring the existing `aspiration_k`/`_min`/`_max`/`random_seed`/`moveoverhead` branches. **Not** clamping: the entire codebase setoption convention is reject-and-leave-unchanged, and the spin `min 6 max 64` already steers well-behaved GUIs into range; the reject path is the malformed-input guard.
- Extend `push_aspiration_params` to carry the two new fields under the worker-join lock.

Advertise the spins as `Aspiration_AdaptiveMinDepth default 6 min 6 max 64` and `Aspiration_AdaptiveMaxDepth default 64 min 6 max 64` (the `max` literal tracks `MAX_PLY`). The §2.2 no-gate invariant holds because the engine never searches past depth 63.

## 4. Test plan (TDD, blind test-suite review)

`src/search.rs` unit tests (extend the AS-series + Unit-1's adaptive tests):
- **DG1** band-interior: adaptive value returned when `adaptive_min_depth ≤ depth ≤ adaptive_max_depth` (e.g. depth 10, band [8,12], large delta ⇒ adaptive half ≠ 50).
- **DG2** below band: `depth = adaptive_min_depth − 1` (e.g. 7, band [8,12]) ⇒ fixed-50 even with adaptive=true + large delta.
- **DG3** above band: `depth = adaptive_max_depth + 1` (e.g. 13, band [8,12]) ⇒ fixed-50.
- **DG4** band boundaries inclusive: depth == min and depth == max both return adaptive (closed interval).
- **DG5** default-band no-op: `adaptive=true`, default band [6,64], depth across 6..20 ⇒ identical to Unit-1 ungated adaptive (regression-guards the +13.03 reproducibility).
- **DG6** OFF-path untouched: `adaptive=false`, any band, any depth ⇒ fixed-50 (the `!adaptive` early-return precedes the band check).
- **DG7** `aspiration_window` integration: full window below `ASPIRATION_MIN_DEPTH`; in-band adaptive vs out-of-band fixed produce the expected `(prior±half)`.
- **DG8** inverted-band degenerate: band `[12, 8]` ⇒ fixed-50 at every depth (documents the accepted degenerate per §2.2).
- **Unit-1 `aspiration_half_width` direct-call migration:** add the `depth` arg to **every existing call** of `aspiration_half_width` — `adaptive_off_half_width_*`, `adaptive_on_half_width_k_centi_arithmetic`, `adaptive_on_half_width_extreme_inputs_no_overflow`, the `min`/`max` clamp tests — passing an **in-band depth (e.g. 10)** so the default `[6,64]` band does not alter their existing expectations. (The `aspiration_window`-level tests AS1–AS5b already carry a `depth` arg and need no signature change — only re-verify their asserted windows still hold.) DG5 separately pins that an in-band depth reproduces the exact pre-migration adaptive value. **Hazard:** if any of these is migrated with a depth < 6, the new band gate fires and the adaptive-value assertion silently breaks — do not "fix" by weakening the assertion; fix the depth.

`src/engine.rs` tests:
- `setoption` round-trip for both new options (parse, reach search via a behavioral go-driven test that kills the `push_aspiration_params` mutant — mirror Unit 1's `setoption_aspiration_adaptive_true_reaches_search_and_changes_behavior`). **The behavioral test must drive `go` to a depth that lies _inside_ the configured band** (e.g. band `[6, 8]` with the depth-7 KPK fixture, so the searched iteration is in-band and the adaptive width can diverge from ±50) — otherwise banded-on is byte-identical to OFF and `assert_ne!(nodes_off, nodes_on)` fails.
- **Excludes-the-depth equality test** (the one that actually pins the gate end-to-end): band `[8, …]` (i.e. `AdaptiveMinDepth=8`) with the depth-7 fixture ⇒ assert node-count **equality** with adaptive-OFF (the searched depth is out of band ⇒ fixed-50 ⇒ no divergence). This kills the "band gate ignored" mutant.
- **Reject tests:** below-`ASPIRATION_MIN_DEPTH` and above-`MAX_PLY` inputs are **rejected** (field unchanged; `info string ...: rejected value` under debug-on), matching the `random_seed`/`moveoverhead`/`aspiration_*` reject precedent — not clamped.

## 5. Verification gates

- `cargo build --release`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`.
- `cargo test` (lib + integration).
- **Bench byte-identity:** d4 `112020` / d7 `1354640` UNCHANGED (adaptive default-OFF ⇒ no search-path change). This is the hard gate.
- `cargo mutants` scoped to `aspiration_half_width` + the new engine branches; survivors classified (equivalent-over-domain documented, genuine gaps closed).
- Blind final review (code+tests jointly).

## 6. SPRT (the strength claim)

After gates + review: mixed-TC + virtual-clock SPRT, **candidate = `[8,12]`-banded adaptive (Aspiration_Adaptive=true, AdaptiveMinDepth=8, AdaptiveMaxDepth=12, K/MIN/MAX=200/25/250)** vs **`M5.F.1`** (adaptive OFF). 400-game cap, elo1=10, ≥2-seed confirm on a positive signal. Decision rule per the ADR-0037 ship-rung ladder: CI-lower>0 ⇒ ship (rung-1); else rung-2 "small-but-not-regression" ship-with-note as the fallback per the parent item. Per-TC buckets reported to confirm the mechanism (expect: 20+0.2 retains gain, 60+0.6 no longer negative).

**Outcome dispositions:**
- **CI-lower>0** ⇒ ship: flip relevant defaults / document the banded config as the new candidate; new production HEAD + bench.
- **Rung-2 positive** ⇒ surface to user (same ship-with-note decision as the ungated candidate, now with the extreme-drag removed).
- **Flat** ⇒ **close the item — do NOT re-pick band values** (no `[9,11]`/`[7,13]` retry). Lever 1 + lever 2 both exhausted ⇒ delta-baseline aspiration confirmed not-a-ship at current strength/TC; search layer at local optimum (consistent with the 3 zero-ship campaigns). Per the M5.I lesson, iterating band values against a flat (null-signal) baseline is random search through the tunable space — the single `[8,12]` candidate is the one shot.

## 7. Parallelization

Small single-file-pair change (search.rs + engine.rs, tightly coupled via `push_aspiration_params`). **Serial, single coder** — no parallel decomposition warranted. The SPRT is the long pole and runs unattended after the code lands.

## 8. Risks

- **Bench drift:** any non-byte-identical bench ⇒ a default-path leak; the §2.2 invariants + DG5/DG6 guard this. Hard-gate on d4/d7.
- **Band hand-pick is unvalidated:** acknowledged; the SPRT is the validator, single candidate. If flat, item closes (not iterated — per the M5.I lesson, iterating band values against a null signal is random search).
- **Depth semantics:** confirm the `depth` passed to `aspiration_window` in the ID loop is the iteration's target depth (the value the window seeds for), not depth−1. Test DG7 + a call-site read pin this.
