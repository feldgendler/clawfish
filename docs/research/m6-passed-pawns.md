# M6.C research — Passed-pawn evaluation

Prior-art synthesis for M6.C. Sources are Chess Programming Wiki articles,
blog posts, and TalkChess discussions — no engine source repos (ADR-0003).
Detection is already shipped (M6.B); this note covers the bonus design.

## 1. Passed-pawn rank bonus (Q1)

### 1.1 Shape and EG dominance

- The mainstream shape is **exponential in relative rank**: small at rank 2, large
  at rank 7 (one step from promotion). ([CPW — Passed Pawn](https://www.chessprogramming.org/Passed_Pawn))
- EG values dominate MG values at every rank. A rank-7 passer in the endgame
  is worth roughly half a pawn in material terms; the same passer in the
  middlegame is a positional plus but not yet decisive.
- Literature framing: "bonus increases as the pawn advances, often by dedicated
  piece-square tables" — the rank table *is* the primary evaluation instrument.
- The EG pawn PST in M6.A (PeSTO-derived) already rewards advanced pawns: rank-7
  (the row just before promotion) carries EG values of +132 to +187 from the
  PST alone. The passed-pawn bonus is **additive on top of** the PST — this is
  the principal double-count risk; see §4.

### 1.2 Literature-default tables

Two coherent sets are available in the literature.

**Set A — MadChess 3.0 Beta Build 103** (complete tapered table, self-consistent,
Texel-tuned within a single engine build):

| Rel rank | MG | EG | EG free-path |
|---|---|---|---|
| 0 (rank 1, impossible) | 0 | 0 | 0 |
| 1 (rank 2, home) | 0 | 0 | 0 |
| 2 | 3 | 4 | 8 |
| 3 | 8 | 18 | 34 |
| 4 | 15 | 42 | 77 |
| 5 | 24 | 75 | 138 |
| 6 | 34 | 118 | 216 |
| 7 (rank 7, one step from promo) | 0\* | 170 | 311 |

\* MadChess reports `000` for rank 7 MG. The bonus is overwhelmingly EG.
Source: [MadChess 3.0 Beta Build 103](https://www.madchess.net/2018/12/27/madchess-3-0-beta-build-103-passed-pawns/).
Elo credit for adding this evaluation: +119 reported. Designed and tuned as a
complete unit.

**Set B — approximate cross-engine center** (used when a single self-consistent
source is unavailable; values are rough literature midpoints):

| Rel rank | MG | EG |
|---|---|---|
| 2 | 5 | 10 |
| 3 | 10 | 20 |
| 4 | 20 | 40 |
| 5 | 35 | 70 |
| 6 | 55 | 110 |
| 7 | 0–20 | 150–200 |

These are not a coherent source — they span independently-authored engines with
heterogeneous PST assumptions.

**Recommendation: use Set A (MadChess tables) as the starting default.**  
Rationale: co-calibrated within one engine, from a single Texel tuning run,
pairs rank-bonus with path-discriminator semantics that are documented together.
The "free path" column becomes the path-clear bonus (§3). The base column is
the always-applied rank bonus, and the free-path column replaces it when the
path is fully clear. One open question: MadChess uses a different EG pawn PST
than M6.A's PeSTO values, so the +170 EG at rank 7 was calibrated against a
different PST baseline — the absolute magnitudes may need rescaling in M6.I
Texel.

### 1.3 Indexing

- **Relative rank for white:** `sq.rank()` (LERF rank index, 0-based). E4 pawn
  → rank index 3 → relative rank 3.
- **Relative rank for black:** `7 - sq.rank()`. E5 black pawn → rank index 4 →
  relative rank 3. This is the symmetric mirror.
- Ranks 0 and 1 are unreachable mid-board for any pawn (rank 0 = promotion
  rank, rank 1 = starting rank with no bonus earned yet). Set both to 0.
- Rank 7 in the table = the square where a promotion push is one move away (the
  LERF rank 6 square for white, LERF rank 1 for black).

## 2. King-distance / king-tropism term (Q2)

### 2.1 Which square to measure to

Three candidates, with literature consensus:

| Square | Rationale | Verdict |
|---|---|---|
| Pawn's own square | Trivial; misses the key question | Rejected — too coarse |
| Stop square (one ahead) | First square the pawn must control | **Literature mainstream** for static eval |
| Promotion square | Definitive destination | Used in Rule-of-the-Square; also mainstream |

CPW Rule of the Square: "programs use comparisons of absolute Chebyshev distance
from king and pawn squares to **promotion square**, considering side to move and
possible double step." ([CPW — Rule of the Square](https://www.chessprogramming.org/Rule_of_the_Square))

For a continuous eval heuristic (not a binary rule-of-the-square check), the
standard design measures king distance to the **promotion square** as the
primary signal, since that answers "how quickly can the king get there?" directly.
Measuring to the stop square is an acceptable simplification that avoids
confusion with the pass-through body of the pawn.

**Decision for M6.C:** measure to the **promotion square** (the h8/a8 square for
the pawn's file), consistent with the CPW canonical description and the
MadChess/endgame-theory motivation.

### 2.2 Distance metric

- **Chebyshev distance (king-move count)** is the canonical choice for king races,
  because a king can move diagonally. Manhattan distance over-counts corner routes.
  ([CPW — Distance](https://www.chessprogramming.org/Distance))
- The project already has `chebyshev_distance` in the codebase (used in mop-up
  eval, ADR-0031).
- Distance range: 0–7 (same square to opposite corner). CPW cap for pawn races:
  `min(5, distance(pawnSq, promoSq))` due to possible double-step.

### 2.3 King-tropism structure

The canonical formulation (paraphrasing CPW King Pawn Tropism):
- Own king near passer (or its promotion square) → **bonus**  
- Enemy king near passer (or its promotion square) → **penalty**

Standard weight for passers: `6` relative to backward pawns (`3`) and other pawns
(`2`) in the Bobby/Demoschach scheme. Translated to modern tapered design:

| Signal | Phase character | Typical coefficient |
|---|---|---|
| Own-king distance (to promo sq) | **EG-only** or EG-dominant | 5 cp per step closer (literature center) |
| Enemy-king distance (to promo sq) | **EG-only** or EG-dominant | −7 cp per step closer (literature center) |

Typical formula shape:
```
eg_king_tropism += (5 - own_king_cheb_to_promo_sq)  * 5;
eg_king_tropism += (enemy_king_cheb_to_promo_sq - 5) * 7;
```
where distances are clamped to [0, 5] — beyond 5 steps, king intervention is
typically too slow to matter at low search depths anyway.

### 2.4 Rank scaling

The king-tropism term is frequently scaled by how advanced the passer is (relative
rank), because a 7th-rank passer with the enemy king 3 steps away is far more
critical than the same king distance from a 3rd-rank passer:

```
tropism_eg += rank_factor[rel_rank] * (own_king_bonus + enemy_king_penalty)
```

A simple linear scale `rank_factor = rel_rank / 7` or `{0,0,1,2,3,4,5,6}` is
the minimal coherent choice. Without rank scaling the tropism is untapered across
advancement — it fires equally for a rank-2 and a rank-7 passer.

**Open question (flag for M6.I):** The exact coefficient (5 cp/step vs 7/step vs
rank-scaled variants) is **not sourced from a single co-calibrated table** in the
accessible literature. CPW King Pawn Tropism describes the concept qualitatively
(with the 6:3:2 ratio for Bobby/Demoschach), but gives no concrete per-step cp
value for a modern tapered engine. MadChess 3.0 uses a per-passer-constant
"King Escorted Passed Pawn: 11 cp" rather than a distance-proportional formula.
Recommend implementing the distance-proportional formula with placeholder
coefficients (5 own, 7 enemy, rank-scaled) and treating these as M6.I Texel
inputs.

## 3. Path-clear / path-blocked discriminator (Q3)

### 3.1 Committed semantic (from roadmap M6.C row)

> "blocked" = any opposing piece occupies a square on the pawn's
> path-forward bitboard. Opposing-pawn-attacked-but-empty squares are *not*
> blocked. Small bonus if path-forward is empty; penalty if blocked.

This is the **simplest CPW variant** and the most common in basic engine
implementations. It is a two-state discriminator applied to the pawn's front-span
bitboard (the same bitboard already used in detection, restricted to occupancy).

### 3.2 Three-state vs two-state

| State | Condition | Action |
|---|---|---|
| Path empty | `front_span & all_pieces == 0` | Apply bonus (path-clear) |
| Enemy piece on path | `front_span & enemy_pieces != 0` | Apply penalty (path-blocked) |
| Friendly piece only | `front_span & friendly_pieces != 0`, no enemy | *Neutral* (no bonus, no penalty) |

The three-state form (empty / enemy-blocked / friendly-only) is the correct
complete reading of the committed semantic. "Opposing piece" = enemy only.
A passer blocked by its own piece gets neither bonus nor penalty.

The CPW Passer discussion and TalkChess thread p=925582 confirm this: "big malus
if the passed pawn's stop square is blocked or attacked [by enemy]; smaller malus
if the path to promotion is blocked or attacked." The bonus variant (positive for
empty path) is the complement.

### 3.3 Magnitudes

MadChess provides the clearest co-designed figure: the "Endgame Free Passed Pawns"
column (§1.2) is the bonus that replaces the base bonus when the path is
completely clear. Computing the differential:

| Rel rank | Free-path EG | Base EG | Path-clear bonus delta |
|---|---|---|---|
| 2 | 8 | 4 | +4 |
| 3 | 34 | 18 | +16 |
| 4 | 77 | 42 | +35 |
| 5 | 138 | 75 | +63 |
| 6 | 216 | 118 | +98 |
| 7 | 311 | 170 | +141 |

The blocked-path penalty is the mirror: in Fabio Gobbato's framing (TalkChess
t=58583), a "big malus related to the rank if the stop square is blocked" and
"smaller malus if the path to promotion is blocked." This implies the penalty
magnitude is roughly symmetric with the path-clear bonus (same rank scaling).

**MG character:** MadChess reports the path-clear distinction only in the EG
column (base MG values are small; the free-path column is EG-only). This is
consistent with the phase character of passed pawns generally: the promotion
threat is an EG feature.

**Recommendation:** Apply the MadChess delta column as the path-clear bonus (above
the base rank bonus), and apply −(delta) as the path-blocked penalty, both EG-only
or heavily EG-weighted. MG bonus/penalty of zero is a defensible start; M6.I
Texel adjusts.

### 3.4 Bitboard implementation note

`path_forward = front_span(pawn_square)` — for white, `north_fill(pawn_square <<
8)` (i.e., all squares strictly ahead). This is already computed in M6.B's
infrastructure. Do not include the pawn's own square. The promotion square is
included in the path.

## 4. Co-design verdict (Q4)

### 4.1 Is the M6.C sub-evaluator jointly designed?

**MadChess 3.0 Build 103** presents the three M6.C terms (rank bonus + path-clear
discriminator + king-escorted bonus) as a **single coherent tuning run**. They
are co-calibrated: the base rank bonus, the free-path bonus, and the king-escort
constant were tuned together, not sourced independently. This is the correct
provenance pattern (contrast with M6.B's pastiche failure).

**The king-distance-proportional formula** (as opposed to MadChess's flat
king-escort constant) does not appear as a coherently sourced MG/EG-split
coefficient table in the accessible literature without reading engine source.
The CPW King Pawn Tropism article gives conceptual rationale and the 6:3:2
relative-weight scheme (not absolute cp values). This means the king-distance
coefficients are **independently sourced** from the rank-bonus and path tables —
a potential M6.B-style pastiche risk if the magnitudes clash.

**Verdict:** Partial co-design. The rank-bonus + path-discriminator pair from
MadChess is coherently sourced. The king-distance term coefficients are
independently chosen. This creates a calibration dependency: the king-distance
term measures king-to-passer distance, while the rank bonus already captures
advancement value that correlates with king urgency. They overlap in that both
are EG-heavy and both respond to rank. However, the overlap axis is weaker than
the M6.B ISO×CONN connectivity double-count: rank-bonus and king-distance are
**different axes** (advancement value vs king race), so additive stacking is
unlikely to produce the catastrophic −100 Elo of M6.B. The risk is over-weighting
king urgency, not a sign-flipping contradiction.

**Risk mitigation:** start with the MadChess rank-bonus + path tables (coherent
source), add small king-distance coefficients as an additive term with M6.I
Texel flagged, and run SPRT per-term if the combined result is suspicious.

### 4.2 Overlap with existing M6.A–B eval terms

| Overlap axis | Existing term | Passed-pawn term | Risk |
|---|---|---|---|
| Advanced pawn rewarded | EG pawn PST (all pawns; PeSTO) | Rank bonus (passers only) | **Real but bounded.** PST rewards all pawns; rank bonus adds an extra premium for the passer property. This is not double-counting the *same fact* — it's "pawn on rank 6" (PST) + "pawn on rank 6 AND is a passer" (rank bonus). Acceptable additive stack. |
| Rank-6/7 advanced pawn | CONN rank table (connected pawns, rank-scaled) | Rank bonus (passers) | **Weaker overlap.** A connected passer gets both CONN and passed-pawn bonus. But CONN measures *connectivity* (phalanx/defended); passed-pawn measures *freedom from opposition*. Different axes. The M6.B ISO×CONN disaster was a same-axis sign-opposite double-count; CONN + passed-pawn is different-axis additive stacking. Acceptable. Monitor in SPRT. |
| Advancement bonus | EG pawn PST at rank 7 (+132 to +187) | Rank-7 bonus (+170 EG in MadChess) | **Highest risk.** At rank 7, the combined PST + passed-rank bonus reaches +300–350 EG for a single passer. This is ~3 pawns of EG credit for a single passed pawn on rank 7, which may be too aggressive at low search depth (search won't see the promotion yet at shallow depth). The MadChess tables were calibrated with a *different* EG pawn PST. A rank-7 passer's combined evaluation should be cross-checked against practical game data. |

### 4.3 Passed-pawn bonus vs CONN for connected passers

A pawn that is both connected (CONN bonus in M6.B) and passed (M6.C rank bonus)
receives both. This is additive stacking. Unlike the ISO×CONN case:
- ISO penalized non-connectivity (no adjacent friendly pawn)  
- CONN rewarded connectivity (adjacent friendly pawn)
- These are literally the same fact, opposite sign → catastrophic

CONN and passed-pawn bonus are **different facts** (connectivity vs opposition-free).
A connected non-passer gets CONN only. An isolated passer gets passed-pawn bonus
only (once ISO is re-introduced in M6.I with correct weights). Connected passer
gets both — which is correct: it is both structurally connected and structurally
free-running. This is the correct semantics, not a double-count.

## 5. Stacking vs suppression (Q5)

**Verdict: additive stacking is the correct design, consistent with M6.B precedent.**

- M6.B ships no if-else suppression between pawn-structure terms (from ADR-0032 §6).
- The passed-pawn bonus belongs in the same additive evaluator: the pawn earns
  the CONN bonus for being connected AND the passed-pawn bonus for being free.
- Literature consensus: passed-pawn bonus is an additional layer, not a
  replacement. The CPW article lists passed pawns as one feature among many,
  applied cumulatively.
- The M6.B catastrophe was not about additive stacking *per se*; it was about
  stacking **opposite-signed same-axis terms with mismatched magnitudes**. The
  M6.C terms measure different axes from the existing CONN, so additive stacking
  is safe.
- Suppress only if SPRT shows an interaction problem — don't pre-suppress.

## 6. Explicitly out of scope for M6.C (Q6)

| Feature | Why deferrable | Destination |
|---|---|---|
| Candidate (half-)passers | Requires a recursive or approximate bitboard check; adds detection complexity; M6.C benefit/risk ratio is better without it | M6.I or later |
| Connected-passer extra bonus | Requires identifying connected passer pairs; moderate complexity; additive on top of existing CONN + passed-rank | M6.I (Texel adjusts implicitly when CONN + passed-rank are jointly tuned) |
| Protected-passer extra bonus | Modest complexity; "long-range" bonus for passed pawn defended by friendly pawn; overlaps with CONN for many cases | M6.I |
| Unstoppable passer / Rule of the Square (static eval) | Search-interacting: "programs rely on search to solve all kinds of possible tactics" (CPW Unstoppable Passer). Static only gives a large bonus that can mislead shallow search. Explicitly fragile. | M8 or later; requires search extension or tablebase support |
| Rook-behind-passer (Tarrasch rule) | Requires rook position info — cannot live in the pawn-hash evaluator; needs piece evaluator integration | M6.D (mobility) or dedicated piece-eval phase |
| Passed-pawn blockade by own piece | Own piece blocking own passer is a strategic weakness but minor in strength gain; search handles it | M6.I candidate or defer |
| Outside passed pawn extra bonus | Requires identifying leftmost/rightmost passer relative to all other pawns; king deflection logic | M6.I candidate |
| Pawn race / tempo-aware evaluation | STM-dependent; cannot be cached in pawn hash; requires careful interaction with search | M8 (post-tablebase) |

None of these are required for a useful M6.C. The three M6.C components (rank
bonus + king-distance + path discriminator) are independently sufficient for a
meaningful strength gain.

## 7. Pitfalls / bug taxonomy (Q7)

| Pitfall | Mechanism | Mitigation |
|---|---|---|
| Off-by-one in front-span | Including the pawn's own square in the path-forward bitboard | `front_span = north_fill(sq << 8)` — shift before fill, not inclusive of own square; identical to M6.B detection code |
| Relative-rank for black | Using raw LERF rank index for black gives the wrong relative rank | Black relative rank = `7 - sq.rank()`; test with a black pawn on e5 (LERF rank 4, relative rank 3) |
| Promotion square for black | White f7 promotes on f8; black f2 promotes on f1 | `promo_sq = file_of(sq) | 0` for black, `file_of(sq) | 7` would be wrong; verify file is preserved, rank 0 for black, rank 7 for white |
| King-distance sign error | Adding both own-king and enemy-king distances as bonuses, or both as penalties | Own king near = bonus (+), enemy king near = penalty (−); a shorter own-king distance is BETTER |
| Path-forward includes promotion square | Some implementations stop the span one square before promotion, others include it | The "free path" test should include the promotion square (it must also be unblocked); include it |
| Double-count with EG pawn PST at rank 7 | PST contributes +132 to +187 EG at rank 7; adding +170 MadChess rank-7 bonus reaches ~+350 EG total | Monitor in SPRT; if rank-7 passer eval is excessive, scale down rank-7 table entry first |
| Double-count with CONN for connected passers | Connected passer gets both CONN rank-scaled bonus and passed-rank bonus | Intentional and correct (different axes); do not suppress — see §4.3 |
| King-distance term in pawn hash | King moves invalidate a cached king-distance value (ADR-0032 §3 explicit) | Compute king-distance live in `evaluate_cached`, outside the pawn-hash probe; confirmed architecture |
| Path-blocked by own piece assigned a penalty | Own piece blocking path → penalty applied, incentivizing moving own piece out | Use three-state discriminator (§3.2): own-piece-only blocking = neutral, no penalty |
| Rank-7 MG bonus non-zero | If the table has a non-zero MG value at rank 7, it contributes in the MG phase when a rank-7 passer should be decisive in EG only | Set MG rank-7 entry to 0 (MadChess does this); rank-7 value is almost entirely EG |

## 8. Recommended default weights

**Rank bonus** (use MadChess Set A, §1.2 — coherent single source):

```
PASSED_MG = [0, 0,  3,  8, 15,  24,  34,   0];  // indexed by relative rank 0..7
PASSED_EG = [0, 0,  4, 18, 42,  75, 118, 170];  // same
```

**Path-clear bonus** (EG-only delta added when front-span clear of ALL pieces):

```
PASSED_FREE_EG_DELTA = [0, 0, 4, 16, 35, 63, 98, 141];  // = MadChess free - base EG
```

So: `passer_eg = PASSED_EG[rel_rank] + if path_clear { PASSED_FREE_EG_DELTA[rel_rank] } else if path_blocked_by_enemy { -PASSED_FREE_EG_DELTA[rel_rank] } else { 0 }`

**King-distance term** (EG-only; placeholder coefficients for M6.I Texel):

```
OWN_KING_BONUS_PER_STEP   = 5;   // per Chebyshev step closer to promo sq
ENEMY_KING_PENALTY_PER_STEP = 7; // per Chebyshev step closer to promo sq
KING_DIST_CAP = 5;               // clamp distance at 5
```

Formula (EG only):
```
let own_dist   = chebyshev_distance(own_king, promo_sq).min(KING_DIST_CAP);
let enemy_dist = chebyshev_distance(enemy_king, promo_sq).min(KING_DIST_CAP);
let rank_scale = rel_rank as i32;  // 0..7 linear scale
eg_bonus += rank_scale * OWN_KING_BONUS_PER_STEP   * (KING_DIST_CAP - own_dist as i32);
eg_bonus -= rank_scale * ENEMY_KING_PENALTY_PER_STEP * (KING_DIST_CAP - enemy_dist as i32);
```

Note: king-distance coefficients (5 and 7) are not co-calibrated with the rank
bonus table. Flag for M6.I Texel. Start small; the M6.B lesson is that
over-scaled independent terms cause cliffs.

## 9. Provenance summary

| Term | Source | Co-calibration status |
|---|---|---|
| Rank bonus MG/EG | MadChess 3.0 Build 103 | **Co-calibrated** within one Texel run |
| Path-clear delta EG | MadChess 3.0 Build 103 (free-path minus base) | **Co-calibrated** (same run as rank bonus) |
| King-distance coefficients | CPW King Pawn Tropism (qualitative); MadChess uses flat constant (11 cp) | **Not co-calibrated** — independent source; treat as placeholder |

The rank-bonus + path-discriminator pair is a coherent co-designed unit.
The king-distance term is an independent addition. Unlike the M6.B ISO×CONN
catastrophe (same axis, opposite sign), king-distance is a *different axis* from
rank-bonus, so catastrophic double-counting is unlikely — but miscalibrated
magnitude is a real risk. SPRT the full M6.C vs M6.B without any term isolation
first; if it regresses, isolate king-distance.

## Sources

- [CPW — Passed Pawn](https://www.chessprogramming.org/Passed_Pawn)
- [CPW — Passed Pawns (Bitboards)](https://www.chessprogramming.org/Passed_Pawns_(Bitboards))
- [CPW — King Pawn Tropism](https://www.chessprogramming.org/King_Pawn_Tropism)
- [CPW — Rule of the Square](https://www.chessprogramming.org/Rule_of_the_Square)
- [CPW — Distance (Chebyshev)](https://www.chessprogramming.org/Distance)
- [CPW — Unstoppable Passer](https://www.chessprogramming.org/Unstoppable_Passer)
- [CPW — Candidate Passed Pawn](https://www.chessprogramming.org/Candidate_Passed_Pawn)
- [CPW — Protected Passed Pawn](https://www.chessprogramming.org/Protected_Passed_Pawn)
- [CPW — Connected Passed Pawns](https://www.chessprogramming.org/Connected_Passed_Pawns)
- [CPW — Blockage Detection](https://www.chessprogramming.org/Blockage_Detection)
- [CPW — Tarrasch Rule](https://www.chessprogramming.org/Tarrasch_Rule)
- [CPW — Evaluation Overlap](https://www.chessprogramming.org/Evaluation_Overlap)
- [CPW — Texel's Tuning Method](https://www.chessprogramming.org/Texel%27s_Tuning_Method)
- [CPW — Automated Tuning](https://www.chessprogramming.org/Automated_Tuning)
- [CPW — PeSTO's Evaluation Function](https://www.chessprogramming.org/PeSTO%27s_Evaluation_Function)
- [MadChess 3.0 Beta Build 103 (Passed Pawns)](https://www.madchess.net/2018/12/27/madchess-3-0-beta-build-103-passed-pawns/)
- [TalkChess — Passer evaluation (p=925582)](https://talkchess.com/viewtopic.php?p=925582)
- [TalkChess — Passed pawn evaluation (t=58583)](https://talkchess.com/viewtopic.php?t=58583)
- [TalkChess — Passed Pawns endgame (t=34198)](https://talkchess.com/forum3/viewtopic.php?t=34198)
- [TalkChess — Most common evaluation features (t=55955)](https://talkchess.com/viewtopic.php?t=55955)
- [MadChess tag: Passed Pawn](https://www.madchess.net/tag/passed-pawn/)
- docs/research/m6-pawn-structure.md (M6.B research, §1.5 and §6)
- docs/decisions/0032-pawn-structure-and-pawn-hash.md (§3 king-distance not cached; §7 M6.B catastrophe)
