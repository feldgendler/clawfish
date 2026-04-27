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
//!   `Stub` search, calls `run`, then `std::process::exit(0)`.

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::search::{Search, SearchContext, SearchLimits, SearchResult, Stub};
use crate::{Command, DebugMode, GoParams, Move, Position, PositionSpec, Register, parse_uci_line};

/// UCI orchestrator. Owns engine state, dispatches parsed `Command`s to
/// per-command handlers, drives the search worker thread.
pub struct Engine<W: Write + Send + 'static, S: Search + Send + 'static> {
    position: Position,
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
        Self {
            position: Position::starting_position(),
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

    fn handle_uci(&mut self) {
        self.write_line(&format!("id name chess {}", env!("CARGO_PKG_VERSION")));
        self.write_line("id author Alex Feldgendler");
        self.write_line("uciok");
    }

    fn handle_isready(&mut self) {
        self.write_line("readyok");
    }

    fn handle_ucinewgame(&mut self) {
        self.position = Position::starting_position();
    }

    fn handle_position(&mut self, spec: PositionSpec, moves: Vec<String>) {
        let base = match spec {
            PositionSpec::StartPos => Position::starting_position(),
            PositionSpec::Fen(ref s) => match Position::from_fen(s) {
                Ok(p) => p,
                Err(e) => {
                    self.info_string_always(&format!("position rejected: invalid FEN: {e}"));
                    return;
                }
            },
        };

        let mut pos = base;
        for mv_str in &moves {
            match Move::from_uci(mv_str, &pos) {
                Ok(mv) => {
                    pos.make_move(mv);
                }
                Err(e) => {
                    self.info_string_always(&format!(
                        "position rejected: move {mv_str} failed: {e}"
                    ));
                    self.position = base;
                    return;
                }
            }
        }
        self.position = pos;
    }

    fn handle_go(&mut self, params: GoParams) {
        // (1) Implicit-stop on back-to-back go: signal the previous worker to
        // stop and join it. Setting stop=true before join is load-bearing for
        // infinite searches; without it the join would block forever waiting
        // for the worker to exit on its own.
        if let Some(h) = self.search_handle.take() {
            self.stop.store(true, Ordering::Relaxed);
            let _ = h.join();
        }
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
/// build an `Engine` with `io::stdout()` and the `Stub` search, drive
/// `run`, then `std::process::exit(0)`.
pub fn run_stdio() -> ! {
    let (tx, rx) = mpsc::channel::<Command>();
    std::thread::spawn(move || {
        reader_loop(std::io::BufReader::new(std::io::stdin()), tx);
    });
    let mut engine = Engine::new(std::io::stdout(), Stub);
    engine.run(rx);
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::Stub;
    use crate::search::{SearchContext, SearchResult};
    use crate::{Command, Position, parse_uci_line};
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

    /// Builds an `Engine<CapturedWriter, Stub>`, sends each UCI line as a
    /// parsed `Command` over an mpsc channel, appends `Command::Quit`, runs
    /// to completion, and returns the captured stdout + a clone of the final
    /// position.
    fn drive(commands: &[&str]) -> (String, Position) {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, Stub);

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
        let uciok_idx = lines
            .iter()
            .position(|l| *l == "uciok")
            .expect("uciok line present");
        assert!(
            id_name_idx < id_author_idx,
            "id name must come before id author"
        );
        assert!(
            id_author_idx < uciok_idx,
            "id author must come before uciok"
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
        // when debug is on. Catches a mutant where the body is replaced
        // with `()`. Also pins both the `Some(value)` and `None` arms of
        // the `details` formatter.
        let (stdout, _) = drive(&[
            "debug on",
            "setoption name Hash value 16",
            "setoption name Clear",
        ]);
        let info_lines: Vec<&str> = stdout
            .lines()
            .filter(|l| l.starts_with("info string setoption received:"))
            .collect();
        assert_eq!(
            info_lines.len(),
            2,
            "expected 2 setoption info-string lines; got:\n{stdout}"
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
        // those two legal moves. Stub picks lex-first within the filter,
        // which is `a2a4` (not `a2a3` — a2a3 is excluded).
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, Stub);
        let (tx, rx) = mpsc::channel::<Command>();

        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go searchmoves a2a4 b2b4")).unwrap();
        tx.send(Command::Quit).unwrap();
        engine.run(rx);

        let stdout = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            stdout.lines().any(|l| l == "bestmove a2a4"),
            "searchmoves a2a4/b2b4 must restrict Stub to lex-first within filter (a2a4);\nstdout:\n{stdout}",
        );
    }

    #[test]
    fn handle_go_searchmoves_all_bad_yields_bestmove_0000() {
        // Pins plan §6: when `searchmoves` parses to a list of all-illegal
        // entries, the resulting filter is `Some(Vec::new())` — Stub finds
        // no candidate and emits `bestmove 0000`. Distinct from "no
        // searchmoves keyword" which would let Stub pick any legal move.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, Stub);
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
        // silently dropped, leaving the filter `[a2a4, b2b4]`. Stub picks
        // lex-first within the filter (a2a4).
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, Stub);
        let (tx, rx) = mpsc::channel::<Command>();

        tx.send(parse_uci_line("position startpos")).unwrap();
        tx.send(parse_uci_line("go searchmoves a2a4 z9z9 b2b4"))
            .unwrap();
        tx.send(Command::Quit).unwrap();
        engine.run(rx);

        let stdout = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            stdout.lines().any(|l| l == "bestmove a2a4"),
            "bad searchmoves entries must be silently dropped, keeping the rest;\nstdout:\n{stdout}",
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
        // Forward-compat caveat: pin will be revised when first real option/registration/ponder lands.
        let (stdout, _) = drive(&["setoption name Hash value 16"]);
        assert!(
            stdout.is_empty(),
            "setoption must produce zero output (no bytes whatsoever) when debug is off; got: {stdout:?}",
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
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, Stub);
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

        // Poll for `bestmove a2a3` before sending Quit. handle_quit does
        // not join the worker (plan §9), so sending Quit immediately
        // would race the worker's write. The poll proves the worker has
        // observed the cancellation and emitted its bestmove.
        let bestmove_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let snap = snapshot_output(&buf_clone);
            if snap.lines().any(|l| l == "bestmove a2a3") {
                break;
            }
            assert!(
                Instant::now() < bestmove_deadline,
                "bestmove a2a3 did not appear within 1 s of stop;\noutput:\n{snap}"
            );
            thread::sleep(Duration::from_millis(2));
        }

        tx.send(Command::Quit).unwrap();
        handle.join().expect("engine thread should not panic");
    }

    #[test]
    fn go_with_movetime_emits_bestmove_after_deadline() {
        // Run the engine on a separate thread so we can time how long it
        // takes for `bestmove` to appear in the buffer — independently of
        // when `Quit` is processed. Sending `Quit` in the same channel
        // would set the cancellation flag immediately, defeating the
        // movetime deadline.
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, Stub);
        let (tx, rx) = mpsc::channel::<Command>();

        tx.send(parse_uci_line("position startpos")).unwrap();

        let buf_clone = Arc::clone(&buf);
        let handle = thread::spawn(move || engine.run(rx));

        // Time from when we send `go movetime 50` until `bestmove` is in
        // the buffer. Anchor: must take >= 40 ms (catches a Stub that
        // ignores movetime). Must complete well within 1 s.
        let go_sent = Instant::now();
        tx.send(parse_uci_line("go movetime 50")).unwrap();

        let bestmove_deadline = go_sent + Duration::from_secs(1);
        let observed_at = loop {
            let snap = snapshot_output(&buf_clone);
            if snap.lines().any(|l| l == "bestmove a2a3") {
                break Instant::now();
            }
            assert!(
                Instant::now() < bestmove_deadline,
                "bestmove a2a3 did not appear within 1 s of go movetime 50;\noutput:\n{snap}"
            );
            thread::sleep(Duration::from_millis(2));
        };

        let elapsed = observed_at.duration_since(go_sent);
        assert!(
            elapsed >= Duration::from_millis(40),
            "go movetime 50 → bestmove must take >= 40 ms; took {elapsed:?} (Stub may be ignoring movetime)"
        );

        tx.send(Command::Quit).unwrap();
        handle.join().expect("engine thread should not panic");
    }

    #[test]
    fn go_completes_immediately_without_infinite_or_movetime() {
        // Bare `go` (no infinite/movetime/ponder): Stub picks the candidate
        // and returns without entering a polling loop (plan §8 "Else: emit
        // immediately"). But the worker still writes from a spawned thread,
        // so the same poll-before-quit pattern as B18 is required to avoid
        // racing handle_quit's no-join return path (plan §9).
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, Stub);
        let (tx, rx) = mpsc::channel::<Command>();

        tx.send(parse_uci_line("position startpos")).unwrap();

        let buf_clone = Arc::clone(&buf);
        let handle = thread::spawn(move || engine.run(rx));

        let go_sent = Instant::now();
        tx.send(parse_uci_line("go")).unwrap();

        let bestmove_deadline = go_sent + Duration::from_secs(1);
        let observed_at = loop {
            let snap = snapshot_output(&buf_clone);
            if snap.lines().any(|l| l == "bestmove a2a3") {
                break Instant::now();
            }
            assert!(
                Instant::now() < bestmove_deadline,
                "bestmove a2a3 did not appear within 1 s of bare go;\noutput:\n{snap}"
            );
            thread::sleep(Duration::from_millis(2));
        };

        // Anchor "immediately" — bare go must complete fast (well under
        // 100 ms even on loaded CI). Catches a regression where Stub
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
        let mut engine = Engine::new(writer, Stub);
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
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, Stub);
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
        // Per plan §9 and B22: do NOT assert bestmove is in output.
        // handle_quit does not join the worker; the worker may still be in
        // its sleep loop when run returns.
    }

    #[test]
    fn back_to_back_go_implicit_stops_previous() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(Arc::clone(&buf));
        let mut engine = Engine::new(writer, Stub);
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
                // SetOption with non-empty name
                "[a-zA-Z][a-zA-Z0-9_]*".prop_map(|name| Command::SetOption { name, value: None }),
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
                let mut engine = Engine::new(writer, Stub);
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
}
