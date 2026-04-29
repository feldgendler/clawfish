# Prior-Art Research: Killer Moves (M4.B)

Sources: Chess Programming Wiki (CPW), TalkChess forums (Bob Hyatt, H.G. Muller, Andrew Short, Sven Schule, et al.), Rustic Chess engine blog, Mediocre Chess blog, MadChess development log, Akl & Newborn 1977, Thé 1992 (McGill thesis), Wikipedia killer heuristic article.

Per ADR-0003, no engine source code was read. All findings come from CPW articles, forum threads, and blog posts.

---

## 1. What Is a Killer Move

- A quiet move (non-capture, non-promotion) that caused a beta-cutoff at a sibling cut-node, or at any earlier node with the same ply distance from the root.
- "Sibling" means: a different child of the same parent — same ply, different move sequence.
- The hypothesis: a move that refuted the opponent's attempt in one branch likely refutes similar attempts in parallel branches at the same ply.
- Killers are ply-indexed, not depth-indexed. [(TalkChess t=24920)](https://talkchess.com/viewtopic.php?t=24920) [(TalkChess t=35045)](http://www.talkchess.com/forum3/viewtopic.php?t=35045)

**Why ply, not remaining depth:**
- Extensions and reductions make remaining depth unpredictable.
- A depth-indexed table would mix killers from different colors when null-move is used.
- Ply is the distance from root, incremented for every make (including null moves).

---

## 2. Slot Count: Why Two

| Slots | Assessment | Source |
|---|---|---|
| 1 | Noticeably weaker than 2 | Rustic chess blog, CPW |
| 2 | Optimal balance; universally adopted | Bob Hyatt (TalkChess t=24920), Rustic, MadChess, Mediocre |
| 3+ | Uniqueness maintenance overhead exceeds gain | Rustic chess blog |

**Bob Hyatt's rationale for exactly 2** [(TalkChess t=24920)](https://talkchess.com/viewtopic.php?t=24920):
- One slot captures the "persistent" refutation that keeps defeating the opponent across many branches.
- A second slot holds the "local" killer — a new, even stronger refutation specific to the current move.
- Three requires enforcing three-way uniqueness at update time; the CPU cost exceeds the marginal ordering benefit.

**Empirical Elo reports:**
- Rustic: ~56 Elo self-play, ~35 Elo in real gauntlets (60% retention rate). [(TalkChess t=77734)](https://talkchess.com/viewtopic.php?t=77734)
- MadChess 2.0 Beta: +61 Elo. [(MadChess dev log)](https://www.madchess.net/tag/killer-move/)
- Akl & Newborn 1977: 20–50% reduction in nodes searched in selective tree searches. [(Semantic Scholar)](https://www.semanticscholar.org/paper/The-principal-continuation-and-the-killer-heuristic-Akl-Newborn/669ab80a9374542c31a4f6a4fba88db061d46834)

The 35–60 Elo range in real gauntlets is the best available estimate; exact gain is engine-dependent and decreases as other ordering heuristics (history, countermove) are added later.

---

## 3. Update Rule

### Standard: Shift-on-Distinct

On a quiet beta-cutoff with move `m` at `ply`:

```
if killers[ply][0] != m:
    killers[ply][1] = killers[ply][0]   // shift old slot-0 to slot-1
    killers[ply][0] = m                  // new cutoff move into slot-0
// if m == killers[ply][0]: no-op (already most recent)
```

- Invariant: both slots always hold distinct moves.
- The equality check before storing is mandatory; omitting it produces duplicate slots, which wastes a slot and may weaken the heuristic. [(TalkChess t=24501)](https://talkchess.com/forum3/viewtopic.php?t=24501)
- One variant reported that enforcing uniqueness actually *weakened* the engine — but the reporter acknowledged uncertainty about whether it was a bug elsewhere. Treat this as an isolated datapoint, not a reversal of consensus. [(TalkChess t=79854)](https://talkchess.com/viewtopic.php?t=79854)

### When to update

- **Yes**: quiet move causes beta-cutoff (the core case).
- **Yes**: move leading to mate by checkmate that is a quiet move (a quiet that mates is still a good ordering hint). CPW notes mate killers are "often separated and treated differently" — this is a separate "mate killer" refinement, not a reason to exclude them. [(CPW Killer Heuristic)](https://www.chessprogramming.org/Killer_Heuristic)
- **No**: captures and promotions. They are tried before killers by MVV-LVA; storing them as killers is redundant. [(Bob Hyatt, TalkChess t=24920)](https://talkchess.com/viewtopic.php?t=24920)

### Null-move interaction

- After a null move, the ply counter must be incremented (null move occupies a ply).
- Killers from within a null-move subtree go to the correct (opponent's) ply index automatically.
- Bob Hyatt: null-move refutations are "good ones to try first" — they are valid killers, not noise. [(TalkChess t=24920)](https://talkchess.com/viewtopic.php?t=24920)
- The fix for any null-move/killer bug is always "index by ply, increment ply for null moves" — not special-casing killers from null subtrees. [(TalkChess t=35045)](http://www.talkchess.com/forum3/viewtopic.php?t=35045)

---

## 4. Ordering Within Killer Slots

- `killers[ply][0]` is the more recent cutoff move; it is tried first.
- `killers[ply][1]` is the older cutoff move; it is tried second.
- No sorting by cutoff frequency — pure recency order.
- CPW: "most of the cutoffs come from the first killer slot." [(CPW Killer Heuristic)](https://www.chessprogramming.org/Killer_Heuristic)

---

## 5. Position in the Move Ordering Stack

The standard stack, consistent across CPW, Mediocre, Rustic, and TalkChess discussions:

| Priority | Moves |
|---|---|
| 1 | TT / hash move |
| 2 | Winning captures (MVV-LVA score > 0) |
| 3 | Equal captures (MVV-LVA score == 0) |
| 4 | **Killer moves** (killer[0] then killer[1]) |
| 5 | Quiets ordered by history heuristic |
| 6 | Losing captures (MVV-LVA score < 0) |

Notes:
- CPW's exact ordering places killers after all captures; some engines place killers between equal and losing captures. This report follows the "after all captures" position as the simpler default.
- The Mediocre Chess blog places killers "before captures" — this is an outlier and contradicts the universal intuition that a known-winning capture (PxQ) beats a speculative quiet. [(Mediocre Chess blog)](http://mediocrechess.blogspot.com/2007/01/guide-aspiration-windows-killer-moves.html)
- **Recommendation**: after all captures (both winning and equal), before history-ordered quiets, before losing captures. Consistent with CPW §5 of Move Ordering.

---

## 6. Score Values for Unified Ordering

When implementing a single unified sort rather than staged move generation, killers need explicit score values that sit below the minimum capture score and above history-heuristic quiet scores.

**Project's current MVV-LVA scale** (from the M3.C implementation at `src/search.rs::mvv_lva_score`):
- Formula: `victim_value * 16 - attacker_value` (PeSTO MG material values in centipawns).
- Smallest non-losing capture: QxP (queen takes pawn). PeSTO MG: P=82, Q=1025. Score = 82·16 − 1025 = **287**.
- (PxP is 82·16 − 82 = 1230 — much higher; pawn attacker cheap relative to victim.)
- The Rustic blog uses an arbitrary offset-based scheme where captures cluster above `MVV_LVA_OFFSET`, and killers sit below it. Exact values are engine-specific.

**Recommendation for this engine:**
- `killer[0]` score = any fixed constant below `min(MVV-LVA capture scores)`.
- `killer[1]` score = any fixed constant below `killer[0]` score.
- History heuristic scores (M4.C) must be further below `killer[1]`.
- Concrete values: with PeSTO MG material under the `*16` formula, minimum capture (QxP) = 287. Assign `killer[0]` = 200, `killer[1]` = 100. History heuristic scores then map to [0, 99].
- These are arbitrary as long as the ordering invariant holds; they can be tuned later.

**Open question**: whether to use a completely separate scoring domain (e.g., tagged enum rather than a single integer sort key) vs. unified integer. Both are common. Unified integer is simpler to implement and sufficient for M4.B.

---

## 7. Inter-Iteration Policy

### The open question

The roadmap asks: clear between iterations, or persist?

### Clear-between-iterations

- The TalkChess discussion that triggered the question explicitly advises: "Reset all killer array elements to 0" before progressive deepening. [(TalkChess t=20068)](https://talkchess.com/viewtopic.php?t=20068)
- CPW's implementation description implies clearing as the default.
- Benefit: no stale killers from a shallower tree polluting a deeper search where different positions are reachable.
- The bench determinism requirement (see M4.A §14) independently requires clearing between bench positions; the two rules are consistent.

### Persist between iterations

- Killers found at depth d are likely to still be good at depth d+1 in the same subtree — the position character changes little between iterations.
- Wikipedia killer heuristic article: "The main advantage of IDDFS in game tree searching is that the earlier searches tend to improve the commonly used heuristics, such as the killer heuristic." This implies *retaining* them provides the advantage.
- No direct TalkChess measurement of Elo difference between clearing vs. persisting found in the research.

### Aspiration re-search interaction (M4.D context)

- An aspiration re-search is a re-search at the **same depth** with a widened window.
- Killers found during the initial (narrow-window) search at depth d are valid cutoff hints for the wider-window re-search at the same depth.
- **Recommendation**: persist killers across aspiration re-searches at the same depth; clear when advancing to depth d+1.
- This is more nuanced than "clear between all iterations" but well-motivated.

### Recommendation for M4.B

**Ship with "clear between iterations" (zero the table at the start of each ID depth-step).** This is the simpler, more conservative default and the one that is explicitly documented in forum discussions. The persist-across-aspiration refinement can be added at M4.D when aspiration windows land, at which point the interaction is directly observable.

---

## 8. Killer Table Shape

### Options Compared

| Shape | Memory (64 plies, 2 slots) | Cache behavior | Ergonomics |
|---|---|---|---|
| `[[Option<Move>; 2]; 64]` | 64 × 2 × 3 bytes = 384 bytes (due to Option<u16> padding to 4 bytes each) | Fine; 384 bytes fits in L1 | Explicit None, easy to check |
| `[[Move; 2]; 64]` with sentinel (Move == 0) | 64 × 2 × 2 bytes = 256 bytes | Smaller; better cache | Requires a "null move" sentinel; need to ensure 0 is not a valid move encoding |
| `[[u16; 2]; 64]` raw | 256 bytes | Same as above | Maximum compactness; no ergonomics |

Notes:
- Our `Move` type is a `u16` per ADR. A raw zero value (`Move(0)` = a1→a1) is not a valid chess move, so it is usable as a sentinel.
- `Option<Move>` (i.e., `Option<u16>`) compiles to 4 bytes due to Rust's Option niche optimization only applying to non-zero types. To get the niche, use `NonZeroU16` as the inner representation; then `Option<NonZeroU16>` is 2 bytes.
- `[[u16; 2]; MAX_PLY]` = 256 bytes — fits comfortably in L1 cache.
- **Recommendation**: `[[u16; 2]; MAX_PLY]` with sentinel `0` (or `[[Move; 2]; MAX_PLY]` where `Move` is `Copy + Default + PartialEq`). Initialize to all-zeros at construction; zero-fill on clear. Compact, branchless equality check for the no-op case.

### Alternative

- `Option<Move>` is more ergonomic at the cost of 2× memory per slot if `Move` doesn't have a niche. Worth it only if the sentinel discipline creates bugs during M4.B development; can be refactored later.

---

## 9. Killer Move Legality Checking

### Why checking is needed

- A killer is stored from a sibling node where a piece occupied the from-square.
- By the time the killer is tried at the current node, the position may differ:
  - The from-square piece may have been captured.
  - The to-square may now be occupied by a friendly piece.
  - A slider's path may be blocked.
- Attempting an illegal killer: wasted movegen call; in the worst case, a king-capture crash. [(TalkChess t=68923)](https://talkchess.com/forum3/viewtopic.php?t=68923)

### Approaches

| Approach | Cost | Coverage |
|---|---|---|
| Check if killer is in legal-move list | One linear scan (max ~218 moves; avg ~30) | 100% correct; no false positives |
| Pseudo-legal check (from-square piece is ours, to-square not friendly) | Minimal | Catches most cases; misses slider obstruction |
| No check; accept occasional illegal attempt | Zero | Risk of crash or move-ordering bug |

### Recommendation for M4.B

**Check if killer is in the already-generated legal-move list** (linear scan by move value). Rationale:
- The project already uses legal-direct movegen (ADR-0007); the legal move list is generated before ordering.
- The scan cost is bounded and negligible relative to search cost.
- Eliminates the need for separate pseudo-legality infrastructure.
- Identical approach to the M4.A TT-move legality check (per §12 of the TT research note).

If a killer is not found in the legal-move list, simply skip it; do not insert it into the ordering.

---

## 10. Qsearch Applicability

**Consensus: killers do not participate in qsearch.**

Reasoning:
- Qsearch (when not in check) generates only captures and queen-promos; there are no quiet moves to order.
- Killers are quiet-move ordering hints; they have no candidates to act on in a captures-only qsearch.
- When in check, qsearch generates all legal evasions including quiets. The evasion set is small (≤ a handful) and the in-check subtree is shallow; killer ordering in this context adds complexity for negligible gain.
- No TalkChess or CPW discussion advocates for killers in qsearch.

**Practical rule for M4.B**: killers are consulted only inside `negamax`, never inside `qsearch`.

---

## 11. Mate Score / Draw Score Interaction with Update

- **Mate cutoffs**: a quiet move that delivers or forces mate causes a beta-cutoff; the move is a valid killer candidate and should update the killer table. The mate score value is irrelevant to this decision — only that the move type is quiet and the cutoff happened.
- **Draw scores**: a draw (0) at a sibling node does not cause a beta-cutoff (unless beta ≤ 0, which is unusual); draw-inducing moves do not become killers in normal play.
- **Mate killers** (a separate technique): some engines maintain a separate "mate killer" slot (priority above winning captures) for moves that refute with checkmate. CPW mentions this as a distinct refinement; it is not the default killer mechanism. [(CPW Killer Heuristic)](https://www.chessprogramming.org/Killer_Heuristic)
- **Recommendation for M4.B**: update killers on any quiet beta-cutoff regardless of score magnitude. Mate killer slot deferred to M5 or later.

---

## 12. Pitfalls Catalogued in the Literature

| Pitfall | Description | Fix |
|---|---|---|
| Depth vs. ply indexing | Killers indexed by remaining depth mix colors under null-move; caused 53.1% strength loss in one engine | Always index by `ply` (incremented for null moves too) |
| Duplicate slots | Both killer slots hold the same move; wastes one slot | Equality check before storing |
| No legality check | Killer from sibling may be illegal in current position (piece moved away, square blocked, friendly piece on to-square) | Scan legal-move list before inserting |
| Captures as killers | Captures are tried before killers via MVV-LVA; storing them in killer slots wastes space | Skip killers update for captures and promos |
| Failing to clear between bench positions | Killers from one bench position leak into the next, breaking bench determinism | Clear killers in `reset_for_new_game()` (already required by M4.A §14) |
| TT move == killer | A TT hit on the position uses the TT move for ordering; if that move also matches a killer slot, it will be tried a second time during killer scan | After inserting TT move at index 0, skip any killer that equals the TT move during the killer ordering pass |
| Killers from "cousin" subtrees | Without clearing `killers[ply+1]` on entry to `ply`, killers can be inherited from cousin nodes (same grandparent, different path) | HGM: "clear the killer slots for the next level, at the top of Search()" [(TalkChess t=61399)](https://talkchess.com/forum3/viewtopic.php?t=61399) — or accept occasional illegal move attempts as negligible (jwes) |

---

## 13. "Killers from Two Plies Ago" Technique

- Some engines also try killers from `ply - 2` (the grandparent's ply, which corresponds to the same side to move).
- CPW: "some programs use killers from two plies ago." [(CPW Killer Heuristic)](https://www.chessprogramming.org/Killer_Heuristic)
- Bob Hyatt: Cray Blitz experimented with killers from other plies "after trying the killers from the current ply." [(TalkChess t=20068)](https://talkchess.com/viewtopic.php?t=20068)
- This is an optional refinement; the standard two-slot current-ply-only implementation is the baseline.
- **Recommendation for M4.B**: skip. Implement standard current-ply killers only. Revisit if history heuristic (M4.C) does not fully subsume the gain.

---

## 14. Contested / Open Questions for the M4.B Plan

### Contested

1. **Clear-between-iterations vs. persist**: no direct Elo measurement found in the literature. The "clear" advice comes from one explicit recommendation focused on debuggability. The "persist is better" argument is implicit from the IDDFS improvement claim. The M4.B plan must pick one and commit; persisting through aspiration re-searches at M4.D is a natural follow-up.

2. **Position of killers relative to losing captures**: CPW places killers before losing captures; some engines vary. The ordering within the losing-capture band has low Elo impact; standardize on "killers before losing captures" as the simpler rule.

### Open questions (flag for plan)

3. **Unified score integer vs. tagged ordering**: whether to extend the existing `mvv_lva_score` function into a `negamax_order_score` that returns a single `i32` for all moves, or use a two-pass staged approach. Both work; unified integer sort is simpler but requires the killer/history score ranges to be chosen carefully now. The plan must decide.

4. **Clearing `killers[ply+1]` at node entry (HGM's advice)**: the CPW/TalkChess consensus is split — HGM recommends it; jwes accepts the rare illegal attempt. With legal-direct movegen and a legality scan, the illegal-attempt problem is already mitigated. The zeroing adds overhead on every node entry. Decision can be deferred to testing; the legality scan makes it non-critical.

---

## Summary of Recommendations

| # | Question | Recommendation | Notes |
|---|---|---|---|
| 1 | Slot count | 2 per ply | Universally confirmed; 1 is weaker, 3+ has overhead cost |
| 2 | Update rule | Shift-on-distinct; no-op if `m == killers[ply][0]` | Standard; captures/promos excluded |
| 3 | Ordering within slots | `killer[0]` before `killer[1]` (recency order) | Most cutoffs come from slot 0 |
| 4 | Position in stack | After all captures, before history quiets, before losing captures | Consistent with CPW Move Ordering §5 |
| 5 | Killer scores (unified sort) | `killer[0]` = 200, `killer[1]` = 100 (below min capture ~738); history [0, 99] | Arbitrary; adjust during M4.C if needed |
| 6 | Table shape | `[[u16; 2]; MAX_PLY]` with sentinel 0; init/clear = `memset 0` | 256 bytes; fits in L1 |
| 7 | Legality check | Linear scan of already-generated legal-move list | Mirrors TT-move check; zero extra movegen cost |
| 8 | Captures as killers | Never | Redundant with MVV-LVA |
| 9 | Mate cutoffs update killers | Yes | A quiet that mates is still a quiet beta-cutoff |
| 10 | Inter-iteration policy | Clear between iterations (zero table at each depth-step start) | Simpler; revisit persist-through-aspiration at M4.D |
| 11 | Qsearch | No killers in qsearch | No quiet moves to order outside in-check; overhead not worth it |
| 12 | Null move ply | Increment ply for null moves | Prevents wrong-color killer contamination |
| 13 | TT move == killer overlap | Skip killer if it equals the TT move already at index 0 | Prevents duplicate ordering |
| 14 | Killers from 2 plies ago | Defer | Standard baseline first |
| 15 | Clear `killers[ply+1]` on entry | Defer (legality scan mitigates the risk) | Revisit if ordering bugs surface |

---

## Source List

- [CPW — Killer Heuristic](https://www.chessprogramming.org/Killer_Heuristic)
- [CPW — Killer Move](https://www.chessprogramming.org/Killer_Move)
- [CPW — Move Ordering](https://www.chessprogramming.org/Move_Ordering)
- [CPW — MVV-LVA](https://www.chessprogramming.org/MVV-LVA)
- [CPW — History Heuristic](https://www.chessprogramming.org/History_Heuristic)
- [Rustic Chess — Killer Moves](https://rustic-chess.org/search/ordering/killers.html)
- [Rustic Chess — MVV-LVA](https://rustic-chess.org/search/ordering/mvv_lva.html)
- [Mediocre Chess — Aspiration Windows, Killer Moves, PVS](http://mediocrechess.blogspot.com/2007/01/guide-aspiration-windows-killer-moves.html)
- [MadChess — Killer Move tag](https://www.madchess.net/tag/killer-move/)
- [DailyChess / Rival — Killer Heuristics](https://www.dailychess.com/rival/programming/killers.php)
- [TalkChess t=77734 — How much Elo from killer moves?](https://talkchess.com/viewtopic.php?t=77734)
- [TalkChess t=79854 — Killer Move Heuristic](https://talkchess.com/viewtopic.php?t=79854)
- [TalkChess t=24920 — Killer moves and history table (Bob Hyatt)](https://talkchess.com/viewtopic.php?t=24920)
- [TalkChess t=20068 — Killer Moves](https://talkchess.com/viewtopic.php?t=20068)
- [TalkChess t=24501 — Killer moves not working](https://talkchess.com/forum3/viewtopic.php?t=24501)
- [TalkChess t=61399 — Killer heuristic (HGM on cousin-killer problem)](https://talkchess.com/forum3/viewtopic.php?t=61399)
- [TalkChess t=68923 — Staged move generation and killers (legality)](https://talkchess.com/forum3/viewtopic.php?t=68923)
- [TalkChess t=35045 — Killer moves with null move pruning](http://www.talkchess.com/forum3/viewtopic.php?t=35045)
- [Akl & Newborn 1977 — The Principal Continuation and the Killer Heuristic (Semantic Scholar)](https://www.semanticscholar.org/paper/The-principal-continuation-and-the-killer-heuristic-Akl-Newborn/669ab80a9374542c31a4f6a4fba88db061d46834)
- [Wikipedia — Killer heuristic](https://en.wikipedia.org/wiki/Killer_heuristic)
