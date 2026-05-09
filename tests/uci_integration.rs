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

/// Wait for a line starting with `prefix` to appear in `rx`, accumulating
/// captured lines into `accum`. Returns when found; panics on timeout.
///
/// Used by M3.E integration tests: under ID, the engine emits multiple `info`
/// lines per `go` and the test must wait for the terminating `bestmove` line
/// before sending `quit`. Sending `quit` before `bestmove` arrives flips the
/// inter-iteration `stop` flag and aborts the ID loop early — the test sees
/// only iteration 1's snapshot rather than the full requested depth.
fn wait_for_line_starting_with(
    rx: &mpsc::Receiver<String>,
    prefix: &str,
    deadline: Instant,
    accum: &mut Vec<String>,
) {
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                let matched = line.starts_with(prefix);
                accum.push(line);
                if matched {
                    return;
                }
            }
            Err(_) => continue,
        }
    }
    let captured = accum.join("\n");
    panic!("timed out waiting for line starting with {prefix:?}; captured:\n{captured}");
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

/// Pipe `position startpos moves e2e4 / go depth 2 / quit`. Assert that the
/// bestmove is a legal black move in the post-e2e4 position (validated via
/// `Move::from_uci`). We validate legality, not a specific move, since the best
/// move depends on eval tuning. `go depth 2` is used (instead of bare `go`)
/// so the search completes before the `quit` stop-flag fires — bare `go` at
/// the default depth-4 with qsearch exceeds the 4096-node cancellation cadence
/// in debug mode before the first root move completes (M3.D timing note).
#[test]
fn integration_position_with_moves_then_go() {
    let mut child = spawn_engine();
    let stdout = child.stdout.take().expect("stdout handle");
    let line_rx = drain_stdout(stdout);

    let mut stdin = child.stdin.take().expect("stdin handle");
    stdin
        .write_all(b"position startpos moves e2e4\ngo depth 2\nquit\n")
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

// Pinned at M4.A Slice C: depth-3 bestmove from startpos shifted back to g1f3
// once the engine-owned TT is wired into SearchContext. With TT-move-first
// ordering, iteration-2's root entry (depth-2 bestmove) guides iteration-3's
// root move order, and the TT-aided search selects g1f3 as the depth-3 choice.
// Previously d2d4 (Slice B, no TT in SearchContext) and g1f3 before that (M3.E,
// prior_root_move hint active).
const EXPECTED_BESTMOVE_DEPTH_3: &str = "g1f3";

#[test]
fn integration_alphabeta_depth3_returns_legal_bestmove_from_startpos() {
    let mut child = spawn_engine();
    let stdout = child.stdout.take().expect("stdout handle");
    let line_rx = drain_stdout(stdout);

    let mut stdin = child.stdin.take().expect("stdin handle");
    // Send commands progressively and wait for `bestmove` BEFORE sending
    // `quit`. M3.E ID inter-iteration `stop` check would otherwise abort
    // mid-search and return only iteration 1's bestmove (g1f3) instead of
    // the depth-3 d2d4.
    stdin.write_all(b"position startpos\ngo depth 3\n").unwrap();

    let mut lines: Vec<String> = Vec::new();
    let bestmove_deadline = Instant::now() + Duration::from_secs(10);
    wait_for_line_starting_with(&line_rx, "bestmove", bestmove_deadline, &mut lines);

    // Now safe to quit: the search has emitted bestmove and unwound.
    stdin.write_all(b"quit\n").unwrap();
    drop(stdin);

    let deadline = Instant::now() + Duration::from_secs(2);
    let exited = wait_for_exit(&mut child, deadline);
    lines.extend(collect_lines(&line_rx));
    let output = lines.join("\n");

    assert!(exited, "E39: engine did not exit within 2 s after quit");

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

// ---------------------------------------------------------------------------
// E40 — `go wtime/btime` triggers ID and emits bestmove within budget (M3.E)
// ---------------------------------------------------------------------------

/// `go wtime 1000 btime 1000` from startpos must:
/// - Trigger iterative deepening (≥ 1 `info depth N` line emitted).
/// - Emit a `bestmove <uci>` line within 2 seconds wallclock.
/// - The bestmove must be a legal move from startpos.
#[test]
fn integration_go_wtime_btime_completes_within_budget_and_emits_bestmove() {
    let mut child = spawn_engine();
    let stdout = child.stdout.take().expect("stdout handle");
    let line_rx = drain_stdout(stdout);

    let mut stdin = child.stdin.take().expect("stdin handle");
    // wtime/btime=1000 with default MoveOverhead=50: caps = (50ms, 150ms).
    // Send go, wait for bestmove, then quit.
    stdin
        .write_all(b"position startpos\ngo wtime 1000 btime 1000\n")
        .unwrap();

    let mut lines: Vec<String> = Vec::new();
    let bestmove_deadline = Instant::now() + Duration::from_secs(3);
    wait_for_line_starting_with(&line_rx, "bestmove", bestmove_deadline, &mut lines);

    stdin.write_all(b"quit\n").unwrap();
    drop(stdin);

    let deadline = Instant::now() + Duration::from_secs(2);
    let exited = wait_for_exit(&mut child, deadline);
    lines.extend(collect_lines(&line_rx));
    let output = lines.join("\n");

    assert!(exited, "E40: engine did not exit within 2 s after quit");

    let info_lines: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("info depth"))
        .collect();
    assert!(
        !info_lines.is_empty(),
        "E40: ID must emit at least one info depth line;\nfull output:\n{output}"
    );

    let bestmove_line = lines
        .iter()
        .find(|l| l.starts_with("bestmove"))
        .unwrap_or_else(|| panic!("E40: bestmove line must be present;\nfull output:\n{output}"));
    let uci_str = bestmove_line
        .strip_prefix("bestmove ")
        .expect("bestmove line has 'bestmove ' prefix");

    let startpos = Position::starting_position();
    let mv = Move::from_uci(uci_str, &startpos)
        .unwrap_or_else(|e| panic!("E40: bestmove '{uci_str}' is not legal from startpos: {e}"));
    let mut ml = MoveList::new();
    generate_moves(&startpos, &mut ml);
    assert!(
        ml.iter().any(|legal| legal == mv),
        "E40: bestmove '{uci_str}' must be in generate_moves(startpos)"
    );
}

// ---------------------------------------------------------------------------
// E41 — MoveOverhead UCI option observably changes search budget (M3.E)
// ---------------------------------------------------------------------------

/// Set MoveOverhead to 500, then drive `go movetime 1000`. The search budget
/// is `max(1, 1000 - 500) = 500ms`. The bestmove line must arrive within
/// `[400ms, 700ms]` — the lower bound is "engine must NOT return before the
/// soft cap" (UCI: don't return early), the upper bound is "soft + slack
/// for CI jitter".
#[test]
fn integration_moveoverhead_subtracts_from_movetime_budget() {
    let mut child = spawn_engine();
    let stdout = child.stdout.take().expect("stdout handle");
    let line_rx = drain_stdout(stdout);

    let mut stdin = child.stdin.take().expect("stdin handle");

    // Send commands progressively, measuring the start-time AT THE MOMENT
    // we send `go`. This excludes engine startup overhead from the timing
    // window — what we measure is genuinely the search-budget elapsed time.
    stdin
        .write_all(
            b"setoption name MoveOverhead value 500\n\
              position startpos\n",
        )
        .unwrap();
    let start = Instant::now();
    stdin.write_all(b"go movetime 1000\n").unwrap();

    let mut lines: Vec<String> = Vec::new();
    let bestmove_deadline = start + Duration::from_secs(3);
    wait_for_line_starting_with(&line_rx, "bestmove", bestmove_deadline, &mut lines);
    let elapsed_to_bestmove = start.elapsed();

    stdin.write_all(b"quit\n").unwrap();
    drop(stdin);

    let exit_deadline = Instant::now() + Duration::from_secs(2);
    let _exited = wait_for_exit(&mut child, exit_deadline);
    lines.extend(collect_lines(&line_rx));
    let output = lines.join("\n");
    let _bestmove_line = lines
        .iter()
        .find(|l| l.starts_with("bestmove"))
        .unwrap_or_else(|| panic!("E41: bestmove must be present;\nfull output:\n{output}"));

    let elapsed = elapsed_to_bestmove;
    // Window distinguishes MoveOverhead=500 (~500ms search) from a broken
    // implementation that ignores MoveOverhead (would use default 50ms,
    // search ~950ms). Correct impl: 500ms search + ~150ms subprocess
    // overhead ≈ 650ms total. Broken impl: 950ms + ~150ms ≈ 1100ms.
    //
    // Lower bound 400ms: rejects an impl that returns immediately
    // (`MoveOverhead` not parsed correctly, defaulting to "no time" or
    // similar). Upper bound 1200ms: 100ms margin above broken-impl 1100ms
    // expected; rejects the common bug "ignore MoveOverhead, use default 50".
    // CI jitter past 1200ms is rare on the dev machine; bump the bound if
    // CI flake materializes.
    assert!(
        elapsed >= Duration::from_millis(400),
        "E41: bestmove arrived too early ({elapsed:?}); MoveOverhead=500 must enforce ≥ ~500ms budget"
    );
    assert!(
        elapsed <= Duration::from_millis(1200),
        "E41: bestmove arrived too late ({elapsed:?}); MoveOverhead=500 should produce ~500ms search \
         (~650ms total). >1200ms suggests MoveOverhead=500 was not applied (e.g. default 50ms used → \
         ~950ms search → ~1100ms total). If this fires under non-flake conditions on a slow CI runner, \
         bump the bound."
    );
}

// ---------------------------------------------------------------------------
// E42 — ID emits one info depth N line per completed iteration (M3.E)
// ---------------------------------------------------------------------------

/// `go depth 4` from startpos must emit info lines for depths 1, 2, 3, and 4
/// in that order. Pins the per-iteration emission contract.
#[test]
fn integration_id_emits_one_info_per_iteration() {
    let mut child = spawn_engine();
    let stdout = child.stdout.take().expect("stdout handle");
    let line_rx = drain_stdout(stdout);

    let mut stdin = child.stdin.take().expect("stdin handle");
    stdin.write_all(b"position startpos\ngo depth 4\n").unwrap();

    let mut lines: Vec<String> = Vec::new();
    let bestmove_deadline = Instant::now() + Duration::from_secs(10);
    wait_for_line_starting_with(&line_rx, "bestmove", bestmove_deadline, &mut lines);

    stdin.write_all(b"quit\n").unwrap();
    drop(stdin);

    let deadline = Instant::now() + Duration::from_secs(2);
    let exited = wait_for_exit(&mut child, deadline);
    lines.extend(collect_lines(&line_rx));
    let output = lines.join("\n");

    assert!(exited, "E42: engine did not exit within 2 s after quit");

    // Find each `info depth N ...` for N = 1..=4 in increasing order.
    let mut last_idx: Option<usize> = None;
    for d in 1u32..=4 {
        let prefix = format!("info depth {d} ");
        let idx = lines
            .iter()
            .position(|l| l.starts_with(&prefix))
            .unwrap_or_else(|| {
                panic!("E42: missing 'info depth {d} …' line;\nfull output:\n{output}")
            });
        if let Some(prev) = last_idx {
            assert!(
                idx > prev,
                "E42: 'info depth {d}' (line {idx}) must come after 'info depth {}' (line {prev})",
                d - 1
            );
        }
        last_idx = Some(idx);
    }
}

// ---------------------------------------------------------------------------
// E43 — bench command emits summary line and engine remains responsive (M3.F)
// ---------------------------------------------------------------------------

/// `bench 4` from a fresh engine must emit the OpenBench-grep-compatible
/// signature line `info string bench: <N> nodes <NPS> nps` within 60 s,
/// followed by `readyok` after a subsequent `isready`, and exit cleanly on
/// `quit`. Pins (a) the bench-as-regression-baseline contract end-to-end
/// through a real subprocess; (b) the engine remains responsive after bench;
/// (c) the process exits within 5 s of `quit`.
///
/// Depth 4 is used (vs the default 7) to keep CI wallclock low; M3.F's
/// in-process unit tests in src/engine.rs cover the default-depth path
/// against the real `BENCH_DEFAULT_DEPTH` constant.
#[test]
fn integration_bench_emits_summary_within_60s_and_remains_responsive() {
    let mut child = spawn_engine();
    let stdout = child.stdout.take().expect("stdout handle");
    let line_rx = drain_stdout(stdout);

    let mut stdin = child.stdin.take().expect("stdin handle");
    stdin.write_all(b"bench 4\n").unwrap();

    let mut lines: Vec<String> = Vec::new();
    // 60 s budget: depth 4 over 16 positions runs in ~1 s on dev hardware
    // and well under 60 s even on slow CI runners.
    let bench_deadline = Instant::now() + Duration::from_secs(60);
    wait_for_line_starting_with(&line_rx, "info string bench: ", bench_deadline, &mut lines);

    // Engine remains responsive after bench.
    stdin.write_all(b"isready\n").unwrap();
    let ready_deadline = Instant::now() + Duration::from_secs(5);
    wait_for_line_starting_with(&line_rx, "readyok", ready_deadline, &mut lines);

    stdin.write_all(b"quit\n").unwrap();
    drop(stdin);

    let exit_deadline = Instant::now() + Duration::from_secs(5);
    let exited = wait_for_exit(&mut child, exit_deadline);
    lines.extend(collect_lines(&line_rx));
    let output = lines.join("\n");

    assert!(exited, "E43: engine did not exit within 5 s after quit");

    // Validate the signature line shape: `info string bench: <N> nodes <NPS> nps`.
    let sig = lines
        .iter()
        .find(|l| l.starts_with("info string bench: "))
        .unwrap_or_else(|| panic!("E43: missing bench signature line;\nfull output:\n{output}"));
    let rest = sig.strip_prefix("info string bench: ").unwrap();
    let parts: Vec<&str> = rest.split_whitespace().collect();
    assert_eq!(
        parts.len(),
        4,
        "E43: signature line shape wrong; got {sig:?}"
    );
    assert_eq!(
        parts[1], "nodes",
        "E43: signature must read 'nodes'; {sig:?}"
    );
    assert_eq!(parts[3], "nps", "E43: signature must read 'nps'; {sig:?}");
    let nodes: u64 = parts[0]
        .parse()
        .unwrap_or_else(|_| panic!("E43: signature N field must parse as u64; {sig:?}"));
    let nps: u64 = parts[2]
        .parse()
        .unwrap_or_else(|_| panic!("E43: signature NPS field must parse as u64; {sig:?}"));
    assert!(nodes > 0, "E43: total nodes must be > 0; {sig:?}");
    assert!(nps > 0, "E43: NPS must be > 0; {sig:?}");

    // readyok must appear AFTER the signature line in stream order.
    let bench_pos = lines
        .iter()
        .position(|l| l.starts_with("info string bench: "))
        .unwrap();
    let ready_pos = lines
        .iter()
        .position(|l| l == "readyok")
        .unwrap_or_else(|| panic!("E43: missing readyok;\nfull output:\n{output}"));
    assert!(
        ready_pos > bench_pos,
        "E43: readyok ({ready_pos}) must arrive AFTER the bench signature line ({bench_pos})"
    );
}

// ---------------------------------------------------------------------------
// E46 — bench is deterministic across two consecutive runs in the same
// engine session, with the M4.C history heuristic in play (M4.C re-pin of
// the M3.F bench-determinism contract).
// ---------------------------------------------------------------------------

/// `bench 4 / bench 4` from a single engine subprocess must produce the
/// same total node count both times. Re-pins the bench-determinism
/// contract with the butterfly history table participating in move
/// ordering: per-position `reset_for_new_game()` must clear the history
/// table alongside the TT and game-history Vec, otherwise ordering would
/// drift from run 1 to run 2 and node counts would differ.
#[test]
fn bench_signature_deterministic_across_two_runs_with_history() {
    let mut child = spawn_engine();
    let stdout = child.stdout.take().expect("stdout handle");
    let line_rx = drain_stdout(stdout);

    let mut stdin = child.stdin.take().expect("stdin handle");
    stdin.write_all(b"bench 4\nbench 4\n").unwrap();

    let mut lines: Vec<String> = Vec::new();
    let bench_deadline_1 = Instant::now() + Duration::from_secs(60);
    wait_for_line_starting_with(
        &line_rx,
        "info string bench: ",
        bench_deadline_1,
        &mut lines,
    );
    // Wait for the SECOND signature line. We accumulate any lines produced
    // between the two bench signatures (per-position info lines from the
    // second bench run), then look for a second prefix match.
    let bench_deadline_2 = Instant::now() + Duration::from_secs(60);
    while Instant::now() < bench_deadline_2 {
        match line_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                let matched = line.starts_with("info string bench: ");
                lines.push(line);
                if matched {
                    break;
                }
            }
            Err(_) => continue,
        }
    }

    stdin.write_all(b"quit\n").unwrap();
    drop(stdin);

    let exit_deadline = Instant::now() + Duration::from_secs(5);
    let exited = wait_for_exit(&mut child, exit_deadline);
    lines.extend(collect_lines(&line_rx));
    let output = lines.join("\n");

    assert!(exited, "E46: engine did not exit within 5 s after quit");

    let signatures: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("info string bench: "))
        .collect();
    assert_eq!(
        signatures.len(),
        2,
        "E46: expected exactly two bench signature lines; got {}.\nfull output:\n{output}",
        signatures.len()
    );

    let parse_nodes = |sig: &str| -> u64 {
        let rest = sig.strip_prefix("info string bench: ").unwrap();
        let parts: Vec<&str> = rest.split_whitespace().collect();
        assert_eq!(parts.len(), 4, "E46: signature shape wrong; {sig:?}");
        parts[0].parse::<u64>().expect("E46: nodes must parse")
    };
    let nodes1 = parse_nodes(signatures[0]);
    let nodes2 = parse_nodes(signatures[1]);
    assert_eq!(
        nodes1, nodes2,
        "E46: bench node counts must match across two runs in the same session \
         (history table cleared per position via reset_for_new_game); got {nodes1} vs {nodes2}"
    );
}

// ---------------------------------------------------------------------------
// E47 — bench is deterministic across two consecutive runs in the same
// engine session, with M4.D aspiration windows in play (re-pin of the
// M3.F / M4.A / M4.C bench-determinism contract).
// ---------------------------------------------------------------------------

/// `bench / bench` from a single engine subprocess must produce the same
/// total node count both times, even with aspiration windows participating
/// in the ID outer loop. Aspiration centers each iteration's window on the
/// prior iteration's score (from `last_complete`); per-position
/// `reset_for_new_game()` clears the per-game state (TT + game_history +
/// killer + history table) so iteration 1 of position N starts from the
/// same state regardless of whether earlier positions ran in the same
/// session, which makes `last_complete` deterministic across runs and
/// hence aspiration's window choices deterministic. Default depth is 7
/// (set by `BENCH_DEFAULT_DEPTH`).
#[test]
fn bench_signature_deterministic_across_two_runs_with_aspiration() {
    let mut child = spawn_engine();
    let stdout = child.stdout.take().expect("stdout handle");
    let line_rx = drain_stdout(stdout);

    let mut stdin = child.stdin.take().expect("stdin handle");
    // E47 uses `bench 4` to keep the test wallclock low; the determinism
    // contract holds at every depth (M3.F BENCH_DEFAULT_DEPTH=7 is exercised
    // by the production `bench` signature recorded in bench/m4.md). E46's
    // 60-second per-run timeout pattern is preserved.
    stdin.write_all(b"bench 4\nbench 4\n").unwrap();

    let mut lines: Vec<String> = Vec::new();
    let bench_deadline_1 = Instant::now() + Duration::from_secs(60);
    wait_for_line_starting_with(
        &line_rx,
        "info string bench: ",
        bench_deadline_1,
        &mut lines,
    );
    let bench_deadline_2 = Instant::now() + Duration::from_secs(60);
    while Instant::now() < bench_deadline_2 {
        match line_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                let matched = line.starts_with("info string bench: ");
                lines.push(line);
                if matched {
                    break;
                }
            }
            Err(_) => continue,
        }
    }

    stdin.write_all(b"quit\n").unwrap();
    drop(stdin);

    let exit_deadline = Instant::now() + Duration::from_secs(5);
    let exited = wait_for_exit(&mut child, exit_deadline);
    lines.extend(collect_lines(&line_rx));
    let output = lines.join("\n");

    assert!(exited, "E47: engine did not exit within 5 s after quit");

    let signatures: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("info string bench: "))
        .collect();
    assert_eq!(
        signatures.len(),
        2,
        "E47: expected exactly two bench signature lines; got {}.\nfull output:\n{output}",
        signatures.len()
    );

    let parse_nodes = |sig: &str| -> u64 {
        let rest = sig.strip_prefix("info string bench: ").unwrap();
        let parts: Vec<&str> = rest.split_whitespace().collect();
        assert_eq!(parts.len(), 4, "E47: signature shape wrong; {sig:?}");
        parts[0].parse::<u64>().expect("E47: nodes must parse")
    };
    let nodes1 = parse_nodes(signatures[0]);
    let nodes2 = parse_nodes(signatures[1]);
    assert_eq!(
        nodes1, nodes2,
        "E47: bench node counts must match across two runs in the same session \
         (aspiration window choices are deterministic given identical per-position \
         start state from reset_for_new_game); got {nodes1} vs {nodes2}"
    );
}

// ---------------------------------------------------------------------------
// E48 — bench is deterministic across two consecutive runs in the same
// engine session with M5.A null-move pruning active (re-pin of the M3.F /
// M4.A / M4.C / M4.D determinism contract).
// ---------------------------------------------------------------------------

/// `bench / bench` from a single engine subprocess must produce the same
/// total node count both times, even with NMP cutting branches inside
/// negamax. NMP itself doesn't introduce any non-determinism — its gate
/// reads pure functions of `Position` (zobrist, side_to_move, in_check,
/// has_non_pawn_material, static_eval_white) and its TT store + ADR-0018
/// §7's preservation rule keeps ordering hints deterministic across runs.
/// Per-position `reset_for_new_game()` continues to clear all per-game
/// state (TT + game_history + killers + history table) so iteration 1 of
/// position N starts from the same state regardless of whether earlier
/// positions ran in the same session.
#[test]
fn bench_signature_deterministic_across_two_runs_with_nmp() {
    let mut child = spawn_engine();
    let stdout = child.stdout.take().expect("stdout handle");
    let line_rx = drain_stdout(stdout);

    let mut stdin = child.stdin.take().expect("stdin handle");
    // E48 uses `bench 4` to keep the test wallclock low; the determinism
    // contract holds at every depth (the default-depth signature is exercised
    // by `bench/m5.md`). E46's 60-second per-run timeout pattern is preserved.
    stdin.write_all(b"bench 4\nbench 4\n").unwrap();

    let mut lines: Vec<String> = Vec::new();
    let bench_deadline_1 = Instant::now() + Duration::from_secs(60);
    wait_for_line_starting_with(
        &line_rx,
        "info string bench: ",
        bench_deadline_1,
        &mut lines,
    );
    let bench_deadline_2 = Instant::now() + Duration::from_secs(60);
    while Instant::now() < bench_deadline_2 {
        match line_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                let matched = line.starts_with("info string bench: ");
                lines.push(line);
                if matched {
                    break;
                }
            }
            Err(_) => continue,
        }
    }

    stdin.write_all(b"quit\n").unwrap();
    drop(stdin);

    let exit_deadline = Instant::now() + Duration::from_secs(5);
    let exited = wait_for_exit(&mut child, exit_deadline);
    lines.extend(collect_lines(&line_rx));
    let output = lines.join("\n");

    assert!(exited, "E48: engine did not exit within 5 s after quit");

    let signatures: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("info string bench: "))
        .collect();
    assert_eq!(
        signatures.len(),
        2,
        "E48: expected exactly two bench signature lines; got {}.\nfull output:\n{output}",
        signatures.len()
    );

    let parse_nodes = |sig: &str| -> u64 {
        let rest = sig.strip_prefix("info string bench: ").unwrap();
        let parts: Vec<&str> = rest.split_whitespace().collect();
        assert_eq!(parts.len(), 4, "E48: signature shape wrong; {sig:?}");
        parts[0].parse::<u64>().expect("E48: nodes must parse")
    };
    let nodes1 = parse_nodes(signatures[0]);
    let nodes2 = parse_nodes(signatures[1]);
    assert_eq!(
        nodes1, nodes2,
        "E48: bench node counts must match across two runs in the same session \
         with NMP active (NMP gate is a pure function of Position; its TT store \
         + ADR-0018 §7 preservation rule keeps ordering deterministic); \
         got {nodes1} vs {nodes2}"
    );
}

// E49 — bench is deterministic across two consecutive runs in the same
// engine session with M5.B reverse-futility pruning active (re-pin of the
// M5.A determinism contract, mirroring E48's shape).
// ---------------------------------------------------------------------------

/// `bench / bench` from a single engine subprocess must produce the same
/// total node count both times, even with RFP cutting branches inside
/// negamax. RFP is purely static (no sub-search, no TT store) so it
/// introduces no non-determinism across identical positions. The value pin
/// originally lived in this test for the M5.B signature (137174 nodes at
/// depth=4); E50 carries the M5.C value pin going forward, and this test
/// retains the determinism-only contract per the E46/E47/E48 pattern.
#[test]
fn bench_signature_deterministic_across_two_runs_with_rfp() {
    let mut child = spawn_engine();
    let stdout = child.stdout.take().expect("stdout handle");
    let line_rx = drain_stdout(stdout);

    let mut stdin = child.stdin.take().expect("stdin handle");
    // E49 uses `bench 4` to keep the test wallclock low; the determinism
    // contract holds at every depth (the default-depth signature is exercised
    // by `bench/m5.md`). E48's 60-second per-run timeout pattern is preserved.
    stdin.write_all(b"bench 4\nbench 4\n").unwrap();

    let mut lines: Vec<String> = Vec::new();
    let bench_deadline_1 = Instant::now() + Duration::from_secs(60);
    wait_for_line_starting_with(
        &line_rx,
        "info string bench: ",
        bench_deadline_1,
        &mut lines,
    );
    let bench_deadline_2 = Instant::now() + Duration::from_secs(60);
    while Instant::now() < bench_deadline_2 {
        match line_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                let matched = line.starts_with("info string bench: ");
                lines.push(line);
                if matched {
                    break;
                }
            }
            Err(_) => continue,
        }
    }

    stdin.write_all(b"quit\n").unwrap();
    drop(stdin);

    let exit_deadline = Instant::now() + Duration::from_secs(5);
    let exited = wait_for_exit(&mut child, exit_deadline);
    lines.extend(collect_lines(&line_rx));
    let output = lines.join("\n");

    assert!(exited, "E49: engine did not exit within 5 s after quit");

    let signatures: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("info string bench: "))
        .collect();
    assert_eq!(
        signatures.len(),
        2,
        "E49: expected exactly two bench signature lines; got {}.\nfull output:\n{output}",
        signatures.len()
    );

    let parse_nodes = |sig: &str| -> u64 {
        let rest = sig.strip_prefix("info string bench: ").unwrap();
        let parts: Vec<&str> = rest.split_whitespace().collect();
        assert_eq!(parts.len(), 4, "E49: signature shape wrong; {sig:?}");
        parts[0].parse::<u64>().expect("E49: nodes must parse")
    };
    let nodes1 = parse_nodes(signatures[0]);
    let nodes2 = parse_nodes(signatures[1]);
    assert_eq!(
        nodes1, nodes2,
        "E49: bench node counts must match across two runs in the same session \
         with RFP active (RFP gate is a pure function of Position + depth; \
         static-only check, no sub-search, no TT store → deterministic); \
         got {nodes1} vs {nodes2}"
    );
}

// E50 — bench is deterministic across two consecutive runs in the same
// engine session with M5.C late-move reductions active. The depth-4 value
// pin (`130884` at M5.C) was dropped at the M5.D landing; E50 now tests
// determinism only. The M5.D depth-4 value pin lives in E51 below.
// ---------------------------------------------------------------------------

/// `bench / bench` from a single engine subprocess must produce the same
/// total node count both times with LMR active (LMR's reduction formula and
/// eligibility predicate are pure functions of `(depth, quiet_index)` plus
/// `Position` / killers / history; nothing introduces non-determinism). The
/// depth-4 value pin was dropped at the M5.D landing per the M5.B → M5.C
/// precedent (E49 dropped its M5.B value pin at the M5.C landing); E50 now
/// tests determinism only. For the default-depth signature, see `bench/m5.md`;
/// for the M5.D depth-4 value pin, see E51.
#[test]
fn bench_signature_deterministic_across_two_runs_with_lmr() {
    let mut child = spawn_engine();
    let stdout = child.stdout.take().expect("stdout handle");
    let line_rx = drain_stdout(stdout);

    let mut stdin = child.stdin.take().expect("stdin handle");
    stdin.write_all(b"bench 4\nbench 4\n").unwrap();

    let mut lines: Vec<String> = Vec::new();
    let bench_deadline_1 = Instant::now() + Duration::from_secs(60);
    wait_for_line_starting_with(
        &line_rx,
        "info string bench: ",
        bench_deadline_1,
        &mut lines,
    );
    let bench_deadline_2 = Instant::now() + Duration::from_secs(60);
    while Instant::now() < bench_deadline_2 {
        match line_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                let matched = line.starts_with("info string bench: ");
                lines.push(line);
                if matched {
                    break;
                }
            }
            Err(_) => continue,
        }
    }

    stdin.write_all(b"quit\n").unwrap();
    drop(stdin);

    let exit_deadline = Instant::now() + Duration::from_secs(5);
    let exited = wait_for_exit(&mut child, exit_deadline);
    lines.extend(collect_lines(&line_rx));
    let output = lines.join("\n");

    assert!(exited, "E50: engine did not exit within 5 s after quit");

    let signatures: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("info string bench: "))
        .collect();
    assert_eq!(
        signatures.len(),
        2,
        "E50: expected exactly two bench signature lines; got {}.\nfull output:\n{output}",
        signatures.len()
    );

    let parse_nodes = |sig: &str| -> u64 {
        let rest = sig.strip_prefix("info string bench: ").unwrap();
        let parts: Vec<&str> = rest.split_whitespace().collect();
        assert_eq!(parts.len(), 4, "E50: signature shape wrong; {sig:?}");
        parts[0].parse::<u64>().expect("E50: nodes must parse")
    };
    let nodes1 = parse_nodes(signatures[0]);
    let nodes2 = parse_nodes(signatures[1]);
    assert_eq!(
        nodes1, nodes2,
        "E50: bench node counts must match across two runs in the same session \
         with LMR active; got {nodes1} vs {nodes2}"
    );
    // M5.C value pin (`130884`) was dropped at M5.D landing per the M5.B → M5.C
    // precedent (E49 dropped its M5.B value pin at the M5.C landing). E50 now
    // tests determinism only; the M5.D depth-4 value pin lives in E51 below.
}

/// E51 — bench signature determinism with M5.F qsearch-in-TT.
///
/// Same shape as E50 (M5.C). Two consecutive `bench 4` invocations must
/// produce identical node counts. The depth-4 value pin moved from M5.D's
/// `120_856` to M5.F's `89_080` (−26.3%) when qsearch TT probe+store landed.
/// E50 stays determinism-only per the M5.B → M5.C → M5.D precedent of
/// dropping the prior pin's value when the new pin lands.
#[test]
fn bench_signature_deterministic_across_two_runs_with_qsearch_tt() {
    let mut child = spawn_engine();
    let stdout = child.stdout.take().expect("stdout handle");
    let line_rx = drain_stdout(stdout);

    let mut stdin = child.stdin.take().expect("stdin handle");
    stdin.write_all(b"bench 4\nbench 4\n").unwrap();

    let mut lines: Vec<String> = Vec::new();
    let bench_deadline_1 = Instant::now() + Duration::from_secs(60);
    wait_for_line_starting_with(
        &line_rx,
        "info string bench: ",
        bench_deadline_1,
        &mut lines,
    );
    let bench_deadline_2 = Instant::now() + Duration::from_secs(60);
    while Instant::now() < bench_deadline_2 {
        match line_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                let matched = line.starts_with("info string bench: ");
                lines.push(line);
                if matched {
                    break;
                }
            }
            Err(_) => continue,
        }
    }

    stdin.write_all(b"quit\n").unwrap();
    drop(stdin);

    let exit_deadline = Instant::now() + Duration::from_secs(5);
    let exited = wait_for_exit(&mut child, exit_deadline);
    lines.extend(collect_lines(&line_rx));
    let output = lines.join("\n");

    assert!(exited, "E51: engine did not exit within 5 s after quit");

    let signatures: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with("info string bench: "))
        .collect();
    assert_eq!(
        signatures.len(),
        2,
        "E51: expected exactly two bench signature lines; got {}.\nfull output:\n{output}",
        signatures.len()
    );

    let parse_nodes = |sig: &str| -> u64 {
        let rest = sig.strip_prefix("info string bench: ").unwrap();
        let parts: Vec<&str> = rest.split_whitespace().collect();
        assert_eq!(parts.len(), 4, "E51: signature shape wrong; {sig:?}");
        parts[0].parse::<u64>().expect("E51: nodes must parse")
    };
    let nodes1 = parse_nodes(signatures[0]);
    let nodes2 = parse_nodes(signatures[1]);
    assert_eq!(
        nodes1, nodes2,
        "E51: bench node counts must match across two runs in the same session \
         with qsearch-in-TT active; got {nodes1} vs {nodes2}"
    );
    // M5.F depth-4 bench-signature pin. Recorded from the production binary
    // after M5.F qsearch TT probe+store landed. M5.D pin was 120_856;
    // M5.F reduces depth-4 nodes by ~26% due to qsearch TT cutoffs.
    const M5F_DEPTH4_BENCH_NODES: u64 = 89_080;
    assert_eq!(
        nodes1, M5F_DEPTH4_BENCH_NODES,
        "E51: bench node count changed from M5.F pin ({M5F_DEPTH4_BENCH_NODES} at depth=4); \
         re-pin if intentional; got {nodes1}"
    );
}
