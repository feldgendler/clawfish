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
//!   `io::stdin().lock()`, builds an `Engine` with `io::stdout()` and the
//!   `GreedyMover` search, calls `run`, then `std::process::exit(0)`.

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::search::{GreedyMover, Search, SearchContext, SearchLimits, SearchResult};
use crate::{Command, DebugMode, GoParams, Move, Position, PositionSpec, Register, parse_uci_line};

// Type is u32; value is 2^31 - 1 (the protocol-declared `max`). Not i32 —
// the comparison band [0, MAX_RANDOM_SEED] is unsigned by construction.
// Must match the `max` in the `option name Random_Seed …` line emitted by
// `handle_uci`.
const MAX_RANDOM_SEED: u32 = 2_147_483_647;

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
}

impl<W: Write + Send + 'static, S: Search + Send + 'static> Engine<W, S> {
    /// Build an engine. Position starts at `Position::starting_position()`,
    /// `debug` off, no search in flight.
    pub fn new(stdout: W, search: S) -> Self {
        let position = Position::starting_position();
        Self {
            game_history: vec![position.zobrist()],
            position,
            debug: false,
            stop: Arc::new(AtomicBool::new(false)),
            stdout: Arc::new(Mutex::new(stdout)),
            search: Arc::new(Mutex::new(search)),
            search_handle: None,
        }
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
        self.write_line("uciok");
    }

    fn handle_isready(&mut self) {
        self.write_line("readyok");
    }

    fn handle_ucinewgame(&mut self) {
        // Signal and join any in-flight worker before mutating state or
        // acquiring the search mutex. A `go infinite` still running when
        // `ucinewgame` arrives would otherwise hold the lock for the full
        // polling duration, blocking the orchestrator and preventing it from
        // ever processing the inevitable `stop`. ADR-0011 v3.
        self.join_in_flight_worker();
        self.position = Position::starting_position();
        self.game_history = vec![self.position.zobrist()];
        self.search.lock().unwrap().reset();
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

        // (4) Compute deadline from movetime. movetime=0 → deadline=now → the
        // worker's first should_abort check fires immediately.
        let deadline = params
            .movetime
            .map(|ms| Instant::now() + Duration::from_millis(ms.max(0) as u64));

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
            deadline,
            start: Instant::now(),
            limits,
            history: self.game_history.clone(),
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
    /// Used by `handle_go` (back-to-back go), `handle_ucinewgame`, and
    /// `handle_setoption`'s `Random_Seed` success path — anything that needs
    /// to mutate engine state or hold the search mutex for a non-trivial
    /// duration. ADR-0011 v3 guarantees the worker exits within ≤ 1 ms (its
    /// cancellation-poll cadence). The caller must clear `self.stop` afterward
    /// when a new search is about to begin (only `handle_go` does this).
    fn join_in_flight_worker(&mut self) {
        if let Some(h) = self.search_handle.take() {
            self.stop.store(true, Ordering::Relaxed);
            let _ = h.join();
        }
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
/// build an `Engine` with `io::stdout()` and `GreedyMover` (seed 0 matches
/// the protocol-declared `default 0`), drive `run`, then
/// `std::process::exit(0)`.
pub fn run_stdio() -> ! {
    let (tx, rx) = mpsc::channel::<Command>();
    std::thread::spawn(move || {
        reader_loop(std::io::BufReader::new(std::io::stdin()), tx);
    });
    let mut engine = Engine::new(std::io::stdout(), GreedyMover::new(0));
    engine.run(rx);
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{GreedyMover, SearchContext, SearchResult};
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

    /// Builds an `Engine<CapturedWriter, GreedyMover>`, sends each UCI line as
    /// a parsed `Command` over an mpsc channel, appends `Command::Quit`, runs
    /// to completion, and returns the captured stdout + a clone of the final
    /// position.
    fn drive(commands: &[&str]) -> (String, Position) {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, GreedyMover::new(0));

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
        // (iii) Random_Seed with valid value under debug on: must be SILENT on
        // success (not echoed — different from unknown options).
        let (stdout, _) = drive(&[
            "debug on",
            "setoption name Hash value 16",
            "setoption name Clear",
            "setoption name Random_Seed value 42",
        ]);
        // (i) Unknown options still echo under debug on — count must still be 2.
        let info_lines: Vec<&str> = stdout
            .lines()
            .filter(|l| l.starts_with("info string setoption received:"))
            .collect();
        assert_eq!(
            info_lines.len(),
            2,
            "expected 2 setoption info-string lines (Hash + Clear only); got:\n{stdout}"
        );
        assert!(
            info_lines.iter().any(|l| l.contains("name Hash value 16")),
            "expected `name Hash value 16` in setoption info string;\nlines: {info_lines:?}",
        );
        assert!(
            info_lines
                .iter()
                .any(|l| l.contains("name Clear") && !l.contains("value")),
            "expected `name Clear` (no value) in setoption info string;\nlines: {info_lines:?}",
        );
        // (iv) Explicit assertion: Random_Seed success must produce zero
        // Random_Seed-mentioning info lines. Catches a mutant that swaps the
        // success branch to echo.
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
        // those two legal moves. GreedyMover picks the best-eval move from the
        // filtered set, so the bestmove must be one of {a2a4, b2b4}.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, GreedyMover::new(0));
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
        // entries, the resulting filter is `Some(Vec::new())` — GreedyMover
        // finds no candidate and emits `bestmove 0000`. Distinct from "no
        // searchmoves keyword" which would let the mover pick any legal move.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, GreedyMover::new(0));
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
        // silently dropped, leaving the filter [a2a4, b2b4]. GreedyMover picks
        // the best-eval move from the filtered set, so bestmove ∈ {a2a4, b2b4}.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, GreedyMover::new(0));
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

        // (ii) Unknown option — preserved M2.C behavior. Silent on debug off.
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
        // a legal bestmove from startpos (not `bestmove 0000`). GreedyMover
        // picks by eval — we assert any legal UCI move, not a specific one.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, GreedyMover::new(0));
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
        let bestmove_deadline = Instant::now() + Duration::from_secs(1);
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
        // Intent: wallclock deadline honored. Run the engine on a separate
        // thread so we can time how long it takes for `bestmove` to appear in
        // the buffer — independently of when `Quit` is processed. GreedyMover
        // picks by eval — we assert any legal UCI move, not a specific one.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, GreedyMover::new(0));
        let (tx, rx) = mpsc::channel::<Command>();

        tx.send(parse_uci_line("position startpos")).unwrap();

        let buf_clone = Arc::clone(&buf);
        let handle = thread::spawn(move || engine.run(rx));

        // Time from when we send `go movetime 50` until `bestmove` is in
        // the buffer. Anchor: must take >= 40 ms (catches a mover that
        // ignores movetime). Must complete well within 1 s.
        let startpos = Position::starting_position();
        let go_sent = Instant::now();
        tx.send(parse_uci_line("go movetime 50")).unwrap();

        let bestmove_deadline = go_sent + Duration::from_secs(1);
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
                "no bestmove appeared within 1 s of go movetime 50;\noutput:\n{snap}"
            );
            thread::sleep(Duration::from_millis(2));
        };

        let elapsed = observed_at.duration_since(go_sent);
        assert!(
            elapsed >= Duration::from_millis(40),
            "go movetime 50 → bestmove must take >= 40 ms; took {elapsed:?} (mover may be ignoring movetime)"
        );

        tx.send(Command::Quit).unwrap();
        handle.join().expect("engine thread should not panic");
    }

    #[test]
    fn go_completes_immediately_without_infinite_or_movetime() {
        // Intent: bare go is immediate. GreedyMover evaluates candidates and
        // returns without entering a polling loop (plan §8 "Else: emit
        // immediately"). The worker still writes from a spawned thread, so the
        // same poll-before-quit pattern as B18 is required to avoid racing
        // handle_quit's no-join return path. We assert any legal UCI move.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, GreedyMover::new(0));
        let (tx, rx) = mpsc::channel::<Command>();

        tx.send(parse_uci_line("position startpos")).unwrap();

        let buf_clone = Arc::clone(&buf);
        let handle = thread::spawn(move || engine.run(rx));

        let startpos = Position::starting_position();
        let go_sent = Instant::now();
        tx.send(parse_uci_line("go")).unwrap();

        let bestmove_deadline = go_sent + Duration::from_secs(1);
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
                "no bestmove appeared within 1 s of bare go;\noutput:\n{snap}"
            );
            thread::sleep(Duration::from_millis(2));
        };

        // Anchor "immediately" — bare go must complete fast (well under
        // 100 ms even on loaded CI). Catches a regression where the mover
        // accidentally enters the polling loop on bare go.
        let elapsed = observed_at.duration_since(go_sent);
        assert!(
            elapsed < Duration::from_millis(100),
            "bare go must emit bestmove in well under 100 ms; took {elapsed:?}"
        );

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
        let mut engine = Engine::new(writer, GreedyMover::new(0));
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
        let readyok_deadline = Instant::now() + Duration::from_secs(1);
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
        let mut engine = Engine::new(writer, GreedyMover::new(0));
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

        let deadline = Instant::now() + Duration::from_secs(1);
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
        let mut engine = Engine::new(writer, GreedyMover::new(0));
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
        let first_deadline = Instant::now() + Duration::from_secs(1);
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
        let second_deadline = Instant::now() + Duration::from_secs(1);
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
        // GreedyMover picks the unique best depth-1 move from startpos (g1f3 with
        // PeSTO eval), so the seed doesn't affect the choice there. We use a KvK
        // position where all 8 moves tie (insufficient material → score 0) so the
        // PRNG tie-break determines the result.
        //
        // (i) Case-insensitivity: three spellings of the option name all route
        // to the same seed value. Verified by capturing the bestmove from each
        // variant (all seeded with 42) and asserting:
        //   (a) all three case-variant bestmoves are equal to each other, and
        //   (b) they differ from the control (no setoption → seed 0).
        // Assertion (b) catches the "silently dropped" mutant: if all three
        // variants are ignored, the seed stays at 0 and matches the control.
        //
        // KvK: white king on e4, black king on e1; 8 legal moves all tied.
        // Pre-computed: seed 0 → e4d3, seed 42 → e4d3.
        // (Both happen to pick the same move; see boundary seed below for the
        // setoption-takes-effect check, where seed 17 differs.)
        const KVK_FEN: &str = "8/8/8/8/4K3/8/8/4k3 w - - 0 1";

        // Control: seed 0 (default, no setoption).
        let (stdout_ctrl, _) = drive(&[&format!("position fen {KVK_FEN}"), "go"]);
        let ctrl_bestmove = stdout_ctrl
            .lines()
            .find(|l| l.starts_with("bestmove"))
            .expect("control bestmove must be present")
            .strip_prefix("bestmove ")
            .expect("bestmove prefix")
            .to_string();

        // Three case variants, all with seed 42.
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

        // (a) All three case-variant bestmoves must be equal.
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

        // (b) Verify the setoption routes correctly by using seed 17 (which is
        // known to produce a different move than seed 0 on KvK). If setoption is
        // silently dropped, the engine stays at seed 0 and produces ctrl_bestmove.
        // Pre-computed: seed 17 → e4d4 (differs from seed 0's ctrl_bestmove).
        let (stdout_seed17, _) = drive(&[
            "setoption name Random_Seed value 17",
            &format!("position fen {KVK_FEN}"),
            "go",
        ]);
        let mv_seed17 = stdout_seed17
            .lines()
            .find(|l| l.starts_with("bestmove"))
            .expect("bestmove must be present with seed 17")
            .strip_prefix("bestmove ")
            .expect("bestmove prefix")
            .to_string();
        assert_ne!(
            mv_seed17, ctrl_bestmove,
            "setoption name Random_Seed value 17 must change the bestmove relative to the \
            seed-0 control — if equal, the option was silently dropped; \
            control='{ctrl_bestmove}' seed17='{mv_seed17}'"
        );

        // (ii) Boundary acceptance: max value 2147483647 is accepted and takes
        // effect (bestmove must be a legal king move from KvK).
        let pos_kvk = Position::from_fen(KVK_FEN).expect("KvK FEN must parse");
        let (stdout_max, _) = drive(&[
            "setoption name Random_Seed value 2147483647",
            &format!("position fen {KVK_FEN}"),
            "go",
        ]);
        let uci_max = stdout_max
            .lines()
            .find(|l| l.starts_with("bestmove"))
            .expect("bestmove must be present after go with max seed (2147483647)")
            .strip_prefix("bestmove ")
            .expect("bestmove prefix")
            .to_string();
        // The boundary seed must produce a legal move.
        Move::from_uci(&uci_max, &pos_kvk).unwrap_or_else(|e| {
            panic!(
                "bestmove '{uci_max}' is not legal for KvK (max seed 2147483647): {e};\nstdout:\n{stdout_max}"
            )
        });
        // The boundary seed must differ from the seed-0 control to prove it was applied.
        // Pre-computed: seed 2147483647 on KvK produces a different move than seed 0.
        // If this assertion fails, pick a different boundary seed.
        assert_ne!(
            uci_max, ctrl_bestmove,
            "setoption name Random_Seed value 2147483647 must change the bestmove \
            relative to the seed-0 control — if equal, the boundary value was rejected; \
            control='{ctrl_bestmove}' max='{uci_max}'"
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
        let mut engine = Engine::new(writer, GreedyMover::new(0));
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
            let deadline = Instant::now() + Duration::from_secs(1);
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
            let deadline = Instant::now() + Duration::from_secs(1);
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

        // (2) Reset seed 42 (back to the start of the seed-42 sequence) so the
        // next go produces the 1st pick. Then go once to advance PRNG one step,
        // send the four bad-value inputs, and go again to capture M_AFTER_BAD.
        // If bad inputs leave the seed unchanged, M_AFTER_BAD == M_REF2.
        tx.send(parse_uci_line("setoption name Random_Seed value 42"))
            .unwrap();
        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go")).unwrap();

        // Wait for go#3 bestmove (3rd total).
        {
            let deadline = Instant::now() + Duration::from_secs(1);
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
            let deadline = Instant::now() + Duration::from_secs(1);
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

    #[test]
    fn handle_setoption_random_seed_changes_future_bestmoves_but_not_past_ones() {
        // GreedyMover picks the unique depth-1 best move from any position where
        // one move is clearly superior. To verify that `setoption Random_Seed`
        // actually changes the tie-break PRNG state, we use a KvK position where
        // ALL legal moves tie at score 0 (insufficient material → evaluate() == 0
        // for every post-make position). The PRNG tie-break then determines which
        // move is chosen, so two different seeds must produce different bestmoves.
        //
        // Pre-computed: SEED_A=42 → e4d3, SEED_B=17 → e4d4 (KvK: white king on e4,
        // black king on e1; 8 legal king moves, all tied).
        const SEED_A: u32 = 42;
        const SEED_B: u32 = 17;
        const KVK_FEN: &str = "8/8/8/8/4K3/8/8/4k3 w - - 0 1";

        // (1) Set seed A, go from KvK, capture M1.
        let (stdout_a, _) = drive(&[
            &format!("setoption name Random_Seed value {SEED_A}"),
            &format!("position fen {KVK_FEN}"),
            "go",
        ]);
        let m1 = stdout_a
            .lines()
            .find(|l| l.starts_with("bestmove"))
            .expect("bestmove M1 present")
            .strip_prefix("bestmove ")
            .expect("prefix")
            .to_string();
        assert_eq!(
            m1, "e4d3",
            "SEED_A={SEED_A} must produce bestmove e4d3 from KvK; got '{m1}'"
        );

        // (2) Set seed B, go from KvK, capture M2.
        let (stdout_b, _) = drive(&[
            &format!("setoption name Random_Seed value {SEED_B}"),
            &format!("position fen {KVK_FEN}"),
            "go",
        ]);
        let m2 = stdout_b
            .lines()
            .find(|l| l.starts_with("bestmove"))
            .expect("bestmove M2 present")
            .strip_prefix("bestmove ")
            .expect("prefix")
            .to_string();
        assert_eq!(
            m2, "e4d4",
            "SEED_B={SEED_B} must produce bestmove e4d4 from KvK; got '{m2}'"
        );

        assert_ne!(m1, m2, "SEED_A and SEED_B must produce different bestmoves");
    }

    #[test]
    fn handle_ucinewgame_resets_greedy_mover_state() {
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
        let mut engine = Engine::new(writer, GreedyMover::new(0));
        let (tx, rx) = mpsc::channel::<Command>();

        let handle = thread::spawn(move || engine.run(rx));

        tx.send(parse_uci_line("setoption name Random_Seed value 7"))
            .unwrap();
        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go")).unwrap();

        // Wait until go#1's bestmove appears so the worker has fully run and
        // advanced PRNG state before we send ucinewgame.
        let m1 = {
            let deadline = Instant::now() + Duration::from_secs(1);
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
            let deadline = Instant::now() + Duration::from_secs(1);
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
        // (1) Set seed 7, go 3 times. State advances 3 steps.
        // (2) Set seed 7 again (resets state back to seed).
        // (3) Next go must match the FIRST go of a fresh seed-7 sequence.
        // Pins §2.1 decision: setoption resets state immediately, not deferred.

        // Fresh seed-7 reference: what does the 1st go produce?
        let (stdout_ref, _) = drive(&[
            "setoption name Random_Seed value 7",
            "position startpos",
            "go",
        ]);
        let m_ref = stdout_ref
            .lines()
            .find(|l| l.starts_with("bestmove"))
            .expect("bestmove reference present")
            .strip_prefix("bestmove ")
            .expect("prefix")
            .to_string();

        // Drive a separate engine on a background thread so we can observe each
        // bestmove before sending the next command. Without this, the orchestrator
        // could process `setoption` (the second set) while go#3's worker is
        // still queued, causing go#4 to start from step1(seed) instead of seed.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let buf_clone = Arc::clone(&buf);
        let mut engine = Engine::new(writer, GreedyMover::new(0));
        let (tx, rx) = mpsc::channel::<Command>();

        let handle = thread::spawn(move || engine.run(rx));

        tx.send(parse_uci_line("setoption name Random_Seed value 7"))
            .unwrap();

        // Helper closure: wait for the N-th bestmove to appear.
        let wait_for_nth_bestmove = |n: usize| -> String {
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let snap = snapshot_output(&buf_clone);
                let bestmoves: Vec<&str> =
                    snap.lines().filter(|l| l.starts_with("bestmove")).collect();
                if bestmoves.len() >= n {
                    return bestmoves[n - 1]
                        .strip_prefix("bestmove ")
                        .expect("prefix")
                        .to_string();
                }
                assert!(
                    Instant::now() < deadline,
                    "bestmove #{n} did not appear within 1 s"
                );
                thread::sleep(Duration::from_millis(2));
            }
        };

        // go#1, go#2, go#3 — advance PRNG 3 steps.
        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go")).unwrap();
        wait_for_nth_bestmove(1);

        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go")).unwrap();
        wait_for_nth_bestmove(2);

        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go")).unwrap();
        wait_for_nth_bestmove(3);

        // Reset: same seed. State must snap back to seed=7.
        tx.send(parse_uci_line("setoption name Random_Seed value 7"))
            .unwrap();
        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go")).unwrap();
        let m_after_reset = wait_for_nth_bestmove(4);

        tx.send(Command::Quit).unwrap();
        handle.join().expect("engine thread should not panic");

        assert_eq!(
            m_ref, m_after_reset,
            "setoption must reset PRNG state immediately; after reset expected '{m_ref}', got '{m_after_reset}'"
        );
    }

    #[test]
    fn greedy_mover_determinism_across_repeated_runs_with_fixed_seed() {
        // Run the same UCI transcript twice with a fresh Engine each time.
        // Assert identical bestmove lines from both runs. (The info line
        // includes a `time <ms>` token that legitimately varies, so we compare
        // only the bestmove output rather than the full stdout.)
        let transcript: &[&str] = &[
            "uci",
            "setoption name Random_Seed value 99",
            "position startpos",
            "go",
            "position startpos moves e2e4",
            "go",
        ];
        let (stdout_a, _) = drive(transcript);
        let (stdout_b, _) = drive(transcript);

        let bestmoves_a: Vec<&str> = stdout_a
            .lines()
            .filter(|l| l.starts_with("bestmove"))
            .collect();
        let bestmoves_b: Vec<&str> = stdout_b
            .lines()
            .filter(|l| l.starts_with("bestmove"))
            .collect();
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

        let deadline = Instant::now() + Duration::from_secs(1);
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
                let mut engine = Engine::new(writer, GreedyMover::new(0xDEAD_BEEF));
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
    /// history invariant checks. Uses `GreedyMover` so it's location-agnostic.
    fn drive_with_history(commands: &[&str]) -> (String, Position, Vec<u64>) {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, GreedyMover::new(0));

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
        let engine = Engine::new(CapturedWriter(Arc::clone(&buf)), GreedyMover::new(0));
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
}
