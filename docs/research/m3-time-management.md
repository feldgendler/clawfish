# M3 Time Management — Research Report

Scope: the time-management algorithm — the function `(remaining_time, increment, movestogo, ...) -> (soft_cap, hard_cap)` — and its interaction with iterative deepening. The threading primitive (deadline in `SearchContext`, polled with `Arc<AtomicBool>`) is established by `docs/research/m2-uci-threading.md` §8. This report covers the algorithm layer that sets the deadline value.

## Headline Calls

- **Soft cap:** `base / 20 + increment / 2` (CPW baseline; "very competitive with advanced schemes").
- **Hard cap:** `3 × soft_cap`, capped at `remaining_time - latency_margin`.
- **ID interaction:** start the next iteration only if `elapsed < soft_cap`. Abort mid-iteration only when `elapsed >= hard_cap`.
- **Latency margin:** subtract 50 ms before computing caps. Expose as `MoveOverhead` UCI option (default 50, range 0–5000).
- **Sudden death (no `movestogo`):** assume 20 remaining moves. `remaining / 20 + increment / 2`.
- **Pondering:** out of M3 scope. M5+.

## 1. UCI Clock-Parameter Taxonomy

### `wtime` / `btime`

- Remaining clock for White/Black in milliseconds; `i64`.
- Can go negative in time-trouble.
- Engine treats any value ≤ 0 as "emit bestmove from first completed iteration."

### `winc` / `binc`

- Fischer increment per move; `u64`.
- Added to allocation after dividing remaining time.

### `movestogo`

- Moves remaining until next time control. Spec: "only sent if x > 0."
- Absent: sudden death.
- Present: use literally as denominator.
- Off-by-one gotcha: `movestogo = 1` means "this move must be played within current budget."
- `movestogo = 0` is a spec violation; defensive fallback = treat as 1.

### `movetime`

- Fixed think time per move in ms; `i64`.
- Trumps clock-based allocation: `soft_cap = hard_cap = movetime - latency_margin`.

### `depth` / `nodes` / `mate`

- Non-time termination. Time management is bypassed: `deadline = Instant::MAX`.

### `infinite`

- Search until `stop`. `deadline = Instant::MAX`. Do not emit `bestmove` until `stop`.

## 2. Allocation-Algorithm Families

| Family | Formula | Tradeoff |
|---|---|---|
| Fixed-fraction | `remaining / N`, N = 20–40 | Simple; no movestogo awareness |
| Classical | `remaining / movestogo + increment * factor` | Correct for TC games |
| **CPW baseline** | `remaining / 20 + increment / 2` | Combines both; "very competitive" |
| Variable-fraction | Multiplier by phase | Squeezes Elo; complex |
| Soft + hard cap | Two thresholds from any of the above | Modern standard; enables ID interaction |
| Curve-based (Leela) | Bell curve over remaining game length | Complex; tuned for MCTS; n/a for alpha-beta |

**Recommendation for M3:** CPW baseline as soft cap; hard cap = `3 × soft_cap`. Variable-fraction and curve-based are M4+ refinements gated by SPRT.

## 3. Soft / Hard Cap Ratios

### Purpose of the split

- **Soft cap:** "don't start the next ID iteration if past this point."
- **Hard cap:** "abort the current iteration immediately."
- Hard-only: risk burning `hard - soft` on a discarded iteration.
- Soft-only: risk poor move from shallow iteration when behind on time.

### Ratio from prior art

- Mediocre Chess: implicit 2× via `elapsed * 2 > allocated`.
- CPW: no explicit ratio; "maximum time threshold" checked per node.
- Stockfish `Move Overhead` default 10 ms (local); Leela `--move-overhead` 100 ms.

### Practical recommendation for M3

| Parameter | Value | Rationale |
|---|---|---|
| Soft cap | `remaining / 20 + increment / 2` | CPW baseline |
| Hard cap | `min(3 × soft_cap, remaining - latency_margin)` | Prevents ID waste; capped to avoid forfeit |
| Latency margin | 50 ms default, configurable via `MoveOverhead` | Matches Leela ÷ 2; safer than Stockfish 10 ms |

## 4. Iterative-Deepening Interaction

### Soft cap check

- `elapsed >= soft_cap` before starting iteration N+1 → break ID loop, emit bestmove from iteration N.

### Mid-iteration hard abort

- `SearchContext::should_abort` poll (every ~2048 nodes) fires when `elapsed >= hard_cap`.
- Recursion stack unwinds with sentinel; ID loop sees abort and breaks.
- Last *completed* iteration's bestmove is used.

### Why abort mid-iteration is safe

- CPW Iterative Deepening: "the program always has the option to fall back to the move selected in the last iteration."

### EBF estimate

- M3 (alpha-beta, no TT, no LMR): EBF ~6–7 per ply.
- Next iteration: ~3–5× current.
- M3 simple check: `elapsed >= soft_cap`. Prediction-based check (will next iteration fit?) deferred.

### Deferrals (not M3 scope)

- **PV stability extension** (extend if score swings between iterations) — M4+ gated by SPRT.
- **Search instability extension** (extend if PV move changed) — M4+.

## 5. Sudden Death (`movestogo` Absent)

- `tc=10+0.1` is the standard fastchess SPRT TC.
- CPW: estimate "25–40 moves remaining."
- Mediocre Chess: divisor 40.
- Recommendation for M3: **N = 20** (conservative). New engine without TT may simplify faster than average.
- Formula: `soft_cap = remaining / 20 + increment / 2`.

### Special case: increment-only TC

- `wtime=0 winc=5000` → `soft_cap = increment / 2`. Correct: spend at most half the increment to stay afloat.

## 6. Latency Hedging

### Sources of latency

- UCI round-trip (stdout → GUI).
- GUI processing.
- Network in remote tournament play.
- macOS scheduling jitter.

### Recommended safety margin

- **50 ms** subtracted from any clock-derived cap.
- Expose as `MoveOverhead` UCI option (`type spin default 50 min 0 max 5000`).
- Implementation: `adjusted_remaining = max(0, remaining - move_overhead)`.

### Hard cap forfeit guard

- `hard_cap` must never exceed `remaining - latency_margin`.
- Formula: `hard_cap = min(3 × soft_cap, remaining - latency_margin)`.
- If `remaining - latency_margin <= 0`: `hard_cap = 1 ms`.

## 7. Edge Cases

| Case | Behavior |
|---|---|
| Very low time (`< 2 × latency_margin`) | `soft = hard = 1 ms`; emit from first completed iteration |
| Negative remaining time | Same as very low |
| `movetime` override | `soft = hard = max(1, movetime - latency_margin)` |
| `movestogo = 1` | Full remaining; capped to `remaining - latency_margin` |
| Integer overflow | Use saturating arithmetic |
| Non-time limits (`depth`/`nodes`/`mate`/`infinite`) | `soft = hard = Duration::MAX` |

### Race between deadline check and `bestmove` emission

- Already handled by M2.C/M2.D compute-before-check invariant.
- Time-management layer doesn't introduce new races.

### GUI sending `go` while previous worker alive

- Handled by `Engine::join_in_flight_worker` (M2.D).

## 8. Pondering

- Out of M3 scope. M5+.

## 9. Test Strategy

All tests mock the clock by injecting an `Instant`-returning closure or by parameterizing the function with elapsed/remaining as plain `Duration`. No sleep in tests.

### Unit tests for the allocation function

`compute_caps(remaining: i64, increment: u64, movestogo: Option<u32>, movetime: Option<i64>, latency: u64) -> (Duration, Duration)`

| Test name | Input | Expected soft | Expected hard |
|---|---|---|---|
| `sudden_death_10s_inc100` | `wtime=10000, winc=100` | `10000/20 + 100/2 - 50 = 500 ms` | `min(1500, 9950) = 1500 ms` |
| `classical_tc_600s_40moves` | `wtime=600000, movestogo=40` | `600000/40 - 50 = 14950 ms` | `min(44850, 599950) = 44850 ms` |
| `movetime_1000` | `movetime=1000` | `950 ms` | `950 ms` |
| `low_time_50ms` | `wtime=50, winc=0` | `1 ms` | `1 ms` |
| `negative_time` | `wtime=-200` | `1 ms` | `1 ms` |
| `no_time_limits` | `depth=5` | `Duration::MAX` | `Duration::MAX` |
| `infinite` | `infinite=true` | `Duration::MAX` | `Duration::MAX` |
| `nodes_limit` | `nodes=100000` | `Duration::MAX` | `Duration::MAX` |
| `movestogo_1` | `wtime=10000, movestogo=1` | `min(10000-50, ...)` | capped |
| `increment_only` | `wtime=0, winc=5000` | `2450 ms` | `7350 ms` |
| `increment_dominates` | `wtime=100, winc=5000` | clamped to `50 ms` | `50 ms` |

### ID-loop unit test

- Mock clock returns increasing timestamps.
- After soft cap elapsed: no new iteration starts.
- After hard cap elapsed mid-iteration: iteration aborts; previous iteration's bestmove returned.

## 10. Pitfalls

- **`movestogo` off-by-one** — includes the current move. `movestogo = 1` = last move before TC reset.
- **`movestogo = 0`** spec violation — defensive fallback to 1.
- **Integer overflow** — `remaining * 3` is safe in practice but use saturating arithmetic.
- **`MoveOverhead` not applied to `movetime`** — easy mistake; test the `movetime_1000` case explicitly.
- **Soft cap with increment > remaining** — `wtime=100 winc=5000` → naively 2505 ms; forfeit guard clamps to 50 ms.

## Citations

- [CPW — Time Management](https://www.chessprogramming.org/Time_Management)
- [CPW — Iterative Deepening](https://www.chessprogramming.org/Iterative_Deepening)
- [CPW — Branching Factor](https://www.chessprogramming.org/Branching_Factor)
- [Mediocre Chess: Time Management](http://mediocrechess.blogspot.com/2007/01/guide-time-management.html)
- [Mediocre Chess: Iterative Deepening](http://mediocrechess.blogspot.com/2007/01/guide-iterative-deepening.html)
- [Leela Chess Zero — Time Management blog](https://lczero.org/blog/2018/09/time-management/)
- [TalkChess — Time Management thread #76463](https://talkchess.com/viewtopic.php?t=76463)
- [Stockfish UCI docs](https://official-stockfish.github.io/docs/stockfish-wiki/UCI-&-Commands.html)
- [Komodo 12 docs](https://komodochess.com/store/pages.php?cmsid=14)
- UCI spec: `docs/reference/uci-protocol-2006.txt`
