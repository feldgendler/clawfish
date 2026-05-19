# M6.E — King Safety Evaluation (Research)

**Feeds:** `docs/plans/m6.e.md` (to be written).
**Committed semantic (roadmap M6.E row):** king-zone (3×3 + forward-wedge) + attacker-weight SafetyTable-style S-curve + pawn-shield + semi/open-file-toward-king; MG-heavy taper; reads M6.B's pawn-hash `pawn_shield_files`.

---

## Headline recommendations

- **King-zone shape: 3×3 around king + three squares one rank further toward the enemy (forward wedge).** This is the Stockfish-lineage definition and the CPW canonical reference. Adds three squares the attacker actually wants to infiltrate; does not bloat the zone to 16+ squares as outer-ring schemes do.
- **Attacker-weight scheme: per-kind multiplier × king-zone-attacked squares, summed to an index, looked up in a SafetyTable S-curve.** CPW's Fruit-lineage approach. The SafetyTable from CPW-Engine (reproduced in §6) is the one citable non-source-repo numeric set; it matches the Glaurung/Stockfish generation parameters and is the default recommended here.
- **Pawn-shield: check only the three files of the castled king (king-file ± 1); two rank tiers (rank 2 bonus SHIELD_1 > rank 3 bonus SHIELD_2); no bonus otherwise.** Cacheable in pawn hash by mask; king-position gates the lookup at eval time (outside the cache). Open/semi-open files carry a separate MG-only penalty.
- **Transfer-risk verdict: HIGH.** Literature-default king safety weights are the single most likely transfer failure among all M6 phases, on top of the three-phase "co-calibrated-elsewhere ≠ transfers here" law. See §8 for the full decomposition.
- **Recommended landing strategy: ship score-neutral (all weights → M6.F), same pattern as M6.C and M6.D.** Rationale in §8. The infrastructure (zone bitboard, attacker counting, pawn-shield mask) ships live at zero weight; M6.F joint Texel derives live weights. This avoids M6.D's costly per-component screen-ladder campaign for a term that is expected to need reshaping.

---

## §1 — King-zone definition

### Variants surveyed

| Variant | Squares included | Square count (typical) | Used by |
|---------|-----------------|----------------------|---------|
| **3×3 king ring** | 8 squares adjacent to king (no forward extension) | 8 | Simpler engines; Alessandro's approach (TalkChess t=82407) |
| **3×3 + forward wedge** | 8 adjacent + 3 squares one rank further toward enemy | 11 | Stockfish HCE, Glaurung, CPW-Engine, CPW-King |
| **Concentric dual-ring** | Inner 8 (adjacent) + outer 16 (two-square radius) | 8/16 | MadChess 3.0 |
| **File-based zone** | All squares on king file and ± 1 file | Varies by rank | TSCP; early engines |

### Canonical definition

**Stockfish / Glaurung / CPW (recommended):**
> "Squares to which the enemy King can move + three more forward squares facing enemy position."
> — [CPW King Safety](https://www.chessprogramming.org/King_Safety)

For White's king: the king ring (8 adjacent squares) + the three squares directly north-east, north, and north-west of the ring's north edge (i.e. king rank + 2 for the three center-facing squares). For Black, mirror vertically.

Pseudocode:
```
king_ring = king_attacks(king_sq)                    // 8 squares
forward_3 = shift_toward_enemy(king_ring, 3_files)   // 3 squares from the north edge
king_zone = king_ring | forward_3
```

### Edge-file handling

When the king is on the a-file or h-file, the zone shifts inward rather than extending off the board:
- King on a1 → zone uses b1, a2, b2, a3, b3 (no c-file extension exists).
- King on h1 → zone uses g1, h2, g2, h3, g3 (no beyond-h extension).
- Standard bitboard king-attacks implementation handles this automatically via the same edge-clamping as normal king-move generation. No special-case code needed.

### Back-rank handling

For a king on the back rank (rank 1 for White):
- The 3×3 ring still includes the two ranks above the king.
- The forward wedge's three extra squares land on rank 3 (one rank above the ring top).
- There are no squares "behind" the king on rank 0 (off board) — bitboard king_attacks handles this.
- The zone is smaller for a back-rank king than for a centralized king; this is correct behavior (fewer approaches are possible).

### Recommendation

Use the **3×3 ring + forward wedge** (11 squares maximum, fewer on edge/corner). Rationale:
- It is the Stockfish/Glaurung definition and the CPW-Engine reference.
- The three forward squares are the squares an attacker most wants to reach before delivering check; they represent real attack threat squares, not just theoretical adjacency.
- Implementing it as `king_attacks(sq) | king_attacks(forward_square_set)` requires one extra king-attacks lookup but avoids bespoke table construction.
- The dual-ring (inner/outer) MadChess approach requires maintaining two separate weighted counters; not justified at this tier.

---

## §2 — Attacker-weight scheme

### The SafetyTable (S-curve) approach

**Structure:** for each enemy piece, count how many king-zone squares it attacks. Multiply by a per-kind weight. Sum across all enemy pieces into a single `attack_units` integer. Look up `SafetyTable[attack_units]` for the penalty.

**Fruit-lineage per-kind weights (from CPW King Safety page):**

| Piece kind | Weight per attacked king-zone square |
|------------|--------------------------------------|
| Knight | 20 |
| Bishop | 20 |
| Rook | 40 |
| Queen | 80 |

Source: [CPW King Safety — Fruit scheme](https://www.chessprogramming.org/King_Safety).

**CPW-Engine attacker-multiplier variant (from CPW-Engine eval page):**

| Piece kind | Multiplier applied to `att` (count of king-zone squares attacked by this piece) |
|------------|-----------------|
| Knight | 2 × att |
| Bishop | 2 × att |
| Rook | 3 × att |
| Queen | 4 × att |

Source: [CPW-Engine eval](https://www.chessprogramming.org/CPW-Engine_eval). This is a compressed form of the Fruit weights, scaling by the number of attacked squares rather than per-square multiplied by kind weight.

**Stockfish HCE attack-unit weights (cited on CPW King Safety page):**

| Piece kind | Attack units per piece that attacks the zone |
|------------|---------------------------------------------|
| Minor piece (N or B) | 2 |
| Rook | 3 |
| Queen | 5 |
| Safe queen contact check | 6 (additional) |
| Safe rook contact check | ~2 (additional) |

Source: [CPW King Safety — Stockfish section](https://www.chessprogramming.org/King_Safety).

Note: Stockfish's scheme is "attack units per attacking *piece*" (attacker-count model), not "per attacked *square*" (attack-square model). The Fruit scheme multiplies by number of attacked squares (more granular). Both are valid; the CPW-Engine uses the simpler per-piece-count version.

### The SafetyTable itself

**Full 100-entry CPW-Engine SafetyTable (directly from CPW-Engine eval source page):**

```rust
static SAFETY_TABLE: [i32; 100] = [
      0,   0,   1,   2,   3,   5,   7,   9,  12,  15,
     18,  22,  26,  30,  35,  39,  44,  50,  56,  62,
     68,  75,  82,  85,  89,  97, 105, 113, 122, 131,
    140, 150, 169, 180, 191, 202, 213, 225, 237, 248,
    260, 272, 283, 295, 307, 319, 330, 342, 354, 366,
    377, 389, 401, 412, 424, 436, 448, 459, 471, 483,
    494, 500, 500, 500, 500, 500, 500, 500, 500, 500,
    500, 500, 500, 500, 500, 500, 500, 500, 500, 500,
    500, 500, 500, 500, 500, 500, 500, 500, 500, 500,
    500, 500, 500, 500, 500, 500, 500, 500, 500, 500,
];
```

Source: [CPW-Engine eval](https://www.chessprogramming.org/CPW-Engine_eval). This is also the Glaurung 1.2 lineage table (same values) and the Fruit lineage table reproduced on CPW's King Safety page. It is the one complete, citable numeric set available outside engine source repos.

**Shape:** S-curve — slow rise at low indices (0–10), steeper rise from index 10–60, flat at 500 centipawns from index 61 onward.

**Stockfish formula (for comparison):** Generates a similar curve via `t = min(Peak, min(0.4·i², t + MaxSlope))` with `MaxSlope=30, Peak=1280`, then rescaled to centipawns. The resulting values are very similar in shape but slightly different in magnitude — also saturates near 500 cp. This is described on CPW but the exact Stockfish constant values are described only in terms of the generation formula [CPW King Safety].

### MadChess 3.0 attacker weights (tuned parameters, from blog post)

Concrete values from `showevalparams` output, scaled per the engine's representation:
- Minor piece outer ring: 8 (per 8)
- Minor piece inner ring: 21 (per 8)
- Rook outer ring: 7 (per 8)
- Rook inner ring: 18 (per 8)
- Queen outer ring: 14 (per 8)
- Queen inner ring: 33 (per 8)

Source: [MadChess 3.0 Beta King Safety blog post](https://www.madchess.net/2020/08/16/madchess-3-0-beta-6794c89-king-safety/).

Note: these are *post-Texel-tuned* values for an engine with its own eval baseline and material scale. They are not directly transferable but illustrate the relative ordering (queen > rook > minor; inner ring > outer ring).

### Attacker-count gating (minimum 2 attackers rule)

**Standard gating condition (CPW, multiple engine implementations):**
> "It is advisable not to evaluate king attack if only two pieces are attacking" [CPW King Safety].

CPW-Engine implements this as:
```c
if (v.attCnt[WHITE] < 2 || b.PieceCount[WHITE][QUEEN] == 0) v.attWeight[WHITE] = 0;
```

This zeroes the attack weight when either fewer than 2 pieces attack the king zone OR the attacking side has no queen. Two justifications:
1. A single attacking piece rarely creates mating threats alone; the metric is noisy at low attacker counts.
2. Queen-less attacks are much less dangerous; excluding them avoids false positives.

Source: [CPW-Engine eval](https://www.chessprogramming.org/CPW-Engine_eval).

The attacker-weight accumulation pattern (from CPW-King and CPW-Engine):
```
// attCnt[side] = number of distinct pieces attacking king zone
// attWeight[side] = accumulated weighted attacks (sum of multiplier × attacked_squares per piece)
scaleAttacks():
  0–1 attackers → attack_weight = 0
  2 attackers → base value (unchanged)
  3 attackers → base × 4/3
  4 attackers → base × 3/2
  5+ attackers → base × 2
```

Source: CPW-King page, `scaleAttacks()` description.

### Linear-sum alternative

Simpler: `penalty = sum(per_kind_weight × count_of_attacking_pieces_of_that_kind)`, no SafetyTable lookup.

| Dimension | Linear sum | SafetyTable S-curve |
|-----------|-----------|---------------------|
| Implementation complexity | Lower | Slightly higher (table lookup) |
| Captures escalation | Does not capture the "critical mass" dynamic (1+1=2; not 1+1=4) | Captures: two attackers is disproportionately more dangerous than two singles |
| Tuning surface | Simpler (per-kind weights only) | More surface (per-kind weights + table shape) |
| Literature consensus | Minority (simpler engines only) | Dominant (Fruit, Glaurung, CPW, Stockfish HCE) |

Recommendation: SafetyTable. The nonlinear escalation at 2–4 attackers is the most important qualitative property of king safety; a linear scheme does not capture it.

---

## §3 — Pawn shield and pawn storm

### Shield structure

**What is evaluated:** for the castled king, check whether friendly pawns remain on rank 2 (unmoved, strongest shield) and rank 3 (moved one square, weaker shield) on the king's own file and the two adjacent files.

**CPW-Engine shield implementation (from CPW-Engine eval page):**

```c
// White kingside (king file > e):
if (isPiece(WHITE, PAWN, F2)) result += e.SHIELD_1;
else if (isPiece(WHITE, PAWN, F3)) result += e.SHIELD_2;
// repeat for G2/G3, H2/H3

// White queenside (king file < d):
if (isPiece(WHITE, PAWN, A2)) result += e.SHIELD_1;
else if (isPiece(WHITE, PAWN, A3)) result += e.SHIELD_2;
// repeat for B2/B3, C2/C3
```

Key observations:
- Evaluated **only when the king has castled** — detected by `COL(KingLoc) > COL_E` (kingside) or `< COL_D` (queenside). No bonus for an uncastled central king.
- Each file checked independently: pawn on rank 2 gives SHIELD_1; else pawn on rank 3 gives SHIELD_2; else 0 for that file.
- SHIELD_1 and SHIELD_2 are constants in the `s_eval_data` struct; their exact values are not published outside the CPW-Engine source repo.

**Literature-center shield values (from surveyed sources):**

| Context | Source | Shield rank 2 bonus | Shield rank 3 bonus |
|---------|--------|---------------------|---------------------|
| Simple implementation | manuelfedele.github.io tutorial | +10 per pawn (undifferentiated) | +10 per pawn |
| MadChess 2.0 | madchess.net (2014) | Unlisted | Unlisted |
| General range (CPW + forums) | Various | +10 to +20 per pawn | +5 to +10 per pawn |

The SHIELD_1 > SHIELD_2 asymmetry (rank 2 > rank 3) is universal: an unmoved shield is strictly better than an advanced one.

**Missing-shield penalty:** the CPW-Engine scheme implicitly penalizes missing shield via the lack of the SHIELD_1/SHIELD_2 bonus (missed bonus = implicit penalty relative to the shield baseline). Some engines add an explicit `P_NO_SHIELD` penalty per empty file. Both formulations are equivalent in effect; the explicit penalty is more aggressive in score magnitude because it subtracts rather than just failing to add.

### Castled-king detection

Standard approach: `king_file ≥ g` (or `≤ b` for queenside) — i.e., king on g1/h1 for white kingside. This avoids evaluating pawn-shield for a king on d1/e1 (not yet castled or moved to center).

The ADR must pick a detection threshold. Options:
- `king_file ∈ {g, h}` (kingside) / `king_file ∈ {a, b}` (queenside) — strict; misses non-standard castling positions.
- `king_file ≥ f` (kingside) / `king_file ≤ c` (queenside) — wider; catches edge-file voluntary king moves.
- Pure "has castled" flag — exact but requires tracking a separate boolean in `Position`. Not recommended (extra state).

CPW-Engine uses `col > E` and `col < D` which maps to the wider variant (`king_file ≥ f`/`≤ c`). This is the recommended approach.

### Pawn storm

Enemy pawns advancing toward the castled king's pawn-shield files create a storm threat.

**CPW guidance:** "Penalties for storming enemy pawns must be lower than penalties for (semi)open files, otherwise the pawn storm might backfire, resulting in a blockage." [CPW King Safety]

TSCP implements pawn-storm scoring scaled by opponent material. Most engines at this tier skip explicit pawn-storm terms and rely on the pawn-structure terms (forward passed/connected pawn bonuses) plus the open-file penalty to indirectly penalize broken shields.

**Recommendation for M6.E: omit explicit pawn-storm term.** Rationale:
- Pawn-storm terms are positive only when enemy pawns are both advanced *and* attacking the shield — a narrow case.
- The term requires knowing enemy pawn positions relative to the friendly king, which is not pawn-hash-cacheable (king position invalidates).
- Three eval phases (passed-pawn, mobility, pawn-structure) already penalize an absence of pawn counter-play; the marginal value of a pawn-storm term at M6.E is low.
- CPW explicitly warns that a miscalibrated pawn-storm term backfires; this is exactly the kind of interaction risk that needs joint Texel calibration.

### Pawn-hash caching for shield

**What is pawn-hash-cacheable (keyed by pawn-only Zobrist, invariant to king position):**
- Shield-file mask bitboards: `shield_pawn_mask_kingside[color]`, `shield_pawn_mask_queenside[color]`.
- Whether each of the three shield files has a pawn on rank 2 or rank 3.
- Open/semi-open file presence on king-zone files (a function of pawn positions only — see §4).

**What is NOT pawn-hash-cacheable (invalidated by king moves):**
- The king's actual file (which shield files to check) — this gates which shield is relevant.
- The applied shield score (bonus only when king is castled and in the shield zone) — depends on king position.

**Standard practice (from CPW Pawn Hash Table):**
> "Pawn-king stuff [can be] cached separately which requires king squares included inside the index calculation." But the simpler approach is to "store pawn shield masks in the pawn hash entry, then apply them at eval time based on the king's actual position."

For this project (M6.B already reserves `pawn_shield_files` in the pawn-hash entry per the roadmap): store the three-file shield bitboard mask per side in the pawn hash. At eval time (outside the cache), check which mask to apply based on the current king file (kingside vs queenside). Apply SHIELD_1/SHIELD_2 bonus based on pawn occupancy of the cached mask.

This correctly separates the pawn-only part (which files have shield pawns) from the king-position-dependent part (which set of files to consult).

---

## §4 — Open and semi-open files toward the king

### Definition

- **Open file:** no pawn of either color on the file.
- **Semi-open file:** no friendly pawn on the file (but possibly an enemy pawn).
- **King-adjacent files:** king file, king file − 1, king file + 1.

**Penalty structure (CPW King Safety + MadChess 3.0):**

| Condition | Penalty type |
|-----------|-------------|
| King file is semi-open | Moderate penalty |
| King file is open | Larger penalty (enemy rook/queen can occupy directly) |
| One adjacent file is semi-open | Smaller penalty |
| Both adjacent files semi-open | Amplified combined penalty |

The "both adjacent files" amplification is a known threshold effect: a king where all three forward files are open has no pawn cover at all — qualitatively worse than one missing file.

**MadChess 3.0 parameter:**
- `MgKingSafetySemiOpenFilePer8: 062` — semi-open file penalty is middlegame-only.

Source: [MadChess 3.0 King Safety](https://www.madchess.net/2020/08/16/madchess-3-0-beta-6794c89-king-safety/).

**Endgame policy:** This is universally treated as **MG-only**. Open files in the endgame are common and do not indicate king danger; the king walks in the endgame. MadChess explicitly: "only for the middlegame, assuming open files are common in the endgame and don't necessarily make a king's position unsafe."

### Pawn-hash caching

Open/semi-open file bitmasks are pure functions of pawn positions:
```
open_files = ~fileFill(all_pawns)
semi_open_files[white] = ~fileFill(white_pawns) & ~open_files   // has black pawn but no white pawn
```

These can be precomputed and stored in the pawn-hash entry (they are already computed for the pawn-structure terms). The king-zone file lookup at eval time is outside the cache.

---

## §5 — Tapering policy

### Survey of approaches

| Policy | Description | Adopted by |
|--------|-------------|-----------|
| **Full-tapered (MG/EG pair)** | King safety has both MG and EG weights; EG weights are near zero but nonzero | Representationally consistent with other terms; can Texel-tune EG weight |
| **Hard-off in EG** | The entire king safety term is zeroed when phase < threshold (e.g. below 8 of 24) | TSCP, CPW-Engine (phase-independent SafetyTable + material scaling) |
| **MG-only with material scaling** | The SafetyTable result is multiplied by `opponent_material / start_material`; naturally goes to zero | Classic Glaurung/TSCP approach |
| **MG weight > 0, EG weight = 0** | King safety contributes to MG score only; EG score contribution is zero | Stockfish HCE-era (attacker danger was primarily MG) |

### Why king safety tapers toward zero in the EG

- In the middlegame, the king needs protection behind a pawn shield; an exposed king is a target for long-range pieces.
- In the endgame, fewer pieces remain; checkmate threats dissipate. The king becomes an active participant.
- PeSTO's EG king PST reflects this: it rewards king centralization in the endgame, which is *opposite* to the MG hiding-in-a-corner posture.
- A large EG king-safety penalty would fight against the EG king PST's incentive to centralize, creating a tug-of-war.

### Recommendation for a strictly tapered engine

**Use the MG/EG pair representation with EG weights near zero.** Concretely:
- King-safety attacker S-curve: MG weight 1.0, EG weight near 0 (e.g., 0.0–0.1).
- Pawn-shield bonuses: MG weight significant, EG weight small or zero.
- Open-file penalties: MG-only (EG weight = 0), consistent with MadChess.

The representational advantage: the tapered eval framework (`(mg·phase + eg·(24 − phase)) / 24`) naturally handles the transition without a separate phase-threshold gate. Setting EG weights to zero is equivalent to "hard-off in EG" at the extreme, but the Texel tuner can discover small nonzero EG contributions if present.

**Alternative: hard-off via a phase threshold gate** (`if phase < 6 { return Score::ZERO; }`). This is simpler but adds a threshold hyperparameter and makes EG phase a hard discontinuity rather than a smooth blend. Not recommended for a tapered engine that already has smooth blending.

### Material-scaling approach (TSCP/CPW-Engine)

The CPW-King pattern:
```c
result *= p.PieceMaterial[opponent];
result /= e.START_MATERIAL;
```

This is an elegant substitute for EG tapering but is equivalent to a linear decay function, not the engine's phase-based tapered blend. For an engine already committed to `(mg·phase + eg·(24−phase))/24`, mixing in a separate material-scaling multiplier on king safety creates inconsistency. **Avoid material scaling**; express the decay through the EG weight being near zero.

---

## §6 — Concrete recommended parameter set

### King-zone shape

- 3×3 ring + forward wedge (11 squares; see §1).
- Implement as: `king_zone = king_attacks(king_sq) | king_attacks(shift_toward_enemy(king_attacks(king_sq), 1))`.
- Bitboard edge clamping is automatic via standard king-attacks.

### Per-kind attacker weights (to build attack_units)

Using the CPW-Engine/Fruit lineage (most citable complete set):

| Piece kind | Per-attacked-king-zone-square weight |
|------------|--------------------------------------|
| Knight | 20 |
| Bishop | 20 |
| Rook | 40 |
| Queen | 80 |

Source: [CPW King Safety — Fruit scheme](https://www.chessprogramming.org/King_Safety).

Alternative (CPW-Engine per-piece-multiplier form, simpler):

| Piece kind | Multiplier on `attacked_squares_count` |
|------------|---------------------------------------|
| Knight | 2 |
| Bishop | 2 |
| Rook | 3 |
| Queen | 4 |

Both are equivalent; the per-square form is slightly more granular. Recommend the CPW-Engine multiplier form for simplicity.

### SafetyTable

```rust
pub(crate) const SAFETY_TABLE: [i32; 100] = [
      0,   0,   1,   2,   3,   5,   7,   9,  12,  15,
     18,  22,  26,  30,  35,  39,  44,  50,  56,  62,
     68,  75,  82,  85,  89,  97, 105, 113, 122, 131,
    140, 150, 169, 180, 191, 202, 213, 225, 237, 248,
    260, 272, 283, 295, 307, 319, 330, 342, 354, 366,
    377, 389, 401, 412, 424, 436, 448, 459, 471, 483,
    494, 500, 500, 500, 500, 500, 500, 500, 500, 500,
    500, 500, 500, 500, 500, 500, 500, 500, 500, 500,
    500, 500, 500, 500, 500, 500, 500, 500, 500, 500,
    500, 500, 500, 500, 500, 500, 500, 500, 500, 500,
];
```

Source: [CPW-Engine eval](https://www.chessprogramming.org/CPW-Engine_eval) (Glaurung/Fruit lineage; exact values as reproduced on wiki).

The index into SAFETY_TABLE is the accumulated `attack_units` value (sum of `multiplier × attacked_squares` across all attacking pieces). Clamped to `min(units, 99)`.

**Gating condition:**
```rust
// Zero out attack_weight if fewer than 2 attackers or if attacker has no queen
if attacker_count < 2 || !attacking_side_has_queen {
    attack_units = 0;
}
let attack_penalty_mg = SAFETY_TABLE[attack_units.min(99) as usize];
// EG weight near 0 per tapering policy
let attack_penalty_eg = 0;  // or a small fraction — M6.F will tune this
```

### Pawn-shield values

- SHIELD_1 (rank 2 pawn): recommended starting value **+10 MG, +5 EG** (center of literature range; [manuelfedele.github.io](https://manuelfedele.github.io/posts/evaluate-chess-position/), CPW).
- SHIELD_2 (rank 3 pawn): recommended starting value **+5 MG, +3 EG** (half of SHIELD_1; penalized for having advanced).
- Applied per file (3 files × per-rank check); activated only when king is on castled-side files (f/g/h or a/b/c for White).

These values are the lowest-risk starting point (small magnitude, low double-count risk). M6.F Texel re-derives.

### Open/semi-open file penalties

- Semi-open king file: **−15 MG, 0 EG**
- Semi-open adjacent file (once): **−10 MG, 0 EG**
- Both adjacent files semi-open amplification: add an additional **−10 MG** (total −20 for both)
- Open file (no pawn of either color): **−20 MG, 0 EG**

All EG values are 0 (MG-only per §4 rationale). These magnitudes are conservative; MadChess uses ~7.75 cp per semi-open file file (`62/8`) as a tuned value — that is post-Texel for a specific engine's baseline. Starting smaller is safer for initial transfer.

### MG/EG split summary

| Component | MG | EG | Notes |
|-----------|----|----|-------|
| Attack S-curve (SAFETY_TABLE) | Full value | 0 | MG-only per tapering policy |
| Pawn shield SHIELD_1 (rank 2) | +10 | +5 | Small EG contribution ok |
| Pawn shield SHIELD_2 (rank 3) | +5 | +3 | |
| Semi-open king file | −15 | 0 | MG-only |
| Semi-open adjacent file | −10 | 0 | MG-only |
| Open king file | −20 | 0 | MG-only |
| Both adjacent semi-open bonus | −10 | 0 | Amplification |

---

## §7 — Independence / double-count analysis

### Interaction with PeSTO king PST

PeSTO's MG king PST penalizes central king placement and rewards castled position (g1/h1 for White get higher values than e1/d1 by ~30–50 cp). This is the PST encoding of king safety at the level of *square occupancy*.

The king-safety term adds a *dynamic* component: the current attacker count on the king zone. These two signals are partially overlapping:
- A king on g1 with intact pawn shield and no attackers: PST gives the "castled king" bonus; king-safety term gives zero penalty. **No double-count — they measure different things.**
- A king on g1 with a wrecked pawn shield and three pieces attacking: PST still gives the "castled king" bonus (square occupancy unchanged); king-safety term gives a large penalty. **The PST over-credits safety here** because the PST only sees the square, not the dynamic state.
- A king that voluntarily moves to d1: PST gives a lower bonus for the worse square; king-safety term increases the attacker count. **Some double-penalization** for the active king-walk move.

**Assessment:** The PST and king-safety-attack-S-curve measure distinct dimensions (static position quality vs. dynamic attacker accumulation). The overlap is small for the attack term. The pawn-shield interaction with the PST is more dangerous: the PST's "castled king" bonus already partially prices in the pawn-shield benefit for canonical castled positions (g1 with f2/g2/h2 intact). Adding a pawn-shield bonus on top could amplify the g1 bonus. This is the most likely double-count axis.

Expected direction: pawn-shield bonuses will over-inflate for canonical castled positions where the PST already rewards the square. This is analogous to M6.D's PST double-count finding for piece mobility.

### Interaction with M6.D mobility term

The M6.D mobility area excludes enemy-pawn-attacked squares. The king-zone includes some enemy-pawn-attacked squares (pawns can attack king-zone squares). The interaction is:

- Enemy piece attacks king-zone squares that are also excluded from its mobility area → the attacker's mobility count is lower (it doesn't count the enemy-pawn-defended square as a mobility square) but the king-safety attack count *is* credited for that piece attacking the zone.
- Result: the same enemy piece may have its mobility slightly undercounted (pawn-defended zone squares excluded from mobility area) and also credit the king-safety attack counter. These are additive from the defender's perspective (the attacker loses some mobility credit but the defender still suffers the attack count increment).

This interaction is a *genuine* separate contribution of king-safety on top of mobility. The M6.D research report noted: "M6.D does not need to account for M6.E's attacker-count logic — the sequencing (D before E) means M6.E's SPRT signal captures the interaction." That statement holds.

**Key point:** the mobility area exclusion and the king-zone attack credit are different transformations on the same underlying attack bitboard. They are not double-counting in the classical sense — they measure different properties of the same attack.

### Interaction with M6.B pawn structure (connected pawn term)

The CONN pawn-structure term already bonuses connected pawns, including the pawn shield positions (f2/g2/h2 as a phalanx/defended formation). Adding a separate pawn-shield bonus for the same f2/g2/h2 pawns creates an overlap.

This is structurally identical to the ISO × CONN double-count discovered in M6.B, where two terms scored the same connectivity axis with different shape. Here: **CONN already partially rewards a healthy pawn shield (connected pawns on the castled side); pawn-shield adds a square-specific bonus for the same pawns.**

The overlap magnitude:
- Three shield pawns in a phalanx formation on the 2nd rank can receive CONN bonus (rank 2 connected = small bonus at CONN_MG[2] = 3, CONN_EG[2] = 5 per pawn per the M6.B shipped constants).
- Adding SHIELD_1 = +10 per pawn on top → ~+13 MG bonus for the same position.
- This is mild because CONN's rank-2 bonus is small (3 cp) and the shield check is more specific (only when castled), so the overlap is bounded.

**Assessment:** Low-severity overlap with CONN. The pawn-shield term is genuinely more specific (file+rank+castled gating) than CONN (rank-only connectivity). They measure different aspects of the same pawn cluster. However, when M6.F re-calibrates CONN and the pawn-shield simultaneously, the CONN rank-2/3 values and the shield bonuses will absorb into a joint solution that decorrelates them.

### Interaction with M6.C passed-pawn term

Passed pawns and pawn shields rarely overlap (a passed pawn is by definition advancing, not a stable 2nd-rank shield pawn). Interaction is negligible.

### Interaction with M6.D mobility (again, for attacker counting)

The attacker count for king safety reads the same per-piece attack bitboards as M6.D's mobility count. They use the same inputs but for different purposes:
- Mobility: counts squares the piece can move to (in its mobility area).
- King-safety: counts squares in the enemy king zone that the piece attacks.

There is no double-count between these two terms — they have different output registers (mobility score vs. attack_units). A piece that attacks many king-zone squares contributes to both a high mobility count and a high king-safety attack count. This is correct: a highly mobile attacker near the king is both a strategically active piece (mobility reward) AND a concrete threat (king safety penalty to the defender). The two terms measure complementary aspects of piece activity.

### Summary of double-count risk rankings

| Overlap | Severity | Affected term | Direction |
|---------|----------|---------------|-----------|
| PeSTO king MG PST × attack S-curve | Low-moderate | Attack S-curve over-magnitudes when king is in canonical castled position | Over-magnitude for S-curve |
| PeSTO king MG PST × pawn-shield bonus | Moderate | Pawn-shield bonuses already partially priced by PST | Over-magnitude for shield |
| CONN × pawn-shield bonus | Low | Mild redundancy on castled 2nd-rank pawn clusters | Over-magnitude for shield |
| Mobility × attack S-curve | Negligible | Different transformations on the same bitboard; no shared output | N/A |
| Pawn-open-file × open-file penalty | None | These measure the same thing but are identical by construction | By design |

**The dominant overlap risk is pawn-shield bonuses + PeSTO king PST**, analogous to the M6.D PST-double-count finding. The attack S-curve overlaps less because it responds to the dynamic attacker count, not the static square.

---

## §8 — Transfer-risk verdict and recommended landing strategy

### Three-phase "co-calibrated-elsewhere ≠ transfers here" law: does it apply?

| Phase | Term | Transfer result |
|-------|------|----------------|
| M6.B | ISO+CONN | Catastrophic −197.94 Elo when combined; shape mismatch |
| M6.C | passed-pawn rank/path | −21.74 all; co-scale failed; defer |
| M6.D | mobility (all 4 kinds) | −131.62 all; co-scale *worsened* (−220.18); defer |

The law has strengthened with each phase. M6.D's co-scale inversion is the most alarming: not just "doesn't help" but actively makes it worse when magnitude is halved. This indicates shape mismatch (wrong relationship between eval terms), not just magnitude mismatch.

### King safety's additional risk factors

King safety compounds the standard calibration mismatch risk with additional specific problems:

1. **The SafetyTable S-curve shape was calibrated against engines with their own PSTs and pawn-structure weights.** The saturation value of 500 cp at index 61+ is appropriate when pawn-structure and mobility are jointly tuned at specific magnitudes. Our zeroed mobility weights (M6.D) and partially-zeroed pawn-structure weights (M6.B) mean the king-safety term is operating against a baseline that is *not* the baseline the SafetyTable was calibrated for.

2. **PeSTO's king MG PST already encodes a large fraction of king safety.** The +30 to +50 cp premium for a castled king position (g1 vs e1) in PeSTO's MG table is precisely the "king safety proxy" that PeSTO's Texel-tuning baked in as a substitute for an explicit king-safety term. Adding the literature-default king-safety term on top doubles-down on that signal. This is the strongest argument against direct transfer.

3. **King safety is the noisiest term to SPRT.** TalkChess discussions and mACE Chess tuning post both confirm that king safety "needs to be tuned for max elo" but "after tuning, the bonus given was much smaller than examples shown in the wiki." [TalkChess t=82407, mACE Chess tuning blog]. Opening pool selection dominates variance: kingside-attacking opening books amplify king-safety Elo signal; quiet positional books dilute it to near-zero. A false-positive SPRT at one TC / opening pool can be reversed at another. The M6.D screen-ladder campaign ran per-kind screens at single-TC; a king-safety per-component screen at mixed-TC is far noisier and more expensive.

4. **The co-scale-worsens fingerprint from M6.D (slider EG magnitude) would likely recur.** If the SafetyTable's peak value of 500 cp is miscalibrated against our PeSTO + zeroed-mobility baseline, a uniform ×0.5 co-scale might not rescue it — the M6.D lesson.

### Per-component independence assessment

Unlike M6.D where N/B/R/Q were assessed as potentially independent, king safety's components are **coupled by design**:

| Component pair | Independence | Coupling mechanism |
|----------------|-------------|-------------------|
| Attack S-curve + pawn-shield | Coupled | Shield weakness directly feeds the attack units (an open file contributes attack-units in Stockfish's formula: `InitKingDanger - pawn_shield/32`) |
| Attack S-curve + open-file penalty | Coupled | Same attacker-facing threat; both penalize the same king-danger state |
| Pawn-shield + open-file | Partially independent | Shield is a bonus for pawns present; open-file is a separate penalty for pawn absence. Orthogonal predicates but same semantic |
| Attack S-curve alone | Somewhat isolated | Responds to opponent pieces, not own pawns; can be zeroed independently |

The coupling means the M6.B CONN-only "interaction-immune subset" finding does not have an obvious analogue here. There is no clearly isolated component that can be shipped positive without the others. The attack S-curve alone (without pawn-shield feeding it) is the least coupled component.

### Recommended landing strategy

**Recommendation: ship score-neutral (all weights zeroed), same pattern as M6.C and M6.D.**

Rationale:

1. **Three precedents say literature defaults fail.** M6.C and M6.D both deployed the full contingency ladder (per-component screen, co-scale probe) and found no positive interaction-immune subset. The cost was a multi-campaign SPRT exercise that discovered the answer "defer to M6.F." Starting at score-neutral skips that cost.

2. **King safety is the noisiest term.** The per-component screen ladder would need to run at mixed-TC (not single-TC) because king safety signals are TC-dependent. Mixed-TC screens are ~5× as expensive as single-TC. The M6.D screen ran single-TC at 10+0.1; applying the same approach here would be cheaper but less diagnostic for a TC-sensitive term.

3. **M6.F joint Texel is the right vehicle.** King safety, pawn shield, and mobility-area weights are all coupled through the same attack-bitboard infrastructure. The joint Texel pass can optimize them simultaneously against the same PeSTO-PST baseline. Shipping any subset live before that pass means the pass must work around a partially-baked starting point.

4. **Infrastructure ships live regardless.** Zone computation, attacker counting, SafetyTable lookup, pawn-shield bitboard population, open-file detection — all of these are code that M6.F's Texel pass reads directly. Zeroing weights does not remove the code; it sets the multipliers to zero. This is identical to M6.C/M6.D's `score-neutral from the start` pattern.

### If the plan changes to "attempt live weights" (contingency ladder)

If the orchestrator decides to attempt a live-weight SPRT despite the above recommendation (a valid choice — "start score-neutral by default, SPRT as a secondary confirm" is not the only defensible path), here is the screen-ladder decomposition:

| Stage | Screen configuration | Independence | Expected outcome |
|-------|---------------------|-------------|-----------------|
| 1 | Open-file penalties only (shield + S-curve zeroed) | High — pure pawn-structure proxy, no opponent-piece dependency | Most likely to transfer; small positive expected |
| 2 | Pawn-shield only (S-curve + open-file zeroed) | Moderate — coupled with CONN overlap (§7) | Positive possible but PST double-count risk |
| 3 | S-curve only (shield + open-file zeroed) | Lower — S-curve calibrated assuming shield is present | Likely over-magnitude; co-scale probe needed |
| 4 | Open-file + pawn-shield combined | Moderate — both MG-only and structurally simpler than S-curve | Positive plausible |
| 5 | All three combined | Low — full co-calibration dependency | Expected to fail as M6.D did |
| 6 | All three × 0.5 co-scale | Last resort — co-scale-inversion risk from M6.D | May worsen |

Note: the open-file penalties (Stage 1) are the most likely positive component because:
- They have the smallest PST overlap (the king MG PST rewards *position*, not file openness).
- They are MG-only (no slider-EG-magnitude problem from M6.D).
- They are structurally independent of the S-curve calibration assumptions.

### SPRT noise management

**If live weights are attempted**, the opening pool selection is load-bearing for king-safety SPRT signal:
- Use a pool with varied pawn-structure positions (not exclusively kingside-attacking).
- The 4-bucket mixed-TC (10+0.1 / 20+0.2 / 40+0.4 / 60+0.6) is required; fast-TC only will produce a noisy single-point estimate.
- Expect wider CI than mobility — accept a "verdict=continue with positive CI lower bound > 0" as a pass criterion (the M6.B outcome-2 precedent).

---

## §9 — Open questions and flagged ambiguities

1. **The exact SHIELD_1 and SHIELD_2 values in CPW-Engine** are referenced in source code but not published numerically outside the source repo. The recommended starting values (+10/+5 for SHIELD_1/SHIELD_2) are center-of-literature estimates, not CPW-Engine exact values. M6.F Texel will supersede them.

2. **Whether the SafetyTable should be applied as a full 500-cp penalty or scaled by the tapered phase.** The CPW-Engine applies it without phase scaling; the recommended approach is MG-weight-1.0 / EG-weight-0 via the tapered pair, which achieves the same result at full phase and smoothly decays at low phase — consistent with the project's tapered-eval framework.

3. **Attacker-count gating: ≥2 attackers AND has queen vs. ≥2 attackers alone.** The CPW-Engine requires a queen on the attacking side; the Fruit/CPW King Safety page discusses the ≥2 attackers rule without the queen constraint. Both are citable. The queen constraint is more conservative (fewer positions trigger king-safety evaluation); recommend including it at M6.E for safety, then removing it if M6.F Texel shows it's over-restrictive.

4. **Pawn-storm omission as a known gap.** The omission is deliberate (§3 rationale) but means the engine will not penalize an advancing enemy pawn wave toward a weak pawn shield until the enemy pawns actually break through (at which point the open-file penalty fires). This is a known strategic blind spot at M6.E; the expected correction is through M6.F Texel on the pawn-structure (passed-pawn advancement bonus) terms.

---

## Sources

- [CPW — King Safety](https://www.chessprogramming.org/King_Safety)
- [CPW — CPW-Engine eval](https://www.chessprogramming.org/CPW-Engine_eval)
- [CPW — CPW King (enhancement)](https://www.chessprogramming.org/CPW_King)
- [CPW — Pawn Hash Table](https://www.chessprogramming.org/Pawn_Hash_Table)
- [CPW — Tapered Eval](https://www.chessprogramming.org/Tapered_Eval)
- [CPW — Evaluation Overlap](https://www.chessprogramming.org/Evaluation_Overlap)
- [CPW — King Pattern](https://www.chessprogramming.org/King_Pattern)
- [MadChess 3.0 Beta King Safety (2020)](https://www.madchess.net/2020/08/16/madchess-3-0-beta-6794c89-king-safety/)
- [MadChess 2.0 Beta Build 032 — King Safety (2014)](https://www.madchess.net/2014/12/24/madchess-2-0-beta-build-32-king-safety/)
- [TalkChess — Underwhelming results from king safety evaluation (t=82407)](https://talkchess.com/viewtopic.php?t=82407)
- [TalkChess — Most difficult feature to tune (p=447618)](https://talkchess.com/viewtopic.php?p=447618)
- [TalkChess — About king safety (p=404175)](https://talkchess.com/viewtopic.php?p=404175)
- [TalkChess — King PST vs separate king safety (t=80557)](https://talkchess.com/viewtopic.php?t=80557)
- [mACE Chess — King safety tuning (2013)](http://macechess.blogspot.com/2013/10/king-safety-tuning.html)
- [PeSTO description on rofchade.nl](https://rofchade.nl/?p=307)
- [CPW — PeSTO's Evaluation Function](https://www.chessprogramming.org/PeSTO%27s_Evaluation_Function)
- [CPW — Pawn Structure](https://www.chessprogramming.org/Pawn_Structure)
- [manuelfedele.github.io — Evaluate Chess Position](https://manuelfedele.github.io/posts/evaluate-chess-position/)
