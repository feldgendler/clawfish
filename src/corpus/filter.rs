//! Game-level + position-level filters (research §4).
//!
//! Implemented by the M6.G `filter+quiet` coder slice per
//! `docs/plans/m6.g.md` §3.3.

use crate::Position;
use crate::movegen::in_check;

use super::pgn::PgnTags;
use super::{HIGH_SCORE_CP, OPENING_SKIP_PLIES};

/// TC-class admitted for the corpus (research §4.1/§4.2).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TcClass {
    /// CCRL 40/15 + Lichess ≥ 5 min — the recommended quality band.
    Standard,
}

/// Game-admission policy.
#[derive(Clone, Debug)]
pub struct GameFilter {
    /// If `false`, `Termination "Time forfeit"`/`Abandoned` games are dropped.
    pub allow_time_forfeit: bool,
    /// Admitted time-control class.
    pub tc_class: TcClass,
    /// Minimum both-player Elo (`None` = no Elo gate).
    pub min_elo: Option<u32>,
}

impl Default for GameFilter {
    fn default() -> Self {
        Self {
            allow_time_forfeit: false,
            tc_class: TcClass::Standard,
            min_elo: Some(2000),
        }
    }
}

/// Return `true` iff the `TimeControl` tag string is Standard-class.
///
/// Admitted forms:
/// - CCRL-style `"moves/minutes"` e.g. `"40/15"` — base ≥ 300 s equivalent.
///   Specifically, 40/15 = 22.5 s/move average; any moves/minutes form is
///   admitted when the minutes value × 60 ≥ 300 (≥ 5 min total for the whole
///   time allotment).
/// - PGN increment `"base+inc"` (seconds) e.g. `"600+5"` — base ≥ 300.
/// - `"-"` (no TC / correspondence) — NOT admitted (unknown distribution).
fn tc_is_standard(tc_str: &str) -> bool {
    let s = tc_str.trim();

    // CCRL-style: "moves/minutes" e.g. "40/15"
    // Interpret the minutes field as the total time bank; require ≥ 5 min.
    if let Some((_, mins_str)) = s.split_once('/') {
        // minutes may be fractional ("40/2.5")
        if let Ok(mins) = mins_str.parse::<f64>() {
            return mins * 60.0 >= 300.0;
        }
        return false;
    }

    // PGN increment form: "base+inc" (seconds)
    if let Some((base_str, _)) = s.split_once('+') {
        if let Ok(base_secs) = base_str.parse::<u64>() {
            return base_secs >= 300;
        }
        return false;
    }

    // Plain seconds (no increment): "600"
    if let Ok(base_secs) = s.parse::<u64>() {
        return base_secs >= 300;
    }

    false
}

/// Whole-game admission: Termination (exclude time-forfeit/abandoned),
/// TC-class, Elo band. A game with no usable `Result` is dropped upstream
/// in `pgn`.
pub fn game_admitted(tags: &PgnTags, f: &GameFilter) -> bool {
    // Termination gate.
    if !f.allow_time_forfeit
        && let Some(term) = &tags.termination
    {
        let t = term.trim();
        if t.eq_ignore_ascii_case("Time forfeit") || t.eq_ignore_ascii_case("Abandoned") {
            return false;
        }
    }

    // TC-class gate.
    match f.tc_class {
        TcClass::Standard => match &tags.time_control {
            Some(tc) => {
                if !tc_is_standard(tc) {
                    return false;
                }
            }
            // Missing TC tag: cannot confirm Standard class → reject.
            None => return false,
        },
    }

    // Elo band gate.
    if let Some(min) = f.min_elo {
        match (tags.white_elo, tags.black_elo) {
            (Some(w), Some(b)) => {
                if w < min || b < min {
                    return false;
                }
            }
            // A missing Elo when min_elo is set ⇒ not admitted (plan §3.3).
            _ => return false,
        }
    }

    true
}

/// Position-level admission: `ply ≥ OPENING_SKIP_PLIES`,
/// `|static_eval| ≤ HIGH_SCORE_CP`, `!in_check`. (Quietness itself is
/// `quiet::is_quiet`.)
pub fn position_admitted(pos: &Position, ply: u32, static_eval_white: i32) -> bool {
    ply >= OPENING_SKIP_PLIES && static_eval_white.abs() <= HIGH_SCORE_CP && !in_check(pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{HIGH_SCORE_CP, OPENING_SKIP_PLIES, QUIET_MARGIN_CP};

    fn tags_normal(tc: &str, white_elo: u32, black_elo: u32) -> PgnTags {
        PgnTags {
            result: None,
            termination: Some("Normal".into()),
            white_elo: Some(white_elo),
            black_elo: Some(black_elo),
            time_control: Some(tc.into()),
        }
    }

    fn default_filter() -> GameFilter {
        GameFilter::default()
    }

    // -----------------------------------------------------------------------
    // interface_constants_golden — re-assert from filter's perspective
    // -----------------------------------------------------------------------
    #[test]
    fn interface_constants_golden() {
        assert_eq!(QUIET_MARGIN_CP, 30);
        assert_eq!(OPENING_SKIP_PLIES, 8);
        assert_eq!(HIGH_SCORE_CP, 600);
    }

    // -----------------------------------------------------------------------
    // Time-forfeit exclusion
    // -----------------------------------------------------------------------
    #[test]
    fn time_forfeit_excluded() {
        let mut tags = tags_normal("600+5", 2200, 2200);
        tags.termination = Some("Time forfeit".into());
        assert!(!game_admitted(&tags, &default_filter()));
    }

    #[test]
    fn abandoned_excluded() {
        let mut tags = tags_normal("600+5", 2200, 2200);
        tags.termination = Some("Abandoned".into());
        assert!(!game_admitted(&tags, &default_filter()));
    }

    #[test]
    fn time_forfeit_allowed_when_flag_set() {
        let mut tags = tags_normal("600+5", 2200, 2200);
        tags.termination = Some("Time forfeit".into());
        let f = GameFilter {
            allow_time_forfeit: true,
            ..default_filter()
        };
        assert!(game_admitted(&tags, &f));
    }

    #[test]
    fn normal_termination_admitted() {
        let tags = tags_normal("600+5", 2200, 2200);
        assert!(game_admitted(&tags, &default_filter()));
    }

    // -----------------------------------------------------------------------
    // TC-class boundary
    // -----------------------------------------------------------------------
    #[test]
    fn tc_class_300s_base_admitted() {
        // Exactly 300 s = 5 min: boundary admitted.
        let tags = tags_normal("300+0", 2200, 2200);
        assert!(game_admitted(&tags, &default_filter()));
    }

    #[test]
    fn tc_class_299s_base_rejected() {
        // 299 s < 300: rejected.
        let tags = tags_normal("299+3", 2200, 2200);
        assert!(!game_admitted(&tags, &default_filter()));
    }

    #[test]
    fn tc_class_ccrl_40_15_admitted() {
        // CCRL 40/15 = 15 minutes × 60 = 900 s ≥ 300.
        let tags = tags_normal("40/15", 2200, 2200);
        assert!(game_admitted(&tags, &default_filter()));
    }

    #[test]
    fn tc_class_ccrl_40_2_rejected() {
        // 40/2 = 2 × 60 = 120 s < 300.
        let tags = tags_normal("40/2", 2200, 2200);
        assert!(!game_admitted(&tags, &default_filter()));
    }

    #[test]
    fn tc_class_missing_rejected() {
        let mut tags = tags_normal("600+5", 2200, 2200);
        tags.time_control = None;
        assert!(!game_admitted(&tags, &default_filter()));
    }

    #[test]
    fn tc_class_plain_seconds_600_admitted() {
        // "600" (10 min) with no increment — plain seconds form.
        let tags = tags_normal("600", 2200, 2200);
        assert!(game_admitted(&tags, &default_filter()));
    }

    #[test]
    fn tc_class_plain_seconds_60_rejected() {
        // 60 s (1 min bullet) — rejected.
        let tags = tags_normal("60", 2200, 2200);
        assert!(!game_admitted(&tags, &default_filter()));
    }

    // -----------------------------------------------------------------------
    // Elo band
    // -----------------------------------------------------------------------
    #[test]
    fn elo_both_meet_min_admitted() {
        let f = GameFilter {
            min_elo: Some(2000),
            ..default_filter()
        };
        let tags = tags_normal("600+5", 2000, 2000);
        assert!(game_admitted(&tags, &f));
    }

    #[test]
    fn elo_white_below_min_rejected() {
        let f = GameFilter {
            min_elo: Some(2000),
            ..default_filter()
        };
        let tags = tags_normal("600+5", 1999, 2200);
        assert!(!game_admitted(&tags, &f));
    }

    #[test]
    fn elo_black_below_min_rejected() {
        let f = GameFilter {
            min_elo: Some(2000),
            ..default_filter()
        };
        let tags = tags_normal("600+5", 2200, 1999);
        assert!(!game_admitted(&tags, &f));
    }

    #[test]
    fn elo_missing_white_when_min_set_rejected() {
        // Missing Elo when min_elo is set ⇒ not admitted (plan §3.3).
        let f = GameFilter {
            min_elo: Some(2000),
            ..default_filter()
        };
        let mut tags = tags_normal("600+5", 2200, 2200);
        tags.white_elo = None;
        assert!(!game_admitted(&tags, &f));
    }

    #[test]
    fn elo_missing_black_when_min_set_rejected() {
        let f = GameFilter {
            min_elo: Some(2000),
            ..default_filter()
        };
        let mut tags = tags_normal("600+5", 2200, 2200);
        tags.black_elo = None;
        assert!(!game_admitted(&tags, &f));
    }

    #[test]
    fn elo_no_min_elo_gate_any_elo_admitted() {
        // No Elo gate: even missing Elo is fine.
        let f = GameFilter {
            min_elo: None,
            ..default_filter()
        };
        let mut tags = tags_normal("600+5", 1000, 1000);
        tags.white_elo = None;
        assert!(game_admitted(&tags, &f));
    }

    // -----------------------------------------------------------------------
    // position_admitted: opening-skip boundary (ply == 8 admitted, 7 not)
    // -----------------------------------------------------------------------
    #[test]
    fn opening_skip_ply_8_admitted() {
        // Start position is not in check and eval ≈ 0.
        let pos = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1")
            .expect("startpos FEN");
        assert!(position_admitted(&pos, 8, 0));
    }

    #[test]
    fn opening_skip_ply_7_rejected() {
        let pos = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1")
            .expect("startpos FEN");
        assert!(!position_admitted(&pos, 7, 0));
    }

    // -----------------------------------------------------------------------
    // position_admitted: |eval| boundary (600 admitted, 601 not)
    // -----------------------------------------------------------------------
    #[test]
    fn high_score_600_admitted() {
        let pos = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1")
            .expect("startpos FEN");
        assert!(position_admitted(&pos, 10, 600));
        assert!(position_admitted(&pos, 10, -600));
    }

    #[test]
    fn high_score_601_rejected() {
        let pos = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1")
            .expect("startpos FEN");
        assert!(!position_admitted(&pos, 10, 601));
        assert!(!position_admitted(&pos, 10, -601));
    }

    // -----------------------------------------------------------------------
    // position_admitted: in-check excluded
    // -----------------------------------------------------------------------
    #[test]
    fn in_check_position_rejected() {
        // White king on e1 in check from black rook on e8; black king on h8.
        let pos = Position::from_fen("4r2k/8/8/8/8/8/8/4K3 w - - 0 1").expect("check FEN");
        assert!(in_check(&pos), "sanity: should be in check");
        assert!(!position_admitted(&pos, 20, 0));
    }

    #[test]
    fn not_in_check_with_good_eval_admitted() {
        // Quiet position: startpos ply 10 eval 0.
        let pos = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1")
            .expect("startpos FEN");
        assert!(!in_check(&pos));
        assert!(position_admitted(&pos, 10, 0));
    }
}
