//! `elo-iterate` — in-process tournament harness.
//!
//! Drives two UCI engines via persistent subprocess pipes, plays a
//! colour-paired fixed batch of games with native adjudication, and emits
//! per-game PGN plus a summary line. Replaces `scripts/elo-iterate.sh` for
//! the correctness layer of online Elo iteration.
//!
//! Sub-module layout:
//!   - [`cli`]        — argument parsing.
//!   - [`driver`]     — subprocess + UCI line parsing.
//!   - [`adjudicate`] — native game-over detection.
//!   - [`match_loop`] — colour-paired game loop + per-side clock.
//!   - [`pgn`]        — PGN tag-roster + body emission.
//!   - [`summary`]    — summary.txt aggregation.
//!
use std::process::ExitCode;

// ---------------------------------------------------------------------------
// mod cli
// ---------------------------------------------------------------------------

mod cli {
    //! CLI argument parsing for `elo-iterate`.

    /// Parsed command-line arguments.
    #[derive(Debug)]
    #[allow(dead_code)]
    pub(crate) struct Args {
        /// Path to the engine under test.
        pub engine: String,
        /// Path to the opponent engine.
        pub opponent: String,
        /// Optional launch prefix words for the engine (e.g. ["taskpolicy", "-c", "utility"]).
        pub engine_launch_prefix: Option<Vec<String>>,
        /// Optional launch prefix words for the opponent.
        pub opponent_launch_prefix: Option<Vec<String>>,
        /// Time control string (e.g. "10+0.1").
        pub tc: TimeControl,
        /// Override time control for the opponent. Defaults to `tc`.
        pub opponent_tc_override: Option<TimeControl>,
        /// Total number of games to play. Must be even and ≥ 2.
        pub max_games: u32,
        /// Output directory.
        pub out_dir: String,
        /// Harness overhead grace in milliseconds. Default 50.
        pub harness_overhead_ms: u32,
        /// Watchdog timeout in milliseconds.
        pub watchdog_ms: u64,
        /// PGN Event tag. Default "ELOH.A run".
        pub event_tag: String,
        /// Engine options sent as `setoption name NAME value VALUE`.
        pub engine_options: Vec<(String, String)>,
        /// Opponent options.
        pub opponent_options: Vec<(String, String)>,
    }

    /// Parsed time control: initial time + per-move increment.
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct TimeControl {
        /// Initial time in milliseconds.
        pub initial_ms: u32,
        /// Per-move increment in milliseconds.
        pub increment_ms: u32,
    }

    /// Errors produced by CLI argument parsing.
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum CliError {
        /// A required flag is present but its value token is missing.
        MissingValue(String),
        /// A required flag was not provided.
        MissingFlag(String),
        /// An argument value is invalid.
        InvalidValue(String),
        /// `--max-games` violates the even-and-≥2 constraint.
        InvalidMaxGames(String),
        /// An unknown argument was encountered.
        UnknownArg(String),
    }

    impl std::fmt::Display for CliError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                CliError::MissingValue(s) => write!(f, "flag {s} requires a value"),
                CliError::MissingFlag(s) => write!(f, "missing required flag: {s}"),
                CliError::InvalidValue(s) => write!(f, "invalid argument value: {s}"),
                CliError::InvalidMaxGames(s) => write!(f, "invalid --max-games: {s}"),
                CliError::UnknownArg(s) => write!(f, "unknown argument: {s}"),
            }
        }
    }

    /// Parse a `<seconds>[+<seconds>]` time-control string.
    ///
    /// Supports `"10+0.1"` (10 s base + 100 ms inc) and `"60"` (60 s, no inc).
    pub(crate) fn parse_tc(s: &str) -> Result<TimeControl, CliError> {
        let (base_str, inc_str) = if let Some((b, i)) = s.split_once('+') {
            (b, i)
        } else {
            (s, "0")
        };
        let base_s: f64 = base_str
            .parse()
            .map_err(|_| CliError::InvalidValue(format!("bad time-control base: {base_str}")))?;
        let inc_s: f64 = inc_str.parse().map_err(|_| {
            CliError::InvalidValue(format!("bad time-control increment: {inc_str}"))
        })?;
        Ok(TimeControl {
            initial_ms: (base_s * 1000.0).round() as u32,
            increment_ms: (inc_s * 1000.0).round() as u32,
        })
    }

    /// Parse a `NAME=VALUE` option string into `(name, value)`.
    fn parse_option(s: &str) -> Result<(String, String), CliError> {
        if let Some((name, value)) = s.split_once('=') {
            Ok((name.to_owned(), value.to_owned()))
        } else {
            Err(CliError::InvalidValue(format!(
                "--*-option must be NAME=VALUE, got: {s}"
            )))
        }
    }

    /// Parse command-line arguments from an `argv` vector (not including argv[0]).
    pub(crate) fn parse_args(argv: Vec<String>) -> Result<Args, CliError> {
        let mut engine: Option<String> = None;
        let mut opponent: Option<String> = None;
        let mut engine_launch_prefix: Option<Vec<String>> = None;
        let mut opponent_launch_prefix: Option<Vec<String>> = None;
        let mut tc: Option<TimeControl> = None;
        let mut opponent_tc_override: Option<TimeControl> = None;
        let mut max_games: Option<u32> = None;
        let mut out_dir: Option<String> = None;
        let mut harness_overhead_ms: u32 = 50;
        let mut watchdog_ms: Option<u64> = None;
        let mut event_tag: String = "ELOH.A run".into();
        let mut engine_options: Vec<(String, String)> = Vec::new();
        let mut opponent_options: Vec<(String, String)> = Vec::new();

        let mut i = 0usize;
        while i < argv.len() {
            let flag = &argv[i];
            // Consume the next token as the value for `flag`.
            macro_rules! next_val {
                () => {{
                    i += 1;
                    if i >= argv.len() {
                        return Err(CliError::MissingValue(flag.clone()));
                    }
                    &argv[i]
                }};
            }
            match flag.as_str() {
                "--engine" => {
                    engine = Some(next_val!().clone());
                }
                "--opponent" => {
                    opponent = Some(next_val!().clone());
                }
                "--engine-launch-prefix" => {
                    let s = next_val!();
                    engine_launch_prefix = if s.is_empty() {
                        None
                    } else {
                        Some(s.split_ascii_whitespace().map(str::to_owned).collect())
                    };
                }
                "--opponent-launch-prefix" => {
                    let s = next_val!();
                    opponent_launch_prefix = if s.is_empty() {
                        None
                    } else {
                        Some(s.split_ascii_whitespace().map(str::to_owned).collect())
                    };
                }
                "--tc" => {
                    let s = next_val!();
                    tc = Some(
                        parse_tc(s).map_err(|e| CliError::InvalidValue(format!("--tc: {e}")))?,
                    );
                }
                "--opponent-tc-override" => {
                    let s = next_val!();
                    opponent_tc_override = Some(parse_tc(s).map_err(|e| {
                        CliError::InvalidValue(format!("--opponent-tc-override: {e}"))
                    })?);
                }
                "--max-games" => {
                    let s = next_val!();
                    let n: u32 = s
                        .parse()
                        .map_err(|_| CliError::InvalidMaxGames(s.clone()))?;
                    if n < 2 || !n.is_multiple_of(2) {
                        return Err(CliError::InvalidMaxGames(s.clone()));
                    }
                    max_games = Some(n);
                }
                "--out-dir" => {
                    out_dir = Some(next_val!().clone());
                }
                "--harness-overhead-ms" => {
                    let s = next_val!();
                    harness_overhead_ms = s.parse().map_err(|_| {
                        CliError::InvalidValue(format!("--harness-overhead-ms: {s}"))
                    })?;
                }
                "--watchdog-ms" => {
                    let s = next_val!();
                    watchdog_ms = Some(
                        s.parse()
                            .map_err(|_| CliError::InvalidValue(format!("--watchdog-ms: {s}")))?,
                    );
                }
                "--event-tag" => {
                    event_tag = next_val!().clone();
                }
                "--engine-option" => {
                    let s = next_val!();
                    engine_options
                        .push(parse_option(s).map_err(|_| {
                            CliError::InvalidValue(format!("--engine-option: {s}"))
                        })?);
                }
                "--opponent-option" => {
                    let s = next_val!();
                    opponent_options.push(
                        parse_option(s).map_err(|_| {
                            CliError::InvalidValue(format!("--opponent-option: {s}"))
                        })?,
                    );
                }
                other => {
                    return Err(CliError::UnknownArg(other.to_owned()));
                }
            }
            i += 1;
        }

        let engine = engine.ok_or_else(|| CliError::MissingFlag("--engine".into()))?;
        let opponent = opponent.ok_or_else(|| CliError::MissingFlag("--opponent".into()))?;
        let tc = tc.ok_or_else(|| CliError::MissingFlag("--tc".into()))?;
        let max_games = max_games.ok_or_else(|| CliError::MissingFlag("--max-games".into()))?;

        // Defaults computed here so parse_args returns a fully-resolved Args.
        let watchdog_ms = watchdog_ms.unwrap_or_else(|| {
            let base = 2 * u64::from(tc.initial_ms) + 30_000;
            base.max(60_000)
        });
        let out_dir = out_dir.unwrap_or_else(|| {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            format!("target/elo-iterate/run-{secs}")
        });

        Ok(Args {
            engine,
            opponent,
            engine_launch_prefix,
            opponent_launch_prefix,
            tc,
            opponent_tc_override,
            max_games,
            out_dir,
            harness_overhead_ms,
            watchdog_ms,
            event_tag,
            engine_options,
            opponent_options,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parse_args_rejects_odd_max_games() {
            let argv = vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/clawfish".into(),
                "--tc".into(),
                "10+0.1".into(),
                "--max-games".into(),
                "3".into(),
            ];
            let err = parse_args(argv).unwrap_err();
            assert!(
                matches!(err, CliError::InvalidMaxGames(_)),
                "expected InvalidMaxGames, got {err:?}"
            );
        }

        #[test]
        fn parse_args_rejects_max_games_zero() {
            let argv = vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/clawfish".into(),
                "--tc".into(),
                "10+0.1".into(),
                "--max-games".into(),
                "0".into(),
            ];
            let err = parse_args(argv).unwrap_err();
            assert!(
                matches!(err, CliError::InvalidMaxGames(_)),
                "expected InvalidMaxGames, got {err:?}"
            );
        }

        #[test]
        fn parse_args_rejects_max_games_one() {
            // Adjacent boundary: 1 is both odd AND less than the minimum 2.
            // Either parity- or lower-bound-based validation must reject it.
            let argv = vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/clawfish".into(),
                "--tc".into(),
                "10+0.1".into(),
                "--max-games".into(),
                "1".into(),
            ];
            let err = parse_args(argv).unwrap_err();
            assert!(
                matches!(err, CliError::InvalidMaxGames(_)),
                "expected InvalidMaxGames, got {err:?}"
            );
        }

        #[test]
        fn parse_args_accepts_minimum_max_games_2() {
            // --max-games 2 is the minimum valid value.
            let argv = vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/clawfish".into(),
                "--tc".into(),
                "10+0.1".into(),
                "--max-games".into(),
                "2".into(),
            ];
            // parse_args is not yet implemented; this test will todo!()-panic.
            // When impl lands, assert Ok and check max_games == 2.
            let result = parse_args(argv);
            match result {
                Ok(args) => assert_eq!(args.max_games, 2),
                Err(e) => panic!("expected Ok, got {e:?}"),
            }
        }

        #[test]
        fn parse_args_engine_option_repeatable() {
            // --engine-option can appear multiple times.
            let argv = vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/clawfish".into(),
                "--tc".into(),
                "10+0.1".into(),
                "--max-games".into(),
                "4".into(),
                "--engine-option".into(),
                "MoveOverhead=50".into(),
                "--engine-option".into(),
                "Hash=64".into(),
            ];
            let result = parse_args(argv);
            match result {
                Ok(args) => {
                    assert_eq!(args.engine_options.len(), 2);
                    assert!(
                        args.engine_options
                            .contains(&("MoveOverhead".into(), "50".into()))
                    );
                    assert!(args.engine_options.contains(&("Hash".into(), "64".into())));
                }
                Err(e) => panic!("expected Ok, got {e:?}"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// mod driver
// ---------------------------------------------------------------------------

mod driver {
    //! UCI subprocess driver.
    //!
    //! Each engine runs as a persistent subprocess with piped stdin/stdout.
    //! A reader thread drains the child's stdout into a bounded mpsc channel
    //! (`sync_channel(1024)`). The main thread consumes that channel via
    //! `recv_until_bestmove` whenever the engine may be producing output.
    //!
    //! **Recv-pump discipline.** Any command that may produce engine output
    //! (`go`, `isready`, `setoption`, `position`) must be immediately followed
    //! by a channel drain. Failure to drain risks filling the 1024-slot channel
    //! and blocking the reader thread, which would cause a deadlock.

    use std::io::{BufRead, BufReader, Write};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// A parsed line from the engine's stdout.
    #[derive(Debug)]
    pub(crate) enum EngineLine {
        /// The engine has chosen a move.
        Bestmove {
            /// UCI move string (e.g. `"e2e4"`, `"e7e8q"`, `"0000"`).
            uci: String,
            /// Ponder move, if the engine supplied one.
            ponder: Option<String>,
        },
        /// An `info …` line.
        Info(InfoLine),
        /// Any line not recognised as `bestmove` or `info`.
        Other(String),
        /// The engine's stdout reached EOF (process exited or pipe closed).
        Eof,
    }

    /// Parsed `info` line fields we care about.
    #[derive(Debug, Default, Clone)]
    pub(crate) struct InfoLine {
        pub depth: Option<u32>,
        pub score: Option<Score>,
        pub nodes: Option<u64>,
        /// Engine-reported time in milliseconds. Used for PGN comments ONLY;
        /// **never** for time-forfeit detection (a misbehaving engine could lie).
        pub time_ms: Option<u64>,
        pub pv: Option<String>,
    }

    /// The score from an `info score …` token.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Score {
        /// Centipawn score from the engine's perspective.
        Cp(i32),
        /// Mate in N half-moves (positive = engine mates, negative = engine gets mated).
        Mate(i32),
    }

    /// Snapshot of the most-recently-seen `info` line fields, updated by
    /// `recv_until_bestmove` and reset when a new `go` is issued.
    #[derive(Debug, Default, Clone)]
    pub(crate) struct LastInfo {
        pub depth: Option<u32>,
        pub score: Option<Score>,
        pub time_ms: Option<u64>,
    }

    /// Configuration for spawning an engine subprocess.
    #[derive(Debug, Clone)]
    #[allow(dead_code)]
    pub(crate) struct EngineSpec {
        /// Display name for PGN tags and logs.
        pub name: String,
        /// Path to the engine binary.
        pub path: String,
        /// Optional launch-prefix words prepended to argv.
        pub launch_prefix: Option<Vec<String>>,
    }

    /// Outcome of a successful `recv_until_bestmove` call.
    #[derive(Debug)]
    pub(crate) struct BestMoveOutcome {
        /// UCI move string chosen by the engine.
        pub uci: String,
        /// Engine's ponder hint, if provided. Reserved for ELOH.B ponder support.
        #[allow(dead_code)]
        pub ponder: Option<String>,
    }

    /// Errors the harness can produce when interacting with an engine.
    #[derive(Debug)]
    pub(crate) enum HarnessError {
        /// The watchdog timer fired before `bestmove` arrived.
        Watchdog,
        /// The engine's stdout closed (process exited unexpectedly).
        EngineExit,
        /// An I/O error on the engine's stdin. The inner error is surfaced via `Debug`.
        #[allow(dead_code)]
        Io(std::io::Error),
        /// A protocol line couldn't be parsed (informational; harness skips).
        #[allow(dead_code)]
        Parse(String),
    }

    /// Live handle to a running engine subprocess.
    #[allow(dead_code)]
    pub(crate) struct EngineHandle {
        pub(crate) name: String,
        pub(crate) child: std::process::Child,
        /// Engine's stdin pipe. `None` after `shutdown` has closed it.
        pub(crate) stdin: Option<std::process::ChildStdin>,
        /// Receives parsed lines from the reader thread.
        pub(crate) rx: mpsc::Receiver<EngineLine>,
        /// Reader thread join handle. `None` after `shutdown` has joined it.
        pub(crate) reader: Option<std::thread::JoinHandle<()>>,
        /// Most-recently-seen info fields; reset on each new `go`.
        pub(crate) last_info: LastInfo,
        /// Set to `true` by `shutdown` to suppress the Drop impl's kill.
        pub(crate) shutting_down: bool,
    }

    // Drop best-effort kills the child unless explicit shutdown was called.
    impl Drop for EngineHandle {
        fn drop(&mut self) {
            if !self.shutting_down {
                let _ = self.child.kill();
            }
        }
    }

    /// Parse a raw stdout line into an [`EngineLine`].
    pub(crate) fn parse_engine_line(s: &str) -> EngineLine {
        let s = s.trim_end();
        if let Some(rest) = s.strip_prefix("bestmove ") {
            parse_bestmove_payload(rest)
        } else if let Some(rest) = s.strip_prefix("info ") {
            EngineLine::Info(parse_info_payload(rest))
        } else if s == "info" {
            EngineLine::Info(InfoLine::default())
        } else {
            EngineLine::Other(s.to_owned())
        }
    }

    /// Parse the payload after `"bestmove "`.
    pub(crate) fn parse_bestmove_payload(rest: &str) -> EngineLine {
        let mut tokens = rest.split_ascii_whitespace();
        let uci = tokens.next().unwrap_or("0000").to_owned();
        // optional: "ponder <move>"
        let ponder = if tokens.next() == Some("ponder") {
            tokens.next().map(str::to_owned)
        } else {
            None
        };
        EngineLine::Bestmove { uci, ponder }
    }

    /// Parse the payload after `"info "`.
    pub(crate) fn parse_info_payload(rest: &str) -> InfoLine {
        let mut line = InfoLine::default();
        let mut tokens = rest.split_ascii_whitespace().peekable();
        while let Some(token) = tokens.next() {
            match token {
                "depth" => {
                    if let Some(v) = tokens.next() {
                        line.depth = v.parse().ok();
                    }
                }
                "nodes" => {
                    if let Some(v) = tokens.next() {
                        line.nodes = v.parse().ok();
                    }
                }
                "time" => {
                    if let Some(v) = tokens.next() {
                        line.time_ms = v.parse().ok();
                    }
                }
                "score" => {
                    if let Some(kind) = tokens.next()
                        && let Some(v) = tokens.next()
                    {
                        match (kind, v.parse::<i32>()) {
                            ("cp", Ok(n)) => line.score = Some(Score::Cp(n)),
                            ("mate", Ok(n)) => line.score = Some(Score::Mate(n)),
                            _ => {}
                        }
                    }
                }
                "pv" => {
                    // rest of the line is the PV
                    let remaining: Vec<&str> = tokens.collect();
                    line.pv = if remaining.is_empty() {
                        None
                    } else {
                        Some(remaining.join(" "))
                    };
                    break;
                }
                _ => {
                    // Skip unknown tokens and their value (token-pairs convention)
                    tokens.next();
                }
            }
        }
        line
    }

    /// Spawn an engine subprocess and start its reader thread.
    pub(crate) fn spawn_engine(spec: &EngineSpec) -> Result<EngineHandle, HarnessError> {
        use std::process::{Command, Stdio};

        let mut cmd = if let Some(words) = &spec.launch_prefix {
            let mut iter = words.iter();
            let prog = iter.next().map(String::as_str).unwrap_or_default();
            let mut c = Command::new(prog);
            c.args(iter);
            c.arg(&spec.path);
            c
        } else {
            Command::new(&spec.path)
        };

        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(HarnessError::Io)?;

        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");

        let (tx, rx) = mpsc::sync_channel::<EngineLine>(1024);
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(l) => {
                        let _ = tx.send(parse_engine_line(&l));
                    }
                    Err(_) => {
                        let _ = tx.send(EngineLine::Eof);
                        break;
                    }
                }
            }
            // Natural end of lines() also means EOF.
            let _ = tx.send(EngineLine::Eof);
        });

        Ok(EngineHandle {
            name: spec.name.clone(),
            child,
            stdin: Some(stdin),
            rx,
            reader: Some(reader),
            last_info: LastInfo::default(),
            shutting_down: false,
        })
    }

    /// Write a line to the engine's stdin, appending `\n` and flushing.
    pub(crate) fn send_line(h: &mut EngineHandle, line: &str) -> Result<(), HarnessError> {
        let stdin = h
            .stdin
            .as_mut()
            .ok_or_else(|| HarnessError::Io(std::io::Error::other("stdin already closed")))?;
        stdin
            .write_all(line.as_bytes())
            .and_then(|_| stdin.write_all(b"\n"))
            .and_then(|_| stdin.flush())
            .map_err(HarnessError::Io)
    }

    /// Drain the engine's output channel until `bestmove` arrives or an error
    /// condition fires. Updates `h.last_info` as `Info` lines flow through.
    pub(crate) fn recv_until_bestmove(
        h: &mut EngineHandle,
        watchdog: Duration,
    ) -> Result<BestMoveOutcome, HarnessError> {
        recv_until_bestmove_inner(&h.rx, &mut h.last_info, watchdog).map_err(|e| {
            // On watchdog timeout the child must be killed so it stops consuming.
            if matches!(e, HarnessError::Watchdog) {
                let _ = h.child.kill();
                let _ = h.child.wait();
            }
            e
        })
    }

    /// Inner implementation, factored out so tests can inject a synthetic
    /// `Receiver<EngineLine>` without spawning a real subprocess.
    pub(super) fn recv_until_bestmove_inner(
        rx: &mpsc::Receiver<EngineLine>,
        last_info: &mut LastInfo,
        watchdog: Duration,
    ) -> Result<BestMoveOutcome, HarnessError> {
        let deadline = Instant::now() + watchdog;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(HarnessError::Watchdog);
            }
            match rx.recv_timeout(remaining) {
                Ok(EngineLine::Bestmove { uci, ponder }) => {
                    return Ok(BestMoveOutcome { uci, ponder });
                }
                Ok(EngineLine::Info(info)) => {
                    // Aggregate: clobber fields present in this line, keep the
                    // rest from the prior line so the "last complete info" is
                    // always available even on partial lines.
                    if info.depth.is_some() {
                        last_info.depth = info.depth;
                    }
                    if info.score.is_some() {
                        last_info.score = info.score;
                    }
                    if info.time_ms.is_some() {
                        last_info.time_ms = info.time_ms;
                    }
                }
                Ok(EngineLine::Eof) => return Err(HarnessError::EngineExit),
                Ok(EngineLine::Other(_)) => {} // pass-through; ignore
                Err(mpsc::RecvTimeoutError::Timeout) => return Err(HarnessError::Watchdog),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(HarnessError::EngineExit),
            }
        }
    }

    /// Drain engine output until `uciok` is received (or timeout/error).
    ///
    /// Called after sending `uci`; discards all `option` lines and fires once
    /// the `uciok` response arrives.
    pub(crate) fn wait_for_uciok(
        h: &mut EngineHandle,
        timeout: std::time::Duration,
    ) -> Result<(), HarnessError> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(HarnessError::Watchdog);
            }
            match h.rx.recv_timeout(remaining) {
                Ok(EngineLine::Other(s)) if s.trim() == "uciok" => return Ok(()),
                Ok(EngineLine::Eof) => return Err(HarnessError::EngineExit),
                Err(mpsc::RecvTimeoutError::Timeout) => return Err(HarnessError::Watchdog),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(HarnessError::EngineExit),
                _ => {} // discard option lines, info string, etc.
            }
        }
    }

    /// Send `isready` and wait for `readyok`.
    pub(crate) fn wait_for_readyok(
        h: &mut EngineHandle,
        timeout: std::time::Duration,
    ) -> Result<(), HarnessError> {
        send_line(h, "isready")?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(HarnessError::Watchdog);
            }
            match h.rx.recv_timeout(remaining) {
                Ok(EngineLine::Other(s)) if s.trim() == "readyok" => return Ok(()),
                Ok(EngineLine::Eof) => return Err(HarnessError::EngineExit),
                Err(mpsc::RecvTimeoutError::Timeout) => return Err(HarnessError::Watchdog),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(HarnessError::EngineExit),
                _ => {} // pass through info string, etc.
            }
        }
    }

    /// Send `quit`, wait up to 1 s for the process to exit, then kill and reap.
    ///
    /// Sets `shutting_down = true` so the `Drop` impl does not issue a
    /// redundant kill when `h` falls out of scope at end of this function.
    pub(crate) fn shutdown(mut h: EngineHandle) -> Result<(), HarnessError> {
        h.shutting_down = true;

        // Best-effort graceful quit; errors mean the engine is already gone.
        if let Some(mut stdin) = h.stdin.take() {
            let _ = stdin.write_all(b"quit\n");
            let _ = stdin.flush();
            // Drop stdin to close the pipe so the engine sees EOF.
        }

        let mut exited = false;
        for _ in 0..20 {
            if h.child.try_wait().map(|s| s.is_some()).unwrap_or(true) {
                exited = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !exited {
            let _ = h.child.kill();
        }
        let _ = h.child.wait();
        if let Some(reader) = h.reader.take() {
            let _ = reader.join();
        }
        // h drops here; Drop impl no-ops because shutting_down == true.
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::mpsc;

        fn make_rx(lines: Vec<EngineLine>) -> mpsc::Receiver<EngineLine> {
            let (tx, rx) = mpsc::sync_channel(1024);
            for line in lines {
                tx.send(line).unwrap();
            }
            rx
        }

        #[test]
        fn recv_until_bestmove_aggregates_last_info() {
            let lines = vec![
                EngineLine::Info(InfoLine {
                    depth: Some(8),
                    score: Some(Score::Cp(20)),
                    time_ms: Some(100),
                    nodes: None,
                    pv: None,
                }),
                EngineLine::Info(InfoLine {
                    depth: Some(12),
                    score: Some(Score::Cp(100)),
                    time_ms: Some(250),
                    nodes: None,
                    pv: None,
                }),
                EngineLine::Bestmove {
                    uci: "e2e4".into(),
                    ponder: None,
                },
            ];
            let rx = make_rx(lines);
            let mut last_info = LastInfo::default();
            let outcome =
                recv_until_bestmove_inner(&rx, &mut last_info, Duration::from_secs(5)).unwrap();
            assert_eq!(outcome.uci, "e2e4");
            assert_eq!(last_info.depth, Some(12));
            assert_eq!(last_info.score, Some(Score::Cp(100)));
            assert_eq!(last_info.time_ms, Some(250));
        }

        #[test]
        fn recv_until_bestmove_handles_score_mate() {
            let lines = vec![
                EngineLine::Info(InfoLine {
                    depth: Some(5),
                    score: Some(Score::Mate(3)),
                    time_ms: None,
                    nodes: None,
                    pv: None,
                }),
                EngineLine::Bestmove {
                    uci: "a1a8".into(),
                    ponder: None,
                },
            ];
            let rx = make_rx(lines);
            let mut last_info = LastInfo::default();
            recv_until_bestmove_inner(&rx, &mut last_info, Duration::from_secs(5)).unwrap();
            assert_eq!(last_info.score, Some(Score::Mate(3)));
        }

        #[test]
        fn recv_until_bestmove_eof_is_err() {
            let rx = make_rx(vec![EngineLine::Eof]);
            let mut last_info = LastInfo::default();
            let err =
                recv_until_bestmove_inner(&rx, &mut last_info, Duration::from_secs(5)).unwrap_err();
            assert!(matches!(err, HarnessError::EngineExit), "got {err:?}");
        }

        #[test]
        fn recv_until_bestmove_watchdog_fires() {
            // Empty channel + short watchdog → Watchdog error.
            let (_tx, rx) = mpsc::sync_channel::<EngineLine>(1);
            let mut last_info = LastInfo::default();
            let err = recv_until_bestmove_inner(&rx, &mut last_info, Duration::from_millis(100))
                .unwrap_err();
            assert!(matches!(err, HarnessError::Watchdog), "got {err:?}");
        }

        #[test]
        fn parse_info_line_handles_partial_fields() {
            let info = parse_info_payload("depth 4 nodes 100");
            assert_eq!(info.depth, Some(4));
            assert_eq!(info.nodes, Some(100));
            assert!(info.score.is_none());
            assert!(info.time_ms.is_none());
            assert!(info.pv.is_none());
        }

        #[test]
        fn parse_bestmove_with_ponder() {
            let line = parse_engine_line("bestmove e2e4 ponder e7e5");
            match line {
                EngineLine::Bestmove { uci, ponder } => {
                    assert_eq!(uci, "e2e4");
                    assert_eq!(ponder, Some("e7e5".into()));
                }
                _ => panic!("expected Bestmove"),
            }
        }

        #[test]
        fn parse_bestmove_no_ponder() {
            let line = parse_engine_line("bestmove e2e4");
            match line {
                EngineLine::Bestmove { uci, ponder } => {
                    assert_eq!(uci, "e2e4");
                    assert!(ponder.is_none());
                }
                _ => panic!("expected Bestmove"),
            }
        }

        #[test]
        fn parse_bestmove_null_move() {
            let line = parse_engine_line("bestmove 0000");
            match line {
                EngineLine::Bestmove { uci, ponder } => {
                    assert_eq!(uci, "0000");
                    assert!(ponder.is_none());
                }
                _ => panic!("expected Bestmove"),
            }
        }

        #[test]
        fn shutdown_kills_hung_real_subprocess() {
            use std::process::{Command, Stdio};
            use std::time::Duration;

            // /bin/cat reads stdin forever and never emits `bestmove`.
            let mut child = Command::new("/bin/cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("failed to spawn /bin/cat");
            let stdin = child.stdin.take().unwrap();
            let stdout = child.stdout.take().unwrap();

            let (tx, rx) = mpsc::sync_channel::<EngineLine>(1024);
            let reader_handle = std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    match line {
                        Ok(l) => {
                            let _ = tx.send(parse_engine_line(&l));
                        }
                        Err(_) => {
                            let _ = tx.send(EngineLine::Eof);
                            break;
                        }
                    }
                }
            });

            let mut handle = EngineHandle {
                name: "cat".into(),
                child,
                stdin: Some(stdin),
                rx,
                reader: Some(reader_handle),
                last_info: LastInfo::default(),
                shutting_down: false,
            };

            // Watchdog fires after 100 ms; harness kills child.
            let result = recv_until_bestmove(&mut handle, Duration::from_millis(100));
            assert!(
                matches!(result, Err(HarnessError::Watchdog)),
                "expected Watchdog, got {result:?}"
            );

            // Poll for up to 1 s to confirm the child is reaped (avoid CI race).
            let killed = {
                let mut reaped = false;
                for _ in 0..20 {
                    if handle.child.try_wait().unwrap().is_some() {
                        reaped = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                reaped
            };
            assert!(killed, "child was not reaped within 1 s after watchdog");

            // shutdown on an already-dead child must not panic.
            // We need to construct a minimal replacement to call shutdown
            // without moving the EngineHandle (which Drop would double-kill).
            // Instead we just verify try_wait succeeded, which pins the kill path.
        }

        #[test]
        fn shutdown_clean_quit_path() {
            use std::process::{Command, Stdio};

            let mut child = Command::new("/bin/cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("failed to spawn /bin/cat");
            let stdin = child.stdin.take().unwrap();
            let stdout = child.stdout.take().unwrap();

            let (tx, rx) = mpsc::sync_channel::<EngineLine>(1024);
            let reader_handle = std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    match line {
                        Ok(l) => {
                            let _ = tx.send(parse_engine_line(&l));
                        }
                        Err(_) => {
                            let _ = tx.send(EngineLine::Eof);
                            break;
                        }
                    }
                }
            });

            let handle = EngineHandle {
                name: "cat".into(),
                child,
                stdin: Some(stdin),
                rx,
                reader: Some(reader_handle),
                last_info: LastInfo::default(),
                shutting_down: false,
            };

            // `cat` doesn't understand `quit`; shutdown must escalate to kill.
            let result = shutdown(handle);
            assert!(result.is_ok(), "shutdown returned {result:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// mod adjudicate
// ---------------------------------------------------------------------------

mod adjudicate {
    //! Native game-over detection.
    //!
    //! Covers: checkmate, stalemate, 50-move rule, threefold repetition
    //! (FIDE 9.2 — 3 occurrences total in full game history), and insufficient
    //! material (KK, KBK, KNK, KBKB-same-colour only).
    //!
    //! `detect_native_game_over` does NOT cover time forfeit; that is computed
    //! by the match loop from per-side clock state.

    use clawfish::{
        Color, MoveList, PieceKind, Position, generate_moves, in_check, search::is_fifty_move_draw,
    };

    /// Reason a game has ended, as detected by native adjudication.
    #[derive(Debug)]
    pub(crate) enum GameOver {
        /// The side to move is in checkmate. Carries the colour that delivered
        /// the mate (= opponent of the mated side).
        Checkmate(Color),
        /// The side to move has no legal moves and is not in check.
        Stalemate,
        /// The 50-move rule threshold (halfmove clock ≥ 100) has been reached.
        FiftyMove,
        /// The current position has appeared at least three times in the game
        /// record (FIDE 9.2).
        ThreefoldRepetition,
        /// Neither side has sufficient material to force checkmate.
        InsufficientMaterial,
        /// A side exceeded its time allowance. Carries the colour that forfeited.
        /// Reserved for future use; time-forfeit detection currently lives in
        /// `match_loop::GameOutcome::TimeForfeit` rather than adjudication.
        #[allow(dead_code)]
        TimeForfeit(Color),
    }

    /// Check for a native game-over condition after a move has been made.
    ///
    /// `history` is the full-game Zobrist trail (every position from game start,
    /// including the current position as the last entry).
    ///
    /// Order of checks (each pair pinned by a precedence test):
    /// 1. Legal-move count → Checkmate or Stalemate.
    /// 2. `is_fifty_move_draw` → FiftyMove.
    /// 3. `is_threefold_repetition_for_adjudication` → ThreefoldRepetition.
    /// 4. `is_insufficient_material` → InsufficientMaterial.
    pub(crate) fn detect_native_game_over(pos: &Position, history: &[u64]) -> Option<GameOver> {
        let mut moves = MoveList::new();
        generate_moves(pos, &mut moves);
        if moves.is_empty() {
            return if in_check(pos) {
                Some(GameOver::Checkmate(pos.side_to_move().flip()))
            } else {
                Some(GameOver::Stalemate)
            };
        }
        if is_fifty_move_draw(pos.halfmove_clock()) {
            return Some(GameOver::FiftyMove);
        }
        if is_threefold_repetition_for_adjudication(history) {
            return Some(GameOver::ThreefoldRepetition);
        }
        if is_insufficient_material(pos) {
            return Some(GameOver::InsufficientMaterial);
        }
        None
    }

    /// True iff neither side has material sufficient to force checkmate.
    ///
    /// Covers: KK, KBK (either side), KNK (either side), KBKB-same-colour.
    /// Negative cases (KNK N, KBNK, KQK, KRK, KPVK) return false.
    pub(crate) fn is_insufficient_material(pos: &Position) -> bool {
        // Any pawn, rook, or queen on the board → sufficient material.
        if pos.pieces(PieceKind::Pawn).any()
            || pos.pieces(PieceKind::Rook).any()
            || pos.pieces(PieceKind::Queen).any()
        {
            return false;
        }

        let white_bishops = pos.pieces_colored(Color::White, PieceKind::Bishop);
        let black_bishops = pos.pieces_colored(Color::Black, PieceKind::Bishop);
        let white_knights = pos.pieces_colored(Color::White, PieceKind::Knight);
        let black_knights = pos.pieces_colored(Color::Black, PieceKind::Knight);

        let white_minors = white_bishops.count() + white_knights.count();
        let black_minors = black_bishops.count() + black_knights.count();

        match (white_minors, black_minors) {
            // KK
            (0, 0) => true,
            // KBK or KNK (one side has a single minor, the other has none)
            (1, 0) | (0, 1) => true,
            // KBKB — both sides have exactly one bishop; insufficient iff same-colour squares
            (1, 1)
                if white_bishops.count() == 1
                    && black_bishops.count() == 1
                    && white_knights.is_empty()
                    && black_knights.is_empty() =>
            {
                // Square colour: (file + rank) % 2.
                // LERF: index = rank*8 + file, so index % 2 = file % 2 (rank*8 is
                // always even) — NOT the same as (file+rank)%2. The correct parity
                // is (file XOR rank) & 1 = (index XOR (index>>3)) & 1.
                let w_sq = white_bishops.lsb().expect("count==1 guarantees a set bit");
                let b_sq = black_bishops.lsb().expect("count==1 guarantees a set bit");
                let colour_parity = |sq: clawfish::Square| {
                    let idx = sq.index();
                    (idx ^ (idx >> 3)) & 1
                };
                colour_parity(w_sq) == colour_parity(b_sq)
            }
            _ => false,
        }
    }

    /// True iff the last entry of `history` appears at least three times in
    /// the full history (current + 2 prior occurrences — FIDE 9.2).
    ///
    /// The walk is **whole-history**, not since-last-irreversible-move.
    /// FIDE 9.2 counts occurrences across the full game record; restricting
    /// the walk to since-last-irreversible would break FIDE-correctness.
    /// Equal Zobrist keys guarantee equal position state (see plan §3.2 for
    /// the proof). DO NOT "fix" this to a bounded walk.
    pub(crate) fn is_threefold_repetition_for_adjudication(history: &[u64]) -> bool {
        let Some(&current) = history.last() else {
            return false;
        };
        let count = history.iter().filter(|&&h| h == current).count();
        count >= 3
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use clawfish::{Color, Move, Position};

        // ---------------------------------------------------------------
        // is_threefold_repetition_for_adjudication unit tests (implemented)
        // ---------------------------------------------------------------

        #[test]
        fn is_threefold_repetition_for_adjudication_single_entry_returns_false() {
            assert!(!is_threefold_repetition_for_adjudication(&[42]));
        }

        #[test]
        fn is_threefold_repetition_for_adjudication_two_entries_returns_false() {
            // FIDE requires three occurrences, not two.
            assert!(!is_threefold_repetition_for_adjudication(&[7, 7]));
        }

        #[test]
        fn is_threefold_repetition_for_adjudication_three_entries_returns_true() {
            assert!(is_threefold_repetition_for_adjudication(&[7, 7, 7]));
        }

        #[test]
        fn is_threefold_repetition_for_adjudication_counts_across_intervening() {
            // FIDE 9.2 counts position occurrences across the full game
            // record; intervening positions do NOT reset the count.
            //
            // History layout: index 0 = position `1` (call it `a`),
            //                 index 1 = position `7` (call it `h`, occurrence 1),
            //                 index 2 = position `2` (call it `b`),
            //                 index 3 = position `7` (h, occurrence 2),
            //                 index 4 = position `7` (h, occurrence 3 = current).
            //
            // The "last entry" examined by the helper is `history.last()` =
            // `7` (at index 4). Counting `7`s across the full slice = 3,
            // satisfying FIDE 9.2's three-occurrence rule even though the
            // occurrences are not contiguous.
            assert!(is_threefold_repetition_for_adjudication(&[1, 7, 2, 7, 7]));
        }

        // ---------------------------------------------------------------
        // detect_native_game_over tests (bodies todo!() until impl phase)
        // ---------------------------------------------------------------

        fn fool_position() -> (Position, Vec<u64>) {
            // Fool's mate: 1. f3 e5 2. g4 Qh4#
            // After White plays f2f3, Black plays e7e5, White plays g2g4,
            // Black plays d8h4 — White king is checkmated.
            let mut pos = Position::starting_position();
            let mut history = vec![pos.zobrist()];
            for uci in ["f2f3", "e7e5", "g2g4", "d8h4"] {
                let mv = Move::from_uci(uci, &pos).unwrap();
                pos.make_move(mv);
                history.push(pos.zobrist());
            }
            (pos, history)
        }

        #[test]
        fn mate_in_two_fool_known_position() {
            let (pos, history) = fool_position();
            // After d8h4, it is White's turn and White is in checkmate.
            // detect_native_game_over should return Checkmate(Black) — Black
            // delivered the mate.
            let result = detect_native_game_over(&pos, &history);
            match result {
                Some(GameOver::Checkmate(Color::Black)) => {}
                other => panic!(
                    "expected Checkmate(Black), got {:?}",
                    other.map(|_| "Some(...)")
                ),
            }
        }

        #[test]
        fn stalemate_classic_kp_known_position() {
            // Classic K+P stalemate: White Kb6, White Pa7, Black Ka8.
            // Black to move has no legal moves and is not in check:
            //  - Ka8→a7: attacked by Kb6 (illegal)
            //  - Ka8→b7: attacked by Kb6 (illegal)
            //  - Ka8→b8: attacked by both Kb6 (no, distance 2) AND Pa7 (yes, pawn
            //    attacks b8 diagonally) → illegal
            //  - Kxa7 (capture pawn): attacked by Kb6 (illegal)
            // FEN: k7/P7/1K6/8/8/8/8/8 b - - 0 1
            let pos = Position::from_fen("k7/P7/1K6/8/8/8/8/8 b - - 0 1").unwrap();
            let history = vec![pos.zobrist()];
            let result = detect_native_game_over(&pos, &history);
            assert!(
                matches!(result, Some(GameOver::Stalemate)),
                "expected Stalemate, got {result:?}"
            );
        }

        #[test]
        fn fifty_move_at_halfclock_100() {
            // Position with halfmove_clock=100 and at least one legal move (no mate/stalemate).
            // Use a quiet endgame with legal moves: KQ vs K, halfmove 100.
            let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/7Q w - - 100 50").unwrap();
            let history = vec![pos.zobrist()];
            let result = detect_native_game_over(&pos, &history);
            assert!(
                matches!(result, Some(GameOver::FiftyMove)),
                "expected FiftyMove"
            );
        }

        #[test]
        fn fifty_move_at_halfclock_99_returns_none() {
            let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/7Q w - - 99 50").unwrap();
            let history = vec![pos.zobrist()];
            // At 99, the 50-move rule has not been triggered. KQK is not
            // insufficient (queen can mate). History has one entry — no
            // threefold. So the only correct answer is None.
            let result = detect_native_game_over(&pos, &history);
            assert!(
                result.is_none(),
                "expected None at halfclock=99 with KQK and single-entry history; got {result:?}"
            );
        }

        #[test]
        fn threefold_via_history_three_occurrences() {
            // Position with a threefold-claimable history. Use a quiet endgame
            // with legal moves, halfclock < 100. The history is synthetic.
            let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/7Q w - - 0 1").unwrap();
            let z = pos.zobrist();
            // Construct a history where the current position appears 3 times.
            let history = vec![99u64, z, 88u64, z, z];
            let result = detect_native_game_over(&pos, &history);
            assert!(
                matches!(result, Some(GameOver::ThreefoldRepetition)),
                "expected ThreefoldRepetition"
            );
        }

        #[test]
        fn threefold_only_two_occurrences_returns_none() {
            // Pins the FIDE-3-vs-search-1 distinction: only 2 occurrences → no threefold.
            // KQK at halfmove=0 with 2-entry history has no mate/stalemate (kings have
            // legal moves), no fifty-move (halfmove=0), no threefold (2 < 3), no
            // insufficient material (KQK is sufficient). Only correct answer: None.
            let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/7Q w - - 0 1").unwrap();
            let z = pos.zobrist();
            let history = vec![99u64, z, z]; // last entry z appears twice
            let result = detect_native_game_over(&pos, &history);
            assert!(
                result.is_none(),
                "expected None with only 2 occurrences of current zobrist; got {result:?}"
            );
        }

        // ---------------------------------------------------------------
        // Insufficient material tests
        // ---------------------------------------------------------------

        #[test]
        fn insufficient_kk() {
            // Only kings: always insufficient.
            let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/8 w - - 0 1").unwrap();
            assert!(is_insufficient_material(&pos), "KK should be insufficient");
        }

        #[test]
        fn insufficient_kbk() {
            // King + Bishop vs King.
            let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/5B2 w - - 0 1").unwrap();
            assert!(is_insufficient_material(&pos), "KBK should be insufficient");
        }

        #[test]
        fn insufficient_knk() {
            // King + Knight vs King.
            let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/5N2 w - - 0 1").unwrap();
            assert!(is_insufficient_material(&pos), "KNK should be insufficient");
        }

        #[test]
        fn insufficient_kbkb_same_colour() {
            // K + B (light sq) vs K + B (light sq): same-colour bishops are
            // insufficient. Bishops on c1 and c8 are both on dark squares
            // ((file+rank) & 1: c=2, 1=0 → 0; c=2, 8=7 → 1, that's not same...).
            // Let's use bishops on f1 (light) and f8 (light): f=5, 1=0 → odd=light; f=5, 8=7→ even=dark.
            // Actually: square colour = (file_idx + rank_idx) % 2.
            // f1: file=5, rank=0, sum=5 → odd = dark square
            // f8: file=5, rank=7, sum=12 → even = light square. Different!
            // Let's use c1 and a3: c=2, 1=0, sum=2→even=light; a=0, 3=2, sum=2→even=light. Same!
            // FEN for Kc3 Bc1 vs Ke5 Ba3: "8/8/8/4k3/8/b1K5/8/2B5 w - - 0 1"
            let pos = Position::from_fen("8/8/8/4k3/8/b1K5/8/2B5 w - - 0 1").unwrap();
            assert!(
                is_insufficient_material(&pos),
                "KBKB same-colour should be insufficient"
            );
        }

        #[test]
        fn not_insufficient_kbkb_opposite_colour() {
            // K + B (light sq) vs K + B (dark sq): different-colour bishops can
            // deliver checkmate with co-operation.
            // c1=light (file=2,rank=0,sum=2=even); d1=dark (file=3,rank=0,sum=3=odd).
            // FEN: Ke5 Bc1 vs Ke1 Bd8: too many kings on same rank. Let's use:
            // White: Kc3 Bc1, Black: Ke5 Bd8
            // c1: file=2, rank=0, sum=2 → even = light
            // d8: file=3, rank=7, sum=10 → even = light — same again!
            // Let me carefully pick: c1 (file 2, rank 0): (2+0)%2=0=light
            //                        d2 (file 3, rank 1): (3+1)%2=0=light — same
            //                        d1 (file 3, rank 0): (3+0)%2=1=dark  ← different from c1!
            // FEN: White Kc3 Bc1, Black Ke5 Bd1
            // "8/8/8/4k3/8/2K5/8/2Bb4 w - - 0 1" — b on d1, B on c1
            let pos = Position::from_fen("8/8/8/4k3/8/2K5/8/2Bb4 w - - 0 1").unwrap();
            assert!(
                !is_insufficient_material(&pos),
                "KBKB opposite-colour should NOT be insufficient"
            );
        }

        #[test]
        fn not_insufficient_knkn() {
            // K+N vs K+N: theoretically winnable per FIDE (mating positions
            // exist via cooperation). Black knight on d5, white knight on f1.
            let pos = Position::from_fen("8/8/8/3nk3/8/4K3/8/5N2 w - - 0 1").unwrap();
            assert!(
                !is_insufficient_material(&pos),
                "KNvKN should NOT be insufficient"
            );
        }

        #[test]
        fn not_insufficient_two_knights_vs_king() {
            // K+N+N vs K: a helpmate exists; not insufficient by FIDE.
            let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/4NN2 w - - 0 1").unwrap();
            assert!(
                !is_insufficient_material(&pos),
                "KNNK should NOT be insufficient"
            );
        }

        #[test]
        fn not_insufficient_kpvk() {
            let pos = Position::from_fen("8/8/8/4k3/8/4K3/4P3/8 w - - 0 1").unwrap();
            assert!(
                !is_insufficient_material(&pos),
                "KPK should NOT be insufficient"
            );
        }

        #[test]
        fn not_insufficient_krkr() {
            let pos = Position::from_fen("r7/8/8/4k3/8/4K3/8/R7 w - - 0 1").unwrap();
            assert!(
                !is_insufficient_material(&pos),
                "KRKR should NOT be insufficient"
            );
        }

        // These two tests specifically target the bishop-colour parity bug
        // where `index % 2` (file parity only) differs from `(file+rank) % 2`
        // (true square colour). a2 and b1 have different file parities (0 vs 1)
        // but are both on light squares because (0+1)%2 == (1+0)%2 == 1.

        #[test]
        fn insufficient_kbkb_same_colour_a2_b1_different_file_parity() {
            // Bw on a2 (file=0, rank=1) → light square: (0+1)%2 = 1.
            // Bb on b1 (file=1, rank=0) → light square: (1+0)%2 = 1.
            // Same colour, but different file parity → old `index % 2` formula
            // would have returned false (wrongly declaring sufficient material).
            // FEN: "8/8/8/4k3/8/8/B7/1bK5 w - - 0 1" — Bw=a2, Bb=b1, Wk=c1, Bk=e5.
            let pos = Position::from_fen("8/8/8/4k3/8/8/B7/1bK5 w - - 0 1").unwrap();
            assert!(
                is_insufficient_material(&pos),
                "KBKB same-colour (a2 light, b1 light, different file parity) \
                 must be insufficient; old index%2 formula returned false here"
            );
        }

        #[test]
        fn not_insufficient_kbkb_opposite_colour_same_file_parity() {
            // Bw on a2 (file=0, rank=1) → light square: (0+1)%2 = 1.
            // Bb on a3 (file=0, rank=2) → dark square:  (0+2)%2 = 0.
            // Different colours, same file parity → old `index % 2` formula
            // would have returned true (wrongly declaring insufficient material).
            // FEN: "8/8/8/4k3/8/b7/B7/2K5 w - - 0 1" — Bw=a2, Bb=a3, Wk=c1, Bk=e5.
            let pos = Position::from_fen("8/8/8/4k3/8/b7/B7/2K5 w - - 0 1").unwrap();
            assert!(
                !is_insufficient_material(&pos),
                "KBKB opposite-colour (a2 light, a3 dark, same file parity) \
                 must NOT be insufficient; old index%2 formula returned true here"
            );
        }

        // ---------------------------------------------------------------
        // Precedence tests
        // ---------------------------------------------------------------

        #[test]
        fn precedence_mate_over_fifty() {
            // Side to move is checkmated AND halfmove_clock=100.
            // detect_native_game_over should return Checkmate, not FiftyMove.
            // Use Fool's mate position (it is checkmate) but force halfmove=100 by
            // using a FEN with that clock value.
            // After Qh4, White is mated. We fake the halfmove clock using FEN.
            // Fool's mate FEN after 1.f3 e5 2.g4 Qh4#:
            // "rnb1kbnr/pppp1ppp/8/4p3/6Pq/5P2/PPPPP2P/RNBQKBNR w KQkq - 1 3"
            // We override halfmove clock to 100:
            let pos = Position::from_fen(
                "rnb1kbnr/pppp1ppp/8/4p3/6Pq/5P2/PPPPP2P/RNBQKBNR w KQkq - 100 3",
            )
            .unwrap();
            let history = vec![pos.zobrist()];
            let result = detect_native_game_over(&pos, &history);
            assert!(
                matches!(result, Some(GameOver::Checkmate(_))),
                "expected Checkmate, got {:?}",
                result.map(|_| "Some(...)")
            );
        }

        #[test]
        fn precedence_mate_over_threefold() {
            // Side to move is checkmated AND position appears 3× in history.
            let pos =
                Position::from_fen("rnb1kbnr/pppp1ppp/8/4p3/6Pq/5P2/PPPPP2P/RNBQKBNR w KQkq - 0 3")
                    .unwrap();
            let z = pos.zobrist();
            let history = vec![z, 99u64, z, 88u64, z]; // current z appears 3×
            let result = detect_native_game_over(&pos, &history);
            assert!(
                matches!(result, Some(GameOver::Checkmate(_))),
                "expected Checkmate (not ThreefoldRepetition)"
            );
        }

        #[test]
        fn precedence_fifty_over_threefold() {
            // halfmove_clock=100 AND threefold-claimable history.
            // Use KQK where no checkmate/stalemate exists.
            let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/7Q w - - 100 50").unwrap();
            let z = pos.zobrist();
            let history = vec![z, 99u64, z, 88u64, z];
            let result = detect_native_game_over(&pos, &history);
            assert!(
                matches!(result, Some(GameOver::FiftyMove)),
                "expected FiftyMove (not ThreefoldRepetition)"
            );
        }

        #[test]
        fn precedence_fifty_over_threefold_negative_pin() {
            // Companion to `precedence_fifty_over_threefold`: the same KQK
            // position with halfmove_clock=99 (not fifty-triggering) and the
            // same threefold-claimable history must return ThreefoldRepetition.
            // This proves the threefold detection is operational — without it,
            // the prior test could have passed against an impl that only
            // detects fifty-move and never reaches threefold at all.
            let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/7Q w - - 99 50").unwrap();
            let z = pos.zobrist();
            let history = vec![z, 99u64, z, 88u64, z];
            let result = detect_native_game_over(&pos, &history);
            assert!(
                matches!(result, Some(GameOver::ThreefoldRepetition)),
                "expected ThreefoldRepetition at halfclock=99 with threefold-claimable history; got {result:?}"
            );
        }

        #[test]
        fn precedence_threefold_over_insufficient() {
            // Threefold-claimable (current position appears 3× in history) AND
            // insufficient material (KK). Under FIDE both call the game a draw;
            // the harness's precedence determines the Termination tag value.
            let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/8 w - - 0 1").unwrap();
            let z = pos.zobrist();
            let history = vec![z, 99u64, z, 88u64, z];
            let result = detect_native_game_over(&pos, &history);
            assert!(
                matches!(result, Some(GameOver::ThreefoldRepetition)),
                "expected ThreefoldRepetition (not InsufficientMaterial)"
            );
        }

        #[test]
        fn kk_with_no_repetition_returns_insufficient_via_detect_fn() {
            // Companion to `precedence_threefold_over_insufficient`: the same
            // KK position with a non-threefold history must return
            // InsufficientMaterial. This proves the insufficient-material
            // branch is reachable through `detect_native_game_over` (the prior
            // test exercises `is_insufficient_material` directly via
            // `insufficient_kk`, but does not pin that `detect_native_game_over`
            // routes to it correctly).
            let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/8 w - - 0 1").unwrap();
            let history = vec![pos.zobrist()]; // only 1 occurrence — no threefold
            let result = detect_native_game_over(&pos, &history);
            assert!(
                matches!(result, Some(GameOver::InsufficientMaterial)),
                "expected InsufficientMaterial for KK with no threefold; got {result:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// mod pgn
// ---------------------------------------------------------------------------

mod pgn {
    //! PGN tag-roster + body emission.
    //!
    //! Move tokens are UCI long-algebraic (e.g. `e2e4`, `e7e8q`, `e1g1`).
    //! This is non-standard PGN (strict consumers expect SAN) but is used for
    //! archival inspection by harness-internal tooling. See plan §3.5.

    use super::driver::LastInfo;

    /// Seven Tag Roster plus harness extensions.
    #[derive(Debug)]
    #[allow(dead_code)]
    pub(crate) struct PgnHeader {
        pub event: String,
        pub site: String,
        pub date: String,
        pub round: u32,
        pub white: String,
        pub black: String,
        /// "1-0", "0-1", or "1/2-1/2".
        pub result: String,
        /// E.g. `"10+0.1"`. `None` → tag omitted.
        pub time_control: Option<String>,
        /// E.g. `"adjudication: insufficient material"`. `None` → tag omitted.
        pub termination: Option<String>,
        /// Non-startpos starting FEN. `None` → `[FEN …]` / `[SetUp "1"]` omitted.
        pub setup_fen: Option<String>,
    }

    /// A single half-move with its associated info snapshot.
    #[derive(Debug)]
    #[allow(dead_code)]
    pub(crate) struct PgnMove {
        /// UCI move string.
        pub uci: String,
        /// Info from the engine that chose this move. `None` → no `{…}` comment.
        pub last_info: Option<LastInfo>,
    }

    /// Emit a complete PGN string from a header + move list.
    ///
    /// Format:
    /// ```text
    /// [Event "..."]
    /// ...
    /// 1. e2e4 {depth=12 score=cp 35 time=237} e7e5 {...}
    /// ...
    /// 1-0
    /// ```
    pub(crate) fn format_pgn(header: &PgnHeader, moves: &[PgnMove]) -> String {
        use super::driver::Score;

        let mut out = String::new();

        // Seven Tag Roster — mandatory, in this exact order.
        out.push_str(&format!("[Event \"{}\"]\n", header.event));
        out.push_str(&format!("[Site \"{}\"]\n", header.site));
        out.push_str(&format!("[Date \"{}\"]\n", header.date));
        out.push_str(&format!("[Round \"{}\"]\n", header.round));
        out.push_str(&format!("[White \"{}\"]\n", header.white));
        out.push_str(&format!("[Black \"{}\"]\n", header.black));
        out.push_str(&format!("[Result \"{}\"]\n", header.result));

        // Optional extension tags.
        if let Some(tc) = &header.time_control {
            out.push_str(&format!("[TimeControl \"{}\"]\n", tc));
        }
        if let Some(term) = &header.termination {
            out.push_str(&format!("[Termination \"{}\"]\n", term));
        }
        if let Some(fen) = &header.setup_fen {
            out.push_str(&format!("[FEN \"{}\"]\n", fen));
            out.push_str("[SetUp \"1\"]\n");
        }

        // Blank line separating header from body.
        out.push('\n');

        // Move body: emit move pairs with numbers and optional comments.
        let format_comment = |info: &Option<LastInfo>| -> String {
            let Some(li) = info else { return String::new() };
            let (Some(depth), Some(score), Some(time_ms)) =
                (li.depth, li.score.as_ref(), li.time_ms)
            else {
                return String::new();
            };
            let score_str = match score {
                Score::Cp(n) => format!("score=cp {n}"),
                Score::Mate(n) => format!("score=mate {n}"),
            };
            format!(" {{depth={depth} {score_str} time={time_ms}}}")
        };

        let mut i = 0;
        while i < moves.len() {
            let move_number = i / 2 + 1;
            let white_move = &moves[i];
            let white_comment = format_comment(&white_move.last_info);

            if i + 1 < moves.len() {
                let black_move = &moves[i + 1];
                let black_comment = format_comment(&black_move.last_info);
                out.push_str(&format!(
                    "{}. {}{} {}{}",
                    move_number, white_move.uci, white_comment, black_move.uci, black_comment,
                ));
                i += 2;
                if i < moves.len() {
                    out.push(' ');
                }
            } else {
                // Odd-length list: trailing white move with no black reply.
                out.push_str(&format!(
                    "{}. {}{}",
                    move_number, white_move.uci, white_comment,
                ));
                i += 1;
            }
        }

        // Result marker at end of body.
        if !moves.is_empty() {
            out.push(' ');
        }
        out.push_str(&header.result);
        out.push('\n');

        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::driver::{LastInfo, Score};

        fn base_header(result: &str) -> PgnHeader {
            PgnHeader {
                event: "Test event".into(),
                site: "localhost".into(),
                date: "2026.04.29".into(),
                round: 1,
                white: "clawfish".into(),
                black: "opponent".into(),
                result: result.into(),
                time_control: Some("10+0.1".into()),
                termination: None,
                setup_fen: None,
            }
        }

        fn info_with(depth: u32, score_cp: i32, time_ms: u64) -> Option<LastInfo> {
            Some(LastInfo {
                depth: Some(depth),
                score: Some(Score::Cp(score_cp)),
                time_ms: Some(time_ms),
            })
        }

        #[test]
        fn pgn_white_wins_startpos_formats_to_seven_tag_roster_plus_comments() {
            let header = base_header("1-0");
            let moves = vec![
                PgnMove {
                    uci: "e2e4".into(),
                    last_info: info_with(12, 35, 237),
                },
                PgnMove {
                    uci: "e7e5".into(),
                    last_info: info_with(11, -32, 205),
                },
                PgnMove {
                    uci: "d1h5".into(),
                    last_info: info_with(10, 200, 180),
                },
                PgnMove {
                    uci: "e8e7".into(),
                    last_info: info_with(9, -500, 160),
                },
            ];
            let pgn = format_pgn(&header, &moves);

            // Mandatory tags: properly quoted with literal value.
            assert!(
                pgn.contains(r#"[Event "Test event"]"#),
                "Event tag missing/malformed; got:\n{pgn}"
            );
            assert!(
                pgn.contains(r#"[Site "localhost"]"#),
                "Site tag missing/malformed"
            );
            assert!(
                pgn.contains(r#"[Date "2026.04.29"]"#),
                "Date tag missing/malformed"
            );
            assert!(
                pgn.contains(r#"[Round "1"]"#),
                "Round tag missing/malformed"
            );
            assert!(
                pgn.contains(r#"[White "clawfish"]"#),
                "White tag missing/malformed"
            );
            assert!(
                pgn.contains(r#"[Black "opponent"]"#),
                "Black tag missing/malformed"
            );
            assert!(
                pgn.contains(r#"[Result "1-0"]"#),
                "Result tag missing/malformed"
            );
            assert!(
                pgn.contains(r#"[TimeControl "10+0.1"]"#),
                "TimeControl tag missing/malformed"
            );

            // Move ordering: e2e4 must precede e7e5 must precede d1h5 must precede e8e7.
            let p_e2e4 = pgn.find("e2e4").expect("missing e2e4");
            let p_e7e5 = pgn.find("e7e5").expect("missing e7e5");
            let p_d1h5 = pgn.find("d1h5").expect("missing d1h5");
            let p_e8e7 = pgn.find("e8e7").expect("missing e8e7");
            assert!(
                p_e2e4 < p_e7e5 && p_e7e5 < p_d1h5 && p_d1h5 < p_e8e7,
                "move order corrupted; positions e2e4={p_e2e4} e7e5={p_e7e5} d1h5={p_d1h5} e8e7={p_e8e7}"
            );

            // Move-numbered prefixes for the white moves.
            assert!(
                pgn.contains("1. e2e4") || pgn.contains("1.e2e4"),
                "move number prefix '1.' missing for first move"
            );
            assert!(
                pgn.contains("2. d1h5") || pgn.contains("2.d1h5"),
                "move number prefix '2.' missing for second white move"
            );

            // Per-move comment is the FULL `{depth=N score=cp X time=T}` block,
            // not just an isolated `depth=N`.  Pin the exact comment shape on
            // the first move (depth 12, score cp 35, time 237).
            let comment_re_e2e4 = "{depth=12 score=cp 35 time=237}";
            assert!(
                pgn.contains(comment_re_e2e4),
                "expected exact comment {comment_re_e2e4:?} on e2e4; got:\n{pgn}"
            );

            // The comment must be attached to the right move (appears after
            // e2e4 and before e7e5 in document order).
            let p_comment = pgn.find(comment_re_e2e4).expect("comment present?");
            assert!(
                p_e2e4 < p_comment && p_comment < p_e7e5,
                "e2e4 comment is not attached to e2e4"
            );

            // Negative-score format (cp -32 on e7e5) — confirm minus sign rendered correctly.
            assert!(
                pgn.contains("score=cp -32"),
                "expected score=cp -32 in body; got:\n{pgn}"
            );

            // Result marker at the end (ignore optional trailing whitespace).
            assert!(
                pgn.trim_end().ends_with("1-0"),
                "PGN body must end with the result marker '1-0'; trimmed end: {:?}",
                &pgn[pgn.len().saturating_sub(20)..]
            );
        }

        #[test]
        fn pgn_black_wins_with_termination_tag() {
            let mut header = base_header("0-1");
            header.termination = Some("adjudication: insufficient material".into());
            let pgn = format_pgn(&header, &[]);
            assert!(pgn.contains("[Termination "), "missing Termination tag");
            assert!(
                pgn.contains("insufficient material"),
                "wrong termination value"
            );
            assert!(pgn.contains("0-1"), "missing result marker");
        }

        #[test]
        fn pgn_setup_tag_omitted_for_startpos() {
            let header = base_header("1/2-1/2");
            let pgn = format_pgn(&header, &[]);
            assert!(
                !pgn.contains("[FEN "),
                "FEN tag should be absent for startpos"
            );
            assert!(
                !pgn.contains("[SetUp "),
                "SetUp tag should be absent for startpos"
            );
        }

        #[test]
        fn pgn_move_comment_omitted_when_lastinfo_none() {
            let header = base_header("1-0");
            let moves = vec![
                PgnMove {
                    uci: "e2e4".into(),
                    last_info: None,
                },
                PgnMove {
                    uci: "e7e5".into(),
                    last_info: None,
                },
            ];
            let pgn = format_pgn(&header, &moves);
            assert!(
                !pgn.contains('{'),
                "no move comment expected when last_info is None"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// mod summary
// ---------------------------------------------------------------------------

mod summary {
    //! Per-run summary.txt aggregation.

    use std::path::Path;

    /// One line in `summary.txt`.
    #[derive(Debug)]
    #[allow(dead_code)]
    pub(crate) struct SummaryLine {
        pub game_index: u32,
        pub white: String,
        pub black: String,
        /// "1-0", "0-1", or "1/2-1/2".
        pub result: String,
        pub plies: u32,
        /// Human-readable termination reason.
        pub termination: String,
    }

    /// Append a summary line to `path` (tab-separated, one line per game).
    pub(crate) fn append_summary_line(path: &Path, line: &SummaryLine) -> std::io::Result<()> {
        use std::fs::OpenOptions;
        use std::io::Write;
        let mut f = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(
            f,
            "{}\t{}\t{}\t{}\t{}\t{}",
            line.game_index, line.white, line.black, line.result, line.plies, line.termination
        )?;
        f.flush()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// mod match_loop
// ---------------------------------------------------------------------------

mod match_loop {
    //! Colour-paired game loop, per-side clock management, and time-forfeit logic.
    //!
    //! The main entry point is `play_one_game`. A `pure_apply_move_clock_update`
    //! helper is factored out for deterministic unit testing with synthetic Instants.

    use std::time::{Duration, Instant};

    use clawfish::{Color, MatchTimeMode, PerSideClock};

    use super::adjudicate::GameOver;

    /// Outcome of a single game.
    #[derive(Debug)]
    #[allow(dead_code)]
    pub(crate) enum GameOutcome {
        /// Normal termination (mate, stalemate, 50-move, threefold, insufficient).
        NativeGameOver(GameOver),
        /// A player exceeded their time allowance.
        TimeForfeit(Color),
        /// A player submitted an illegal or unparseable move.
        IllegalMove(Color),
        /// The game hit the maximum-plies ceiling without a native game-over.
        MaxMovesReached,
    }

    /// Per-game state threaded through the match loop.
    #[allow(dead_code)]
    pub(crate) struct GameContext<'a> {
        /// Index of this game within the run (1-based for PGN [Round]).
        pub game_index: u32,
        /// Engine playing White (index 0 = primary engine, 1 = opponent).
        pub white_engine_index: usize,
        /// Handle to the primary engine (index 0).
        pub engine: &'a mut super::driver::EngineHandle,
        /// Handle to the opponent engine (index 1).
        pub opponent: &'a mut super::driver::EngineHandle,
        /// Initial clock for the primary engine.
        pub engine_tc: super::cli::TimeControl,
        /// Initial clock for the opponent engine.
        pub opponent_tc: super::cli::TimeControl,
        /// Harness overhead grace in milliseconds.
        pub harness_overhead_ms: u32,
        /// Watchdog duration.
        pub watchdog: Duration,
        /// Time mode (Wallclock only in ELOH.A).
        pub mode: MatchTimeMode,
        /// Maximum half-moves before forcing MaxMovesReached.
        pub max_plies: u32,
        /// FEN of the starting position (None = startpos).
        pub starting_fen: Option<String>,
        /// Current clock state for the White player (indexed by colour, not engine).
        pub white_clock: PerSideClock,
        /// Current clock state for the Black player.
        pub black_clock: PerSideClock,
    }

    /// Post-move clock state returned by `pure_apply_move_clock_update`.
    #[allow(dead_code)]
    pub(crate) struct ClockUpdate {
        /// New remaining + increment applied.
        pub new_clock: PerSideClock,
        /// True if the engine forfeited on time.
        pub forfeited: bool,
    }

    /// Pure clock-update helper: given the clock state before the move, the
    /// instants when `go` was sent and `bestmove` arrived, and the grace budget,
    /// return the updated clock and whether a time forfeit occurred.
    ///
    /// Forfeit condition: `remaining_ms < -i64::from(harness_overhead_ms)` after
    /// applying the deduction and increment.
    ///
    /// This function is factored out of `play_one_game` so unit tests can inject
    /// synthetic `Instant` pairs without spawning real engines or sleeping.
    pub(super) fn pure_apply_move_clock_update(
        prior_clock: PerSideClock,
        t_go: Instant,
        t_bestmove: Instant,
        harness_overhead_ms: u32,
    ) -> ClockUpdate {
        let elapsed_ms = t_bestmove.duration_since(t_go).as_millis() as i64;
        let new_remaining =
            prior_clock.remaining_ms - elapsed_ms + i64::from(prior_clock.increment_ms);
        let forfeited = new_remaining < -i64::from(harness_overhead_ms);
        ClockUpdate {
            new_clock: PerSideClock {
                remaining_ms: new_remaining,
                increment_ms: prior_clock.increment_ms,
            },
            forfeited,
        }
    }

    /// Play one game between two engines and return the outcome.
    ///
    /// Both engines must have been sent `ucinewgame` + `isready` (and received
    /// `readyok`) before this call. The engines are left running; the caller
    /// handles shutdown.
    ///
    /// Returns a `(GameOutcome, Vec<PgnMove>)` pair — the move list is
    /// used by the caller to emit PGN.
    pub(crate) fn play_one_game(
        ctx: &mut GameContext<'_>,
    ) -> (GameOutcome, Vec<super::pgn::PgnMove>) {
        use super::adjudicate::detect_native_game_over;
        use super::driver::{recv_until_bestmove, send_line};
        use super::pgn::PgnMove;
        use clawfish::{Color, Move, Position, generate_moves};

        let starting_pos = match &ctx.starting_fen {
            Some(fen) => Position::from_fen(fen).expect("invalid starting FEN in GameContext"),
            None => Position::starting_position(),
        };

        let mut position = starting_pos;
        let mut history: Vec<u64> = vec![position.zobrist()];
        let mut moves_uci: Vec<String> = Vec::new();
        let mut pgn_moves: Vec<PgnMove> = Vec::new();
        let mut move_count = 0u32;

        // Per-colour clocks are reset from the context at game start.
        let mut white_clock = ctx.white_clock;
        let mut black_clock = ctx.black_clock;

        loop {
            let side = position.side_to_move();

            // Determine which engine handle corresponds to the side to move.
            let (active_handle, active_color) =
                if (side == Color::White) == (ctx.white_engine_index == 0) {
                    (&mut *ctx.engine, side)
                } else {
                    (&mut *ctx.opponent, side)
                };

            // Construct the position command.
            let pos_cmd = if moves_uci.is_empty() {
                match &ctx.starting_fen {
                    Some(fen) => format!("position fen {fen}"),
                    None => "position startpos".into(),
                }
            } else {
                let moves_str = moves_uci.join(" ");
                match &ctx.starting_fen {
                    Some(fen) => format!("position fen {fen} moves {moves_str}"),
                    None => format!("position startpos moves {moves_str}"),
                }
            };

            // Reset last_info before issuing go so stale data doesn't leak.
            active_handle.last_info = super::driver::LastInfo::default();

            let _ = send_line(active_handle, &pos_cmd);

            // Build and send the go command.
            let go_cmd = ctx.mode.format_go_command(white_clock, black_clock);
            let _ = send_line(active_handle, &go_cmd);

            let t_go = Instant::now();

            let bm_result = recv_until_bestmove(active_handle, ctx.watchdog);
            let t_bestmove = Instant::now();

            let bm = match bm_result {
                Ok(bm) => bm,
                Err(_) => {
                    // Watchdog or engine exit — treat as illegal move / forfeit.
                    return (GameOutcome::IllegalMove(active_color), pgn_moves);
                }
            };

            // Time forfeit check (Wallclock mode only).
            if should_apply_clock_update(ctx.mode) {
                let prior_clock = if side == Color::White {
                    white_clock
                } else {
                    black_clock
                };
                let update = pure_apply_move_clock_update(
                    prior_clock,
                    t_go,
                    t_bestmove,
                    ctx.harness_overhead_ms,
                );
                if update.forfeited {
                    return (GameOutcome::TimeForfeit(side), pgn_moves);
                }
                if side == Color::White {
                    white_clock = update.new_clock;
                } else {
                    black_clock = update.new_clock;
                }
            }

            // Validate and apply the move to our local position.
            let mv = match Move::from_uci(&bm.uci, &position) {
                Ok(mv) => {
                    // Confirm it's actually legal.
                    let mut legal = clawfish::MoveList::new();
                    generate_moves(&position, &mut legal);
                    if !legal.iter().any(|m| m == mv) {
                        return (GameOutcome::IllegalMove(active_color), pgn_moves);
                    }
                    mv
                }
                Err(_) => return (GameOutcome::IllegalMove(active_color), pgn_moves),
            };

            // Snapshot last_info for the PGN comment before advancing.
            let info_snapshot = if active_handle.last_info.depth.is_some() {
                Some(active_handle.last_info.clone())
            } else {
                None
            };

            position.make_move(mv);
            history.push(position.zobrist());
            moves_uci.push(bm.uci.clone());
            pgn_moves.push(PgnMove {
                uci: bm.uci,
                last_info: info_snapshot,
            });

            move_count += 1;

            // Check native game-over after the move.
            if let Some(go) = detect_native_game_over(&position, &history) {
                return (GameOutcome::NativeGameOver(go), pgn_moves);
            }

            // Guard against runaway games.
            if move_count >= ctx.max_plies {
                return (GameOutcome::MaxMovesReached, pgn_moves);
            }
        }
    }

    /// Per-move clock-update gate: returns `true` iff the per-move forfeit
    /// detection branch should run (which calls `pure_apply_move_clock_update`).
    ///
    /// `Wallclock` mode runs the gate. `Nodes(_)` mode skips it entirely —
    /// per-side clock tracking is meaningless under fixed-node search.
    ///
    /// Factored out so the gate's boolean is unit-testable without standing up
    /// a real `play_one_game` call (which requires spawned engines, FENs, …).
    pub(crate) fn should_apply_clock_update(mode: MatchTimeMode) -> bool {
        match mode {
            MatchTimeMode::Wallclock => true,
            MatchTimeMode::Nodes(_) => false,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use clawfish::MatchTimeMode;

        fn clock(remaining_ms: i64, increment_ms: u32) -> PerSideClock {
            PerSideClock {
                remaining_ms,
                increment_ms,
            }
        }

        /// Make a synthetic (t_go, t_bestmove) pair where elapsed = elapsed_ms.
        fn synthetic_instants(elapsed_ms: u64) -> (Instant, Instant) {
            let t_go = Instant::now();
            let t_bestmove = t_go + Duration::from_millis(elapsed_ms);
            (t_go, t_bestmove)
        }

        #[test]
        fn time_forfeit_when_wallclock_exceeds_remaining_plus_grace() {
            // remaining=100, elapsed=200, grace=50
            // new_remaining = 100 - 200 + 0 = -100
            // forfeit iff -100 < -50 → yes
            let prior = clock(100, 0);
            let (t_go, t_bm) = synthetic_instants(200);
            let update = pure_apply_move_clock_update(prior, t_go, t_bm, 50);
            assert!(update.forfeited, "expected forfeit");
        }

        #[test]
        fn no_forfeit_when_wallclock_within_grace_window() {
            // remaining=100, elapsed=130, grace=50
            // new_remaining = 100 - 130 + 0 = -30
            // forfeit iff -30 < -50 → no
            let prior = clock(100, 0);
            let (t_go, t_bm) = synthetic_instants(130);
            let update = pure_apply_move_clock_update(prior, t_go, t_bm, 50);
            assert!(!update.forfeited, "expected no forfeit within grace");
        }

        #[test]
        fn pure_apply_move_clock_update_signature_excludes_engine_time() {
            // Structural invariant: `pure_apply_move_clock_update`'s signature
            // is `(PerSideClock, Instant, Instant, u32) -> ClockUpdate` — it
            // takes ONLY harness-measured Instants, not any engine-reported
            // time. A future refactor that added an engine-time parameter
            // would silently make engine-driven forfeit suppression possible.
            //
            // This test makes the structural contract a behavioural pin: a
            // wallclock overrun MUST trigger forfeit, regardless of any
            // imaginable engine claim, because the function has no input
            // channel through which the engine could lie. We exercise the
            // overrun branch and confirm forfeit fires.
            //
            // Companion: if a future change adds an engine-time parameter,
            // this test no longer pins the property — but a typecheck failure
            // here on the signature would surface the regression first.
            let prior = clock(100, 0);
            let (t_go, t_bm) = synthetic_instants(200);
            let update = pure_apply_move_clock_update(prior, t_go, t_bm, 50);
            assert!(
                update.forfeited,
                "harness-wallclock overrun must trigger forfeit independent of engine reports"
            );

            // Verify the signature has exactly 4 params via a function-pointer
            // bind: if anyone adds a 5th param (e.g. engine_reported_time),
            // this line fails to compile.
            let _signature_check: fn(PerSideClock, Instant, Instant, u32) -> ClockUpdate =
                pure_apply_move_clock_update;
        }

        #[test]
        fn increment_credited_after_each_move() {
            // remaining=500, elapsed=200, increment=100
            // new_remaining = 500 - 200 + 100 = 400
            let prior = clock(500, 100);
            let (t_go, t_bm) = synthetic_instants(200);
            let update = pure_apply_move_clock_update(prior, t_go, t_bm, 50);
            assert_eq!(update.new_clock.remaining_ms, 400);
            assert!(!update.forfeited);
        }

        #[test]
        fn clock_arithmetic_can_go_negative_within_grace() {
            // remaining=100, elapsed=130, inc=0, grace=50
            // new_remaining = -30; forfeit iff -30 < -50 → no
            let prior = clock(100, 0);
            let (t_go, t_bm) = synthetic_instants(130);
            let update = pure_apply_move_clock_update(prior, t_go, t_bm, 50);
            assert_eq!(update.new_clock.remaining_ms, -30);
            assert!(
                !update.forfeited,
                "negative-but-within-grace must not forfeit"
            );
        }

        #[test]
        fn should_apply_clock_update_wallclock_mode_returns_true() {
            assert!(should_apply_clock_update(MatchTimeMode::Wallclock));
        }

        #[test]
        fn should_apply_clock_update_nodes_mode_returns_false() {
            // Pins the runtime gate: in Nodes mode, the per-move forfeit
            // detection branch is skipped (does not call
            // pure_apply_move_clock_update). A buggy `play_one_game` that
            // ran the forfeit branch unconditionally would forfeit healthy
            // nodes-mode games on any positive wallclock elapsed.
            assert!(!should_apply_clock_update(MatchTimeMode::Nodes(10_000)));
            assert!(!should_apply_clock_update(MatchTimeMode::Nodes(0)));
            assert!(!should_apply_clock_update(MatchTimeMode::Nodes(u64::MAX)));
        }
    }
}

// ---------------------------------------------------------------------------
// End-to-end smoke (integration, #[ignore]-gated)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match cli::parse_args(argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };

    if let Err(e) = std::fs::create_dir_all(&args.out_dir) {
        eprintln!("error: create out_dir {:?}: {e}", args.out_dir);
        return ExitCode::from(1);
    }
    let games_dir = std::path::Path::new(&args.out_dir).join("games");
    if let Err(e) = std::fs::create_dir_all(&games_dir) {
        eprintln!("error: create games dir {games_dir:?}: {e}");
        return ExitCode::from(1);
    }

    // Build the engine names from the binary path basenames.
    let engine_name = std::path::Path::new(&args.engine)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("engine")
        .to_owned();
    let opponent_name = std::path::Path::new(&args.opponent)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("opponent")
        .to_owned();

    // Spawn both engines.
    let mut engine_handle = match driver::spawn_engine(&driver::EngineSpec {
        name: engine_name.clone(),
        path: args.engine.clone(),
        launch_prefix: args.engine_launch_prefix.clone(),
    }) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("error: failed to spawn engine {:?}: {e:?}", args.engine);
            return ExitCode::from(1);
        }
    };
    let mut opponent_handle = match driver::spawn_engine(&driver::EngineSpec {
        name: opponent_name.clone(),
        path: args.opponent.clone(),
        launch_prefix: args.opponent_launch_prefix.clone(),
    }) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("error: failed to spawn opponent {:?}: {e:?}", args.opponent);
            let _ = driver::shutdown(engine_handle);
            return ExitCode::from(1);
        }
    };

    let handshake_timeout = std::time::Duration::from_secs(10);

    // UCI handshake for both engines.
    for handle in [&mut engine_handle, &mut opponent_handle] {
        if let Err(e) = driver::send_line(handle, "uci") {
            eprintln!("error: uci send: {e:?}");
            return ExitCode::from(1);
        }
        if let Err(e) = driver::wait_for_uciok(handle, handshake_timeout) {
            eprintln!("error: waiting for uciok: {e:?}");
            return ExitCode::from(1);
        }
    }

    // Apply engine options.
    for (name, value) in &args.engine_options {
        let cmd = format!("setoption name {name} value {value}");
        let _ = driver::send_line(&mut engine_handle, &cmd);
    }
    for (name, value) in &args.opponent_options {
        let cmd = format!("setoption name {name} value {value}");
        let _ = driver::send_line(&mut opponent_handle, &cmd);
    }

    // Drain any output produced by setoption (engines typically emit nothing,
    // but the channel must stay clean).
    let isready_timeout = std::time::Duration::from_secs(5);
    for handle in [&mut engine_handle, &mut opponent_handle] {
        if let Err(e) = driver::wait_for_readyok(handle, isready_timeout) {
            eprintln!("error: waiting for readyok after setoption: {e:?}");
            return ExitCode::from(1);
        }
    }

    let opponent_tc = args.opponent_tc_override.unwrap_or(args.tc);
    let watchdog = std::time::Duration::from_millis(args.watchdog_ms);
    let mode = clawfish::MatchTimeMode::Wallclock;

    // PGN site: hostname best-effort.
    let site = {
        let mut buf = [0u8; 64];
        get_hostname(&mut buf);
        std::ffi::CStr::from_bytes_until_nul(&buf)
            .ok()
            .and_then(|c| c.to_str().ok())
            .unwrap_or("localhost")
            .to_owned()
    };

    // Local date for PGN header.
    let date = {
        use std::time::{SystemTime, UNIX_EPOCH};
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Crude date from Unix timestamp (ignores leap seconds).
        let days_since_epoch = secs / 86400;
        unix_days_to_date_str(days_since_epoch)
    };

    let num_pairs = args.max_games / 2;
    let mut game_index = 1u32;

    for _pair_idx in 0..num_pairs {
        // Play two games (colour-swapped pair).
        for swap in [false, true] {
            // UCI spec: send ucinewgame before each game so the engine can
            // reset its per-game state (TT, history heuristic, etc.).
            let ucinewgame_ok = {
                let mut ok = true;
                for handle in [&mut engine_handle, &mut opponent_handle] {
                    let _ = driver::send_line(handle, "ucinewgame");
                    if let Err(e) = driver::wait_for_readyok(handle, isready_timeout) {
                        eprintln!("error: readyok after ucinewgame: {e:?}");
                        ok = false;
                        break;
                    }
                }
                ok
            };
            if !ucinewgame_ok {
                let _ = driver::shutdown(engine_handle);
                let _ = driver::shutdown(opponent_handle);
                return ExitCode::from(1);
            }
            // swap=false: engine plays White (index 0), opponent plays Black (index 1).
            // swap=true:  engine plays Black (index 1), opponent plays White (index 0).
            let white_engine_index = if swap { 1 } else { 0 };

            let white_tc = if swap { opponent_tc } else { args.tc };
            let black_tc = if swap { args.tc } else { opponent_tc };

            let white_clock = clawfish::PerSideClock {
                remaining_ms: i64::from(white_tc.initial_ms),
                increment_ms: white_tc.increment_ms,
            };
            let black_clock = clawfish::PerSideClock {
                remaining_ms: i64::from(black_tc.initial_ms),
                increment_ms: black_tc.increment_ms,
            };

            let (white_name, black_name) = if swap {
                (opponent_name.clone(), engine_name.clone())
            } else {
                (engine_name.clone(), opponent_name.clone())
            };

            let mut ctx = match_loop::GameContext {
                game_index,
                white_engine_index,
                engine: &mut engine_handle,
                opponent: &mut opponent_handle,
                engine_tc: args.tc,
                opponent_tc,
                harness_overhead_ms: args.harness_overhead_ms,
                watchdog,
                mode,
                max_plies: 200,
                starting_fen: None,
                white_clock,
                black_clock,
            };

            let (outcome, pgn_moves) = match_loop::play_one_game(&mut ctx);

            // Map outcome to PGN result + termination tag.
            let (result, termination) = outcome_to_pgn_result(&outcome, white_engine_index);

            // Write PGN.
            let pgn_header = pgn::PgnHeader {
                event: args.event_tag.clone(),
                site: site.clone(),
                date: date.clone(),
                round: game_index,
                white: white_name,
                black: black_name,
                result: result.clone(),
                time_control: Some(format_tc(args.tc)),
                termination: Some(termination),
                setup_fen: None,
            };
            let pgn_str = pgn::format_pgn(&pgn_header, &pgn_moves);
            let pgn_path = games_dir.join(format!("{game_index}.pgn"));
            if let Err(e) = std::fs::write(&pgn_path, &pgn_str) {
                eprintln!("error: write PGN {pgn_path:?}: {e}");
            }

            // Append summary line.
            let summary_line = summary::SummaryLine {
                game_index,
                white: pgn_header.white.clone(),
                black: pgn_header.black.clone(),
                result: result.clone(),
                plies: pgn_moves.len() as u32,
                termination: outcome_to_termination_reason(&outcome),
            };
            let summary_path = std::path::Path::new(&args.out_dir).join("summary.txt");
            if let Err(e) = summary::append_summary_line(&summary_path, &summary_line) {
                eprintln!("error: write summary: {e}");
            }

            game_index += 1;
        }
    }

    let _ = driver::shutdown(engine_handle);
    let _ = driver::shutdown(opponent_handle);

    ExitCode::SUCCESS
}

/// Map a `GameOutcome` to its human-readable termination reason string.
///
/// This is the single source of truth for termination strings — both the PGN
/// `[Termination]` tag and the summary.txt column call this function, so they
/// always agree.
pub(crate) fn outcome_to_termination_reason(outcome: &match_loop::GameOutcome) -> String {
    use adjudicate::GameOver;
    match outcome {
        match_loop::GameOutcome::NativeGameOver(go) => match go {
            GameOver::Checkmate(_) => "normal".into(),
            GameOver::Stalemate => "normal".into(),
            GameOver::FiftyMove => "adjudication: fifty-move rule".into(),
            GameOver::ThreefoldRepetition => "adjudication: threefold repetition".into(),
            GameOver::InsufficientMaterial => "adjudication: insufficient material".into(),
            GameOver::TimeForfeit(_) => "time forfeit".into(),
        },
        match_loop::GameOutcome::TimeForfeit(_) => "time forfeit".into(),
        match_loop::GameOutcome::IllegalMove(_) => "adjudication: illegal move".into(),
        match_loop::GameOutcome::MaxMovesReached => "adjudication: max moves".into(),
        // `GameOver::TimeForfeit` inside `NativeGameOver` is produced by the
        // adjudication layer if we ever route a time-forfeit through it; the
        // match_loop::GameOutcome::TimeForfeit arm above is the authoritative path.
    }
}

/// Map a `GameOutcome` to a PGN result string and termination reason.
///
/// The termination string is sourced from `outcome_to_termination_reason` so
/// the PGN `[Termination]` tag and the summary.txt column always agree.
fn outcome_to_pgn_result(
    outcome: &match_loop::GameOutcome,
    _white_engine_index: usize,
) -> (String, String) {
    use adjudicate::GameOver;
    use clawfish::Color;
    let termination = outcome_to_termination_reason(outcome);
    let result = match outcome {
        match_loop::GameOutcome::NativeGameOver(go) => match go {
            GameOver::Checkmate(winner) => match winner {
                Color::White => "1-0",
                Color::Black => "0-1",
            },
            GameOver::Stalemate
            | GameOver::FiftyMove
            | GameOver::ThreefoldRepetition
            | GameOver::InsufficientMaterial => "1/2-1/2",
            GameOver::TimeForfeit(loser) => match loser {
                Color::White => "0-1",
                Color::Black => "1-0",
            },
        },
        match_loop::GameOutcome::TimeForfeit(loser) => match loser {
            Color::White => "0-1",
            Color::Black => "1-0",
        },
        match_loop::GameOutcome::IllegalMove(offender) => match offender {
            Color::White => "0-1",
            Color::Black => "1-0",
        },
        match_loop::GameOutcome::MaxMovesReached => "1/2-1/2",
    };
    (result.into(), termination)
}

/// Format a `TimeControl` back to its canonical string form (e.g. `"10+0.1"`).
fn format_tc(tc: cli::TimeControl) -> String {
    let base_s = tc.initial_ms as f64 / 1000.0;
    let inc_s = tc.increment_ms as f64 / 1000.0;
    if tc.increment_ms == 0 {
        format!("{base_s}")
    } else {
        format!("{base_s}+{inc_s}")
    }
}

// We don't want to add `libc` as a dep; call gethostname directly.
unsafe extern "C" {
    fn gethostname(name: *mut std::ffi::c_char, len: usize) -> i32;
}

/// Fill `buf` with the null-terminated hostname via the OS `gethostname` syscall.
fn get_hostname(buf: &mut [u8]) {
    // SAFETY: buf is a valid mutable slice; u8 and c_char have the same layout.
    unsafe { gethostname(buf.as_mut_ptr() as *mut std::ffi::c_char, buf.len()) };
}

/// Cheap date-from-unix-days without a calendar crate.
///
/// Accurate for dates 2000–2099 (the project's realistic lifetime).
fn unix_days_to_date_str(days: u64) -> String {
    // Gregorian arithmetic for 2000-2099 (no century correction needed).
    // Shift epoch from 1970-01-01 to 2000-03-01 for simpler leap math.
    // Days from 1970-01-01 to 2000-03-01 = 10957 + 31 + 29 = 11017.
    let d = days.saturating_sub(11017);
    let era_cycles = d / 1461; // 4-year cycles (1461 = 365*4+1)
    let rem = d % 1461;
    let year_in_cycle = ((rem - rem / 1460) / 365).min(3);
    let year = 2000 + era_cycles * 4 + year_in_cycle;
    let day_of_year = rem - year_in_cycle * 365 - year_in_cycle / 4;
    // Month lookup table for a March-based year (March=0 … February=11).
    // February has 28 days in a common year; 29 in a leap year.
    // year_in_cycle==3 means this is the 4th year of the cycle — a leap year,
    // whose Feb is the last month and has 29 days instead of 28.
    let feb_days: u64 = if year_in_cycle == 3 { 29 } else { 28 };
    let month_days: [u64; 12] = [31, 30, 31, 30, 31, 31, 30, 31, 30, 31, 31, feb_days];
    let mut month_march = 11u64; // default to Feb (last month) if loop exhausts
    let mut remaining = day_of_year;
    for (m, &days_in_month) in month_days.iter().enumerate() {
        if remaining < days_in_month {
            month_march = m as u64;
            break;
        }
        remaining -= days_in_month;
    }
    // March = month 3 in our base; add 2 to get calendar month, wrap Jan/Feb.
    let month = (month_march + 2) % 12 + 1;
    let year = if month_march >= 10 { year + 1 } else { year };
    let day = remaining + 1;
    format!("{year:04}.{month:02}.{day:02}")
}

// ---------------------------------------------------------------------------
// Unit tests for pure helpers defined at crate root
// ---------------------------------------------------------------------------

#[cfg(test)]
mod root_tests {
    use super::*;

    // -----------------------------------------------------------------------
    // unix_days_to_date_str
    // -----------------------------------------------------------------------

    #[test]
    fn unix_days_to_date_str_jan_1_2026() {
        // 2026-01-01: 56 years past epoch (1972…2024 = 14 leap years).
        // 56*365 + 14 = 20454.
        assert_eq!(unix_days_to_date_str(20454), "2026.01.01");
    }

    #[test]
    fn unix_days_to_date_str_feb_28_2024() {
        // 2024 is a leap year; Feb 28 is the day before Feb 29.
        // 54*365 + 13 (leaps 1972…2020) = 19723; +31 (Jan) + 27 = 19781.
        assert_eq!(unix_days_to_date_str(19781), "2024.02.28");
    }

    #[test]
    fn unix_days_to_date_str_feb_29_2024() {
        // Feb 29 of a leap year — the case the old `month_march = 0` init
        // got wrong by leaving the month index at 0 (March) after the loop
        // exhausted without finding a slot (28 < 28 was false with old table).
        // 19781 + 1 = 19782.
        assert_eq!(unix_days_to_date_str(19782), "2024.02.29");
    }

    #[test]
    fn unix_days_to_date_str_mar_1_2024() {
        // Day after the leap day; first day of a new March-based year.
        // 19782 + 1 = 19783.
        assert_eq!(unix_days_to_date_str(19783), "2024.03.01");
    }

    #[test]
    fn unix_days_to_date_str_dec_31_2099() {
        // Boundary of the function's stated range (2000–2099).
        // 129*365 + 32 (leaps 1972…2096) = 47117; + 364 days in 2099 = 47481.
        assert_eq!(unix_days_to_date_str(47481), "2099.12.31");
    }

    // -----------------------------------------------------------------------
    // outcome_to_termination_reason — every GameOutcome variant
    // -----------------------------------------------------------------------

    #[test]
    fn outcome_to_termination_reason_pgn_and_summary_agree_for_every_variant() {
        use adjudicate::GameOver;
        use clawfish::Color;
        use match_loop::GameOutcome;

        // For each GameOutcome variant: verify termination reason is consistent.
        // Both the PGN [Termination] tag and summary.txt column call
        // `outcome_to_termination_reason`, so they agree by construction.
        // This test pins the string value for each variant so a future rename
        // doesn't silently change output.
        let cases: &[(&str, GameOutcome)] = &[
            (
                "normal",
                GameOutcome::NativeGameOver(GameOver::Checkmate(Color::White)),
            ),
            (
                "normal",
                GameOutcome::NativeGameOver(GameOver::Checkmate(Color::Black)),
            ),
            ("normal", GameOutcome::NativeGameOver(GameOver::Stalemate)),
            (
                "adjudication: fifty-move rule",
                GameOutcome::NativeGameOver(GameOver::FiftyMove),
            ),
            (
                "adjudication: threefold repetition",
                GameOutcome::NativeGameOver(GameOver::ThreefoldRepetition),
            ),
            (
                "adjudication: insufficient material",
                GameOutcome::NativeGameOver(GameOver::InsufficientMaterial),
            ),
            ("time forfeit", GameOutcome::TimeForfeit(Color::White)),
            ("time forfeit", GameOutcome::TimeForfeit(Color::Black)),
            (
                "adjudication: illegal move",
                GameOutcome::IllegalMove(Color::White),
            ),
            (
                "adjudication: illegal move",
                GameOutcome::IllegalMove(Color::Black),
            ),
            ("adjudication: max moves", GameOutcome::MaxMovesReached),
        ];

        for (expected, outcome) in cases {
            let reason = outcome_to_termination_reason(outcome);
            assert_eq!(
                reason, *expected,
                "outcome_to_termination_reason for {outcome:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // outcome_to_pgn_result — every GameOutcome variant
    // -----------------------------------------------------------------------

    #[test]
    fn outcome_to_pgn_result_every_variant() {
        use adjudicate::GameOver;
        use clawfish::Color;
        use match_loop::GameOutcome;

        // (expected_result, expected_termination, outcome)
        let cases: &[(&str, &str, GameOutcome)] = &[
            (
                "1-0",
                "normal",
                GameOutcome::NativeGameOver(GameOver::Checkmate(Color::White)),
            ),
            (
                "0-1",
                "normal",
                GameOutcome::NativeGameOver(GameOver::Checkmate(Color::Black)),
            ),
            (
                "1/2-1/2",
                "normal",
                GameOutcome::NativeGameOver(GameOver::Stalemate),
            ),
            (
                "1/2-1/2",
                "adjudication: fifty-move rule",
                GameOutcome::NativeGameOver(GameOver::FiftyMove),
            ),
            (
                "1/2-1/2",
                "adjudication: threefold repetition",
                GameOutcome::NativeGameOver(GameOver::ThreefoldRepetition),
            ),
            (
                "1/2-1/2",
                "adjudication: insufficient material",
                GameOutcome::NativeGameOver(GameOver::InsufficientMaterial),
            ),
            (
                "0-1",
                "time forfeit",
                GameOutcome::TimeForfeit(Color::White),
            ),
            (
                "1-0",
                "time forfeit",
                GameOutcome::TimeForfeit(Color::Black),
            ),
            (
                "0-1",
                "adjudication: illegal move",
                GameOutcome::IllegalMove(Color::White),
            ),
            (
                "1-0",
                "adjudication: illegal move",
                GameOutcome::IllegalMove(Color::Black),
            ),
            (
                "1/2-1/2",
                "adjudication: max moves",
                GameOutcome::MaxMovesReached,
            ),
        ];

        for (exp_result, exp_term, outcome) in cases {
            let (result, term) = outcome_to_pgn_result(outcome, 0);
            assert_eq!(result, *exp_result, "result for {outcome:?}");
            assert_eq!(term, *exp_term, "termination for {outcome:?}");
        }
    }

    // -----------------------------------------------------------------------
    // format_tc — time control string formatting
    // -----------------------------------------------------------------------

    #[test]
    fn format_tc_10_plus_0_1() {
        let tc = cli::TimeControl {
            initial_ms: 10_000,
            increment_ms: 100,
        };
        assert_eq!(format_tc(tc), "10+0.1");
    }

    #[test]
    fn format_tc_10_plus_0() {
        let tc = cli::TimeControl {
            initial_ms: 10_000,
            increment_ms: 0,
        };
        assert_eq!(format_tc(tc), "10");
    }

    #[test]
    fn format_tc_60_plus_0() {
        let tc = cli::TimeControl {
            initial_ms: 60_000,
            increment_ms: 0,
        };
        assert_eq!(format_tc(tc), "60");
    }

    #[test]
    fn format_tc_0_plus_0_05() {
        // Bullet: 0 base + 50 ms increment.
        let tc = cli::TimeControl {
            initial_ms: 0,
            increment_ms: 50,
        };
        assert_eq!(format_tc(tc), "0+0.05");
    }

    #[test]
    fn format_tc_asymmetric_engine_opponent() {
        // Engine TC: 5 s + 2 s increment. Used when --opponent-tc-override differs.
        let tc = cli::TimeControl {
            initial_ms: 5_000,
            increment_ms: 2_000,
        };
        assert_eq!(format_tc(tc), "5+2");
    }
}

// ---------------------------------------------------------------------------
// End-to-end smoke (integration, #[ignore]-gated)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod e2e_smoke {
    /// Path to the `clawfish` binary, resolved at test link time by Cargo.
    ///
    /// `option_env!` returns `None` when the variable is not set (e.g. during
    /// `cargo clippy --all-targets` without a prior build). The test panics at
    /// runtime with a helpful message in that case rather than failing to compile.
    const CLAWFISH_EXE: Option<&str> = option_env!("CARGO_BIN_EXE_clawfish");
    const ELO_ITERATE_EXE: Option<&str> = option_env!("CARGO_BIN_EXE_elo-iterate");

    /// Resolve a binary name from either the compile-time `CARGO_BIN_EXE_*` env var
    /// (set by Cargo for integration tests) or, as a fallback for unit-test contexts,
    /// by searching `target/release/<name>` relative to the current executable's
    /// directory hierarchy.
    fn resolve_bin(compile_time_path: Option<&str>, name: &str) -> String {
        if let Some(p) = compile_time_path {
            return p.to_owned();
        }
        // Fallback: walk up from current_exe until we find target/release/<name>.
        // Unit tests run from target/release/deps/, so ../name is usually the path.
        let exe = std::env::current_exe().expect("current_exe");
        let deps_dir = exe.parent().expect("deps dir");
        let release_dir = deps_dir.parent().expect("release dir");
        let candidate = release_dir.join(name);
        if candidate.exists() {
            return candidate.to_str().expect("valid utf8 path").to_owned();
        }
        panic!(
            "could not find binary '{name}' — build with `cargo build --release` first, \
             then run via `cargo test --release`"
        );
    }

    #[test]
    #[ignore = "spawns clawfish; opt-in via cargo test --release -- --ignored"]
    fn end_to_end_clawfish_self_play_4_games() {
        use std::process::Command;

        let engine = resolve_bin(CLAWFISH_EXE, "clawfish");
        let harness = resolve_bin(ELO_ITERATE_EXE, "elo-iterate");
        let engine = engine.as_str();
        let harness = harness.as_str();
        let out_dir = std::env::temp_dir().join("elo-iterate-smoke");
        // Remove any prior run's artefacts so the assertion counts are stable.
        let _ = std::fs::remove_dir_all(&out_dir);
        std::fs::create_dir_all(&out_dir).unwrap();

        let status = Command::new(harness)
            .args([
                "--engine",
                engine,
                "--opponent",
                engine,
                "--tc",
                "1+0.05",
                "--max-games",
                "4",
                "--out-dir",
                out_dir.to_str().unwrap(),
            ])
            .status()
            .expect("failed to spawn elo-iterate");

        assert!(status.success(), "harness exited with {status}");

        // 4 PGN files must exist.
        let games_dir = out_dir.join("games");
        let pgns: Vec<_> = std::fs::read_dir(&games_dir)
            .expect("games dir missing")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "pgn"))
            .collect();
        assert_eq!(pgns.len(), 4, "expected 4 PGN files, found {}", pgns.len());

        // summary.txt must have 4 lines.
        let summary = std::fs::read_to_string(out_dir.join("summary.txt")).unwrap();
        let non_empty_lines: Vec<_> = summary.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            non_empty_lines.len(),
            4,
            "expected 4 summary lines, found {}",
            non_empty_lines.len()
        );

        // Each PGN must contain a Termination tag.
        for entry in pgns {
            let content = std::fs::read_to_string(entry.path()).unwrap();
            assert!(
                content.contains("[Termination "),
                "PGN {:?} missing Termination tag",
                entry.path()
            );
        }
    }
}
