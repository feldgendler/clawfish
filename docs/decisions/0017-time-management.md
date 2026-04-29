# ADR-0017 — Time management: soft/hard cap discipline + iterative deepening + MoveOverhead UCI option

**Status:** Accepted (lands with M3.E, 2026-04-29).

## Context

M3.E is the first phase that *manages time* — wraps M3.C/M3.D's negamax + qsearch in an iterative-deepening (ID) outer loop driven by soft and hard wallclock caps. The decisions bind once `Search::go` is restructured around the ID loop and the orchestrator wires a per-engine latency-hedge value through `compute_caps`.

Prior-art research and headline-call rationale: `docs/research/m3-time-management.md` (algorithm + §9 test table that the unit-test contract pins) and `docs/research/m3-search-basics.md` §6 (ID outer-loop discipline).

This ADR records the *commitment*; the research is the *justification*. ADR-0011 (UCI threading) supplies the cancellation primitive (`Arc<AtomicBool>`) that the hard cap is layered on top of; ADR-0016 (search structure) supplies the negamax body that the ID loop wraps.

## Decision

### 1. Allocation algorithm: CPW baseline + forfeit guard

Pure function `compute_caps(limits: &SearchLimits, side_to_move: Color, move_overhead: u64) -> TimeCaps` returns `(soft, hard)` durations. `Duration::MAX` is the "no cap" sentinel.

Formula tree (top-down, first match wins):

| Input shape | Result |
|---|---|
| Any of `infinite` / `ponder` / `depth` / `nodes` / `mate` is set | `(MAX, MAX)` — non-time limits trump the clock |
| `movetime` is set (and no non-time flag above) | `soft = hard = max(1, movetime - move_overhead)` |
| Side's clock (`wtime`/`btime`) is `None` | `(MAX, MAX)` — degenerate `go` with no time info |
| Clock present, `rem == 0 && inc == 0` (or negative `rem` clamped to 0) | `(1ms, 1ms)` — very-low-time floor |
| Clock present, `rem == 0 && inc > 0` (increment-only TC) | `soft = max(1, raw_soft - mo)`, `hard = 3 × soft`. **No forfeit clamp** (research §5: `inc/2` is correct because the increment refills after the move) |
| Clock present, `rem > 0` (general case) | `raw_soft = rem/denom + inc/2`; `soft_unclamped = saturating_sub(raw_soft, mo)`; `max_clamp = (rem - mo).max(1)`; `soft = soft_unclamped.min(max_clamp).max(1)`; `hard = (3 × soft).min(max_clamp).max(1)` |

`denom` (the divisor) per `movestogo`:

- `None` → 20 (sudden-death CPW baseline; research §5 conservative pick)
- `Some(0)` → 1 (UCI spec violation: defensively treat as "this is the last move")
- `Some(n)` → `n` (classical TC with explicit moves-to-go)

**On the `rem == 0` branches, `denom`/`movestogo` is intentionally not consulted.** The increment-only formula `inc/2` supersedes movestogo-aware allocation when there is no budget to divide.

### 2. Iterative-deepening outer loop

`Search::go` (per ADR-0016 §1 trait signature, unchanged) now runs:

```
for depth in 1..=max_depth_from_limits(&ctx.limits) {
    self.aborted = false; self.root_score = None; pv.clear_all();
    let returned = self.negamax(&mut pos_clone, depth, 0, -INF, INF, ctx);
    if self.aborted { break; }                                 // mid-iteration abort
    last_complete = Some((depth, bestmove, full_pv, returned));
    self.prior_root_move = bestmove;                            // ordering hint for next iter
    info_sink(&format!("info depth {depth} score … nodes … pv …"));
    if depth >= max_depth { break; }
    if ctx.stop.load(Relaxed) { break; }                        // load-bearing: see below
    if let Some(soft) = ctx.soft_deadline && now >= soft { break; }
}
```

**Per-iteration reset semantics:**

- `aborted`, `root_score`, and `pv` cleared per iteration.
- `nodes` is **NOT** reset between iterations — accumulates (cumulative `info nodes N` semantic + `go nodes <N>` budget applied across the whole `go`).
- `prior_root_move` is **NOT** reset between iterations — it's set at the END of each completed iteration so the next iteration sees the prior best as the ordering hint.

**Top-of-go reset semantics:**

- `prior_root_move = None` at the start of every `go` (so iteration 1 has no hint).
- `history`, `nodes`, `aborted`, `root_score`, `pv` all cleared.

**Mid-iteration abort discipline:**

- The hard cap fires via `should_abort` at the 4096-node cadence inside `negamax`/`qsearch` (ADR-0016 §7). The aborted iteration's partial PV is discarded; `last_complete` from the prior iteration becomes the reported result.
- The inter-iteration `stop` check is **load-bearing**: per-iteration `aborted` reset means a `stop` flipped between iterations would otherwise be missed until the next 4096-cadence poll. The explicit `if ctx.stop.load(Relaxed) { break; }` between iterations closes that gap.
- Iteration 1 is unconditionally allowed to start (the soft check is at end-of-iteration). This guarantees "always have a move to play" — even when the soft cap is already past at `go`-time, iteration 1 runs at least once.

**Final-result fallback:**

- If `last_complete` is `None` (iteration 1 itself aborted before any root move improved alpha), fall back to whatever `pv[0][0]` and `root_score` hold from the in-progress iteration. If still empty, emit `bestmove 0000` per ADR-0011's null-move sentinel.

### 3. `max_depth_from_limits` resolution

| Input | Result |
|---|---|
| `depth = Some(d)` | `d.min(MAX_PLY - 1) = d.min(63)` |
| Any other time/resource limit (`infinite` / `ponder` / `movetime` / `nodes` / `mate` / `wtime` / `btime`) | `MAX_PLY - 1 = 63` |
| Bare `go` (no fields set) | `4` (legacy fallback for backward-compat with M2.D / M3.A / M3.D's bare-`go` semantics) |

The 63 cap is the PV-table bound — `MAX_PLY = 64` per ADR-0016 §5; ply indexing is 0-based, so depth-63 leaves can reach ply 63 which is the last valid index.

### 4. Root prior-PV ordering hint

`AlphaBetaMover` gains a `prior_root_move: Option<Move>` field. Set after each completed iteration; consumed by `negamax` at `ply == 0` only:

```
moves_vec.sort_by_cached_key(|&m| -mvv_lva_score(m, pos));   // existing MVV-LVA
if ply == 0 && let Some(pm) = self.prior_root_move
   && let Some(idx) = moves_vec.iter().position(|m| *m == pm)
   && idx != 0
{
    let prior = moves_vec.remove(idx);
    moves_vec.insert(0, prior);
}
```

`remove(idx) + insert(0)` (not `swap`) preserves the relative MVV-LVA order of the remaining moves. The `idx != 0` guard skips the no-op case (prior already first). The `position()` lookup gracefully handles the case where the prior move is not in the current movelist (e.g., `searchmoves` filter removed it, or position changed via `position startpos` since the prior `go`).

### 5. `MoveOverhead` UCI option

**Storage**: `Engine::move_overhead: u64` (ms), default 50.

**`handle_uci`** emits between `Random_Seed` and `uciok`:

```
option name MoveOverhead type spin default 50 min 0 max 5000
```

**`handle_setoption`** parses `MoveOverhead` (case-insensitive) as `u64`, accepts `[0, 5000]`, rejects out-of-range / unparseable / missing-value. Mirrors the `Random_Seed` arm's discipline (silent on accept; `info_string_debug` on reject; rejected values leave the existing `move_overhead` untouched).

**Default value 50 ms** matches research §6's recommendation — safer than Stockfish's 10ms default for typical macOS scheduling jitter, half of Leela's 100ms.

**Max 5000 ms** is generous for fastchess CI runners. Future revisits possible if remote tournament play introduces network-latency spikes warranting a higher ceiling.

### 6. `SearchContext` extension

One additive field:

```rust
pub struct SearchContext {
    /* existing: stop, deadline, start, limits, history */
    pub soft_deadline: Option<Instant>,
}
```

`should_abort` semantics unchanged — it remains the **hard-cap path** (polled at the 4096-node cadence by `negamax`/`qsearch`). `soft_deadline` is polled by the **ID outer loop** between iterations. The two paths do not interact (soft check is post-iteration; hard check is mid-iteration via cancellation poll).

### 7. Wait-loop interaction

`Search::go`'s post-ID-loop wait loop (`infinite || movetime.is_some() || ponder` → spin until `should_abort`) is unchanged from M3.D. Under `go movetime 1000` with `MoveOverhead = 50`, caps = `(950, 950)`. ID exits at the soft cap (~950 ms wall after `go`-start). The post-loop wait then spins until the hard cap (also 950 ms) elapses — a no-op on the same wallclock instant. Correct UCI behavior: the engine does not return early before its budget expires.

Under `go infinite`, caps = `(MAX, MAX)`, ID may iterate to depth 63 on trivial positions and fall into the wait loop. The wait loop spins until `stop` arrives.

## Consequences

- `Search` trait signature **unchanged**.
- `SearchContext` gains one field (additive, no consumer breakage).
- `AlphaBetaMover` gains one field (`prior_root_move`).
- `Engine` gains one field + one constant + ~25 LOC of UCI option plumbing.
- ID overhead: ~10–20% extra nodes from depths 1..N-1 versus single-pass depth-N, but better root-move ordering at depth N (via prior-PV hint) typically nets a small node *reduction* at the same final depth on tactical positions.
- `info` line emission shifts from once-per-`go` to once-per-completed-iteration. Existing tests that asserted exactly one info line were updated.
- `bench` UCI command (M3.F): the deterministic per-iteration cadence makes `go bench` reliable — same position + same depth/node cap → same node count.

## Alternatives rejected

| Alternative | Why rejected |
|---|---|
| Variable-fraction allocation (multiplier by phase) | Squeezes Elo at significant tuning complexity; M4+ refinement gated by SPRT |
| Bell-curve (Leela-style) over remaining moves | Tuned for MCTS; n/a for alpha-beta |
| Predictive "will iteration N+1 fit?" check | Needs EBF estimation (M3 EBF data is sparse); simple `elapsed >= soft` rule is the M3 pick |
| Aspiration windows | Without a TT, fail-high re-search re-does all work; instability risk per CPW; M4+ |
| PVS / NegaScout | Saves ~10% only when the first move is consistently best; without TT-move it's marginal; M4 |
| `ctx.deadline` repurposed as soft (no second field) | The two checks need different polling sites (mid-iteration vs between-iterations); separating them is clearer |
| `prior_root_move` as part of `SearchContext` | Owned by `AlphaBetaMover` because it's persistent state across iterations within one `go`, not a per-go input |
| `MoveOverhead` default = 10 (Stockfish) | macOS scheduling jitter + UCI round-trip routinely exceeds 10ms; 50ms is the safer hedge |
| Reset `nodes` between iterations | Breaks `go nodes <N>` budget contract (cap should apply across whole `go`, not per-iter) |

## References

- `docs/research/m3-time-management.md` — primary research (§9 test table is the contract).
- `docs/research/m3-search-basics.md` §6 — iterative-deepening prior art.
- `docs/decisions/0011-uci-io-threading.md` — cancellation primitive.
- `docs/decisions/0016-search-structure.md` — negamax body that ID wraps.
- `docs/plans/m3.e.md` — implementation plan.
- `docs/architecture.md` — "Search v1 (production: alpha-beta)" subsection.
