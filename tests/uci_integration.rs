//! Process-spawn integration tests for the UCI engine binary (M2.C, §E).
//!
//! These tests spawn the `chess` binary as a subprocess, pipe UCI commands
//! in, and verify the output. A dedicated reader thread prevents the test
//! from blocking on a full pipe buffer; `child.kill()` is the timeout
//! safety net.
//!
//! Each test exercises a specific protocol-level behavior end-to-end —
//! complementary to the in-process unit tests in `src/engine.rs::tests`,
//! which exercise the orchestrator without going through real stdin/stdout.

use chess::{Move, MoveList, Position, generate_moves};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command as ProcCommand, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Helpers shared by all three tests.
// ---------------------------------------------------------------------------

/// Spawn the chess binary with piped stdin/stdout and stderr discarded.
fn spawn_engine() -> std::process::Child {
    ProcCommand::new(env!("CARGO_BIN_EXE_chess"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn chess binary")
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
}

// ---------------------------------------------------------------------------
// E34 — position with a move applied, then go
// ---------------------------------------------------------------------------

/// Pipe `position startpos moves e2e4 / go / quit`. Assert that the bestmove
/// is a legal black move in the post-e2e4 position (validated via
/// `Move::from_uci`). RandomMover picks uniformly — we do not pin a specific
/// move, only that it is legal.
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
    // it is a legal black move. RandomMover with seed 0 picks uniformly, so
    // we only validate legality, not a specific move.
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
// E36 — self-play game with seed 8 terminates legally
//
// Spawns the binary; sets Random_Seed to 8; drives repeated
// `position startpos moves <accumulated>` + `go movetime 10` until the
// position has no legal moves (mate or stalemate). Validates every bestmove
// via Move::from_uci and make_move on a parallel Position. Pins final ply
// count and last bestmove.
//
// Seed 8 was chosen because seed 0 (the plan's first choice) cycles past
// MAX_PLY without reaching a terminal position — this is expected behavior
// for a pure random mover without 50-move-rule or repetition detection. Seeds
// 1–7 also cycle. Seed 8 terminates at ply 106. This is not a movegen bug;
// it is a property of the PRNG and the specific game trajectory. The engine
// will cycle with many seeds until repetition/50MR detection is added.
//
// EXPECTED values determined empirically (Phase 4) by running
// `find_terminating_seed_for_e36` in `src/search.rs::tests`.
// ---------------------------------------------------------------------------

/// Maximum ply before the test declares a deadlock / run-away game.
const MAX_PLY: usize = 300;

/// Empirical values from seed 8 run. Pins full reproducibility.
const SELF_PLAY_SEED: u64 = 8;
const EXPECTED_FINAL_PLY: usize = 106;
const EXPECTED_LAST_BESTMOVE: &str = "d3d1";

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

#[test]
// Every code path through this function either calls wait_for_exit() (which
// calls child.try_wait/child.kill) or panics. Clippy's zombie-process lint
// cannot see through the loop-with-conditional structure, so we suppress it.
#[allow(clippy::zombie_processes)]
fn integration_self_play_game_terminates_legally() {
    let movetime_ms: u64 = 10;
    // Per-move timeout: 1 s is 100× the requested movetime (10 ms), generous
    // for CI scheduling jitter. 10 s was unnecessarily lenient and would let
    // a movetime-ignored regression run for ~50 minutes before failing.
    let per_move_timeout = Duration::from_secs(1);

    let mut child = spawn_engine();
    let stdout = child.stdout.take().expect("stdout handle");
    let line_rx = drain_stdout(stdout);
    let mut stdin = child.stdin.take().expect("stdin handle");

    // Set seed so the game is deterministic. Seed 8 chosen because seeds 0–7
    // all cycle past MAX_PLY on this engine (no repetition detection yet).
    // Use isready/readyok handshake after setoption to synchronize: guarantees
    // the engine has processed setoption before the game loop begins.
    stdin
        .write_all(
            format!("setoption name Random_Seed value {SELF_PLAY_SEED}\nisready\n").as_bytes(),
        )
        .unwrap();
    stdin.flush().unwrap();

    // Wait for readyok to confirm setoption was processed.
    {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match line_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(line) if line == "readyok" => break,
                Ok(_) => {} // ignore other lines (e.g. id name from uci)
                Err(_) => {
                    if Instant::now() >= deadline {
                        panic!("engine did not respond to isready within 2 s");
                    }
                }
            }
        }
    }

    // Parallel position tracker for validating bestmoves.
    let mut pos = Position::starting_position();
    let mut accumulated_moves: Vec<String> = Vec::new();

    let mut last_bestmove = String::new();

    for ply in 0..=MAX_PLY {
        // Check for terminal position before asking for a move.
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        if ml.is_empty() {
            // Terminal position (mate or stalemate). Assertions:
            //
            // At least one full move must have been played before reaching a
            // terminal position. ply == 0 here would mean the starting position
            // is already terminal (or the engine returned bestmove 0000 on a
            // legal position), both of which indicate a movegen bug.
            assert!(
                ply >= 2,
                "game must play at least one full move before reaching a terminal position; \
                terminated at ply {ply} (engine may have returned bestmove 0000 on a legal position)"
            );
            // Pin final ply count and last bestmove for full reproducibility.
            assert_eq!(
                ply, EXPECTED_FINAL_PLY,
                "seed-{SELF_PLAY_SEED} game must end at exactly EXPECTED_FINAL_PLY={EXPECTED_FINAL_PLY} plies; ended at {ply}"
            );
            assert_eq!(
                last_bestmove, EXPECTED_LAST_BESTMOVE,
                "seed-{SELF_PLAY_SEED} game last bestmove must be EXPECTED_LAST_BESTMOVE={EXPECTED_LAST_BESTMOVE:?}; got '{last_bestmove}'"
            );
            stdin.write_all(b"quit\n").unwrap();
            drop(stdin);
            let exit_deadline = Instant::now() + Duration::from_secs(5);
            // wait_for_exit calls child.kill() on timeout, satisfying the
            // zombie-process lint: child is always waited on.
            let exited = wait_for_exit(&mut child, exit_deadline);
            assert!(exited, "engine did not exit within 5 s after quit");
            return;
        }

        if ply == MAX_PLY {
            // Build context for the panic message.
            let last_10: Vec<&String> = accumulated_moves.iter().rev().take(10).rev().collect();
            stdin.write_all(b"quit\n").unwrap();
            drop(stdin);
            let _ = wait_for_exit(&mut child, Instant::now() + Duration::from_secs(5));
            panic!(
                "cycled-or-runaway: did not terminate within MAX_PLY={MAX_PLY} plies; \
                last 10 moves: {last_10:?}"
            );
        }

        // Build `position startpos moves <accumulated>` command.
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

        // Wait for bestmove.
        let uci_str = expect_bestmove(&line_rx, per_move_timeout, &accumulated_moves);

        // Validate the bestmove is legal in the current position.
        let mv = Move::from_uci(&uci_str, &pos).unwrap_or_else(|e| {
            panic!(
                "bestmove '{uci_str}' is not legal at ply {ply}: {e}; \
                moves so far: {:?}",
                accumulated_moves
            )
        });

        // Advance the parallel position.
        pos.make_move(mv);
        accumulated_moves.push(uci_str.clone());
        last_bestmove = uci_str;
    }
    // Unreachable: the loop always returns or panics at MAX_PLY.
    unreachable!("loop exits via return or panic before falling through");
}
