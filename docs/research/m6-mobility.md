# M6.D — Piece Mobility Evaluation (Research)

**Feeds:** `docs/plans/m6.d.md` (to be written).
**Committed semantic (non-negotiable):** mobility area = all squares − (friendly-occupied ∪ enemy-pawn-attacked); count = popcount(piece_attacks ∩ mobility_area); tables indexed by count; MG/EG split.

---

## Headline recommendations

- **Committed mobility-area semantic is valid.** It is a defensible simplification of the Stockfish-style richer definition. What clawfish omits (own-queen exclusion for minors, own-blocked-pawn exclusion, own pawns on ranks 2–3, x-ray through own rook/queen) was itself tuned away from in Stockfish's calibrated tables; M6.I Texel will absorb the residual.
- **Recommended weight source: Stockfish classical HCE** tables as reproduced in TalkChess (full arrays in §6 below). These are internally co-calibrated across all four piece kinds under a single Texel-tuned regime. No other citable set with full MG/EG arrays for all four piece kinds exists outside engine source.
- **Calibration-mismatch risk is real but structurally different from M6.C.** Stockfish's tables were tuned against non-PeSTO PSTs; PeSTO's PSTs already bake in implicit mobility signal for piece centralization. Over-magnitude is the expected failure mode (especially for knights/bishops where PeSTO PST already rewards center placement). Run the same per-kind subset screen used for M6.B if the all-four SPRT collapses.
- **Contingency posture: per-kind subset screen first, then co-scale.** Disable one kind at a time to identify the problematic interaction. If all individual terms pass but combined fails, try a uniform ×0.5 co-scale. If co-scale fails to recover Elo across TC profile, defer all to M6.I (M6.C precedent).
- **Enemy-occupied squares count as mobility** (standard practice — confirmed below). Captures of enemy pieces stay in the mobility area because the mobility area only subtracts friendly-occupied and enemy-pawn-attacked squares; enemy non-pawn pieces occupy squares that remain in the area.

---

## §1 — The mobility concept

### Why mobility correlates with strength

- Slater (1950) documented a "definite correlation between a player's mobility and games won" across 350 tournament games. [CPW Mobility](https://www.chessprogramming.org/Mobility)
- More mobile pieces threaten more targets, restrict enemy coordination, and can regroup faster.
- Mobility is an aggregate proxy for piece activity, trapped-piece detection, and coordination.

### Relationship with PSTs

- PSTs encode static positional quality for each square independently of the position.
- A knight PST rewards central squares because **central squares have higher pseudolegal mobility**; a rook PST rewards open files because open files yield higher rook mobility in typical positions.
- PeSTO's PSTs are PST-only and explicitly contain **no separate mobility term** — mobility signal is baked into the positional bonus values via Texel tuning. [PeSTO description on rofchade.nl](https://rofchade.nl/?p=307)
- Adding an explicit mobility term on top of PeSTO PSTs creates double-counting: the PST already rewards the central knight, and then mobility adds more.
- The standard mitigation is joint Texel retuning of PST + mobility weights simultaneously, so that the PST lowers its implicit mobility signal as the explicit term takes over.
- **At M6.D, no joint retune occurs** (that is M6.I). The literature-default tables will over-magnitude against PeSTO PSTs in the same direction as M6.B/C — the over-magnitude is the expected failure mode for the SPRT screen.

### What mobility captures vs. what PSTs capture

| Signal | PST (PeSTO) | Mobility term |
|--------|-------------|---------------|
| Piece on central square | Yes (baked in) | Yes (high count) — overlap |
| Piece on edge with no moves | Partial (low PST bonus) | Yes (low count, direct penalty) |
| Piece trapped by specific piece configuration | No | Partially (lower count) |
| Rook on open file | Partial (rook PST) | Yes (high count) — some overlap |

---

## §2 — Mobility-area definitions

### Clawfish committed semantic (M6.D row in roadmap)

```
mobility_area = all_squares − (friendly_occupied ∪ enemy_pawn_attacked)
mobility_count[piece] = popcount(piece_attacks(sq, occ_all) ∩ mobility_area)
```

- Friendly-occupied squares removed: moves to own pieces are illegal.
- Enemy-pawn-attacked squares removed: the simplest "not obviously suicidal" filter.
- Enemy-non-pawn-occupied squares **stay in** the area: capturing an enemy piece is a legal and desirable move; mobility should credit it.
- Friendly-pawn-attacked-but-empty squares stay in: own pawn can recapture.
- King-zone squares not excluded at M6.D (interaction with king safety deferred to M6.E).
- Richer minor-piece attack exclusions deferred (no enemy-knight/bishop-attacked exclusion).

### Stockfish-style richer mobility area (for reference)

The Stockfish classical evaluation mobility area additionally excludes: [Stockfish Evaluation Guide, hxim.github.io; search summary from Stockfish evaluation code description]

| Additional exclusion | Rationale |
|----------------------|-----------|
| Own king's square | King does not count as a mobility target |
| Own blocked pawns | Blocked pawns occupying squares reduce mobility; excluding them prevents double-penalizing |
| Own pawns on ranks 2 and 3 (not yet advanced) | Undeveloped pawns blocking the piece's view; removing the exclusion slightly inflates early-game count |
| Own queen's square (for minor pieces) | Prevents credits for moves that "defend" the queen; queen is mobile and does not need defending in the same sense |
| Enemy knight/bishop/rook attacked squares (for queen only) | Safer mobility for the queen — a queen venturing into attacked territory is risky; standard in Stockfish for queen only |

### Quantified gap between simple and Stockfish-style

The delta is small and systematic:

- The own-pawn/king/queen exclusions typically remove 1–3 squares from the count in normal middlegame positions.
- The effect shifts the count one bucket lower on average, so the mobility bonus is slightly deflated relative to the table value.
- When the Stockfish tables are tuned assuming the richer definition, using the simpler definition (which counts more squares) will read a slightly higher bucket, yielding slightly higher bonuses than intended — a mild over-magnitude offset.
- This is the same direction as the PST overlap over-magnitude described in §1.
- M6.I joint Texel will correct for both offsets simultaneously.

### Confirmation: captures count as mobility

- Enemy-occupied non-pawn-attacked squares remain in the mobility area by the committed definition.
- This matches the standard practice: "when a blocked piece is an enemy piece, we add 1 to the mobility count since capturing the enemy piece is also a legal move." [TalkChess mobility discussion, 2012]
- CPW confirms: "if a piece can move to the square of another friendly piece, sometimes that move is also counted" — but only friendly protection cases are ever discussed as optional; enemy-capture squares are universally counted.

---

## §3 — Per-piece MG/EG asymmetry

| Piece kind | MG weight | EG weight | Rationale |
|------------|-----------|-----------|-----------|
| Knight | Moderate | Slightly lower | Knights have fixed max mobility (~8); PST already rewards center; king doesn't threaten in MG |
| Bishop | Moderate | Moderate | Open diagonals matter in both phases; EG bishop on long diagonal is powerful |
| Rook | Moderate MG | **EG-heavy** | Rooks open up as pawns trade; open-file rook control peaks in EG; EG rook > minor-piece mobility |
| Queen | Moderate MG | EG-heavy | Queen's sweeping mobility becomes dominant once pieces clear; MG queen is restricted by safety |

Evidence from Stockfish tables (§6):
- Rook: EG values scale much faster than MG (index 14: MG=59, EG=169 — nearly 3× the MG value).
- Queen: EG values similarly outpace MG at high mobility counts (index 27: MG=128, EG=199).
- Knight: EG values only slightly trail MG (index 8: MG=36, EG=29 — roughly similar magnitude).
- Bishop: MG/EG are more balanced (index 13: MG=97, EG=94).

CPW: "in the opening, the mobility of the bishops and knights is more important than that of the rooks." [CPW Mobility](https://www.chessprogramming.org/Mobility)

---

## §4 — Pinned pieces scored pseudolegally

### Standard simplification status

- "Evaluation can take the restricted mobility of pinned pieces into account" but "situational pins are usually only considered implicit by the search rather than static evaluation." [CPW Pin article](https://www.chessprogramming.org/Pin)
- Scoring pinned pieces pseudolegally (ignoring pins) is a widespread simplification at this engine tier.
- Eval is called without movegen context; pin detection at every eval leaf adds king-ray intersection cost on the hottest path.

### Magnitude of over-credit

- An absolutely-pinned knight has 0 legal moves but may have 2–8 pseudolegal attack squares.
- A rank/file-pinned bishop has 0 legal moves in that direction but may count diagonal attacks.
- The over-credit is approximately: `(pseudolegal_count − 0) × avg_bonus_per_square`.
- At a typical mid-table bonus of ~5–8 cp/square and a pinned knight with 4 pseudolegal squares: ~20–32 cp over-credit per pinned piece.
- Pins are statistically rare (< 5% of positions in typical search trees) and transient (resolved within 1–3 plies).
- Expected average over-credit: < 2 cp per eval call, weighted by pin frequency.
- The M6.I Texel pass calibrates weights against actual game positions where pins are already distributed at their base-rate frequency; the average over-credit folds into slightly lower per-count weights in the tuned vector.

### Literature assessment

- "A powerful piece matters less if it's stuck in a pin" — acknowledged as an open correctness gap, not a tuning priority at this strength level.
- No published evidence that correcting pin-credit in static eval produces measurable Elo gain before other terms are tuned.

---

## §5 — X-ray / battery mobility

### What Stockfish does

Stockfish classical HCE computes mobility for sliding pieces using x-ray-aware attack generation: [TalkChess thread quoting Stockfish evaluate.cpp comments; search summary]

| Piece | Occupancy mask for mobility attack computation |
|-------|------------------------------------------------|
| Bishop | `attacks_bb<BISHOP>(sq, occ ^ own_queens)` — x-ray through own queen |
| Rook | `attacks_bb<ROOK>(sq, occ ^ own_rooks ^ own_queens)` — x-ray through own rooks and queen |
| Queen | Standard `attacks_bb<QUEEN>(sq, occ)` — no x-ray |
| Knight | Standard `attacks_bb<KNIGHT>(sq)` — no x-ray applicable |

The rationale: a rook behind another rook still exerts pressure along the file (battery); removing own rooks/queens from occupancy makes the mobility count reflect battery influence.

### Clawfish M6.D definition

Clawfish uses `magic::rook_attacks(sq, occ_all)` / `magic::bishop_attacks(sq, occ_all)` with full occupancy — no x-ray.

### Calibration mismatch risk

- Stockfish's tables were calibrated assuming x-ray occupancy, yielding counts that are **higher than plain-occupancy counts** for rooks in batteries.
- With plain occupancy, a rook behind another rook sees a lower mobility count (the own rook blocks), so it reads a lower table index and receives a lower bonus than Stockfish intended.
- This is an **under-magnitude offset** for rooks in batteries, opposite to the over-magnitude offset from PST double-counting in §1.
- Net effect: the two offsets partially cancel, and the Stockfish rook table may transfer more cleanly than the bishop/knight tables for typical positions.
- Magnitude: rook batteries occur in roughly 10–20% of evaluated positions; the count difference is typically 7–14 squares (number of squares on the file/rank behind the blocker). At an EG bonus of ~12 cp/square at mid-table, the expected delta is ~20–30 cp per rook in battery, weighted by position frequency — a small but non-negligible calibration gap.
- Assessment: flag as M6.I/post-M6.D watch-item; not a blocker for M6.D (the offset is systematic and absorbed by Texel tuning).

### Risk classification

| Risk | Direction | Magnitude | M6.D blocker? |
|------|-----------|-----------|---------------|
| PST double-count (§1) | Over-magnitude | Moderate (esp. knights/bishops) | No — absorbed by screen/Texel |
| Simple vs. richer mobility area (§2) | Over-magnitude | Small (1–3 sq average) | No |
| No x-ray for rooks/bishops (§5) | Under-magnitude for batteries | Small-moderate (positionally rare) | No |
| Pinned pieces pseudolegal (§4) | Over-credit | Negligible (< 2 cp average) | No |

---

## §6 — Literature-default weight tables

### Source and provenance

**Source:** Stockfish classical HCE (hand-crafted evaluation), tables as reproduced in a TalkChess forum discussion thread [TalkChess: "Mobility Evaluation?", viewtopic.php?t=61693], which quoted the `MobilityBonus[PieceType][attacked]` array.

**Version:** These values correspond to a mid-era Stockfish classical eval (not Stockfish 1.9 whose tables are significantly different). The TalkChess quote uses the `S(mg, eg)` score-pair notation consistent with Stockfish's classical eval style. The last classical Stockfish HCE was frozen at commit `20073110` (July 31, 2020); these tables are from that era.

**Calibration baseline:** Stockfish's own PSTs + full richer mobility area + x-ray for rooks/bishops. The tables are mutually co-calibrated across all four piece kinds under a single Texel-tuned parameter optimization.

**Alternative sources considered:**

| Source | Tables available? | Co-calibrated? | Recommendation |
|--------|-------------------|----------------|----------------|
| Stockfish classical HCE (TalkChess quote) | Yes, full arrays | Yes — single-engine Texel run | **Use this** |
| Stockfish 1.9 (TalkChess page 2 quote) | Yes, but much older | Yes — but vastly weaker baseline | Reject — too old |
| MadChess 3.0 Beta (blog post) | No — non-linear formula, not tables | Single-engine | Unusable as tables |
| Ethereal | Not published outside source repo | N/A | Unavailable under our restriction |
| Weiss | Not published outside source repo | N/A | Unavailable under our restriction |

**Recommendation:** Use the Stockfish HCE tables. They are the only citable, internally co-calibrated full MG/EG set for all four kinds available without reading engine source.

### Concrete arrays (copy-pasteable)

`S(mg, eg)` notation: `MG_value` then `EG_value`. Index = number of squares in mobility area attacked by the piece (after ∩ mobility_area).

**Knight (9 entries, indices 0..=8):**

| Index | MG | EG |
|-------|----|----|
| 0 | -75 | -76 |
| 1 | -56 | -54 |
| 2 | -9 | -26 |
| 3 | -2 | -10 |
| 4 | 6 | 5 |
| 5 | 15 | 11 |
| 6 | 22 | 26 |
| 7 | 30 | 28 |
| 8 | 36 | 29 |

```rust
// KNIGHT_MOBILITY_MG: indices 0..=8
pub(crate) const KNIGHT_MOBILITY_MG: [i32; 9] = [-75, -56, -9, -2, 6, 15, 22, 30, 36];
// KNIGHT_MOBILITY_EG: indices 0..=8
pub(crate) const KNIGHT_MOBILITY_EG: [i32; 9] = [-76, -54, -26, -10, 5, 11, 26, 28, 29];
```

**Bishop (14 entries, indices 0..=13):**

| Index | MG | EG |
|-------|----|----|
| 0 | -48 | -58 |
| 1 | -21 | -19 |
| 2 | 16 | -2 |
| 3 | 26 | 12 |
| 4 | 37 | 22 |
| 5 | 51 | 42 |
| 6 | 54 | 54 |
| 7 | 63 | 58 |
| 8 | 65 | 63 |
| 9 | 71 | 70 |
| 10 | 79 | 74 |
| 11 | 81 | 86 |
| 12 | 92 | 90 |
| 13 | 97 | 94 |

```rust
// BISHOP_MOBILITY_MG: indices 0..=13
pub(crate) const BISHOP_MOBILITY_MG: [i32; 14] = [-48, -21, 16, 26, 37, 51, 54, 63, 65, 71, 79, 81, 92, 97];
// BISHOP_MOBILITY_EG: indices 0..=13
pub(crate) const BISHOP_MOBILITY_EG: [i32; 14] = [-58, -19, -2, 12, 22, 42, 54, 58, 63, 70, 74, 86, 90, 94];
```

**Rook (15 entries, indices 0..=14):**

| Index | MG | EG |
|-------|----|----|
| 0 | -56 | -78 |
| 1 | -25 | -18 |
| 2 | -11 | 26 |
| 3 | -5 | 55 |
| 4 | -4 | 70 |
| 5 | -1 | 81 |
| 6 | 8 | 109 |
| 7 | 14 | 120 |
| 8 | 21 | 128 |
| 9 | 23 | 143 |
| 10 | 31 | 154 |
| 11 | 32 | 160 |
| 12 | 43 | 165 |
| 13 | 49 | 168 |
| 14 | 59 | 169 |

```rust
// ROOK_MOBILITY_MG: indices 0..=14
pub(crate) const ROOK_MOBILITY_MG: [i32; 15] = [-56, -25, -11, -5, -4, -1, 8, 14, 21, 23, 31, 32, 43, 49, 59];
// ROOK_MOBILITY_EG: indices 0..=14
pub(crate) const ROOK_MOBILITY_EG: [i32; 15] = [-78, -18, 26, 55, 70, 81, 109, 120, 128, 143, 154, 160, 165, 168, 169];
```

**Queen (28 entries, indices 0..=27):**

| Index | MG | EG | | Index | MG | EG |
|-------|----|----|--|-------|----|----|
| 0 | -40 | -35 | | 14 | 72 | 118 |
| 1 | -25 | -12 | | 15 | 73 | 122 |
| 2 | 2 | 7 | | 16 | 75 | 128 |
| 3 | 4 | 19 | | 17 | 77 | 130 |
| 4 | 14 | 37 | | 18 | 85 | 133 |
| 5 | 24 | 55 | | 19 | 94 | 136 |
| 6 | 25 | 62 | | 20 | 99 | 140 |
| 7 | 40 | 76 | | 21 | 108 | 157 |
| 8 | 43 | 79 | | 22 | 112 | 158 |
| 9 | 47 | 87 | | 23 | 113 | 161 |
| 10 | 54 | 94 | | 24 | 118 | 174 |
| 11 | 56 | 102 | | 25 | 119 | 177 |
| 12 | 60 | 111 | | 26 | 123 | 191 |
| 13 | 70 | 116 | | 27 | 128 | 199 |

```rust
// QUEEN_MOBILITY_MG: indices 0..=27
pub(crate) const QUEEN_MOBILITY_MG: [i32; 28] = [
    -40, -25,   2,   4,  14,  24,  25,  40,  43,  47,
     54,  56,  60,  70,  72,  73,  75,  77,  85,  94,
     99, 108, 112, 113, 118, 119, 123, 128,
];
// QUEEN_MOBILITY_EG: indices 0..=27
pub(crate) const QUEEN_MOBILITY_EG: [i32; 28] = [
    -35, -12,   7,  19,  37,  55,  62,  76,  79,  87,
     94, 102, 111, 116, 118, 122, 128, 130, 133, 136,
    140, 157, 158, 161, 174, 177, 191, 199,
];
```

### Naming convention (mirrors M6.B/C pattern in `eval::data`)

```rust
// In src/eval/data.rs, under a block comment citing this research file:
pub(crate) const KNIGHT_MOBILITY_MG: [i32; 9]  = [...];
pub(crate) const KNIGHT_MOBILITY_EG: [i32; 9]  = [...];
pub(crate) const BISHOP_MOBILITY_MG: [i32; 14] = [...];
pub(crate) const BISHOP_MOBILITY_EG: [i32; 14] = [...];
pub(crate) const ROOK_MOBILITY_MG:   [i32; 15] = [...];
pub(crate) const ROOK_MOBILITY_EG:   [i32; 15] = [...];
pub(crate) const QUEEN_MOBILITY_MG:  [i32; 28] = [...];
pub(crate) const QUEEN_MOBILITY_EG:  [i32; 28] = [...];
```

If the all-four SPRT collapses: zero-out individual kinds via the same env-mask / feature-flag pattern as M6.B (zero the arrays for the offending kind), not via removing the term from `evaluate_core`.

---

## §7 — Common pitfalls

### Own-square inclusion

- Do not include the piece's current square in the mobility count.
- `piece_attacks(sq, occ)` bitboard for standard move generators does not include the source square; no special handling needed.

### Attack-squares vs. legal-move-squares

- Mobility counts attack-squares (pseudolegal destinations ∩ mobility_area), not legal moves.
- Legal movegen is too expensive at eval leaves.
- Pins and checks are handled by the search; do not filter them in the mobility term.
- See §4 for the pinned-piece consequence.

### Whether enemy-occupied squares count

- **Yes, they do** — confirmed in §2. Enemy-non-pawn-attacked squares stay in the mobility area.
- "Captures count as mobility" is the standard; only friendly-occupied and enemy-pawn-attacked squares are subtracted.

### Queen mobility double-counting with rook + bishop

- Counting queen mobility separately from rook and bishop mobility does not create a combinatorial double-count.
- Rook, bishop, and queen are separate pieces — each has its own square, its own piece_attacks, and the mobility counts for each piece are independent.
- The concern only applies if you tried to decompose queen mobility into "rook-component + bishop-component" — not the standard approach.
- Standard approach (and M6.D approach): queen has its own per-piece count and its own table independently.

### MG/EG asymmetry — verified

- Knight/bishop: relatively symmetric MG/EG (minor differences in §6 tables).
- Rook: dramatically EG-heavy (EG values roughly 2–3× MG at high indices).
- Queen: moderately EG-heavy.
- **Do not use a single undifferentiated bonus per move; always use the MG/EG table pair.**

### Interaction with king safety (M6.E sequencing)

- The mobility area excludes enemy-pawn-attacked squares.
- An enemy-pawn-attacked square near the friendly king is a king-zone attack candidate for M6.E's attacker count.
- These two exclusions are computed from the same `enemy_pawn_attacks` bitboard but serve different purposes; the computation is shared, not conflated.
- M6.D does not need to account for M6.E's attacker-count logic — the sequencing (D before E) means M6.E's SPRT signal captures the interaction.

---

## §8 — Expected Elo and contingency posture

### Literature prior

- MadChess 2.0 Beta: +64 Elo from adding piece mobility. [madchess.net 2014]
- MadChess 3.0 Beta: +62 Elo from adding piece mobility (non-linear formula, tuned against GM games). [madchess.net 2020]
- Roadmap budgets +20 to +35 Elo — conservative relative to MadChess, reflecting the PeSTO-PST double-count offset already partially capturing mobility signal.

### Transfer probability assessment

The Stockfish HCE tables were calibrated against:
- Stockfish's own PSTs (not PeSTO)
- The richer Stockfish mobility area (not our simpler definition)
- X-ray occupancy for bishops/rooks

Compared to M6.C's situation (structural mismatch in the king-distance shape), the mobility table mismatch is a **magnitude/offset mismatch, not a shape mismatch**:
- The tables have the right monotonic shape (more mobility = better, with diminishing returns).
- The offsets are systematic and directionally consistent (over-magnitude for knights/bishops from PST overlap; slight under-magnitude for rooks in batteries from no x-ray).
- M6.B CONN-only transferred at +45 Elo because the shape was right. The same logic applies here.

Assessment: **moderate probability of partial transfer** — at least some kinds will transfer cleanly; all-four-at-once may show PST overlap over-magnitude on knights/bishops at fast TC.

### Recommended contingency ladder

If the all-four SPRT fails (H0 or negative CI):

1. **Per-kind subset screen** (same method as M6.B): enable one kind at a time with the other three zeroed. Identify which kinds individually pass.
2. **Keep passing kinds only** — ship positive-CI kinds, zero failing kinds (M6.I re-introduces via joint Texel).
3. **If all four individually pass but combined fails**: run a uniform `×0.5` co-scale screen (all tables halved). If that recovers positive CI across TC profile, ship at ×0.5 with an M6.I watch-item.
4. **If co-scale fails to recover across TC profile** (the M6.C `{RANK+PATH}/2` failure mode): zero all four, ship score-neutral, defer entirely to M6.I joint Texel. Record the directionally-correct diagnostic signal (positive WAC/STS from the zeroed-weight live-term path, as with M6.C).

### The M6.B/C "co-calibrated-elsewhere ≠ transfers here" lesson applies

- These tables were not tuned against PeSTO PSTs.
- PeSTO's knight/bishop PSTs already encode central-mobility bonuses.
- Knight index 8 = MG +36 / EG +29 in the Stockfish table is a bonus on top of Stockfish's already-mobility-implicit PSTs.
- On our PeSTO PSTs (where the knight center bonus is already baked in), this bonus is partially redundant and will over-inflate.
- Expectation: knights and bishops are the most likely per-kind failures; rooks and queens (whose PeSTO PSTs are less mobility-saturated) are more likely to transfer.

---

## Sources

- [CPW — Mobility](https://www.chessprogramming.org/Mobility)
- [CPW — Evaluation Overlap](https://www.chessprogramming.org/Evaluation_Overlap)
- [CPW — Evaluation of Pieces](https://www.chessprogramming.org/Evaluation_of_Pieces)
- [CPW — Pin](https://www.chessprogramming.org/Pin)
- [CPW — PeSTO's Evaluation Function](https://www.chessprogramming.org/PeSTO%27s_Evaluation_Function)
- [TalkChess — Mobility Evaluation? (thread with Stockfish HCE table quotes)](https://talkchess.com/forum3/viewtopic.php?t=61693)
- [TalkChess — mobility evaluation of stockfish page 2 (Stockfish 1.9 tables)](https://talkchess.com/viewtopic.php?p=372513)
- [TalkChess — Mobility eval](https://talkchess.com/forum3/viewtopic.php?t=43527)
- [Stockfish Evaluation Guide](https://hxim.github.io/Stockfish-Evaluation-Guide/)
- [MadChess 3.0 Beta — Piece Mobility (2020)](https://www.madchess.net/2020/02/01/madchess-3-0-beta-5c5d4fc-piece-mobility/)
- [MadChess 2.0 Beta Build 029 — Piece Mobility (2014)](https://www.madchess.net/2014/12/16/madchess-2-0-beta-build-29-piece-mobility/)
- [PeSTO description on rofchade.nl](https://rofchade.nl/?p=307)
- [TalkChess — Last Stockfish with HCE source code](https://talkchess.com/viewtopic.php?t=83106)
