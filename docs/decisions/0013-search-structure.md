# ADR-0013 — Search structure: fail-soft negamax + triangular PV + ply-adjusted mate scores + mate-distance pruning

Status: accepted (M3.C, 2026-04-28).

## Context

M3.C is the first phase that *searches* — replaces depth-1 GreedyMover with full alpha-beta recursion. Several structural choices bind once the negamax body is written; rewriting them later costs Elo (via SPRT instability while the change settles) and code (every recursive call site touches the same data). The choices and tradeoffs are documented in `docs/research/m3-search-basics.md` (Sonnet) and `docs/research/m3-search-basics.opus.md` (Opus parallel pass). Both research passes converged on every headline call modulo wording.

## Decision

### 1. Fail-soft negamax

Returns the best score actually found, not clamped to `[alpha, beta]`. Per CPW "Fail-Soft" / Fishburn 1983 — strictly fewer-or-equal nodes than fail-hard, no instability without a TT (TT lands M4). Bug taxonomy: fail-soft surfaces score-out-of-range bugs immediately at the root rather than masking them in a clamp.

### 2. Score type: `i32`

`MATE = 30000`, `INF = 30001`, `MATE_IN_MAX_PLY = MATE - MAX_PLY = 29936`. Eval saturation comfortably below `MATE_IN_MAX_PLY`.

Why `i32`: matches `eval::evaluate(&Position) -> i32` and existing `SearchResult::score_cp: Option<i32>`. No casts at module boundaries. The Opus research §1.4 case for `i16` (SIMD-friendly NNUE inference) is real but pays off at M9, not M3 — converting later is one type alias.

### 3. Mate scoring: ply-adjusted

| Position | Negamax returns |
|---|---|
| Side-to-move mated, ply n | `-(MATE - n)` |
| Side-to-move delivers mate at ply n | `MATE - n` |
| Drawn position (stalemate, repetition, 50-move, insufficient material) | `0` |

`MATE - ply` for the mating side ⇒ faster mate scores higher. `-(MATE - ply)` for the mated side ⇒ slower mate (further away) scores higher (less bad). Side trying to escape mate prefers slower; mater prefers faster. UCI emit `score mate N` with `N = sign(score) * ((MATE - |score| + 1) / 2)` — full moves rounded up, sign positive when winning.

### 4. Mate-distance pruning

```rust
let mating_value = MATE - ply;
if mating_value < beta {
    beta = mating_value;
    if alpha >= mating_value { return mating_value; }
}
let mated_value = -(MATE - ply);
if mated_value > alpha {
    alpha = mated_value;
    if beta <= mated_value { return mated_value; }
}
```

Five lines at the top of negamax. CPW: "safe type of pruning" — only cuts already-decided branches. Marginal Elo gain; included now because the cost is negligible and it documents the mate-bound semantics in code.

(Opus research recommends deferring; we honor the roadmap's commitment to include in M3.C. The cost difference is rounding-error.)

### 5. Triangular PV table

Two-dimensional array `[[Move; MAX_PLY]; MAX_PLY]` with per-ply length `[usize; MAX_PLY]`. Total ~8 KiB at `MAX_PLY = 64`. Update rule per CPW "Triangular PV-Table":

```
pv.moves[ply][0] = mv
copy pv.moves[ply+1][0..pv.lengths[ply+1]] into pv.moves[ply][1..]
pv.lengths[ply] = 1 + pv.lengths[ply+1]
```

Update fires only at PV nodes (where a move improves alpha) and only when the recursive subtree completed (not aborted).

**Parallel root-score invariant.** A separate `root_score: Option<i32>` field on `AlphaBetaMover` is maintained in lockstep with the root PV update — same condition (`ply == 0 && score > alpha`), same call site. `Search::go` consumes it via `if self.aborted { self.root_score.unwrap_or(0) } else { returned_score }`, so the emitted `info ... score cp <N>` line reflects the score of the *completed* root subtree under partial-abort scenarios — not the abort sentinel 0 propagated by negamax. The `unwrap_or(0)` covers the pure-abort case (no root move improved alpha → `root_score` stays `None`, score 0 paired with empty PV — internally consistent). This invariant was added in M3.C v2 after final-review surfaced score-contamination under quit-during-search.

### 6. Move ordering (M3.C scope only)

- **Captures**: MVV-LVA, scored as `victim_value × 16 - attacker_value` using PeSTO MG values (P=82, N=337, B=365, R=477, Q=1025, K=0).
  - **EnPassant**: victim is a pawn (the capture-square is `to ± 8`, but MVV-LVA only needs the victim *kind*).
  - **`*PromoCapture`**: score = `victim_value × 16 - attacker_value + promo_value`. Pushes queen-promo-captures to the top.
  - **`*Promo` (non-capture)**: score = `promo_value` only — no victim, no attacker subtraction.
- **Quiets** (Quiet / DoublePush / KingCastle / QueenCastle): score 0. Movegen order. M4 will replace with killer/history.

**Worked-out ordering with PeSTO values:**

| Move | Score | Note |
|---|---|---|
| QxQ | 1025·16 − 1025 = 15375 | high tier |
| RxQ | 1025·16 − 477 = 15923 | RxQ > QxQ ✓ |
| PxQ | 1025·16 − 82 = 16318 | smaller attacker, larger margin ✓ |
| Q-promo (non-capture) | 1025 | between large captures and small captures |
| QxR | 477·16 − 1025 = 6607 | victim still dominates |
| QxP | 82·16 − 1025 = 287 | small victim, large attacker — *below* a non-capture queen-promo (1025) |
| Quiet / DoublePush / Castle | 0 | bottom tier |

The "non-capture queen-promo (1025) > QxP (287)" ordering is intentional: a non-capture queen-promotion gains ~+1000 cp of material (a pawn becoming a queen ≈ 1025 − 82 = 943 cp), while QxP gains only ~+82 cp. Searching the promotion first surfaces the larger material swing earlier in the alpha-beta tree. Consistent with the "promotions are big material events" intuition that motivates qsearch's queen-promo inclusion (M3.D) and is why we use `promo_value` as the bonus rather than a fixed constant.

PV-move-from-prior-iteration ordering is **deferred**: M3.C is single-iteration (no ID), so there is no "prior iteration" — that hint binds at M3.E when iterative deepening lands.

### 7. Cancellation: 4096-node cadence + sentinel-return

Top of `negamax` (after early outs, before move generation):

```
self.nodes += 1;
if self.nodes & 4095 == 0 && ctx.should_abort(self.nodes) {
    self.aborted = true;
    return 0;
}
```

After each recursive call, before updating `best`:

```
let score = -self.negamax(...);
self.history.pop();
pos.unmake_move(mv, undo);
if self.aborted { return 0; }
if score > best { best = score; ... }
```

`self.aborted: bool` is a per-search flag set once and checked at every call site. Sentinel return value `0` is never trusted — the post-call `aborted` check skips the score-update and PV-update.

### 8. Repetition + 50-move integration (M3.B helpers)

At top of `negamax`, after early outs, **only at `ply > 0`** (root must always return a move):

```
if ply > 0 {
    if is_fifty_move_draw(pos.halfmove_clock()) { return 0; }
    if is_repetition(&self.history, pos.halfmove_clock()) { return 0; }
}
```

`self.history: Vec<u64>` is the search-owned mutable history, initialized at the top of `Search::go` from `ctx.history.clone()` (i.e. cloned twice from `Engine::game_history`: once at `handle_go` time into `ctx.history`, once at `Search::go` time into `self.history`). The double-clone is intentional and resolves M3.B's `SearchContext::history` location-deferral question (M3.B plan §11): Search owns its own mutable history; SearchContext stays immutable, the trait stays unchanged.

Push/pop happens around each recursive call (caller-side, post-make / pre-unmake):

```
pos.make_move(mv); let undo = ...;
self.history.push(pos.zobrist());
let score = -self.negamax(pos, depth-1, ply+1, -beta, -alpha, ctx, ...);
self.history.pop();
pos.unmake_move(mv, undo);
```

The M3.B invariant `self.history.last() == pos.zobrist()` is maintained throughout the recursion.

### 9. Quiescence search and iterative deepening

**Both deferred.** Qsearch is M3.D; iterative deepening + time management is M3.E. M3.C calls `evaluate(pos)` at `depth == 0` (horizon effect accepted; the chosen tactical leaf is *some* leaf, not the worst-case adversarial leaf). `Search::go` runs a single negamax invocation at the requested `go depth N` (default 4 if no depth specified — sane fallback for users who don't pass `depth`).

### 10. PVS / aspiration windows / killers / history / TT

All deferred to M4 per the research consensus. M3.C is plain alpha-beta + MVV-LVA only.

## Consequences

- **Code surface**: ~700 LOC across `src/search.rs` (negamax body, qsearch-leaf-stub via direct `evaluate` call, PV table, MVV-LVA scorer, mate-score conversion) + ~30 LOC in `src/engine.rs` for the production switch.
- **`Search` trait unchanged**: `Search::go(&mut self, &Position, &SearchContext, &dyn Fn(&str)) -> SearchResult` keeps the M2.C signature. M3.B's deferred `SearchContext::history` mutation question resolves to "search owns its own history; SearchContext stays immutable."
- **`GreedyMover` deleted**. Same M3.A treatment of `RandomMover`. The `Random_Seed` UCI option is **preserved as a no-op for M3.C** — the value has no observable effect on alpha-beta (the search is deterministic given position + depth), but the option parses, accepts, and `set_seed` is still invoked on `AlphaBetaMover` (default no-op trait method). Removing the option would touch `scripts/match.sh`'s self-play target (which differentiates the two engines via `option.Random_Seed=1` vs `=2`) and a non-trivial chunk of M2.D's test surface — out of M3.C's search-implementation scope. The cleanup is deferred to a separate commit once the strength-validation harness no longer relies on per-engine seed-driven divergence.
- **Tests already cover the M3.B helpers**. M3.C adds tests for negamax behaviors (mate finding, repetition draw, alpha-beta cutoffs, PV recovery, cancellation propagation).

## Alternatives rejected

| Alternative | Why rejected |
|---|---|
| Fail-hard | Worse node counts (Fishburn 1983); masks score-out-of-range bugs that fail-soft surfaces; no TT-interaction concern at M3 since no TT exists. |
| `i16` scores | SIMD payoff is M9 (NNUE inference); cost of casting at the `eval` boundary is non-zero today for zero benefit today. |
| `MATE = 32000` | `MATE = 30000` is enough headroom (eval saturates at ~3000 cp; MATE_IN_MAX_PLY at 29936 is well clear). 32000 vs 30000 is wash; pick the smaller for slightly more eval headroom. |
| Defer mate-distance pruning to M4 | Roadmap commits to M3.C; cost is 5 lines; safe pruning. Opus research-suggested deferral noted but overridden by roadmap. |
| `Search::go(&mut SearchContext)` | Trait change cascades through every Search impl; the search-owns-its-history alternative keeps the trait stable and the SearchContext immutable. Two-clone cost (~200 bytes per `go`) is negligible. |
| `Search::go` takes a separate `&mut Vec<u64>` parameter | Plumbs an extra reference through every call site. Less coupling but more parameter friction. The search-owns-history clone-from-ctx variant has the same effective semantics with one fewer parameter. |
| Move-ordering: SEE-based capture filter at M3 | Static Exchange Evaluation needs piece values stable and good ordering already in place; adds complexity for marginal gain at M3 strength. M5+. |
| PV-from-previous-iteration root-ordering hint | Requires iterative deepening, deferred to M3.E. M3.C single-iteration alpha-beta has no "previous iteration" to hint from. |

## References

- `docs/research/m3-search-basics.md` (Sonnet) — primary research note.
- `docs/research/m3-search-basics.opus.md` (Opus parallel pass) — calibration A/B; converged on every headline call.
- `docs/plans/m3.c.md` — implementation plan.
