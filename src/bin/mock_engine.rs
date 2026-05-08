//! Test fixture: minimal UCI mock engine.
//!
//! Records every UCI command received from stdin to the file at
//! `MOCK_ENGINE_RECORD_PATH`, replies per a small subset of UCI sufficient to
//! drive `controller::production_worker_fn` through one full pair (handshake,
//! per-pair setoption block, two color-swapped games) without giving the mock
//! real chess knowledge. Each game terminates after exactly one ply via
//! `bestmove 0000` (UCI null-move encoding); the harness adjudicates as
//! `IllegalMove(active_color)` and proceeds to the next game.
//!
//! Used only by `src/bin/elo-iterate.rs::mod controller::tests::production_worker_tests`.
//!
//! See `docs/plans/tooling-mock-engine-fixture.md` for the full design.
//!
//! # Environment variables
//!
//! - `MOCK_ENGINE_RECORD_PATH` *(required)*: path to the recording file. If
//!   unset or empty, the mock prints to stderr and `exit(2)` immediately.
//! - `MOCK_ENGINE_VIRTUAL_CLOCK_ADVERTISED` *(optional, default `"0"`)*: when
//!   `"1"`, the `uci` reply includes `option name VirtualClock type check
//!   default false`. Used by the VirtualClock-negotiation test.

use std::io::{BufRead, Write};

fn main() {
    let record_path = match std::env::var("MOCK_ENGINE_RECORD_PATH") {
        Ok(s) if !s.is_empty() => s,
        _ => {
            eprintln!(
                "mock-engine: MOCK_ENGINE_RECORD_PATH must be set to a non-empty path; \
                 this binary is a test fixture, not a real engine"
            );
            std::process::exit(2);
        }
    };

    let advertise_vc = matches!(
        std::env::var("MOCK_ENGINE_VIRTUAL_CLOCK_ADVERTISED").as_deref(),
        Ok("1")
    );

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        record_command(&record_path, &line);

        let trimmed = line.trim();
        match trimmed {
            "uci" => {
                writeln!(stdout, "id name MockEngine").ok();
                writeln!(stdout, "id author clawfish-test").ok();
                writeln!(
                    stdout,
                    "option name UCI_Elo type spin default 1320 min 1320 max 3190"
                )
                .ok();
                writeln!(
                    stdout,
                    "option name UCI_LimitStrength type check default false"
                )
                .ok();
                if advertise_vc {
                    writeln!(stdout, "option name VirtualClock type check default false").ok();
                }
                writeln!(stdout, "uciok").ok();
                stdout.flush().ok();
            }
            "isready" => {
                writeln!(stdout, "readyok").ok();
                stdout.flush().ok();
            }
            "quit" => {
                stdout.flush().ok();
                std::process::exit(0);
            }
            _ if trimmed.starts_with("go") => {
                writeln!(stdout, "bestmove 0000").ok();
                stdout.flush().ok();
            }
            // setoption, ucinewgame, position, and unknown commands are
            // silently recorded with no reply (UCI spec for setoption / position;
            // permissive for unknowns by design — see plan §4.1).
            _ => {}
        }
    }
}

fn record_command(record_path: &str, line: &str) {
    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(record_path)
    {
        Ok(f) => f,
        Err(e) => panic!("mock-engine: cannot open record path {record_path:?}: {e}"),
    };
    writeln!(file, "{line}").expect("mock-engine: write to record path");
    file.flush().expect("mock-engine: flush record path");
}
