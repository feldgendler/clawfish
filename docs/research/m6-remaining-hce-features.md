# M6 Remaining HCE Features — Prior-Art Research

**Topic:** Is clawfish's classical HCE feature set complete enough, and is any remaining HCE feature-engineering worth doing before NNUE (M12)?

**Date:** 2026-05-19

**Source-code restriction honored:** All findings derive from the Chess Programming Wiki, blog posts (MadChess dev blog, Dogestamp, rofchade), TalkChess forum discussions, Larry Kaufman's published writing, the Stockfish NNUE blog post, Texel tuning methodology documentation, and academic papers. No engine source repositories were read (Stockfish, Leela, Ethereal, any Rust engine, or any other `src/`).

**Milestone-renumber note (2026-05-19, post-acceptance):** This note's Priority-1 recommendation was accepted. The three recommended features (outposts, rook-on-open/semi-open-file, endgame scaling) are now the **new `M6.F`** feature phase; the joint Texel pass — referred to throughout the original analysis below as "M6.F" — was **renumbered to `M6.G`**. The milestone-code labels below have been relabelled M6.F→M6.G for the Texel pass accordingly (the analysis and sources are unchanged); "before/pre-M6.G" now reads as "in the new M6.F feature phase, before the M6.G joint Texel pass." See `docs/roadmap.md` §M6 and CLAUDE.md "Current status".

**Milestone-split note (2026-05-19):** The `M6.G` above was subsequently **split**: new `M6.G` = corpus construction (a reusable labeled-position data-infra phase, data-quality gate not SPRT), and `M6.I` = the joint Texel pass. The analysis below is unchanged; its Texel-pass forward-labels have been mechanically relabelled M6.G→M6.I (so "before/pre-M6.G" now reads "in the new M6.F feature phase, before the M6.I joint Texel pass, which consumes the frozen M6.G corpus"). No new corpus content is authored in this historical research note. See `docs/roadmap.md` §M6 (M6.G + M6.I scope detail) and CLAUDE.md "Current status".

---

## 1. Standard Pre-NNUE HCE Feature Taxonomy

The field converges on a roughly ordered tier list, roughly correlated with Elo contribution at an intermediate engine strength (~2400–2700 CCRL):

**Tier 0 — Mandatory foundation (already in clawfish)**
- Material balance
- Piece-square tables (MG + EG tapered)
- Passed pawns
- Basic pawn structure (isolated, doubled, connected)
- Piece mobility (N/B/R/Q)
- King safety (zone attackers, pawn shield, open file)
- Bishop pair

**Tier 1 — Standard completions (high ROI, near-universal)**
- Rook on open/semi-open file
- Rook on 7th rank (conditional)
- Outposts (knight-centric, pawn-supported)
- Endgame scaling / draw-dampening (OC bishops, material-imbalance reductions)
- Kaufman-style material adjustments (knight/rook pawn-count scaling)

**Tier 2 — Common but second-order (modest ROI, meaningful interaction with Tier 1)**
- Threat evaluation (hanging piece / attack bonus)
- Doubled rooks on open file
- Bad bishop / trapped bishop specific cases
- Space evaluation

**Tier 3 — Specialist / diminishing returns pre-NNUE**
- Pawn storm
- Tempo bonus
- King-file opposition / backward major piece penalties
- Advanced pawn blockade patterns

**Where clawfish currently sits:** Tier 0 is complete (all weights zeroed pending M6.I Texel, but infrastructure present). Tier 1 is partially absent. Tier 2 and 3 are largely absent.

---

## 2. Per-Feature Analysis

### 2.1 Outposts

| Dimension | Finding |
|---|---|
| Literature-attested Elo range | ~20–30 Elo. Rebel: removing outpost eval caused ~28 Elo drop ([TalkChess t=42918](http://www.talkchess.com/forum3/viewtopic.php?t=42918&start=10)). MadChess 2.0 Beta: +25 Elo for knight-outpost bonus on 5th/6th rank ([MadChess knight outpost post](https://www.madchess.net/2021/01/10/knight-outpost/)). CPW quotes 10–16 cp bonus per pawn-supported outpost piece ([CPW Outposts](https://www.chessprogramming.org/Outposts)). |
| Implementation cost/complexity | Low-moderate. Bitboard predicate: a square in the enemy half, not attackable by enemy pawn front-span (`enemy_pawn_attacks_spans` already computed in M6.B infrastructure), occupied by own N or B, defended by own pawn. Reads M6.B's `pawn_attack_spans` fills and the `file_fill` helper already present. Can live in `evaluate_core` as a live term (not pawn-hash-cached — king-independent, but relies on pawn positions; could be pawn-hash-cacheable since it is a function of pawn+piece interaction, but the piece component makes it borderline). Per-square rank-scaled bonus table, ~20–40 LOC. |
| Overlap with existing features | **Moderate.** PeSTO PSTs award N/B centralization bonuses at ranks 4–6 center squares, which partially prices the outpost concept without the pawn-supported condition. M6.D mobility already rewards pieces with high move-count (an outposted knight has decent mobility). The outpost term adds the *specific* condition of pawn-support + pawn-unchallengeable — a discrimination the PST/mobility combo cannot make. |
| NNUE survival | **Does not survive.** NNUE implicitly learns the outpost concept from game data. At M12 this becomes dead weight. However, clawfish's STS theme #3 "Knight Outposts" measured **−40** on the rejected M6.D live-mobility config — a measured strategic blindspot. This is not a pure theoretical gap; it is empirically confirmed. The negative STS score means the *existing* mobility-weighted eval actively misleads on outpost positions, likely because the mobility term rewards pieces that are not pawn-supported but mobile, at the expense of pawn-supported outposted pieces whose mobility count happens to be average. |
| NNUE-obsolescence verdict | Pure throwaway pre-M12. **However**: the −40 STS score means it is a directional correctness gap the M6.I Texel pass alone cannot fix (Texel finds optimal weights for existing features; it cannot add a discriminator that does not exist). The gap is likely to persist into NNUE training: a weak teacher signal on outpost positions produces weak NNUE endgame/middlegame generalization in positions that are structurally outpost-dependent. |

**Recommendation flag:** The measured −40 STS score makes this distinctly above theoretical. It is the single externally-measured weakness in the current feature set. See section 5 for priority ordering.

---

### 2.2 Rook Placement

| Feature | Elo range | Implementation cost | Overlap | NNUE survival |
|---|---|---|---|---|
| **Rook on open file** | Literature: 8–20 cp bonus ([CPW Rook on Open File](https://www.chessprogramming.org/Rook_on_Open_File)); ~15–25 Elo aggregate with semi-open. Rival/dailychess example: 10 cp open, ~5 cp semi-open. | Low. `file_fill(rooks) & ~(file_fill(own_pawns) | file_fill(enemy_pawns))` uses M6.B's `file_fill` helper already present. 1–2 lines of bitboard arithmetic per color. | **Low.** PeSTO rook PST has modest file-position bias but no per-file-open discriminator. M6.D mobility counts moves but not the file-open condition. Low interaction risk. | No — NNUE replaces. But low-risk, low-cost addition. |
| **Rook on 7th rank** | CPW: only when enemy pawns on 7th or enemy king on 8th — else PST already prices rank position ([CPW Rook on Seventh](https://www.chessprogramming.org/Rook_on_Seventh)). Rival example: 20 cp. | Low-moderate. Conditional on pawn/king configuration (gating avoids PST double-count). | **Moderate double-count with PST** if applied unconditionally — the PeSTO rook EG PST already has rank-7 bonus. Gate on "enemy pawns present on 7th OR enemy king on 8th" eliminates the double-count (CPW recommendation). | No — NNUE replaces. |
| **Doubled rooks** | Rival example: 15 cp bonus. CPW: "some programs" add bonus for doubling on open file. | Trivial. `popcount(rooks & file_of_rook) > 1`. | Low. Neither PST nor mobility specifically prices rook coordination. | No — NNUE replaces. |

**Open-file note:** The M6.D STS screen showed "Open Files and Diagonals" (#2) gaining +90 when the full mobility term was live — the rook open-file concept is directionally captured by mobility count, but the dedicated open-file predicate adds the direct signal. The M6.I Texel pass will re-scale mobility, which may partially capture the open-file benefit; a dedicated rook-on-open-file term after M6.I tuning provides additional discrimination.

---

### 2.3 Material Imbalance (Kaufman-Style)

| Dimension | Finding |
|---|---|
| Key terms | Knight value: +1/16 pawn per pawn above 5 present. Rook value: −1/16 per pawn above 5 (converse: rooks gain as pawns vanish). Bishop pair: ~+0.5 pawn, declining on crowded board. Exchange value: 1.75 pawns (not the traditional 2). Rook-pawn: −0.15 vs average pawn. ([Kaufman 1999](https://www.chess.com/article/view/the-evaluation-of-material-imbalances-by-im-larry-kaufman)) |
| Elo range | No direct SPRT citation in the literature. Kaufman's original work states "a pawn advantage works out to about 200 Elo at master level." The CPW Material Tables article describes precomputed tables below 0.25 MB with ≥99% hit rate — the implication is the technique is worth implementing for engines expecting imbalance positions. The CPW-Engine eval already includes per-pawn-count knight/rook adjustment arrays (`knight_adj[-20..+12]`, `rook_adj[+15..-9]`). |
| Implementation cost | Moderate. Two 9-entry adjustment arrays (knight and rook, indexed by pawn count) plus bishop-pair scaling. Clawfish's bishop pair is already in M6.A. The pawn-count adjustment reads `popcount(all_pawns)` at eval time — trivial. No pawn hash dependency. Main cost is integration into the incremental `static_eval_white` accessor vs. `evaluate()` only. |
| Overlap | **Moderate.** M6.A already has bishop pair. The knight/rook pawn-count adjustment overlaps with M6.D mobility (fewer pawns → more open files → rook mobility rises anyway; the explicit pawn-count scaling is an additional signal on top). Low risk of Texel conflict. |
| NNUE survival | No — NNUE learns imbalances from data. **But**: this is also a *correctness* improvement: clawfish's current material assessment gives equal weight to a rook vs. a bishop regardless of pawn count. This distorts search-side pruning gates (RFP, NMP, FFP all use `static_eval_white`). Kaufman-style adjustments are partially a correctness fix for the pruning infrastructure, not just a positional eval bonus. |

**Open question:** The Kaufman knight/rook pawn-count adjustment would enter `static_eval_white()` or `evaluate()` — that boundary choice (which M6.A–E establish as the `evaluate()` side for non-PST terms) has implications for the pruning gates. This would need architectural resolution before implementation. The correctness argument for it is stronger than the raw Elo argument.

---

### 2.4 Endgame Scaling Factors / Opposite-Colored Bishop Drawishness

| Dimension | Finding |
|---|---|
| What it is | A multiplicative score reduction applied when a position is structurally drawn or very difficult to win: KBvKB-same-color (already in clawfish M6.A), opposite-colored bishops with pawns, pawnless draws (KNN vs K, etc.), and the 50-move proximity taper. |
| Elo range | MadChess: +12 Elo from implementing IsPawnlessDraw + DetermineEndgameScale + 50-move taper ([MadChess endgame scaling post](https://www.madchess.net/2021/04/08/madchess-3-0-beta-4d22dec-endgame-eval-scaling/)). Small Elo gain, but functionally this is a **correctness feature** — engines miscalibrate draw probability in OCB endgames, leading to misguidance in search. |
| Correctness dimension | The CPW draws-of-opposite-bishops article notes that engines "evaluate many drawn positions as ±1 because they only count material." NNUE-boosted Stockfish fixed this; a pure HCE engine without scaling actively misleads search in these positions. Unlike most eval features, draw scaling is **not purely an Elo-chasing exercise** — it corrects a structural search error where the engine pursues "winning" plans that are objectively drawn. |
| Implementation cost | Moderate. Pattern matching on material configuration (piece counts by type) + a multiplier on the final blended eval. OCB condition: `opp_bishops` and `(both_bishops & LIGHT_SQUARES).count() == 1`. No new infrastructure needed. |
| NNUE survival | **Partially survives.** NNUE learns drawn positions from data, but the draw-scaling of search (scaling the propagated eval score, not just the leaf eval) provides a correctness signal that NNUE does not automatically deliver at the search integration layer. Many NNUE engines still implement explicit draw scoring for known-drawn endgame patterns. This is one of the few HCE features that is **not pure throwaway pre-M12**. |

**Key distinctions:**
- KBvKB-same-color: already in clawfish (M6.A, `is_insufficient_material`).
- 50-move proximity taper: low-cost, correctness-grade.
- OCB with pawns scaling: the main missing case; ~5–15 cp structural correction.

---

### 2.5 Threats / Hanging-Piece Terms

| Dimension | Finding |
|---|---|
| Elo range | MadChess 3.1 Beta: **+7 Elo** from adding threat evaluation (pawn/minor threatening more valuable piece on next move). ([MadChess threats post](https://www.madchess.net/2021/10/24/madchess-3-1-beta-26e5323-threats/)) This is a low figure relative to other features. |
| "Search already finds this" overlap | **Very high.** The standard argument (supported by MadChess's own implementation notes): quiescence search resolves captures, so a hanging piece at depth N is found by the search before the eval ever fires on the threatened position. The eval bonus helps primarily at shallow depths or in positions where the threat is on the horizon of qsearch. At depth ≥ 5 (typical for clawfish mid-game positions), search discovers threats within a few plies; the eval bonus is redundant in most cases. The CPW "Hanging Piece" article confirms this: the threat eval is most useful at shallow-search regimes where qsearch doesn't fully resolve the position. |
| Pre-NNUE ROI | Low. At clawfish's current search depth (M5's deep pruning infrastructure means effective depth is high relative to node budget), the ROI on threat evaluation is near the bottom of Tier 2. The +7 Elo MadChess figure was achieved at a regime (C# engine, lower effective depth) where threats were more frequently off the search horizon. |
| NNUE survival | No. NNUE implicitly scores threats through pattern recognition. |
| Recommendation | Skip pre-NNUE. The search already resolves most threats, and the implementation cost exceeds expected return at clawfish's current depth regime. |

---

### 2.6 Lower-Priority Features

| Feature | Elo range | Cost | Notes |
|---|---|---|---|
| **Space** | Small. Stockfish uses safe-squares-on-central-files weighted by piece count minus open files — a complex formula. No SPRT-quoted Elo figure found. | Moderate. | Partially captured by mobility. Low priority. |
| **Bad bishop** | Anecdotal: small negative to neutral. One TalkChess developer tried a tripled-pawn penalty "and the engine became weaker" — the bad-bishop pattern is similar: the penalty is hard to isolate from existing PST + mobility overlap. | Moderate. | Can penalize bishops blocked behind own pawns. Risk of double-counting with mobility (a bad bishop has low mobility, already penalized). |
| **Trapped bishop/rook** | Specific pattern penalties (bishop on a7/h7/a2/h2 in CPW-Engine eval). CPW-Engine includes `P_BISHOP_TRAPPED_A7` etc. | Low. Pattern match. | A correctness fix for known worst-case positions the PST doesn't capture. Low Elo but avoids gross misevaluation. |
| **Tempo bonus** | CPW: "some programs give a small bonus for side to move." Very small. | Trivial. | Minimal value at clawfish's current TC regime. |

---

## 3. NNUE-Obsolescence ROI Synthesis

### 3.1 The community verdict (post-2020)

The community consensus since Stockfish's NNUE integration (2020) is nuanced:

- **NNUE delivers ~80–300 Elo over the equivalent HCE** in the Stockfish regime (Stockfish 16 NNUE vs HCE: ~295 Elo per [TalkChess t=84921](https://talkchess.com/viewtopic.php?t=84921); initial SF NNUE: +80–90 Elo at introduction per [Stockfish NNUE blog](https://stockfishchess.org/blog/2020/introducing-nnue-evaluation/)). The +295 figure reflects years of NNUE development; the initial transition was +80–90 Elo.
- **"HCE is dead if you want Elo"** (Andrew Grant, TalkChess) — but this refers to *frontier* engines competing at 3700+ Elo. For an engine in the 2400–2800 range, the situation is different: NNUE advantage at lower strength is smaller because the search at shallow depth benefits less from NNUE's positional depth.
- **Texel tuning's implicit message** ([CPW Texel's Tuning Method](https://www.chessprogramming.org/Texel%27s_Tuning_Method)): "the algorithm can be expected to improve the engine's knowledge about things that it *partially* already knows." Tuning cannot discover features that are absent. Adding features before tuning is therefore prerequisite to getting full tuning value from those features.
- **"Tune then extend" vs "extend then tune"**: The Texel article and TalkChess practitioners are clear: adding features and then re-running Texel is the correct loop because Texel cannot substitute for absent features. However, tuning an incomplete feature set first and then adding features requires re-tuning — the second Texel run co-calibrates everything. Marcel Vanthoor's advice ("get all 'classic' techniques right, tested, and verified, THEN tune") describes the *extend-then-tune* order at the milestone level.

**Synthesis:** The correct order is **extend feature set to a target completeness level, then Texel-tune all weights jointly**. This is exactly clawfish's M6.A–E → M6.I plan structure (with the new M6.F Tier-1 feature phase inserted before M6.I — the very recommendation this note makes). The question is whether there are missing Tier-1 features that should enter the joint Texel pass.

### 3.2 Where clawfish's existing surface sits on the diminishing-returns curve

The standard progression:
1. Material + PST: 2000–2200 CCRL Elo range per CPW.
2. + Mobility + King safety + Pawn structure + Passed pawns: ~2400–2600 range.
3. + Rook placement + Outposts + Endgame scaling + tuned together: ~2600–2800 range.
4. + Threats + Space + Bad bishop etc.: marginal gains at this tier; NNUE starts delivering better ROI.

Clawfish's post-M6.I position (material + PST + mobility + pawn structure + passed pawns + king safety, all jointly Texel-tuned) corresponds to the **2–3 tier transition**. The features absent from that pass that could add meaningful strength before the NNUE transition are primarily rook placement and outposts — both Tier-1 items.

### 3.3 Is pre-M12 HCE extension worth doing?

The argument *for* extending HCE before NNUE:
- NNUE training needs a **teacher signal** (the HCE eval, or self-play guided by HCE). A HCE that systematically misjudges outpost positions generates training data that under-represents outpost positions' value — the NNUE inherits the teacher's bias.
- Rook placement, outposts, and endgame scaling are **structural correctness features** as well as Elo features — they eliminate categories of systematic eval error that Texel tuning cannot fix.
- The cost of adding these features is low (rook open file: ~30 LOC; outposts: ~50 LOC; OC bishop scaling: ~30 LOC), whereas the cost of a Texel tuning run is the same whether the feature set is smaller or larger.
- Extending *after* M6.I would require a second Texel pass (re-tuning all weights again), which is more expensive than including the features in the first joint pass.

The argument *against*:
- Rook open file is partially captured by M6.D's slider mobility (a rook on an open file scores higher mobility). The marginal discriminative value post-M6.I Texel is smaller than it would be on an untuned engine.
- Outpost evaluation introduces its own PST double-count axis (PeSTO N/B PSTs reward centralization; the outpost term adds pawn-support condition on top, which is additive but potentially over-values centrally placed pieces if not co-calibrated).
- Every feature added pre-M6.I increases M6.I's parameter surface and may cause Texel to over-fit or take longer to converge.

**Community consensus verdict:** The threshold for "worth adding before tuning" is approximately: does the feature represent a **structural discrimination** the existing features cannot make even after optimal tuning? By this criterion:
- **Rook on open file**: yes — mobility counts moves, not file-open status.
- **Outposts**: yes — mobility/PST cannot discriminate pawn-supported vs. pawn-unsupported piece placement.
- **Endgame scaling**: yes — draw recognition is a correctness gate, not a tunable weight.
- **Everything else at Tier 2/3**: no — either search already resolves the discrimination, or Texel on the existing surface already captures it.

---

## 4. Tune-Then-Extend vs. Extend-Then-Tune Ordering Verdict

**The correct order for clawfish's situation: extend by the missing Tier-1 features, then run M6.I Texel jointly over the full surface.**

Evidence:
1. Texel cannot discover absent features — it can only calibrate weights for present ones (CPW Texel article).
2. Adding features after Texel requires re-running Texel (the M6.I obligation already mandates a joint pass; adding features pre-M6.I includes them in the one pass rather than forcing a second pass post-M12-ramp-up).
3. The M6.B–D empirical record shows that adding any non-co-calibrated term degrades performance — this is the "extend-then-tune" lesson. The joint Texel pass is the calibration step. If Tier-1 features are added *after* M6.I, they would either ship with wrong weights (same failure mode as M6.B–D) or require a second Texel pass.
4. The exception: if NNUE (M12) is very close on the roadmap and the Tier-1 features are low-confidence (e.g., their interaction with the PeSTO PST baseline is unknown), deferring to NNUE is defensible. Given the measured −40 STS gap for outposts and the structural correctness gap for endgame scaling, these are not "low confidence" additions.

**Ordering recommendation:** Add rook placement + outpost + endgame scaling *before* M6.I (as the new M6.F feature phase), include them in the joint Texel pass, then close M6 at M6.I. This avoids a second Texel pass and co-calibrates the full Tier-0+1 surface in one pass.

---

## 5. Prioritized Bottom-Line Recommendation

### Priority 1 — Add before M6.I (the new M6.F feature phase; include in M6.I's joint Texel pass)

| Feature | Reason | Estimated Elo |
|---|---|---|
| **Outposts (knight/bishop, pawn-supported)** | **Empirically measured −40 STS gap** (M6.D rejected-config diagnostic, theme #3). This is not theoretical — it is a confirmed strategic blindspot. Outpost is a structural discriminator that mobility/PST cannot make: pawn-supported vs. unsupported placement. Low implementation cost (~50 LOC, reads M6.B pawn data already present). Must enter M6.I joint Texel to co-calibrate against PST/mobility overlap. | ~20–30 Elo (literature) |
| **Rook on open/semi-open file** | Structural discriminator: mobility counts moves but not file-open status. Literature: 8–20 cp bonus (open), 4–10 cp (semi-open). Low implementation cost (~30 LOC, uses M6.B `file_fill` helper). M6.D STS showed "+90 Open Files" on live mobility — adding the explicit predicate provides additive discrimination post-Texel. | ~15–25 Elo (estimate) |
| **Endgame scaling (OC bishop + pawnless draws + 50-move taper)** | Correctness feature, not just Elo. Clawfish miscalibrates draw probability in OCB+pawns endgames. MadChess: +12 Elo. KBvKB-same-color already present (M6.A); the missing case is OCB with pawns + material-only pawnless draw patterns (KNN vs K etc.). ~30 LOC. Does not need Texel calibration for the *scaling* coefficient (it is a multiplier on the blended eval, set to ~0.5–0.75 for OCB positions from literature). | ~10–15 Elo |

### Priority 2 — Defer to post-M6.I or skip pre-NNUE

| Feature | Reason |
|---|---|
| **Rook on 7th rank** | Moderate double-count with PeSTO rook EG PST unless gated correctly. The gating condition (enemy pawns on 7th OR enemy king on 8th) makes it safe but raises implementation cost relative to open file. Low marginal value when rook-mobility (M6.D) and rook-on-open-file (Priority 1) are both in place. Defer to post-M6.I empirical screen. |
| **Doubled rooks** | 15 cp example value is within Texel noise relative to other terms; mobility partially captures rook coordination. Skip pre-M12 unless STS screen shows a theme gap. |
| **Kaufman material adjustments (knight/rook pawn-count scaling)** | Medium complexity (architectural question: `static_eval_white` vs `evaluate()`). Correctness value is real (pruning gates use blended eval; knight/rook pawn-count bias distorts NMP/RFP in deep endgames). However, the pawn-count effect is partially captured by the EG/MG phase blend (EG weight increases as material reduces, covering the "rooks gain as pawns vanish" effect implicitly). The incremental benefit above a properly tuned phase blend is unclear. Defer; assess after M6.I results. |
| **Threats / hanging piece** | +7 Elo at best (MadChess data), and that is at a lower effective depth. Skip — search already finds threats. |
| **Space, bad bishop, trapped bishop/rook, tempo** | Tier-3. Skip entirely pre-M12. Space: partially captured by mobility + pawn structure. Bad bishop: double-counts with mobility (low mobility = bad bishop). Trapped bishop: specific a7/h7/a2/h2 pattern penalties are a marginal correction, worth adding only if STS screen shows a specific gap. |

### Priority 3 — Flag as open question (source-code restriction boundary)

**Rook on 7th rank double-count quantification:** The exact magnitude of the PeSTO rook EG PST's rank-7 bonus is an internal implementation detail. Knowing whether the EG PST has, say, +25 cp on rank 7 vs. +10 cp would directly determine the safe size of a dedicated rook-on-7th bonus. This is resolvable by reading clawfish's own `src/eval/data.rs` (in-project), not external engine source. It is flagged here as a required check before implementing rook-on-7th.

---

## 6. Executive Verdict

Clawfish's post-M6.E feature set (material + PeSTO PSTs + pawn structure + passed pawns + mobility + king safety) represents **Tier-0 completion** and sits squarely at the 2–3 tier transition on the HCE diminishing-returns curve. The M6.I joint Texel pass is the correct next step and will deliver meaningful strength — but it will leave a confirmed strategic blindspot (STS theme #3 "Knight Outposts" scored −40 on the mobility-enabled build) and structural correctness gaps (OCB draw scaling, rook-file discrimination) that Texel cannot fix because they require absent features. The evidence-based recommendation is to add three lightweight features — **outpost evaluation, rook-on-open/semi-open-file, and OC bishop endgame scaling** — *before* M6.I runs (as the new M6.F feature phase), so they are co-calibrated in the one joint Texel pass rather than forcing a second pass later or shipping them with uncalibrated weights. This is the "extend then tune" law the M6.B–E experience established empirically. Adding these three features is ~110–150 LOC total, uses infrastructure already present (M6.B pawn fills, `file_fill` helper), and avoids re-spending the Texel campaign. Everything at Tier 2 and below (threats, space, bad bishop, doubled rooks, rook on 7th, Kaufman material adjustments, tempo) should be skipped pre-M12: either search already resolves the discrimination, the Tier-1 features already cover the axis post-Texel, or the NNUE transition makes them immediately obsolete. The NNUE replacement at M12 discards all HCE features, but the three Tier-1 additions are justified not only as Elo-chasers but as correctness improvements to the teacher signal that M12's NNUE training will use.

---

## Sources

- [CPW: Outposts](https://www.chessprogramming.org/Outposts)
- [CPW: Rook on Open File](https://www.chessprogramming.org/Rook_on_Open_File)
- [CPW: Rook on Seventh](https://www.chessprogramming.org/Rook_on_Seventh)
- [CPW: Bishops of Opposite Colors](https://www.chessprogramming.org/Bishops_of_Opposite_Colors)
- [CPW: Material Tables](https://www.chessprogramming.org/Material_Tables)
- [CPW: Evaluation of Pieces](https://www.chessprogramming.org/Evaluation_of_Pieces)
- [CPW: Hanging Piece](https://www.chessprogramming.org/Hanging_Piece)
- [CPW: Evaluation Overlap](https://www.chessprogramming.org/Evaluation_Overlap)
- [CPW: Space](https://www.chessprogramming.org/Space)
- [CPW: Texel's Tuning Method](https://www.chessprogramming.org/Texel%27s_Tuning_Method)
- [CPW: CPW-Engine eval](https://www.chessprogramming.org/CPW-Engine_eval)
- [CPW: NNUE](https://www.chessprogramming.org/NNUE)
- [Larry Kaufman: The Evaluation of Material Imbalances (Chess.com)](https://www.chess.com/article/view/the-evaluation-of-material-imbalances-by-im-larry-kaufman)
- [MadChess: Piece Mobility (+62 Elo)](https://www.madchess.net/2020/02/01/madchess-3-0-beta-5c5d4fc-piece-mobility/)
- [MadChess: Endgame Eval Scaling (+12 Elo)](https://www.madchess.net/2021/04/08/madchess-3-0-beta-4d22dec-endgame-eval-scaling/)
- [MadChess: Threats (+7 Elo)](https://www.madchess.net/2021/10/24/madchess-3-1-beta-26e5323-threats/)
- [MadChess: Knight Outpost (+25 Elo)](https://www.madchess.net/2021/01/10/knight-outpost/)
- [Stockfish: Introducing NNUE Evaluation](https://stockfishchess.org/blog/2020/introducing-nnue-evaluation/)
- [TalkChess: Outpost evaluation thread (Rebel −28 Elo removal)](http://www.talkchess.com/forum3/viewtopic.php?t=42918&start=10)
- [TalkChess: Stockfish 16 NNUE vs HCE (~295 Elo delta)](https://talkchess.com/viewtopic.php?t=84921)
- [TalkChess: I declare HCE is dead (Andrew Grant thread)](https://talkchess.com/viewtopic.php?t=77571&start=10)
- [TalkChess: Evaluation tuning — where to start](https://talkchess.com/viewtopic.php?t=75234)
- [TalkChess: Some more HCE Texel tuning data](https://talkchess.com/viewtopic.php?t=77502)
- [TalkChess: How to get started with NNUE](https://talkchess.com/viewtopic.php?t=83170)
- [Dogestamp: Chess engine pt. 6 NNUE](https://www.dogeystamp.com/chess6/)
- [Zurichess evaluation improvements (Alexandru Mosoi, Medium)](https://medium.com/@brtzsnr/hi-all-a73c1b7b7a73)
