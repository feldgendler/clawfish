//! CLI argument parsing for `elo-iterate`.

/// Default Robbins-Monro initial K. Frozen-K sentinel is 0.0.
pub(crate) const K0_DEFAULT: f64 = 40.0;
/// Default Robbins-Monro decay constant τ.
pub(crate) const TAU_DEFAULT: f64 = 10.0;
/// Default σ-stopping threshold. Disabled sentinel is 0.0.
pub(crate) const TARGET_SIGMA_DEFAULT: f64 = 30.0;
/// Default trailing-σ window length.
pub(crate) const STOP_WINDOW_DEFAULT: usize = 30;
/// Default consecutive-confirmation count for σ-stopping.
pub(crate) const STOP_WINDOW_CONFIRM_DEFAULT: usize = 5;

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
    // ELOH.E SPRT fields. All four `sprt_*` are `Some` together iff SPRT mode
    // is active. See `docs/decisions/0022-eloh-sprt-mechanics.md`.
    /// `--sprt-elo0 N`. When `Some`, SPRT mode is active.
    pub sprt_elo0: Option<f64>,
    /// `--sprt-elo1 N`.
    pub sprt_elo1: Option<f64>,
    /// `--sprt-alpha F`. Must be in (0, 1).
    pub sprt_alpha: Option<f64>,
    /// `--sprt-beta F`. Must be in (0, 1).
    pub sprt_beta: Option<f64>,

    /// When `true`, the harness sends `setoption name VirtualClock value true` to
    /// engines advertising the option (clawfish does; Stockfish does not). Engines
    /// with the option measure search time in thread CPU time rather than wallclock,
    /// making rating measurements more robust to thermal throttling. Default off.
    ///
    /// Note: CPU time is not fully thermal-invariant; combine with P-core pinning
    /// and external cooling for tighter results. See ADR-0021 /
    /// `docs/research/tooling-cpu-cycle-counters.md` for the reasoning.
    pub virtual_clock: bool,

    // SPSA tuning fields. Active when `spsa` is true.
    /// `--spsa` — activate SPSA mode.
    /// Mutually exclusive with `--sprt-*` and K-update/σ-stopping flags.
    pub spsa: bool,
    /// Per-parameter specs parsed from `--spsa-param NAME:THETA0:LO:HI:CEND:ENC`.
    pub spsa_params: Vec<super::spsa::SpsaParam>,
    /// Total SPSA iteration budget.
    pub spsa_iters: Option<u64>,
    /// Number of games per iteration (must be even; default 2).
    pub spsa_games_per_iter: u32,
    /// Desired relative apply factor at the last iteration (default 0.002).
    pub spsa_r_end: f64,
    /// Spall stability constant override. When `None`, defaults to `0.1 * spsa_iters`.
    pub spsa_a_override: Option<f64>,
    /// Tail-averaging window (last T iterates). Default: 10% of `spsa_iters`.
    pub spsa_tail_average: Option<u64>,
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
    let inc_s: f64 = inc_str
        .parse()
        .map_err(|_| CliError::InvalidValue(format!("bad time-control increment: {inc_str}")))?;
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

/// Parse a `NAME:THETA0:LO:HI:CEND:ENC` SPSA parameter spec.
///
/// `ENC` must be `cp` or `centik` (case-insensitive).
pub(crate) fn parse_spsa_param(s: &str) -> Result<super::spsa::SpsaParam, CliError> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        return Err(CliError::InvalidValue(format!(
            "--spsa-param must be NAME:THETA0:LO:HI:CEND:ENC, got: {s}"
        )));
    }
    let name = parts[0].to_owned();
    let theta: f64 = parts[1].parse().map_err(|_| {
        CliError::InvalidValue(format!("--spsa-param THETA0 not a number: {}", parts[1]))
    })?;
    let lo: f64 = parts[2].parse().map_err(|_| {
        CliError::InvalidValue(format!("--spsa-param LO not a number: {}", parts[2]))
    })?;
    let hi: f64 = parts[3].parse().map_err(|_| {
        CliError::InvalidValue(format!("--spsa-param HI not a number: {}", parts[3]))
    })?;
    let c_end: f64 = parts[4].parse().map_err(|_| {
        CliError::InvalidValue(format!("--spsa-param CEND not a number: {}", parts[4]))
    })?;
    let encoding = match parts[5].to_ascii_lowercase().as_str() {
        "cp" => super::spsa::Encoding::IntCp,
        "centik" => super::spsa::Encoding::CentiK,
        other => {
            return Err(CliError::InvalidValue(format!(
                "--spsa-param ENC must be 'cp' or 'centik', got: {other}"
            )));
        }
    };
    if lo > hi {
        return Err(CliError::InvalidValue(format!(
            "--spsa-param LO ({lo}) must be <= HI ({hi}) for param '{name}'"
        )));
    }
    if !(lo <= theta && theta <= hi) {
        return Err(CliError::InvalidValue(format!(
            "--spsa-param THETA0 ({theta}) must be in [LO={lo}, HI={hi}] for param '{name}'"
        )));
    }
    if c_end <= 0.0 {
        return Err(CliError::InvalidValue(format!(
            "--spsa-param CEND must be > 0, got: {c_end} for param '{name}'"
        )));
    }
    Ok(super::spsa::SpsaParam::new(
        name, theta, lo, hi, c_end, encoding,
    ))
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
    let mut k0: f64 = K0_DEFAULT;
    let mut tau: f64 = TAU_DEFAULT;
    let mut target_sigma: f64 = TARGET_SIGMA_DEFAULT;
    let mut stop_window: usize = STOP_WINDOW_DEFAULT;
    let mut stop_window_confirm: usize = STOP_WINDOW_CONFIRM_DEFAULT;
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

    // ELOH.E SPRT fields. All four required when sprt_elo0 is set.
    let mut sprt_elo0: Option<f64> = None;
    let mut sprt_elo1: Option<f64> = None;
    let mut sprt_alpha: Option<f64> = None;
    let mut sprt_beta: Option<f64> = None;

    // SPSA fields.
    let mut spsa: bool = false;
    let mut spsa_params: Vec<super::spsa::SpsaParam> = Vec::new();
    let mut spsa_iters: Option<u64> = None;
    let mut spsa_games_per_iter: u32 = 2;
    let mut spsa_r_end: f64 = 0.002;
    let mut spsa_a_override: Option<f64> = None;
    let mut spsa_tail_average: Option<u64> = None;

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
                tc = Some(parse_tc(s).map_err(|e| CliError::InvalidValue(format!("--tc: {e}")))?);
            }
            "--opponent-tc-override" => {
                let s = next_val!();
                opponent_tc_override =
                    Some(parse_tc(s).map_err(|e| {
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
                harness_overhead_ms = s
                    .parse()
                    .map_err(|_| CliError::InvalidValue(format!("--harness-overhead-ms: {s}")))?;
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
                engine_options.push(
                    parse_option(s)
                        .map_err(|_| CliError::InvalidValue(format!("--engine-option: {s}")))?,
                );
            }
            "--opponent-option" => {
                let s = next_val!();
                opponent_options.push(
                    parse_option(s)
                        .map_err(|_| CliError::InvalidValue(format!("--opponent-option: {s}")))?,
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
                let v: usize = s
                    .parse()
                    .map_err(|_| CliError::InvalidValue(format!("--stop-window-confirm: {s}")))?;
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
            // ELOH.E: SPRT bounds. All four required together; presence of
            // --sprt-elo0 activates SPRT mode at the post-loop validation.
            "--sprt-elo0" => {
                let s = next_val!();
                sprt_elo0 = Some(
                    s.parse()
                        .map_err(|_| CliError::InvalidValue(format!("--sprt-elo0: {s}")))?,
                );
            }
            "--sprt-elo1" => {
                let s = next_val!();
                sprt_elo1 = Some(
                    s.parse()
                        .map_err(|_| CliError::InvalidValue(format!("--sprt-elo1: {s}")))?,
                );
            }
            "--sprt-alpha" => {
                let s = next_val!();
                sprt_alpha = Some(
                    s.parse()
                        .map_err(|_| CliError::InvalidValue(format!("--sprt-alpha: {s}")))?,
                );
            }
            "--sprt-beta" => {
                let s = next_val!();
                sprt_beta = Some(
                    s.parse()
                        .map_err(|_| CliError::InvalidValue(format!("--sprt-beta: {s}")))?,
                );
            }
            // SPSA mode flags.
            "--spsa" => {
                spsa = true;
            }
            "--spsa-param" => {
                let s = next_val!();
                spsa_params.push(
                    parse_spsa_param(s)
                        .map_err(|e| CliError::InvalidValue(format!("--spsa-param: {e}")))?,
                );
            }
            "--spsa-iters" => {
                let s = next_val!();
                let v: u64 = s
                    .parse()
                    .map_err(|_| CliError::InvalidValue(format!("--spsa-iters: {s}")))?;
                if v < 1 {
                    return Err(CliError::InvalidValue("--spsa-iters must be >= 1".into()));
                }
                spsa_iters = Some(v);
            }
            "--spsa-games-per-iter" => {
                let s = next_val!();
                let v: u32 = s
                    .parse()
                    .map_err(|_| CliError::InvalidValue(format!("--spsa-games-per-iter: {s}")))?;
                if v < 2 || !v.is_multiple_of(2) {
                    return Err(CliError::InvalidValue(
                        "--spsa-games-per-iter must be even and >= 2".into(),
                    ));
                }
                spsa_games_per_iter = v;
            }
            "--spsa-r-end" => {
                let s = next_val!();
                let v: f64 = s
                    .parse()
                    .map_err(|_| CliError::InvalidValue(format!("--spsa-r-end: {s}")))?;
                if v <= 0.0 {
                    return Err(CliError::InvalidValue("--spsa-r-end must be > 0".into()));
                }
                spsa_r_end = v;
            }
            "--spsa-A" => {
                let s = next_val!();
                let v: f64 = s
                    .parse()
                    .map_err(|_| CliError::InvalidValue(format!("--spsa-A: {s}")))?;
                if v < 0.0 {
                    return Err(CliError::InvalidValue("--spsa-A must be >= 0".into()));
                }
                spsa_a_override = Some(v);
            }
            "--tail-average" => {
                let s = next_val!();
                let v: u64 = s
                    .parse()
                    .map_err(|_| CliError::InvalidValue(format!("--tail-average: {s}")))?;
                if v < 1 {
                    return Err(CliError::InvalidValue("--tail-average must be >= 1".into()));
                }
                spsa_tail_average = Some(v);
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
    // --max-games is required in SPRT/Elo mode but not in SPSA mode (which uses --spsa-iters).
    let max_games = if spsa {
        max_games.unwrap_or(2)
    } else {
        max_games.ok_or_else(|| CliError::MissingFlag("--max-games".into()))?
    };

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
    // --initial-elo is required in Elo mode but not in SPSA mode.
    let initial_elo = if spsa {
        initial_elo.unwrap_or(2000.0)
    } else {
        initial_elo.ok_or_else(|| CliError::MissingFlag("--initial-elo".into()))?
    };

    // Sentinel composition: --k0 0 requires --target-sigma 0.
    // With K=0 the estimate trail is constant, so σ=0 always, and σ-stopping
    // would fire trivially. Enforce explicit --target-sigma 0 to declare
    // fixed-anchor intent clearly.
    if k0 == 0.0 && target_sigma != 0.0 {
        return Err(CliError::InvalidValue(
            "--k0 0 requires --target-sigma 0 (frozen-K fixed-anchor mode)".into(),
        ));
    }

    // ELOH.E SPRT mutex: --sprt-* is incompatible with K-update flags and
    // with the rating-estimate frozen-anchor mode. SPRT compares two
    // binaries at fixed (unknown) Elo via LLR-bound stopping; combining it
    // with a moving K-update estimate is methodologically incoherent.
    let sprt_active = sprt_elo0.is_some();
    if sprt_active {
        let all_set = sprt_elo0.is_some()
            && sprt_elo1.is_some()
            && sprt_alpha.is_some()
            && sprt_beta.is_some();
        if !all_set {
            return Err(CliError::InvalidValue(
                "--sprt-elo0 requires all of --sprt-elo1, --sprt-alpha, --sprt-beta".into(),
            ));
        }
        let k_update_default = k0 == K0_DEFAULT && tau == TAU_DEFAULT;
        let sigma_stopping_default = target_sigma == TARGET_SIGMA_DEFAULT
            && stop_window == STOP_WINDOW_DEFAULT
            && stop_window_confirm == STOP_WINDOW_CONFIRM_DEFAULT;
        // initial_elo is required by the frozen-anchor flow but harmless
        // for SPRT (the SPRT verdict ignores it). Treat it as default-OK
        // when set, but reject explicit --k0 / --tau / --target-sigma /
        // --stop-window / --stop-window-confirm overrides.
        if !(k_update_default && sigma_stopping_default) {
            return Err(CliError::InvalidValue(
                "--sprt-* is incompatible with K-update / σ-stopping flags \
                 (--k0, --tau, --target-sigma, --stop-window, --stop-window-confirm). \
                 Remove the offending flags and re-run."
                    .into(),
            ));
        }
    }
    if let Some(a) = sprt_alpha
        && !(a > 0.0 && a < 1.0)
    {
        return Err(CliError::InvalidValue(
            "--sprt-alpha must be in (0, 1)".into(),
        ));
    }
    if let Some(b) = sprt_beta
        && !(b > 0.0 && b < 1.0)
    {
        return Err(CliError::InvalidValue(
            "--sprt-beta must be in (0, 1)".into(),
        ));
    }

    // SPSA mutex: --spsa is incompatible with --sprt-* flags and with the
    // K-update / σ-stopping flags. SPSA drives θ via match outcomes, not
    // Elo tracking; combining modes is methodologically incoherent.
    if spsa {
        for (flag, is_set) in [
            ("--sprt-elo0", sprt_elo0.is_some()),
            ("--sprt-elo1", sprt_elo1.is_some()),
            ("--sprt-alpha", sprt_alpha.is_some()),
            ("--sprt-beta", sprt_beta.is_some()),
        ] {
            if is_set {
                return Err(CliError::InvalidValue(format!(
                    "--spsa and {flag} are mutually exclusive"
                )));
            }
        }
        let k_update_default = k0 == K0_DEFAULT && tau == TAU_DEFAULT;
        let sigma_stopping_default = target_sigma == TARGET_SIGMA_DEFAULT
            && stop_window == STOP_WINDOW_DEFAULT
            && stop_window_confirm == STOP_WINDOW_CONFIRM_DEFAULT;
        if !(k_update_default && sigma_stopping_default) {
            return Err(CliError::InvalidValue(
                "--spsa is incompatible with K-update / σ-stopping flags \
                 (--k0, --tau, --target-sigma, --stop-window, --stop-window-confirm). \
                 Remove the offending flags and re-run."
                    .into(),
            ));
        }
        if spsa_iters.is_none() {
            return Err(CliError::MissingFlag(
                "--spsa-iters is required when --spsa is set".into(),
            ));
        }
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
        sprt_elo0,
        sprt_elo1,
        sprt_alpha,
        sprt_beta,
        spsa,
        spsa_params,
        spsa_iters,
        spsa_games_per_iter,
        spsa_r_end,
        spsa_a_override,
        spsa_tail_average,
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

    // --- §6.2 ELOH.E SPRT CLI tests ---

    fn sprt_base_argv() -> Vec<String> {
        vec![
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
        ]
    }

    #[test]
    fn parse_args_sprt_all_four_flags_accepted() {
        let mut argv = sprt_base_argv();
        argv.extend([
            "--sprt-elo0".into(),
            "0".into(),
            "--sprt-elo1".into(),
            "10".into(),
            "--sprt-alpha".into(),
            "0.05".into(),
            "--sprt-beta".into(),
            "0.05".into(),
        ]);
        let args = parse_args(argv).expect("all four sprt flags must parse");
        assert_eq!(args.sprt_elo0, Some(0.0));
        assert_eq!(args.sprt_elo1, Some(10.0));
        assert_eq!(args.sprt_alpha, Some(0.05));
        assert_eq!(args.sprt_beta, Some(0.05));
    }

    #[test]
    fn parse_args_sprt_partial_set_rejected() {
        let mut argv = sprt_base_argv();
        argv.extend([
            "--sprt-elo0".into(),
            "0".into(),
            "--sprt-elo1".into(),
            "10".into(),
        ]);
        let err = parse_args(argv).unwrap_err();
        assert!(
            matches!(err, CliError::InvalidValue(_)),
            "expected InvalidValue when only elo0+elo1 set; got {err:?}"
        );
    }

    #[test]
    fn parse_args_sprt_alpha_out_of_range_rejected() {
        let mut argv = sprt_base_argv();
        argv.extend([
            "--sprt-elo0".into(),
            "0".into(),
            "--sprt-elo1".into(),
            "10".into(),
            "--sprt-alpha".into(),
            "0.0".into(),
            "--sprt-beta".into(),
            "0.05".into(),
        ]);
        let err = parse_args(argv).unwrap_err();
        assert!(matches!(err, CliError::InvalidValue(_)));

        let mut argv = sprt_base_argv();
        argv.extend([
            "--sprt-elo0".into(),
            "0".into(),
            "--sprt-elo1".into(),
            "10".into(),
            "--sprt-alpha".into(),
            "1.5".into(),
            "--sprt-beta".into(),
            "0.05".into(),
        ]);
        let err = parse_args(argv).unwrap_err();
        assert!(matches!(err, CliError::InvalidValue(_)));
    }

    #[test]
    fn parse_args_sprt_alpha_exactly_one_rejected() {
        // --sprt-alpha 1.0 is out of the open interval (0, 1) and must
        // be rejected. Pins the `< → <=` mutation on `a < 1.0` which
        // would accept 1.0 as if it were within (0, 1].
        let mut argv = sprt_base_argv();
        argv.extend([
            "--sprt-elo0".into(),
            "0".into(),
            "--sprt-elo1".into(),
            "10".into(),
            "--sprt-alpha".into(),
            "1.0".into(),
            "--sprt-beta".into(),
            "0.05".into(),
        ]);
        let err = parse_args(argv).unwrap_err();
        assert!(
            matches!(err, CliError::InvalidValue(_)),
            "--sprt-alpha 1.0 must be rejected (open interval); got Ok"
        );
    }

    #[test]
    fn parse_args_sprt_beta_out_of_range_rejected() {
        // Three boundary mutations on the sprt_beta guard `!(b > 0.0 && b < 1.0)`:
        //   1. `&& → ||` would accept b=1.5 (outer false iff b <= 0 AND b >= 1).
        //   2. `> → >=` would accept b=0.0 (>= 0.0 is true, allowing the zero boundary).
        //   3. `< → <=` would accept b=1.0 (the exact upper boundary).
        // Each subtest asserts the value is rejected. The beta guard mirrors
        // the alpha guard but has no corresponding test in the existing suite.

        // Case 1: b > 1.0 — caught by `&&→||` mutant.
        let mut argv = sprt_base_argv();
        argv.extend([
            "--sprt-elo0".into(),
            "0".into(),
            "--sprt-elo1".into(),
            "10".into(),
            "--sprt-alpha".into(),
            "0.05".into(),
            "--sprt-beta".into(),
            "1.5".into(),
        ]);
        let err = parse_args(argv).unwrap_err();
        assert!(
            matches!(err, CliError::InvalidValue(_)),
            "--sprt-beta 1.5 must be rejected; got Ok"
        );

        // Case 2: b = 0.0 — caught by `>→>=` mutant.
        let mut argv = sprt_base_argv();
        argv.extend([
            "--sprt-elo0".into(),
            "0".into(),
            "--sprt-elo1".into(),
            "10".into(),
            "--sprt-alpha".into(),
            "0.05".into(),
            "--sprt-beta".into(),
            "0.0".into(),
        ]);
        let err = parse_args(argv).unwrap_err();
        assert!(
            matches!(err, CliError::InvalidValue(_)),
            "--sprt-beta 0.0 must be rejected (open interval); got Ok"
        );

        // Case 3: b = 1.0 — caught by `<→<=` mutant.
        let mut argv = sprt_base_argv();
        argv.extend([
            "--sprt-elo0".into(),
            "0".into(),
            "--sprt-elo1".into(),
            "10".into(),
            "--sprt-alpha".into(),
            "0.05".into(),
            "--sprt-beta".into(),
            "1.0".into(),
        ]);
        let err = parse_args(argv).unwrap_err();
        assert!(
            matches!(err, CliError::InvalidValue(_)),
            "--sprt-beta 1.0 must be rejected (open interval); got Ok"
        );
    }

    #[test]
    fn parse_args_sprt_with_k0_override_rejected() {
        let mut argv = sprt_base_argv();
        argv.extend([
            "--sprt-elo0".into(),
            "0".into(),
            "--sprt-elo1".into(),
            "10".into(),
            "--sprt-alpha".into(),
            "0.05".into(),
            "--sprt-beta".into(),
            "0.05".into(),
            "--k0".into(),
            "1.0".into(),
        ]);
        let err = parse_args(argv).unwrap_err();
        assert!(
            matches!(err, CliError::InvalidValue(_)),
            "SPRT + non-default --k0 must be rejected; got {err:?}"
        );
    }

    #[test]
    fn parse_args_sprt_with_target_sigma_override_rejected() {
        let mut argv = sprt_base_argv();
        argv.extend([
            "--sprt-elo0".into(),
            "0".into(),
            "--sprt-elo1".into(),
            "10".into(),
            "--sprt-alpha".into(),
            "0.05".into(),
            "--sprt-beta".into(),
            "0.05".into(),
            "--target-sigma".into(),
            "50".into(),
        ]);
        let err = parse_args(argv).unwrap_err();
        assert!(
            matches!(err, CliError::InvalidValue(_)),
            "SPRT + non-default --target-sigma must be rejected; got {err:?}"
        );
    }

    #[test]
    fn parse_args_sprt_with_stop_window_override_rejected() {
        let mut argv = sprt_base_argv();
        argv.extend([
            "--sprt-elo0".into(),
            "0".into(),
            "--sprt-elo1".into(),
            "10".into(),
            "--sprt-alpha".into(),
            "0.05".into(),
            "--sprt-beta".into(),
            "0.05".into(),
            "--stop-window".into(),
            "50".into(),
        ]);
        let err = parse_args(argv).unwrap_err();
        assert!(
            matches!(err, CliError::InvalidValue(_)),
            "SPRT + non-default --stop-window must be rejected; got {err:?}"
        );
    }

    #[test]
    fn parse_args_sprt_with_rating_estimate_frozen_anchor_rejected() {
        // The rating-estimate frozen-anchor mode (--k0 0 --target-sigma 0)
        // is also incompatible with SPRT (the K-update consts diverge from
        // their defaults).
        let mut argv = sprt_base_argv();
        argv.extend([
            "--sprt-elo0".into(),
            "0".into(),
            "--sprt-elo1".into(),
            "10".into(),
            "--sprt-alpha".into(),
            "0.05".into(),
            "--sprt-beta".into(),
            "0.05".into(),
            "--k0".into(),
            "0".into(),
            "--target-sigma".into(),
            "0".into(),
        ]);
        let err = parse_args(argv).unwrap_err();
        assert!(
            matches!(err, CliError::InvalidValue(_)),
            "SPRT + frozen-anchor must be rejected; got {err:?}"
        );
    }

    #[test]
    fn parse_args_no_sprt_flags_means_classic_mode() {
        let args = parse_args(sprt_base_argv()).expect("classic mode parses");
        assert!(args.sprt_elo0.is_none());
        assert!(args.sprt_elo1.is_none());
        assert!(args.sprt_alpha.is_none());
        assert!(args.sprt_beta.is_none());
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
    fn parse_args_draw_score_positive_accepted() {
        // --draw-score 5 is a valid positive value. Pins the `< → >`
        // mutation on `if draw_score < 0` which would reject positive
        // values instead of only negative ones.
        let mut argv = base_argv();
        argv.extend(["--draw-score".into(), "5".into()]);
        let args = parse_args(argv).expect("--draw-score 5 must be accepted");
        assert_eq!(args.thresholds.draw_score, 5);
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
        let args = parse_args(argv).expect("--virtual-clock followed by another flag should parse");
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

    // -----------------------------------------------------------------------
    // 2.6.g — SPSA CLI parsing tests
    // -----------------------------------------------------------------------

    /// Minimal valid SPSA argv (no --max-games, no --initial-elo required).
    fn spsa_argv() -> Vec<String> {
        vec![
            "--engine".into(),
            "/bin/clawfish".into(),
            "--opponent".into(),
            "/bin/clawfish".into(),
            "--tc".into(),
            "1+0.05".into(),
            "--spsa".into(),
            "--spsa-iters".into(),
            "100".into(),
            "--spsa-param".into(),
            "Aspiration_K:200:0:1000:20:centik".into(),
            "--spsa-param".into(),
            "Aspiration_Min:25:0:1000:4:cp".into(),
            "--spsa-param".into(),
            "Aspiration_Max:250:0:2000:12:cp".into(),
        ]
    }

    #[test]
    fn parse_spsa_param_round_trips_centik() {
        let p = parse_spsa_param("Aspiration_K:200:0:1000:20:centik").expect("parse ok");
        assert_eq!(p.name, "Aspiration_K");
        assert!((p.theta - 200.0).abs() < 1e-9);
        assert!((p.lo - 0.0).abs() < 1e-9);
        assert!((p.hi - 1000.0).abs() < 1e-9);
        assert!((p.c_end - 20.0).abs() < 1e-9);
        assert_eq!(p.encoding, crate::elo_iterate::spsa::Encoding::CentiK);
    }

    #[test]
    fn parse_spsa_param_round_trips_cp() {
        let p = parse_spsa_param("Aspiration_Min:25:0:1000:4:cp").expect("parse ok");
        assert_eq!(p.name, "Aspiration_Min");
        assert!((p.theta - 25.0).abs() < 1e-9);
        assert_eq!(p.encoding, crate::elo_iterate::spsa::Encoding::IntCp);
    }

    #[test]
    fn parse_spsa_param_rejects_bad_enc() {
        let err = parse_spsa_param("P:100:0:500:10:badunit").unwrap_err();
        assert!(
            matches!(err, CliError::InvalidValue(_)),
            "bad ENC must produce InvalidValue, got {err:?}"
        );
    }

    #[test]
    fn parse_spsa_param_rejects_bad_numeric_field() {
        // THETA0 = "abc" → parse error
        let err = parse_spsa_param("P:abc:0:500:10:cp").unwrap_err();
        assert!(matches!(err, CliError::InvalidValue(_)));
    }

    #[test]
    fn parse_spsa_param_rejects_lo_gt_hi() {
        let err = parse_spsa_param("P:100:500:0:10:cp").unwrap_err();
        assert!(
            matches!(err, CliError::InvalidValue(_)),
            "LO > HI must be rejected"
        );
    }

    #[test]
    fn parse_spsa_param_rejects_theta_below_lo() {
        // theta0=5 < lo=10: out of box
        let err = parse_spsa_param("P:5:10:500:10:cp").unwrap_err();
        assert!(
            matches!(err, CliError::InvalidValue(_)),
            "theta0 below lo must be rejected; got {err:?}"
        );
    }

    #[test]
    fn parse_spsa_param_rejects_theta_above_hi() {
        // theta0=600 > hi=500: out of box
        let err = parse_spsa_param("P:600:0:500:10:cp").unwrap_err();
        assert!(
            matches!(err, CliError::InvalidValue(_)),
            "theta0 above hi must be rejected; got {err:?}"
        );
    }

    #[test]
    fn parse_spsa_param_rejects_zero_c_end() {
        let err = parse_spsa_param("P:100:0:500:0:cp").unwrap_err();
        assert!(
            matches!(err, CliError::InvalidValue(_)),
            "c_end=0 must be rejected"
        );
    }

    #[test]
    fn parse_spsa_param_rejects_wrong_field_count() {
        let err = parse_spsa_param("P:100:0:500").unwrap_err();
        assert!(matches!(err, CliError::InvalidValue(_)));
    }

    #[test]
    fn parse_args_spsa_accepts_valid_argv() {
        let args = parse_args(spsa_argv()).expect("valid SPSA argv must parse ok");
        assert!(args.spsa, "spsa flag must be set");
        assert_eq!(args.spsa_params.len(), 3);
        assert_eq!(args.spsa_iters, Some(100));
        assert_eq!(args.spsa_games_per_iter, 2, "default games-per-iter is 2");
        assert!(
            (args.spsa_r_end - 0.002).abs() < 1e-9,
            "default r_end is 0.002"
        );
    }

    #[test]
    fn parse_args_spsa_and_sprt_are_mutually_exclusive() {
        let mut argv = spsa_argv();
        argv.extend([
            "--sprt-elo0".into(),
            "0".into(),
            "--sprt-elo1".into(),
            "10".into(),
            "--sprt-alpha".into(),
            "0.05".into(),
            "--sprt-beta".into(),
            "0.05".into(),
        ]);
        let err = parse_args(argv).unwrap_err();
        assert!(
            matches!(err, CliError::InvalidValue(_)),
            "--spsa and --sprt-* must be mutually exclusive; got {err:?}"
        );
    }

    #[test]
    fn parse_args_spsa_and_k_update_flags_incompatible() {
        let mut argv = spsa_argv();
        argv.extend(["--k0".into(), "20".into()]);
        let err = parse_args(argv).unwrap_err();
        assert!(
            matches!(err, CliError::InvalidValue(_)),
            "--spsa and --k0 must be mutually exclusive; got {err:?}"
        );
    }

    #[test]
    fn parse_args_spsa_and_sigma_flags_incompatible() {
        let mut argv = spsa_argv();
        argv.extend(["--target-sigma".into(), "10".into()]);
        let err = parse_args(argv).unwrap_err();
        assert!(
            matches!(err, CliError::InvalidValue(_)),
            "--spsa and --target-sigma must be mutually exclusive; got {err:?}"
        );
    }

    #[test]
    fn parse_args_spsa_requires_spsa_iters() {
        let argv: Vec<String> = vec![
            "--engine".into(),
            "/bin/clawfish".into(),
            "--opponent".into(),
            "/bin/clawfish".into(),
            "--tc".into(),
            "1+0.05".into(),
            "--spsa".into(),
            "--spsa-param".into(),
            "P:100:0:500:10:cp".into(),
        ];
        let err = parse_args(argv).unwrap_err();
        assert!(
            matches!(err, CliError::MissingFlag(_)),
            "--spsa without --spsa-iters must fail; got {err:?}"
        );
    }

    #[test]
    fn parse_args_spsa_games_per_iter_must_be_even() {
        let mut argv = spsa_argv();
        argv.extend(["--spsa-games-per-iter".into(), "3".into()]);
        let err = parse_args(argv).unwrap_err();
        assert!(
            matches!(err, CliError::InvalidValue(_)),
            "odd --spsa-games-per-iter must be rejected"
        );
    }

    #[test]
    fn parse_args_spsa_games_per_iter_parsed() {
        let mut argv = spsa_argv();
        argv.extend(["--spsa-games-per-iter".into(), "4".into()]);
        let args = parse_args(argv).expect("parse ok");
        assert_eq!(args.spsa_games_per_iter, 4);
    }

    #[test]
    fn parse_args_spsa_tail_average_parsed() {
        let mut argv = spsa_argv();
        argv.extend(["--tail-average".into(), "10".into()]);
        let args = parse_args(argv).expect("parse ok");
        assert_eq!(args.spsa_tail_average, Some(10));
    }
}
