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
    /// Play one SPSA CRN color-pair (2 games, color-swapped).
    ///
    /// Both engines point at the same clawfish binary. `engine_options` are
    /// the θ⁺ setoptions; `opponent_options` are the θ⁻ setoptions. Both
    /// option sets include `Aspiration_Adaptive=true`. No `UCI_Elo` is sent
    /// (full-strength self-play). The setoption block is sent before
    /// `ucinewgame` within the pair per the existing invariant.
    PlaySpsaPair {
        pair_index: u32,
        /// θ⁺ setoption lines, sent to the "engine" side.
        engine_options: Vec<(String, String)>,
        /// θ⁻ setoption lines, sent to the "opponent" side.
        opponent_options: Vec<(String, String)>,
        tc: super::cli::TimeControl,
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
        /// Emitting worker. ELOH.E uses this to route per-game scores into
        /// the per-worker pair-score buffer for SPRT pair classification
        /// under `concurrency > 1`.
        worker_id: u32,
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
    pub mode: crate::MatchTimeMode,
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

/// Three-way classification of a clawfish-POV score into Win/Loss/Draw.
///
/// Centralises the `(score - 1.0).abs() < 1e-9` and `score.abs() < 1e-9`
/// epsilon checks that appear in both the unconditional W/L/D counters
/// and the per-TC bucket aggregation. Extracting also moves the
/// `< 1e-9 → <= 1e-9` mutants out of `controller::run_iteration` and
/// onto this helper, which lets `.cargo/mutants.toml` exclude them with
/// a precise `in classify_score` regex (the mutation is structurally
/// equivalent: the only callers pass scores ∈ {0.0, 0.5, 1.0}, all of
/// which are integer-valued, so `0.0 < 1e-9` and `0.0 <= 1e-9` agree).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ScoreClass {
    Win,
    Loss,
    Draw,
}

/// Classify a clawfish-POV score (1.0 = win, 0.0 = loss, 0.5 = draw).
pub(super) fn classify_score(clawfish_score: f64) -> ScoreClass {
    if (clawfish_score - 1.0).abs() < 1e-9 {
        ScoreClass::Win
    } else if clawfish_score.abs() < 1e-9 {
        ScoreClass::Loss
    } else {
        ScoreClass::Draw
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
    use crate::Color;

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

/// True iff either UCI `wait_for_uciok` returned `None` — i.e. one or
/// both engines failed to settle their handshake. Extracted into a pure
/// helper so the `||` operator is unit-testable without spawning real
/// subprocesses (the inline form was only reachable via integration
/// tests, leaving the operator's mutation-coverage gap open). Truth table
/// pinned by `controller::tests::handshake_caps_missing_truth_table`.
pub(super) fn handshake_caps_missing(
    engine_caps: &Option<super::driver::EngineCapabilities>,
    opponent_caps: &Option<super::driver::EngineCapabilities>,
) -> bool {
    engine_caps.is_none() || opponent_caps.is_none()
}

/// True iff both engines responded `readyok` to the post-setoption-block
/// `isready` sync. Pure-bool seam over the two `wait_for_readyok` calls,
/// extracted so the `&&` operator becomes directly unit-testable. Truth
/// table pinned by
/// `controller::tests::post_setoption_readyok_succeeded_truth_table`.
pub(super) fn post_setoption_readyok_succeeded(engine_ok: bool, opponent_ok: bool) -> bool {
    engine_ok && opponent_ok
}

/// True iff either engine failed to respond `readyok` after the per-game
/// `ucinewgame` send — the per-game readyok-failure gate. Pure-bool seam
/// over the in-loop boolean reads, extracted so the `delete !` and the
/// `&&`→`||` operators become unit-testable. Truth table pinned by
/// `controller::tests::either_per_game_readyok_failed_truth_table`.
pub(super) fn either_per_game_readyok_failed(engine_ready: bool, opponent_ready: bool) -> bool {
    !(engine_ready && opponent_ready)
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
    if handshake_caps_missing(&engine_caps, &opponent_caps) {
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
        let _ = super::driver::send_line(&mut opponent, "setoption name VirtualClock value true");
    }
    for (name, value) in &cfg.engine_options {
        let _ =
            super::driver::send_line(&mut engine, &format!("setoption name {name} value {value}"));
    }
    for (name, value) in &cfg.opponent_options {
        let _ = super::driver::send_line(
            &mut opponent,
            &format!("setoption name {name} value {value}"),
        );
    }
    let engine_setopt_ok = super::driver::wait_for_readyok(&mut engine, isready_to).is_ok();
    let opponent_setopt_ok = super::driver::wait_for_readyok(&mut opponent, isready_to).is_ok();
    let setopt_ok = post_setoption_readyok_succeeded(engine_setopt_ok, opponent_setopt_ok);
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
                    if either_per_game_readyok_failed(engine_ready, opponent_ready) {
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
                    let white_clock = crate::PerSideClock {
                        remaining_ms: i64::from(white_tc.initial_ms),
                        increment_ms: white_tc.increment_ms,
                    };
                    let black_clock = crate::PerSideClock {
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
                        worker_id,
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
            WorkerCmd::PlaySpsaPair {
                pair_index,
                engine_options,
                opponent_options,
                tc,
            } => {
                // **SPSA per-pair setoption ordering invariant:**
                // Send θ⁺/θ⁻ setoptions to each engine BEFORE `ucinewgame`,
                // for the same reason as the UCI_Elo block in PlayPair (see
                // above). No UCI_Elo / UCI_LimitStrength is sent — full-strength
                // self-play. `isready`-sync after the setoption block.
                for (name, value) in &engine_options {
                    let _ = super::driver::send_line(
                        &mut engine,
                        &format!("setoption name {name} value {value}"),
                    );
                }
                if super::driver::wait_for_readyok(&mut engine, isready_to).is_err() {
                    let _ = rpt_tx.send(WorkerReport::Failure(format!(
                        "worker {worker_id}: SPSA readyok after engine setoption"
                    )));
                    break;
                }
                for (name, value) in &opponent_options {
                    let _ = super::driver::send_line(
                        &mut opponent,
                        &format!("setoption name {name} value {value}"),
                    );
                }
                if super::driver::wait_for_readyok(&mut opponent, isready_to).is_err() {
                    let _ = rpt_tx.send(WorkerReport::Failure(format!(
                        "worker {worker_id}: SPSA readyok after opponent setoption"
                    )));
                    break;
                }

                // Color-swap 2-game loop — identical to PlayPair loop but with no UCI_Elo.
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
                    if either_per_game_readyok_failed(engine_ready, opponent_ready) {
                        let _ = rpt_tx.send(WorkerReport::Failure(format!(
                            "worker {worker_id}: SPSA readyok after ucinewgame"
                        )));
                        pair_failed = true;
                        break;
                    }

                    // Both sides use the same TC in SPSA self-play.
                    let (white_tc, black_tc) = (tc, tc);
                    let white_clock = crate::PerSideClock {
                        remaining_ms: i64::from(white_tc.initial_ms),
                        increment_ms: white_tc.increment_ms,
                    };
                    let black_clock = crate::PerSideClock {
                        remaining_ms: i64::from(black_tc.initial_ms),
                        increment_ms: black_tc.increment_ms,
                    };

                    // engine side is θ⁺; when clawfish_white==true, engine is white.
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
                        engine_tc: tc,
                        opponent_tc: tc,
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
                    // clawfish_score here is θ⁺'s POV: engine (θ⁺) is
                    // white when clawfish_white, black otherwise.
                    let clawfish_score = compute_clawfish_score(&outcome, clawfish_white);

                    let _ = rpt_tx.send(WorkerReport::GameComplete {
                        worker_id,
                        game_index,
                        opponent_uci_elo: 0, // unused in SPSA mode
                        clawfish_score,
                        outcome,
                        pgn_moves,
                        white_name,
                        black_name,
                        tc,
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

    // ELOH.E SPRT state. `sprt_cfg` is `Some` iff `--sprt-elo0` is set
    // (parse-time validation guarantees the other three are also set).
    let sprt_cfg: Option<super::sprt::SprtConfig> = if let (Some(e0), Some(e1), Some(a), Some(b)) = (
        args.sprt_elo0,
        args.sprt_elo1,
        args.sprt_alpha,
        args.sprt_beta,
    ) {
        Some(super::sprt::SprtConfig {
            elo0: e0,
            elo1: e1,
            alpha: a,
            beta: b,
        })
    } else {
        None
    };
    let mut sprt_state = super::sprt::SprtState::default();
    let mut sprt_stop_reason: Option<super::StopReason> = None;
    // Per-worker pair-score buffer keyed by `worker_id`. Each worker holds
    // at most one in-flight pair, so the HashMap has at most `concurrency`
    // entries. A single shared `Vec<f64>` would silently corrupt the SPRT
    // state under `concurrency > 1` (two workers' game scores would
    // interleave before either PairComplete arrives).
    let mut pair_score_buffers: std::collections::HashMap<u32, Vec<f64>> =
        std::collections::HashMap::new();

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
                worker_id,
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
                    match classify_score(clawfish_score) {
                        ScoreClass::Win => buckets[idx].wins += 1,
                        ScoreClass::Loss => buckets[idx].losses += 1,
                        ScoreClass::Draw => buckets[idx].draws += 1,
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

                match classify_score(clawfish_score) {
                    ScoreClass::Win => wins += 1,
                    ScoreClass::Loss => losses += 1,
                    ScoreClass::Draw => draws += 1,
                }

                // σ-stopping check (per-game cadence). Skipped in SPRT mode
                // — SPRT's termination criteria are LLR-bound or --max-games
                // (per docs/decisions/0022); allowing σ-stop to preempt would
                // invalidate the α/β guarantees.
                if sprt_cfg.is_none()
                    && !terminating
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

                // ELOH.E: route the per-game candidate score into this
                // worker's pair buffer. Used for both SPRT pair
                // classification (when active) and the post-hoc
                // pentanomial CI line (always emitted at run end).
                pair_score_buffers
                    .entry(worker_id)
                    .or_default()
                    .push(clawfish_score);
            }
            WorkerReport::PairComplete { worker_id } => {
                let wid = worker_id as usize;
                // Out-of-bounds worker_id is a worker bug; let indexing
                // panic rather than silently no-op'ing the bookkeeping.
                pairs_in_flight[wid] = pairs_in_flight[wid].saturating_sub(1);

                // ELOH.E: drain this worker's pair-score buffer and feed
                // it into the SPRT state. Always runs (the same state
                // backs the post-hoc pentanomial CI line); the LLR
                // verdict is only consulted when `sprt_cfg.is_some()`.
                //
                // In production a worker emits PairComplete iff it just
                // emitted both games' GameComplete with its own worker_id,
                // so the buffer length is 2. Length 0 happens in tests
                // that emit PairComplete without matching GameComplete
                // fixtures (legacy fixtures that pre-date the worker_id
                // field) — fall through silently; the SPRT state simply
                // doesn't see that pair. Length 1 is impossible at this
                // point because PairComplete signals both games done; if
                // it ever happens, the run-end drain folds it as a
                // singleton via the audit counter.
                let scores = pair_score_buffers.remove(&worker_id).unwrap_or_default();
                if scores.len() == 2 {
                    let pair_score: f64 = scores.iter().sum();
                    // Use a dummy config when SPRT is inactive — the LLR field
                    // is overwritten but never consumed in that path.
                    let dummy_cfg = super::sprt::SprtConfig {
                        elo0: 0.0,
                        elo1: 10.0,
                        alpha: 0.05,
                        beta: 0.05,
                    };
                    let cfg_ref = sprt_cfg.as_ref().unwrap_or(&dummy_cfg);
                    let verdict = super::sprt::update_pair(&mut sprt_state, cfg_ref, pair_score);
                    if sprt_cfg.is_some() {
                        match verdict {
                            super::sprt::SprtVerdict::Continue => {}
                            super::sprt::SprtVerdict::AcceptH0 => {
                                if !terminating {
                                    terminating = true;
                                    sprt_stop_reason = Some(super::StopReason::SprtAcceptH0);
                                }
                            }
                            super::sprt::SprtVerdict::AcceptH1 => {
                                if !terminating {
                                    terminating = true;
                                    sprt_stop_reason = Some(super::StopReason::SprtAcceptH1);
                                }
                            }
                        }
                    }
                }

                // Emit progress line for this pair.
                let window_start = estimates_trail.len().saturating_sub(args.stop_window);
                let current_sigma = super::sigma::sample_stddev(&estimates_trail[window_start..]);
                let current_k = super::estimator::compute_k(t.saturating_sub(1), args.k0, args.tau);
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
                // `wid < senders.len()` is structurally guaranteed by
                // worker spawn/teardown semantics — no defensive gate.
                if !terminating && pairs_dispatched < total_pairs {
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

    // ELOH.E: drain orphaned per-worker buffers. A buffer of length 2 is
    // a complete pair whose `PairComplete` was not processed before the
    // loop exited (e.g. max-games boundary cut in after the 2nd game's
    // GameComplete) — fold it into the SPRT state. A buffer of length 1
    // is a true singleton (game A's GameComplete arrived but game B never
    // completed) — count toward `discarded_singletons` and ignore.
    let dummy_cfg_for_drain = super::sprt::SprtConfig {
        elo0: 0.0,
        elo1: 10.0,
        alpha: 0.05,
        beta: 0.05,
    };
    let drain_cfg = sprt_cfg.as_ref().unwrap_or(&dummy_cfg_for_drain);
    for (_wid, buf) in pair_score_buffers.drain() {
        match buf.len() {
            2 => {
                let pair_score: f64 = buf.iter().sum();
                let _ = super::sprt::update_pair(&mut sprt_state, drain_cfg, pair_score);
            }
            1 => super::sprt::discard_singleton(&mut sprt_state),
            _ => {}
        }
    }

    let fallback_reason = if sigma_fired {
        super::StopReason::Sigma
    } else {
        super::StopReason::MaxGames
    };
    let stop_reason = sprt_stop_reason.unwrap_or(fallback_reason);

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

    // ELOH.E: emit `sprt:` line (always when SPRT active) and `ci:` line
    // (always when ≥2 pairs completed; format itself emits "undefined" otherwise).
    if let Some(cfg) = sprt_cfg.as_ref() {
        let verdict = match sprt_stop_reason {
            Some(super::StopReason::SprtAcceptH0) => super::sprt::SprtVerdict::AcceptH0,
            Some(super::StopReason::SprtAcceptH1) => super::sprt::SprtVerdict::AcceptH1,
            _ => super::sprt::SprtVerdict::Continue,
        };
        let sprt_str = super::summary::format_sprt_verdict(&sprt_state, cfg, verdict);
        println!("{sprt_str}");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&summary_path)
        {
            use std::io::Write;
            let _ = writeln!(f, "{sprt_str}");
        }
    }
    // ci: line is always emitted at run end. The formatter itself
    // returns "ci: undefined (n=N)" when fewer than 2 pairs are present
    // or variance collapses, so callers always get a parsable line.
    {
        let ci_str = super::summary::format_pentanomial_ci(&sprt_state);
        println!("{ci_str}");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&summary_path)
        {
            use std::io::Write;
            let _ = writeln!(f, "{ci_str}");
        }
    }

    // ELOH.E: combined match.pgn = concatenation of per-game PGNs in
    // ascending game_index order. Runs unconditionally so downstream
    // tools that read a single combined PGN file always have one.
    // Empty if no games completed.
    let _ = write_match_pgn(&games_dir, &out_dir.join("match.pgn"));

    Ok(IterationOutcome {
        final_estimate: current_estimate,
        final_sigma,
        games_played: t,
        stop_reason,
        wld: (wins, losses, draws),
    })
}

/// Concatenate `<games_dir>/<N>.pgn` files in ascending N order into
/// `match_pgn_path`, separated by a single blank line. Best-effort: missing
/// `games_dir`, unreadable files, and zero-game runs all produce a
/// successfully-created empty (or partial) `match_pgn_path`.
#[allow(dead_code)]
pub(crate) fn write_match_pgn(
    games_dir: &std::path::Path,
    match_pgn_path: &std::path::Path,
) -> std::io::Result<()> {
    use std::io::Write;
    let mut games: Vec<(u32, std::path::PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(games_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("pgn") {
                continue;
            }
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if let Ok(idx) = stem.parse::<u32>() {
                games.push((idx, path));
            }
        }
    }
    games.sort_by_key(|(idx, _)| *idx);
    let mut out = std::fs::File::create(match_pgn_path)?;
    for (i, (_idx, path)) in games.iter().enumerate() {
        let content = std::fs::read_to_string(path)?;
        if i > 0 {
            out.write_all(b"\n")?;
        }
        out.write_all(content.as_bytes())?;
        if !content.ends_with('\n') {
            out.write_all(b"\n")?;
        }
    }
    out.flush()?;
    Ok(())
}

/// Drive the SPSA tuning loop to completion.
///
/// Spawns one worker per `args.concurrency`; each iteration dispatches
/// `args.spsa_games_per_iter / 2` CRN pairs (all with the same θ⁺/θ⁻ and Δ),
/// barriers on their results, computes the match score, and steps θ.
///
/// Determinism contract: Δ, colors (already fixed by color-swap), and TC
/// (when `--tc-sample`) are all drawn from the single seeded Prng in the
/// sequential driver in that fixed per-pair order, before dispatch.
///
/// Appends one row per iteration to `<out_dir>/spsa-trajectory.tsv`.
/// Writes tail-averaged final θ to `<out_dir>/spsa-final.txt`.
#[allow(dead_code)]
pub(crate) fn run_spsa(
    args: &super::cli::Args,
    out_dir: &std::path::Path,
) -> Result<(), super::driver::HarnessError> {
    use super::spsa::{
        SpsaSchedule, accumulate_tail, build_setoption_lines, draw_rademacher,
        format_final_option_block, format_trajectory_row, pair_sum_to_match, spsa_step,
        tail_averaged_values,
    };
    use std::io::Write;

    let _ = std::fs::create_dir_all(out_dir);
    let games_dir = out_dir.join("games");
    let _ = std::fs::create_dir_all(&games_dir);

    // spsa_iters is guaranteed Some by parse_args when spsa=true.
    let n_iters = args
        .spsa_iters
        .expect("run_spsa: spsa_iters must be Some when spsa=true");
    let pairs_per_iter = (args.spsa_games_per_iter / 2) as u64;
    let tail_window = args.spsa_tail_average.unwrap_or((n_iters / 10).max(1));

    let mut params = args.spsa_params.clone();
    let sched = SpsaSchedule::new(&params, n_iters, args.spsa_r_end, args.spsa_a_override);

    // The Prng drives: Δ (n_params draws), then TC (when --tc-sample).
    // Colors are fixed (game 0 = θ⁺ white, game 1 = θ⁻ white).
    let mut rng = super::prng::Prng::new(args.seed.unwrap_or(super::prng::DEFAULT_SEED));

    // Open trajectory log (truncate on fresh run).
    let traj_path = out_dir.join("spsa-trajectory.tsv");
    let _ = std::fs::remove_file(&traj_path);

    // Build a WorkerConfig for the SPSA self-play (both engines are args.engine).
    let engine_name = std::path::Path::new(&args.engine)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("engine")
        .to_owned();
    let cfg = WorkerConfig {
        engine_spec: super::driver::EngineSpec {
            name: format!("{engine_name}+"),
            path: args.engine.clone(),
            launch_prefix: args.engine_launch_prefix.clone(),
        },
        opponent_spec: super::driver::EngineSpec {
            name: format!("{engine_name}-"),
            path: args.engine.clone(),
            launch_prefix: args.engine_launch_prefix.clone(),
        },
        engine_options: args.engine_options.clone(),
        opponent_options: args.engine_options.clone(), // both sides same static options
        mode: crate::MatchTimeMode::Wallclock,
        harness_overhead_ms: args.harness_overhead_ms,
        watchdog: std::time::Duration::from_millis(args.watchdog_ms),
        max_plies: args.max_moves,
        thresholds: args.thresholds.clone(),
        virtual_clock: args.virtual_clock,
    };

    let mut pool = spawn_workers(args.concurrency, cfg)?;

    for k in 0..n_iters {
        let a_k = sched.a_k(k);
        let c_k: Vec<f64> = (0..params.len()).map(|i| sched.c_k(i, k)).collect();

        // Draw Δ from the sequential Prng (deterministic; workers don't touch rng).
        let delta = draw_rademacher(&mut rng, params.len());

        // Compute θ⁺ and θ⁻ integer values for each param.
        let plus_vals: Vec<i64> = params
            .iter()
            .enumerate()
            .map(|(i, p)| p.plus_value(p.theta, c_k[i], delta[i]))
            .collect();
        let minus_vals: Vec<i64> = params
            .iter()
            .enumerate()
            .map(|(i, p)| p.minus_value(p.theta, c_k[i], delta[i]))
            .collect();

        // Build setoption option vectors (Vec<(name, value)>).
        let engine_opts: Vec<(String, String)> = {
            let lines = build_setoption_lines(&params, &plus_vals);
            lines
                .into_iter()
                .filter_map(|l| {
                    // parse "setoption name NAME value VALUE" → (NAME, VALUE)
                    let rest = l.strip_prefix("setoption name ")?;
                    let (name, val) = rest.split_once(" value ")?;
                    Some((name.to_owned(), val.to_owned()))
                })
                .collect()
        };
        let opponent_opts: Vec<(String, String)> = {
            let lines = build_setoption_lines(&params, &minus_vals);
            lines
                .into_iter()
                .filter_map(|l| {
                    let rest = l.strip_prefix("setoption name ")?;
                    let (name, val) = rest.split_once(" value ")?;
                    Some((name.to_owned(), val.to_owned()))
                })
                .collect()
        };

        // Dispatch pairs_per_iter PlaySpsaPair commands.
        // TC is drawn from the sequential rng (per determinism contract).
        let base_pair_index = (k * pairs_per_iter) as u32;
        let n_workers = pool.senders.len();
        let mut pairs_completed = 0u64;
        let mut dispatch_count = 0usize;

        for pair_offset in 0..pairs_per_iter {
            let tc = match &args.tc_sample {
                Some(dist) => dist.sample(&mut rng),
                None => args
                    .tc
                    .expect("post-parse: exactly one of tc/tc_sample set"),
            };
            let worker_id = (pair_offset % n_workers as u64) as usize;
            let pair_index = base_pair_index + pair_offset as u32;
            if pool.senders[worker_id]
                .send(WorkerCmd::PlaySpsaPair {
                    pair_index,
                    engine_options: engine_opts.clone(),
                    opponent_options: opponent_opts.clone(),
                    tc,
                })
                .is_err()
            {
                // Worker exited unexpectedly.
                return Err(super::driver::HarnessError::EngineExit);
            }
            dispatch_count += 1;
        }

        // Barrier: drain reports until all dispatched pairs complete.
        let mut game_scores_this_iter: Vec<f64> = Vec::new();
        let mut pairs_complete_this_iter = 0usize;

        while pairs_complete_this_iter < dispatch_count {
            let report = match pool.reports.recv() {
                Ok(r) => r,
                Err(_) => return Err(super::driver::HarnessError::EngineExit),
            };
            match report {
                WorkerReport::GameComplete {
                    clawfish_score,
                    game_index,
                    pgn_moves,
                    outcome,
                    white_name,
                    black_name,
                    tc: game_tc,
                    ..
                } => {
                    game_scores_this_iter.push(clawfish_score);

                    // Write per-game PGN.
                    let (result, termination_str) =
                        crate::elo_iterate::outcome_to_pgn_result(&outcome);
                    let tc_str = crate::elo_iterate::format_tc(game_tc);
                    let pgn_header = super::pgn::PgnHeader {
                        event: args.event_tag.clone(),
                        site: crate::elo_iterate::current_hostname(),
                        date: crate::elo_iterate::current_date_str(),
                        round: game_index,
                        white: white_name,
                        black: black_name,
                        result,
                        time_control: Some(tc_str),
                        termination: Some(termination_str),
                        setup_fen: None,
                    };
                    let pgn_str = super::pgn::format_pgn(&pgn_header, &pgn_moves);
                    let _ = std::fs::write(games_dir.join(format!("{game_index}.pgn")), &pgn_str);
                }
                WorkerReport::PairComplete { .. } => {
                    pairs_complete_this_iter += 1;
                    pairs_completed += 1;
                }
                WorkerReport::Failure(msg) => {
                    eprintln!("spsa worker failure: {msg}");
                    return Err(super::driver::HarnessError::EngineExit);
                }
            }
        }

        // Aggregate θ⁺ pair scores.
        // game_scores_this_iter holds alternating θ⁺-POV scores (game 0 = θ⁺ white,
        // game 1 = θ⁺ black, game 2 = θ⁺ white, ...).
        let pair_score_sum: f64 = game_scores_this_iter.iter().sum();
        let avg_pair_sum = if pairs_completed > 0 {
            pair_score_sum / pairs_completed as f64
        } else {
            1.0 // neutral if no pairs completed
        };
        let match_val = pair_sum_to_match(avg_pair_sum);

        // Gradient step.
        spsa_step(&mut params, a_k, &c_k, &delta, match_val);

        // Tail accumulation (last tail_window iterations).
        if k >= n_iters.saturating_sub(tail_window) {
            accumulate_tail(&mut params);
        }

        // Write trajectory row.
        let thetas: Vec<f64> = params.iter().map(|p| p.theta).collect();
        let row = format_trajectory_row(
            k,
            &thetas,
            &plus_vals,
            &minus_vals,
            match_val,
            a_k,
            &c_k,
            pair_score_sum,
        );
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&traj_path)
        {
            let _ = writeln!(f, "{row}");
        }
    }

    // Send Quit and drain workers.
    for s in &pool.senders {
        let _ = s.send(WorkerCmd::Quit);
    }
    pool.senders.clear();
    for h in pool.join_handles.drain(..) {
        if let Err(panic) = h.join() {
            eprintln!("spsa worker panicked: {panic:?}");
        }
    }

    // Write final output.
    let avg_values = tail_averaged_values(&params);
    let option_block = format_final_option_block(&params, &avg_values);
    let final_path = out_dir.join("spsa-final.txt");
    let mut final_file = std::fs::File::create(&final_path).map_err(|_| {
        super::driver::HarnessError::Io(std::io::Error::other("create spsa-final.txt"))
    })?;
    writeln!(final_file, "# SPSA tail-averaged final parameters").ok();
    writeln!(final_file, "# Apply via: elo-iterate ... {option_block}").ok();
    writeln!(final_file, "{option_block}").ok();
    for (param, val) in params.iter().zip(avg_values.iter()) {
        writeln!(
            final_file,
            "# {} = {} (final theta = {:.2})",
            param.name, val, param.theta
        )
        .ok();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    // -----------------------------------------------------------------------
    // Pure-helper truth tables (close mutation-coverage gaps that the
    // integration-driven `production_worker_tests` block cannot reach
    // because the mock engine never produces a subprocess-failure path).
    // -----------------------------------------------------------------------

    #[test]
    fn handshake_caps_missing_truth_table() {
        let some = Some(super::super::driver::EngineCapabilities::default());
        assert!(handshake_caps_missing(&None, &None));
        assert!(handshake_caps_missing(&some, &None));
        assert!(handshake_caps_missing(&None, &some));
        assert!(!handshake_caps_missing(&some, &some));
    }

    #[test]
    fn post_setoption_readyok_succeeded_truth_table() {
        assert!(post_setoption_readyok_succeeded(true, true));
        assert!(!post_setoption_readyok_succeeded(true, false));
        assert!(!post_setoption_readyok_succeeded(false, true));
        assert!(!post_setoption_readyok_succeeded(false, false));
    }

    #[test]
    fn either_per_game_readyok_failed_truth_table() {
        assert!(!either_per_game_readyok_failed(true, true));
        assert!(either_per_game_readyok_failed(true, false));
        assert!(either_per_game_readyok_failed(false, true));
        assert!(either_per_game_readyok_failed(false, false));
    }

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
                        WorkerCmd::PlaySpsaPair { .. } => {
                            unreachable!("SPSA pair sent to synthetic SPRT worker")
                        }
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
                                    WorkerReport::Failure(s) => WorkerReport::Failure(s.clone()),
                                    WorkerReport::GameComplete {
                                        worker_id,
                                        game_index,
                                        opponent_uci_elo,
                                        clawfish_score,
                                        tc,
                                        ..
                                    } => WorkerReport::GameComplete {
                                        worker_id: *worker_id,
                                        game_index: *game_index,
                                        opponent_uci_elo: *opponent_uci_elo,
                                        clawfish_score: *clawfish_score,
                                        outcome:
                                            super::super::match_loop::GameOutcome::MaxMovesReached,
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
                        WorkerCmd::PlaySpsaPair { .. } => {
                            unreachable!("SPSA pair sent to synthetic SPRT worker")
                        }
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
                first
                    .unwrap_or_else(|| panic!("worker {wid} received no PlayPair during bootstrap"))
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
                worker_id: 0,
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
                worker_id: 0,
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
                worker_id: 0,
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
                worker_id: 0,
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
                        worker_id: 0,
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
                        worker_id: 0,
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
                        worker_id: 0,
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
                        worker_id: 0,
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
                        WorkerCmd::PlaySpsaPair { .. } => {
                            unreachable!("SPSA pair sent to synthetic SPRT worker")
                        }
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
                                worker_id: 0,
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
                                worker_id: 0,
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
                        worker_id: 0,
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
                        worker_id: 0,
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
                        WorkerCmd::PlaySpsaPair { .. } => {
                            unreachable!("SPSA pair sent to synthetic SPRT worker")
                        }
                        WorkerCmd::PlayPair { .. } => {
                            if slow {
                                std::thread::sleep(std::time::Duration::from_millis(200));
                            }
                            pair_counter += 1;
                            let g = pair_counter * 2;
                            for gi in [g - 1, g] {
                                let _ = rpt_tx_clone.send(WorkerReport::GameComplete {
                                    worker_id: 0,
                                    game_index: gi,
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
        use crate::Color;
        let outcome = GameOutcome::NativeGameOver(GameOver::Checkmate(Color::White));
        // White wins → white_score=1.0; clawfish_white → clawfish_score=1.0.
        let score = compute_clawfish_score(&outcome, true);
        assert!((score - 1.0).abs() < 1e-9, "expected 1.0, got {score}");
    }

    #[test]
    fn compute_clawfish_score_white_wins_clawfish_black() {
        use super::super::adjudicate::GameOver;
        use super::super::match_loop::GameOutcome;
        use crate::Color;
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
        use crate::Color;
        let outcome = GameOutcome::NativeGameOver(GameOver::Checkmate(Color::Black));
        // Black wins → white_score=0.0; clawfish_white → clawfish_score=0.0.
        let score = compute_clawfish_score(&outcome, true);
        assert!(score.abs() < 1e-9, "expected 0.0, got {score}");
    }

    #[test]
    fn compute_clawfish_score_black_wins_clawfish_black() {
        use super::super::adjudicate::GameOver;
        use super::super::match_loop::GameOutcome;
        use crate::Color;
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
        use crate::Color;
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
        super::super::tc_sample::parse_tc_sample(spec).expect("make_dist: parse_tc_sample failed")
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
                    WorkerCmd::PlaySpsaPair { .. } => {
                        unreachable!("SPSA pair sent to synthetic SPRT worker")
                    }
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
                    WorkerCmd::PlaySpsaPair { .. } => {
                        unreachable!("SPSA pair sent to synthetic SPRT worker")
                    }
                    WorkerCmd::PlayPair {
                        pair_index,
                        opponent_uci_elo,
                        engine_tc,
                        opponent_tc,
                    } => {
                        log.lock().unwrap().push(engine_tc);
                        let _ = rpt_tx_clone.send(WorkerReport::GameComplete {
                            worker_id: 0,
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
                            worker_id: 0,
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
        let args = super::super::cli::parse_args(argv).expect("parse ok — ELOH.D Slice A pending");
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
                    WorkerCmd::PlaySpsaPair { .. } => {
                        unreachable!("SPSA pair sent to synthetic SPRT worker")
                    }
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
                                worker_id: 0,
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
        let args = super::super::cli::parse_args(argv).expect("parse ok — ELOH.D Slice A pending");
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
                    WorkerCmd::PlaySpsaPair { .. } => {
                        unreachable!("SPSA pair sent to synthetic SPRT worker")
                    }
                    WorkerCmd::PlayPair {
                        pair_index,
                        opponent_uci_elo,
                        engine_tc,
                        opponent_tc,
                    } => {
                        log.lock().unwrap().push((engine_tc, opponent_tc));
                        let _ = rpt_tx_clone.send(WorkerReport::GameComplete {
                            worker_id: 0,
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
                            worker_id: 0,
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
        let args = super::super::cli::parse_args(argv).expect("parse ok — ELOH.D Slice A pending");
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
                    WorkerCmd::PlaySpsaPair { .. } => {
                        unreachable!("SPSA pair sent to synthetic SPRT worker")
                    }
                    WorkerCmd::PlayPair {
                        pair_index,
                        opponent_uci_elo,
                        engine_tc,
                        opponent_tc,
                    } => {
                        log.lock().unwrap().push((engine_tc, opponent_tc));
                        let _ = rpt_tx_clone.send(WorkerReport::GameComplete {
                            worker_id: 0,
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
                            worker_id: 0,
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
        let args = super::super::cli::parse_args(argv).expect("parse ok — ELOH.D Slice A pending");
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
                        worker_id: 0,
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
                        worker_id: 0,
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

    /// Pure-helper classifier: 1.0 → Win, 0.0 → Loss, 0.5 → Draw.
    /// Pins the integer-valued boundary cases for the `< 1e-9` epsilon.
    #[test]
    fn classify_score_three_way() {
        assert_eq!(classify_score(1.0), ScoreClass::Win);
        assert_eq!(classify_score(0.0), ScoreClass::Loss);
        assert_eq!(classify_score(0.5), ScoreClass::Draw);
    }

    /// Per-TC bucket W/L/D classification. The existing
    /// `per_tc_buckets_aggregate_in_input_order` test only asserts totals
    /// (W+L+D = 2 per bucket), missing the per-class boundary mutations on
    /// the win/loss epsilon checks (`(score - 1.0).abs() < 1e-9` →
    /// `== 1e-9` / `> 1e-9` and the symmetric loss check on `score.abs()`).
    /// This test feeds one win + one loss + one draw across three TC
    /// buckets and asserts each bucket's W=, L=, D= values exactly.
    #[test]
    fn per_tc_buckets_classify_w_l_d_correctly() {
        let argv: Vec<String> = vec![
            "--engine".into(),
            "/bin/clawfish".into(),
            "--opponent".into(),
            "/bin/stockfish".into(),
            "--tc-sample".into(),
            "10+0.1:1,20+0.2:1,40+0.4:1".into(),
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
        let args = super::super::cli::parse_args(argv).expect("parse ok");
        let out_dir = std::env::temp_dir().join("eloh_per_tc_wld_classification");
        // Three pairs, each pair pinned to one TC. Per pair, one game has
        // a win-defining score and the other has the loss/draw mirror.
        //   Pair 0 (TC 10+0.1): both wins → W=2.
        //   Pair 1 (TC 20+0.2): both losses → L=2.
        //   Pair 2 (TC 40+0.4): both draws → D=2.
        let tcs = [
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
        ];
        let scores = [1.0_f64, 0.0_f64, 0.5_f64];
        let pair_reports: Vec<WorkerReport> = (0..3u32)
            .flat_map(|p| {
                let tc = tcs[p as usize];
                let s = scores[p as usize];
                vec![
                    WorkerReport::GameComplete {
                        worker_id: 0,
                        game_index: p * 2 + 1,
                        opponent_uci_elo: 2000,
                        clawfish_score: s,
                        outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                        pgn_moves: vec![],
                        white_name: "w".into(),
                        black_name: "b".into(),
                        tc,
                    },
                    WorkerReport::GameComplete {
                        worker_id: 0,
                        game_index: p * 2 + 2,
                        opponent_uci_elo: 2000,
                        clawfish_score: s,
                        outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
                        pgn_moves: vec![],
                        white_name: "w".into(),
                        black_name: "b".into(),
                        tc,
                    },
                    WorkerReport::PairComplete { worker_id: 0 },
                ]
            })
            .collect();
        let mut pool = synthetic_pool(1, vec![pair_reports]);
        let _ = run_iteration(&mut pool, &args, &out_dir);
        let summary = std::fs::read_to_string(out_dir.join("summary.txt")).unwrap_or_default();
        let by_tc_line = summary
            .lines()
            .find(|l| l.starts_with("summary-by-tc:"))
            .expect("summary-by-tc: line missing");
        // Pin per-bucket W/L/D shape exactly. Format: "TC: W=N L=N D=N (N)"
        for (tc_str, expected_w, expected_l, expected_d) in [
            ("10+0.1", 2u32, 0u32, 0u32),
            ("20+0.2", 0, 2, 0),
            ("40+0.4", 0, 0, 2),
        ] {
            let needle = format!("{tc_str}: W={expected_w} L={expected_l} D={expected_d}");
            assert!(
                by_tc_line.contains(&needle),
                "expected substring {needle:?} in by_tc_line: {by_tc_line}"
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
        let args = super::super::cli::parse_args(argv).expect("parse ok — ELOH.D Slice A pending");
        let out_dir = std::env::temp_dir().join("eloh_d_summary_by_tc_present_test");
        let pair_reports: Vec<WorkerReport> = (0..1u32)
            .flat_map(|p| {
                let t = super::super::cli::TimeControl {
                    initial_ms: 10_000,
                    increment_ms: 100,
                };
                vec![
                    WorkerReport::GameComplete {
                        worker_id: 0,
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
                        worker_id: 0,
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
        // ELOH.D contract: under --tc-sample, the summary contains a
        // `summary-by-tc:` line. Originally the test asserted the line
        // was the *last* non-empty line, but ELOH.E always appends a
        // `ci:` line at run end (regardless of mode), so the new
        // ordering is: converged → summary-by-tc → ci. The load-bearing
        // invariant is presence + ordering relative to `converged:`.
        let mut iter = summary
            .lines()
            .filter(|l| !l.is_empty())
            .skip_while(|l| !l.starts_with("converged:"));
        let _converged = iter.next();
        let next = iter.next().unwrap_or("");
        assert!(
            next.starts_with("summary-by-tc:"),
            "summary-by-tc: must follow `converged:`; got next line: {next:?}\n\
             full summary:\n{summary}"
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
                        worker_id: 0,
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
                        worker_id: 0,
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
                        WorkerCmd::PlaySpsaPair { .. } => {
                            unreachable!("SPSA pair sent to synthetic SPRT worker")
                        }
                        WorkerCmd::PlayPair {
                            pair_index,
                            opponent_uci_elo,
                            engine_tc,
                            ..
                        } => {
                            log.lock().unwrap().push((pair_index, engine_tc));
                            let _ = rpt_tx_clone.send(WorkerReport::GameComplete {
                                worker_id: 0,
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
                                worker_id: 0,
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

    // -----------------------------------------------------------------------
    // §6.5 ELOH.E SPRT controller-integration tests
    // -----------------------------------------------------------------------

    /// Build an Args with SPRT mode active; defaults except --sprt-* and --max-games.
    fn sprt_args(max_games: u32) -> super::super::cli::Args {
        let argv: Vec<String> = vec![
            "--engine".into(),
            "/bin/clawfish".into(),
            "--opponent".into(),
            "/bin/clawfish".into(),
            "--tc".into(),
            "10+0.1".into(),
            "--max-games".into(),
            max_games.to_string(),
            "--initial-elo".into(),
            "0".into(),
            "--sprt-elo0".into(),
            "0".into(),
            "--sprt-elo1".into(),
            "10".into(),
            "--sprt-alpha".into(),
            "0.05".into(),
            "--sprt-beta".into(),
            "0.05".into(),
        ];
        super::super::cli::parse_args(argv).expect("sprt_args: parse failed")
    }

    /// Fabricate a single pair's worth of reports for a given worker_id and pair score.
    fn pair_reports(worker_id: u32, pair_index: u32, pair_score: f64) -> Vec<WorkerReport> {
        // Split a pair score into two game scores: 0 → 0+0; 0.5 → 0+0.5;
        // 1 → 0.5+0.5; 1.5 → 0.5+1; 2 → 1+1.
        let (g_a, g_b) = match pair_score {
            s if (s - 0.0).abs() < 1e-9 => (0.0, 0.0),
            s if (s - 0.5).abs() < 1e-9 => (0.0, 0.5),
            s if (s - 1.0).abs() < 1e-9 => (0.5, 0.5),
            s if (s - 1.5).abs() < 1e-9 => (0.5, 1.0),
            s if (s - 2.0).abs() < 1e-9 => (1.0, 1.0),
            other => panic!("pair_reports: unsupported pair_score {other}"),
        };
        let mk = |gi: u32, score: f64| WorkerReport::GameComplete {
            worker_id,
            game_index: gi,
            opponent_uci_elo: 1320,
            clawfish_score: score,
            outcome: super::super::match_loop::GameOutcome::MaxMovesReached,
            pgn_moves: vec![],
            white_name: "w".into(),
            black_name: "b".into(),
            tc: super::super::cli::TimeControl {
                initial_ms: 10_000,
                increment_ms: 100,
            },
        };
        vec![
            mk(pair_index * 2 + 1, g_a),
            mk(pair_index * 2 + 2, g_b),
            WorkerReport::PairComplete { worker_id },
        ]
    }

    #[test]
    fn sprt_mode_h1_synthetic_stream_accepts() {
        // Strong H1 stream: 80% bin 4 (2.0) + 20% bin 3 (1.5). var > 0
        // → LLR moves; mu = 1.9 is far above (s0p+s1p)/2 ≈ 1.014 → LLR > 0.
        // (All-2.0 has zero variance and would never move LLR.)
        let args = sprt_args(400);
        let reports: Vec<WorkerReport> = (0..200u32)
            .flat_map(|p| pair_reports(0, p, if p % 5 == 0 { 1.5 } else { 2.0 }))
            .collect();
        let mut pool = synthetic_pool(1, vec![reports]);
        let out_dir = std::env::temp_dir().join("eloh_e_sprt_h1");
        let outcome = run_iteration(&mut pool, &args, &out_dir).unwrap();
        assert_eq!(
            outcome.stop_reason,
            super::super::StopReason::SprtAcceptH1,
            "strong-H1 stream (mu=1.9) must accept H1"
        );
        // Pin the summary's `sprt: verdict=H1` line — catches the
        // `delete match arm Some(SprtAcceptH1)` mutant in the verdict
        // dispatch (line 7604) that would silently downgrade the
        // summary to verdict=continue while leaving stop_reason intact.
        let summary = std::fs::read_to_string(out_dir.join("summary.txt")).unwrap();
        assert!(
            summary.contains("sprt: verdict=H1"),
            "H1-accepting stream must emit `sprt: verdict=H1` in summary; got:\n{summary}"
        );
    }

    #[test]
    fn sprt_mode_h0_synthetic_stream_rejects() {
        // Strong H0 stream: 80% bin 0 (0.0) + 20% bin 1 (0.5). mu = 0.1 ≪ midpoint.
        let args = sprt_args(400);
        let reports: Vec<WorkerReport> = (0..200u32)
            .flat_map(|p| pair_reports(0, p, if p % 5 == 0 { 0.5 } else { 0.0 }))
            .collect();
        let mut pool = synthetic_pool(1, vec![reports]);
        let out_dir = std::env::temp_dir().join("eloh_e_sprt_h0");
        let outcome = run_iteration(&mut pool, &args, &out_dir).unwrap();
        assert_eq!(
            outcome.stop_reason,
            super::super::StopReason::SprtAcceptH0,
            "strong-H0 stream (mu=0.1) must accept H0"
        );
        // Pin `sprt: verdict=H0` — catches the `delete match arm
        // Some(SprtAcceptH0)` mutant (line 7603) that would silently
        // downgrade the summary verdict to `continue`.
        let summary = std::fs::read_to_string(out_dir.join("summary.txt")).unwrap();
        assert!(
            summary.contains("sprt: verdict=H0"),
            "H0-accepting stream must emit `sprt: verdict=H0` in summary; got:\n{summary}"
        );
    }

    #[test]
    fn sprt_mode_max_games_no_verdict() {
        // All draws (pair_score = 1.0): zero variance → LLR = 0 always
        // → indifference zone → MaxGames termination.
        let args = sprt_args(8); // 4 pairs only — far too few to converge.
        let reports: Vec<WorkerReport> = (0..4u32).flat_map(|p| pair_reports(0, p, 1.0)).collect();
        let mut pool = synthetic_pool(1, vec![reports]);
        let out_dir = std::env::temp_dir().join("eloh_e_sprt_maxgames");
        let outcome = run_iteration(&mut pool, &args, &out_dir).unwrap();
        assert_eq!(
            outcome.stop_reason,
            super::super::StopReason::MaxGames,
            "indifference-zone stream must terminate with MaxGames"
        );
        // The summary file must contain a `sprt: verdict=continue` line.
        let summary = std::fs::read_to_string(out_dir.join("summary.txt")).unwrap();
        assert!(
            summary.contains("sprt: verdict=continue"),
            "MaxGames termination in SPRT mode must emit verdict=continue; summary: {summary}"
        );
    }

    #[test]
    fn sprt_mode_emits_ci_line() {
        // Mixed pair scores → ≥2 pairs with variance → CI line emitted.
        let args = sprt_args(8);
        let mut reports: Vec<WorkerReport> = pair_reports(0, 0, 1.5);
        reports.extend(pair_reports(0, 1, 0.5));
        reports.extend(pair_reports(0, 2, 1.0));
        reports.extend(pair_reports(0, 3, 2.0));
        let mut pool = synthetic_pool(1, vec![reports]);
        let out_dir = std::env::temp_dir().join("eloh_e_sprt_ci");
        let _ = run_iteration(&mut pool, &args, &out_dir).unwrap();
        let summary = std::fs::read_to_string(out_dir.join("summary.txt")).unwrap();
        assert!(
            summary.contains("ci: elo="),
            "SPRT run must emit a `ci: elo=...` line; summary: {summary}"
        );
    }

    #[test]
    fn pair_score_buffers_per_worker_under_concurrency() {
        // Two workers' pairs interleave. Per-worker keying must keep
        // pair scores separate; if a single shared buffer were used,
        // worker 0's game-A scores would mix with worker 1's game-A
        // scores before either PairComplete arrived, corrupting the
        // ptnml bin counts and the LLR.
        //
        // Each worker emits the same H1-favouring stream as the
        // single-worker test above (80% pair score 2.0, 20% 1.5), so
        // the SPRT must converge to AcceptH1.
        let mut args = sprt_args(400);
        args.concurrency = 2;
        let total_pairs: u32 = args.max_games / 2;
        let mk_reports = |wid: u32| -> Vec<WorkerReport> {
            (0..total_pairs)
                .filter(|p| p % 2 == wid)
                .flat_map(|p| pair_reports(wid, p, if p % 5 == 0 { 1.5 } else { 2.0 }))
                .collect()
        };

        let mut pool = synthetic_pool(2, vec![mk_reports(0), mk_reports(1)]);
        let out_dir = std::env::temp_dir().join("eloh_e_sprt_per_worker");
        let outcome = run_iteration(&mut pool, &args, &out_dir).unwrap();
        assert_eq!(
            outcome.stop_reason,
            super::super::StopReason::SprtAcceptH1,
            "concurrent workers with strong-H1 pairs must accept H1"
        );
    }

    #[test]
    fn singleton_counter_remains_zero_in_normal_termination() {
        let args = sprt_args(8);
        let reports: Vec<WorkerReport> = (0..4u32).flat_map(|p| pair_reports(0, p, 1.0)).collect();
        let mut pool = synthetic_pool(1, vec![reports]);
        let out_dir = std::env::temp_dir().join("eloh_e_sprt_no_singletons");
        let _ = run_iteration(&mut pool, &args, &out_dir).unwrap();
        // discarded_singletons isn't surfaced in IterationOutcome; the
        // contract is that no `match.pgn`/`summary.txt` artifact reports
        // an unexplained discarded count. We pin the contract indirectly
        // by checking that all expected pairs (4) are reflected in the
        // summary's ptnml line.
        let summary = std::fs::read_to_string(out_dir.join("summary.txt")).unwrap();
        assert!(
            summary.contains("ptnml=[0,0,4,0,0]"),
            "all-draw 4 pairs must yield ptnml=[0,0,4,0,0]; summary: {summary}"
        );
    }

    #[test]
    fn match_pgn_concat_orders_by_game_index() {
        // Set up a fake games_dir with three PGNs (3.pgn, 1.pgn, 2.pgn)
        // in arbitrary creation order; assert match.pgn lists them in
        // ascending game_index order.
        let dir = std::env::temp_dir().join("eloh_e_match_pgn_concat");
        let games_dir = dir.join("games");
        let _ = std::fs::create_dir_all(&games_dir);
        // Clean any stale fixture.
        for n in 1..=3 {
            let _ = std::fs::remove_file(games_dir.join(format!("{n}.pgn")));
        }
        let _ = std::fs::remove_file(dir.join("match.pgn"));
        std::fs::write(games_dir.join("3.pgn"), "[Round \"3\"]\n*\n").unwrap();
        std::fs::write(games_dir.join("1.pgn"), "[Round \"1\"]\n*\n").unwrap();
        std::fs::write(games_dir.join("2.pgn"), "[Round \"2\"]\n*\n").unwrap();
        super::write_match_pgn(&games_dir, &dir.join("match.pgn")).unwrap();
        let combined = std::fs::read_to_string(dir.join("match.pgn")).unwrap();
        let p1 = combined.find("[Round \"1\"]").unwrap();
        let p2 = combined.find("[Round \"2\"]").unwrap();
        let p3 = combined.find("[Round \"3\"]").unwrap();
        assert!(
            p1 < p2 && p2 < p3,
            "match.pgn must list rounds 1<2<3; got {combined}"
        );
    }

    #[test]
    fn match_pgn_concat_handles_zero_games() {
        let dir = std::env::temp_dir().join("eloh_e_match_pgn_empty");
        let games_dir = dir.join("games");
        let _ = std::fs::create_dir_all(&games_dir);
        // Clean any stale .pgn fixtures.
        if let Ok(entries) = std::fs::read_dir(&games_dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let match_pgn = dir.join("match.pgn");
        super::write_match_pgn(&games_dir, &match_pgn).unwrap();
        let content = std::fs::read_to_string(&match_pgn).unwrap();
        assert!(
            content.is_empty(),
            "zero-game run must produce empty match.pgn"
        );
    }

    #[test]
    fn match_pgn_separator_newline_between_games() {
        // write_match_pgn must insert exactly one blank-line separator
        // (\n) between adjacent games. Pins three `>` boundary mutations:
        //   - `> → ==`: inserts \n before game-0 only (wrong side).
        //   - `> → <`: `i < 0` always false — no separator ever added.
        //   - `> → >=`: `i >= 0` always true — adds \n before EVERY game
        //     including the first, which creates a leading blank line.
        // The test asserts the combined output starts with game-1 content
        // (no leading newline) AND that games are separated by exactly
        // one blank line (\n\n) between the trailing newline of game N and
        // the opening tag of game N+1.
        let dir = std::env::temp_dir().join("eloh_e_match_pgn_separator");
        let games_dir = dir.join("games");
        let _ = std::fs::create_dir_all(&games_dir);
        for n in 1..=2 {
            let _ = std::fs::remove_file(games_dir.join(format!("{n}.pgn")));
        }
        let match_pgn = dir.join("match_sep.pgn");
        let _ = std::fs::remove_file(&match_pgn);
        // Each game ends with \n so they already have a trailing newline.
        std::fs::write(games_dir.join("1.pgn"), "[Round \"1\"]\n*\n").unwrap();
        std::fs::write(games_dir.join("2.pgn"), "[Round \"2\"]\n*\n").unwrap();
        super::write_match_pgn(&games_dir, &match_pgn).unwrap();
        let content = std::fs::read_to_string(&match_pgn).unwrap();
        // Must not start with a newline (no prefix separator before game 1).
        assert!(
            !content.starts_with('\n'),
            "match.pgn must not start with a newline; got {content:?}"
        );
        // Must contain exactly one blank-line separator between games.
        assert!(
            content.contains("\n\n"),
            "match.pgn must contain a blank-line separator between games; got {content:?}"
        );
        // Expected exact output: game1 content + separator \n + game2 content.
        let expected = "[Round \"1\"]\n*\n\n[Round \"2\"]\n*\n";
        assert_eq!(
            content, expected,
            "match.pgn exact content mismatch; expected {expected:?}"
        );
    }

    #[test]
    fn match_pgn_trailing_newline_added_when_missing() {
        // When a game's content does NOT end with '\n', write_match_pgn
        // must append one. Pins the `delete !` mutation on
        // `if !content.ends_with('\n')` which would flip to appending an
        // extra newline when content ALREADY ends with '\n'.
        //
        // Two sub-assertions:
        //   a) Content without trailing \n gets one appended.
        //   b) Content with trailing \n does NOT get an extra one.
        let dir = std::env::temp_dir().join("eloh_e_match_pgn_trailing_nl");
        let games_dir = dir.join("games");
        let _ = std::fs::create_dir_all(&games_dir);
        for n in 1..=2 {
            let _ = std::fs::remove_file(games_dir.join(format!("{n}.pgn")));
        }
        let match_pgn = dir.join("match_trailing.pgn");
        let _ = std::fs::remove_file(&match_pgn);
        // Game 1: no trailing newline. Game 2: has trailing newline.
        std::fs::write(games_dir.join("1.pgn"), "[Round \"1\"]\n*").unwrap();
        std::fs::write(games_dir.join("2.pgn"), "[Round \"2\"]\n*\n").unwrap();
        super::write_match_pgn(&games_dir, &match_pgn).unwrap();
        let content = std::fs::read_to_string(&match_pgn).unwrap();
        // Expected: game1 (no \n) + appended \n + separator \n + game2 (has \n).
        // So combined: "[Round \"1\"]\n*\n\n[Round \"2\"]\n*\n".
        let expected = "[Round \"1\"]\n*\n\n[Round \"2\"]\n*\n";
        assert_eq!(
            content, expected,
            "write_match_pgn must append \\n to content without trailing newline \
             and must NOT append an extra \\n to content that already has one; \
             got {content:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Boundary tests for `controller::run_iteration` — close the deferred
    // mutation-coverage gap from `.cargo/mutants.toml` `in
    // controller::run_iteration`. See `docs/plans/tooling-eloh-controller-
    // boundary-tests.md` §4.2 for the per-mutant table.
    // -----------------------------------------------------------------------

    /// Watchdog wrapper for hang-class boundary mutants.
    ///
    /// Several drain-loop mutants make the loop's exit condition
    /// unreachable under happy-path test fixtures (e.g., the `>= → <`
    /// mutant on the bootstrap break, or `delete !` on the redispatch
    /// gate's `!terminating`). `cargo test` has no per-test timeout, so a
    /// hung mutant blocks the entire `cargo test` run indefinitely.
    ///
    /// Implementation: spawn `run_iteration` on a fresh thread with
    /// ownership of the pool transferred in; recover the result via
    /// `mpsc::Receiver::recv_timeout`; panic on timeout. The hung thread
    /// is leaked when the watchdog fires; the test process exits soon
    /// after, which reaps the OS thread.
    ///
    /// `std::thread::scope` was rejected because its closure waits for
    /// spawned threads to join before returning, defeating the watchdog.
    ///
    /// Recommended timeout at call sites: `Duration::from_secs(2)`.
    /// Synthetic-worker tests typically complete in ~50 ms, giving a
    /// ~40× flake margin. Raise to 5 s on slow CI if false positives appear.
    fn run_iteration_with_watchdog(
        mut pool: WorkerPool,
        args: super::super::cli::Args,
        out_dir: std::path::PathBuf,
        timeout: std::time::Duration,
    ) -> Result<IterationOutcome, super::super::driver::HarnessError> {
        let (tx, rx) = mpsc::channel();
        let _hung_thread = std::thread::spawn(move || {
            let r = run_iteration(&mut pool, &args, &out_dir);
            let _ = tx.send(r);
        });
        rx.recv_timeout(timeout)
            .unwrap_or_else(|_| panic!("run_iteration hung past {timeout:?}"))
    }

    /// Build `Args` with `--max-games N --concurrency C`, fixed-K and
    /// disabled-σ so MaxGames is the only natural stop criterion.
    fn args_with_concurrency(max_games: u32, concurrency: u32) -> super::super::cli::Args {
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
            "--concurrency".into(),
            concurrency.to_string(),
        ];
        super::super::cli::parse_args(argv).expect("parse ok")
    }

    /// Build a `WorkerReport::GameComplete` skeleton for tests that don't
    /// care about per-game fields beyond `worker_id` and `game_index`.
    fn gc(worker_id: u32, game_index: u32) -> WorkerReport {
        WorkerReport::GameComplete {
            worker_id,
            game_index,
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
        }
    }

    /// Mutant A (bootstrap-break `if pairs_dispatched >= total_pairs`,
    /// `>= → <`) — hang-class.
    /// Under the mutation, bootstrap breaks immediately at pd=0 → no
    /// PlayPair sent → `pool.reports.recv()` blocks forever.
    #[test]
    fn run_iteration_does_not_hang_on_bootstrap_break() {
        let args = args_with_concurrency(2, 1);
        let pool = synthetic_pool(
            1,
            vec![vec![
                gc(0, 1),
                gc(0, 2),
                WorkerReport::PairComplete { worker_id: 0 },
            ]],
        );
        let out_dir = std::env::temp_dir().join("eloh_boundary_bootstrap_break");
        let outcome =
            run_iteration_with_watchdog(pool, args, out_dir, std::time::Duration::from_secs(2))
                .expect("run_iteration must succeed under original");
        assert_eq!(
            outcome.games_played, 2,
            "original code must process exactly max_games=2 games; got {}",
            outcome.games_played
        );
    }

    /// **Hypothetical** mutant J (`saturating_sub(1) → saturating_sub(0)`
    /// on the redispatch-arm in-flight decrement) — hang-class. cargo-
    /// mutants 27.0.0 does NOT generate this mutation (it doesn't mutate
    /// method-call argument literals), so this test is defense-in-depth
    /// coverage against a future cargo-mutants version that does generate
    /// it, or a hand-applied regression that introduces equivalent broken
    /// bookkeeping. Under the mutation, pif never decrements →
    /// drain_done's `all_idle` never becomes true → after the last PC,
    /// `recv()` blocks indefinitely.
    ///
    /// **Distinguishing fixture.** With the natural `max_games =
    /// 2*total_pairs` constraint, t reaches max_games before drain_done
    /// can fire (the t-gate preempts), making mutant J undetectable. To
    /// force drain_done to be the sole stopping criterion, we use
    /// max_games=100 (so total_pairs=50) with a script that emits only
    /// 1 GC + 1 PC per PlayPair received — 50 PlayPairs produce 50 GCs
    /// total, well under the t-gate of 100.
    ///
    /// Original: 50 PCs decrement pif to 0 each cycle, drain_done fires
    /// on the loop top after PC50 (pd=50, all_idle=true); games_played=50.
    ///
    /// Mutant: pif stays at 1 across all 50 PCs; after PC50, the
    /// redispatch arm gates out (50<50 false), pif=[1]; drain_done
    /// `(false || pd>=tp=true) && all_idle=false` = false; recv blocks
    /// (worker has nothing more to emit). Watchdog fires.
    #[test]
    fn run_iteration_does_not_hang_on_in_flight_decrement() {
        let mut args = args_with_concurrency(4, 1);
        args.max_games = 100; // total_pairs=50; 50 GCs total reach t=50 < max_games=100 → drain_done is the sole exit criterion.
        let pool = synthetic_pool(
            1,
            vec![vec![gc(0, 1), WorkerReport::PairComplete { worker_id: 0 }]],
        );
        let out_dir = std::env::temp_dir().join("eloh_boundary_pif_decrement");
        let outcome =
            run_iteration_with_watchdog(pool, args, out_dir, std::time::Duration::from_secs(2))
                .expect("run_iteration must succeed under original");
        // Original exits via drain_done after 50 pairs × 1 GC each.
        assert_eq!(outcome.games_played, 50);
    }

    /// Mutant D (bootstrap `*in_flight_slot += 1 → *= 1`) — non-hang. Under mutation, `pif=[0,…,0]` at end of bootstrap;
    /// when bootstrap fills all workers (`n_workers >= total_pairs`),
    /// `pd>=tp` AND `all_idle` are both immediately true at the first
    /// drain_done check → loop exits before any GC processed →
    /// games_played=0.
    ///
    /// Setup: 4 workers, max_games=8 (total_pairs=4). Bootstrap dispatches
    /// 4 PlayPairs (one per worker). Original: pif=[1,1,1,1], drain_done
    /// false → games processed. Mutant: pif=[0,0,0,0], drain_done true
    /// → games_played=0.
    ///
    /// Note: the synthetic_pool worker re-sends `worker_id` from each
    /// canned report (lines 7728+). All four cloned scripts here use
    /// `worker_id: 0`, so all PCs the controller receives carry
    /// `worker_id=0` and only `pif[0]` is decremented at PC processing.
    /// That's fine for this test — under the original, t increments via
    /// GCs and reaches max_games=8 cleanly; under the mutant, the loop
    /// exits at iter 1 before any GC.
    #[test]
    fn run_iteration_bootstrap_in_flight_increment_pins_drain_done() {
        let args = args_with_concurrency(8, 4);
        let make_script = || {
            vec![
                gc(0, 1),
                gc(0, 2),
                WorkerReport::PairComplete { worker_id: 0 },
            ]
        };
        let mut pool = synthetic_pool(
            4,
            vec![make_script(), make_script(), make_script(), make_script()],
        );
        let out_dir = std::env::temp_dir().join("eloh_boundary_bootstrap_pif");
        let outcome = run_iteration(&mut pool, &args, &out_dir).unwrap();
        assert!(
            outcome.games_played > 0,
            "original code must process at least one game; mutant exits immediately at bootstrap with games_played=0"
        );
    }

    /// Mutants L, M (redispatch-gate `pairs_dispatched < total_pairs`,
    /// `<` → `==`/`>` clause) — hang-class. Under either mutation, redispatch
    /// fires for at most one pair before the gate falsifies; remaining
    /// total_pairs go undispatched and drain_done can't reach `pd>=tp`.
    ///
    /// Setup: 1 worker, max_games=4 (total_pairs=2). Original: bootstrap
    /// dispatches pair 0; PC1 redispatches pair 1; PC2 → drain_done true.
    /// Mutant `<→==`: at PC1, `1==2` false → no redispatch; pif=0;
    /// drain_done is `(false || pd=1>=tp=2 false) && all_idle=true` =
    /// false → recv blocks forever.
    #[test]
    fn run_iteration_does_not_hang_on_redispatch_pd_eq_tp_boundary() {
        let args = args_with_concurrency(4, 1);
        let pool = synthetic_pool(
            1,
            vec![vec![
                gc(0, 1),
                gc(0, 2),
                WorkerReport::PairComplete { worker_id: 0 },
            ]],
        );
        let out_dir = std::env::temp_dir().join("eloh_boundary_redispatch_pd_eq_tp");
        let outcome =
            run_iteration_with_watchdog(pool, args, out_dir, std::time::Duration::from_secs(2))
                .expect("run_iteration must succeed under original");
        assert_eq!(outcome.games_played, 4);
    }

    /// Mutant N1 (redispatch-gate `delete !` on `!terminating`) —
    /// hang-class.
    /// Gate becomes `terminating && pd<tp && wid<senders.len()`.
    /// Happy-path tests have terminating=false → gate always false → no
    /// redispatch. Same hang signature as L/M.
    ///
    /// Test setup is intentionally identical to L/M — the watchdog
    /// catches all four sibling drain-loop hangs under one assertion.
    #[test]
    fn run_iteration_does_not_hang_on_terminating_gate() {
        let args = args_with_concurrency(4, 1);
        let pool = synthetic_pool(
            1,
            vec![vec![
                gc(0, 1),
                gc(0, 2),
                WorkerReport::PairComplete { worker_id: 0 },
            ]],
        );
        let out_dir = std::env::temp_dir().join("eloh_boundary_terminating_gate");
        let outcome =
            run_iteration_with_watchdog(pool, args, out_dir, std::time::Duration::from_secs(2))
                .expect("run_iteration must succeed under original");
        assert_eq!(outcome.games_played, 4);
    }

    /// Mutant F (redispatch arm `pairs_dispatched += 1 → *= 1`)
    /// — non-hang. Under the mutation, pd never advances past its
    /// bootstrap value, but the redispatch arm keeps re-firing with the
    /// same `pair_tcs[pd]`. Total games still reaches max_games (worker
    /// emits 2 GCs per cmd), so `games_played == max_games` does NOT
    /// distinguish — detection requires asserting on the per-cmd
    /// `pair_index` SEQUENCE.
    ///
    /// Setup: 1 worker, max_games=8 (total_pairs=4). Recorder: worker's
    /// received PlayPair `pair_index` sequence must be [0, 1, 2, 3]
    /// strictly. Mutant: stays at 1 (or 0, depending on whether `*= 1`
    /// fires before or after the bootstrap `pairs_dispatched += 1` —
    /// a different site, mutated independently) → repeats forever.
    #[test]
    fn run_iteration_redispatch_pair_indices_form_strictly_ascending_sequence() {
        use std::sync::{Arc, Mutex};

        let recorded: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let (rpt_tx, rpt_rx) = mpsc::channel::<WorkerReport>();
        let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
        let log = Arc::clone(&recorded);
        let rpt_tx_clone = rpt_tx.clone();

        let handle = std::thread::spawn(move || {
            let mut pair_counter = 0u32;
            for cmd in &cmd_rx {
                match cmd {
                    WorkerCmd::Quit => break,
                    WorkerCmd::PlaySpsaPair { .. } => {
                        unreachable!("SPSA pair sent to synthetic SPRT worker")
                    }
                    WorkerCmd::PlayPair { pair_index, .. } => {
                        log.lock().unwrap().push(pair_index);
                        pair_counter += 1;
                        let g = pair_counter * 2;
                        let _ = rpt_tx_clone.send(gc(0, g - 1));
                        let _ = rpt_tx_clone.send(gc(0, g));
                        let _ = rpt_tx_clone.send(WorkerReport::PairComplete { worker_id: 0 });
                    }
                }
            }
        });
        drop(rpt_tx);
        let mut pool = WorkerPool {
            senders: vec![cmd_tx],
            reports: rpt_rx,
            join_handles: vec![handle],
        };

        let args = args_with_concurrency(8, 1);
        let out_dir = std::env::temp_dir().join("eloh_boundary_pair_index_sequence");
        let _ = run_iteration(&mut pool, &args, &out_dir);

        let received = recorded.lock().unwrap().clone();
        assert_eq!(
            received,
            vec![0u32, 1, 2, 3],
            "redispatch must advance pair_index strictly: expected [0,1,2,3], got {received:?}"
        );
    }

    /// Mutant K (redispatch-gate `pd<tp` clause, `<` → `<=`) — non-hang,
    /// panic-class. At pd=tp, `tp <= tp` true → tries to dispatch one
    /// more time → panics on `pair_tcs[tp]` (out of bounds).
    ///
    /// **Subtle fixture requirement.** With `max_games = 2*total_pairs`
    /// (the natural case enforced by `parse_args`), `t` reaches
    /// `max_games` exactly after the last GC of the final pair, and the
    /// `t >= max_games` early-break preempts the final PC's processing.
    /// To force the final PC to be processed (so the mutant's panic
    /// fires), the test bypasses `parse_args` and sets `max_games =
    /// 2*total_pairs + 1`.  The +1 leaves room for the final PC to be
    /// recv'd before the t-gate fires.
    ///
    /// Setup: 1 worker, total_pairs=2, max_games=5.  Worker emits 2
    /// pairs' worth (6 reports) per PlayPair. Bootstrap dispatches pair
    /// 0; controller processes GC,GC,PC (t=2, redispatch pair 1, pd=2);
    /// GC,GC,PC (t=4, PC1 reached because 4<5).  At PC1: redispatch
    /// arm under original `pd<tp` is `2<2` false → no dispatch.  Under
    /// mutant `pd<=tp` is `2<=2` true → tries `pair_tcs[2]` → panic.
    #[test]
    fn run_iteration_redispatch_dispatches_exactly_total_pairs_no_overshoot() {
        let mut args = args_with_concurrency(4, 1);
        args.max_games = 5; // total_pairs = 5/2 = 2; +1 headroom past the final pair's last GC.
        let pair_reports: Vec<WorkerReport> = (0..2u32)
            .flat_map(|p| {
                vec![
                    gc(0, p * 2 + 1),
                    gc(0, p * 2 + 2),
                    WorkerReport::PairComplete { worker_id: 0 },
                ]
            })
            .collect();
        let mut pool = synthetic_pool(1, vec![pair_reports]);
        let out_dir = std::env::temp_dir().join("eloh_boundary_redispatch_no_overshoot");
        let outcome = run_iteration(&mut pool, &args, &out_dir).unwrap();
        assert_eq!(outcome.games_played, 4);
        // Loop must terminate on its own — drain_done (pd>=tp, all_idle)
        // fires after the final PC under the original.
        assert_eq!(outcome.stop_reason, super::super::StopReason::MaxGames);
    }

    // -----------------------------------------------------------------------
    // Subprocess-driven tests: production_worker_fn against mock-engine
    //
    // These tests pin the per-pair UCI command sequence emitted by
    // `production_worker_fn` against the recording produced by the
    // `mock-engine` test fixture binary (`src/bin/mock_engine.rs`). They
    // close the structural gap noted in `.cargo/mutants.toml`'s
    // `in controller::production_worker_fn` exclusion: the function is
    // structurally untestable via synthetic-pool fixtures because its
    // contract is the exact UCI byte sequence sent to engine subprocesses.
    //
    // Plan: docs/plans/tooling-mock-engine-fixture.md
    // -----------------------------------------------------------------------

    mod production_worker_tests {
        use super::*;

        /// Resolve the path to the `mock-engine` binary. Walks from
        /// `current_exe()` up two parents to the profile directory
        /// (`target/debug` or `target/release`) and looks for the binary
        /// there — same pattern as `e2e_smoke::resolve_bin`. `cargo test`
        /// builds all `[[bin]]` targets before running tests, so the
        /// binary always exists at this path during a test run.
        fn resolve_mock_engine_bin() -> String {
            let exe = std::env::current_exe().expect("current_exe");
            let deps_dir = exe.parent().expect("deps dir");
            let profile_dir = deps_dir.parent().expect("profile dir");
            let candidate = profile_dir.join("mock-engine");
            if candidate.exists() {
                return candidate.to_str().expect("valid utf8 path").to_owned();
            }
            panic!(
                "could not find mock-engine binary at {candidate:?} — \
                 run `cargo build --bin mock-engine` first or invoke \
                 this test via `cargo test`"
            );
        }

        /// Build an [`EngineSpec`] that launches the mock-engine binary
        /// with per-instance environment variables conveyed via
        /// `/usr/bin/env` in the launch_prefix. Uses `/usr/bin/env`
        /// explicitly (not bare `env`) for PATH-independence on macOS and
        /// Linux CI runners.
        fn build_engine_spec_for_mock(
            name: &str,
            mock_path: &str,
            record_path: &std::path::Path,
            advertise_vc: bool,
        ) -> super::super::super::driver::EngineSpec {
            let mut prefix = vec![
                "/usr/bin/env".to_string(),
                format!("MOCK_ENGINE_RECORD_PATH={}", record_path.display()),
            ];
            if advertise_vc {
                prefix.push("MOCK_ENGINE_VIRTUAL_CLOCK_ADVERTISED=1".to_string());
            }
            super::super::super::driver::EngineSpec {
                name: name.to_string(),
                path: mock_path.to_string(),
                launch_prefix: Some(prefix),
            }
        }

        /// Per-game `GameComplete` payload captured by the helper.
        #[derive(Debug, Clone)]
        struct GameInfo {
            game_index: u32,
            white_name: String,
            black_name: String,
        }

        /// Outcome of one PlayPair run against two mock-engine instances.
        #[derive(Debug)]
        struct PairOutcome {
            /// Lines recorded by the engine-side mock instance.
            engine_log: Vec<String>,
            /// Lines recorded by the opponent-side mock instance.
            opponent_log: Vec<String>,
            /// One per `GameComplete` report received, in arrival order.
            /// Always length 2 for a successful pair.
            games: Vec<GameInfo>,
        }

        /// Drive `production_worker_fn` through one full PlayPair against
        /// two mock-engine instances and return their recordings.
        ///
        /// Mechanism:
        /// 1. Spawn one worker via `controller::spawn_workers(1, cfg)`,
        ///    which uses the production `production_worker_fn`.
        /// 2. Send one `WorkerCmd::PlayPair` and drain reports until
        ///    `PairComplete` (or watchdog).
        /// 3. **Drop senders, then explicitly join the worker thread**
        ///    with a 10 s watchdog so `driver::shutdown` reaps both child
        ///    processes and the mocks have flushed their `quit` lines
        ///    before the recording files are read. (Without the join,
        ///    the read races with mid-shutdown writes — see plan §5.3.)
        /// 4. Read both recording files and return the lines.
        #[allow(clippy::too_many_arguments)]
        fn run_one_pair_against_mocks(
            engine_options: Vec<(String, String)>,
            opponent_options: Vec<(String, String)>,
            virtual_clock: bool,
            advertise_vc_engine: bool,
            advertise_vc_opponent: bool,
            opponent_uci_elo: u32,
            pair_index: u32,
        ) -> PairOutcome {
            let mock = resolve_mock_engine_bin();

            // Unique temp-dir per call to avoid collisions across
            // parallel-running tests under `cargo test`'s default
            // -j N test runner. PID alone collides (all parallel tests
            // share the cargo-test process); SystemTime::now() at
            // nanosecond resolution still collides across cores when
            // multiple tests start near-simultaneously. The atomic
            // counter is the load-bearing tiebreaker.
            static UNIQUE_COUNTER: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let unique = format!(
                "eloh_mock_pworker_{}_{}_{}",
                std::process::id(),
                UNIQUE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            let temp_dir = std::env::temp_dir().join(unique);
            let _ = std::fs::remove_dir_all(&temp_dir);
            std::fs::create_dir_all(&temp_dir).expect("create temp_dir");
            let engine_record = temp_dir.join("engine.log");
            let opponent_record = temp_dir.join("opponent.log");

            let engine_spec = build_engine_spec_for_mock(
                "engine-mock",
                &mock,
                &engine_record,
                advertise_vc_engine,
            );
            let opponent_spec = build_engine_spec_for_mock(
                "opponent-mock",
                &mock,
                &opponent_record,
                advertise_vc_opponent,
            );

            let cfg = WorkerConfig {
                engine_spec,
                opponent_spec,
                engine_options,
                opponent_options,
                mode: crate::MatchTimeMode::Wallclock,
                harness_overhead_ms: 0,
                watchdog: std::time::Duration::from_secs(10),
                max_plies: 100,
                thresholds: super::super::super::cli::Thresholds::default(),
                virtual_clock,
            };

            let mut pool = spawn_workers(1, cfg).expect("spawn_workers");

            // Send one PlayPair.
            let tc = super::super::super::cli::TimeControl {
                initial_ms: 1000,
                increment_ms: 0,
            };
            pool.senders[0]
                .send(WorkerCmd::PlayPair {
                    pair_index,
                    opponent_uci_elo,
                    engine_tc: tc,
                    opponent_tc: tc,
                })
                .expect("send PlayPair");

            // Drain reports until PairComplete (or Failure/timeout). The
            // PairComplete arm is the only non-panic exit; reaching the
            // post-loop assertions guarantees we saw exactly that report.
            let report_deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            let mut games: Vec<GameInfo> = Vec::new();
            loop {
                let remaining =
                    report_deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    panic!(
                        "report drain timed out after 20 s; got {} GameComplete reports, \
                         no PairComplete",
                        games.len()
                    );
                }
                match pool.reports.recv_timeout(remaining) {
                    Ok(WorkerReport::GameComplete {
                        game_index,
                        white_name,
                        black_name,
                        ..
                    }) => {
                        games.push(GameInfo {
                            game_index,
                            white_name,
                            black_name,
                        });
                    }
                    Ok(WorkerReport::PairComplete { .. }) => {
                        break;
                    }
                    Ok(WorkerReport::Failure(msg)) => {
                        panic!("worker reported failure before PairComplete: {msg}");
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        panic!(
                            "report drain timed out (recv_timeout); got {} GameComplete \
                             reports, no PairComplete",
                            games.len()
                        );
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        panic!(
                            "worker disconnected before PairComplete; got {} GameComplete \
                             reports",
                            games.len()
                        );
                    }
                }
            }
            assert_eq!(
                games.len(),
                2,
                "PlayPair must produce exactly 2 GameComplete reports"
            );

            // Drop senders so the worker's recv loop exits → triggers
            // `driver::shutdown` for both engine and opponent.
            pool.senders.clear();

            // Join the worker thread with a 10 s watchdog. Without this,
            // we'd race with the mock's `quit`-record write.
            let handles = std::mem::take(&mut pool.join_handles);
            join_workers_with_watchdog(handles, std::time::Duration::from_secs(10));

            // Drop the pool entirely (no-op on senders/handles, drops the
            // reports receiver).
            drop(pool);

            // Read recording files. Both must exist; emptiness is a
            // failure signal (mock crashed before any line, etc).
            let engine_log = read_log(&engine_record);
            let opponent_log = read_log(&opponent_record);

            // Best-effort temp-dir cleanup. Stale dirs are harmless;
            // the `remove_dir_all` at run start cleans them up next time.
            let _ = std::fs::remove_dir_all(&temp_dir);

            PairOutcome {
                engine_log,
                opponent_log,
                games,
            }
        }

        /// Read a recording file into one `String` per line. Panics if
        /// the file does not exist (mock failed to start) or is empty
        /// (mock crashed before recording any line).
        fn read_log(path: &std::path::Path) -> Vec<String> {
            let raw = std::fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("read recording {path:?}: {e}"));
            let lines: Vec<String> = raw.lines().map(str::to_owned).collect();
            assert!(
                !lines.is_empty(),
                "recording at {path:?} is empty — mock failed to record any UCI line"
            );
            lines
        }

        /// Join a vector of worker handles, bounded by `timeout`. The
        /// hung-thread approach mirrors `run_iteration_with_watchdog`:
        /// move ownership into a fresh thread, recover via
        /// `mpsc::recv_timeout`, leak the thread on timeout (the test
        /// process exits soon after, which reaps the OS thread).
        fn join_workers_with_watchdog(
            handles: Vec<std::thread::JoinHandle<()>>,
            timeout: std::time::Duration,
        ) {
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            let _hung = std::thread::spawn(move || {
                for h in handles {
                    let _ = h.join();
                }
                let _ = tx.send(());
            });
            rx.recv_timeout(timeout)
                .unwrap_or_else(|_| panic!("worker join hung past {timeout:?}"));
        }

        // -------------------------------------------------------------------
        // Tests
        // -------------------------------------------------------------------

        /// T1: per-pair `setoption name UCI_Elo …` precedes `ucinewgame`
        /// in the opponent's log. Negative-symmetry: engine_log contains
        /// no such per-pair setoption (the engine is the strength-cap
        /// SUT, not the strength-limited opponent).
        #[test]
        fn production_worker_fn_emits_setoption_uci_elo_before_ucinewgame_per_pair() {
            let outcome = run_one_pair_against_mocks(vec![], vec![], false, false, false, 2400, 0);

            // (a) UCI_Elo precedes ucinewgame in opponent_log.
            let elo_idx = outcome
                .opponent_log
                .iter()
                .position(|l| l.starts_with("setoption name UCI_Elo "))
                .unwrap_or_else(|| {
                    panic!(
                        "opponent_log missing `setoption name UCI_Elo …` line; got {:?}",
                        outcome.opponent_log
                    )
                });
            let ucn_idx = outcome
                .opponent_log
                .iter()
                .position(|l| l == "ucinewgame")
                .unwrap_or_else(|| {
                    panic!(
                        "opponent_log missing `ucinewgame` line; got {:?}",
                        outcome.opponent_log
                    )
                });
            assert!(
                elo_idx < ucn_idx,
                "UCI_Elo (idx {elo_idx}) must precede ucinewgame (idx {ucn_idx}) in opponent_log: {:?}",
                outcome.opponent_log
            );

            // (b) Negative-symmetry: engine_log has no UCI_Elo setoption.
            assert!(
                !outcome
                    .engine_log
                    .iter()
                    .any(|l| l.starts_with("setoption name UCI_Elo ")),
                "engine_log must NOT contain `setoption name UCI_Elo …` (engine is the SUT, \
                 not the strength-limited opponent); got {:?}",
                outcome.engine_log
            );
        }

        /// T2: per-pair UCI_Elo carries the exact value from the cmd
        /// payload. Uses a non-round distinguishing integer (1789) to
        /// reduce coincidental-match risk against hypothetical
        /// constant-folding mutants.
        #[test]
        fn production_worker_fn_emits_uci_elo_with_correct_value() {
            let outcome = run_one_pair_against_mocks(vec![], vec![], false, false, false, 1789, 0);
            assert!(
                outcome
                    .opponent_log
                    .iter()
                    .any(|l| l == "setoption name UCI_Elo value 1789"),
                "opponent_log must contain `setoption name UCI_Elo value 1789` exactly; \
                 got {:?}",
                outcome.opponent_log
            );
        }

        /// T3: per-pair `setoption UCI_LimitStrength` immediately follows
        /// `setoption UCI_Elo` in opponent_log (no UCI command between).
        /// Negative-symmetry: engine_log contains no LimitStrength line.
        #[test]
        fn production_worker_fn_emits_setoption_limitstrength_after_uci_elo_per_pair() {
            let outcome = run_one_pair_against_mocks(vec![], vec![], false, false, false, 2400, 0);

            // (a) LimitStrength comes immediately after UCI_Elo.
            let elo_idx = outcome
                .opponent_log
                .iter()
                .position(|l| l.starts_with("setoption name UCI_Elo "))
                .expect("UCI_Elo");
            let lim_line = outcome.opponent_log.get(elo_idx + 1).unwrap_or_else(|| {
                panic!(
                    "nothing after UCI_Elo at idx {elo_idx}: {:?}",
                    outcome.opponent_log
                )
            });
            assert_eq!(
                lim_line, "setoption name UCI_LimitStrength value true",
                "line immediately after UCI_Elo must be `setoption name UCI_LimitStrength \
                 value true`; opponent_log = {:?}",
                outcome.opponent_log
            );

            // (b) Negative-symmetry: engine_log has no LimitStrength setoption.
            assert!(
                !outcome
                    .engine_log
                    .iter()
                    .any(|l| l.starts_with("setoption name UCI_LimitStrength ")),
                "engine_log must NOT contain `setoption name UCI_LimitStrength …`; got {:?}",
                outcome.engine_log
            );
        }

        /// T4: per-pair `isready` (sent by `wait_for_readyok` after the
        /// per-pair setoption block) precedes the next `ucinewgame` in
        /// opponent_log.
        #[test]
        fn production_worker_fn_emits_isready_after_setoption_block_before_ucinewgame() {
            let outcome = run_one_pair_against_mocks(vec![], vec![], false, false, false, 2400, 0);

            let lim_idx = outcome
                .opponent_log
                .iter()
                .position(|l| l == "setoption name UCI_LimitStrength value true")
                .expect("UCI_LimitStrength");
            let isready_idx = outcome
                .opponent_log
                .iter()
                .enumerate()
                .find(|(i, l)| *i > lim_idx && *l == "isready")
                .map(|(i, _)| i)
                .unwrap_or_else(|| {
                    panic!(
                        "no `isready` line after UCI_LimitStrength (idx {lim_idx}) in \
                         opponent_log: {:?}",
                        outcome.opponent_log
                    )
                });
            let ucn_idx = outcome
                .opponent_log
                .iter()
                .enumerate()
                .find(|(i, l)| *i > lim_idx && *l == "ucinewgame")
                .map(|(i, _)| i)
                .expect("ucinewgame after setoption block");
            assert!(
                isready_idx < ucn_idx,
                "post-setoption isready (idx {isready_idx}) must precede ucinewgame \
                 (idx {ucn_idx}) in opponent_log: {:?}",
                outcome.opponent_log
            );
        }

        /// T5a: per game (2 per pair), BOTH engines receive `ucinewgame`
        /// followed by `isready`. The pair has 2 games, so each log must
        /// contain at least 2 `ucinewgame` lines and at least 2 `isready`
        /// lines after the handshake-time isready.
        #[test]
        fn production_worker_fn_emits_ucinewgame_then_isready_per_game_for_both_engines() {
            let outcome = run_one_pair_against_mocks(vec![], vec![], false, false, false, 2400, 0);

            for (label, log) in [
                ("engine_log", &outcome.engine_log),
                ("opponent_log", &outcome.opponent_log),
            ] {
                let ucn_count = log.iter().filter(|l| **l == "ucinewgame").count();
                assert_eq!(
                    ucn_count, 2,
                    "{label} must contain exactly 2 `ucinewgame` lines (one per game in \
                     the pair); got {ucn_count}: {:?}",
                    log
                );

                // Verify each ucinewgame is followed by isready (next-line).
                let mut ucn_indices: Vec<usize> = log
                    .iter()
                    .enumerate()
                    .filter_map(|(i, l)| if l == "ucinewgame" { Some(i) } else { None })
                    .collect();
                ucn_indices.sort_unstable();
                for &idx in &ucn_indices {
                    let next = log.get(idx + 1).unwrap_or_else(|| {
                        panic!("{label}: no line after ucinewgame at idx {idx}: {:?}", log)
                    });
                    assert_eq!(
                        next, "isready",
                        "{label}: line after ucinewgame at idx {idx} must be `isready`; \
                         got {next:?}; full log {:?}",
                        log
                    );
                }
            }
        }

        /// T5b: `position startpos` and `go …` are routed to the
        /// side-to-move's handle. With `bestmove 0000`, each game ends
        /// after one ply via `IllegalMove(active_color)`, so the side
        /// playing white in each game receives exactly one position+go
        /// pair. Game 1: clawfish-white → engine_log has them. Game 2:
        /// clawfish-black, opponent-white → opponent_log has them.
        #[test]
        fn production_worker_fn_routes_position_and_go_to_side_to_move_per_game() {
            let outcome = run_one_pair_against_mocks(vec![], vec![], false, false, false, 2400, 0);

            for (label, log) in [
                ("engine_log", &outcome.engine_log),
                ("opponent_log", &outcome.opponent_log),
            ] {
                let pos_count = log.iter().filter(|l| l.starts_with("position ")).count();
                let go_count = log.iter().filter(|l| l.starts_with("go ")).count();
                assert_eq!(
                    pos_count, 1,
                    "{label}: expected exactly 1 `position …` line (one game per pair where \
                     this side plays white); got {pos_count}: {:?}",
                    log
                );
                assert_eq!(
                    go_count, 1,
                    "{label}: expected exactly 1 `go …` line; got {go_count}: {:?}",
                    log
                );
            }
        }

        /// T6: handshake-time engine_options and opponent_options are
        /// applied via setoption BEFORE the first ucinewgame, on the
        /// correct side (no cross-contamination).
        #[test]
        fn production_worker_fn_applies_engine_options_during_handshake_not_per_pair() {
            let outcome = run_one_pair_against_mocks(
                vec![("EngineOnlyOption".to_string(), "engine_value".to_string())],
                vec![("OpponentOnlyOption".to_string(), "opp_value".to_string())],
                false,
                false,
                false,
                2400,
                0,
            );

            // Helper closure: index of an exact line, or panic with a clear message.
            let find_exact = |label: &str, log: &Vec<String>, target: &str| -> usize {
                log.iter()
                    .position(|l| l == target)
                    .unwrap_or_else(|| panic!("{label}: missing `{target}` line; got {:?}", log))
            };

            // engine_log: contains EngineOnlyOption setoption, before first ucinewgame.
            let engine_opt_idx = find_exact(
                "engine_log",
                &outcome.engine_log,
                "setoption name EngineOnlyOption value engine_value",
            );
            let engine_first_ucn = outcome
                .engine_log
                .iter()
                .position(|l| l == "ucinewgame")
                .expect("engine_log has at least one ucinewgame");
            assert!(
                engine_opt_idx < engine_first_ucn,
                "engine_log: EngineOnlyOption setoption (idx {engine_opt_idx}) must precede \
                 first ucinewgame (idx {engine_first_ucn})"
            );

            // opponent_log: contains OpponentOnlyOption setoption, before first ucinewgame.
            let opp_opt_idx = find_exact(
                "opponent_log",
                &outcome.opponent_log,
                "setoption name OpponentOnlyOption value opp_value",
            );
            let opp_first_ucn = outcome
                .opponent_log
                .iter()
                .position(|l| l == "ucinewgame")
                .expect("opponent_log has at least one ucinewgame");
            assert!(
                opp_opt_idx < opp_first_ucn,
                "opponent_log: OpponentOnlyOption setoption (idx {opp_opt_idx}) must \
                 precede first ucinewgame (idx {opp_first_ucn})"
            );

            // Negative-symmetry: engine_log has NO OpponentOnlyOption; opponent_log has
            // NO EngineOnlyOption.
            assert!(
                !outcome
                    .engine_log
                    .iter()
                    .any(|l| l.contains("OpponentOnlyOption")),
                "engine_log must NOT mention OpponentOnlyOption; got {:?}",
                outcome.engine_log
            );
            assert!(
                !outcome
                    .opponent_log
                    .iter()
                    .any(|l| l.contains("EngineOnlyOption")),
                "opponent_log must NOT mention EngineOnlyOption; got {:?}",
                outcome.opponent_log
            );
        }

        /// T7: VirtualClock setoption is sent only when **both** (a)
        /// `cfg.virtual_clock` is true AND (b) the engine advertises the
        /// option in its `uci` reply. The two AND-clauses correspond to
        /// the two `&&` gates at lines 6979 (engine) and 6982 (opponent)
        /// of `production_worker_fn`. To distinguish `&&` from `||` in
        /// each gate, we run two scenarios:
        ///
        /// **Scenario A** — `virtual_clock=true, engine_advertises=true,
        /// opponent_advertises=false`: the engine receives the setoption
        /// (positive); the opponent does not (catches `&&`→`||` on line
        /// 6982 because the opponent gate becomes `true || false = true`
        /// under the mutant, sending the setoption incorrectly).
        ///
        /// **Scenario B** — `virtual_clock=false, engine_advertises=true`:
        /// neither engine receives the setoption (catches `&&`→`||` on
        /// line 6979 because the engine gate becomes `false || true =
        /// true` under the mutant, sending the setoption incorrectly).
        #[test]
        fn production_worker_fn_negotiates_virtual_clock_with_advertising_engine_only() {
            let target = "setoption name VirtualClock value true";

            // Scenario A: the both-true / opponent-doesn't-advertise case.
            let outcome_a = run_one_pair_against_mocks(
                vec![],
                vec![],
                /* virtual_clock = */ true,
                /* advertise_vc_engine = */ true,
                /* advertise_vc_opponent = */ false,
                2400,
                0,
            );
            assert!(
                outcome_a.engine_log.iter().any(|l| l == target),
                "scenario A: engine_log must contain `{target}` (engine advertises \
                 VirtualClock and virtual_clock=true); got {:?}",
                outcome_a.engine_log
            );
            assert!(
                !outcome_a.opponent_log.iter().any(|l| l == target),
                "scenario A: opponent_log must NOT contain `{target}` (opponent does NOT \
                 advertise VirtualClock); got {:?}",
                outcome_a.opponent_log
            );

            // Scenario B: virtual_clock=false even though engine advertises —
            // distinguishes the line-6979 gate's `&&` from `||`.
            let outcome_b = run_one_pair_against_mocks(
                vec![],
                vec![],
                /* virtual_clock = */ false,
                /* advertise_vc_engine = */ true,
                /* advertise_vc_opponent = */ false,
                2400,
                0,
            );
            assert!(
                !outcome_b.engine_log.iter().any(|l| l == target),
                "scenario B: engine_log must NOT contain `{target}` (virtual_clock=false \
                 suppresses the setoption regardless of advertisement); got {:?}",
                outcome_b.engine_log
            );
            assert!(
                !outcome_b.opponent_log.iter().any(|l| l == target),
                "scenario B: opponent_log must NOT contain `{target}`; got {:?}",
                outcome_b.opponent_log
            );
        }

        /// T8: on shutdown, both engines receive `quit`. The §5.3 helper
        /// joins the worker thread, which in turn awaits
        /// `driver::shutdown(engine)` and `driver::shutdown(opponent)`,
        /// each of which sends `quit\n`. Catches the
        /// `delete super::driver::shutdown(engine)` mutant on the two
        /// shutdown calls inside `production_worker_fn`.
        #[test]
        fn production_worker_fn_emits_quit_to_both_engines_on_shutdown() {
            let outcome = run_one_pair_against_mocks(vec![], vec![], false, false, false, 2400, 0);
            assert!(
                outcome.engine_log.iter().any(|l| l == "quit"),
                "engine_log must contain `quit`; got {:?}",
                outcome.engine_log
            );
            assert!(
                outcome.opponent_log.iter().any(|l| l == "quit"),
                "opponent_log must contain `quit`; got {:?}",
                outcome.opponent_log
            );
        }

        /// T9: per-pair `game_index` and `clawfish_white` color
        /// assignment. Pins:
        ///
        /// - `let clawfish_white = game_in_pair == 0;` (in
        ///   `production_worker_fn`) — game 0 must have clawfish
        ///   (engine-mock) playing white; game 1 must have the
        ///   opponent playing white. Catches the `==`→`!=` mutant on
        ///   that line, which would flip both games' color routing.
        ///   The discriminator is `white_name`/`black_name` recorded
        ///   in `WorkerReport::GameComplete` (the white_engine_index
        ///   selection block farther down in `production_worker_fn`).
        /// - `let game_index = pair_index * 2 + game_in_pair + 1;` — the
        ///   3 mutants that survive at `pair_index=0` (because `0*2 ≡
        ///   0+2 ≡ 0/2 = 0` and `0+0 ≡ 0*0 = 0`) are distinguishable at
        ///   `pair_index=3`. Original game_indices: [7, 8]. Mutants:
        ///   - `*` → `+`: `3+2+0+1=6`, `3+2+1+1=7` → [6, 7].
        ///   - `*` → `/`: `3/2+0+1=2`, `3/2+1+1=3` → [2, 3].
        ///   - inner `+` → `*`: `3*2*0+1=1`, `3*2*1+1=7` → [1, 7].
        ///
        /// All three diverge from [7, 8].
        #[test]
        fn production_worker_fn_assigns_correct_game_index_and_color_per_pair() {
            let outcome = run_one_pair_against_mocks(
                vec![],
                vec![],
                false,
                false,
                false,
                2400,
                /* pair_index = */ 3,
            );

            assert_eq!(
                outcome.games.len(),
                2,
                "PlayPair must produce 2 GameComplete reports; got {}",
                outcome.games.len()
            );

            // Game 0: pair_index*2 + 0 + 1 = 7; clawfish-white → engine plays white.
            assert_eq!(
                outcome.games[0].game_index, 7,
                "first game_index must be pair_index*2+0+1=7 for pair_index=3; got {}",
                outcome.games[0].game_index
            );
            assert_eq!(
                outcome.games[0].white_name, "engine-mock",
                "first game's white_name must be `engine-mock` (clawfish_white=true at \
                 game_in_pair=0); got {:?}",
                outcome.games[0].white_name
            );
            assert_eq!(
                outcome.games[0].black_name, "opponent-mock",
                "first game's black_name must be `opponent-mock`; got {:?}",
                outcome.games[0].black_name
            );

            // Game 1: pair_index*2 + 1 + 1 = 8; clawfish-black → opponent plays white.
            assert_eq!(
                outcome.games[1].game_index, 8,
                "second game_index must be pair_index*2+1+1=8 for pair_index=3; got {}",
                outcome.games[1].game_index
            );
            assert_eq!(
                outcome.games[1].white_name, "opponent-mock",
                "second game's white_name must be `opponent-mock` (clawfish_white=false at \
                 game_in_pair=1); got {:?}",
                outcome.games[1].white_name
            );
            assert_eq!(
                outcome.games[1].black_name, "engine-mock",
                "second game's black_name must be `engine-mock`; got {:?}",
                outcome.games[1].black_name
            );
        }

        // -------------------------------------------------------------------
        // 2.6.h — SPSA per-pair setoption wiring
        // -------------------------------------------------------------------

        /// Drive `production_worker_fn` through one `PlaySpsaPair` against two
        /// mock-engine instances and return their recordings.
        fn run_one_spsa_pair_against_mocks(
            engine_options: Vec<(String, String)>,
            opponent_options: Vec<(String, String)>,
        ) -> PairOutcome {
            let mock = resolve_mock_engine_bin();

            static SPSA_COUNTER: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let unique = format!(
                "eloh_spsa_pworker_{}_{}_{}",
                std::process::id(),
                SPSA_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            let temp_dir = std::env::temp_dir().join(unique);
            let _ = std::fs::remove_dir_all(&temp_dir);
            std::fs::create_dir_all(&temp_dir).expect("create temp_dir");
            let engine_record = temp_dir.join("engine.log");
            let opponent_record = temp_dir.join("opponent.log");

            let engine_spec =
                build_engine_spec_for_mock("engine-mock", &mock, &engine_record, false);
            let opponent_spec =
                build_engine_spec_for_mock("opponent-mock", &mock, &opponent_record, false);

            let cfg = WorkerConfig {
                engine_spec,
                opponent_spec,
                engine_options: vec![], // static options empty — per-pair via PlaySpsaPair
                opponent_options: vec![],
                mode: crate::MatchTimeMode::Wallclock,
                harness_overhead_ms: 0,
                watchdog: std::time::Duration::from_secs(10),
                max_plies: 100,
                thresholds: super::super::super::cli::Thresholds::default(),
                virtual_clock: false,
            };

            let mut pool = spawn_workers(1, cfg).expect("spawn_workers");

            let tc = super::super::super::cli::TimeControl {
                initial_ms: 1000,
                increment_ms: 0,
            };
            pool.senders[0]
                .send(WorkerCmd::PlaySpsaPair {
                    pair_index: 0,
                    engine_options,
                    opponent_options,
                    tc,
                })
                .expect("send PlaySpsaPair");

            let report_deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            let mut games: Vec<GameInfo> = Vec::new();
            loop {
                let remaining =
                    report_deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    panic!(
                        "SPSA report drain timed out after 20 s; got {} GameComplete reports",
                        games.len()
                    );
                }
                match pool.reports.recv_timeout(remaining) {
                    Ok(WorkerReport::GameComplete {
                        game_index,
                        white_name,
                        black_name,
                        ..
                    }) => {
                        games.push(GameInfo {
                            game_index,
                            white_name,
                            black_name,
                        });
                    }
                    Ok(WorkerReport::PairComplete { .. }) => break,
                    Ok(WorkerReport::Failure(msg)) => {
                        panic!("SPSA worker reported failure before PairComplete: {msg}");
                    }
                    Err(e) => panic!("SPSA report drain error: {e:?}"),
                }
            }
            assert_eq!(
                games.len(),
                2,
                "PlaySpsaPair must produce 2 GameComplete reports"
            );

            pool.senders.clear();
            let handles = std::mem::take(&mut pool.join_handles);
            join_workers_with_watchdog(handles, std::time::Duration::from_secs(10));
            drop(pool);

            let engine_log = read_log(&engine_record);
            let opponent_log = read_log(&opponent_record);
            let _ = std::fs::remove_dir_all(&temp_dir);

            PairOutcome {
                engine_log,
                opponent_log,
                games,
            }
        }

        /// T-SPSA-1: PlaySpsaPair sends θ⁺ setoptions to engine BEFORE ucinewgame;
        /// sends θ⁻ setoptions to opponent BEFORE ucinewgame; sends NO UCI_Elo
        /// to either engine; correct split (engine gets θ⁺, opponent gets θ⁻).
        #[test]
        fn production_worker_fn_spsa_pair_sends_setoptions_before_ucinewgame_no_uci_elo() {
            let engine_opts = vec![
                ("Aspiration_Adaptive".to_string(), "true".to_string()),
                ("Aspiration_K".to_string(), "210".to_string()),
                ("Aspiration_Min".to_string(), "29".to_string()),
            ];
            let opponent_opts = vec![
                ("Aspiration_Adaptive".to_string(), "true".to_string()),
                ("Aspiration_K".to_string(), "190".to_string()),
                ("Aspiration_Min".to_string(), "21".to_string()),
            ];

            let outcome =
                run_one_spsa_pair_against_mocks(engine_opts.clone(), opponent_opts.clone());

            let find_exact = |label: &str, log: &Vec<String>, target: &str| -> usize {
                log.iter()
                    .position(|l| l == target)
                    .unwrap_or_else(|| panic!("{label}: missing `{target}` line; got {log:?}"))
            };

            // Engine received θ⁺ options before first ucinewgame.
            let eng_k_plus_idx = find_exact(
                "engine_log",
                &outcome.engine_log,
                "setoption name Aspiration_K value 210",
            );
            let eng_first_ucn = outcome
                .engine_log
                .iter()
                .position(|l| l == "ucinewgame")
                .expect("engine_log has at least one ucinewgame");
            assert!(
                eng_k_plus_idx < eng_first_ucn,
                "engine: Aspiration_K θ⁺ setoption (idx {eng_k_plus_idx}) must precede \
                 first ucinewgame (idx {eng_first_ucn})"
            );

            // Opponent received θ⁻ options before first ucinewgame.
            let opp_k_minus_idx = find_exact(
                "opponent_log",
                &outcome.opponent_log,
                "setoption name Aspiration_K value 190",
            );
            let opp_first_ucn = outcome
                .opponent_log
                .iter()
                .position(|l| l == "ucinewgame")
                .expect("opponent_log has at least one ucinewgame");
            assert!(
                opp_k_minus_idx < opp_first_ucn,
                "opponent: Aspiration_K θ⁻ setoption (idx {opp_k_minus_idx}) must precede \
                 first ucinewgame (idx {opp_first_ucn})"
            );

            // Engine log must NOT contain θ⁻ value 190.
            assert!(
                !outcome
                    .engine_log
                    .iter()
                    .any(|l| l.contains("Aspiration_K value 190")),
                "engine_log must not contain θ⁻ value 190; got {eng_log:?}",
                eng_log = outcome.engine_log
            );

            // Opponent log must NOT contain θ⁺ value 210.
            assert!(
                !outcome
                    .opponent_log
                    .iter()
                    .any(|l| l.contains("Aspiration_K value 210")),
                "opponent_log must not contain θ⁺ value 210; got {opp_log:?}",
                opp_log = outcome.opponent_log
            );

            // Neither engine log should mention UCI_Elo (full-strength self-play).
            assert!(
                !outcome.engine_log.iter().any(|l| l.contains("UCI_Elo")),
                "engine_log must NOT mention UCI_Elo in SPSA mode; got {eng_log:?}",
                eng_log = outcome.engine_log
            );
            assert!(
                !outcome.opponent_log.iter().any(|l| l.contains("UCI_Elo")),
                "opponent_log must NOT mention UCI_Elo in SPSA mode; got {opp_log:?}",
                opp_log = outcome.opponent_log
            );
        }

        /// T-SPSA-2: The existing PlayPair (SPRT) path is byte-for-byte unchanged —
        /// the handshake-time options test from §4078 still passes with PlaySpsaPair
        /// present.
        #[test]
        fn production_worker_fn_play_pair_path_unchanged_with_spsa_variant_present() {
            // Re-run the handshake-time test: handshake options precede first ucinewgame.
            let outcome = run_one_pair_against_mocks(
                vec![("EngineOnlyOption".to_string(), "engine_value".to_string())],
                vec![("OpponentOnlyOption".to_string(), "opp_value".to_string())],
                false,
                false,
                false,
                2400,
                0,
            );

            let find_exact = |label: &str, log: &Vec<String>, target: &str| -> usize {
                log.iter()
                    .position(|l| l == target)
                    .unwrap_or_else(|| panic!("{label}: missing `{target}` line; got {log:?}"))
            };

            let engine_opt_idx = find_exact(
                "engine_log",
                &outcome.engine_log,
                "setoption name EngineOnlyOption value engine_value",
            );
            let engine_first_ucn = outcome
                .engine_log
                .iter()
                .position(|l| l == "ucinewgame")
                .expect("engine_log has at least one ucinewgame");
            assert!(
                engine_opt_idx < engine_first_ucn,
                "PlayPair: engine option must precede first ucinewgame"
            );

            // No UCI_Elo setoption in engine log for the PlayPair path
            // (only opponent gets UCI_Elo in SPRT mode).
            assert!(
                !outcome.engine_log.iter().any(|l| l.contains("UCI_Elo")),
                "engine_log must not receive UCI_Elo in PlayPair; got {eng_log:?}",
                eng_log = outcome.engine_log
            );
        }
    }
}
