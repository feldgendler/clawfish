# Prior-Art Research: Late Move Reductions (M5.C)

Sources consulted: Chess Programming Wiki (CPW) on Late Move Reductions / Reductions, TalkChess discussions on LMR conditions and tuning, Arasan's programmer guide (high-level prose only).

Per ADR-0003, no third-party engine source code was read. Findings below come from prose sources, forum discussions, and documentation rather than engine repositories.

---

## TL;DR — Five-Bullet Bottom Line

- LMR is a **move-loop reduction**, not a prologue cutoff: search later quiet moves at reduced depth, then re-search at full depth only if the reduced search returns above alpha.
- The stable v1 gate set is conservative: **non-PV, not in check, quiet move, depth at least 3, move ordered late enough, and not a trusted quiet** (TT move, killers, strong-history quiets).
- A **log-log base reduction** is the modern workhorse. For a first implementation, a pure helper of the form `base + ln(depth) * ln(index) / divisor`, clamped into a small legal range, matches the roadmap and keeps mutations directly testable.
- Full-depth re-search on reduced fail-high is still the safest first landing. Adaptive re-search depth exists in stronger engines, but it adds another tuning surface and weakens attribution for the first M5.C SPRT.
- LMR composes naturally with clawfish's current search shape: it belongs inside the step-13 move loop, after move ordering and before recursion, and it should use the existing `quiets_searched`, killers, and history infrastructure rather than invent new move classes.

---

## 1. What LMR Is

LMR assumes that strong move ordering means **late quiets are less likely to matter** than earlier moves. Instead of pruning them outright, the engine searches them at reduced depth first. If that reduced search still improves alpha, the move is re-searched at full depth.

This is narrower than forward futility or late-move pruning:

| Technique | Action | Typical scope |
|---|---|---|
| LMR | Search move at reduced depth, maybe re-search | Late moves in the normal move loop |
| Frontier futility | Skip some quiets at shallow depth | Fail-low side of the tree |
| Late-move pruning | Skip very late quiets entirely | More aggressive, usually after LMR exists |

For clawfish M5.C, the roadmap commits to the first of these only.

---

## 2. Stable Conditions Across Sources

The common conditions were consistent across CPW and forum discussions:

| Condition | Consensus | M5.C implication |
|---|---|---|
| Non-PV only | Strong | Clawfish's `is_pv` gate remains the top-level LMR gate |
| Not in check | Strong | Skip reductions when `in_check(pos)` at the current node |
| Quiet moves only for v1 | Strong | Captures / promotions stay full-depth in M5.C |
| Depth floor around 3 | Strong | No LMR at `depth < 3` |
| Later moves only | Strong | First few moves stay full-depth |
| Trusted quiets reduced less or not at all | Strong | Killers and good-history quiets should be exempt in v1 |

The two disputed conditions were:

- Checking quiets: many engines reduce them less or not at all, but clawfish does not yet have a cheap "gives check" classification at the move-loop decision point that is clearly worth adding just for M5.C.
- Pawn moves / passed pawns: often special-cased in stronger engines, but that is more tuning than a first landing needs.

Recommendation for M5.C v1: **do not add new chess-specific move classes**. Reuse the current quiet/non-quiet split plus existing killer/history signals.

---

## 3. Reduction Formula

CPW's summary is the best high-level fit for the roadmap: reduction grows with both **depth** and **move number**, commonly with a log-log shape.

Representative formulas cited in prose sources:

| Shape | Characteristic |
|---|---|
| `constant` | Too blunt; easy to land, weak tuning surface |
| `sqrt(depth) + sqrt(moves)` | Older style; coarser than modern practice |
| `base + ln(depth) * ln(moves) / divisor` | Modern workhorse; roadmap already points here |

For clawfish, the value of the log-log helper is not that it is magically optimal, but that it gives:

- small reduction for early-eligible quiets,
- larger reduction only when both depth and quiet index grow,
- a pure helper whose boundary behavior can be tested directly.

### Recommendation

Use a helper like:

```text
base_reduction = floor(base + ln(depth) * ln(late_quiet_index) / divisor)
```

with:

- `depth >= 3`,
- `late_quiet_index >= 2` by construction,
- clamp to `0..=(depth - 2)` or tighter,
- v1 constants chosen conservatively.

The exact constants are an SPRT-tunable surface. The important design commitment is the **shape** and the **clamp discipline**.

---

## 4. Which Move Index To Use

This is the most important clawfish-specific choice.

Possible indices:

| Index | Pros | Cons |
|---|---|---|
| Full move-loop index | Cheap, already available | Polluted by captures and TT move placement |
| Quiet-only index | Matches the roadmap wording "late quiets" | Requires a per-node quiet counter |
| History-ranked percentile | Stronger signal | More machinery than M5.C needs |

Because clawfish already searches captures before killers before history quiets, **full move index is a distorted proxy** for what M5.C actually wants. A quiet could be the first quiet searched but still have a large overall loop index because several captures came first.

Recommendation: use a **quiet-only ordinal** within the move loop, incremented only for quiet moves that are eligible to be considered for reduction. This aligns with the roadmap's wording and reduces coupling to future changes in capture ordering.

---

## 5. Trusted Quiets: Skip or Reduce Less

The sources agree on the idea, not the exact policy.

Candidates for "trusted quiets":

- TT move
- killer moves
- quiets with strong history

For a first landing there are two policy families:

| Policy | Simplicity | Risk |
|---|---|---|
| Skip reduction entirely | Higher | Lower |
| Reduce less than ordinary quiets | Lower | Higher |

Recommendation for M5.C v1: **skip reduction entirely** for trusted quiets. This is simpler, easier to test, and fits the roadmap text ("reduction skipped for TT-move, killers, in-check, and high-history quiets").

The only new design choice is what counts as "high history." The cleanest v1 rule is a named threshold constant against the existing butterfly history score.

---

## 6. Re-search Policy

Classical LMR behavior:

- reduced search first,
- if reduced result `<= alpha`, accept it,
- if reduced result `> alpha`, re-search at full depth.

This is the safest first implementation because:

- it preserves correctness without relying on reduction-aware second-pass depth math,
- it is directly testable using score / node-count deltas,
- it keeps the first SPRT attributable to the presence of LMR rather than to extra tuning layers.

Recommendation for M5.C v1: **full-depth re-search on reduced fail-high only**.

---

## 7. Placement in Clawfish

LMR belongs in clawfish's step-13 move loop, not in the prologue.

The natural insertion point is:

1. make move,
2. compute child window and child PV flag,
3. decide whether this quiet is LMR-eligible,
4. if eligible, call reduced-depth negamax first,
5. if reduced result beats alpha, re-run at full depth,
6. unmake, then continue with the existing fail-soft / PV / killer / history logic.

Why this fits the current engine:

- `order_moves(...)` already produces the ranking LMR depends on.
- `is_quiet(mv)` already exists.
- `killers` and `history_table` already exist.
- `quiets_searched` already tracks quiets that fully completed without cutting, so M5.C does not need to invent another per-node quiet list.

---

## 8. Interactions With Existing M5 State

### With RFP and NMP

RFP and NMP operate in the prologue before move generation. LMR operates inside the move loop after move ordering. They are complementary rather than overlapping.

### With killers and history

LMR depends on the quality of quiet ordering. That means clawfish's M4.B/M4.C layering is exactly the substrate LMR wants:

- killers identify tactically hot quiets,
- history identifies statistically successful quiets,
- the remaining quiets are the right ones to distrust.

### With qsearch

Reduced depth can reach `depth == 0` earlier, so LMR increases reliance on qsearch correctness. That is acceptable because M5.E and M5.F are already sequenced later to clean up qsearch and TT interaction separately.

---

## 9. Main Risks

| Risk | Why it matters | Mitigation |
|---|---|---|
| Over-reducing early quiets | Can miss principal tactical refutations | Quiet-only index + conservative depth floor + skip trusted quiets |
| Reduction formula bug | Off-by-one can silently turn "reduce" into "prune" or "no-op" | Pure helper + direct boundary tests |
| Re-search discipline bug | Reduced fail-high not re-searched changes semantics | Direct tests that pin full-depth retry behavior |
| Empirical pins drifting | M5.A/M5.B already have firing counters sensitive to tree shape | Prefer direct M5.C counters for LMR-specific behavior rather than broad node-count pins |

---

## 10. Recommendation for M5.C v1

Implement **conservative quiet-only LMR** with the following commitments:

- Non-PV only.
- Current node not in check.
- `depth >= 3`.
- Quiet moves only.
- Quiet-only ordinal drives the "late" notion.
- Skip reductions for killer quiets and high-history quiets.
- Use a named pure helper for the base reduction with a log-log shape.
- Clamp the reduction so the reduced search still has meaningful depth.
- Re-search at full depth only when the reduced search returns above alpha.

This is the smallest design that matches the roadmap text and uses the current M4/M5 infrastructure cleanly. More aggressive variants like reducing checking quiets less, reducing some captures, or adaptive re-search depth are good follow-ups, not good v1 commitments.
