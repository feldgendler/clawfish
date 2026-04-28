# M3 prior-art research — alpha-beta search basics

Research pass for M3 (negamax alpha-beta + quiescence + iterative deepening + SPRT). Scope: structural choices that bind once code is written and would be expensive to revisit. Material eval is a separate research note (M3.A); time management is a separate research note (M3.D); this note covers M3.B (negamax), M3.C (qsearch), the parts of M3.D that touch the search loop, and the search-side implications of M3.E (`bench` / SPRT).

Sources: Chess Programming Wiki, Wikipedia, dogeystamp's chess-engine series, the UCI 2006 spec, TalkChess prose on stop-handling and stalemate detection. **No engine source code read** (ADR-0003). Mate-distance arithmetic and i16/i32 score-range arithmetic worked from first principles and cross-checked against CPW prose.

---

## Headline calls

| Question | Recommendation |
|---|---|
| Negamax framing | `fn negamax(&mut self, pos: &mut Position, depth: i16, ply: i16, alpha: i16, beta: i16, ctx: &SearchContext, pv: &mut PvLine) -> i16`. Pass `&mut Position` + use existing `make_move`/`unmake_move` (zero-clone hot path). |
| Fail-soft vs fail-hard | **Fail-soft**. Wikipedia calls it "nearly universal"; CPW notes Fishburn 1983 introduced it as a strict improvement. M3 has no TT to interact pathologically with. |
| Score type | **`i16`** with `MATE = 32000`, `INF = 32001`, `MATE_IN_MAX_PLY = MATE - MAX_PLY`. Comfortably inside `i16::MAX = 32767`. SIMD-friendly when M9 lands. |
| Mate scoring | `MATE - ply` for "I deliver mate"; `-MATE + ply` for "I'm getting mated". Faster mates score higher by construction. UCI `score mate y`: `y = ((MATE - score + 1) / 2) * sign(score)` for the mater; sign flips for the mated side. |
| Mate-distance pruning | **Defer to M4.** Marginal in fixed-depth play; cleaner to land alongside TT. |
| PV recovery | **Triangular PV table** (`[Move; MAX_PLY * MAX_PLY / 2]` ≈ 4 KiB at MAX_PLY=64). Per-ply slice writes. No TT alternative available. |
| PVS / NegaScout | **Defer to M4.** Saves ~10% with good ordering, but the gain depends on a TT-move and killer/history that M3 doesn't have. |
| Quiescence scope | **Stand-pat + captures + queen promotions only**. No checks (explosion risk without good ordering). No delta pruning (defer; needs material values stable, an open question if eval research lands a non-standard scale). Disallow stand-pat in check. |
| Move ordering | MVV-LVA on captures, quiet moves in movegen order. PV-move-from-previous-iteration at the **root only** (one comparison; trivial). |
| Iterative deepening | Outer loop over `depth = 1..=MAX_PLY`. **Discard partial-iteration result on cancellation**, return previous iteration's bestmove. No aspiration windows in M3. |
| Cancellation cadence | 4096 nodes via `SearchContext::should_abort`. Poll at the **top of `negamax`** (after early-out checks for terminal / depth==0). On abort, propagate a sentinel by returning `0` from the cancelled subtree but never updating `pv` or accepting the result at the calling `if score > best` site — done by checking `ctx.aborted` after the recursive call returns. |
| Repetition / 50-move | **Implement minimal in-search repetition stack** (push/pop in negamax, scan backwards across halfmove-clock window). Threefold not needed — return draw on first repeat per CPW prose. **Game history**: not implemented in M3; UCI `position … moves` reconstruction populates the stack as a starting state. |
| Insufficient material | **In eval** (M3.A's territory). Search just calls eval — keeping draw detection co-located with the static scorer keeps M3.B negamax pure. |
| Performance budget | Plain alpha-beta + qsearch + MVV-LVA, no TT, on Apple M4 release: **0.5–2 Mnps** (eval-bound — qsearch dominates leaf count). Reaching depth ~6–9 main + ~4–6 qsearch in 100ms per move (`tc=10+0.1`). |

---

## 1. Negamax framing

### 1.1 Signature recommendation

```rust
fn negamax(
    &mut self,
    pos: &mut Position,
    depth: i16,
    ply: i16,
    mut alpha: i16,
    beta: i16,
    ctx: &SearchContext,
    pv: &mut PvLine,
) -> i16;
```

- **`&mut Position` + `make_move`/`unmake_move`** — uses the existing M1.E hot path (~30 ns/cycle quiet moves per `bench/m1.e.md`). Cloning the position per node would cost ~200 B/clone × millions of nodes/sec = bandwidth-bound search.
- **`i16` for `depth`/`ply`/`alpha`/`beta`** — branches won't exceed 127 ply in any plausible game; `i16` keeps the negamax frame small (Rust will pad to alignment anyway, but the convention matters when SIMD-friendly NNUE arrives in M9).
- **Separate `depth` and `ply`** — mate-score arithmetic needs `ply` (distance from root); termination needs `depth` (remaining). Conflating them is a classic bug source — recovered as a "should-fix" in TalkChess folklore on alpha-beta refactors.
- **`pv: &mut PvLine`** — explicit, not threaded through `SearchContext`. Each frame writes to its own `PvLine` (per-ply slice into a triangular table — see §3).
- **No `&mut self` on the search inner loop** — keeps borrow checker compliant when the search holds caches (killer table in M4, history in M4). M3 has no per-thread state to mutate from inside negamax, so the `&mut self` could be elided, but keeping it consistent with `Search::go(&mut self, ...)` simplifies the M4 transition.

### 1.2 Fail-soft vs fail-hard

| Property | Fail-hard | Fail-soft |
|---|---|---|
| Returned score | Clamped to `[alpha, beta]` | May exceed bounds |
| Information loss | Yes (bounds clip true value) | No |
| Nodes searched | More (CPW Fail-Soft article: Fishburn 1983 showed fail-soft is strictly fewer-or-equal) | Fewer-or-equal |
| TT interaction | Cleaner (bounds always satisfy `alpha ≤ stored ≤ beta`) | More care needed (stored bounds may be tighter than the search window) |
| Wikipedia claim | "Fail-hard" | "Fail-soft … nearly universal" ([Wikipedia: Alpha–beta pruning](https://en.wikipedia.org/wiki/Alpha%E2%80%93beta_pruning)) |

**Recommendation: fail-soft.** Two reasons:

- M3 has no TT, so the historical TT-interaction concern doesn't bind.
- M4 will land TT *and* aspiration windows; landing fail-soft now matches the trajectory and avoids a double conversion.

### 1.3 Bug taxonomy each hides

**Fail-hard masks**:
- A buggy eval that returns a score outside `[-MATE, MATE]` — fail-hard clips it silently to a bound; fail-soft propagates the garbage to the root, where it shows up immediately in `info score cp` lines.
- Miscalculated mate distance — fail-hard clamping at `MATE` discards the ply offset.

**Fail-soft masks**:
- Window-out-of-bounds bugs at re-search time (M4 aspiration windows). Fail-hard would have failed loudly with `bestmove` matching a clipped value; fail-soft re-searches with the wider window and the bug is invisible until a later iteration's score regression.
- TT entries with bounds tighter than the current search window. Fail-soft is more likely to return a value outside the current window when the TT supplies it.

**Net**: M3 fail-soft is the cleaner choice. The TT-interaction failure modes are M4 concerns.

### 1.4 Score type and mate-score representation

The score type is determined by what fits **alongside the mate range** in the chosen integer width.

`i16::MAX = 32767`. Common allocation:

| Constant | Value | Meaning |
|---|---|---|
| `INF` | 32001 | Unbeatable bound; used as initial `(alpha, beta) = (-INF, INF)` at root |
| `MATE` | 32000 | Side-to-move delivers mate at ply 0 |
| `MATE_IN_MAX_PLY` | 32000 - 256 = 31744 | Threshold for "this is a mate score" — any \|score\| ≥ this is a mate |
| Eval range | `[-30000, 30000]` | Material-imbalance saturation in a sane eval |

Eval is centipawns (per ADR/architecture commitments). 30000 cp = 300 pawns ≈ "nine queens of imbalance" — comfortable headroom.

CPW's [Score](https://www.chessprogramming.org/Score) article and [Checkmate](https://www.chessprogramming.org/Checkmate) page confirm `i16` (or 15-bit-into-i16) is standard; CPW's `VALUE_MATED = SHRT_MIN/2` formulation (= -16384) is mathematically equivalent. The ±32000 idiom is more common in modern engine prose.

**Why not `i32`**: bigger struct sizes, no SIMD payoff at M3 stage, no reason. Convert to `i32` if M6+ eval demands wider range — it's a one-type refactor.

### 1.5 PV-Search / NegaScout

CPW [Principal Variation Search](https://www.chessprogramming.org/Principal_Variation_Search): "expects about 10% of a search effort" savings *with good move ordering*. The ~10% comes from zero-window scout searches on non-PV-move children; if the first move is the best (high-frequency cutoff), the scout proves the rest are worse cheaply.

**Defer to M4.** Two reasons:
- The 10% savings depend on **good move ordering** (TT move first, then MVV-LVA, then killers/history). M3 has only MVV-LVA; the cutoff frequency is much lower, the scouts re-search more often, and the savings shrink.
- PVS code is a fork in negamax that's easier to write *once* alongside the TT-move ordering hook (M4) than to bolt on now and refactor.

---

## 2. Mate scoring conventions

### 2.1 Internal representation

`MATE - ply` for "I deliver mate at this ply". Worked example, using `MATE = 32000`:

| Position | Score returned by negamax at that node |
|---|---|
| Side-to-move is checkmated (no legal moves, in check) at ply 0 | `-MATE` = -32000 |
| Side-to-move is checkmated at ply 5 | `-MATE + 5` = -31995 |
| Side-to-move can deliver mate in 1 ply (opponent is mated next) | `MATE - 1` = 31999 |
| Side-to-move can deliver mate in 5 plies | `MATE - 5` = 31995 |

The `+ ply` in the mated-side score and `- ply` in the mating-side score together create **faster-mate preference**: a mate in 3 plies (-31997 from the mated side) is *worse* than a mate in 5 plies (-31995), so a side trying to escape mate prefers the slower one; conversely a mater prefers the faster (higher) score. CPW [Mate Distance Pruning](https://www.chessprogramming.org/Mate_Distance_Pruning) confirms: *"the difference between the actual value and SCORE_MATE is always the number of plies the mate is away from the root position."*

### 2.2 The "is this a mate score" test

```
is_mate_score(s) = abs(s) >= MATE_IN_MAX_PLY
```

This is what triggers ply adjustment for the UCI output and (in M4) for the TT.

### 2.3 UCI `score mate y` translation

The UCI 2006 spec says: *"mate in y moves, not plies. If the engine is getting mated use negativ values for y."*

From the engine's internal score (positive = side-to-move-at-root delivers mate):

```
plies_to_mate = MATE - score          // for positive (winning) mate scores
moves_to_mate = (plies_to_mate + 1) / 2   // round up: ply 1 → mate in 1; ply 2 → mate in 1; ply 3 → mate in 2
sign:           positive
```

For a negative (losing) mate:

```
plies_to_mate = MATE + score          // score is negative; result is positive ply distance
moves_to_mate = (plies_to_mate + 1) / 2
sign:           negative
```

The `(plies + 1) / 2` rounding is per the UCI convention — moves are full moves (white + black). Mate-in-1-move covers both `mate-in-1-ply` and `mate-in-2-ply` situations (the latter being "I move, opponent has only one response, I mate"). Worked example: `score = MATE - 1 = 31999` → `plies = 1` → `moves = 1` → emit `score mate 1`. `score = MATE - 5 = 31995` → `plies = 5` → `moves = 3` → emit `score mate 3`.

### 2.4 Mate-distance pruning

CPW pseudocode:

```
mating_value = MATE - ply            // best possible from here
if mating_value < beta:
    beta = mating_value
    if alpha >= mating_value: return mating_value

mating_value = -MATE + ply           // worst possible from here
if mating_value > alpha:
    alpha = mating_value
    if beta <= mating_value: return mating_value
```

**Defer to M4.** The CPW article documents "marginal playing strength improvements." It pays off most in mate-finding analysis runs, not in the mid-game tactical calls that dominate fast-time-control play. M4 is the right home — it lands alongside TT (which has its own ply-adjustment logic for mate scores, and the two interact).

---

## 3. PV recovery without TT

### 3.1 The technique

The triangular PV table exploits "max PV length at depth d is `MAX_PLY - d`" — the array is laid out as concentric triangles, one per ply.

Layout (from the CPW [Triangular PV-Table](https://www.chessprogramming.org/Triangular_PV-Table) page):

```
ply 0: [m0 m1 m2 m3 ... m63]    (MAX_PLY moves)
ply 1: [m0 m1 m2 ... m62]       (MAX_PLY - 1 moves)
ply 2: [m0 m1 m2 ... m61]       (MAX_PLY - 2 moves)
...
ply MAX_PLY-1: [m0]              (1 move)
```

Total slot count `= MAX_PLY * (MAX_PLY + 1) / 2`. At MAX_PLY=64: 2080 `Move` slots × 2 bytes/Move = **4 KiB**. Stack-allocatable; one `[Move; 2080]` lives on the search struct.

### 3.2 Update rule

When a move improves alpha at ply `p`:

```
pv[p][0] = move
copy pv[p+1][0..len(p+1)] into pv[p][1..]
len(p) = 1 + len(p+1)
```

The "copy" is `memcpy` of a small slice — typically <10 moves at non-trivial depth. CPW notes the cost is "rarely a measurable performance penalty."

### 3.3 Bug taxonomy

- **Off-by-one in ply indexing**. Easy to write `pv[p+1]` where `pv[p]` is meant. Catch with a property test: any returned PV's first move, when applied to root, must be a legal move; subsequent moves applied in sequence must each remain legal.
- **PV not cleared on alpha-improvement-but-cancelled subtree**. If a recursive call returns a "score better than alpha" but was actually cancelled (M3.D abort partway through the iteration), copying its half-built PV into the parent corrupts. Fix: check `ctx.aborted` after every recursive call before applying the score-and-PV-update.
- **PV from a different iteration**. If iteration `d-1`'s PV is reused as the M4 ordering hint, but iteration `d` only partially completes and leaves stale entries, the displayed PV may not match the actual played `bestmove`. Discipline: only emit the PV for *completed* iterations; keep the previous iteration's PV as the "displayed" one if cancellation hits during iteration `d`.

### 3.4 No TT alternative — confirmation

CPW [Principal Variation](https://www.chessprogramming.org/Principal_Variation): "TT-overwrites may shorten the PV." With no TT in M3, the alternative doesn't apply. Triangular table is the only path to a working PV in M3.

---

## 4. Quiescence search design

### 4.1 Structural template

```
fn qsearch(pos: &mut Position, mut alpha: i16, beta: i16, ply: i16, ctx: &SearchContext) -> i16:
    if ctx.should_abort(...): return 0
    let in_check = movegen::in_check(pos)
    if !in_check:
        let stand_pat = eval(pos)
        if stand_pat >= beta: return stand_pat       // fail-soft beta cutoff
        if stand_pat > alpha: alpha = stand_pat
    let moves = if in_check { generate_moves(pos) } else { generate_captures_and_promos(pos) }
    if in_check && moves.is_empty(): return -MATE + ply   // mate
    let mut best = if in_check { -INF } else { stand_pat }
    sort_moves_mvv_lva(&mut moves)
    for mv in moves:
        make_move(pos, mv)
        let score = -qsearch(pos, -beta, -alpha, ply+1, ctx)
        unmake_move(pos, mv, undo)
        if score > best: best = score
        if score > alpha: alpha = score
        if alpha >= beta: break                      // fail-soft cutoff
    return best
```

### 4.2 Stand-pat baseline

CPW [Quiescence Search](https://www.chessprogramming.org/Quiescence_Search): *"the position's evaluation is used to establish a lower bound on the score"* — this is the **null-move-equivalent** assumption that "the side to move can usually find a move at least as good as the static eval." It's wrong in zugzwang (M5 null-move pruning has the same issue), but the qsearch budget per leaf is tiny, so the harm is bounded.

**Disallow stand-pat in check.** From [TalkChess search](https://talkchess.com/forum3/viewtopic.php?t=49429) prose: *"if the player is in check, you are not allowed to use stand-pat, since you might be ignoring checkmate."* Concrete: in check, stand-pat would let the search return `eval(pos) ≥ beta` without any move, claiming "I can hold this position" while the king is actually being mated next ply.

### 4.3 Move scope: captures + queen promos, no checks

| Move type | Include? | Rationale |
|---|---|---|
| Captures | Yes | The *defining* qsearch move — resolves material imbalance. |
| Queen promotions (with or without capture) | Yes | Material change of ~+800 cp; static eval *just before* a promo wildly underestimates. |
| Underpromos | **No for M3**. | Adds noise (MVV-LVA tied with queen promo); the missed Allumwandlung tactical case is rare enough at M3 strength to not matter. |
| Quiet checks | **No for M3**. | Without good ordering / SEE, qsearch tree explodes (CPW: *"limiting the generation of checks to the first X plies of quiescence"*). M4+ when ordering is mature. |
| Check evasions | Yes (when in check) — generate **all** moves, not just captures, since stand-pat is disallowed. |

### 4.4 Delta pruning

CPW [Delta Pruning](https://www.chessprogramming.org/Delta_Pruning): `if stand_pat + capture_value + safety_margin < alpha: skip`. Skips captures that can't recover the material deficit even if they win the captured piece "for free."

**Defer to M3.C+1 or M4.** Two open questions block it now:
- Material values aren't fixed at M3.B time — eval research (M3.A) may pick a non-100/300/500/900 scale that needs the delta margin tuned.
- The technique requires confidence in eval termsymmetry — turning it on with a buggy eval can cause silent move-ordering regressions caught only by SPRT.

Add in a follow-up phase once the eval scale is settled.

### 4.5 Terminal detection in qsearch

The hard case: qsearch generates only captures (when not in check), so an empty move list is *not* "stalemate" — it's just "no captures available." The position may have many quiet moves.

**Rules**:
- Not in check, no captures generated → **stand-pat is the answer**, no terminal claim.
- In check, no moves generated → **mate**: return `-MATE + ply`.
- In check, some moves generated → recurse normally; never claim mate.

False stalemate: documented in [TalkChess](https://talkchess.com/forum3/viewtopic.php?t=49429) and [CPW Stalemate](https://www.chessprogramming.org/Stalemate). The classic bug: qsearch returns a static eval near 0 for a position that's actually mate, because the quiescence move list was empty *for non-mating reasons* (no captures) and the engine read it as "quiet position, eval applies." Fixed by the rules above: only claim mate **when in check** with no legal moves.

### 4.6 Recommended scope for M3.C

- Stand-pat (with in-check disallow).
- Captures only (when not in check), MVV-LVA-ordered.
- Queen promos and queen-promo-captures.
- Full move generation when in check (evasions); no stand-pat.
- Mate detection on in-check + no-moves.
- **No** delta pruning, **no** SEE, **no** quiet checks, **no** depth cap (the natural depth cap is "no more captures available" — typically resolves in <10 ply).

---

## 5. Move ordering at M3 stage

### 5.1 Available signals

With no TT, no killer, no history: the only ordering signal is **the move's intrinsic properties**.

| Signal | Source | Cost | Useful in M3? |
|---|---|---|---|
| MVV-LVA | piece values from eval | O(n) over move list | Yes — primary |
| In-check (move gives check) | movegen could return | per-move check test ~50 ns | No — too expensive without batching |
| Captures-before-quiets | flag bits in `Move` | free (sort key) | Yes — implied by MVV-LVA |
| Promotion piece | flag bits in `Move` | free | Yes — queen promo at top |
| PV-move-from-previous-iter | iterative deepening loop | free at root only | Yes — root only (see §6.3) |
| TT-move | (M4) | — | Defer to M4 |
| Killer | (M4) | — | Defer to M4 |
| History | (M4) | — | Defer to M4 |

### 5.2 MVV-LVA scoring formula

CPW [MVV-LVA](https://www.chessprogramming.org/MVV-LVA): victim value first, attacker value (negated) second. Common encoding:

```
score = victim_value * 16 - attacker_value
// Or, with piece kinds 0..6 and a 6x6 lookup:
score = MVV_LVA_TABLE[victim_kind][attacker_kind]
```

Both work. The lookup table is constant-time and avoids a multiply; the formula is one fewer cache line. For M3 either is fine.

**Edge cases**:
- **En passant**: victim is a pawn, on a square different from `to`. Score normally as PxP — no special case needed if MVV-LVA reads the move flag.
- **Promotion-capture**: victim is whatever's on `to`; the *promoted* piece is also a value bonus. Score `victim_value + promo_bonus` to push promo-captures above plain captures.
- **Plain promotion (non-capture)**: no victim. Score as a capture of an "empty" with `promo_bonus` — places it between captures and quiets.

### 5.3 Quiet move ordering

**Movegen order is fine for M3.** Movegen emits in piece-type order (king, queens, rooks, bishops, knights, pawns by piece-type stride), then by destination — a reasonable proxy for "active moves first" without per-move computation. M4's killer/history will replace this.

---

## 6. Iterative deepening discipline

### 6.1 Outer loop

```
fn search_iterative(pos, ctx) -> SearchResult:
    let mut last_completed = SearchResult::default()
    for depth in 1..=MAX_PLY:
        if ctx.deadline_too_close_for_next_iter(): break    // soft bound (M3.D research)
        let mut pv = PvLine::new()
        let score = negamax(pos, depth, 0, -INF, INF, &ctx, &mut pv)
        if ctx.aborted():
            break    // discard partial-iteration result
        last_completed = SearchResult { depth, score, pv, ... }
        emit_info_line(last_completed)
        if is_mate_score(score): break                      // found mate, no point deeper
    last_completed
```

### 6.2 When to abort vs complete

CPW [Iterative Deepening](https://www.chessprogramming.org/Iterative_Deepening): *"in case of an unfinished search, the program always has the option to fall back to the move selected in the last iteration of the search."*

- **Abort mid-iteration** on hard deadline (cancellation flag flips, or `Instant::now() >= deadline`). Discard the iteration's score and PV. The bestmove returned is the **previous iteration's bestmove**.
- **Complete the iteration** if the deadline allows. Emit `info` with the new score/PV. Update `last_completed`.

The "previous iteration's PV is preserved" invariant is what triangle-table users have to be careful about: don't write into `last_completed.pv` from a cancelled iteration.

### 6.3 PV from previous iter as ordering hint

**At the root only, in M3.** Tradeoffs:

| Approach | Implementation cost | Benefit |
|---|---|---|
| Root: try previous-iter best move first | 1 comparison at root | Significant — root cutoff cascades |
| All plies: try previous-iter PV move first | Requires PV path + ply-indexed lookup | Moderate, but messy without TT — defer to M4 TT-move |

The root-only version is essentially free: the iterative-deepening loop already has `last_completed.pv[0]`; pass it to negamax as `Option<Move>` and try it before sorting if present. CPW: *"most important inside an iterative deepening framework is to try the principal variation of the previous iteration as the leftmost path for the next iteration"* — but the marginal gain past the root in plain alpha-beta (no TT) is small enough that the engineering cost isn't worth it in M3.

### 6.4 Aspiration windows

CPW [Aspiration Windows](https://www.chessprogramming.org/Aspiration_Windows): narrow `[score - δ, score + δ]` around the previous iteration's score; on fail-high/fail-low, widen the failed bound exponentially.

**Defer to M4.** Reasoning:
- Aspiration windows pay off when most iterations stay near the previous score (stable position). At M3 strength with shallow depth, scores swing more between iterations as new tactics enter the horizon.
- The widening logic interacts with fail-soft re-search semantics; testing it in isolation from TT (M4) means SPRT-validating it twice.

---

## 7. Cancellation cadence

### 7.1 Confirmation: 4096 nodes

The M2 research note (`docs/research/m2-uci-threading.md` §3.1, §9) settled on 4096 nodes. M3 search is slower per-node than perft (eval at every leaf, MVV-LVA sort, qsearch), so the wall-clock interval grows:

| Engine state | Nodes/sec | 4096-node interval |
|---|---|---|
| M1.G perft bulk | 119 Mnps | ~34 µs |
| M3 search (estimated) | 1 Mnps | ~4 ms |

4 ms is fine — well inside the M2 budget (`stop` → `bestmove` < 10 ms steady state). The cadence stays at 4096; no reason to change.

### 7.2 Where the poll lives

**Top of `negamax`, after the depth==0 / qsearch-entry check, before move generation.**

```
fn negamax(...):
    if depth <= 0: return qsearch(...)
    if ctx.should_abort(self.nodes): return 0   // ← here
    self.nodes += 1
    // ... rest
```

The poll is one `AtomicBool::load(Relaxed)` + an `Instant::now()` comparison + a conditional `nodes >= limits.nodes` — all of these are inlined. The cost at 1 Mnps and 4096-node cadence is ~244 polls/sec, ~1 µs each, ~0.025% overhead. Negligible.

**Same poll inside qsearch.** Qsearch leaves dominate node count (5–10× the main-search leaf count is a CPW-prose ballpark); polling there too keeps the worst-case latency bounded.

### 7.3 Clean abort propagation

The "return 0 from cancelled subtree" sentinel is fine **if and only if** the parent never accepts the score. Discipline:

```
let score = -negamax(pos, depth-1, ply+1, -beta, -alpha, ctx, &mut child_pv);
unmake_move(pos, mv, undo);
if ctx.aborted(): return 0;           // ← propagate cancellation, don't update best/pv
if score > best: { best = score; ... }
```

`ctx.aborted()` is the same `should_abort` reading the same flag — but we read it *after* the recursive call returned (which may have flipped from a deeper node). The post-call check is what guarantees the score is valid before being committed.

Alternative: return `Option<i16>` from negamax. Cleaner type signature, but every recursive call site has an extra pattern-match on the hot path. Sentinel `0` + post-call check is the lower-overhead idiom.

---

## 8. Repetition + 50-move rule in search

### 8.1 What we have, what we don't

| Component | Status |
|---|---|
| Halfmove clock in `Position` | Present (M1.B) |
| Zobrist hash in `Position` | Present (M1.D), incrementally maintained (M1.E) |
| Game-history position list (pre-search) | **Not present** |
| Search-stack repetition | **Not present** |

### 8.2 Recommendation: minimal in-search repetition stack

**Implement** in M3.B as part of `SearchContext`:

```rust
pub struct SearchContext {
    // ... existing fields ...
    pub history: Vec<u64>,           // Zobrist keys of all positions on the search stack + game history
}
```

**Push on entry to negamax, pop on exit:**

```
ctx.history.push(pos.zobrist());
let score = -negamax(...);
ctx.history.pop();
```

**Repetition check at top of negamax** (before move generation, after depth-zero check):

```
if pos.halfmove_clock() >= 100: return DRAW_SCORE          // 50-move rule
if is_repetition(&ctx.history, pos.zobrist(), pos.halfmove_clock()):
    return DRAW_SCORE
```

`is_repetition` scans backward through `ctx.history` only as far as the halfmove clock — captures and pawn moves reset the clock and break repetition chains, so the search range is bounded.

### 8.3 First-repetition vs threefold

CPW [Repetitions](https://www.chessprogramming.org/Repetitions): *"during search, when being at least two plies away from root, most people assume that it is safe to return a draw score already on the first repetition."*

**Use first-repetition** in M3. Rationale: drawing a position twice in a search tree means the search is in a cycle — playing on doesn't improve anything, and conceding the draw lets the search prune the cycle. Threefold is the legal claim rule, not the search rule.

### 8.4 Game-history reconstruction

The UCI `position [startpos | fen X] moves m1 m2 ...` command applies moves to a starting position. To populate the search-time repetition list with pre-search history:

```
fn handle_position(...):
    // existing: parse FEN/startpos, apply moves
    let mut history = Vec::with_capacity(...);
    history.push(starting_pos.zobrist());
    for mv in moves:
        make_move(&mut pos, mv);
        history.push(pos.zobrist());
    self.position = pos;
    self.history = history;
```

Then `handle_go` clones `self.history` into `SearchContext::history` and search pushes/pops from there.

**Engine refactor scope**: extends `Engine` with `history: Vec<u64>` field, populated in `handle_position`, cleared in `handle_ucinewgame`. ~30 lines.

### 8.5 DRAW_SCORE choice

**Use `0`.** A draw is `0 cp` — neutral. CPW prose discusses "contempt" (offsetting draw score by ±20 cp to avoid drawing against weaker / accept drawing against stronger), but contempt is M5+ tuning territory.

---

## 9. Draw detection in search vs eval

### 9.1 Insufficient material

CPW [Insufficient Material](https://www.chessprogramming.org/Insufficient_Material) (referenced via Draw page) covers KvK, KvKN, KvKB, KvBB-same-color. Detection is a bitboard test:

```
if pawns | rooks | queens == 0:
    let knights_count = popcount(knights)
    let bishops_count = popcount(bishops)
    if knights_count + bishops_count == 0: return true                  // KvK
    if knights_count + bishops_count == 1: return true                  // KvK+minor
    if knights_count == 0 && bishops_count == 2:
        // both bishops same color => insufficient
        if (white_bishops & light_squares == 0 && black_bishops & light_squares == 0)
           || (white_bishops & dark_squares == 0 && black_bishops & dark_squares == 0):
            return true
```

### 9.2 Where it lives

**In eval (M3.A territory).** Three reasons:

- Eval is the only place that owns "what does this position score" knowledge. Search asking "is this drawn?" is asking eval-domain questions; routing it through eval keeps responsibilities clean.
- The bitboard test runs at every leaf anyway (eval is called there); doing it once in eval avoids duplicating the check in search at every node.
- Future eval enhancements (bishops-of-opposite-colors endgames, R+wrong-rook-pawn-vs-K-and-B) extend the same logic — they belong with eval where the piece-square interaction lives.

**Implication for M3.B**: search calls `eval(pos)` and trusts it to return `0` for insufficient-material positions. Mate/stalemate detection (which require movegen) stay in search.

### 9.3 50-move and repetition

**In search.** They're path-dependent (need history); eval is a pure function of the current position. Specifically the 50-move rule needs to know the halfmove clock (state on `Position`), and repetition needs the search-stack history. The split:

| Draw rule | Lives in | Why |
|---|---|---|
| Insufficient material | Eval | Pure function of piece bitboards |
| 50-move rule | Search | Needs `Position::halfmove_clock`, but path-independent enough to live in either; search is cleaner because the response is "return draw score" not "score this position" |
| Repetition | Search | Needs path history (`SearchContext::history`) |
| Stalemate | Search | Needs movegen (no legal moves + not in check → stalemate); detected when `generate_moves` returns empty |
| Checkmate | Search | Same as stalemate but with in_check=true |

---

## 10. Common pitfalls

### 10.1 Score-bound vs node-type confusion

CPW [Node Types](https://www.chessprogramming.org/Node_Types):

| Node type | Score returned | Bound type |
|---|---|---|
| PV-node | Inside `(alpha, beta)` | Exact |
| Cut-node | `≥ beta` | Lower bound |
| All-node | `≤ alpha` | Upper bound |

Bug: storing a Cut-node score as exact in the TT. M3 has no TT so this can't happen yet — but the discipline of returning the correct value in fail-soft (`best`, not `beta` or `alpha`) is what prepares for clean TT integration in M4.

### 10.2 Search instability in fail-soft

CPW [Search Instability](https://www.chessprogramming.org/Search_Instability) lists window-dependent pruning as a cause. M3 has no aspiration / NMP / LMR / futility pruning — instability shouldn't manifest. **But** if the search is re-run (e.g. SPRT regression chasing), the same position may yield slightly different scores depending on iteration count, due to qsearch leaf-eval differences across iterations. Expected; not a bug.

### 10.3 Hash-move legality

Not applicable in M3 (no TT). Flag for M4: any TT-supplied "best move" must be re-validated as legal in the current position before being tried. Position aliasing in Zobrist (collisions, ~1 in 2^50) means a TT entry might contain a move that's illegal in the current position despite hash match.

### 10.4 PV-move-from-previous-iter not in legal move list

Edge case at root: the previous iteration's bestmove may not appear in the current iteration's `searchmoves` filter (if the GUI changed `searchmoves` mid-search — uncommon but spec-legal). Discipline: try-PV-move-first must validate `pv_move` is in the move list before reordering, and skip silently if not.

### 10.5 Mate score not normalized when crossing iteration boundary

If iter `d-1` returns `MATE - 5` and iter `d` is starting from `ply = 0`, the previous score still says "5 plies to mate." This is correct *in the search tree* — the mate distance is path-independent — but if the PV from iter `d-1` is reused as a starting hint at iter `d`, the `ply` index in the new iteration must start fresh. The score itself is the right answer; only the *display* needs the `(MATE - score) / 2` translation per §2.3.

### 10.6 `bestmove` being `0000` when search is cancelled before finding any move

M2.C settled this: always compute the candidate (lex-first legal move) before returning. M3.B: at root, the iterative-deepening loop guarantees iteration 1 completes (1-ply search is microseconds; cancellation happens after that). If iteration 1 doesn't complete (worst-case cancellation arrives within microseconds of `go`), fall back to a lex-first legal move — same M2.C policy.

### 10.7 Returning score from inside qsearch when the position is empty (no captures, in check)

Rule from §4.5: in-check + no-moves → mate. The bug is forgetting that in-check disallows stand-pat *and* changes the move-list scope (all moves, not just captures). Easy to test: construct a back-rank-mate position, force qsearch to be entered, assert `qsearch(...) == -MATE + ply`.

### 10.8 Off-by-one on horizon transition

`negamax(depth=0, ...) → qsearch(...)` is the standard. `negamax(depth=1, ...)` searches one ply, then enters qsearch on the resulting position. Bug pattern: `if depth <= 0 { return eval(pos) }` skips qsearch entirely — quiet positions evaluate fine, but tactical positions (captures still pending) hand the opponent a free move. Fix: replace `eval` with `qsearch` at the horizon.

### 10.9 Eval perspective inverted

Negamax requires eval from the **side-to-move's** perspective. Bug pattern: classical eval is written "white-relative" and the negamax wrapper forgets to negate when black is to move. Pin in tests: `eval(starting_position) == 0` (symmetry); `eval(after_e2e4) ≈ +small` from white's perspective when called with white-to-move — but the search calls it with black-to-move (after e2e4), so the returned score is `-small` (good for black). Property test pair: for any position P, `eval(P) == eval(mirror(P))` after color-swap and side-flip.

### 10.10 MAX_PLY overflow

If MAX_PLY=64, mate scores reach `MATE - 64 = 31936`, well above `MATE_IN_MAX_PLY = 31744`. But if the search ever recurses deeper than MAX_PLY (extensions in M5+), the PV table overflows. M3 has no extensions; cap depth at MAX_PLY-1 in iterative deepening to avoid the case entirely. Add a `debug_assert!(ply < MAX_PLY)` at the top of `negamax`.

---

## 11. Performance budget

### 11.1 Estimation

Per-node cost in M3 (rough breakdown):

| Component | Cost / node |
|---|---|
| Move generation (M1.G headline ~200 ns/call, but search-time positions are deeper / more loaded) | ~400 ns |
| Eval (material + PST, M3.A): ~16 piece additions + ~16 PST lookups | ~100 ns |
| MVV-LVA scoring + sort: O(n log n), n ≈ 20–40 | ~500 ns |
| Make / unmake: M1.E says ~30 ns/cycle quiet, ~40 capture | ~70 ns |
| Cancellation poll (1/4096 nodes) | ~0.001 ns/node amortized |
| Recursion overhead | ~50 ns |
| **Total** | **~1100 ns/node ≈ 0.9 Mnps** |

Qsearch leaf cost is similar, possibly faster (no recursion past horizon). Empirically engines without TT report 0.5–2 Mnps in this configuration; the 1 Mnps estimate is mid-range.

### 11.2 Depth at `tc=10+0.1`

10 seconds + 0.1 increment. Time per move estimate (M3.D research): ~200 ms early, ~100 ms typical, dropping in time pressure. At 1 Mnps and 100 ms per move: 100k nodes/move.

Branching factor with MVV-LVA only (no TT) is roughly `√(legal_moves)` — empirically ~6–10 in mid-game. Effective depth from 100k nodes:

| Effective branching factor | Reachable depth |
|---|---|
| 6 | log_6(100000) ≈ 6.4 |
| 8 | log_8(100000) ≈ 5.5 |
| 10 | log_10(100000) ≈ 5 |

**Estimated search depth: 5–7 main-search ply, with 4–6 plies of qsearch** at typical mid-game positions. Endgames go deeper (smaller move lists). Mate-finding goes deeper (mate scores prune the rest).

### 11.3 SPRT against random mover

CPW prose and TalkChess folklore: alpha-beta + material eval is ~1500–1800 Elo. Random mover is ~−500 Elo. Differential is so large that **SPRT crosses the upper bound after ~5–10 games**. The exit criterion `beats the random mover ~100% via SPRT` (per `docs/roadmap.md` M3) is not at risk; the load-bearing question is whether the engine **hangs / crashes / emits illegal moves** during the run, not the score.

### 11.4 Bench command

The M3.E `bench` UCI command should run a fixed list of positions (~50 from a standard suite like the Bratko-Kopec or WAC tactical sets, or the Stockfish bench corpus reproduced from PGN positions) at fixed depth (`go depth 10` or similar), accumulate total node count, emit:

```
Nodes: <n>
Time:  <ms>
NPS:   <n / ms * 1000>
```

This becomes the deterministic regression check across commits — same node count == same search behavior. Diverging node counts on the same bench positions across commits flag changes that affected ordering or pruning, even when the score doesn't change. Standard discipline in modern engines (Stockfish, Ethereal, etc., per public READMEs).

---

## Citations

- [Chess Programming Wiki — Alpha-Beta](https://www.chessprogramming.org/Alpha-Beta)
- [Chess Programming Wiki — Negamax](https://www.chessprogramming.org/Negamax)
- [Chess Programming Wiki — Fail-Soft](https://www.chessprogramming.org/Fail-Soft)
- [Chess Programming Wiki — Score](https://www.chessprogramming.org/Score)
- [Chess Programming Wiki — Checkmate](https://www.chessprogramming.org/Checkmate)
- [Chess Programming Wiki — Mate Distance Pruning](https://www.chessprogramming.org/Mate_Distance_Pruning)
- [Chess Programming Wiki — Quiescence Search](https://www.chessprogramming.org/Quiescence_Search)
- [Chess Programming Wiki — CPW-Engine quiescence](https://www.chessprogramming.org/CPW-Engine_quiescence)
- [Chess Programming Wiki — Delta Pruning](https://www.chessprogramming.org/Delta_Pruning)
- [Chess Programming Wiki — Stalemate](https://www.chessprogramming.org/Stalemate)
- [Chess Programming Wiki — Iterative Deepening](https://www.chessprogramming.org/Iterative_Deepening)
- [Chess Programming Wiki — Aspiration Windows](https://www.chessprogramming.org/Aspiration_Windows)
- [Chess Programming Wiki — Triangular PV-Table](https://www.chessprogramming.org/Triangular_PV-Table)
- [Chess Programming Wiki — Principal Variation](https://www.chessprogramming.org/Principal_Variation)
- [Chess Programming Wiki — Principal Variation Search](https://www.chessprogramming.org/Principal_Variation_Search)
- [Chess Programming Wiki — Move Ordering](https://www.chessprogramming.org/Move_Ordering)
- [Chess Programming Wiki — MVV-LVA](https://www.chessprogramming.org/MVV-LVA)
- [Chess Programming Wiki — Node Types](https://www.chessprogramming.org/Node_Types)
- [Chess Programming Wiki — Search Instability](https://www.chessprogramming.org/Search_Instability)
- [Chess Programming Wiki — Repetitions](https://www.chessprogramming.org/Repetitions)
- [Chess Programming Wiki — Fifty-move Rule](https://www.chessprogramming.org/Fifty-move_Rule)
- [Chess Programming Wiki — Draw](https://www.chessprogramming.org/Draw)
- [Wikipedia — Alpha-beta pruning](https://en.wikipedia.org/wiki/Alpha%E2%80%93beta_pruning)
- [Wikipedia — Quiescence search](https://en.wikipedia.org/wiki/Quiescence_search)
- [UCI 2006 Specification (WBEC Ridderkerk)](https://www.wbec-ridderkerk.nl/html/UCIProtocol.html)
- [TalkChess — implementing UCI stop command](http://talkchess.com/forum3/viewtopic.php?t=46368)
- [TalkChess — stalemate detection and pruning](https://talkchess.com/forum3/viewtopic.php?t=49429)
- [dogeystamp — Chess engine, pt. 4: α-β pruning and better search](https://www.dogeystamp.com/chess4/)
- [dogeystamp — Chess engine, pt. 5: Quiescence search, endgames, repetition avoidance](https://www.dogeystamp.com/chess5/)
- [python-chess UCI/XBoard engine communication](https://python-chess.readthedocs.io/en/latest/engine.html) (mate-score sign convention reference)
