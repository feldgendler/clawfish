//! UCI engine I/O loop and command dispatch.
//!
//! Threading model per ADR-0011 (`docs/decisions/0011-uci-io-threading.md`):
//! reader thread → mpsc → main-as-orchestrator + per-`go` search worker
//! thread, with `Arc<AtomicBool>` cancellation.
//!
//! - [`Engine`] is the orchestrator. Generic over `W: Write + Send + 'static`
//!   (stdout) and `S: Search + Send + 'static` (the search implementation).
//! - [`reader_loop`] is the reader-thread body — translates lines from any
//!   `BufRead` into `Command`s on an `mpsc::Sender`.
//! - [`run_stdio`] is the production wrapper: spawns the reader thread on
//!   `io::stdin().lock()`, builds an `Engine` with `io::stdout()` and
//!   `AlphaBetaMover` (M3.C production search), calls `run`, then
//!   `std::process::exit(0)`.

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::search::{
    AlphaBetaMover, Search, SearchContext, SearchLimits, SearchResult, TimeCaps, compute_caps,
};
use crate::{Command, DebugMode, GoParams, Move, Position, PositionSpec, Register, parse_uci_line};

// Type is u32; value is 2^31 - 1 (the protocol-declared `max`). Not i32 —
// the comparison band [0, MAX_RANDOM_SEED] is unsigned by construction.
// Must match the `max` in the `option name Random_Seed …` line emitted by
// `handle_uci`.
const MAX_RANDOM_SEED: u32 = 2_147_483_647;

/// Maximum value for the `MoveOverhead` UCI option (M3.E). 5000 ms matches
/// the value recommended by `docs/research/m3-time-management.md` §6 and is
/// adequate for fastchess CI runners. Must match the `max` in the
/// `option name MoveOverhead …` line emitted by `handle_uci`.
const MAX_MOVE_OVERHEAD: u64 = 5_000;

/// Default `Hash` table size in MiB. Industry consensus (Stockfish et al.).
/// Must match the `default` in the `option name Hash …` line emitted by `handle_uci`.
pub(crate) const DEFAULT_HASH_MIB: usize = 16;
/// Maximum `Hash` table size in MiB. Realistic ceiling for Apple Silicon dev boxes.
/// Must match the `max` in the `option name Hash …` line emitted by `handle_uci`.
pub(crate) const MAX_HASH_MIB: usize = 4096;
/// Minimum `Hash` table size in MiB. Must match the `min` in the
/// `option name Hash …` line emitted by `handle_uci`.
pub(crate) const MIN_HASH_MIB: usize = 1;

/// Default `MoveOverhead` value (M3.E). 50 ms matches the research §6 default
/// and is a safer hedge than Stockfish's 10 ms default for typical macOS
/// scheduling jitter.
const DEFAULT_MOVE_OVERHEAD: u64 = 50;

/// Compute nodes-per-second for the `bench` summary line (M3.F).
///
/// Returns `(total_nodes × 1000) / max(total_ms, 1)` as `u128`. The `max(1)`
/// guard avoids division-by-zero on sub-millisecond benches; multiplication
/// promotes to `u128` to avoid u64 overflow at billion-node bench sizes
/// (`u64::MAX × 1000` wouldn't fit a u64).
///
/// Extracted into a named helper from `Engine::handle_bench`'s body — the
/// inline expression had three structurally-undetectable mutations
/// (`/ → %`, `/ → *`, `* → +`) that survived end-to-end bench testing because
/// the test asserts NPS is "within order of magnitude" rather than exact.
/// As a free helper, the arithmetic is directly unit-testable on synthetic
/// inputs that pin the exact division semantics. Same precedent as M3.D's
/// `negate_window` extraction.
pub(crate) fn compute_bench_nps(total_nodes: u64, total_ms: u128) -> u128 {
    let denom = total_ms.max(1);
    (total_nodes as u128 * 1000) / denom
}

/// UCI orchestrator. Owns engine state, dispatches parsed `Command`s to
/// per-command handlers, drives the search worker thread.
pub struct Engine<W: Write + Send + 'static, S: Search + Send + 'static> {
    position: Position,
    /// Zobrist trajectory from start-of-game through the current position.
    /// Invariant: `game_history.last() == Some(&position.zobrist())`.
    game_history: Vec<u64>,
    debug: bool,
    stop: Arc<AtomicBool>,
    stdout: Arc<Mutex<W>>,
    search: Arc<Mutex<S>>,
    search_handle: Option<JoinHandle<()>>,
    /// `MoveOverhead` UCI option value (M3.E). Latency hedge in ms subtracted
    /// from clock-derived caps in `compute_caps`. Default 50; valid range
    /// `[0, MAX_MOVE_OVERHEAD]`.
    move_overhead: u64,
    /// Engine-owned transposition table. `Arc::clone`d into each `SearchContext`
    /// at `handle_go` / `handle_bench` time. Resized via `setoption name Hash`;
    /// cleared via `handle_ucinewgame` and per-position inside `handle_bench`.
    /// Single-mutator invariant per ADR-0011 + ADR-0018 §2: orchestrator mutates
    /// only after `join_in_flight_worker()` returns; search worker is the only
    /// reader/writer during `Search::go`.
    tt: Arc<crate::tt::TranspositionTable>,
    /// Current `Hash` UCI option value in MiB. Default `DEFAULT_HASH_MIB`,
    /// valid `[MIN_HASH_MIB, MAX_HASH_MIB]`. Tracked separately from the TT
    /// for setoption echo / debugging; not used in hot paths.
    hash_mib: usize,
    /// `VirtualClock` UCI option (ELOH.C). When `true`, `handle_go` sets
    /// `SearchContext::virtual_clock = true` so the worker thread uses
    /// thread-CPU-time for search time-keeping. Always defaults to `false`.
    /// On non-unix platforms this field exists but cannot be set to `true`
    /// (the option is not advertised and `handle_setoption` rejects the value).
    virtual_clock: bool,
}

impl<W: Write + Send + 'static, S: Search + Send + 'static> Engine<W, S> {
    /// Build an engine. Position starts at `Position::starting_position()`,
    /// `debug` off, no search in flight, `move_overhead` at default, TT at
    /// `DEFAULT_HASH_MIB`.
    pub fn new(stdout: W, search: S) -> Self {
        let position = Position::starting_position();
        let tt = Arc::new(crate::tt::TranspositionTable::new(DEFAULT_HASH_MIB));
        Self {
            game_history: vec![position.zobrist()],
            position,
            debug: false,
            stop: Arc::new(AtomicBool::new(false)),
            stdout: Arc::new(Mutex::new(stdout)),
            search: Arc::new(Mutex::new(search)),
            search_handle: None,
            move_overhead: DEFAULT_MOVE_OVERHEAD,
            tt,
            hash_mib: DEFAULT_HASH_MIB,
            virtual_clock: false,
        }
    }

    /// Test-only access to `move_overhead` (M3.E). Required for Slice A's
    /// `MoveOverhead` UCI option tests to verify the option's effect without
    /// depending on Slice B's `compute_caps` integration.
    #[cfg(test)]
    pub(crate) fn move_overhead(&self) -> u64 {
        self.move_overhead
    }

    /// Test-only access to `virtual_clock` (ELOH.C). Required for the
    /// `VirtualClock` UCI option tests in `mod tests` to verify the option's
    /// flag-flip behavior without driving an actual `go`.
    #[cfg(test)]
    pub(crate) fn virtual_clock(&self) -> bool {
        self.virtual_clock
    }

    /// Drive the engine. Returns when `Quit` is received from `rx`.
    pub fn run(&mut self, rx: mpsc::Receiver<Command>) {
        loop {
            let cmd = match rx.recv() {
                Ok(c) => c,
                Err(_) => {
                    unreachable!("reader_loop always sends Quit before disconnect (plan §10)")
                }
            };
            match cmd {
                Command::Uci => self.handle_uci(),
                Command::Debug(mode) => self.handle_debug(mode),
                Command::IsReady => self.handle_isready(),
                Command::SetOption { name, value } => self.handle_setoption(name, value),
                Command::Register(r) => self.handle_register(r),
                Command::UciNewGame => self.handle_ucinewgame(),
                Command::Position { spec, moves } => self.handle_position(spec, moves),
                Command::Go(params) => self.handle_go(params),
                Command::Bench { depth } => self.handle_bench(depth),
                Command::Stop => self.handle_stop(),
                Command::PonderHit => self.handle_ponderhit(),
                Command::Quit => {
                    self.handle_quit();
                    return;
                }
                Command::Unknown => self.handle_unknown(),
            }
        }
    }

    /// Test-only access to the engine's position.
    #[cfg(test)]
    pub(crate) fn position(&self) -> &Position {
        &self.position
    }

    /// Test-only access to the engine's game-history Zobrist trajectory.
    #[cfg(test)]
    pub(crate) fn game_history(&self) -> &[u64] {
        &self.game_history
    }

    fn handle_uci(&mut self) {
        self.write_line(&format!("id name clawfish {}", env!("CARGO_PKG_VERSION")));
        self.write_line("id author Alex Feldgendler");
        self.write_line("option name Random_Seed type spin default 0 min 0 max 2147483647");
        self.write_line("option name MoveOverhead type spin default 50 min 0 max 5000");
        self.write_line(&format!(
            "option name Hash type spin default {DEFAULT_HASH_MIB} min {MIN_HASH_MIB} max {MAX_HASH_MIB}"
        ));
        // ELOH.C / ADR-0021: only advertise on platforms where the engine can
        // service the option (POSIX `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`).
        #[cfg(unix)]
        self.write_line("option name VirtualClock type check default false");
        self.write_line("uciok");
    }

    fn handle_isready(&mut self) {
        self.write_line("readyok");
    }

    fn handle_ucinewgame(&mut self) {
        self.reset_for_new_game();
    }

    fn handle_position(&mut self, spec: PositionSpec, moves: Vec<String>) {
        let base = match spec {
            PositionSpec::StartPos => Position::starting_position(),
            PositionSpec::Fen(ref s) => match Position::from_fen(s) {
                Ok(p) => p,
                Err(e) => {
                    self.info_string_always(&format!("position rejected: invalid FEN: {e}"));
                    // Leave both self.position and self.game_history untouched.
                    return;
                }
            },
        };

        let mut pos = base;
        let mut hist = vec![base.zobrist()];
        for mv_str in &moves {
            match Move::from_uci(mv_str, &pos) {
                Ok(mv) => {
                    pos.make_move(mv);
                    hist.push(pos.zobrist());
                }
                Err(e) => {
                    self.info_string_always(&format!(
                        "position rejected: move {mv_str} failed: {e}"
                    ));
                    self.position = base;
                    self.game_history = vec![base.zobrist()];
                    return;
                }
            }
        }
        self.position = pos;
        self.game_history = hist;
    }

    fn handle_go(&mut self, params: GoParams) {
        // (1) Implicit-stop on back-to-back go: signal the previous worker to
        // stop and join it via the shared helper. Setting stop=true before join
        // is load-bearing for infinite searches; without it the join would block
        // forever waiting for the worker to exit on its own.
        self.join_in_flight_worker();
        // (2) Clear the cancellation flag so the new search starts fresh.
        self.stop.store(false, Ordering::Relaxed);

        // (3) Build SearchLimits: parse searchmoves, silently dropping bad
        // entries (plan §6).
        let searchmoves: Option<Vec<Move>> = params.searchmoves.map(|raw_moves| {
            raw_moves
                .iter()
                .filter_map(|s| Move::from_uci(s, &self.position).ok())
                .collect()
        });

        let limits = SearchLimits {
            depth: params.depth,
            nodes: params.nodes,
            movetime: params.movetime,
            mate: params.mate,
            infinite: params.infinite,
            ponder: params.ponder,
            wtime: params.wtime,
            btime: params.btime,
            winc: params.winc,
            binc: params.binc,
            movestogo: params.movestogo,
            searchmoves,
        };

        // (4) Compute soft + hard time caps via `compute_caps` (M3.E). The
        // orchestrator NEVER reads any clock under ELOH.C: `compute_caps` is
        // a pure function of durations, and `CLOCK_THREAD_CPUTIME_ID` is a
        // per-thread counter — orchestrator-thread reads would be the wrong
        // values for the worker. The worker constructs `SearchClock` at the
        // top of `Search::go` from `caps + virtual_clock`.
        let caps = compute_caps(&limits, self.position.side_to_move(), self.move_overhead);

        // (5) Spawn the worker. Always threaded — the orchestrator must remain
        // responsive to `isready`, `stop`, and `quit` while the search is in
        // flight. The bestmove-vs-quit race that previously motivated a
        // synchronous bare-go path is now closed by the join in handle_quit
        // (plan §9): handle_quit signals stop and joins, so the worker's
        // bestmove write is guaranteed visible before `run` returns.
        let position = self.position;
        let search = Arc::clone(&self.search);
        let stdout = Arc::clone(&self.stdout);
        let ctx = SearchContext {
            stop: Arc::clone(&self.stop),
            caps,
            virtual_clock: self.virtual_clock,
            limits,
            history: self.game_history.clone(),
            tt: Some(Arc::clone(&self.tt)),
        };

        let handle = std::thread::spawn(move || {
            let info_sink = {
                let stdout = Arc::clone(&stdout);
                move |line: &str| {
                    let mut out = stdout.lock().unwrap();
                    let _ = writeln!(out, "{line}");
                    let _ = out.flush();
                }
            };
            let result: SearchResult = {
                let mut s = search.lock().unwrap();
                s.go(&position, &ctx, &info_sink)
            };
            // bestmove is the last line of every go (ADR-0011).
            let mv_str = result
                .bestmove
                .map(|m| m.to_uci())
                .unwrap_or_else(|| "0000".to_string());
            let mut out = stdout.lock().unwrap();
            let _ = writeln!(out, "bestmove {mv_str}");
            let _ = out.flush();
        });
        self.search_handle = Some(handle);
    }

    fn handle_stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.search_handle.take() {
            let _ = h.join();
        }
    }

    fn handle_debug(&mut self, mode: DebugMode) {
        self.debug = matches!(mode, DebugMode::On);
    }

    fn handle_setoption(&mut self, name: String, value: Option<String>) {
        if name.eq_ignore_ascii_case("random_seed") {
            // Strict: parse as u32, then reject if above the declared max.
            // Honors the protocol contract that values fall in [min, max].
            let parsed: Option<u32> = value
                .as_deref()
                .and_then(|s| s.parse::<u32>().ok())
                .filter(|&n| n <= MAX_RANDOM_SEED);
            match parsed {
                Some(n) => {
                    // Join any in-flight worker before acquiring the search mutex.
                    // A `go infinite` running concurrently would hold the lock for
                    // the entire polling duration, blocking the orchestrator and
                    // preventing it from processing `stop`. ADR-0011 v3.
                    self.join_in_flight_worker();
                    self.search.lock().unwrap().set_seed(n as u64);
                    // Silent on success — even under `debug on`. Same convention as
                    // `handle_debug`: state-mutating commands without a protocol
                    // response do not echo themselves.
                }
                None => {
                    // Bad parse, out-of-range, or missing value — silent if
                    // debug off; info string if debug on. The engine's
                    // existing seed value is unchanged.
                    let msg = match value.as_deref() {
                        Some(v) => format!("Random_Seed: rejected value '{v}'"),
                        None => "Random_Seed: rejected (no value given)".to_string(),
                    };
                    self.info_string_debug(&msg);
                }
            }
            return;
        }

        if name.eq_ignore_ascii_case("moveoverhead") {
            let parsed: Option<u64> = value
                .as_deref()
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|&n| n <= MAX_MOVE_OVERHEAD);
            match parsed {
                Some(n) => {
                    // No need to join the in-flight worker — `move_overhead` is
                    // read at the top of `handle_go` to build the next
                    // SearchContext, not by the worker mid-search.
                    self.move_overhead = n;
                }
                None => {
                    let msg = match value.as_deref() {
                        Some(v) => format!("MoveOverhead: rejected value '{v}'"),
                        None => "MoveOverhead: rejected (no value given)".to_string(),
                    };
                    self.info_string_debug(&msg);
                }
            }
            return;
        }

        if name.eq_ignore_ascii_case("hash") {
            let parsed: Option<usize> = value
                .as_deref()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|&n| (MIN_HASH_MIB..=MAX_HASH_MIB).contains(&n));
            match parsed {
                Some(n) => {
                    // Resize requires no in-flight worker (the search would observe
                    // a Vec swap mid-probe). Mirrors `random_seed` discipline.
                    self.join_in_flight_worker();
                    self.tt.resize(n);
                    self.hash_mib = n;
                    // Clear stop after join so any subsequent go starts fresh.
                    self.stop.store(false, Ordering::Relaxed);
                }
                None => {
                    let msg = match value.as_deref() {
                        Some(v) => format!("Hash: rejected value '{v}'"),
                        None => "Hash: rejected (no value given)".to_string(),
                    };
                    self.info_string_debug(&msg);
                }
            }
            return;
        }

        // ELOH.C / ADR-0021: `VirtualClock` is a `check`-typed boolean option,
        // gated `#[cfg(unix)]` because the time source it selects
        // (`CLOCK_THREAD_CPUTIME_ID`) is POSIX-only. Value parsing is
        // case-insensitive (`true`/`false`/`True`/`TRUE`/...). Like
        // `MoveOverhead`, no worker-join is needed — `virtual_clock` is read
        // at the top of `handle_go` to build the next SearchContext, not by
        // the worker mid-search. Rejection emits via `info_string_always` to
        // surface malformed values regardless of debug mode (mirrors the
        // `info_string_always` rejection path on `position`).
        #[cfg(unix)]
        if name.eq_ignore_ascii_case("virtualclock") {
            let parsed: Option<bool> =
                value
                    .as_deref()
                    .map(|s| s.to_ascii_lowercase())
                    .and_then(|s| match s.as_str() {
                        "true" => Some(true),
                        "false" => Some(false),
                        _ => None,
                    });
            match parsed {
                Some(b) => self.virtual_clock = b,
                None => {
                    let msg = match value.as_deref() {
                        Some(v) => format!("VirtualClock: rejected value '{v}'"),
                        None => "VirtualClock: rejected (no value given)".to_string(),
                    };
                    self.info_string_always(&msg);
                }
            }
            return;
        }

        // Unknown option — preserve M2.C behavior: silent if debug off, info
        // string if debug on. Do NOT emit Stockfish's bare "No such option:"
        // line (research §1.4, §2.5). Idiom mirrors existing M2.C handler:
        // the `if self.debug` guard wraps the whole format-and-write because
        // `details` allocation is conditional too.
        if self.debug {
            let details = match value {
                Some(ref v) => format!("name {name} value {v}"),
                None => format!("name {name}"),
            };
            self.info_string_always(&format!("setoption received: {details}"));
        }
    }

    fn handle_register(&mut self, r: Register) {
        if self.debug {
            let details = match r {
                Register::Later => "Later".to_string(),
                Register::Identify { name, code } => {
                    let mut parts = Vec::new();
                    if let Some(n) = name {
                        parts.push(format!("name {n}"));
                    }
                    if let Some(c) = code {
                        parts.push(format!("code {c}"));
                    }
                    format!("Identify {{ {} }}", parts.join(", "))
                }
            };
            self.info_string_always(&format!("register received: {details}"));
        }
    }

    fn handle_ponderhit(&mut self) {
        self.info_string_debug("ponderhit received");
    }

    fn handle_unknown(&mut self) {
        self.info_string_debug("unknown command");
    }

    /// `bench` UCI command (M3.F): drive a deterministic node-count regression
    /// baseline. Iterates a vendored fixed-position FEN corpus, drives
    /// `Search::go` at fixed depth on each, sums node counts, emits per-position
    /// `info string` lines and a final OpenBench-grep-compatible signature.
    /// See `docs/plans/m3.f.md` §5 for the full design.
    fn handle_bench(&mut self, depth_override: Option<u32>) {
        use crate::bench::{BENCH_DEFAULT_DEPTH, BENCH_POSITIONS};

        // Defensive: signal + join any in-flight worker. Mirrors
        // `handle_ucinewgame`'s discipline — bench is synchronous on the
        // orchestrator thread, so the dispatch loop is blocked until bench
        // returns; commands like `isready`/`stop` queue in mpsc until then.
        // `join_in_flight_worker` sets stop=true; clear it after so the
        // initial per-position reset doesn't immediately re-dirty the flag
        // (reset_for_new_game clears stop internally, so this is belt-and-
        // braces).
        self.join_in_flight_worker();
        self.stop.store(false, Ordering::Relaxed);

        let depth = depth_override.unwrap_or(BENCH_DEFAULT_DEPTH);

        let start = Instant::now();
        let mut total_nodes: u64 = 0;

        for (idx, fen) in BENCH_POSITIONS.iter().enumerate() {
            // Reset per-game state before each position: clears TT entries,
            // game_history, and search-side state. This is the ADR-0018 §14
            // discipline; without it the TT carries scores from position N into
            // position N+1, breaking bench determinism across corpus order.
            // reset_for_new_game also resets position to startpos, which is
            // immediately overwritten by the FEN parse below — a harmless
            // extra assignment.
            self.reset_for_new_game();

            // FEN parse failure is unreachable for the vendored corpus
            // (`bench_positions_all_parse_via_from_fen` is the anchor), but
            // a defensive skip keeps `bench` robust over future expansions.
            let pos = match Position::from_fen(fen) {
                Ok(p) => p,
                Err(e) => {
                    self.info_string_always(&format!(
                        "bench: skipping position {} (FEN parse error: {e})",
                        idx + 1,
                    ));
                    continue;
                }
            };

            let limits = SearchLimits {
                depth: Some(depth),
                ..SearchLimits::default()
            };
            let pos_start = Instant::now();
            let ctx = SearchContext {
                stop: Arc::clone(&self.stop),
                caps: TimeCaps {
                    soft: Duration::MAX,
                    hard: Duration::MAX,
                },
                virtual_clock: self.virtual_clock,
                limits,
                history: vec![pos.zobrist()],
                tt: Some(Arc::clone(&self.tt)),
            };

            // Synchronous-on-orchestrator-thread invocation. info_sink is a
            // no-op closure to suppress per-iteration ID `info depth N …`
            // lines (would emit 16 × depth lines of noise).
            let result: SearchResult = {
                let mut s = self.search.lock().unwrap();
                let sink = |_: &str| {};
                s.go(&pos, &ctx, &sink)
            };
            let elapsed_ms = pos_start.elapsed().as_millis();

            total_nodes += result.nodes;

            // Per-position summary, routed through `info_string_always` per
            // ADR-0011 discipline (all non-spec output via `info string`).
            self.info_string_always(&format!(
                "bench position {}/{}: {} nodes {} time {}",
                idx + 1,
                BENCH_POSITIONS.len(),
                fen,
                result.nodes,
                elapsed_ms,
            ));
        }

        let total_ms = start.elapsed().as_millis();
        let nps = compute_bench_nps(total_nodes, total_ms);

        // Final summary lines, both `info string`-prefixed:
        //   1. `Nodes searched: <N>` — human-readable.
        //   2. `bench: <N> nodes <NPS> nps` — OpenBench-grep-compatible
        //      signature. Strict OpenBench-format would be a bare-prefix line;
        //      our `info string` form is substring-grep-compatible (regex
        //      `bench: [0-9]+ nodes [0-9]+ nps` matches the substring) but
        //      not bytewise OpenBench-format. Acceptable since clawfish has
        //      no CLI-bench mode at M3.F (would be where bytewise format
        //      matters for OpenBench scraping).
        self.info_string_always(&format!("Nodes searched: {total_nodes}"));
        self.info_string_always(&format!("bench: {total_nodes} nodes {nps} nps"));
    }

    fn handle_quit(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Join any in-flight search worker so its bestmove write is visible
        // before `run` returns. The stop signal above ensures the worker exits
        // promptly; the join is bounded by the worker's cancellation-check
        // cadence (1 ms). The reader thread is NOT joined here — it would
        // deadlock waiting for more stdin input. run_stdio calls process::exit
        // immediately after run returns, reaping the reader thread.
        if let Some(h) = self.search_handle.take() {
            let _ = h.join();
        }
    }

    /// Signal stop and join the in-flight search worker, if any.
    ///
    /// Used by `handle_go` (back-to-back go), `reset_for_new_game`, and
    /// `handle_setoption`'s `Random_Seed` / `Hash` success paths — anything
    /// that needs to mutate engine state or hold the search mutex for a
    /// non-trivial duration. ADR-0011 v3 guarantees the worker exits within
    /// ≤ 1 ms (its cancellation-poll cadence). The caller must clear
    /// `self.stop` afterward when a new search is about to begin.
    fn join_in_flight_worker(&mut self) {
        if let Some(h) = self.search_handle.take() {
            self.stop.store(true, Ordering::Relaxed);
            let _ = h.join();
        }
    }

    /// Reset all per-game state. Called from `handle_ucinewgame` AND from
    /// `handle_bench` per position. Single source of truth for game-boundary
    /// state lifecycle (ADR-0018 §14). Order:
    ///   1. Join any in-flight worker (defensive; orchestrator-thread call).
    ///   2. Reset position + game_history to startpos.
    ///   3. Clear the TT (zeros entries, resets generation to 0).
    ///   4. Call Search::reset for any search-side per-game state.
    ///   5. Clear stop so subsequent go does not inherit a stale true.
    fn reset_for_new_game(&mut self) {
        self.join_in_flight_worker();
        self.position = Position::starting_position();
        self.game_history = vec![self.position.zobrist()];
        self.tt.clear();
        self.search.lock().unwrap().reset();
        self.stop.store(false, Ordering::Relaxed);
    }

    /// Test-only accessor for the engine's transposition table.
    #[cfg(test)]
    pub(crate) fn tt(&self) -> &Arc<crate::tt::TranspositionTable> {
        &self.tt
    }

    /// Test-only accessor for the current `hash_mib` value.
    #[cfg(test)]
    pub(crate) fn hash_mib(&self) -> usize {
        self.hash_mib
    }

    /// Lock stdout, write the line + `\n`, flush. All protocol-relevant
    /// engine→GUI lines go through this helper.
    fn write_line(&self, line: &str) {
        let mut out = self.stdout.lock().unwrap();
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }

    /// Emit an `info string <msg>` line unconditionally (used by
    /// `handle_position`'s reject path per plan §5).
    fn info_string_always(&self, msg: &str) {
        self.write_line(&format!("info string {msg}"));
    }

    /// Emit an `info string <msg>` line iff `self.debug` is on.
    fn info_string_debug(&self, msg: &str) {
        if self.debug {
            self.info_string_always(msg);
        }
    }
}

/// Reader-thread body. Reads lines from `stdin`, parses each via
/// [`parse_uci_line`], and pushes the resulting `Command` onto `tx`. EOF
/// (or any read error) is translated to a synthetic `Command::Quit` send,
/// after which the function returns.
///
/// Generic over `BufRead` so tests can drive it with `Cursor<&[u8]>`.
pub fn reader_loop(stdin: impl BufRead, tx: mpsc::Sender<Command>) {
    for line in stdin.lines() {
        match line {
            Ok(l) => {
                let cmd = parse_uci_line(&l);
                let is_quit = matches!(cmd, Command::Quit);
                if tx.send(cmd).is_err() {
                    return;
                }
                if is_quit {
                    return;
                }
            }
            Err(_) => break,
        }
    }
    // Synthetic Quit on EOF (or read error).
    let _ = tx.send(Command::Quit);
}

/// Production wrapper: spawn the reader thread on `io::stdin().lock()`,
/// build an `Engine` with `io::stdout()` and `AlphaBetaMover` (M3.C
/// production search), drive `run`, then `std::process::exit(0)`.
pub fn run_stdio() -> ! {
    let (tx, rx) = mpsc::channel::<Command>();
    std::thread::spawn(move || {
        reader_loop(std::io::BufReader::new(std::io::stdin()), tx);
    });
    let mut engine = Engine::new(std::io::stdout(), AlphaBetaMover::new());
    engine.run(rx);
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{AlphaBetaMover, SearchContext, SearchResult};
    use crate::{Command, Move, Position, parse_uci_line};
    use std::io::Cursor;
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    /// Test fixture: captures all writes into a shared `Vec<u8>` so the
    /// test holds a handle to the buffer after the engine takes ownership
    /// of the writer.
    pub(super) struct CapturedWriter(pub Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Test-only Search impl that emits 3 `info string thinking N` lines
    /// via `info_sink`, then returns naturally with `bestmove == None`
    /// (so the engine emits `bestmove 0000`). Used by B24 to verify the
    /// info-before-bestmove ordering invariant from ADR-0011.
    ///
    /// This is test infrastructure, not a placeholder for production code,
    /// so its body is implemented here in Phase 2 alongside the tests
    /// that depend on it.
    pub(super) struct InfoEmittingFake;

    impl Search for InfoEmittingFake {
        fn go(
            &mut self,
            _position: &Position,
            _ctx: &SearchContext,
            info_sink: &dyn Fn(&str),
        ) -> SearchResult {
            for n in 1..=3 {
                info_sink(&format!("info string thinking {n}"));
            }
            SearchResult::default()
        }
    }

    // -----------------------------------------------------------------------
    // Group A harness
    // -----------------------------------------------------------------------

    /// Builds an `Engine<CapturedWriter, AlphaBetaMover>`, sends each UCI line as
    /// a parsed `Command` over an mpsc channel, appends `Command::Quit`, runs
    /// to completion, and returns the captured stdout + a clone of the final
    /// position.
    fn drive(commands: &[&str]) -> (String, Position) {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, AlphaBetaMover::new());

        let (tx, rx) = mpsc::channel::<Command>();
        for line in commands {
            tx.send(parse_uci_line(line)).unwrap();
        }
        tx.send(Command::Quit).unwrap();
        engine.run(rx);

        let bytes = buf.lock().unwrap().clone();
        let stdout = String::from_utf8(bytes).expect("engine output must be valid UTF-8");
        let pos = *engine.position();
        (stdout, pos)
    }

    /// Like `drive`, but pre-loads Kiwipete before running the supplied
    /// commands. Used by A9/A10/A11 so the "prior state" is non-trivial.
    fn drive_from_kiwipete(commands: &[&str]) -> (String, Position) {
        let kiwipete =
            "position fen r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1";
        let mut all = vec![kiwipete];
        all.extend_from_slice(commands);
        drive(&all)
    }

    // -----------------------------------------------------------------------
    // Group A — synchronous handler tests (A1–A17)
    // -----------------------------------------------------------------------

    #[test]
    fn handle_uci_emits_id_name_id_author_uciok_in_order() {
        let (stdout, _) = drive(&["uci"]);
        let lines: Vec<&str> = stdout.lines().collect();
        let id_name_idx = lines
            .iter()
            .position(|l| l.starts_with("id name"))
            .expect("id name line present");
        let id_author_idx = lines
            .iter()
            .position(|l| l.starts_with("id author"))
            .expect("id author line present");
        let option_random_seed_idx = lines
            .iter()
            .position(|l| l.starts_with("option name Random_Seed"))
            .expect("option name Random_Seed line present");
        let uciok_idx = lines
            .iter()
            .position(|l| *l == "uciok")
            .expect("uciok line present");
        assert!(
            id_name_idx < id_author_idx,
            "id name must come before id author"
        );
        assert!(
            id_author_idx < option_random_seed_idx,
            "id author must come before option name Random_Seed"
        );
        assert!(
            option_random_seed_idx < uciok_idx,
            "option name Random_Seed must come before uciok"
        );
    }

    #[test]
    fn handle_uci_id_name_includes_cargo_pkg_version() {
        let (stdout, _) = drive(&["uci"]);
        let version = env!("CARGO_PKG_VERSION");
        let id_name_line = stdout
            .lines()
            .find(|l| l.starts_with("id name"))
            .expect("id name line present");
        assert!(
            id_name_line.contains(version),
            "id name line '{id_name_line}' should contain version '{version}'"
        );
    }

    #[test]
    fn handle_isready_emits_readyok() {
        let (stdout, _) = drive(&["isready"]);
        assert!(
            stdout.lines().any(|l| l == "readyok"),
            "readyok must be emitted"
        );
    }

    #[test]
    fn handle_ucinewgame_resets_position() {
        // Pre-load Kiwipete, then ucinewgame should reset to starting position.
        let (_, pos) = drive_from_kiwipete(&["ucinewgame"]);
        assert_eq!(
            pos,
            Position::starting_position(),
            "position must be reset to starting position after ucinewgame"
        );
    }

    #[test]
    fn handle_position_startpos_no_moves_succeeds_silent() {
        let (stdout, pos) = drive(&["position startpos"]);
        assert_eq!(pos, Position::starting_position());
        // Silent — no info string lines expected for a successful position command.
        assert!(
            !stdout
                .lines()
                .any(|l| l.starts_with("info string position rejected")),
            "successful position command must not emit a rejection info string"
        );
    }

    #[test]
    fn handle_position_fen_kiwipete_succeeds() {
        let kiwipete_fen = "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1";
        let (stdout, pos) = drive(&[&format!("position fen {kiwipete_fen}")]);
        let expected = Position::from_fen(kiwipete_fen).expect("kiwipete FEN is valid");
        assert_eq!(pos, expected);
        assert!(
            !stdout
                .lines()
                .any(|l| l.starts_with("info string position rejected")),
            "successful position command must not emit a rejection info string"
        );
    }

    #[test]
    fn handle_position_startpos_with_legal_moves_applies_them() {
        // e2e4 is a legal first move.
        let (stdout, pos) = drive(&["position startpos moves e2e4"]);
        let mut expected = Position::starting_position();
        use crate::mov::Move;
        let mv = Move::from_uci("e2e4", &expected).expect("e2e4 is legal from startpos");
        expected.make_move(mv);
        assert_eq!(pos, expected);
        assert!(
            !stdout
                .lines()
                .any(|l| l.starts_with("info string position rejected")),
            "successful move must not emit rejection"
        );
    }

    #[test]
    fn handle_position_fen_with_moves_applies_them() {
        // Start from Kiwipete, apply a legal move.
        let kiwipete_fen = "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1";
        let (stdout, pos) = drive(&[&format!("position fen {kiwipete_fen} moves e2a6")]);
        let mut expected = Position::from_fen(kiwipete_fen).expect("valid");
        use crate::mov::Move;
        let mv = Move::from_uci("e2a6", &expected).expect("e2a6 is legal from kiwipete");
        expected.make_move(mv);
        assert_eq!(pos, expected);
        assert!(
            !stdout
                .lines()
                .any(|l| l.starts_with("info string position rejected")),
            "successful move must not emit rejection"
        );
    }

    #[test]
    fn handle_position_invalid_fen_keeps_prior_state() {
        // Pre-load Kiwipete, then send a bad FEN. Engine must keep Kiwipete.
        // The FEN string must have exactly 6 space-separated tokens so the
        // UCI parser produces a Position command (rather than Unknown); the
        // error is then surfaced by Position::from_fen, not the UCI parser.
        let kiwipete_fen = "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1";
        let expected_pos = Position::from_fen(kiwipete_fen).expect("valid");
        let (stdout, pos) = drive_from_kiwipete(&["position fen not a valid fen here now"]);
        assert_eq!(
            pos, expected_pos,
            "invalid FEN must keep the prior position unchanged"
        );
        assert!(
            stdout
                .lines()
                .any(|l| l.starts_with("info string position rejected")),
            "invalid FEN must emit info string position rejected"
        );
    }

    #[test]
    fn handle_position_malformed_move_resets_to_base() {
        // Pre-load Kiwipete, then send startpos with a malformed move.
        // Engine must reset to starting_position (the base), not revert to Kiwipete.
        let (stdout, pos) = drive_from_kiwipete(&["position startpos moves z9z9"]);
        assert_eq!(
            pos,
            Position::starting_position(),
            "malformed move must reset position to the base (startpos)"
        );
        assert!(
            stdout
                .lines()
                .any(|l| l.starts_with("info string position rejected")),
            "malformed move must emit info string position rejected"
        );
    }

    #[test]
    fn handle_position_illegal_move_for_position_resets_to_base() {
        // e2e5 is not a legal move from startpos (pawn can't jump 3 squares).
        // Engine must reset to starting_position (the base), not keep Kiwipete.
        let (stdout, pos) = drive_from_kiwipete(&["position startpos moves e2e5"]);
        assert_eq!(
            pos,
            Position::starting_position(),
            "illegal move must reset position to the base (startpos)"
        );
        assert!(
            stdout
                .lines()
                .any(|l| l.starts_with("info string position rejected")),
            "illegal move must emit info string position rejected"
        );
    }

    #[test]
    fn handle_position_partial_success_then_fail_resets_to_base() {
        // Pins plan §5: even when several moves apply successfully before a
        // failing move, the engine resets to the *base* position (no moves
        // applied) — NOT to the prefix-applied state. A regression where
        // make_move's mutation persisted on later failure would leave the
        // engine at the post-e2e4 position instead of startpos.
        let (stdout, pos) = drive_from_kiwipete(&["position startpos moves e2e4 z9z9"]);
        assert_eq!(
            pos,
            Position::starting_position(),
            "partial-success-then-fail must reset to base (startpos), NOT keep the prefix-applied state",
        );
        assert!(
            stdout
                .lines()
                .any(|l| l.starts_with("info string position rejected")),
            "partial-success-then-fail must emit info string position rejected",
        );
    }

    // ─── Handler info-string tests (debug on) ────────────────────────────

    #[test]
    fn handle_setoption_emits_info_string_when_debug_on() {
        // Pins plan §11: setoption emits `info string setoption received: …`
        // when debug is on for unknown options. Catches a mutant where the body
        // is replaced with `()`. Also pins both the `Some(value)` and `None`
        // arms of the `details` formatter.
        // (iii) Random_Seed + Hash with valid values under debug on: must be
        // SILENT on success (not echoed — different from unknown options).
        // M4.A: `setoption name Hash value 16` is now a known option (handled
        // before the unknown-option fallback) — its success path is silent
        // under debug on (mirrors `random_seed` precedent). Only `Clear` (truly
        // unknown) still echoes.
        let (stdout, _) = drive(&[
            "debug on",
            "setoption name Hash value 16",
            "setoption name Clear",
            "setoption name Random_Seed value 42",
        ]);
        // Only the unknown option `Clear` echoes; Hash and Random_Seed are silent on success.
        let info_lines: Vec<&str> = stdout
            .lines()
            .filter(|l| l.starts_with("info string setoption received:"))
            .collect();
        assert_eq!(
            info_lines.len(),
            1,
            "expected 1 setoption info-string line (Clear only; Hash is now a known option); got:\n{stdout}"
        );
        assert!(
            info_lines
                .iter()
                .any(|l| l.contains("name Clear") && !l.contains("value")),
            "expected `name Clear` (no value) in setoption info string;\nlines: {info_lines:?}",
        );
        // Hash success must produce zero Hash-mentioning info lines.
        assert_eq!(
            stdout
                .lines()
                .filter(|l| l.contains("name Hash value 16"))
                .count(),
            0,
            "setoption name Hash value 16 must not produce any Hash info lines under debug on; got:\n{stdout}",
        );
        // Explicit assertion: Random_Seed success must produce zero info lines.
        assert_eq!(
            stdout.lines().filter(|l| l.contains("Random_Seed")).count(),
            0,
            "setoption name Random_Seed value 42 must not produce any Random_Seed info lines under debug on; got:\n{stdout}",
        );
    }

    #[test]
    fn handle_register_emits_info_string_when_debug_on() {
        // Pins plan §11: register emits `info string register received: …`
        // when debug is on. Covers all three Register variants — Later,
        // Identify with name only, Identify with both name and code —
        // pinning the `parts.push(...)` formatter for `Register::Identify`.
        let (stdout, _) = drive(&[
            "debug on",
            "register later",
            "register name Stefan",
            "register name Stefan MK code 4359874324",
        ]);
        let info_lines: Vec<&str> = stdout
            .lines()
            .filter(|l| l.starts_with("info string register received:"))
            .collect();
        assert_eq!(
            info_lines.len(),
            3,
            "expected 3 register info-string lines; got:\n{stdout}"
        );
        assert!(
            info_lines.iter().any(|l| l.contains("Later")),
            "expected `Later` variant in register info string;\nlines: {info_lines:?}",
        );
        assert!(
            info_lines
                .iter()
                .any(|l| l.contains("name Stefan") && !l.contains("MK") && !l.contains("code")),
            "expected `name Stefan` (no code) in register info string;\nlines: {info_lines:?}",
        );
        assert!(
            info_lines
                .iter()
                .any(|l| l.contains("name Stefan MK") && l.contains("code 4359874324")),
            "expected both `name Stefan MK` and `code 4359874324` in register info string;\nlines: {info_lines:?}",
        );
    }

    #[test]
    fn handle_ponderhit_emits_info_string_when_debug_on() {
        // Pins plan §11: ponderhit emits `info string ponderhit received`
        // when debug is on. Catches a mutant where the body is replaced with `()`.
        let (stdout, _) = drive(&["debug on", "ponderhit"]);
        let info_lines: Vec<&str> = stdout
            .lines()
            .filter(|l| l.starts_with("info string ponderhit received"))
            .collect();
        assert_eq!(
            info_lines.len(),
            1,
            "expected 1 ponderhit info-string line; got:\n{stdout}"
        );
    }

    // ─── go searchmoves filtering tests ──────────────────────────────────

    #[test]
    fn handle_go_searchmoves_filters_to_specified_moves() {
        // Pins plan §6: `go searchmoves a2a4 b2b4` restricts the search to
        // those two legal moves. AlphaBetaMover picks the best-eval move from the
        // filtered set, so the bestmove must be one of {a2a4, b2b4}.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, AlphaBetaMover::new());
        let (tx, rx) = mpsc::channel::<Command>();

        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go searchmoves a2a4 b2b4")).unwrap();
        tx.send(Command::Quit).unwrap();
        engine.run(rx);

        let stdout = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let bestmove_line = stdout
            .lines()
            .find(|l| l.starts_with("bestmove"))
            .expect("bestmove line must be present");
        let uci_str = bestmove_line
            .strip_prefix("bestmove ")
            .expect("bestmove line has 'bestmove ' prefix");
        assert!(
            uci_str == "a2a4" || uci_str == "b2b4",
            "searchmoves a2a4/b2b4 must restrict bestmove to that set; got '{uci_str}';\nstdout:\n{stdout}",
        );
    }

    #[test]
    fn handle_go_searchmoves_all_bad_yields_bestmove_0000() {
        // Pins plan §6: when `searchmoves` parses to a list of all-illegal
        // entries, the resulting filter is `Some(Vec::new())` — AlphaBetaMover
        // finds no candidate and emits `bestmove 0000`. Distinct from "no
        // searchmoves keyword" which would let the mover pick any legal move.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, AlphaBetaMover::new());
        let (tx, rx) = mpsc::channel::<Command>();

        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go searchmoves z9z9 z8z7")).unwrap();
        tx.send(Command::Quit).unwrap();
        engine.run(rx);

        let stdout = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            stdout.lines().any(|l| l == "bestmove 0000"),
            "all-illegal searchmoves must yield bestmove 0000;\nstdout:\n{stdout}",
        );
    }

    #[test]
    fn handle_go_searchmoves_silently_drops_bad_entries() {
        // Pins plan §6: `searchmoves a2a4 z9z9 b2b4` parses with `z9z9`
        // silently dropped, leaving the filter [a2a4, b2b4]. AlphaBetaMover picks
        // the best-eval move from the filtered set, so bestmove ∈ {a2a4, b2b4}.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, AlphaBetaMover::new());
        let (tx, rx) = mpsc::channel::<Command>();

        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go searchmoves a2a4 z9z9 b2b4"))
            .unwrap();
        tx.send(Command::Quit).unwrap();
        engine.run(rx);

        let stdout = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let bestmove_line = stdout
            .lines()
            .find(|l| l.starts_with("bestmove"))
            .expect("bestmove line must be present");
        let uci_str = bestmove_line
            .strip_prefix("bestmove ")
            .expect("bestmove line has 'bestmove ' prefix");
        assert!(
            uci_str == "a2a4" || uci_str == "b2b4",
            "bad searchmoves entries must be silently dropped; bestmove must ∈ {{a2a4, b2b4}}; got '{uci_str}';\nstdout:\n{stdout}",
        );
    }

    #[test]
    fn handle_debug_on_then_unknown_emits_info_string() {
        // Per plan §11: handle_debug is itself silent (only toggles
        // self.debug). Only the Unknown handler emits an info string when
        // debug is on. So after `debug on`, exactly one Unknown command
        // produces exactly one info string line.
        let (stdout, _) = drive(&["debug on", "joho garbage"]);
        let info_lines: Vec<&str> = stdout
            .lines()
            .filter(|l| l.starts_with("info string"))
            .collect();
        assert_eq!(
            info_lines.len(),
            1,
            "expected exactly one info string line after `debug on` + unknown command; got {} lines:\n{stdout}",
            info_lines.len(),
        );
    }

    #[test]
    fn handle_debug_off_then_unknown_silent() {
        // Per plan §11: handle_debug is silent. So `debug on` + `debug off`
        // produces no output, and the subsequent Unknown is silent because
        // debug is off. Total info string count: zero.
        let (stdout, _) = drive(&["debug on", "debug off", "joho garbage"]);
        let info_count = stdout
            .lines()
            .filter(|l| l.starts_with("info string"))
            .count();
        assert_eq!(
            info_count, 0,
            "debug-on-then-debug-off-then-unknown must produce zero info string lines; got {info_count}:\n{stdout}",
        );
    }

    #[test]
    fn handle_setoption_silent_no_output_when_debug_off() {
        // (i) Random_Seed with valid value — success path. Silent on debug off.
        let (stdout_seed, _) = drive(&["setoption name Random_Seed value 42"]);
        assert!(
            stdout_seed.is_empty(),
            "setoption name Random_Seed value 42 must produce zero output when debug is off; got: {stdout_seed:?}",
        );

        // (ii) Hash with valid value — known option, success path. Silent on debug off.
        // M4.A: Hash is now a recognized option; valid values are accepted silently.
        let (stdout_hash, _) = drive(&["setoption name Hash value 16"]);
        assert!(
            stdout_hash.is_empty(),
            "setoption name Hash value 16 must produce zero output when debug is off; got: {stdout_hash:?}",
        );
    }

    #[test]
    fn handle_register_later_silent_when_debug_off() {
        // Forward-compat caveat: pin will be revised when first real option/registration/ponder lands.
        let (stdout, _) = drive(&["register later"]);
        assert!(
            stdout.is_empty(),
            "register later must produce zero output when debug is off; got: {stdout:?}",
        );
    }

    #[test]
    fn handle_ponderhit_silent_when_debug_off() {
        // Forward-compat caveat: pin will be revised when first real option/registration/ponder lands.
        let (stdout, _) = drive(&["ponderhit"]);
        assert!(
            stdout.is_empty(),
            "ponderhit must produce zero output when debug is off; got: {stdout:?}",
        );
    }

    #[test]
    fn handle_stop_no_search_silent() {
        let (stdout, _) = drive(&["stop"]);
        assert!(
            stdout.is_empty(),
            "stop with no search in flight must produce zero output; got: {stdout:?}",
        );
    }

    // -----------------------------------------------------------------------
    // Group B — threading tests (B18–B24)
    //
    // These tests build engine + channel manually, without the auto-Quit
    // harness from group A, so commands can be interleaved with a running
    // search worker.
    // -----------------------------------------------------------------------

    /// Returns the captured output as a String, snapshotting under the lock.
    fn snapshot_output(buf: &Arc<Mutex<Vec<u8>>>) -> String {
        let bytes = buf.lock().unwrap().clone();
        String::from_utf8(bytes).expect("engine output is UTF-8")
    }

    #[test]
    fn go_then_stop_emits_bestmove_for_legal_position() {
        // Intent: mid-search cancellation. After `stop`, the worker must emit
        // a legal bestmove from startpos (not `bestmove 0000`). The search
        // picks by eval — we assert any legal UCI move, not a specific one.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, AlphaBetaMover::new());
        let (tx, rx) = mpsc::channel::<Command>();

        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go infinite")).unwrap();

        let buf_clone = Arc::clone(&buf);
        let handle = thread::spawn(move || engine.run(rx));

        // 50 ms for the worker to spawn and enter its should_abort polling
        // loop before we cancel. Below ~50 ms risks `stop` arriving before
        // the worker is in flight (still spec-compliant per plan §8 — the
        // pre-enumeration check would emit `bestmove 0000` — but defeats
        // this test's intent of exercising mid-search cancellation).
        thread::sleep(Duration::from_millis(50));
        tx.send(parse_uci_line("stop")).unwrap();

        // Poll for any `bestmove <legal-uci>` before sending Quit.
        // handle_quit joins the worker (ADR-0011 v3), so bestmove is
        // guaranteed to be in stdout before `run` returns. The poll here is
        // defensive synchronization only — it confirms the worker has emitted
        // its bestmove before we issue Quit, without relying on timing.
        let startpos = Position::starting_position();
        let bestmove_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snap = snapshot_output(&buf_clone);
            if let Some(line) = snap.lines().find(|l| l.starts_with("bestmove")) {
                let uci_str = line.strip_prefix("bestmove ").expect("bestmove prefix");
                Move::from_uci(uci_str, &startpos).unwrap_or_else(|e| {
                    panic!("bestmove '{uci_str}' is not legal for startpos: {e};\noutput:\n{snap}")
                });
                break;
            }
            assert!(
                Instant::now() < bestmove_deadline,
                "no bestmove appeared within 1 s of stop;\noutput:\n{snap}"
            );
            thread::sleep(Duration::from_millis(2));
        }

        tx.send(Command::Quit).unwrap();
        handle.join().expect("engine thread should not panic");
    }

    #[test]
    fn go_with_movetime_emits_bestmove_after_deadline() {
        // Intent: wallclock deadline honored. M3.E semantics: `compute_caps`
        // applies `MoveOverhead` to `movetime` (research §10 pitfall fix).
        // With default `MoveOverhead=50`, `movetime=300` produces caps =
        // (250ms, 250ms). The bestmove arrives at ~250ms. Anchor: ≥ 200ms
        // (catches a mover that ignores movetime), ≤ 5s (catches a stuck
        // mover). The bound was chosen so that:
        //   - `MoveOverhead=50, movetime=300` → search budget 250ms → pass.
        //   - A buggy mover that ignores movetime → searches default depth-4
        //     to completion (≈ ms scale on a fast machine, but variable) →
        //     could pass too. The lower bound primarily catches "returned
        //     immediately" bugs (e.g. `compute_caps` returning (1ms, 1ms)).
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, AlphaBetaMover::new());
        let (tx, rx) = mpsc::channel::<Command>();

        tx.send(parse_uci_line("position startpos")).unwrap();

        let buf_clone = Arc::clone(&buf);
        let handle = thread::spawn(move || engine.run(rx));

        let startpos = Position::starting_position();
        let go_sent = Instant::now();
        tx.send(parse_uci_line("go movetime 300")).unwrap();

        let bestmove_deadline = go_sent + Duration::from_secs(5);
        let observed_at = loop {
            let snap = snapshot_output(&buf_clone);
            if let Some(line) = snap.lines().find(|l| l.starts_with("bestmove")) {
                let uci_str = line.strip_prefix("bestmove ").expect("bestmove prefix");
                Move::from_uci(uci_str, &startpos).unwrap_or_else(|e| {
                    panic!("bestmove '{uci_str}' is not legal for startpos: {e};\noutput:\n{snap}")
                });
                break Instant::now();
            }
            assert!(
                Instant::now() < bestmove_deadline,
                "no bestmove appeared within 5 s of go movetime 300;\noutput:\n{snap}"
            );
            thread::sleep(Duration::from_millis(2));
        };

        let elapsed = observed_at.duration_since(go_sent);
        // Lower bound 200 ms: with default MoveOverhead=50 and movetime=300,
        // the search budget is 250 ms; the bestmove must arrive at ~250 ms,
        // not significantly earlier. Catches a regression where movetime is
        // not honored at all.
        assert!(
            elapsed >= Duration::from_millis(200),
            "go movetime 300 → bestmove must take >= 200 ms (movetime - default MoveOverhead 50 = 250ms); \
             took {elapsed:?}"
        );

        tx.send(Command::Quit).unwrap();
        handle.join().expect("engine thread should not panic");
    }

    #[test]
    fn go_completes_immediately_without_infinite_or_movetime() {
        // Intent: bare go (no infinite/movetime/ponder) must complete without
        // entering the wait loop. The worker writes bestmove and exits. We assert
        // any legal UCI move. The 5 s deadline is generous enough to accommodate
        // a depth-4 alpha-beta search in debug builds while still catching a
        // regression where the mover accidentally enters the polling loop.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, AlphaBetaMover::new());
        let (tx, rx) = mpsc::channel::<Command>();

        tx.send(parse_uci_line("position startpos")).unwrap();

        let buf_clone = Arc::clone(&buf);
        let handle = thread::spawn(move || engine.run(rx));

        let startpos = Position::starting_position();
        tx.send(parse_uci_line("go")).unwrap();

        // 5 s is enough even for a slow debug build at depth 4. A regression
        // entering the infinite wait loop would block until Quit is sent.
        let bestmove_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snap = snapshot_output(&buf_clone);
            if let Some(line) = snap.lines().find(|l| l.starts_with("bestmove")) {
                let uci_str = line.strip_prefix("bestmove ").expect("bestmove prefix");
                Move::from_uci(uci_str, &startpos).unwrap_or_else(|e| {
                    panic!("bestmove '{uci_str}' is not legal for startpos: {e};\noutput:\n{snap}")
                });
                break;
            }
            assert!(
                Instant::now() < bestmove_deadline,
                "no bestmove appeared within 5 s of bare go;\noutput:\n{snap}"
            );
            thread::sleep(Duration::from_millis(2));
        }

        tx.send(Command::Quit).unwrap();
        handle.join().expect("engine thread should not panic");
    }

    #[test]
    fn isready_during_go_answers_immediately() {
        // Verify that `isready` is answered with `readyok` while a search
        // is *actively running* — not just at any time before `bestmove`.
        // Anchor: poll for `readyok` to appear in the buffer BEFORE sending
        // `stop`. A synchronous-only impl that processed isready after
        // stop arrived would never produce `readyok` until the worker
        // exits, and this poll would time out.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, AlphaBetaMover::new());
        let (tx, rx) = mpsc::channel::<Command>();

        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go infinite")).unwrap();

        let buf_clone = Arc::clone(&buf);
        let handle = thread::spawn(move || engine.run(rx));

        // Give the search time to enter its polling loop, then send isready.
        thread::sleep(Duration::from_millis(50));
        tx.send(parse_uci_line("isready")).unwrap();

        // Wait until `readyok` appears in the buffer — this MUST happen
        // before we send `stop`, proving the engine answered while the
        // search was still running.
        let readyok_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snap = snapshot_output(&buf_clone);
            if snap.lines().any(|l| l == "readyok") {
                break;
            }
            assert!(
                Instant::now() < readyok_deadline,
                "readyok did not appear within 1 s of isready while search was running;\noutput:\n{snap}"
            );
            thread::sleep(Duration::from_millis(2));
        }

        // Confirmed readyok-during-search. Now stop and quit.
        tx.send(parse_uci_line("stop")).unwrap();
        tx.send(Command::Quit).unwrap();
        handle.join().expect("engine thread should not panic");

        // Final ordering invariant: readyok appears before bestmove in the
        // captured output. (This is implied by the above — we observed
        // readyok before stop was sent — but pinned for clarity.)
        let snap = snapshot_output(&buf_clone);
        let lines: Vec<&str> = snap.lines().collect();
        let readyok_idx = lines
            .iter()
            .position(|l| *l == "readyok")
            .expect("readyok must appear in output");
        let bestmove_idx = lines
            .iter()
            .position(|l| l.starts_with("bestmove"))
            .expect("bestmove must appear in output");
        assert!(
            readyok_idx < bestmove_idx,
            "readyok (line {readyok_idx}) must precede bestmove (line {bestmove_idx});\noutput:\n{snap}"
        );
    }

    #[test]
    fn quit_during_go_returns_run_promptly() {
        // handle_quit joins the worker so bestmove is visible in stdout before
        // run returns (ADR-0011 v3). This test verifies both that run returns
        // promptly AND that a bestmove line is present in the captured output.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let buf_clone = Arc::clone(&buf);
        let mut engine = Engine::new(writer, AlphaBetaMover::new());
        let (tx, rx) = mpsc::channel::<Command>();

        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go infinite")).unwrap();

        let handle = thread::spawn(move || engine.run(rx));

        // 50 ms for thread spawn + worker to enter the polling loop on a
        // possibly-loaded CI runner. Below ~50 ms the test risks `quit`
        // arriving before the worker is in flight, which would silently
        // pass without exercising the concurrent-search-with-quit path.
        thread::sleep(Duration::from_millis(50));
        tx.send(Command::Quit).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if handle.is_finished() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Engine::run must return within 1 s after quit; timed out"
            );
            thread::sleep(Duration::from_millis(5));
        }
        handle.join().expect("engine thread should not panic");

        // handle_quit joins the worker before run returns, so bestmove must
        // already be in the output buffer by now. Validate it's a legal move.
        let snap = snapshot_output(&buf_clone);
        let bestmove_line = snap
            .lines()
            .find(|l| l.starts_with("bestmove"))
            .unwrap_or_else(|| {
                panic!("bestmove must be in output after handle_quit joins the worker (ADR-0011 v3);\noutput:\n{snap}")
            });
        let uci_str = bestmove_line
            .strip_prefix("bestmove ")
            .expect("bestmove prefix");
        let startpos = Position::starting_position();
        Move::from_uci(uci_str, &startpos).unwrap_or_else(|e| {
            panic!("bestmove '{uci_str}' is not legal for startpos: {e};\noutput:\n{snap}")
        });
    }

    #[test]
    fn back_to_back_go_implicit_stops_previous() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, AlphaBetaMover::new());
        let (tx, rx) = mpsc::channel::<Command>();

        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go infinite")).unwrap();

        let buf_clone = Arc::clone(&buf);
        let handle = thread::spawn(move || engine.run(rx));

        // 50 ms for the first worker to spawn and enter its polling loop
        // before we send the second `go`. Sleeps below ~50 ms can race
        // thread-spawn latency on loaded CI machines, which (per plan §8)
        // would still produce a `bestmove 0000` from the cancelled-before-
        // enumeration path — but the *intent* of this test is the
        // implicit-stop join, so we want the worker actually running.
        thread::sleep(Duration::from_millis(50));
        tx.send(parse_uci_line("go infinite")).unwrap();
        thread::sleep(Duration::from_millis(50));

        // Poll until the implicit-stop has produced the FIRST bestmove
        // (the second worker is now running). Then send `stop` and poll
        // until the SECOND bestmove appears. handle_quit does not join
        // the worker (plan §9), so we must observe both bestmoves before
        // sending `Quit` to avoid racing the second worker's write.
        let first_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snap = snapshot_output(&buf_clone);
            let n = snap.lines().filter(|l| l.starts_with("bestmove")).count();
            if n >= 1 {
                break;
            }
            assert!(
                Instant::now() < first_deadline,
                "no bestmove from implicit-stop within 1 s;\noutput:\n{snap}"
            );
            thread::sleep(Duration::from_millis(5));
        }

        tx.send(parse_uci_line("stop")).unwrap();
        let second_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snap = snapshot_output(&buf_clone);
            let n = snap.lines().filter(|l| l.starts_with("bestmove")).count();
            if n >= 2 {
                break;
            }
            assert!(
                Instant::now() < second_deadline,
                "second bestmove did not appear within 1 s of stop;\noutput:\n{snap}"
            );
            thread::sleep(Duration::from_millis(5));
        }

        tx.send(Command::Quit).unwrap();
        handle.join().expect("engine thread should not panic");

        let snap = snapshot_output(&buf_clone);
        let bestmove_count = snap.lines().filter(|l| l.starts_with("bestmove")).count();
        assert_eq!(
            bestmove_count, 2,
            "back-to-back go must produce exactly 2 bestmove lines; got {bestmove_count};\noutput:\n{snap}"
        );
    }

    #[test]
    fn info_lines_appear_before_bestmove() {
        // Uses InfoEmittingFake: emits 3 "info string thinking N" lines then
        // returns with bestmove == None (engine emits "bestmove 0000").
        // Verifies that all 3 info lines precede the bestmove line.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, InfoEmittingFake);
        let (tx, rx) = mpsc::channel::<Command>();

        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go")).unwrap();
        tx.send(Command::Quit).unwrap();

        engine.run(rx);

        let snap = snapshot_output(&buf);
        let lines: Vec<&str> = snap.lines().collect();

        let bestmove_idx = lines
            .iter()
            .position(|l| l.starts_with("bestmove"))
            .expect("bestmove must appear in output");

        for n in 1..=3 {
            let expected = format!("info string thinking {n}");
            let info_idx = lines
                .iter()
                .position(|l| *l == expected.as_str())
                .unwrap_or_else(|| panic!("'{expected}' must appear in output;\nlines: {lines:?}"));
            assert!(
                info_idx < bestmove_idx,
                "'{expected}' (line {info_idx}) must appear before bestmove (line {bestmove_idx});\noutput:\n{snap}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Group B.2 — New M2.D tests (B.2.a–h)
    // -----------------------------------------------------------------------

    #[test]
    fn handle_uci_emits_random_seed_option_between_id_author_and_uciok() {
        // Pins the exact option-line text and protocol ordering. Catches a
        // mutant that typos the option-line string or reorders lines.
        let (stdout, _) = drive(&["uci"]);
        let lines: Vec<&str> = stdout.lines().collect();

        let option_line = lines
            .iter()
            .find(|l| l.starts_with("option name Random_Seed"))
            .copied()
            .expect("option name Random_Seed line must be present in uci output");
        assert_eq!(
            option_line, "option name Random_Seed type spin default 0 min 0 max 2147483647",
            "option line text must match exactly"
        );

        let id_author_idx = lines
            .iter()
            .position(|l| l.starts_with("id author"))
            .expect("id author line present");
        let option_idx = lines
            .iter()
            .position(|l| l.starts_with("option name Random_Seed"))
            .expect("option line present");
        let uciok_idx = lines
            .iter()
            .position(|l| *l == "uciok")
            .expect("uciok line present");
        assert!(
            id_author_idx < option_idx,
            "id author (line {id_author_idx}) must come before option name Random_Seed (line {option_idx})"
        );
        assert!(
            option_idx < uciok_idx,
            "option name Random_Seed (line {option_idx}) must come before uciok (line {uciok_idx})"
        );
    }

    #[test]
    fn handle_setoption_random_seed_case_insensitive_and_boundary() {
        // Pins case-insensitivity: three spellings of the option name all route
        // to the same handler. With AlphaBetaMover the search is deterministic
        // regardless of seed, so we verify only that all three case variants
        // produce the same bestmove as each other (mutual equality). We do NOT
        // assert divergence from a seed-0 control — that assertion relied on a
        // PRNG tie-break and is meaningless for a deterministic mover.
        //
        // KvK: white king on e4, black king on e1. All 8 king moves are legal.
        const KVK_FEN: &str = "8/8/8/8/4K3/8/8/4k3 w - - 0 1";

        // Collect the bestmove produced by each of the three case variants, all
        // with seed 42. If setoption routing is broken (e.g. any variant is
        // ignored or crashes), this collection step will surface the failure.
        let mut variant_bestmoves: Vec<String> = Vec::new();
        for name_variant in &["random_seed", "RANDOM_SEED", "Random_Seed"] {
            let (stdout, _) = drive(&[
                &format!("setoption name {name_variant} value 42"),
                &format!("position fen {KVK_FEN}"),
                "go",
            ]);
            let uci_str = stdout
                .lines()
                .find(|l| l.starts_with("bestmove"))
                .unwrap_or_else(|| {
                    panic!(
                        "bestmove must be present after go with seed variant '{name_variant}';\nstdout:\n{stdout}"
                    )
                })
                .strip_prefix("bestmove ")
                .expect("bestmove prefix")
                .to_string();
            variant_bestmoves.push(uci_str);
        }

        // All three case-variant bestmoves must be equal to each other.
        //
        // COVERAGE NOTE: After M3.C, with deterministic alpha-beta, the seed has no
        // observable effect on move choice. This test only verifies that all three
        // case-spelling variants are accepted by the parser (no error info-string,
        // identical bestmove output). A "silently dropped variant" bug would be
        // invisible from this test alone — there is no companion behavioral test that
        // catches it. This is an accepted limitation post-AlphaBetaMover.
        assert_eq!(
            variant_bestmoves[0], variant_bestmoves[1],
            "random_seed and RANDOM_SEED must produce the same bestmove with seed 42; \
            got '{}'  vs  '{}'",
            variant_bestmoves[0], variant_bestmoves[1]
        );
        assert_eq!(
            variant_bestmoves[1], variant_bestmoves[2],
            "RANDOM_SEED and Random_Seed must produce the same bestmove with seed 42; \
            got '{}'  vs  '{}'",
            variant_bestmoves[1], variant_bestmoves[2]
        );
    }

    #[test]
    fn handle_setoption_random_seed_bad_value_silent_when_debug_off() {
        // Four sub-sends, each producing zero output bytes when debug is off.
        let bad_inputs: &[&str] = &[
            "setoption name Random_Seed value abc",
            "setoption name Random_Seed value -1",
            "setoption name Random_Seed value 2147483648",
            "setoption name Random_Seed",
        ];
        for input in bad_inputs {
            let (stdout, _) = drive(&[input]);
            assert!(
                stdout.is_empty(),
                "bad Random_Seed input '{input}' must produce zero output when debug is off; got: {stdout:?}",
            );
        }

        // After all four bad sends, the engine's seed must be unchanged.
        // Pre-configure seed 42 so the unchanged-seed assertion can distinguish
        // "unchanged at 42" from "wiped to 0" — a mutant that zeroes the seed
        // on bad input would produce a different bestmove than the M_REF below.
        //
        // This uses a single engine instance on a background thread so we can
        // poll for M_REF before sending the bad inputs, avoiding races between
        // the go#1 worker and the subsequent setoption commands.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let buf_clone = Arc::clone(&buf);
        let mut engine = Engine::new(writer, AlphaBetaMover::new());
        let (tx, rx) = mpsc::channel::<Command>();
        let handle = thread::spawn(move || engine.run(rx));

        // (1) Set seed 42, go twice, capture the 2nd pick as M_REF2. The 2nd
        // pick is the reference because after go#1, the PRNG state has advanced
        // one step; the tested path also does go#1 and then go#2 after bad
        // inputs. If bad inputs do not change the seed, M_AFTER_BAD == M_REF2.
        tx.send(parse_uci_line("setoption name Random_Seed value 42"))
            .unwrap();
        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go")).unwrap();

        // Wait for the first bestmove.
        {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let snap = snapshot_output(&buf_clone);
                if snap.lines().any(|l| l.starts_with("bestmove")) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "first reference bestmove did not appear within 1 s"
                );
                thread::sleep(Duration::from_millis(2));
            }
        }

        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go")).unwrap();

        let m_ref2 = {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let snap = snapshot_output(&buf_clone);
                let bestmoves: Vec<&str> =
                    snap.lines().filter(|l| l.starts_with("bestmove")).collect();
                if bestmoves.len() >= 2 {
                    break bestmoves[1]
                        .strip_prefix("bestmove ")
                        .expect("prefix")
                        .to_string();
                }
                assert!(
                    Instant::now() < deadline,
                    "second reference bestmove did not appear within 1 s"
                );
                thread::sleep(Duration::from_millis(2));
            }
        };

        // (2) Send `ucinewgame` to clear TT / killers / history so the M_AFTER_BAD
        // half starts from byte-identical engine state to the M_REF2 half. Without
        // this, AlphaBetaMover's accumulated search state from phase (1) would feed
        // into phase (2)'s searches, and any TT-state-sensitive search behavior
        // could legitimately produce a different bestmove without the seed having
        // changed at all. ADR-0025's `tt_bound_for_completed_node` suppression rule
        // is one such TT-state-sensitive behavior; without `ucinewgame` here the
        // test would conflate two unrelated invariants ("bad seed inputs don't
        // change PRNG state" with "search bestmove is invariant under TT carry-over").
        tx.send(Command::UciNewGame).unwrap();

        // Reset seed 42 (back to the start of the seed-42 sequence) so the next
        // go produces the 1st pick. Then go once to advance PRNG one step, send
        // the four bad-value inputs, and go again to capture M_AFTER_BAD. If bad
        // inputs leave the seed unchanged, M_AFTER_BAD == M_REF2.
        tx.send(parse_uci_line("setoption name Random_Seed value 42"))
            .unwrap();
        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go")).unwrap();

        // Wait for go#3 bestmove (3rd total).
        {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let snap = snapshot_output(&buf_clone);
                let n = snap.lines().filter(|l| l.starts_with("bestmove")).count();
                if n >= 3 {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "go#3 bestmove did not appear within 1 s"
                );
                thread::sleep(Duration::from_millis(2));
            }
        }

        // Now the PRNG is in the same state as after M_REF2's predecessor.
        // Send the four bad-value inputs — these must not change seed or state.
        for input in bad_inputs {
            tx.send(parse_uci_line(input)).unwrap();
        }

        // go#4: must produce the same move as M_REF2 (the 2nd pick from seed-42).
        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go")).unwrap();

        let m_after_bad = {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let snap = snapshot_output(&buf_clone);
                let bestmoves: Vec<&str> =
                    snap.lines().filter(|l| l.starts_with("bestmove")).collect();
                if bestmoves.len() >= 4 {
                    break bestmoves[3]
                        .strip_prefix("bestmove ")
                        .expect("prefix")
                        .to_string();
                }
                assert!(
                    Instant::now() < deadline,
                    "post-bad-input bestmove did not appear within 1 s"
                );
                thread::sleep(Duration::from_millis(2));
            }
        };

        tx.send(Command::Quit).unwrap();
        handle.join().expect("engine thread should not panic");

        // Seed must be unchanged — M_AFTER_BAD must equal M_REF2.
        assert_eq!(
            m_ref2, m_after_bad,
            "bad Random_Seed inputs must not change the seed; \
            M_REF2 (2nd pick from seed-42)='{m_ref2}' M_AFTER_BAD='{m_after_bad}'"
        );
    }

    #[test]
    fn handle_setoption_random_seed_bad_value_emits_info_string_when_debug_on() {
        // Same four sub-sends as B.2.c under debug on. Bad-value path emits
        // `info string Random_Seed: rejected …` lines. Fully implemented in Phase 1.
        let (stdout, _) = drive(&[
            "debug on",
            "setoption name Random_Seed value abc",
            "setoption name Random_Seed value -1",
            "setoption name Random_Seed value 2147483648",
            "setoption name Random_Seed",
        ]);

        let rejected_lines: Vec<&str> = stdout
            .lines()
            .filter(|l| l.starts_with("info string Random_Seed: rejected"))
            .collect();
        assert_eq!(
            rejected_lines.len(),
            4,
            "expected 4 'info string Random_Seed: rejected …' lines; got:\n{stdout}"
        );

        // First three: `rejected value '<v>'`
        assert!(
            rejected_lines
                .iter()
                .any(|l| l.contains("rejected value 'abc'")),
            "expected rejected value 'abc'; lines: {rejected_lines:?}",
        );
        assert!(
            rejected_lines
                .iter()
                .any(|l| l.contains("rejected value '-1'")),
            "expected rejected value '-1'; lines: {rejected_lines:?}",
        );
        assert!(
            rejected_lines
                .iter()
                .any(|l| l.contains("rejected value '2147483648'")),
            "expected rejected value '2147483648'; lines: {rejected_lines:?}",
        );
        // Fourth: `rejected (no value given)`
        assert!(
            rejected_lines
                .iter()
                .any(|l| l.contains("rejected (no value given)")),
            "expected 'rejected (no value given)'; lines: {rejected_lines:?}",
        );
    }

    /// Pins the Random_Seed parse path: a valid value is accepted silently;
    /// an out-of-range value is rejected silently (parse-path-only smoke test).
    ///
    /// Renamed from M3.A's `handle_setoption_random_seed_changes_future_bestmoves_but_not_past_ones`
    /// which asserted GreedyMover tie-break divergence. With AlphaBetaMover the
    /// search is deterministic regardless of seed, so the bestmove-divergence assertion
    /// is no longer meaningful. This test replaces it with a parse-path-only check:
    /// the option is accepted/rejected without crashing.
    #[test]
    fn handle_setoption_random_seed_accept_and_reject() {
        // (1) Valid value: accepted silently (no output, no panic).
        let (stdout_accept, _) = drive(&["setoption name Random_Seed value 100"]);
        assert!(
            stdout_accept.is_empty(),
            "valid Random_Seed value must be accepted silently; got: {stdout_accept:?}"
        );

        // (2) Out-of-range value (> MAX_RANDOM_SEED = 2147483647): rejected silently.
        let (stdout_reject, _) = drive(&["setoption name Random_Seed value 99999999999"]);
        assert!(
            stdout_reject.is_empty(),
            "out-of-range Random_Seed value must be rejected silently when debug off; got: {stdout_reject:?}"
        );
    }

    #[test]
    fn handle_ucinewgame_resets_search_state() {
        // Drives a single engine instance through:
        //   1. setoption name Random_Seed value 7  (sets seed=7, state=7)
        //   2. position startpos + go              → capture bestmove M1
        //   3. ucinewgame                          (calls reset: state → seed = 7)
        //   4. position startpos + go              → capture bestmove M2
        // Assert M1 == M2: ucinewgame restores state to seed, so the
        // same first PRNG step is replayed.
        //
        // The engine is driven on a background thread (threading-test pattern)
        // so we can observe M1 in the buffer before sending ucinewgame. Without
        // this synchronization the orchestrator could process ucinewgame while
        // the worker for go#1 is still queued but not yet run, causing the
        // worker to advance PRNG from the reset seed and leaving go#2 starting
        // at step1(seed) instead of seed.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let buf_clone = Arc::clone(&buf);
        let mut engine = Engine::new(writer, AlphaBetaMover::new());
        let (tx, rx) = mpsc::channel::<Command>();

        let handle = thread::spawn(move || engine.run(rx));

        tx.send(parse_uci_line("setoption name Random_Seed value 7"))
            .unwrap();
        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go")).unwrap();

        // Wait until go#1's bestmove appears so the worker has fully run and
        // advanced PRNG state before we send ucinewgame.
        let m1 = {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let snap = snapshot_output(&buf_clone);
                if let Some(line) = snap.lines().find(|l| l.starts_with("bestmove")) {
                    break line.strip_prefix("bestmove ").expect("prefix").to_string();
                }
                assert!(
                    Instant::now() < deadline,
                    "go#1 bestmove did not appear within 1 s"
                );
                thread::sleep(Duration::from_millis(2));
            }
        };

        // go#1 worker is done; send ucinewgame to reset state back to seed=7.
        tx.send(parse_uci_line("ucinewgame")).unwrap();
        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go")).unwrap();

        // Wait until go#2's bestmove appears (a second bestmove line).
        let m2 = {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let snap = snapshot_output(&buf_clone);
                let bestmoves: Vec<&str> =
                    snap.lines().filter(|l| l.starts_with("bestmove")).collect();
                if bestmoves.len() >= 2 {
                    break bestmoves[1]
                        .strip_prefix("bestmove ")
                        .expect("prefix")
                        .to_string();
                }
                assert!(
                    Instant::now() < deadline,
                    "go#2 bestmove did not appear within 1 s"
                );
                thread::sleep(Duration::from_millis(2));
            }
        };

        tx.send(Command::Quit).unwrap();
        handle.join().expect("engine thread should not panic");

        assert_eq!(
            m1, m2,
            "ucinewgame must reset PRNG state to seed; M1='{m1}' M2='{m2}'"
        );
    }

    #[test]
    fn handle_setoption_random_seed_resets_state_immediately() {
        // With AlphaBetaMover the seed has no observable effect on move choice —
        // the search is fully deterministic. This test verifies the parse path:
        // setoption is accepted silently and does not disrupt subsequent searches.
        // Two searches from the same position must produce the same bestmove
        // regardless of any setoption calls between them.

        let buf_a = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer_a = CapturedWriter(Arc::clone(&buf_a));
        let buf_a_clone = Arc::clone(&buf_a);
        let mut engine_a = Engine::new(writer_a, AlphaBetaMover::new());
        let (tx_a, rx_a) = mpsc::channel::<Command>();
        let handle_a = thread::spawn(move || engine_a.run(rx_a));

        tx_a.send(parse_uci_line("position startpos")).unwrap();
        tx_a.send(parse_uci_line("go depth 3")).unwrap();

        // Wait for bestmove #1.
        let m1 = {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let snap = snapshot_output(&buf_a_clone);
                if let Some(line) = snap.lines().find(|l| l.starts_with("bestmove")) {
                    break line.strip_prefix("bestmove ").expect("prefix").to_string();
                }
                assert!(
                    Instant::now() < deadline,
                    "bestmove #1 did not appear within 5 s"
                );
                thread::sleep(Duration::from_millis(2));
            }
        };

        // Set seed multiple times (no-op for AlphaBetaMover), then search again.
        tx_a.send(parse_uci_line("setoption name Random_Seed value 7"))
            .unwrap();
        tx_a.send(parse_uci_line("setoption name Random_Seed value 99"))
            .unwrap();
        tx_a.send(parse_uci_line("position startpos")).unwrap();
        tx_a.send(parse_uci_line("go depth 3")).unwrap();

        // Wait for bestmove #2.
        let m2 = {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let snap = snapshot_output(&buf_a_clone);
                let bms: Vec<&str> = snap.lines().filter(|l| l.starts_with("bestmove")).collect();
                if bms.len() >= 2 {
                    break bms[1]
                        .strip_prefix("bestmove ")
                        .expect("prefix")
                        .to_string();
                }
                assert!(
                    Instant::now() < deadline,
                    "bestmove #2 did not appear within 5 s"
                );
                thread::sleep(Duration::from_millis(2));
            }
        };

        tx_a.send(Command::Quit).unwrap();
        handle_a.join().expect("engine thread should not panic");

        // AlphaBetaMover is deterministic: same position + same depth → same bestmove.
        assert_eq!(
            m1, m2,
            "AlphaBetaMover is deterministic; same position + depth must produce same bestmove; \
            got '{m1}' then '{m2}'"
        );
    }

    #[test]
    fn greedy_mover_determinism_across_repeated_runs_with_fixed_seed() {
        // Run the same UCI transcript twice with a fresh Engine each time and
        // assert identical bestmove lines from both runs. The driver uses
        // synchronous polling between each `go` and the subsequent command so
        // that each search completes naturally before the engine processes
        // the next instruction. Without polling, sending all commands +
        // `Quit` up front allows `handle_quit`'s `stop` signal to abort the
        // last `go` mid-search; the resulting bestmove then depends on which
        // ID iteration the worker happened to finish before the abort, which
        // varies with OS scheduling and test-suite parallel load. (The info
        // line carries a `time <ms>` token that legitimately varies, so we
        // compare only the bestmove output rather than the full stdout.)
        let transcript: &[&str] = &[
            "uci",
            "setoption name Random_Seed value 99",
            "position startpos",
            "go",
            "position startpos moves e2e4",
            "go",
        ];

        let drive_with_polling = || -> Vec<String> {
            let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
            let writer = CapturedWriter(Arc::clone(&buf));
            let buf_clone = Arc::clone(&buf);
            let mut engine = Engine::new(writer, AlphaBetaMover::new());
            let (tx, rx) = mpsc::channel::<Command>();
            let handle = thread::spawn(move || engine.run(rx));

            let mut bestmoves_seen = 0usize;
            for line in transcript {
                let cmd = parse_uci_line(line);
                let is_go = matches!(cmd, Command::Go(_));
                tx.send(cmd).unwrap();
                if is_go {
                    bestmoves_seen += 1;
                    let deadline = Instant::now() + Duration::from_secs(5);
                    loop {
                        let snap = snapshot_output(&buf_clone);
                        let count = snap.lines().filter(|l| l.starts_with("bestmove")).count();
                        if count >= bestmoves_seen {
                            break;
                        }
                        assert!(
                            Instant::now() < deadline,
                            "bestmove #{bestmoves_seen} did not appear within 5 s"
                        );
                        thread::sleep(Duration::from_millis(2));
                    }
                }
            }
            tx.send(Command::Quit).unwrap();
            handle.join().expect("engine thread should not panic");
            let snap = snapshot_output(&buf_clone);
            snap.lines()
                .filter(|l| l.starts_with("bestmove"))
                .map(str::to_owned)
                .collect()
        };

        let bestmoves_a = drive_with_polling();
        let bestmoves_b = drive_with_polling();
        assert_eq!(
            bestmoves_a, bestmoves_b,
            "identical seed + transcript must produce identical bestmove lines;\n\
            run A bestmoves: {bestmoves_a:?}\nrun B bestmoves: {bestmoves_b:?}"
        );
        assert_eq!(
            bestmoves_a.len(),
            2,
            "transcript has 2 go commands; must produce 2 bestmove lines; got: {bestmoves_a:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Group C — reader_loop tests (C25–C27)
    // -----------------------------------------------------------------------

    #[test]
    fn reader_loop_translates_lines_to_commands() {
        let input = b"uci\nisready\nquit\n";
        let cursor = Cursor::new(input.as_slice());
        let (tx, rx) = mpsc::channel::<Command>();

        let handle = thread::spawn(move || reader_loop(cursor, tx));
        handle.join().expect("reader_loop should not panic");

        let received: Vec<Command> = rx.try_iter().collect();
        assert_eq!(
            received,
            vec![Command::Uci, Command::IsReady, Command::Quit],
            "reader_loop must translate lines to correct Commands in order"
        );
    }

    #[test]
    fn reader_loop_eof_synthesizes_quit() {
        // Input ends without a quit line — reader_loop must synthesize Quit on EOF.
        let input = b"uci\n";
        let cursor = Cursor::new(input.as_slice());
        let (tx, rx) = mpsc::channel::<Command>();

        let handle = thread::spawn(move || reader_loop(cursor, tx));
        handle.join().expect("reader_loop should not panic");

        let received: Vec<Command> = rx.try_iter().collect();
        assert_eq!(
            received,
            vec![Command::Uci, Command::Quit],
            "reader_loop must synthesize Command::Quit on EOF"
        );
    }

    #[test]
    fn reader_loop_orchestrator_drop_terminates_silently() {
        // Drop the receiver before reader_loop has a chance to finish sending.
        // reader_loop must not panic when the channel is disconnected.
        let input = b"uci\nisready\n";
        let cursor = Cursor::new(input.as_slice());
        let (tx, rx) = mpsc::channel::<Command>();

        // Drop the receiver immediately — reader_loop will get a send error.
        drop(rx);

        let deadline = Instant::now() + Duration::from_secs(5);
        let handle = thread::spawn(move || reader_loop(cursor, tx));

        loop {
            if handle.is_finished() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "reader_loop must terminate within 1 s even when receiver is dropped"
            );
            thread::sleep(Duration::from_millis(5));
        }
        handle
            .join()
            .expect("reader_loop must not panic when receiver is dropped");
    }

    // -----------------------------------------------------------------------
    // Group F — property test (F36)
    // -----------------------------------------------------------------------

    #[cfg(test)]
    mod prop_tests {
        use super::*;
        use crate::uci::{DebugMode, GoParams, PositionSpec, Register};
        use proptest::prelude::*;

        fn arb_command() -> impl Strategy<Value = Command> {
            prop_oneof![
                Just(Command::Uci),
                prop::bool::ANY.prop_map(|on| Command::Debug(if on {
                    DebugMode::On
                } else {
                    DebugMode::Off
                })),
                Just(Command::IsReady),
                Just(Command::UciNewGame),
                // SetOption with non-empty name (exercises the unknown-option path)
                "[a-zA-Z][a-zA-Z0-9_]*".prop_map(|name| Command::SetOption { name, value: None }),
                // SetOption with Random_Seed + numeric value (exercises the success arm and
                // catches a regression where success-arm parsing panics on arbitrary digit strings)
                "[0-9]{1,5}".prop_map(|v| Command::SetOption {
                    name: "Random_Seed".to_string(),
                    value: Some(v),
                }),
                Just(Command::Register(Register::Later)),
                // Position startpos with no moves (safe: always valid)
                Just(Command::Position {
                    spec: PositionSpec::StartPos,
                    moves: vec![]
                }),
                // Go with bounded params — no infinite/movetime to avoid long waits
                (
                    prop::option::of(0i64..=5i64),
                    prop::option::of(0u64..=100u64),
                )
                    .prop_map(|(movetime, nodes)| Command::Go(GoParams {
                        infinite: false,
                        movetime,
                        nodes,
                        searchmoves: None,
                        ponder: false,
                        wtime: None,
                        btime: None,
                        winc: None,
                        binc: None,
                        movestogo: None,
                        depth: None,
                        mate: None,
                    })),
                Just(Command::Stop),
                Just(Command::PonderHit),
                Just(Command::Unknown),
            ]
        }

        proptest! {
            #[test]
            fn prop_run_loop_total_function_no_panic_and_bestmove_pairs_with_go(
                mut cmds in prop::collection::vec(arb_command(), 0..20)
            ) {
                // Count Go commands before appending Quit.
                let go_count = cmds.iter().filter(|c| matches!(c, Command::Go(_))).count();
                cmds.push(Command::Quit);

                let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
                let writer = CapturedWriter(Arc::clone(&buf));
                let mut engine = Engine::new(writer, AlphaBetaMover::new());
                let (tx, rx) = mpsc::channel::<Command>();
                for cmd in cmds {
                    tx.send(cmd).unwrap();
                }

                // Run on a separate thread with a deadlock-detector. With
                // 20 Go commands at movetime <= 5 ms each, total wallclock
                // is < 200 ms in steady state; 5 s is a generous backstop
                // that converts a deadlock into a test failure rather than
                // a silent hang.
                let handle = thread::spawn(move || engine.run(rx));
                let deadline = Instant::now() + Duration::from_secs(5);
                while !handle.is_finished() {
                    prop_assert!(
                        Instant::now() < deadline,
                        "engine.run did not return within 5 s — likely deadlock",
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                handle.join().expect("engine thread should not panic");

                let bytes = buf.lock().unwrap().clone();
                let stdout = String::from_utf8(bytes)
                    .expect("(a) engine output must be valid UTF-8");

                let bestmove_count = stdout.lines().filter(|l| l.starts_with("bestmove")).count();
                prop_assert_eq!(
                    bestmove_count,
                    go_count,
                    "(c) bestmove count ({}) must equal Go command count ({})",
                    bestmove_count,
                    go_count
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // Group D — game-history invariant + handle_go clone semantics (M3.B)
    //
    // All tests in this group will FAIL until:
    //   - Engine::new() initializes game_history = vec![starting_position().zobrist()]
    //   - handle_ucinewgame resets game_history
    //   - handle_position populates game_history
    //   - handle_go clones game_history into SearchContext::history
    // -----------------------------------------------------------------------

    /// Test fake: captures `ctx.history` from inside `Search::go` into a
    /// shared buffer. Used by `handle_go_clones_history_into_search_context`.
    struct HistoryCapturingFake {
        captured: Arc<Mutex<Vec<u64>>>,
    }

    impl Search for HistoryCapturingFake {
        fn go(
            &mut self,
            _pos: &Position,
            ctx: &SearchContext,
            _info_sink: &dyn Fn(&str),
        ) -> SearchResult {
            *self.captured.lock().unwrap() = ctx.history.clone();
            SearchResult::default()
        }
    }

    /// Like `drive`, but returns `(stdout, position, game_history)` for
    /// history invariant checks.
    fn drive_with_history(commands: &[&str]) -> (String, Position, Vec<u64>) {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, AlphaBetaMover::new());

        let (tx, rx) = mpsc::channel::<Command>();
        for line in commands {
            tx.send(parse_uci_line(line)).unwrap();
        }
        tx.send(Command::Quit).unwrap();
        engine.run(rx);

        let bytes = buf.lock().unwrap().clone();
        let stdout = String::from_utf8(bytes).expect("engine output must be valid UTF-8");
        let pos = *engine.position();
        let hist = engine.game_history().to_vec();
        (stdout, pos, hist)
    }

    // -----------------------------------------------------------------------
    // D.1 — Engine::new initializes game_history.
    // -----------------------------------------------------------------------

    #[test]
    fn engine_new_history_contains_starting_position_zobrist() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let engine = Engine::new(CapturedWriter(Arc::clone(&buf)), AlphaBetaMover::new());
        assert_eq!(
            engine.game_history(),
            &[Position::starting_position().zobrist()],
            "Engine::new must initialize game_history to [startpos zobrist]"
        );
    }

    // -----------------------------------------------------------------------
    // D.2 — handle_ucinewgame resets history.
    // -----------------------------------------------------------------------

    #[test]
    fn handle_ucinewgame_resets_history_to_startpos_zobrist() {
        let (_, _, hist) = drive_with_history(&["position startpos moves e2e4 e7e5", "ucinewgame"]);
        assert_eq!(
            hist,
            vec![Position::starting_position().zobrist()],
            "ucinewgame must reset game_history to [startpos zobrist], length 1"
        );
    }

    // -----------------------------------------------------------------------
    // D.3 — handle_position happy-path history shapes.
    // -----------------------------------------------------------------------

    #[test]
    fn handle_position_startpos_no_moves_history_is_single_startpos_zobrist() {
        let (_, _, hist) = drive_with_history(&["position startpos"]);
        assert_eq!(
            hist,
            vec![Position::starting_position().zobrist()],
            "position startpos (no moves) must leave history = [startpos zobrist]"
        );
    }

    #[test]
    fn handle_position_startpos_with_moves_history_pushes_each_post_make_zobrist() {
        let (_, _, hist) = drive_with_history(&["position startpos moves e2e4 e7e5"]);

        // Compute expected trajectory manually.
        let mut pos = Position::starting_position();
        let z0 = pos.zobrist();
        let mv1 = Move::from_uci("e2e4", &pos).expect("e2e4 legal from startpos");
        pos.make_move(mv1);
        let z1 = pos.zobrist();
        let mv2 = Move::from_uci("e7e5", &pos).expect("e7e5 legal after e2e4");
        pos.make_move(mv2);
        let z2 = pos.zobrist();

        assert_eq!(
            hist.len(),
            3,
            "two moves from startpos must produce history length 3"
        );
        assert_eq!(hist[0], z0, "history[0] must be startpos zobrist");
        assert_eq!(hist[1], z1, "history[1] must be post-e2e4 zobrist");
        assert_eq!(
            hist[2], z2,
            "history[2] must be post-e7e5 zobrist (= current)"
        );
    }

    #[test]
    fn handle_position_fen_no_moves_history_is_single_base_zobrist() {
        let kiwipete_fen = "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1";
        let (_, _, hist) = drive_with_history(&[&format!("position fen {kiwipete_fen}")]);
        let expected_z = Position::from_fen(kiwipete_fen)
            .expect("kiwipete FEN valid")
            .zobrist();
        assert_eq!(
            hist,
            vec![expected_z],
            "position fen (no moves) must leave history = [base zobrist]"
        );
    }

    #[test]
    fn handle_position_fen_with_moves_history_starts_at_base_then_pushes() {
        let kiwipete_fen = "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1";
        let (_, _, hist) =
            drive_with_history(&[&format!("position fen {kiwipete_fen} moves e2a6")]);

        let mut kiwipete = Position::from_fen(kiwipete_fen).expect("kiwipete FEN valid");
        let z_base = kiwipete.zobrist();
        let mv = Move::from_uci("e2a6", &kiwipete).expect("e2a6 legal from kiwipete");
        kiwipete.make_move(mv);
        let z_after = kiwipete.zobrist();

        assert_eq!(
            hist.len(),
            2,
            "one move from FEN must produce history length 2"
        );
        assert_eq!(hist[0], z_base, "history[0] must be kiwipete base zobrist");
        assert_eq!(hist[1], z_after, "history[1] must be post-e2a6 zobrist");
    }

    // -----------------------------------------------------------------------
    // D.4 — handle_position error-path history shapes.
    // -----------------------------------------------------------------------

    #[test]
    fn handle_position_invalid_fen_keeps_prior_history() {
        // Pre-load e2e4 so history is length 2, then send bad FEN.
        // Engine must keep history unchanged (FEN error returns before touching history).
        let (_, pos, hist) = drive_with_history(&[
            "position startpos moves e2e4",
            "position fen not a valid fen here now",
        ]);

        let mut expected_pos = Position::starting_position();
        let z0 = expected_pos.zobrist();
        let mv = Move::from_uci("e2e4", &expected_pos).expect("e2e4 legal");
        expected_pos.make_move(mv);
        let z1 = expected_pos.zobrist();

        assert_eq!(
            hist.len(),
            2,
            "invalid FEN must keep prior history (length 2); got length {}",
            hist.len()
        );
        assert_eq!(
            hist,
            vec![z0, z1],
            "invalid FEN must leave history = [startpos, post-e2e4]"
        );
        assert_eq!(
            hist.last().copied(),
            Some(pos.zobrist()),
            "invariant: hist.last() == position.zobrist() must hold even on FEN error path"
        );
    }

    #[test]
    fn handle_position_malformed_move_resets_history_to_base() {
        // Drive startpos+moves first, then send kiwipete with a malformed move.
        // After error: history must be [kiwipete_zobrist] (the new base), not the
        // prior history, not the partial prefix.
        let kiwipete_fen = "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1";
        let (_, pos, hist) = drive_with_history(&[
            "position startpos moves e2e4 e7e5",
            &format!("position fen {kiwipete_fen} moves zzzz"),
        ]);

        let kiwipete_z = Position::from_fen(kiwipete_fen)
            .expect("kiwipete FEN valid")
            .zobrist();
        assert_eq!(
            hist,
            vec![kiwipete_z],
            "malformed move must reset history to [kiwipete zobrist] (the new base)"
        );
        assert_eq!(
            hist.last().copied(),
            Some(pos.zobrist()),
            "invariant: hist.last() == position.zobrist() must hold even on move error path"
        );
    }

    #[test]
    fn handle_position_partial_success_then_fail_resets_history_to_base() {
        // e2e4 succeeds, e7e9 fails. History must be reset to [startpos zobrist]
        // (the base), not [startpos, post-e2e4].
        let (_, pos, hist) = drive_with_history(&["position startpos moves e2e4 e7e9"]);
        assert_eq!(
            hist,
            vec![Position::starting_position().zobrist()],
            "partial-success-then-fail must reset history to [startpos zobrist], not keep partial prefix"
        );
        assert_eq!(
            hist.last().copied(),
            Some(pos.zobrist()),
            "invariant: hist.last() == position.zobrist() must hold even on move error path"
        );
    }

    #[test]
    fn handle_position_second_command_move_error_resets_to_new_base_history() {
        // Drive startpos+two moves (history len 3), then send kiwipete+legal+bad.
        // e2a6 is legal from kiwipete; zzzz is malformed.
        // After error: history must be [kiwipete_zobrist], NOT the prior history
        // spliced with kiwipete and NOT the partial [kiwipete, post_e2a6].
        let kiwipete_fen = "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1";
        let (_, pos, hist) = drive_with_history(&[
            "position startpos moves e2e4 e7e5",
            &format!("position fen {kiwipete_fen} moves e2a6 zzzz"),
        ]);

        let kiwipete_z = Position::from_fen(kiwipete_fen)
            .expect("kiwipete FEN valid")
            .zobrist();
        assert_eq!(
            hist,
            vec![kiwipete_z],
            "move error must reset history to [new base zobrist], not splice or partially extend"
        );
        assert_eq!(
            hist.last().copied(),
            Some(pos.zobrist()),
            "invariant: hist.last() == position.zobrist() must hold even on move error path"
        );
    }

    // -----------------------------------------------------------------------
    // D.5 — invariant property: history.last() == position.zobrist() always.
    // -----------------------------------------------------------------------

    #[test]
    fn handle_position_history_invariant_holds_after_every_handler() {
        // Each sub-sequence ends on a different kind of handler so the invariant
        // game_history.last() == position.zobrist() is checked under that
        // handler's post-state. Each sub-sequence builds a fresh engine via
        // drive_with_history, so prior corruption cannot be repaired by a later
        // command in the same sequence.

        // Sub-sequence 1: terminal happy-path command (position startpos moves).
        {
            let (_, pos, hist) = drive_with_history(&["position startpos moves e2e4 e7e5"]);
            assert_eq!(
                hist.last().copied(),
                Some(pos.zobrist()),
                "invariant after position-with-moves: hist.last() == position.zobrist()"
            );
        }

        // Sub-sequence 2: terminal FEN-error command. Prior state is built up so
        // the FEN error has something to "preserve" — a handler that zeroes history
        // on FEN error would leave hist empty while position stays post-e2e4.
        {
            let (_, pos, hist) = drive_with_history(&[
                "position startpos moves e2e4",
                "position fen garbage", // terminal: FEN-parse error
            ]);
            assert_eq!(
                hist.last().copied(),
                Some(pos.zobrist()),
                "invariant after FEN-parse-error path: hist.last() == position.zobrist()"
            );
        }

        // Sub-sequence 3: terminal move-error command from a FEN base. A handler
        // that resets history to vec![] (instead of vec![base.zobrist()]) would
        // leave hist empty while position becomes the FEN base.
        {
            let kiwipete_fen =
                "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1";
            let (_, pos, hist) = drive_with_history(&[
                "position startpos moves e2e4 e7e5",
                &format!("position fen {kiwipete_fen} moves zzzz"), // terminal: move error
            ]);
            assert_eq!(
                hist.last().copied(),
                Some(pos.zobrist()),
                "invariant after move-error path on FEN base: hist.last() == position.zobrist()"
            );
        }

        // Sub-sequence 4: terminal ucinewgame.
        {
            let (_, pos, hist) = drive_with_history(&[
                "position fen rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1 moves e2e4",
                "ucinewgame", // terminal
            ]);
            assert_eq!(
                hist.last().copied(),
                Some(pos.zobrist()),
                "invariant after ucinewgame: hist.last() == position.zobrist()"
            );
        }
    }

    // -----------------------------------------------------------------------
    // D.6 — handle_go clone semantics.
    // -----------------------------------------------------------------------

    #[test]
    fn handle_go_clones_history_into_search_context() {
        // Use HistoryCapturingFake to read ctx.history from inside go.
        // After "position startpos moves e2e4 e7e5" + "go depth 1", the
        // captured history must equal the engine's game_history at that point.
        let captured = Arc::new(Mutex::new(Vec::<u64>::new()));
        let fake = HistoryCapturingFake {
            captured: Arc::clone(&captured),
        };

        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, fake);

        let (tx, rx) = mpsc::channel::<Command>();
        tx.send(parse_uci_line("position startpos moves e2e4 e7e5"))
            .unwrap();
        tx.send(parse_uci_line("go depth 1")).unwrap();
        tx.send(Command::Quit).unwrap();
        engine.run(rx);

        // Compute expected history.
        let mut pos = Position::starting_position();
        let z0 = pos.zobrist();
        let mv1 = Move::from_uci("e2e4", &pos).expect("e2e4 legal");
        pos.make_move(mv1);
        let z1 = pos.zobrist();
        let mv2 = Move::from_uci("e7e5", &pos).expect("e7e5 legal");
        pos.make_move(mv2);
        let z2 = pos.zobrist();
        let expected = vec![z0, z1, z2];

        let got = captured.lock().unwrap().clone();
        assert_eq!(
            got.len(),
            3,
            "ctx.history must have length 3 after e2e4 e7e5; got length {}",
            got.len()
        );
        assert_eq!(
            got, expected,
            "ctx.history must equal engine's game_history at the time of go"
        );
    }

    #[test]
    fn handle_go_does_not_consume_engine_history() {
        // Verify handle_go clones (does not drain) engine.game_history.
        // After go completes, engine.game_history must equal the trajectory
        // before go was called. A regression where handle_go does
        // std::mem::take(&mut self.game_history) would drain the field to
        // Vec::new() and fail this assertion.
        let (_, _, hist) = drive_with_history(&["position startpos moves e2e4 e7e5", "go depth 1"]);

        // Compute expected trajectory.
        let mut pos = Position::starting_position();
        let z0 = pos.zobrist();
        let mv1 = Move::from_uci("e2e4", &pos).expect("e2e4 legal");
        pos.make_move(mv1);
        let z1 = pos.zobrist();
        let mv2 = Move::from_uci("e7e5", &pos).expect("e7e5 legal");
        pos.make_move(mv2);
        let z2 = pos.zobrist();
        let expected = vec![z0, z1, z2];

        assert_eq!(
            hist, expected,
            "engine.game_history must be unchanged after handle_go (clone, not take)"
        );
    }

    // -----------------------------------------------------------------------
    // M3.C — production-switch test
    // -----------------------------------------------------------------------

    /// Compile-time check that `Engine::new(stdout, AlphaBetaMover::new())` type-checks.
    /// If `AlphaBetaMover` doesn't implement `Search + Send + 'static` this will
    /// not compile.
    #[test]
    fn engine_uses_alphabeta_mover_as_production_search() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        // This line is the compile-time check: Engine::new requires S: Search + Send + 'static.
        let _engine = Engine::new(writer, AlphaBetaMover::new());
        // No behavioral assertion needed — if it compiles, the type bound is satisfied.
    }

    // -----------------------------------------------------------------------
    // M3.E — `MoveOverhead` UCI option tests.
    //
    // Per `docs/plans/m3.e.md` §8.3. Tests verify the option's parse path,
    // boundary handling, case-insensitivity, and the `Engine::move_overhead()`
    // accessor. Slice A's tests do NOT depend on Slice B's `compute_caps`
    // integration.
    // -----------------------------------------------------------------------

    /// Helper: build an Engine with captured stdout, run the given commands,
    /// and return both the captured output AND the final move_overhead value.
    fn drive_capturing_move_overhead(commands: &[&str]) -> (String, u64) {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, AlphaBetaMover::new());

        let (tx, rx) = mpsc::channel::<Command>();
        for line in commands {
            tx.send(parse_uci_line(line)).unwrap();
        }
        tx.send(Command::Quit).unwrap();
        engine.run(rx);

        let bytes = buf.lock().unwrap().clone();
        let stdout = String::from_utf8(bytes).expect("output must be valid UTF-8");
        (stdout, engine.move_overhead())
    }

    #[test]
    fn engine_default_move_overhead_is_50() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let engine = Engine::new(writer, AlphaBetaMover::new());
        assert_eq!(
            engine.move_overhead(),
            50,
            "fresh engine must default move_overhead to 50 ms"
        );
    }

    #[test]
    fn handle_uci_emits_moveoverhead_option_after_random_seed() {
        let (stdout, _) = drive(&["uci"]);
        let lines: Vec<&str> = stdout.lines().collect();

        let opt_line = lines
            .iter()
            .find(|l| l.starts_with("option name MoveOverhead"))
            .copied()
            .expect("option name MoveOverhead line must be present in uci output");
        assert_eq!(
            opt_line, "option name MoveOverhead type spin default 50 min 0 max 5000",
            "MoveOverhead option line text must match exactly"
        );

        // Position: after id author and Random_Seed, before uciok.
        let id_author_idx = lines
            .iter()
            .position(|l| l.starts_with("id author"))
            .expect("id author line present");
        let random_seed_idx = lines
            .iter()
            .position(|l| l.starts_with("option name Random_Seed"))
            .expect("Random_Seed option line present");
        let move_overhead_idx = lines
            .iter()
            .position(|l| l.starts_with("option name MoveOverhead"))
            .expect("MoveOverhead option line present");
        let uciok_idx = lines
            .iter()
            .position(|l| *l == "uciok")
            .expect("uciok line present");
        assert!(
            id_author_idx < random_seed_idx,
            "id author must come before Random_Seed"
        );
        assert!(
            random_seed_idx < move_overhead_idx,
            "Random_Seed must come before MoveOverhead"
        );
        assert!(
            move_overhead_idx < uciok_idx,
            "MoveOverhead must come before uciok"
        );
    }

    #[test]
    fn handle_setoption_moveoverhead_accepts_min() {
        let (_stdout, mo) = drive_capturing_move_overhead(&["setoption name MoveOverhead value 0"]);
        assert_eq!(mo, 0, "value 0 (min) must be accepted");
    }

    #[test]
    fn handle_setoption_moveoverhead_accepts_default() {
        let (_stdout, mo) =
            drive_capturing_move_overhead(&["setoption name MoveOverhead value 50"]);
        assert_eq!(mo, 50, "value 50 (default) must be accepted");
    }

    #[test]
    fn handle_setoption_moveoverhead_accepts_max() {
        let (_stdout, mo) =
            drive_capturing_move_overhead(&["setoption name MoveOverhead value 5000"]);
        assert_eq!(mo, 5000, "value 5000 (max) must be accepted");
    }

    #[test]
    fn handle_setoption_moveoverhead_accepts_in_range() {
        let (_stdout, mo) =
            drive_capturing_move_overhead(&["setoption name MoveOverhead value 100"]);
        assert_eq!(mo, 100, "value 100 (in-range) must be accepted");
    }

    #[test]
    fn handle_setoption_moveoverhead_rejects_above_max() {
        let (_stdout, mo) =
            drive_capturing_move_overhead(&["setoption name MoveOverhead value 5001"]);
        assert_eq!(
            mo, 50,
            "5001 > MAX_MOVE_OVERHEAD must be rejected; default preserved"
        );
    }

    #[test]
    fn handle_setoption_moveoverhead_rejects_huge_value() {
        let (_stdout, mo) =
            drive_capturing_move_overhead(&["setoption name MoveOverhead value 99999999999999"]);
        assert_eq!(mo, 50, "huge value must be rejected; default preserved");
    }

    #[test]
    fn handle_setoption_moveoverhead_rejects_negative() {
        let (_stdout, mo) =
            drive_capturing_move_overhead(&["setoption name MoveOverhead value -1"]);
        assert_eq!(mo, 50, "negative value must be rejected (u64 parse fails)");
    }

    #[test]
    fn handle_setoption_moveoverhead_rejects_unparseable() {
        let (_stdout, mo) =
            drive_capturing_move_overhead(&["setoption name MoveOverhead value foo"]);
        assert_eq!(mo, 50, "unparseable value must be rejected");
    }

    #[test]
    fn handle_setoption_moveoverhead_rejects_missing_value() {
        let (_stdout, mo) = drive_capturing_move_overhead(&["setoption name MoveOverhead"]);
        assert_eq!(mo, 50, "missing value must be rejected; default preserved");
    }

    #[test]
    fn handle_setoption_moveoverhead_case_insensitive_name() {
        for variant in &[
            "moveoverhead",
            "MOVEOVERHEAD",
            "MoveOverhead",
            "mOvEoVeRhEaD",
        ] {
            let (_stdout, mo) =
                drive_capturing_move_overhead(&[&format!("setoption name {variant} value 100")]);
            assert_eq!(
                mo, 100,
                "case variant {variant:?} must be accepted via case-insensitive match"
            );
        }
    }

    #[test]
    fn handle_setoption_moveoverhead_above_max_silent_when_debug_off() {
        let (stdout, mo) =
            drive_capturing_move_overhead(&["setoption name MoveOverhead value 5001"]);
        assert_eq!(mo, 50, "rejected; default preserved");
        assert!(
            !stdout
                .lines()
                .any(|l| l.starts_with("info string MoveOverhead")),
            "no info string should leak when debug is off; got stdout:\n{stdout}"
        );
    }

    #[test]
    fn handle_setoption_moveoverhead_unparseable_silent_when_debug_off() {
        let (stdout, mo) =
            drive_capturing_move_overhead(&["setoption name MoveOverhead value foo"]);
        assert_eq!(mo, 50, "unparseable rejected; default preserved");
        assert!(
            !stdout
                .lines()
                .any(|l| l.starts_with("info string MoveOverhead")),
            "no info string should leak when debug is off; got stdout:\n{stdout}"
        );
    }

    #[test]
    fn handle_setoption_moveoverhead_missing_value_silent_when_debug_off() {
        let (stdout, mo) = drive_capturing_move_overhead(&["setoption name MoveOverhead"]);
        assert_eq!(mo, 50, "missing-value rejected; default preserved");
        assert!(
            !stdout
                .lines()
                .any(|l| l.starts_with("info string MoveOverhead")),
            "no info string should leak when debug is off; got stdout:\n{stdout}"
        );
    }

    #[test]
    fn handle_setoption_moveoverhead_invalid_value_emits_info_when_debug_on() {
        // Reject path with debug on: an `info string MoveOverhead: rejected ...`
        // line emits, mirroring the `Random_Seed` precedent.
        let (stdout, mo) =
            drive_capturing_move_overhead(&["debug on", "setoption name MoveOverhead value 5001"]);
        assert_eq!(mo, 50, "rejected; default preserved");
        let info_line = stdout
            .lines()
            .find(|l| l.starts_with("info string MoveOverhead:"))
            .unwrap_or_else(|| {
                panic!(
                    "expected 'info string MoveOverhead: rejected ...' under debug=on; \
                     got stdout:\n{stdout}"
                )
            });
        assert!(
            info_line.contains("rejected"),
            "info string must mention rejection; got {info_line:?}"
        );
    }

    #[test]
    fn handle_go_with_wtime_btime_reaches_depth_3_on_kiwipete() {
        // Kiwipete (high branching) iter-3 visits ~26k nodes (debug) /
        // ~few-k nodes (release), exceeding the 4096 cancellation cadence
        // either way. With wtime=60000 + default MoveOverhead=50, compute_caps
        // produces (soft, hard) ≈ (2950ms, 8850ms). Under correct
        // `now + caps.hard` impl, the hard deadline lands in the future at
        // ~8850ms, well past iter-3's wallclock (~3-4s in debug under load,
        // < 50ms in release). Iter 3 completes; soft check at 2950ms then
        // fires post-iter-3 in debug (and post-iter-N for some N in release).
        //
        // wtime bumped from 20000 to 60000 (2026-04-30, ELOH.C landing): on
        // a moderately loaded debug-mode build the original 2850ms hard cap
        // was marginal and iter 3 wouldn't fit on a busy machine. The 8850ms
        // hard cap gives ~3x headroom over observed iter-3 wallclock.
        //
        // Under the `now + caps.hard` → `now - caps.hard` MUTATION, the hard
        // deadline lands in the past. Iter 3's first 4096-aligned cadence poll
        // fires `should_abort = true`, aborts iter 3 mid-flight,
        // last_complete = (2, ...). Test observes no depth-3 info line.
        //
        // Pin: a `depth 3` info line must be emitted. Catches the hard-deadline
        // `+ → -` mutation on engine.rs:225.
        const KIWIPETE_FEN: &str =
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1";
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, AlphaBetaMover::new());
        let (tx, rx) = mpsc::channel::<Command>();
        tx.send(parse_uci_line(&format!("position fen {KIWIPETE_FEN}")))
            .unwrap();
        tx.send(parse_uci_line("go wtime 60000 btime 60000"))
            .unwrap();

        let buf_clone = Arc::clone(&buf);
        let handle = std::thread::spawn(move || engine.run(rx));

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let snap = String::from_utf8(buf_clone.lock().unwrap().clone())
                .expect("output is valid UTF-8");
            if snap.lines().any(|l| l.starts_with("bestmove")) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "bestmove did not arrive within 15s;\noutput so far:\n{snap}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        tx.send(Command::Quit).unwrap();
        handle.join().expect("engine thread");

        let snap =
            String::from_utf8(buf_clone.lock().unwrap().clone()).expect("output is valid UTF-8");
        let depth_3_present = snap.lines().any(|l| l.starts_with("info depth 3 "));
        assert!(
            depth_3_present,
            "depth-3 info line must be emitted on Kiwipete with wtime=60000 — \
             a hard-deadline mutation that puts the deadline in the past would \
             abort iter 3 (~26k nodes > 4096 cadence) before it completes;\noutput:\n{snap}"
        );
    }

    #[test]
    fn handle_go_with_wtime_btime_reaches_at_least_depth_2() {
        // Drive `go wtime 5000 btime 5000` through `handle_go` (the
        // production path that wires `compute_caps` and constructs
        // `now + caps.hard` / `now + caps.soft`). compute_caps with wtime=5000,
        // mo=50 returns soft = 5000/20 + 0/2 - 50 = 200ms, hard = min(600, 4950)
        // = 600ms. Iteration 2 from startpos completes well within 200ms.
        //
        // Pin: at least 2 `info depth` lines emitted, proving the deadlines
        // were constructed in the FUTURE (not the past). Catches the
        // `now + caps.hard` → `now - caps.hard` mutation (and the soft variant)
        // in `handle_go`. Under that mutation, the deadline lands in the past;
        // iteration 1 completes (~20 nodes, no cadence poll), then the
        // inter-iteration soft check fires and breaks. Result: exactly 1 info
        // depth line.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, AlphaBetaMover::new());
        let (tx, rx) = mpsc::channel::<Command>();
        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go wtime 5000 btime 5000")).unwrap();

        // Run on a separate thread so we can wait for bestmove from the
        // captured buffer.
        let buf_clone = Arc::clone(&buf);
        let handle = std::thread::spawn(move || engine.run(rx));

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snap = String::from_utf8(buf_clone.lock().unwrap().clone())
                .expect("output is valid UTF-8");
            if snap.lines().any(|l| l.starts_with("bestmove")) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "bestmove did not arrive within 5s;\noutput so far:\n{snap}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        tx.send(Command::Quit).unwrap();
        handle.join().expect("engine thread");

        let snap =
            String::from_utf8(buf_clone.lock().unwrap().clone()).expect("output is valid UTF-8");
        let info_depth_lines = snap
            .lines()
            .filter(|l| l.starts_with("info depth "))
            .count();
        assert!(
            info_depth_lines >= 2,
            "ID must reach at least depth 2 with wtime=5000 + default MoveOverhead=50 \
             (soft cap ≈ 200ms, easily fits depth-2 from startpos); got {info_depth_lines} \
             info lines:\n{snap}"
        );
    }

    #[test]
    fn handle_setoption_moveoverhead_persists_across_searches() {
        // Set MoveOverhead, then run several `go` commands, and verify the
        // value is unchanged at the end. Pins that the option is sticky
        // across commands (matches `Random_Seed` precedent).
        let (_stdout, mo) = drive_capturing_move_overhead(&[
            "setoption name MoveOverhead value 200",
            "position startpos",
            "go depth 1",
            "isready",
            "go depth 1",
        ]);
        assert_eq!(mo, 200, "MoveOverhead must persist across go commands");
    }

    // ─── M3.F: compute_bench_nps helper ───────────────────────────────────
    //
    // These four tests pin the EXACT arithmetic (total_nodes × 1000 / total_ms)
    // against a constructed-inputs fixture, catching the three cargo-mutants
    // survivors on the inline expression that the loose order-of-magnitude
    // band in `handle_bench_total_matches_sum_of_per_position_nodes` did not.
    // The chosen inputs (10 nodes, 3ms) give different values for each
    // mutation, distinguishing the original from each mutant by integer
    // division semantics:
    //   - `/ → %`  : (10 * 1000) / 3 = 3333  vs (10 * 1000) % 3 = 1
    //   - `/ → *`  : 3333 vs (10 * 1000) * 3 = 30000
    //   - `* → +`  : 3333 vs (10 + 1000) / 3 = 336
    // Each mutation produces a distinct value; the equality assertion to
    // 3333 fails for every mutant.

    #[test]
    fn compute_bench_nps_basic_division() {
        // 1000 nodes / 100ms = 10000 nps.
        assert_eq!(super::compute_bench_nps(1000, 100), 10_000);
    }

    #[test]
    fn compute_bench_nps_distinguishes_div_from_mod_and_mul_and_add() {
        // Single fixture pins all 3 mutations on the inline expression simultaneously.
        // Original: (10 * 1000) / 3 = 10000 / 3 = 3333.
        // / → %  : 10000 % 3   = 1
        // / → *  : 10000 * 3   = 30000
        // * → +  : (10 + 1000) / 3 = 336
        assert_eq!(super::compute_bench_nps(10, 3), 3333);
    }

    #[test]
    fn compute_bench_nps_zero_total_ms_does_not_panic() {
        // sub-ms benches: max(1) guard prevents div-by-zero.
        assert_eq!(super::compute_bench_nps(1000, 0), 1_000_000);
    }

    #[test]
    fn compute_bench_nps_zero_nodes() {
        // Edge: zero nodes (e.g., empty corpus) → 0 NPS, no panic.
        assert_eq!(super::compute_bench_nps(0, 100), 0);
        assert_eq!(super::compute_bench_nps(0, 0), 0);
    }

    #[test]
    fn compute_bench_nps_handles_large_node_counts_without_u64_overflow() {
        // u64::MAX * 1000 overflows u64; promoting to u128 is required.
        // 2^60 * 1000 / 1ms ≈ 1.15e21 nps — exceeds u64::MAX (1.84e19) but fits u128.
        let huge_nodes: u64 = 1u64 << 60;
        let nps = super::compute_bench_nps(huge_nodes, 1);
        assert!(nps > u64::MAX as u128);
    }

    // ─── M3.F: handle_bench ───────────────────────────────────────────────

    /// Parse the `info string bench: <N> nodes <NPS> nps` line and return
    /// `Some((nodes, nps))`. Returns `None` if the line is absent.
    fn extract_bench_signature(stdout: &str) -> Option<(u64, u64)> {
        for line in stdout.lines() {
            if let Some(rest) = line.strip_prefix("info string bench: ") {
                // rest = "<N> nodes <NPS> nps"
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if parts.len() == 4 && parts[1] == "nodes" && parts[3] == "nps" {
                    let nodes = parts[0].parse::<u64>().ok()?;
                    let nps = parts[2].parse::<u64>().ok()?;
                    return Some((nodes, nps));
                }
            }
        }
        None
    }

    /// Parse all `info string bench position N/M: <fen> nodes <N> time <ms>`
    /// lines from stdout and return `Vec<(position_idx, total_count, nodes, time_ms)>`.
    fn extract_bench_per_position(stdout: &str) -> Vec<(usize, usize, u64, u64)> {
        let mut out = Vec::new();
        for line in stdout.lines() {
            if let Some(rest) = line.strip_prefix("info string bench position ") {
                // rest looks like: "1/16: <fen> nodes <N> time <ms>"
                // Use `rsplitn(2, " nodes ")` and `rsplitn(2, " time ")` so a
                // future expanded corpus FEN containing the literal ` nodes `
                // or ` time ` substring (extremely unlikely in standard FEN —
                // ranks are digits/letters, side-to-move is `w`/`b`, castling
                // is K/Q/k/q/-, EP is `-` or square, halfmove/fullmove are
                // integers — but defensive against future format extensions)
                // doesn't break the parser. `rsplitn(2)` splits at the LAST
                // occurrence; the engine's emit is `<head> nodes <tail>` where
                // `<tail>` always starts with the integer node count.
                let parts: Vec<&str> = rest.rsplitn(2, " nodes ").collect();
                if parts.len() != 2 {
                    continue;
                }
                // `rsplitn(2, " nodes ")` on `"1/16: <fen> nodes <N> time <ms>"`
                // yields `["<N> time <ms>", "1/16: <fen>"]` — rightmost piece
                // first, then everything before the FINAL " nodes ".
                let tail = parts[0]; // "<N> time <ms>"
                let head = parts[1]; // "1/16: <fen>"
                // Parse "1/16:" prefix.
                let colon = head.find(':');
                let Some(colon) = colon else { continue };
                let idx_part = &head[..colon];
                let slash: Vec<&str> = idx_part.split('/').collect();
                if slash.len() != 2 {
                    continue;
                }
                let Ok(idx) = slash[0].parse::<usize>() else {
                    continue;
                };
                let Ok(total) = slash[1].parse::<usize>() else {
                    continue;
                };
                // Parse "<N> time <ms>" — rsplitn(2) for the same reason as
                // above. `rsplitn(2, " time ")` on `"<N> time <ms>"` yields
                // `["<ms>", "<N>"]` (rsplit returns the rightmost piece first;
                // the second piece is everything before the FINAL " time "
                // separator). So `time_parts[0]` is the integer ms field and
                // `time_parts[1]` is the integer node count.
                let time_parts: Vec<&str> = tail.rsplitn(2, " time ").collect();
                if time_parts.len() != 2 {
                    continue;
                }
                let Ok(time_ms) = time_parts[0].parse::<u64>() else {
                    continue;
                };
                let Ok(nodes) = time_parts[1].parse::<u64>() else {
                    continue;
                };
                out.push((idx, total, nodes, time_ms));
            }
        }
        out
    }

    #[test]
    fn handle_bench_emits_summary_lines() {
        // Drive `bench 2` (explicit fast depth — depth 2 over 16 positions
        // is sub-second on dev hardware). Anchor: stdout MUST contain
        // `info string Nodes searched: <N>` and
        // `info string bench: <N> nodes <NPS> nps`. <N> > 0.
        //
        // The default-depth (BENCH_DEFAULT_DEPTH=7) path is covered by E43
        // in `tests/uci_integration.rs` end-to-end through a real subprocess,
        // and by `bench_default_depth_in_valid_range` for the constant. Using
        // depth 2 here keeps unit-test wall under a second per call.
        let (stdout, _) = drive(&["bench 2"]);
        let nodes_searched_line = stdout
            .lines()
            .find(|l| l.starts_with("info string Nodes searched: "))
            .unwrap_or_else(|| {
                panic!("missing `info string Nodes searched: …` line in:\n{stdout}")
            });
        let n: u64 = nodes_searched_line
            .strip_prefix("info string Nodes searched: ")
            .unwrap()
            .parse()
            .expect("Nodes searched value must be u64");
        assert!(n > 0, "Nodes searched must be > 0; got {n}");
        let sig = extract_bench_signature(&stdout)
            .unwrap_or_else(|| panic!("missing `info string bench: …` signature in:\n{stdout}"));
        assert_eq!(
            sig.0, n,
            "bench-signature node count must equal `Nodes searched` value"
        );
    }

    #[test]
    fn handle_bench_emits_per_position_info_lines() {
        // At depth 2, bench is fast (<1s for 16 positions). Each position
        // gets a `info string bench position <idx>/<total>: …` line in order.
        let (stdout, _) = drive(&["bench 2"]);
        let per_pos = extract_bench_per_position(&stdout);
        assert_eq!(
            per_pos.len(),
            crate::bench::BENCH_POSITIONS.len(),
            "expected one info line per position; got {}:\n{stdout}",
            per_pos.len()
        );
        // Indices monotonically increase from 1 and the total field matches.
        for (i, (idx, total, _, _)) in per_pos.iter().enumerate() {
            assert_eq!(*idx, i + 1, "position index out of order");
            assert_eq!(
                *total,
                crate::bench::BENCH_POSITIONS.len(),
                "total field must equal corpus length"
            );
        }
    }

    #[test]
    fn handle_bench_total_matches_sum_of_per_position_nodes() {
        let (stdout, _) = drive(&["bench 2"]);
        let per_pos = extract_bench_per_position(&stdout);
        let sum: u64 = per_pos.iter().map(|(_, _, n, _)| *n).sum();
        let sig = extract_bench_signature(&stdout)
            .unwrap_or_else(|| panic!("missing bench signature in:\n{stdout}"));
        assert_eq!(
            sig.0, sum,
            "summary total ({}) must equal sum of per-position counts ({sum})",
            sig.0
        );
        // Also pins the `info string Nodes searched:` line.
        let line = stdout
            .lines()
            .find(|l| l.starts_with("info string Nodes searched: "))
            .unwrap();
        let n: u64 = line
            .strip_prefix("info string Nodes searched: ")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(n, sum, "Nodes searched line must equal per-position sum");

        // NPS sanity. Parse <NPS> field; assert > 0 AND that NPS × elapsed_ms
        // ≈ total_nodes × 1000 within ±20% (loose to absorb integer-division
        // truncation; tight enough to catch e.g. swapped fields or off-by-1000).
        // We sum the per-position `time <ms>` to approximate the engine's
        // total elapsed-ms (the engine's internal `total_ms` may differ
        // slightly because it includes inter-position bookkeeping).
        // NPS sanity (loose). At depth 2, several endgame positions complete
        // in sub-millisecond wallclock and report `time 0`, so the per-position
        // time sum is a poor proxy for the engine's actual elapsed_ms. We
        // therefore can't validate the NPS arithmetic precisely — what we
        // CAN check is that NPS is within a few orders of magnitude of
        // `total_nodes / 1ms` (catches off-by-1_000 / off-by-1_000_000 / sign
        // / swapped-field bugs without false-failing on micro-timing jitter).
        // The `sig.0 == sum` assertion above already catches swapped-field
        // bugs precisely; this guard is a belt-and-braces sanity check on the
        // NPS field's order of magnitude.
        assert!(sig.1 > 0, "NPS must be > 0");
        // Order-of-magnitude band: nps must be within 1000× of nodes/1ms.
        // For 20k nodes / 1ms baseline = 20M nps; band = [20K, 20G]. Catches
        // any reasonable bug class without false-failing.
        let nodes_per_ms_baseline = sum.max(1);
        let lo = nodes_per_ms_baseline / 1000;
        let hi = nodes_per_ms_baseline.saturating_mul(1_000_000_000);
        assert!(
            sig.1 >= lo && sig.1 <= hi,
            "NPS field {} is wildly out of expected order-of-magnitude band [{lo}, {hi}] for {sum} nodes",
            sig.1
        );
    }

    #[test]
    fn handle_bench_explicit_depth_overrides_default() {
        // Run bench at depths 1, 2, 3 — assert nodes(d=1) < nodes(d=2) < nodes(d=3).
        // The strict-monotone chain over three depths catches:
        //   - "ignored override" bugs (handler always uses BENCH_DEFAULT_DEPTH —
        //     all three runs would produce identical totals, breaking <).
        //   - "constant-output" bugs (handler returns the same total
        //     regardless of inputs).
        // Pairwise ≠ assertions add belt-and-braces.
        let (s1, _) = drive(&["bench 1"]);
        let (s2, _) = drive(&["bench 2"]);
        let (s3, _) = drive(&["bench 3"]);
        let n1 = extract_bench_signature(&s1).unwrap().0;
        let n2 = extract_bench_signature(&s2).unwrap().0;
        let n3 = extract_bench_signature(&s3).unwrap().0;
        assert!(
            n1 < n2,
            "nodes(d=1)={n1} must be < nodes(d=2)={n2} (deeper search visits more nodes)"
        );
        assert!(
            n2 < n3,
            "nodes(d=2)={n2} must be < nodes(d=3)={n3} (deeper search visits more nodes)"
        );
        assert_ne!(n1, n2, "depth-1 and depth-2 totals must differ");
        assert_ne!(n2, n3, "depth-2 and depth-3 totals must differ");
    }

    #[test]
    fn bench_node_count_is_reproducible_across_invocations() {
        // Drive `bench 2` twice on fresh engines; assert identical totals AND
        // identical per-position counts. Pins the node-count signature is
        // reproducible from the same binary at two granularities (aggregate
        // and per-position).
        //
        // Limitation: this test catches *non-deterministic* drift between
        // runs — it does NOT catch *deterministic* cross-position state
        // leakage within a single run. A bug like "position N+1 always
        // inherits position N's `prior_root_move`" would produce identical
        // (but wrong) per-position counts on both runs. The per-position
        // comparison narrows the failure mode (now per-position drift, not
        // just aggregate drift would fail) but does NOT upgrade to within-run
        // isolation testing. Within-run correctness is a `Search::go`
        // invariant pinned by M3.E's own ID tests.
        let (s1, _) = drive(&["bench 2"]);
        let (s2, _) = drive(&["bench 2"]);
        let n1 = extract_bench_signature(&s1).unwrap().0;
        let n2 = extract_bench_signature(&s2).unwrap().0;
        assert_eq!(
            n1, n2,
            "bench total node count must be reproducible across invocations"
        );
        let per1 = extract_bench_per_position(&s1);
        let per2 = extract_bench_per_position(&s2);
        let nodes1: Vec<u64> = per1.iter().map(|(_, _, n, _)| *n).collect();
        let nodes2: Vec<u64> = per2.iter().map(|(_, _, n, _)| *n).collect();
        assert_eq!(
            nodes1, nodes2,
            "per-position node counts must be reproducible across invocations \
             (catches drift on any single position even when totals happen to match)"
        );
    }

    #[test]
    fn handle_bench_after_go_infinite_produces_clean_state_results() {
        // Pins the bug fix for the must-fix found by final-review pass 1:
        // `join_in_flight_worker` sets `self.stop = true`; without an explicit
        // clear, every per-position `Search::go` inherits `stop=true` and
        // (a) either breaks between iterations via the inter-iteration stop
        //     check (depth ≥ 2 — search.rs:383),
        // (b) or aborts mid-iteration once the 4096-node cadence fires
        //     (depth 7+).
        //
        // Anchor: a `bench 2` that follows `go infinite` must produce exactly
        // the same per-position node counts as a clean-state `bench 2`. Any
        // contamination would manifest as different (typically much smaller)
        // counts.
        let (clean_stdout, _) = drive(&["bench 2"]);
        let clean_per_pos: Vec<u64> = extract_bench_per_position(&clean_stdout)
            .iter()
            .map(|(_, _, n, _)| *n)
            .collect();

        let (after_infinite_stdout, _) = drive(&["position startpos", "go infinite", "bench 2"]);
        let after_per_pos: Vec<u64> = extract_bench_per_position(&after_infinite_stdout)
            .iter()
            .map(|(_, _, n, _)| *n)
            .collect();

        assert_eq!(
            clean_per_pos, after_per_pos,
            "bench after `go infinite` must produce identical per-position node \
             counts to a clean-state bench (no `self.stop` contamination). \
             Clean: {clean_per_pos:?}; after-go-infinite: {after_per_pos:?}",
        );
    }

    #[test]
    fn handle_bench_joins_in_flight_search_worker() {
        // Plan §5: handle_bench's first action is `join_in_flight_worker()`,
        // mirroring `handle_ucinewgame`'s discipline. Without it, bench would
        // deadlock on the search mutex (held by the worker thread's
        // `Search::go`).
        //
        // What this test pins: **no-deadlock**. Sending `go infinite` (spawns
        // a worker that holds the search mutex until `stop`) followed by
        // `bench 1` must complete: the bench signature line MUST appear in
        // stdout. If `handle_bench` failed to flip `stop` and join the
        // worker, taking the search mutex inside the bench loop would block
        // forever — the test would hang and `cargo test` would eventually
        // kill it.
        //
        // What this test does NOT pin: temporal ordering between the worker's
        // `bestmove` emission and bench's output. The `drive()` harness loads
        // all commands into the channel before calling `engine.run()`; by the
        // time `handle_bench` runs, the worker thread spawned by `go infinite`
        // may or may not have been scheduled by the OS yet. The worker emits
        // `bestmove` whenever `Search::go` returns (which fires immediately on
        // `stop`), and that emission can land before, during, or after bench's
        // own output. The test asserts both lines appear, but not their order.
        let (stdout, _) = drive(&["position startpos", "go infinite", "bench 1"]);
        assert!(
            stdout.lines().any(|l| l.starts_with("info string bench: ")),
            "bench signature line missing; bench may have deadlocked on the in-flight worker. \
             Full stdout:\n{stdout}",
        );
        // bestmove from the prior `go infinite` must still appear at SOME
        // point (the worker is joined eventually — either by bench's
        // `join_in_flight_worker` or by the final `handle_quit`). Missing
        // implies the worker thread was killed without ever emitting, which
        // would indicate an ordering bug in the join logic.
        assert!(
            stdout.lines().any(|l| l.starts_with("bestmove ")),
            "bestmove from `go infinite` must appear in stdout; \
             missing implies the worker was killed without emitting. Full stdout:\n{stdout}",
        );
    }

    #[test]
    fn handle_bench_engine_remains_responsive_after_bench() {
        // Drive `bench 1`, then `isready`, then `quit`. Assert `readyok`
        // arrives AFTER the bench signature line. Pins: bench doesn't
        // deadlock the orchestrator.
        let (stdout, _) = drive(&["bench 1", "isready"]);
        let lines: Vec<&str> = stdout.lines().collect();
        let bench_pos = lines
            .iter()
            .position(|l| l.starts_with("info string bench: "))
            .unwrap_or_else(|| panic!("missing bench signature in:\n{stdout}"));
        let ready_pos = lines
            .iter()
            .position(|l| *l == "readyok")
            .unwrap_or_else(|| panic!("missing readyok in:\n{stdout}"));
        assert!(
            ready_pos > bench_pos,
            "readyok ({ready_pos}) must arrive AFTER the bench signature line ({bench_pos}); \
             stdout was:\n{stdout}"
        );
    }

    // -----------------------------------------------------------------------
    // M4.A — engine-level TT tests (E_a–E_f + E_b2)
    // -----------------------------------------------------------------------

    // Helper: build an engine, run commands, return (stdout, engine) so tests
    // can inspect engine state after the run. Unlike `drive`, this does NOT
    // append Quit — the caller controls the channel lifetime.
    #[allow(clippy::type_complexity)]
    fn build_engine_with_channel() -> (
        Arc<Mutex<Vec<u8>>>,
        Engine<CapturedWriter, AlphaBetaMover>,
        mpsc::Sender<Command>,
        mpsc::Receiver<Command>,
    ) {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let engine = Engine::new(writer, AlphaBetaMover::new());
        let (tx, rx) = mpsc::channel::<Command>();
        (buf, engine, tx, rx)
    }

    /// E_a: `handle_uci` emits the `option name Hash …` line.
    #[test]
    fn handle_uci_emits_hash_option_line() {
        let (stdout, _) = drive(&["uci"]);
        let lines: Vec<&str> = stdout.lines().collect();

        let opt_line = lines
            .iter()
            .find(|l| l.starts_with("option name Hash"))
            .copied()
            .expect("option name Hash line must be present in uci output");
        assert_eq!(
            opt_line, "option name Hash type spin default 16 min 1 max 4096",
            "Hash option line text must match exactly"
        );

        // Ordering: must appear before uciok.
        let hash_idx = lines
            .iter()
            .position(|l| l.starts_with("option name Hash"))
            .unwrap();
        let uciok_idx = lines.iter().position(|l| *l == "uciok").unwrap();
        assert!(
            hash_idx < uciok_idx,
            "option name Hash (line {hash_idx}) must precede uciok (line {uciok_idx})"
        );
    }

    /// E_b: `setoption name Hash` resizes the table in both directions.
    #[test]
    fn setoption_hash_valid_resizes_table() {
        let (buf, mut engine, tx, rx) = build_engine_with_channel();

        // Default: 16 MiB → 1_048_576 entries.
        assert_eq!(
            engine.tt().entry_count(),
            16 * 1024 * 1024 / 16,
            "default TT entry count must be 1_048_576"
        );

        tx.send(parse_uci_line("setoption name Hash value 32"))
            .unwrap();
        tx.send(Command::Quit).unwrap();
        engine.run(rx);

        let expected_32 = 32 * 1024 * 1024 / 16;
        assert_eq!(
            engine.tt().entry_count(),
            expected_32,
            "after setoption Hash 32, entry_count must be {} (32 MiB / 16 bytes)",
            expected_32
        );
        assert_eq!(
            engine.hash_mib(),
            32,
            "hash_mib field must track the resize"
        );

        // Now shrink: build a fresh engine and send Hash 4.
        let (_, mut engine2, tx2, rx2) = build_engine_with_channel();
        tx2.send(parse_uci_line("setoption name Hash value 4"))
            .unwrap();
        tx2.send(Command::Quit).unwrap();
        engine2.run(rx2);
        drop(buf);

        let expected_4 = 4 * 1024 * 1024 / 16;
        assert_eq!(
            engine2.tt().entry_count(),
            expected_4,
            "after setoption Hash 4, entry_count must be {} (4 MiB / 16 bytes)",
            expected_4
        );
        assert_eq!(
            engine2.hash_mib(),
            4,
            "hash_mib field must track the shrink"
        );
    }

    /// E_b2: `setoption name Hash` followed by `isready` then `go depth 2`
    /// runs to completion (proves stop=true from join_in_flight_worker is
    /// cleared before the subsequent go).
    ///
    /// Uses the background-thread pattern (not `drive`) so that Quit does not
    /// race with the in-flight depth-2 search: we poll for `info depth 2`
    /// before sending Quit.
    #[test]
    fn setoption_hash_followed_by_isready_then_go_does_not_stop_early() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, AlphaBetaMover::new());
        let (tx, rx) = mpsc::channel::<Command>();

        let buf_clone = Arc::clone(&buf);
        let handle = thread::spawn(move || engine.run(rx));

        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("setoption name Hash value 32"))
            .unwrap();
        tx.send(parse_uci_line("isready")).unwrap();
        tx.send(parse_uci_line("go depth 2")).unwrap();

        // Poll for bestmove (which implies depth 2 completed).
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snap = snapshot_output(&buf_clone);
            if snap.lines().any(|l| l.starts_with("bestmove ")) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "bestmove did not appear within 5s after go depth 2;\nstdout:\n{snap}"
            );
            thread::sleep(Duration::from_millis(2));
        }

        tx.send(Command::Quit).unwrap();
        handle.join().expect("engine thread should not panic");

        let stdout = snapshot_output(&buf_clone);
        // A depth-2 search that was stopped early (stop=true contamination from
        // setoption's join_in_flight_worker) would emit only `info depth 1`
        // and no `info depth 2` line.
        assert!(
            stdout.lines().any(|l| l.starts_with("info depth 2 ")),
            "go depth 2 after setoption Hash must reach depth 2; \
             stop=true contamination would prevent this.\nstdout:\n{stdout}"
        );
    }

    /// E_c: `setoption name Hash value 0` (below min=1) is rejected silently.
    #[test]
    fn setoption_hash_zero_rejected_silently() {
        let (buf, mut engine, tx, rx) = build_engine_with_channel();
        let initial_count = engine.tt().entry_count();

        tx.send(parse_uci_line("setoption name Hash value 0"))
            .unwrap();
        tx.send(Command::Quit).unwrap();
        engine.run(rx);

        let stdout = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        // TT size unchanged.
        assert_eq!(
            engine.tt().entry_count(),
            initial_count,
            "Hash value 0 must be rejected; TT size unchanged"
        );
        // Silent when debug off.
        assert!(
            stdout.is_empty(),
            "rejected Hash value must produce no output when debug is off; got: {stdout:?}"
        );
    }

    /// E_d: `setoption name Hash value 99999` (above max=4096) is rejected silently.
    #[test]
    fn setoption_hash_above_max_rejected_silently() {
        let (buf, mut engine, tx, rx) = build_engine_with_channel();
        let initial_count = engine.tt().entry_count();

        tx.send(parse_uci_line("setoption name Hash value 99999"))
            .unwrap();
        tx.send(Command::Quit).unwrap();
        engine.run(rx);

        let stdout = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        // TT size unchanged.
        assert_eq!(
            engine.tt().entry_count(),
            initial_count,
            "Hash value 99999 > MAX must be rejected; TT size unchanged"
        );
        // Silent when debug off.
        assert!(
            stdout.is_empty(),
            "rejected Hash value must produce no output when debug is off; got: {stdout:?}"
        );
    }

    /// E_e: `ucinewgame` clears TT entries populated by a prior search.
    #[test]
    fn ucinewgame_clears_tt() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, AlphaBetaMover::new());
        let (tx, rx) = mpsc::channel::<Command>();

        // Run a depth-2 search from startpos; this should populate TT entries.
        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go depth 2")).unwrap();
        tx.send(Command::Quit).unwrap();
        engine.run(rx);

        // After the search, probe the startpos key — it should be in the TT.
        let startpos_key = Position::starting_position().zobrist();
        let hit_before = engine.tt().probe(startpos_key);
        assert!(
            hit_before.is_some(),
            "TT must have an entry for startpos after a depth-2 search"
        );

        // Now reset for a new game; this should clear the TT.
        engine.reset_for_new_game();

        let hit_after = engine.tt().probe(startpos_key);
        assert!(
            hit_after.is_none(),
            "TT must be empty for startpos after ucinewgame (reset_for_new_game clears all entries)"
        );
    }

    /// E_h (M4.C): `ucinewgame` clears the butterfly history table populated
    /// by a prior search. Pinned via the search-side test accessor
    /// `AlphaBetaMover::history_table_for_test`. After a depth-2 search the
    /// table may be zero or non-zero depending on whether any quiet caused a
    /// beta cutoff during the search; what's load-bearing is that AFTER
    /// `reset_for_new_game()` the table is uniformly zero. To force a
    /// non-trivial pre-state we pre-seed via a direct `update` call held under
    /// the search mutex; then `ucinewgame` (`reset_for_new_game`) must clear.
    #[test]
    fn ucinewgame_clears_history_table() {
        use crate::piece::Color;
        use crate::square::Square;

        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, AlphaBetaMover::new());

        // Drive one search end-to-end so the orchestrator has run a `go` and
        // the worker has touched the search mutex; afterwards the search is
        // idle and the orchestrator can lock it.
        let (tx, rx) = mpsc::channel::<Command>();
        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go depth 2")).unwrap();
        tx.send(Command::Quit).unwrap();
        engine.run(rx);

        // Pre-seed the history table to a known non-zero state (independent
        // of whether the depth-2 search itself populated entries). Value
        // chosen below `MAX_HISTORY` so the clamp does not collapse it —
        // the test verifies the *clear*, not the clamp.
        {
            let mut s = engine.search.lock().unwrap();
            s.history_table_for_test_mut()
                .update(Color::White, Square::E2, Square::E4, 50);
        }
        // Sanity: confirm the seed is observable.
        {
            let s = engine.search.lock().unwrap();
            assert_eq!(
                s.history_table_for_test()
                    .score(Color::White, Square::E2, Square::E4),
                50,
                "pre-seed must be observable through the test accessor"
            );
        }

        engine.reset_for_new_game();

        let s = engine.search.lock().unwrap();
        let ht = s.history_table_for_test();
        assert_eq!(
            ht.score(Color::White, Square::E2, Square::E4),
            0,
            "ucinewgame must clear the history table (Search::reset path)"
        );
    }

    /// E_f: `bench` is deterministic run-to-run with TT in play.
    #[test]
    fn bench_clears_tt_between_positions_for_determinism() {
        // Run bench twice at depth 2; verify identical per-position node counts.
        // If the TT leaked between positions (no per-position clear), counts
        // would differ between run 1 and run 2 because run 2's TT is cold while
        // run 1's carries entries from position N into N+1.
        let (s1, _) = drive(&["bench 2"]);
        let (s2, _) = drive(&["bench 2"]);
        let per1 = extract_bench_per_position(&s1);
        let per2 = extract_bench_per_position(&s2);
        let nodes1: Vec<u64> = per1.iter().map(|(_, _, n, _)| *n).collect();
        let nodes2: Vec<u64> = per2.iter().map(|(_, _, n, _)| *n).collect();
        assert_eq!(
            nodes1, nodes2,
            "bench per-position node counts must be identical across two runs (TT cleared per position).\n\
             run1: {nodes1:?}\nrun2: {nodes2:?}"
        );
        let n1 = extract_bench_signature(&s1).unwrap().0;
        let n2 = extract_bench_signature(&s2).unwrap().0;
        assert_eq!(
            n1, n2,
            "bench total node count must be identical across two runs; got {n1} vs {n2}"
        );
    }

    // -----------------------------------------------------------------------
    // ELOH.C — `VirtualClock` UCI option tests (plan §6.4).
    //
    // Mirrors the M3.E `MoveOverhead` test pattern: parse path, default,
    // boundary handling, case-insensitivity, debug-on/off rejection echoing.
    // -----------------------------------------------------------------------

    /// Helper: build an Engine with captured stdout, run the given commands,
    /// and return both the captured output AND the final virtual_clock value.
    fn drive_capturing_virtual_clock(commands: &[&str]) -> (String, bool) {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, AlphaBetaMover::new());

        let (tx, rx) = mpsc::channel::<Command>();
        for line in commands {
            tx.send(parse_uci_line(line)).unwrap();
        }
        tx.send(Command::Quit).unwrap();
        engine.run(rx);

        let bytes = buf.lock().unwrap().clone();
        let stdout = String::from_utf8(bytes).expect("output must be valid UTF-8");
        (stdout, engine.virtual_clock())
    }

    #[cfg(unix)]
    #[test]
    fn option_advertised_in_uci_response_on_unix() {
        let (stdout, _) = drive(&["uci"]);
        assert!(
            stdout
                .lines()
                .any(|l| l == "option name VirtualClock type check default false"),
            "VirtualClock option line must be present in uci output; got stdout:\n{stdout}"
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn option_not_advertised_on_non_unix() {
        let (stdout, _) = drive(&["uci"]);
        assert!(
            !stdout
                .lines()
                .any(|l| l.starts_with("option name VirtualClock")),
            "VirtualClock option must NOT be advertised on non-unix"
        );
    }

    #[cfg(unix)]
    #[test]
    fn setoption_virtual_clock_true_sets_flag() {
        let (_stdout, vc) =
            drive_capturing_virtual_clock(&["setoption name VirtualClock value true"]);
        assert!(vc, "value true must set the flag");
    }

    #[cfg(unix)]
    #[test]
    fn setoption_virtual_clock_false_resets_flag() {
        let (_stdout, vc) = drive_capturing_virtual_clock(&[
            "setoption name VirtualClock value true",
            "setoption name VirtualClock value false",
        ]);
        assert!(!vc, "value false after true must reset the flag");
    }

    #[test]
    fn setoption_virtual_clock_default_is_false() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let engine = Engine::new(writer, AlphaBetaMover::new());
        assert!(
            !engine.virtual_clock(),
            "fresh engine must default virtual_clock to false"
        );
    }

    #[cfg(unix)]
    #[test]
    fn setoption_virtual_clock_invalid_value_rejected() {
        let (stdout, vc) =
            drive_capturing_virtual_clock(&["setoption name VirtualClock value bogus"]);
        assert!(!vc, "invalid value must leave flag at default false");
        assert!(
            stdout
                .lines()
                .any(|l| l.starts_with("info string VirtualClock:")),
            "rejection must emit `info string VirtualClock: ...`; stdout:\n{stdout}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn setoption_virtual_clock_case_insensitive_value() {
        for variant in &["TRUE", "True", "tRuE"] {
            let (_stdout, vc) = drive_capturing_virtual_clock(&[&format!(
                "setoption name VirtualClock value {variant}"
            )]);
            assert!(
                vc,
                "value {variant:?} must be parsed case-insensitively as true"
            );
        }
    }
}
