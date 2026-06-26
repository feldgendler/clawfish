# ADR-0043 — Principal Variation Search (PVS): scout window, three-step LMR ladder, retained `is_pv`

**Status:** Accepted (mechanism); M8.A **shelved** (2-seed SPRT net ≈ −20 Elo, depth-amplifying
— see [`bench/sprt/2026-06-25-m8a-pvs-vs-m7b2.md`](../../bench/sprt/2026-06-25-m8a-pvs-vs-m7b2.md)).
**Superseded for shipping by ADR-0044 (M8.A.1 depth-conditioned PVS)**, which conditions the
scout on ID root depth via a smooth ramp; the "Tuning levers if the SPRT regresses" list in
§Consequences below is superseded by that ramp.

## Context

M8.A is the first search-layer change since M7.B.2 (ADR-0041), the opening unit of
M8 (Search refinements I, pre-NNUE). It adds **Principal Variation Search** to the
negamax move loop: non-first children are scouted with a null window and re-searched
at the full window only when the scout result is interior. PVS is the canonical
companion to the M5.C late-move-reduction loop (ADR-0025) — under PVS the §5
"reduced → full-depth re-search" generalizes to a three-step ladder.

The baseline for the M8.A SPRT is `M7.B.2` (current production search HEAD; eval
`M6.J`). PVS is **value-exact vs pure alpha-beta**, but this engine's NMP/RFP/FFP/LMR
prunes are `is_pv`-gated, and PVS marks the full-window re-search node `is_pv = true`
(prunes suppressed) where today the only full-window search of a late move runs
`is_pv = false` (prunes fire); PVS also scouts non-first children of a PV node with a
*narrow* window where today they received the full window (RFP/NMP read `beta`). So
PVS **changes the search tree** and is **not** bench-identical to `M7.B.2` — it is a
strength change, SPRT-gated per ADR-0037, not a bit-exact refactor.

Prior-art survey: [`docs/research/m8.a-pvs.md`](../research/m8.a-pvs.md). Plan and
test surface: [`docs/plans/m8.a.md`](../plans/m8.a.md).

This phase layers on top of:

- ADR-0018 §11 (`is_pv` TT-cutoff gating) — left **unchanged** (the `is_pv` parameter
  is retained; see §1).
- ADR-0025 §5–§7 (LMR re-search policy, TT-store discipline, history interaction) —
  the §5 re-search becomes Step 1 → Step 2 of the new ladder; §6/§7 are preserved
  unchanged (see ADR-0025's M8.A amendment).
- ADR-0041 (qsearch SEE-prune ramp) — orthogonal; the scout windows do not touch the
  qsearch threshold.

## Decision

### 1. `is_pv` is retained — window-derivation deferred

The roadmap names "retire the synthetic `is_pv` in favour of
`is_pv = ply == 0 || beta - alpha > 1`." Deriving the predicate and **dropping the
parameter** is **rejected for this unit** because it makes the state "non-PV node with
a wide search window" *unrepresentable*, which irrecoverably breaks two classes of
existing test:

1. ~16 prune-*firing* tests pass `is_pv = false` with a wide `(-INF, INF)` window and
   assert e.g. `lmr_reduced_searches > 0` (anti-vacuous). The only way to derive
   `is_pv = false` is a width-1 window, which collapses the search to an immediate
   fail-low/high — the firing can no longer be elicited.
2. `negamax_skips_lmr_at_ply_zero_even_when_is_pv_false` (and its NMP/RFP siblings)
   pass `ply = 0, is_pv = false` to anchor the `ply > 0` defense-in-depth root guard
   *independently of `is_pv`*. Derivation forces `is_pv = true` at `ply == 0`, so the
   `is_pv = false`-at-root state — the only thing distinguishing `ply > 0` from
   `!is_pv` — cannot be constructed; the guard becomes untestable.

**Keeping the parameter is behaviorally identical to deriving it.** Every production
call site already passes an `is_pv` consistent with its window (root wide ⇒ `true`;
NMP/SE children width-1 ⇒ `false`; first child inherits; scout `false`; re-search
`is_pv`). The only thing derivation buys is one fewer `bool` in the signature — a
roadmap-aesthetics win that does not earn the test-coverage loss. **Window-derivation
is deferred to a future mechanical-cleanup unit** that can re-express the affected
tests through the production `go()` path. This ADR records the deferral and the reason;
ADR-0018 §11's `is_pv`-gated TT-cutoff semantics are unchanged.

### 2. PVS scout window and fail-soft re-search

A non-first child is searched with a **null window `(alpha, alpha+1)`** (width 1).
Under fail-soft:

- `score <= alpha` — fail-low; the move does not improve `best`. No re-search.
- `alpha < score < beta` — **interior**: the move may improve, but the null window
  cannot establish an exact score (a width-1 window has zero interior). Trigger a
  **full-window re-search** `(alpha, beta)`; the re-search returns the actual score
  (fail-soft — it is *not* clamped to the scout bound).
- `score >= beta` — fail-high; immediate cutoff, return the fail-soft score.

The re-search condition is `score > alpha && score < beta` — **not** `score > original_alpha`.
As long as `alpha` has not moved (e.g. the first move failed low), subsequent moves
correctly receive a full-window search because no bound has been established to probe
against. Cite research §1, §5.

### 3. The three-step LMR ladder

For each non-first searched move, with `R = lmr_reduction` (0 when LMR-ineligible):

```
Step 1 — pilot:   reduced depth (depth-1-R), null window (alpha, alpha+1), child_is_pv = false
    → score <= alpha: done (fail-low at reduced depth and null window)

Step 2 — verify:  full depth (depth-1),     null window (alpha, alpha+1), child_is_pv = false
    → fires only if R > 0 && pilot score > alpha (escalate a reduced fail-high to full depth)

Step 3 — exact:   full depth (depth-1),     full window (alpha, beta),    child_is_pv = is_pv
    → fires only if alpha < score < beta (interior to the real window)
```

The key engine-specific observation: **Step 2 fires only at non-PV nodes.** LMR is
gated `!is_pv` (ADR-0025 §2), so `R > 0` ⇒ `!is_pv` ⇒ `beta == alpha+1`. At such a
node the Step-2 null window `(alpha, alpha+1)` **equals** the node window `(alpha, beta)`,
so Step 2 is behaviorally **identical to the existing ADR-0025 §5 full-depth LMR
re-search** — the window relabeling is a no-op at every node where it fires. The
genuinely-new paths are therefore:

- the **PV-node scout** (Step 1 at `R == 0`): non-first children of a PV node now get a
  null window where today they got the full window; and
- the **Step-3 full-window re-search**: a late move that survives the scout earns a
  full-window exact search.

Step 2 and Step 3 are **mutually exclusive per node-type** and never co-increment for
the same move: Step 2 requires `R > 0` (non-PV); Step 3's `score < beta` is impossible
after `score > alpha` at a width-1 node, so Step 3 is unreachable wherever Step 2 fires.
At a non-PV node the loop degenerates to today's full-window scout with no special-casing
(`beta == alpha+1` ⇒ Step-3 condition is contradictory). Cite plan §3.2.

### 4. `child_is_pv` propagation

```
first searched move:  child_is_pv = is_pv     // inherit parent PV-ness
Step 1 / Step 2:      child_is_pv = false     // scouts are non-PV
Step 3 re-search:     child_is_pv = is_pv     // the re-search node IS a PV node
```

Marking the Step-3 re-search `child_is_pv = is_pv` (i.e. `true` at a PV node) is the
canonical "the full-window re-search node is a PV node" — a late move that survives the
scout and needs an exact score is a PV candidate, so prunes are suppressed in that
subtree (research §3.2). This is the *only* change from today's single
`child_is_pv = is_pv && cur_i == 0` (which already yields `is_pv` for the first move and
`false` for the rest). It is also the source of the strength delta in §1 of the Context:
the re-search node flips `is_pv` `false → true`, changing `is_pv`-gated prune firing in
that subtree.

The **first non-excluded** move always gets the full window at full depth — never a
scout — even at a singular-extension verification frame where `cur_i == 0` is the
excluded TT move and the first searched move is `cur_i == 1` (else the verification
score would be a scout result, not a real one). SE `move_extension` keeps using
`cur_i == 0`; verification frames are non-PV, so the `child_is_pv` distinction is moot
there. Cite plan §3.1, §3.2.

### 5. `move_is_full_depth` provenance — unchanged

`move_is_full_depth` is `false` **only** when the contributing score is reduced-depth —
`R > 0 && pilot_score <= alpha` (Step 1, no escalation). This is identical to today's
reduced-only rule; it feeds `best_is_full_depth_after_score` and the ADR-0025 §6
reduced-only TT-store suppression unchanged. A move that clears Steps 1–2 but fails low
at Step 3 is still full-depth-witnessed (the Step-2 score is full-depth), so
`move_is_full_depth = true` there is correct. ADR-0018 §11 TT-cutoff gating is unchanged.
Cite plan §3.3.

### 6. MATE-boundary safety

`alpha + 1` is safe across the score range: `alpha <= MATE_IN_MAX_PLY = 29936` ⇒
`<= 29937`; worst case `alpha = INF - 1 = 30000` ⇒ `alpha + 1 = INF = 30001`, a valid
width-1 window. No overflow; `score_to_tt` narrowing to i16 stays within
`|adjusted| <= MATE + MAX_PLY = 30064 < i16::MAX` (ADR-0018 §5). Pinned by a unit test
asserting both no-panic search **and** a correct fail-high TT-store round-trip. Cite
plan §3.4, research §7.3.

## Consequences

**Positive:**

- Expected **+5–15 Elo / 10–15% node reduction** with this engine's current ordering
  quality (TT move + killers + history + SEE qsearch prune); the literature
  characterizes PVS as primarily a node-efficiency gain whose Elo conversion depends on
  the saved nodes buying depth within the time budget (research §6).
- No signature changes; no new production constants; search-layer only (no
  `mov.rs` / `position.rs` / `tt.rs` / `eval.rs` / movegen changes).

**Negative / risk:**

- **Not bench-identical to `M7.B.2`** (§1 of Context) — surfaced, not a defect. SPRT is
  the gate. Bench is recorded for determinism only.
- **First-move ordering risk:** when the first move is not best, the scout fails high and
  a full-window re-search costs two searches. If the re-search rate
  (`pvs_research_full_window` / non-first PV-node moves) consistently exceeds 15–20%,
  ordering may have regressed (research §6.2; expect 5–20%).

**Tuning levers if the SPRT regresses (in order):**

1. **Two-step ladder** — drop Step 2 (the full-depth null-window verify). Step 2 is the
   instability safety valve recommended for an NMP/TT engine (research §2.2); removing it
   is the first lever if the SPRT is flat/negative.
2. **Re-search as non-PV** — mark the Step-3 child `child_is_pv = false`, keeping the
   re-search closer to a pure scout optimization (prunes not suppressed in the re-search
   subtree).

Both levers are local single-arm edits; neither touches the signature.

## Test surface (lands with M8.A)

Two `#[cfg(test)]` exactness-harness flags (default `false`, compiled out of release)
anchor the new paths against a value reference:

- `test_force_all_pv` — forces `child_is_pv = true` for every recursive child,
  neutralizing all `is_pv`-gated prunes (NMP/RFP/FFP) and LMR (so `R == 0` everywhere ⇒
  Step 2 is unreachable *by design* under this flag).
- `test_disable_scout` — replicates the pre-PVS move loop exactly (non-first children get
  the LMR reduced pilot at the *full* window → full-depth full-window re-search if
  `> alpha`; non-LMR children at full window). The value reference for both exactness
  anchors.

Tests:

- **`pvs_exact_under_all_pv_vs_full_window`** (+ `prop_pvs_exact_under_all_pv` proptest)
  — strong exactness anchor for the **new PV paths**: with `test_force_all_pv = true`
  (all prunes/LMR off), PVS-on must equal the full-window reference on both score and
  bestmove for curated and random positions at depth ∈ [1,4]. Exercises the PV-node scout
  and Step-3 re-search in isolation from the prune interaction.
- **`pvs_nonpv_bit_identical_to_reference`** (+ proptest) — **Step 1 + Step 2 value/node
  anchor**: at a non-PV node (width-1 window, `ply > 0`, depth ≥ 3 so LMR fires) under
  normal config, PVS-on is **bit-identical** to `test_disable_scout` on score *and* node
  count, because at a non-PV node the scout window equals the node window. Catches any
  window/depth arithmetic error in the reduced/verify path.
- **`pvs_depth1_exact_vs_full_window`** — secondary anchor: at a depth-1 *root* call
  (children are prune-free qsearch; root is PV) PVS score+bestmove equal a full-window
  reference on startpos/Kiwipete/tactical positions. Cheap and assumption-light.
- **`pvs_research_fires_only_when_score_interior`** — PV node: a non-first move scoring
  in `(alpha, beta)` ⇒ exactly one Step-3 re-search; `<= alpha` ⇒ none; `>= beta` ⇒ cut,
  no re-search (counter assertions).
- **`pvs_non_pv_node_never_full_window_researches`** — width-1 node ⇒
  `pvs_research_full_window == 0` regardless of child scores.
- **`pvs_first_move_searched_full_window`** — first move never scouted.
- **`pvs_lmr_three_step_ladder_counters`** — LMR-eligible late move failing high
  reduced→full: Step-1 + Step-2 fire (Step-3 absent at the non-PV node where LMR lives);
  a reduced-only fail-low asserts `move_is_full_depth = false` via the stored TT bound.
- **`pvs_mate_boundary_null_window`** — `alpha = INF - 1` node: null window
  `(INF-1, INF)` searched without panic; fail-high stores and round-trips correctly (§6).
- **`pvs_root_research_when_first_move_fails_low`** — research §5 gotcha: at the root
  under an aspiration-narrowed window, with the first root move forced to fail low (alpha
  unmoved), a later root move scoring in `(alpha, beta)` still triggers a Step-3
  re-search (`score > alpha && score < beta`, not `> original_alpha`). Driven through
  `go()`.
A `pvs_aspiration_root_is_pv_guard` was considered (a synthetic width-1-window root
call asserting `is_pv` stays effective at `ply == 0`) but **not added**: the production
aspiration window floors at width 50 (§1), so a width-1 root is unreachable, and the
retained `..._at_ply_zero_even_when_is_pv_false` guards already anchor the `ply == 0`
root guard independently of `is_pv`. Adding it would have tested an unreachable state.

Diagnostic counters (mirror `nmp_firings` / `lmr_*`; reset in `go` and
`negamax_for_test`): `pvs_scout_searches` (Step-1 pilots), `pvs_lmr_verify_searches`
(Step-2 verifies), `pvs_research_full_window` (Step-3 re-searches; the ordering-health
diagnostic).

**TT handling for exactness anchors:** each run (PVS-on and reference) uses a fresh
per-run mover with `tt: None` (or a freshly-cleared TT) — never shared — so scout-generated
Lower/Upper entries cannot perturb the reference's ordering and flip a tie-broken
bestmove. Matches the existing exactness-test convention.

The stacked-NMP canary (`negamax_passes_allow_null_false_in_null_subsearch`,
`STACKED_NULL_GATE_REACHABLE`) is re-validated — PVS introduces null windows in the move
loop, the exact condition it guards — confirming `STACKED_NULL_GATE_REACHABLE` stays
provably 0; its `NMP_FIRINGS_PINNED` secondary anchor is re-pinned to the new PVS tree.
Any other exact node/firing-count anchor that legitimately shifts is re-pinned with a
PVS-attribution comment. The ~16 wide-window `is_pv = false` firing tests and the
`..._at_ply_zero_even_when_is_pv_false` guards stay **unchanged** (the parameter is
retained — §1). Cite plan §5.
