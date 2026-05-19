//! Game-level + position-level filters (research §4).
//!
//! Implemented by the M6.G `filter+quiet` coder slice per
//! `docs/plans/m6.g.md` §3.3.

use crate::Position;

use super::pgn::PgnTags;

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

/// Whole-game admission: Termination (exclude time-forfeit/abandoned),
/// TC-class, Elo band. A game with no usable `Result` is dropped upstream
/// in `pgn`.
pub fn game_admitted(_tags: &PgnTags, _f: &GameFilter) -> bool {
    todo!("M6.G filter slice")
}

/// Position-level admission: `ply ≥ OPENING_SKIP_PLIES`,
/// `|static_eval| ≤ HIGH_SCORE_CP`, `!in_check`. (Quietness itself is
/// `quiet::is_quiet`.)
pub fn position_admitted(_pos: &Position, _ply: u32, _static_eval_white: i32) -> bool {
    todo!("M6.G filter slice")
}
