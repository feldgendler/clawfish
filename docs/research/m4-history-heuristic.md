# Prior-Art Research: History Heuristic Design for M4.C

Sources consulted: Chess Programming Wiki (CPW), TalkChess forum threads (Hyatt, Muller, Costalba, et al.), Mediocre Chess blog (Jonatan Dahl), MadChess blog (Erik Madsen), Rustic chess engine book (Marcel Vanthoor), Winands et al. 2004 "The Relative History Heuristic" (Computers and Games), Beowulf chess theory (Colin Frayn).

Per ADR-0003, no third-party engine source code was read; CPW articles, papers, blog posts, and forum threads were the source material. This report is the evidence base for the M4.C ADR.

---

## Recommended Choices (Quick Reference)

| Decision | Recommendation | Rationale |
|---|---|---|
| Indexing | `[side][from][to]` (butterfly, 2 × 64 × 64) | Most common; distinguishes color; moderate memory (32–64 KiB) |
| Increment formula | `+= depth * depth` for bonus | Standard; near-universal; depth-linear biases leaves too heavily |
| History malus | Yes — penalize all quiets searched before the cutoff-move | Standard with `depth * depth`; prevents saturation and improves ordering |
| Aging strategy | Saturate at `[-MAX_HISTORY, MAX_HISTORY]` with history-gravity formula (advanced), OR halve-on-threshold (simple) | Gravity is the modern default; halve-on-threshold is a simpler valid choice |
| Inter-iteration | Persist across ID iterations within a single `go`; clear on `ucinewgame` / bench-reset | Cross-iteration carry-over is the main benefit of history; clearing between iterations destroys it |
| Score range | `i16`, clamped to `±16384` | Fits in i16 with headroom; avoids overflow on `depth * depth` accumulation |
| Datatype | `i16` per entry | Same range as TT score field; i32 safe alternative if overflow is a concern |
| Pipeline position | After killers, before unordered quiets | Confirmed by CPW, forum consensus, Rustic docs |
| Capture history | Out of scope for M4.C | Standard history applies to quiets only; capture history is a distinct M5+ technique |
| TT-hit update | Do NOT update history on TT early-return | Only real-search beta cutoffs have the "this move refuted" semantics; TT-hit is a memory-level shortcut |

---

## 1. Indexing Scheme

### Options

| Scheme | Array shape | Entries per color | Total bytes (i16) |
|---|---|---|---|
| `[from][to]` (butterfly) | 64 × 64 | 4,096 | 8 KiB × colors |
| `[side][from][to]` | 2 × 64 × 64 | 4,096 per color | 16 KiB |
| `[piece][to]` | 12 × 64 | 768 (6 types × 2 colors) | 1.5 KiB |
| `[side][piece][to]` | 2 × 6 × 64 | 768 per color | 1.5 KiB |

### Key Facts

- Butterfly (`[from][to]`) is the most widely cited scheme; it is the basis of the name "Butterfly Boards." [(CPW Butterfly Boards)](https://www.chessprogramming.org/Butterfly_Boards)
- CPW gives the canonical example as `history[sideToMove][from][to] += depth*depth`, confirming the side dimension is standard. [(CPW History Heuristic)](https://www.chessprogramming.org/History_Heuristic)
- Butterfly has **low density**: only 1,792 of 4,096 `[from][to]` pairs correspond to legal moves, giving 7/16 (44%) utilization. The wasted entries cost no runtime — they are never accessed — but do consume cache lines. [(CPW Butterfly Boards)](https://www.chessprogramming.org/Butterfly_Boards)
- The `[piece][to]` variant (12 × 64 = 768 entries) saves ~75% of the memory and improves density. One forum developer reported a 25%+ endgame speed improvement with piece-type indexing; in that context the density improvement was more cache-friendly. [(TalkChess t=27598)](https://talkchess.com/viewtopic.php?t=27598)
- `[piece][to]` conflates multiple positions with the same move: e.g., all queen moves to d4 from any origin share a single counter. This loses origin-square context; empirically reported as effective but theoretically less discriminative.
- `[side][from][to]` disambiguates White and Black for the same move. Without the color dimension, a White Rook e2–e4 and a Black Rook e7–e5 share a counter, causing interference. CPW's canonical pseudocode includes the side dimension. [(CPW History Heuristic)](https://www.chessprogramming.org/History_Heuristic)
- Omitting the color dimension ("plain butterfly") is reported to work in practice because quiet-move cutoffs are statistically unlikely to have systematic opposite-color interference, but it is conceptually wrong and not recommended.

### Recommendation

**`[side][from][to]`** — 2 × 64 × 64 = 8,192 `i16` entries = 16 KiB. Correct color separation; standard CPW citation; moderate memory cost well within budget. The `[piece][to]` variant is a valid lower-memory alternative if cache pressure becomes measurable, but the density savings are unlikely to matter at classical-eval strength.

---

## 2. Increment Formula

### Variants Compared

| Formula | Growth per depth | Behavior |
|---|---|---|
| `+= 1` | Linear in hits | Original Schaeffer 1983; does not weight by depth |
| `+= depth` | Linear in depth | Weighted but grows slowly at shallow depth |
| `+= depth * depth` | Quadratic in depth | Most common modern default |
| `+= 1 << depth` | Exponential | Schaeffer's original suggestion; overflows at depth ~31 for 32-bit counter; abandoned |
| History gravity | Self-limiting; see below | Modern saturation-aware formula |

### Key Facts

- The exponential `1 << depth` formula was Schaeffer's original suggestion but is unworkable: at depth 31 a single increment is `2^31` which overflows a 32-bit counter. Bob Hyatt was reportedly the first to switch to `depth * depth` as a practical quadratic alternative. [(TalkChess t=25118)](https://talkchess.com/viewtopic.php?t=25118&start=10)
- `depth * depth` is the de facto standard cited in CPW, Mediocre Chess, Rustic, and MadChess. The rationale: moves near the leaves (small depth) contribute modestly; moves at higher iterative-deepening depths contribute more, but the quadratic growth is bounded and predictable. [(CPW History Heuristic)](https://www.chessprogramming.org/History_Heuristic)
- Winands et al. 2004 tested `{1, depth, depth², 2^depth}` for the Relative History Heuristic in Lines of Action and found `1` worked best in that game — but chess results diverge; `depth²` is the chess consensus. [(CPW Relative History Heuristic)](https://www.chessprogramming.org/Relative_History_Heuristic)
- One developer reported `depth * depth + depth - 1` gained ~10 Elo over plain `depth * depth`. Small linear correction; may or may not be worth the deviation from the canonical form. [(TalkChess t=81716)](https://talkchess.com/viewtopic.php?t=81716)
- Modern bonus formula from CPW: `const int bonus = 300 * depth - 250;` (empirically tuned linear formula used as the `clampedBonus` argument to the gravity formula below). [(CPW History Heuristic)](https://www.chessprogramming.org/History_Heuristic)

### History Gravity Formula (Advanced)

Modern engines use a self-limiting update instead of a raw add:

```
clampedBonus = clamp(bonus, -MAX_HISTORY, MAX_HISTORY)
history[side][from][to] += clampedBonus
                         - history[side][from][to] * abs(clampedBonus) / MAX_HISTORY
```

This formula:
- **Scales up** updates when the cutoff is unexpected (history value near 0 → large update).
- **Scales down** updates when the cutoff was expected (history value near MAX_HISTORY → small update).
- **Automatically bounds** values in `[-MAX_HISTORY, MAX_HISTORY]` without explicit clamping after the update — the multiplicative term acts as a damper. [(CPW History Heuristic)](https://www.chessprogramming.org/History_Heuristic)

**For M4.C:** start with plain `+= depth * depth` (simple, well-understood). The gravity formula is a refinement for M5+ once move ordering quality is measurable by SPRT.

---

## 3. History Malus / Decrement on Fail

### What It Is

When a quiet move Q causes a beta cutoff at node N, all quiet moves searched **before** Q at node N receive a negative adjustment ("malus"). The rationale: those earlier moves were tried and failed to cause a cutoff; they are evidenced as weak in this context and should rank lower next time.

### Key Facts

- CPW documents the pattern: "apply a penalty to all quiet moves that were previously searched. This not only prevents saturated history values, but also gives unpromising moves negative history." [(CPW History Heuristic)](https://www.chessprogramming.org/History_Heuristic)
- The standard loop:
  ```
  bonus = depth * depth   // (or 300 * depth - 250 in modern form)
  history.update(bestMove, +bonus)
  for move in quietsSearched:
      history.update(move, -bonus)
  ```
  The malus magnitude matches the bonus magnitude. [(CPW History Heuristic)](https://www.chessprogramming.org/History_Heuristic)
- MadChess 3.0 Beta Build 084 implemented bidirectional (bonus + malus) history and measured +28 Elo on bullet. The combined effect accounted for the gain. [(MadChess Build 084)](https://www.madchess.net/2018/12/03/madchess-3-0-beta-build-84-history-heuristics/)
- The malus also mitigates saturation: without it, all quiet moves at frequently-visited positions trend toward MAX_HISTORY; the penalty for non-cutters keeps scores spread.
- **Negative history values are intentional** and require a signed integer type. MadChess reported values down to -56 in testing. Using an unsigned type is a gotcha. [(MadChess Build 084)](https://www.madchess.net/2018/12/03/madchess-3-0-beta-build-84-history-heuristics/)
- Requires maintaining a list of "quiets searched so far at this node" — typically a small stack allocated per node frame (max ~218 moves, typical much less). This is the implementation overhead.
- With the gravity formula, the malus is naturally expressed as a negative bonus input; no separate logic needed. With plain `+= depth * depth`, the malus requires an explicit subtraction with a clamp to avoid exceeding `[-MAX_HISTORY, +MAX_HISTORY]`.

### Recommendation

**Implement history malus in M4.C.** It is part of the canonical CPW description, widely confirmed to improve ordering, and prevents saturation for free. The implementation cost is a small per-node `Vec<Move>` (or fixed-size array) of quiets already searched.

---

## 4. Aging vs Saturation

### Strategies

| Strategy | Description | Pros | Cons |
|---|---|---|---|
| Saturate (clamp) | Hard-clamp entries to `[-MAX, +MAX]` on write | Simple; with gravity formula, saturation is implicit | Stale good moves stay at MAX indefinitely |
| Halve-on-threshold | When any entry exceeds threshold T, divide all entries by 2 | Keeps values bounded and fresh | O(N) scan on threshold hit; threshold choice is a tuning parameter |
| Halve-on-overflow | Divide all by 2 when an entry would overflow | Same as above but triggered by single-entry overflow | Frequency of halving depends on depth and position count |
| Halve-periodic | Divide all by 2 every K nodes | Keeps values fresh automatically | K choice is a tuning parameter; extra work proportional to search size |
| Clear between ID iterations | Zero all entries before each ID iteration | No stale data; simplest | **Destroys cross-iteration benefit; table is empty for depth 1 of every iteration** |
| Clear per-`go` | Zero all on each root search | Fresh per position | Loses game-level learning |
| Clear on `ucinewgame` | Zero only on new game | Maximizes retention | Risk of cross-game contamination |

### Key Facts

- Forum consensus from t=24522: clearing between ID iterations "loses its effectiveness" because shallow iterations generate the statistics deep iterations rely on. [(TalkChess t=24522)](https://talkchess.com/viewtopic.php?t=24522)
- Bob Hyatt (Crafty): dividing all counters by 2 periodically is the simplest viable aging strategy; inactive entries trend to zero; active entries survive. [(TalkChess t=24522)](https://talkchess.com/viewtopic.php?t=24522)
- Threshold-based halving: Harald Johnsen divides by 2 when good_hit values exceed 0x00100000; Kempelen divides by 4 when any entry exceeds 8191. Both report stable behavior. [(TalkChess t=24522)](https://talkchess.com/viewtopic.php?t=24522)
- History gravity formula makes explicit aging mostly unnecessary: the multiplicative damper in the update formula naturally prevents new bonuses from piling on top of already-high values. It converges to an equilibrium rather than drifting to saturation.
- Rotor engine: multiplies all entries by 0.5 every 10,000 nodes. Node-count-based decay is position-independent. [(TalkChess t=24522)](https://talkchess.com/viewtopic.php?t=24522)
- Clearing on `ucinewgame` boundary is the consensus: "On ucinewgame the engine does once-per-game init like resetting the TT." History follows the same reset contract. [(CPW History Heuristic)](https://www.chessprogramming.org/History_Heuristic)

### Recommendation for M4.C

**Saturate at `±MAX_HISTORY` with explicit clamp on write.** Formula:

```rust
fn update_history(table: &mut i16, bonus: i16, max: i16) {
    let clamped = bonus.clamp(-max, max);
    *table += clamped - (*table as i32 * clamped.abs() as i32 / max as i32) as i16;
}
```

This is the gravity formula variant; it implicitly bounds values. If the gravity formula feels like too much complexity for M4.C, use:

```rust
fn update_history(table: &mut i16, bonus: i16, max: i16) {
    *table = (*table + bonus).clamp(-max, max);
}
```

and add halve-on-threshold as a separate follow-up if saturation is observed.

**Do not clear between ID iterations.** Cross-iteration carry-over is the primary benefit of history: the table built at depth D helps order moves at depth D+1, and so on. Clearing would negate this.

---

## 5. Inter-Iteration and Inter-Search Discipline

### Literature Findings

- Within a `go`: history accumulates across all ID iterations. The CPW describes history as accumulating "irrespectively from the position" — it is position-independent by design. Clear between iterations defeats the purpose. [(CPW History Heuristic)](https://www.chessprogramming.org/History_Heuristic)
- Between `go` commands (within a game): the literature is split, but the weight of evidence supports **persisting** the history table across `go` commands within a game, treating it like a learned prior. The same moves (e.g., a recurring thematic tactic) will keep ranking well. The staleness cost is low because the gravity formula or threshold-halving keeps values from drifting.
- On `ucinewgame`: clear history (zero all entries). This is consistent with the TT's `ucinewgame` contract. Forum consensus: engines clear history on `ucinewgame`. [(TalkChess t=62676)](https://talkchess.com/viewtopic.php?t=62676)
- For **bench determinism**: clear history between bench positions — the same contract as the TT reset. The M4.A research already identifies this in the per-game-state inventory table. [(TalkChess t=77569)](https://talkchess.com/viewtopic.php?t=77569)
- MadChess reported that aging history "by a percentage" during iterative deepening prevents "moves from shallow searches from having too much influence." This is the halve-on-iteration variant. It is one valid point on the spectrum; the gravity formula addresses the same concern implicitly. [(MadChess Build 040)](https://www.madchess.net/2015/01/04/madchess-2-0-beta-build-40-history-heuristic-late-move-reductions/)

### Summary Table

| Boundary | Policy | Rationale |
|---|---|---|
| Between ID iterations (within `go`) | Persist (do NOT clear) | Cross-iteration carry-over is the value of history |
| Between `go` commands (within game) | Persist (optional: halve-on-threshold or gravity keeps it bounded) | Reuse learned ordering; staleness is bounded by aging |
| `ucinewgame` | Clear (zero all entries) | Fresh game; avoids cross-game contamination |
| Bench position boundary | Clear (same as `ucinewgame`) | Determinism; follows the M4.A `reset_for_new_game()` contract |

---

## 6. Score Scale and Datatype

### Key Facts

- History scores must be **signed**: the malus produces negative values, which are semantically meaningful (this move has been a weak choice relative to the rest). Using unsigned type is a common gotcha. [(MadChess Build 084)](https://www.madchess.net/2018/12/03/madchess-3-0-beta-build-84-history-heuristics/)
- `i16` is sufficient: with `MAX_HISTORY = 16384` and saturation/gravity, values stay within `[-16384, 16384]` ⊂ `[-32767, 32767]`. An `i16` table is 2 bytes per entry; `8192 entries × 2 = 16 KiB` for the full `[2][64][64]` table.
- `i32` is a safe alternative (no overflow risk on arithmetic temporaries), at 4× the memory (64 KiB). Given a 16 KiB history table fits in L1 cache on Apple Silicon (192 KiB L1d on M4), the `i16` table is preferable.
- The `depth * depth` formula: at max depth 63 (project's `MAX_PLY = 64`), a single increment is `63 * 63 = 3969`. Without saturation/aging this would overflow `i16` (max 32767) after `32767 / 3969 ≈ 8` cumulative increments for the same move. The gravity formula or explicit clamp at `MAX_HISTORY = 16384` avoids this. With the raw `+= depth * depth` approach, saturation at `16384` happens after `16384 / 3969 ≈ 4` deep-search cutoffs — fast but fine because the gravity formula keeps the value from compounding further.
- Exponential `1 << depth` would overflow `i16` at depth 15 (`1 << 15 = 32768`). Do not use for `i16` tables.

### Killer + History Score Layering

Killers and history are **discrete tiers**, not values on a shared arithmetic scale:

- Killers are identified by equality check (`move == killer_1` or `move == killer_2`), not by a score.
- In move ordering, killers are assigned a fixed large bonus score (e.g., `MVV_LVA_OFFSET - 10` and `- 20`) to place them immediately below captures.
- History scores are applied only to moves that are **not** the TT move and **not** killers. They sort the remaining quiet pool among themselves.
- There is no need to unify killers and history onto a shared numeric axis; the pipeline stage separation is the design. [(Rustic chess engine docs)](https://rustic-chess.org/search/ordering/killers.html) [(TalkChess t=76601)](https://talkchess.com/viewtopic.php?t=76601)

---

## 7. Position in the Move-Ordering Pipeline

### Standard Pipeline (Post-M4.B)

```
1. TT move (if in legal list)
2. Winning captures (MVV-LVA score > 0, or SEE > 0)
3. Equal captures (MVV-LVA score == 0)
4. Killer 1 (if legal, not already tried as TT move or capture)
5. Killer 2 (if legal, not already tried)
6. Quiet moves scored by history (descending) — non-killers, non-TT-move
7. Losing captures (MVV-LVA score < 0)
```

Some engines place losing captures between killers and history; the placement of losing captures is engine-specific and measurable by SPRT. For M4.C, placing losing captures last (step 7) is the simpler starting point. [(CPW Move Ordering)](https://www.chessprogramming.org/Move_Ordering) [(TalkChess t=76601)](https://talkchess.com/viewtopic.php?t=76601)

### Key Invariants

- The TT move must **not** receive a history update as if it were a "quiet scored by history." It is tried first and, if it produces a cutoff, may then update history (see §8). If it does not cut, it must not receive a penalty on the second search through quiets — it is not in the quiet pool.
- Killers **must not** be double-counted in the quiet pool. If a killer is also a capture, it should appear as a capture (step 2/3). If a killer is also the TT move, it should appear at step 1 and not again as a killer. [(TalkChess t=24920)](https://talkchess.com/viewtopic.php?t=24920) [(TalkChess t=76601)](https://talkchess.com/viewtopic.php?t=76601)
- History heuristic is a **quiet-only** technique in M4.C. Captures are ordered by MVV-LVA (and later SEE at M5). The two schemes do not mix for M4.C scope. Applied naively to captures, history loses 20–50 Elo in one reported test. [(TalkChess t=77152)](https://talkchess.com/viewtopic.php?t=77152)

---

## 8. GHI / TT Interaction

### The Question

Should history be updated when a TT probe returns early (beta cutoff from a cached score), rather than from a real search?

### Literature Position

- **History tracks search behavior, not position evaluations.** A TT-hit early-return is a skip of the actual move search at this node — no moves are tried, no cutoff-move is identified at this depth. There is no "refuting quiet move" to credit. [(CPW History Heuristic)](https://www.chessprogramming.org/History_Heuristic)
- The CPW examples show history updating inside the move loop, **after** the move is searched and `score >= beta` is detected — not at the TT early-return site.
- Standard practice: **do not update history on a TT early-return**. The best-move from the TT entry is used for move ordering, but the history tables are not modified.
- If the TT move happens to be a quiet move that causes a beta cutoff during the **actual** move search at a node that wasn't cut by TT, it does receive a history bonus through the normal move-loop path. This is correct: it is a real refutation observed at this node.
- An open question (flagged as such by the author of this report): some engines may update history on TT-hit cutoffs as an approximation to "this quiet move was the right choice at this node." No forum thread or CPW article explicitly recommends this; the safer default is to update only on real-search cutoffs.

### Recommendation

**Do not update history on TT early-return.** History updates fire only in the move loop when `score >= beta` is observed on a move that was actually searched at the current node.

---

## 9. Empirical Elo Gains

### Findings

- History heuristic (combined with LMR) contributed **+50 Elo** to MadChess 2.0 Beta bullet; the individual history contribution was not separated from LMR in that report. [(MadChess Build 040)](https://www.madchess.net/2015/01/04/madchess-2-0-beta-build-40-history-heuristic-late-move-reductions/)
- Adding history malus contributed **+28 Elo** to MadChess 3.0 Beta (separate from the base history). [(MadChess Build 084)](https://www.madchess.net/2018/12/03/madchess-3-0-beta-build-84-history-heuristics/)
- A forum developer reported `depth * depth + depth - 1` gaining **~10 Elo** over plain `depth * depth`. [(TalkChess t=81716)](https://talkchess.com/viewtopic.php?t=81716)
- Counter moves alone: **10+ Elo** in several reports. [(TalkChess t=83257)](https://talkchess.com/viewtopic.php?t=83257)
- Bob Hyatt (Crafty): removed history entirely from Crafty at some point with no measurable strength loss — engine-specific; not a general finding. [(TalkChess t=25118)](https://talkchess.com/viewtopic.php?t=25118&start=10)
- The Rustic engine book reports killer moves alone give ~35 Elo net (self-play inflated to ~56). History adds on top of killers. [(Rustic engine docs)](https://rustic-chess.org/progress/playing_strength.html)

### Expected Range for This Project

At ~2114 Elo (M3.F anchor), the following is a calibrated estimate:

- **History alone (over TT + killers):** +15–50 Elo in self-play SPRT at `tc=10+0.1`. Wide range because: (a) history gain is depth-dependent (more valuable at deeper search), (b) the project's classical eval means quiet-move discrimination matters more, (c) self-play Elo numbers inflate vs cross-engine results.
- Using `elo0=0, elo1=5` SPRT bounds per `docs/workflow.md`, history should produce a clear H1 accept. If it does not within 400 games, suspect: wrong reset boundary (clearing between iterations), unsigned counter overflow (malus silently wrapping to MAX_U16), or double-counting killers in the quiet pool.

---

## 10. Pitfalls and Gotchas

| Pitfall | Description | Mitigation |
|---|---|---|
| Unsigned counter type | If history table uses `u16`, negative malus wraps to MAX_U16 (~65535), making penalized moves appear excellent. | Use `i16` or `i32`. |
| Signed overflow without clamp | `+= depth * depth` at depth 63 adds 3969 per cutoff; without saturation, `i16` overflows after ~8 hits for the same move at that depth. | Clamp to `±MAX_HISTORY` after every update, or use gravity formula. |
| Clearing between ID iterations | History table is empty at the start of each iteration; depth-2 statistics don't feed depth-3. | Clear only on `ucinewgame` and bench-position boundaries. |
| Double-counting TT move and killers | If the TT move or a killer move is also in the quiet pool, it receives a history malus when it fails to cut (it already cut via a different code path). | Exclude TT move and killers from the quiet pool when applying malus. |
| Updating history on TT-hit | Updating history from a TT early-return mixes cached search results with live search statistics. | Update history only inside the move loop when an actual search produces `score >= beta`. |
| History applied to captures | Applying quiet-move history to captures loses 20–50 Elo in reported tests. | Gate the history update on `is_quiet_move(mv)`. Capture history is a separate M5+ technique with its own table structure. |
| Exponential increment `1 << depth` | Overflows `i16` at depth 15, `i32` at depth 31. | Do not use. Stick with `depth * depth`. |
| Incrementing at all nodes including PV nodes | History is most meaningful at cut-nodes; incrementing at PV nodes also is not harmful but adds noise. Standard practice is to update on any `score >= beta` regardless of node type — the condition itself is the gate. | No special action needed; `score >= beta` at any node type is the correct condition. |
| Penalizing killers | Killers are non-captures tested before the quiet pool. If the killer move does not cut and a later quiet does, the killer receives a malus. This is correct — the killer was "tried and failed" at this node. No special case needed. |
| Forgetting `bench` reset | History from bench position N pollutes position N+1, breaking node-count determinism. | Call `reset_for_new_game()` between bench positions as per M4.A contract. |

---

## 11. Capture History (Out of M4.C Scope)

Standard history applies only to quiet moves. Captures are ordered by MVV-LVA (and later SEE). Forum consensus: applying the standard quiet-move history table to captures is harmful, losing 20–50 Elo. [(TalkChess t=77152)](https://talkchess.com/viewtopic.php?t=77152)

**Capture history** is a separate technique: a table indexed by `[moved piece][to][captured piece]` (or similar), tracking which captures have been good refutations historically. It is a replacement for or complement to MVV-LVA in pure-capture ordering. This is M5+ scope.

---

## 12. Future Extensions (M5+ Scope)

### Counter-Move Heuristic

- Indexed by `[piece_of_prev_move][to_of_prev_move]` → stores the move that best refuted it.
- Works alongside history; adds context-sensitivity (same quiet move is scored higher when responding to a specific opponent move).
- Gain: 10+ Elo in several reports; after 1-ply continuation history is added, the counter-move table becomes partially redundant. [(CPW Countermove Heuristic)](https://www.chessprogramming.org/Countermove_Heuristic) [(TalkChess t=83257)](https://talkchess.com/viewtopic.php?t=83257)
- Table size: 12 × 64 = 768 entries × 2 colors = negligible memory.

### Continuation History

- Generalizes counter-move history: 1-ply continuation history (CHS-1) is "what move did side-to-move play 1 ply ago?" → score the current move accordingly.
- 2-ply CHS-2 = "what did the same side play 2 plies ago?" (follow-up history).
- Table: indexed by `[previous_piece][previous_to][current_piece][current_to]` — 12 × 64 × 12 × 64 ≈ 590K entries per level. This is larger than the simple history table by a factor of ~72.
- Combined 1-ply + 2-ply continuation history is the approach in Stockfish 7+, originally dubbed "Counter Moves History" (Bill Henry 2015, implemented by Geschwentner).
- After 1-ply continuation history is in place, the plain counter-move table is "kinda superfluous." [(TalkChess t=83257)](https://talkchess.com/viewtopic.php?t=83257)
- Elo gain vs history-only: rough estimate 15–30 Elo across the combination. The implementation complexity is significantly higher. M5+ milestone.

### History Gravity Formula (Deferred from M4.C)

The gravity formula documented in §2 is the production-quality version. M4.C can start with plain `+= depth * depth` plus explicit clamp; migrating to gravity at M5 is a one-function swap.

---

## Source List

- [CPW History Heuristic](https://www.chessprogramming.org/History_Heuristic)
- [CPW Butterfly Boards](https://www.chessprogramming.org/Butterfly_Boards)
- [CPW Relative History Heuristic](https://www.chessprogramming.org/Relative_History_Heuristic)
- [CPW Move Ordering](https://www.chessprogramming.org/Move_Ordering)
- [CPW Killer Heuristic](https://www.chessprogramming.org/Killer_Heuristic)
- [CPW Countermove Heuristic](https://www.chessprogramming.org/Countermove_Heuristic)
- [TalkChess: History Heuristic — when to reset counts (t=24522)](https://talkchess.com/viewtopic.php?t=24522)
- [TalkChess: Killer moves and history heuristic table (t=24920)](https://talkchess.com/viewtopic.php?t=24920)
- [TalkChess: Relative history heuristic page 2 (t=25118)](https://talkchess.com/viewtopic.php?t=25118&start=10)
- [TalkChess: Captures and history heuristic (t=27598)](https://talkchess.com/viewtopic.php?t=27598)
- [TalkChess: Improved history heuristic (t=50512)](https://talkchess.com/viewtopic.php?t=50512)
- [TalkChess: Countermove heuristic (t=62676)](https://talkchess.com/viewtopic.php?t=62676)
- [TalkChess: How much Elo from killer moves? (t=77734)](https://talkchess.com/viewtopic.php?t=77734)
- [TalkChess: Move ordering heuristics for captures (t=77152)](https://talkchess.com/viewtopic.php?t=77152)
- [TalkChess: Killer move heuristic (t=79854)](https://talkchess.com/viewtopic.php?t=79854)
- [TalkChess: Move ordering discussion (t=81716)](https://talkchess.com/viewtopic.php?t=81716)
- [TalkChess: Question about countermoves (t=83257)](https://talkchess.com/viewtopic.php?t=83257)
- [TalkChess: Continuation history implementation (t=84435)](https://talkchess.com/viewtopic.php?t=84435)
- [TalkChess: Sorting moves during move ordering (t=76491)](https://talkchess.com/viewtopic.php?t=76491)
- [TalkChess: Improving move ordering question (t=76601)](https://talkchess.com/viewtopic.php?t=76601)
- [TalkChess: When to clear the transposition table (t=77569)](https://talkchess.com/viewtopic.php?t=77569)
- [MadChess Build 084 — history heuristics](https://www.madchess.net/2018/12/03/madchess-3-0-beta-build-84-history-heuristics/)
- [MadChess Build 040 — history + LMR](https://www.madchess.net/2015/01/04/madchess-2-0-beta-build-40-history-heuristic-late-move-reductions/)
- [Mediocre Chess: advanced killer and history guide](http://mediocrechess.blogspot.com/2007/01/guide-trying-out-advanced-killer-moves.html)
- [Rustic chess engine — killer moves ordering](https://rustic-chess.org/search/ordering/killers.html)
- [Rustic chess engine — playing strength](https://rustic-chess.org/progress/playing_strength.html)
- [Beowulf chess theory — history heuristic](http://www.frayn.net/beowulf/theory.html)
- [likeawizard blog — history heuristic frustrations](https://lichess.org/@/likeawizard/blog/the-highly-frustrating-side-of-chess-engine-development/s6jHNBcd)
