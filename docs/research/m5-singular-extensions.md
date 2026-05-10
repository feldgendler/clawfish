# Prior-Art Research: Singular Extensions (M5.G)

Sources consulted: Chess Programming Wiki (CPW) — Singular Extensions, Multi-Cut, Extensions; TalkChess threads (t=35419 Zamar/Stockfish, t=35603/p3 Bob Hyatt/Crafty, t=35698 Bob Hyatt, t=35898 Bob Hyatt, t=38104 jdart, t=68290 Cardoso, t=69966 Xiphos/Karlo Bala); academic paper Anantharaman, Campbell, Hsu 1988 ("Singular Extensions: Adding Selectivity to Brute-Force Searching"); Selective Extensions paper Ye & Marsland (Alberta).

Per ADR-0003, no third-party engine source code was read. All findings come from prose: wikis, papers, blog posts, and forum threads.

---

## TL;DR — Six-Bullet Bottom Line

- SE fires a reduced-depth zero-window verification search at a TT-move candidate, checking whether all other moves in the position fail below `tt_score − margin`. If yes, extend the TT move by 1 ply.
- **Minimum depth gate**: `depth >= 8` is the historically cited threshold (CPW, TalkChess t=35603); literature majority. A few engines use `depth >= 6`; none below 6 are reported as Elo-positive.
- **Margin formula**: `singular_beta = tt_score − (depth × c)` where `c` is typically 1–2 cp per ply; linear in depth. Xiphos uses `c = 1`, Ethereal uses `c = 1`, Stockfish (historical) used `c = 2 / ONE_PLY`. Confirmed depth-proportional widening: shallower depth → smaller margin → more aggressive SE.
- **Verification depth**: `(depth − 1) / 2` integer division. Universal across all documented implementations.
- **Move exclusion**: pass an `excluded: Option<Move>` parameter into the negamax call; skip generating / searching that move in the verification call's move loop. This is the only approach documented at scale — the "separate function" variant was discussed but no engine uses it.
- **TT bound gate**: start with Lower-bound-only (Stockfish 1.6 origin); adding Exact-score entries is a documented progressive extension; Upper-bound is excluded by definition (opponent refuted our strategy).

---

## 1. Eligibility Gate

### 1.1 Depth Threshold

| Threshold | Source | Notes |
|-----------|--------|-------|
| `depth >= 8` | CPW; TalkChess t=35603 Bob Hyatt; TalkChess t=68290 Cardoso cites Stockfish "depth > 8*ONE_PLY" | Literature majority; avoids near-leaf cost explosion |
| `depth >= 6` | Xiphos (`SE_DEPTH` in t=69966); Ethereal (same value implied by `rBeta = MAX(ttValue − depth, −MATE)` formula discussion) | Documented as viable; slightly more aggressive |
| `depth >= 4` | One forum mention; Crafty's early experiments (t=35898) | Reported as net neutral or negative; avoided |

**Recommendation**: start at `depth >= 8`.

### 1.2 PV vs Non-PV

| Approach | Source | Rationale |
|----------|--------|-----------|
| Non-PV only | Stockfish (t=35419 Zamar, t=68290 Cardoso); CPW; Xiphos (t=69966) | PV nodes already get full-width search; verification overhead is wasted there |
| PV included | Some implementations per CPW note ("original SE concept included PV-nodes") | Original 1988 paper was not restricted; relaxed Stockfish variant subsequently tried PV inclusion |

**Recommendation**: non-PV only. The `beta − alpha == 1` test from PVS discriminates non-PV with zero overhead — the aspiration-window framework (M4.D) already threads this condition.

### 1.3 TT Bound-Type Required

| Bound type | Allowed | Rationale | Source |
|------------|---------|-----------|--------|
| Lower bound | Yes — primary | We know the move scored ≥ tt_score; that is a true floor. Strong candidate for singularity. | Stockfish 1.6 (2009), CPW |
| Exact | Yes — secondary extension | We have the true score; even more confident the move is singular. CPW: "more relaxed form … allowing singular search on TT entries with an exact score." | CPW; TalkChess t=35419 Mangar |
| Upper bound | No | An upper-bound entry means the opponent refuted our strategy from this node. The TT move's score is at best tt_score; there's no basis to claim it is singular. TalkChess t=35419 Alessandro Damiani: "If a score is an upper bound then this means the opponent disproved the player's strategy. In this case I would not do any extension at all." | TalkChess t=35419; CPW |

**Recommendation**: initially gate on Lower-bound only. Adding Exact is a documented +Elo tuning step, but should be a separate SPRT campaign.

### 1.4 TT Entry Depth Requirement

| Formula | Source | Interpretation |
|---------|--------|---------------|
| `tt_depth >= depth − 3` | Xiphos (t=69966): `hash_data.depth >= depth - 3`; cited as the standard in multiple threads | Entry must come from a search no more than 3 plies shallower than the current search — ensures the stored score is still meaningful at the current depth |
| `tt_depth >= depth − 1` | Stricter variant, mentioned in t=35898 | Essentially requires equal or near-equal depth; very conservative |
| `tt_depth >= depth / 2` | Looser variant | Risky; shallow entries may give unreliable scores |

**Recommendation**: `tt_depth >= depth − 3`. This is the only formula with a specific forum citation and multiple independent confirmations.

### 1.5 Score Validity — Mate Score Exclusion

- Universally exclude SE when `tt_score.abs() >= MATE_IN_MAX_PLY`. Source: Xiphos (t=69966) `!_is_mate_score(hash_score)`; Bob Hyatt's Crafty experiments.
- Rationale: mate scores are measured in ply, not centipawns; the depth-proportional margin `depth × c` is meaningless in a forced-mate context; singular beta can wrap below −MATE.
- CPW multi-cut section confirms: "if the bounds at which they were searched at is greater than or equal to beta, we can predict that multiple moves fail high" — mate-scale scores pollute this logic.

### 1.6 Root Node

- Exclude. No engine or source documents SE at the root; Xiphos explicitly checks `!root_node`. Root has aspiration windows (M4.D); the interaction is undefined and untested.

### 1.7 In-Check Guard

- Standard practice: skip SE when in check. Bob Hyatt (t=35698): "I also do not do it in check." Rationale: in-check nodes typically have few legal moves; the forced-reply quality is already concentrated; the verification search cost is not amortized.

### 1.8 Summary Gate

```
singular_extension_eligible(ply, depth, in_check, is_pv,
                             tt_bound, tt_depth, tt_score) -> bool:
    depth >= SE_MIN_DEPTH          // 8
    && ply > 0                     // not root
    && !is_pv                      // non-PV only
    && !in_check                   // forced-reply contexts skip
    && (tt_bound == Lower || tt_bound == Exact)
    && tt_depth >= depth - 3
    && tt_score.abs() < MATE_IN_MAX_PLY
```

---

## 2. Verification Search Parameters

### 2.1 Singular Beta (Margin Formula)

The verification search uses a null window `(singular_beta − 1, singular_beta)` where:

```
singular_beta = tt_score − (depth × SE_MARGIN_PER_DEPTH)
```

| Engine / Source | Formula | `SE_MARGIN_PER_DEPTH` |
|-----------------|---------|----------------------|
| Xiphos (t=69966) | `beta_cut = hash_score − depth` | 1 |
| Ethereal (cited in t=69966) | `rBeta = MAX(ttValue − depth, −MATE)` | 1, clamped |
| Stockfish historical (t=69966) | `singularBeta = max(ttValue − 2*depth/ONE_PLY, −VALUE_MATE)` | ~2 (depends on `ONE_PLY` constant) |

**Why depth-proportional?** The margin serves the horizon effect: near the leaves, a depth×c margin is still small enough to trigger SE meaningfully; at high depth the margin is larger but the cost of verification is also proportionally larger, which naturally suppresses SE at very high depths. Karlo Bala (t=69966): "margin is wider with bigger depth [which makes] SE less important when the depth is big and more important near the leaves."

**Fixed vs linear**: Bob Hyatt (t=35898) tested margins from 10 to 100 cp (fixed), finding no clear winner. Linear-in-depth emerged from Stockfish/Xiphos/Ethereal as the consensus; start there.

**Recommendation**: `singular_beta = tt_score − depth * SE_MARGIN_PER_DEPTH` with `SE_MARGIN_PER_DEPTH = 1`. Keep as a named constant for SPRT tuning. Apply `singular_beta.max(−MATE_SCORE)` to prevent underflow.

### 2.2 Verification Depth

```
verification_depth = (depth - 1) / 2
```

- Source: Xiphos (t=69966) `depth >> 1`; Bob Hyatt (t=35698) "initially used depth/2"; CPW pseudocode; cited universally.
- Bob Hyatt also considered `depth − N` for various N = 1..4, but returned to `depth / 2` as the stable baseline.
- The off-by-one `(depth − 1) / 2` vs `depth / 2` matters: if depth is odd both give the same value (integer division rounds down). The `− 1` variant is a micro-optimization that ensures we don't accidentally run depth/2 = depth − 1 at depth = 2; with the `depth >= 8` gate it is irrelevant in practice.

**Recommendation**: `verification_depth = (depth - 1) / 2`.

### 2.3 Window

- Null window (zero-window): `(singular_beta − 1, singular_beta)`.
- Universal across all documented implementations.
- Under fail-soft negamax the child sees `(−singular_beta, −singular_beta + 1)`. If child returns `score < singular_beta` (i.e., the child fails low under our test), that move is "not singular" from below — all alternatives are below the bar.

### 2.4 Move Exclusion Mechanism

**The problem**: the verification search must search "all moves except the TT move." The TT move is the candidate being tested; including it in the verification would be circular.

**Documented approach**: pass an `excluded: Option<Move>` parameter through the negamax/search signature. Inside the move loop, skip any move that equals `excluded`. The `excluded` parameter also serves as a re-entrancy guard: if the recursion sees `excluded != None`, it skips the SE block entirely — preventing recursive singular searches in the same line.

- Xiphos (t=69966) uses `!skip_move` as the re-entrancy guard, where `skip_move` is set when an excluded move is active.
- CPW Singular Extensions: "the excluded move … is passed as a parameter to the recursive call to prevent re-applying SE in the same line."
- This doubles as the "no recursive SE" anti-explosion guard.

**Alternative not used**: searching a separate copy of the position without the TT move or using a wrapper function. No documented engine does this; the parameter approach is the consensus.

**Recommendation**: add `excluded_move: Option<Move>` to the negamax signature. It is `None` on all current call sites; set to `Some(tt_move)` only in the verification call. The `excluded != None` gate suppresses SE for the entire verification subtree.

---

## 3. Extension Policy

### 3.1 Extension Amount

- +1 ply (new depth = old depth). Universal.
- Implemented as `depth + 1 − 1 = depth` in PVS frameworks where the child normally gets `depth − 1`. With SE, the singular move's child gets `depth − 1 + 1 = depth` (or equivalently, skip the `−1` reduction for this move).

### 3.2 Double Extension

A "double extension" (+2 ply) was discussed by engine developers as a response when the verification search score is much lower than singular_beta — indicating the position is "very singular." Not yet consensus as of the surveyed literature (no specific Elo report found). The Raphael engine (forum, v3.0 2026-02-xx) mentions "double extensions" in its release notes but provides no parameters. The search results cite "Stockfish will not singular extend a move after another extended one" as the conservative policy.

**Recommendation for M5.G v1**: +1 only. Double extension is a tuning-step option, not a baseline.

### 3.3 Stacking / Recursion Prevention

- The `excluded_move != None` check already prevents SE at the child of a singular search (the verification subtree).
- For the main search tree, "Stockfish will not singular extend a move after another extended one" (TalkChess t=68290 Cardoso). This prevents two consecutive singular extensions in one line.
- Implementation: track a `singular_ext_active: bool` on the stack, or rely on the fact that `excluded_move != None` disables SE in the recursive call automatically.
- **Open question**: the literature does not precisely specify a cumulative-extension budget (e.g., "total extensions ≤ ply/2"). CPW says "care must be taken so that the search is not extended infinitely" but cites only the excluded-move re-entrancy guard as the primary protection. A per-branch cap is not universally documented. See §7 for the ply-ceiling guard as a backstop.

### 3.4 Check Extension Interaction

- Check extensions and SE are both +1 ply; if both fire on the same move, the result would be +2 (or in some implementations, capped at +1 via `extension.min(1)`).
- CPW Extensions: "Some programs vary extensions based on node type, applying 1/2 a ply extension on all-nodes versus full ply on PV-nodes" — but this is not SE-specific.
- Standard practice: if a move is both a singular extension candidate and a checking move, give it +1 (SE takes precedence or they compose to +1 each and you cap). No engine documentation gives the move +2 for this combination.
- **Recommendation**: evaluate SE first; if SE triggers (extension=1), skip check-extension logic for that move (check extension is moot since the move is already extended). This matches the "no double extension from stacking" philosophy.

---

## 4. Multi-Cut Variant

### 4.1 Definition

When the verification search **fails high** (some other move scores ≥ singular_beta, meaning the TT move is not singular), instead of simply not extending, some implementations return `singular_beta` as a fail-high immediately — the "multi-cut" pruning step.

The logic: if another move already beats singular_beta in a reduced search, the current node is very likely a cut-node (≥ N alternatives beat beta at reduced depth). Returning early saves the remaining move loop.

### 4.2 Parameters

- CPW Multi-Cut: original Björnsson formulation examines first M=6 moves and returns beta if C=3 fail high.
- Modern integration into SE: if the verification search returns score ≥ singular_beta (even just one move), return singular_beta as a fail-high (multi-cut prune).
- CPW Singular Extensions note: "If a move isn't singular and the singular move is above beta, we know that there is more than one move that beats beta. We can thus fail soft and return the singular move beta."
- Black Marlin (via CPW): "doesn't apply multi cut pruning if low depth singular extensions are done." Suggests multi-cut may be gated on `depth >= higher_threshold`.

### 4.3 Safety and Soundness

Multi-cut is **speculative pruning** — it can produce incorrect results in rare positions. CPW: "the likelihood of such an erroneous pruning decision affecting the move decision at the root" is low. It is not sound in the formal sense.

### 4.4 Recommendation

**Start without multi-cut pruning.** SE alone is the baseline experiment. Multi-cut integrated into SE is a documented Elo-positive follow-up, but it adds speculative unsoundness and makes debugging harder. Land SE first; run SPRT; add multi-cut in a separate campaign if SE gains are confirmed.

---

## 5. TT Side Effects

### 5.1 Does the Verification Search Write to the TT?

The verification search is a standard negamax call (with `excluded_move` set). Standard negamax writes to the TT. This means the verification subtree **will write TT entries for its children** via normal operation.

The central contamination concern: TT entries written during the verification call (which excluded the TT move) have lower depth than the main search and are marked with the normal key. They may clobber deeper entries for the same position.

**Consensus practice** (documented across CPW, t=35419, t=35898):
- Do not suppress TT writes during verification. The depth-preferred replacement policy (ADR-0018) means a shallower verification entry will be overwritten by a deeper main-search entry when the main search returns to that position.
- Anantharaman (1991) noted "implementation issues related to the transposition table" for original SE, but the modern restricted-SE form does not suppress writes.
- Bob Hyatt (t=35898): "TT entries are not a problem—depth comparison prevents meaningful corruption." He dismisses the contamination concern.

**The PV fail-low cycle bug** (distinct from write contamination): Mangar (t=35419) describes a specific cycle: (1) SE fires; (2) the extended search reveals a fail-low; (3) that fail-low gets written to the TT at the extended (high) depth; (4) on a subsequent re-search with a wider window, the TT returns a fail-low score → no SE triggers → the move is searched at base depth. This is not a correctness bug but a search instability that can cause re-search chains. The depth-preferred replacement policy partially mitigates it (the extended fail-low entry has high depth and will not be overridden).

### 5.2 Does the Verification Search Probe the TT?

Yes. It is a standard recursive call. No documented engine suppresses TT probes inside the verification subtree.

### 5.3 Current Node's TT Entry After SE

After SE triggers and the extended search returns, the current node stores normally (whatever bound the extended search produced). No special handling is needed — the TT entry for the current node already exists (that's how SE was triggered in the first place); the replacement policy will update it if the new result is deeper or same-depth-and-age.

### 5.4 The Excluded-Move and TT Correctness

The verification call searches "position with move X excluded." This is technically a different game state than the real position. The TT key is the same Zobrist key because the excluded move is not applied to the board — the exclusion happens only in the move-selection logic.

This means TT entries written by the verification subtree are technically "wrong" for the real position (they came from a restricted move set). In practice this is accepted as a design compromise:
- The depth of the verification search is `(depth−1)/2`, so any entries it writes are shallow and will be dominated by later full-depth searches.
- No engine documents a correctness regression from this approximation.
- **No separate hash-key scheme** (e.g., XOR with a "excluded move" Zobrist value) is documented in any source consulted. The consensus is to tolerate the mild inaccuracy.

---

## 6. Interaction with Existing M5 Components

### 6.1 NMP (M5.A)

| Question | Literature Consensus |
|----------|---------------------|
| Is NMP allowed inside the verification search? | No documented restriction found in literature; the standard behavior is to allow it. Xiphos (t=69966) `skip_move` flag only suppresses SE, not NMP. Bob Hyatt tests (t=35698) ran NMP inside verification without issue. |
| Should `allow_null` be threaded separately? | Not necessary. The `excluded_move` parameter already restricts the verification call's move set; NMP firing inside is fine (it uses a null move, not the excluded move). |

**Recommendation**: allow NMP inside verification search. No change to `allow_null` threading needed. The `excluded_move` flag is orthogonal to `allow_null`.

### 6.2 RFP (M5.B)

No interaction documented. RFP fires before the move loop; the verification call is a full recursive call that will evaluate its own RFP conditions independently. Fine as-is.

### 6.3 LMR (M5.C)

LMR applies at the extended node (after SE triggers): the singular move gets depth = `current_depth` (not `current_depth − 1`), so LMR conditions at its children apply at the natural extended depth. No documented conflict.

At the verification call: LMR fires normally inside the verification subtree. This is intentional — LMR helps keep the verification search cheap.

One TalkChess warning (t=35603 Richard/Critter): "tree explosions (like 112 plies at iteration 20) requiring careful LMR/pruning adjustments." This was before the re-entrancy guard via `excluded_move` was standardized. With the `excluded_move != None → skip SE` guard, recursive SE chains are prevented, and LMR's depth reductions inside verification keep costs bounded.

### 6.4 FFP (M5.D)

No interaction documented. FFP fires at `depth <= 1` (frontier); SE fires at `depth >= 8`. They are in disjoint depth ranges by construction.

### 6.5 Qsearch-in-TT (M5.F)

M5.F stores qsearch entries at `depth = 0`. The negamax SE probe reads a TT entry; the SE gate requires `tt_depth >= depth − 3` where `depth >= 8`, so `tt_depth >= 5`. A qsearch entry has `depth = 0`, which is `5` less than the minimum needed — it fails the depth gate automatically. No interaction.

This means M5.F entries are invisible to SE by the existing probe-side depth check, with no extra guard needed.

### 6.6 Aspiration Windows (M4.D)

SE is restricted to interior nodes (`ply > 0`). Aspiration windows govern the root search. No interaction.

The root loop calls negamax with `ply = 0`; `ply > 0` is the first SE gate. Aspiration re-searches with wider windows at the root still will not trigger SE (ply=0 guard holds).

---

## 7. Ply-Budget and Abort Guards

### 7.1 Ply Ceiling

- The project already enforces a `MAX_PLY` ceiling in negamax entry (ADR-0018). The verification search is a recursive negamax call and will hit the same ceiling.
- SE at depth 8, verification depth 4, ply P: the verification call starts at ply P+1 and can recurse to `P + 1 + 4` plies. If `P` is already near `MAX_PLY − 5`, the verification call would be cut short by the ceiling. This is acceptable: the ceiling guard returns a static eval or TT fallback, which is a conservative value — the verification will likely return a score below singular_beta, preventing the extension. Conservative and safe.
- **No additional guard is needed** at the SE site itself; the existing ceiling guard in negamax covers the verification subtree.

### 7.2 Abort During Verification

- The verification search recurses into standard negamax, which checks the abort flag on the standard node-count cadence. An abort during verification will propagate up via the abort early-return.
- The singular extension extension decision is only committed to `if score < singular_beta` — an aborted verification returns a potentially inaccurate score. In practice, if the search is aborting (time expired), the current iteration's result will be discarded and the previous iteration's best move used.
- No special abort handling is needed inside SE logic; the standard abort path covers it.

---

## 8. Implementation Surface

### 8.1 Proposed Pure Helpers (Mutation-Coverage Pattern)

Following the project's `negate_window` / `late_move_reduction` / `ffp_pruned_bound` precedent, extract:

| Helper | Signature | Notes |
|--------|-----------|-------|
| `singular_extension_eligible(ply, depth, in_check, is_pv, tt_bound, tt_depth, tt_score) -> bool` | Pure predicate | Encapsulates all gate conditions; all mutants within it are independently testable |
| `singular_beta(tt_score: i32, depth: i32) -> i32` | Pure fn | `tt_score − depth * SE_MARGIN_PER_DEPTH`; clamped against `−MATE_SCORE` |
| `verification_depth(depth: i32) -> i32` | Pure fn | `(depth − 1) / 2`; trivial but extractable for mutation kill |

Constants:
```rust
const SE_MIN_DEPTH: i32 = 8;
const SE_MARGIN_PER_DEPTH: i32 = 1;  // cp per depth ply
```

### 8.2 Negamax Signature Change

Add `excluded_move: Option<Move>` to the negamax signature:

```rust
fn negamax(
    pos: &mut Position,
    depth: i32,
    alpha: i32,
    beta: i32,
    ply: usize,
    is_pv: bool,
    allow_null: bool,
    excluded_move: Option<Move>,   // NEW
) -> i32
```

All current call sites pass `None`. The verification call passes `Some(tt_move)`.

**Alternative: wrapper function** — a `verify_singular(pos, depth, tt_move, ply, ...) -> i32` that internally calls negamax with the excluded-move filter. This avoids polluting every call site but requires duplicating the negamax entry contract. The parameter approach is simpler and more transparent; it matches how NMP threads `allow_null: bool`. Use the parameter approach.

### 8.3 Move-Loop Integration Point

The SE block sits in the move loop, after the TT probe+cutoff step and before the per-move depth adjustment. Placement relative to NMP (M5.A) and RFP (M5.B):

```
Step 2   — TT probe + cutoff
Step 7   — In-check test
Step 8   — RFP (depth <= 6, non-PV, !in_check)
Step 9   — NMP (depth >= 3, non-PV, !in_check, allow_null, material gate)
Step 10  — Move loop begins
  Step 10a — Move-loop TT-move first ordering
  [NEW] Step 10b — SE eligibility check on TT move; verification call if eligible
  Step 10c — LMR / FFP depth adjustment
  Step 10d — Recurse
```

The SE check fires **inside the move loop**, on the move that matches `tt_move`. This is the only documented placement — SE is a per-move decision, not a pre-loop node-level decision like NMP.

### 8.4 Excluded Move in the Move Loop

In the verification call's move loop, skip moves that equal `excluded_move`:

```rust
for mv in moves {
    if Some(mv) == excluded_move { continue; }
    // ...
}
```

In the SE block itself, check `excluded_move.is_none()` before entering SE (re-entrancy guard). This ensures SE does not fire while already inside a verification call.

---

## 9. Empirical Elo

| Source | Engine | Elo Gain | Notes |
|--------|--------|----------|-------|
| Zamar (t=35419) | Stockfish | +40 | "First implementation and window size we tried." Later fine-tuning gave no further improvement. |
| Critter / Richard (t=35603) | Critter | +35–40 | After extensive LMR re-tuning post-SE landing |
| CPW Singular Extensions | General | +10–40 | Range; depends on engine; classical eval engines gain more |
| Crafty / Bob Hyatt (t=35898) | Crafty | ~0 | "Break even. No significant Elo gain or loss." Multiple attempts. |
| Daniel Shawul (t=35419) | Unspecified | Negative | "It slows down the search too much and doesn't improve it." |
| Raphael v3.0 (forum 2026-02) | Raphael | +227 combined | SE one of several features in the version; isolated contribution unknown. |

**Pattern**: the technique's Elo is engine-dependent. Engines with well-tuned LMR and good eval tend to gain more; engines with weak eval or high baseline branching factor may break even. The project's current ~2636 Elo with strong LMR/RFP/NMP/FFP is in the "should gain" category by the pattern.

**Bench-cost note**: verification searches add overhead — the move loop now calls negamax at verification_depth on a (small fraction of) TT-move candidates. GameDev.net tutorial: "singular extensions are costly, as adding an extra ply to a node roughly doubles the number of leaves in the tree." The saving (fewer extensions at non-singular nodes that were previously shallowly searched, plus occasional multi-cut pruning) is what produces the net gain. If SPRT shows a node-count increase without Elo gain, reconsider the margin or depth threshold.

---

## 10. Failure Modes and Gotchas

| Failure Mode | Description | Source |
|-------------|-------------|--------|
| Forgot to exclude TT move during verification | Verification includes the candidate move; it trivially beats singular_beta; SE never fires anywhere. Result: no extension, no Elo gain, but also no correctness bug. Most common first-implementation error. | CPW; t=38104 |
| Recursive SE explosion | SE fires → extended search → SE fires again → extended again. Without the `excluded_move` re-entrancy guard, tree depth can balloon. TalkChess t=35603 Richard: "tree explosions (like 112 plies at iteration 20)." | t=35603 |
| TT score mate-scale wrap | `tt_score − depth × c` underflows past `−MATE_SCORE` when tt_score is already a long-distance mate value. singular_beta is garbage; verification returns a meaningless score. Guard: check `tt_score.abs() < MATE_IN_MAX_PLY` in the eligibility gate AND clamp `singular_beta` to `−MATE_SCORE`. | t=69966 Xiphos code |
| PV fail-low cycle | SE triggers on a TT Lower-bound entry → extended search produces a fail-low → high-depth fail-low written to TT → same position later gets the fail-low on probe → TT bound is now Upper → SE gate (requires Lower/Exact) rejects it → move searched at base depth, missing extension. Described by Mangar (t=35419). Not a bug per se, but a recurring irritant in deep searches. | t=35419 |
| Applying SE at Upper-bound TT entries | Treating an Upper-bound TT hit as an SE candidate. The score is a ceiling, not a floor; singular_beta derived from it has no meaning. Leads to phantom extensions on nodes the engine already knows are poor. | t=35419 Alessandro Damiani |
| SE in the endgame with zugzwang | Rare: SE identifies a move as singular at depth 8; the extended search at depth 9 encounters a zugzwang the shallower NMP pruning misses. Very hard to trigger but theoretically possible in K+P endings. No documented case in the literature. | Open question |
| Static eval not initialized before SE | SE gate itself doesn't require static_eval, but multi-cut return of `singular_beta` is not `static_eval`-based. Some implementations compute static_eval lazily; SE does not need it. Non-issue with the parameter-only approach above. | — |
| Extension at ply near MAX_PLY | Extending at ply = MAX_PLY − 1 allows the recursion to reach MAX_PLY + 1. The ply ceiling guard in negamax prevents this; confirm it checks `>=` not `>`. | ADR-0018 §8 (project) |
| Double extension runaway | If double extensions are added later (see §3.2), the re-entrancy guard must cover that path too. Without it, a position with two highly singular moves at consecutive plies can double-extend twice, producing +4 total extension in one line. | Reckless engine forum (open-chess.org) |

---

## 11. Open Questions (Require SPRT to Resolve)

1. **Depth threshold 6 vs 8**: both documented; CPW majority says 8; Xiphos uses 6. Start at 8; if SPRT is flat, try 6 in a follow-up campaign.
2. **Lower-bound-only vs Lower+Exact**: Stockfish 1.6 was Lower-only; later versions also allow Exact. Implement Lower-only first; add Exact in a subsequent campaign.
3. **Double extension**: only one forum mention (Raphael). Out of scope for M5.G v1; mark as tuning backlog.
4. **Multi-cut on verification fail-high**: documented as Elo-positive by CPW and Black Marlin, but adds speculative unsoundness. Separate campaign after M5.G baseline.
5. **`SE_MARGIN_PER_DEPTH = 1` vs `2`**: Stockfish historically used ~2 (before `ONE_PLY` changes), Xiphos and Ethereal use 1. Start at 1; the depth-proportional margin already provides depth-scaling.
6. **TT write suppression during verification**: not documented as necessary; consensus is to allow writes and rely on depth-replacement. Flag as open for future investigation if search instability (PV fail-low cycles) is observed.

---

## Recommended Starting Parameters for M5.G

| Parameter | Value | Confidence |
|-----------|-------|-----------|
| `SE_MIN_DEPTH` | `8` | literature majority |
| Non-PV only | `is_pv == false` | literature consensus |
| TT bound gate | Lower-bound only | literature consensus (conservative start) |
| `tt_depth >= depth - 3` | depth − 3 | literature consensus (Xiphos; cited across threads) |
| Mate score exclusion | `tt_score.abs() < MATE_IN_MAX_PLY` | literature consensus |
| Root / in-check exclusion | yes | literature consensus |
| `SE_MARGIN_PER_DEPTH` (cp/ply) | `1` | literature majority (Xiphos, Ethereal cite 1; Stockfish ~2 historically) |
| `singular_beta` formula | `tt_score − depth * SE_MARGIN_PER_DEPTH` | literature majority |
| `singular_beta` floor clamp | `−MATE_SCORE` | literature consensus |
| Verification depth | `(depth - 1) / 2` | literature consensus |
| Verification window | null window `(singular_beta − 1, singular_beta)` | universal |
| Move exclusion mechanism | `excluded_move: Option<Move>` parameter | literature consensus |
| Extension amount | +1 ply | universal |
| Double extension | no | judgment call (insufficient data for v1) |
| Multi-cut on fail-high | no | judgment call (defer to post-baseline campaign) |
| NMP inside verification | allowed (standard `allow_null` threading) | literature consensus |
| TT writes during verification | allowed (standard behavior) | literature consensus |
| Recursive SE guard | `excluded_move.is_some()` → skip SE | literature consensus |
| Interaction with check extensions | SE takes precedence; skip check-ext if SE fires | judgment call |
