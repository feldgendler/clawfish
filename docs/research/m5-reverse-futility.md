# Prior-Art Research: Reverse Futility Pruning (M5.B)

Sources consulted: Chess Programming Wiki (CPW), TalkChess forums (ZirconiumX, lucasart, zamar, hgm, Don Beal, Bob Hyatt, Rasmus Althoff/CT800, and others), OpenChess forums (H.G. Muller), Mediocre Chess blog (Jonatan Dahl), MadChess blog (Erik Madsen), Lynx engine documentation (DeepWiki), int0x80.ca chess engine blog, H.G. Muller's deep futility page.

Per ADR-0003, no third-party engine source code was read. All findings come from CPW articles, blog posts, and forum threads.

---

## TL;DR — Five-Bullet Bottom Line

- RFP is a one-line static check: `if static_eval - margin(depth) >= beta { return static_eval - margin(depth); }`. No recursive call, no move generation — cheaper than any other search pruning technique.
- Gates: `!is_pv`, `!in_check`, `depth <= MAX_RFP_DEPTH` (recommend 6), `abs(beta) < MATE_IN_MAX_PLY`, and the margin condition. No zugzwang guard needed (no passed-turn semantics).
- **Margin formula for v1: linear `100 * depth` cp** (conservative; tunes upward cleanly). CPW workhorse is `150 * depth`; both are well-documented starting points. Per-depth table (100/150/250 at d=1/2/3) is an alternative that matches MadChess-style legacy implementations.
- **Return value under fail-soft: `static_eval - margin(depth)`** — this is the proved lower bound (we proved even after discounting by the margin the position beats beta). Some modern engines return `(static_eval + beta) / 2` for smoother eval trees; `static_eval` alone slightly overstates the proof. Do not store in TT.
- **Place RFP before NMP** in the negamax prologue, directly after the TT probe. RFP is cheaper (no sub-search); if it fires, NMP is skipped entirely. Both read `static_eval`; hoisting `static_eval` to a shared variable read once before both blocks is the natural M5.B refactor.

---

## 1. Definition and Intuition

### What RFP Is

Reverse Futility Pruning (also called **Static Null Move Pruning** or **Beta Pruning**) tests: "Is this position so far above beta, even after assuming the opponent gets a free kick of `margin(depth)` centipawns, we still beat beta?" If yes, return immediately without generating or searching any moves.

Formally, the condition is:

```
static_eval - margin(depth) >= beta
```

On a true condition, return `static_eval - margin(depth)` (fail-soft lower bound). The node is a **proved fail-high** based solely on static evaluation and a conservative per-depth margin.

### Intuition

The null-move observation (Donninger 1993): "giving the opponent a free move rarely changes the outcome in non-zugzwang positions." RFP is the **degenerate case** of this observation: if the position is so good that even after discounting by a large margin (representing the best the opponent could plausibly do in `depth` moves) we still beat beta, there is no point searching. [CPW — Reverse Futility Pruning]

This is exactly the standing-pat logic in quiescence search, generalized to pre-frontier depths. At qsearch depth 0 (stand-pat), the margin is 0. At depth 1 the margin represents the best the opponent can gain in one ply; at depth 2, two plies, etc. [TalkChess t=41302, ZirconiumX; CPW — Reverse Futility Pruning]

### How It Differs from NMP

| Aspect | NMP | RFP |
|--------|-----|-----|
| Mechanism | Recursive null-search at `depth - 1 - R` | No search; pure static eval comparison |
| Cost | Sub-search (entire recursive call) | Single integer comparison |
| Depth range | `depth >= 3` (lower bound) | `depth <= 6` (upper bound) |
| Condition | `static_eval >= beta` (pass-then-search) | `static_eval - margin*depth >= beta` (static only) |
| Return value | `null_score` (capped at `beta` for mate) | `static_eval - margin*depth` |
| Zugzwang risk | Real (hence `has_non_pawn_material` guard) | Negligible (no passed-turn — opponent has not been given an extra move) |
| TT store on cutoff | Yes (`Bound::Lower`) | Consensus: no |

RFP is sometimes described as "NMP without the null-move part" (Don, Komodo developer, TalkChess t=41302). This framing is accurate: the intuition is identical but the proof is static-only, so costs essentially nothing.

---

## 2. Gate Set

Every condition below must hold. Cheap predicates should appear first to avoid the `static_eval` read on nodes that clearly cannot prune.

| Gate | Condition | Why |
|------|-----------|-----|
| Non-PV node | `!is_pv` (or equivalently `beta - alpha == 1` once PVS is in use) | At PV nodes we need the full PV. RFP would silently shorten it. Same reasoning as NMP. [CPW RFP; TalkChess t=41302 lucasart] |
| Not in check | `!in_check(pos)` | Static eval does not capture imminent threats. A check position is exactly the scenario where static_eval is most unreliable — the opponent has a forcing sequence that eval ignores. [CPW RFP; TalkChess t=62522 ZirconiumX; OpenChess t=3056] |
| Shallow depth | `depth <= MAX_RFP_DEPTH` | At deep nodes, `margin * depth` grows large enough that the condition `static_eval - margin * depth >= beta` becomes increasingly restrictive and will rarely fire — but when it does, the margin of `150 * 8 = 1200 cp` represents a very coarse approximation that may mask real threats. Recommended limit: **6**. Stockfish historical practice was `depth < 7` (i.e., ≤ 6 plies). [CPW RFP; TalkChess search excerpts] |
| Not a mate score for beta | `abs(beta) < MATE_IN_MAX_PLY` | When beta is a mate score, `static_eval - margin * depth` may be large in centipawns but below a mate beta, causing RFP to fire when it should not; or the margin comparison wraps. More importantly, static eval does not capture mate threats, so RFP near mate scores is fundamentally unreliable. [TalkChess t=41302 lucasart; OpenChess t=3056 H.G. Muller: "will only refrain from doing this if you have mate scores"] |
| Margin condition | `static_eval - margin(depth) >= beta` | The core condition. Holds iff the proved lower bound exceeds beta. |

### Gates Not Needed for RFP (Unlike NMP)

| Omitted Gate | Why Not Needed |
|---|---|
| `allow_null` / stacked-null | RFP makes no recursive call; there is nothing to stack. |
| `has_non_pawn_material` | RFP does not pass the turn — no zugzwang semantics apply. The opponent has not been given a free move; there is no "pass = beneficial" risk. |
| `ply > 0` | RFP at the root would return immediately with no bestmove. However, the `!is_pv` gate already prevents RFP at the root (root is always PV). Defense-in-depth: still include `ply > 0` as a structural guard matching ADR-0023 §3's pattern. |

---

## 3. Margin Formula

The margin represents the maximum expected centipawn swing the opponent can produce in `depth` moves. It scales with depth because deeper remaining search means more time for the opponent to find a refutation.

### Survey of Documented Approaches

| Formula | Depth 1 | Depth 2 | Depth 3 | Depth 6 | Characteristic | Source |
|---------|---------|---------|---------|---------|----------------|--------|
| `100 * depth` | 100 | 200 | 300 | 600 | Conservative linear; simple constant | TalkChess t=41302; roadmap M5.B row |
| `120 * depth` | 120 | 240 | 360 | 720 | Slightly larger; CPW CPW-Engine reference code | OpenChess t=3056 ("120 * depth") |
| `150 * depth` | 150 | 300 | 450 | 900 | CPW workhorse; widely cited modern default | CPW RFP article; int0x80.ca blog |
| Per-depth table: 100 / 150 / 250 | 100 | 150 | 250 | N/A (depth ≤ 3 only) | Non-linear; historic MadChess-style | MadChess Build 037 (forward futility table) |
| Per-depth table: 200 / 300 / 500 | 200 | 300 | 500 | N/A | CPW-Engine's `fmargin[]` for futility | CPW-Engine search code |
| Piece-value table: bishop/rook/queen | ~300 | ~500 | ~900 | N/A | Semantic: "absorb one piece per ply" | TalkChess t=41302, first ZirconiumX proposal |

**Piece-value table**: the original TalkChess t=41302 proposal (ZirconiumX) used bishop (~300 cp) / rook (~500 cp) / queen (~900 cp) at depths 1/2/3. This is a large margin — aggressive pruning. Forum reaction was mixed; lucasart tested it and found "improvement is quite useless" in his engine. The modern consensus moved toward smaller linear constants.

**`150 * depth`**: the most commonly cited modern workhorse. CPW's RFP article shows `eval >= beta + 150 * depth` (note CPW uses the equivalent form `eval >= beta + margin` where margin = `150 * depth`). One blog implementation (int0x80.ca) obtained SPRT +145.83 ± 24.41 Elo using `150 * depth`.

**Improving-aware adjustment** (advanced): compare static_eval at current ply vs static_eval at ply-2. If improving, lower the margin (prune more aggressively — you are on an upward trajectory). CPW — Improving article; Lynx engine documentation. **Defer to post-M5.B SPRT-tune** — adds a search-stack field and a tuning parameter.

### Recommendation for M5.B v1

**Linear `100 * depth`** (conservative baseline):
- At depth 1: 100 cp — roughly one pawn's worth of slack. Safe.
- At depth 2: 200 cp — roughly two pawns. Still conservative.
- At depth 6: 600 cp — a rook. Conservative at deep end; fires rarely.
- One constant (`RFP_MARGIN_PER_DEPTH = 100`); trivially tunable upward to 120 or 150 via SPRT.
- The CPW workhorse of 150 is a well-documented alternative for the first SPRT-tune after v1 lands.

---

## 4. Depth Bound

| Limit | Characteristic | Source |
|-------|----------------|--------|
| `depth <= 3` | Conservative; matches CPW-Engine's `depth < 3` form; only covers pre-frontier nodes | CPW-Engine reference; OpenChess t=3056 |
| `depth <= 6` (`depth < 7`) | Stockfish historical (DD era, "last six plies"); modern consensus | TalkChess forum citations; int0x80.ca blog; CPW RFP |
| `depth <= 8` | Some modern implementations; aggressive | DeepWiki Lynx documentation |

**Recommendation for M5.B v1: `MAX_RFP_DEPTH = 6`** (i.e., `depth <= 6`).

Rationale:
- Documented Stockfish DD used `depth < 7` (six plies); this is the most cited modern floor.
- At depth 7–8 with `100 * depth` margin, the threshold is 700–800 cp above beta — restrictive enough that the gate barely fires, but the tactical blindness risk grows with depth.
- Conservative start; can expand to 8 after SPRT validation if warranted.
- Avoids overlap with LMR (M5.C): LMR reduces depth on quiets at most depths; RFP at depths ≤ 6 targets the prologue before any moves are examined, while LMR targets within the move loop.

---

## 5. Return Value

### Options

| Return | Fail-Soft Semantics | Characteristic | Source |
|--------|---------------------|----------------|--------|
| `beta` | Fail-hard equivalent; returns the cutoff threshold | Simple; standard for fail-hard engines | TalkChess t=41302 (hgm: "fail-hard this should return beta") |
| `static_eval` | Returns the full static eval, ignoring the margin discount | Slightly overestimates — we didn't prove eval, we proved eval-margin | CPW RFP article (naive form); some early implementations |
| `static_eval - margin(depth)` | Returns the proved lower bound | Correct fail-soft lower bound: we proved this value exceeds beta | OpenChess t=3056; TalkChess t=62522 (ZirconiumX: "for fail-soft, return eval_margin"); recommended |
| `(static_eval + beta) / 2` | Compromise between eval and beta | Smoother eval tree; avoids discontinuities across depths | Lynx engine (citing Ciekce); CPW RFP article |

### Analysis

Under fail-soft (clawfish is fail-soft per ADR-0016), the search contract is "return the best provably reachable score." What did RFP prove?

- `static_eval - margin(depth) >= beta` was the triggering condition.
- That means `static_eval - margin(depth)` is a valid lower bound on the node's true minimax value (the discount represents an overestimate of the opponent's counter, so the actual value is probably higher, but `static_eval - margin(depth)` is the conservative proved floor).
- Returning `static_eval - margin(depth)` is the tightest honest lower bound. It correctly informs the parent that this node is at least this good.
- Returning `static_eval` is slightly dishonest — we didn't prove the full static eval, we proved eval minus margin. However, the difference only matters when the parent's alpha is between `static_eval - margin` and `static_eval`, which is rare and harmless in practice.
- `(static_eval + beta) / 2` is an interpolation that reduces discontinuities when the same position is visited at different depths (since `margin * depth` varies). This is the most sophisticated option but adds complexity.

**Recommendation for M5.B v1: return `static_eval - margin(depth)`**. This is the proved lower bound, consistent with the fail-soft contract, and directly derivable from the triggering condition. It is the form documented in ZirconiumX's summary at TalkChess t=62522. The `(static_eval + beta) / 2` variant is a follow-up SPRT-tune candidate.

---

## 6. TT Interaction

### Store Policy: Do Not Store

| Policy | Argument For | Argument Against | Consensus |
|--------|-------------|-----------------|---------|
| Store as `Bound::Lower` at current depth | Subsequent probes of the same position benefit | The "proof" is `static_eval - margin * depth >= beta`; static_eval is not a deep search result; depth of the "proof" is the depth at which we gave up, not the depth we searched | Do not store |
| Do not store | Clean; no TT pollution with imprecise bounds | Miss the opportunity to cache the cutoff | Standard practice |

**Consensus: do not store in TT on RFP cutoff.** [CPW RFP article; H.G. Muller OpenChess t=3056: "normally tested in the parent node to avoid wasting time on move generation ... should not store". Forum discussions consistently omit TT-store for static heuristics]

Rationale: The RFP condition is a static heuristic, not a depth-n search result. `static_eval` is the same for every visit to this position regardless of depth; the margin is depth-dependent. Storing a `Bound::Lower` at depth N would be used by a subsequent probe at depth M — but the proof only holds for the specific margin at depth N, not depth M. The TT entry would be formally incorrect.

By contrast, NMP's TT store (ADR-0023 §7) is sound because the null-search *actually ran* a sub-search at `depth - 1 - R`: that is a real depth-preferable search result whose lower-bound semantics hold for any future probe at `depth ≤ stored_depth`.

### Probe Policy: Unchanged

The TT probe already happens at step 7 (before RFP). If the TT returns an early cutoff at step 7, RFP never runs. If the TT returns a bound that narrows beta but not to cutoff, RFP sees the updated beta normally. No special handling needed.

---

## 7. Composition with NMP — Ordering

### Two Orderings in the Literature

| Order | Characteristic | Source |
|-------|---------------|--------|
| RFP **before** NMP | RFP is cheaper (no sub-search); fires first at shallow depths where both overlap (depth 3). If RFP cuts off, NMP sub-search is skipped entirely. | CPW RFP article ("right before null move pruning"); TalkChess t=82201 (Rasmus Althoff: "static [no search] reverse futility pruning against beta + margin at low depth, placed immediately before null-move pruning"); Lynx documentation |
| RFP **after** NMP | NMP's fail-high has been confirmed by an actual search, arguably higher-quality information. | Uncommon in documented implementations |

**Consensus: RFP before NMP.** The CPW article and multiple forum contributors describe RFP as the first prologue check, placed before NMP because it is cheaper. Given the depth ranges:

- RFP fires at `depth <= 6`. NMP fires at `depth >= 3`. Overlap at depth 3–6.
- At depth 3: RFP fires first (static check). If RFP cuts off, NMP's sub-search is skipped.
- At depth 7+: RFP is disabled; NMP runs normally.

The placement that maximizes pruning-per-cost: RFP → NMP.

### Shared `static_eval` Variable

Both NMP and RFP read `static_eval` from the same negamax prologue location. In M5.A's implementation, `static_eval` is read lazily inside the NMP gate (after `has_non_pawn_material` passes). With M5.B's RFP landing before NMP, the natural refactor is:

```
// Step 7: TT probe (unchanged)
// Step 8: static_eval read — shared by both RFP and NMP
let static_eval = /* sign-flip pos.static_eval_white() for stm */;

// Step 8a: RFP (new, depth <= MAX_RFP_DEPTH)
if !is_pv && !in_check && depth <= MAX_RFP_DEPTH && abs(beta) < MATE_IN_MAX_PLY {
    let rfp_margin = (depth as i32) * RFP_MARGIN_PER_DEPTH;
    if static_eval - rfp_margin >= beta {
        return static_eval - rfp_margin;
    }
}

// Step 8b: NMP (depth >= NMP_MIN_DEPTH; internal gate was previously the static_eval site)
if ply > 0 && allow_null && !is_pv && depth >= NMP_MIN_DEPTH && !in_check {
    let stm = pos.side_to_move();
    if has_non_pawn_material(pos, stm) && static_eval >= beta {
        /* null-search */
    }
}
```

This refactor removes the redundant `static_eval` read from inside the NMP gate (M5.A had it there as "first prologue consumer"). M5.A's plan §1 anticipates this: "M5.B/RFP may hoist the read to a shared location when its plan lands and a second consumer exists."

---

## 8. Composition with Forward Futility Pruning (M5.D)

RFP and forward futility pruning (M5.D) are **complementary, non-interacting** techniques:

| Aspect | RFP (M5.B) | Forward Futility Pruning (M5.D) |
|--------|-----------|--------------------------------|
| Node type targeted | Fail-high (position too good) | Fail-low (position too bad) |
| Condition | `static_eval - margin >= beta` | `static_eval + margin < alpha` |
| Location | Prologue, before move loop | Inside move loop, per quiet-move |
| Depth | `depth <= MAX_RFP_DEPTH` | `depth <= 2` (frontier + pre-frontier) |
| Prunes | Entire node | Individual quiet moves |

At a given node, at most one fires: if the position is above beta (RFP might fire), it cannot simultaneously be below alpha (futility cannot fire). They target opposite tails of the position-value distribution.

Bob Hyatt (TalkChess t=29777): combined gain from futility + extended futility + razoring is "between 10 and 30 Elo total" when added on top of NMP and LMR. This suggests RFP (as a reverse-futility form) and forward futility together deliver moderate additive gains when NMP and LMR already exist.

---

## 9. Composition with LMR (M5.C)

RFP is a **prologue cut** (fires before any moves are examined). LMR is a **per-move-loop reduction** (fires inside the move loop for late quiet moves).

- Both read `static_eval` — RFP in the prologue, LMR may read `static_eval` for move-skip decisions.
- **No code overlap**: RFP is an `if` block before move generation; LMR is inside the move loop.
- **No Elo double-counting**: at nodes where RFP fires, the move loop never runs, so LMR is irrelevant. At nodes where RFP does not fire, LMR operates normally.
- **Sequencing**: M5.B (RFP) before M5.C (LMR) is the planned order. If both were present from the start, their combined SPRT signal would conflate the two techniques. Keep separate per the M5.A precedent for M5.A vs M5.B.

---

## 10. Mate-Score Handling

When `beta` is a mate score, `abs(beta) >= MATE_IN_MAX_PLY`, RFP must not fire.

**Why**: static eval uses centipawn units, not mate-distance units. A `static_eval` of +500 cp with `beta = MATE_IN_5` (a huge value in mate units) would make `static_eval - margin * depth` very negative — i.e., RFP would not fire, which is correct. But the direction of error can also go the other way: if `beta` happens to be a small positive integer near the boundary of normal centipawn values but was actually a mate-in-X, the margin comparison is ill-defined. The safest gate is `abs(beta) < MATE_IN_MAX_PLY`. [TalkChess t=41302 lucasart; OpenChess t=3056 H.G. Muller]

This is the same guard used in other prologue decisions where mate scores create boundary ambiguity.

---

## 11. Expected Elo

| Metric | Estimate | Conditioning |
|--------|----------|-------------|
| Elo gain vs no-RFP baseline (standalone) | +20–50 Elo | Engine-dependent; wider range when NMP + LMR are not yet present |
| Elo gain in MadChess with forward futility (similar) | +54 Elo | Forward futility at d=1/2/3/4; from 1950 to 2004 [MadChess Build 037] |
| Elo gain from one blog implementation (int0x80.ca) | +145.83 ± 24.41 | RFP was one of several simultaneous improvements; not isolated |
| Bob Hyatt (Crafty) combined futility variants | +10–30 Elo total | When added after NMP + LMR |

**Calibration caveat**: the +145 Elo figure includes multiple simultaneous changes (int0x80.ca blog) and cannot be attributed to RFP alone. The literature consensus for a standalone RFP on top of NMP + killer + history + aspiration is **+20–50 Elo**, concentrated at slower TCs (deeper searches mean more node savings). At fast TC the savings are smaller because fewer deep nodes are reached.

Clawfish context: M5.A's NMP gain was +400 Elo (enormous; the engine was at 2600 Elo and NMP unlocked deep search). RFP's signal will be smaller — it prunes near-terminal nodes that NMP would have missed (NMP fires at `depth >= 3`; RFP also fires at depth 1–2 where NMP doesn't). The +20–50 Elo prior is realistic for our configuration.

---

## 12. Failure Modes and Pitfalls

| Failure Mode | Cause | Mitigation | Residual Risk |
|---|---|---|---|
| Tactical blindness near beta | `static_eval` overestimates the position's safety margin; a depth-7 tactic would refute the pruning but depth-6 RFP fires | Depth bound (≤ 6) limits exposure; non-PV gate prevents root | Low with depth ≤ 6 |
| PV truncation | RFP fires at a PV node, cutting the PV short | `!is_pv` gate. Same as NMP's gate. | Test-catchable |
| Mate-score boundary bug | `beta` is a mate score; centipawn margin comparison is ill-defined | `abs(beta) < MATE_IN_MAX_PLY` gate | Test-catchable |
| RFP at root | Root is always PV; `!is_pv` gate prevents this | Defense-in-depth: also add `ply > 0` | Covered by `!is_pv` |
| Over-pruning in pawn endgames | Static eval in K+P endings may be unreliable for zugzwang detection | Zugzwang is not an issue for RFP (no passed turn) — this failure mode doesn't apply | N/A |
| Static eval sign error | Forgetting to flip `static_eval_white` for Black STM | Same sign-flip as NMP (shared variable); already pinned by M5.A's tests | Test-catchable |
| `depth = 0` signed underflow | If `depth` is `u32` and `depth <= 6`, no underflow risk. But `depth as i32` for the margin multiply should be safe up to depth 127. | Use explicit `depth as i32` cast | No risk with `u32` depth |
| TT store on RFP cutoff | Storing a `Bound::Lower` at depth N from a static heuristic that only holds for that specific margin | Do not store in TT on RFP cutoff | Architecture decision: explicitly no-store |
| Over-lapping with NMP at depth 3 | Both gates can pass at depth 3. RFP first (cheaper); if RFP fires, NMP is skipped. | Sequential `if` blocks; RFP first | Intentional — not a bug |
| Bench non-determinism | RFP fires before move generation; if `static_eval` is not deterministic, bench varies | `static_eval_white` is incremental and deterministic (M3.A); RFP adds no non-determinism | None if static eval is deterministic |

---

## 13. Recommendation for M5.B v1

Concrete numbers for the plan to consume:

| Parameter | Recommended v1 Value | Rationale |
|---|---|---|
| Margin formula | `RFP_MARGIN_PER_DEPTH = 100` (linear `100 * depth`) | Conservative; clearly SPRT-positive starting point; easily tunable to 120 or 150 in a follow-up |
| Max depth | `MAX_RFP_DEPTH = 6` (i.e., `depth <= 6`) | Stockfish DD historical; matches CPW consensus; conservative start |
| Return value | `static_eval - margin(depth)` | Proved lower bound; correct under fail-soft; one-to-one with the trigger condition |
| Order vs NMP | **Before NMP** (cheaper; fires first at d=3–6) | CPW + forum consensus; saves NMP sub-search when RFP fires |
| `static_eval` sharing | **Hoist to shared variable** read before both RFP and NMP | M5.A plan §1 anticipated this; eliminates redundant read; makes the two blocks visually parallel |
| Mate-score gate | `abs(beta) < MATE_IN_MAX_PLY` | Standard; required for correctness |
| Zugzwang guard | **None needed** | RFP makes no null move; no passed-turn risk |
| TT store | **None** | Heuristic proof, not search-depth proof |

### Post-M5.B SPRT-Tune Candidates

| Parameter | Alternative | Motivation |
|---|---|---|
| Margin | `RFP_MARGIN_PER_DEPTH = 150` | CPW workhorse; ~50% more aggressive |
| Return value | `(static_eval + beta) / 2` | Smoother eval tree; Lynx/Ciekce recommendation |
| Improving flag | Lower margin when `static_eval > (ss-2)->static_eval` | Documented benefit in CPW Improving article; adds search-stack field |
| Max depth | `MAX_RFP_DEPTH = 8` | If SPRT at depth 7–8 is positive |

### Implementation Shape

```
// After step 7 (TT probe + cutoff), before NMP:

// Step 8: hoist static_eval (shared by RFP + NMP)
let static_eval = if pos.side_to_move() == Color::White {
    pos.static_eval_white()
} else {
    -pos.static_eval_white()
};

// Step 8a: RFP — cheap static beta-cutoff
const RFP_MARGIN_PER_DEPTH: i32 = 100;
const MAX_RFP_DEPTH: u32 = 6;
if !is_pv
    && !in_check
    && depth <= MAX_RFP_DEPTH
    && abs_score(beta) < MATE_IN_MAX_PLY
{
    let rfp_margin = (depth as i32) * RFP_MARGIN_PER_DEPTH;
    if static_eval - rfp_margin >= beta {
        return static_eval - rfp_margin;
    }
}

// Step 8b: NMP (depth >= 3; static_eval already in scope)
if ply > 0
    && allow_null
    && !is_pv
    && depth >= NMP_MIN_DEPTH
    && !in_check
    && has_non_pawn_material(pos, stm)
    && static_eval >= beta
{
    /* null-search as in M5.A */
}
```

The `in_check` predicate is called once per node regardless of how many prologue guards read it — it is computed before either block (either hoisted or computed once before the RFP block, reused in the NMP block). This is the same pattern as M5.A's gate ordering (cheap predicates first, `in_check` once).

---

## 14. SPRT Methodology for M5.B

| Parameter | Value |
|---|---|
| Baseline tag | `baseline/m5a-nmp` (commit `e63eb15`) |
| SPRT bounds | `elo0=0, elo1=5` (RFP is smaller than NMP; 5 Elo signal is credible) |
| TC sampling | `--tc-sample 10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1` (mixed-TC; RFP benefits more at slow TC where deeper nodes matter) |
| Acceptance | Mixed-game SPRT H1 + Δ Elo ≥ 0 at every TC |
| Follow-up tune | After SPRT accepts: compare `RFP_MARGIN_PER_DEPTH = 100` vs `150`; compare return value `static_eval - margin` vs `(static_eval + beta) / 2` |

---

## 15. Sources Cited

- [CPW — Reverse Futility Pruning](https://www.chessprogramming.org/Reverse_Futility_Pruning)
- [CPW — Futility Pruning](https://www.chessprogramming.org/Futility_Pruning)
- [CPW — Pruning](https://www.chessprogramming.org/Pruning)
- [CPW — Improving](https://www.chessprogramming.org/Improving)
- [CPW — CPW-Engine search](https://www.chessprogramming.org/CPW-Engine_search)
- [CPW — Null Move Pruning](https://www.chessprogramming.org/Null_Move_Pruning)
- [TalkChess t=41302 — Reverse Futility Pruning (ZirconiumX, lucasart, zamar, Don, hgm)](https://talkchess.com/viewtopic.php?t=41302&start=1)
- [TalkChess t=62522 — Static null move pruning (ZirconiumX, H.G. Muller)](https://talkchess.com/viewtopic.php?t=62522)
- [TalkChess t=82201 — Null Move Pruning gives worse results p.2 (Rasmus Althoff / CT800)](https://talkchess.com/viewtopic.php?t=82201&start=10)
- [TalkChess t=29777 — Futility pruning / razoring (Bob Hyatt, Don)](https://talkchess.com/viewtopic.php?t=29777)
- [TalkChess t=59661 — Futile attempts at futility pruning](https://talkchess.com/forum3/viewtopic.php?t=59661)
- [TalkChess t=59315 — Futility pruning (JVMerlino)](https://talkchess.com/viewtopic.php?t=59315)
- [OpenChess t=3056 — Static NULL Move (H.G. Muller)](https://open-chess.org/viewtopic.php?t=3056)
- [OpenChess t=3099 — Futility pruning](https://open-chess.org/viewtopic.php?t=3099)
- [Mediocre Chess — Guide: Futile attempts with futility pruning (Jonatan Dahl)](http://mediocrechess.blogspot.com/2007/01/guide-futile-attempts-with-futility.html)
- [MadChess — Build 037 Futility Pruning (Erik Madsen)](https://www.madchess.net/2014/12/29/madchess-2-0-beta-build-37-futility-pruning/)
- [DeepWiki — Lynx engine: Pruning Techniques (Lynx / Ciekce)](https://deepwiki.com/lynx-chess/Lynx/3.5-pruning-techniques)
- [int0x80.ca — Chess engines post 9: RFP and NMP](https://int0x80.ca/posts/chess-engines/9-rfp-nmp)
- [H.G. Muller — Deep Futility Pruning](https://home.hccnet.nl/h.g.muller/deepfut.html)
- [Wikipedia — Null-move heuristic](https://en.wikipedia.org/wiki/Null-move_heuristic)
- Donninger, C. (1993). "Null Move and Deep Search: Selective Search Heuristics for Obtuse Chess Programs." ICCA Journal 16(3):137–143.
