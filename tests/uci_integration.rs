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

/// Spawn the clawfish binary with piped stdin/stdout and stderr discarded.
fn spawn_engine() -> std::process::Child {
    ProcCommand::new(env!("CARGO_BIN_EXE_clawfish"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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
// Self-play game helpers (shared by E38).
// ---------------------------------------------------------------------------

/// Maximum ply before the test declares a deadlock / run-away game.
const MAX_PLY: usize = 300;

/// Wait for a `bestmove` line from `rx` within `timeout_per_move`. Returns the
/// UCI move string (without the `bestmove ` prefix), or panics with context.
fn expect_bestmove(
    rx: &mpsc::Receiver<String>,
    timeout_per_move: Duration,
    last_moves: &[String],
) -> String {
    let deadline = Instant::now() + timeout_per_move;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!(
                "engine did not respond with bestmove within timeout; last moves: {:?}",
                last_moves.iter().rev().take(10).rev().collect::<Vec<_>>()
            );
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(line) if line.starts_with("bestmove ") => {
                return line
                    .strip_prefix("bestmove ")
                    .expect("just checked")
                    .to_string();
            }
            Ok(_) => {} // info lines, etc. — keep waiting
            Err(_) => {
                // timeout or disconnect
                panic!(
                    "engine stdout disconnected while waiting for bestmove; last moves: {:?}",
                    last_moves.iter().rev().take(10).rev().collect::<Vec<_>>()
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// E38 — GreedyMover self-play game terminates legally
//
// Spawns the binary; sets Random_Seed to GREEDY_SELF_PLAY_SEED; drives
// repeated `position startpos moves <accumulated>` + `go movetime 10` until
// the position has no legal moves (mate or stalemate). Validates every
// bestmove via Move::from_uci. Pins final ply count and last bestmove.
//
// GreedyMover plays materially best moves at each step, so games decay toward
// terminal states much faster than the uniform-random mover.
//
// EXPECTED constants calibrated empirically (M3.A, release build, seed 0):
//   ply 116, last bestmove f2f3.
// To re-calibrate: temporarily set GREEDY_EXPECTED_FINAL_PLY=usize::MAX, run
// the test, read the panic message for the actual values, then restore.
// ---------------------------------------------------------------------------

const GREEDY_SELF_PLAY_SEED: u64 = 0;
const GREEDY_EXPECTED_FINAL_PLY: usize = 116;
const GREEDY_EXPECTED_LAST_BESTMOVE: &str = "f2f3";

#[test]
#[allow(clippy::zombie_processes)]
fn integration_greedy_self_play_terminates() {
    let movetime_ms: u64 = 10;
    let per_move_timeout = Duration::from_secs(1);

    let mut child = spawn_engine();
    let stdout = child.stdout.take().expect("stdout handle");
    let line_rx = drain_stdout(stdout);
    let mut stdin = child.stdin.take().expect("stdin handle");

    // Set seed and synchronize with isready/readyok.
    stdin
        .write_all(
            format!("setoption name Random_Seed value {GREEDY_SELF_PLAY_SEED}\nisready\n")
                .as_bytes(),
        )
        .unwrap();
    stdin.flush().unwrap();

    {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match line_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(line) if line == "readyok" => break,
                Ok(_) => {}
                Err(_) => {
                    if Instant::now() >= deadline {
                        panic!("E38: engine did not respond to isready within 2 s");
                    }
                }
            }
        }
    }

    let mut pos = Position::starting_position();
    let mut accumulated_moves: Vec<String> = Vec::new();
    let mut last_bestmove = String::new();

    for ply in 0..=MAX_PLY {
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        if ml.is_empty() {
            assert!(
                ply >= 2,
                "E38: game must play at least one full move before reaching a terminal position; \
                terminated at ply {ply}"
            );
            assert_eq!(
                ply, GREEDY_EXPECTED_FINAL_PLY,
                "E38: greedy seed-{GREEDY_SELF_PLAY_SEED} game must end at exactly \
                GREEDY_EXPECTED_FINAL_PLY={GREEDY_EXPECTED_FINAL_PLY} plies; ended at {ply}; \
                last_bestmove='{last_bestmove}'"
            );
            assert_eq!(
                last_bestmove, GREEDY_EXPECTED_LAST_BESTMOVE,
                "E38: last bestmove must be GREEDY_EXPECTED_LAST_BESTMOVE={GREEDY_EXPECTED_LAST_BESTMOVE:?}; \
                got '{last_bestmove}'"
            );
            stdin.write_all(b"quit\n").unwrap();
            drop(stdin);
            let exit_deadline = Instant::now() + Duration::from_secs(5);
            let exited = wait_for_exit(&mut child, exit_deadline);
            assert!(exited, "E38: engine did not exit within 5 s after quit");
            return;
        }

        if ply == MAX_PLY {
            let last_10: Vec<&String> = accumulated_moves.iter().rev().take(10).rev().collect();
            stdin.write_all(b"quit\n").unwrap();
            drop(stdin);
            let _ = wait_for_exit(&mut child, Instant::now() + Duration::from_secs(5));
            panic!(
                "E38: cycled-or-runaway: did not terminate within MAX_PLY={MAX_PLY} plies; \
                last 10 moves: {last_10:?}"
            );
        }

        let pos_cmd = if accumulated_moves.is_empty() {
            "position startpos\n".to_string()
        } else {
            format!("position startpos moves {}\n", accumulated_moves.join(" "))
        };
        stdin.write_all(pos_cmd.as_bytes()).unwrap();
        stdin
            .write_all(format!("go movetime {movetime_ms}\n").as_bytes())
            .unwrap();
        stdin.flush().unwrap();

        let uci_str = expect_bestmove(&line_rx, per_move_timeout, &accumulated_moves);

        let mv = Move::from_uci(&uci_str, &pos).unwrap_or_else(|e| {
            panic!(
                "E38: bestmove '{uci_str}' is not legal at ply {ply}: {e}; \
                moves so far: {:?}",
                accumulated_moves
            )
        });

        pos.make_move(mv);
        accumulated_moves.push(uci_str.clone());
        last_bestmove = uci_str;
    }
    unreachable!("loop exits via return or panic before falling through");
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
