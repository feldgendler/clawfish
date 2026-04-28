# M3 Search Basics — Research Report

Covers negamax alpha-beta framing, mate scoring, PV recovery, quiescence search, move ordering, iterative deepening, cancellation, repetition/50-move detection, draw classification, common pitfalls, and performance budget. No engine source code was read. Sources: Chess Programming Wiki, UCI 2006 spec, TalkChess threads, OpenChess forums, python-chess docs.

## Headline Calls

| Decision | Recommendation |
|---|---|
| Alpha-beta variant | Fail-soft negamax |
| Score type | `i32` throughout; `i16` acceptable but `i32` safer |
| MATE constant | `30_000` (leaves room for pawn values summed; `32_000` also standard) |
| Mate-in-N encoding | `MATE - ply` (winning); `-(MATE - ply)` (losing) |
| UCI `info score mate N` | N is full moves, not plies; divide `(MATE - abs(score))` by 2, rounding up |
| PV recovery (no TT) | Triangular PV table |
| PVS / NegaScout | Defer to M4; too little gain without a TT-move |
| Aspiration windows | Defer to M4; instability without TT creates re-search loops |
| Mate-distance pruning | Include in M3.B; safe, ~5 lines, negligible cost |
| Qsearch scope | Captures only + in-check all-evasions; no checks, no underpromotions |
| Move ordering | MVV-LVA captures first; quiets in movegen order |
| Iterative deepening | Abort between iterations only; never inside a started iteration |
| Repetition detection | Game-history + search-stack Zobrist list; defer 50-move to same phase |
| Draw detection | Insufficient material in eval; repetition/50-move in search |
| Cancellation cadence | Every 2048 nodes; same `should_abort` hook from `SearchContext` |

## 1. Negamax Framing

### 1.1 Recommended signature

```rust
fn negamax(
    pos: &mut Position,
    depth: i32,
    ply: i32,
    mut alpha: i32,
    beta: i32,
    ctx: &SearchContext,
    pv: &mut PvLine,
    nodes: &mut u64,
) -> i32
```

- `depth` counts remaining plies to search; `ply` counts from root (for mate scoring and triangular table indexing).
- `pos` is mutated by make/unmake; pass `&mut` rather than cloning.
- `pv` is the output PV line for this node; caller passes in its own PV row.

### 1.2 Fail-soft vs fail-hard

| Property | Fail-Hard | Fail-Soft |
|---|---|---|
| Return values | Clamped to `[alpha, beta]` | May be outside `[alpha, beta]` |
| Information preserved | Only bound | Actual best score |
| Node count | Slightly higher | Slightly lower |
| Required by PVS/NegaScout | No | Yes |
| Instability risk (no TT) | Lower | Negligible without TT |
| Recommendation | Acceptable | Preferred |

- Fail-soft is the contemporary consensus (CPW Alpha-Beta article, Fishburn 1983).
- The instability problems are all TT-interaction bugs; without a TT, fail-soft is stable.
- **Recommendation: fail-soft.**

### 1.3 Score type

- `i16` fits scores up to ±32767 and a `MATE = 30_000` comfortably.
- `i32` avoids overflow during intermediate multiplications (e.g., `score * side_sign`).
- The negation `-score` on a `MATE` value that's exactly `i16::MIN` will overflow in `i16`.
- **Recommendation: `i32` for all search scores; `i16` only if memory pressure forces it in TT entries (M4).**

### 1.4 PVS / NegaScout at M3

- PVS saves ~10% of search effort (CPW PVS article) only when the first move is consistently best — which requires a TT-move or prior-iteration PV move to be searched first.
- Without a TT move, the first move in M3 is just the first capture (MVV-LVA) or the first quiet move; there is no strong prior ordering guarantee.
- **Recommendation: plain fail-soft alpha-beta for M3; add PVS when TT lands in M4.**

## 2. Mate Scoring Conventions

### 2.1 Score range layout

```
-MATE        = losing position; mated on the board
-(MATE - ply) = getting mated in `ply` plies from root
0            = drawn position
+(MATE - ply) = delivering mate in `ply` plies from root
+MATE        = (unused; the winning position one step before)
```

- Standard constant: `MATE = 30_000` (CPW Score article uses `SHRT_MAX/2 ≈ 16384`; many engines use 30000–32000).
- Using `30_000` leaves ~2_000 cp of headroom below `i16::MAX` and is safely distant from any realistic material sum.

### 2.2 Mate detection at leaves

```rust
if move_count == 0 {
    if in_check(pos) {
        return -(MATE - ply);
    } else {
        return 0;
    }
}
```

- Ply-adjustment ensures a mate-in-1 scores higher than a mate-in-3.
- `-(MATE - ply)` means `ply = 1` gives `-29999`, `ply = 3` gives `-29997` — closer to 0, so the root prefers ply = 1.

### 2.3 Mate distance pruning

- Safe: cuts only already-decided branches (CPW Mate Distance Pruning: "safe type of pruning").
- ~5 lines; fires only when a mating line is already known.
- **Recommendation: include in M3.B.**

### 2.4 UCI `info score mate N` conversion

- UCI spec: "mate in y moves, not plies."
- `N` is full moves (1 move = 2 plies).
- Formula: `N = (MATE - abs(score) + 1) / 2`.
- Sign: positive if we are mating; negative if we are being mated.
- Example: internal score `MATE - 5` (mate in 5 plies) → UCI `score mate 3`.

## 3. PV Recovery Without TT

### 3.1 Triangular PV table

- At depth `d`, the PV can be at most `d` moves long.
- Total storage: `½ × d × (d+1)` moves.
- At `d = 64`: `½ × 64 × 65 = 2080` moves × 2 bytes each ≈ **4 KB**.
- At typical M3 depths (10–14 ply): `½ × 14 × 15 = 105` moves ≈ negligible.

### 3.2 Practical design

```rust
struct PvLine {
    length: usize,
    moves: [Move; MAX_PLY],
}
```

- Pass `pv: &mut [PvLine; MAX_PLY]` to `negamax`; index by `ply`.
- Only update when `score > alpha` (i.e., at PV nodes).
- The root's `pv[0]` is the final PV after the search completes.

### 3.3 Alternative: PV from prior iteration (without TT)

- Without a TT there is no persistent storage between iterations.
- The prior iteration's PV can be saved in a separate array and used to order the root's moves on the next iteration.
- **Recommendation: save prior-iteration PV for root move ordering in M3.D; deeper TT-move ordering waits for M4.**

## 4. Quiescence Search

### 4.1 Stand-pat baseline

- If in check: skip stand-pat; generate all legal moves (evasions); if none → `-(MATE - ply)`.
- Else: `stand_pat = evaluate(pos)`. If `stand_pat >= beta` return `stand_pat` (fail-soft). If `stand_pat > alpha` update alpha.

### 4.2 What to extend in M3.C

| Move class | Include? | Rationale |
|---|---|---|
| Captures (all) | Yes | Primary purpose of qsearch |
| Queen promotions | Yes | Material gain, same category as captures |
| Under-promotions (N/B/R) | No | Rarely beneficial; adds complexity |
| Non-capture checks | No | Causes search explosion without delta pruning; defer to M4+ |
| In-check evasions | Yes (all legal) | Position is not quiet; no stand-pat |

### 4.3 Delta pruning

- Prune captures where `stand_pat + captured_piece_value + margin < alpha`.
- Typical margin: ~200 cp.
- **Safety: disable in the endgame** — CPW warns that delta pruning misses transitions to won endgames.
- **Recommendation: skip delta pruning for M3.C** (material eval is simple; the endgame check is fragile).

### 4.4 Terminal detection in qsearch

- Before stand-pat: check `in_check(pos)`.
- If in check: do not stand-pat; generate all legal moves. If none → `-(MATE - ply)`.
- If not in check and no captures: return stand-pat (quiet position).
- Out-of-check stalemate is invisible to qsearch and will be caught by main search at `depth == 0`.

## 5. Move Ordering at M3 Stage

### 5.1 Priority order (no TT, no killer, no history)

1. **Captures ordered by MVV-LVA**.
2. **Quiet moves in movegen order**.

### 5.2 MVV-LVA scoring

- Score = `victim_value * 10 - attacker_value`.
- Resulting order: PxQ > NxQ > BxQ > RxQ > QxQ > KxQ > PxR > ... > KxP.

### 5.3 What not to add in M3

- Killer / History / SEE all defer to M4 or M5.

## 6. Iterative Deepening

### 6.1 Outer loop

```rust
for depth in 1..=max_depth {
    let score = negamax(pos, depth, 0, -INF, INF, ...);
    if ctx.should_abort(nodes) { break; }
    best_move = pv[0].moves[0];
    best_score = score;
    emit info line;
}
```

### 6.2 Abort discipline

- **Never abort inside a started iteration** for time reasons — always let the iteration complete, then check.
- If time expires mid-iteration: discard it; use the prior iteration's `best_move`.
- The `should_abort` flag fires mid-search; propagate the abort sentinel up the stack.

### 6.3 Prior-iteration PV for ordering (M3.D)

- Save `pv[0]` after each iteration.
- On the next iteration, try the root's prior-iteration best move first.
- Without TT: this hint is root-only.
- **Worth doing even without TT** — root move ordering is the single most impactful ordering decision.

### 6.4 Aspiration windows — defer to M4

- Without a TT, fail-high re-search re-does all the work; benefit over full-window is marginal.
- The interaction creates the classic "fail-high then fail-low" instability.
- **Recommendation: defer to M4.**

## 7. Cancellation Cadence

### 7.1 Where to check

```rust
*nodes += 1;
if *nodes % 2048 == 0 && ctx.should_abort(*nodes) {
    return ABORT_SCORE;
}
```

- Place at top of `negamax` and `qsearch`.

### 7.2 Cadence

- M2 research established 4096 nodes for `RandomMover`.
- Alpha-beta nodes are much faster; 2048 nodes ≈ 10–50 µs at M3 throughput.
- **Recommendation: 2048 for alpha-beta/qsearch; refine by profiling.**

### 7.3 Abort propagation

- An aborted iteration's scores are invalid; do not commit.
- Track `aborted: bool` in `SearchContext`; root checks after `negamax`.

## 8. Repetition and 50-Move Rule in Search

### 8.1 Repetition detection without TT

- Maintain a `history: Vec<u64>` holding Zobrist hashes since game start.
- On `make_move`: push the new hash. On `unmake_move`: pop.
- At each search node: walk backward by 2-ply steps; stop at `halfmove_clock = 0`.
- Inside the search: a **single** prior occurrence counts as a draw.
- In game history: need **two** prior occurrences for the FIDE three-fold claim.

Performance: negligible (CPW: "rarely called").

### 8.2 50-move rule

- Track `halfmove_clock` in `Position` (already present from M1.B).
- In search: if `halfmove_clock >= 100` → return draw score (0).

### 8.3 Current engine state

- The engine does not currently track game-history positions (M2.D explicit non-goal).
- M3 will need to: (a) pass a mutable history list through `SearchContext`, (b) push/pop hashes around make/unmake.

## 9. Draw Detection: Search vs. Eval

| Draw type | Layer | Rationale |
|---|---|---|
| Threefold repetition | Search | Path-dependent; requires position history |
| 50-move rule | Search | Tracked by `halfmove_clock` |
| Insufficient material (KvK, KvN, KvB, KvBBsame) | Eval | Material-only check; no history needed |
| Stalemate | Search (via `move_count == 0 && !in_check`) | No legal moves; detected at leaves |

## 10. Common Pitfalls

- **Score outside `[alpha, beta]` confusion** in fail-soft.
- **Stale PV from aborted iteration** — only update saved PV when iteration completes.
- **`-(MATE - ply)` vs `-MATE + ply` sign error** — equivalent but easy to flip.
- **Forgetting outer `-` at recursive call.**
- **Alpha initialization at root** — use `MATE + 1` not `MATE` to avoid pruning mate-in-1.
- **Qsearch not called at depth 0** — calling `evaluate` directly causes horizon effect.
- **Stand-pat in check** — unsound; engine may "stand pat" on a position where every move loses the king.

## 11. Performance Budget

### 11.1 Node throughput estimate for M3

- M1 movegen: 119 Mnps bulk perft (starting D4); ~33 Mnps plain.
- Expected NPS for alpha-beta: 5–20 Mnps on Apple M4 (judgment).
- Typical C/C++ alpha-beta engine without NNUE runs 1–10 Mnps.

### 11.2 Depth expectation at `tc=10+0.1`

- ~10 s per move × ~10 Mnps = ~100M nodes per move.
- Alpha-beta with good ordering: `O(b^(d/2))` with `b ≈ 35`.
- Effective depth at 10s: **6–9 ply** in main search, plus qsearch extension.

## Citations

- [CPW — Alpha-Beta](https://www.chessprogramming.org/Alpha-Beta)
- [CPW — Fail-Soft](https://www.chessprogramming.org/Fail-Soft)
- [CPW — Negamax](https://www.chessprogramming.org/Negamax)
- [CPW — Quiescence Search](https://www.chessprogramming.org/Quiescence_Search)
- [CPW — Triangular PV-Table](https://www.chessprogramming.org/Triangular_PV-Table)
- [CPW — Iterative Deepening](https://www.chessprogramming.org/Iterative_Deepening)
- [CPW — Mate Distance Pruning](https://www.chessprogramming.org/Mate_Distance_Pruning)
- [CPW — MVV-LVA](https://www.chessprogramming.org/MVV-LVA)
- [CPW — Move Ordering](https://www.chessprogramming.org/Move_Ordering)
- [CPW — Repetitions](https://www.chessprogramming.org/Repetitions)
- [CPW — Score](https://www.chessprogramming.org/Score)
- [CPW — Draw Evaluation](https://www.chessprogramming.org/Draw_Evaluation)
- [CPW — Search Instability](https://www.chessprogramming.org/Search_Instability)
- [CPW — Aspiration Windows](https://www.chessprogramming.org/Aspiration_Windows)
- [CPW — Principal Variation Search](https://www.chessprogramming.org/Principal_Variation_Search)
- [CPW — Nodes per Second](https://www.chessprogramming.org/Nodes_per_Second)
- [UCI Protocol — WBEC Ridderkerk](https://www.wbec-ridderkerk.nl/html/UCIProtocol.html)
- [TalkChess — Repetition Detection #32597](https://talkchess.com/viewtopic.php?t=32597)
- [TalkChess — Mate Distance Pruning #26995](https://talkchess.com/viewtopic.php?t=26995)
- [TalkChess — Search Instability #21365](https://talkchess.com/viewtopic.php?t=21365)
- [OpenChess — Quiescence Best Practices #2852](https://open-chess.org/viewtopic.php?t=2852)
