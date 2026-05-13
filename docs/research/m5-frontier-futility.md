# Prior-Art Research: Frontier Futility Pruning (M5.D)

Sources consulted: Chess Programming Wiki (CPW) — Futility Pruning, Frontier Nodes, Pruning, CPW-Engine search, Razoring, Late Move Reductions; Heinz 1998 "Extended Futility Pruning" (ICCA Journal Vol. 21, No. 2) via Semantic Scholar abstract; H.G. Muller's Deep Futility Pruning page; MadChess blog (Erik Madsen, Build 037); Mediocre Chess blog (Jonatan Dahl); Frayn Beowulf theory page; TalkChess threads t=59315, t=63368, t=74403, t=77451, t=77644, t=35955; OpenChess t=3099; Lynx engine DeepWiki documentation; int0x80.ca chess engine blog.

Per ADR-0003, no third-party engine source code was read. All findings come from CPW articles, blog posts, forum threads, and paper abstracts.

---

## TL;DR — Five-Bullet Bottom Line

- FFP is a **per-move skip** inside the negamax move loop at shallow depth: before recursing into a quiet move, check `static_eval + margin(depth) <= alpha`. If true, skip that move — it cannot improve the position enough to matter.
- The canonical gate is: `!is_pv`, `!in_check(pos)`, `depth <= FFP_MAX_DEPTH`, `alpha.abs() < MATE_IN_MAX_PLY`, quiet-only, not a checking move (gives-check exempt).
- **Recommended v1 margin table: 100 / 150 / 250 cp at depths 1 / 2 / 3.** Roadmap's CPW reference. Supported by TalkChess t=74403's successful {100, 150} pair (+25 Elo) and MadChess Build 037's {150, 250} pair (+54 Elo combined). CPW-Engine's {200, 300, 500} is more aggressive.
- **Recommended `FFP_MAX_DEPTH = 2` for v1.** Depth 1 (frontier) is the classically sound scope; depth 2 (pre-frontier / extended futility) is the most widely validated extension. Depth 3 adds modest savings but risks more tactical blindness, especially before LMP-style move-count pruning.
- **FFP prune first, then decide LMR.** The per-quiet decision inside the move loop should check the FFP skip condition before the LMR reduce condition. A pruned move never reaches the LMR path.

---

## 1. What Problem Does FFP Solve

### Position in the M5 Pruning Stack

The M5 pruning stack addresses different regimes of the search tree:

| Technique | Where | Direction | Scope |
|---|---|---|---|
| NMP (M5.A) | Prologue, before move loop | Fail-high (position too good) | Entire subtree |
| RFP (M5.B) | Prologue, before move loop | Fail-high | Entire node |
| LMR (M5.C) | Move loop, per quiet | Neutral (depth reduction) | Individual quiet (reduces) |
| FFP (M5.D) | Move loop, per quiet | Fail-low (position too bad) | Individual quiet (skips) |
| LMP (future) | Move loop, per quiet | Move-count based | Individual quiet (skips) |

RFP is the fail-high test at the node boundary: "if the position is so good after a discount, don't even generate moves." FFP is the fail-low test at the per-move boundary: "if even after adding the maximum positional gain a quiet move could produce, we still cannot reach alpha, skip that move."

### The Core Intuition

A quiet move (no captures, no promotions) cannot improve material. Its maximum effect is positional: some bounded centipawn gain from PSQT, mobility, king safety, pawn structure. If:

```
static_eval + maximum_positional_gain(depth) <= alpha
```

then the move is **futile** — it cannot possibly improve alpha. The `maximum_positional_gain(depth)` estimate is the margin: larger at greater depths because deeper remaining search gives the opponent more opportunities to exploit positional factors.

This is the CPW definition verbatim: "Futility Pruning discards moves that have no potential of raising alpha, which in turn requires some estimate of a potential value of a move." [CPW — Futility Pruning]

### History

The technique dates to Schaeffer 1989 (shallow-quiet pruning at depth 1 in Phoenix), formalised by Heinz 1998 in "Extended Futility Pruning" (ICCA Journal Vol. 21, No. 2), which added depth-2 ("pre-frontier") coverage with a larger margin. The "extended" in Heinz's title refers to extending from depth 1 only (classical frontier) to depth 2 (pre-frontier). Both formulations appear in CPW under "Futility Pruning" with the sub-cases named by node type (frontier = depth 1 parent, pre-frontier = depth 2 parent).

---

## 2. Classical Formulation

### Node Taxonomy

| Node name | Depth | Children are |
|---|---|---|
| Frontier | 1 | Leaf nodes (qsearch entries) |
| Pre-frontier | 2 | Frontier nodes |
| Pre-pre-frontier | 3 | Pre-frontier nodes |

Heinz 1998 applied futility pruning at frontier nodes (depth=1) and "extended futility pruning" at pre-frontier nodes (depth=2). [Heinz 1998 abstract via Semantic Scholar; CPW — Frontier Nodes]

### The Per-Move Test

Before searching a candidate quiet move at depth D, evaluate:

```
if static_eval + margin(D) <= alpha {
    // skip this move
    continue;
}
```

If the condition holds, the move is skipped without recursion. The search continues with the next move. [CPW — Futility Pruning; CPW-Engine search pseudocode]

### Fail-Soft Return Value When All Moves Pruned

If FFP prunes all legal quiet moves and no captures/checks produced a better score, the node returns `alpha` (fail-soft: the best score achieved, which did not improve alpha). This is the natural fail-soft return — there is no special FFP-cutoff return value because FFP is a per-move skip, not a whole-node cutoff.

If at least one move (a capture or check) was searched before all quiets were pruned, the node returns whatever best score those moves produced. FFP does not force a specific return value; it only skips specific moves. [OpenChess t=3099 H.G. Muller; CPW — Pruning]

### No TT Store

FFP is a per-move skip; it does not produce a node bound. Nothing is stored in the TT. The TT entry from the TT probe (step 7) is unaffected. [CPW — Futility Pruning; consensus in all forum sources]

### No Abort / Cutoff

FFP never directly causes a beta cutoff. A beta cutoff can still occur if a non-pruned move (capture, check) returns a score above beta. FFP only prevents certain quiets from being searched; the move loop continues with the next move after each skip.

---

## 3. Margin Formula Choices

The margin represents the maximum centipawn improvement a quiet move can produce before recursing to depth D-1. It must be large enough to avoid pruning moves that could genuinely improve the position, but small enough to actually prune.

### Survey of Documented Values

| Source | Depth 1 | Depth 2 | Depth 3 | Notes |
|---|---|---|---|---|
| Heinz 1998 classical | ~300 (minor piece) | ~500 (rook) | N/A | Piece-value based; original formulation |
| Frayn / Mediocre Chess | ~300 (minor piece) | ~500 (rook) | ~900 (queen) | Extending the piece-value progression |
| CPW-Engine `fmargin[]` | 200 | 300 | 500 | Explicit pseudocode; three-element table `{0, 200, 300, 500}` |
| MadChess Build 037 | 150 | 250 | 400 | Table at {qsearch=100, d=1=150, d=2=250, d=3=400}; +54 Elo combined |
| TalkChess t=74403 | 100 | 150 | 300 | Tested `{0, 100, 150, 300}`; +25 Elo |
| Roadmap M5.D row | 100 | 150 | 250 | "CPW" reference; conservative modern starting point |
| Lynx (SPSA-tuned) | ~100–150 + depth factor | — | — | `FP_Margin + FP_DepthScalingFactor * depth` |

[Heinz 1998 abstract; Frayn Beowulf theory; CPW-Engine search; MadChess Build 037; TalkChess t=74403; roadmap §M5.D; Lynx DeepWiki]

### Analysis by Formula Family

**Piece-value table (Heinz 1998 classical):** Minor piece / rook / queen at depths 1/2/3. Semantically grounded ("absorb one piece per ply") but over-generous at depth 1 with modern PeSTO-style eval (300 cp for a bishop is more than two pawns at depth 1). The modern consensus moved toward smaller constants.

**CPW-Engine `{0, 200, 300, 500}`:** Explicitly documented in pseudocode. 200 cp at depth 1 is moderate — approximately a pawn + knight exchange. 300/500 at depths 2/3 are aggressive. CPW-Engine is a reference implementation; these values are not universally recommended.

**MadChess {150, 250, 400}:** Moderate. Included qsearch futility (delta pruning) at 100 cp. Resulted in +54 Elo combined with the whole table in MadChess 2.0 Beta Build 037. The depth-1 value of 150 is the midpoint of the 100–200 range seen in most discussions.

**{100, 150, 250} / {100, 150, 300}:** The most conservative modern family. TalkChess t=74403 documented +25 Elo with {100, 150} at depths 1–2. The roadmap M5.D row references {100, 150, 250} as the "CPW" workhorse. This is the recommended v1 starting point for clawfish.

**Linear `k * depth`:** Simpler than a table; `k = 100` gives 100/200/300 at depths 1/2/3. One SPRT-tunable constant. Less commonly cited in the literature for FFP (more common for RFP) but defensible as a starting approximation.

### Tradeoffs

| Property | Smaller margin | Larger margin |
|---|---|---|
| Tactical blindness | Lower | Higher |
| Node savings | Less | More |
| SPRT signal | Smaller (may be inconclusive) | Larger (risks regression if over-aggressive) |
| Tuning cost | Single SPRT per increment | Same |

**Recommendation: per-depth table `{100, 150, 250}` at depths 1/2/3.** This is the conservative modern workhorse documented in the roadmap, consistent with the successful TalkChess t=74403 result. It is more tuning-friendly than a linear formula because each depth can be tuned independently.

---

## 4. What FFP_MAX_DEPTH Should Be

| Max depth | Node taxonomy | Characteristic | Risk |
|---|---|---|---|
| 1 (frontier only) | Frontier | Classical Heinz frontier FP; smallest scope | Very low; one ply from qsearch |
| 2 (pre-frontier) | + Pre-frontier | "Extended futility pruning" (Heinz 1998); well validated | Low; still shallow |
| 3 (pre-pre-frontier) | + Pre-pre-frontier | Covers additional shallow nodes; begins to blur with razoring | Moderate; three-ply tactics may be missed |
| ≥ 4 | Deeper interior nodes | Lynx SPSA-tuned to depth 7–9 | Higher; approach requires much larger margins |

[CPW — Futility Pruning; Heinz 1998; CPW-Engine (depth ≤ 3); Lynx DeepWiki (depth ≤ 7–9)]

### At Depth 3: Overlap with Razoring

Razoring is classically applied at depth 3 (pre-pre-frontier) and reduces to qsearch rather than skipping moves. Applying FFP at depth 3 with margin ≤ 250 cp is a weaker form of the same idea. Some implementations conflate the two at depth 3. The roadmap includes depth 3 in the margin table (250 cp) as an optional extension, but the primary scope is depths 1–2.

Heinz 1998 explicitly called depth-3 coverage "limited razoring" rather than "extended futility pruning," treating them as distinct techniques. [Heinz 1998; CPW — Razoring]

### Recommendation for v1

**`FFP_MAX_DEPTH = 2`** (depths 1 and 2, i.e., frontier + pre-frontier).

- Depth 1 is the classical, well-validated scope.
- Depth 2 (Heinz's extension) is validated in DARKTHOUGHT experiments: "10 to 20 percent search tree reduction on average compared with normal futility pruning" with "hardly any loss of tactical strength." [Heinz 1998 abstract via Semantic Scholar]
- Depth 3 can be added as a post-landing SPRT campaign (`FFP_MAX_DEPTH = 3` with margin 250).
- Lynx-style depth ≥ 4 is a separate SPRT-tunable surface better addressed after the baseline is established.

The margin table can include a `depth == 3` entry (250 cp) as a named constant for future use without activating it at v1.

---

## 5. Where to Read `static_eval`

### The Pattern

`static_eval` is used by both the node-level prologue (RFP at step 8, NMP at step 9) and the per-move FFP check inside the move loop. The question is where to read it relative to the move loop.

### Options

| Option | Description | Cost | Risk |
|---|---|---|---|
| Read once before move loop, reuse per-move | Hoist `static_eval` above the move loop; each FFP check reuses the same value | One eval per node | None for quiet moves (their eval effect is not material-changing) |
| Read inside the gate block (lazy, once) | Read `static_eval` lazily when FFP is first needed; cache in a local | One eval per node (same result) | None |
| Read inside the gate block per-move | Re-evaluate before each quiet move | O(move_count) evaluations per node | Performance: catastrophic |

### Why Once Is Correct

Quiet moves do not change material. The incremental static eval for a quiet move differs from the parent position's static eval only by positional adjustments (PSQT, mobility). The positional delta is exactly what the margin approximates. Therefore, the node's `static_eval` (pre-any-move) is the correct operand for the FFP test: it represents the position's standing before the quiet move, and the margin represents the maximum positional upside the move can produce. [CPW — Futility Pruning: "calculate potential move value by adding a safety margin to current position evaluation"; TalkChess t=77451: "it isn't important whether you take the evaluation before or after the move since non-captures don't significantly change the evaluation"]

### Relation to M5.B's Lazy-Dup Design

ADR-0024 §6 reads `static_eval` lazily inside the RFP gate, independently of NMP's read ("lazy-dup"). This preserved ADR-0023's NMP semantics byte-identical and kept the M5.B SPRT attributable to RFP alone.

For M5.D (FFP), the situation is different: `static_eval` is needed inside the move loop per quiet move. Reading it once before the move loop and caching it in a local variable is the natural pattern. This does not conflict with the RFP or NMP reads (they occur at step 8/9, before movegen at step 10, before the move loop at step 13). The cached value can be the same local that RFP and NMP computed.

**Recommended:** hoist `static_eval` before the move loop at step 10, reuse in the FFP check per quiet at step 13. The hoisting is architecturally compatible with the lazy-dup design of M5.B because the move loop is a separate code region from the prologue — there is no "second consumer" conflict in the prologue.

---

## 6. Quiet-Only Justification

### Why Captures Are Exempt

A capture changes material by definition. The maximum benefit of a capture (from the capturing side's perspective) is not bounded by a positional margin — a capture can gain a queen, a rook, or force checkmate. Applying FFP to a capture would require a capture-specific margin (SEE-based or MVV-LVA-based), which is a separate technique (delta pruning in qsearch; SEE-based futility in regular search).

**v1 punts: FFP applies only to quiet moves.** [CPW — Futility Pruning: "requires searching all captures (or at minimum those exceeding alpha + margin)"; CPW-Engine: `!move_iscapt(move) && !move_isprom(move)` as the per-move quiet filter]

### Why Promotions Are Exempt

Promotions change material (queen or rook gain). Clawfish's `is_quiet` definition (per ADR-0025 §1: "Captures, promotions, TT-move handling ... are unchanged") classifies promotions as non-quiet. They are therefore implicitly exempt from FFP, which applies to quiet moves only. No explicit promotion check is needed in the FFP filter — `!is_quiet(mv)` already handles it. [ADR-0025 §1; CPW-Engine: `!move_isprom(move)`]

---

## 7. Interaction with Check Moves

### Two Senses of "Check"

1. **Current position is in check (to-move side):** FFP must not fire. A check position demands an evasion; static eval does not capture the urgency of responding to check. The node-level gate `!in_check(pos)` prevents FFP from activating at all when the side to move is in check. [CPW — Futility Pruning; Heinz 1998; all forum sources]

2. **The candidate quiet move gives check to the opponent:** Conservative engines exempt giving-check moves from FFP. The rationale: a checking move might be the start of a mating attack or a forcing sequence that changes the positional evaluation by more than the margin allows. [Mediocre Chess / Frayn: "the current move about to be searched is not a checking move"; CPW — Futility Pruning; H.G. Muller deep futility page: "at d>=2, checks are never futile"]

### The Gives-Check Classification Cost

Determining whether a move gives check requires calling the engine's attack detection code with the move's destination square. For clawfish, as of M5.C, this is not pre-computed in the move loop — no "gives check" bit is attached to each move during generation. Adding it would require either:

- Generating attacks after each `make_move` call (post-make, expensive), or
- Pre-computing `gives_check(pos, mv)` before `make_move` (mid-cost; requires a call to the attack detection infrastructure).

TalkChess t=74403 identified a major implementation bug from failing to correctly detect gives-check moves — the bug caused significant negative Elo until fixed. The lesson: if gives-check exemption is implemented, it must be correct across all move types (quiet, captures, promotions alike).

### Open Question for v1

This is the primary **open question** for M5.D. Two policies:

| Policy | Node savings | Safety | Implementation cost |
|---|---|---|---|
| Skip gives-check exemption (prune checking quiets) | Maximum | Lower (risks missing forcing lines starting with quiet checks) | Zero |
| Exempt gives-check moves | Slightly reduced | Higher | Requires `gives_check(pos, mv)` before recursion |
| Post-make check-detection | Same as above | Same | Make then check — `in_check` on child already computed by NMP/LMR path |

**Recommended for v1: implement the node-level `!in_check(pos)` gate only (the standard gate), and do NOT attempt per-move gives-check exemption.** The reasoning:

- LMR already exempts moves leading to check via the M5.C implicit exemption path (in-check at child detected by the recursive `in_check` call at the child node level, which prevents LMR from reducing further).
- FFP at depth 1 will only prune moves where `static_eval + 100 <= alpha`. In such a losing position, a quiet checking move is usually not the only resource; if alpha is well above the position's eval, the position is losing enough that checking quiet moves rarely suffice either.
- The gives-check detection infrastructure is not yet cheap in clawfish. The TalkChess t=74403 bug report shows the cost of getting this wrong.

Flag this as an SPRT-tunable: post-v1, add `gives_check(pos, mv)` detection and measure via SPRT.

---

## 8. Interaction with Promotions

As covered in §6: promotions are non-quiet per clawfish's `is_quiet` definition. The per-move FFP condition reads `is_quiet(mv)` first. A promotion move never passes this filter. No explicit promotion exemption is needed. [ADR-0025 §1]

This matches CPW-Engine's explicit `!move_isprom(move)` filter — both approaches achieve the same result.

---

## 9. Interaction with M5.C LMR

### Both Touch the Per-Quiet Decision

The M5.C move loop, for each quiet move, decides:
- Is this move LMR-eligible? If yes, search reduced.
- If reduced score > alpha, re-search full depth.

M5.D FFP adds a **prior** decision:
- Is this move FFP-prunable? If yes, skip entirely.

### Standard Ordering: FFP First, Then LMR

The natural layering is:

```
for each quiet move:
    // FFP: prune first
    if ffp_enabled && is_lmr_node && static_eval + ffp_margin(depth) <= alpha {
        // skip; do NOT add to quiets_searched; do NOT apply LMR
        continue;
    }
    // LMR: reduce if not pruned
    if lmr_enabled && is_lmr_eligible_quiet(mv, quiet_index) {
        // search at reduced depth
        ...
    }
```

A move pruned by FFP never reaches the LMR path. This is the correct design because:

1. FFP establishes that the move cannot improve alpha even at full depth. Reducing it via LMR and searching would be redundant.
2. LMR is a depth reduction, not a skip. If FFP prunes, there is nothing to reduce.
3. CPW-Engine confirms this ordering: "futility pruning is evaluated before the LMR logic." [CPW-Engine search]

### FFP and LMR at the Same Node

Both share the same node-level gate: `!is_pv`, `!in_check`, `depth >= threshold`. For FFP, the depth threshold is a ceiling (`depth <= FFP_MAX_DEPTH`); for LMR, it is a floor (`depth >= LMR_MIN_DEPTH`). At depth 3, both can be active simultaneously:

- Depth 3 is at or below FFP_MAX_DEPTH (if using depth ≤ 3 coverage).
- Depth 3 meets LMR's `depth >= 3` floor.
- For a given quiet move at depth 3: FFP checks first (prune or not), then LMR applies if not pruned.

At depth 1 and 2 (FFP's primary scope), LMR does not fire (`depth < LMR_MIN_DEPTH = 3`). So at the core FFP depth range, the interaction is simple: only FFP fires for quiet moves.

---

## 10. Interaction with `quiets_searched` / History Malus

### The Analogous M5.C Decision

ADR-0025 §7 decided: reduced-only quiets (LMR first pass; no full-depth re-search) do not enter `quiets_searched`, so they do not receive history malus on a subsequent beta cutoff. Rationale: a coarser-depth probe is a noisier signal; including it at full `+depth*depth` malus magnitude over-trains history.

### FFP-Pruned Quiets

For FFP-pruned moves (no recursion at all — neither reduced nor full-depth), the question is whether to apply history malus.

| Policy | Reasoning | Risk |
|---|---|---|
| Exclude from history malus | No recursion happened; no evidentiary basis for "tried-and-failed at any depth" | Consistent with M5.C's reduced-only exclusion |
| Include in history malus | The position was evaluated and deemed inferior; the move is genuinely bad from this position | Over-trains on a pure static comparison, not a search result |

The pruning decision was based solely on `static_eval + margin <= alpha` — a static comparison with no recursive evidence. Including the move in `quiets_searched` for subsequent malus application would apply a `depth*depth` malus to a move that was never actually searched. This is weaker evidence than even a reduced-only LMR probe.

**Recommended for v1: exclude FFP-pruned quiets from `quiets_searched`.** This is the most conservative choice and mirrors the M5.C reduced-only exclusion logic. The alternative (include with a reduced malus weighting) is a post-v1 SPRT-tunable.

### History Bonus Unaffected

The history bonus is applied on beta cutoffs (`update_history_on_quiet_cutoff`). A beta cutoff requires a move to actually be searched and return above beta. FFP-pruned moves are never searched; they cannot cause a beta cutoff. The bonus path is unaffected.

---

## 11. Interaction with TT

### No TT Store on FFP Skips

FFP is a per-move skip, not a node bound. Skipping a move does not prove anything about the node's value beyond "that move was not searched." The node's eventual return value (from captures, checks, or other quiets) is what gets stored via the normal TT-store path at the end of the move loop.

**No TT write for FFP.** [CPW — Futility Pruning; OpenChess t=3099; consensus in all sources]

### TT Probe Policy: Unchanged

The TT probe at step 7 already happens before any pruning. If the TT provides an early cutoff at step 7, FFP never runs. If the TT provides a bound that narrows alpha without cutoff, the FFP check at step 13 sees the updated alpha. No special handling is needed.

### TT Move Exemption

The TT move (best move from a previous search stored in the TT) is searched at the top of the move loop (step 12 reorder, per ADR-0018 §12 and ADR-0025 §3). When it is a quiet move, it receives `quiet_index = 1` and is implicitly exempt from LMR (floor at `LMR_MIN_QUIET_INDEX = 2`). For FFP, no additional exemption is strictly needed — the TT move is searched before the FFP-eligible quiet pool in the move loop. However, if FFP is implemented as a node-level flag set before the loop (CPW-Engine style), the TT move would need to be searched before the flag is set. The preferred per-move check avoids this issue entirely.

**Recommended: per-move check (not a node-level flag set before the loop).** This is architecturally cleaner for clawfish given the existing M5.C move-loop structure.

---

## 12. Alpha-Distance vs. Raw Alpha

### Two Algebraically Equivalent Forms

The FFP condition can be written two ways:

```
// Form 1: additive (CPW standard form)
static_eval + margin(depth) <= alpha

// Form 2: difference (some engines use this sign convention)
alpha - static_eval >= margin(depth)
// i.e., alpha - static_eval > margin(depth) - 1 (integer equivalent with strict <)
```

Both forms are mathematically identical for integer arithmetic. The CPW form (`static_eval + margin <= alpha`) is the most widely cited and directly matches the problem statement ("does the position's potential value reach alpha?"). [CPW — Futility Pruning; TalkChess t=59315, t=74403]

### Strict vs. Non-Strict Inequality

`<= alpha` vs. `< alpha`:

- `<= alpha` means: the maximum achievable value exactly equals alpha; since fail-soft needs to beat alpha (strictly), this move should be pruned.
- `< alpha` means: prune only if the maximum achievable value is strictly below alpha.

The standard form `static_eval + margin <= alpha` is conservative: a move that could exactly match alpha is pruned (matching alpha doesn't improve the lower bound). This is the CPW form and is correct for fail-soft.

### Sign Convention

`static_eval` must be side-to-move-relative (the same sign convention used in the negamax call). Clawfish uses `static_eval_white()` and flips for Black STM. The same flip used by RFP and NMP applies here. No special consideration beyond what is already in place for M5.A/M5.B.

---

## 13. Implementation Pitfalls

### Pitfall 1: Mate-Window Guard (Load-Bearing)

When `alpha` is near a mate value (`alpha.abs() >= MATE_IN_MAX_PLY`), the centipawn margin comparison is unreliable. A mate-in-5 alpha is a large integer; adding 100 or 150 cp to a centipawn static eval will never reach it, and FFP would never fire — which is incidentally correct behavior. But the near-mate window can also cause alpha to be a negative mate score (when searching for a refutation to an apparent checkmate), where static eval + margin might accidentally exceed alpha and falsely prune.

**Gate: include `alpha.abs() < MATE_IN_MAX_PLY` in the node-level check, mirroring the RFP and NMP mate-guard pattern.** [CPW-Engine: `abs(alpha) < 9000`; TalkChess t=63368 Ferdy: `alpha > MIN_EVAL_SCORE`; ADR-0023/0024 mate-guard gates]

### Pitfall 2: PV-Node Exemption (Required)

At PV nodes (`is_pv == true`), the engine must search all moves to construct the principal variation. FFP at a PV node would silently shorten the PV. Gate: `!is_pv`. [CPW — Futility Pruning; all sources]

### Pitfall 3: Root-Node Exemption

The root node is always a PV node (`ply == 0`, `is_pv == true`). The `!is_pv` gate already covers this. Defense-in-depth: also include `ply > 0` as a structural guard, matching ADR-0023/0024's pattern.

### Pitfall 4: In-Check Exemption (Required)

`!in_check(pos)` is the most critical gate. In a check position, static eval is unreliable (it doesn't account for the forced response). FFP on a check position would prune valid evasions. [All sources; TalkChess t=59315: "futility is not used when the side to move is in check"]

### Pitfall 5: Eval Sign Convention

`static_eval` must be side-to-move-relative. The same sign flip applied for M5.B RFP and M5.A NMP applies here. A bug that uses the white-perspective eval without flipping for Black STM would reverse the FFP condition's polarity — pruning when the position is actually favorable, and not pruning when it is unfavorable. This is the most dangerous category of FFP bug.

### Pitfall 6: The "All Moves Pruned" Corner Case

If FFP prunes all quiet moves and no captures or checking moves were searched, the node has found no legal moves. The engine must correctly distinguish between:

- **Stalemate or checkmate**: no legal moves at all (including captures) → return draw score or mated score.
- **All quiet moves pruned, but captures were searched**: return the best capture score.
- **All quiet moves pruned, no captures, not mate/stalemate**: the engine generated zero moves but FFP eliminated them all. In this case, returning alpha (fail-soft) is correct — the position did not improve the bound.

The standard pattern: maintain `legal_moves_searched` counter as in the existing clawfish negamax move loop. FFP-pruned moves increment a `moves_pruned` counter but do NOT increment `legal_moves_searched`. If `legal_moves_searched == 0` after the loop, test for checkmate/stalemate as normal. [OpenChess t=3099; TalkChess t=59315: "set a flag when a move is skipped to distinguish between mate/stalemate versus all moves being futile"]

### Pitfall 7: Node-Level Flag vs. Per-Move Check

CPW-Engine uses a pre-loop `f_prune = 1` flag, set before iterating moves, then checks the flag per move. Lynx breaks the entire loop when FFP fires (abandoning remaining moves). The per-move check pattern (test the condition for each eligible quiet move) is cleaner in clawfish's M5.C-structured loop where moves are processed one at a time. The CPW-Engine flag approach does not interact well with move-by-move streaming (e.g., staged movegen in M5.H); the per-move check is forward-compatible.

---

## 14. Open SPRT-Tunable Parameters

| Parameter | v1 value | Post-v1 candidates |
|---|---|---|
| `FFP_MAX_DEPTH` | 2 | 3 (add pre-pre-frontier), 1 (conservative baseline) |
| `FFP_MARGIN_D1` | 100 cp | 125, 150, 200 |
| `FFP_MARGIN_D2` | 150 cp | 200, 250, 300 |
| `FFP_MARGIN_D3` | 250 cp (inactive at v1) | Activate via `FFP_MAX_DEPTH = 3`; tune |
| Gives-check exemption | Off (prune checking quiets) | On: add `gives_check(pos, mv)` before recursion |
| `quiets_searched` policy for pruned moves | Exclude | Include / depth-weighted malus |
| Linear formula alternative | Per-depth table | `k * depth` with tunable `k` |

---

## 15. Recommended v1 Commitments for Clawfish (M5.D ADR)

### Core Parameters

| Parameter | v1 commitment | Rationale |
|---|---|---|
| `FFP_MAX_DEPTH` | 2 | Frontier + pre-frontier; Heinz 1998 validated; depth 3 deferred to SPRT |
| `FFP_MARGIN_D1` | 100 cp | Conservative; TalkChess t=74403 successful at 100 for depth 1 |
| `FFP_MARGIN_D2` | 150 cp | Conservative; roadmap row; successful in t=74403's {100, 150} pair (+25 Elo) |
| `FFP_MARGIN_D3` | 250 cp (define constant; inactive) | Pre-defined for post-v1 `FFP_MAX_DEPTH = 3` SPRT |

### Gate Set (Node-Level, Computed Once Before Move Loop)

```
ply > 0
&& !is_pv
&& !in_check(pos)
&& depth <= FFP_MAX_DEPTH
&& alpha.abs() < MATE_IN_MAX_PLY
```

### Per-Move FFP Check (Inside Move Loop, Quiet Moves Only)

```
// FFP skip: test before LMR
if ffp_node_gate_passed && is_quiet(mv) {
    let margin = ffp_margin(depth);  // 100 cp at depth 1, 150 cp at depth 2
    if static_eval + margin <= alpha {
        // skip; do not recurse; do not add to quiets_searched
        continue;
    }
}
// LMR: only reached if FFP did not skip
if lmr_node_gate_passed && is_lmr_eligible_quiet(mv, quiet_index, history) {
    ...
}
```

### `static_eval` Sharing

Read once before the move loop in the same local already used by RFP/NMP. FFP reuses this cached value per-move inside the loop. No additional eval reads.

### `quiets_searched` / History Policy

FFP-pruned quiets: excluded from `quiets_searched`. They receive no history malus on a subsequent beta cutoff. Rationale: no recursive evidence; consistent with ADR-0025 §7's reduced-only exclusion.

### Gives-Check Exemption

Off for v1. No per-move gives-check detection. Flag as SPRT-tunable post-v1.

### TT Policy

No TT write on FFP skip. TT probe at step 7 unchanged.

### Return Value

No special return value. FFP is a per-move skip; the node returns its best score from non-pruned moves via the normal fail-soft path.

### Helper Function

```rust
/// Per-depth FFP margin table.
///
/// Depths outside [1, FFP_MAX_DEPTH] return 0 (no pruning).
pub(crate) fn frontier_futility_margin(depth: u32) -> i32 {
    match depth {
        1 => FFP_MARGIN_D1,
        2 => FFP_MARGIN_D2,
        3 => FFP_MARGIN_D3,  // inactive until FFP_MAX_DEPTH raised to 3
        _ => 0,
    }
}
```

This is a pure function (depth → margin), unit-testable at every depth boundary including the inactive depth-3 entry.

### Expected Elo Impact

| Source | Reported gain | Conditioning |
|---|---|---|
| TalkChess t=74403 (margins {100, 150}) | +25 Elo | Depths 1–2; after NMP |
| MadChess Build 037 (full table d=0–4) | +54 Elo | Combined qsearch + main search futility; after NMP |
| Heinz 1998 (DARKTHOUGHT, d=1+2) | 10–20% tree reduction | Search-tree nodes; Elo not quantified in abstract |
| Bob Hyatt combined futility variants | +10–30 Elo total | When added after NMP + LMR; includes multiple variants |
| Roadmap M5.D row | +20–40 Elo expected | After NMP + RFP + LMR (clawfish M5.A/B/C stack) |

**Calibration:** The +20–40 Elo roadmap estimate is the most credible prior for clawfish's M5.D, given the current stack (NMP + RFP + LMR already in place). MadChess's +54 Elo included qsearch delta pruning at d=0, which M5.D does not address. The TalkChess +25 figure used depths 1–2 only (matching v1 scope) against a stack that may have differed. The Bob Hyatt combined-futility estimate is the most directly comparable: "combined" implies NMP + LMR already present, then futility variants added.

### SPRT Methodology

| Parameter | Value |
|---|---|
| Baseline tag | `M5.C` (commit `0f9bd88`) |
| SPRT bounds | `elo0=0, elo1=5` (FFP is moderate; +20–40 Elo prior → 5 Elo signal is credible minimum) |
| TC sampling | `--tc-sample 10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1` (mixed-TC; FFP benefits more at slow TC) |
| Acceptance | Mixed-game SPRT H1 + Δ Elo ≥ 0 at every TC |
| Follow-up tune 1 | `FFP_MAX_DEPTH = 2` vs `3`; add margin 250 at depth 3 |
| Follow-up tune 2 | `FFP_MARGIN_D1`: 100 vs 125 vs 150; `FFP_MARGIN_D2`: 150 vs 200 vs 250 |
| Follow-up tune 3 | Gives-check exemption: off (v1) vs on |

---

## 16. Sources Cited

- [CPW — Futility Pruning](https://www.chessprogramming.org/Futility_Pruning)
- [CPW — Frontier Nodes](https://www.chessprogramming.org/Frontier_Nodes)
- [CPW — Pruning](https://www.chessprogramming.org/Pruning)
- [CPW — CPW-Engine search](https://www.chessprogramming.org/CPW-Engine_search)
- [CPW — Razoring](https://www.chessprogramming.org/Razoring)
- [CPW — Late Move Reductions](https://www.chessprogramming.org/Late_Move_Reductions)
- [CPW — Reverse Futility Pruning](https://www.chessprogramming.org/Reverse_Futility_Pruning)
- [Heinz, Ernst A. (1998). "Extended Futility Pruning." ICCA Journal, Vol. 21, No. 2, pp. 75–83.](https://www.semanticscholar.org/paper/Extended-Futility-Pruning-Heinz/d1e82a9613f8c4ab99113bd2bd72ebc8bdf22d9b)
- [H.G. Muller — Deep Futility Pruning](https://home.hccnet.nl/h.g.muller/deepfut.html)
- [MadChess — Build 037 Futility Pruning (Erik Madsen)](https://www.madchess.net/2014/12/29/madchess-2-0-beta-build-37-futility-pruning/)
- [Mediocre Chess — Guide: Futile attempts with futility pruning (Jonatan Dahl)](http://mediocrechess.blogspot.com/2007/01/guide-futile-attempts-with-futility.html)
- [Frayn Beowulf — Computer Chess Programming Theory](http://www.frayn.net/beowulf/theory.html)
- [TalkChess t=59315 — futility pruning (JVMerlino)](https://talkchess.com/forum3/viewtopic.php?t=59315)
- [TalkChess t=63368 — futile futility pruning attempt](https://talkchess.com/forum3/viewtopic.php?t=63368)
- [TalkChess t=74403 — Futility Pruning Issues (+25 Elo with margins {100, 150})](https://talkchess.com/viewtopic.php?t=74403)
- [TalkChess t=77451 — Futility Pruning and its Relation to Quiescence Search](https://talkchess.com/viewtopic.php?t=77451)
- [TalkChess t=77644 — Futility reductions](https://talkchess.com/viewtopic.php?t=77644)
- [TalkChess t=35955 — Move count based pruning](https://talkchess.com/viewtopic.php?t=35955)
- [OpenChess t=3099 — Futility pruning](https://open-chess.org/viewtopic.php?t=3099)
- [DeepWiki — Lynx engine: Pruning Techniques](https://deepwiki.com/lynx-chess/Lynx/3.5-pruning-techniques)
- [int0x80.ca — Chess engines post 11: Futility Pruning](https://int0x80.ca/posts/chess-engines/11-fp)
