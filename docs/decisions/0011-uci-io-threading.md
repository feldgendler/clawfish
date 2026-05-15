# 0011 — UCI I/O threading model

**Status:** Accepted, 2026-04-27 (binds at M2.C).

## Context

M2.C is the engine I/O loop. The UCI 2006 spec (`docs/reference/uci-protocol-2006.txt`) places hard real-time obligations on the engine that are visible only when the engine is mid-search:

- `isready` must be answered with `readyok` *immediately*, even while a `go` is in progress (lines 83–84).
- `stop` must terminate the search "as soon as possible" and be followed by `bestmove` (lines 173–175).
- `quit` must exit the program "as soon as possible" (line 182).
- `debug [on|off]` may arrive mid-search (lines 68–73); `setoption` only between searches per spec, but defensive engines tolerate it either way.
- The engine must silently ignore commands it doesn't recognize or shouldn't have received (lines 38–42).

The architectural choice is the threading shape. Full prior-art reasoning, candidate-architecture survey, and latency-budget analysis live in `docs/research/m2-uci-threading.md` (researched 2026-04-27). This ADR records the *commitment*; the research is the *justification*. The research considered four candidates and recommended the one chosen below.

This ADR also commits the project to a `Search` trait now (M2), so M2.D / M3 can plug in implementations without re-shaping the orchestrator.

## Decision

**Reader thread → mpsc → main-as-orchestrator + per-`go` search worker thread, with `Arc<AtomicBool>` cancellation shared between orchestrator and search.**

### Threads and channel topology

```
┌──────────┐  stdin  ┌─────────┐ Command  ┌──────────────┐
│  stdin   │────────▶│ Reader  │─────────▶│ Orchestrator │── stdout (replies, info, bestmove)
│ (kernel) │         │ thread  │  mpsc    │ (main thread)│
└──────────┘         └─────────┘          └──────┬───────┘
                                                 │ spawn / signal
                                                 ▼
                                          ┌─────────────┐
                                          │   Search    │  reads Position; polls flag + deadline;
                                          │   worker    │  writes info / bestmove via shared Mutex<Stdout>
                                          └─────────────┘
                                                ▲
                                                │ Arc<AtomicBool> (cancellation)
                                                └──── flipped by orchestrator on stop / time expiry
```

- **Reader thread.** Dedicated `std::thread::spawn`'d thread doing blocking `BufRead::lines()` on `stdin().lock()`. Each line is parsed via `parse_uci_line` (M2.B), then sent on `std::sync::mpsc::Sender<Command>`. EOF translates to a synthetic `Command::Quit`. The reader is never joined cleanly — on `quit`, the orchestrator calls `std::process::exit(0)` and the OS reaps it.
- **Orchestrator.** Runs on the main thread. Owns engine state (`Position`, options, RNG seed, `search_handle: Option<JoinHandle>`, `stop: Arc<AtomicBool>`). Receives `Command`s on the channel and dispatches to per-handler routines. Writes all protocol replies that aren't `info`/`bestmove`.
- **Search worker.** Spawned per `go`. Reads a cloned `Position` and a `SearchContext` carrying the cancellation flag, deadline, and limits. Writes `info` lines and the terminating `bestmove` to a shared `Mutex<Stdout>`. Exits cleanly on cancellation or natural completion.

### Cancellation primitive

- `Arc<AtomicBool>` with `Ordering::Relaxed`.
- Polled inside search; set by the orchestrator on `stop` and on deadline expiry; cleared by the orchestrator at the start of each `go`.
- Same primitive scales to lazy-SMP (M9) — every worker thread holds an `Arc` clone of the same flag.

### Inter-thread channel

- `std::sync::mpsc::Sender<Command>` / `Receiver<Command>`. No external dependencies (no `crossbeam`, no `tokio`).
- One direction only: reader → orchestrator. Cancellation back to search uses the atomic, not a channel.

### Stdin idiom

- Blocking `BufRead::lines()` on `stdin().lock()`; the lock is held for the lifetime of the reader thread.
- `\r` and stray trailing whitespace handled by `split_whitespace` inside `parse_uci_line` (M2.B), so the reader does not need a `trim_end`.
- EOF on stdin is treated as a synthetic `Command::Quit`. Covers cute-chess closing the engine's stdin after `quit`, terminal hangup in interactive use, and parent-process death before `quit`.

### Stdout idiom

- Shared `Arc<Mutex<Stdout>>` between orchestrator and search worker. Output lines (`bestmove`, `info`, `id`, `option`, `uciok`, `readyok`) are written under the lock with explicit `flush()` after every protocol-relevant line.
- `bestmove` is printed by the *search worker*, not the orchestrator, so it is the last line of every `go` (after any `info`) and appears even if the orchestrator is busy receiving the next command.
- A search that finds no legal move emits `bestmove 0000` (the spec's null-move sentinel, line 49 of the spec) — covers checkmate / stalemate positions and any future "stopped before any move was found" state.

### Quit discipline

- On `quit`: flip the cancellation flag, **join the search worker** (if one is in flight), then return from the run loop. `run_stdio` calls `std::process::exit(0)` after the run loop returns. The reader thread is **not** joined; the OS reaps it.
- The join is **bounded by the cancellation polling cadence**: the search worker's `should_abort` cycle (1 ms for the M2.C `Stub`, similar in M2.D's random-mover, configurable in M3+ search). Worst-case wait: one cadence + one stdout flush — microseconds, well under the 1 s budget.
- The reader thread cannot be joined cleanly because its blocking `read_line` is uncancellable per the [tokio Stdin docs](https://docs.rs/tokio/latest/tokio/io/struct.Stdin.html) caveat (which applies equally to `std::io::Stdin`). On `quit`, `process::exit(0)` terminates the process and the OS reaps the reader thread.
- **Joining the search worker is required for testability outside `run_stdio`.** A unit test calling `Engine::run` directly (without `run_stdio`'s `process::exit` safety net) would race the worker's `bestmove` write against `run`'s return; the join eliminates the race. Integration tests piping through real pipes face the same race when the OS closes the pipe before the worker flushes; the join eliminates that one too.
- A pathological future `Search` impl that ignores the cancellation flag could hang `quit`. That is a contract violation by the impl, not the plumbing — the trait's contract requires obeying `should_abort`. Bug fixes happen at the impl layer.
- `std::process::abort` is rejected — it skips destructors and on macOS produces a crash-report dialog and a non-zero exit code that confuses tournament tools.

(Revision history: M2.C's plan §9 v1 proposed a ~100 ms ceiling on the join; v2 dropped the join entirely; v3 — current — restores it as bounded by cancellation cadence, motivated by testability concerns surfaced during Phase 4 implementation.)

### Layering rule (anti-pattern firewall)

| Layer | May call | May not call |
|---|---|---|
| Reader thread | `BufRead::read_line`, `parse_uci_line`, `mpsc::Sender::send` | engine state, stdout |
| Orchestrator | engine state, command handlers, stdout under the mutex, `thread::spawn`, `flag.store`, `JoinHandle::join` | stdin |
| Search worker | `flag.load`, `Instant::now`, eval/movegen, stdout under the mutex | stdin, engine-mutable state outside what the context grants |

### `Search` trait — committed at M2

The trait is defined now (M2.C) so M2.D / M3 plug in without re-shaping the orchestrator. Sketch (full signatures land in the M2.C plan):

```rust
#[derive(Clone)]
pub struct SearchContext {
    pub stop:     Arc<AtomicBool>,
    pub deadline: Option<Instant>,
    pub start:    Instant,
    pub limits:   SearchLimits,
}

pub trait Search {
    fn go(
        &mut self,
        position: &Position,
        ctx: &SearchContext,
        info_sink: &dyn Fn(&str),
    ) -> SearchResult;
}
```

- `SearchContext::should_abort(nodes: u64) -> bool` is the standard polling site — checks the flag, the deadline, and the optional node cap. Search implementations call it every ~4096 nodes (per `docs/research/m2-uci-threading.md` §3 — prose-standard cadence; tunable later).
- `SearchResult` carries `bestmove`, `ponder`, `depth`, `score_cp`, `nodes`. M2.D's random-mover fills in `bestmove`; M3+ fills the rest.
- M2.C's stub implementation returns immediately with a placeholder; M2.D replaces it with the random-mover.

### Ordering and safety

- `Ordering::Relaxed` is sufficient for the cancellation flag. The flag does not synchronize *other* memory — search will exit cleanly and the orchestrator owns the `bestmove`-writing critical section. M4+ TT writes use the standard XOR-trick lockless pattern (out of scope here).

## Consequences

- **M2.C** owns: `Engine` struct, the reader thread + channel, the `Arc<AtomicBool>` and `Arc<Mutex<Stdout>>` plumbing, the `Search` trait + `SearchContext`, command handlers including a `go` handler that drives a stub `Search` impl. Latency budgets per `docs/research/m2-uci-threading.md` §4: `isready` < 1 ms, `stop` → `bestmove` < 10 ms steady state, `quit` → exit < 1 s. (The `quit` budget is met trivially because no join blocks the exit path — see "Quit discipline" above.)
- **M2.D** plugs the random-mover `Search` impl into the existing handler skeleton — no orchestrator changes.
- **M3** plugs alpha-beta into the same `Search` trait — no orchestrator changes.
- **M9 (lazy-SMP)** spawns multiple workers from inside a single `Search::go` invocation, all sharing the same `Arc<AtomicBool>`. Only the *main* worker writes to stdout. Orchestrator unchanged.
- **M10 (NNUE)** is per-position eval inside the search worker — zero interaction with threading.

## Variants considered and rejected

- **Single thread, search polls non-blocking stdin.** Rejected primarily because `isready` during search demands a separate-thread design to meet the spec's "answer immediately" rule. Secondary: cross-platform non-blocking stdin in Rust is a `#[cfg]` minefield that re-introduces dependencies. Doesn't compose with lazy-SMP.
- **Async runtime (`tokio`).** `tokio::io::Stdin` is itself a blocking read on a hidden thread — net architecture is identical to the chosen design but mediated by a runtime. CPU-bound search is the wrong fit for async. Adds compile-time, dependencies, and a futures mental model for zero structural benefit.
- **mpsc cancellation channel** (one channel orchestrator→search instead of an atomic). Heavier on the polling site (mutex on disconnect-detection path) and worse for SMP (one channel per worker). The atomic is canonical for this use case.
- **Drop-the-channel pattern** (matklad's worker-shutdown idiom). Elegant for single-shot workers; awkward when the same search runs repeatedly across many `go`s in a single game (the channel would be rebuilt each time). The flag is reused; the JoinHandle is per-`go`.
- **Dedicated timer thread** for deadline enforcement. Unnecessary — the search already polls on the same cadence as cancellation, so checking `Instant::now()` alongside the flag costs essentially nothing.

## How to apply

- M2.C constructs the `Engine`, reader thread, channel, atomic, and stdout mutex per the topology above. The `Search` trait is defined and a stub implementation is wired through the `go` handler so that the protocol is exercisable end-to-end. The M2.E integration test (one full game through `fastchess`) is what proves the threading model works in practice.
- M2.D replaces the stub `Search` with a random-mover.
- M3 replaces the random-mover with alpha-beta.
- This ADR is referenced from `docs/architecture.md` (UCI threading row, added in M2.C).
- The cancellation polling cadence (4096 nodes from prose) is a search-internal tuning knob; this ADR commits only to the *primitive* and *site*, not to a specific number. Profile-driven adjustments are routine future work.
