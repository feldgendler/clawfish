# M3 Evaluation — Material + Piece-Square Tables

Project context: the engine has a complete move generator, make/unmake with Undo, Zobrist hashing, and UCI infrastructure. M3 adds the first real search. Eval is the only new component being designed here.

## Headline calls

- **Use PeSTO middlegame values verbatim** as the M3 starting point. They are Texel-tuned, publicly published as data, and well-characterized.
- **Single-phase (middlegame only) is fine for M3.** Main loss: king behavior in bare-king endgames. Acceptable vs the SPRT target. Tapering = M6 per roadmap.
- **Eval perspective: always return from side-to-move.** Compute `white_score - black_score`, then negate if Black is to move.
- **Symmetry via rank-flip at lookup time.** Store one table; black accesses square directly, white accesses `square ^ 56`. Zero storage overhead.
- **Incremental eval from M3.A.** Add `pst_score: i32` to `Undo` and update in `make_move`/`unmake_move`. NNUE-ready hook is already the design.
- **Defer bishop pair and same-color-bishop draw check to M6.** Both are 2–5 LOC but add noise to M3's test surface.

## 1. Material values

### Classical vs. centipawn vs. PeSTO

| System | P | N | B | R | Q | Source |
|---|---|---|---|---|---|---|
| Classical ratio | 1 | 3 | 3 | 5 | 9 | Human tradition |
| Simplified (Michniewski) | 100 | 320 | 330 | 500 | 900 | CPW Simplified Eval |
| PeSTO middlegame | 82 | 337 | 365 | 477 | 1025 | CPW PeSTO |
| PeSTO endgame | 94 | 281 | 297 | 512 | 936 | CPW PeSTO |

### Recommendation for M3 material values

```
P = 82
N = 337
B = 365
R = 477
Q = 1025
K = 0  (not scored; terminal detection owns checkmate/stalemate)
```

These are already tuned; no bishop pair at M3.

## 2. Piece-square table source

### Options

| Source | Provenance | Phase support |
|---|---|---|
| Simplified Eval (Michniewski) | Hand-designed; pedagogical | Single-phase |
| PeSTO (Friederich) | Texel-tuned; widely adopted as baseline | Two-phase (MG + EG) |
| Wukong | GNU Chess heritage | Single-phase |
| GNU Chess origin | Historical | Single-phase |

### Recommendation

**PeSTO middlegame tables.** Texel-tuned against actual game data. MinimalChess gained ~200 Elo switching from Simplified to PeSTO-style tapered tables.

### Citation

Tables at https://www.chessprogramming.org/PeSTO%27s_Evaluation_Function — sourced from Ronald Friederich's RofChade TalkChess post and posted at https://rofchade.nl/?p=307.

## 3. Single-phase choice

### Why single-phase is acceptable for M3

- SPRT target is "beats RandomMover ~100%." Eval quality is not the bottleneck.
- King behavior in MG PST is wrong in endgames (penalizes central king), but at M3 depths the search tree mostly sees middlegame.
- Empirical: MinimalChess 0.3 (non-tuned non-tapered PST) reached **1439 Elo on CCRL** — far above the M3 target.
- MadChess 2.0 gained 107 Elo from tapered eval — meaningful but well above what M3 needs.

### Known weakness to document

- Engine plays poorly in bare-king endgames (passive king).
- May fail to convert KQ vs K or KR vs K within reasonable depth.
- Both addressed by tapering at M6.

## 4. Eval perspective

### Convention

**Always return from the side-to-move's perspective** (negamax requires this).

### Standard formulation

```
absolute_score = (white_material + white_pst) - (black_material + black_pst)
eval_for_search = if side_to_move == White { absolute_score } else { -absolute_score }
```

A score of +100 means "the side to move is up ~1 pawn."

## 5. Symmetry property

### Convention

PSTs defined from White's perspective. Black uses the same table with the square index rank-flipped.

### Implementation

- Store one set of tables (per piece type, 64 entries each).
- **White lookup:** `table[piece][square ^ 56]`.
- **Black lookup:** `table[piece][square]`.

### Open question to confirm at plan time

Verify the engine's internal square numbering. If a1 = 0 and h8 = 63 (LSB=a1 / LERF), the flip is `square ^ 56`. If a8 = 0 and h1 = 63, the flip is `square ^ 7`. Confirm before writing the lookup.

## 6. Insufficient-material draws

### Positions that are draws

| Position | FIDE status | Detection cost |
|---|---|---|
| KvK | Mandatory draw | Trivial: total piece count = 2 |
| KvN (either side) | Mandatory draw | Trivial |
| KvB (either side) | Mandatory draw | Trivial |
| KBvKB, same-color bishops | Mandatory draw | Requires bishop-square color check |

### Recommendation for M3

Implement KvK/KvN/KvB in eval (return 0). Skip same-color bishops at M3 (rare at M3 depth). Cost is negligible (~4 ns per leaf eval).

## 7. Endgame edge cases (KQ vs K, KR vs K, KP vs K)

- A pure material eval knows it is winning by 500–900 cp.
- Without "mop-up" (corner-the-lone-king bonus), the engine may fail to convert within practical depth.
- **Recommendation for M3:** accept this limitation. Mop-up belongs in M6.

## 8. Incremental eval via make/unmake hooks

### Performance

| Approach | Cost |
|---|---|
| Full-board recompute | ~32 ops × 1 ns ≈ 30–40 ns per leaf eval |
| Incremental delta | ~5–8 ns per `make_move`/`unmake_move` |

Movegen is 80–270 ns/call (M1.G); full eval at leaves is 10–40% overhead.

### Complexity

- `Undo` already carries 5 fields; adding `prior_pst_score: i32` is a one-field extension.
- ~5 additional lines in `make_move` (load entering-square PST + leaving-square PST; sum delta).

### NNUE alignment

- ADR-0004: make/unmake is the NNUE hook point.
- Incremental PST score = exact same pattern as the NNUE accumulator update.
- Implementing incremental PST now makes the NNUE hook structural, not exploratory.

### Recommendation

**Incremental from M3.A.** Minimal complexity; real performance benefit; aligns with existing decisions.

## 9. Performance estimate

### Full-board recompute per leaf (Apple M4)

| Operation | Count | Total |
|---|---|---|
| Iterate piece bitboards | 12 popcount loops | ~32 ns for 16 avg pieces |
| PST lookups + sums | included | (above) |
| **Total** | | **~30–40 ns** |

### Incremental update per make/unmake

| Operation | Count | Cost |
|---|---|---|
| Load prior score from Undo | 1 | ~1 ns |
| PST lookup (from-square, to-square) | 2 | ~2 ns |
| Add capture value | conditional | ~1 ns |
| Store delta | 1 | ~1 ns |
| **Total** | | **~5–8 ns** |

## 10. Tuning

- **Vendor PeSTO MG values verbatim** (Texel-tuned data).
- **No tuning at M3.** Texel tuning requires a game database and is an M6 deliverable.
- **Snapshot the values in this report** as the M3 citation.

## 11. Test strategy

### Required tests

| Test | What it verifies |
|---|---|
| Starting position evaluates to 0 | White/black symmetry |
| One-pawn-up evaluates to +82 cp | Correct material value |
| Same position, side flipped → negated score | Eval perspective correctness |
| Mirror-image position → 0 | Full board symmetry |
| KvK / KvN / KvB → 0 | Insufficient material |

### Property tests

- **Color-swap invariant:** for any reachable P, `eval(P, White) == -eval(mirror(P), Black)`.
- **PST symmetry:** white PST value at `s` equals black PST value at `s ^ 56`.

## Recommended PST data (vendor verbatim for M3.A)

All values from PeSTO's Evaluation Function (Texel-tuned by Ronald Friederich).

### Material values (middlegame)

```
P = 82, N = 337, B = 365, R = 477, Q = 1025, K = 0
```

### PST orientation note

Tables laid out with **a8 at index 0, h8 at index 7, a1 at index 56, h1 at index 63** — standard CPW/PeSTO array order. Black accesses `table[square]` directly; White accesses `table[square ^ 56]`.

### MG Pawn PST

```
  0,   0,   0,   0,   0,   0,   0,   0,
 98, 134,  61,  95,  68, 126,  34, -11,
 -6,   7,  26,  31,  65,  56,  25, -20,
-14,  13,   6,  21,  23,  12,  17, -23,
-27,  -2,  -5,  12,  17,   6,  10, -25,
-26,  -4,  -4, -10,   3,   3,  33, -12,
-35,  -1, -20, -23, -15,  24,  38, -22,
  0,   0,   0,   0,   0,   0,   0,   0
```

### MG Knight PST

```
-167, -89, -34, -49,  61, -97, -15, -107,
 -73, -41,  72,  36,  23,  62,   7,  -17,
 -47,  60,  37,  65,  84, 129,  73,   44,
  -9,  17,  19,  53,  37,  69,  18,   22,
 -13,   4,  16,  13,  28,  19,  21,   -8,
 -23,  -9,  12,  10,  19,  17,  25,  -16,
 -29, -53, -12,  -3,  -1,  18, -14,  -19,
-105, -21, -58, -33, -17, -28, -19,  -23
```

### MG Bishop PST

```
-29,   4, -82, -37, -25, -42,   7,  -8,
-26,  16, -18, -13,  30,  59,  18, -47,
-16,  37,  43,  40,  35,  50,  37,  -2,
 -4,   5,  19,  50,  37,  37,   7,  -2,
 -6,  13,  13,  26,  34,  12,  10,   4,
  0,  15,  15,  15,  14,  27,  18,  10,
  4,  15,  16,   0,   7,  21,  33,   1,
-33,  -3, -14, -21, -13, -12, -39, -21
```

### MG Rook PST

```
 32,  42,  32,  51,  63,   9,  31,  43,
 27,  32,  58,  62,  80,  67,  26,  44,
 -5,  19,  26,  36,  17,  45,  61,  16,
-24, -11,   7,  26,  24,  35,  -8, -20,
-36, -26, -12,  -1,   9,  -7,   6, -23,
-45, -25, -16, -17,   3,   0,  -5, -33,
-44, -16, -20,  -9,  -1,  11,  -6, -71,
-19, -13,   1,  17,  16,   7, -37, -26
```

### MG Queen PST

```
-28,   0,  29,  12,  59,  44,  43,  45,
-24, -39,  -5,   1, -16,  57,  28,  54,
-13, -17,   7,   8,  29,  56,  47,  57,
-27, -27, -16, -16,  -1,  17,  -2,   1,
 -9, -26,  -9, -10,  -2,  -4,   3,  -3,
-14,   2, -11,  -2,  -5,   2,  14,   5,
-35,  -8,  11,   2,   8,  15,  -3,   1,
 -1, -18,  -9,  10, -15, -25, -31, -50
```

### MG King PST

```
-65,  23,  16, -15, -56, -34,   2,  13,
 29,  -1, -20,  -7,  -8,  -4, -38, -29,
 -9,  24,   2, -16, -20,   6,  22, -22,
-17, -20, -12, -27, -30, -25, -14, -36,
-49,  -1, -27, -39, -46, -44, -33, -51,
-14, -14, -22, -46, -44, -30, -15, -27,
  1,   7,  -8, -64, -43, -16,   9,   8,
-15,  36,  12, -54,   8, -28,  24,  14
```

## Open questions

1. **Square numbering** — the flip formula assumes a1=0 / h8=63. Confirm against the existing bitboard code.
2. **Pawn PST on rank 1 and rank 8** — non-zero in PeSTO tables but unreachable in practice. Eval should either assert unreachable or leave dead data.
3. **King PST score inclusion** — king material value is 0, but king PST is non-zero and should be included.

## Citations

- [CPW — Simplified Evaluation Function](https://www.chessprogramming.org/Simplified_Evaluation_Function)
- [CPW — PeSTO's Evaluation Function](https://www.chessprogramming.org/PeSTO%27s_Evaluation_Function)
- [MinimalChess 0.4 TalkChess thread](https://talkchess.com/viewtopic.php?t=77089)
- [MinimalChess 0.3 TalkChess thread](https://talkchess.com/viewtopic.php?t=76823)
- [MadChess tapered evaluation](https://www.madchess.net/tag/tapered-evaluation/)
- [CPW — Piece-Square Tables](https://www.chessprogramming.org/Piece-Square_Tables)
- [CPW — Endgame](https://www.chessprogramming.org/Endgame)
- [CPW — Tapered Eval](https://www.chessprogramming.org/Tapered_Eval)
- [CPW — Material](https://www.chessprogramming.org/Material)
- [CPW — Draw Evaluation](https://www.chessprogramming.org/Draw_Evaluation)
- [CPW — Incremental Updates](https://www.chessprogramming.org/Incremental_Updates)
- [Rustic chess engine PST article](https://rustic-chess.org/evaluation/psqt.html)
- [PeSTO blog post (Friederich)](https://rofchade.nl/?p=307)
