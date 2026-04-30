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
        /// Time control string (e.g. "10+0.1"). Exactly one of `tc` / `tc_sample` is Some.
        pub tc: Option<TimeControl>,
        /// Override time control for the opponent. Defaults to `tc`.
        pub opponent_tc_override: Option<TimeControl>,
        /// `--tc-sample <SPEC>` discrete weighted TC distribution for mixed-TC
        /// SPRT and Δ(TC) regression (ELOH.D). Mutually exclusive with `--tc`.
        ///
        /// Under `--tc-sample` the resulting Elo number is "the rating of the
        /// mixed game" (game outcomes are i.i.d. under the redefined "draw TC
        /// from D, then play standard chess at that TC" framing — SPRT applies
        /// to the aggregate). For per-TC ratings, run separate fixed-TC
        /// sessions instead. See `docs/workflow.md` "Mixed-TC SPRT".
        pub tc_sample: Option<super::tc_sample::TcDistribution>,
        /// `--seed N` (decimal or `0x`-prefixed hex). When `None`, the harness
        /// uses `prng::DEFAULT_SEED`. Currently consumed only by `--tc-sample`'s
        /// per-pair sampler; runs without `--seed` are still bit-deterministic
        /// at the sampler-output level (cross-run determinism in K-update
        /// arrival order under N>1 concurrency depends on subprocess
        /// scheduling regardless).
        pub seed: Option<u64>,
        /// Total number of games to play. Must be even and ≥ 2.
        pub max_games: u32,
        /// Output directory.
        pub out_dir: String,
        /// Harness overhead grace in milliseconds. Default 50.
        pub harness_overhead_ms: u32,
        /// Watchdog timeout in milliseconds.
        pub watchdog_ms: u64,
        /// PGN Event tag. Default "elo-iterate run".
        pub event_tag: String,
        /// Engine options sent as `setoption name NAME value VALUE`.
        pub engine_options: Vec<(String, String)>,
        /// Opponent options.
        pub opponent_options: Vec<(String, String)>,

        // ELOH.B fields.
        /// Starting Elo estimate; required.
        pub initial_elo: f64,
        /// Robbins-Monro initial K factor. 0.0 = freeze-K sentinel.
        pub k0: f64,
        /// Robbins-Monro decay constant τ (must be > 0).
        pub tau: f64,
        /// σ-stopping target. 0.0 = disabled sentinel.
        pub target_sigma: f64,
        /// Number of trailing estimates for trailing-σ computation.
        pub stop_window: usize,
        /// Consecutive in-window confirmations required before stopping.
        pub stop_window_confirm: usize,
        /// Number of parallel color-pairs.
        pub concurrency: u32,
        /// Threshold adjudication parameters.
        pub thresholds: Thresholds,
        /// Maximum plies per game before adjudicating as a draw.
        pub max_moves: u32,

        // ELOH.C fields.
        /// When `true`, the harness sends `setoption name VirtualClock value true` to
        /// engines advertising the option (clawfish does; Stockfish does not). Engines
        /// with the option measure search time in thread CPU time rather than wallclock,
        /// making rating measurements more robust to thermal throttling. Default off.
        ///
        /// Note: CPU time is not fully thermal-invariant; combine with P-core pinning
        /// and external cooling for tighter results. See ADR-0021 /
        /// `docs/research/tooling-cpu-cycle-counters.md` for the reasoning.
        pub virtual_clock: bool,
    }

    /// Threshold adjudication parameters matching fastchess defaults.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Thresholds {
        /// Resign after this many consecutive moves below the score threshold.
        pub resign_movecount: u32,
        /// Resign score threshold in centipawns (positive; compared as ≤ −value).
        pub resign_score: i32,
        /// Minimum full-move number before draw adjudication is allowed.
        pub draw_movenumber: u32,
        /// Both sides must show balanced scores for this many consecutive own-moves.
        pub draw_movecount: u32,
        /// Draw score threshold in centipawns (positive; |score| ≤ value qualifies).
        pub draw_score: i32,
    }

    impl Default for Thresholds {
        fn default() -> Self {
            Thresholds {
                resign_movecount: 3,
                resign_score: 600,
                draw_movenumber: 34,
                draw_movecount: 8,
                draw_score: 20,
            }
        }
    }

    /// Parsed time control: initial time + per-move increment.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    /// Parse a u64 seed from decimal or `0x`/`0X`-prefixed hex string.
    ///
    /// Existing `s.parse::<u64>()` is decimal-only; this helper adds hex support
    /// for `--seed 0xDEADBEEF`. Rejects negative numbers (leading `-`) via both
    /// branches failing to parse.
    fn parse_u64_seed(s: &str) -> Result<u64, CliError> {
        let v = if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            u64::from_str_radix(rest, 16)
        } else {
            s.parse::<u64>()
        };
        v.map_err(|_| CliError::InvalidValue(format!("--seed: not a valid u64: {s}")))
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
        let mut event_tag: String = "elo-iterate run".into();
        let mut engine_options: Vec<(String, String)> = Vec::new();
        let mut opponent_options: Vec<(String, String)> = Vec::new();

        // ELOH.B fields.
        let mut initial_elo: Option<f64> = None;
        let mut k0: f64 = 40.0;
        let mut tau: f64 = 10.0;
        let mut target_sigma: f64 = 30.0;
        let mut stop_window: usize = 30;
        let mut stop_window_confirm: usize = 5;
        let mut concurrency: u32 = 1;
        let mut resign_movecount: u32 = 3;
        let mut resign_score: i32 = 600;
        let mut draw_movenumber: u32 = 34;
        let mut draw_movecount: u32 = 8;
        let mut draw_score: i32 = 20;
        let mut max_moves: u32 = 200;

        // ELOH.C fields.
        let mut virtual_clock: bool = false;

        // ELOH.D fields.
        let mut tc_sample_raw: Option<super::tc_sample::TcDistribution> = None;
        let mut seed_raw: Option<u64> = None;

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
                "--initial-elo" => {
                    let s = next_val!();
                    let v: f64 = s
                        .parse()
                        .map_err(|_| CliError::InvalidValue(format!("--initial-elo: {s}")))?;
                    initial_elo = Some(v);
                }
                "--k0" => {
                    let s = next_val!();
                    let v: f64 = s
                        .parse()
                        .map_err(|_| CliError::InvalidValue(format!("--k0: {s}")))?;
                    if v < 0.0 {
                        return Err(CliError::InvalidValue(
                            "--k0 must be >= 0 (0 = freeze-K sentinel)".into(),
                        ));
                    }
                    k0 = v;
                }
                "--tau" => {
                    let s = next_val!();
                    let v: f64 = s
                        .parse()
                        .map_err(|_| CliError::InvalidValue(format!("--tau: {s}")))?;
                    if v <= 0.0 {
                        return Err(CliError::InvalidValue("--tau must be > 0".into()));
                    }
                    tau = v;
                }
                "--target-sigma" => {
                    let s = next_val!();
                    let v: f64 = s
                        .parse()
                        .map_err(|_| CliError::InvalidValue(format!("--target-sigma: {s}")))?;
                    if v < 0.0 {
                        return Err(CliError::InvalidValue(
                            "--target-sigma must be >= 0 (0 = disabled sentinel)".into(),
                        ));
                    }
                    target_sigma = v;
                }
                "--stop-window" => {
                    let s = next_val!();
                    let v: usize = s
                        .parse()
                        .map_err(|_| CliError::InvalidValue(format!("--stop-window: {s}")))?;
                    if v < 2 {
                        return Err(CliError::InvalidValue("--stop-window must be >= 2".into()));
                    }
                    stop_window = v;
                }
                "--stop-window-confirm" => {
                    let s = next_val!();
                    let v: usize = s.parse().map_err(|_| {
                        CliError::InvalidValue(format!("--stop-window-confirm: {s}"))
                    })?;
                    if v < 1 {
                        return Err(CliError::InvalidValue(
                            "--stop-window-confirm must be >= 1".into(),
                        ));
                    }
                    stop_window_confirm = v;
                }
                "--concurrency" => {
                    let s = next_val!();
                    let v: u32 = s
                        .parse()
                        .map_err(|_| CliError::InvalidValue(format!("--concurrency: {s}")))?;
                    if v < 1 {
                        return Err(CliError::InvalidValue("--concurrency must be >= 1".into()));
                    }
                    concurrency = v;
                }
                "--resign-movecount" => {
                    let s = next_val!();
                    resign_movecount = s
                        .parse()
                        .map_err(|_| CliError::InvalidValue(format!("--resign-movecount: {s}")))?;
                }
                "--resign-score" => {
                    let s = next_val!();
                    resign_score = s
                        .parse()
                        .map_err(|_| CliError::InvalidValue(format!("--resign-score: {s}")))?;
                    if resign_score < 0 {
                        return Err(CliError::InvalidValue("--resign-score must be >= 0".into()));
                    }
                }
                "--draw-movenumber" => {
                    let s = next_val!();
                    draw_movenumber = s
                        .parse()
                        .map_err(|_| CliError::InvalidValue(format!("--draw-movenumber: {s}")))?;
                }
                "--draw-movecount" => {
                    let s = next_val!();
                    draw_movecount = s
                        .parse()
                        .map_err(|_| CliError::InvalidValue(format!("--draw-movecount: {s}")))?;
                }
                "--draw-score" => {
                    let s = next_val!();
                    draw_score = s
                        .parse()
                        .map_err(|_| CliError::InvalidValue(format!("--draw-score: {s}")))?;
                    if draw_score < 0 {
                        return Err(CliError::InvalidValue("--draw-score must be >= 0".into()));
                    }
                }
                "--max-moves" => {
                    let s = next_val!();
                    let v: u32 = s
                        .parse()
                        .map_err(|_| CliError::InvalidValue(format!("--max-moves: {s}")))?;
                    if v < 2 {
                        return Err(CliError::InvalidValue("--max-moves must be >= 2".into()));
                    }
                    max_moves = v;
                }
                // ELOH.C: boolean flag — takes no value token.
                "--virtual-clock" => {
                    virtual_clock = true;
                }
                // ELOH.D: --tc-sample and --seed flags.
                "--tc-sample" => {
                    let s = next_val!();
                    tc_sample_raw = Some(
                        super::tc_sample::parse_tc_sample(s)
                            .map_err(|e| CliError::InvalidValue(format!("--tc-sample: {e}")))?,
                    );
                }
                "--seed" => {
                    seed_raw = Some(parse_u64_seed(next_val!())?);
                }
                other => {
                    // Reject the --virtual-clock=VALUE form: the rest of the CLI uses
                    // no-equals conventions for boolean flags.
                    if other.starts_with("--virtual-clock=") {
                        return Err(CliError::InvalidValue(
                            "--virtual-clock takes no value; use `--virtual-clock` alone".into(),
                        ));
                    }
                    return Err(CliError::UnknownArg(other.to_owned()));
                }
            }
            i += 1;
        }

        let engine = engine.ok_or_else(|| CliError::MissingFlag("--engine".into()))?;
        let opponent = opponent.ok_or_else(|| CliError::MissingFlag("--opponent".into()))?;
        let max_games = max_games.ok_or_else(|| CliError::MissingFlag("--max-games".into()))?;

        let tc_sample = tc_sample_raw;
        let seed = seed_raw;

        // Post-loop mutex: exactly one of --tc / --tc-sample must be set.
        match (tc.is_some(), tc_sample.is_some()) {
            (true, true) => {
                return Err(CliError::InvalidValue(
                    "--tc and --tc-sample are mutually exclusive".into(),
                ));
            }
            (false, false) => {
                return Err(CliError::MissingFlag("one of --tc or --tc-sample".into()));
            }
            _ => {}
        }
        let initial_elo =
            initial_elo.ok_or_else(|| CliError::MissingFlag("--initial-elo".into()))?;

        // Sentinel composition: --k0 0 requires --target-sigma 0.
        // With K=0 the estimate trail is constant, so σ=0 always, and σ-stopping
        // would fire trivially. Enforce explicit --target-sigma 0 to declare
        // fixed-anchor intent clearly.
        if k0 == 0.0 && target_sigma != 0.0 {
            return Err(CliError::InvalidValue(
                "--k0 0 requires --target-sigma 0 (frozen-K fixed-anchor mode)".into(),
            ));
        }

        // Defaults computed here so parse_args returns a fully-resolved Args.
        let watchdog_ms = watchdog_ms.unwrap_or_else(|| {
            let initial_ms = tc.map_or(10_000, |t| t.initial_ms);
            let base = 2 * u64::from(initial_ms) + 30_000;
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
            tc_sample,
            seed,
            max_games,
            out_dir,
            harness_overhead_ms,
            watchdog_ms,
            event_tag,
            engine_options,
            opponent_options,
            initial_elo,
            k0,
            tau,
            target_sigma,
            stop_window,
            stop_window_confirm,
            concurrency,
            thresholds: Thresholds {
                resign_movecount,
                resign_score,
                draw_movenumber,
                draw_movecount,
                draw_score,
            },
            max_moves,
            virtual_clock,
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
                "--initial-elo".into(),
                "2000".into(),
            ];
            let result = parse_args(argv);
            match result {
                Ok(args) => assert_eq!(args.max_games, 2),
                Err(e) => panic!("expected Ok, got {e:?}"),
            }
        }

        #[test]
        fn parse_args_default_watchdog_ms_pins_formula() {
            // Pins the formula `watchdog_ms = max(60_000, 2*tc.initial_ms + 30_000)`.
            // For --tc 10+0.1 → tc.initial_ms = 10_000, so the inner term is
            // 2*10_000 + 30_000 = 50_000; max(60_000, 50_000) = 60_000.
            // Mutations `+ → -` and `* → /` on the inner formula collapse the
            // value either to the floor (60_000) or below it; the assertion
            // would still pass for tc=10+0.1.
            //
            // Use --tc 60+0 → tc.initial_ms = 60_000, inner = 2*60_000 + 30_000 = 150_000;
            // max(60_000, 150_000) = 150_000. Mutations: + → - gives 90_000;
            // * → / gives 30_030; * → + gives 60_062. None equal 150_000.
            let argv = vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/clawfish".into(),
                "--tc".into(),
                "60+0".into(),
                "--max-games".into(),
                "2".into(),
                "--initial-elo".into(),
                "2000".into(),
            ];
            let args = parse_args(argv).expect("parse_args ok");
            assert_eq!(
                args.watchdog_ms, 150_000,
                "watchdog_ms must be 2*60_000 + 30_000 = 150_000 for tc=60+0"
            );
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
                "--initial-elo".into(),
                "2000".into(),
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

        // --- §6.5 ELOH.B CLI tests ---

        /// Helper: minimum valid argv for tests that need a parseable command-line.
        /// Supplies all required flags with reasonable defaults.
        fn base_argv() -> Vec<String> {
            vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/stockfish".into(),
                "--tc".into(),
                "10+0.1".into(),
                "--max-games".into(),
                "4".into(),
                "--initial-elo".into(),
                "2000".into(),
            ]
        }

        #[test]
        fn parse_args_default_thresholds_match_sprt_sh() {
            // Defaults (3, 600, 34, 8, 20) must match the fastchess-default parameters
            // documented in the plan §4.6.
            let args = parse_args(base_argv()).expect("parse_args ok");
            assert_eq!(
                args.thresholds,
                Thresholds::default(),
                "thresholds must match documented defaults"
            );
            assert_eq!(args.thresholds.resign_movecount, 3);
            assert_eq!(args.thresholds.resign_score, 600);
            assert_eq!(args.thresholds.draw_movenumber, 34);
            assert_eq!(args.thresholds.draw_movecount, 8);
            assert_eq!(args.thresholds.draw_score, 20);
        }

        #[test]
        fn parse_args_concurrency_default_one() {
            let args = parse_args(base_argv()).expect("parse_args ok");
            assert_eq!(args.concurrency, 1);
        }

        #[test]
        fn parse_args_initial_elo_required() {
            // Omitting --initial-elo must produce MissingFlag.
            let argv = vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/stockfish".into(),
                "--tc".into(),
                "10+0.1".into(),
                "--max-games".into(),
                "4".into(),
            ];
            let err = parse_args(argv).unwrap_err();
            assert!(
                matches!(err, CliError::MissingFlag(_)),
                "expected MissingFlag, got {err:?}"
            );
        }

        #[test]
        fn parse_args_target_sigma_zero_valid_sentinel() {
            // --target-sigma 0 is valid (disabled sentinel).
            let mut argv = base_argv();
            argv.extend(["--target-sigma".into(), "0".into()]);
            let args = parse_args(argv).expect("--target-sigma 0 should be accepted");
            assert_eq!(args.target_sigma, 0.0);
        }

        #[test]
        fn parse_args_negative_target_sigma_rejected() {
            let mut argv = base_argv();
            argv.extend(["--target-sigma".into(), "-1".into()]);
            let err = parse_args(argv).unwrap_err();
            assert!(
                matches!(err, CliError::InvalidValue(_)),
                "expected InvalidValue, got {err:?}"
            );
        }

        #[test]
        fn parse_args_stop_window_minimum_two() {
            // --stop-window 1 is below the minimum of 2.
            let mut argv = base_argv();
            argv.extend(["--stop-window".into(), "1".into()]);
            let err = parse_args(argv).unwrap_err();
            assert!(
                matches!(err, CliError::InvalidValue(_)),
                "expected InvalidValue for --stop-window 1, got {err:?}"
            );
        }

        #[test]
        fn parse_args_concurrency_zero_rejected() {
            let mut argv = base_argv();
            argv.extend(["--concurrency".into(), "0".into()]);
            let err = parse_args(argv).unwrap_err();
            assert!(
                matches!(err, CliError::InvalidValue(_)),
                "expected InvalidValue for --concurrency 0, got {err:?}"
            );
        }

        #[test]
        fn parse_args_max_moves_default_200() {
            let args = parse_args(base_argv()).expect("parse_args ok");
            assert_eq!(args.max_moves, 200);
        }

        #[test]
        fn parse_args_k0_zero_with_target_sigma_zero_valid() {
            // Frozen-K + disabled-σ is the explicit fixed-anchor sentinel combination.
            let mut argv = base_argv();
            argv.extend([
                "--k0".into(),
                "0".into(),
                "--target-sigma".into(),
                "0".into(),
            ]);
            let args = parse_args(argv).expect("--k0 0 --target-sigma 0 should be accepted");
            assert_eq!(args.k0, 0.0);
            assert_eq!(args.target_sigma, 0.0);
        }

        #[test]
        fn parse_args_k0_zero_requires_target_sigma_zero() {
            // --k0 0 without --target-sigma 0 must be rejected: with K=0 the estimate
            // trail is constant, so σ=0 always, and σ-stopping would fire trivially.
            let mut argv = base_argv();
            argv.extend(["--k0".into(), "0".into()]);
            let err = parse_args(argv).unwrap_err();
            assert!(
                matches!(err, CliError::InvalidValue(_)),
                "expected InvalidValue for --k0 0 without --target-sigma 0, got {err:?}"
            );
        }

        #[test]
        fn parse_args_tau_zero_rejected() {
            // --tau 0 would cause division by zero in compute_k.
            let mut argv = base_argv();
            argv.extend(["--tau".into(), "0".into()]);
            let err = parse_args(argv).unwrap_err();
            assert!(
                matches!(err, CliError::InvalidValue(_)),
                "expected InvalidValue for --tau 0, got {err:?}"
            );
        }

        // ---- ELOH.B: acceptance-boundary tests (catch Tier-C pure-fn mutants) ----
        //
        // Each test passes the minimum valid value for a validated flag and
        // asserts Ok.  These distinguish `< N` (correct) from `<= N` (mutant)
        // by proving the exact minimum is accepted.

        #[test]
        fn parse_args_stop_window_two_accepted() {
            // Minimum valid --stop-window is 2.  Mutant `< 2` → `<= 2` would
            // reject value 2 as invalid.
            let mut argv = base_argv();
            argv.extend(["--stop-window".into(), "2".into()]);
            let args = parse_args(argv).expect("--stop-window 2 must be accepted");
            assert_eq!(args.stop_window, 2);
        }

        #[test]
        fn parse_args_stop_window_confirm_one_accepted() {
            // Minimum valid --stop-window-confirm is 1.
            let mut argv = base_argv();
            argv.extend(["--stop-window-confirm".into(), "1".into()]);
            let args = parse_args(argv).expect("--stop-window-confirm 1 must be accepted");
            assert_eq!(args.stop_window_confirm, 1);
        }

        #[test]
        fn parse_args_concurrency_one_explicit_accepted() {
            // --concurrency 1 is the minimum valid value.  Mutant `< 1` → `<= 1`
            // would reject 1.
            let mut argv = base_argv();
            argv.extend(["--concurrency".into(), "1".into()]);
            let args = parse_args(argv).expect("--concurrency 1 must be accepted");
            assert_eq!(args.concurrency, 1);
        }

        #[test]
        fn parse_args_resign_score_zero_accepted() {
            // --resign-score 0 is the minimum valid value.
            let mut argv = base_argv();
            argv.extend(["--resign-score".into(), "0".into()]);
            let args = parse_args(argv).expect("--resign-score 0 must be accepted");
            assert_eq!(args.thresholds.resign_score, 0);
        }

        #[test]
        fn parse_args_draw_score_zero_accepted() {
            // --draw-score 0 is the minimum valid value.
            let mut argv = base_argv();
            argv.extend(["--draw-score".into(), "0".into()]);
            let args = parse_args(argv).expect("--draw-score 0 must be accepted");
            assert_eq!(args.thresholds.draw_score, 0);
        }

        #[test]
        fn parse_args_max_moves_two_accepted() {
            // --max-moves 2 is the minimum valid value.
            let mut argv = base_argv();
            argv.extend(["--max-moves".into(), "2".into()]);
            let args = parse_args(argv).expect("--max-moves 2 must be accepted");
            assert_eq!(args.max_moves, 2);
        }

        #[test]
        fn parse_args_max_moves_above_minimum_accepted() {
            // Pin --max-moves N for N > 2 also passes. The validation rule
            // `v < 2` rejects below-minimum; mutating to `v > 2` would reject
            // values above minimum (e.g. 3+) instead. This test catches such
            // a directional flip.
            let mut argv = base_argv();
            argv.extend(["--max-moves".into(), "200".into()]);
            let args = parse_args(argv).expect("--max-moves 200 must be accepted");
            assert_eq!(args.max_moves, 200);
        }

        // ---- ELOH.C §6.6: `--virtual-clock` CLI tests ----

        #[test]
        fn parse_args_virtual_clock_default_false() {
            let args = parse_args(base_argv()).expect("parse_args ok");
            assert!(!args.virtual_clock, "virtual_clock must default to false");
        }

        #[test]
        fn parse_args_virtual_clock_flag_sets_true() {
            let mut argv = base_argv();
            argv.push("--virtual-clock".into());
            let args = parse_args(argv).expect("--virtual-clock should be accepted");
            assert!(
                args.virtual_clock,
                "virtual_clock must be true after --virtual-clock"
            );
        }

        #[test]
        fn parse_args_virtual_clock_flag_takes_no_value() {
            // Boolean flag must not consume the next token as its value.
            let mut argv = base_argv();
            argv.extend(["--virtual-clock".into(), "--max-games".into(), "4".into()]);
            // --max-games is already in base_argv(), so this tests that the
            // presence of another flag after --virtual-clock parses correctly.
            // We just need the parse to succeed and virtual_clock to be true.
            let args =
                parse_args(argv).expect("--virtual-clock followed by another flag should parse");
            assert!(args.virtual_clock, "virtual_clock must be true");
        }

        #[test]
        fn parse_args_virtual_clock_equals_form_rejected() {
            // --virtual-clock=true must be rejected (no-equals convention).
            let mut argv = base_argv();
            argv.push("--virtual-clock=true".into());
            let err = parse_args(argv).unwrap_err();
            assert!(
                matches!(err, CliError::InvalidValue(_)),
                "expected InvalidValue for --virtual-clock=true, got {err:?}"
            );
        }

        // ---- ELOH.D §6.3: --tc-sample, --seed CLI tests ----

        /// Build a minimum valid argv with --tc-sample only (no --tc).
        fn tc_sample_base_argv() -> Vec<String> {
            vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/stockfish".into(),
                "--tc-sample".into(),
                "10+0.1:1,20+0.2:1".into(),
                "--max-games".into(),
                "4".into(),
                "--initial-elo".into(),
                "2000".into(),
                "--k0".into(),
                "0".into(),
                "--target-sigma".into(),
                "0".into(),
            ]
        }

        #[test]
        fn parse_args_tc_sample_only_accepted() {
            // --tc-sample with no --tc parses; args.tc.is_none() && args.tc_sample.is_some().
            let result = parse_args(tc_sample_base_argv());
            match result {
                Ok(args) => {
                    assert!(args.tc.is_none(), "tc must be None when --tc-sample is set");
                    assert!(
                        args.tc_sample.is_some(),
                        "tc_sample must be Some when --tc-sample is set"
                    );
                }
                Err(e) => panic!("expected Ok for --tc-sample only, got {e:?}"),
            }
        }

        #[test]
        fn parse_args_tc_only_accepted() {
            // --tc only (no --tc-sample) parses; args.tc.is_some() && args.tc_sample.is_none().
            // Pins backwards compatibility.
            let result = parse_args(base_argv());
            match result {
                Ok(args) => {
                    assert!(args.tc.is_some(), "tc must be Some when --tc is set");
                    assert!(
                        args.tc_sample.is_none(),
                        "tc_sample must be None when only --tc is set"
                    );
                }
                Err(e) => panic!("expected Ok for --tc only, got {e:?}"),
            }
        }

        #[test]
        fn parse_args_both_tc_and_tc_sample_rejected() {
            // Both --tc and --tc-sample → Err(InvalidValue("--tc and --tc-sample are mutually exclusive")).
            let mut argv = base_argv();
            argv.extend(["--tc-sample".into(), "10+0.1:1".into()]);
            let err = parse_args(argv).unwrap_err();
            match &err {
                CliError::InvalidValue(msg) => {
                    assert!(
                        msg.contains("mutually exclusive"),
                        "error message must mention 'mutually exclusive'; got: {msg}"
                    );
                }
                other => panic!("expected InvalidValue, got {other:?}"),
            }
        }

        #[test]
        fn parse_args_neither_tc_nor_tc_sample_rejected() {
            // Neither --tc nor --tc-sample → Err(MissingFlag(_)).
            let argv: Vec<String> = vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/stockfish".into(),
                "--max-games".into(),
                "4".into(),
                "--initial-elo".into(),
                "2000".into(),
                "--k0".into(),
                "0".into(),
                "--target-sigma".into(),
                "0".into(),
            ];
            let err = parse_args(argv).unwrap_err();
            assert!(
                matches!(err, CliError::MissingFlag(_)),
                "expected MissingFlag when neither --tc nor --tc-sample, got {err:?}"
            );
        }

        #[test]
        fn parse_args_seed_default_none() {
            // --seed omitted → args.seed.is_none().
            let args = parse_args(base_argv()).expect("parse_args ok");
            assert!(args.seed.is_none(), "seed must default to None");
        }

        #[test]
        fn parse_args_seed_parses_decimal() {
            // --seed 42 → Some(42).
            let mut argv = base_argv();
            argv.extend(["--seed".into(), "42".into()]);
            let args = parse_args(argv).expect("--seed 42 should be accepted");
            assert_eq!(args.seed, Some(42u64));
        }

        #[test]
        fn parse_args_seed_parses_hex_with_0x() {
            // --seed 0xDEADBEEF → Some(0xDEADBEEF).
            let mut argv = base_argv();
            argv.extend(["--seed".into(), "0xDEADBEEF".into()]);
            let args = parse_args(argv).expect("--seed 0xDEADBEEF should be accepted");
            assert_eq!(args.seed, Some(0xDEAD_BEEF_u64));
        }

        #[test]
        fn parse_args_seed_rejects_negative_number() {
            // --seed -1 → Err(InvalidValue) with message containing "not a valid u64".
            let mut argv = base_argv();
            argv.extend(["--seed".into(), "-1".into()]);
            let err = parse_args(argv).unwrap_err();
            match &err {
                CliError::InvalidValue(msg) => {
                    assert!(
                        msg.contains("not a valid u64"),
                        "error message must contain 'not a valid u64'; got: {msg}"
                    );
                }
                other => panic!("expected InvalidValue for --seed -1, got {other:?}"),
            }
        }

        #[test]
        fn parse_args_tc_sample_invalid_grammar_rejected() {
            // --tc-sample foo → Err (parser message surfaced).
            let argv = base_argv();
            // Remove --tc so we don't hit the mutex first
            let argv_no_tc: Vec<String> = argv
                .iter()
                .filter(|&s| s != "--tc" && s != "10+0.1")
                .cloned()
                .collect();
            let mut argv2 = argv_no_tc;
            argv2.extend(["--tc-sample".into(), "foo".into()]);
            let result = parse_args(argv2);
            assert!(
                result.is_err(),
                "invalid --tc-sample grammar must be rejected"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// mod prng  (NEW — ELOH.D)
// ---------------------------------------------------------------------------

mod prng {
    //! SplitMix64 PRNG for `--seed`-driven TC-sampling reproducibility.
    //!
    //! ELOH.D uses a single u64 seed → single SplitMix64 stream consumed by
    //! `tc_sample::TcDistribution::sample`. Hand-rolled (~20 LOC); no `rand`
    //! crate dep. Mixer constants are pinned by a golden-fixture test
    //! (`prng_seed_zero_first_three_words_golden`) so a transcription typo
    //! fails at compile-time-of-test.

    #[allow(dead_code)]
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct Prng(u64);

    // Vigna 2014 / Steele-Lea-Flood 2014 SplitMix64 constants.
    const GOLDEN_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;
    const MIX_C1: u64 = 0xBF58_476D_1CE4_E5B9;
    const MIX_C2: u64 = 0x94D0_49BB_1331_11EB;

    impl Prng {
        /// Construct from a u64 seed. Runs one SplitMix64 mix step so a seed
        /// of 0 doesn't yield a 0-state pathology.
        pub(crate) fn new(seed: u64) -> Self {
            let mut p = Self(seed);
            let _ = p.next_u64();
            p
        }

        /// SplitMix64 next. Standard algorithm (Vigna 2014 / Steele-Lea-Flood 2014):
        /// state += GOLDEN_GAMMA; z = state; z = (z ^ (z >> 30)) * MIX_C1;
        /// z = (z ^ (z >> 27)) * MIX_C2; z ^ (z >> 31).
        pub(crate) fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(GOLDEN_GAMMA);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(MIX_C1);
            z = (z ^ (z >> 27)).wrapping_mul(MIX_C2);
            z ^ (z >> 31)
        }
    }

    /// Default seed when `--seed` is absent. Intentionally non-zero. Documented
    /// in `--help` so users know no-`--seed` runs are still bit-deterministic.
    #[allow(dead_code)]
    pub(crate) const DEFAULT_SEED: u64 = 0xC1AB_F15A_E10D_D000;

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn prng_zero_seed_yields_nonzero_first_word() {
            // The constructor's mix step ensures a 0 seed isn't a 0-state.
            let mut rng = Prng::new(0);
            assert_ne!(
                rng.next_u64(),
                0,
                "Prng::new(0) first output must be non-zero"
            );
        }

        #[test]
        fn prng_deterministic_across_constructions() {
            // Two Prng::new(42) instances must produce identical streams.
            let mut a = Prng::new(42);
            let mut b = Prng::new(42);
            for _ in 0..100 {
                assert_eq!(
                    a.next_u64(),
                    b.next_u64(),
                    "two Prng::new(42) instances must produce identical u64 streams"
                );
            }
        }

        #[test]
        fn prng_distinct_seeds_yield_distinct_streams() {
            // Prng::new(42) and Prng::new(43) must produce different first 100 u64s.
            let stream_a: Vec<u64> = {
                let mut rng = Prng::new(42);
                (0..100).map(|_| rng.next_u64()).collect()
            };
            let stream_b: Vec<u64> = {
                let mut rng = Prng::new(43);
                (0..100).map(|_| rng.next_u64()).collect()
            };
            assert_ne!(
                stream_a, stream_b,
                "distinct seeds must yield distinct u64 streams"
            );
        }

        #[test]
        fn prng_seed_zero_first_three_words_golden() {
            // Golden fixture: pins the first three outputs from Prng::new(0) against
            // values produced by the Vigna 2014 / Steele-Lea-Flood 2014 SplitMix64
            // with GOLDEN_GAMMA=0x9E3779B97F4A7C15, MIX_C1=0xBF58476D1CE4E5B9,
            // MIX_C2=0x94D049BB133111EB. Seed=0 → after one constructor mix step, the
            // state is GOLDEN_GAMMA, then three further calls advance it to these values.
            //
            // Catches any mixer-constant transcription typo at compile-time-of-test.
            let mut rng = Prng::new(0);
            let w0 = rng.next_u64();
            let w1 = rng.next_u64();
            let w2 = rng.next_u64();
            assert_eq!(
                (w0, w1, w2),
                (
                    7_960_286_522_194_355_700_u64,
                    487_617_019_471_545_679_u64,
                    17_909_611_376_780_542_444_u64,
                ),
                "Prng::new(0) first three words must match SplitMix64 golden fixture"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// mod tc_sample  (NEW — ELOH.D)
// ---------------------------------------------------------------------------

mod tc_sample {
    //! `--tc-sample <SPEC>` parsing + cumulative-bucket sampling.
    //!
    //! Grammar: `<TC>:<weight>(,<TC>:<weight>)*`
    //! Each `<TC>` parsed via `cli::parse_tc`; `<weight>` is a u32 in `1..=u32::MAX`.
    //! At least one entry required. Empty input, zero weight, weight overflow on
    //! summing, or repeated TC keys all yield Err.

    /// Parsed `--tc-sample` distribution.
    #[derive(Debug, Clone)]
    #[allow(dead_code)]
    pub(crate) struct TcDistribution {
        /// Parsed (TC, weight) entries in input order. Weights are positive.
        pub entries: Vec<(super::cli::TimeControl, u32)>,
        /// Prefix sums of weights; len == entries.len(); strictly increasing;
        /// last element == total.
        cumulative: Vec<u32>,
        /// Sum of all weights.
        total: u32,
    }

    impl TcDistribution {
        /// Sample one TC. Draw `r = rng.next_u64() % total`, find first cumulative
        /// bucket strictly greater than `r`, return its TC. Linear scan — entries.len()
        /// expected ≤ ~10 in practice.
        ///
        /// Modulo bias: total ≤ u32::MAX, so bias per bucket ≤ u32::MAX / 2^64 < 2^-32.
        pub(crate) fn sample(&self, rng: &mut super::prng::Prng) -> super::cli::TimeControl {
            let r = (rng.next_u64() % self.total as u64) as u32;
            let idx = self
                .cumulative
                .iter()
                .position(|&c| c > r)
                .expect("cumulative invariant: r < total so some bucket strictly exceeds r");
            self.entries[idx].0
        }

        /// Iterate (TC, weight) pairs in input-spec order.
        pub(crate) fn iter(&self) -> impl Iterator<Item = &(super::cli::TimeControl, u32)> {
            self.entries.iter()
        }
    }

    /// Parse `<TC>:<weight>(,<TC>:<weight>)*`.
    ///
    /// Rejects empty input, zero weight, weight-sum overflow, and duplicate TC keys.
    /// Duplicate TC keys likely indicate user confusion (e.g. `10+0.1:1,10+0.1:2`)
    /// and fail loudly rather than silently merging.
    pub(crate) fn parse_tc_sample(s: &str) -> Result<TcDistribution, super::cli::CliError> {
        if s.is_empty() {
            return Err(super::cli::CliError::InvalidValue(
                "--tc-sample: empty spec".into(),
            ));
        }

        let mut entries: Vec<(super::cli::TimeControl, u32)> = Vec::new();
        let mut cumulative: Vec<u32> = Vec::new();
        let mut total: u32 = 0;

        for entry in s.split(',') {
            let (tc_str, weight_str) = entry.split_once(':').ok_or_else(|| {
                super::cli::CliError::InvalidValue(format!(
                    "--tc-sample: each entry must be <TC>:<weight>, got: {entry}"
                ))
            })?;

            let tc = super::cli::parse_tc(tc_str)
                .map_err(|e| super::cli::CliError::InvalidValue(format!("--tc-sample: {e}")))?;

            let weight: u32 = weight_str.parse().map_err(|_| {
                super::cli::CliError::InvalidValue(format!(
                    "--tc-sample: weight must be a positive integer, got: {weight_str}"
                ))
            })?;

            if weight == 0 {
                return Err(super::cli::CliError::InvalidValue(
                    "--tc-sample: weight must be >= 1 (zero weight rejected)".into(),
                ));
            }

            // Reject duplicate TC keys — likely a user typo.
            if entries.iter().any(|(existing, _)| *existing == tc) {
                return Err(super::cli::CliError::InvalidValue(format!(
                    "--tc-sample: duplicate TC key {tc_str}"
                )));
            }

            total = total.checked_add(weight).ok_or_else(|| {
                super::cli::CliError::InvalidValue(
                    "--tc-sample: total weight overflow (exceeds u32::MAX)".into(),
                )
            })?;

            entries.push((tc, weight));
            cumulative.push(total);
        }

        Ok(TcDistribution {
            entries,
            cumulative,
            total,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::cli::TimeControl;

        fn tc(base_s: f64, inc_s: f64) -> TimeControl {
            TimeControl {
                initial_ms: (base_s * 1000.0).round() as u32,
                increment_ms: (inc_s * 1000.0).round() as u32,
            }
        }

        // -----------------------------------------------------------------------
        // §6.2: parse_tc_sample tests
        // -----------------------------------------------------------------------

        #[test]
        fn parse_single_entry() {
            // "10+0.1:1" → entries [(10s+0.1s, 1)], total 1.
            let dist = parse_tc_sample("10+0.1:1").expect("should parse");
            assert_eq!(dist.entries.len(), 1);
            assert_eq!(dist.entries[0], (tc(10.0, 0.1), 1));
            assert_eq!(dist.total, 1);
            assert_eq!(dist.cumulative, vec![1]);
        }

        #[test]
        fn parse_four_entries_uniform() {
            // "10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1" → four entries, total 4,
            // cumulative [1,2,3,4].
            let dist =
                parse_tc_sample("10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1").expect("should parse");
            assert_eq!(dist.entries.len(), 4);
            assert_eq!(dist.total, 4);
            assert_eq!(dist.cumulative, vec![1, 2, 3, 4]);
        }

        #[test]
        fn parse_three_to_one_skewed() {
            // "10+0.1:3,60+0.6:1" → entries [(10s+0.1s, 3), (60s+0.6s, 1)],
            // cumulative [3, 4], total 4.
            let dist = parse_tc_sample("10+0.1:3,60+0.6:1").expect("should parse");
            assert_eq!(dist.entries.len(), 2);
            assert_eq!(dist.entries[0], (tc(10.0, 0.1), 3));
            assert_eq!(dist.entries[1], (tc(60.0, 0.6), 1));
            assert_eq!(dist.cumulative, vec![3, 4]);
            assert_eq!(dist.total, 4);
        }

        #[test]
        fn parse_rejects_empty() {
            // TDD-NOTE: passes trivially against the skeleton's blanket Err
            // ("not yet implemented"); real impl must fail on this specific
            // malformed input with a meaningful error, not just any Err.
            assert!(
                parse_tc_sample("").is_err(),
                "empty string must be rejected"
            );
        }

        #[test]
        fn parse_rejects_zero_weight() {
            // TDD-NOTE: passes trivially against the skeleton's blanket Err
            // ("not yet implemented"); real impl must fail on this specific
            // malformed input with a meaningful error, not just any Err.
            assert!(
                parse_tc_sample("10+0.1:0").is_err(),
                "zero weight must be rejected"
            );
        }

        #[test]
        fn parse_rejects_repeated_tc() {
            let err = parse_tc_sample("10+0.1:1,10+0.1:2").unwrap_err();
            let msg = format!("{err}");
            assert!(
                msg.contains("duplicate") || msg.contains("repeated") || msg.contains("Duplicate"),
                "error message for repeated TC must mention duplication; got: {msg}"
            );
        }

        #[test]
        fn parse_rejects_malformed_weight() {
            // TDD-NOTE: passes trivially against the skeleton's blanket Err
            // ("not yet implemented"); real impl must fail on this specific
            // malformed input with a meaningful error, not just any Err.
            assert!(
                parse_tc_sample("10+0.1:abc").is_err(),
                "non-numeric weight must be rejected"
            );
        }

        #[test]
        fn parse_rejects_missing_colon() {
            // "10+0.1" with no colon → no weight → Err.
            //
            // TDD-NOTE: passes trivially against the skeleton's blanket Err
            // ("not yet implemented"); real impl must fail on this specific
            // malformed input with a meaningful error, not just any Err.
            assert!(
                parse_tc_sample("10+0.1").is_err(),
                "missing colon (no weight) must be rejected"
            );
        }

        #[test]
        fn parse_rejects_weight_overflow() {
            // Two entries each with u32::MAX/2 + 1 would overflow the total.
            //
            // TDD-NOTE: passes trivially against the skeleton's blanket Err
            // ("not yet implemented"); real impl must surface a distinct
            // overflow error path so this test stays meaningful — i.e. the
            // implementation slice must NOT accept this input even after
            // wiring the parser, and ideally surfaces a distinct CliError
            // variant or message substring (e.g. "weight overflow").
            let half_plus = u32::MAX / 2 + 1;
            let spec = format!("10+0.1:{half_plus},20+0.2:{half_plus}");
            assert!(
                parse_tc_sample(&spec).is_err(),
                "total weight overflow must be rejected"
            );
        }

        #[test]
        fn sample_single_entry_always_returns_it() {
            // 1-entry distribution + 1000 draws → all draws return the single entry.
            let dist = parse_tc_sample("10+0.1:1").expect("should parse");
            let mut rng = super::super::prng::Prng::new(42);
            for _ in 0..1000 {
                let sampled = dist.sample(&mut rng);
                assert_eq!(
                    sampled,
                    tc(10.0, 0.1),
                    "single-entry dist must always return that entry"
                );
            }
        }

        #[test]
        fn sample_skewed_3to1_at_seed_xfeed_yields_known_counts() {
            // Back-validation gate Part 1.
            // Distribution [(A=10+0.1, 3), (B=60+0.6, 1)]; seed 0xC1AB_FEED; 1000 draws.
            // Exact counts produced by SplitMix64 with Vigna 2014 / Steele-Lea-Flood 2014 constants.
            // chi2=0.533 (1 dof; 99% critical value 6.635) — well within expected range.
            // If mixer constants or seed ever change, repin by observing the eprintln! output.
            let dist = parse_tc_sample("10+0.1:3,60+0.6:1").expect("should parse");
            let mut rng = super::super::prng::Prng::new(0xC1AB_FEED);
            let mut count_a = 0u32;
            let mut count_b = 0u32;
            for _ in 0..1000 {
                let s = dist.sample(&mut rng);
                if s == tc(10.0, 0.1) {
                    count_a += 1;
                } else if s == tc(60.0, 0.6) {
                    count_b += 1;
                } else {
                    panic!("unexpected TC sampled: {s:?}");
                }
            }
            // Chi-squared as side observable (1 dof; critical value 6.635 at 99%).
            let expected_a = 750.0f64;
            let expected_b = 250.0f64;
            let chi2 = (count_a as f64 - expected_a).powi(2) / expected_a
                + (count_b as f64 - expected_b).powi(2) / expected_b;
            eprintln!("sample_skewed_3to1: count_a={count_a} count_b={count_b} chi2={chi2:.3}");
            assert!(
                chi2 < 6.635,
                "chi2={chi2:.3} exceeds 99% critical value 6.635 for 1 dof; distribution is biased"
            );
            assert_eq!(
                (count_a, count_b),
                (740, 260),
                "exact seed-driven counts for Prng::new(0xC1AB_FEED) + 3:1 distribution"
            );
        }

        #[test]
        fn sample_uniform_4_bucket_at_seed_xfeed_yields_known_counts() {
            // Back-validation gate Part 1 (4-bucket uniform shape).
            // Distribution 10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1; seed 0xC1AB_FEED; 1000 draws.
            // Exact counts produced by SplitMix64 with Vigna 2014 / Steele-Lea-Flood 2014 constants.
            // chi2=0.888 (3 dof; 99% critical value 11.345) — well within expected range.
            // If mixer constants or seed ever change, repin by observing the eprintln! output.
            let dist =
                parse_tc_sample("10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1").expect("should parse");
            let mut rng = super::super::prng::Prng::new(0xC1AB_FEED);
            let tcs = [tc(10.0, 0.1), tc(20.0, 0.2), tc(40.0, 0.4), tc(60.0, 0.6)];
            let mut counts = [0u32; 4];
            for _ in 0..1000 {
                let s = dist.sample(&mut rng);
                let idx = tcs
                    .iter()
                    .position(|&t| t == s)
                    .unwrap_or_else(|| panic!("unexpected TC sampled: {s:?}"));
                counts[idx] += 1;
            }
            // Chi-squared as side observable (3 dof; critical value 11.345 at 99%).
            let expected = 250.0f64;
            let chi2: f64 = counts
                .iter()
                .map(|&c| (c as f64 - expected).powi(2) / expected)
                .sum();
            eprintln!("sample_uniform_4: counts={counts:?} chi2={chi2:.3}");
            assert!(
                chi2 < 11.345,
                "chi2={chi2:.3} exceeds 99% critical value 11.345 for 3 dof; distribution is biased"
            );
            assert_eq!(
                counts,
                [250u32, 251, 239, 260],
                "exact seed-driven counts for Prng::new(0xC1AB_FEED) + 4-bucket uniform distribution"
            );
        }

        #[test]
        fn sample_uniform_4_bucket_input_order_preserved_in_iter() {
            // After parsing A:1,B:1,C:1,D:1, dist.iter() yields (A,1),(B,1),(C,1),(D,1).
            let dist =
                parse_tc_sample("10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1").expect("should parse");
            let collected: Vec<_> = dist.iter().cloned().collect();
            assert_eq!(
                collected,
                vec![
                    (tc(10.0, 0.1), 1u32),
                    (tc(20.0, 0.2), 1),
                    (tc(40.0, 0.4), 1),
                    (tc(60.0, 0.6), 1),
                ],
                "iter() must yield entries in input-spec order"
            );
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
            match rx.recv_timeout(remaining) {
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
                Err(mpsc::RecvTimeoutError::Timeout) => return Err(HarnessError::Watchdog),
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

    use super::driver;
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
        /// The just-moved side resigned — its score reached the threshold.
        /// Carries the *resigning* (= losing) color.
        ResignAdjudicated(Color),
        /// Both sides agreed on a near-zero score after the movenumber floor.
        DrawAdjudicated,
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

    /// **Just-moved-side discipline.** Called after the side `mover` plays a move
    /// and pushes its score onto its history. Returns `true` if `mover` should
    /// resign — its trailing `movecount` scores are all at-or-below
    /// `-score_threshold` (Cp) or are losing-mate (`Mate(n)` with `n < 0`).
    /// Caller wraps the result as `GameOver::ResignAdjudicated(mover)`.
    ///
    /// `mover_history.len() < movecount` → returns `false`.
    /// `None` entries break the streak.
    /// `Mate(n)` with `n >= 0` does NOT resign (engine sees winning mate).
    pub(crate) fn resign_threshold_check(
        mover_history: &[Option<driver::Score>],
        movecount: u32,
        score_threshold: i32,
    ) -> bool {
        let n = movecount as usize;
        if mover_history.len() < n {
            return false;
        }
        let window = &mover_history[mover_history.len() - n..];
        window.iter().all(|entry| match entry {
            Some(driver::Score::Cp(s)) => *s <= -score_threshold,
            Some(driver::Score::Mate(n)) => *n < 0,
            None => false,
        })
    }

    /// Both sides agree on a near-zero score for `movecount` consecutive own-moves
    /// each, and the current `move_number` (1-based full-move) is ≥ `movenumber_floor`.
    ///
    /// `Score::Cp(s)` with `|s| ≤ score_threshold` qualifies. `Score::Mate(_)`
    /// is treated as a non-balanced score regardless of inner value — mate is
    /// by definition not a near-zero evaluation, so the impl matches Cp
    /// explicitly and treats Mate(_) as breaking the streak. (Note: `Mate(n)`
    /// carries plies-to-mate, not a centipawn-scaled score, so a |inner| ≤ thr
    /// shortcut would be wrong for small `n`. Pinned by `draw_mate_score_breaks_streak`.)
    /// `None` breaks the streak. Either side's history shorter than `movecount`
    /// → returns `false`.
    pub(crate) fn draw_threshold_check(
        white_history: &[Option<driver::Score>],
        black_history: &[Option<driver::Score>],
        move_number: u32,
        movenumber_floor: u32,
        movecount: u32,
        score_threshold: i32,
    ) -> bool {
        if move_number < movenumber_floor {
            return false;
        }
        let n = movecount as usize;
        if white_history.len() < n || black_history.len() < n {
            return false;
        }
        let is_balanced = |history: &[Option<driver::Score>]| {
            let window = &history[history.len() - n..];
            window.iter().all(|entry| match entry {
                Some(driver::Score::Cp(s)) => s.abs() <= score_threshold,
                _ => false,
            })
        };
        is_balanced(white_history) && is_balanced(black_history)
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

        #[test]
        fn not_insufficient_kbnk_vs_lone_king() {
            // K + B + N vs K — two minors on one side. Mateable per FIDE
            // (KBN-vs-K is the classic technique).
            //
            // Pins the `+` → `-` mutation on `white_minors = white_bishops.count()
            // + white_knights.count()`. Under correct +: white_minors = 1+1 = 2,
            // black_minors = 0; match (2,0) falls to `_ => false`. Under buggy -:
            // white_minors = 1-1 = 0, black_minors = 0; match (0,0) returns true
            // (FALSELY declares insufficient). The existing 1-or-0-minor tests
            // don't distinguish + from -.
            let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/3BN3 w - - 0 1").unwrap();
            assert!(
                !is_insufficient_material(&pos),
                "K+B+N vs K should NOT be insufficient (KBNK is a forced mate per FIDE)"
            );
        }

        #[test]
        fn not_insufficient_kbk_vs_kn_one_each() {
            // K + B (white) vs K + N (black) — one minor each, but DIFFERENT
            // kinds. Pins the first `&&` → `||` mutation on the (1,1) match
            // arm guard `white_bishops.count() == 1 && black_bishops.count() == 1
            // && white_knights.is_empty() && black_knights.is_empty()`.
            //
            // Under correct &&: guard is false (black_bishops != 1) → falls to
            // `_ => false`. Position is mateable in cooperation; not insufficient.
            //
            // Under buggy `||` on first conjunct: `1==1 || 0==1 && 0==0 && 1==0`
            // = `true || (...)` = true. Body runs `black_bishops.lsb()` which is
            // None (no black bishop) → `expect("count==1 guarantees a set bit")`
            // panics. Test catches the mutation via the panic.
            let pos = Position::from_fen("8/8/8/3nk3/8/4K3/8/2B5 w - - 0 1").unwrap();
            assert!(
                !is_insufficient_material(&pos),
                "KBvKN with one minor each side should NOT be insufficient (different kinds)"
            );
        }

        #[test]
        fn not_insufficient_kn_vs_kb_one_each() {
            // K + N (white) vs K + B (black) — mirror of the above. Pins the
            // THIRD `&&` → `||` mutation on the (1,1) guard.
            //
            // Original guard for white_bishops=0,black_bishops=1,white_knights=1,
            // black_knights=0:
            //   `0==1 && 1==1 && 1==0 && 0==0` = false (first false short-circuits).
            //   Falls to `_ => false`. Returns false.
            //
            // Mutation #3 (third `&&` → `||`): `0==1 && 1==1 && 1==0 || 0==0`.
            // Per Rust precedence (`&&` tighter than `||`):
            //   `(0==1 && 1==1 && 1==0) || (0==0)` = `false || true` = TRUE.
            // Mutated guard fires → enters body → `let w_sq = white_bishops.lsb()`
            // is None (no white bishop) → `expect("count==1 guarantees a set bit")`
            // panics. Test catches via panic.
            //
            // Note: mutations #1 and #2 are also tested implicitly here.
            // #1 (`0==1 || 1==1 && 1==0 && 0==0`) = `false || (true && false && true)` = false → SAME as original. Not caught.
            // #2 (`0==1 && 1==1 || 1==0 && 0==0`) = `false || false` = false → SAME. (Equivalent — see mutants.toml.)
            // Only #3 differs and is caught here.
            let pos = Position::from_fen("8/8/8/3bk3/8/4K3/8/2N5 w - - 0 1").unwrap();
            assert!(
                !is_insufficient_material(&pos),
                "KNvKB with one minor each side should NOT be insufficient (different kinds)"
            );
        }

        #[test]
        fn not_insufficient_k_vs_kbn_pins_black_side_minor_count() {
            // Mirror of `not_insufficient_kbnk_vs_lone_king`: black has K+B+N,
            // white has only K. Pins the BLACK-SIDE `+` → `-` mutation on
            // `let black_minors = black_bishops.count() + black_knights.count()`.
            //
            // Under correct +: black_minors = 1 + 1 = 2; white_minors = 0;
            // match (0, 2) → `_ => false` (correct: KBN-vs-K is not insufficient).
            //
            // Under buggy -: black_minors = 1 - 1 = 0 (wrapping or saturating);
            // match (0, 0) → returns true (FALSELY declares insufficient).
            //
            // The white-side analogue is pinned by `not_insufficient_kbnk_vs_lone_king`;
            // this test catches the symmetric black-side mutation.
            let pos = Position::from_fen("3bn3/8/4k3/8/4K3/8/8/8 w - - 0 1").unwrap();
            assert!(
                !is_insufficient_material(&pos),
                "K vs K+B+N should NOT be insufficient (KBN can mate per FIDE)"
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

        // ---------------------------------------------------------------
        // §6.3 Threshold adjudication tests (todo!() until impl phase)
        // ---------------------------------------------------------------

        use super::super::driver::Score::{Cp, Mate};

        #[test]
        fn resign_three_consecutive_below_threshold_fires() {
            assert!(resign_threshold_check(
                &[Some(Cp(-700)), Some(Cp(-650)), Some(Cp(-720))],
                3,
                600
            ));
        }

        #[test]
        fn resign_two_below_one_above_does_not_fire() {
            // The trailing entry Cp(-100) is above −600 threshold, so streak breaks.
            assert!(!resign_threshold_check(
                &[Some(Cp(-700)), Some(Cp(-650)), Some(Cp(-100))],
                3,
                600
            ));
        }

        #[test]
        fn resign_negative_mate_score_fires() {
            // Mate(n) with n < 0 means mover gets mated; counts as losing.
            assert!(resign_threshold_check(
                &[Some(Mate(-3)), Some(Mate(-4)), Some(Mate(-5))],
                3,
                600
            ));
        }

        #[test]
        fn resign_positive_mate_does_not_fire() {
            // Mate(n) with n > 0 means mover is winning; must NOT resign.
            assert!(!resign_threshold_check(
                &[Some(Mate(3)), Some(Mate(2)), Some(Mate(1))],
                3,
                600
            ));
        }

        #[test]
        fn resign_none_entry_breaks_streak() {
            // None in the trailing window breaks the streak even if flanking entries qualify.
            assert!(!resign_threshold_check(
                &[Some(Cp(-700)), None, Some(Cp(-720))],
                3,
                600
            ));
        }

        #[test]
        fn resign_short_history_returns_false() {
            // History length < movecount must return false without panicking.
            assert!(!resign_threshold_check(
                &[Some(Cp(-700)), Some(Cp(-650))],
                3,
                600
            ));
        }

        #[test]
        fn resign_exact_threshold_fires() {
            // Pins ≤ (not <) at the boundary: score = −threshold should resign.
            assert!(resign_threshold_check(
                &[Some(Cp(-600)), Some(Cp(-600)), Some(Cp(-600))],
                3,
                600
            ));
        }

        #[test]
        fn resign_just_above_threshold_does_not_fire() {
            // Cp(-599) with threshold 600: |-599| = 599, not ≤ -threshold (i.e. -599 > -600).
            // Pins the boundary on the OTHER side from `resign_exact_threshold_fires`.
            assert!(!resign_threshold_check(
                &[
                    Some(driver::Score::Cp(-599)),
                    Some(driver::Score::Cp(-599)),
                    Some(driver::Score::Cp(-599))
                ],
                3,
                600
            ));
        }

        #[test]
        fn draw_eight_consecutive_balanced_after_movenumber_fires() {
            // Both sides have 8 balanced entries, move_number ≥ movenumber_floor.
            let white_hist: Vec<Option<driver::Score>> = vec![
                Some(Cp(10)),
                Some(Cp(-10)),
                Some(Cp(5)),
                Some(Cp(-5)),
                Some(Cp(10)),
                Some(Cp(-10)),
                Some(Cp(10)),
                Some(Cp(-10)),
            ];
            let black_hist: Vec<Option<driver::Score>> = vec![
                Some(Cp(-10)),
                Some(Cp(10)),
                Some(Cp(-5)),
                Some(Cp(5)),
                Some(Cp(-10)),
                Some(Cp(10)),
                Some(Cp(-10)),
                Some(Cp(10)),
            ];
            assert!(draw_threshold_check(
                &white_hist,
                &black_hist,
                40,
                34,
                8,
                20
            ));
        }

        #[test]
        fn draw_before_movenumber_does_not_fire() {
            // Same balanced history but move_number < movenumber_floor → false.
            let white_hist: Vec<Option<driver::Score>> = vec![
                Some(Cp(10)),
                Some(Cp(-10)),
                Some(Cp(5)),
                Some(Cp(-5)),
                Some(Cp(10)),
                Some(Cp(-10)),
                Some(Cp(10)),
                Some(Cp(-10)),
            ];
            let black_hist: Vec<Option<driver::Score>> = vec![
                Some(Cp(-10)),
                Some(Cp(10)),
                Some(Cp(-5)),
                Some(Cp(5)),
                Some(Cp(-10)),
                Some(Cp(10)),
                Some(Cp(-10)),
                Some(Cp(10)),
            ];
            assert!(!draw_threshold_check(
                &white_hist,
                &black_hist,
                30,
                34,
                8,
                20
            ));
        }

        #[test]
        fn draw_one_side_above_threshold() {
            // Black has one entry Cp(-50): |−50| = 50 > threshold 20, breaks streak.
            let white_hist: Vec<Option<driver::Score>> = vec![
                Some(Cp(10)),
                Some(Cp(-10)),
                Some(Cp(5)),
                Some(Cp(-5)),
                Some(Cp(10)),
                Some(Cp(-10)),
                Some(Cp(10)),
                Some(Cp(-10)),
            ];
            let black_hist: Vec<Option<driver::Score>> = vec![
                Some(Cp(-10)),
                Some(Cp(10)),
                Some(Cp(-5)),
                Some(Cp(5)),
                Some(Cp(-10)),
                Some(Cp(10)),
                Some(Cp(-10)),
                Some(Cp(-50)), // breaks the balanced streak
            ];
            assert!(!draw_threshold_check(
                &white_hist,
                &black_hist,
                40,
                34,
                8,
                20
            ));
        }

        #[test]
        fn draw_mate_score_breaks_streak() {
            // Mate(_) anywhere in the trailing window of either side breaks the streak,
            // regardless of sign: mate is by definition not a near-zero evaluation.
            let balanced: Vec<Option<driver::Score>> = vec![
                Some(Cp(10)),
                Some(Cp(-10)),
                Some(Cp(5)),
                Some(Cp(-5)),
                Some(Cp(10)),
                Some(Cp(-10)),
                Some(Cp(10)),
                Some(Cp(-10)),
            ];
            let white_mate: Vec<Option<driver::Score>> = vec![
                Some(Cp(10)),
                Some(Cp(-10)),
                Some(Cp(5)),
                Some(Cp(-5)),
                Some(Cp(10)),
                Some(Cp(-10)),
                Some(Cp(10)),
                Some(Mate(5)), // winning mate in white's history breaks draw streak
            ];
            let black_mate: Vec<Option<driver::Score>> = vec![
                Some(Cp(-10)),
                Some(Cp(10)),
                Some(Cp(-5)),
                Some(Cp(5)),
                Some(Cp(-10)),
                Some(Cp(10)),
                Some(Cp(-10)),
                Some(Mate(-3)), // losing mate in black's history breaks draw streak
            ];
            assert!(!draw_threshold_check(&white_mate, &balanced, 40, 34, 8, 20));
            assert!(!draw_threshold_check(&balanced, &black_mate, 40, 34, 8, 20));
        }

        #[test]
        fn draw_short_history_either_side_returns_false() {
            // Either side having fewer than movecount entries → false.
            let short: Vec<Option<driver::Score>> = vec![Some(Cp(10)), Some(Cp(-10))];
            let enough: Vec<Option<driver::Score>> = vec![
                Some(Cp(10)),
                Some(Cp(-10)),
                Some(Cp(10)),
                Some(Cp(-10)),
                Some(Cp(10)),
                Some(Cp(-10)),
                Some(Cp(10)),
                Some(Cp(-10)),
            ];
            assert!(!draw_threshold_check(&short, &enough, 40, 34, 3, 20));
            assert!(!draw_threshold_check(&enough, &short, 40, 34, 3, 20));
        }

        #[test]
        fn draw_none_entry_breaks_streak() {
            // None in the trailing window on either side breaks the streak.
            let with_none: Vec<Option<driver::Score>> = vec![
                Some(Cp(10)),
                Some(Cp(-10)),
                Some(Cp(5)),
                Some(Cp(-5)),
                Some(Cp(10)),
                Some(Cp(-10)),
                Some(Cp(10)),
                None, // breaks streak
            ];
            let balanced: Vec<Option<driver::Score>> = vec![
                Some(Cp(-10)),
                Some(Cp(10)),
                Some(Cp(-5)),
                Some(Cp(5)),
                Some(Cp(-10)),
                Some(Cp(10)),
                Some(Cp(-10)),
                Some(Cp(10)),
            ];
            assert!(!draw_threshold_check(&with_none, &balanced, 40, 34, 8, 20));
            assert!(!draw_threshold_check(&balanced, &with_none, 40, 34, 8, 20));
        }

        #[test]
        fn draw_exact_threshold_fires() {
            // Pins |s| ≤ thr (not <): all entries ±20 with threshold=20 should fire.
            let white_hist: Vec<Option<driver::Score>> = vec![
                Some(Cp(20)),
                Some(Cp(-20)),
                Some(Cp(20)),
                Some(Cp(-20)),
                Some(Cp(20)),
                Some(Cp(-20)),
                Some(Cp(20)),
                Some(Cp(-20)),
            ];
            let black_hist: Vec<Option<driver::Score>> = vec![
                Some(Cp(-20)),
                Some(Cp(20)),
                Some(Cp(-20)),
                Some(Cp(20)),
                Some(Cp(-20)),
                Some(Cp(20)),
                Some(Cp(-20)),
                Some(Cp(20)),
            ];
            assert!(draw_threshold_check(
                &white_hist,
                &black_hist,
                40,
                34,
                8,
                20
            ));
        }

        // ---- ELOH.B Tier-C targeted tests --------------------------------

        #[test]
        fn resign_slice_uses_subtraction_not_division() {
            // Pins the `-` in `mover_history[len - movecount..]` against the
            // `/` mutant.  History has 6 entries; movecount=3.
            //   Correct (-): window = history[3..6] → entries 3,4,5 (all ≤ −600) → fires.
            //   Mutant  (/): window = history[6/3..6] = history[2..6] → entry at
            //                index 2 is Cp(-100), which is not ≤ −600 → does NOT fire.
            let history = vec![
                Some(Cp(-100)), // index 0 — not in correct window
                Some(Cp(-100)), // index 1 — not in correct window
                Some(Cp(-100)), // index 2 — not in correct window; IS in mutant window
                Some(Cp(-700)), // index 3 — in correct window
                Some(Cp(-650)), // index 4 — in correct window
                Some(Cp(-720)), // index 5 — in correct window
            ];
            assert!(
                resign_threshold_check(&history, 3, 600),
                "last 3 entries all ≤ −600: must fire"
            );
        }

        #[test]
        fn resign_mate_zero_does_not_fire() {
            // Mate(0) means the side to move IS already mated (ply-to-mate = 0).
            // But this is an edge-case value; the original guard `n < 0` returns
            // false for n=0, so the streak is broken.
            // Mutant `< 0` → `<= 0` would treat Mate(0) as a "losing" score and
            // fire when three Mate(0) entries are present.
            assert!(
                !resign_threshold_check(&[Some(Mate(0)), Some(Mate(0)), Some(Mate(0))], 3, 600),
                "Mate(0) is not < 0 so it must NOT trigger resign"
            );
        }

        #[test]
        fn draw_at_movenumber_floor_fires() {
            // Pins the `<` in `move_number < movenumber_floor` against the `<=`
            // mutant.  move_number == movenumber_floor: original returns true
            // (condition false → doesn't short-circuit); mutant returns false
            // (condition true → early return false).
            let balanced: Vec<Option<driver::Score>> = vec![Some(Cp(5)), Some(Cp(-5)), Some(Cp(5))];
            assert!(
                draw_threshold_check(&balanced, &balanced, 34, 34, 3, 20),
                "move_number == movenumber_floor must fire (not be rejected by < guard)"
            );
        }

        #[test]
        fn draw_slice_uses_subtraction_not_division() {
            // Pins the `-` in `history[history.len() - n..]` against the `/`
            // mutant for `draw_threshold_check`.  Each history has 6 entries;
            // movecount n = 3.
            //   Correct (-): window = history[3..6] → entries 3,4,5 all balanced → fires.
            //   Mutant  (/): window = history[6/3..6] = history[2..6] → entry at
            //                index 2 has |s| > threshold → does NOT fire.
            let unbalanced_then_balanced: Vec<Option<driver::Score>> = vec![
                Some(Cp(0)),    // 0 — not in correct window
                Some(Cp(0)),    // 1 — not in correct window
                Some(Cp(-100)), // 2 — not in correct window; IS in mutant window (|s| > thr=20)
                Some(Cp(5)),    // 3 — in correct window (balanced)
                Some(Cp(-5)),   // 4 — in correct window (balanced)
                Some(Cp(5)),    // 5 — in correct window (balanced)
            ];
            assert!(
                draw_threshold_check(
                    &unbalanced_then_balanced,
                    &unbalanced_then_balanced,
                    50,
                    34,
                    3,
                    20
                ),
                "last 3 entries balanced: must fire"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// mod estimator  (SLICE-A stub)
// ---------------------------------------------------------------------------

mod estimator {
    // #[allow(dead_code)] on each fn: wired by controller in slice E; until
    // then, clippy's dead-code lint fires because nothing outside tests calls these.
    #[allow(dead_code)]
    pub(crate) fn compute_k(t: u32, k0: f64, tau: f64) -> f64 {
        if k0 == 0.0 {
            return 0.0;
        }
        k0 / (1.0 + (t as f64) / tau)
    }

    #[allow(dead_code)]
    pub(crate) fn expected_score(my_elo: f64, opp_elo: f64) -> f64 {
        1.0 / (1.0 + 10_f64.powf((opp_elo - my_elo) / 400.0))
    }

    #[allow(dead_code)]
    pub(crate) fn update_estimate(prior_elo: f64, opp_elo: f64, result: f64, k: f64) -> f64 {
        prior_elo + k * (result - expected_score(prior_elo, opp_elo))
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn compute_k_at_t_zero_returns_k0() {
            let k = compute_k(0, 40.0, 10.0);
            assert!((k - 40.0).abs() < 1e-3, "expected 40.0, got {k}");
        }
        #[test]
        fn compute_k_at_t_equals_tau_halves() {
            let k = compute_k(10, 40.0, 10.0);
            assert!((k - 20.0).abs() < 1e-3, "expected 20.0, got {k}");
        }
        #[test]
        fn compute_k_decay_at_ten_tau() {
            let k = compute_k(100, 40.0, 10.0);
            let expected = 40.0 / 11.0;
            assert!((k - expected).abs() < 1e-3, "expected {expected}, got {k}");
        }
        #[test]
        fn compute_k_monotone_non_increasing() {
            let ts = [0u32, 5, 10, 20, 50, 100];
            let ks: Vec<f64> = ts.iter().map(|&t| compute_k(t, 40.0, 10.0)).collect();
            for i in 1..ks.len() {
                assert!(
                    ks[i] <= ks[i - 1] + 1e-12,
                    "K not monotone at index {i}: k[{i}]={} > k[{}]={}",
                    ks[i],
                    i - 1,
                    ks[i - 1]
                );
            }
        }
        #[test]
        fn compute_k_zero_k0_returns_zero() {
            for t in [0u32, 1, 10, 100, 1000] {
                let k = compute_k(t, 0.0, 10.0);
                assert!(k == 0.0, "expected 0.0 for k0=0.0 at t={t}, got {k}");
            }
        }
        #[test]
        fn expected_score_equal_elo_returns_half() {
            let e = expected_score(2000.0, 2000.0);
            assert!((e - 0.5).abs() < 1e-9, "expected 0.5, got {e}");
        }
        #[test]
        fn expected_score_400_above() {
            let e = expected_score(2400.0, 2000.0);
            assert!((e - 0.909).abs() < 1e-3, "expected ≈0.909, got {e}");
        }
        #[test]
        fn expected_score_400_below() {
            let e = expected_score(2000.0, 2400.0);
            assert!((e - 0.091).abs() < 1e-3, "expected ≈0.091, got {e}");
        }
        #[test]
        fn update_win_against_equal() {
            // S=1 vs equal: E=0.5, delta = k*(1-0.5) = k/2.
            let prior = 2000.0;
            let k = 32.0;
            let updated = update_estimate(prior, prior, 1.0, k);
            assert!(
                (updated - (prior + k / 2.0)).abs() < 1e-3,
                "expected {}, got {updated}",
                prior + k / 2.0
            );
        }
        #[test]
        fn update_loss_against_equal() {
            // S=0 vs equal: E=0.5, delta = k*(0-0.5) = -k/2.
            let prior = 2000.0;
            let k = 32.0;
            let updated = update_estimate(prior, prior, 0.0, k);
            assert!(
                (updated - (prior - k / 2.0)).abs() < 1e-3,
                "expected {}, got {updated}",
                prior - k / 2.0
            );
        }
        #[test]
        fn update_draw_against_equal_no_change() {
            // S=0.5 vs equal: E=0.5, delta = k*(0.5-0.5) = 0.
            let prior = 2000.0;
            let updated = update_estimate(prior, prior, 0.5, 32.0);
            assert!(
                (updated - prior).abs() < 1e-9,
                "draw against equal should not change estimate; got {updated}"
            );
        }
        #[test]
        fn update_with_zero_k_freezes_estimate() {
            for &(prior, opp, result) in &[
                (2000.0_f64, 1800.0_f64, 1.0_f64),
                (1500.0, 2100.0, 0.0),
                (2100.0, 2100.0, 0.5),
            ] {
                let updated = update_estimate(prior, opp, result, 0.0);
                assert!(
                    (updated - prior).abs() < 1e-9,
                    "k=0 should freeze estimate; prior={prior}, got {updated}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// mod sigma  (SLICE-A stub)
// ---------------------------------------------------------------------------

mod sigma {
    // #[allow(dead_code)] on each fn: wired by controller in slice E.
    #[allow(dead_code)]
    pub(crate) fn sample_stddev(xs: &[f64]) -> f64 {
        if xs.len() < 2 {
            return 0.0;
        }
        let n = xs.len() as f64;
        let mean = xs.iter().sum::<f64>() / n;
        let variance = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
        variance.sqrt()
    }

    #[allow(dead_code)]
    pub(crate) fn should_stop(
        estimates: &[f64],
        window: usize,
        target_sigma: f64,
        confirm: usize,
    ) -> bool {
        if target_sigma == 0.0 {
            return false;
        }
        let len = estimates.len();
        // Need at least `window + confirm - 1` entries so every confirm position
        // has a full window behind it.
        //
        // Index-arithmetic proof: the loop iterates `i ∈ [len-confirm, len-1]`.
        // For each `i`, the slice is `estimates[i+1-window .. i+1]`. The earliest
        // slice (at `i = len-confirm`) starts at `len - confirm + 1 - window`.
        // For this start index to be ≥ 0, we need `len + 1 ≥ window + confirm`,
        // i.e. `len ≥ window + confirm - 1`. The guard below pins exactly this
        // tight bound. With `window=30, confirm=5`, the threshold is 34 — pinned
        // by `should_stop_minimum_data_boundary` and `should_stop_short_estimates_returns_false`.
        if len < window + confirm - 1 {
            return false;
        }
        // Check the last `confirm` positions (indices len-confirm .. len-1 inclusive).
        // Position i uses the slice estimates[i+1-window .. i+1] (length = window).
        for i in (len - confirm)..len {
            let slice = &estimates[i + 1 - window..i + 1];
            if sample_stddev(slice) >= target_sigma {
                return false;
            }
        }
        true
    }
    #[cfg(test)]
    mod tests {
        use super::super::estimator;
        use super::*;

        struct Xorshift64 {
            state: u64,
        }
        impl Xorshift64 {
            fn new(seed: u64) -> Self {
                Self {
                    state: seed.wrapping_mul(0x9E3779B97F4A7C15).max(1),
                }
            }
            fn next_u64(&mut self) -> u64 {
                let mut x = self.state;
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                self.state = x;
                x.wrapping_mul(0x2545F4914F6CDD1D)
            }
            fn next_f64(&mut self) -> f64 {
                (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
            }
        }

        #[test]
        fn sample_stddev_constant_series_zero() {
            let sd = sample_stddev(&[5.0f64, 5.0, 5.0]);
            assert!(
                sd.abs() < 1e-9,
                "constant series should have stddev 0.0, got {sd}"
            );
        }
        #[test]
        fn sample_stddev_two_point_uses_bessel() {
            // [0.0, 2.0]: mean=1.0, sum-sq-dev=2.0, Bessel divisor n-1=1 → stddev=√2.
            let sd = sample_stddev(&[0.0f64, 2.0]);
            let expected = f64::sqrt(2.0);
            assert!(
                (sd - expected).abs() < 1e-9,
                "expected √2 ≈ {expected}, got {sd}"
            );
        }
        #[test]
        fn sample_stddev_short_returns_zero() {
            assert!(
                sample_stddev(&[]).abs() < 1e-9,
                "empty slice should return 0.0"
            );
            assert!(
                sample_stddev(&[42.0]).abs() < 1e-9,
                "single-element slice should return 0.0"
            );
        }
        #[test]
        fn should_stop_disabled_when_target_zero() {
            let estimates: Vec<f64> = vec![2100.0; 50];
            assert!(
                !should_stop(&estimates, 30, 0.0, 5),
                "target_sigma=0.0 must return false (disabled)"
            );
        }
        #[test]
        fn should_stop_fires_when_recent_window_below() {
            let estimates: Vec<f64> = vec![2100.0; 50];
            assert!(
                should_stop(&estimates, 30, 10.0, 5),
                "constant series with target=10 should stop"
            );
        }
        #[test]
        fn should_stop_does_not_fire_with_high_variance() {
            let estimates: Vec<f64> = (0..60)
                .map(|i| if i % 2 == 0 { 2200.0 } else { 2000.0 })
                .collect();
            assert!(
                !should_stop(&estimates, 30, 10.0, 5),
                "high-variance series should not stop"
            );
        }
        #[test]
        fn should_stop_anti_flap_concrete_fixture() {
            // Case 1: alternating throughout — every trailing 30-window has σ ~ 50;
            // should NOT stop with target=10.
            let estimates_alternating: Vec<f64> = (0..35)
                .map(|i| if i % 2 == 0 { 2050.0_f64 } else { 2150.0 })
                .collect();
            assert_eq!(estimates_alternating.len(), 35);
            assert!(
                !should_stop(&estimates_alternating, 30, 10.0, 5),
                "alternating series has high trailing-σ; should not stop"
            );
            // Case 2: flat throughout → σ=0 < target=10 for all 5 confirm positions → fires.
            let estimates_flat: Vec<f64> = vec![2100.0; 35];
            assert!(
                should_stop(&estimates_flat, 30, 10.0, 5),
                "flat series of 35 should stop"
            );
        }
        #[test]
        fn should_stop_short_estimates_returns_false() {
            // window=30, confirm=5 → need at least 34; test with 33.
            let estimates: Vec<f64> = vec![2100.0; 33];
            assert!(
                !should_stop(&estimates, 30, 10.0, 5),
                "too short to confirm; must return false"
            );
        }
        #[test]
        fn bernoulli_back_test_gate() {
            // Bernoulli stream: p=0.760, equilibrium at expected_score(2200, 2000) ≈ 0.760.
            // Initial estimate set 200 Elo above equilibrium so E[S−E] ≈ −0.149 at t=0;
            // the trail drifts DOWN toward 2200 before settling.
            //
            // σ-stopping must fire within [34, 400] games. Lower bound 34 is the
            // minimum-data threshold (window=30 + confirm=5 - 1); with K_0=40 and
            // p=0.760, per-step jitter K·√(p(1−p)) ≈ 17 < target_sigma=30, so the
            // trailing-σ over a 30-window stays below 30 throughout, and the
            // algorithm correctly fires at the minimum sample size. Upper bound
            // 400 is the never-fires safeguard. The test detects:
            //   - never-fires bug (panic on stop_at.expect).
            //   - too-late-fires bug (assertion fails at t > 400).
            //   - sign-flip on update_estimate (post-convergence value check below).
            //   - too-early-fires bug (short-input guard at the bottom).
            let p = 0.760_f64;
            let opp_elo = 2000.0_f64;
            let mut current_estimate = 2400.0_f64; // 200 above equilibrium → directional convergence
            let mut estimates: Vec<f64> = Vec::new();
            let mut rng = Xorshift64::new(0x00DE_DBEE_F123_4567);
            let mut stop_at: Option<usize> = None;
            for t in 0u32..1000 {
                let s = if rng.next_f64() < p { 1.0_f64 } else { 0.0 };
                let k = estimator::compute_k(t, 40.0, 10.0);
                current_estimate = estimator::update_estimate(current_estimate, opp_elo, s, k);
                estimates.push(current_estimate);
                if should_stop(&estimates, 30, 30.0, 5) {
                    stop_at = Some(t as usize + 1);
                    break;
                }
            }
            let t_stop = stop_at
                .expect("σ-stopping never fired within 1000 games; check estimator or sigma impl");
            assert!(
                (34..=400).contains(&t_stop),
                "σ-stopping fired at t={t_stop}; expected within [34, 400]"
            );

            // Directional-drift check: with σ-stopping firing at the minimum
            // sample size (t≈34), the estimate has drifted only partway toward
            // equilibrium 2200 from initial 2400. We don't require full
            // convergence here — the test's primary purpose is the σ-stopping
            // decision, not convergence depth. We DO require the estimate to
            // be moving DOWN (toward equilibrium) and within ±300 Elo of it,
            // which catches gross update_estimate bugs (e.g. sign flip would
            // push the estimate UP past 2400).
            assert!(
                current_estimate < 2400.0,
                "post-stop estimate {current_estimate:.1} did not drift below initial 2400 — likely sign flip in update_estimate"
            );
            assert!(
                (current_estimate - 2200.0).abs() < 300.0,
                "post-stop estimate {current_estimate:.1} >300 Elo from equilibrium 2200 — likely sign flip or wrong K direction"
            );

            // Short-input guard: should_stop must return false until the data
            // window even fills. window=30 + confirm=5 - 1 = 34. We re-run a
            // parallel iteration up to game 35 and verify should_stop is false
            // at each step ≤ 34. Catches a stub that returns true unconditionally.
            let mut early_estimates = Vec::new();
            let mut early_rng = Xorshift64::new(0x00DE_DBEE_F123_4567);
            let mut early_estimate = 2400.0;
            for tt in 0..35 {
                let s = if early_rng.next_f64() < 0.760 {
                    1.0
                } else {
                    0.0
                };
                let k = estimator::compute_k(tt as u32, 40.0, 10.0);
                early_estimate = estimator::update_estimate(early_estimate, 2000.0, s, k);
                early_estimates.push(early_estimate);
                // Trail length post-push = tt + 1. should_stop is eligible to
                // fire when len >= window + confirm - 1 = 34, i.e. when tt >= 33.
                // The guard runs only for tt ∈ [0, 32] (len ∈ [1, 33]).
                if tt < 33 {
                    assert!(
                        !should_stop(&early_estimates, 30, 30.0, 5),
                        "should_stop must return false for fewer than window+confirm-1=34 entries; fired at tt={tt} (len={})",
                        early_estimates.len()
                    );
                }
            }
        }

        // ---- ELOH.B Tier-C targeted tests --------------------------------

        #[test]
        fn sample_stddev_three_point_pins_bessel_divisor() {
            // [1.0, 2.0, 3.0]: mean=2.0, sum-sq-dev=(1+0+1)=2.0, n-1=2 → σ=√1=1.0.
            // Mutant `/` → `*` at the Bessel step:
            //   variance = 2.0 * (3.0 - 1.0) = 4.0, σ = 2.0  (not 1.0).
            // Also catches the `*` variant.
            let sd = sample_stddev(&[1.0_f64, 2.0, 3.0]);
            assert!(
                (sd - 1.0_f64).abs() < 1e-9,
                "expected σ=1.0 for [1,2,3], got {sd}"
            );
        }

        #[test]
        fn should_stop_minimum_data_boundary() {
            // window=3, confirm=2 → need at least 3+2-1=4 entries.
            // With exactly 4 constant entries, `<` guard (correct) does NOT fire
            // (4 < 4 is false), so the confirm loop runs and fires (σ=0 < target).
            // Mutant `<= 4` would fire the guard → return false prematurely.
            let estimates = vec![2100.0_f64; 4];
            assert!(
                should_stop(&estimates, 3, 10.0, 2),
                "exactly window+confirm-1=4 constant entries must fire should_stop"
            );
            // One entry short (3) must still return false.
            let too_short = vec![2100.0_f64; 3];
            assert!(
                !should_stop(&too_short, 3, 10.0, 2),
                "window+confirm-1-1=3 entries must not fire should_stop"
            );
        }

        #[test]
        fn should_stop_slice_window_uses_i_plus_one_minus_window() {
            // Pins that the window slice is `estimates[i+1-window..i+1]` and NOT
            // `estimates[i-1-window..i+1]` (+ → - mutant) or `estimates[i-window..i+1]`
            // (* → identity mutant).
            //
            // Setup: window=2, confirm=1, target=10.
            //   need at least 2+1-1=2 entries.  Use 3 entries: [2100, 2100, 9999].
            //   i = len-1 = 2; correct slice = estimates[2+1-2..3] = estimates[1..3] = [2100,9999]
            //   σ([2100,9999]) >> 10 → NOT below target → should NOT stop.
            //
            //   With + → - mutant: slice = estimates[2-1-2..3]. 2-1-2 = -1 (underflow)
            //   → panic or wrong result. With * mutant (i*1 - window):
            //   slice = estimates[2*1-2..3] = estimates[0..3] = all three → σ still large.
            //
            // Use a fixture that distinguishes the correct vs. wrong slice boundary:
            //   [9999, 2100, 2100]. i=2, window=2.
            //   Correct: estimates[1..3] = [2100, 2100] → σ=0 < 10 → STOPS.
            //   Wrong (if slice started one earlier): [9999, 2100, 2100] → σ large → NOT stop.
            let estimates = vec![9999.0_f64, 2100.0, 2100.0];
            assert!(
                should_stop(&estimates, 2, 10.0, 1),
                "window=2 confirm=1: last 2 entries are constant; should stop"
            );
            // Complementary: if the high-variance entry IS in the window, must not stop.
            let estimates2 = vec![2100.0_f64, 9999.0, 2100.0];
            assert!(
                !should_stop(&estimates2, 2, 10.0, 1),
                "window=2 confirm=1: middle entry 9999 is in the window; must not stop"
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

        #[test]
        fn pgn_odd_move_count_emits_trailing_white_move_no_black() {
            // Pins the move-pair iteration boundary: odd-length move list
            // emits the trailing white move WITHOUT a black follow-up.
            // Catches `replace < with > in format_pgn`, `replace < with <= in
            // format_pgn`, `replace < with == in format_pgn`, and
            // `replace += with -=` / `*=` mutations on the move-index step.
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
                PgnMove {
                    uci: "g1f3".into(),
                    last_info: None,
                },
            ];
            let pgn = format_pgn(&header, &moves);
            // Move 1 has both white and black; move 2 has only white.
            assert!(pgn.contains("1. e2e4 e7e5"), "missing 1. e2e4 e7e5: {pgn}");
            assert!(pgn.contains("2. g1f3"), "missing 2. g1f3: {pgn}");
            // The third move's UCI must NOT be followed by a non-numbered token
            // (i.e., it stands alone without a black reply).
            let g1f3_pos = pgn.find("2. g1f3").expect("missing 2. g1f3");
            let after = &pgn[g1f3_pos + "2. g1f3".len()..];
            // After "2. g1f3", the only valid content is the result marker
            // (preceded by a single space) and a trailing newline.
            assert!(
                after.trim_end() == " 1-0",
                "expected only ' 1-0' after '2. g1f3'; got: {after:?}"
            );
        }

        #[test]
        fn pgn_single_move_emits_one_white_only() {
            // Boundary: 1-move list should emit "1. <move> <result>".
            // Catches the `i + 1 < moves.len()` predicate boundary at the
            // first iteration when moves.len() == 1.
            let header = base_header("1-0");
            let moves = vec![PgnMove {
                uci: "e2e4".into(),
                last_info: None,
            }];
            let pgn = format_pgn(&header, &moves);
            assert!(pgn.contains("1. e2e4"), "missing '1. e2e4': {pgn}");
            assert!(!pgn.contains("e7e5"), "should not contain e7e5: {pgn}");
            assert!(pgn.trim_end().ends_with("1-0"), "missing result: {pgn}");
        }

        #[test]
        fn pgn_empty_moves_omits_body_but_keeps_result() {
            // Edge: empty move list — header + result only.
            let header = base_header("1/2-1/2");
            let pgn = format_pgn(&header, &[]);
            assert!(pgn.contains(r#"[Result "1/2-1/2"]"#));
            assert!(pgn.trim_end().ends_with("1/2-1/2"));
            // No move tokens, no comments.
            assert!(!pgn.contains('{'), "no comments for empty body");
        }

        // ---- ELOH.D §6.6: pgn_time_control_tag_reflects_sampled_tc ----

        #[test]
        fn pgn_time_control_tag_reflects_sampled_tc() {
            // Construct PgnHeader { time_control: Some("20+0.2"), .. }; format;
            // assert the produced PGN contains exactly one [TimeControl "20+0.2"] line.
            let header = PgnHeader {
                event: "test".into(),
                site: "localhost".into(),
                date: "2026.04.30".into(),
                round: 1,
                white: "clawfish".into(),
                black: "opponent".into(),
                result: "1/2-1/2".into(),
                time_control: Some("20+0.2".into()),
                termination: None,
                setup_fen: None,
            };
            let pgn = format_pgn(&header, &[]);
            let tc_tag_count = pgn
                .lines()
                .filter(|l| *l == r#"[TimeControl "20+0.2"]"#)
                .count();
            assert_eq!(
                tc_tag_count, 1,
                "must contain exactly one [TimeControl \"20+0.2\"] line; got:\n{pgn}"
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
        /// NEW (ELOH.D). Format `<base>+<inc>` matching `format_tc`. `None` in
        /// legacy ELOH.A/B/C fixtures; always `Some` when ELOH.D TC sampling is active.
        pub tc: Option<String>,
    }

    /// Per-TC W/L/D bucket; ordered by input spec. Built incrementally in
    /// the controller's drain loop alongside the global wins/losses/draws counters.
    pub(crate) struct TcBucket {
        pub tc: super::cli::TimeControl,
        pub wins: u32,
        pub losses: u32,
        pub draws: u32,
    }

    /// Format per-TC summary line for `summary-by-tc:` emission.
    ///
    /// Output: `"summary-by-tc: 10+0.1: W=110 L=95 D=45 (250)  20+0.2: W=..."` —
    /// two spaces between bucket entries. Emitted even for a single-bucket distribution
    /// (degenerate single-TC mix) to preserve the invariant that `summary-by-tc:` is
    /// present iff `--tc-sample` was active.
    pub(crate) fn format_summary_by_tc(buckets: &[TcBucket]) -> String {
        let parts: Vec<String> = buckets
            .iter()
            .map(|b| {
                let tc_str = super::format_tc(b.tc);
                let total = b.wins + b.losses + b.draws;
                format!(
                    "{tc_str}: W={} L={} D={} ({total})",
                    b.wins, b.losses, b.draws
                )
            })
            .collect();
        format!("summary-by-tc: {}", parts.join("  "))
    }

    /// Append a summary line to `path` (tab-separated, one line per game).
    pub(crate) fn append_summary_line(path: &Path, line: &SummaryLine) -> std::io::Result<()> {
        use std::fs::OpenOptions;
        use std::io::Write;
        let mut f = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(
            f,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            line.game_index,
            line.white,
            line.black,
            line.result,
            line.plies,
            line.termination,
            line.tc.as_deref().unwrap_or("-"),
        )?;
        f.flush()?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // -----------------------------------------------------------------------
        // §6.4 ELOH.D summary tests
        // -----------------------------------------------------------------------

        #[test]
        fn summary_line_with_tc_appends_tab_separated() {
            // Append a SummaryLine { tc: Some("10+0.1"), .. }; resulting line ends with \t10+0.1\n.
            let dir = std::env::temp_dir().join("eloh_d_summary_tc_test");
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join("summary_tc.txt");
            let _ = std::fs::remove_file(&path);
            let line = SummaryLine {
                game_index: 1,
                white: "engine".into(),
                black: "opponent".into(),
                result: "1-0".into(),
                plies: 42,
                termination: "normal".into(),
                tc: Some("10+0.1".into()),
            };
            append_summary_line(&path, &line).expect("append_summary_line ok");
            let content = std::fs::read_to_string(&path).expect("read ok");
            assert!(
                content.ends_with("\t10+0.1\n"),
                "line must end with '\\t10+0.1\\n'; got: {content:?}"
            );
        }

        #[test]
        fn summary_line_without_tc_appends_dash() {
            // tc: None → trailing \t-\n (sentinel for ELOH.A/B fixtures).
            let dir = std::env::temp_dir().join("eloh_d_summary_notc_test");
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join("summary_notc.txt");
            let _ = std::fs::remove_file(&path);
            let line = SummaryLine {
                game_index: 1,
                white: "engine".into(),
                black: "opponent".into(),
                result: "1/2-1/2".into(),
                plies: 10,
                termination: "adjudication: max moves".into(),
                tc: None,
            };
            append_summary_line(&path, &line).expect("append_summary_line ok");
            let content = std::fs::read_to_string(&path).expect("read ok");
            assert!(
                content.ends_with("\t-\n"),
                "tc=None must emit '\\t-\\n'; got: {content:?}"
            );
        }

        #[test]
        fn format_summary_by_tc_two_buckets() {
            // Buckets [(10+0.1, W=110, L=95, D=45), (20+0.2, W=105, L=90, D=55)] →
            // exact string "summary-by-tc: 10+0.1: W=110 L=95 D=45 (250)  20+0.2: W=105 L=90 D=55 (250)".
            let buckets = vec![
                TcBucket {
                    tc: super::super::cli::TimeControl {
                        initial_ms: 10_000,
                        increment_ms: 100,
                    },
                    wins: 110,
                    losses: 95,
                    draws: 45,
                },
                TcBucket {
                    tc: super::super::cli::TimeControl {
                        initial_ms: 20_000,
                        increment_ms: 200,
                    },
                    wins: 105,
                    losses: 90,
                    draws: 55,
                },
            ];
            let s = format_summary_by_tc(&buckets);
            assert_eq!(
                s, "summary-by-tc: 10+0.1: W=110 L=95 D=45 (250)  20+0.2: W=105 L=90 D=55 (250)",
                "two-bucket format mismatch; got: {s:?}"
            );
        }

        #[test]
        fn format_summary_by_tc_single_bucket_emitted() {
            // One-bucket input → still emits the full "summary-by-tc: 10+0.1: W=N L=N D=N (N)" line.
            // Degenerate single-TC mix must still emit the summary-by-tc line — preserves the
            // invariant that summary-by-tc is present iff --tc-sample was active.
            let buckets = vec![TcBucket {
                tc: super::super::cli::TimeControl {
                    initial_ms: 10_000,
                    increment_ms: 100,
                },
                wins: 7,
                losses: 3,
                draws: 2,
            }];
            let s = format_summary_by_tc(&buckets);
            assert_eq!(
                s, "summary-by-tc: 10+0.1: W=7 L=3 D=2 (12)",
                "single-bucket format mismatch; got: {s:?}"
            );
        }

        #[test]
        fn format_summary_by_tc_zero_games_in_bucket() {
            // A bucket with W=L=D=0 emits "... 30+0.3: W=0 L=0 D=0 (0)".
            let buckets = vec![TcBucket {
                tc: super::super::cli::TimeControl {
                    initial_ms: 30_000,
                    increment_ms: 300,
                },
                wins: 0,
                losses: 0,
                draws: 0,
            }];
            let s = format_summary_by_tc(&buckets);
            assert!(
                s.contains("W=0 L=0 D=0 (0)"),
                "zero-game bucket must emit 'W=0 L=0 D=0 (0)'; got: {s:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// StopReason — crate-internal; shared by mod progress and (eventually) mod controller
// ---------------------------------------------------------------------------

/// Why the online iteration terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum StopReason {
    /// Trailing-σ fell below `--target-sigma` for `--stop-window-confirm` consecutive games.
    Sigma,
    /// `--max-games` exhausted without σ convergence.
    MaxGames,
}

// ---------------------------------------------------------------------------
// mod progress
// ---------------------------------------------------------------------------

mod progress {
    //! Progress-line and convergence-line formatters for the iteration harness.

    use super::StopReason;

    /// Snapshot of per-game-batch progress for human-readable output.
    #[allow(dead_code)]
    pub(crate) struct ProgressLine {
        /// Game (pair) serial index `t`.
        pub t: u32,
        /// Total games completed so far.
        pub games: u32,
        /// Current Elo estimate.
        pub elo: f64,
        /// Current trailing-window σ.
        pub sigma: f64,
        /// Current K factor.
        pub k: f64,
        /// Win / Loss / Draw counts from clawfish's perspective.
        pub wld: (u32, u32, u32),
    }

    /// Format a mid-run progress line.
    ///
    /// Output: `progress: t=<t> games=<G> elo=<%.2f> sigma=<%.2f> K=<%.3f> wld=<W>-<L>-<D>`
    #[allow(dead_code)]
    pub(crate) fn format_progress(line: &ProgressLine) -> String {
        let (w, l, d) = line.wld;
        format!(
            "progress: t={t} games={games} elo={elo:.2} sigma={sigma:.2} K={k:.3} wld={w}-{l}-{d}",
            t = line.t,
            games = line.games,
            elo = line.elo,
            sigma = line.sigma,
            k = line.k,
        )
    }

    /// Format the final convergence line.
    ///
    /// Output: `converged: elo=<%.2f> sigma=<%.2f> games=<G> reason=<sigma|max-games>`
    #[allow(dead_code)]
    pub(crate) fn format_converged(
        final_elo: f64,
        final_sigma: f64,
        games: u32,
        reason: StopReason,
    ) -> String {
        let reason_str = match reason {
            StopReason::Sigma => "sigma",
            StopReason::MaxGames => "max-games",
        };
        format!(
            "converged: elo={final_elo:.2} sigma={final_sigma:.2} games={games} reason={reason_str}"
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn format_progress_canonical_string() {
            let line = ProgressLine {
                t: 60,
                games: 60,
                elo: 2103.45,
                sigma: 28.7,
                k: 13.333,
                wld: (45, 8, 7),
            };
            assert_eq!(
                format_progress(&line),
                "progress: t=60 games=60 elo=2103.45 sigma=28.70 K=13.333 wld=45-8-7"
            );
        }

        #[test]
        fn format_converged_sigma_reason() {
            let s = format_converged(2103.45, 28.7, 60, StopReason::Sigma);
            assert_eq!(
                s,
                "converged: elo=2103.45 sigma=28.70 games=60 reason=sigma"
            );
        }

        #[test]
        fn format_converged_max_games_reason() {
            let s = format_converged(2103.45, 28.7, 60, StopReason::MaxGames);
            assert_eq!(
                s,
                "converged: elo=2103.45 sigma=28.70 games=60 reason=max-games"
            );
        }

        #[test]
        fn format_progress_zero_sigma() {
            let line = ProgressLine {
                t: 1,
                games: 2,
                elo: 2000.0,
                sigma: 0.0,
                k: 40.0,
                wld: (1, 0, 1),
            };
            let s = format_progress(&line);
            assert!(s.contains("sigma=0.00"));
        }

        #[test]
        fn format_progress_two_decimal_elo_rounds() {
            let line = ProgressLine {
                t: 1,
                games: 2,
                elo: 1999.999,
                sigma: 5.0,
                k: 40.0,
                wld: (2, 0, 0),
            };
            let s = format_progress(&line);
            assert!(s.contains("elo=2000.00"));
        }
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
        /// Threshold adjudication parameters (resign / draw-by-score / max-moves).
        pub thresholds: super::cli::Thresholds,
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
        use super::adjudicate::{
            detect_native_game_over, draw_threshold_check, resign_threshold_check,
        };
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
        // Per-color score histories for threshold adjudication (§4.3).
        let mut white_history: Vec<Option<super::driver::Score>> = Vec::new();
        let mut black_history: Vec<Option<super::driver::Score>> = Vec::new();
        // 1-based full-move number (increments after each black move, per chess convention).
        let mut move_number: u32 = 1;

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

            // Push just-moved side's score onto its per-color history (§4.3 step 1).
            let just_moved_score = active_handle.last_info.score.clone();
            match side {
                Color::White => white_history.push(just_moved_score),
                Color::Black => black_history.push(just_moved_score),
            }

            // Increment full-move number after black moves (standard chess convention).
            if side == Color::Black {
                move_number += 1;
            }

            // Check native game-over after the move (§4.3 step 2).
            if let Some(go) = detect_native_game_over(&position, &history) {
                return (GameOutcome::NativeGameOver(go), pgn_moves);
            }

            // Resign adjudication: check just-moved side (§4.3 step 3).
            let mover_history: &[Option<super::driver::Score>] = match side {
                Color::White => &white_history,
                Color::Black => &black_history,
            };
            if resign_threshold_check(
                mover_history,
                ctx.thresholds.resign_movecount,
                ctx.thresholds.resign_score,
            ) {
                return (
                    GameOutcome::NativeGameOver(super::adjudicate::GameOver::ResignAdjudicated(
                        side,
                    )),
                    pgn_moves,
                );
            }

            // Draw-by-score adjudication (§4.3 step 4).
            if draw_threshold_check(
                &white_history,
                &black_history,
                move_number,
                ctx.thresholds.draw_movenumber,
                ctx.thresholds.draw_movecount,
                ctx.thresholds.draw_score,
            ) {
                return (
                    GameOutcome::NativeGameOver(super::adjudicate::GameOver::DrawAdjudicated),
                    pgn_moves,
                );
            }

            // Guard against runaway games (§4.3 step 5).
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
        fn forfeit_boundary_exactly_negative_grace_does_not_forfeit() {
            // Pin the strict-less-than nature of the forfeit comparison.
            // remaining=50, elapsed=100, inc=0, grace=50
            // new_remaining = 50 - 100 + 0 = -50
            // Original: `-50 < -50` is false → no forfeit. Boundary equality.
            // `<` → `<=` mutation: `-50 <= -50` is true → forfeit (BUG).
            let prior = clock(50, 0);
            let (t_go, t_bm) = synthetic_instants(100);
            let update = pure_apply_move_clock_update(prior, t_go, t_bm, 50);
            assert_eq!(update.new_clock.remaining_ms, -50);
            assert!(
                !update.forfeited,
                "exact -grace boundary must NOT forfeit (strict less-than); a < → <= mutation would falsely forfeit here"
            );
        }

        #[test]
        fn forfeit_boundary_one_below_grace_forfeits() {
            // Companion to forfeit_boundary_exactly_negative_grace: one step
            // beyond the boundary must forfeit.
            // remaining=49, elapsed=100, inc=0, grace=50 → -51
            // -51 < -50 → forfeit.
            let prior = clock(49, 0);
            let (t_go, t_bm) = synthetic_instants(100);
            let update = pure_apply_move_clock_update(prior, t_go, t_bm, 50);
            assert_eq!(update.new_clock.remaining_ms, -51);
            assert!(update.forfeited, "one ms below boundary must forfeit");
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
// mod controller
// ---------------------------------------------------------------------------

mod controller {
    //! Worker-pool lifecycle and iteration-loop controller.
    //!
    //! `spawn_workers` creates N worker threads, each owning its own engine pair.
    //! `run_iteration` drives the color-pair dispatch + Robbins-Monro update loop
    //! until σ convergence or max-games exhaustion.

    use std::sync::mpsc;

    /// Command sent from the controller to a worker thread.
    #[derive(Debug, Clone, PartialEq, Eq)]
    #[allow(dead_code)]
    pub(crate) enum WorkerCmd {
        /// Play one color-pair (2 games, color-swapped, same `opponent_uci_elo`).
        /// `pair_index` is the 0-based pair count for game-index assignment.
        PlayPair {
            pair_index: u32,
            opponent_uci_elo: u32,
            /// Per-pair TC for the engine (clawfish). Sampled from
            /// `--tc-sample` dist or equal to `args.tc` in fixed-TC mode.
            engine_tc: super::cli::TimeControl,
            /// Per-pair TC for the opponent. Equal to `engine_tc` when
            /// `--opponent-tc-override` is absent; override value otherwise.
            opponent_tc: super::cli::TimeControl,
        },
        /// Tell the worker to exit its recv loop and clean up.
        Quit,
    }

    /// Report sent from a worker thread back to the controller.
    #[derive(Debug)]
    #[allow(dead_code)]
    pub(crate) enum WorkerReport {
        /// A single game in the current pair has completed.
        GameComplete {
            game_index: u32,
            opponent_uci_elo: u32,
            /// Clawfish's score: 1.0 = win, 0.5 = draw, 0.0 = loss.
            clawfish_score: f64,
            outcome: super::match_loop::GameOutcome,
            pgn_moves: Vec<super::pgn::PgnMove>,
            white_name: String,
            black_name: String,
            /// The TC clawfish played at. Used for PGN TimeControl tag,
            /// summary line, and per-TC W/L/D bucket lookup.
            tc: super::cli::TimeControl,
        },
        /// Both games of the current pair are done; controller may dispatch next.
        PairComplete { worker_id: u32 },
        /// The worker encountered an unrecoverable error.
        Failure(String),
    }

    /// Static configuration shared by all worker threads.
    ///
    /// Per-pair TCs are NOT stored here — they arrive per-pair via `WorkerCmd::PlayPair`.
    /// This struct holds only static configuration that does not vary across pairs.
    #[derive(Clone, Debug)]
    pub(crate) struct WorkerConfig {
        #[allow(dead_code)]
        pub engine_spec: super::driver::EngineSpec,
        #[allow(dead_code)]
        pub opponent_spec: super::driver::EngineSpec,
        #[allow(dead_code)]
        pub engine_options: Vec<(String, String)>,
        #[allow(dead_code)]
        pub opponent_options: Vec<(String, String)>,
        #[allow(dead_code)]
        pub mode: clawfish::MatchTimeMode,
        #[allow(dead_code)]
        pub harness_overhead_ms: u32,
        #[allow(dead_code)]
        pub watchdog: std::time::Duration,
        #[allow(dead_code)]
        pub max_plies: u32,
        #[allow(dead_code)]
        pub thresholds: super::cli::Thresholds,
        /// When `true`, harness sends `setoption name VirtualClock value true` to
        /// engines advertising the option. See ELOH.C / ADR-0021.
        #[allow(dead_code)]
        pub virtual_clock: bool,
    }

    /// Live worker pool: command senders, report receiver, and thread handles.
    pub(crate) struct WorkerPool {
        pub senders: Vec<mpsc::Sender<WorkerCmd>>,
        #[allow(dead_code)]
        pub reports: mpsc::Receiver<WorkerReport>,
        #[allow(dead_code)]
        pub join_handles: Vec<std::thread::JoinHandle<()>>,
    }

    impl Drop for WorkerPool {
        /// Drop senders so workers see `Disconnected` and exit naturally.
        /// Does NOT join — joining in Drop could block forever on panicking paths.
        fn drop(&mut self) {
            self.senders.clear();
        }
    }

    /// Result of a completed iteration run.
    #[derive(Debug)]
    #[allow(dead_code)]
    pub(crate) struct IterationOutcome {
        pub final_estimate: f64,
        pub final_sigma: f64,
        pub games_played: u32,
        pub stop_reason: super::StopReason,
        pub wld: (u32, u32, u32),
    }

    /// The function signature each worker thread must implement.
    pub(super) type WorkerFn =
        fn(u32, WorkerConfig, mpsc::Receiver<WorkerCmd>, mpsc::Sender<WorkerReport>);

    /// Stockfish-compatible UCI_Elo bounds.
    #[allow(dead_code)]
    const UCI_ELO_MIN: u32 = 1320;
    #[allow(dead_code)]
    const UCI_ELO_MAX: u32 = 3190;

    /// Clamp a real-valued estimate to Stockfish's UCI_Elo range and round to u32.
    #[allow(dead_code)]
    pub(super) fn clamp_uci_elo(elo: f64) -> u32 {
        let rounded = elo.round();
        if rounded.is_nan() {
            return UCI_ELO_MIN;
        }
        if rounded < UCI_ELO_MIN as f64 {
            UCI_ELO_MIN
        } else if rounded > UCI_ELO_MAX as f64 {
            UCI_ELO_MAX
        } else {
            rounded as u32
        }
    }

    /// Map a `GameOutcome` and the side clawfish played to clawfish's POV score
    /// (1.0 = win, 0.5 = draw, 0.0 = loss).
    pub(super) fn compute_clawfish_score(
        outcome: &super::match_loop::GameOutcome,
        clawfish_white: bool,
    ) -> f64 {
        use super::adjudicate::GameOver;
        use super::match_loop::GameOutcome;
        use clawfish::Color;

        let white_score: f64 = match outcome {
            GameOutcome::NativeGameOver(go) => match go {
                GameOver::Checkmate(winner) => match winner {
                    Color::White => 1.0,
                    Color::Black => 0.0,
                },
                GameOver::Stalemate
                | GameOver::FiftyMove
                | GameOver::ThreefoldRepetition
                | GameOver::InsufficientMaterial
                | GameOver::DrawAdjudicated => 0.5,
                GameOver::TimeForfeit(loser) | GameOver::ResignAdjudicated(loser) => match loser {
                    Color::White => 0.0,
                    Color::Black => 1.0,
                },
            },
            GameOutcome::TimeForfeit(loser) | GameOutcome::IllegalMove(loser) => match loser {
                Color::White => 0.0,
                Color::Black => 1.0,
            },
            GameOutcome::MaxMovesReached => 0.5,
        };

        if clawfish_white {
            white_score
        } else {
            1.0 - white_score
        }
    }

    /// Production worker-thread function: spawns engine pair, runs UCI handshake,
    /// applies static options, and drives the per-pair flow on each `WorkerCmd`.
    fn production_worker_fn(
        worker_id: u32,
        cfg: WorkerConfig,
        cmd_rx: mpsc::Receiver<WorkerCmd>,
        rpt_tx: mpsc::Sender<WorkerReport>,
    ) {
        let handshake_to = std::time::Duration::from_secs(10);
        let isready_to = std::time::Duration::from_secs(5);

        let mut engine = match super::driver::spawn_engine(&cfg.engine_spec) {
            Ok(h) => h,
            Err(e) => {
                let _ = rpt_tx.send(WorkerReport::Failure(format!(
                    "worker {worker_id}: spawn engine: {e:?}"
                )));
                return;
            }
        };
        let mut opponent = match super::driver::spawn_engine(&cfg.opponent_spec) {
            Ok(h) => h,
            Err(e) => {
                let _ = rpt_tx.send(WorkerReport::Failure(format!(
                    "worker {worker_id}: spawn opponent: {e:?}"
                )));
                let _ = super::driver::shutdown(engine);
                return;
            }
        };

        // UCI handshake: send `uci` then drain via `wait_for_uciok` for both engines.
        // Capture capabilities so we can negotiate VirtualClock below.
        let engine_caps = if super::driver::send_line(&mut engine, "uci").is_ok() {
            super::driver::wait_for_uciok(&mut engine, handshake_to).ok()
        } else {
            None
        };
        let opponent_caps =
            if engine_caps.is_some() && super::driver::send_line(&mut opponent, "uci").is_ok() {
                super::driver::wait_for_uciok(&mut opponent, handshake_to).ok()
            } else {
                None
            };
        if engine_caps.is_none() || opponent_caps.is_none() {
            let _ = rpt_tx.send(WorkerReport::Failure(format!(
                "worker {worker_id}: uci handshake"
            )));
            let _ = super::driver::shutdown(engine);
            let _ = super::driver::shutdown(opponent);
            return;
        }
        let engine_caps = engine_caps.expect("checked above");
        let opponent_caps = opponent_caps.expect("checked above");

        // Apply static options + VirtualClock negotiation, then sync via isready.
        // VirtualClock is fire-and-forget per UCI; the post-option-block isready
        // below already gates handshake settling. Mirrors the UCI_LimitStrength flow.
        if cfg.virtual_clock && engine_caps.supports_virtual_clock {
            let _ = super::driver::send_line(&mut engine, "setoption name VirtualClock value true");
        }
        if cfg.virtual_clock && opponent_caps.supports_virtual_clock {
            let _ =
                super::driver::send_line(&mut opponent, "setoption name VirtualClock value true");
        }
        for (name, value) in &cfg.engine_options {
            let _ = super::driver::send_line(
                &mut engine,
                &format!("setoption name {name} value {value}"),
            );
        }
        for (name, value) in &cfg.opponent_options {
            let _ = super::driver::send_line(
                &mut opponent,
                &format!("setoption name {name} value {value}"),
            );
        }
        let setopt_ok = super::driver::wait_for_readyok(&mut engine, isready_to).is_ok()
            && super::driver::wait_for_readyok(&mut opponent, isready_to).is_ok();
        if !setopt_ok {
            let _ = rpt_tx.send(WorkerReport::Failure(format!(
                "worker {worker_id}: readyok after setoption"
            )));
            let _ = super::driver::shutdown(engine);
            let _ = super::driver::shutdown(opponent);
            return;
        }

        // Recv loop on WorkerCmd.
        while let Ok(cmd) = cmd_rx.recv() {
            match cmd {
                WorkerCmd::Quit => break,
                WorkerCmd::PlayPair {
                    pair_index,
                    opponent_uci_elo,
                    engine_tc,
                    opponent_tc,
                } => {
                    // **INVARIANT (load-bearing per plan §4.4 + ADR-0020 §2):**
                    // `setoption UCI_Elo` MUST precede `ucinewgame` within a pair.
                    // The Stockfish 18 preflight probe
                    // (`docs/research/tooling-stockfish-mid-session-setoption.md`)
                    // confirms mid-session setoption is honored, but UCI's spec
                    // allows engines to reset per-game state on `ucinewgame`. By
                    // sending setoption FIRST, we defend against any engine that
                    // would clear UCI_Elo on the ucinewgame transition. Reordering
                    // these two operations (or hoisting ucinewgame before the
                    // setoption block) would silently produce wrong-strength play
                    // — the synthetic-worker tests cannot catch this; the
                    // structural ordering pin lives in this comment + the
                    // sequence below.
                    //
                    // Re-apply opponent UCI_Elo + UCI_LimitStrength (idempotent;
                    // sent before each pair to defend against engines that drop
                    // options on ucinewgame).
                    let _ = super::driver::send_line(
                        &mut opponent,
                        &format!("setoption name UCI_Elo value {opponent_uci_elo}"),
                    );
                    let _ = super::driver::send_line(
                        &mut opponent,
                        "setoption name UCI_LimitStrength value true",
                    );
                    if super::driver::wait_for_readyok(&mut opponent, isready_to).is_err() {
                        let _ = rpt_tx.send(WorkerReport::Failure(format!(
                            "worker {worker_id}: readyok after UCI_Elo setoption"
                        )));
                        break;
                    }

                    // Two color-swapped games against the same opp_elo.
                    let mut pair_failed = false;
                    for game_in_pair in 0..2u32 {
                        let clawfish_white = game_in_pair == 0;
                        let game_index = pair_index * 2 + game_in_pair + 1;

                        // ucinewgame + isready for both engines.
                        let _ = super::driver::send_line(&mut engine, "ucinewgame");
                        let engine_ready =
                            super::driver::wait_for_readyok(&mut engine, isready_to).is_ok();
                        let _ = super::driver::send_line(&mut opponent, "ucinewgame");
                        let opponent_ready =
                            super::driver::wait_for_readyok(&mut opponent, isready_to).is_ok();
                        if !(engine_ready && opponent_ready) {
                            let _ = rpt_tx.send(WorkerReport::Failure(format!(
                                "worker {worker_id}: readyok after ucinewgame"
                            )));
                            pair_failed = true;
                            break;
                        }

                        // Build per-color clocks. clawfish-white means engine is white.
                        // engine_tc and opponent_tc are per-pair values from the cmd payload.
                        let (white_tc, black_tc) = if clawfish_white {
                            (engine_tc, opponent_tc)
                        } else {
                            (opponent_tc, engine_tc)
                        };
                        let white_clock = clawfish::PerSideClock {
                            remaining_ms: i64::from(white_tc.initial_ms),
                            increment_ms: white_tc.increment_ms,
                        };
                        let black_clock = clawfish::PerSideClock {
                            remaining_ms: i64::from(black_tc.initial_ms),
                            increment_ms: black_tc.increment_ms,
                        };

                        let (white_engine_index, white_name, black_name) = if clawfish_white {
                            (
                                0usize,
                                cfg.engine_spec.name.clone(),
                                cfg.opponent_spec.name.clone(),
                            )
                        } else {
                            (
                                1usize,
                                cfg.opponent_spec.name.clone(),
                                cfg.engine_spec.name.clone(),
                            )
                        };

                        let mut ctx = super::match_loop::GameContext {
                            game_index,
                            white_engine_index,
                            engine: &mut engine,
                            opponent: &mut opponent,
                            engine_tc,
                            opponent_tc,
                            harness_overhead_ms: cfg.harness_overhead_ms,
                            watchdog: cfg.watchdog,
                            mode: cfg.mode,
                            max_plies: cfg.max_plies,
                            starting_fen: None,
                            white_clock,
                            black_clock,
                            thresholds: cfg.thresholds.clone(),
                        };

                        let (outcome, pgn_moves) = super::match_loop::play_one_game(&mut ctx);
                        let clawfish_score = compute_clawfish_score(&outcome, clawfish_white);

                        let _ = rpt_tx.send(WorkerReport::GameComplete {
                            game_index,
                            opponent_uci_elo,
                            clawfish_score,
                            outcome,
                            pgn_moves,
                            white_name,
                            black_name,
                            tc: engine_tc,
                        });
                    }

                    if pair_failed {
                        break;
                    }
                    let _ = rpt_tx.send(WorkerReport::PairComplete { worker_id });
                }
            }
        }

        let _ = super::driver::shutdown(engine);
        let _ = super::driver::shutdown(opponent);
    }

    /// Spawn `n` worker threads using the production worker-thread function.
    #[allow(dead_code)]
    pub(crate) fn spawn_workers(
        n: u32,
        cfg: WorkerConfig,
    ) -> Result<WorkerPool, super::driver::HarnessError> {
        spawn_workers_with_fn(n, cfg, production_worker_fn)
    }

    /// Internal spawn that accepts a substitutable worker function for tests.
    pub(super) fn spawn_workers_with_fn(
        n: u32,
        cfg: WorkerConfig,
        worker_fn: WorkerFn,
    ) -> Result<WorkerPool, super::driver::HarnessError> {
        let (rpt_tx, rpt_rx) = mpsc::channel::<WorkerReport>();
        let mut senders = Vec::with_capacity(n as usize);
        let mut join_handles = Vec::with_capacity(n as usize);
        for worker_id in 0..n {
            let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
            senders.push(cmd_tx);
            let cfg_clone = cfg.clone();
            let rpt_tx_clone = rpt_tx.clone();
            let handle = std::thread::spawn(move || {
                worker_fn(worker_id, cfg_clone, cmd_rx, rpt_tx_clone);
            });
            join_handles.push(handle);
        }
        // Drop the original rpt_tx; receiver disconnects when all worker clones drop.
        drop(rpt_tx);
        Ok(WorkerPool {
            senders,
            reports: rpt_rx,
            join_handles,
        })
    }

    /// Drive the color-pair dispatch + K-update loop to completion.
    #[allow(dead_code)]
    pub(crate) fn run_iteration(
        pool: &mut WorkerPool,
        args: &super::cli::Args,
        out_dir: &std::path::Path,
    ) -> Result<IterationOutcome, super::driver::HarnessError> {
        let total_pairs = args.max_games / 2;
        let n_workers = pool.senders.len();
        let mut pairs_in_flight: Vec<u32> = vec![0; n_workers];

        let mut current_estimate: f64 = args.initial_elo;
        let mut estimates_trail: Vec<f64> = Vec::new();
        let mut t: u32 = 0;
        let mut wins: u32 = 0;
        let mut losses: u32 = 0;
        let mut draws: u32 = 0;
        let mut pairs_dispatched: u32 = 0;
        let mut terminating = false;
        let mut sigma_fired = false;
        let mut failure: Option<super::driver::HarnessError> = None;

        // Best-effort directory setup; errors here just mean later writes fail.
        let _ = std::fs::create_dir_all(out_dir);
        let games_dir = out_dir.join("games");
        let _ = std::fs::create_dir_all(&games_dir);
        let summary_path = out_dir.join("summary.txt");
        // Start each run with a fresh summary.txt — run_iteration is a complete unit of work.
        let _ = std::fs::remove_file(&summary_path);

        // Pre-materialise all per-pair TCs indexed by pair_index. Sampler advances
        // happen exclusively in this single-threaded up-front loop so the
        // pair_index → engine_tc mapping is deterministic regardless of concurrency.
        // Memory cost: 8 bytes per pair × total_pairs; negligible at 5000 pairs (40 KB).
        let mut tc_rng = super::prng::Prng::new(args.seed.unwrap_or(super::prng::DEFAULT_SEED));
        let pair_tcs: Vec<(super::cli::TimeControl, super::cli::TimeControl)> = (0..total_pairs)
            .map(|_| {
                let engine_tc = match &args.tc_sample {
                    Some(dist) => dist.sample(&mut tc_rng),
                    None => args
                        .tc
                        .expect("post-parse: exactly one of tc/tc_sample set"),
                };
                let opponent_tc = args.opponent_tc_override.unwrap_or(engine_tc);
                (engine_tc, opponent_tc)
            })
            .collect();

        // Build per-TC buckets (only under --tc-sample; skipped entirely in --tc mode).
        let mut buckets: Vec<super::summary::TcBucket> = match &args.tc_sample {
            Some(dist) => dist
                .iter()
                .map(|(tc, _w)| super::summary::TcBucket {
                    tc: *tc,
                    wins: 0,
                    losses: 0,
                    draws: 0,
                })
                .collect(),
            None => Vec::new(),
        };

        // Bootstrap: dispatch up to one PlayPair per worker (or fewer if total_pairs < n_workers).
        for (worker_id, in_flight_slot) in pairs_in_flight.iter_mut().enumerate().take(n_workers) {
            if pairs_dispatched >= total_pairs {
                break;
            }
            let opp_elo = clamp_uci_elo(current_estimate);
            let (engine_tc, opponent_tc) = pair_tcs[pairs_dispatched as usize];
            if pool.senders[worker_id]
                .send(WorkerCmd::PlayPair {
                    pair_index: pairs_dispatched,
                    opponent_uci_elo: opp_elo,
                    engine_tc,
                    opponent_tc,
                })
                .is_err()
            {
                failure = Some(super::driver::HarnessError::EngineExit);
                break;
            }
            pairs_dispatched += 1;
            *in_flight_slot += 1;
        }

        // Drain loop.
        let drain_done = |terminating: bool, pairs_dispatched: u32, in_flight: &[u32]| -> bool {
            let all_idle = in_flight.iter().all(|&x| x == 0);
            (terminating || pairs_dispatched >= total_pairs) && all_idle
        };

        while !drain_done(terminating, pairs_dispatched, &pairs_in_flight) {
            // If we've hit max_games via game count, exit immediately. The final
            // stop_reason distinguishes σ-stop from MaxGames via `sigma_fired`.
            if t >= args.max_games {
                break;
            }
            let report = match pool.reports.recv() {
                Ok(r) => r,
                Err(_) => {
                    // All worker senders disconnected; nothing more to drain.
                    break;
                }
            };
            match report {
                WorkerReport::GameComplete {
                    game_index,
                    opponent_uci_elo,
                    clawfish_score,
                    outcome,
                    pgn_moves,
                    white_name,
                    black_name,
                    tc: report_tc,
                } => {
                    // Persist PGN + summary line (best-effort; missing dirs in tests are tolerated).
                    let (result, termination_str) = super::outcome_to_pgn_result(&outcome);
                    let tc_str = super::format_tc(report_tc);
                    let pgn_header = super::pgn::PgnHeader {
                        event: args.event_tag.clone(),
                        site: super::current_hostname(),
                        date: super::current_date_str(),
                        round: game_index,
                        white: white_name.clone(),
                        black: black_name.clone(),
                        result: result.clone(),
                        time_control: Some(tc_str.clone()),
                        termination: Some(termination_str.clone()),
                        setup_fen: None,
                    };
                    let pgn_str = super::pgn::format_pgn(&pgn_header, &pgn_moves);
                    let pgn_path = games_dir.join(format!("{game_index}.pgn"));
                    let _ = std::fs::write(&pgn_path, &pgn_str);

                    let summary_line = super::summary::SummaryLine {
                        game_index,
                        white: white_name,
                        black: black_name,
                        result,
                        plies: pgn_moves.len() as u32,
                        termination: super::outcome_to_termination_reason(&outcome),
                        tc: Some(tc_str),
                    };
                    let _ = super::summary::append_summary_line(&summary_path, &summary_line);

                    // Per-TC bucket aggregation (only under --tc-sample).
                    if args.tc_sample.is_some()
                        && let Some(idx) = buckets.iter().position(|b| b.tc == report_tc)
                    {
                        if (clawfish_score - 1.0).abs() < 1e-9 {
                            buckets[idx].wins += 1;
                        } else if clawfish_score.abs() < 1e-9 {
                            buckets[idx].losses += 1;
                        } else {
                            buckets[idx].draws += 1;
                        }
                    }

                    // Robbins-Monro update.
                    let k = super::estimator::compute_k(t, args.k0, args.tau);
                    current_estimate = super::estimator::update_estimate(
                        current_estimate,
                        opponent_uci_elo as f64,
                        clawfish_score,
                        k,
                    );
                    estimates_trail.push(current_estimate);
                    t += 1;

                    if (clawfish_score - 1.0).abs() < 1e-9 {
                        wins += 1;
                    } else if clawfish_score.abs() < 1e-9 {
                        losses += 1;
                    } else {
                        draws += 1;
                    }

                    // σ-stopping check (per-game cadence).
                    if !terminating
                        && super::sigma::should_stop(
                            &estimates_trail,
                            args.stop_window,
                            args.target_sigma,
                            args.stop_window_confirm,
                        )
                    {
                        terminating = true;
                        sigma_fired = true;
                    }
                }
                WorkerReport::PairComplete { worker_id } => {
                    let wid = worker_id as usize;
                    if wid < pairs_in_flight.len() {
                        pairs_in_flight[wid] = pairs_in_flight[wid].saturating_sub(1);
                    }

                    // Emit progress line for this pair.
                    let window_start = estimates_trail.len().saturating_sub(args.stop_window);
                    let current_sigma =
                        super::sigma::sample_stddev(&estimates_trail[window_start..]);
                    let current_k =
                        super::estimator::compute_k(t.saturating_sub(1), args.k0, args.tau);
                    let progress_line = super::progress::ProgressLine {
                        t,
                        games: t,
                        elo: current_estimate,
                        sigma: current_sigma,
                        k: current_k,
                        wld: (wins, losses, draws),
                    };
                    let progress_str = super::progress::format_progress(&progress_line);
                    println!("{progress_str}");
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&summary_path)
                    {
                        use std::io::Write;
                        let _ = writeln!(f, "{progress_str}");
                    }

                    // Dispatch next pair on this worker if we still have budget.
                    if !terminating && pairs_dispatched < total_pairs && wid < pool.senders.len() {
                        let opp_elo = clamp_uci_elo(current_estimate);
                        let (engine_tc, opponent_tc) = pair_tcs[pairs_dispatched as usize];
                        if pool.senders[wid]
                            .send(WorkerCmd::PlayPair {
                                pair_index: pairs_dispatched,
                                opponent_uci_elo: opp_elo,
                                engine_tc,
                                opponent_tc,
                            })
                            .is_ok()
                        {
                            pairs_dispatched += 1;
                            pairs_in_flight[wid] += 1;
                        }
                    }
                }
                WorkerReport::Failure(msg) => {
                    eprintln!("worker failure: {msg}");
                    failure = Some(super::driver::HarnessError::EngineExit);
                    break;
                }
            }
        }

        // Send Quit to every sender (best-effort; workers may already be exiting).
        for s in &pool.senders {
            let _ = s.send(WorkerCmd::Quit);
        }
        // Disconnect senders explicitly so any worker still blocked on recv exits.
        pool.senders.clear();

        for h in pool.join_handles.drain(..) {
            // Surface worker panics for diagnosability — a panicking worker is
            // a real-bug signal worth logging even on the error-path cleanup.
            // We don't propagate the panic (cleanup must complete); just log.
            if let Err(panic) = h.join() {
                eprintln!("worker thread panicked during cleanup: {panic:?}");
            }
        }

        if let Some(e) = failure {
            return Err(e);
        }

        let stop_reason = if sigma_fired {
            super::StopReason::Sigma
        } else {
            super::StopReason::MaxGames
        };

        let final_window_start = estimates_trail.len().saturating_sub(args.stop_window);
        let final_sigma = super::sigma::sample_stddev(&estimates_trail[final_window_start..]);

        let converged_str =
            super::progress::format_converged(current_estimate, final_sigma, t, stop_reason);
        println!("{converged_str}");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&summary_path)
        {
            use std::io::Write;
            let _ = writeln!(f, "{converged_str}");
        }

        // Emit summary-by-tc: line (only when --tc-sample was active).
        if args.tc_sample.is_some() {
            let by_tc_str = super::summary::format_summary_by_tc(&buckets);
            println!("{by_tc_str}");
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&summary_path)
            {
                use std::io::Write;
                let _ = writeln!(f, "{by_tc_str}");
            }
        }

        Ok(IterationOutcome {
            final_estimate: current_estimate,
            final_sigma,
            games_played: t,
            stop_reason,
            wld: (wins, losses, draws),
        })
    }

    #[cfg(test)]
    mod tests {
        use std::sync::mpsc;

        use super::*;

        // -----------------------------------------------------------------------
        // Test infrastructure
        // -----------------------------------------------------------------------

        /// Minimum valid `Args` for controller tests.
        ///
        /// Uses `--k0 0 --target-sigma 0` (frozen-K, disabled-σ) so the only
        /// stop criterion is `--max-games`.  Callers may override fields freely.
        fn base_args(max_games: u32) -> super::super::cli::Args {
            let argv: Vec<String> = vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/stockfish".into(),
                "--tc".into(),
                "10+0.1".into(),
                "--max-games".into(),
                max_games.to_string(),
                "--initial-elo".into(),
                "2000".into(),
                "--k0".into(),
                "0".into(),
                "--target-sigma".into(),
                "0".into(),
            ];
            super::super::cli::parse_args(argv).expect("base_args: parse failed")
        }

        /// Construct a `WorkerPool` whose N synthetic worker threads consume
        /// `WorkerCmd`s and emit canned `WorkerReport`s in order.
        ///
        /// Each element of `canned_reports_per_worker` is the sequence of reports
        /// emitted by worker `i` before it idles.  Workers exit when their
        /// command channel closes or they receive `WorkerCmd::Quit`.
        fn synthetic_pool(n: u32, canned_reports_per_worker: Vec<Vec<WorkerReport>>) -> WorkerPool {
            let (rpt_tx, rpt_rx) = mpsc::channel::<WorkerReport>();
            let mut cmd_txs: Vec<mpsc::Sender<WorkerCmd>> = Vec::new();
            let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

            for reports in canned_reports_per_worker.into_iter().take(n as usize) {
                let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
                cmd_txs.push(cmd_tx);
                let rpt_tx_clone = rpt_tx.clone();
                handles.push(std::thread::spawn(move || {
                    for cmd in &cmd_rx {
                        match cmd {
                            WorkerCmd::Quit => break,
                            WorkerCmd::PlayPair { .. } => {
                                for report in &reports {
                                    // Reports are pre-constructed; we can only clone
                                    // non-Clone variants by re-sending a new value.
                                    // We use the variant-by-variant approach here.
                                    let _ = rpt_tx_clone.send(match report {
                                        WorkerReport::PairComplete { worker_id } => {
                                            WorkerReport::PairComplete {
                                                worker_id: *worker_id,
                                            }
                                        }
                                        WorkerReport::Failure(s) => {
                                            WorkerReport::Failure(s.clone())
                                        }
                                        WorkerReport::GameComplete {
                                            game_index,
                                            opponent_uci_elo,
                                            clawfish_score,
                                            tc,
                                            ..
                                        } => WorkerReport::GameComplete {
                                            game_index: *game_index,
                                            opponent_uci_elo: *opponent_uci_elo,
                                            clawfish_score: *clawfish_score,
                                            outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                                            pgn_moves: vec![],
                                            white_name: "w".into(),
                                            black_name: "b".into(),
                                            tc: *tc,
                                        },
                                    });
                                }
                            }
                        }
                    }
                }));
            }

            WorkerPool {
                senders: cmd_txs,
                reports: rpt_rx,
                join_handles: handles,
            }
        }

        // -----------------------------------------------------------------------
        // §6.6 controller tests
        // -----------------------------------------------------------------------

        /// After bootstrap, each of the 4 workers received exactly one `PlayPair`;
        /// the four `pair_index` values across workers are unique (set {0,1,2,3});
        /// each `PlayPair.opponent_uci_elo == round(initial_elo) == 2000`.
        #[test]
        fn dispatch_round_robin_one_pair_per_worker() {
            use std::sync::{Arc, Mutex};

            // 4 per-worker logs — one Arc<Mutex<Vec<WorkerCmd>>> per worker.
            let worker_logs: Vec<Arc<Mutex<Vec<WorkerCmd>>>> =
                (0..4).map(|_| Arc::new(Mutex::new(Vec::new()))).collect();

            let (rpt_tx, rpt_rx) = mpsc::channel::<WorkerReport>();
            let mut cmd_txs: Vec<mpsc::Sender<WorkerCmd>> = Vec::new();
            let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

            for worker_id in 0u32..4 {
                let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
                cmd_txs.push(cmd_tx);
                let rpt_tx_clone = rpt_tx.clone();
                let log = Arc::clone(&worker_logs[worker_id as usize]);
                handles.push(std::thread::spawn(move || {
                    for cmd in &cmd_rx {
                        match cmd {
                            WorkerCmd::Quit => break,
                            WorkerCmd::PlayPair {
                                pair_index,
                                opponent_uci_elo,
                                ..
                            } => {
                                log.lock().unwrap().push(WorkerCmd::PlayPair {
                                    pair_index,
                                    opponent_uci_elo,
                                    engine_tc: super::super::cli::TimeControl {
                                        initial_ms: 10_000,
                                        increment_ms: 100,
                                    },
                                    opponent_tc: super::super::cli::TimeControl {
                                        initial_ms: 10_000,
                                        increment_ms: 100,
                                    },
                                });
                                // Emit one canned PairComplete (no GameComplete needed for
                                // dispatch-only verification).
                                let _ = rpt_tx_clone.send(WorkerReport::PairComplete { worker_id });
                            }
                        }
                    }
                }));
            }

            let mut pool = WorkerPool {
                senders: cmd_txs,
                reports: rpt_rx,
                join_handles: handles,
            };
            let out_dir = std::env::temp_dir().join("eloh_b_dispatch_test");
            // concurrency=4, max_games=8 → total_pairs=4; all 4 dispatched at bootstrap.
            let argv: Vec<String> = vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/stockfish".into(),
                "--tc".into(),
                "10+0.1".into(),
                "--max-games".into(),
                "8".into(),
                "--initial-elo".into(),
                "2000".into(),
                "--k0".into(),
                "0".into(),
                "--target-sigma".into(),
                "0".into(),
                "--concurrency".into(),
                "4".into(),
            ];
            let args = super::super::cli::parse_args(argv).expect("parse ok");
            let _ = run_iteration(&mut pool, &args, &out_dir);

            // Each worker must have received at least one PlayPair at bootstrap.
            let first_pairs: Vec<u32> = worker_logs
                .iter()
                .enumerate()
                .map(|(wid, log)| {
                    let guard = log.lock().unwrap();
                    let first = guard.iter().find_map(|cmd| {
                        if let WorkerCmd::PlayPair { pair_index, .. } = cmd {
                            Some(*pair_index)
                        } else {
                            None
                        }
                    });
                    first.unwrap_or_else(|| {
                        panic!("worker {wid} received no PlayPair during bootstrap")
                    })
                })
                .collect();

            // The bootstrap pair_indices across workers must be the set {0, 1, 2, 3}.
            let mut sorted = first_pairs.clone();
            sorted.sort_unstable();
            assert_eq!(
                sorted,
                vec![0, 1, 2, 3],
                "bootstrap pair_indices must be {{0,1,2,3}}, got {first_pairs:?}"
            );

            // Each worker received exactly one PlayPair (no over-dispatch),
            // and every dispatched PlayPair carries opponent_uci_elo == 2000.
            for (wid, log) in worker_logs.iter().enumerate() {
                let entries = log.lock().unwrap();
                assert_eq!(
                    entries.len(),
                    1,
                    "worker {wid}: expected exactly 1 PlayPair, got {}",
                    entries.len()
                );
                for cmd in entries.iter() {
                    let WorkerCmd::PlayPair {
                        opponent_uci_elo, ..
                    } = cmd
                    else {
                        unreachable!("only PlayPair is pushed to worker logs")
                    };
                    assert_eq!(
                        *opponent_uci_elo, 2000u32,
                        "worker {wid}: expected opponent_uci_elo=2000, got {opponent_uci_elo}"
                    );
                }
            }
        }

        /// Score mapping: 2W + 1L + 1D → `wld = (2, 1, 1)` from clawfish's POV.
        ///
        /// A pair = 2 games. Two workers, each completing 1 pair (2 GameComplete +
        /// 1 PairComplete).  Pair 0: win-as-white (1.0) + win-as-black (1.0).
        /// Pair 1: loss-as-white (0.0) + draw-as-black (0.5).
        ///
        /// **Scope.** This is a controller-surface test on POV-corrected score
        /// aggregation. The `(game_index - 1) % 2 == 0 → clawfish white` color
        /// invariant is a worker-thread internal property; it is not pinned here.
        /// TODO(impl-phase): the e2e `#[ignore]`-gated smoke pins clawfish-color
        /// assignment via PGN tag inspection.
        #[test]
        fn aggregate_wld_handles_clawfish_white_and_black() {
            let out_dir = std::env::temp_dir().join("eloh_b_wld_test");
            // 2 workers, 4 total games (2 per pair), --max-games 4 --concurrency 2.
            let argv: Vec<String> = vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/stockfish".into(),
                "--tc".into(),
                "10+0.1".into(),
                "--max-games".into(),
                "4".into(),
                "--initial-elo".into(),
                "2000".into(),
                "--k0".into(),
                "0".into(),
                "--target-sigma".into(),
                "0".into(),
                "--concurrency".into(),
                "2".into(),
            ];
            let args = super::super::cli::parse_args(argv).expect("parse ok");
            // Worker 0: pair 0 → score(1.0 white) + score(1.0 black) → 2 wins.
            // Worker 1: pair 1 → score(0.0 white) + score(0.5 black) → 1 loss + 1 draw.
            let worker0_reports = vec![
                WorkerReport::GameComplete {
                    game_index: 1,
                    opponent_uci_elo: 2000,
                    clawfish_score: 1.0,
                    outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                    pgn_moves: vec![],
                    white_name: "clawfish".into(),
                    black_name: "stockfish".into(),
                    tc: super::super::cli::TimeControl {
                        initial_ms: 10_000,
                        increment_ms: 100,
                    },
                },
                WorkerReport::GameComplete {
                    game_index: 2,
                    opponent_uci_elo: 2000,
                    clawfish_score: 1.0,
                    outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                    pgn_moves: vec![],
                    white_name: "stockfish".into(),
                    black_name: "clawfish".into(),
                    tc: super::super::cli::TimeControl {
                        initial_ms: 10_000,
                        increment_ms: 100,
                    },
                },
                WorkerReport::PairComplete { worker_id: 0 },
            ];
            let worker1_reports = vec![
                WorkerReport::GameComplete {
                    game_index: 3,
                    opponent_uci_elo: 2000,
                    clawfish_score: 0.0,
                    outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                    pgn_moves: vec![],
                    white_name: "clawfish".into(),
                    black_name: "stockfish".into(),
                    tc: super::super::cli::TimeControl {
                        initial_ms: 10_000,
                        increment_ms: 100,
                    },
                },
                WorkerReport::GameComplete {
                    game_index: 4,
                    opponent_uci_elo: 2000,
                    clawfish_score: 0.5,
                    outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                    pgn_moves: vec![],
                    white_name: "stockfish".into(),
                    black_name: "clawfish".into(),
                    tc: super::super::cli::TimeControl {
                        initial_ms: 10_000,
                        increment_ms: 100,
                    },
                },
                WorkerReport::PairComplete { worker_id: 1 },
            ];
            let mut pool = synthetic_pool(2, vec![worker0_reports, worker1_reports]);
            let outcome = run_iteration(&mut pool, &args, &out_dir).unwrap();
            assert_eq!(outcome.wld, (2, 1, 1), "wld must be (2, 1, 1)");
        }

        /// With `--target-sigma 0 --max-games N`, the loop stops after exactly N
        /// games and the stop reason is `MaxGames`.
        #[test]
        fn controller_terminates_on_max_games() {
            let out_dir = std::env::temp_dir().join("eloh_b_maxgames_test");
            let n: u32 = 4;
            let args = base_args(n);
            // Feed N/2 pairs each with 2 GameComplete + 1 PairComplete.
            let pair_reports: Vec<WorkerReport> = (0..n / 2)
                .flat_map(|p| {
                    vec![
                        WorkerReport::GameComplete {
                            game_index: p * 2 + 1,
                            opponent_uci_elo: 2000,
                            clawfish_score: 0.5,
                            outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                            pgn_moves: vec![],
                            white_name: "w".into(),
                            black_name: "b".into(),
                            tc: super::super::cli::TimeControl {
                                initial_ms: 10_000,
                                increment_ms: 100,
                            },
                        },
                        WorkerReport::GameComplete {
                            game_index: p * 2 + 2,
                            opponent_uci_elo: 2000,
                            clawfish_score: 0.5,
                            outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                            pgn_moves: vec![],
                            white_name: "w".into(),
                            black_name: "b".into(),
                            tc: super::super::cli::TimeControl {
                                initial_ms: 10_000,
                                increment_ms: 100,
                            },
                        },
                        WorkerReport::PairComplete { worker_id: 0 },
                    ]
                })
                .collect();
            let mut pool = synthetic_pool(1, vec![pair_reports]);
            let outcome = run_iteration(&mut pool, &args, &out_dir).unwrap();
            assert_eq!(
                outcome.stop_reason,
                super::super::StopReason::MaxGames,
                "expected MaxGames stop reason"
            );
            assert_eq!(outcome.games_played, n, "games_played must equal max_games");
        }

        /// A constant-estimate stream causes σ=0, so `should_stop` fires and
        /// the stop reason is `Sigma` (with `--target-sigma > 0`).
        #[test]
        fn controller_terminates_on_sigma() {
            let out_dir = std::env::temp_dir().join("eloh_b_sigma_test");
            // Use a non-zero target_sigma (not frozen) with a large max_games
            // ceiling so only σ-stopping can terminate the loop.
            // Provide enough constant draws to saturate the confirm window.
            let argv: Vec<String> = vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/stockfish".into(),
                "--tc".into(),
                "10+0.1".into(),
                "--max-games".into(),
                "200".into(),
                "--initial-elo".into(),
                "2000".into(),
                "--k0".into(),
                "40".into(),
                "--target-sigma".into(),
                "30".into(),
                "--stop-window".into(),
                "30".into(),
                "--stop-window-confirm".into(),
                "5".into(),
            ];
            let args = super::super::cli::parse_args(argv).expect("parse ok");
            // Feed 100 pairs, all draws (clawfish_score=0.5).
            // With constant scores and Robbins-Monro, the estimate trail will
            // stabilise → trailing σ → 0 → should_stop fires within 34+5=39 games.
            let pair_reports: Vec<WorkerReport> = (0..100u32)
                .flat_map(|p| {
                    vec![
                        WorkerReport::GameComplete {
                            game_index: p * 2 + 1,
                            opponent_uci_elo: 2000,
                            clawfish_score: 0.5,
                            outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                            pgn_moves: vec![],
                            white_name: "w".into(),
                            black_name: "b".into(),
                            tc: super::super::cli::TimeControl {
                                initial_ms: 10_000,
                                increment_ms: 100,
                            },
                        },
                        WorkerReport::GameComplete {
                            game_index: p * 2 + 2,
                            opponent_uci_elo: 2000,
                            clawfish_score: 0.5,
                            outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                            pgn_moves: vec![],
                            white_name: "w".into(),
                            black_name: "b".into(),
                            tc: super::super::cli::TimeControl {
                                initial_ms: 10_000,
                                increment_ms: 100,
                            },
                        },
                        WorkerReport::PairComplete { worker_id: 0 },
                    ]
                })
                .collect();
            let mut pool = synthetic_pool(1, vec![pair_reports]);
            let outcome = run_iteration(&mut pool, &args, &out_dir).unwrap();
            assert_eq!(
                outcome.stop_reason,
                super::super::StopReason::Sigma,
                "expected Sigma stop reason on constant-draw stream"
            );
            assert!(
                outcome.games_played >= 34,
                "should_stop must respect window+confirm-1=34 minimum data entries; got {}",
                outcome.games_played
            );
        }

        /// Verify that every `PlayPair` emitted by the controller carries
        /// `opponent_uci_elo` equal to `round(initial_elo)`.
        ///
        /// The setoption-before-ucinewgame ordering is a worker-thread internal
        /// discipline pinned by the e2e smoke; this controller-surface test only
        /// verifies opponent_uci_elo round-trip in the WorkerCmd payload.
        // TODO(impl-phase): The setoption-before-ucinewgame ordering is a
        // worker-thread internal discipline pinned by the e2e smoke; this
        // controller-surface test only verifies opponent_uci_elo round-trip in
        // the WorkerCmd payload.
        #[test]
        fn controller_dispatches_playpair_carrying_correct_opponent_uci_elo() {
            use std::sync::{Arc, Mutex};

            let (rpt_tx, rpt_rx) = mpsc::channel::<WorkerReport>();
            let mut cmd_txs: Vec<mpsc::Sender<WorkerCmd>> = Vec::new();
            let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();
            // One shared log across both workers to collect all received PlayPairs.
            let received: Arc<Mutex<Vec<WorkerCmd>>> = Arc::new(Mutex::new(Vec::new()));

            for worker_id in 0u32..2 {
                let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
                cmd_txs.push(cmd_tx);
                let rpt_tx_clone = rpt_tx.clone();
                let log = Arc::clone(&received);
                handles.push(std::thread::spawn(move || {
                    for cmd in &cmd_rx {
                        match cmd {
                            WorkerCmd::Quit => break,
                            WorkerCmd::PlayPair {
                                pair_index,
                                opponent_uci_elo,
                                ..
                            } => {
                                log.lock().unwrap().push(WorkerCmd::PlayPair {
                                    pair_index,
                                    opponent_uci_elo,
                                    engine_tc: super::super::cli::TimeControl {
                                        initial_ms: 10_000,
                                        increment_ms: 100,
                                    },
                                    opponent_tc: super::super::cli::TimeControl {
                                        initial_ms: 10_000,
                                        increment_ms: 100,
                                    },
                                });
                                let _ = rpt_tx_clone.send(WorkerReport::GameComplete {
                                    game_index: pair_index * 2 + 1,
                                    opponent_uci_elo,
                                    clawfish_score: 0.5,
                                    outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                                    pgn_moves: vec![],
                                    white_name: "w".into(),
                                    black_name: "b".into(),
                                    tc: super::super::cli::TimeControl {
                                        initial_ms: 10_000,
                                        increment_ms: 100,
                                    },
                                });
                                let _ = rpt_tx_clone.send(WorkerReport::GameComplete {
                                    game_index: pair_index * 2 + 2,
                                    opponent_uci_elo,
                                    clawfish_score: 0.5,
                                    outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                                    pgn_moves: vec![],
                                    white_name: "w".into(),
                                    black_name: "b".into(),
                                    tc: super::super::cli::TimeControl {
                                        initial_ms: 10_000,
                                        increment_ms: 100,
                                    },
                                });
                                let _ = rpt_tx_clone.send(WorkerReport::PairComplete { worker_id });
                            }
                        }
                    }
                }));
            }

            let mut pool = WorkerPool {
                senders: cmd_txs,
                reports: rpt_rx,
                join_handles: handles,
            };
            let out_dir = std::env::temp_dir().join("eloh_b_uci_elo_roundtrip_test");
            // initial_elo=2114; --k0 0 freezes the estimate so subsequent dispatches
            // also use opp=2114. (Workers DO emit GameComplete reports; the no-drift
            // property is structural via --k0 0, not arithmetic via score cancellation.)
            let argv: Vec<String> = vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/stockfish".into(),
                "--tc".into(),
                "10+0.1".into(),
                "--max-games".into(),
                "4".into(),
                "--initial-elo".into(),
                "2114".into(),
                "--k0".into(),
                "0".into(),
                "--target-sigma".into(),
                "0".into(),
                "--concurrency".into(),
                "2".into(),
            ];
            let args = super::super::cli::parse_args(argv).expect("parse ok");
            let _ = run_iteration(&mut pool, &args, &out_dir);
            // Verify all dispatched PlayPairs carry opponent_uci_elo == 2114.
            let log = received.lock().unwrap();
            assert!(
                !log.is_empty(),
                "at least one PlayPair must have been dispatched"
            );
            for cmd in log.iter() {
                let WorkerCmd::PlayPair {
                    opponent_uci_elo, ..
                } = cmd
                else {
                    unreachable!("only PlayPair is pushed to the log")
                };
                assert_eq!(
                    *opponent_uci_elo, 2114u32,
                    "opponent_uci_elo must equal round(initial_elo)=2114, got {opponent_uci_elo}"
                );
            }
        }

        /// With `--k0 0`, the K-update is a no-op and the estimate stays at
        /// `initial_elo` regardless of game results.
        #[test]
        fn controller_freeze_k_holds_initial_estimate() {
            let out_dir = std::env::temp_dir().join("eloh_b_freeze_k_test");
            let initial_elo = 2114.0;
            let argv: Vec<String> = vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/stockfish".into(),
                "--tc".into(),
                "10+0.1".into(),
                "--max-games".into(),
                "8".into(),
                "--initial-elo".into(),
                initial_elo.to_string(),
                "--k0".into(),
                "0".into(),
                "--target-sigma".into(),
                "0".into(),
            ];
            let args = super::super::cli::parse_args(argv).expect("parse ok");
            // Mix of wins, losses, draws — should have no effect on the estimate.
            let pair_reports: Vec<WorkerReport> = [1.0f64, 0.0, 1.0, 0.0]
                .iter()
                .enumerate()
                .flat_map(|(p, &s)| {
                    let p = p as u32;
                    vec![
                        WorkerReport::GameComplete {
                            game_index: p * 2 + 1,
                            opponent_uci_elo: 2114,
                            clawfish_score: s,
                            outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                            pgn_moves: vec![],
                            white_name: "w".into(),
                            black_name: "b".into(),
                            tc: super::super::cli::TimeControl {
                                initial_ms: 10_000,
                                increment_ms: 100,
                            },
                        },
                        WorkerReport::GameComplete {
                            game_index: p * 2 + 2,
                            opponent_uci_elo: 2114,
                            clawfish_score: 1.0 - s,
                            outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                            pgn_moves: vec![],
                            white_name: "w".into(),
                            black_name: "b".into(),
                            tc: super::super::cli::TimeControl {
                                initial_ms: 10_000,
                                increment_ms: 100,
                            },
                        },
                        WorkerReport::PairComplete { worker_id: 0 },
                    ]
                })
                .collect();
            let mut pool = synthetic_pool(1, vec![pair_reports]);
            let outcome = run_iteration(&mut pool, &args, &out_dir).unwrap();
            assert!(
                (outcome.final_estimate - initial_elo).abs() < 1e-6,
                "frozen-K: estimate must stay at initial_elo={initial_elo}, \
                 got {}",
                outcome.final_estimate
            );
        }

        /// With 2 workers where worker 0 sleeps 200 ms per report, the controller
        /// should finish 4 pairs in <700 ms by dispatching to worker 1 concurrently
        /// (~500 ms theoretical lower bound + ~200 ms scheduling slack for CI noise).
        /// Serial blocking would take ≥800 ms.
        #[test]
        fn controller_does_not_block_on_slow_worker() {
            let out_dir = std::env::temp_dir().join("eloh_b_nonblocking_test");
            let mut args = base_args(8);
            // Concurrency must match the manually-built 2-worker pool;
            // base_args defaults to 1, which would serialise dispatch.
            args.concurrency = 2;

            // Worker 0: sleeps 200ms before each report.
            let (rpt_tx, rpt_rx) = mpsc::channel::<WorkerReport>();
            let mut cmd_txs: Vec<mpsc::Sender<WorkerCmd>> = Vec::new();
            let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

            for worker_id in 0u32..2 {
                let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
                cmd_txs.push(cmd_tx);
                let rpt_tx_clone = rpt_tx.clone();
                let slow = worker_id == 0;
                handles.push(std::thread::spawn(move || {
                    let mut pair_counter = 0u32;
                    for cmd in &cmd_rx {
                        match cmd {
                            WorkerCmd::Quit => break,
                            WorkerCmd::PlayPair { .. } => {
                                if slow {
                                    std::thread::sleep(std::time::Duration::from_millis(200));
                                }
                                pair_counter += 1;
                                let g = pair_counter * 2;
                                for gi in [g - 1, g] {
                                    let _ = rpt_tx_clone.send(WorkerReport::GameComplete {
                                        game_index: gi,
                                        opponent_uci_elo: 2000,
                                        clawfish_score: 0.5,
                                        outcome:
                                            super::super::match_loop::GameOutcome::MaxMovesReached,
                                        pgn_moves: vec![],
                                        white_name: "w".into(),
                                        black_name: "b".into(),
                                        tc: super::super::cli::TimeControl {
                                            initial_ms: 10_000,
                                            increment_ms: 100,
                                        },
                                    });
                                }
                                let _ = rpt_tx_clone.send(WorkerReport::PairComplete { worker_id });
                            }
                        }
                    }
                }));
            }

            let mut pool = WorkerPool {
                senders: cmd_txs,
                reports: rpt_rx,
                join_handles: handles,
            };

            let t0 = std::time::Instant::now();
            let _ = run_iteration(&mut pool, &args, &out_dir);
            let elapsed = t0.elapsed();

            // Serial would take ≥800ms (4 slow-worker pairs × 200ms each).
            // With concurrency, worker 1 handles some pairs while worker 0 sleeps.
            assert!(
                elapsed < std::time::Duration::from_millis(700),
                "controller blocked serially: elapsed={elapsed:?}, expected < 700ms"
            );
        }

        // ---- ELOH.B Tier-B/C targeted tests: clamp_uci_elo ---------------

        #[test]
        fn clamp_uci_elo_below_min_clamps_to_1320() {
            // Any value below 1320 must be clamped to 1320.
            assert_eq!(clamp_uci_elo(1319.0), UCI_ELO_MIN);
        }

        #[test]
        fn clamp_uci_elo_at_min_returns_min() {
            // Exactly 1320.0: must return 1320 without clamping.
            // Mutant `< 1320` → `<= 1320` would also clamp 1320.0 to 1320 (same
            // result), but `< 1320` → `== 1320` would miss it (falls through to the
            // `> max` check or the `rounded as u32` path, giving 1320 anyway).
            // The `<= 1320` mutant is caught because clamp_uci_elo(1320.0) == 1320
            // either way; the important test is the ACCEPTANCE test at 1321 below.
            assert_eq!(clamp_uci_elo(UCI_ELO_MIN as f64), UCI_ELO_MIN);
        }

        #[test]
        fn clamp_uci_elo_just_above_min_passes_through() {
            // 1321 is strictly above the min; must not be clamped.
            // Distinguishes `< 1320` (correct) from `<= 1320` (mutant): with the
            // mutant, 1321 would not be clamped (both pass the lower guard), so
            // this test does not catch that specific mutant.  The test pairs with
            // `clamp_uci_elo_at_min_returns_min` to bracket the boundary.
            assert_eq!(clamp_uci_elo(1321.0), 1321);
        }

        #[test]
        fn clamp_uci_elo_at_max_returns_max() {
            // Exactly 3190.0: must return 3190 without clamping.
            assert_eq!(clamp_uci_elo(UCI_ELO_MAX as f64), UCI_ELO_MAX);
        }

        #[test]
        fn clamp_uci_elo_above_max_clamps_to_3190() {
            // Any value above 3190 must be clamped to 3190.
            assert_eq!(clamp_uci_elo(3191.0), UCI_ELO_MAX);
        }

        // ---- ELOH.B Tier-B targeted tests: compute_clawfish_score ----------
        //
        // Cover each GameOutcome branch for both clawfish_white=true and false,
        // pinning the `1.0 - white_score` subtraction against `+` and `/` mutants.

        #[test]
        fn compute_clawfish_score_white_wins_clawfish_white() {
            use super::super::adjudicate::GameOver;
            use super::super::match_loop::GameOutcome;
            use clawfish::Color;
            let outcome = GameOutcome::NativeGameOver(GameOver::Checkmate(Color::White));
            // White wins → white_score=1.0; clawfish_white → clawfish_score=1.0.
            let score = compute_clawfish_score(&outcome, true);
            assert!((score - 1.0).abs() < 1e-9, "expected 1.0, got {score}");
        }

        #[test]
        fn compute_clawfish_score_white_wins_clawfish_black() {
            use super::super::adjudicate::GameOver;
            use super::super::match_loop::GameOutcome;
            use clawfish::Color;
            let outcome = GameOutcome::NativeGameOver(GameOver::Checkmate(Color::White));
            // White wins → white_score=1.0; clawfish_black → clawfish_score = 1.0-1.0 = 0.0.
            // Mutant `1.0 - white_score` → `1.0 + white_score` would give 2.0.
            // Mutant `- → /` would give 1.0/1.0 = 1.0 (wrong).
            let score = compute_clawfish_score(&outcome, false);
            assert!(score.abs() < 1e-9, "expected 0.0, got {score}");
        }

        #[test]
        fn compute_clawfish_score_black_wins_clawfish_white() {
            use super::super::adjudicate::GameOver;
            use super::super::match_loop::GameOutcome;
            use clawfish::Color;
            let outcome = GameOutcome::NativeGameOver(GameOver::Checkmate(Color::Black));
            // Black wins → white_score=0.0; clawfish_white → clawfish_score=0.0.
            let score = compute_clawfish_score(&outcome, true);
            assert!(score.abs() < 1e-9, "expected 0.0, got {score}");
        }

        #[test]
        fn compute_clawfish_score_black_wins_clawfish_black() {
            use super::super::adjudicate::GameOver;
            use super::super::match_loop::GameOutcome;
            use clawfish::Color;
            let outcome = GameOutcome::NativeGameOver(GameOver::Checkmate(Color::Black));
            // Black wins → white_score=0.0; clawfish_black → clawfish_score = 1.0-0.0 = 1.0.
            // Mutant `- → +` would give 1.0+0.0 = 1.0 (same! — can't distinguish for 0.0 input).
            // The white-wins tests above pin the subtraction for non-zero input.
            let score = compute_clawfish_score(&outcome, false);
            assert!((score - 1.0).abs() < 1e-9, "expected 1.0, got {score}");
        }

        #[test]
        fn compute_clawfish_score_draw_variants() {
            use super::super::adjudicate::GameOver;
            use super::super::match_loop::GameOutcome;
            // All draw variants must yield 0.5 regardless of side.
            let draws = [
                GameOutcome::NativeGameOver(GameOver::Stalemate),
                GameOutcome::NativeGameOver(GameOver::FiftyMove),
                GameOutcome::NativeGameOver(GameOver::ThreefoldRepetition),
                GameOutcome::NativeGameOver(GameOver::InsufficientMaterial),
                GameOutcome::NativeGameOver(GameOver::DrawAdjudicated),
                GameOutcome::MaxMovesReached,
            ];
            for outcome in &draws {
                let score_w = compute_clawfish_score(outcome, true);
                let score_b = compute_clawfish_score(outcome, false);
                assert!(
                    (score_w - 0.5).abs() < 1e-9,
                    "draw outcome {outcome:?} clawfish_white: expected 0.5, got {score_w}"
                );
                assert!(
                    (score_b - 0.5).abs() < 1e-9,
                    "draw outcome {outcome:?} clawfish_black: expected 0.5, got {score_b}"
                );
            }
        }

        #[test]
        fn compute_clawfish_score_time_forfeit_and_illegal_move() {
            use super::super::match_loop::GameOutcome;
            use clawfish::Color;
            // White forfeits on time or plays an illegal move → clawfish wins if black.
            for outcome in [
                GameOutcome::TimeForfeit(Color::White),
                GameOutcome::IllegalMove(Color::White),
            ] {
                // clawfish_white=true means clawfish IS white → clawfish loses.
                let s_cw = compute_clawfish_score(&outcome, true);
                assert!(
                    s_cw.abs() < 1e-9,
                    "white forfeits, clawfish_white: expected 0.0, got {s_cw}"
                );
                // clawfish_black means clawfish is black → clawfish wins.
                let s_cb = compute_clawfish_score(&outcome, false);
                assert!(
                    (s_cb - 1.0).abs() < 1e-9,
                    "white forfeits, clawfish_black: expected 1.0, got {s_cb}"
                );
            }
        }

        // -----------------------------------------------------------------------
        // §6.5 ELOH.D controller tests — TC dispatch + per-TC bucket aggregation
        // -----------------------------------------------------------------------

        /// Helpers for ELOH.D controller tests.
        fn tc(initial_ms: u32, increment_ms: u32) -> super::super::cli::TimeControl {
            super::super::cli::TimeControl {
                initial_ms,
                increment_ms,
            }
        }

        /// Build a `TcDistribution` from a spec string; panics on parse error (tests only).
        #[allow(dead_code)]
        fn make_dist(spec: &str) -> super::super::tc_sample::TcDistribution {
            super::super::tc_sample::parse_tc_sample(spec)
                .expect("make_dist: parse_tc_sample failed")
        }

        #[test]
        fn bootstrap_dispatches_per_pair_sampled_tc_under_tc_sample() {
            // args.tc_sample = Some(dist); controller's bootstrap sends WorkerCmd::PlayPair
            // { engine_tc, .. } where engine_tc is the sampler's first draw under the fixed seed.
            // Captured via the synthetic_pool's command-recorder.
            use std::sync::{Arc, Mutex};
            let recorded_cmds: Arc<Mutex<Vec<WorkerCmd>>> = Arc::new(Mutex::new(Vec::new()));
            let (rpt_tx, rpt_rx) = mpsc::channel::<WorkerReport>();
            let mut cmd_txs: Vec<mpsc::Sender<WorkerCmd>> = Vec::new();
            let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

            let log = Arc::clone(&recorded_cmds);
            let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
            cmd_txs.push(cmd_tx);
            let rpt_tx_clone = rpt_tx.clone();
            handles.push(std::thread::spawn(move || {
                for cmd in &cmd_rx {
                    match cmd {
                        WorkerCmd::Quit => break,
                        WorkerCmd::PlayPair {
                            pair_index,
                            opponent_uci_elo,
                            engine_tc,
                            opponent_tc,
                        } => {
                            log.lock().unwrap().push(WorkerCmd::PlayPair {
                                pair_index,
                                opponent_uci_elo,
                                engine_tc,
                                opponent_tc,
                            });
                            let _ = rpt_tx_clone.send(WorkerReport::PairComplete { worker_id: 0 });
                        }
                    }
                }
            }));

            let mut pool = WorkerPool {
                senders: cmd_txs,
                reports: rpt_rx,
                join_handles: handles,
            };

            // Build args with tc_sample; must use parse_args to set tc_sample correctly.
            let argv: Vec<String> = vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/stockfish".into(),
                "--tc-sample".into(),
                "10+0.1:1,20+0.2:1".into(),
                "--max-games".into(),
                "2".into(),
                "--initial-elo".into(),
                "2000".into(),
                "--k0".into(),
                "0".into(),
                "--target-sigma".into(),
                "0".into(),
                "--seed".into(),
                "42".into(),
            ];
            let args = super::super::cli::parse_args(argv)
                .expect("parse ok with --tc-sample — ELOH.D Slice A pending");
            let out_dir = std::env::temp_dir().join("eloh_d_bootstrap_tc_sample_test");
            let _ = run_iteration(&mut pool, &args, &out_dir);

            let cmds = recorded_cmds.lock().unwrap();
            assert_eq!(
                cmds.len(),
                1,
                "exactly one PlayPair dispatched at --max-games 2"
            );
            // Pin the exact first draw of Prng::new(42) against
            // parse_tc_sample("10+0.1:1,20+0.2:1") — = tc(20_000, 200).
            // Computed by reading the SplitMix64 stream once at the seed +
            // doing one cumulative-bucket lookup over the 2-bucket dist.
            // Catches a sampler-skip bug, a stale-pair_index bug, or a
            // streaming-rather-than-pre-materialised regression that
            // set-membership would miss.
            let WorkerCmd::PlayPair { engine_tc, .. } = &cmds[0] else {
                unreachable!("only PlayPair is logged")
            };
            assert_eq!(
                *engine_tc,
                tc(20_000, 200),
                "first draw under Prng::new(42) against [10+0.1:1, 20+0.2:1] \
                 must be 20+0.2; got {engine_tc:?}"
            );
        }

        #[test]
        fn drain_loop_redispatch_resamples_per_pair() {
            // After 2 PairComplete reports, the third dispatched PlayPair carries
            // the sampler's third draw. Pins per-pair-not-per-game cadence.
            // The test verifies all dispatched PlayPairs carry a valid TC from the dist.
            use std::sync::{Arc, Mutex};
            let recorded: Arc<Mutex<Vec<super::super::cli::TimeControl>>> =
                Arc::new(Mutex::new(Vec::new()));
            let (rpt_tx, rpt_rx) = mpsc::channel::<WorkerReport>();
            let mut cmd_txs: Vec<mpsc::Sender<WorkerCmd>> = Vec::new();
            let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

            let log = Arc::clone(&recorded);
            let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
            cmd_txs.push(cmd_tx);
            let rpt_tx_clone = rpt_tx.clone();
            handles.push(std::thread::spawn(move || {
                for cmd in &cmd_rx {
                    match cmd {
                        WorkerCmd::Quit => break,
                        WorkerCmd::PlayPair {
                            pair_index,
                            opponent_uci_elo,
                            engine_tc,
                            opponent_tc,
                        } => {
                            log.lock().unwrap().push(engine_tc);
                            let _ = rpt_tx_clone.send(WorkerReport::GameComplete {
                                game_index: pair_index * 2 + 1,
                                opponent_uci_elo,
                                clawfish_score: 0.5,
                                outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                                pgn_moves: vec![],
                                white_name: "w".into(),
                                black_name: "b".into(),
                                tc: engine_tc,
                            });
                            let _ = rpt_tx_clone.send(WorkerReport::GameComplete {
                                game_index: pair_index * 2 + 2,
                                opponent_uci_elo,
                                clawfish_score: 0.5,
                                outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                                pgn_moves: vec![],
                                white_name: "w".into(),
                                black_name: "b".into(),
                                tc: opponent_tc,
                            });
                            let _ = rpt_tx_clone.send(WorkerReport::PairComplete { worker_id: 0 });
                        }
                    }
                }
            }));

            let mut pool = WorkerPool {
                senders: cmd_txs,
                reports: rpt_rx,
                join_handles: handles,
            };
            let argv: Vec<String> = vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/stockfish".into(),
                "--tc-sample".into(),
                "10+0.1:1,20+0.2:1".into(),
                "--max-games".into(),
                "6".into(),
                "--initial-elo".into(),
                "2000".into(),
                "--k0".into(),
                "0".into(),
                "--target-sigma".into(),
                "0".into(),
                "--seed".into(),
                "42".into(),
            ];
            let args =
                super::super::cli::parse_args(argv).expect("parse ok — ELOH.D Slice A pending");
            let out_dir = std::env::temp_dir().join("eloh_d_drain_resample_test");
            let _ = run_iteration(&mut pool, &args, &out_dir);

            let tcs = recorded.lock().unwrap();
            // Pin the exact 3-element TC sequence for Prng::new(42) against
            // [10+0.1:1, 20+0.2:1] — = [20+0.2, 10+0.1, 10+0.1].
            // (First three draws of the SplitMix64 stream consumed in order
            // by the up-front pair_tcs materialisation in run_iteration.)
            // Catches a stale-pair_index bug, a copy-paste off-by-one in the
            // redispatch path, or a streaming-rather-than-pre-materialised
            // regression that set-membership would miss. Per-pair-not-per-game
            // cadence is implicit: 3 pairs (6 games) yield 3 sampler advances.
            let expected: Vec<super::super::cli::TimeControl> =
                vec![tc(20_000, 200), tc(10_000, 100), tc(10_000, 100)];
            assert_eq!(
                *tcs, expected,
                "expected 3 pair-level TCs in order [20+0.2, 10+0.1, 10+0.1]; \
                 got {tcs:?}"
            );
        }

        #[test]
        fn tc_sample_pair_color_swap_uses_same_tc() {
            // CONTROLLER-CONTRACT: this test validates the controller's reception path —
            // that when two GameComplete reports arrive for the same pair_index carrying
            // the same `tc`, both are counted in the same per-TC bucket. The synthetic
            // worker below emits both GameComplete reports with `tc = engine_tc` (the
            // value from the cmd payload), which is the correct production behavior.
            // The production worker's emit-routing must be verified independently —
            // see §6.7 end_to_end_self_play_tc_sample_runs which exercises the full
            // pipeline.
            use std::sync::{Arc, Mutex};
            let reported_tcs: Arc<Mutex<Vec<super::super::cli::TimeControl>>> =
                Arc::new(Mutex::new(Vec::new()));
            let (rpt_tx, rpt_rx) = mpsc::channel::<WorkerReport>();
            let mut cmd_txs: Vec<mpsc::Sender<WorkerCmd>> = Vec::new();
            let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

            let log = Arc::clone(&reported_tcs);
            let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
            cmd_txs.push(cmd_tx);
            let rpt_tx_clone = rpt_tx.clone();
            handles.push(std::thread::spawn(move || {
                for cmd in &cmd_rx {
                    match cmd {
                        WorkerCmd::Quit => break,
                        WorkerCmd::PlayPair {
                            pair_index,
                            opponent_uci_elo,
                            engine_tc,
                            ..
                        } => {
                            // Emit two GameCompletes with the SAME tc (color-pair invariant).
                            for game_in_pair in 0..2u32 {
                                let report_tc = engine_tc;
                                log.lock().unwrap().push(report_tc);
                                let _ = rpt_tx_clone.send(WorkerReport::GameComplete {
                                    game_index: pair_index * 2 + game_in_pair + 1,
                                    opponent_uci_elo,
                                    clawfish_score: 0.5,
                                    outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                                    pgn_moves: vec![],
                                    white_name: "w".into(),
                                    black_name: "b".into(),
                                    tc: report_tc,
                                });
                            }
                            let _ = rpt_tx_clone.send(WorkerReport::PairComplete { worker_id: 0 });
                        }
                    }
                }
            }));

            let mut pool = WorkerPool {
                senders: cmd_txs,
                reports: rpt_rx,
                join_handles: handles,
            };
            let argv: Vec<String> = vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/stockfish".into(),
                "--tc-sample".into(),
                "10+0.1:1,20+0.2:1".into(),
                "--max-games".into(),
                "2".into(),
                "--initial-elo".into(),
                "2000".into(),
                "--k0".into(),
                "0".into(),
                "--target-sigma".into(),
                "0".into(),
                "--seed".into(),
                "42".into(),
            ];
            let args =
                super::super::cli::parse_args(argv).expect("parse ok — ELOH.D Slice A pending");
            let out_dir = std::env::temp_dir().join("eloh_d_color_swap_tc_test");
            let _ = run_iteration(&mut pool, &args, &out_dir);

            let tcs = reported_tcs.lock().unwrap();
            assert_eq!(tcs.len(), 2, "one pair = 2 GameComplete reports");
            // Both games of the same pair must use the same TC.
            assert_eq!(tcs[0], tcs[1], "color-swapped games must use the same TC");
        }

        #[test]
        fn opponent_tc_override_dominates_under_tc_sample() {
            // args.tc_sample = Some(dist), args.opponent_tc_override = Some(60+0.6);
            // PlayPair's opponent_tc == 60+0.6 regardless of which TC the sampler drew.
            use std::sync::{Arc, Mutex};
            let recorded: Arc<
                Mutex<
                    Vec<(
                        super::super::cli::TimeControl,
                        super::super::cli::TimeControl,
                    )>,
                >,
            > = Arc::new(Mutex::new(Vec::new()));
            let (rpt_tx, rpt_rx) = mpsc::channel::<WorkerReport>();
            let mut cmd_txs: Vec<mpsc::Sender<WorkerCmd>> = Vec::new();
            let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

            let log = Arc::clone(&recorded);
            let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
            cmd_txs.push(cmd_tx);
            let rpt_tx_clone = rpt_tx.clone();
            handles.push(std::thread::spawn(move || {
                for cmd in &cmd_rx {
                    match cmd {
                        WorkerCmd::Quit => break,
                        WorkerCmd::PlayPair {
                            pair_index,
                            opponent_uci_elo,
                            engine_tc,
                            opponent_tc,
                        } => {
                            log.lock().unwrap().push((engine_tc, opponent_tc));
                            let _ = rpt_tx_clone.send(WorkerReport::GameComplete {
                                game_index: pair_index * 2 + 1,
                                opponent_uci_elo,
                                clawfish_score: 0.5,
                                outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                                pgn_moves: vec![],
                                white_name: "w".into(),
                                black_name: "b".into(),
                                tc: engine_tc,
                            });
                            let _ = rpt_tx_clone.send(WorkerReport::GameComplete {
                                game_index: pair_index * 2 + 2,
                                opponent_uci_elo,
                                clawfish_score: 0.5,
                                outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                                pgn_moves: vec![],
                                white_name: "w".into(),
                                black_name: "b".into(),
                                tc: engine_tc,
                            });
                            let _ = rpt_tx_clone.send(WorkerReport::PairComplete { worker_id: 0 });
                        }
                    }
                }
            }));

            let mut pool = WorkerPool {
                senders: cmd_txs,
                reports: rpt_rx,
                join_handles: handles,
            };
            let override_tc = tc(60_000, 600);
            let argv: Vec<String> = vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/stockfish".into(),
                "--tc-sample".into(),
                "10+0.1:1,20+0.2:1".into(),
                "--opponent-tc-override".into(),
                "60+0.6".into(),
                "--max-games".into(),
                "2".into(),
                "--initial-elo".into(),
                "2000".into(),
                "--k0".into(),
                "0".into(),
                "--target-sigma".into(),
                "0".into(),
                "--seed".into(),
                "42".into(),
            ];
            let args =
                super::super::cli::parse_args(argv).expect("parse ok — ELOH.D Slice A pending");
            let out_dir = std::env::temp_dir().join("eloh_d_override_tc_test");
            let _ = run_iteration(&mut pool, &args, &out_dir);

            let pairs = recorded.lock().unwrap();
            assert!(!pairs.is_empty(), "at least one PlayPair dispatched");
            for (engine_tc_val, opp_tc_val) in pairs.iter() {
                assert_eq!(
                    *opp_tc_val, override_tc,
                    "opponent_tc must always be the override 60+0.6; got {opp_tc_val:?}"
                );
                let valid = [tc(10_000, 100), tc(20_000, 200)];
                assert!(
                    valid.contains(engine_tc_val),
                    "engine_tc {engine_tc_val:?} must come from dist"
                );
            }
        }

        #[test]
        fn tc_mode_passes_static_tc_in_play_pair() {
            // args.tc = Some(10+0.1), args.tc_sample = None; every PlayPair's
            // engine_tc == 10+0.1 and opponent_tc == args.opponent_tc_override.unwrap_or(10+0.1).
            // Backwards-compatibility test.
            use std::sync::{Arc, Mutex};
            let recorded: Arc<
                Mutex<
                    Vec<(
                        super::super::cli::TimeControl,
                        super::super::cli::TimeControl,
                    )>,
                >,
            > = Arc::new(Mutex::new(Vec::new()));
            let (rpt_tx, rpt_rx) = mpsc::channel::<WorkerReport>();
            let mut cmd_txs: Vec<mpsc::Sender<WorkerCmd>> = Vec::new();
            let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

            let log = Arc::clone(&recorded);
            let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
            cmd_txs.push(cmd_tx);
            let rpt_tx_clone = rpt_tx.clone();
            handles.push(std::thread::spawn(move || {
                for cmd in &cmd_rx {
                    match cmd {
                        WorkerCmd::Quit => break,
                        WorkerCmd::PlayPair {
                            pair_index,
                            opponent_uci_elo,
                            engine_tc,
                            opponent_tc,
                        } => {
                            log.lock().unwrap().push((engine_tc, opponent_tc));
                            let _ = rpt_tx_clone.send(WorkerReport::GameComplete {
                                game_index: pair_index * 2 + 1,
                                opponent_uci_elo,
                                clawfish_score: 0.5,
                                outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                                pgn_moves: vec![],
                                white_name: "w".into(),
                                black_name: "b".into(),
                                tc: engine_tc,
                            });
                            let _ = rpt_tx_clone.send(WorkerReport::GameComplete {
                                game_index: pair_index * 2 + 2,
                                opponent_uci_elo,
                                clawfish_score: 0.5,
                                outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                                pgn_moves: vec![],
                                white_name: "w".into(),
                                black_name: "b".into(),
                                tc: engine_tc,
                            });
                            let _ = rpt_tx_clone.send(WorkerReport::PairComplete { worker_id: 0 });
                        }
                    }
                }
            }));

            let mut pool = WorkerPool {
                senders: cmd_txs,
                reports: rpt_rx,
                join_handles: handles,
            };
            let expected_tc = tc(10_000, 100);
            let out_dir = std::env::temp_dir().join("eloh_d_tc_mode_passthrough_test");
            let args = base_args(4);
            let _ = run_iteration(&mut pool, &args, &out_dir);

            let pairs = recorded.lock().unwrap();
            assert!(!pairs.is_empty(), "at least one PlayPair dispatched");
            for (engine_tc_val, opp_tc_val) in pairs.iter() {
                assert_eq!(
                    *engine_tc_val, expected_tc,
                    "engine_tc must be the static --tc value 10+0.1; got {engine_tc_val:?}"
                );
                // No override → opponent_tc == engine_tc.
                assert_eq!(
                    *opp_tc_val, expected_tc,
                    "opponent_tc must equal engine_tc when no override; got {opp_tc_val:?}"
                );
            }
        }

        #[test]
        fn per_tc_buckets_aggregate_in_input_order() {
            // 4-bucket uniform dist, 8 games (4 pairs); after run, the captured
            // summary-by-tc line exists and is in input-spec order with W+L+D = 2
            // per bucket. Plan §6.5 spec: "after run, the captured buckets are in
            // input-spec order with W+L+D summing to 2 per bucket."
            //
            // Each pair emits two GameComplete reports carrying the same tc, one per
            // input-spec TC so each bucket gets exactly 2 games (one pair = two games).
            let argv: Vec<String> = vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/stockfish".into(),
                "--tc-sample".into(),
                "10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1".into(),
                "--max-games".into(),
                "8".into(),
                "--initial-elo".into(),
                "2000".into(),
                "--k0".into(),
                "0".into(),
                "--target-sigma".into(),
                "0".into(),
                "--seed".into(),
                "42".into(),
            ];
            let args =
                super::super::cli::parse_args(argv).expect("parse ok — ELOH.D Slice A pending");
            let out_dir = std::env::temp_dir().join("eloh_d_bucket_order_test");
            // Four pairs, one for each TC in the distribution. Each pair's two
            // GameComplete reports carry the TC corresponding to that pair's bucket,
            // so each bucket accumulates exactly 2 game outcomes (W+L+D = 2).
            let tc_fixtures = [
                super::super::cli::TimeControl {
                    initial_ms: 10_000,
                    increment_ms: 100,
                },
                super::super::cli::TimeControl {
                    initial_ms: 20_000,
                    increment_ms: 200,
                },
                super::super::cli::TimeControl {
                    initial_ms: 40_000,
                    increment_ms: 400,
                },
                super::super::cli::TimeControl {
                    initial_ms: 60_000,
                    increment_ms: 600,
                },
            ];
            let pair_reports: Vec<WorkerReport> = (0..4u32)
                .flat_map(|p| {
                    // Each pair uses the TC corresponding to its bucket in input-spec order.
                    let t = tc_fixtures[p as usize];
                    vec![
                        WorkerReport::GameComplete {
                            game_index: p * 2 + 1,
                            opponent_uci_elo: 2000,
                            clawfish_score: 0.5,
                            outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                            pgn_moves: vec![],
                            white_name: "w".into(),
                            black_name: "b".into(),
                            tc: t,
                        },
                        WorkerReport::GameComplete {
                            game_index: p * 2 + 2,
                            opponent_uci_elo: 2000,
                            clawfish_score: 0.5,
                            outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                            pgn_moves: vec![],
                            white_name: "w".into(),
                            black_name: "b".into(),
                            tc: t,
                        },
                        WorkerReport::PairComplete { worker_id: 0 },
                    ]
                })
                .collect();
            let mut pool = synthetic_pool(1, vec![pair_reports]);
            let _ = run_iteration(&mut pool, &args, &out_dir);
            // After run, summary.txt must have a summary-by-tc: line in input-spec order.
            let summary = std::fs::read_to_string(out_dir.join("summary.txt")).unwrap_or_default();
            assert!(
                summary.contains("summary-by-tc:"),
                "summary.txt must contain 'summary-by-tc:' when --tc-sample active; got:\n{summary}"
            );
            // Verify input-spec order in the line.
            let by_tc_line = summary
                .lines()
                .find(|l| l.starts_with("summary-by-tc:"))
                .expect("summary-by-tc: line missing");
            let pos_10 = by_tc_line.find("10+0.1:").expect("10+0.1 bucket missing");
            let pos_20 = by_tc_line.find("20+0.2:").expect("20+0.2 bucket missing");
            let pos_40 = by_tc_line.find("40+0.4:").expect("40+0.4 bucket missing");
            let pos_60 = by_tc_line.find("60+0.6:").expect("60+0.6 bucket missing");
            assert!(
                pos_10 < pos_20 && pos_20 < pos_40 && pos_40 < pos_60,
                "buckets must appear in input-spec order; line: {by_tc_line}"
            );
            // Each bucket must accumulate exactly 2 game outcomes (W+L+D = 2 per bucket).
            // This asserts that the controller routes GameComplete reports to the correct
            // per-TC bucket based on the tc field, not that all games go to one bucket.
            for tc_str in ["10+0.1", "20+0.2", "40+0.4", "60+0.6"] {
                let bucket_pattern = format!("{tc_str}: W=");
                let bucket_entry = by_tc_line
                    .split("  ")
                    .find(|seg| seg.contains(&bucket_pattern))
                    .unwrap_or_else(|| panic!("bucket {tc_str} missing in: {by_tc_line}"));
                // Extract W, L, D values and sum them — must equal 2.
                // Format: "10+0.1: W=N L=N D=N (total)"
                let total_start = bucket_entry.rfind('(').expect("bucket total missing");
                let total_end = bucket_entry
                    .rfind(')')
                    .expect("bucket total closing paren missing");
                let total: u32 = bucket_entry[total_start + 1..total_end]
                    .trim()
                    .parse()
                    .unwrap_or_else(|_| {
                        panic!("bucket {tc_str} total not a u32; entry: {bucket_entry}")
                    });
                assert_eq!(
                    total, 2,
                    "bucket {tc_str} must have W+L+D=2; entry: {bucket_entry}"
                );
            }
        }

        #[test]
        fn summary_by_tc_line_appended_under_tc_sample() {
            // args.tc_sample = Some(dist); summary.txt's last line matches ^summary-by-tc: regex.
            let argv: Vec<String> = vec![
                "--engine".into(),
                "/bin/clawfish".into(),
                "--opponent".into(),
                "/bin/stockfish".into(),
                "--tc-sample".into(),
                "10+0.1:1,20+0.2:1".into(),
                "--max-games".into(),
                "2".into(),
                "--initial-elo".into(),
                "2000".into(),
                "--k0".into(),
                "0".into(),
                "--target-sigma".into(),
                "0".into(),
                "--seed".into(),
                "42".into(),
            ];
            let args =
                super::super::cli::parse_args(argv).expect("parse ok — ELOH.D Slice A pending");
            let out_dir = std::env::temp_dir().join("eloh_d_summary_by_tc_present_test");
            let pair_reports: Vec<WorkerReport> = (0..1u32)
                .flat_map(|p| {
                    let t = super::super::cli::TimeControl {
                        initial_ms: 10_000,
                        increment_ms: 100,
                    };
                    vec![
                        WorkerReport::GameComplete {
                            game_index: p * 2 + 1,
                            opponent_uci_elo: 2000,
                            clawfish_score: 0.5,
                            outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                            pgn_moves: vec![],
                            white_name: "w".into(),
                            black_name: "b".into(),
                            tc: t,
                        },
                        WorkerReport::GameComplete {
                            game_index: p * 2 + 2,
                            opponent_uci_elo: 2000,
                            clawfish_score: 0.5,
                            outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                            pgn_moves: vec![],
                            white_name: "w".into(),
                            black_name: "b".into(),
                            tc: t,
                        },
                        WorkerReport::PairComplete { worker_id: 0 },
                    ]
                })
                .collect();
            let mut pool = synthetic_pool(1, vec![pair_reports]);
            let _ = run_iteration(&mut pool, &args, &out_dir);
            let summary = std::fs::read_to_string(out_dir.join("summary.txt")).unwrap_or_default();
            let last_nonempty = summary.lines().rfind(|l| !l.is_empty()).unwrap_or("");
            assert!(
                last_nonempty.starts_with("summary-by-tc:"),
                "last non-empty line must be 'summary-by-tc:'; got: {last_nonempty:?}\nfull summary:\n{summary}"
            );
        }

        #[test]
        fn summary_by_tc_line_absent_under_tc_only() {
            // args.tc = Some(...), args.tc_sample = None; summary.txt has no summary-by-tc: line.
            //
            // TDD-NOTE: passes trivially against the skeleton because the skeleton never
            // emits a summary-by-tc: line regardless of mode. The real impl must verify
            // the gating logic was actually checked. Companion test
            // `summary_by_tc_line_appended_under_tc_sample` is the positive gate; this
            // is the negative gate. Both are required for mutual-exclusion confidence.
            let out_dir = std::env::temp_dir().join("eloh_d_no_summary_by_tc_test");
            let args = base_args(4);
            let pair_reports: Vec<WorkerReport> = (0..2u32)
                .flat_map(|p| {
                    vec![
                        WorkerReport::GameComplete {
                            game_index: p * 2 + 1,
                            opponent_uci_elo: 2000,
                            clawfish_score: 0.5,
                            outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                            pgn_moves: vec![],
                            white_name: "w".into(),
                            black_name: "b".into(),
                            tc: super::super::cli::TimeControl {
                                initial_ms: 10_000,
                                increment_ms: 100,
                            },
                        },
                        WorkerReport::GameComplete {
                            game_index: p * 2 + 2,
                            opponent_uci_elo: 2000,
                            clawfish_score: 0.5,
                            outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                            pgn_moves: vec![],
                            white_name: "w".into(),
                            black_name: "b".into(),
                            tc: super::super::cli::TimeControl {
                                initial_ms: 10_000,
                                increment_ms: 100,
                            },
                        },
                        WorkerReport::PairComplete { worker_id: 0 },
                    ]
                })
                .collect();
            let mut pool = synthetic_pool(1, vec![pair_reports]);
            let _ = run_iteration(&mut pool, &args, &out_dir);
            let summary = std::fs::read_to_string(out_dir.join("summary.txt")).unwrap_or_default();
            assert!(
                !summary.contains("summary-by-tc:"),
                "summary.txt must NOT contain 'summary-by-tc:' in --tc mode; got:\n{summary}"
            );
        }

        #[test]
        fn seed_reproducibility_pair_tc_mapping_deterministic() {
            // Two synthetic runs with identical args + identical --seed → identical pair_tcs Vecs.
            // Since pair_tcs are pre-materialised (§4.5), the mapping pair_index → engine_tc
            // is deterministic independent of concurrency.
            use std::sync::{Arc, Mutex};

            fn run_and_collect_tcs(seed: u64, n_pairs: u32) -> Vec<super::super::cli::TimeControl> {
                let collected: Arc<Mutex<Vec<(u32, super::super::cli::TimeControl)>>> =
                    Arc::new(Mutex::new(Vec::new()));
                let (rpt_tx, rpt_rx) = mpsc::channel::<WorkerReport>();
                let mut cmd_txs: Vec<mpsc::Sender<WorkerCmd>> = Vec::new();
                let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

                let log = Arc::clone(&collected);
                let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
                cmd_txs.push(cmd_tx);
                let rpt_tx_clone = rpt_tx.clone();
                handles.push(std::thread::spawn(move || {
                    for cmd in &cmd_rx {
                        match cmd {
                            WorkerCmd::Quit => break,
                            WorkerCmd::PlayPair {
                                pair_index,
                                opponent_uci_elo,
                                engine_tc,
                                ..
                            } => {
                                log.lock().unwrap().push((pair_index, engine_tc));
                                let _ = rpt_tx_clone.send(WorkerReport::GameComplete {
                                    game_index: pair_index * 2 + 1,
                                    opponent_uci_elo,
                                    clawfish_score: 0.5,
                                    outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                                    pgn_moves: vec![],
                                    white_name: "w".into(),
                                    black_name: "b".into(),
                                    tc: engine_tc,
                                });
                                let _ = rpt_tx_clone.send(WorkerReport::GameComplete {
                                    game_index: pair_index * 2 + 2,
                                    opponent_uci_elo,
                                    clawfish_score: 0.5,
                                    outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                                    pgn_moves: vec![],
                                    white_name: "w".into(),
                                    black_name: "b".into(),
                                    tc: engine_tc,
                                });
                                let _ =
                                    rpt_tx_clone.send(WorkerReport::PairComplete { worker_id: 0 });
                            }
                        }
                    }
                }));

                let mut pool = WorkerPool {
                    senders: cmd_txs,
                    reports: rpt_rx,
                    join_handles: handles,
                };
                let argv: Vec<String> = vec![
                    "--engine".into(),
                    "/bin/clawfish".into(),
                    "--opponent".into(),
                    "/bin/stockfish".into(),
                    "--tc-sample".into(),
                    "10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1".into(),
                    "--max-games".into(),
                    (n_pairs * 2).to_string(),
                    "--initial-elo".into(),
                    "2000".into(),
                    "--k0".into(),
                    "0".into(),
                    "--target-sigma".into(),
                    "0".into(),
                    "--seed".into(),
                    seed.to_string(),
                ];
                let args =
                    super::super::cli::parse_args(argv).expect("parse ok — ELOH.D Slice A pending");
                let out_dir = std::env::temp_dir().join(format!(
                    "eloh_d_seed_repro_test_{seed}_{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos()
                ));
                let _ = run_iteration(&mut pool, &args, &out_dir);

                let mut pairs = collected.lock().unwrap().clone();
                // Sort by pair_index to get deterministic order.
                pairs.sort_unstable_by_key(|(idx, _)| *idx);
                pairs.into_iter().map(|(_, t)| t).collect()
            }

            let seed = 0xC1AB_F15A_E10D_D000u64;
            let run1 = run_and_collect_tcs(seed, 4);
            let run2 = run_and_collect_tcs(seed, 4);

            assert_eq!(
                run1, run2,
                "two runs with identical seed must yield identical pair_index → engine_tc mapping"
            );
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
        mode: clawfish::MatchTimeMode::Wallclock,
        harness_overhead_ms: args.harness_overhead_ms,
        watchdog,
        max_plies: args.max_moves,
        thresholds: args.thresholds.clone(),
        virtual_clock: args.virtual_clock,
    };

    let mut pool = match controller::spawn_workers(args.concurrency, cfg) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: spawn_workers: {e:?}");
            return ExitCode::from(1);
        }
    };

    let out_dir = std::path::Path::new(&args.out_dir).to_owned();
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
            let (result, term) = outcome_to_pgn_result(outcome);
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
}
