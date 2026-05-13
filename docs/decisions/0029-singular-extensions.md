# ADR-0029 — Singular extensions

**Status:** Accepted (lands with M5.G).

## Context

`docs/roadmap.md` §M5.G defers singular extensions (SE) to this sub-milestone. SE is a per-node selectivity technique: at non-PV interior nodes whose TT entry shows the TT-stored best move is much better than every alternative, search the TT move at `depth + 1` (extend) — the position is a "singular" win-or-lose hinge.

Plan and test surface: `docs/plans/m5.g.md`. Prior-art research: `docs/research/m5-singular-extensions.md` (depth threshold 8, Lower-bound gate, `tt_score − depth · 1` margin, `(depth − 1) / 2` verification, immediate-frame re-entrancy guard via `excluded_move`).

M5.G builds on the post-M5.F TT semantics (qsearch entries at depth=0 are excluded from SE eligibility by clause 7 because at `depth ≥ SE_MIN_DEPTH = 8` the gate requires `tt_depth ≥ 5`).

## Decision

### 1. Constants and gate

**`SE_MIN_DEPTH = 6`** (v2 retune), `SE_MARGIN_PER_DEPTH = 1`, `SE_TT_DEPTH_DELTA = 3`. New compile-time invariant `const _: () = assert!(FFP_MAX_DEPTH < SE_MIN_DEPTH)` clustered with the existing `FFP_MAX_DEPTH < LMR_MIN_DEPTH` invariant — guarantees FFP and SE never co-fire on the same node.

**Per-`SE_MIN_DEPTH` retune history.** v1 plan-time literature default `SE_MIN_DEPTH = 8` (CPW majority; Stockfish-historical) shipped first and SPRT-failed at outcome 3 (Δ Elo −13.03 [−37.84, +11.64], strongly bimodal per-TC: 10+0.1 +66, 60+0.6 +5, 20+0.2 −60, 40+0.4 −44). The v1 result identified the verification-cost-vs-extension-benefit equilibrium as TC-dependent: at mid TCs the engine reaches depth ~SE_MIN_DEPTH only intermittently, so SE fires sporadically and verification overhead dominates the rare extensions. v2 retune lowered `SE_MIN_DEPTH` to 6 (Xiphos default) → SE fires more often → verification cost amortizes → mid TCs flip from clear-negative to slight-/strong-positive. v2 SPRT: Δ Elo **+23.49 [+0.65, +46.53]**, plan §11 outcome 2. v3 retune (`SE_MIN_DEPTH = 8` reverted with `SE_MARGIN_PER_DEPTH = 2` instead) was attempted in parallel and SPRT-rejected with formal H0 acceptance: Δ Elo **−46.66 [−73.63, −20.25]** at 382 games (LLR=−2.88, crossed H0 boundary). The more-selective margin direction does not help — it produces a stronger regression than v1.

The eligibility predicate `singular_extension_eligible` is a 9-clause conjunction: `excluded_move.is_none()` (immediate-frame re-entrancy guard); `ply > 0`; `!is_pv`; `!in_check`; `depth >= SE_MIN_DEPTH`; `tt_bound == Lower`; `tt_depth >= depth - 3`; `tt_score.abs() < MATE_IN_MAX_PLY`; `tt_move != 0`. Pure function; per-clause flip-mutants independently testable.

### 2. Verification search

`singular_beta(tt_score, depth) = (tt_score − depth · 1).max(−(MATE − 1))` (saturating; floor is defense-in-depth, gate clause 8 makes the underflow case unreachable in production).

`verification_depth(depth) = (depth − 1) / 2`. Caller-side gate's clause 5 (`depth >= SE_MIN_DEPTH`) is the only protection against `u32` underflow at `depth == 0`; a debug-assert pins this in debug builds.

The verification call recurses on the *same* node at the same `ply` with `(s_β − 1, s_β)` zero-width window, `is_pv = false`, `allow_null = true`, and `excluded_move = Some(moves_vec[0])`. On verification fail-low (`verif_score < s_β`), the SE block sets `tt_move_extension = 1`; otherwise `0`.

### 3. Move-loop integration

The SE block sits at step 12.5 (between move ordering and the move loop). Inside the move loop, two new lines at the top:
1. `if Some(mv) == excluded_move { continue; }` — skip the excluded move; preserves M5.D `quiet_index` semantic.
2. `let move_extension: u32 = if i == 0 { tt_move_extension } else { 0 };` — extension applies only to the TT move at index 0.

Non-LMR `search_child` argument changes from `depth - 1` to `depth - 1 + move_extension`. LMR branch and FFP branch untouched: SE-extension fires only on `i == 0`; LMR-reduction fires only on `i >= 1`; the `i == 0` vs `i >= 1` predicates are disjoint by construction. FFP and SE are depth-disjoint by the compile-time invariant.

### 4. TT side effects: three `excluded_move.is_none()` guards

The verification frame must not pollute the parent's TT entry. M5.G adds three guards:

(a) **Step-7 cutoff branch** (`src/search.rs:1388`). **Load-bearing.** Without this guard, the verification frame's same-zobrist Lower-bound TT entry self-cuts (`tt_score >= beta_verif = singular_beta = tt_score − depth_parent` reduces to `depth_parent >= 0`, always true), returning `tt_score` and trivially `>= singular_beta` → SE never extends. The probe-for-ordering portion (the `tt_move = entry.best_move` capture) runs unconditionally — irrelevant since the move-loop excluded-move skip removes it anyway.

(b) **Step-9 NMP TT-store** (`src/search.rs:1498`). When NMP fires inside the verification frame, the proved Lower-bound is for the verification's modified game (TT move excluded), not for `pos.zobrist()`. Suppress the store; the cutoff `return cutoff_score` is fine.

(c) **Step-14 main TT-store** (`src/search.rs:1816`). The verification frame's post-loop bound describes a sub-game with the TT move excluded; storing it at `pos.zobrist()` would pollute future probes that don't exclude anything.

Recursive children below the verification frame (different zobrist keys) write normally — the cached entries describe their own positions correctly.

### 5. Re-entrancy: immediate-frame only

Clause 1 of the predicate (`excluded_move.is_none()`) prevents the verification frame from itself attempting SE. At descendants of the verification frame, `search_child` always passes `excluded_move = None`, so descendant SE *can* fire if the depth budget allows. Per the recurrence `f(d) = 1 + f((d−1)/2)` truncated at `SE_MIN_DEPTH = 8`: chain depth at most 3 at realistic engine depths (`d ≤ MAX_PLY = 64`); per-layer cost is geometric-sum-dominated, total `O(b^{d/2})`. The literature consensus (research §3.3) is that this is acceptable, and engines that rely on the immediate-frame guard alone report acceptable NPS. We follow the consensus.

The propagated `singular_ext_active` flag through `search_child` (Stockfish-style stack tracking) is deferred to the tuning backlog — revisit if SPRT shows verification-subtree NPS regression at deep TC.

### 6. Negamax signature change

`negamax` and `negamax_for_test` gain `excluded_move: Option<Move>` after `allow_null`. All ~70 existing call sites pass `None`; only the SE block's verification call passes `Some(moves_vec[0])`. `search_child` always passes `None` to its inner `negamax` call — children never inherit a parent's excluded move (different position).

### 7. TT-snapshot capture

Step-7's `tt.probe(pos.zobrist())` captures a `TtProbeSnapshot { tt_move: u16, bound: TtBound, depth: u8, score: i32 }` (with `score` ply-adjusted via `score_from_tt`) into a local `Option<TtProbeSnapshot>` for the SE block at step 12.5. The cutoff branch is moved inside the `excluded_move.is_none()` guard (decision §4(a)).

### 8. `#[cfg(test)] se_extensions` counter

Test-only `u32` field on `AlphaBetaMover`, incremented on verification fail-low (the SE-extends-the-TT-move case). Reset by `negamax_for_test` alongside the existing per-`go` counters. Accessor: `se_extensions_for_test(&self) -> u32`.

## Alternatives considered and rejected

(a) **Wrapper-function SE** (separate `verify_singular(...)` helper that internally calls `negamax`) vs `excluded_move: Option<Move>` parameter on `negamax`. Rejected wrapper for transparency: it would duplicate the negamax entry contract. Parameter approach mirrors how M5.A NMP threads `allow_null: bool`.

(b) **Lower+Exact gate at v1** vs Lower-only gate. Rejected mixed gate to start: at non-PV nodes (where SE fires), TT entries are predominantly Lower (fail-high cutoff outcomes); Exact bounds are rare (only when the move loop produces a score strictly inside `(original_alpha, beta)`). The Lower-only gate captures the dominant case; Exact extension is incremental, not load-bearing for v1. Tuning backlog.

(c) **Verification-frame TT cutoff: allow-cutoff (per literal reading of research §5.2) vs suppress-cutoff-but-allow-probe-for-ordering**. **Suppress cutoff, allow probe.** The literal-§5.2 reading was based on a misinterpretation; §5.2's consensus refers to TT cutoffs in the *subtree* of the verification frame at *different* zobrist keys, not the verification frame's own same-zobrist cutoff. Without cutoff suppression, the verification frame's Lower-bound entry self-cuts every time, defeating SE entirely.

(d) **Excluded-move TT-key separation** (Hyatt-style hashing the excluded-move into the TT key) vs accept depth-replacement contamination. Rejected key separation: doubles TT memory footprint or halves unique-zobrist coverage. The three `excluded_move.is_none()` guards are the cheaper alternative.

(e) **Propagated `singular_ext_active` flag through `search_child`** vs immediate-frame `excluded_move` guard alone. Immediate-frame only at v1, per research §3.3. Tuning backlog if SPRT shows NPS regression at deep TC.

(f) **Double extensions at v1** (extend by 2 plies when verification fails low strongly). Defer per research §3.2.

(g) **Multi-cut on verification fail-high** (return `singular_beta` as a fail-high if 2+ moves beat it). Defer per research §11.4.

## Interactions with existing M5 components

- **NMP (M5.A)**: NMP is allowed inside the verification subtree (`allow_null = true` at the verification call site) per research §6.1. The step-9 NMP TT-store at the verification frame is suppressed (decision §4(b)).
- **RFP (M5.B)**: independent; depths disjoint at v1 (`RFP_MAX_DEPTH = 6 < SE_MIN_DEPTH = 8`, but RFP can fire at the verification frame's children once parent_depth ≥ ~12 — the verification frame's own depth (`(parent_depth − 1)/2`) is at the RFP-eligible range from parent_depth ≥ 13). No interference.
- **LMR (M5.C)**: SE-extension and LMR-reduction never compose on the same move — `i == 0` vs `i >= 1` predicate disjointness.
- **FFP (M5.D)**: depth-disjoint by the new compile-time invariant.
- **Qsearch correctness (M5.E)**: independent; SE doesn't fire in qsearch.
- **Qsearch-in-TT (M5.F)**: qsearch entries have `depth = 0` and are excluded from SE eligibility by clause 7 (`tt_depth ≥ depth − 3`, with `depth ≥ 8` ⇒ minimum `tt_depth = 5`).
- **Aspiration windows (M4.D)**: SE fires at `ply > 0`, root unaffected.

## Empirical signal

**At v2 (`SE_MIN_DEPTH = 6`, the landed configuration):**

- Bench: `bench: 1147614 nodes <NPS> nps` — **+7.0% vs M5.F** (bench runs at depth 7; SE fires at depths 6–7, so verification searches add nodes). The +Elo signal in SPRT shows search-quality gain outweighs the per-node cost.
- WAC (clean re-run on v2 binary): **278/300 (92.7%)** vs M5.F 267/300 — **+11 positions, well above ±2 wallclock-noise band**. Strong tactical positive.
- STS (clean re-run on v2 binary): **9239/15000 (61.6%, est. 2499 STS-Elo)** vs M5.F 8822/15000 (58.8%, est. 2376) — **+417 credit, well above ±68 wallclock-noise band, est. +123 STS-Elo**. Decisive tactical+strategic positive.
- Mixed-TC rating estimate vs Stockfish UCI_Elo=2641 (seed `_00E`, 200 games + a 200-game independent confirmation run): Δ Elo **+3.47 [−40.84, +47.90]** (and confirmation run −6.95 [−51.16, +37.04]). Combined estimate ~**2639 ± 44 Elo** on Stockfish UCI_LimitStrength scale — statistically indistinguishable from M5.F's 2636, M5.E's 2622, and M5.D-v2's 2641. Both runs CPU-contended (concurrent rating-estimates); qualitative CI-straddling-zero result robust to contention.
- Mixed-TC SPRT vs `M5.F` (seed `0xC1ABF15AE10DD00C`): **`verdict=continue` at 400 games, Δ Elo +23.49 [+0.65, +46.53]**, pentanomial `[10,40,71,71,8]`. Per-TC: 10+0.1 51.2%, 20+0.2 51.0%, 40+0.4 64.6% (decisive!), 60+0.6 48.9%. Plan §11 outcome 2 (small-but-not-regression).

**At v1 and v3 (rejected configurations):**

- v1 (`SE_MIN_DEPTH=8, SE_MARGIN=1`): Δ Elo **−13.03 [−37.84, +11.64]**, outcome 3 (flat-negative). Bimodal per-TC: extreme TCs slightly positive, mid TCs clear-negative.
- v3 (`SE_MIN_DEPTH=8, SE_MARGIN=2`): **`verdict=H0` at 382 games**, Δ Elo **−46.66 [−73.63, −20.25]**, plan §11 outcome 4 (H0-accept hard regression). The more-selective margin direction (firing SE less often) does not help — SE works best when it fires often enough to amortize the verification overhead. v3's regression is *stronger* than v1's flat-negative.

Detailed SPRT logs: `bench/sprt/2026-05-09-m5.g-vs-m5f-mixed-tc.md` (v1), `bench/sprt/2026-05-10-m5.g-v2-min-depth-6-vs-m5f-mixed-tc.md` (v2 — landed), `bench/sprt/2026-05-10-m5.g-v3-margin-2-vs-m5f-mixed-tc.md` (v3 — rejected).

## Open / tuning backlog

- Lower+Exact gate.
- `SE_MARGIN_PER_DEPTH = 2` retune at `SE_MIN_DEPTH = 6` (v3 tested margin=2 at SE_MIN_DEPTH=8; the v2-baseline equivalent retune is unexplored).
- `SE_MIN_DEPTH = 4` retune (further-lower; risks pushing too much verification cost into shallow nodes).
- ~~`SE_MIN_DEPTH = 6` retune~~ — done at v2, adopted as landed configuration.
- ~~`SE_MARGIN_PER_DEPTH = 2` retune~~ — done at v3 (with SE_MIN_DEPTH=8); rejected.
- Multi-cut on verification fail-high.
- Double extensions.
- PV-SE (extend SE eligibility to PV nodes).
- Excluded-move TT-key separation.
- Propagated `singular_ext_active` flag through `search_child` (revisit if verification-subtree NPS regression observed at deep TC).
- Boundary test `negamax_se_extension_at_singular_beta_boundary` — closes the two missed mutants on the `s_beta - 1` window expression. See `docs/tuning-backlog.md`.

## References

- Plan: `docs/plans/m5.g.md`.
- Research: `docs/research/m5-singular-extensions.md`.
- Anantharaman, Campbell, Hsu (1988). "Singular Extensions: Adding Selectivity to Brute-Force Searching."
- Chess Programming Wiki — Singular Extensions, Multi-Cut, Extensions.
- ADR-0018 (TT semantics), ADR-0023 (NMP `allow_null` precedent), ADR-0025 (LMR `is_lmr_eligible_quiet` / `search_child` precedent), ADR-0028 (qsearch-in-TT, the immediate predecessor).
