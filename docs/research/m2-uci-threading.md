# M2 prior-art research — UCI threading and stdin concurrency

Research pass for M2 (UCI random-mover). Scope: how a UCI chess engine should structure threads, channels, and cancellation so that stdin stays responsive while the engine is searching, and so the model survives the layering of alpha-beta (M3), iterative deepening + time management (M4–M5), lazy-SMP (M11), and NNUE (M12) without rewrite.

Sources: the UCI 2006 specification (vendored at `docs/reference/uci-protocol-2006.txt`), Chess Programming Wiki, chessprogramming.net, TalkChess threads, dogeystamp's chess-engine series, matklad on Rust worker shutdown, the Rust standard library and tokio docs, and Cute Chess issue/manpage prose. No engine source code was read; cutechess source *was* read (it's a tournament runner, not an engine — outside the prohibition in `decisions/0003`).

---

## 1. What UCI actually demands

The protocol is unambiguous on responsiveness. The 2006 spec (`docs/reference/uci-protocol-2006.txt`) states: *"the engine must always be able to process input from stdin, even while thinking"* (line 15). Three commands have hard real-time obligations during a search:

- **`isready`** *"can be sent also when the engine is calculating in which case the engine should also immediately answer with `readyok` without stopping the search"* (lines 83–84). Used by GUIs as a liveness ping.
- **`stop`** *"stop calculating as soon as possible, don't forget the `bestmove` ... when finishing the search"* (lines 173–175). The stop-then-bestmove pair acts as confirmation that the engine has acknowledged the stop ([UCI Protocol – wbec-ridderkerk](https://www.wbec-ridderkerk.nl/html/UCIProtocol.html)).
- **`quit`** *"quit the program as soon as possible"* (line 182).

Two more arrive only while idle by spec, but defensive engines handle them either way:

- **`setoption`** *"will only be sent when the engine is waiting"* (line 89).
- **`debug`** *"can be sent any time, also when the engine is thinking"* (line 73). So `debug` is mid-search-legal even though `setoption` isn't.

Crucially, the spec also says *"if the engine receives a command which is not supposed to come, for example `stop` when the engine is not calculating, it should also just ignore it"* (lines 41–42). The state machine must therefore tolerate spurious commands silently — no error replies, no aborts.

The Chess Programming Wiki acknowledges the architectural tax bluntly: *"It is hard for chess engines to process input/output without an extra thread for that duty"* ([CPW UCI](https://www.chessprogramming.org/UCI)). That sentence is the entire reason this research note exists.

**Recommendation: treat the UCI requirements as three concrete latency budgets — `isready` answer, `stop` → `bestmove` time, and `quit` → process exit time — and design the threading model around hitting all three.** Any model that can't service `isready` while a 30-second `go infinite` is running is non-compliant by construction.

---

## 2. The four candidate threading models

### 2.1 Single thread, search polls non-blocking stdin

The classical pre-thread approach. Search recurses; every N nodes (1024–4096 are the typical numbers in prose, e.g. [implementing UCI stop command](http://talkchess.com/forum3/viewtopic.php?t=46368)) the search calls something like `peek_stdin_nonblocking()` and bails if it sees `stop` / `quit`. On POSIX this is `select(stdin, timeout=0)` (informed intuition based on the AlvaroBegue suggestion in that same thread); on Windows it's `PeekNamedPipe` (hgm in the same thread).

Pros: zero thread-coordination surface. No atomics, no channels, no panics-across-threads. Trivial to reason about, trivial to debug single-stepping.

Cons:

- Polling latency is bounded by node-throughput. At our 33 Mnps starting-D4 plain perft figure (M1.G headline), 4096 nodes is ~120 µs — fine. But search will be slower per node than perft (eval, move ordering, TT probes), and 4096 nodes in a *quiescence search* leaf can be much faster. Still well under 1 ms in steady state. Judgment call: latency is fine for M2–M3.
- Cross-platform stdin polling is a `#[cfg]` minefield in Rust. There is no portable non-blocking read in `std`. We'd be reaching for `mio`, `nix::poll`, or equivalent — bringing back the dependency we'd be avoiding by skipping a thread.
- The polling cadence interacts with eval and move ordering — you can't easily check time mid-makemove. So the worst-case latency is "one full node," and a complex node can be many microseconds.
- Doesn't compose with lazy-SMP. With N worker threads each searching, *who* polls stdin? Either the main thread does (and we're back to the multi-thread model anyway), or every worker checks (N× more syscalls and contention).

### 2.2 Dedicated reader thread + dedicated search thread + control channel

The pattern dogeystamp describes ([Chess engine, pt. 1](https://www.dogeystamp.com/chess1/)): *"two threads, one input thread, and one engine thread. The input thread processes the UCI commands, and the engine thread thinks."* Communication is one-way at runtime: reader → orchestrator/search via a channel; cancellation back to search via a shared atomic flag.

The reader thread does a blocking `BufRead::lines()` loop on `stdin().lock()`. Each line is parsed into a `Command` enum and pushed onto an `mpsc` channel. The main/orchestrator thread receives commands; for `go`, it spawns or signals the search worker; for `stop`, it flips an `Arc<AtomicBool>`; for `isready`, it just prints `readyok` directly.

Pros:

- Reader is *always* responsive. Even if the search thread is heads-down in recursion, the reader has parsed the next command into the channel.
- Stop latency = (time from `AtomicBool::store` to next `load` in search) + (time to unwind recursion) + (time to print `bestmove`). Search polls the flag every M nodes (4096 is standard); on Apple Silicon a relaxed load is one or two cycles ([std::sync::atomic::AtomicBool](https://doc.rust-lang.org/std/sync/atomic/struct.AtomicBool.html)). End-to-end well under 1 ms in steady state. Judgment call backed by the AtomicBool docs: this is the canonical pattern for cross-thread cancellation in Rust.
- Maps directly onto lazy-SMP. *"set abort flag for each helper and wait for each to stop searching"* — that's exactly what the CPW Lazy SMP pseudocode says ([CPW Lazy SMP](https://www.chessprogramming.org/Lazy_SMP)). The same `Arc<AtomicBool>` is shared with all worker threads in M11; they all poll it on the same cadence. Stockfish's documented mechanism is *"atomic flags and condition variables"* per the DeepWiki summary surfaced in our search results, which matches.
- `isready` answer comes from the reader thread directly — zero contention with search. **This is the single biggest argument for a reader thread**: the spec demands an immediate `readyok` answer regardless of search state, and only a separate-thread design satisfies that without polling overhead in search.
- Minimal Rust-specific friction: `std::thread::spawn`, `std::sync::mpsc::channel`, `std::sync::Arc<AtomicBool>` are all in `std`. No `tokio`, no `crossbeam` dependency required at this layer.

Cons:

- Two extra concurrency primitives to reason about (channel + atomic). matklad's [Stopping a Rust Worker](https://matklad.github.io/2018/03/03/stopping-a-rust-worker.html) is the canonical write-up of the ownership/drop subtleties; worth reading once.
- Reader thread cannot be cancelled cleanly. Per [tokio Stdin docs](https://docs.rs/tokio/latest/tokio/io/struct.Stdin.html): *"stdin is implemented by using an ordinary blocking read on a separate thread, and it is impossible to cancel that read. This can make shutdown of the runtime hang until the user presses enter."* The same caveat applies to `std::io::Stdin` with a hand-rolled thread. Mitigation: on `quit`, the orchestrator just exits the process via `std::process::exit(0)`, abandoning the reader; the OS reaps it.

### 2.3 Async runtime (tokio)

`tokio::io::Stdin` exists, but its own docs (above) admit it's a blocking read on a hidden thread under the hood. Net architecture is identical to §2.2 except mediated by a runtime. We'd add ~3 MB of compile-time tokio for what amounts to an mpsc channel and a thread spawn.

Cons that rule it out:

- Search is CPU-bound, not I/O-bound. The async sweet spot (many low-cost concurrent I/O tasks) doesn't apply. Search work would run on `spawn_blocking`, defeating the purpose.
- Adds a dependency, compile time, and a mental model (futures, executors, `Send` bounds on every coroutine state machine) for zero structural benefit over `std::thread`.
- Lazy-SMP is naturally a thread-pool primitive, not a future. Mapping it onto tokio is fighting the runtime.

**Ruled out.** No identified upside vs. §2.2. Including only because the prompt asked.

### 2.4 Reader thread → channel → orchestrator → search worker

Variant of §2.2 where the *orchestrator* (main thread) is logically distinct from the *search worker* (separately-spawned thread per `go`). The reader pumps `Command`s to the orchestrator; the orchestrator owns engine state (current position, options, transposition table reference) and decides what to do with each command — including launching, signalling, or joining a search worker.

This is the shape every nontrivial engine converges to once iterative deepening lands. It separates three concerns:

- **Reader**: parse text → `Command`. Stateless (no engine state needed for parsing).
- **Orchestrator**: own `Position`, `Options`, deadline, search-handle. Routes each command to either an immediate reply (`isready`, `uci`, `quit`) or an effect on the search (`go`, `stop`).
- **Search worker**: pure compute. Reads `Position` (clone or `Arc`), reads cancellation flag and deadline, writes `info` and `bestmove`.

Pros: clean separation of concerns; orchestrator is single-threaded, so option mutation, position updates, and deadline computation are race-free; search worker can be replaced (random-mover for M2, alpha-beta for M3, lazy-SMP pool for M11) without touching the reader or orchestrator.

Cons: more wiring than §2.2 if you collapse them. But "main thread *is* orchestrator" is the natural Rust idiom — no extra spawn.

**Recommendation: §2.4. Reader thread + main-thread-as-orchestrator + per-`go` search worker thread, mpsc channel reader→orchestrator, `Arc<AtomicBool>` orchestrator→search.** §2.2 collapses into §2.4 if you merge orchestrator and search; the explicit split is what we want once M3 lands and `go` becomes a long-running concurrent computation. Single-threaded poll-stdin (§2.1) is rejected primarily because of the `isready`-during-search requirement and the M11 scaling story.

---

## 3. Cancellation primitive

### 3.1 The choices

- **`Arc<AtomicBool>`** with `Ordering::Relaxed`. Search calls `flag.load(Relaxed)` periodically; orchestrator calls `flag.store(true, Relaxed)` on `stop`. Relaxed is sufficient because we don't need the cancellation read to synchronize *other* memory — the search will exit cleanly and re-enter `bestmove` printing under the orchestrator's control ([std::sync::atomic::AtomicBool](https://doc.rust-lang.org/std/sync/atomic/struct.AtomicBool.html), [Rust Atomics and Locks Ch. 3](https://mara.nl/atomics/memory-ordering.html)).
- **`std::sync::mpsc` from orchestrator to search**. Search calls `try_recv` periodically. Heavier (an mpsc receive involves a mutex on the disconnect-detection path), and we'd still need the search to handle the channel after returning. Worse for SMP: we'd need one channel per worker.
- **`crossbeam::channel`**. Same conceptual fit as mpsc but with `select!` and faster `try_recv`. Brings a dependency. The Inanis changelog ([Inanis releases](https://github.com/Tearth/Inanis/releases) prose surfaced in search) notes that engine moving *away* from crossbeam toward `std::thread`. Not load-bearing for M2.
- **Drop-the-channel pattern** (matklad): orchestrator drops a `Sender`, search's `Receiver` returns `Disconnected`. Elegant for single-shot workers; awkward when the same search runs repeatedly across many `go`s in a single game (we'd have to rebuild the channel each time).

### 3.2 Failure modes to think about

The hard one: search is *deep* in recursion when `stop` arrives. The flag flips. The next polling site sees it and returns `None` / a sentinel. Every recursive frame has to propagate the cancellation upward and **must not commit results to the transposition table or update the principal variation as if the search were complete**. CPW [Iterative Deepening](https://www.chessprogramming.org/Iterative_Deepening) makes the standard observation: *"in case of an unfinished search, the program always has the option to fall back to the move selected in the last iteration of the search."* This is a search-loop invariant, not a threading one — but the threading model has to provide a clean signal that "this iteration is garbage; use the previous one."

The other one: what if `stop` arrives between iterations? Then iterative deepening simply breaks out of its outer loop and the previous iteration's `bestmove` is returned. No partial-iteration discard needed.

### 3.3 Memory ordering nuance

`Relaxed` is fine for the cancellation flag *itself*. But search threads also write to a shared TT in M4+; that's where ordering matters. Common engine practice ([Lockless Hashing](https://www.chessprogramming.org/Shared_Hash_Table)) uses XOR-trick lockless TT entries, sidestepping the issue. M2 doesn't have a TT so the question is moot.

**Recommendation: `Arc<AtomicBool>` with `Ordering::Relaxed`. Polled every 4096 nodes inside search. Set by orchestrator on `stop` and on time expiry. Same primitive scales to lazy-SMP: every worker thread holds an `Arc` clone of the same flag.**

---

## 4. Latency expectations

How fast must `stop` → `bestmove` be? The spec says "as soon as possible" — no number. From prose:

- Cute Chess sets a configurable per-engine *quit timeout* and an *idle timeout* (informed intuition from the cutechess source-prose summary in §6 below). Engines that don't respond promptly get killed; the message *"Terminating process of engine X"* surfaces in the logs ([TalkChess Terminating process](https://talkchess.com/viewtopic.php?t=84024)).
- The default quit timeout in cutechess is on the order of seconds, not milliseconds. Issue [#476](https://github.com/cutechess/cutechess/issues/476) is a feature request to harden timeouts further. Engines that take >1 s to respond to `isready` are flagged as misbehaving in user reports.
- In *short-time-control* tournament play (1+0 games), every millisecond of stop-to-bestmove latency is play time the engine loses on the clock. Practitioner consensus surfaced in our searches: keep stop-latency well under 10 ms; 1 ms is comfortable.

Concrete budget for our engine:

| Event | Target | Failure mode |
|---|---|---|
| `isready` → `readyok` | <1 ms | GUI marks engine dead |
| `stop` → `bestmove` (steady state) | <10 ms | clock loss in fast TCs |
| `stop` → `bestmove` (worst case) | <100 ms | acceptable; ratchet down later |
| `quit` → process exit | <1 s | cutechess sends SIGTERM |

These are judgment calls informed by the cutechess prose. Our 4096-node polling interval at 33 Mnps means ~120 µs between cancellation checks — comfortably inside the budget.

**Recommendation: target <1 ms `isready` reply (trivially met by reader-thread design), <10 ms `stop` → `bestmove` in steady state, <1 s `quit` → exit.**

---

## 5. Stdin idioms in Rust on macOS

### 5.1 Reading

`std::io::stdin()` returns a buffered handle. `stdin.lock().lines()` yields `Result<String, io::Error>` for each `\n`-terminated line ([std::io::BufRead](https://doc.rust-lang.org/std/io/trait.BufRead.html)). The lock is essential: without it, every `read_line` re-takes the mutex internally. UCI is line-based and the spec allows arbitrary whitespace within tokens (`docs/reference/uci-protocol-2006.txt` line 23–25), so we trim and split on whitespace once we have the line.

### 5.2 EOF / partial lines

The `lines()` iterator returns `None` on EOF. EOF on stdin happens when:
1. Cute Chess closes the engine's stdin (which it does after sending `quit`, judgment call from cutechess source-prose).
2. The terminal is closed (interactive use).
3. The parent process dies before sending `quit` (crash recovery).

The reader thread on EOF should send a synthetic `Command::Quit` (or close its end of the channel) and exit. The orchestrator, on receiving `Quit` *or* on detecting the channel disconnect, terminates the process. Treating EOF-without-quit as equivalent to `quit` is the safe default — there's nothing useful to do without input.

### 5.3 Partial lines

`BufRead::read_line` waits for `\n` before returning. So we never see a partial line. The spec confirms commands always end in `\n` (line 17). The Note about `\n` being `\r\n` on Windows (line 19) is handled by `trim_end` on each line.

### 5.4 What cutechess actually does

From the cutechess source-prose summary (§6): cutechess sends `quit`, starts a *quit timer*, and if the engine doesn't exit before `defaultQuitTimeout`, it `kill()`s the process (closes the I/O device and terminates). Engines must therefore handle `quit` cleanly *or* tolerate stdin closing as the second-best signal. Our reader thread's EOF handling covers both paths.

### 5.5 Buffering of stdout

[Capturing User Input](https://www.chessprogramming.net/uci-protocol-capturing-user-input/) recommends `setbuf(stdout, NULL)` in C — disable stdout buffering so the GUI sees `bestmove` immediately. In Rust, the equivalent is to call `stdout().flush()` after each line, or to wrap stdout in a `LineWriter` (which is the default for `std::io::Stdout` already). Verify; don't trust silent buffering in tournament play. **Use `println!` and explicitly `stdout().flush()` after `bestmove` and `readyok`.** Judgment call backed by chessprogramming.net article.

**Recommendation: blocking `BufRead::lines()` on `stdin().lock()` in a dedicated reader thread; trim and tokenise; send parsed `Command`s through `mpsc::channel`. Treat EOF as synthetic `Quit`. Always `flush()` stdout after `bestmove`, `readyok`, `uciok`, `info`.**

---

## 6. Process termination — what cutechess does

The cutechess source-prose summary (from `projects/lib/src/chessengine.cpp`, paraphrased above): the GUI calls `sendQuit()` (which writes `quit\n`) and starts `m_quitTimer`. If the engine hasn't exited by `defaultQuitTimeout` (configurable via `QSettings`, default on the order of seconds), `onQuitTimeout()` fires and calls `kill()` — closing the I/O device and terminating. Separately, an *idle timeout* (`defaultIdleTimeout`) fires if the engine stops responding to *anything* for too long; that path also kills the process and forfeits the game as `StalledConnection`.

GitHub issues [#27](https://github.com/cutechess/cutechess/issues/27), [#405](https://github.com/cutechess/cutechess/issues/405), and [#476](https://github.com/cutechess/cutechess/issues/476) document repeated user complaints about engines hanging cutechess; those issues are the prior art establishing that real engines do get stuck and tournament runners *do* eventually kill them.

**Implication for our engine**: on `quit`, exit the process within a second. The cleanest path is `std::process::exit(0)` from the orchestrator after writing nothing further to stdout. Don't try to gracefully join the reader thread (it's blocked on a `read` that can't be cancelled per the [tokio Stdin docs](https://docs.rs/tokio/latest/tokio/io/struct.Stdin.html) caveat that applies equally to `std::io::Stdin`). The OS reaps the thread when the process exits.

**Recommendation: on `quit`, the orchestrator flips the cancellation flag (so any in-flight search bails out), waits up to ~100 ms for the search worker to join, then `std::process::exit(0)`. Do not attempt to clean up the reader thread.**

---

## 7. Logging conventions

Three options, none of them load-bearing for M2 but all worth committing to once:

- **`info string ...`** to stdout. Per the spec (lines 297–299): *"any string str which will be displayed [by] the engine, if there is a string command the rest of the line will be interpreted as `<str>`."* Cute Chess shows these in its Engine Debug pane and in the `-debug` log of `cutechess-cli` ([TalkChess #66124](https://talkchess.com/viewtopic.php?t=66124), [GitHub issue #33](https://github.com/cutechess/cutechess/issues/33)). Stockfish uses `info string` for things like NNUE-loaded notices (search prose). **Visible to the user, no protocol overhead, intermixes cleanly with other `info` lines.**
- **`eprintln!` to stderr.** Tournament runners typically *don't* display stderr — `cutechess-cli --debug` captures protocol traffic, not stderr (informed intuition from the talkchess prose). Useful for engine-developer diagnostics that shouldn't bother the GUI; harmful as the only logging channel because nobody sees it.
- **File log.** Strongest option for post-mortem. Costs an open file handle, a writer-thread or careful flushing, and disk I/O during search.

The spec also has a `debug [on|off]` command (lines 68–73) intended for engines to *increase* their info-string emission rate when debug mode is on; this is the right place to wire `info string` verbosity. `debug` arrives on stdin and is mid-search-legal — handled by the reader thread, applied by the orchestrator, read by the search via a shared atomic or a mutexed config struct.

**Recommendation: route engine-developer diagnostics through `info string`, gated by the `debug on/off` UCI command (default off). Add a sidecar file log later if post-mortem becomes load-bearing — out of scope for M2. Stderr stays unused in protocol-affecting paths.**

---

## 8. Time-management interface

Where does the deadline live? Three places it could:

- **In the search itself.** Search owns an `Instant` deadline; checks `Instant::now() >= deadline` periodically (same cadence as cancellation flag).
- **In the orchestrator.** Orchestrator computes a deadline on `go`; sets a timer thread that flips the cancellation flag when it expires.
- **In a dedicated timer thread.** `std::thread::sleep_until(deadline); flag.store(true, Relaxed);` is two lines.

Mainstream prose ([CPW Time Management](https://www.chessprogramming.org/Time_Management), [CPW Iterative Deepening](https://www.chessprogramming.org/Iterative_Deepening)) describes a *two-level* deadline: a soft bound checked between iterations (don't start the next one if past soft) and a hard bound checked periodically inside the iteration (abort immediately if past hard). That's a search concern, not a threading one. The threading model just provides a single cancellation signal that the search and any timer agree on.

For M2 we have neither iterative deepening nor time pressure. `go movetime <ms>` is trivial: orchestrator records `deadline = Instant::now() + Duration::from_millis(ms)` before launching search; search picks a random move and returns instantly. Time management is a no-op until M4.

For M4+ the cleanest design fuses the cancellation flag and the deadline:

- Orchestrator sets `deadline` and clears `flag` on `go`.
- Search polls *both* every 4096 nodes: `if flag.load(Relaxed) || Instant::now() >= deadline { abort }`.
- A timer thread is unnecessary; the polling-cadence-vs-deadline-precision tradeoff is in the noise (4096 nodes ≈ 120 µs at 33 Mnps).

This avoids the spurious complexity of a fourth thread that exists only to flip a bool.

**Recommendation: deadline lives in an `Arc<Instant>` (or, more cheaply, in a `SearchContext` cloned to the worker on launch). Search polls `Instant::now()` and the cancellation flag on the same cadence (4096 nodes). No separate timer thread. Two-level (soft/hard) split is a search-internal refinement for M4.**

---

## 9. Anti-patterns

These look reasonable on paper and bite in tournament play:

- **Busy-waiting for input in the search thread.** Burns a CPU core, contends with search for cache, doesn't actually get input any faster than a blocking thread does.
- **Blocking `read_line` in the search thread.** Self-evident; search wouldn't run.
- **Parsing stdin from inside `make_move`.** Real-world: a contributor adds a "let's check input here too, it's cheap" call deep in the search recursion. Now `make_move` sometimes blocks. Violates the layering.
- **Cancellation via `panic!` + `catch_unwind`.** Tempting because it unwinds the recursion automatically. Wrong because panic across thread boundaries in Rust has subtle semantics, and the unwinder runs destructors that may include TT writes — committing dirty state.
- **Flushing stdout only on Drop.** A search that crashes won't flush its `bestmove`. Cute Chess sees stdin close without `bestmove` and forfeits the game. Always flush explicitly.
- **`std::process::abort` on quit.** Skips destructors; on macOS may produce a crash-report dialog and a non-zero exit code that confuses tournament tools. Use `std::process::exit(0)`.
- **Re-entrant `info` writes from multiple threads without coordination.** With lazy-SMP, each worker is tempted to print its own `info depth N pv ...`. Without serialization, lines interleave and the GUI sees garbage. Fix: only the *main* search worker prints; helpers stay silent. Out of scope for M2 but worth flagging.
- **Trusting `setoption` is not received during search.** Spec says it won't be (line 89), but a defensive engine *queues* it and applies between searches. Cheap insurance.
- **Polling stdin too often in the search.** Every poll is a syscall. At 100 Mnps, polling every 64 nodes is 1.5M syscalls/sec — measurable overhead. 4096 is the standard cadence (see [implementing UCI stop](http://talkchess.com/forum3/viewtopic.php?t=46368)). With a reader thread, polling becomes an `AtomicBool::load` (1–2 cycles) rather than a syscall, which makes the cadence less critical — but staying around 4096 keeps the engineering judgment honest.

**Recommendation: enforce a layering rule — search code calls only `flag.load()` and `Instant::now()`, never any I/O. Reader thread calls only `BufRead::read_line` and `mpsc::Sender::send`. Orchestrator owns all stdout writes that are protocol replies.**

---

## 10. Recommended threading model for our engine

### Architecture

**Reader thread → mpsc → main-as-orchestrator + per-`go` search worker thread, cancellation via `Arc<AtomicBool>` shared between orchestrator and search.**

```
┌────────────┐   stdin   ┌────────────┐ Command  ┌──────────────┐
│   stdin    │──────────▶│   Reader   │─────────▶│ Orchestrator │──── stdout (replies, info, bestmove)
│  (kernel)  │           │   thread   │ mpsc     │ (main thread)│
└────────────┘           └────────────┘          └──────┬───────┘
                                                         │ spawn / signal
                                                         ▼
                                                  ┌────────────┐
                                                  │   Search   │ ── reads Position, polls flag+deadline,
                                                  │   worker   │    writes info via channel back to orchestrator
                                                  └────────────┘
                                                        ▲
                                                        │ Arc<AtomicBool> (cancellation)
                                                        └──────── flipped by orchestrator on stop / time expiry
```

### Cancellation primitive

`Arc<AtomicBool>` with `Ordering::Relaxed`. Polled every 4096 nodes by search. Set by orchestrator on `stop` and on deadline expiry. Cleared by orchestrator at the start of each `go`. Same primitive scales unchanged to lazy-SMP — every worker holds an `Arc` clone.

### Stdin reader strategy

Dedicated `std::thread::spawn`'d reader thread doing blocking `stdin.lock().lines()`. Each line is trimmed and parsed into a `Command` enum and pushed onto an `std::sync::mpsc::Sender<Command>`. EOF translates to a synthetic `Command::Quit`. The reader is never joined cleanly — on `quit`, the orchestrator calls `std::process::exit(0)` and the OS reaps it.

### Key invariants the search loop must obey

1. **Poll cadence**: check `flag.load(Relaxed)` *and* `Instant::now() >= deadline` every 4096 nodes (one `if` at the top of every recursive `search` call, gated by a node counter).
2. **Cancellation propagation**: if either check fires, return a sentinel result immediately. Every recursive frame propagates upward without committing TT writes for the cancelled subtree (M4+).
3. **No I/O in search**: search never reads stdin, never writes stdout directly. `info` lines for M3+ go through a channel back to the orchestrator (or are written under a stdout `Mutex` if the channel is too heavy — a judgment call to revisit).
4. **Deadline is read-only**: search never updates the deadline. Orchestrator owns it.
5. **`bestmove` is the *last* line of every `go`**: print `info` lines up to the cancellation point, then `bestmove`, then flush. The spec mandates `bestmove` for every `go` (line 211).
6. **Search worker exits cleanly on cancellation**: it does not panic, abort, or block. The orchestrator must be able to `join()` it within ~100 ms of flipping the flag.

### Minimal `Search` trait sketch for M3 plug-in

```rust
/// Cancellation + time-bound interface every search implementation reads from.
/// Cheap to clone — typically holds `Arc`s.
#[derive(Clone)]
pub struct SearchContext {
    pub stop:     Arc<AtomicBool>,
    pub deadline: Option<Instant>,
    pub start:    Instant,           // for `info time`
    pub limits:   SearchLimits,      // depth, nodes, movetime, etc., parsed from `go`
}

impl SearchContext {
    /// Called inside the search every 4096 nodes.
    /// `#[inline]`. Return `true` to abort this iteration immediately.
    #[inline]
    pub fn should_abort(&self, nodes: u64) -> bool {
        if self.stop.load(Ordering::Relaxed) { return true; }
        if let Some(d) = self.deadline {
            if Instant::now() >= d { return true; }
        }
        if let Some(n) = self.limits.nodes {
            if nodes >= n { return true; }
        }
        false
    }
}

/// What every `go` produces. M2 returns immediately with a random move
/// and `Default` everything else; M3+ fills in score/depth/pv as they go.
#[derive(Default)]
pub struct SearchResult {
    pub bestmove: Option<Move>,
    pub ponder:   Option<Move>,
    pub depth:    u32,
    pub score_cp: Option<i32>,
    pub nodes:    u64,
}

pub trait Search {
    /// Run a search. Must obey `ctx`: poll cancellation, respect deadline,
    /// emit `info` lines via `info_sink`, return cleanly on cancellation.
    /// Must not write to stdout directly. Must not read stdin.
    fn go(
        &mut self,
        position: &Position,
        ctx: &SearchContext,
        info_sink: &dyn Fn(&str),  // serialized in orchestrator
    ) -> SearchResult;
}
```

For M2, the implementation is one line: pick a uniform-random element from `generate_moves(position)`, return it. The trait exists *now* so M3 doesn't have to invent it under deadline pressure.

The orchestrator's `go` handler:

```rust
// pseudocode
fn handle_go(&mut self, limits: SearchLimits) {
    let ctx = SearchContext {
        stop: self.stop.clone(),
        deadline: limits.compute_deadline(&self.position, &self.clock),
        start: Instant::now(),
        limits,
    };
    self.stop.store(false, Ordering::Relaxed);

    let pos = self.position.clone();
    let mut search = self.search.clone();   // Box<dyn Search> or generic
    let stdout_mu = self.stdout_mu.clone();
    let handle = std::thread::spawn(move || {
        let result = search.go(&pos, &ctx, &|line| {
            let mut out = stdout_mu.lock().unwrap();
            writeln!(out, "{}", line).unwrap();
            out.flush().unwrap();
        });
        // Print bestmove from inside the worker so the line is the last
        // emission of this search; the lock serializes with info lines.
        let mut out = stdout_mu.lock().unwrap();
        match result.bestmove {
            Some(mv) => writeln!(out, "bestmove {}", mv.to_uci()).unwrap(),
            None     => writeln!(out, "bestmove 0000").unwrap(),  // null move per spec line 49
        }
        out.flush().unwrap();
    });
    self.search_handle = Some(handle);
}

fn handle_stop(&mut self) {
    self.stop.store(true, Ordering::Relaxed);
    if let Some(h) = self.search_handle.take() {
        let _ = h.join();   // worker prints bestmove on its way out
    }
}

fn handle_isready(&self) {
    let mut out = self.stdout_mu.lock().unwrap();
    writeln!(out, "readyok").unwrap();
    out.flush().unwrap();
}

fn handle_quit(&mut self) -> ! {
    self.stop.store(true, Ordering::Relaxed);
    if let Some(h) = self.search_handle.take() {
        let _ = h.join();
    }
    std::process::exit(0);
}
```

Three load-bearing details in the sketch:

1. **`bestmove` is printed by the search worker, not the orchestrator.** That guarantees it's the last line of the search (after all `info`) and that it appears even if the orchestrator is busy receiving the next command. The `stdout_mu` lock serializes it with `info` lines from the same worker and (later) helper workers in lazy-SMP.
2. **Null-move fallback `bestmove 0000`** per spec line 49 — needed if the search is stopped before finding any legal move, or in checkmate/stalemate positions if the GUI somehow asks us to search anyway.
3. **`isready` answer comes from the orchestrator**, not the worker. The reader thread parses `isready`, sends `Command::IsReady` to the orchestrator, the orchestrator immediately writes `readyok`. Latency = one channel send + one print + one flush. Well under 1 ms. Spec compliance demonstrated.

### Scaling to lazy-SMP (M11)

The architecture above is forward-compatible without refactor:

- The search worker thread becomes the *main* worker. On `go`, it spawns N–1 *helpers* via `thread::scope` or a thread pool.
- All workers share the same `Arc<AtomicBool>` and `Arc<TT>`.
- Helpers do not write `info` or `bestmove` — only the main worker does. The `stdout_mu` ensures no interleaving even if a future bug introduces a helper write.
- Stop propagation: orchestrator flips the flag once; all N workers see it on their next poll. Wait-for-all-to-stop is a `thread::scope` join.

This matches CPW's *"set abort flag for each helper and wait for each to stop searching"* description ([CPW Lazy SMP](https://www.chessprogramming.org/Lazy_SMP)). No new primitives needed.

### Scaling to NNUE (M12)

NNUE is per-position eval. It runs inside the search worker and has zero interaction with threading. The architecture above is unaffected.

---

## Summary of recommendations

| Area | Recommendation |
|---|---|
| Threading model | Reader thread + main-as-orchestrator + per-`go` search worker. (§2.4) |
| Async runtime | No. tokio adds dependency and indirection for zero structural benefit; CPU-bound work is wrong fit. |
| Cancellation primitive | `Arc<AtomicBool>` with `Ordering::Relaxed`. Polled every 4096 nodes. |
| Inter-thread channel | `std::sync::mpsc` reader → orchestrator. No `crossbeam` / `tokio` dependency at this layer. |
| Stdin idiom | Blocking `BufRead::lines()` on `stdin().lock()` in dedicated thread. EOF → synthetic `Quit`. |
| Stdout idiom | `Mutex<StdoutLock>` shared between orchestrator and search worker. Explicit `flush()` after every protocol-reply line. |
| Logging | `info string` (visible in cutechess); gated by UCI `debug on/off`. Stderr unused. File log deferred. |
| Time management | Deadline in `SearchContext`, polled at the same cadence as cancellation. No timer thread. |
| `bestmove` discipline | Always last line of a `go`; printed by the search worker; null-move `0000` if no legal move. |
| `isready` discipline | Answered immediately by orchestrator without touching search. |
| `quit` discipline | Flip flag, brief join, `std::process::exit(0)`. Reader thread is abandoned. |
| Anti-pattern firewall | Search code never touches stdin/stdout. Reader code never touches engine state. Orchestrator owns all coupling. |
| `Search` trait | Defined now (M2), implemented as random-mover for M2, alpha-beta for M3, lazy-SMP wrapper for M11. No signature change expected. |

### Open uncertainties

- **stdout `Mutex` vs. info-channel.** The sketch uses a shared `Mutex<Stdout>`. An alternative is sending `String` info lines through a channel back to the orchestrator and having the orchestrator print them. Channel adds latency to `info` (no real cost — these are display, not protocol-critical). Mutex adds contention (negligible for one search worker; potentially noticeable in lazy-SMP if helpers ever write, which they shouldn't). Defer the choice to M3 when actual `info` lines exist.
- **Orchestrator owns position vs. Arc-Position.** For M2 we clone the position into the search worker (cheap — ~200 B). For lazy-SMP we want `Arc<Position>` if the position is read-only across helpers. Both work; the trait sketch passes `&Position` and is agnostic.
- **Cancellation polling cadence**. 4096 is the prose-standard number ([implementing UCI stop](http://talkchess.com/forum3/viewtopic.php?t=46368)). Could be tuned per-engine; profile-driven only — not an architectural choice.
- **Ponder support**. `go ponder` and `ponderhit` are deferred to a later milestone. The threading model accommodates them: `ponderhit` is just a command that updates the deadline (search keeps running), `stop` during ponder is exactly the same cancellation path. No structural addition required.
- **Windows compatibility**. macOS is primary. The model uses only `std::thread`, `std::sync`, `std::io::stdin().lock()` — all portable. Windows-specific behavior (CRLF line endings handled by `trim_end`; non-blocking-stdin nuances irrelevant since we're using a blocking reader thread) shouldn't bite.
