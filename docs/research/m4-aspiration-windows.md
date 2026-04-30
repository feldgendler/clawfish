# Prior-Art Research: Aspiration Windows (M4.D)

Sources consulted: Chess Programming Wiki (CPW), TalkChess forum threads (Bob Hyatt, H.G. Muller, Erik Madsen, Joost Buijs, et al.), Mediocre Chess blog (Jonatan Dahl), MadChess development log (Erik Madsen), Beowulf chess theory (Colin Frayn), Wikipedia "Aspiration window," GameDev.net discussions, Shams/Kaindl/Horacek 1991 (IJCAI paper), Kaindl/Shams/Horacek 1992 (IEEE TPAMI), OpenChess forum.

Per ADR-0003, no third-party engine source code was read; CPW articles, papers, blog posts, and forum threads were the sole source material. This report is the evidence base for the M4.D plan.

---

## Headline Calls (Quick Reference)

| Decision | Recommendation | Confidence |
|---|---|---|
| First-try half-width | ±50 cp (literature workhorse; start here, SPRT-tune after) | High |
| Widening on fail-high | `(L, +∞)` where L is the fail-soft return | High |
| Widening on fail-low | `(−∞, L′)` where L′ is the fail-soft return | High |
| Depth threshold | Depth ≥ 4 (no aspiration below; breadth of literature) | High |
| Tier count | Two tiers only; third deferred to M5 | Medium |
| Mate score handling | Detect `|score| > MATE_IN_MAX_PLY`; fall back to full window | High |
| Killer persistence across re-search | Yes — killers from same-depth first try remain valid | Medium |
| PV clear before re-search | Yes — stale PV from failed first try must be cleared | High |
| TT entries from failed search | Keep them; bound type is correct; re-search exploits them | High |
| Score from prior iteration outside prior window | Still usable as center; widen immediately on first search | High |
| Abort mid re-search | Treat same as regular abort: discard, fall back to `last_complete` | High |
| SPRT methodology | Mixed-TC (roadmap §M4.D); width-tune via pairwise comparison | High |

**Roadmap design is consistent with the literature.** The chosen approach — two-tier asymmetric widening, ±50 cp first try, fail-soft returns as the proved bound on the non-failing side — matches the mainstream CPW/Beowulf/forum consensus. Reservations are documented in §7 (failure modes) and §13 (open questions).

---

## 1. What Aspiration Windows Are

### Definition

Aspiration windows narrow the alpha-beta search at the ID outer loop by substituting a small window `(prev − W, prev + W)` for the full window `(−∞, +∞)`, where `prev` is the prior iteration's score.

- A narrower window means more alpha and beta cutoffs in the tree, reducing the nodes searched.
- The gain is realized only when the true score at the next depth is within the window.
- If the score falls outside the window, a re-search at the same depth is required — potentially costing more than a full-window search if re-searches are frequent.

### Why They Help (Expected-Value Analysis)

- The expected value is positive **only** when first-move ordering quality is high enough that the prior iteration's score is a reliable predictor of the next iteration's score.
- CPW: "Typical window sizes are 1/2 to 1/4 of a pawn on either side of the guess." [(CPW Aspiration Windows)](https://www.chessprogramming.org/Aspiration_Windows)
- With TT + killers + history (M4.A–C) ordering, the first move at most nodes is strong, and score fluctuations between depths are usually small.
- The core assumption: "score at depth D is a reasonable estimate of score at depth D+1." This is the critical presupposition flagged by H.G. Muller — it holds when move ordering is good; it fails in tactically volatile positions. [(TalkChess t=76115)](https://talkchess.com/viewtopic.php?t=76115)

### Reported Elo Gains

| Source | Reported Gain | Notes |
|---|---|---|
| Forum consensus (TalkChess t=76115) | +10 to +20 Elo | "Modest" gains, multiple reports |
| Ras (TalkChess t=76115) | Positive with ±50 cp | Simple re-search logic |
| MadChess (Madsen, 2020) | −9 Elo (removed) | Engine-specific; instability dominated |
| Meesha (TalkChess t=78910) | Positive with staged widening | ±18 → ±20 → ±100 → ±325 → full |
| Forum search (various) | +18 Elo in one case | tc=10s/game, 10,000 games, simple ±50 |

Gains are **implementation-dependent** and strongly correlated with underlying move-ordering quality.

---

## 2. Interaction with Iterative Deepening and Fail-Soft Alpha-Beta

### How Prior Score Seeds the Window

At the start of ID iteration `d`:

```
prev = score from last_complete at depth d-1
alpha = prev - W
beta  = prev + W
result = negamax(pos, d, 0, alpha, beta, ...)
```

- `prev` is the fail-soft score stored in `last_complete: Option<(depth, bestmove, score)>` (already plumbed by M3.E per ADR-0017).
- Aspiration is a **no-op** until there is a `last_complete` result, i.e. until iteration 2.
- Aspiration is additionally suppressed below the depth threshold (§4).

### Fail-Soft Requirement

- Aspiration windows **require** fail-soft: the re-search logic depends on the returned score being the actual best found (which can be outside `[alpha, beta]`), not clamped to `beta`.
- CPW: "for both the PVS algorithm and the aspiration window technique you must use a fail-soft framework (otherwise you don't get scores outside the alpha/beta window)." [(CPW PVS and Aspiration)](https://www.chessprogramming.org/PVS_and_Aspiration)
- This project uses fail-soft (ADR-0016 §1). No change needed.

### Window and Full Search Interaction

- ID builds tree knowledge across iterations.
- TT entries stored in iteration d-1 help the aspiration re-search in iteration d find cutoffs faster (the TT supplies well-ordered moves).
- If the TT primes the re-search well, re-search cost is far below the cost of a cold full-window search.
- Beowulf: "hash tables will be rather full" when a re-search is needed, mitigating re-search cost. [(Beowulf theory)](http://www.frayn.net/beowulf/theory.html)

---

## 3. Two-Tier Asymmetric Widening Schedule

### The Roadmap's Chosen Approach

```
First try:  window = (prev − 50, prev + 50)
Fail-high:  result L ≥ prev+50  →  re-search window (L, +∞)
Fail-low:   result L′ ≤ prev−50 →  re-search window (−∞, L′)
```

This is consistent with the asymmetric widening principle documented in CPW, Beowulf, and TalkChess discussions.

### Why Asymmetric (Not Symmetric ±100)

- When the first try fails high (score ≥ beta), the window has **proved** the score is at least `L = returned_score ≥ prev+50`.
  - Re-searching with `(L, +∞)` keeps the proved lower bound and only opens the upper side.
  - Symmetric `(prev−50, prev+150)` would be wrong: it re-opens a range already proved empty below `L`.
- When the first try fails low (score ≤ alpha), the window has proved the score is at most `L′ = returned_score ≤ prev−50`.
  - Re-searching with `(−∞, L′)` keeps the proved upper bound and only opens the lower side.
  - Symmetric `(prev−150, prev+50)` would re-open a range already proved empty above `L′`.
- The asymmetric form is the unanimous recommendation in the literature. CPW describes the Crafty approach: "if a window `[g − 1/4, g + 1/4]` fails high, the next search becomes `[g − 1/4, g + 1]`" — keeping the stable side.
- A critical implementation pitfall: "Be very careful — when the search fails low and gives you value x < alpha < beta, re-search with `[new_alpha, beta]`, NOT `[new_alpha, alpha]`." Squeezing the upper bound triggers artificial fail-highs (yo-yo effect). [(TalkChess t=46624)](https://www.talkchess.com/forum/viewtopic.php?topic_view=threads&p=499768&t=46624)

### Parameter Sensitivity: Window Width

| Width (half) | Characterization | Source |
|---|---|---|
| ±8–16 cp | Very aggressive; requires very low EBF (~1.6); many re-searches | Modern top engines (CPW) |
| ±25–33 cp | Common in engines with good ordering; ~1/4 pawn | CPW, Beowulf (30 cp), literature |
| ±50 cp | "Workhorse default"; flat EV curve across position types | TalkChess consensus, roadmap |
| ±75–100 cp | Conservative; fewer re-searches but weaker cutoff benefit | Some forum discussions |
| ±150+ cp | Minimal cutoff gain; essentially no-op | Noted as "excessive" in t=41870 |

**EV curve shape:**

- The EV function (node reduction minus re-search cost) is roughly flat over the ±25–75 cp range for an engine with good move ordering.
- It drops sharply below ±15 cp (too many re-searches) and flattens above ±100 cp (too few cutoffs).
- For engines with high score volatility or weak ordering, even ±50 cp may produce net negative EV.
- Shams/Kaindl 1991 found that well-ordered trees (80% first-move-best in test suite) benefited most from aspiration windows; the margin narrowed as ordering quality declined. [(Shams/Kaindl 1991 IJCAI)](https://www.ijcai.org/Proceedings/91-1/Papers/031.pdf)

**Practical recommendation for this project:**

- Start with ±50 cp.
- Tune via the mixed-TC SPRT methodology described in the roadmap: test ±25 and ±75 as alternatives.
- If move ordering with M4.A–C is strong (TT hit rate high, history well-populated), a smaller window (±25–33 cp) may be superior.
- Do not tune blindly on node count alone — MadChess found improved node count from aspiration but net Elo loss, suggesting re-search cost in time-critical positions can dominate. [(MadChess removal)](https://www.madchess.net/2020/12/20/madchess-3-0-beta-4b7963b-remove-aspiration-windows/)

### Why Two Tiers, Not Three

A third intermediate tier (e.g., ±50 → ±150 → full) would add value only if its TT-primed cost is below `p × cost_full_after_fail`, where `p` is the probability of the third-tier search succeeding. Per the roadmap walkthrough:

- With good ordering, the failure probability at the second tier (asymmetric full-on-failure) is already low.
- A third tier adds parameter-tuning surface (choice of intermediate width) and code complexity.
- CPW documents both Crafty (multi-step widening) and simpler engines (immediate full on failure) as viable.
- The margin for a third tier to improve EV requires `c_intermediate < p × c_full` ≈ 0.6 of a cold full search — tight and hard to guarantee without engine-specific profiling.
- **Defer to M5**, following the roadmap's staged approach. The two-tier design is the correct starting point.

---

## 4. Threshold Gating

### Why Aspiration Is a No-Op Below Depth ~4–6

- Depth 1 trivially has no prior score.
- Depths 2–3: score volatility is high; tactical jumps are common; the prior score is an unreliable predictor.
- At low depths, the full-window search is already very fast (sub-millisecond); saving nodes by narrowing the window is not worth the re-search overhead if the window fails.
- Meesha (TalkChess t=78910): "open windows until depth 5, then narrow windows (±18 cp)."
- One forum contributor: "only apply aspiration at depth 6 and further, because lower than depth 6, the search takes less than half a second." [(TalkChess t=76115)](https://talkchess.com/viewtopic.php?t=76115)
- CPW does not give an explicit minimum but implies the technique is for deeper iterations.

### Common Threshold Values

| Threshold | Source |
|---|---|
| depth ≥ 4 | Forum discussions (most common), roadmap default |
| depth ≥ 5 | Meesha (t=78910) |
| depth ≥ 6 | One contributor (t=76115) |

**Recommendation:** depth ≥ 4. This is the most common value and provides headroom above the trivially fast iterations while still engaging aspiration early enough to build useful stats in the mixed-TC SPRT.

**Sensitivity:** The depth threshold is less sensitive than window width (it only determines which iterations participate, not how well each one performs). Pick 4, verify no SPRT regression, and do not spend tuning budget on it.

---

## 5. Fail-Soft Caveats: Prior Score Outside Prior Window

### The Situation

The prior iteration's score `prev` is itself a fail-soft result from the prior window. After the prior iteration, `prev` may have been:

1. An Exact score (within the aspiration window) — reliable center for the next window.
2. A fail-high bound (≥ prior beta) — `prev` is a lower bound, not the exact score.
3. A fail-low bound (≤ prior alpha) — `prev` is an upper bound, not the exact score.

Case 2 or 3 can arise if iteration `d-1` itself had an aspiration failure and was re-searched with an asymmetric window that still produced a fail.

### Handling Rules

- **Case 1 (Exact):** Use `prev` directly as the window center. Standard.
- **Case 2 (fail-high):** The score stored in `last_complete` is the result of the last successful (non-aborting) search. ADR-0017 ensures `last_complete` is updated only on completed, non-aborted iterations. So `prev` from `last_complete` is always an exact result (within whatever window was used for that iteration). The aspiration re-search's own fail-soft return is what feeds the next-tier window.
- **Implication:** The `last_complete` score is always trustworthy as a center, because it only gets written on a complete, unaborted iteration. No special handling needed for "prior score outside prior window" — that case simply did not produce a `last_complete` update.
- **What not to do:** Using the fail-soft return from an aborted mid-iteration re-search as the next center. Since `last_complete` is never written during an aborted iteration (ADR-0017 §2), this scenario is already prevented by the existing plumbing.

---

## 6. PV-Node Discipline Interaction

### Current State (M4.A–C)

ADR-0018 §11 introduces a synthetic `is_pv: bool` parameter to suppress TT cutoffs at PV nodes. Under plain alpha-beta (not PVS), `is_pv` is set `true` at the root and propagated to the first child `i == 0` in move-loop recursion order.

### What Aspiration Introduces

- Aspiration changes the **root window** `(alpha, beta)` but does not introduce a PVS-style null window at any node.
- The `is_pv` discipline is about whether TT entries are used for cutoffs vs ordering only. Aspiration does not change this: PV nodes are still ordered by TT moves but not cut.
- **No change to `is_pv` propagation required for M4.D.** The synthetic parameter continues to serve its purpose (protecting the displayed PV line from premature truncation).

### Note on PVS Interaction (M5 Context)

- PVS introduces a true `beta - alpha == 1` (zero-window) distinction for non-PV nodes.
- CPW documents complications when PVS meets aspiration: specifically, the first root move may not improve alpha in a re-search, which can leave the PV empty. The recommended approach is to search the first move with an open window in PVS + aspiration combined.
- **This project uses plain alpha-beta in M4.D.** There is no zero-window scout at non-root nodes. The PVS/aspiration complication does not apply until M5.
- The current synthetic `is_pv = parent_is_pv && i == 0` propagation is correct for M4.D and does not need adjustment.

### TT Ordering in Re-Searches

- TT entries stored during the first-try aspiration search are valid lower or upper bounds.
- A TT lower-bound entry (fail-high from first try) can legitimately produce a beta cutoff in the full-window re-search if the stored score exceeds the new beta. This is correct behavior — the TT entry's bound type is honored.
- H.G. Muller: the TT entries from a failed aspiration search are not "wrong" — they have the correct bound type. They can be usefully consulted in re-searches. [(OpenChess t=2995)](https://open-chess.org/viewtopic.php?t=2995)
- The problematic case is implementations that improperly shrink windows based on TT bounds or treat upper bounds as exact scores. This project's TT implementation correctly distinguishes `Lower`, `Upper`, and `Exact` bound types (ADR-0018), so no additional discipline is needed.

---

## 7. Failure Modes and SPRT Regression Risks

### 7.1 Yo-Yo / Oscillation Pattern

The most dangerous failure mode:

1. First try `(prev−50, prev+50)` fails high → return `L`.
2. Re-search `(L, +∞)` fails low → return `L′ < L`.
3. L′ is below L but above `prev−50` — the original window's range was never fully wrong.
4. The engine has now spent 3 searches to produce a result it could have found with 1.

This pattern is well-documented in TalkChess and MadChess. [(MadChess removal, 2020)](https://www.madchess.net/2020/12/20/madchess-3-0-beta-4b7963b-remove-aspiration-windows/) [(TalkChess t=78910)](https://talkchess.com/viewtopic.php?t=78910)

**Mitigation:**
- The asymmetric widening (§3) already addresses the worst case: re-search `(L, +∞)` cannot fail low if the fail-soft return `L′ < L` (it would need `L′ < L` which contradicts the lower-bound proof from the first try). Actually, it can fail low if the full-window re-search finds the true score is below `L` — this indicates a TT inconsistency or search instability, not a logic error.
- The two-tier design limits maximum re-searches to 2 per iteration. If the second tier (asymmetric full-window) also fails, a bug is likely in the bound update logic.
- The yo-yo pattern most often indicates: wrong side widened (§3 pitfall), or TT entries from prior iterations producing spurious cutoffs at intermediate depths.

### 7.2 Mate Score Interaction

- Aspiration window `(prev−50, prev+50)` centered on a non-mate score will fail immediately if the next iteration finds a mate, since mate scores are far outside any cp range.
- Concrete example: `prev = 20 cp`, first try `(−30, 70)`, next iteration finds mate-in-4 = `30000 − 8 = 29992`. The search returns `L ≥ 70`; re-search with `(L, +∞)` correctly finds the mate.
- No special handling is needed — the asymmetric widening handles this naturally.
- **However:** some implementations short-circuit to full window whenever the window edge is a mate score. This is unnecessary but harmless. Recommendation: let the fail-soft mechanism handle it; do not special-case mate detection in the aspiration loop.
- One forum developer noted: "if alpha or beta changes at a score of mate, search with full window." This is a valid but conservative guard — the asymmetric widening already does the right thing. [(TalkChess t=41870)](https://talkchess.com/viewtopic.php?t=41870)

### 7.3 Score Stability Under M4.A–C

- With TT + killers + history, the first move at each node has higher expected value than without these features.
- Expected first-try success rate for ±50 cp at depth ≥ 4 in an engine with good ordering: roughly 70–85% per reported forum observations (no published precise figures; this is inferred from the reported node-reduction in engines where aspiration helps).
- If M4.D's SPRT fails: suspect ordering quality first (check M4.A TT hit rate, M4.C history population rate), then check aspiration window direction logic.
- Roadmap note: "if M4.A `Hash` default is unusually small for the SPRT corpus, raise it (e.g. 64 MB or 128 MB) and re-run before debugging M4.D widening."

### 7.4 Sticky Aspiration (Consecutive-Iteration Failures)

- Consecutive iterations failing aspiration at the same depth is unusual but can occur in positions with score oscillation between depths.
- Recovery: each re-search resets correctly via the asymmetric widening (the second tier is full-window on the failing side, which is guaranteed to succeed if negamax is correct).
- Cost: two re-searches per iteration × N consecutive failures. This can dominate search time for specific positions.
- Mitigation: depth threshold gating (§4) limits exposure at shallow depths. No additional mitigation is documented for deep iterations beyond the two-tier design.

### 7.5 Net ELo Loss Risk

- MadChess's documented removal (+9 Elo without aspiration windows) illustrates the worst case.
- The MadChess case had search instability dominating the positional gain. The engine had already moved to a different dev stage where aspiration may not have interacted well with other pruning.
- This project does not yet have LMR or NMP (those are M5). Without these aggressive pruning techniques, score volatility between depths may be higher, making aspiration more likely to fail. This is a genuine risk.
- **SPRT criterion is the guard.** If SPRT at mixed-TC fails, the right response is to diagnose (check re-search rates, check which positions cause failures) before committing to removal.

---

## 8. Tunable Parameters

| Parameter | Load-Bearing? | Tunable Range | Recommendation |
|---|---|---|---|
| First-try half-width `W` | Yes — primary Elo lever | ±15 to ±75 cp | Start ±50; SPRT-tune ±25, ±75 |
| Depth threshold `T` | Weakly | 3–6 | Start 4; don't spend SPRT budget |
| Tier count | Structural | 2 or 3 | 2 for M4.D; 3 at M5 if warranted |
| Fallback discipline | Structural — must be asymmetric | N/A | `(L, +∞)` / `(−∞, L′)` |
| Score overflow handling | Not tunable — i32 is safe | N/A | No action needed (§12) |

**What is load-bearing (must not change without SPRT):**

- The asymmetric fallback direction — widening the wrong side is a correctness error, not a tuning decision.
- The iteration where aspiration first engages (must be iteration 2+, when `last_complete` has a score).
- The TT participation during re-searches — the bound type must be honored (§6).

**What is tunable:**

- `W` — the roadmap plans 3 SPRT comparisons (±25, ±50, ±75).
- Whether to reset killers between first-try and re-search at the same depth (see §9).

---

## 9. Adaptive Width Strategies (Deferred to Post-M4.D)

### Width as a Function of Depth

- Tighter windows at deeper iterations, where score volatility is lower.
- Intuition: depth-12 scores are more stable than depth-5 scores; a ±25 cp window is appropriate at depth 12 but may fail frequently at depth 5.
- Implementation: `W(d) = W_base / sqrt(d)` or `W(d) = W_base - k*(d - T)` for depth above the threshold.
- **Doubles the tuning surface** (two parameters: `W_base` and the scaling factor). Defer to post-M4.D per the roadmap.

### Width as a Function of Prior PV Stability

- Stockfish and RobboLito use variance-based adaptive width: if the root score has been stable across multiple prior iterations, shrink the window; if it has oscillated, widen it.
- Concrete: width ∝ `sqrt(mean_squared_error(scores over last k iterations))`.
- Requires tracking per-iteration scores beyond just the most recent `last_complete`.
- This is significantly more complex and the gain is uncertain without engine-specific profiling. Defer.

---

## 10. Tested-and-Rejected Variants in the Literature

### Symmetric Widening

- On fail-high, widen to `(prev−50, prev+150)`. On fail-low, widen to `(prev−150, prev+50)`.
- Throws away the proved bound from the first try, wasting nodes on already-disproved score ranges.
- Universally noted as suboptimal in TalkChess discussions.
- No modern engine uses symmetric widening. CPW documents asymmetric as the standard.

### Immediate Full-Window on Any Failure

- Mediocre Chess uses: on failure, reset to `(−∞, +∞)` and re-search.
- Simple to implement and correct.
- Discards the proved bound from the first try: the asymmetric approach recovers ~half the nodes by keeping the correct bound.
- Mediocre reports "no loss of accuracy" — the simplicity works but leaves nodes on the table.
- Recommendation: use asymmetric (§3) instead. The implementation cost difference is minimal.

### Infinite Re-Search Loop

- On failure, widen by a fixed delta and retry; repeat until success.
- Multi-step Meesha approach: ±18 → ±20 → ±100 → ±325 → full.
- CPW documents Crafty's gradual widening.
- Pro: can succeed with fewer nodes than immediate full-window if the true score is only slightly outside the initial window.
- Con: adds code complexity, more tuning parameters, and risk of yo-yo instability across many small steps.
- For M4.D: two tiers is the right choice. Multi-step is M5 scope if warranted.

### No Aspiration at Root Only (Recursive Aspiration)

- Some early engines applied aspiration windows at every node, not just the root.
- This is theoretically sound but produces instability: each recursive call can trigger a cascade of re-searches at intermediate nodes.
- Universal consensus: apply aspiration only at the ID loop root level, not recursively. The recursive case is PVS at non-PV nodes, which is a separate technique.

---

## 11. SPRT Methodology for M4.D

### Roadmap's Mixed-TC Approach

The roadmap specifies a mixed-TC SPRT for M4.D. Rationale: M4.C's empirical data showed fast-TC SPRT was blind to history's Elo gain (+3.47 ± 5.88 at 10+0.1; +35.74 ± 27.51 at 60+0.6). Aspiration windows amplify with depth, making the same TC-blindness likely.

Procedure:

1. Run 4 fixed-TC matches: tc=10+0.1, 20+0.2, 40+0.4, 60+0.6 (~150 games each = ~600 total).
2. Union of games as a discrete-uniform mixed-TC sample.
3. Apply SPRT `(elo0=0, elo1=5)` to the union for accept/reject.
4. Fit Δ Elo vs TC for a diagnostic curve.
5. Separately run width-comparison pairs: ±25 vs ±50, ±75 vs ±50 under the same mixed-TC methodology.

### Width Tuning via SPRT

- The literature does not provide a clean Elo curve shape for width tuning — every report is engine-specific.
- The TalkChess consensus is that a pairwise SPRT comparison between two fixed widths (e.g., ±25 vs ±50) at sufficient games (2000+) gives a reliable result.
- One developer reports +18 Elo from ±50 vs full-window over 10,000 games at 10s/game. [(TalkChess t=76115)](https://talkchess.com/viewtopic.php?t=76115)
- The brtzsnr evaluation study (18,500 games, tc=40/15+0.05) found no measurable difference from window algorithm changes — a reminder that insufficient TC and game count can produce null results. [(TalkChess t=55117)](https://www.talkchess.com/forum3/viewtopic.php?t=55117)

---

## 12. Implementation Pitfalls

### 12.1 Forgetting to Clear the PV Before Re-Search

- **Problem:** The first-try search updates the PV table for some nodes before the fail-high or fail-low is detected. If the PV is not cleared before the re-search, stale entries from the failed try (which may reflect a score outside the window) remain in the PV table and corrupt the re-search's PV.
- **Effect:** Emitted `info pv` line may contain moves from the failed search rather than the re-search's actual PV.
- **Fix:** Clear per-ply PV lengths at the start of each aspiration attempt (first-try and any re-search). This is the same operation as the per-iteration PV clear already in the ID loop.
- **ADR-0016 §5:** "Update fires only at PV nodes... and only when the recursive subtree completed (not aborted)." The triangular PV's `pv.clear_ply(ply)` at the start of `negamax` already clears each ply's slot as a node is visited — the issue is specifically with the re-search starting with stale lengths from the first-try iteration. Resetting `pv.lengths[0..MAX_PLY]` to zero before each aspiration attempt (not just per-iteration) is the correct discipline.

### 12.2 Wrong Side Widened on Failure

- **Problem:** On fail-high, widening the lower side (`alpha = prev − 100`) instead of the upper side (`beta = +∞`). On fail-low, widening the upper side.
- **Effect:** The re-search uses a window that still contains the already-disproved region.
- **Fix:** Strictly follow the asymmetric rule (§3). On fail-high: `(L, +∞)` where `L` is the fail-soft return (which satisfies `L ≥ prev + W`). On fail-low: `(−∞, L′)` where `L′ = fail-soft return ≤ prev − W`.

### 12.3 TT Entries from Failed Iterations

- **Common misconception:** Entries stored during a failed aspiration search are "dirty" and should be discarded.
- **Reality:** Entries from a failed search carry a correct bound type:
  - A fail-high (Lower bound) entry is valid — any probe that returns Lower and score ≥ beta will correctly produce a beta cutoff.
  - A fail-low (Upper bound) entry is valid — any probe that returns Upper and score ≤ alpha will correctly produce an alpha cutoff.
- **What to avoid:** Disabling TT storage during aspiration searches. One developer tried this and found it "even slower than not using aspiration windows at all." [(OpenChess t=2995)](https://open-chess.org/viewtopic.php?t=2995)
- **Standard discipline:** Keep TT storage during all aspiration searches. The bound type handles correctness automatically.

### 12.4 Stop-Flag Interaction During Re-Search

- **Problem:** If the stop flag fires during the asymmetric full-window re-search (second tier), the search is aborted mid-re-search.
- **Effect:** The aborted re-search produces no valid result. `last_complete` from the prior completed iteration should be the fallback.
- **Fix:** The existing abort discipline (ADR-0016 §7, ADR-0017 §2) already handles this:
  - `self.aborted = true` → return 0 sentinel.
  - After the re-search returns: check `self.aborted`; if true, break the aspiration loop and break the ID loop.
  - `last_complete` from depth `d-1` becomes the result.
- **No additional code needed** beyond honoring the existing abort check after each search call.

### 12.5 Mid-Re-Search Abort: Is the Score Usable?

- A partial fail-high re-search (`(L, +∞)`) aborted with score `S` — is `S` usable?
- `S` is the best score found in the subtrees that completed before the abort. As a lower bound, `S ≥ L` if any root move improved alpha.
- **Standard practice:** Do not use partial re-search scores. Discard and use `last_complete`. This is consistent with ADR-0017 §2: "mid-iteration abort discards partial PV/score."
- The asymmetric re-search is logically a new iteration at the same depth; applying the same abort-discard policy is correct.

### 12.6 Score Type and Overflow

- This project uses `i32` for all search scores (ADR-0016 §2).
- `MATE = 30000`, `W = 50`. `prev ± 50` stays well within `i32` range.
- Even at `prev = MATE_IN_MAX_PLY = 29936`: `prev + 50 = 29986 < 30000 = MATE`. This is within bounds — the aspiration window would fail immediately (the search finds a mate, which is a higher score), which is the correct behavior.
- `prev - 50` at `prev = -(MATE_IN_MAX_PLY) = -29936`: `prev - 50 = -29986 > -30000`. Safe.
- **No overflow risk.** No action needed.

### 12.7 Killer Persistence Across Aspiration Re-Search

- The M4.B research (§7) deferred the question: "Persist killers across aspiration re-searches at the same depth?"
- **Recommendation:** Yes — persist killers across the aspiration re-search at the same depth.
  - Killers found during the first-try search at depth d are valid cutoff hints. The same positions are searched in the re-search; a move that caused a beta cutoff at ply k in the first try will likely cause one in the re-search.
  - Clearing killers between the first-try and re-search discards valid ordering information for no benefit.
- This is distinct from clearing killers between ID depth steps (first try at d vs first try at d+1). Clearing between depth steps is the M4.B policy; preserving them through aspiration re-searches at the same depth is the M4.D refinement.
- **Implementation:** The existing `clear_killers` call is at the start of each depth step's first try only. Do not add a `clear_killers` call between the first-try and re-search at the same depth.

---

## 13. Open Questions for the M4.D Plan

1. **Killers: clear between depth steps only, or also between re-searches?** Recommendation is clear-between-depth-steps only (§12.7), but no direct Elo measurement found in the literature. Flag in plan as "recommended; verify by SPRT if re-search rate is high."

2. **Exact threshold value.** Depth 4 is the recommendation (§4); the plan should encode it as a named constant (`ASPIRATION_MIN_DEPTH = 4`) for testability and future tuning.

3. **PV clear discipline on re-search.** The plan must explicitly specify resetting `pv.lengths` at the start of each aspiration attempt (§12.1). This is not currently explicit in the per-iteration reset prose.

4. **Width constant naming.** The plan should name `ASPIRATION_HALF_WIDTH = 50` (in cp) as an explicit constant. This makes the SPRT width-tune experiments a one-line change.

5. **`last_complete` score as center: what if depth d−1 itself was a re-searched iteration?** Per §5: `last_complete` is only written on completed, non-aborted iterations. Whether that completion was on a first-try or a re-search, the score is exact. No special handling needed.

6. **TT probe during re-search: can a fail-low TT entry from the first-try cause a spurious cutoff?** An upper-bound entry from the first-try with score ≤ `prev − W` will be at most `L′` in the re-search. In the re-search window `(−∞, L′)`, a stored Upper bound with score `s ≤ L′` satisfies the cutoff condition — but this is correct, not spurious. The re-search is genuinely trying `(−∞, L′)` as the window.

7. **Score-stability under M4.C ordering.** The research does not give an empirical first-try success rate for ±50 cp with this engine's ordering quality. The mixed-TC SPRT will empirically measure this. No open question to resolve before implementation — the design is correct, and SPRT is the validation.

---

## 14. Consistency Assessment: Roadmap vs Literature

### Points of Strong Agreement

- Two-tier design (CPW, Beowulf, multiple forum threads).
- Asymmetric fallback keeping the proved bound (CPW, TalkChess consensus).
- ±50 cp as the starting default (multiple forum reports; "workhorse default").
- Depth threshold ≥ 4 (most common value in forum discussions).
- Fail-soft requirement (CPW, TalkChess).
- No aspiration below threshold depth (universal).

### Reservations

1. **Modest expected Elo gain.** Literature reports 10–20 Elo in favorable conditions. The M4.C gain (+35 Elo at slow TC) was visible; aspiration may be smaller or mixed at fast TC. The mixed-TC methodology correctly accounts for this.

2. **MadChess precedent.** One well-documented case of aspiration causing net regression at a similar engine development stage (no LMR/NMP). This project is in the same pre-pruning stage. If the SPRT fails, the right response is systematic diagnosis before removal — the MadChess case may have had an unrelated bug (the author later concluded it was a complex interaction with other search code).

3. **Third-tier deferral.** Some engines (Crafty) use multi-step gradual widening. The roadmap defers this to M5. The literature does not give a clear Elo advantage for the third tier over the two-tier design; the deferral is well-motivated.

4. **Adaptive width.** Modern top engines (Stockfish) use variance-adaptive width. The roadmap defers this. Correct call at M4.D scope — the fixed-width baseline must be established first.

---

## Source List

- [CPW — Aspiration Windows](https://www.chessprogramming.org/Aspiration_Windows)
- [CPW — PVS and Aspiration](https://www.chessprogramming.org/PVS_and_Aspiration)
- [CPW — Principal Variation Search](https://www.chessprogramming.org/Principal_Variation_Search)
- [CPW — Triangular PV-Table](https://www.chessprogramming.org/Triangular_PV-Table)
- [CPW — Fail-High](https://www.chessprogramming.org/Fail-High)
- [CPW — Fail-Low](https://www.chessprogramming.org/Fail-Low)
- [CPW — Window](https://www.chessprogramming.org/Window)
- [TalkChess t=41870 — Aspiration windows](https://talkchess.com/viewtopic.php?t=41870)
- [TalkChess t=76115 — Are Aspiration Windows Worthless?](https://talkchess.com/viewtopic.php?t=76115)
- [TalkChess t=78910 — Aspiration Window Instability?](https://talkchess.com/viewtopic.php?t=78910)
- [TalkChess t=47996 — Aspiration windows](https://talkchess.com/forum3/viewtopic.php?t=47996)
- [TalkChess t=65589 — Aspiration window problem](https://talkchess.com/viewtopic.php?t=65589)
- [TalkChess t=46624 — Aspiration window - effect? Issue with hashtables?!](https://www.talkchess.com/forum/viewtopic.php?topic_view=threads&p=499768&t=46624)
- [TalkChess t=55117 — Evaluating aspiration window algorithm changes](https://www.talkchess.com/forum3/viewtopic.php?t=55117)
- [TalkChess t=69079 — How to solve Transposition Table with Aspiration Window?](https://talkchess.com/viewtopic.php?t=69079)
- [Mediocre Chess — Guide: Aspiration Windows, Killer Moves, PVS (blog)](http://mediocrechess.blogspot.com/2007/01/guide-aspiration-windows-killer-moves.html)
- [Mediocre Chess — Aspiration Windows Guide (sourceforge)](https://mediocrechess.sourceforge.net/guides/aspirationwindows.html)
- [MadChess — Remove Aspiration Windows (2020)](https://www.madchess.net/2020/12/20/madchess-3-0-beta-4b7963b-remove-aspiration-windows/)
- [MadChess — Aspiration Window tag](https://www.madchess.net/tag/aspiration-window/)
- [Beowulf Chess Theory — Aspiration Windows](http://www.frayn.net/beowulf/theory.html)
- [OpenChess t=2995 — Aspiration window with TT question](https://open-chess.org/viewtopic.php?t=2995)
- [GameDev.net — Aspiration Windows in Chess Tree Search](https://www.gamedev.net/forums/topic/291146-aspiration-windows-in-chess-tree-search/291146/)
- [Wikipedia — Aspiration window](https://en.wikipedia.org/wiki/Aspiration_window)
- [Shams, Kaindl, Horacek — "Using Aspiration Windows for Minimax Algorithms" (IJCAI 1991)](https://www.ijcai.org/Proceedings/91-1/Papers/031.pdf)
- [Kaindl, Shams, Horacek — "Minimax Search Algorithms With and Without Aspiration Windows" (IEEE TPAMI, 1992)](https://ieeexplore.ieee.org/document/106996/)
- [Semantic Scholar — Shams/Kaindl 1991 paper](https://www.semanticscholar.org/paper/Using-Aspiration-Windows-for-Minimax-Algorithms-Shams-Kaindl/8626616b81aa96f5afd7fb986044c74cfa750a23)
