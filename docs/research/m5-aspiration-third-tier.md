# Prior-Art Research: Aspiration Windows — Third Tier (M5.I)

Sources consulted: Chess Programming Wiki (CPW), TalkChess forum threads (Bob Hyatt, H.G. Muller, Erik Madsen, Meesha author, Joost Buijs, lucasart, et al.), MadChess development log, Beowulf chess theory (Colin Frayn), Shams/Kaindl/Horacek 1991 IJCAI paper, grokipedia summary. Per ADR-0003, no third-party engine source code was read.

This note extends `docs/research/m4-aspiration-windows.md` with the third-tier-specific evidence. The M4 note's §3 ("Why Two Tiers, Not Three") deferred the third tier to M5; this is the M5.I evidence base.

---

## Headline Calls (Quick Reference)

| Question | Finding | Confidence |
|---|---|---|
| Best widening family for intermediate tier | Family A (fixed intermediate half-width) or Family B (multiplicative delta), not C (score-relative recentering) | High |
| Recommended intermediate half-width | ±100–150 cp; Meesha's +20/+100/+325 sequence is the only documented multi-step schedule with specific values | Medium |
| Proved-bound on fail-high second tier | `alpha = returned_score` (the fail-soft return), NOT `alpha = prev + W_intermediate`; CPW, Crafty, and lucasart all confirm asymmetric form | High |
| Third-tier = existing fallback | Yes — the full-window re-search already in M4.D is exactly the third tier; the "intermediate" widening is a second tier inserted between the current first and second | High |
| EV evidence for third tier | No clean SPRT-validated A/B; EV depends on TT-primed intermediate cost < ~0.6 × cold full search; condition likely holds with good TT at depth ≥ 6 | Low |
| Three-tier yo-yo risk | Higher than two-tier; an intermediate tier can fail high then low then high again ("yo-yo cascade") if intermediate window is too narrow | Medium |
| Mate handling at third tier | Asymmetric fail-soft handles mate naturally; mate causes fail-high, which the intermediate tier correctly propagates as `alpha = returned_score`; explicit mate check is optional / defensive | High |
| Depth-dependent intermediate width | Theoretically motivated; not documented with SPRT evidence; adds tuning surface | Low |
| Specific recommendation for this engine | Fixed `±150 cp` intermediate tier with `alpha = returned_score` proved-bound preservation; three-tier cap; SPRT expected inconclusive | Medium |

---

## 1. What the Literature Actually Says About Multi-Step Widening

### 1.1 Crafty: Two-Step Widening (Fixed Intermediate)

CPW describes Crafty's approach as the canonical two-step widening (which is the prototype for the three-tier design):

> "Some programs, such as Crafty, also use a gradual widening on re-searches. If an initial window `[g − 1/4, g + 1/4]` fails high, the next search becomes `[g − 1/4, g + 1]`. It's important to note that the bound that didn't fail is unchanged." — [CPW Aspiration Windows](https://www.chessprogramming.org/Aspiration_Windows)

Crafty's specific schedule in pawn units: ±1/4 pawn (~25 cp) first try → fail-high → widen beta to +1 pawn (~100 cp) while keeping alpha at prev − 1/4. This is Family A (fixed intermediate half-width), and the intermediate width is ~75 cp on the failing side (beta moves from +25 to +100).

The key property CPW emphasizes: "the bound that didn't fail is unchanged." This is the proved-bound preservation principle stated as an explicit design rule.

### 1.2 Modern Engines: Exponential Delta (Family B)

> "Modern engines, like RobboLito or Stockfish, start with a rather small aspiration window, and increase the bound that fails in an exponential fashion." — [CPW Aspiration Windows](https://www.chessprogramming.org/Aspiration_Windows)

From a TalkChess t=46624 discussion, lucasart (documenting RobboLito style):

> "delta += delta / 2" for gradual expansion (RobboLito style), or "delta += delta" (delta doubles each step) for faster widening.

These engines use a loop rather than a fixed tier count: `while not succeeded: alpha = max(prev - delta, -INF); beta = min(prev + delta, +INF); delta += delta`. The loop is implicitly multi-tier with the tier count determined by how many iterations run before the window is wide enough to contain the true score.

The Joost Buijs report from t=76115: "With a fail low/high I open up the window with exponential steps in one direction only." — same family B, but one-sided: only the failing side widens; the proved bound on the other side is preserved.

### 1.3 Meesha's Multi-Step Schedule

From TalkChess t=78910:

> "I expand the window in the appropriate direction another 20 cps, then another 100, then another 325 before opening the window completely." — Meesha

Initial window ±18 cp at depth 5+. The schedule in cumulative half-widths on the failing side: 18 → 38 (+20) → 138 (+100) → 463 (+325) → ∞. This is four tiers plus full-window fallback. The key structure: small steps between tier 1 and tier 2 (+20); large jump to tier 3 (+100); very large jump to tier 4 (+325).

For a design point comparison: if this engine's starting tier is ±50 cp (vs Meesha's ±18 cp), the analogous second tier would be around 150–200 cp, the third tier 450–550 cp, and the fourth tier is the existing full-window fallback. Since the engine already has the full-window fallback, adding one intermediate tier at ~150 cp on the failing side is what M5.I proposes.

### 1.4 A Documented Three-Tier Counter-Data Point

From t=76115, MadChess used windows of `100 cp, 500 cp, ∞` (three tiers including full-window) at minimum depth 7:

> "I received mixed answers. Many got a measurable gain, some (like me) got no gain or an ELO loss." — Erik Madsen

MadChess removed aspiration windows entirely and gained ~9 Elo. The removal context: the engine was using a three-tier schedule (100, 500, ∞) and the result was net negative. **This is the strongest published counter-evidence against the third tier.** The confound: MadChess was operating at depth 7+ minimum, which should be favorable; the loss suggests search instability from the multi-tier yo-yo pattern dominated the re-search savings.

---

## 2. Proved-Bound Preservation in the Intermediate Tier

### 2.1 The Core Rule

The asymmetry principle documented in CPW and confirmed by TalkChess consensus:

> "when the search fails low and gives you a value x < alpha < beta, you research the window [new_alpha, beta], and NOT [new_alpha, alpha] or [new_alpha, x]." — lucasart, TalkChess t=46624

By symmetry, on fail-high: the new window is `[returned_score, new_beta]`, NOT `[alpha, new_beta]`. The fail-soft returned score is the proved lower bound; alpha was merely the threshold that was exceeded.

### 2.2 Application to the Intermediate Tier Specifically

Scenario: first tier `(prev - 50, prev + 50)` fails high, returning `L ≥ prev + 50`.

The intermediate tier's `alpha` MUST be `L` (the fail-soft return), not `prev` or `prev + W_intermediate`.

If `L ≥ prev + 150` (the intermediate beta) and `W_intermediate = 150`, then the intermediate window `(L, prev + 150)` would have `alpha ≥ beta` — an empty/inverted window. In this case the intermediate tier is a no-op: the first tier's return already proved the score is at or above the intermediate beta. The right response is to skip directly to the next tier.

The literature describes this implicitly but does not explicitly document the "skip-if-already-past-intermediate-beta" rule. This is an **open question** flagged below.

### 2.3 The Degenerate Case: When Fixed Width is Narrower Than the Proved Bound

If `W_intermediate = 150` cp and the returned score `L = prev + 200` (the first tier failed by 150 cp), then:
- `alpha = L = prev + 200`
- `beta = prev + 150`
- `alpha > beta` — inverted window

This situation means the intermediate tier's beta (`prev + 150`) is already below the proved lower bound. The intermediate tier must be skipped entirely, and the search should proceed directly to the next tier (widening beta further).

**None of the literature sources directly address this degenerate case for the intermediate tier.** The exponential-delta approach (Family B) avoids it by design: the new `beta = prev + delta` (with `delta > L - prev` by construction after doubling), so the window never inverts. A fixed intermediate width (Family A) can invert if the first-tier failure exceeded the intermediate width.

**Open question**: should the intermediate-tier check be `if L >= prev + W_intermediate: skip to next tier` explicitly? Or use `alpha = max(alpha, L)` and `beta = prev + W_intermediate`, then let the search detect the empty window? The latter is safe under fail-soft (an empty window immediately fail-highs), but burns a search call on a no-op. Explicit skip is preferable.

---

## 3. Sequencing: Three Tiers and the Failure Cascade

### 3.1 The Three-Tier Structure for This Engine

The engine's current two-tier design:
- Tier 1: `(prev - 50, prev + 50)` — first try
- Tier 2: `(L, +∞)` or `(-∞, L′)` — asymmetric full-window fallback

The proposed three-tier design:
- Tier 1: `(prev - 50, prev + 50)` — unchanged
- Tier 2 (new): `(L, prev + W_int)` on fail-high, or `(prev - W_int, L′)` on fail-low
- Tier 3: `(L₂, +∞)` on fail-high, or `(-∞, L₂′)` on fail-low — same as current tier 2

Where `W_int` is the intermediate half-width (e.g., 150 cp), `L₂` is the tier-2 fail-soft return.

### 3.2 The Full-Window Before Intermediate Order

No literature source documents placing the full-window search before the intermediate widening step. The universally documented order is: narrow → intermediate → wider → full. The reason: an intermediate search with TT priming from the first tier is cheaper than a cold full-window search, so the intermediate tier saves nodes if it succeeds.

### 3.3 Three Tiers in Practice

Documented examples with more than two tiers:

| Engine / Source | Schedule (cumulative failing-side half-widths) | Outcome |
|---|---|---|
| Meesha (t=78910) | 18 → 38 → 138 → 463 → ∞ (4 tiers + full) | Not SPRT-quantified |
| Unknown (CPW mention) | 100 → 200 → 300 → ... → 1000 → 2000 → 5000 → ∞ | Not SPRT-quantified |
| MadChess 3.0 beta | 100 → 500 → ∞ (3 tiers with full) | Net **−9 Elo** (removed) |

No source documents an A/B SPRT comparing two-tier against three-tier directly.

---

## 4. Expected EV Analysis

### 4.1 The Condition for a Profitable Intermediate Tier

The M4.D research (§3 "Why Two Tiers, Not Three") identified the condition:

`c_intermediate < p × c_full ≈ 0.6 × c_full`

Where:
- `c_intermediate` = cost of the intermediate-tier re-search (nodes × time)
- `c_full` = cost of the full-window re-search (already in tier 3 / current tier 2)
- `p` ≈ probability the intermediate tier succeeds (saves the full-window re-search)
- The 0.6 factor comes from: with good TT priming, a full-window re-search at depth D costs ~60% of a cold full search (TT provides move ordering and some cutoffs)

At the engine's current state (TT + killers + history + LMR + NMP + RFP + FFP + SE + qsearch-in-TT), the TT is well-primed after tier 1. The intermediate tier re-search should benefit substantially from the tier-1 TT entries.

### 4.2 What the Literature Reports on EV

No published SPRT numbers exist for two-tier vs. three-tier comparison. The available evidence:

- Meesha (t=78910) reports using multi-step widening ("±18 → ±38 → ±138 → ±463 → ∞") but provides no Elo data.
- MadChess's removal of a three-tier schedule (100, 500, ∞) yielded +9 Elo; this is the opposite of the expected EV argument.
- The brtzsnr evaluation study (18,500 games, t=55117) found "W:5350 L:5373 D:7777 T:18500 LOS:0.4121 ELO:-0.43±3.68" — statistically indistinguishable from zero for an aspiration window algorithm change (though the specific change was not two-tier vs. three-tier).

**Finding**: the literature lacks a controlled comparison. The EV analysis remains theoretical. The roadmap's own note — "SPRT may come back inconclusive; that's an expected outcome" — correctly characterizes the uncertainty.

### 4.3 Factors Favoring the Third Tier for This Engine

- High TT hit rate at depth ≥ 6 (M4.A + M5.F qsearch-in-TT means the TT is dense)
- Killers preserved across re-searches (M4.D, §12.7) — reduces intermediate re-search cost
- History from tier-1 persists — further reduces intermediate re-search cost
- Engine reaches depth ≥ 6 reliably at mixed TCs (M5.G SE_MIN_DEPTH = 6 fires regularly)

These factors push `c_intermediate` down, making the intermediate tier more likely to be profitable.

### 4.4 Factors Against

- Score volatility at depth 6–12: the engine is in the NMP+LMR+RFP+SE regime, which can produce deeper horizon-effect jumps between iterations. First-tier failures may carry large excess (L >> prev + 50), which degrades the intermediate tier's success rate.
- MadChess precedent: the removal of three tiers gained +9 Elo, consistent with yo-yo instability dominating the savings.
- The M5.H2 lazy quiet sort failure (described in CLAUDE.md) demonstrated that techniques benefiting slow-TC deep searches can regress at fast TC. The same TC-bimodal pattern is a plausible risk for the intermediate tier.

---

## 5. Yo-Yo and Instability Patterns in the Three-Tier Case

### 5.1 Two-Tier Yo-Yo (Established)

The two-tier yo-yo pattern documented in the M4 research:
1. Tier 1 `(prev - 50, prev + 50)` fails high → return `L`
2. Tier 2 `(L, +∞)` fails low → return `L' < L`
3. The true score was between `prev - 50` and `L` — both tiers wasted

This is a documented pattern but is logically self-limiting: `L' < L` at tier 2 contradicts the tier-1 lower-bound proof only if there is a TT inconsistency (stale entries from a prior iteration's failed search at a different depth). With correct TT bound handling (Lower/Upper/Exact per ADR-0018), this should not occur. In practice it can occur when search instability produces different trees under different windows.

### 5.2 Three-Tier Yo-Yo (Extension)

The three-tier case adds a second opportunity for oscillation:
1. Tier 1 fails high → return `L₁`
2. Tier 2 `(L₁, prev + W_int)` fails low → return `L₂ < L₁`
3. Tier 3 `(-∞, L₂)` — wait, this can't be right if tier 1 proved `score ≥ L₁`

Actually, the three-tier yo-yo with consistent proved bounds is impossible:
- If tier 2 uses `alpha = L₁` (proved lower bound from tier 1), then a fail-low from tier 2 would mean `score < L₁` — contradicting tier 1's proof.
- This can only happen under search instability (different subtrees explored at different windows), not under logical correctness.

The instability risk is not from yo-yo in the logical sense but from **inconsistent trees**: tier 2 explores a somewhat different subtree (due to different TT cutoff patterns under the narrower window) and returns a score that conflicts with tier 1's bound. This is the documented source of MadChess's observed regression.

### 5.3 Three-Tier Cascade Instability

A more realistic failure mode: the intermediate window `(L₁, prev + W_int)` succeeds but returns a score close to `prev + W_int - 1` (just inside beta). This triggers the engine to store the result and proceed, but the score is highly unstable — the next depth iteration starts from this near-beta score and immediately fails high again, multiplying the re-search cost across iterations.

Mitigation from the literature: **cap the number of tiers** (2–3 before full-window) rather than using an unbounded loop. CPW explicitly recommends:

> "It is advisable to cap the widening after 2-3 attempts, reverting to a full-width search (alpha = -∞, beta = +∞) to ensure completeness." — [CPW via grokipedia summary](https://grokipedia.com/page/aspiration_window)

The three-tier design (first try + intermediate + full) satisfies the 2–3 cap recommendation.

---

## 6. Mate-Score and Edge-Case Handling

### 6.1 Two-Tier Already Handles Mate Correctly

From the M4 research (§7.2): the asymmetric two-tier handles mate naturally via fail-soft. A mate score from tier 1 exceeds any finite beta, triggering tier 2 `(L, +∞)` with `L = MATE_SCORE`. Tier 2's window `(MATE_SCORE, +∞)` is immediately correct — the search finds the mate and returns it.

### 6.2 Three-Tier Adds One New Mate Scenario

With an intermediate tier at `W_int = 150 cp`:
- Tier 1 first try `(prev - 50, prev + 50)` fails high with `L = 29992` (mate in 4)
- Tier 2 attempts `(29992, prev + 150)`
- Since `29992 >> prev + 150` for any non-mating `prev`, the intermediate window `(29992, prev + 150)` is inverted: `alpha > beta`

This inverted window requires the explicit skip rule: if `L ≥ prev + W_int` after tier-1 fail-high, skip tier 2 and go directly to tier 3. Without the skip, the search issues an inverted-window call to negamax, which is either a logic error or returns immediately under fail-soft (returning `alpha = 29992` since `alpha > beta`).

From t=41870, Engin documents a practical guard:

> "If the alpha or beta change at a score of mate I will search with full window." — Engin, TalkChess t=41870

This is a slightly more conservative version of the same principle: any mate in the window (not just inverted window) triggers full-window. The recommended rule for this engine: check `if returned_score.abs() >= MATE_IN_MAX_PLY` after tier 1 → skip intermediate tier → fall through to tier 3 (full-window).

**This is a load-bearing difference from the two-tier design.** The two-tier design did not require this guard because tier 2 was already full-window. The intermediate tier requires it.

### 6.3 Fail-Low Mate Symmetry

If tier 1 fails low with `L' = -(MATE_IN_MAX_PLY)`, the intermediate tier `(prev - W_int, L')` similarly inverts if `prev - W_int > L'`. Same guard applies: detect mate score, skip intermediate tier.

---

## 7. Tuning Landscape

### 7.1 Width Range for the Intermediate Tier

Given the engine's first-try width of ±50 cp, the intermediate tier's failing-side beta should cover the range where tier-1 failures typically occur but tier-3 (full-window) is overkill. The literature data points:

| Source | Intermediate width (on failing side from first try) | Notes |
|---|---|---|
| Crafty (CPW) | +75 cp from first try beta (±25 first try → +100 intermediate beta) | Pawn-scale target |
| Meesha (t=78910) | +20 cp from first try beta (±18 first try → +38 intermediate) | Very small step |
| MadChess (t=76115) | +50 cp (but first try at ±100 cp — so intermediate beta at +150 from prev) | Three-tier total |

For this engine (±50 cp first try), the natural analogues:
- **Crafty-proportional**: intermediate beta at `prev + 150` cp (3× the first-try half-width)
- **Meesha-proportional**: intermediate beta at `prev + 70` cp (first-try + 40% step, very narrow — high instability risk)
- **MadChess-derived**: intermediate beta at `prev + 100` cp (2× first-try half-width)

The ±150 cp recommendation from the task brief is at the Crafty-proportional end. The ±100 cp is a reasonable alternative. The ±70 cp range is likely too narrow (high failure rate, many tier-2 re-searches with minimal benefit over first try).

### 7.2 Is Depth-Dependent Intermediate Width Recommended?

From the literature: not documented with SPRT evidence. The motivation exists (deeper depths have lower score volatility, so a narrower intermediate window is appropriate), but it doubles the tuning surface.

The m4 research note §9 (Adaptive Width Strategies) applies here: depth-dependent width adds a scaling factor to tune alongside the base intermediate width. Without a validated baseline, this is premature. Recommendation: fix `W_int` at a single value, SPRT, and defer depth-dependent tuning to a later pass if the intermediate tier lands.

### 7.3 Tunability of the Intermediate Width

| Width | Characterization |
|---|---|
| `prev + 75` cp (half-pawn) | Aggressive; high tier-2 failure rate; risk of cascading re-searches |
| `prev + 100` cp (one pawn) | Conservative entry point; analogous to MadChess's first intermediate step |
| `prev + 150` cp (1.5 pawns) | Crafty-proportional; balanced |
| `prev + 200` cp (two pawns) | Wide; tier 2 rarely fails after tier 1; minimal node saving over full-window |
| `prev + 300 cp+` | Effectively full-window for typical positions; no benefit |

The ±150 cp recommendation from the brief is well-supported as a starting point.

---

## 8. Concrete Recommendation

### 8.1 Design Point

For this engine at its current development state (M5.H1, ~2650 Elo, typical TC depths 8–12, mixed-TC SPRT as gate), the most defensible design:

**Three-tier schedule:**
- Tier 1 (unchanged): `alpha = prev - 50, beta = prev + 50`
- Tier 2 (new): on fail-high with return `L`: `alpha = L, beta = prev + 150`; on fail-low with return `L'`: `alpha = prev - 150, beta = L'`
- Tier 3 (unchanged): on tier-2 fail-high with return `L₂`: `(L₂, +∞)`; on tier-2 fail-low with return `L₂'`: `(-∞, L₂')`

**Proved-bound preservation:**
- Tier 2 `alpha = L` (fail-soft return from tier 1), not `alpha = prev`
- Tier 2 `beta = prev + 150` (fixed intermediate width added to the original center, not to the tier-1 return)
- Tier 3 `alpha = L₂` (fail-soft return from tier 2)

**Mate-score / inverted-window guard:**
- After tier 1: if `|L| >= MATE_IN_MAX_PLY` → skip tier 2, go directly to tier 3
- After tier 1: if `L >= prev + 150` (tier-2 window is inverted) → same skip
- These two conditions cover all edge cases (the second subsumes the first when mate is a fail-high, and is the symmetric fail-low case)

**Cap:** Three tiers total (tier 1 + tier 2 + tier 3); tier 3 is always full-window on the failing side — guaranteed to succeed under correct negamax.

**Symmetry:** Asymmetric on the failing side (proved-bound preserved on the non-failing side), symmetric in structure for fail-high vs. fail-low.

**No depth-dependent intermediate width** at this stage — adds tuning surface, no prior evidence of payoff.

### 8.2 Why ±150 cp Specifically

- Proportional to Crafty's documented schedule (which is the only cleanly described multi-step widening in CPW with a named source)
- Covers the range of "one significant positional swing": a pawn + half-pawn
- Wide enough that the inverted-window case (tier-2 window inverted after large first-tier miss) is rare for non-mate positions
- Narrow enough that the intermediate tier has meaningful probability of succeeding (true score likely within `(L, prev + 150)` if the first-try failure was in the ±50–100 cp range)

### 8.3 Why Not Score-Relative Recentering (Family C)

Family C (Crafty-style recentering: `(returned - D, returned + D)` with small `D`) is not recommended. The CPW note explains why:

> "due to search instability in real engines, attempting [a narrower re-search like `(g + 1/4, g + 1)`] often produces unreliable results, making it preferable to preserve one bound and expand the other." — [CPW Aspiration Windows](https://www.chessprogramming.org/Aspiration_Windows)

A symmetric window centered on the returned score re-opens the proved-empty region below `L` (on fail-high). This wasted work is exactly the "wrong side widened" pitfall documented in the M4 research.

### 8.4 Expected SPRT Outcome

The M5.I roadmap entry says: "SPRT may come back inconclusive; that's an expected outcome." This is consistent with the literature. The only published three-tier data point (MadChess) showed net regression; the technique lacks a published positive SPRT. The recommendation is to run the SPRT and treat inconclusive as a valid outcome (no-change retrospective). Given the TC-bimodal pattern seen in other M5 features (M5.H2, M5.G v1), the mixed-TC SPRT design is essential — a fast-TC-only SPRT risks a false-negative.

---

## 9. Open Questions

1. **Inverted-window skip condition:** The literature does not give an explicit formula for when to skip the intermediate tier because the first-try return `L ≥ prev + W_int`. The recommended formulation is `if L >= beta_int` (where `beta_int = prev + W_int`), skip tier 2. This is derivable from the asymmetry principle but not explicitly stated anywhere in the literature.

2. **Symmetry of intermediate width on both sides:** The design point above uses a symmetric intermediate structure (±150 cp from `prev` on both sides). An asymmetric structure (e.g., +150 on fail-high, +100 on fail-low) is theoretically motivated if score distribution around `prev` is asymmetric (fail-highs more common than fail-lows at depth ≥ 6). The literature does not address this for intermediate tiers specifically.

3. **SPRT validated three-tier vs. two-tier:** No published controlled comparison exists. The MadChess removal of a three-tier schedule is the only data point, and it was a three-tier → zero removal, not three-tier vs. two-tier.

4. **TC-bimodal risk:** The M5.H2 rejection showed that features with a "TT freshness" dependency (history quality at depth) exhibit TC-bimodal behavior. The intermediate tier has a similar dependency: TT priming quality from tier 1 determines tier-2's cost. Whether this produces bimodal TC behavior is unknown without running the SPRT.

---

## Source List

- [CPW — Aspiration Windows](https://www.chessprogramming.org/Aspiration_Windows)
- [CPW — PVS and Aspiration](https://www.chessprogramming.org/PVS_and_Aspiration)
- [CPW — Fail-High](https://www.chessprogramming.org/Fail-High)
- [TalkChess t=78910 — Aspiration Window Instability?](https://talkchess.com/viewtopic.php?t=78910)
- [TalkChess t=76115 — Are Aspiration Windows Worthless?](https://talkchess.com/viewtopic.php?t=76115)
- [TalkChess t=46624 — Aspiration window - effect? Issue with hashtables?!](https://www.talkchess.com/forum/viewtopic.php?topic_view=threads&p=499768&t=46624)
- [TalkChess t=41870 — Aspiration windows](https://talkchess.com/viewtopic.php?t=41870)
- [TalkChess t=65589 — Aspiration window problem](https://talkchess.com/viewtopic.php?t=65589)
- [TalkChess t=84124 — aspiration windows](https://talkchess.com/viewtopic.php?t=84124)
- [TalkChess t=55117 — Evaluating aspiration window algorithm changes](https://www.talkchess.com/forum3/viewtopic.php?t=55117)
- [TalkChess t=69079 — How to solve Transposition Table with Aspiration Window?](https://talkchess.com/viewtopic.php?t=69079)
- [MadChess — Remove Aspiration Windows (2020)](https://www.madchess.net/2020/12/20/madchess-3-0-beta-4b7963b-remove-aspiration-windows/)
- [Beowulf Chess Theory — Aspiration Windows](http://www.frayn.net/beowulf/theory.html)
- [Shams, Kaindl, Horacek — "Using Aspiration Windows for Minimax Algorithms" (IJCAI 1991)](https://www.ijcai.org/Proceedings/91-1/Papers/031.pdf)
- [grokipedia — Aspiration window summary](https://grokipedia.com/page/aspiration_window)
