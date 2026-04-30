# ADR-0019 — History heuristic: indexing, bonus/malus formula, saturation, reset boundary

**Status:** Accepted (lands with M4.C).

## Context

M4.C is the third M4 phase. It adds a path-independent **history heuristic** for ordering quiet moves: a butterfly-indexed table accumulating `+depth*depth` for quiet moves that produce a beta cutoff and `-depth*depth` for quiets searched-but-failed at the same node. Layered into negamax move ordering as the comparator for non-capture, non-promotion, non-EP moves; the history score sorts the quiet pool descending. Plumbed into `Engine::reset_for_new_game()` for the `ucinewgame` + bench-position lifecycle the M4.A ADR-0018 §14 inventory codified.

Prior-art: `docs/research/m4-history-heuristic.md` (12-section survey of the design space). This ADR records the *commitments*; the research is the *evidence*.

ADR-0011 (UCI threading) supplies the single-mutator invariant; ADR-0016 (search structure) supplies the negamax body the integration extends; ADR-0018 (TT) supplies the per-game reset boundary M4.C joins.

Plan and test surface: `docs/plans/m4.c.md`.

**Parallel-branch development.** M4.B (killer moves) is being developed in parallel. M4.C does NOT contain killer-slot code; killer integration into the move-ordering pipeline lands at merge time. ADR-0019 codifies the M4.C-side commitments only; the merge plan is documented in `docs/plans/m4.c.md` §7.

## Decision

### 1. Indexing scheme: `[side][from][to]` butterfly

```rust
pub(crate) struct HistoryTable {
    entries: [[[i16; 64]; 64]; 2],   // [side][from][to], 16 KiB
}
```

- 2 (Color) × 64 (from-square) × 64 (to-square) = 8,192 entries × `i16` = **16 KiB**.
- Side dimension required: White's e2-e4 and Black's e7-e5 are different moves; conflating them silently degrades ordering quality on alternating moves of the same shape.
- Per CPW canonical formula `history[side][from][to]`. Alternative `[side][piece][to]` (12 × 64 = 768 entries × i16 = 1.5 KiB) saves 90% memory but conflates origin squares; rejected for M4.C and revisitable if cache pressure manifests.
- The 16 KiB table fits in Apple Silicon M4's 192 KiB L1d.
- **Density ≈ 44%**: only ~1,792 of 4,096 `[from][to]` pairs correspond to legal moves (Butterfly Boards property). The unused entries cost no runtime — never accessed — but consume cache lines. Acceptable.

### 2. Bonus formula: `+= depth*depth` with explicit clamp

On quiet-move beta cutoff at depth `d`, the cutter's `(side, from, to)` entry receives `+d²`. Each prior quiet in `quiets_searched` at the same node receives `-d²`.

```rust
pub fn update(&mut self, side: Color, from: Square, to: Square, bonus: i32) {
    let entry = &mut self.entries[side as usize][from.index() as usize][to.index() as usize];
    let new_value = (*entry as i32 + bonus).clamp(-MAX_HISTORY as i32, MAX_HISTORY as i32);
    *entry = new_value as i16;
}
```

`bonus` is `i32` so callers can pass `depth * depth` (max `63² = 3969`) or `-d²` without intermediate clamping. The clamp lands AFTER the widened add, preventing `i16` overflow.

**Why `depth*depth` over alternatives:**
- `+= 1`: doesn't weight by depth; loses the "cutoffs at higher depth are more credible" signal.
- `+= depth`: linear; under-weights deep cutoffs.
- `+= 1 << depth`: exponential; overflows `i16` at depth 15.
- `+= depth*depth`: quadratic; the de-facto standard cited in CPW, Mediocre Chess, Rustic, MadChess. Bounded and predictable.

The history-gravity formula (`+= clamped_bonus - score * |clamped_bonus| / MAX_HISTORY`) is the modern Stockfish-style refinement; **deferred to M5**. M4.C uses the simpler explicit-clamp variant.

### 3. History malus: bidirectional update

When a quiet causes a beta cutoff, **all** quiet moves that have been recursed before the cutter at the same node ("priors", tracked in a stack-local `MoveList` accumulator named `quiets_searched`) receive a `-depth*depth` penalty.

The malus is **mandatory**, not optional. Per research §3 (CPW + MadChess Build 084 +28 Elo for malus alone): "the unidirectional bonus-only history accumulates positives indefinitely; without the matching malus, all quiet moves at frequently-visited positions trend toward MAX_HISTORY and ordering quality degrades."

The malus magnitude equals the bonus magnitude.

**Killers (post-merge with M4.B) are NOT excluded from `quiets_searched`.** Per research §10's "Penalizing killers" row: a killer that fails to cut at this node and a later quiet does should receive malus — it was tried-and-failed. The merge plan documents this explicitly to prevent a future maintainer from re-introducing an unfounded killer-skip.

### 4. Saturation: clamp at `±MAX_HISTORY = 16384`

```rust
pub(crate) const MAX_HISTORY: i16 = 16384;
```

- `16384 < i16::MAX = 32767` leaves headroom for transient overflow during the i32 intermediate before clamping.
- Bonus of `63² = 3969` per cutoff at maximum depth saturates after ~5 cutoffs of the same `(side, from, to)`. Acceptable; stable thereafter.
- **No periodic halve-on-threshold aging.** Saturation is the only bounding mechanism. If post-merge SPRT reveals saturation drift (history values stuck at MAX_HISTORY everywhere; ordering quality degrades over a long game), follow-up unit `M4-history-aging` adds halve-on-threshold or migrates to the gravity formula.

### 5. Reset boundary: `Engine::reset_for_new_game()` (additive)

History table clears on:

- `ucinewgame` (per existing M4.A boundary).
- Per-position inside `handle_bench` (per existing M4.A boundary; preserves bench determinism).

The mechanism is `Search::reset()` body extension (additive, NOT replacement):
```rust
fn reset(&mut self) {
    self.history.clear();           // M3.B carry-forward: game-history Zobrist Vec.
    self.history_table.clear();     // M4.C: butterfly history table.
}
```

`Engine::reset_for_new_game()` calls `Search::reset()` per the M4.A discipline. No engine-side change required.

**`Hash` UCI option does NOT cover the history table.** The 16 KiB table is below the threshold cited in TalkChess t=67878 ("kill-move tables, history tables, static lookup tables are negligible and need not be counted" against `Hash`). M4.A ADR-0018 §4 codified `Hash` as the TT-only knob.

### 6. Inter-iteration discipline: persist across ID iterations within `go`

History accumulates across all iterative-deepening iterations within a single `go`. The depth-D iteration's history feeds the depth-(D+1) iteration's ordering. **Do NOT clear between iterations** — that destroys the cross-iteration carry-over which is the primary value of history.

History also persists across `go` invocations within a game. Resets only on the M4.A boundary (`ucinewgame` + bench-position).

### 7. Pipeline placement: history scores non-killer quiets

Post-M4.C-rebase, the comparator is the M4.B-merged `negamax_move_order_score` extended with a history branch:

```rust
fn negamax_move_order_score(
    mv: Move,
    pos: &Position,
    killer0: Move,
    killer1: Move,
    history_table: &HistoryTable,
) -> i32 {
    if !is_quiet(mv) {
        return mvv_lva_score(mv, pos) + CAPTURE_OFFSET;
    }
    if mv == killer0 {
        KILLER0_SCORE
    } else if mv == killer1 {
        KILLER1_SCORE
    } else {
        history_table.score(pos.side_to_move(), mv.from_square(), mv.to_square()) as i32
    }
}
```

**Score-tier discipline** (post-M4.B-merge revision):

```text
captures (mvv_lva + CAPTURE_OFFSET) > KILLER0_SCORE > KILLER1_SCORE > MAX_HISTORY > -MAX_HISTORY
```

| Constant | Value | Role |
|---|---|---|
| `CAPTURE_OFFSET` | `1_000_000` | Added to every non-quiet's `mvv_lva_score` to lift captures above all killers and all quiets, regardless of how high the small-victim raw scores reach. |
| `KILLER0_SCORE` | `100_001` | Most-recent quiet beta-cutoff at this ply. Above `KILLER1_SCORE` and `MAX_HISTORY`. |
| `KILLER1_SCORE` | `100_000` | Prior quiet beta-cutoff at this ply. Above `MAX_HISTORY`. |
| `MAX_HISTORY` | `16384` | History-score cap; range `[-MAX_HISTORY, MAX_HISTORY]`. Literature standard (CPW + MadChess + general practice). |

**Background on the constants.** M4.B originally landed `KILLER0_SCORE = 200, KILLER1_SCORE = 100` — chosen to fit between 0 and the smallest MVV-LVA capture score (QxP=287). M4.B's research §6 noted these were arbitrary and could be tuned in M4.C. M4.C's research recommended `MAX_HISTORY = 16384` (literature standard). To preserve the captures > killers > history-quiets hierarchy with both constants at their literature-recommended values, the M4.C rebase bumped killer constants above the history range, and lifted captures above killers via `CAPTURE_OFFSET`. Relative orderings within each tier (e.g., between captures, between promotions and captures via promo-piece-value) are unchanged from M4.B's discipline.

Pinned by:
- Compile-time `_SCORE_TIER_INVARIANTS` const-assert at the top of `src/search.rs`'s M4.B+M4.C section: `CAPTURE_OFFSET > KILLER0_SCORE > KILLER1_SCORE > MAX_HISTORY > -MAX_HISTORY`.
- Runtime test `killer_scores_strictly_below_capture_path_output_and_above_max_history` (S23, post-rebase): `KILLER0_SCORE < mvv_lva_score(QxP) + CAPTURE_OFFSET` against the actual MVV-LVA values.
- HS12 (post-rebase): asserts `s_promo > s_cap > s_killer > s_history_quiet` end-to-end through `negamax_move_order_score` with synthetic killer + history pre-population.

TT-move-first bubble (M4.A) runs AFTER the comparator sort, unchanged from M4.A. `negamax_move_order_score` is the single comparator in `negamax`'s `sort_by_cached_key` step.

**Capture history (separate `[from][to][captured-piece]` table) is M5+ scope** (research §11). Applying quiet-move history to captures naively is harmful (research cites 20–50 Elo loss in reported tests).

### 8. TT-hit discipline: do NOT update history on TT early-return

History is updated **only inside the move loop, after a real recursion completes with `score >= beta`**. A TT-cutoff early-return at the M4.A negamax prologue step 7 has no "refuting move" to credit at this node — the cached score is a memory-level shortcut. Updating history on TT-hit conflates two semantically different events.

Pinned by HS3 (`negamax_does_not_update_history_on_tt_cutoff`).

### 9. Datatype: `i16` per entry, signed

Negative history values are intentional (malus-driven). Unsigned types are the canonical history-implementation gotcha — `u16` would wrap negatives to MAX_U16 and visibly invert the signal. ADR commits to `i16`.

`i32` is a safe alternative at 4× memory (64 KiB). Given the i16 table fits in L1d, the i16 choice stands.

### 10. Sentinel exclusion

`Move::default()` (a1→a1 Quiet) is the no-move sentinel used in `PvTable`. Movegen never produces it, but a buggy code path injecting it into `quiets_searched` would corrupt `[side][a1][a1]`. The negamax push site has:

```rust
debug_assert!(
    mv.from_square() != mv.to_square(),
    "Move::default() sentinel must never enter quiets_searched"
);
```

The assert fires in debug builds only; release trusts the movegen-source guarantee.

## Consequences

**Positive:**

- Search-tree size at default bench depth 7 falls from M4.A's 39,964,046 nodes to **19,259,623 nodes (-51.8%)**. The reduction is substantially larger than the +15–50 Elo cited in the research; the empirical Elo gain at SPRT-time will likely sit in that range nonetheless (node-count reduction does not translate 1-for-1 to Elo).
- NPS within 1% of M4.A — no per-node regression from the bonus/malus dispatch + per-frame `MoveList` accumulator.
- Move ordering composes with M4.B at merge time without code changes in the M4.C-side: killers slot between captures and history-sorted quiets at merge, killers participate in `quiets_searched` like any other quiet (per research §10).

**Negative:**

- `quiets_searched` consumes 32 KiB across the worker thread's call stack at MAX_PLY=64 (1.5% of the default 2 MiB stack). Acknowledged.
- `sort_by_cached_key` heap-allocates the i32 cache per node (~120 bytes typical, up to 872 bytes at max-legal). M4.A inheritance, not introduced by M4.C.
- The simple `+= depth*depth` formula saturates entries quickly at high depth. Aging is deferred. If SPRT post-merge shows saturation drift, the follow-up unit lands gravity formula or halve-on-threshold.

**Migration paths:**

- **Gravity formula** (M5 candidate): replace the `update` body with the multiplicative-damper formula. The function signature is unchanged; the integration points don't move. Bonus-magnitude tuning (e.g., `300 * depth - 250`) is also a one-function swap.
- **Capture history** (M5+ candidate): add a separate `CaptureHistoryTable` indexed by `[side][piece][to][captured-piece]` (or similar); update on capture-cutoff. Sums into `move_ordering_score`'s capture branch alongside MVV-LVA. ADR follows the M4.C structure.
- **Continuation history** (M5+ candidate): 1-ply CHS-1 indexed by `[prev_piece][prev_to][curr_piece][curr_to]`. Significantly larger memory; requires plumbing the previous move into `negamax` recursion. Distinct ADR.

## Test surface (lands with M4.C)

- **`history` module** (`src/history.rs`): 12 unit tests (H1–H12) pinning `new`/`clear`/`score`/`update` semantics, including positive/negative clamp boundaries, side-independence, square-independence, layout (16 KiB), and `Send + Sync` discipline.
- **`mov` module** (`src/mov.rs`): 1 test (`move_is_quiet_classifies_all_flags`) pinning the 14-flag classification.
- **`search` module** (`src/search.rs`): 16 tests (HS1–HS12 + HS3b/HS3c/HS4b/HS8b) pinning the bonus/malus dispatch, the `quiets_searched` capture-exclusion, the abort/TT-cutoff/rep/MDP early-exit history-untouched discipline, the side-to-move invariant at root and non-root, the saturation clamp via integration, and the `move_ordering_score` capture-above-quiet + non-capture-promo-above-capture orderings.
- **`engine` module** (`src/engine.rs`): 1 test (`ucinewgame_clears_history_table`) pinning the reset boundary.
- **integration** (`tests/uci_integration.rs`): 1 test (`bench_signature_deterministic_across_two_runs_with_history`) pinning bench determinism with history in play.

## Cross-references

- Research: [`docs/research/m4-history-heuristic.md`](../research/m4-history-heuristic.md)
- Plan: [`docs/plans/m4.c.md`](../plans/m4.c.md)
- Retrospective: [`docs/milestones/m4.c.md`](../milestones/m4.c.md)
- Predecessor ADRs: [`0016-search-structure.md`](0016-search-structure.md), [`0018-transposition-table.md`](0018-transposition-table.md)
- M4.B parallel branch (separate ADR forthcoming at M4.B landing).
