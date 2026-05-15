# ADR-0031 — Tapered eval foundation (MG/EG blend + bishop pair + KBvKB-same-color + mop-up)

**Status:** Accepted (lands with M6.A).

## Context

ADR-0014 §1 established a single-phase MG-only evaluation: `evaluate` returns a combined material + PST score with no interpolation between opening and endgame tables. That design was deliberately minimal — sufficient for the M3.A SPRT target (vs. RandomMover) and isolated from the additional complexity of phase blending, which was deferred to M6.

The M6 milestone goal is to expand eval discrimination across multiple thematic dimensions. The first sub-phase, M6.A, replaces the single-PSQT representation with a tapered MG/EG pair blended by a material-derived phase tag. Three bonus terms — bishop pair, KBvKB-same-color insufficient material, and a mop-up term for simple endings — piggyback on the new representation and are landed in the same bundle.

Plan and test surface: `docs/plans/m6.a.md`. Research and design-space documentation: `docs/research/m6-tapered-eval.md`.

## Decision

### 1. Phase tag formula

Phase weight per piece kind: `[P=0, N=1, B=1, R=2, Q=4, K=0]` (stored as `PHASE_DELTA[kind]: u8` in `src/eval/data.rs`).

The sum at the starting position is `4·1 + 4·1 + 4·2 + 2·4 = 4 + 4 + 8 + 8 = 24` (knights + bishops + rooks + queens; pawns and kings contribute 0). The `K=0` entry is load-bearing: kings are always present on both sides, so a non-zero king weight would add a constant offset to every position's phase computation that does not reflect material reduction.

Phase is accumulated raw across all piece counts, including promoted pieces. A position with a promoted queen reaches `raw_phase = 28` (24 + 4). **The raw value is never clamped during accumulation.** Clamping to 24 occurs only at blend time (decision §2). Incremental maintenance mirrors this: `raw_phase: u8` on `Position` stores the raw sum; both `update_static_eval_after_make` in `mov.rs` and `eval_state_from_scratch` in `eval.rs` accumulate without clamping.

### 2. Blend formula

```
blended = (mg · min(raw, 24) + eg · (24 − min(raw, 24))) / 24
```

Where `mg` and `eg` are the white-perspective MG and EG eval components (including bishop-pair addends at their respective phase weights); `raw` is the raw accumulated phase tag before clamping; and `/` is Rust integer division (truncates toward zero).

The `min(raw, 24)` clamp appears at two independent sites:

- In `Position::static_eval_white()` — the blended accessor used by the search-side RFP / NMP / FFP gates.
- In `evaluate(&Position)` — the evaluation entry point called at quiescence leaves.

Both sites must apply the clamp independently so that a future change removing one does not silently mis-blend promoted-pawn positions. The test `static_eval_white_accessor_clamp_at_overflowed_phase` pins the accessor's clamp in isolation; `eval_promoted_queen_position_phase_clamped` pins the `evaluate` path.

At `raw >= 24`, `(24 − min(raw, 24)) = 0` and the blend collapses to the pure MG value. At `raw = 0`, the blend collapses to pure EG.

### 3. Storage: explicit triple

`Position` stores three fields replacing the former `static_eval_white: i32`:

```rust
static_mg_white: i32,   // white-perspective MG component, incrementally maintained
static_eg_white: i32,   // white-perspective EG component, incrementally maintained
raw_phase: u8,          // raw phase sum, may exceed 24 in promoted-pawn positions
```

`Undo` gains three replacement fields: `prior_static_mg: i32`, `prior_static_eg: i32`, `prior_raw_phase: u8`.

Two alternative storage layouts were evaluated and rejected (see Alternatives considered).

The `static_eval_white()` accessor is preserved with an identical signature for all search-side call sites; its implementation becomes the blended formula from decision §2. This avoids churn across ~12 call sites in `search.rs` and `position.rs` that read the accessor for RFP / NMP / FFP. The accessor intentionally excludes the bishop-pair and mop-up addends (those live only in `evaluate`), which is consistent with the M3.A precedent of factoring insufficient-material into `evaluate` but not into the incremental accessor.

### 4. Bishop pair

**Predicate.** A side has the bishop pair when its bishops occupy both color complexes — i.e., `(bishops & LIGHT_SQUARES).any() && (bishops & DARK_SQUARES).any()`. This is a necessary and sufficient condition for possessing bishops on opposite colors.

**Bonus.** MG +30 cp / EG +50 cp per side that has the pair (literature defaults from CPW Bishop Pair article; subject to M6.F Texel tuning). `bishop_pair_term_white(pos, value)` returns `+value` if only White has the pair, `−value` if only Black has it, `0` otherwise (including the symmetric case at the start where both sides have it). The bishop-pair addend is folded into the pre-blend `mg` and `eg` before the divide:

```
blended = ((mg + bp_mg) · phase + (eg + bp_eg) · (24 − phase)) / 24
```

**Live vs. incremental.** The predicate runs at eval-call time as two popcount operations per side — O(1). Per-call recomputation is chosen over incremental tracking (see Alternatives considered §"per-bishop-pair incremental tracking").

**Logical relationship to KBvKB-same-color.** The bishop-pair predicate fires when a side's bishops cover *both* color complexes. The KBvKB-same-color predicate fires when each side has exactly one bishop and both bishops are on the *same* color complex. The two predicates are logically opposite: any position satisfying KBvKB-same-color has no bishop pair on either side and returns 0 from bishop-pair; any position where one side has the bishop pair has at least two bishops of different colors and cannot be KBvKB-same-color.

### 5. KBvKB-same-color insufficient material

**Predicate.** Fires when `total_count == 4` AND each side has exactly one bishop AND both bishops are on the same color complex — tested via `(bishops & LIGHT_SQUARES).any()` returning the same result for both. This is the FIDE-recognized insufficient-material configuration: with both bishops on the same color, neither side can force checkmate.

**Placement.** The `is_kbkb_same_color` guard extends `is_insufficient_material` at the `total_count == 4` branch. The caller guarantees `total_count == 4` before delegating. `is_insufficient_material` continues to run unconditionally before any field reads — it gates on `pos.occupied_all().count()`, not on `raw_phase` (see decision §7 and research §9.5 for why phase-based gating would be fragile here).

This extends the M3.A insufficient-material set (KvK, KvN, KvB) to include KBvKB-same-color, which ADR-0014 §5 explicitly deferred to M6.

### 6. Mop-up term

**Activation gates.** `raw_phase < MOP_UP_PHASE_MAX` AND `|advantage_cp| > MOP_UP_MIN_ADVANTAGE_CP`.

`MOP_UP_PHASE_MAX = 5`. This value (not 4 as the research §6.5 first-draft stated) is load-bearing for including KQK in the activation range: KQK has `raw_phase = 4` (the lone queen), which satisfies `< 5` but not `< 4`. The set of activated endgames is `raw_phase ∈ {0, 1, 2, 3, 4}`, covering KQK (`4`), KRK (`2`), KBBK (`2`), KRBK (`3`), KBNK (`2`). Larger material endgames (e.g., K+Q+R vs. K at `raw_phase = 6`) skip mop-up; material differential alone guides conversion there.

`MOP_UP_MIN_ADVANTAGE_CP = 100`. Mop-up is a king-placement correction for already-winning positions; firing near equality would distort eval in competitive positions.

**Formula.** The winning side drives the losing king to a corner and centralizes its own king:

```
bonus = 4 · CMD(losing_king) + 2 · (14 − chebyshev(winning_king, losing_king))
```

Where `CMD` is the center Manhattan distance to the nearest square in {d4, d5, e4, e5} — a measure of how corner-bound the king is — and `chebyshev` is the Chebyshev distance between the two kings. The bonus is signed: `+bonus` when White is winning, `−bonus` when Black is winning (white-perspective throughout).

Ranges: `CMD ∈ [0, 6]`, `chebyshev ∈ [0, 7]`. Bonus range `[0, 50]` cp — small relative to the material gradient in any winning KQK / KRK position, so it contributes guidance rather than distorting material-evaluation ordering.

**Live vs. incremental.** The mop-up term is computed at `evaluate()` call time only, not tracked incrementally. The phase gate means it fires only in the deepest endgame positions where `evaluate()` is called infrequently (qsearch leaf nodes in won endgames); the per-call king-distance arithmetic is O(1).

### 7. Mate-score interaction

PeSTO-magnitude evaluation produces values in roughly `[−14000, +14000]` cp. The mate band begins at `MATE_IN_MAX_PLY − 1 = 29935` (per `search.rs`). These ranges do not overlap: the maximum mop-up addend is ~50 cp, the maximum bishop-pair addend is ~50 cp (EG only), and PeSTO material + PST stays well under ±14000 cp in any legal position.

No saturating arithmetic is needed in the blend. A `debug_assert!(result.abs() < MATE_IN_MAX_PLY - 1)` in `evaluate()` acts as an invariant sentinel: if an overflow or miscombination were to produce a mate-band collision, it fires in debug/test builds but imposes zero overhead in release. The assert is the full mitigation.

### 8. Debug round-trip assertion

`make_move` and `unmake_move` both carry a debug-build post-application assert:

```rust
debug_assert_eq!(
    (pos.static_mg_white(), pos.static_eg_white(), pos.raw_phase()),
    crate::eval::eval_state_from_scratch(pos),
    "incremental eval state diverged after make_move {mv:?}",
);
```

`eval_state_from_scratch(&Position) -> (i32, i32, u8)` is a single-pass, non-incremental recomputation that iterates all pieces and accumulates MG, EG, and raw phase from scratch. The tuple comparison detects any incremental delta that diverges from ground truth. This mirrors the M3.A pattern for the single-phase assert; it extends it to the triple.

The release-build perf sentinel test `make_move_no_eval_recompute_in_release` is preserved and continues to assert that the release binary does not call `eval_state_from_scratch` on the hot path.

## Consequences

- **Eval cost.** Two PSQT table reads per piece-touch in `update_static_eval_after_make` (was one). `evaluate()` adds bishop-pair bitboard ops and mop-up king-distance arithmetic. Per-call eval cost rises roughly 1.5–3×. Expected SPRT Elo gain (+50 to +80 Elo literature prior) far exceeds any plausible NPS-regression cost-per-node.
- **Bench node count.** From M6.A onward, the bench node count is no longer a no-regression signal; per roadmap §"Bench-node-count regression policy", the count may rise (more-expensive eval influences pruning gates and changes tree shape). Bench *stability across reruns at a fixed HEAD* remains the deterministic-bench contract (ADR-0010).
- **NPS.** A > 30% NPS drop is a hard should-fix (plan §13.2). A > 10% NPS drop is a should-investigate flag. Moderate NPS drop is expected and acceptable given the ~2× eval-cost growth.
- **RFP / NMP / FFP behavior.** `static_eval_white()` now returns a blended (MG·phase + EG·(24−phase)) / 24 value instead of pure MG. In endgame positions where the blend leans EG, the pruning gates' firing patterns shift. The SPRT itself is the headline gate; per-feature firing-count deltas are measured in the M6.A retrospective to make the confound measurable.
- **Search.rs call sites.** No production code changes required; the `static_eval_white()` accessor signature is preserved.
- **ADR-0014 §1** (single-phase claim) is superseded by this ADR. ADR-0014 §5 (insufficient-material set definition, which excluded KBvKB-same-color) is extended by this ADR.

## Alternatives considered

**Packed-i32 layout.** Storing MG and EG in the high and low 16-bit halves of a single `i32`. Rejected: each 16-bit half is treated as a signed quantity, requiring careful sign-extension on unpack. The PeSTO EG king PST has large positive central values (e.g., +54 cp near center) and large negative corner values (−53 cp); combining with material the EG component can reach ±2500 cp comfortably within 16-bit range signed, but the bit manipulation is fragile and any mistake in the unpack expression silently mis-evaluates positions. The explicit triple is unambiguous.

**Packed-i64 layout.** Storing MG in the high 32-bit word and EG in the low 32-bit word of a `u64` (Stockfish `Score` approach). Rejected: no runtime advantage over the explicit triple on Apple Silicon (ARM64 can load a pair of 32-bit registers in a single `ldp`; the u64 split does not improve on that). The explicit triple is more readable and avoids the sign-extension hazard of the i32 variant.

**Per-bishop-pair incremental tracking.** Maintaining a `has_white_bishop_pair: bool` field on `Position` and updating it in `make_move`. Rejected: the bishop-pair predicate is two popcount + bitwise-and operations per side — O(1) constant-time at eval-call boundaries. The incremental machinery costs more in code complexity and bug surface than the tiny per-call saving. This is consistent with the M3.A decision to place insufficient-material detection at the `evaluate()` boundary rather than in the incremental delta.

**Mate-score-safe saturating arithmetic in the blend.** Replacing the integer divide in the blend formula with `saturating_add` / `saturating_mul` chains. Rejected: the PeSTO magnitude analysis (decision §7) shows the eval and mate bands do not overlap; saturating arithmetic would add complexity with no safety benefit. The `debug_assert` in `evaluate()` is the appropriate safety net.

**MG-only `static_eval_white()` accessor for RFP / NMP / FFP.** Keeping the search-side static-eval reads as pure MG (i.e., returning `self.static_mg_white` directly) and reserving the blend for `evaluate()` only. Rejected: the blended value is the correct semantic for the pruning gates — an endgame position should use the EG-weighted static eval to decide whether to prune. The MG-only behavior in M3.A was a simplification born of the single-phase representation, not a correctness commitment. Introducing a `static_mg_white()` accessor for pruning would also require ~12 call-site changes with no strength benefit. The confound in RFP / NMP / FFP firing patterns is measurable (decision §8 firing-count instrumentation) and is accepted.

## References

- `docs/plans/m6.a.md` — full M6.A plan including data tables, function signatures, and test corpus.
- `docs/research/m6-tapered-eval.md` — full design space, rejected alternatives, and vendored PeSTO EG data.
- `docs/decisions/0014-eval-material-pst.md` — the M3.A ADR this decision extends.
- [CPW — Tapered Eval](https://www.chessprogramming.org/Tapered_Eval)
- [CPW — PeSTO's Evaluation Function](https://www.chessprogramming.org/PeSTO%27s_Evaluation_Function)
- [CPW — Bishop Pair](https://www.chessprogramming.org/Bishop_Pair)
- [CPW — Mop-up Evaluation](https://www.chessprogramming.org/Mop-up_evaluation)
- [CPW — Draw Evaluation](https://www.chessprogramming.org/Draw_Evaluation)

## Supersedes

- **ADR-0014 §1** — "single-phase MG-only" claim. M6.A replaces the single `PSQT` table with `PSQT_MG` + `PSQT_EG` blended by phase; "single-phase" no longer applies.
- **ADR-0014 §5** — insufficient-material set definition (KvK, KvN, KvB). M6.A extends the set with KBvKB-same-color, which ADR-0014 §5 explicitly deferred to M6.
