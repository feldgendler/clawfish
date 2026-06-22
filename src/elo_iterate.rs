//! `elo-iterate` — in-process tournament harness.
//!
//! Drives two UCI engines via persistent subprocess pipes, plays a
//! colour-paired fixed batch of games with native adjudication, and emits
//! per-game PGN plus a summary line. Replaces `scripts/elo-iterate.sh` for
//! the correctness layer of online Elo iteration.
//!
//! Sub-module layout:
//!   - `cli`        — argument parsing.
//!   - `driver`     — subprocess + UCI line parsing.
//!   - `adjudicate` — native game-over detection.
//!   - `match_loop` — colour-paired game loop + per-side clock.
//!   - `pgn`        — PGN tag-roster + body emission.
//!   - `summary`    — summary.txt aggregation.
//!
//! See `docs/decisions/0020-eloh-harness.md`, ADR-0021, ADR-0022 for
//! architectural commitments.

pub(crate) mod adjudicate;
pub(crate) mod cli;
pub(crate) mod controller;
pub(crate) mod driver;
pub(crate) mod estimator;
pub(crate) mod match_loop;
pub(crate) mod pgn;
pub(crate) mod prng;
pub(crate) mod progress;
pub(crate) mod sigma;
pub(crate) mod sprt;
pub(crate) mod spsa;
pub(crate) mod summary;
pub(crate) mod tc_sample;

use std::process::ExitCode;

// ---------------------------------------------------------------------------
// StopReason — crate-internal; shared by mod progress and (eventually) mod controller
// ---------------------------------------------------------------------------

/// Why the online iteration terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopReason {
    /// Trailing-σ fell below `--target-sigma` for `--stop-window-confirm` consecutive games.
    Sigma,
    /// `--max-games` exhausted without σ convergence.
    MaxGames,
    /// SPRT LLR ≤ B (lower Wald bound). H0 accepted (patch fails).
    SprtAcceptH0,
    /// SPRT LLR ≥ A (upper Wald bound). H1 accepted (patch passes).
    SprtAcceptH1,
}

/// Binary entry point. Called from `src/bin/elo-iterate.rs`.
pub fn main() -> ExitCode {
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

    // post-parse: exactly one of tc/tc_sample is Some (enforced by parse_args).
    // Per-pair TCs are pre-materialised in run_iteration via pair_tcs; WorkerConfig
    // no longer holds static tc/opponent_tc — they are passed per-pair via WorkerCmd::PlayPair.
    let watchdog = std::time::Duration::from_millis(args.watchdog_ms);

    let cfg = controller::WorkerConfig {
        engine_spec: driver::EngineSpec {
            name: engine_name,
            path: args.engine.clone(),
            launch_prefix: args.engine_launch_prefix.clone(),
        },
        opponent_spec: driver::EngineSpec {
            name: opponent_name,
            path: args.opponent.clone(),
            launch_prefix: args.opponent_launch_prefix.clone(),
        },
        engine_options: args.engine_options.clone(),
        opponent_options: args.opponent_options.clone(),
        mode: crate::MatchTimeMode::Wallclock,
        harness_overhead_ms: args.harness_overhead_ms,
        watchdog,
        max_plies: args.max_moves,
        thresholds: args.thresholds.clone(),
        virtual_clock: args.virtual_clock,
    };

    let out_dir = std::path::Path::new(&args.out_dir).to_owned();

    // SPSA mode: `run_spsa` owns the tuning loop and does not use the worker pool
    // for Elo estimation — it builds its own pair config per-iteration.
    if args.spsa {
        return match controller::run_spsa(&args, &out_dir) {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: run_spsa: {e:?}");
                ExitCode::from(1)
            }
        };
    }

    let mut pool = match controller::spawn_workers(args.concurrency, cfg) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: spawn_workers: {e:?}");
            return ExitCode::from(1);
        }
    };

    match controller::run_iteration(&mut pool, &args, &out_dir) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: run_iteration: {e:?}");
            ExitCode::from(1)
        }
    }
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
            // Slice D wires match_loop to produce these; unreachable until then.
            GameOver::ResignAdjudicated(_) => "adjudication: resign".into(),
            GameOver::DrawAdjudicated => "adjudication: draw-by-score".into(),
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
pub(crate) fn outcome_to_pgn_result(outcome: &match_loop::GameOutcome) -> (String, String) {
    use crate::Color;
    use adjudicate::GameOver;
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
            // Slice D wires match_loop to produce these; unreachable until then.
            // White resigns → white loses → "0-1"; black resigns → "1-0".
            GameOver::ResignAdjudicated(loser) => match loser {
                Color::White => "0-1",
                Color::Black => "1-0",
            },
            GameOver::DrawAdjudicated => "1/2-1/2",
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
pub(crate) fn format_tc(tc: cli::TimeControl) -> String {
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

/// Best-effort current hostname, defaulting to `"localhost"`.
pub(crate) fn current_hostname() -> String {
    let mut buf = [0u8; 64];
    get_hostname(&mut buf);
    std::ffi::CStr::from_bytes_until_nul(&buf)
        .ok()
        .and_then(|c| c.to_str().ok())
        .unwrap_or("localhost")
        .to_owned()
}

/// Current local-day date string in `YYYY.MM.DD` form for PGN headers.
pub(crate) fn current_date_str() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days_since_epoch = secs / 86400;
    unix_days_to_date_str(days_since_epoch)
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
        // 2024-02-29 (leap day). Immediately after Feb 28 (19781).
        assert_eq!(unix_days_to_date_str(19782), "2024.02.29");
    }

    #[test]
    fn unix_days_to_date_str_mar_1_2024() {
        // Day after 2024-02-29.
        assert_eq!(unix_days_to_date_str(19783), "2024.03.01");
    }

    #[test]
    fn unix_days_to_date_str_dec_31_2025() {
        // 2025-12-31: 55*365 + 14 = 20089; +364 = 20453.
        assert_eq!(unix_days_to_date_str(20453), "2025.12.31");
    }

    #[test]
    fn unix_days_to_date_str_epoch() {
        // 1970-01-01 = days 0, but all values before 2000-03-01 saturate to
        // 0 in d after the subtraction. The exact returned date for inputs in
        // 1970–1999 is undefined (but must not panic).
        let _ = unix_days_to_date_str(0);
    }

    // -----------------------------------------------------------------------
    // format_tc
    // -----------------------------------------------------------------------

    #[test]
    fn format_tc_no_increment() {
        let tc = cli::TimeControl {
            initial_ms: 60_000,
            increment_ms: 0,
        };
        assert_eq!(format_tc(tc), "60");
    }

    #[test]
    fn format_tc_with_increment() {
        let tc = cli::TimeControl {
            initial_ms: 10_000,
            increment_ms: 100,
        };
        assert_eq!(format_tc(tc), "10+0.1");
    }

    #[test]
    fn format_tc_with_integral_increment() {
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

    // ---- ELOH.C §6.7: VirtualClock end-to-end smokes ----

    #[test]
    #[ignore = "spawns clawfish; opt-in via cargo test --release -- --ignored"]
    fn end_to_end_self_play_virtual_clock_runs() {
        // Both engines receive `setoption name VirtualClock value true` because
        // clawfish advertises the option and `--virtual-clock` is set.
        use std::process::Command;

        let engine = resolve_bin(CLAWFISH_EXE, "clawfish");
        let harness = resolve_bin(ELO_ITERATE_EXE, "elo-iterate");
        let engine = engine.as_str();
        let harness = harness.as_str();
        let out_dir = std::env::temp_dir().join("elo-iterate-smoke-vc");
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
                "2",
                "--out-dir",
                out_dir.to_str().unwrap(),
                "--virtual-clock",
                "--initial-elo",
                "2000",
                "--k0",
                "0",
                "--target-sigma",
                "0",
            ])
            .status()
            .expect("failed to spawn elo-iterate");

        assert!(status.success(), "harness exited with {status}");

        let games_dir = out_dir.join("games");
        let pgns: Vec<_> = std::fs::read_dir(&games_dir)
            .expect("games dir missing")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "pgn"))
            .collect();
        assert_eq!(pgns.len(), 2, "expected 2 PGN files, found {}", pgns.len());

        let summary = std::fs::read_to_string(out_dir.join("summary.txt")).unwrap();
        // The summary file ends with a `converged:` line when --target-sigma 0.
        assert!(
            summary.contains("converged:"),
            "summary must contain 'converged:' line"
        );
    }

    // ---- ELOH.D §6.7: mixed-TC sampling end-to-end smoke ----

    #[test]
    #[ignore = "spawns clawfish; opt-in via cargo test --release -- --ignored"]
    fn end_to_end_self_play_tc_sample_runs() {
        // --tc-sample 2+0.5:1,3+0.5:1 --concurrency 1 --max-games 4 --target-sigma 0
        // --initial-elo 2000 --k0 0 --seed 42
        // TCs are 2-3s base / 0.5s inc — generous enough that clawfish-vs-clawfish
        // doesn't time-forfeit on a hot CI runner.
        use std::process::Command;

        let engine = resolve_bin(CLAWFISH_EXE, "clawfish");
        let harness = resolve_bin(ELO_ITERATE_EXE, "elo-iterate");
        let engine = engine.as_str();
        let harness = harness.as_str();
        let out_dir = std::env::temp_dir().join("elo-iterate-smoke-tc-sample");
        let _ = std::fs::remove_dir_all(&out_dir);
        std::fs::create_dir_all(&out_dir).unwrap();

        let status = Command::new(harness)
            .args([
                "--engine",
                engine,
                "--opponent",
                engine,
                "--tc-sample",
                "2+0.5:1,3+0.5:1",
                "--concurrency",
                "1",
                "--max-games",
                "4",
                "--target-sigma",
                "0",
                "--initial-elo",
                "2000",
                "--k0",
                "0",
                "--seed",
                "42",
                "--out-dir",
                out_dir.to_str().unwrap(),
            ])
            .status()
            .expect("failed to spawn elo-iterate");

        assert!(status.success(), "harness exited with {status}");

        // 4 PGN files must exist, each with a TimeControl tag from the configured set.
        let games_dir = out_dir.join("games");
        let pgns: Vec<_> = std::fs::read_dir(&games_dir)
            .expect("games dir missing")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "pgn"))
            .collect();
        assert_eq!(pgns.len(), 4, "expected 4 PGN files, found {}", pgns.len());

        for entry in &pgns {
            let content = std::fs::read_to_string(entry.path()).unwrap();
            let has_tc = content.contains("[TimeControl \"2+0.5\"]")
                || content.contains("[TimeControl \"3+0.5\"]");
            assert!(
                has_tc,
                "PGN {:?} must have TimeControl tag from configured set; content:\n{}",
                entry.path(),
                &content[..content.len().min(500)]
            );
        }

        // summary.txt structure: 4 game-summary lines + N progress: lines (one per
        // PairComplete; with --concurrency 1 and 4 games = 2 pairs that's 2) + 1
        // converged: line + 1 summary-by-tc: line = 8 non-empty lines. Assert the
        // structural pieces directly rather than pinning the total count, which is
        // sensitive to harness-internal write cadence and would couple this test to
        // implementation choices that aren't load-bearing for ELOH.D's contract.
        let summary = std::fs::read_to_string(out_dir.join("summary.txt")).unwrap();
        let non_empty_lines: Vec<_> = summary.lines().filter(|l| !l.is_empty()).collect();
        assert!(
            non_empty_lines.len() >= 4,
            "expected at least 4 summary lines (one per game), found {} — full text:\n{}",
            non_empty_lines.len(),
            summary
        );
        // Exactly 4 lines with the tc= field — one per GameComplete; progress: and
        // converged: lines have no tc= field.
        let tc_lines = non_empty_lines.iter().filter(|l| l.contains("tc=")).count();
        assert_eq!(
            tc_lines, 4,
            "expected exactly 4 lines with tc= field (one per game), found {tc_lines}"
        );
        assert!(
            summary.contains("summary-by-tc:"),
            "summary.txt must contain summary-by-tc: line"
        );
        assert!(
            summary.contains("converged:"),
            "summary.txt must contain converged: line"
        );
    }

    #[test]
    #[ignore = "spawns clawfish + stockfish; opt-in via cargo test --release -- --ignored"]
    fn end_to_end_vs_stockfish_virtual_clock_falls_back_silently() {
        // Stockfish does not advertise VirtualClock; the harness must fall back
        // silently (send setoption only to clawfish, not Stockfish) and complete
        // the run without errors.
        use std::process::Command;

        // Skip if Stockfish is not on PATH.
        let stockfish_ok = Command::new("stockfish")
            .arg("quit")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status()
            .is_ok();
        if !stockfish_ok {
            eprintln!("test skipped: stockfish not found on PATH");
            return;
        }

        let engine = resolve_bin(CLAWFISH_EXE, "clawfish");
        let harness = resolve_bin(ELO_ITERATE_EXE, "elo-iterate");
        let engine = engine.as_str();
        let harness = harness.as_str();
        let out_dir = std::env::temp_dir().join("elo-iterate-smoke-vc-sf");
        let _ = std::fs::remove_dir_all(&out_dir);
        std::fs::create_dir_all(&out_dir).unwrap();

        let output = Command::new(harness)
            .args([
                "--engine",
                engine,
                "--opponent",
                "stockfish",
                "--tc",
                "1+0.05",
                "--max-games",
                "2",
                "--out-dir",
                out_dir.to_str().unwrap(),
                "--virtual-clock",
                "--initial-elo",
                "2000",
                "--k0",
                "0",
                "--target-sigma",
                "0",
            ])
            .output()
            .expect("failed to spawn elo-iterate");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "harness must exit 0 even when opponent doesn't support VirtualClock; stderr={stderr}"
        );
        // No error about VirtualClock in stderr.
        assert!(
            !stderr.contains("VirtualClock"),
            "stderr must not mention VirtualClock on silent fallback; got: {stderr}"
        );
    }

    // ---- §6.6 ELOH.E end-to-end smoke ----

    #[test]
    #[ignore = "spawns clawfish; opt-in via cargo test --release -- --ignored"]
    fn end_to_end_sprt_clawfish_self_play_max_games_short() {
        use std::process::Command;

        let engine = resolve_bin(CLAWFISH_EXE, "clawfish");
        let harness = resolve_bin(ELO_ITERATE_EXE, "elo-iterate");
        let out_dir = std::env::temp_dir().join("elo-iterate-eloh-e-sprt-smoke");
        let _ = std::fs::remove_dir_all(&out_dir);
        std::fs::create_dir_all(&out_dir).unwrap();

        let status = Command::new(&harness)
            .args([
                "--engine",
                &engine,
                "--opponent",
                &engine,
                "--tc",
                "1+0.05",
                "--max-games",
                "20",
                "--concurrency",
                "1",
                "--initial-elo",
                "0",
                "--sprt-elo0",
                "0",
                "--sprt-elo1",
                "10",
                "--sprt-alpha",
                "0.05",
                "--sprt-beta",
                "0.05",
                "--out-dir",
                out_dir.to_str().unwrap(),
            ])
            .status()
            .expect("spawn harness");
        assert!(status.success(), "harness must exit 0");
        let summary = std::fs::read_to_string(out_dir.join("summary.txt")).unwrap();
        assert!(
            summary.contains("sprt: verdict="),
            "summary must contain a `sprt: verdict=` line; got: {summary}"
        );
        // Verify match.pgn exists.
        assert!(
            out_dir.join("match.pgn").exists(),
            "match.pgn must be created by the run-end concatenation step"
        );
    }

    #[test]
    #[ignore = "spawns clawfish; opt-in via cargo test --release -- --ignored"]
    fn end_to_end_match_mode_with_engine_option() {
        use std::process::Command;

        let engine = resolve_bin(CLAWFISH_EXE, "clawfish");
        let harness = resolve_bin(ELO_ITERATE_EXE, "elo-iterate");
        let out_dir = std::env::temp_dir().join("elo-iterate-eloh-e-match-smoke");
        let _ = std::fs::remove_dir_all(&out_dir);
        std::fs::create_dir_all(&out_dir).unwrap();

        let status = Command::new(&harness)
            .args([
                "--engine",
                &engine,
                "--opponent",
                &engine,
                "--tc",
                "1+0.05",
                "--max-games",
                "4",
                "--concurrency",
                "1",
                "--initial-elo",
                "0",
                "--k0",
                "0",
                "--target-sigma",
                "0",
                "--engine-option",
                "Random_Seed=1",
                "--opponent-option",
                "Random_Seed=2",
                "--out-dir",
                out_dir.to_str().unwrap(),
            ])
            .status()
            .expect("spawn harness");
        assert!(status.success(), "harness must exit 0");
        let summary = std::fs::read_to_string(out_dir.join("summary.txt")).unwrap();
        // Match mode → no `sprt:` line, but `ci:` line is still emitted by SPRT
        // state being absent → no `ci:` either. Confirm only `converged:` exists.
        assert!(
            !summary.contains("sprt: verdict="),
            "match mode must NOT emit a `sprt: verdict=` line; got: {summary}"
        );
        assert!(
            summary.contains("converged: "),
            "match mode summary must end with a converged: line"
        );
        assert!(
            out_dir.join("match.pgn").exists(),
            "match.pgn must be created by the run-end concatenation step"
        );
    }

    // ---- 2.6.i SPSA end-to-end smoke ----

    #[test]
    #[ignore = "spawns clawfish; opt-in via cargo test --release -- --ignored"]
    fn end_to_end_spsa_smoke_3_iters_2_games_per_iter() {
        use std::process::Command;

        let engine = resolve_bin(CLAWFISH_EXE, "clawfish");
        let harness = resolve_bin(ELO_ITERATE_EXE, "elo-iterate");
        let out_dir = std::env::temp_dir().join("elo-iterate-spsa-smoke");
        let _ = std::fs::remove_dir_all(&out_dir);
        std::fs::create_dir_all(&out_dir).unwrap();

        let run = |out: &std::path::Path| {
            Command::new(&harness)
                .args([
                    "--engine",
                    &engine,
                    "--opponent",
                    &engine,
                    "--tc",
                    "1+0.05",
                    "--spsa",
                    "--spsa-iters",
                    "3",
                    "--spsa-games-per-iter",
                    "2",
                    "--spsa-param",
                    "Aspiration_K:200:0:1000:20:centik",
                    "--spsa-param",
                    "Aspiration_Min:25:0:1000:4:cp",
                    "--spsa-param",
                    "Aspiration_Max:250:0:2000:12:cp",
                    "--seed",
                    "42",
                    "--out-dir",
                    out.to_str().unwrap(),
                ])
                .status()
                .expect("spawn harness")
        };

        let status = run(&out_dir);
        assert!(status.success(), "SPSA harness must exit 0; got {status}");

        // Verify spsa-trajectory.tsv has exactly 3 rows (one per iteration).
        let traj_path = out_dir.join("spsa-trajectory.tsv");
        assert!(traj_path.exists(), "spsa-trajectory.tsv must exist");
        let traj = std::fs::read_to_string(&traj_path).unwrap();
        let traj_rows: Vec<&str> = traj.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            traj_rows.len(),
            3,
            "spsa-trajectory.tsv must have 3 rows; got {}: {traj:?}",
            traj_rows.len()
        );

        // Verify spsa-final.txt exists with a parseable option block.
        let final_path = out_dir.join("spsa-final.txt");
        assert!(final_path.exists(), "spsa-final.txt must exist");
        let final_content = std::fs::read_to_string(&final_path).unwrap();
        assert!(
            final_content.contains("Aspiration_Adaptive=true"),
            "spsa-final.txt must contain Aspiration_Adaptive=true; got: {final_content:?}"
        );
        assert!(
            final_content.contains("Aspiration_K="),
            "spsa-final.txt must contain Aspiration_K=; got: {final_content:?}"
        );

        // Structural validation: each row must have the expected column count
        // and parseable numeric fields.
        //
        // For 3 params the TSV column layout per row is:
        //   [0]k  [1..3]theta  [4..6]plus_vals  [7..9]minus_vals
        //   [10]match  [11]a_k  [12..14]c_k  [15]pair_score  → 16 columns total
        const EXPECTED_COLS: usize = 16;
        for (row_idx, row) in traj_rows.iter().enumerate() {
            let cols: Vec<&str> = row.split('\t').collect();
            assert_eq!(
                cols.len(),
                EXPECTED_COLS,
                "trajectory row {row_idx} must have {EXPECTED_COLS} columns; got {}: {row:?}",
                cols.len()
            );
            let k: u64 = cols[0].parse().expect("col 0 (k) must be u64");
            assert_eq!(
                k, row_idx as u64,
                "row {row_idx} k-field must equal row index"
            );
            // All remaining columns must be parseable as f64.
            for (ci, &col) in cols[1..].iter().enumerate() {
                col.parse::<f64>().unwrap_or_else(|_| {
                    panic!(
                        "trajectory row {row_idx} col {} '{col}' must be f64",
                        ci + 1
                    )
                });
            }
        }

        // Same-seed iteration-0 Δ reproducibility: since iteration 0's θ⁺/θ⁻ values
        // are computed from the *initial* θ (independent of any game outcome), they
        // must be identical across two runs with the same seed. Subsequent iterations
        // perturb the *current* θ (which evolves with game outcomes) so only iter-0
        // is strictly seed-determined.
        let out_dir2 = std::env::temp_dir().join("elo-iterate-spsa-smoke-2");
        let _ = std::fs::remove_dir_all(&out_dir2);
        std::fs::create_dir_all(&out_dir2).unwrap();
        let status2 = run(&out_dir2);
        assert!(status2.success(), "second SPSA run must exit 0");

        let traj2 = std::fs::read_to_string(out_dir2.join("spsa-trajectory.tsv")).unwrap();
        let iter0_cols = |tsv: &str| -> Vec<String> {
            let row = tsv
                .lines()
                .find(|l| !l.is_empty())
                .expect("at least one row");
            let cols: Vec<&str> = row.split('\t').collect();
            // plus_vals (cols 4-6), minus_vals (cols 7-9), schedule (cols 11-14)
            [4, 5, 6, 7, 8, 9, 11, 12, 13, 14]
                .iter()
                .filter(|&&c| c < cols.len())
                .map(|&c| cols[c].to_owned())
                .collect()
        };
        assert_eq!(
            iter0_cols(&traj),
            iter0_cols(&traj2),
            "same-seed iter-0 Δ/schedule columns must be identical; \
             run-1 row-0:\n{}\nrun-2 row-0:\n{}",
            traj.lines().next().unwrap_or(""),
            traj2.lines().next().unwrap_or("")
        );
    }
}
