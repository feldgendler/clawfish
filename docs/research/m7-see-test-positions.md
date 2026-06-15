# M7.A — SEE Test Positions: Prior Art & Curated Suite

Research for the Static Exchange Evaluation (SEE) correctness suite.
Sources consulted: Chess Programming Wiki, TalkChess forum threads, Mediocre Chess blog.
No engine source code read (ADR-0003 binding).

---

## 1. SEE Algorithm Fundamentals

### The Recursive Definition

The canonical recursive form (corrected by Gerd Isenberg on CPW, confirmed by Sven Schüle):

```
int see(int square, int side)
{
  value = 0;
  piece = get_smallest_attacker(square, side);
  if ( piece )
  {
    make_capture(piece, square);
    // key: max(0, ...) is the stand-pat / stop option
    value = max(0, piece_just_captured() - see(square, other(side)));
    undo_capture(piece, square);
  }
  return value;
}
```

For the *initial forced* capture (where the first mover has no stand-pat option):

```
int seeCapture(int from, int to, int side)
{
  piece = board[from];
  make_capture(piece, to);
  value = piece_just_captured() - see(to, other(side));
  undo_capture(piece, to);
  return value;
}
```

Source: [CPW Static Exchange Evaluation](https://www.chessprogramming.org/Static_Exchange_Evaluation),
[TalkChess correction thread](https://talkchess.com/viewtopic.php?t=30905)

### The Iterative Swap-List Algorithm (production form)

The CPW "Swap Algorithm" article documents an iterative bitboard version that avoids `make_capture`/`undo_capture`. Key steps:

1. Collect all attackers of the target square into `attadef` using `attacksTo(occ, toSq)`.
2. Maintain a working occupancy `occ`; after each capture, XOR out the capturing piece from both `occ` and `attadef`.
3. After removing a piece from `occ`, re-query for x-ray attackers that were hiding behind it: if the removed piece was a potential x-ray blocker (`mayXray = pawns | bishops | rooks | queens`), call `considerXrays(occ, ...)` to add newly revealed sliders.
4. Always pick the least-valuable remaining attacker for the side to move.
5. Store gains in `gain[d]`; apply minimax backward at the end: `gain[d-1] = -max(-gain[d-1], gain[d])`.

Source: [CPW SEE - The Swap Algorithm](https://www.chessprogramming.org/SEE_-_The_Swap_Algorithm)

### Standard Piece Value Scale

The swap-algorithm article uses an illustrative scale of:

| Piece | Value |
|-------|-------|
| Pawn  | 100   |
| Knight | 325  |
| Bishop | 325  |
| Rook  | 500   |
| Queen | 1000  |
| King  | ∞ (very large, e.g. 20000) |

The TalkChess t=83073 test suite uses the PeSTO-adjacent scale: P=82, N=337, B=365, R=477, Q=1025, K=12000.

The t=69052 suite expresses expected values symbolically as `value[pawn]`, `value[rook]`, etc. — intentionally scale-agnostic. **Our test suite should do the same** so the test remains valid regardless of which material scale the engine uses.

---

## 2. Definitional Ambiguities

These are areas where sources disagree or leave behaviour unspecified. Our spec must take an explicit position on each.

### 2.1 Pin Awareness

- Standard SEE **does not respect absolute pins**. A piece that is absolutely pinned (would expose the king to check if it moved) is counted as a valid attacker in the exchange sequence even though the move would be illegal.
- This means SEE can overestimate the defensive value of a position.
- Sources unanimously treat pin-ignorance as acceptable: "pins, overloads accumulate so rapidly that their result almost never is meaningful" (HGM, TalkChess t=48609).
- "Pin aware SEE" is referenced in two 2004 TalkChess threads by Dan Honeycutt but no algorithmic treatment appears in public prose.
- **Spec decision needed**: Does our SEE ignore pins (standard) or flag pinned pieces as illegal? The community default is ignore. If we choose pin-aware, no prose-sourced test suite covers this; all test cases would need to be hand-authored.
- The TalkChess t=69052 suite's positions 11 and 12 test a multi-attacker scenario that may appear to involve pinned pieces but the expected value of 0 is consistent with a pin-agnostic implementation (see Category 5 below).

### 2.2 Promotions

Three approaches documented in prose:

| Approach | Description | Source |
|----------|-------------|--------|
| Ignore promotion | Treat promoting pawn as worth pawn value throughout | Bob's view, t=73194 |
| Upgrade pawn value when capturing onto back rank | When a pawn's capture lands on rank 1 or 8, use `queen_value - pawn_value` as its "piece value" in the gain array | abulmo2, t=77787 |
| Exact recursive treatment | Play out promotion as a normal capture; the promoted queen joins the attacker set on the next ply | Jakob Progsch, t=77787 |

- The upgrade approach (option 2) is the consensus choice: it adds ~6 Elo in self-play tests (abulmo2) and integrates without special-casing the exchange loop itself.
- The test positions in t=69052 assume option 2 or 3: positions with a pawn capturing on the back rank have expected value `value[bishop]-value[pawn]` or `value[knight]-value[pawn]`, meaning the promoted piece's value enters the calculation.
- **Key gotcha**: A position with FEN `rn6/P7/K7/8/8/6k1/1Q6/8 w - - 0 1` from t=73194: Qxb8+. Without promotion: SEE = −1. With promotion accounting: +3 (Queen captures knight, Rook recaptures queen, Pawn promotes to Queen). Ignoring promotions produces a wrong result here.

### 2.3 En Passant

- En passant is the only capture where the captured piece is **not on the target square**. The capturing pawn moves to the en passant square (e.g., h6); the captured pawn is removed from h5 (a different square).
- This has two consequences for the bitboard SEE implementation:
  1. The initial `target` value must be `value[pawn]` even though no piece sits on the target square.
  2. The occupancy update must remove **both** pawns (the capturing pawn from its origin and the captured pawn from its actual location, not from the ep square). Failure to remove the captured pawn produces a wrong x-ray scan because that pawn's square is still treated as occupied.
- No prose source provides a complete worked example with explicit occupancy-bit manipulation. The TalkChess t=69052 suite provides two ep positions (see Category 3 below) with expected values, but does not explain the occupancy trick.
- **Spec decision needed**: Our implementation must handle the double-pawn-removal explicitly; the ep-square-only removal is a known bug class.

### 2.4 King in SEE Sequences

- The king can participate as an attacker in SEE (it can capture an undefended piece), but the sequence should stop if "capturing" with the king would expose it to a recapture (which would be illegal).
- The standard swap algorithm handles this implicitly: the king is added to the attacker set with a very high value (20000). Since it is the most expensive piece, it is only used when it is the last attacker remaining, and a recapture would mean the opponent "wins the king" — the stand-pat branch of the recursion kicks in because winning a king exceeds any material gain.
- CPW notes: "pieces hiding behind kings are futile since if it is a piece of opposite color it would just catch the king."
- **Practical implication**: Set `value[king]` to a value larger than any material sequence can overcome (20000 centipawns works; the TalkChess t=83073 suite uses 12000).

### 2.5 The Stand-Pat / Optional-Recapture Stop Condition

- A player **may choose not to recapture** even if they have a piece available. The `max(0, ...)` in the recursive formula encodes this.
- Consequence: `PxQ` = +900 even if the queen is defended by another pawn, because the capturing side can stop after winning the queen and still be ahead.
- Consequence: equal-value captures (NxN, BxB, RxR) with no further attackers = 0 (neither side gains; the stand-pat option terminates the sequence at 0 after the initial capture's gain is negated by the opponent's recapture).
- Equal-value captures with continued attackers on both sides may still result in 0 or positive/negative — it depends on the full sequence.
- **Ambiguity flagged by community**: When both sides have exactly one attacker remaining and the values are equal (RxR, no further pieces), the value is 0. Some implementations return `value[captured]` at depth 0 without the backward minimax pass; this would give 0 for NxN when the N is defended only once — which happens to coincide with the correct answer. But the implementation is wrong for positions with further attackers. This is not a test-suite ambiguity — the correct answer is 0 in these cases — but a common implementation error.

---

## 3. Test Positions by Corner-Case Category

All positions from the TalkChess t=69052 thread (tsoj's conversion; Jon Dart / Arasan source for most positions). Expected values are symbolic (`value[X]`) to be independent of piece-value scale.

**Important**: This suite originated from Jon Dart's Arasan engine `unit.cpp`. The full suite is referenced in TalkChess threads by multiple developers who treat the expected values as ground truth. ADR-0003 prohibits us from reading `unit.cpp` directly; these positions are reproduced here from the forum post in t=69052, which quoted them in full as prose/code.

### 3.1 X-Ray Attackers (Slider Battery Revealed After Front Piece Moves)

X-ray: a slider (bishop/rook/queen) sits behind another piece on the same ray. When the front piece captures and is removed from the occupancy, the x-ray slider gains line-of-sight to the target square and joins the exchange. The SEE scan must re-query `attacksTo` with the updated occupancy after each capture.

---

**XR-1**: Rook battery, x-ray rook behind attacking rook

- FEN: `4R3/2r3p1/5bk1/1p1r3p/p2PR1P1/P1BK1P2/1P6/8 b - - 0 1`
- Move: `h5g4` (pawn captures pawn)
- Expected: **0**
- Note: Multiple attackers and defenders on g4; the sequence nets zero. Tests that the algorithm counts a second rook or other hidden attacker joining after the first capture without overvaluing the exchange.

Source: TalkChess t=69052

---

**XR-2**: Same structure, different pawn layout (confirms XR-1 is not pawn-position-dependent)

- FEN: `4R3/2r3p1/5bk1/1p1r1p1p/p2PR1P1/P1BK1P2/1P6/8 b - - 0 1`
- Move: `h5g4`
- Expected: **0**

Source: TalkChess t=69052

---

**XR-3**: X-ray through multiple sliders; initial capture appears favorable

- FEN: `7r/5qpk/p1Qp1b1p/3r3n/BB3p2/5p2/P1P2P2/4RK1R w - - 0 1`
- Move: `e1e8` (rook captures on e8)
- Expected: **0**
- Note: Rook on e1 captures on e8; hidden rook on h1 eventually enters via x-ray after pieces clear. Net is zero. Tests multi-step x-ray involving multiple sliders.

Source: TalkChess t=69052

---

**XR-4**: Same square, attacker set reduced — x-ray now not enough

- FEN: `6rr/6pk/p1Qp1b1p/2n5/1B3p2/5p2/P1P2P2/4RK1R w - - 0 1`
- Move: `e1e8`
- Expected: **−value[rook]**
- Note: Two rooks defend e8; the single attacking rook loses a rook. Tests that when the x-ray side is outgunned, the SEE correctly returns a negative value.

Source: TalkChess t=69052

---

**XR-5**: Knight added, changing defense count

- FEN: `7r/5qpk/2Qp1b1p/1N1r3n/BB3p2/5p2/P1P2P2/4RK1R w - - 0 1`
- Move: `e1e8`
- Expected: **−value[rook]**

Source: TalkChess t=69052

---

**XR-6**: Rook-behind-rook x-ray on c-file; bishop net win

- FEN: `2r4k/2r4p/p7/2b2p1b/4pP2/1BR5/P1R3PP/2Q4K w - - 0 1`
- Move: `c3c5` (rook captures bishop)
- Expected: **value[bishop]**
- Note: White wins a bishop. Tests that the x-ray rook on c2 eventually enters the exchange after c3 moves, and the sequence terminates with a positive result.

Source: TalkChess t=69052

---

**XR-7**: Bishop swap that reveals a negative exchange

- FEN: `8/pp6/2pkp3/4bp2/2R3b1/2P5/PP4B1/1K6 w - - 0 1`
- Move: `g2c6` (bishop captures pawn on c6)
- Expected: **value[pawn] − value[bishop]**
- Note: White loses the exchange. Tests that an x-ray or second defender correctly produces a negative result.

Source: TalkChess t=69052

---

**XR-8**: Rook exchange where a hidden attacker tips the balance

- FEN: `4q3/1p1pr1k1/1B2rp2/6p1/p3PP2/P3R1P1/1P2R1K1/4Q3 b - - 0 1`
- Move: `e6e4` (rook captures pawn)
- Expected: **value[pawn] − value[rook]**
- Note: The rook loses material after the full exchange. Tests a multi-stage sequence with negative outcome.

Source: TalkChess t=69052

---

**XR-9**: Reachable second attacker changes a losing capture to a win

- FEN: `4q3/1p1pr1kb/1B2rp2/6p1/p3PP2/P3R1P1/1P2R1K1/4Q3 b - - 0 1`
- Move: `h7e4` (bishop captures pawn)
- Expected: **value[pawn]**
- Note: Compare with XR-8: the bishop on h7 (instead of the rook on e6 as initial attacker) changes the sequence outcome to positive. Tests that the first-mover's piece identity matters.

Source: TalkChess t=69052

---

### 3.2 Promotion-on-Capture (Pawn Capturing onto the Back Rank)

The key implementation choice: when a pawn's capture lands on rank 1 (for Black) or rank 8 (for White), the pawn promotes. The value used for that pawn in the gain array should reflect the promoted piece, not just pawn value.

Standard consensus: substitute `value[queen] - value[pawn]` for the pawn's value when it captures onto the promotion rank. This means the gain at that ply is `value[captured] + value[queen] - value[pawn]`.

---

**PR-1**: Pawn promotes to queen on capture, defended by bishop

- FEN: `6RR/4bP2/8/8/5r2/3K4/5p2/4k3 w - - 0 1`
- Move: `f7f8q` (pawn promotes to queen capturing on f8)
- Expected: **value[bishop] − value[pawn]**
- Note: White pawn captures on f8=Q; Black bishop recaptures; net = bishop − pawn. The promoting pawn's value should be counted as `queen`, not `pawn`. With standard pawn value this would return a misleading result (appearing to lose queen − bishop + pawn).
- Piece scale context (CPW illustrative): P=100, B=325, Q=1000 → expected ≈ 225.

Source: TalkChess t=69052

---

**PR-2**: Pawn promotes to knight on capture

- FEN: `6RR/4bP2/8/8/5r2/3K4/5p2/4k3 w - - 0 1`
- Move: `f7f8n` (pawn promotes to knight)
- Expected: **value[knight] − value[pawn]**
- Note: Same position as PR-1 but underpromotion to knight. The gain should reflect the knight's value, not the queen's. This tests that the SEE handles underpromotion choices correctly.

Source: TalkChess t=69052

---

**PR-3**: Pawn promotes to rook (underpromotion), defended by queen

- FEN: `7R/4bP2/8/8/1q6/3K4/5p2/4k3 w - - 0 1`
- Move: `f7f8r` (pawn promotes to rook)
- Expected: **−value[pawn]**
- Note: The promoted rook is captured by Black's queen after the bishop recapture; the net is negative. Tests that the full sequence beyond the initial promotion is evaluated correctly.

Source: TalkChess t=69052

---

**PR-4**: Promotion with multiple back-rank pieces; x-ray interplay

- FEN: `2r4r/1P4pk/p2p1b1p/7n/BB3p2/2R2p2/P1P2P2/4RK2 w - - 0 1`
- Move: `c3c8` (rook captures on c8, with b7 pawn able to promote)
- Expected: **value[rook]**
- Note: The b7 pawn can promote to a queen after the rook captures c8, so defenders must reckon with the promotion threat. The expected value is a full rook gain — not just a bishop — because the promotion makes continuing the exchange too costly for Black. This is the "taking promotion into account" variant of the famous Arasan test position.

Source: TalkChess t=69052, t=83073

---

**PR-5**: Same position, no second rook on h8 (Black's rook already gone)

- FEN: `2r5/1P4pk/p2p1b1p/5b1n/BB3p2/2R2p2/P1P2P2/4RK2 w - - 0 1`
- Move: `c3c8`
- Expected: **value[rook]**
- Note: The pawn promotion threat on b7 still makes Black unwilling to recapture, so White wins a full rook. This is the "canonical" Arasan test position quoted most frequently in forum discussions. The naive implementation (ignoring promotion) returns a bishop value instead.

Source: TalkChess t=69052, t=73194, t=83073

---

**PR-6**: Promotion test with exchange value determined by queening pawn

- FEN: `rn6/P7/K7/8/8/6k1/1Q6/8 w - - 0 1`
- Move: `b1b8` (Queen captures knight) — note: this is the *seeCapture* perspective on Qxb8
- Expected (with promotion): **+3** (in units of pawn, i.e., 300 with P=100 scale)
- Expected (without promotion): **−1**
- Sequence: Qxn (+300), Rxq (−900+300=−600 cumulative), Pxr=Q (+500−(−600)=... see below)
- Worked minimax: depth 0: gain = 300 (knight value). depth 1: gain = 900−300 = 600 (queen). depth 2: gain = (500+900−100) − 600 = 700 (rook + queen upgrade − pawn). Backward: gain[1] = −max(−600, 700) = 600; gain[0] = −max(−300, 600) = 300. SEE = 300 (positive, queen capture is good).
- Note: Tests that promotion dramatically changes a seemingly losing queen exchange into a positive one.

Source: TalkChess t=73194 (Richard Delorme / abulmo2)

---

### 3.3 En Passant (Captured Pawn Not on Target Square)

En passant is the only capture where the captured piece (the pawn) is not on the destination square of the moving piece. The target square (the ep square in FEN field 4) is where the moving pawn arrives; the captured pawn is one rank back.

**Implementation requirement**: When the move is en passant, the initial `target` piece value is `value[pawn]` (not 0, even though the ep square is empty). The occupancy update must remove the **captured pawn from its real location** (one rank behind the ep square), not from the ep square itself. Failure causes the captured pawn's bit to remain set, producing incorrect x-ray scans on subsequent iterations.

---

**EP-1**: En passant capture, one defender — SEE = 0

- FEN: `3q2nk/pb1r1p2/np6/3P2Pp/2p1P3/2R4B/PQ3P1P/3R2K1 w - h6 0 1`
- Move: `g5h6` (White pawn g5 captures en passant on h6)
- Expected: **0**
- Note: The g5 pawn captures the h5 pawn (value[pawn]) en passant, arriving on h6. A defender exists that recaptures the g-pawn. Net is 0. Tests the baseline en passant case.

Source: TalkChess t=69052

---

**EP-2**: En passant capture, additional bishop defender — SEE = value[pawn]

- FEN: `3q2nk/pb1r1p2/np6/3P2Pp/2p1P3/2R1B2B/PQ3P1P/3R2K1 w - h6 0 1`
- Move: `g5h6`
- Expected: **value[pawn]**
- Note: Same position but White has an extra bishop on e3 that enters the exchange after the g-pawn captures. With the extra attacker, White nets a pawn. Compares directly to EP-1 to isolate the effect of one additional attacker. Tests that the x-ray/further-attacker logic interacts correctly with the ep-capture setup.

Source: TalkChess t=69052

---

**Coverage note**: Only two en passant positions appear in this suite. Neither tests the x-ray from a slider that was behind the captured pawn (i.e., a rook on h5 or a bishop on the diagonal exposed when h5 pawn is removed). That specific sub-case — slider revealed by removal of the *captured* ep pawn — must be **hand-authored** (see Section 5).

---

### 3.4 Multi-Defender / Multi-Attacker Squares (Least-Valuable-Attacker Ordering)

These positions have several pieces from each side involved in the exchange. The correct answer depends on the side choosing the least-valuable attacker at each step.

---

**MA-1**: Bishop captures knight, multiple defenders — SEE = 0

- FEN: `4r1k1/5pp1/nbp4p/1p2p2q/1P2P1b1/1BP2N1P/1B2QPPK/3R4 b - - 0 1`
- Move: `g4f3` (bishop captures knight)
- Expected: **0**
- Note: Equal trade; further recaptures exist but the side to move correctly evaluates the exchange as net-zero. Tests the basic LVA equal-trade stop condition.

Source: TalkChess t=69052

---

**MA-2**: Pawn captures pawn on e5, multi-attacker

- FEN: `2r1r1k1/pp1bppbp/3p1np1/q3P3/2P2P2/1P2B3/P1N1B1PP/2RQ1RK1 b - - 0 1`
- Move: `d6e5`
- Expected: **value[pawn]**
- Note: Black wins a pawn after the exchange; the LVA ordering produces a net positive result.

Source: TalkChess t=69052

---

**MA-3**: Knight captures pawn; many defenders and attackers on e5

- FEN: `r2qk1nr/pp2ppbp/2b3p1/2p1p3/8/2N2N2/PPPP1PPP/R1BQR1K1 w kq - 0 1`
- Move: `f3e5` (knight captures pawn)
- Expected: **value[pawn]**
- Note: Classic opening-structure position with multiple pieces contesting e5. The ordering of captures (pawn first, then minor pieces) produces a net pawn gain for White.

Source: TalkChess t=69052

---

**MA-4**: Bishop captures pawn, balanced exchange — SEE = 0

- FEN: `6r1/4kq2/b2p1p2/p1pPb3/p1P2B1Q/2P4P/2B1R1P1/6K1 w - - 0 1`
- Move: `f4e5`
- Expected: **0**

Source: TalkChess t=69052

---

**MA-5**: Queen captures rook, multi-piece exchange on c1

- FEN: `2r2r1k/6bp/p7/2q2p1Q/3PpP2/1B6/P5PP/2RR3K b - - 0 1`
- Move: `c5c1` (queen captures rook on c1)
- Expected: **2×value[rook] − value[queen]**
- Note: Black queen captures the c1 rook; after full exchange Black wins two rooks for the queen (positive for Black). Tests a complex multi-piece exchange that crosses the exchange-value threshold.

Source: TalkChess t=69052

---

**MA-6**: First of two "same-attacker-set" positions that test attacker-bitset handling

- FEN: `8/4kp2/2npp3/1Nn5/1p2PQP1/7q/1PP1B3/4KR1r b - - 0 1`
- Move: `h1f1` (rook captures rook)
- Expected: **0**
- Note: Equal rook exchange; subtle because multiple pieces surround the square. With the queen present, the sequence must correctly determine that the net is zero, not negative.

Source: TalkChess t=69052

---

**MA-7**: Same attacker set minus White queen — confirming queen's role

- FEN: `8/4kp2/2npp3/1Nn5/1p2P1P1/7q/1PP1B3/4KR1r b - - 0 1`
- Move: `h1f1`
- Expected: **0**
- Note: Despite removing the queen from MA-6, the result is still 0. Together MA-6 and MA-7 form a pair that tests whether the engine correctly counts or misses the queen's contribution.

Source: TalkChess t=69052

---

### 3.5 Pinned-Piece Edge Cases and Equal-Value Stop Conditions

This is the category with the fewest unambiguously-sourced test positions. The community consensus is that standard SEE **ignores pins** and still returns useful heuristic values. No published prose suite provides positions designed specifically to test pin-aware SEE.

---

**EQ-1 / Pin-ignorance pair**: Two positions from the Arasan suite (MA-6/MA-7 above, positions 11 and 12 in t=69052) appear to test a scenario where a defending queen could be considered "soft-pinned" or overloaded, yet both positions yield SEE = 0. The pair demonstrates that a correct pin-ignorant SEE agrees with a pin-aware answer in these specific cases; they do not isolate pin behaviour.

---

**Open question — hand-authored positions needed**:

The following scenarios have no unambiguous prose-sourced positions:

1. **Absolutely pinned piece as the only defender**: e.g., a bishop pinned to the king on the diagonal is the only piece defending a pawn. Pin-ignorant SEE counts it as a defender; pin-aware SEE does not. Engines may legitimately differ. Any test position here must document which behaviour is expected.

2. **Equal-value trade with optional recapture, multiple layers**: e.g., NxN where both sides have one further attacker (a second knight and a second knight). The stand-pat option should cause each side to stop after capturing the first knight; expected SEE = 0 from the mover's perspective. This seems unambiguous, but no published prose positions confirm it.

3. **Overloaded defender**: A rook defends both c5 and c8; SEE on c5 counts it as a defender, but in reality it cannot recapture on c5 if it is also the only defender of c8. SEE structurally cannot model this (HGM, t=48609). No test positions exist in public prose precisely because the "correct" answer is engine-specific.

---

## 4. Quick-Reference Table

| ID | FEN | Move | Expected SEE | Category |
|----|-----|------|-------------|----------|
| XR-1 | `4R3/2r3p1/5bk1/1p1r3p/p2PR1P1/P1BK1P2/1P6/8 b - - 0 1` | h5g4 | 0 | X-ray |
| XR-2 | `4R3/2r3p1/5bk1/1p1r1p1p/p2PR1P1/P1BK1P2/1P6/8 b - - 0 1` | h5g4 | 0 | X-ray |
| XR-3 | `7r/5qpk/p1Qp1b1p/3r3n/BB3p2/5p2/P1P2P2/4RK1R w - - 0 1` | e1e8 | 0 | X-ray |
| XR-4 | `6rr/6pk/p1Qp1b1p/2n5/1B3p2/5p2/P1P2P2/4RK1R w - - 0 1` | e1e8 | −value[rook] | X-ray |
| XR-5 | `7r/5qpk/2Qp1b1p/1N1r3n/BB3p2/5p2/P1P2P2/4RK1R w - - 0 1` | e1e8 | −value[rook] | X-ray |
| XR-6 | `2r4k/2r4p/p7/2b2p1b/4pP2/1BR5/P1R3PP/2Q4K w - - 0 1` | c3c5 | value[bishop] | X-ray |
| XR-7 | `8/pp6/2pkp3/4bp2/2R3b1/2P5/PP4B1/1K6 w - - 0 1` | g2c6 | value[pawn]−value[bishop] | X-ray |
| XR-8 | `4q3/1p1pr1k1/1B2rp2/6p1/p3PP2/P3R1P1/1P2R1K1/4Q3 b - - 0 1` | e6e4 | value[pawn]−value[rook] | X-ray |
| XR-9 | `4q3/1p1pr1kb/1B2rp2/6p1/p3PP2/P3R1P1/1P2R1K1/4Q3 b - - 0 1` | h7e4 | value[pawn] | X-ray |
| PR-1 | `6RR/4bP2/8/8/5r2/3K4/5p2/4k3 w - - 0 1` | f7f8q | value[bishop]−value[pawn] | Promotion |
| PR-2 | `6RR/4bP2/8/8/5r2/3K4/5p2/4k3 w - - 0 1` | f7f8n | value[knight]−value[pawn] | Promotion |
| PR-3 | `7R/4bP2/8/8/1q6/3K4/5p2/4k3 w - - 0 1` | f7f8r | −value[pawn] | Promotion |
| PR-4 | `2r4r/1P4pk/p2p1b1p/7n/BB3p2/2R2p2/P1P2P2/4RK2 w - - 0 1` | c3c8 | value[rook] | Promotion |
| PR-5 | `2r5/1P4pk/p2p1b1p/5b1n/BB3p2/2R2p2/P1P2P2/4RK2 w - - 0 1` | c3c8 | value[rook] | Promotion |
| PR-6 | `rn6/P7/K7/8/8/6k1/1Q6/8 w - - 0 1` | a7a8q | +3 (P=100 scale) | Promotion |
| EP-1 | `3q2nk/pb1r1p2/np6/3P2Pp/2p1P3/2R4B/PQ3P1P/3R2K1 w - h6 0 1` | g5h6 | 0 | En passant |
| EP-2 | `3q2nk/pb1r1p2/np6/3P2Pp/2p1P3/2R1B2B/PQ3P1P/3R2K1 w - h6 0 1` | g5h6 | value[pawn] | En passant |
| MA-1 | `4r1k1/5pp1/nbp4p/1p2p2q/1P2P1b1/1BP2N1P/1B2QPPK/3R4 b - - 0 1` | g4f3 | 0 | Multi-piece |
| MA-2 | `2r1r1k1/pp1bppbp/3p1np1/q3P3/2P2P2/1P2B3/P1N1B1PP/2RQ1RK1 b - - 0 1` | d6e5 | value[pawn] | Multi-piece |
| MA-3 | `r2qk1nr/pp2ppbp/2b3p1/2p1p3/8/2N2N2/PPPP1PPP/R1BQR1K1 w kq - 0 1` | f3e5 | value[pawn] | Multi-piece |
| MA-4 | `6r1/4kq2/b2p1p2/p1pPb3/p1P2B1Q/2P4P/2B1R1P1/6K1 w - - 0 1` | f4e5 | 0 | Multi-piece |
| MA-5 | `2r2r1k/6bp/p7/2q2p1Q/3PpP2/1B6/P5PP/2RR3K b - - 0 1` | c5c1 | 2×value[rook]−value[queen] | Multi-piece |
| MA-6 | `8/4kp2/2npp3/1Nn5/1p2PQP1/7q/1PP1B3/4KR1r b - - 0 1` | h1f1 | 0 | Multi-piece |
| MA-7 | `8/4kp2/2npp3/1Nn5/1p2P1P1/7q/1PP1B3/4KR1r b - - 0 1` | h1f1 | 0 | Multi-piece |

---

## 5. Coverage Gaps — Hand-Authored Positions Needed

These categories are under-served by prose-sourced positions:

| Gap | Description | Reason no prose source covers it |
|-----|-------------|----------------------------------|
| EP + x-ray exposed by captured pawn | EP capture where the *captured pawn* (on its real square, one rank back) was blocking a slider that now attacks the ep target square | No prose example found; only en passant generics |
| Absolutely pinned sole defender | Pin-ignorant SEE counts pinned piece as valid defender; pin-aware does not | No published test positions; community says to ignore pins |
| EP where captured pawn blocks a diagonal | Bishop on a5-e1 diagonal blocked by a pawn on d4; EP takes d4, revealing the bishop as a new attacker | Not in any prose suite |
| King as final attacker | Sequence ends with only the king able to capture; king participates because no recapture is possible | No explicit FEN examples in prose; algorithm handles via value[king] = ∞ |
| Overloaded defender | Rook defends two squares; SEE on one square ignores the overload | Community explicitly flags this as outside SEE's scope; no test suite addresses it |
| Promotion to non-queen in sequence (not initial move) | A pawn in the middle of the exchange sequence promotes mid-chain (not the initial capture) | The t=69052 suite only has promotion as the initial move |

---

## 6. Recommended Spec Decisions for M7.A

Based on community consensus and the test positions:

| Decision | Recommendation | Rationale |
|----------|---------------|-----------|
| Pin handling | Ignore pins (standard) | Universal practice; no prose test suite for pin-aware SEE |
| Promotion value | Use `value[queen] − value[pawn]` when pawn captures onto back rank | ~6 Elo gain confirmed (abulmo2); matches all t=69052 promotion positions |
| Underpromotion in SEE | Use actual promoted piece's value, not queen's | Positions PR-1 and PR-2 in t=69052 explicitly test this |
| En passant | Initial target = `value[pawn]`; remove captured pawn from its real square in occupancy | Mandatory for correct x-ray scan |
| King in sequence | Include king with `value[king] = very large (e.g. 20000)`; stand-pat stops the sequence before a "king capture" is played | Standard approach; CPW confirms |
| Equal-value stop | Rely on the `max(0, ...)` stand-pat in the recursive form / backward minimax in swap form | Zero is the correct answer for NxN with no further attackers |

---

## 7. Sources

- [CPW — Static Exchange Evaluation](https://www.chessprogramming.org/Static_Exchange_Evaluation)
- [CPW — SEE - The Swap Algorithm](https://www.chessprogramming.org/SEE_-_The_Swap_Algorithm)
- [CPW — X-ray Attacks (Bitboards)](https://www.chessprogramming.org/X-ray_Attacks_(Bitboards))
- [CPW — En passant](https://www.chessprogramming.org/En_passant)
- [TalkChess t=69052 — "testpositions for static exchange evaluation?"](https://www.talkchess.com/forum3/viewtopic.php?t=69052) — primary source for the 24-position suite
- [TalkChess t=83073 — "Static Exchange Evaluation"](https://talkchess.com/viewtopic.php?t=83073) — piece value scale; PR-5 discussion
- [TalkChess t=73194 — "Promotions in SEE"](https://talkchess.com/viewtopic.php?t=73194) — PR-6 example; Bob vs HGM vs abulmo2 debate
- [TalkChess t=77787 p2 — "Static exchange evaluation with promotion" page 2](https://talkchess.com/viewtopic.php?t=77787&start=10) — Jakob Progsch position; consensus on upgrade approach
- [TalkChess t=48609 — "Static Exchange Evaluation..."](https://www.talkchess.com/forum3/viewtopic.php?t=48609) — HGM on SEE limitations; pins/overloads
- [TalkChess t=76750 — "SEE pruning nodes"](https://talkchess.com/viewtopic.php?t=76750) — soft-pin scenario; SEE vs full search
- [TalkChess t=30905 — "SEE algorithm on chessprogramming wiki"](https://talkchess.com/viewtopic.php?t=30905) — corrected recursive pseudocode
- [Mediocre Chess blog — "Guide: Static Exchange Evaluation"](http://mediocrechess.blogspot.com/2007/03/guide-static-exchange-evaluation-see.html) — iterative swap procedure description
