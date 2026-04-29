//! Process-spawn integration tests for the UCI engine binary (M2.C, §E).
//!
//! These tests spawn the `clawfish` binary as a subprocess, pipe UCI commands
//! in, and verify the output. A dedicated reader thread prevents the test
//! from blocking on a full pipe buffer; `child.kill()` is the timeout
//! safety net.
//!
//! Each test exercises a specific protocol-level behavior end-to-end —
//! complementary to the in-process unit tests in `src/engine.rs::tests`,
//! which exercise the orchestrator without going through real stdin/stdout.

use clawfish::{Move, MoveList, Position, generate_moves};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command as ProcCommand, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Helpers shared by all three tests.
// ---------------------------------------------------------------------------

/// Spawn the clawfish binary with piped stdin/stdout. Stderr is inherited
/// so any engine panic / debug output shows up in cargo test's per-test
/// capture buffer when a test fails — diagnostic value when the engine
/// subprocess dies unexpectedly (e.g. Linux CI flake investigation).
fn spawn_engine() -> std::process::Child {
    ProcCommand::new(env!("CARGO_BIN_EXE_clawfish"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn clawfish binary")
}

/// Start a thread that reads lines from `stdout` into an mpsc channel until
/// EOF. Returns the receiving end.
fn drain_stdout(stdout: std::process::ChildStdout) -> mpsc::Receiver<String> {
    let (line_tx, line_rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    let _ = line_tx.send(l);
                }
                Err(_) => break,
            }
        }
    });
    line_rx
}

/// Collect all lines currently queued in `rx`, draining with a short timeout
/// until no further lines arrive. Returns the collected lines.
fn collect_lines(rx: &mpsc::Receiver<String>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Ok(line) = rx.recv_timeout(Duration::from_millis(100)) {
        lines.push(line);
    }
    lines
}

/// Poll `child.try_wait()` until the process exits or `deadline` is reached.
/// Returns `true` if the process exited naturally; calls `child.kill()` and
/// returns `false` on timeout.
fn wait_for_exit(child: &mut std::process::Child, deadline: Instant) -> bool {
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            Err(_) => return true, // treat a query error as "done"
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// E33 — full handshake from starting position
// ---------------------------------------------------------------------------

/// Pipe a full `uci / isready / position startpos / go / quit` sequence and
/// assert that all required UCI handshake tokens appear in stdout.
#[test]
fn integration_full_handshake_starting_position() {
    let mut child = spawn_engine();
    let stdout = child.stdout.take().expect("stdout handle");
    let line_rx = drain_stdout(stdout);

    let mut stdin = child.stdin.take().expect("stdin handle");
    stdin
        .write_all(b"uci\nisready\nposition startpos\ngo\nquit\n")
        .unwrap();
    drop(stdin);

    let deadline = Instant::now() + Duration::from_secs(2);
    let exited = wait_for_exit(&mut child, deadline);

    let lines = collect_lines(&line_rx);
    let output = lines.join("\n");

    assert!(exited, "engine did not exit within 2 s");
    assert!(
        lines.iter().any(|l| l.starts_with("id name")),
        "expected 'id name' in output;\nfull output:\n{output}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("id author")),
        "expected 'id author' in output;\nfull output:\n{output}"
    );
    assert!(
        lines.iter().any(|l| l == "uciok"),
        "expected 'uciok' in output;\nfull output:\n{output}"
    );
    assert!(
        lines.iter().any(|l| l == "readyok"),
        "expected 'readyok' in output;\nfull output:\n{output}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("bestmove")),
        "expected 'bestmove' in output;\nfull output:\n{output}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("info depth ")),
        "expected an 'info depth …' line in output (per plan §11);\nfull output:\n{output}"
    );
}

// ---------------------------------------------------------------------------
// E34 — position with a move applied, then go
// ---------------------------------------------------------------------------

/// Pipe `position startpos moves e2e4 / go / quit`. Assert that the bestmove
/// is a legal black move in the post-e2e4 position (validated via
/// `Move::from_uci`). GreedyMover picks by depth-1 eval — we validate
/// legality, not a specific move, since the best move depends on eval tuning.
#[test]
fn integration_position_with_moves_then_go() {
    let mut child = spawn_engine();
    let stdout = child.stdout.take().expect("stdout handle");
    let line_rx = drain_stdout(stdout);

    let mut stdin = child.stdin.take().expect("stdin handle");
    stdin
        .write_all(b"position startpos moves e2e4\ngo\nquit\n")
        .unwrap();
    drop(stdin);

    let deadline = Instant::now() + Duration::from_secs(2);
    let exited = wait_for_exit(&mut child, deadline);

    let lines = collect_lines(&line_rx);
    let output = lines.join("\n");

    assert!(exited, "engine did not exit within 2 s");

    // Parse the bestmove UCI against the actual post-e2e4 position to confirm
    // it is a legal black move. We validate legality, not a specific move,
    // since the best depth-1 move depends on eval tuning.
    let mut post_e2e4 = Position::starting_position();
    let e2e4 = Move::from_uci("e2e4", &post_e2e4).expect("e2e4 is legal from startpos");
    post_e2e4.make_move(e2e4);

    let bestmove_line = lines
        .iter()
        .find(|l| l.starts_with("bestmove"))
        .unwrap_or_else(|| panic!("bestmove line must be present;\nfull output:\n{output}"));
    let uci_str = bestmove_line
        .strip_prefix("bestmove ")
        .expect("bestmove line has 'bestmove ' prefix");

    Move::from_uci(uci_str, &post_e2e4).unwrap_or_else(|e| {
        panic!(
            "bestmove '{uci_str}' is not a legal black move after e2e4: {e};\nfull output:\n{output}"
        )
    });
}

// ---------------------------------------------------------------------------
// E35 — EOF on stdin terminates the engine cleanly
// ---------------------------------------------------------------------------

/// Pipe `uci\n` then drop stdin (EOF). Assert the engine exits within 2 s.
/// The EOF must be translated to a synthetic `Command::Quit` by `reader_loop`
/// (per plan §10), causing `run` to return and `run_stdio` to call
/// `process::exit(0)`.
#[test]
fn integration_eof_terminates_engine_cleanly() {
    let mut child = spawn_engine();
    let stdout = child.stdout.take().expect("stdout handle");
    let _line_rx = drain_stdout(stdout);

    let mut stdin = child.stdin.take().expect("stdin handle");
    stdin.write_all(b"uci\n").unwrap();
    drop(stdin); // EOF: engine's reader sees end-of-stream and synthesizes Quit.

    let deadline = Instant::now() + Duration::from_secs(2);
    let exited = wait_for_exit(&mut child, deadline);

    assert!(exited, "engine did not exit within 2 s after stdin EOF");
}

// ---------------------------------------------------------------------------
// E39 — AlphaBetaMover depth-3 search returns a legal bestmove from startpos
//
// Launches the binary, sends `position startpos` + `go depth 3`, parses the
// `bestmove` line, and asserts the move is a legal move from startpos.
// ---------------------------------------------------------------------------

// Pinned at M3.C impl: depth-3 bestmove from startpos (2181 nodes, score cp 38,
// PV b1c3 g8f6 c3d5). Change only when the search algorithm changes.
const EXPECTED_BESTMOVE_DEPTH_3: &str = "b1c3";

#[test]
fn integration_alphabeta_depth3_returns_legal_bestmove_from_startpos() {
    let mut child = spawn_engine();
    let stdout = child.stdout.take().expect("stdout handle");
    let line_rx = drain_stdout(stdout);

    let mut stdin = child.stdin.take().expect("stdin handle");
    stdin
        .write_all(b"position startpos\ngo depth 3\nquit\n")
        .unwrap();
    drop(stdin);

    // Generous timeout: depth-3 search should complete quickly, but allow
    // a slow CI runner and the binary cold-start overhead.
    let deadline = Instant::now() + Duration::from_secs(10);
    let exited = wait_for_exit(&mut child, deadline);

    let lines = collect_lines(&line_rx);
    let output = lines.join("\n");

    assert!(exited, "E39: engine did not exit within 10 s");

    let bestmove_line = lines
        .iter()
        .find(|l| l.starts_with("bestmove"))
        .unwrap_or_else(|| panic!("E39: bestmove line must be present;\nfull output:\n{output}"));
    let uci_str = bestmove_line
        .strip_prefix("bestmove ")
        .expect("bestmove line has 'bestmove ' prefix");

    // Assert the move is legal from startpos.
    let startpos = Position::starting_position();
    let mv = Move::from_uci(uci_str, &startpos).unwrap_or_else(|e| {
        panic!(
            "E39: bestmove '{uci_str}' is not a legal move from startpos: {e};\nfull output:\n{output}"
        )
    });

    // Verify against the full legal moveset.
    let mut ml = MoveList::new();
    generate_moves(&startpos, &mut ml);
    assert!(
        ml.iter().any(|legal| legal == mv),
        "E39: bestmove '{uci_str}' parsed ok but is not in generate_moves(startpos);\nfull output:\n{output}"
    );

    assert_eq!(
        uci_str, EXPECTED_BESTMOVE_DEPTH_3,
        "E39: depth-3 bestmove regression: expected '{EXPECTED_BESTMOVE_DEPTH_3}', got '{uci_str}'"
    );
}

// ---------------------------------------------------------------------------
// E37 — unknown command silently ignored
// ---------------------------------------------------------------------------

/// Pipe a garbage line followed by `isready` and `quit`. Assert that the
/// complete captured output is exactly `["readyok"]` — proving the unknown
/// command is silently dropped (M2.C contract: `Unknown` commands are silent
/// when `debug` is off) AND the `isready` round-trip still works AND nothing
/// else is emitted (no spurious echo, no `info string` debug line, no
/// stray `option`/`id` lines).
///
/// `assert_eq!` on the full line list is tighter than separate `any`/`!any`
/// assertions: a regression that echoes the garbage line, or emits a
/// stray `info string` debug line, or any other non-empty line, would fail
/// here. The contract is total silence-but-`readyok`; the test pins it
/// totally.
#[test]
fn integration_unknown_command_silently_ignored() {
    let mut child = spawn_engine();
    let stdout = child.stdout.take().expect("stdout handle");
    let line_rx = drain_stdout(stdout);

    let mut stdin = child.stdin.take().expect("stdin handle");
    stdin.write_all(b"joho garbage\nisready\nquit\n").unwrap();
    drop(stdin);

    let deadline = Instant::now() + Duration::from_secs(2);
    let exited = wait_for_exit(&mut child, deadline);

    let lines = collect_lines(&line_rx);

    assert!(exited, "engine did not exit within 2 s");
    assert_eq!(
        lines,
        vec!["readyok".to_string()],
        "expected exactly one line of output, 'readyok'; got {lines:?}"
    );
}
