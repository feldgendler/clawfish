# Prior-Art Research: Qsearch Participation in the Transposition Table (M5.F)

Research brief for M5.F — adding TT probe and store inside `qsearch`. This phase closes the +5–15 Elo gap acknowledged in ADR-0018 §6 when M4.A shipped TT support but deliberately scoped qsearch out. M5.E landed first to ensure the TT does not memoize qsearch holes; M5.F is the immediate next phase.

Per ADR-0003, no third-party engine source code was read. Sources: Chess Programming Wiki, TalkChess forums, rec.games.chess.computer, Mediocre Chess blog, Bruce Moreland's "Programming a Computer to Play Chess", open-chess.org discussions, and the Stockfish commit log (commit messages and diff summaries only, not source bodies).

---

## 1. Probe-Only vs Probe-and-Store

### What "probe-only" means

Probe-only at qsearch means: on entry to each qsearch frame, consult the TT; if a matching entry with a compatible bound exists, return the stored score immediately (or use it for ordering only at PV nodes). Never write qsearch results back to the TT. This is the "probe-but-don't-store" intermediate mentioned in ADR-0018 §6.

### What "probe-and-store" means

Full qsearch-in-TT: both read and write. On entry, probe and potentially shortcut. On exit (after the move loop or any early return), write the result back with an appropriate depth tag and bound type.

### What the literature says about each

| Mode | Engine report | Elo delta | Notes |
|---|---|---|---|
| Full probe-and-store | jd1 (TalkChess t=47373) | +25 Elo @ 5s+100ms / 32 MB hash | 4000-game test |
| Full probe-and-store | MartinBryant (TalkChess p=892662) | +44 Elo @ 1280 games, LOS 100% | Node count −9%; 23% of qsearch nodes TT-hit |
| Full probe-and-store | lucasart / DiscoCheck (TalkChess p=892662) | Clear regression on removal | Not quantified in available excerpt |
| Probe-only | AndrewGrant / Ethereal (TalkChess p=892662) | Tested extensively; preferred probe-not-store | "Worth maybe two elo" with TT moves in QS |
| No qsearch TT | Bob Hyatt / Crafty (TalkChess t=47373) | Wash (+0 Elo) | 10% bigger tree, 10% faster; perfectly offset |
| Separate eval cache | Jon Dart / Arasan (TalkChess t=47373) | −10 Elo when switching to main TT | Cache stores static score + best move only |
| Full probe-and-store | Diep (rec.games.chess.computer) | −20% time-to-depth improvement | Reduced node counts substantially |
| Fast-eval engine | Joost Buijs / abulmo2-Dumb (TalkChess p=892662) | Counter-productive | With very fast eval, memory traffic hurts |
| Slow-eval engine (NNUE) | abulmo2-Amoeba (TalkChess p=892662) | Strongly positive | Eval cost dominates; TT savings dominate |

**Key finding.** The benefit is heavily engine-dependent. The single biggest predictor is evaluation speed: a cheap classical PeSTO eval makes the probe-overhead-vs-saved-eval tradeoff tighter than for NNUE. Our engine uses classical PeSTO, putting us closer to Crafty's "wash" territory than the NNUE engines. The +25–44 Elo figures from the forum come from engines with different eval costs and different hash pressures; our realistic expectation is in the +5–15 Elo range stated in ADR-0018 §6.

**Probe-only as an intermediate.** ADR-0018 §6 noted the "probe-but-don't-store" pattern. The literature confirms this pattern exists (Tord Romstad mentioned it in rec.games.chess.computer). Its practical benefit is a subset of full probe-and-store: it catches positions where a prior negamax search left a deep entry at a qsearch-reachable position, without polluting the table with shallow qsearch results. It does not help positions that are reached only via qsearch interior nodes (the main source of redundancy). Andrew Grant's probe-only stance in Ethereal suggests it is the conservative choice; however his engine is strong enough that the marginal Elo is small. For a weaker classical engine the store side may add more. The clean design for M5.F is full probe-and-store, matching the mainstream literature consensus.

**Sources:** [TalkChess t=47373](https://talkchess.com/viewtopic.php?t=47373), [TalkChess p=892662](https://talkchess.com/viewtopic.php?p=892662), [rec.games.chess.computer QS hash](https://rec.games.chess.computer.narkive.com/QBiQWYWt/hash-table-and-quiescence-search), [rec.games.chess.computer QS TT](https://rec.games.chess.computer.narkive.com/b8bSE4W5/transposition-table-for-quiescence-search)

---

## 2. Depth-Field Convention for Qsearch Entries

### The problem

ADR-0018 §3 stores depth as `u8`. The current `is_empty()` check is:

```rust
self.key == 0 && self.depth == 0 && self.age_and_bound == 0 && self.best_move == 0
```

The store-side `debug_assert!(data.depth >= 1, ...)` pins the invariant that no live entry has `depth == 0`. This was forward-planned in `src/tt.rs` line 224: *"a future qsearch-in-TT (M5) adopts a different empty-slot discriminator before going to depth=0."* Three principal options exist:

### Options

| Option | Mechanism | empty-slot discriminator | Implications |
|---|---|---|---|
| A — depth=0 for qsearch | Store all qsearch entries with `depth = 0`; change empty-slot test to `age_and_bound == 0` (an empty slot has never had its bound or age packed in) | `age_and_bound == 0` distinguishes empty from live (empty never has age or bound written) | Simple; single-byte discriminator; qsearch entries never overwrite negamax entries under depth-preferred (negamax always has `depth >= 1`); qsearch probes from negamax (whose probe test is `entry.depth >= depth`) silently skip them (0 < any negamax depth) |
| B — i8 signed depth, -1 for qsearch | Widen depth field to `i8`; use `depth = -1` for qsearch; empty-slot test stays `depth == 0` | `depth == 0` | Canonical across many cited engines (jdart: "negative depth") but requires changing the field type from `u8` to `i8` — a wider migration |
| C — depth=1 for qsearch | Store all qsearch entries with `depth = 1`; no change to discriminator | unchanged | Requires a depth-comparison guard (`entry.depth > 1`) at the negamax probe site to suppress qsearch-sourced cutoffs; risk of off-by-one in probe logic |

### Recommendation

**Option A (depth=0, `age_and_bound != 0` as discriminator)** is the cleanest for this codebase. Rationale:

- The forward-planning comment in `src/tt.rs` explicitly anticipates it.
- `age_and_bound == 0` is a reliable discriminator: the empty default has both age=0 and bound=Exact (bit value 0), but in practice no live entry ever has generation=0 after the first `new_search()` call. Even on the very first search (gen=0) an entry will have `age_and_bound != 0` only if its bound is non-zero. This creates a subtle gap: a gen-0 Exact entry would have `age_and_bound == 0` — the same as empty. To close this cleanly, the discriminator should be `key != 0` (the Zobrist key of the real start position is non-zero with overwhelming probability). The canonical fix is: **use `key != 0` as the sole empty-slot test**, which was always sufficient and avoids the depth coupling entirely.
- Alternatively, use `depth != 0 || age_and_bound != 0`: a qsearch entry (depth=0) must have a non-zero `age_and_bound` (age packed in), so any live depth-0 entry passes this disjunction; an empty slot has both zero.
- Depth-preferred replacement works naturally: negamax entries always have `depth >= 1`; they will never be overwritten by a depth-0 qsearch entry under the `old.depth <= data.depth` replacement rule. Qsearch entries are freely replaced by any negamax entry.
- The probe-side depth comparison in negamax (`entry.depth as u32 >= depth` where `depth >= 1`) naturally excludes depth-0 qsearch entries from causing negamax cutoffs — which is exactly correct (a qsearch entry searched only captures/promotions; its score is not valid as a full-width negamax result at any depth).

**Corner case: using a depth-0 qsearch TT entry to trigger a cutoff inside qsearch itself.** When qsearch probes and finds a depth-0 entry, the depth comparison is `0 >= 0` — true. This is intended: a qsearch entry is as deep as any qsearch probe needs.

**The "depth tiers" analogy.** HGM noted (TalkChess t=47373) that "with shallowest-of-4 replacement, depth-0 entries will never be more than 25% of the table." Our engine uses depth-preferred single-slot tables (direct-mapped), not buckets. Depth-0 entries *will* be overwritten by any negamax entry at the same index — so table pollution is naturally self-limiting. Qsearch entries only survive when negamax never stores at the same index with a deeper result. In practice this is common (many positions are only ever reached by qsearch).

**Sources:** [TalkChess t=47373 p.5](https://talkchess.com/viewtopic.php?t=47373&start=40), [rec.games.chess.computer TT for QS](https://rec.games.chess.computer.narkive.com/b8bSE4W5/transposition-table-for-quiescence-search)

---

## 3. Bound Semantics in Fail-Soft Qsearch

Bound classification uses the standard formula from HGM (TalkChess t=63251):

```
bound = (best > original_alpha) * LOWER + (best < beta) * UPPER
```

Which resolves to: Lower if best > original\_alpha, Upper if best < beta, Exact if both (i.e., original\_alpha < best < beta). This applies uniformly to qsearch with the following specific cases:

### Stand-pat >= beta (fail-high, !in\_check arm)

Returns stand-pat immediately. **Lower bound.** The stand-pat score is the best known lower bound; a capture could have improved it further. Storing `score = stand_pat, depth = 0, bound = Lower` is correct; any future probe that reaches this node when beta is lower may save the qsearch entirely.

### Stand-pat >= alpha (alpha raised but no cutoff, !in\_check)

Move loop runs. If no capture beats the raised alpha and move loop exits, we return `best = stand_pat` (best was never raised above stand_pat). Because `best == stand_pat > original_alpha` but `best < beta`, this classifies as **Exact** under the formula — but see §3 gotcha below.

### All captures fail low (best == stand_pat < original\_alpha, !in\_check)

Move loop ran but nothing improved the stand-pat. `best = stand_pat`, `best < original_alpha` and `best < beta`. **Upper bound.** The actual score may be higher (some unplayed quiet might improve, but qsearch doesn't search quiets). Storing as Upper is correct and conservative.

### Empty move list, !in\_check, M5.E #2 (true stalemate)

Returns 0. This is an **Exact** score — FIDE-definite. Can be stored as Exact, depth=0. No ambiguity.

### Empty move list, in\_check (mate at horizon)

Returns `-(MATE - ply)`. This is also definitively correct (all evasions exhausted = checkmate). **Exact** bound. However: the mate depth is ply-relative; the standard `score_to_tt` adjustment applies (`adjusted = score - ply` for negative mates per ADR-0018 §5). The adjusted value is stored; a future probe at a different ply applies `score_from_tt` symmetrically.

### M5.E #1: single-reply extension recursion result

The extension recurses and returns the negated child's score. This score has the same bound properties as any other qsearch recursion result — it is a full qsearch result at ply+1. The bound classification follows the same formula applied to the recursive result vs the current node's original_alpha and beta. Store at depth=0 (same depth as all qsearch frames).

### The "Exact in qsearch is dangerous" gotcha

**Stockfish commit 45e5e65 (Nov 2021)** removed Exact-bound storage in qsearch entirely, keeping only Upper and Lower. The reasoning: in qsearch, even when `best > original_alpha && best < beta`, the "exact" score was computed from stand-pat or a subset of moves (captures only). It does not reflect a full-width search over all legal moves. Calling it Exact overstates precision — a future negamax probe at this position might receive a score claiming Exact when the true minimax value (if all quiets were searched) could differ. In practice this causes the TT to short-circuit a future PV node with an incorrect Exact score.

The practical consequence for M5.F: **only Lower and Upper bounds should be stored in qsearch entries.** Never Exact. The rule becomes: if `best >= beta`, store Lower; otherwise store Upper. The "middle case" (best > original_alpha, best < beta) stores as Upper because qsearch cannot prove Exact with a restricted move set. This is conservative and consistent with Stockfish's tested conclusion.

**Worked examples:**

| Case | best vs original_alpha | best vs beta | Correct bound |
|---|---|---|---|
| Stand-pat >= beta | > original_alpha | >= beta | Lower |
| Capture improves, no cutoff | > original_alpha | < beta | Upper (NOT Exact) |
| Nothing improves stand-pat, stand-pat < alpha | < original_alpha | < beta | Upper |
| True stalemate (0 returned early) | — | — | Exact (special case: FIDE-definite) |
| Mate at horizon (in-check, all evasions fail) | — | — | Exact (special case: terminal node) |

The stalemate and mate-at-horizon cases are terminal nodes whose scores are definitively known regardless of the move restriction; Exact is sound for them.

**Sources:** [TalkChess t=63251](http://www.talkchess.com/forum3/viewtopic.php?t=63251), [Stockfish commit 45e5e65](https://github.com/official-stockfish/Stockfish/commit/45e5e65a28ce7e304c279fabf5f8a83cced73013), [CPW Quiescence Search](https://www.chessprogramming.org/Quiescence_Search)

---

## 4. PV-vs-Non-PV Cutoff Suppression in Qsearch

### Does qsearch have a meaningful PV?

In classical PVS, PV nodes are those with `beta - alpha > 1` (open window). Our current engine threads `is_pv: bool` through negamax, but qsearch does not receive `is_pv`. The question for M5.F is whether to pass it in and suppress TT cutoffs at qsearch PV nodes.

### The literature consensus

- CPW: *"In more advanced engines transposition table cutoffs are not performed on PV-Nodes."* This primarily targets the "short PV" problem — an Exact hit at a PV node returns immediately without searching all moves, producing a truncated `info pv` line. However, qsearch does not contribute to the displayed PV (qsearch results are not part of the triangular PV array).
- ADR-0018 §11 already suppresses cutoffs at PV nodes in negamax for this reason.
- In qsearch, the triangular PV array is not updated (per ADR-0018 §13 and our current code: `self.pv.clear_ply(ply)` is only in negamax, not qsearch). Therefore the "short PV" concern does not apply to qsearch.
- Andrew Grant reported (TalkChess t=69629) that using TT moves in qsearch "reduced total nodes while maintaining comparable or slightly greater search depth" — the TT benefit is real and suppressing it at "PV nodes" in qsearch would cost a small fraction of that benefit.

### Recommendation for M5.F

**Do not pass `is_pv` to qsearch.** Do not suppress TT cutoffs in qsearch. Apply full probe-and-cutoff semantics uniformly at all qsearch nodes. Rationale:

- The "short PV" motivation for PV-suppression does not apply (qsearch is not in the displayed PV).
- Qsearch TT cutoffs are the main mechanism by which repeated interior qsearch nodes are short-circuited; suppressing them at PV nodes would eliminate the efficiency gain for those nodes.
- Simpler code: no `is_pv` threading through qsearch's signature.

**Sources:** [CPW Transposition Table](https://www.chessprogramming.org/Transposition_Table), [ADR-0018 §11], [TalkChess t=69629](https://talkchess.com/viewtopic.php?t=69629)

---

## 5. TT-Move Ordering Inside Qsearch

### The core tension

The TT stores the "best move" from a prior search of a position. When that position was a negamax interior node (depth >= 1), the best move was likely a capture or a quiet move that caused a beta-cutoff. When it was a qsearch node (depth=0), the best move was whatever capture had the highest score. The question is: what should qsearch do with a stored best-move that might be a quiet?

### Options

| Approach | Behaviour | Risk |
|---|---|---|
| A — Ignore TT move for ordering, use only for cutoff | Skip the stored move in the qsearch loop entirely; use TT score only for early return | Loses move-ordering benefit of the TT |
| B — Accept TT move if it passes the qsearch move filter | Try TT move first if it is a capture/promo; skip if it is a quiet | Clean; avoids quiet chain problem; may miss a few ordering benefits when TT move is a quiet |
| C — Accept any TT move unconditionally | Allow a quiet TT move to enter the qsearch loop | **Long-chain problem** (see below) |
| D — Accept TT move only for cutoff probe, not for ordering | Probe → if bound allows early return, return; else proceed with no TT-move reordering | Eliminates all TT move ordering; leaves node count savings only from cutoffs |

**The long-chain problem (Option C gotcha).** Andrew Grant documented this clearly (TalkChess t=69629): if a TT entry stores a quiet move, and qsearch processes it and lands on another position whose TT entry also stores a quiet, a chain of quiet moves forms with no captures to terminate it. Grant reported that "a very long chain of TT moves" can form if none of the chain's nodes produce a cutoff. His data showed that despite this risk, node counts actually *decreased* with TT moves, suggesting the cutoffs usually fire. But the pathological case is real. Some engines guard against it by only admitting TT moves with depth above a threshold (e.g., depth >= -3), but Grant called this "arbitrary."

**Recommendation for M5.F.** Use **Option B**: accept the TT move for first-in-loop ordering only if it passes the qsearch move filter (capture/promo or in-check move). If the TT move is a quiet, skip it for ordering but still use the bound for the cutoff probe. This is the conservative choice that avoids the long-chain risk while preserving the primary ordering benefit in the common case (negamax stored a tactical move; that move is usually a capture in contested positions).

**Implementation detail.** The existing legality check (scan the legal-move list for membership — ADR-0018 §12) also applies in qsearch. Because qsearch generates the full legal-move list in M5.E's implementation before filtering, the membership check is available without extra cost. For ordering, the TT move should be moved to the front of `moves_vec` (post-filter) if it passes both the filter and the membership check.

**Sources:** [TalkChess t=69629](https://talkchess.com/viewtopic.php?t=69629), [TalkChess t=47373 p.5](https://talkchess.com/viewtopic.php?t=47373&start=40)

---

## 6. Mate-Score Discipline Reuse

### ADR-0018 §5 applies uniformly

`score_to_tt` and `score_from_tt` are ply-relative adjustments that apply regardless of whether the score originated in negamax or qsearch. The threshold `|score| > MATE_IN_MAX_PLY` is the same. The store-side narrowing to `i16` and the `debug_assert!` are also unchanged.

### Qsearch-specific mate cases

- **In-check arm returning `-(MATE - ply)`.** This is a positive-distance mated score (the current side is mated). `score_to_tt(-(MATE - ply), ply) = -(MATE - ply) - ply = -MATE`. Stored as `-MATE = -30000`. A future probe at ply P applies `score_from_tt(-30000, P) = -30000 + P = -(MATE - P)`, meaning "mated in P plies from here." This is exactly right — the absolute mate node is the same, just viewed from a different ply. The existing discipline handles it without modification.
- **Single-reply extension at ply N returning a mate score.** The recursed child is at ply N+1. The score returned to the M5.E #1 extension code is already ply-adjusted by the recursion (the child called `score_to_tt` on its own mate result). The M5.F TT store at the *current* qsearch frame uses `score_to_tt(best, ply)` as usual. This double-composition is correct: `score_to_tt(score_from_tt(inner, ply+1), ply)` correctly encodes the outer node's ply-relative distance.
- **Stalemate score of 0.** `score_to_tt(0, ply) = 0`; `score_from_tt(0, P) = 0`. No adjustment needed; 0 is outside the mate-score threshold. Store as Exact.

**No new mate-discipline code is needed for M5.F.** Reuse the existing helpers as-is.

**Sources:** [ADR-0018 §5](../decisions/0018-transposition-table.md), [CPW Score](https://www.chessprogramming.org/Score)

---

## 7. GHI / Repetition Concerns at Qsearch

### ADR-0027 §7 status

M5.E deliberately preserved M3.D's "qsearch does NOT consult repetition / 50-move helpers." This was an acknowledged deferred item.

### Does qsearch-in-TT worsen GHI?

**Yes, marginally, but the literature accepts it.** The mechanism: a qsearch TT entry stores a score computed along path A where the halfmove clock was, say, 20. Path B reaches the same position with halfmove clock 80 (just below the 50-move trigger). The stored score doesn't account for the imminent 50-move draw. In the negamax layer this GHI already exists (ADR-0018 §10); qsearch adds more entries with the same property.

However, the practical consequence is small:

1. Qsearch entries have `depth = 0`. The negamax probe from the main search checks `entry.depth >= depth` (where depth >= 1), so qsearch entries cannot cause a cutoff in the main search. The GHI manifests only within a qsearch subtree that hits a qsearch TT entry — a much smaller exposure than the negamax case.
2. Kishimoto & Müller's 2004 formal GHI solution exists but is widely considered over-engineering for practical engines. The CPW page and multiple TalkChess threads confirm that "live with it" is the industry consensus. Adding Option 2 (suppress probe/store when `halfmove_clock > 80`) remains a cheap fallback if SPRT data shows unusual draw-misevaluation.
3. The M3.D deliberate skip (no repetition check in qsearch) means qsearch never returns 0 for a draw-by-repetition; the stored score does not encode a false draw. The risk is the inverse: a position that should be a draw (by repetition on the qsearch path) is instead evaluated as a non-draw. This is a pre-existing imprecision, not newly introduced by qsearch-in-TT.

**Recommendation for M5.F.** Do not add repetition checks inside qsearch as part of this phase. Maintain ADR-0027 §7's preserved skip. The GHI exposure from qsearch TT entries is bounded to within-qsearch subtrees and is consistent with the main-search GHI already accepted in ADR-0018 §10.

**Sources:** [CPW Graph History Interaction](https://www.chessprogramming.org/Graph_History_Interaction), [Kishimoto & Müller 2004](https://cdn.aaai.org/AAAI/2004/AAAI04-102.pdf), [CPW Repetitions](https://www.chessprogramming.org/Repetitions)

---

## 8. Interaction with M5.E Corrections

### Single-reply extension (M5.E #1) recursion result

The extension recurses at `ply + 1` and returns the negated child score. This is a full qsearch result from a real recursion. From the perspective of the *current* qsearch frame, the result is the node's only score (no stand-pat, no move loop ran). Bound classification:

- `score > original_alpha && score < beta` → Upper (not Exact, per §3 gotcha).
- `score >= beta` → Lower.
- `score <= original_alpha` → Upper.

The recursive child stored its own TT entry (depth=0, computed at ply+1). The current frame stores its own TT entry (depth=0, computed at ply) after the extension returns. The two entries are at different Zobrist keys (different positions) and do not interfere.

**Should the extension result be stored at depth 0 or 1?** Depth 0. The qsearch depth convention (Option A above) assigns all qsearch entries depth=0, regardless of whether they contain a stand-pat evaluation, a capture-only move loop result, or a single-reply recursion. The depth field is a "tier" marker (qsearch vs negamax), not a measure of search effort within qsearch. A future negamax probe at `depth >= 1` will correctly skip the entry.

### True-stalemate detection (M5.E #2)

Returns 0. Store as Exact, depth=0, best_move=0. This is a definitively correct terminal score; an Exact bound does not overstate precision here. A future qsearch probe at the same position immediately returns 0 without generating moves — a cheap win.

### Stalemate-conditional rook/bishop under-promo (M5.E #3)

The under-promo loop may raise `best` and `alpha` above the queen-promo's stalemate score. The bound at the *parent* qsearch node (the one that searched the queen-promo and then tried the under-promos) is classified normally: Lower if `best >= beta`, Upper otherwise. The under-promo scores are folded into `best` like any other move score; no special bound case arises.

**Important:** The under-promo's *child* qsearch frames each store their own TT entries independently. No special treatment is needed at the synthesized-move level.

### MAX_PLY ceiling guard (M5.E #4)

Returns `evaluate(pos)` at `ply >= MAX_PLY - 1 && !in_check`. This is a stand-pat return (evaluated but no moves searched). Bound: Upper (we evaluated but did not confirm any lower bound via captures). Do not store this entry — or if stored, Upper bound at depth=0 is correct. Given that MAX_PLY entries are corner-case rare and represent an artificial ceiling, the simplest rule is to **skip the TT store when the MAX_PLY guard fires**, to avoid polluting the TT with a score that reflects an artificial truncation rather than a genuine stand-pat decision.

---

## 9. Empirical Elo Magnitude

### Literature claims

| Source | Claimed Elo | Conditions |
|---|---|---|
| CPW survey (cited in ADR-0018 §6) | +5–15 Elo | General range; multiple engines |
| jd1 / TalkChess t=47373 | +25 Elo | 5s+100ms, 32 MB hash, 4000 games |
| MartinBryant / TalkChess p=892662 | +44 Elo | 1280 games, 99% CI, LOS 100% |
| Andrew Grant / Ethereal TT-move in QS | ~+2 Elo | TT-moves-only sub-feature (not full probe-and-store) |
| Bob Hyatt / Crafty | 0 Elo | Offsetting tree-size and speed |
| Jon Dart / Arasan (switching from eval cache to main TT) | −10 Elo | Engine-specific; their eval cache may be superior |

### What to expect for this engine

The CPW +5–15 figure is the most conservative and most cited. Given:
- Classical PeSTO eval (fast; less to save than NNUE)
- NMP + LMR + RFP + FFP already pruning the main search heavily (fewer positions reach qsearch)
- M5.E correctness corrections already in place (qsearch interior nodes now return correct scores worth memoizing)

A realistic estimate is **+5–15 Elo** with a mode around +8–12. The Crafty "wash" outcome is possible if qsearch nodes are so fast that probe overhead offsets savings. Run a mixed-TC SPRT with `elo0=0, elo1=10` or similar.

**Sources:** [TalkChess t=47373](https://talkchess.com/viewtopic.php?t=47373), [TalkChess p=892662](https://talkchess.com/viewtopic.php?p=892662)

---

## 10. Implementation Snares

### (a) Qsearch Lower bound from stand-pat probed at negamax

**Scenario:** qsearch stores a `Lower` bound for a position (stand-pat >= beta). Negamax later probes the same position at depth=2. The probe test is `entry.depth as u32 >= depth` → `0 >= 2` → false. The qsearch entry is silently skipped. **No bug.** This is the depth-comparison guard described in §2.

**But:** if Option A is used with depth=0, the qsearch Lower entry will also be skipped by the negamax probe. The concern is not about soundness (the check prevents unsound cutoffs) but about whether qsearch should use negamax-stored Lower bounds for its own cutoffs. Answer: yes — the negamax entry has `depth >= 1`, and the qsearch depth-comparison `0 >= 1` fails, so qsearch cannot use the negamax entry for a cutoff probe either. This is a known limitation: qsearch probes only succeed when a same-depth qsearch entry is in the slot. The ordering benefit (TT move) still works regardless of depth comparison, because move extraction does not require a depth check.

**Mitigation:** To allow qsearch to benefit from negamax entries' *ordering* hint even without a score cutoff, split the TT probe into two steps: (1) extract `tt_move` regardless of depth comparison; (2) apply score cutoff only if `entry.depth == 0` (qsearch-tier). This maximises move-ordering benefit without allowing unsound cross-tier cutoffs.

### (b) TT-move legality re-validation cost in qsearch

Qsearch already generates the full legal-move list (for M5.E's `ml.len()` checks). The same membership scan used in negamax is available. Cost: O(N) where N is the legal-move count — typically 5–30 in qsearch positions. Negligible.

### (c) Pseudo-legal vs legal move encoding

Our engine uses legal-direct movegen (ADR-0007). All moves in the legal list are legal. The TT move stored by a prior search is also from a legal move at that position. With full 64-bit keys (ADR-0018 §2), collision-caused illegal moves are astronomically rare. The existing membership scan catches them.

### (d) Depth-comparison mismatch when probing qsearch entries from negamax

As noted above: with Option A (depth=0 for qsearch entries), the `entry.depth >= depth` test in negamax always fails for qsearch entries. This is intentional and correct. No bug; the comment in `src/tt.rs` line 222 already describes this as the intended future behavior.

### (e) Abort-during-qsearch storing a corrupt entry

The existing abort discipline in qsearch (check `self.aborted` after unmake, return 0 immediately) means the `best` accumulator may hold a partial result when the abort fires. The current code returns 0 and never stores to TT in qsearch. In M5.F, the TT store is at the *end* of qsearch (after the move loop and all early returns). The abort check fires before the store; the guard `if self.aborted { return 0; }` already in the move loop means the qsearch exits via the early-abort path, never reaching the end-of-function store. No extra guard is needed — the same pattern as negamax's `if self.aborted { return 0; }` at step 12 (`skip if self.aborted`, per ADR-0018 §13).

**Verify:** the store must come after all abort-propagation paths exit. In qsearch, early returns (stand-pat, stalemate, in-check mate, MAX_PLY guard, single-reply extension) should each optionally store before returning. The safest structure: collect the final `best` at any return point and optionally call a shared `qsearch_tt_store(key, best, original_alpha, beta, ply)` helper, with the `!self.aborted` guard inside.

### (f) TT pollution from frequent shallow qsearch entries

With depth=0 for all qsearch entries and depth-preferred replacement (`old.depth <= data.depth` → `0 <= 0` is true → always replace same-depth entries), every qsearch entry overwrites the previous depth-0 entry at the same index. Negamax entries (depth >= 1) always win over qsearch entries on replacement. The practical effect: the TT effectively has one tier for qsearch and one for negamax. The qsearch tier is a "most recently seen" cache at each index — useful for repeated qsearch visits within the same root search, but not retained across searches if the same index holds a negamax entry from the current or prior `go`.

This is an acceptable tradeoff. It means qsearch TT entries are more ephemeral than negamax entries, but that is consistent with their lower depth and higher frequency.

### (g) Best-move preservation interaction with depth-0 entries

ADR-0018 §7 preserves the old `best_move` when storing a new fail-low (Upper) entry with no best move, provided `old.key == key`. This applies to qsearch stores as well. If a prior qsearch entry at the same index for the same position had `best_move = Rxc5`, and the new store is a stand-pat fail-low with no best move, the old best_move is preserved. This is correct and valuable: a capture that was previously best-in-qsearch remains a good ordering hint.

---

## 11. Recommendation for M5.F

### Architecture decision

**Full probe-and-store.** Not probe-only. Rationale: the store side is the primary source of benefit for interior qsearch nodes (the main miss ADR-0018 §6 documented); probe-only only catches pre-existing negamax entries, not the qsearch-internal redundancy.

### Depth convention

**Option A: depth=0 for all qsearch entries.** Change `is_empty()` to use `key != 0` as the sole discriminant (or `depth != 0 || age_and_bound != 0` as the composite), removing the `depth == 0 → is_empty` collision risk. Remove the `debug_assert!(data.depth >= 1, ...)` in `store()` and add a separate qsearch-store helper or extend `store()` to accept depth=0 with the new discriminator.

### Bound classification

Use `Lower` when `best >= beta`; `Upper` in all other cases. Do not store `Exact` for qsearch results (except the terminal stalemate and mate-at-horizon cases, which are definitively correct). Skip the TT store when the MAX_PLY ceiling guard fires.

### TT-move ordering

Accept the TT move for first-in-loop ordering only if it passes the qsearch move filter. Use the bound for the early-return cutoff probe regardless of whether the move is a capture.

### PV suppression

Do not pass `is_pv` to qsearch. Apply full probe-and-cutoff at all qsearch nodes.

### GHI

Maintain the existing "live with it" stance (ADR-0018 §10, ADR-0027 §7). Do not add repetition checks inside qsearch as part of M5.F.

### Mate-score discipline

Reuse `score_to_tt` / `score_from_tt` unchanged. No qsearch-specific mate logic needed.

### SPRT baseline

`M5.E` (as specified in CLAUDE.md). Suggested SPRT parameters: `elo0=0, elo1=10` (conservative; the Crafty "wash" is possible). Accept at H1 or run 400-game cap; the CI lower bound should be above −10 Elo to treat as "not a regression."

### Node-budget consideration

Qsearch TT probes add one hash lookup per qsearch frame. At ~1.47M bench nodes (M5.E baseline), the majority of nodes are qsearch frames. The probe cost is a single cache-resident lookup (the same cache line was already loaded by negamax before delegating). Expected overhead: <3% per-node; likely absorbed by tree savings.

---

## Sources Consulted

- [CPW Quiescence Search](https://www.chessprogramming.org/Quiescence_Search)
- [CPW Transposition Table](https://www.chessprogramming.org/Transposition_Table)
- [CPW Graph History Interaction](https://www.chessprogramming.org/Graph_History_Interaction)
- [CPW Repetitions](https://www.chessprogramming.org/Repetitions)
- [CPW Score](https://www.chessprogramming.org/Score)
- [CPW Node Types](https://www.chessprogramming.org/Node_Types)
- [CPW Principal Variation Search](https://www.chessprogramming.org/Principal_Variation_Search)
- [TalkChess: Transposition table usage in quiescent search? (t=47373)](https://talkchess.com/viewtopic.php?t=47373)
- [TalkChess: Transposition table usage in quiescent search? page 5 (t=47373 start=40)](https://talkchess.com/viewtopic.php?t=47373&start=40)
- [TalkChess: Transposition table usage in quiescent search? page 6 (t=47373 start=50)](https://talkchess.com/viewtopic.php?t=47373&start=50)
- [TalkChess: Playing transposition table moves in the Quiescence search (t=69629)](https://talkchess.com/viewtopic.php?t=69629)
- [TalkChess: For or against the transposition table probe in quiet search? (p=892662)](https://talkchess.com/viewtopic.php?p=892662)
- [TalkChess: Exact TT scores and alpha/beta (t=63251)](http://www.talkchess.com/forum3/viewtopic.php?t=63251)
- [TalkChess: Best practices for transposition tables (t=76508)](https://talkchess.com/viewtopic.php?t=76508)
- [TalkChess: Transposition table replacement scheme (t=76499)](https://talkchess.com/viewtopic.php?t=76499)
- [rec.games.chess.computer: Hash Table and Quiescence Search](https://rec.games.chess.computer.narkive.com/QBiQWYWt/hash-table-and-quiescence-search)
- [rec.games.chess.computer: Transposition table for quiescence search?](https://rec.games.chess.computer.narkive.com/b8bSE4W5/transposition-table-for-quiescence-search)
- [OpenChess: Strange results when TT's and quiescence are combined (t=2292)](http://www.open-chess.org/viewtopic.php?f=5&t=2292)
- [OpenChess: Quiescence search best practices (t=2852)](https://open-chess.org/viewtopic.php?t=2852)
- [Mediocre Chess: Guide to transposition tables](http://mediocrechess.blogspot.com/2007/01/guide-transposition-tables.html)
- [Stockfish commit 45e5e65: do not store qsearch positions in TT as exact](https://github.com/official-stockfish/Stockfish/commit/45e5e65a28ce7e304c279fabf5f8a83cced73013)
- [Kishimoto & Müller 2004: A General Solution to the Graph History Interaction Problem (AAAI 2004)](https://cdn.aaai.org/AAAI/2004/AAAI04-102.pdf)
