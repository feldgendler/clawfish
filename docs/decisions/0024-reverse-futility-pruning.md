# ADR-0024 — Reverse futility pruning: gate set, margin formula, return value, TT-store policy, ordering vs NMP

**Status:** Accepted (lands with M5.B).

## Context

M5.B adds **reverse futility pruning** (also called "static null-move pruning" or "beta pruning") to the negamax prologue at new step 8, immediately before M5.A's NMP block (step 9). At non-PV shallow non-check non-mate-beta interior nodes, if `static_eval - margin*depth >= beta`, RFP returns immediately with a fail-soft proved lower bound, without generating a single move.

RFP is cheaper than NMP (no sub-search, no null-move make/unmake) and fires in the same depth range as NMP on the overlap `d=3..6`. Ordering RFP before NMP means an RFP cutoff skips NMP's sub-search entirely on the overlap. This is a net node reduction.

Prior-art: `docs/research/m5-reverse-futility.md` (15-section survey of the design space). This ADR records the **commitments**; the research is the **evidence**.

ADR-0023 (NMP) is **unchanged** by M5.B: the NMP block at step 9 is preserved byte-identical to M5.A.

Plan and test surface: `docs/plans/m5.b.md`.

## Decision

### 1. Margin formula: linear, `margin = RFP_MARGIN_PER_DEPTH * depth`

Constants:

```rust
pub(crate) const RFP_MAX_DEPTH: u32 = 6;
pub(crate) const RFP_MARGIN_PER_DEPTH: i32 = 100;

pub(crate) fn reverse_futility_margin(depth: u32) -> i32 {
    RFP_MARGIN_PER_DEPTH * depth as i32
}
```

- At depth=1: 100 cp (≈ one pawn). At depth=6: 600 cp (≈ a rook).
- Conservative v1 starting value. CPW workhorse alternative is 150; a width-tune SPRT campaign post-landing compares 100 vs 120 vs 150.
- Linear vs per-depth table: linear is simpler and the CPW-Engine default. Per-depth table (e.g., 100/150/250 at d=1/2/3 — MadChess style) is a post-landing SPRT-tune candidate.

### 2. Maximum depth: `RFP_MAX_DEPTH = 6`

At depth=7+, `static_eval - margin*depth >= beta` is rarely true (the margin grows faster than any realistic eval surplus), and the tactical-blindness risk from a depth-7+ refutation grows. Stockfish DD historical: `depth < 7` (i.e., `depth <= 6`); CPW consensus. `RFP_MAX_DEPTH = 6` ± 1 is the primary SPRT-tunable post-landing.

### 3. Gate set (six conditions, all required)

```rust
if ply > 0
    && !is_pv
    && !in_check(pos)
    && depth <= RFP_MAX_DEPTH
    && beta.abs() < MATE_IN_MAX_PLY
{
    let static_eval = /* sign-flipped pos.static_eval_white() for stm */;
    let margin = reverse_futility_margin(depth);
    if static_eval - margin >= beta {
        return static_eval - margin;
    }
}
```

| Gate | Why |
|---|---|
| `ply > 0` | Structural-root guard. Defense-in-depth against a future PVS refactor that would change `is_pv`'s semantics. Parallels ADR-0023 §3's `ply > 0` first gate for the same reason. |
| `!is_pv` | RFP cuts off; PV nodes need the full PV. Same predicate as NMP. |
| `!in_check(pos)` | In check the position demands a response; RFP's static-eval assumption breaks down. Also: the `static_eval` read avoids a potentially misleading eval under check. |
| `depth <= RFP_MAX_DEPTH` | See §2. |
| `beta.abs() < MATE_IN_MAX_PLY` | Mate-magnitude beta implies a near-mate line has already been found; RFP's centipawn-based margin is meaningless in that context. The `static_eval` read is still inside the gate — lazy, not shared with NMP. |
| `static_eval - margin >= beta` | Core margin condition: we are `margin` cp above beta even after discounting. |

`static_eval` is read lazily inside the gate (sign-flipped by STM, identical pattern to ADR-0023 §3's NMP block). It is **not** shared with NMP's `static_eval` read — see §6.

### 4. Return value on RFP cutoff: `static_eval - margin` (fail-soft proved lower bound)

```rust
return static_eval - margin;
```

The position is at least `margin` cp above beta from static evaluation. Returning `static_eval - margin` is a fail-soft proved lower bound: even if the position is `margin` cp worse in reality, it still beats beta. Returning `beta` (fail-hard) or `static_eval` (full eval) would be less precise. Research §5 confirms `static_eval - margin` as the standard return for fail-soft engines.

### 5. TT store policy on RFP cutoff: NONE

RFP does not write a TT entry. The proof (`static_eval - margin >= beta`) is tied to the specific depth used to compute the margin: the same position at a different probe depth would require a different margin. Storing the result as a lower bound at `current_depth` would be incorrect at shallower probes (where `margin * shallow_depth < margin * current_depth`). Research §6/§7 consensus: RFP is a heuristic, not a search-quality bound. NMP's TT store (ADR-0023 §7) is preserved unchanged.

### 6. Lazy-dup `static_eval` reads: each block reads independently

RFP at step 8 and NMP at step 9 each read `static_eval` inside their own gate. The reads are duplicated, not shared via a hoisted eager read above both blocks.

**Rejected alternative**: hoist `static_eval` above both blocks, share one read. Rejected at plan-review pass 1 because:
- It would enlarge the read population on K+P endgame nodes (where NMP's `has_non_pawn_material` gate currently prevents the read; a shared hoist would fire regardless).
- The NMP block's behavior would silently change (the lazy-read-inside-`has_non_pawn_material` structure is load-bearing per ADR-0023 §3).
- The SPRT would blend RFP's signal with an NMP side-channel change, muddying attribution against `baseline/m5a-nmp`.

Keeping the reads independent preserves ADR-0023 §3 byte-identically and makes the M5.B SPRT attributable to RFP alone.

### 7. Order vs NMP: RFP fires before NMP

RFP (step 8) runs before NMP (step 9). Rationale: RFP is cheaper (pure static comparison, no sub-search). On the `d=3..6` depth overlap, an RFP cutoff skips NMP's null-search entirely. If RFP misses (margin condition fails), the search falls through to NMP's own gate, which has its own `static_eval` read and its own `static_eval >= beta` condition.

### 8. Out of scope (deferred)

- **Eval-aware margin** (`margin /= 2` when STM's eval is improving across plies). Research §13.
- **Per-depth margin table** (100/150/250 cp at d=1/2/3). Research §13.
- **Smoothed return `(static_eval + beta) / 2`**. Research §13 (Lynx/Ciekce recommendation).
- **Improving heuristic** (margin halves when `static_eval(ply) > static_eval(ply-2)`). Research §13.

## Open SPRT-tunable parameters

Post-M5.B campaigns:

1. **`RFP_MARGIN_PER_DEPTH`**: compare 100 / 120 / 150 cp.
2. **`RFP_MAX_DEPTH`**: compare 6 / 7 / 8.

## Consequences

**Positive:**

- Node count expected −5..−25% additional reduction over M5.A's NMP (research §11). Actual clawfish result: −37.2% (5,345,534 → 3,355,270 nodes at bench depth 8).
- Composes additively with M5.A's NMP: at `d=3..6`, RFP fires first (no sub-search), NMP fires on miss. Below `d=3` (under NMP_MIN_DEPTH), only RFP operates. Above `d=6` (RFP_MAX_DEPTH), only NMP operates.
- No signature change. No new UCI options. No changes to `src/mov.rs`, `src/position.rs`, `src/tt.rs`, or `src/eval.rs`.

**Negative:**

- Bench node count drops significantly (−37.2%). Any search-behavior test pinned to node counts must be re-pinned.
- RFP prunes branches without move generation. This introduces a new "tactical blindness" risk: positions where the static eval is misleadingly high are pruned without verifying. The `depth <= 6` ceiling and `beta.abs() < MATE_IN_MAX_PLY` gate reduce but do not eliminate this risk.

**Migration paths:**

- **Improving heuristic**: add `search_stack[ply].static_eval` field; compare with `search_stack[ply-2].static_eval`. One field addition + one gate condition.
- **Per-depth margin table**: replace `RFP_MARGIN_PER_DEPTH * depth` with a per-depth constant lookup. One constant array addition; `reverse_futility_margin` signature unchanged.

## Test surface (lands with M5.B)

- **`search` module** (`src/search.rs`): 5 helper tests for `reverse_futility_margin`; 12 negamax-behavior tests using the sister-fixture pattern + the `#[cfg(test)] rfp_firings` counter.
- **`tests/uci_integration.rs`**: E49 bench-determinism re-pin (pinned to 137174 nodes at depth=4; 3355270 at depth=8).
