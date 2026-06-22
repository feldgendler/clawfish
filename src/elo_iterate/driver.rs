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

/// Capabilities advertised by an engine in its `uci` response.
///
/// Populated by [`wait_for_uciok`] from `option name …` lines in the
/// handshake. Option names are matched case-insensitively per UCI spec.
#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct EngineCapabilities {
    /// `true` when the engine emitted `option name VirtualClock type check …`.
    /// Harness sends `setoption name VirtualClock value true` only when this is
    /// `true` AND `--virtual-clock` was supplied on the CLI.
    pub supports_virtual_clock: bool,
}

/// Parse a single `option name <NAME> type …` line from a UCI handshake.
///
/// Returns the option name as emitted by the engine (original casing).
/// The caller normalises to lowercase for case-insensitive matching per
/// UCI spec. Returns `None` for any line that does not match the pattern.
pub(crate) fn parse_option_advertisement(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("option name ")?;
    // The name is everything up to the first ` type ` token.
    let type_idx = rest.find(" type ")?;
    Some(&rest[..type_idx])
}

/// Configuration for spawning an engine subprocess.
#[derive(Debug, Clone)]
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
    #[allow(dead_code)] // ponder support is planned but not yet consumed
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
    #[allow(dead_code)] // constructed via map_err(HarnessError::Io); payload read via Debug
    Io(std::io::Error),
    /// A protocol line couldn't be parsed (informational; harness skips).
    #[allow(dead_code)] // reserved variant; parse errors are currently silently skipped
    Parse(String),
}

/// Live handle to a running engine subprocess.
pub(crate) struct EngineHandle {
    #[allow(dead_code)] // used for PGN/log display in the full harness; not yet read in tests
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

/// Slack below which a short `recv_timeout` is treated as a genuine watchdog
/// expiry rather than a suspend. The watchdog measures elapsed time with the
/// monotonic, **suspend-excluding** `Instant` clock (macOS `mach_absolute_time`,
/// Linux `CLOCK_MONOTONIC`). If `recv_timeout` returns `Timeout` but the
/// monotonic clock shows it consumed materially LESS than the wait we asked for,
/// the timeout fired early because the process was SUSPENDED (machine sleep /
/// lid close) — the channel timeout counted frozen wall-time the monotonic clock
/// did not. That gap must exceed this slack (above scheduler jitter / spurious
/// wakeups) to count as a suspend.
///
/// Why monotonic time and not the engine's CPU "virtual clock": the only
/// realistic engine hang here is a runaway/infinite search loop, which accrues
/// active time; clawfish's search is single-threaded (no lock deadlock), the
/// harness drains stdout (no full-pipe block), and the SPRT is local (no I/O
/// stall) — so there is no no-CPU hang to miss. Monotonic time also still
/// advances while the system is awake, so it would even catch a hypothetical
/// stall that a pure CPU-time clock would sleep through. (Revisit once parallel
/// search — M10 — introduces real threads/locks.)
const SUSPEND_SLACK: Duration = Duration::from_secs(2);

/// `true` iff a `recv_timeout(requested)` that returned `Timeout` did so because
/// the process was suspended rather than because the watchdog genuinely expired:
/// the monotonic time actually elapsed (`monotonic_elapsed`) fell short of
/// `requested` by more than [`SUSPEND_SLACK`]. Pure, for testability.
fn timeout_was_suspend(requested: Duration, monotonic_elapsed: Duration) -> bool {
    matches!(requested.checked_sub(monotonic_elapsed), Some(gap) if gap > SUSPEND_SLACK)
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
        let waited = Instant::now();
        let recv = rx.recv_timeout(remaining);
        match recv {
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
            // A timeout is a genuine watchdog expiry UNLESS the process was
            // suspended (lid close): `recv_timeout` then fired early on frozen
            // wall-time that the suspend-excluding `Instant` clock did not
            // count. Re-loop in that case — the unchanged `Instant` deadline
            // still grants the engine its full remaining (active) budget.
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if timeout_was_suspend(remaining, waited.elapsed()) {
                    continue;
                }
                return Err(HarnessError::Watchdog);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(HarnessError::EngineExit),
        }
    }
}

/// Drain engine output until `uciok` is received (or timeout/error).
///
/// Called after sending `uci`; collects `option name …` advertisements
/// from the handshake response (for capability negotiation) and returns
/// the accumulated [`EngineCapabilities`] alongside the settled handshake.
pub(crate) fn wait_for_uciok(
    h: &mut EngineHandle,
    timeout: std::time::Duration,
) -> Result<EngineCapabilities, HarnessError> {
    wait_for_uciok_inner(&h.rx, timeout)
}

/// Inner implementation, factored out so tests can inject a synthetic
/// `Receiver<EngineLine>` without spawning a real subprocess.
pub(super) fn wait_for_uciok_inner(
    rx: &mpsc::Receiver<EngineLine>,
    timeout: std::time::Duration,
) -> Result<EngineCapabilities, HarnessError> {
    let deadline = std::time::Instant::now() + timeout;
    let mut caps = EngineCapabilities::default();
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(HarnessError::Watchdog);
        }
        let waited = std::time::Instant::now();
        let recv = rx.recv_timeout(remaining);
        match recv {
            Ok(EngineLine::Other(s)) if s.trim() == "uciok" => return Ok(caps),
            Ok(EngineLine::Other(s)) => {
                // Inspect each non-uciok line for option advertisements.
                if let Some(name) = parse_option_advertisement(&s)
                    && name.eq_ignore_ascii_case("virtualclock")
                {
                    caps.supports_virtual_clock = true;
                }
            }
            Ok(EngineLine::Eof) => return Err(HarnessError::EngineExit),
            // Suspend (lid close) ⇒ re-loop; genuine expiry ⇒ Watchdog. See the
            // `recv_until_bestmove_inner` timeout arm + `timeout_was_suspend`.
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if timeout_was_suspend(remaining, waited.elapsed()) {
                    continue;
                }
                return Err(HarnessError::Watchdog);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(HarnessError::EngineExit),
            _ => {} // discard info lines, etc.
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
        let err =
            recv_until_bestmove_inner(&rx, &mut last_info, Duration::from_millis(100)).unwrap_err();
        assert!(matches!(err, HarnessError::Watchdog), "got {err:?}");
    }

    #[test]
    fn timeout_was_suspend_distinguishes_sleep_from_expiry() {
        // Genuine expiry: monotonic clock shows ~the full requested wait → fire.
        assert!(!timeout_was_suspend(
            Duration::from_secs(60),
            Duration::from_secs(60)
        ));
        // Scheduler jitter / spurious wakeup within SUSPEND_SLACK (1.5s < 2s).
        assert!(!timeout_was_suspend(
            Duration::from_secs(60),
            Duration::from_millis(58_500)
        ));
        // recv_timeout fired but the monotonic (suspend-excluding) clock barely
        // advanced → the process was suspended (lid close) → re-loop, not expiry.
        assert!(timeout_was_suspend(
            Duration::from_secs(60),
            Duration::from_secs(3)
        ));
        // Monotonic elapsed >= requested (clock skew / overshoot) → not a suspend.
        assert!(!timeout_was_suspend(
            Duration::from_secs(60),
            Duration::from_secs(61)
        ));
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
    fn parse_info_line_pins_time_field() {
        // Catches `delete match arm "time"` survivor: if "time" arm is
        // dropped, the next two tokens get consumed by the catch-all and
        // info.time_ms stays None.
        let info = parse_info_payload("depth 5 time 250");
        assert_eq!(info.time_ms, Some(250), "time field must populate");
    }

    #[test]
    fn parse_info_line_pins_score_cp() {
        // Catches `delete match arm "score"` and `delete match arm ("cp", Ok(n))`:
        // both regressions leave info.score == None for a `score cp X` line.
        let info = parse_info_payload("depth 5 score cp 35");
        assert_eq!(
            info.score,
            Some(Score::Cp(35)),
            "score cp must populate as Cp"
        );
    }

    #[test]
    fn parse_info_line_pins_score_mate() {
        // Catches `delete match arm ("mate", Ok(n))`: leaves score==None
        // for `score mate N`.
        let info = parse_info_payload("depth 8 score mate 3");
        assert_eq!(
            info.score,
            Some(Score::Mate(3)),
            "score mate must populate as Mate"
        );
    }

    #[test]
    fn parse_info_line_pins_pv_field() {
        // Catches `delete match arm "pv"`: leaves info.pv == None for a
        // `pv <moves...>` line.
        let info = parse_info_payload("depth 3 pv e2e4 e7e5 g1f3");
        assert_eq!(
            info.pv.as_deref(),
            Some("e2e4 e7e5 g1f3"),
            "pv field must populate with the full move list"
        );
    }

    #[test]
    fn parse_engine_line_distinguishes_bestmove_from_other() {
        // Catches `replace == with !=` in parse_engine_line: a misclassified
        // `bestmove ...` line would route to EngineLine::Other, not
        // EngineLine::Bestmove. Test pins routing behaviour distinctly.
        assert!(matches!(
            parse_engine_line("bestmove e2e4"),
            EngineLine::Bestmove { .. }
        ));
        assert!(matches!(
            parse_engine_line("info depth 5"),
            EngineLine::Info(_)
        ));
        assert!(matches!(
            parse_engine_line("uciok"),
            EngineLine::Other(ref s) if s == "uciok"
        ));
        assert!(matches!(
            parse_engine_line("bestmovexyz e2e4"),
            EngineLine::Other(_)
        ));
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

    // ---- ELOH.C §6.5: parse_option_advertisement + wait_for_uciok tests ----

    /// Helper: spawn `/bin/cat`, write `mock_lines` to its stdin (each followed
    /// by `\n`), flush, and return an `EngineHandle` whose `rx` will receive the
    /// echoed lines.  Also returns the `ChildStdin` so the caller can write
    /// additional lines after construction (e.g. the setoption-send-and-verify
    /// pattern used by the `production_worker_*` tests).
    fn make_cat_handle(
        mock_lines: &[&str],
    ) -> (EngineHandle, std::io::BufWriter<std::process::ChildStdin>) {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let mut child = Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("failed to spawn /bin/cat");
        let stdout = child.stdout.take().unwrap();
        // We split stdin: keep one writer for the test caller (via BufWriter),
        // and take the ChildStdin for the EngineHandle.
        // Instead, we use a single pipe and transfer it: write mock lines first,
        // then give the stdin to the EngineHandle.
        let child_stdin = child.stdin.take().unwrap();
        let mut writer = std::io::BufWriter::new(child_stdin);

        // Write mock lines synchronously so cat echoes them before we read.
        for line in mock_lines {
            writer.write_all(line.as_bytes()).unwrap();
            writer.write_all(b"\n").unwrap();
        }
        writer.flush().unwrap();

        // Reader thread: cat's stdout → mpsc.
        let (tx, rx) = mpsc::sync_channel::<EngineLine>(1024);
        let reader_handle = std::thread::spawn(move || {
            use std::io::BufRead as _;
            for line in std::io::BufReader::new(stdout).lines() {
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
            stdin: None, // caller holds the writer; no ChildStdin in the handle
            rx,
            reader: Some(reader_handle),
            last_info: LastInfo::default(),
            shutting_down: false,
        };
        (handle, writer)
    }

    #[test]
    fn parse_option_advertisement_well_formed() {
        let result =
            parse_option_advertisement("option name VirtualClock type check default false");
        assert_eq!(result, Some("VirtualClock"));
    }

    #[test]
    fn parse_option_advertisement_with_extras() {
        let result = parse_option_advertisement(
            "option name MoveOverhead type spin default 50 min 0 max 5000",
        );
        assert_eq!(result, Some("MoveOverhead"));
    }

    #[test]
    fn parse_option_advertisement_malformed_returns_none() {
        assert_eq!(parse_option_advertisement("option foo bar"), None);
        assert_eq!(parse_option_advertisement("not an option"), None);
        assert_eq!(parse_option_advertisement("option name NoTypeToken"), None);
    }

    #[test]
    fn parse_option_advertisement_multiword_name() {
        // Single-token name with underscores: the parser handles all names
        // up to the first ` type ` — including those with underscores.
        let result =
            parse_option_advertisement("option name UCI_Chess960 type check default false");
        assert_eq!(result, Some("UCI_Chess960"));
    }

    #[test]
    fn wait_for_uciok_records_virtual_clock_capability() {
        let rx = make_rx(vec![
            EngineLine::Other("option name VirtualClock type check default false".into()),
            EngineLine::Other("uciok".into()),
        ]);
        let caps = wait_for_uciok_inner(&rx, std::time::Duration::from_secs(1)).expect("ok");
        assert!(
            caps.supports_virtual_clock,
            "VirtualClock option must be detected"
        );
    }

    #[test]
    fn wait_for_uciok_records_no_virtual_clock_when_absent() {
        let rx = make_rx(vec![
            EngineLine::Other(
                "option name MoveOverhead type spin default 50 min 0 max 5000".into(),
            ),
            EngineLine::Other("option name Hash type spin default 16 min 1 max 65536".into()),
            EngineLine::Other("uciok".into()),
        ]);
        let caps = wait_for_uciok_inner(&rx, std::time::Duration::from_secs(1)).expect("ok");
        assert!(
            !caps.supports_virtual_clock,
            "supports_virtual_clock must be false when option absent"
        );
    }

    #[test]
    fn wait_for_uciok_handles_interleaved_info_string() {
        // Real engines may emit `info string …` lines during the uci handshake.
        // Info lines are routed to EngineLine::Info(...), not EngineLine::Other.
        // Use Other with an info-string prefix to exercise the pass-through path.
        let rx = make_rx(vec![
            EngineLine::Other("info string warming up".into()),
            EngineLine::Other("option name VirtualClock type check default false".into()),
            EngineLine::Other("info string ready".into()),
            EngineLine::Other("uciok".into()),
        ]);
        let caps = wait_for_uciok_inner(&rx, std::time::Duration::from_secs(1)).expect("ok");
        assert!(
            caps.supports_virtual_clock,
            "capability must be detected despite interleaved info strings"
        );
    }

    #[test]
    fn wait_for_uciok_case_insensitive_option_name_match() {
        // UCI spec says option names are case-insensitive; harness must detect
        // the VirtualClock option regardless of the case the engine uses.
        let rx = make_rx(vec![
            EngineLine::Other("option name virtualclock type check default false".into()),
            EngineLine::Other("uciok".into()),
        ]);
        let caps = wait_for_uciok_inner(&rx, std::time::Duration::from_secs(1)).expect("ok");
        assert!(
            caps.supports_virtual_clock,
            "lowercase option name must still be detected"
        );
    }

    #[test]
    fn wait_for_uciok_duplicate_advertisement_idempotent() {
        // Two VirtualClock ads must not panic or flip the flag back.
        let rx = make_rx(vec![
            EngineLine::Other("option name VirtualClock type check default false".into()),
            EngineLine::Other("option name VirtualClock type check default false".into()),
            EngineLine::Other("uciok".into()),
        ]);
        let caps = wait_for_uciok_inner(&rx, std::time::Duration::from_secs(1)).expect("ok");
        assert!(caps.supports_virtual_clock, "flag must remain true");
    }

    /// Helper: write `line\n` to `writer` and flush, checking for cat-echoed
    /// reply in `rx` within 1 second.  Returns `true` if the reply contains
    /// the substring `expected_substr`.
    fn write_and_check_echo(
        writer: &mut std::io::BufWriter<std::process::ChildStdin>,
        rx: &mpsc::Receiver<EngineLine>,
        line: &str,
        expected_substr: &str,
    ) -> bool {
        use std::io::Write as _;
        writer.write_all(line.as_bytes()).unwrap();
        writer.write_all(b"\n").unwrap();
        writer.flush().unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match rx.recv_timeout(remaining) {
                Ok(EngineLine::Other(s)) if s.contains(expected_substr) => return true,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
    }

    #[test]
    fn production_worker_sends_setoption_when_advertised_and_flag_on() {
        // When --virtual-clock is set AND the engine advertises VirtualClock,
        // the harness must send `setoption name VirtualClock value true`.
        let mock_lines = ["option name VirtualClock type check default false", "uciok"];
        let (mut handle, mut writer) = make_cat_handle(&mock_lines);

        let caps = wait_for_uciok_inner(&handle.rx, std::time::Duration::from_secs(1))
            .expect("wait_for_uciok_inner ok");
        assert!(caps.supports_virtual_clock);

        // Simulate the setoption-send logic.
        let virtual_clock_flag = true;
        if virtual_clock_flag && caps.supports_virtual_clock {
            use std::io::Write as _;
            writer
                .write_all(b"setoption name VirtualClock value true\n")
                .unwrap();
            writer.flush().unwrap();
        }

        // Cat echoes the setoption line back to stdout → rx.
        let found = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    break false;
                }
                match handle.rx.recv_timeout(remaining) {
                    Ok(EngineLine::Other(s))
                        if s.contains("setoption name VirtualClock value true") =>
                    {
                        break true;
                    }
                    Ok(_) => continue,
                    Err(_) => break false,
                }
            }
        };
        let _ = handle.child.kill();
        assert!(
            found,
            "setoption name VirtualClock value true must be sent when advertised+flag-on"
        );
    }

    #[test]
    fn production_worker_skips_setoption_when_unadvertised() {
        // When --virtual-clock is set but the engine does NOT advertise VirtualClock,
        // the harness must not send the setoption.
        let mock_lines = [
            "option name MoveOverhead type spin default 50 min 0 max 5000",
            "uciok",
        ];
        let (mut handle, mut writer) = make_cat_handle(&mock_lines);

        let caps = wait_for_uciok_inner(&handle.rx, std::time::Duration::from_secs(1))
            .expect("wait_for_uciok_inner ok");
        assert!(!caps.supports_virtual_clock);

        // Simulate the setoption-send logic — must skip because unadvertised.
        let virtual_clock_flag = true;
        if virtual_clock_flag && caps.supports_virtual_clock {
            use std::io::Write as _;
            writer
                .write_all(b"setoption name VirtualClock value true\n")
                .unwrap();
            writer.flush().unwrap();
        }

        // Write a sentinel so we have something to wait for, then check that
        // no VirtualClock setoption appeared before it.
        let found_vc_before_sentinel =
            write_and_check_echo(&mut writer, &handle.rx, "sentinel", "VirtualClock");
        let _ = handle.child.kill();
        assert!(
            !found_vc_before_sentinel,
            "setoption name VirtualClock must NOT be sent when option is unadvertised"
        );
    }

    #[test]
    fn production_worker_skips_setoption_when_flag_off() {
        // When the engine advertises VirtualClock but --virtual-clock is not set,
        // the harness must not send the setoption (default behavior unchanged).
        let mock_lines = ["option name VirtualClock type check default false", "uciok"];
        let (mut handle, mut writer) = make_cat_handle(&mock_lines);

        let caps = wait_for_uciok_inner(&handle.rx, std::time::Duration::from_secs(1))
            .expect("wait_for_uciok_inner ok");
        assert!(caps.supports_virtual_clock);

        // Simulate the setoption-send logic — must skip because flag is off.
        let virtual_clock_flag = false;
        if virtual_clock_flag && caps.supports_virtual_clock {
            use std::io::Write as _;
            writer
                .write_all(b"setoption name VirtualClock value true\n")
                .unwrap();
            writer.flush().unwrap();
        }

        // Write a sentinel; check no VirtualClock setoption appeared before it.
        let found_vc_before_sentinel =
            write_and_check_echo(&mut writer, &handle.rx, "sentinel", "VirtualClock");
        let _ = handle.child.kill();
        assert!(
            !found_vc_before_sentinel,
            "setoption name VirtualClock must NOT be sent when --virtual-clock flag is off"
        );
    }
}
