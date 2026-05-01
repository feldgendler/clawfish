# ADR-0023 — Null-move pruning: gates, reduction formula, zugzwang policy, mate-cap, TT-store

**Status:** Accepted (lands with M5.A).

## Context

M5.A is the first M5 sub-phase. It adds **null-move pruning** to the negamax prologue: at non-PV interior nodes that pass a seven-condition gate, search a reduced-depth "passed turn" position; on `null_score >= beta`, return a fail-high without entering the move loop.

NMP is the largest standalone search-pruning gain in the literature (+30–70 Elo CPW prior). It composes with M4.A's TT (probe before NMP; store after NMP cutoff), M4.B's killers (no interaction), M4.C's history (no interaction), and M4.D's aspiration windows (zero-window non-PV nodes are exactly where NMP fires).

Prior-art: `docs/research/m5-null-move-pruning.md` (16-section survey of the design space). This ADR records the **commitments**; the research is the **evidence**.

ADR-0014 (eval) supplies the incremental `static_eval_white` field NMP reads as its gate; ADR-0016 (search structure) supplies the negamax body the integration extends; ADR-0018 (TT) supplies §3's depth-field semantic and §7's best-move-on-overwrite preservation rule that the NMP TT-store relies on.

Plan and test surface: `docs/plans/m5.a.md`.

## Decision

### 1. Reduction formula: `R = 2 + depth/6` (integer division)

Constants:

```rust
pub(crate) const NMP_BASE_R: u32 = 2;
pub(crate) const NMP_DEPTH_DIVISOR: u32 = 6;

pub(crate) fn null_move_reduction(depth: u32) -> u32 {
    NMP_BASE_R + depth / NMP_DEPTH_DIVISOR
}
```

- Smooth linear growth: `R=2` at depth 3–5, `R=3` at 6–11, `R=4` at 12–17, `R=5` at 18+.
- CPW workhorse default; literature SPRT-positive starting point. The CPW-Engine step-function alternative (`depth > 6 ? R=3 : R=2`) is a comparable starting point; the linear variant is M5.A's choice. Both are tunable post-landing via SPRT campaign.
- Eval-aware bonus (`R += min((static_eval - beta)/SCALE, CAP)`) **deferred** — adds a tuning parameter for marginal gain.

### 2. Minimum depth: `NMP_MIN_DEPTH = 3`

Below depth 3, `depth - 1 - R` ≤ 0 dispatches to qsearch, which defeats the cost/benefit calculation. CPW-Engine convention `depth > 2` (i.e., `depth >= 3`).

### 3. Gate set (seven conditions, all required)

Order matters — cheap predicates first; the `static_eval` read is **lazy** (only fires after `has_non_pawn_material` passes):

```rust
if ply > 0
    && allow_null
    && !is_pv
    && depth >= NMP_MIN_DEPTH
    && !in_check(pos)
{
    let stm = pos.side_to_move();
    if has_non_pawn_material(pos, stm) {
        let static_eval = /* sign-flipped pos.static_eval_white() for stm */;
        if static_eval >= beta { /* null-search */ }
    }
}
```

| Gate | Why |
|---|---|
| `ply > 0` | Structural-root guard. Defense-in-depth against a future PVS refactor that would change `is_pv`'s semantics. The root must always pick a move; NMP cutoff at root would return without a bestmove. |
| `allow_null` | Stacked-null prevention. `false` only in the NMP null-search recursive call (§5). |
| `!is_pv` | NMP cuts off; PV nodes need the full PV. Under M4.D's synthetic `is_pv` predicate (root + first child of PV parent); PVS at M5+ replaces with `beta - alpha == 1`. |
| `depth >= NMP_MIN_DEPTH` | See §2. |
| `!in_check(pos)` | A null move while in check leaves the king attacked; the resulting position is illegal. |
| `has_non_pawn_material(pos, stm)` | Zugzwang guard. See §4. |
| `static_eval >= beta` | NMP is only profitable when our position is at or above beta. Marginal +4 Elo per TalkChess t=69722; cheap and standard. |

### 4. Zugzwang policy: `has_non_pawn_material` (non-pawn piece count ≥ 1)

```rust
pub(crate) fn has_non_pawn_material(pos: &Position, side: Color) -> bool {
    let pieces = pos.pieces_colored(side, PieceKind::Knight)
        | pos.pieces_colored(side, PieceKind::Bishop)
        | pos.pieces_colored(side, PieceKind::Rook)
        | pos.pieces_colored(side, PieceKind::Queen);
    !pieces.is_empty()
}
```

- Zugzwang (pass = beneficial) is dominantly a K+P-ending phenomenon. `has_non_pawn_material` excludes K+P-only positions and is the consensus choice (Fruit/Toga/CPW-Engine/jdart/Arasan).
- Residual zugzwang risk in K+R+P, K+B+P endgames is **acknowledged and accepted**; rare in standard time-control games. SPRT regression in late-game positions would prompt a follow-up adding verification search.
- "Non-pawn piece count ≥ 2" (over-conservative) is rejected — no Elo benefit per literature, ~10% of middlegame positions excluded.

### 5. Stacked-null discipline: `allow_null: bool` parameter

`negamax` gains an `allow_null: bool` parameter threaded through alongside `is_pv`. All recursive calls in the move loop pass `allow_null = true`. The NMP null-search recursive call passes `allow_null = false` (the only place in the codebase). Top-level call from `Search::go` passes `true`.

`bool` parameter chosen over a state field (`ply_of_last_null`) for simplicity — proven across reference implementations; no documented Elo advantage to state-tracked allowances.

### 6. Mate-cap on cutoff

When `null_score >= beta`, the returned cutoff_score is mate-capped:

```rust
let cutoff_score = if null_score >= MATE_IN_MAX_PLY {
    beta
} else {
    null_score
};
```

NMP doesn't prove mate. A `null_score >= MATE_IN_MAX_PLY` means the opponent mates after we pass — a dangerous position, but not real proof of mate. Returning the mate-magnitude score would mis-rank the position in the parent search and propagate unsoundness via TT.

### 7. TT store on cutoff: `Bound::Lower` at the current depth, `best_move = 0`

```rust
tt.store(pos.zobrist(), TtData {
    score: score_to_tt(cutoff_score, ply as i32) as i16,
    depth: depth as u8,                  // current depth, NOT depth - 1 - R
    bound: TtBound::Lower,
    best_move: 0,
});
```

- **Depth = current depth** (not the reduced child depth). The cutoff proves a lower bound at this node — fail-soft contract — not at the reduced child. ADR-0018 §3 "depth at which entry was stored" semantic + §1 depth-preferred replacement.
- **Score = `cutoff_score` (mate-capped)**, not raw `null_score`. A mate-magnitude score in TT would later be returned by a probe at a different ply as a real mate proof; mate-capping closes that propagation channel.
- **`best_move = 0`**: NMP didn't pick a move at this node. ADR-0018 §7's preservation rule keeps any prior `best_move` for the key, so the existing best-move hint is not destroyed.

### 8. Verification search: deferred

Tabibi & Netanyahu 2002's reduced-depth verification with `allow_null = false` after a null-search fail-high catches some K+R+P / K+B+P zugzwang false positives.

**Deferred from M5.A v1.** `has_non_pawn_material` is the primary protection; verification doubles work on the uncertain branch for marginal gain (Hyatt/Crafty testing). If post-M5.A SPRT shows endgame-position regression, a follow-up adds verification at depth ≥ 8.

### 9. Out of scope (deferred to M5.B+ or later)

- **Eval-aware R bonus** (`R += min((static_eval - beta)/SCALE, CAP)`).
- **TT-driven threat-extraction post-NMP-fail** (CPW: extract refutation move from null-search TT entry to bias subsequent move ordering).
- **Adaptive R as `f(eval, depth)`**.
- **Bundling with M5.B/RFP**. Explicit decision: keep separate. Independent SPRT signals; clean Elo attribution per technique. Plan §1 codifies.

## Open SPRT-tunable parameters

Post-M5.A campaigns; not blocking M5.B:

1. **R formula constants**: linear `2 + depth/6` vs CPW-Engine step `depth > 6 ? 3 : 2`. Both are SPRT-positive starting points.
2. **Static-eval gate margin**: currently `static_eval >= beta`; could be `static_eval + margin >= beta` (looser gate, more NMP firings, ~+4 Elo per TalkChess t=69722).

## Consequences

**Positive:**

- Search-tree node count at default bench depth 7 expected to fall ~30–50% (literature; clawfish has strong M4 ordering, expect upper end).
- Composes additively with M4 ordering (TT-move-first → killers → history): NMP cuts off when the position is too good, before any move ordering is consulted.
- Sets up M5.B/RFP shared static-eval-read pattern; M5.B refactors the read out of the NMP block when its plan lands.

**Negative:**

- New abort site (the null-search recursive call into `negamax`); flows through M3.E's existing `should_abort` discipline unchanged. Tested via the post-NMP unmake-then-abort-check pattern matching the move-loop discipline.
- `negamax` signature gains a parameter (`allow_null: bool`). `negamax_for_test` and ~45 test call sites are mechanically updated; existing tests pass `true`.
- Mate-cap `>=` → `>` boundary mutation is structurally undetectable from any chess fixture (`MATE_IN_MAX_PLY = 29936` corresponds to mate-in-MAX_PLY which can't be searched). Documented as expected-survivor in `docs/mutants-backlog.md`.

**Migration paths:**

- **Verification search** (M5+ candidate): add a second reduced-depth recursive call after a null-search fail-high; only return cutoff if both succeed. Single block addition inside the existing NMP block.
- **Eval-aware R bonus** (M5+ candidate): extend `null_move_reduction` to `null_move_reduction(depth, static_eval, beta)`. One-function refactor; gates and TT-store discipline unchanged.
- **TT threat-extraction** (M5+ candidate): probe the null-search's TT entry on fail; record the threat target square; add an ordering boost. Distinct ADR; touches move ordering.

## Test surface (lands with M5.A)

- **`mov` module** (`src/mov.rs`): 13 unit tests on `make_null_move` / `unmake_null_move` (state changes, Zobrist XOR, round-trip including a proptest via `arb_position`).
- **`search` module** (`src/search.rs`): 18 unit tests (10 helper tests for `null_move_reduction` + `has_non_pawn_material`; 12 negamax-behavior tests using the sister-fixture pattern + the `#[cfg(test)] nmp_firings` counter for the stacked-null direct kill).
- **`tests/uci_integration.rs`**: 1 bench-determinism re-pin (E48 or next available).
