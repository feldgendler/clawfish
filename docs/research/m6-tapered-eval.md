# Prior-Art Research: M6.A — Tapered Eval Foundation

Sources consulted: Chess Programming Wiki (CPW) — Tapered Eval, PeSTO's Evaluation Function, CPW-Engine Eval, Bishop Pair, Mop-up Evaluation, Draw Evaluation, KRK, Score, King Centralization, Bishops of Opposite Colors; TalkChess threads (t=77546 game-phase discussion, t=51656 tapered eval guide, t=61850 Stockfish packed-score Q&A, t=76265 Texel tuning interaction); Mediocre Chess blog (tapered eval guide, 2011); Rustic chess engine documentation (tapering, FIDE draw detection); MadChess blog (tapered eval series).

Per ADR-0003, no third-party engine source code was read. All findings come from prose: wikis, papers, blog posts, and forum threads.

---

## TL;DR — Nine-Bullet Bottom Line

- Phase tag `[P=0, N=1, B=1, R=2, Q=4, K=0]` summing to 24 at full material is the CPW and PeSTO workhorse. King weight 0 is load-bearing; saturate/clamp to 24 on early promotion.
- Blend formula `(mg·phase + eg·(24 − phase)) / 24` with truncation-toward-zero is exact CPW/PeSTO idiom; max product ~48 000 fits safely in `i32`.
- Use the triple `(mg_white: i32, eg_white: i32, phase: u8)` layout. The packed-i32 trick has a well-documented sign-extension hazard when sign bits of the lower half overflow into the upper half; packed-i64 is safe but offers no advantage over the explicit triple for a Rust engine.
- Bishop-pair bonus: MG +28 cp / EG +50 cp is a defensible center of the literature range; the Rustic engine uses W(10, 40), Kaufman's estimate implies +50 cp. Recommended starting point: MG +30 / EG +50.
- KBvKB same-color: BOTH bishops on same-color complex → draw. Predicate is the **opposite** of bishop-pair predicate (which requires bishops on **different** color complexes).
- Mop-up: activate when `phase < 4` (no queens, at most one rook). Formula: `4 × CMD + 2 × (14 − chebyshev_kings)` where CMD = center Manhattan distance of the losing king. Scale the result by `advantage_cp / 256` to prevent a large mop-up bonus from dominating a small material edge.
- Mate-score band: `MATE_IN_MAX_PLY - 1 = 29 935` is the centipawn ceiling. Nine queens + all minor pieces yields at most ~14 000 cp raw; PST contributions at PeSTO scale add at most ~500 cp on top. No overflow into the mate band at piece-saturation positions.
- Debug round-trip assert under tapering becomes three asserts per make/unmake (triple layout). That triples the C4 proptest budget; the plan should time-check C4 and halve `cases` from 256 to 128 if necessary.
- The single largest gotcha: failing to clamp `phase` to 24 before the division produces wildly wrong evaluations in queened-pawn positions — clamp is mandatory.

---

## 1. Phase Tag Formula

### 1.1 CPW Workhorse

CPW's Tapered Eval article and PeSTO's Evaluation Function both use the same weights:

| Piece | Phase delta | Starting count | Contribution |
|-------|-------------|---------------|--------------|
| Pawn  | 0           | 16            | 0            |
| Knight| 1           | 4             | 4            |
| Bishop| 1           | 4             | 4            |
| Rook  | 2           | 4             | 8            |
| Queen | 4           | 2             | 8            |
| King  | 0           | 2             | 0            |

Total at full material: 24. Phase counts up from 0 (endgame-only, all pieces traded) to 24 (full middlegame). PeSTO's published pseudocode uses a `gamephaseInc[12]` array indexed by piece type where `[pawn]=0, [knight]=1, [bishop]=1, [rook]=2, [queen]=4, [king]=0`; then `mgPhase = min(gamePhase, 24)` before the blend. CPW-Engine_eval uses `if (v.gamePhase > 24) v.gamePhase = 24` on the accumulated value.

### 1.2 Alternative Formulations

| System | P | N | B | R | Q | Max | Scale factor | Source |
|--------|---|---|---|---|---|-----|--------------|--------|
| PeSTO / CPW workhorse | 0 | 1 | 1 | 2 | 4 | 24 | 1/24 | CPW, PeSTO |
| CPW generalized | 0 | 1 | 1 | 2 | 4 | 24 | same | Mediocre Chess blog |
| Fruit (power-of-two scale) | 0 | 12 | 12 | 22 | 44 | ~224 | 1/224 | TalkChess t=77546 (Madsen) |
| Explicit-pawn variant | 1 | 1 | 1 | 2 | 4 | ~40 | 1/40 | TalkChess t=51656 (debated) |

- Fruit's scale uses powers of two to replace division with a right-shift; the ratio 1:1:2:4 across N/B/R/Q is identical to PeSTO — only the divisor changes. The Madsen discussion in t=77546 confirms "doubling of importance minor → rook → queen."
- Including pawns in the phase delta is debated (t=51656): trading pawns does not advance the game toward the endgame the way piece trades do; pawns should stay at 0.
- King phase delta 0 is load-bearing: kings are always present, so a non-zero king weight would be a constant offset to the phase on every position — it would simply shift all evaluations identically without discriminating phase.

### 1.3 Roadmap Alignment

The roadmap commits to `[P=0, N=1, B=1, R=2, Q=4, K=0]` summing to 24. This matches PeSTO exactly and is confirmed as the CPW workhorse.

### 1.4 Saturation / Clamping

Promoted pieces add to the phase count beyond the starting material:

- A pawn promoted to a queen adds 4 to `gamePhase`.
- With 8 promoted queens on the board, `gamePhase` could reach `8×4 + 0 (rooks traded) + 4 (knights/bishops) + 8 (rooks) = 24 + 32 = 56`.
- Without clamping, the blend becomes `(mg·56 + eg·(24−56)) / 24 = (mg·56 − eg·32) / 24` — a nonsense value that subtracts the EG term.

**All CPW sources recommend clamping**: `mgPhase = min(gamePhase, 24)`. TalkChess t=77546 includes a brief discussion of whether to clamp vs. extrapolate; the consensus is clamp because promoted-queen positions are very deeply into the "late phase" and extrapolation gains nothing.

The per-make/unmake incremental update must apply the clamp only during the final blend, not during the accumulation: accumulate raw `gamePhase` (may exceed 24 in queened-pawn positions), then clamp at blend time. This way the raw phase count is correct after unmake without having to reconstruct it.

---

## 2. Blend Formula and Rounding

### 2.1 Formula

```
mg_phase = min(raw_phase, 24)
eg_phase = 24 - mg_phase
score = (mg · mg_phase + eg · eg_phase) / 24
```

PeSTO pseudocode uses exactly this. CPW-Engine_eval uses exactly this. The CPW Tapered Eval article uses a 256-scale variant for easier fixed-point, but PeSTO's own code uses 24 and integer division.

### 2.2 Precision and i32 Overflow

- Typical `mg` magnitude: PeSTO material + PST combined, max around +2000 cp for a large material advantage (e.g. being up a queen + two rooks + two bishops in a lopsided test position).
- `mg × mg_phase` at `mg_phase = 24`, `mg = 2000`: `2000 × 24 = 48 000`. Fits in i32 (max ~2.1 billion).
- Worst-case PST saturated position (nine queens + all minor pieces, see §7): raw eval around ±14 000 cp. `14 000 × 24 = 336 000`. Still far from i32 overflow.
- The blend itself is safe in i32. The two intermediate products `mg * mg_phase` and `eg * eg_phase` need not be promoted to i64.

### 2.3 Rounding Semantics

Integer division in Rust truncates toward zero. The CPW 256-scale variant adds `TotalPhase/2` before dividing to implement round-to-nearest. PeSTO's own 24-scale pseudocode does not add any rounding correction — it truncates.

- The difference between truncation and round-to-nearest at a divisor of 24 is at most 11 centipawns on any single score contribution.
- In practice, over a full position evaluation, truncation errors partially cancel across multiple piece scores.
- CPW's Tapered Eval notes the 256-scale rounding correction as an option ("adds rounding correction") but does not require it; PeSTO itself skips it.
- **Recommendation**: truncation toward zero (Rust default). The error is bounded and practically negligible; the round-to-nearest correction is not worth the added complexity at the 24-scale.

---

## 3. Storage Layout

Three candidate layouts for the fields on `Position`:

### 3.1 Option A: Explicit Triple

```
static_mg_white: i32
static_eg_white: i32
phase: u8
```

- Straightforward. No encoding/decoding.
- Debug round-trip assert: three comparisons — `mg_actual == mg_scratch`, `eg_actual == eg_scratch`, `phase_actual == phase_scratch`.
- Incremental delta cost: two `i32` additions (mg and eg) per placed/removed piece + one `u8` increment/decrement per placed/removed piece.
- No overflow risk anywhere.
- Rust struct alignment: `i32` + `i32` + `u8` + 3 bytes padding = 9 bytes effective (aligned to 12 bytes). Adding `u8` vs. two `i32` vs. one `i64` is negligible on ARM64 (Apple Silicon's 64-byte cache lines hold many fields regardless).

### 3.2 Option B: Packed i64

```
static_eval_packed: i64   // mg in upper 32 bits, eg in lower 32 bits
phase: u8
```

- Pack: `(mg as i64) << 32 | ((eg as i32) as i64 & 0xFFFF_FFFF)`.
- Unpack: `mg = (packed >> 32) as i32`, `eg = (packed & 0xFFFF_FFFF) as u32 as i32`.
- SIMD-style parallel delta: a single add to the packed field updates both mg and eg simultaneously, but only if deltas are themselves packed. The packing/unpacking overhead erases any gain for a non-SIMD CPU path.
- Debug round-trip: two asserts (one on the packed i64 + one on phase), or three if the values are unpacked before comparison.
- i64 is wider than needed (mg and eg are both i32-range); no overflow risk, but wastes the extra 32 bits compared to i32+i32.
- The TalkChess t=61850 packed-i32 discussion shows that i64 packing is the *safe* variant of the packed trick (no sign-extension hazard).

### 3.3 Option C: Packed i32 (mg in upper 16 bits, eg in lower 16 bits)

```
static_eval_packed: i32   // mg in upper 16 bits, eg in lower 16 bits
phase: u8
```

- Known as the "S(mg,eg) macro" idiom or "make_score" idiom in the C engine tradition (TalkChess t=61850 dissects this pattern).
- Pack: `(mg << 16) | (eg & 0xFFFF)`.
- Unpack mg: `packed >> 16` — but this requires arithmetic right shift for correct sign extension of negative mg values; Rust `i32 >> 16` is arithmetic, so this works.
- Unpack eg: the lower 16 bits must be sign-extended from 16 to 32 bits. In C this was done via union; in Rust it requires `(packed as i16) as i32` or explicit sign extension.
- **Critical hazard**: when accumulating incremental deltas, carrying bits from the lower 16-bit eg half into the upper 16-bit mg half produces silent corruption of mg. Specifically: if the eg field crosses the i16 boundary (±32 767), the carry propagates into the mg bits. PeSTO's centipawn magnitudes reach ±2000 for individual pieces; with 32 pieces summed, total eg can approach ±14 000 — well within i16 range for a single position, but mid-accumulation individual deltas can be large, and the hazard is a correctness time-bomb under unusual material. TalkChess t=61850 identifies this as the primary hazard: "bits from the lower 16-bit parts can overflow into the higher 16-bit part, requiring special handling."
- **Not recommended** for this project.

### 3.4 Recommendation: Option A (Explicit Triple)

| Criterion | Option A (triple) | Option B (packed i64) | Option C (packed i32) |
|-----------|-------------------|----------------------|----------------------|
| Sign-extension hazard | None | None | Yes — carry from eg into mg |
| Debug-round-trip cost | 3 asserts | 2 asserts | 2 asserts (if careful) |
| Incremental delta cost | 2 i32 adds + 1 u8 add | 1 i64 add (packed) + 1 u8 add | 1 i32 add + 1 u8 add |
| Code clarity | High | Medium | Low |
| Rust idiomatic | Yes | Yes | No (requires C-style sign tricks) |

Option A is recommended. The performance advantage of packed storage is marginal on a scalar CPU path — ARM64 can execute two i32 additions in a single cycle via instruction-level parallelism, matching Option B's single i64 add. Option C has a documented correctness hazard and should not be used.

The `static_eval_white: i32` field on `Position` becomes `(static_mg_white: i32, static_eg_white: i32, raw_phase: u8)`. The accessor `static_eval_white()` is replaced by the `evaluate()` consumer which does the blend.

---

## 4. Bishop Pair Term

### 4.1 Predicate

A side has the bishop pair when it controls **both color complexes** — at least one bishop on a light square AND at least one bishop on a dark square. Detection:

```
bishops = pos.pieces_colored(color, PieceKind::Bishop)
has_bishop_pair = (bishops & LIGHT_SQUARES).any() && (bishops & DARK_SQUARES).any()
```

Where `LIGHT_SQUARES: Bitboard` and `DARK_SQUARES: Bitboard` are precomputed constants. Equivalently, the popcount of each intersection must be ≥ 1. Rustic uses `trailing_zeros()` on single-bit boards, which is also correct when each side has exactly one bishop — but the popcount-and-intersection form handles the general case (two light-square bishops after promotion correctly returns `false` for the dark-square test).

The correct constant: in LERF indexing (a1=0, h8=63), the light squares are `0x55AA55AA55AA55AA` (a1 is dark, a2 is light — standard chess convention with a1 = dark square). Confirm against `Square::from_index(0).is_dark()` in the existing code before writing the constant.

### 4.2 Value Survey

| Source | MG | EG | Notes |
|--------|----|----|-------|
| Rustic chess engine (documented) | +10 | +40 | W(10, 40) — explicit in docs |
| Kaufman (1999, empirical) | +50 (no split) | +50 | Half a pawn; no MG/EG distinction |
| Simplified Eval (Michniewski) | +50 | +50 | No MG/EG split |
| CPW literature consensus | 30–50 | 40–60 | Range quoted in multiple sources |
| Typical Texel-tuned engines | 28–35 | 48–58 | Post-tuning converges toward this range |

The literature range for MG is 10–50; EG is 40–60. The cluster in Texel-tuned engines (engines that started at 50/50 and allowed the tuner to move values) tends toward MG ~30–35, EG ~48–55.

**Recommendation**: MG +30 cp / EG +50 cp as the literature-default starting values. This is conservative enough not to distort the SPRT signal from the tapering change itself, and the correct values will be recovered by the M6.I Texel pass.

### 4.3 Predicate Inversion vs. KBvKB-Same-Color

| Predicate | Condition | Value |
|-----------|-----------|-------|
| Bishop pair bonus | Both color complexes covered (bishops on different colors) | +30/+50 |
| KBvKB same-color draw | Each side exactly one bishop, both bishops on **same** color complex | Returns 0 |

These are logically opposite. The bishop pair requires **different** colors; the draw requires **same** colors. The implementation risk is a copy-paste error that checks the same predicate for both. The plan must use separate named functions: `has_bishop_pair(bishops: Bitboard) -> bool` and `is_same_color_bishop(wb: Square, bb: Square) -> bool`.

---

## 5. KBvKB Same-Color Insufficient Material

### 5.1 FIDE Rule

FIDE Laws of Chess Art. 5.2.2(b): the game is drawn when "it is not possible to reach a checkmate by any series of legal moves." King + bishop vs. King + bishop with both bishops on the same color is a mandatory draw because neither side can threaten the bishop's color complex.

### 5.2 Detection

```
// Precondition: total piece count == 4 (the caller checks this)
fn is_kbkb_same_color(pos: &Position) -> bool {
    let white_bishops = pos.pieces_colored(White, Bishop);
    let black_bishops = pos.pieces_colored(Black, Bishop);
    // Each side has exactly 1 bishop at this total count
    debug_assert_eq!(white_bishops.count(), 1);
    debug_assert_eq!(black_bishops.count(), 1);
    let wb_sq = white_bishops.trailing_zeros();
    let bb_sq = black_bishops.trailing_zeros();
    square_color(wb_sq) == square_color(bb_sq)
}
```

Where `square_color(sq) -> SquareColor` uses the same light/dark classification as §4.1. Alternatively: `(LIGHT_SQUARES >> wb_sq) & 1 == (LIGHT_SQUARES >> bb_sq) & 1`.

### 5.3 Placement in the Existing Insufficient-Material Guard

The existing `evaluate()` checks `total_count == 3` for KvN/KvB. The KBvKB case needs `total_count == 4` AND `white_bishops.count() == 1` AND `black_bishops.count() == 1` AND same-color test. The count structure aligns with the existing guard's style.

Contrast with KBvKB *opposite-color* (total_count == 4, both bishops present, but on different colors): this is **not** a draw — it is a draw *tendency* in practice but not a mandatory draw under FIDE, so it must not return 0 from `evaluate`.

### 5.4 Comparison to M3 Predicates

The M3 draw predicates (`KvK`, `KvN`, `KvB`) use simple count tests. The KBvKB predicate adds one bitboard intersection query on top of a count test. Cost is one popcount + one bit-and, approximately the same order as the existing M3 predicates.

---

## 6. Mop-Up Term for KQK / KRK

### 6.1 The Problem

In a KQK or KRK position, a pure material evaluation correctly identifies the side with a queen or rook as winning. But without a "corner-attractor" term, the losing king has no square preference, and the engine may waste many moves failing to confine it — or in a poorly searched tree, may not find mate at all within the horizon.

The CPW Mop-up Evaluation article states: "simple heuristics of a mop-up evaluation, only considering Center Manhattan-distance of the losing king and Chebyshev distance and/or Manhattan distance between both kings, along with a tiny search, should be sufficient to execute the mate in both KRK and KQK endgames."

### 6.2 Distance Metrics

| Metric | Definition | Bonus for whom |
|--------|-----------|----------------|
| Center Manhattan distance (CMD) of losing king | Sum of absolute rank-distance and file-distance from the center (d4/e4/d5/e5) | Higher CMD = king closer to edge/corner — **good** for winning side |
| Chebyshev distance between kings | max(|file_diff|, |rank_diff|) | Lower Chebyshev = kings closer — **good** for winning side (king proximity) |
| Manhattan distance between kings | |file_diff| + |rank_diff| | Same purpose as Chebyshev for "king closes in" |

CPW recommends a weighted sum of Chebyshev and Manhattan "to have a higher bonus for the corners and straight opposition rather than diagonal opposition."

### 6.3 Chess 4.x Formula (CPW Reference)

```
PosEval = 4.7 × CMD + 1.6 × (14 − MD)
```

Where CMD is the center Manhattan distance of the losing king and MD is the Manhattan distance between kings. This gives:
- Maximum mop-up bonus: `4.7 × 6 + 1.6 × 14 ≈ 50.6` — roughly half a pawn, which is small enough not to dominate the queen/rook material advantage but large enough to create a gradient.
- The `14 − MD` term gives higher bonus when kings are close (MD=2 → 12, MD=10 → 4).

For our engine's purposes, the rational approximation `4 × CMD + 2 × (14 − CMD_kings)` where `CMD_kings` uses the same center-distance metric for the inter-king term is simpler and preserves the rough gradient.

### 6.4 Simplified Integer Formula (Recommendation)

```
fn mop_up_bonus(losing_king: Square, winning_king: Square) -> i32 {
    let cmd = center_manhattan_distance(losing_king);
    let chebyshev_dist = chebyshev_distance(losing_king, winning_king);
    4 * cmd + 2 * (14 - chebyshev_dist)
}
```

- Range: CMD ∈ [0, 6] for corner (0,0) to center. Chebyshev ∈ [1, 7].
- Maximum: `4×6 + 2×13 = 50 cp`. Minimum (edge king, far kings): `4×0 + 2×(14−7) = 14 cp`.
- This bonus is applied as `+mop_up_bonus(losing_king, winning_king)` from the **winning side's perspective** (i.e., added to `static_eval_white` when white is winning, subtracted when black is winning).

### 6.5 Activation Threshold

Mop-up applies only when:
1. The winning side has a large material advantage (to avoid activating for equal positions or small advantages where mop-up is irrelevant).
2. The position is in a late endgame where it is meaningful.

Two approaches:
- **Phase-based**: activate when `raw_phase < 4`. At `phase < 4`, all queens are gone and at most one rook remains (2 rooks = phase 4). This is the roadmap's suggested threshold and aligns with "no queens + few rooks" endgame.
- **Material-based**: activate when `total_non_king_material_advantage > 300 cp` (winning side has at least a rook). More precise but requires an extra material computation.

**Recommendation**: `raw_phase < 4` gate from the roadmap. The phase field is already maintained incrementally; no additional computation needed. The phase gate is conservative (it includes KRK and KQK but also K+minor vs. K+rook combinations — mop-up in those is harmless since the weaker side tends to win anyway via material, and the mop-up nudge does not contradict that).

### 6.6 Scaling the Bonus

The raw bonus (+14 to +50 cp) is small relative to the material advantage (e.g., KQK has ~1025 cp base advantage). To prevent the mop-up term from dominating in positions where the advantage is uncertain (e.g., due to deep promotions in an unexpected direction), the bonus can be gated by ensuring the winning side truly has a material advantage:

```
if advantage_cp > 100 && raw_phase < 4 {
    eval += mop_up_bonus(losing_king, winning_king)
}
```

This is a simple guard; the M6.A plan should decide whether to apply this scaling or rely purely on the phase gate. The phase gate alone is likely sufficient at the M6.A stage; refinement via Texel tuning in M6.I will calibrate the magnitudes.

---

## 7. Mate-Score / Tapered-Eval Interaction

### 7.1 Score Band Invariant

The engine uses `MATE = 30_000` and `MATE_IN_MAX_PLY = MATE - MAX_PLY = 29_936`. Scores with `|score| >= MATE_IN_MAX_PLY` (i.e., ≥ 29 936) are treated as forced-mate scores by `score_to_uci`, `score_to_tt`, and mate-distance pruning.

Heuristic eval scores must satisfy `|score| < MATE_IN_MAX_PLY - 1 = 29_935`.

### 7.2 Worst-Case Material + PST Bound

At a position with nine queens and all minor pieces (absurd but possible during proofing):

| Component | Max contribution per side |
|-----------|--------------------------|
| 9 queens (EG) | 9 × 936 = 8 424 |
| 2 rooks (EG) | 2 × 512 = 1 024 |
| 2 knights (EG) | 2 × 281 = 562 |
| 2 bishops (EG) | 2 × 297 = 594 |
| 8 pawns (EG) | 8 × 94 = 752 |
| PST bonus max | ~500 (PeSTO PST values rarely exceed ±130 per piece; with 23 non-king pieces at max +130 = +2 990 but typical max is ~+500 over baseline) |

Total one-sided worst-case: 8 424 + 1 024 + 562 + 594 + 752 + 500 ≈ **11 856 cp**. Bilateral imbalance (one side has max, other side has only kings): ~11 856 cp.

Adding bishop-pair bonus (+50) and mop-up (+50) brings the practical ceiling to approximately **12 000 cp** — well below 29 935.

Even the MG path is bounded: 9 × 1025 = 9 225 for queens alone; full MG material tops at roughly **13 000 cp** one-sided.

**No overflow concern exists for the centipawn eval band at piece-saturation positions.** The PST magnitudes in PeSTO are all small (most entries within ±130 cp); even with inflated PSTs the sum cannot approach the 29 935 boundary.

### 7.3 Recommended Debug Assert

Add to `evaluate(&Position)`:

```rust
debug_assert!(
    result.abs() < MATE_IN_MAX_PLY - 1,
    "eval score {} outside centipawn band [-{}, +{}]",
    result, MATE_IN_MAX_PLY - 1, MATE_IN_MAX_PLY - 1
);
```

This assert fires in debug builds only and catches:
- Accidental inclusion of mate-score constants in PST data.
- Arithmetic overflow in the blend formula (unlikely but detectable).
- Off-by-one bugs where a PST value has an accidental 10× typo.

The bound check costs one comparison per evaluate call in debug builds — negligible compared to the evaluation computation itself.

---

## 8. Debug-Build Round-Trip Assert Cost

### 8.1 Current Cost

ADR-0014 §6 describes an always-on debug-build assert in `make_move`/`unmake_move`:
```
post-make assert: static_eval_white == eval_white_from_scratch(&pos)
```

C4 proptest at `src/mov.rs:3434`: 256 cases, 4-ply random walk, approximately 256 × 4 = 1 024 `eval_white_from_scratch` calls per test run.

### 8.2 Under Tapering

The triple layout `(mg_white, eg_white, phase)` extends the assert to three comparisons:
```
debug_assert_eq!(pos.static_mg_white(), eval_mg_white_from_scratch(&pos));
debug_assert_eq!(pos.static_eg_white(), eval_eg_white_from_scratch(&pos));
debug_assert_eq!(pos.raw_phase(), eval_phase_from_scratch(&pos));
```

Or equivalently, `eval_state_from_scratch` returns a `(i32, i32, u8)` tuple and the assert is a single tuple comparison.

Cost increase: `eval_white_from_scratch` currently iterates 64 squares and sums PSQT contributions (one lookup per occupied square). Under tapering, the from-scratch recomputation does two PSQT lookups per piece (MG table + EG table) plus one phase-table lookup. This roughly doubles the from-scratch cost per call.

### 8.3 C4 Proptest Budget

C4 uses 256 proptest cases × 4 plies × ~1 ns per make_move-without-assert + assert cost.

With the tapering extension:
- `eval_white_from_scratch` cost: roughly 2× current (two PST lookups per piece instead of one).
- Three asserts instead of one: the assert itself is cheap; the `eval_*_from_scratch` call is the cost.
- Combined: approximately 3× the current debug-build C4 cost per ply.

The roadmap scope detail notes: "M6.A's test plan includes a timing-sanity-check on C4 and adjusts proptest `cases` if necessary." The plan should:
1. Time C4 after implementation.
2. If C4 exceeds the proptest default timeout (1 second per test on most CI systems), halve `cases` from 256 to 128 — which still provides ~500 assert checks and maintains adequate coverage.

### 8.4 Refactoring Option

An alternative: combine all three from-scratch computations into one pass of `eval_state_from_scratch(&pos) -> (i32, i32, u8)` that iterates the board once and returns `(mg_white, eg_white, phase)` in a single loop. This reduces the debug-mode overhead from 3 separate full-board iterations to 1. It also simplifies the assert to:

```rust
debug_assert_eq!(
    (pos.static_mg_white(), pos.static_eg_white(), pos.raw_phase()),
    eval_state_from_scratch(pos),
    "incremental eval/phase mismatch after make_move"
);
```

Recommended: implement `eval_state_from_scratch` as the single-pass combined recompute.

---

## 9. Risks and Gotchas

### 9.1 Phase Clamp — Most Dangerous Omission

Failing to clamp `raw_phase` to 24 before division in the blend formula produces wrong scores for any position with a promoted queen. The symptom is large negative EG scores even in winning positions. The clamp `let phase = raw_phase.min(24)` must be in the blend call, not in the accumulation step.

### 9.2 Bishop Pair vs. KBvKB Predicate Confusion

Both predicates inspect bishop positions. They are logically opposite:
- Bishop pair: bishops on **different** color complexes → bonus.
- KBvKB draw: both bishops on **same** color complex → zero return.

A copy-paste error producing `!has_bishop_pair(...)` for the draw predicate would have the right structure but the wrong semantics. Use explicitly named functions with different signatures.

### 9.3 Light/Dark Square Mask — LERF Alignment

The light/dark square masks must match the engine's LERF indexing (a1=0). In standard chess, a1 is a dark square. The light-squares mask in LERF is `0xAA55AA55AA55AA55` (a2, b1, c2, ... are light). Verify against a printed chessboard before hardcoding. An inverted mask produces bishop-pair bonuses for engines with only one same-colored bishop and does not detect KBvKB draws correctly.

### 9.4 EG PST for Kings — Large Positive Entries

The PeSTO EG king PST has large positive values in the center (up to +45 for central squares). In a bare-king endgame the king PST is the dominant positional term and correctly drives the king toward the center. This is intentional and correct. The EG king PST should not be confused with the MG king PST (which has large negatives for exposed central squares).

### 9.5 `phase` Field Must Not Gate Insufficient-Material Predicates

The insufficient-material checks (`KvK`, `KvN`, `KvB`, `KBvKB same-color`) must be evaluated before the tapered blend. In these positions `phase = 0` or very small, but the correct return value is 0 (draw), not a near-zero blend result. The code structure should be:

```rust
pub fn evaluate(pos: &Position) -> i32 {
    if is_insufficient_material(pos) { return 0; }
    let blend_score = ...;
    blend_score // signed for side-to-move
}
```

### 9.6 Mop-Up Direction Must Be Conditionally Applied

The mop-up formula is always a bonus for the **stronger side** and a penalty for the **weaker side**. The formula needs to be applied symmetrically:

```
if white_advantage {
    white_mop_up = mop_up_bonus(black_king, white_king)
} else if black_advantage {
    black_mop_up = mop_up_bonus(white_king, black_king)
}
```

A bug where mop-up is always applied from white's perspective would produce negative bonuses in KQK-with-black-winning positions.

### 9.7 Raw Phase Accumulation Must Handle Captures and Promotions Correctly

The incremental phase update in `make_move`:
- **Capture**: the captured piece's phase delta must be subtracted (`raw_phase -= PHASE_DELTA[captured_kind]`).
- **Promotion**: the pawn leaves (PHASE_DELTA[Pawn] = 0, so no subtraction) and the promoted piece enters (`raw_phase += PHASE_DELTA[promoted_kind]`).
- **EP capture**: the captured pawn's PHASE_DELTA is 0 (pawns have delta 0), so the net change is 0.

Forgetting to add the promoted piece's delta leaves `raw_phase` too low after promotion — the blend would use an artificially endgame-skewed phase for a position that just got a queen.

### 9.8 Proptest Seeds Cover Limited Opening Positions

C4's proptest uses 6 fixed FEN seeds. Those seeds include Kiwipete and the M1 test positions but do not include:
- K+Q vs. K (mop-up activation path).
- KBvKB same-color endgame.
- Positions with promoted queens (phase > 24 before clamp).

The plan must add named (non-proptest) unit tests for each of these edge cases to complement C4's random walk coverage.

---

## 10. Recommendations Summary

1. **Phase formula**: use `[P=0, N=1, B=1, R=2, Q=4, K=0]` summing to 24, exactly as specified in the roadmap and matching PeSTO / CPW. Accumulate `raw_phase: u8` incrementally; clamp to 24 only at blend time.

2. **Blend formula**: `(mg × min(raw_phase, 24) + eg × (24 − min(raw_phase, 24))) / 24` with Rust truncation-toward-zero. No round-to-nearest correction needed. Products fit safely in `i32`.

3. **Storage layout**: use the explicit triple `(static_mg_white: i32, static_eg_white: i32, raw_phase: u8)` on `Position`. Do not use the packed-i32 trick (documented sign-extension hazard at i16 saturation). The packed-i64 is safe but offers no advantage over the explicit triple. Provide a single-pass `eval_state_from_scratch` helper for the debug round-trip assert.

4. **Bishop pair bonus**: MG +30 cp / EG +50 cp as literature-default starting values, recoverable by Texel tuning in M6.I. Predicate: `(bishops & LIGHT_SQUARES).any() && (bishops & DARK_SQUARES).any()`. Implement as a named function distinct from the KBvKB-same-color predicate.

5. **KBvKB same-color draw**: predicate `square_color(wb_sq) == square_color(bb_sq)` activated when total piece count is 4, white has exactly one bishop, and black has exactly one bishop. Returns 0 from `evaluate`. The predicate checks for **same** color — opposite from the bishop-pair condition.

6. **Mop-up activation**: gate on `raw_phase < 4` (no queens, at most one rook present). Formula `4 × CMD + 2 × (14 − chebyshev_kings)` where CMD is the center Manhattan distance of the losing king and `chebyshev_kings` is the Chebyshev distance between the two kings. Apply only when the winning side has a material advantage (`advantage_cp > 100`). Maximum bonus ~50 cp; does not approach the mate-score band.

7. **Mate-score / eval interaction**: the centipawn ceiling `MATE_IN_MAX_PLY - 1 = 29 935` is unreachable by any PST-based evaluation. Worst-case nine-queen position with PeSTO material values produces at most ~13 000 cp one-sided raw material, plus PST and bonus terms bringing the practical ceiling to ~14 000 cp — safely below the 29 935 threshold. Add a `debug_assert!(result.abs() < MATE_IN_MAX_PLY - 1)` in `evaluate` as a safety net.

8. **Debug round-trip assert cost**: extend to tuple `(mg_white, eg_white, raw_phase)` via a single-pass `eval_state_from_scratch`. Expected 2–3× cost increase vs. current C4. If C4 exceeds proptest timeout, reduce `cases` from 256 to 128.

9. **Light/dark square masks**: verify that `LIGHT_SQUARES` matches the engine's LERF convention (a1=dark, a2=light) before hardcoding. Inversion of this mask inverts both the bishop-pair bonus and the KBvKB draw predicate simultaneously — a subtle, hard-to-catch bug.

10. **Unit test coverage beyond proptest**: add named tests for (a) KQK mop-up activation, (b) KBvKB same-color → 0, (c) KBvKB opposite-color → non-zero, (d) position with promoted queen (raw_phase > 24) to confirm clamping, (e) bishop-pair bonus visible in a simple bishop+king vs. king position.

---

## Citations

- [CPW — Tapered Eval](https://www.chessprogramming.org/Tapered_Eval)
- [CPW — PeSTO's Evaluation Function](https://www.chessprogramming.org/PeSTO%27s_Evaluation_Function)
- [CPW — CPW-Engine eval](https://www.chessprogramming.org/CPW-Engine_eval)
- [CPW — Bishop Pair](https://www.chessprogramming.org/Bishop_Pair)
- [CPW — Mop-up Evaluation](https://www.chessprogramming.org/Mop-up_Evaluation)
- [CPW — Draw Evaluation](https://www.chessprogramming.org/Draw_Evaluation)
- [CPW — KRK](https://www.chessprogramming.org/KRK)
- [CPW — Score](https://www.chessprogramming.org/Score)
- [CPW — King Centralization](https://www.chessprogramming.org/King_Centralization)
- [CPW — Bishops of Opposite Colors](https://www.chessprogramming.org/Bishops_of_Opposite_Colors)
- [TalkChess t=77546 — Game Phase and tapered PSQT evaluation](https://talkchess.com/viewtopic.php?t=77546)
- [TalkChess t=51656 — Tapered evaluation](https://talkchess.com/viewtopic.php?t=51656)
- [TalkChess t=61850 — Packed score representation (Stockfish dissection)](https://talkchess.com/viewtopic.php?t=61850)
- [TalkChess t=76265 — Tapered Evaluation and MSE (Texel Tuning)](https://talkchess.com/forum3/viewtopic.php?t=76265)
- [Mediocre Chess — Guide: Tapered Eval](http://mediocrechess.blogspot.com/2011/10/guide-tapered-eval.html)
- [Rustic chess engine — Tapering the evaluation](https://rustic-chess.org/evaluation/tapering.html)
- [Rustic chess engine — Detecting FIDE draws](https://rustic-chess.org/board_functionality/detecting_fide_draws.html)
- [MadChess — Tapered Evaluation series](https://www.madchess.net/tag/tapered-evaluation/)
- [PeSTO blog post (Friederich)](https://rofchade.nl/?p=307)
