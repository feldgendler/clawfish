# ADR-0025 — Late move reductions: gate set, reduction formula, re-search policy, and TT/history interaction

**Status:** Accepted (lands with M5.C).

## Context

M5.C adds **late move reductions (LMR)** to the negamax move loop. Once move ordering is already strong (TT move at index 0 via the step-12 reorder, killers next, history-sorted quiets after that), later quiets are less likely to be best moves, so they can be searched at reduced depth first and only re-searched at full depth when the reduced result still beats alpha.

This phase layers on top of:

- ADR-0018 transposition-table semantics (the §6 TT-store rule is the load-bearing interaction).
- ADR-0019 history heuristic and `quiets_searched` semantics (the §7 history rule narrows the M4.C "tried-and-failed" notion).
- ADR-0023 null-move pruning (the prologue cuts that compose with the move-loop reductions).
- ADR-0024 reverse futility pruning (same).

Prior-art survey: `docs/research/m5-late-move-reductions.md`.
Plan and test surface: `docs/plans/m5.c.md`.

## Decision

### 1. Scope: quiet moves only

LMR applies only to quiet moves inside the negamax move loop. Captures, promotions, TT-move handling, qsearch, and root move ordering are unchanged. Quiet checking moves are not specially handled in v1 (they take the same LMR path as any other quiet); the cost of adding a "gives check" classification at the move-loop decision point is not justified for v1.

### 2. Node-level gate

LMR is enabled only when all four conditions hold (computed once before the move loop, not inside it):

```rust
ply > 0
&& !is_pv
&& depth >= LMR_MIN_DEPTH
&& !in_check(pos)
```

| Gate | Why |
|---|---|
| `ply > 0` | Structural-root guard. Defense-in-depth against a future PVS refactor that would change `is_pv`'s semantics. Parallels ADR-0023 §3 / ADR-0024 §3. Listed first so the cheapest predicate fails fast. |
| `!is_pv` | Preserve PV exactness. |
| `depth >= LMR_MIN_DEPTH` | A reduced child must still have a useful depth budget. |
| `!in_check` | Evasions are tactically dense; reducing them risks missing forced sequences. |

### 3. Per-quiet skip policy

A quiet is LMR-eligible only when its `quiet_index` (the node's quiet-only ordinal, 1-based) is at least `LMR_MIN_QUIET_INDEX`, it is not in either killer slot, and its butterfly-history score is strictly below `LMR_HIGH_HISTORY_THRESHOLD`.

The TT move is implicitly exempt: after the step-12 reorder it is the first searched move (ADR-0018 §12 pins TT-move-at-index-0); if it is a quiet it receives `quiet_index = 1` and the floor at `LMR_MIN_QUIET_INDEX = 2` rules it out without an explicit TT-move parameter being threaded through `is_lmr_eligible_quiet`.

Pinned constants:

```rust
pub(crate) const LMR_MIN_DEPTH: u32 = 3;
pub(crate) const LMR_MIN_QUIET_INDEX: u32 = 2;
pub(crate) const LMR_HIGH_HISTORY_THRESHOLD: i16 = 4_096;

const _: () = assert!(LMR_HIGH_HISTORY_THRESHOLD as i32 <= MAX_HISTORY as i32);
```

The compile-time assertion pins `THRESHOLD <= MAX_HISTORY = 16384`, killing a refactor that silently disables the skip by raising the threshold above the cap.

### 4. Reduction formula

Pinned v1 constants:

```rust
pub(crate) const LMR_BASE_OFFSET: f64 = 0.99;
#[allow(clippy::approx_constant)] // conservative-band placeholder; SPRT-tunable
pub(crate) const LMR_LOG_DIVISOR: f64 = 3.14;
```

`LMR_LOG_DIVISOR = 3.14` is a memorable placeholder in the conservative band `[~2.9, ~3.5]` — the probe table at v1 constants in plan §3.1 is invariant across that range. π provides only the mnemonic; the value carries no theoretical weight and is the first target of the §"Open SPRT-tunable parameters" row 1 joint re-fit.

Reduction:

```rust
R = floor(LMR_BASE_OFFSET + ln(depth) * ln(quiet_index) / LMR_LOG_DIVISOR)
R = R.clamp(0, depth - 2)        // reduced child is always at least depth 1
```

Domain guard: `late_move_reduction(depth, quiet_index)` returns `0` for `depth < LMR_MIN_DEPTH` or `quiet_index < LMR_MIN_QUIET_INDEX`. The helper is `pub(crate)` and unit-tested at the boundaries; do not rely on the call-site gate alone. The move loop treats `R == 0` as "skip the LMR path entirely" — preventing the doubled-work pattern under any future tuning that pushes the formula's floor down.

### 5. Re-search policy

Eligible quiets are searched first at `depth - 1 - R`.

If the reduced result is strictly above alpha (`reduced_score > alpha`, not `>=`), the move is immediately re-searched at full depth `depth - 1`. The classical rule: a fail-low at exactly alpha did not improve the bound, so no re-search is required.

If the reduced result stays at or below alpha, the reduced result stands and the move is marked **reduced-only** for TT/history bookkeeping. Re-search depth is always `depth - 1` in v1; adaptive variants (e.g., `depth - 1 - R/2`) are deferred (§8).

### 6. TT store discipline (load-bearing correctness commitment)

TT storage is suppressed when the node's final best score is backed only by reduced-only evidence. Helper:

```rust
fn tt_bound_for_completed_node(
    best: i32,
    beta: i32,
    original_alpha: i32,
    best_is_full_depth: bool,
) -> Option<TtBound>;
```

`None` (suppress store) when `best_is_full_depth == false`; otherwise the existing Lower / Exact / Upper classification.

**Why suppression is a real correctness call, not a tautology.** Walk the three TT-bound shapes:

- **Lower** requires `best >= beta`. To reach `best`, a move's accepted score must beat the running `best` (and thus eventually `alpha`). Under §5, any reduced-only score `> alpha` triggers a full-depth re-search; the score that lands in `best` is that re-search result, which is full-depth-witnessed. So a Lower bound is always full-depth-proven.
- **Exact** requires `best > original_alpha && best < beta`. Same argument as Lower for the `best > original_alpha` half: only a full-depth re-search produces a score that updates `best` past `original_alpha`. Exact is always full-depth-proven.
- **Upper** requires `best <= original_alpha`. This is the case suppression actually changes. A reduced search at depth `d - 1 - R` proved `score <= alpha`; this does NOT prove `score <= alpha` at the parent's claimed depth `d`, because a deeper search can find new tactical resources that improve the score. Storing an Upper entry tagged with depth `d` would be *false advertising*: a future probe at depth `d` could legitimately re-use the entry without triggering a full search, propagating an under-tight bound. We prefer the suppression: occasionally re-search the position later, never store an inflated-depth bound.

`best_is_full_depth_after_score` carries strict-improvement semantics: a `score > best` always replaces the flag with the new move's provenance; equal-score ties OR the flag (a later full-depth witness restores eligibility); `score < best` leaves the flag unchanged. The "first move at any node carries `quiet_index = 1`, never reduced" invariant from §3 (combined with `LMR_MIN_QUIET_INDEX = 2`) ensures the corner where strict-improvement could lose information is impossible by construction.

The alternative — store at the *actual* probe depth `d - 1 - R` — is theoretically sound but complicates `entry.depth` semantics across the rest of the search and is harder to argue against ADR-0018 §13's bound-handling rules. Deferred (§8).

### 7. History interaction

Reduced-only quiets (those that took LMR's first pass and did NOT trigger the full-depth re-search) **do not enter `quiets_searched`**. Quiets that were re-searched at full depth follow ADR-0019 semantics unchanged.

Rationale: ADR-0019 §3 includes killers in `quiets_searched` because killers are *full-depth* tried-and-failed signals; the malus has matching evidentiary weight. Reduced-only LMR probes are *coarser-depth* tried-and-failed signals — including them at full `+depth*depth` magnitude over-trains history on noisy probes. The principled alternative would weight the malus by the actual probe depth (e.g., `(depth - 1 - R)^2`); deferred (§8). The v1 binary choice is **exclude**, narrowing ADR-0019 §3's "tried-and-failed" notion to "full-depth tried-and-failed" within LMR-eligible nodes.

The cutter at any node is by construction full-depth-witnessed: a beta-cutoff requires `score >= beta > alpha`, and §5 forces a full-depth re-search on any reduced score `> alpha`. So the cutter passed through `update_history_on_quiet_cutoff` is always a full-depth result; the `+depth*depth` bonus is not contaminated by reduced-only probes. ADR-0019 §3's history-update soundness extends to the LMR-modified loop without modification.

### 8. Out of scope (deferred)

- Depth- or improving-aware reduction adjustments.
- Capture / promotion reductions.
- PVS-coupled LMR (zero-window verification searches).
- Verification-search variants beyond `reduced_score > alpha`.
- LMR in qsearch.
- "Reduce less" for trusted quiets (instead of skip-entirely).
- Adaptive re-search depth (e.g., `depth - 1 - R/2`).
- Stockfish-style "store at the actual reduced depth" TT semantics.
- Depth-weighted history malus for reduced-only quiets.

## Open SPRT-tunable parameters

Post-M5.C campaigns:

1. **`LMR_BASE_OFFSET` and `LMR_LOG_DIVISOR`** jointly — re-fit the formula to the post-M5.C tree. Probe table at v1 constants in plan §3.1.
2. **`LMR_MIN_DEPTH`**: compare 2 / 3 / 4.
3. **`LMR_MIN_QUIET_INDEX`**: compare 2 / 3.
4. **`LMR_HIGH_HISTORY_THRESHOLD`**: compare 2048 / 4096 / 8192, or replace the binary skip with a "reduce less" rule (R−1 instead of skip).
5. **`quiets_searched` policy for reduced-only quiets**: compare exclude (v1) / include / depth-weighted (`(depth - 1 - R)^2`).

The v1 commitments above are conservative and chosen for **clean SPRT attribution**, not for proven optimality.

## Consequences

**Positive:**

- Bench node count drops substantially on the local implementation benchmark: `3355270 → 1651610` nodes (`-50.8%` vs M5.B). Sits inside the plan §3.1 expectation band of −30 to −55%.
- Composes naturally with M5.A/M5.B: prologue pruning still runs before the move loop, and TT/history ordering still decides which quiets are considered late.
- No signature changes. No new UCI options. No changes to `src/mov.rs`, `src/position.rs`, `src/tt.rs`, `src/eval.rs`, or movegen.

**Negative:**

- Search-tree shape changes substantially. Exact-count tests that observe NMP firings or aspiration behaviour may need legitimate re-pins; one NMP-firings pin moved 34 → 2 and one aspiration test was narrowed at landing.
- TT storage becomes provenance-sensitive: reduced-only best scores are intentionally discarded until a full-depth witness exists.

**Migration paths for deferred variants:**

- **Adaptive re-search depth**: change the §5 `if lmr_needs_full_research(...)` arm to call `search_child` at `depth - 1 - reduction.saturating_sub(K)` for some tuned K instead of `depth - 1`; no other change required.
- **Depth-weighted history malus for reduced-only quiets**: change the `move_is_full_depth = false` arm in the move loop to push the move into a separate `reduced_quiets_searched: MoveList` carrying its `child_depth`; extend `update_history_on_quiet_cutoff` to consume both lists with per-list bonus weights.
- **Stockfish-style TT depth tagging**: change the §6 helper's `None` arm to `Some(TtBound)` carrying the actual probe depth; re-validate ADR-0018 §13 / §7 invariants against the new per-bound depth provenance.

## Test surface (lands with M5.C)

- **Pure helper tests**: `late_move_reduction` boundary + monotonicity + clamp tests; `is_lmr_eligible_quiet` per-arm rejection + acceptance tests; `lmr_needs_full_research` strict-greater-than-alpha; `tt_bound_for_completed_node` suppression + classification + the §6 worked-case Lower-arm anchor; `best_is_full_depth_after_score` equal-score upgrade + strict-improvement provenance replacement; `update_history_on_quiet_cutoff` direct +d² / -d² pin.
- **Move-loop behavior tests**: PV / ply=0 / in-check / depth-below-min skip tests with anti-vacuous sister assertions; first-quiet-after-captures skip; killer-equality skip at quiet_index=2 (load-bearing — both killer slots seeded so the killer-arm is exercised, not the first-quiet arm); high-history skip at quiet_index=2 (same load-bearing pattern); reduces-when-eligible firing test; full-depth re-search firing test; behavioral no-re-search test pinning the move-loop wiring of `lmr_needs_full_research`; reduced-only quiet exclusion from `quiets_searched`; full-depth quiet inclusion in `quiets_searched` (anti-vacuous sister).
- **Integration**: `tests/uci_integration.rs::bench_signature_deterministic_across_two_runs_with_lmr` (E50) pins the depth-4 bench signature at `130884` and asserts determinism across two consecutive runs with LMR active. E49 (M5.B's pin) drops its value pin and tests determinism only.
