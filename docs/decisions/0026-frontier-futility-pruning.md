# ADR-0026 — Frontier futility pruning: gate set, margin table, fail-soft policy, history/TT discipline

**Status:** Accepted (lands with M5.D).

## Context

M5.D adds **frontier futility pruning (FFP)** to the negamax move loop. Inside the move loop at shallow depth (`depth ≤ FFP_MAX_DEPTH`), before recursing into a quiet move, FFP checks whether `static_eval + margin(depth) ≤ alpha`. If yes, the move is skipped — its true score cannot improve alpha given the proved positional ceiling.

This phase layers on top of:

- ADR-0018 transposition-table semantics (no TT store on FFP fire; the end-of-loop TT-store path is unchanged).
- ADR-0019 history heuristic and `quiets_searched` semantics (the §6 history rule extends the M5.C reduced-only exclusion to FFP-pruned quiets).
- ADR-0023 null-move pruning, ADR-0024 reverse futility pruning, ADR-0025 late move reductions (the prologue cuts and per-quiet decisions FFP composes with).

Prior-art survey: `docs/research/m5-frontier-futility.md`.
Plan and test surface: `docs/plans/m5.d.md`.

## Decision

### 1. Scope: quiet moves only, per-move skip

FFP applies only to quiet moves inside the negamax move loop. Captures, promotions, EP, and TT-move handling are unchanged. Quiet checking moves are **not** specially handled in v1 — the gives-check classification cost is not justified (research §7; TalkChess t=74403 documented a major bug from getting it wrong without proper infrastructure). Deferred to a post-landing SPRT campaign.

FFP is a **per-move skip**, not a node-level cutoff: when the FFP condition fires, the move is `continue`d past, and the move loop carries on with the next move. There is no FFP-cutoff return value.

### 2. Node-level gate

Five conditions, all required (computed once before the move loop):

```rust
ply > 0
&& !is_pv
&& depth >= 1
&& depth <= FFP_MAX_DEPTH
&& alpha.abs() < MATE_IN_MAX_PLY
&& !in_check(pos)
```

| Gate | Why |
|---|---|
| `ply > 0` | Structural-root guard. Defense-in-depth against a future PVS refactor that would change `is_pv`'s semantics. Mirrors ADR-0023 §3 / ADR-0024 §3 / ADR-0025 §2 first gate. |
| `!is_pv` | At PV nodes the engine must search all moves to construct the principal variation. FFP at PV would silently shorten the PV. |
| `depth >= 1` | Structural: `negamax` at `depth == 0` delegates to `qsearch` (search-body step 2), so the lower bound is unreachable in production; the explicit guard makes the helper's domain match the call-site invariant. |
| `depth <= FFP_MAX_DEPTH` | At depths above the ceiling, the centipawn margin grows beyond what realistic position deltas can absorb; pruning becomes unsound (research §4). |
| `alpha.abs() < MATE_IN_MAX_PLY` | Mate-magnitude alpha implies a near-mate line is in play; centipawn-based futility margins are meaningless in that regime (research §13 Pitfall 1). Mirrors ADR-0024 §3. |
| `!in_check(pos)` | In check the position demands a response; static eval doesn't capture the urgency of evasion. Without this guard, FFP would prune valid evasions (research §13 Pitfall 4). |

### 3. Lazy-dup `static_eval` read (preserves M5.A/B/C byte-identical)

FFP reads its own STM-relative `static_eval` inside the node-level gate when it passes — independently of RFP's read at step 8 and NMP's read at step 9.

```rust
let ffp_static_eval: Option<i32> = if ply > 0
    && !is_pv
    && depth >= 1
    && depth <= FFP_MAX_DEPTH
    && alpha.abs() < MATE_IN_MAX_PLY
    && !in_check(pos)
{
    let stm = pos.side_to_move();
    Some(if stm == Color::White {
        pos.static_eval_white()
    } else {
        -pos.static_eval_white()
    })
} else {
    None
};
```

`ffp_static_eval == Some(_)` IS the eligibility predicate; the per-quiet branch in §6 matches on `Some(static_eval)` and skips otherwise. No separate `ffp_node_eligible: bool` to keep in sync.

**Rejected alternative**: hoist `static_eval` once at the top of `negamax` and share with all three blocks. Rejected for the same reason ADR-0024 §6 rejected a shared eager read: it would change the read population on K+P endgame nodes (NMP's `has_non_pawn_material` gate currently prevents the eval read; a shared hoist would fire regardless), and it would silently change the M5.A/B byte sequences — muddying the M5.D SPRT against `baseline/m5c-lmr`.

The independent reads keep ADR-0023 §3 and ADR-0024 §6 byte-identical and make the M5.D SPRT signal attributable to FFP alone.

### 4. Margin table and compile-time invariant

Pinned v1 constants:

```rust
pub(crate) const FFP_MAX_DEPTH: u32 = 2;
pub(crate) const FFP_MARGIN_D1: i32 = 100;
pub(crate) const FFP_MARGIN_D2: i32 = 150;
pub(crate) const FFP_MARGIN_D3: i32 = 250;  // inactive at v1; activated by FFP_MAX_DEPTH = 3

const _: () = assert!(FFP_MAX_DEPTH < LMR_MIN_DEPTH);  // production tripwire
```

`frontier_futility_margin(depth: u32) -> i32` is the per-depth lookup, returning 0 outside `[1, FFP_MAX_DEPTH]`. The depth-3 entry is defined for forward compatibility — a post-landing SPRT can raise `FFP_MAX_DEPTH` without touching the constant inventory.

| Depth | Margin (v1) | Active at v1? | Source |
|---|---|---|---|
| 1 (frontier) | 100 cp | yes | TalkChess t=74403 (+25 Elo with `{100, 150}`); research §3 conservative band |
| 2 (pre-frontier / Heinz extended) | 150 cp | yes | Research §3 conservative band; roadmap M5.D row |
| 3 (pre-pre-frontier / "limited razoring") | 250 cp | **no** (inactive) | Defined for post-v1 SPRT; CPW reference |

The compile-time `assert!(FFP_MAX_DEPTH < LMR_MIN_DEPTH)` is a production tripwire (mirrors ADR-0025 §3's `LMR_HIGH_HISTORY_THRESHOLD <= MAX_HISTORY` assert pattern). At v1 it pins that FFP and LMR have non-overlapping depth ranges. A future tuning that violates the invariant must remove the assert in the same patch that updates §6 (the per-quiet ordinal semantics question).

### 5. Per-move FFP gate + bound (single helper)

```rust
pub(crate) fn ffp_pruned_bound(
    static_eval: i32,
    depth: u32,
    alpha: i32,
) -> Option<i32> {
    if depth == 0 || depth > FFP_MAX_DEPTH {
        return None;
    }
    let bound = static_eval.saturating_add(frontier_futility_margin(depth));
    if bound <= alpha { Some(bound) } else { None }
}
```

`Some(bound)` payload is the FFP-proved fail-soft upper bound on the move's true score, guaranteed `<= alpha` by the gate. `None` means do not prune.

**One helper, two responsibilities by design.** Two-helper alternatives (separate `is_ffp_pruneable` bool helper + `ffp_proved_bound` i32 helper) create a saturation-overflow asymmetry: the gate must be saturating (else `+`-overflow could let the bound test pass when arithmetic says no), and the call-site contribution must reuse the same saturated value (else the `+`-overflow could re-emerge at the call site). The single helper closes the asymmetry by computing the bound once.

The helper-level depth guard (`depth ∈ [1, FFP_MAX_DEPTH]`) is defense-in-depth (M5.C `late_move_reduction` precedent) against a future call-site refactor that drops the node-level `depth <= FFP_MAX_DEPTH` gate.

`saturating_add` defends against `i32` overflow when `static_eval` is near `MATE`. The node-level gate's `alpha.abs() < MATE_IN_MAX_PLY` makes that case unreachable in production, but the helper is `pub(crate)` and unit-tested independently.

The inequality is `<=`, not `<`: a move whose true score could exactly equal alpha does not improve alpha (fail-soft requires strict improvement to update); pruning at equality is the standard CPW form (research §12).

### 6. Per-quiet move-loop integration

FFP fires before LMR. At v1 constants, FFP (`depth ≤ 2`) and LMR (`depth ≥ 3`) cannot fire at the same node (pinned by §4's compile-time invariant) — but the **layering rule** is committed for forward compatibility:

```rust
for (i, &mv) in moves_vec.iter().enumerate() {
    let quiet_move = is_quiet(mv);

    // M5.D — FFP skip (lands BEFORE LMR)
    if quiet_move
        && let Some(static_eval) = ffp_static_eval
        && let Some(pruned_bound) = ffp_pruned_bound(static_eval, depth, alpha)
    {
        // Provenance downgrade: this contribution is NOT full-depth-witnessed.
        // ADR-0026 §7 walks through why the downgrade is load-bearing.
        best_is_full_depth = best_is_full_depth_after_score(
            best, best_is_full_depth, pruned_bound, /*move_is_full_depth=*/ false,
        );
        if pruned_bound > best {
            best = pruned_bound;
        }
        continue;  // do NOT advance quiet_index, do NOT enter quiets_searched, do NOT recurse
    }

    // M5.C — LMR (unchanged)
    let lmr_reduction = if quiet_move {
        quiet_index += 1;
        ...
    };
}
```

**`quiet_index` semantics.** FFP `continue`s before reaching `quiet_index += 1`. The semantic rationale: `quiet_index` is LMR's *considered-for-reduction* ordinal, not a *seen-in-ordering* ordinal. ADR-0025 §3 makes this explicit: the TT move when quiet is searched first via the step-12 reorder and counts as `quiet_index = 1` even though it is implicitly exempt from reduction (the floor at `LMR_MIN_QUIET_INDEX = 2` makes the exemption work). FFP-skipped quiets are *not considered* for either reduction or recursion — they are never searched at all — so they semantically should not advance the ordinal that drives reduction decisions for subsequent quiets. **At v1 this is unobservable** (FFP_MAX_DEPTH < LMR_MIN_DEPTH; the two pruning paths don't overlap), enforced structurally by the `continue` placement and invariantly by the §4 compile-time assert. If a future SPRT raises `FFP_MAX_DEPTH ≥ LMR_MIN_DEPTH`, this section and §4's assert must be amended jointly; the per-quiet ordinal semantics question is reopened then.

### 7. Fail-soft return-value preservation + provenance downgrade (load-bearing correctness)

When FFP fires, the move's true score is bounded above by `static_eval + margin ≤ alpha`. Before `continue`, contribute that proved upper bound to the node's running `best`, **routed through `best_is_full_depth_after_score`** (the M5.C provenance helper from ADR-0025 §6) with `move_is_full_depth = false`:

```rust
best_is_full_depth = best_is_full_depth_after_score(
    best, best_is_full_depth, pruned_bound, /*move_is_full_depth=*/ false,
);
if pruned_bound > best {
    best = pruned_bound;
}
```

**Why the bound contribution is load-bearing.** Without it, an all-quiets-pruned-no-captures node would return `best = -INF` (the negamax initialiser), which the parent negates to `+INF`, registering a phantom fail-high. The pruned bound is `≤ alpha` by construction, so it never improves alpha and never causes a beta cutoff at this node — it only floors the eventual return value (research §13 Pitfall 6).

**Why the provenance downgrade is load-bearing.** Consider a mixed FFP-eligible node (some captures, some FFP-pruneable quiets):

1. Capture 1 runs at full depth and returns `score = 50 ≤ alpha`. `best_is_full_depth_after_score(-INF, false, 50, true)` returns `true`. State: `best = 50, flag = true`.
2. FFP-pruneable quiet at this node: `pruned_bound = 80 ≤ alpha`. Without the downgrade, the contribution `if pruned_bound > best { best = pruned_bound; }` gives `best = 80, flag = true (unchanged)`. **Wrong**: `best` is now backed by static-comparison evidence, not full-depth evidence; the flag's `true` value is stale.
3. End of loop with `best = 80, flag = true, best <= original_alpha`: `tt_bound_for_completed_node` returns `Some(Upper)`. Engine stores Upper at the current depth `d` with score `80`.
4. A future probe at the same Zobrist key and `depth ≥ d`: reads `tt_score = 80`, sees `bound = Upper`, sees `tt_score ≤ alpha`, returns `80` immediately — propagating the inflated bound at full claimed depth.

This is the same failure mode ADR-0025 §6 worked out for reduced-only-LMR scores ("Upper would be false advertising"). The fix is the same: route the contribution through `best_is_full_depth_after_score` so that any post-FFP improvement of `best` clears the full-depth flag, suppressing the TT store at end-of-loop.

**Walk-through of the correct path with the downgrade:**

1. `best = 50, flag = true`.
2. FFP contribution: `best_is_full_depth_after_score(50, true, 80, false)` → strict-improvement arm (`80 > 50`) returns `move_is_full_depth = false`. State: `best = 80, flag = false`.
3. End of loop with `flag = false`: `tt_bound_for_completed_node` returns `None`. Store suppressed. Sound.

**Equal-score and non-improving contributions** are handled by the same helper:

- `best_is_full_depth_after_score(80, true, 80, false)` (equal) → `true || (true && false)` = `true`. The flag persists at `true`, so an FFP contribution that ties an existing full-depth witness preserves the witness's provenance — the TT store can still fire on a real full-depth-witnessed best.
- `best_is_full_depth_after_score(80, false, 50, false)` (non-improving) → `false || (false && false)` = `false`. No change.
- `best_is_full_depth_after_score(80, true, 50, false)` (non-improving with prior full-depth) → `true || (false && false)` = `true`. Prior witness preserved.

The helper's existing semantics, designed for M5.C's reduced-only contributions, generalize verbatim to FFP's pruned-bound contributions. No new helper, no signature change.

### 8. `quiets_searched` and history malus

FFP-pruned quiets do **not** enter `quiets_searched`. They receive no history malus on a subsequent beta cutoff at this node.

Rationale: `quiets_searched` exists for ADR-0019's "tried-and-failed" malus dynamic. FFP-pruned moves were never searched at any depth — there is no recursive evidence backing the malus. Including a pure static-comparison-rejected move in the malus pool would over-train history on a comparison that has no search-quality basis. This mirrors ADR-0025 §7's reduced-only exclusion logic and extends the "tried-and-failed" notion to "actually searched" within FFP-eligible nodes.

The history bonus path (`update_history_on_quiet_cutoff`) is unaffected: FFP-pruned moves are never searched, so they cannot cause a beta cutoff. Bonus is applied to the cutter, which is by construction not FFP-pruned (the cutter was searched and returned `score >= beta`).

### 9. TT store policy

FFP does not write to the TT directly. The end-of-loop TT-store path (search step 14) is unchanged in code: bound classification reads `best`, `beta`, `original_alpha`, and the `best_is_full_depth` flag.

The behavior change is via the §7 provenance downgrade. Three cases:

- **All-quiets-FFP-pruned, no captures**: `best_is_full_depth = false` (initial; never set to `true` because no full-depth move ran). `best = max(pruned_bounds) ≤ alpha`. `tt_bound_for_completed_node` returns `None` → store suppressed. Sound: no full-depth witness, no TT entry.
- **Mixed: full-depth captures + FFP-pruned quiets, no full-depth alpha improver**: §7's walk-through. The downgrade fires when an FFP contribution overwrites a previously full-depth-witnessed `best`; `flag = false` at end-of-loop; store suppressed. **Without §7's downgrade**, the engine would store an inflated Upper bound at the current depth from coarse static-comparison evidence — false advertising.
- **Full-depth alpha improver**: a capture or non-FFP quiet returns `score > alpha`. `flag = true`. FFP-pruned contributions all carry `pruned_bound ≤ alpha < score`, so the strict-improvement arm of `best_is_full_depth_after_score` is never re-entered with `move_is_full_depth = false` after the alpha-improver landed. End of loop: `flag = true` → `tt_bound_for_completed_node` classifies as Lower or Exact, store fires with full-depth provenance. Sound.

### 10. Out of scope (deferred)

- Gives-check exemption (research §7 / SPRT-tunable §6 row 5).
- Depth 3 activation (`FFP_MAX_DEPTH = 3` post-v1 SPRT).
- Linear-formula alternative (`k * depth`).
- Eval-aware margin (margin halved when STM's eval is improving across plies).
- Per-move SEE-based capture futility / delta pruning.
- LMP / move-count-based pruning.
- Razoring at depth 3.
- Adaptive margin as a function of search depth or improving heuristic.

## Open SPRT-tunable parameters

Post-M5.D campaigns:

1. **`FFP_MAX_DEPTH`**: compare 2 / 3 (activates `FFP_MARGIN_D3 = 250`).
2. **`FFP_MARGIN_D1`**: compare 100 / 125 / 150 / 200 cp.
3. **`FFP_MARGIN_D2`**: compare 150 / 200 / 250 / 300 cp.
4. **`FFP_MARGIN_D3`** (gated on `FFP_MAX_DEPTH = 3`): compare 250 / 400 / 500.
5. **Gives-check exemption**: SPRT off (v1) vs on (after `gives_check(pos, mv)` infrastructure lands).
6. **`quiets_searched` policy** for pruned moves: exclude (v1) / include / depth-weighted malus.
7. **Linear formula alternative**: `k * depth` instead of per-depth table.

The v1 commitments are conservative and chosen for **clean SPRT attribution**, not for proven optimality.

## Consequences

**Positive:**

- Bench node count expected to drop further on the local implementation benchmark; research §15's CPW-based prior is +20–40 Elo over `baseline/m5c-lmr`.
- Composes naturally with M5.A/M5.B/M5.C: prologue pruning still runs before the move loop, RFP / NMP cut whole subtrees, FFP skips individual quiets, LMR reduces individual quiets. At v1 constants, FFP and LMR have non-overlapping depth ranges, keeping the move-loop logic clean.
- No signature changes. No new UCI options. No changes to `src/mov.rs`, `src/position.rs`, `src/tt.rs`, `src/eval.rs`, or movegen.

**Negative:**

- Search-tree shape changes; some existing tree-shape pins may need legitimate re-pins (M5.C precedent).
- A new lazy-dup `static_eval` read inside the move-loop entry. At v1 constants the overhead is bounded by the FFP gate's `depth ≤ FFP_MAX_DEPTH = 2` filter — only frontier and pre-frontier nodes incur the read.

**Migration paths for deferred variants:**

- **Gives-check exemption**: extend the per-move FFP test to `is_quiet(mv) && !gives_check(pos, mv) && is_ffp_pruneable(...)`. Requires `gives_check(pos, mv)` infrastructure (post-make `in_check` on child, or pre-make attack scan).
- **Depth 3 activation**: change `FFP_MAX_DEPTH` from 2 to 3. Helper `frontier_futility_margin` already returns 250 at depth 3; no other change required.
- **Improving heuristic**: read `search_stack[ply-2].static_eval` and halve the margin when eval is improving. Requires per-ply static_eval stack.
- **`quiets_searched` inclusion for pruned moves**: drop the `continue` in §6's FFP block; let the existing `quiets_searched.push(mv)` at the bottom of the move loop fire.

## Test surface (lands with M5.D)

- **Pure helper tests** (~14 tests): `frontier_futility_margin` per-depth pins (0, 1, 2, 3, 4) + anti-vacuous "not always 0"; `ffp_pruned_bound` domain guard at d=0 and d>FFP_MAX_DEPTH; boundary-inequality tests at d=1 and d=2 (inclusive at the equality); payload-arithmetic pins at d=1 and d=2 with non-zero static_eval; overflow defense via `saturating_add`.
- **Move-loop behavior tests** (~12 tests): all five gate-skip tests (ply=0, is_pv, in_check, depth>max, alpha-near-mate) with anti-vacuous sisters; capture-only fixture (FFP doesn't fire on captures); fires-when-eligible test; exact firings-counter pin; quiets_searched-exclusion pin with anti-vacuous sister; sign-convention pin (Black STM); fail-soft pruned-bound contribution pin (equality-strength, not `>=`); **§7 provenance-downgrade pin** — fixture: capture returns `score ≤ alpha`, FFP-pruned quiet contributes `pruned_bound > score` but still `≤ alpha`, assert post-search TT entry at the node's Zobrist key is suppressed. Anti-vacuous sister: same shape without FFP-pruneable quiet stores an Upper TT entry. Without the §7 downgrade, the FFP fixture would store an inflated Upper from coarse static-comparison evidence.
- **Integration**: `tests/uci_integration.rs::bench_signature_deterministic_across_two_runs_with_ffp` (E51) pins the M5.D depth-4 bench signature; E50 (M5.C) is dropped to determinism-only OR re-pinned to the M5.D value, decided at impl time per the M5.B/M5.C precedent.
- **Compile-time invariant**: `const _: () = assert!(FFP_MAX_DEPTH < LMR_MIN_DEPTH)` adjacent to the FFP constants in `src/search.rs` (production code, not test code). Tripwire: a future tuning that violates the invariant must remove the assert in the same patch that updates §6.
