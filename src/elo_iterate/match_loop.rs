//! Colour-paired game loop, per-side clock management, and time-forfeit logic.
//!
//! The main entry point is `play_one_game`. A `pure_apply_move_clock_update`
//! helper is factored out for deterministic unit testing with synthetic Instants.

use std::time::{Duration, Instant};

use crate::{Color, MatchTimeMode, PerSideClock};

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
    let new_remaining = prior_clock.remaining_ms - elapsed_ms + i64::from(prior_clock.increment_ms);
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
pub(crate) fn play_one_game(ctx: &mut GameContext<'_>) -> (GameOutcome, Vec<super::pgn::PgnMove>) {
    use super::adjudicate::{
        detect_native_game_over, draw_threshold_check, resign_threshold_check,
    };
    use super::driver::{recv_until_bestmove, send_line};
    use super::pgn::PgnMove;
    use crate::{Color, Move, Position, generate_moves};

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
                let mut legal = crate::MoveList::new();
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
                GameOutcome::NativeGameOver(super::adjudicate::GameOver::ResignAdjudicated(side)),
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
    use crate::MatchTimeMode;

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
