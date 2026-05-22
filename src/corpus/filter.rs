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

/// Minimum mainline length, in plies, for a game's result to be a usable label
/// (10 full moves — the `pgn-extract --minmoves 10` convention; ADR-0036
/// filter-spec amendment). Removes aborts / disconnects / mouse-slips that
/// escaped the `Abandoned` tag and ensures the game reached a real middlegame.
pub const MIN_GAME_PLIES: u32 = 20;

/// Game-admission policy.
#[derive(Clone, Debug)]
pub struct GameFilter {
    /// If `false`, `Termination "Time forfeit"`/`Abandoned`/`Rules infraction`
    /// games are dropped.
    pub allow_time_forfeit: bool,
    /// Admitted time-control class.
    pub tc_class: TcClass,
    /// If `true`, require a `TimeControl` tag of `tc_class` (Lichess); if
    /// `false`, the TC gate is skipped — for CCRL, whose PGN encodes the TC in
    /// `[Event]` (e.g. `"CCRL 40/15"`) and carries no `[TimeControl]` tag, so
    /// requiring one would reject every game (ADR-0036 filter-spec amendment).
    pub require_tc: bool,
    /// Minimum both-player Elo (`None` = no Elo gate).
    pub min_elo: Option<u32>,
    /// Minimum mainline plies (drops aborts; ADR-0036 amendment).
    pub min_plies: u32,
}

impl Default for GameFilter {
    fn default() -> Self {
        Self {
            allow_time_forfeit: false,
            tc_class: TcClass::Standard,
            require_tc: true,
            min_elo: Some(2000),
            min_plies: MIN_GAME_PLIES,
        }
    }
}

/// The CCRL-appropriate filter: same quality band, but the TC gate is skipped
/// (CCRL has no `[TimeControl]` tag). Elo (≥ 2000, a no-op for engine-scale
/// ratings) + min-length + the Termination/non-standard-start gates still apply.
pub fn ccrl_filter() -> GameFilter {
    GameFilter {
        require_tc: false,
        ..GameFilter::default()
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

/// Whole-game admission. `ply_count` is the game's position count
/// (`gp.positions.len()` = mainline plies + 1, the start position). Gates:
/// Termination blocklist, non-standard-start reject, min length, (optional)
/// TC-class, Elo band. A game with no usable `Result` is dropped upstream in
/// `pgn`. See ADR-0036 filter-spec amendment.
pub fn game_admitted(tags: &PgnTags, ply_count: usize, f: &GameFilter) -> bool {
    // Termination gate: admin-decided / non-played-out results.
    if !f.allow_time_forfeit
        && let Some(term) = &tags.termination
    {
        let t = term.trim();
        if t.eq_ignore_ascii_case("Time forfeit")
            || t.eq_ignore_ascii_case("Abandoned")
            || t.eq_ignore_ascii_case("Rules infraction")
        {
            return false;
        }
    }

    // Non-standard-start gate (universal variant filter + parser-correctness
    // safeguard — the parser replays from the standard start and IGNORES the
    // FEN, so a from-position game would mis-replay). `[SetUp "1"]` is the
    // PGN-spec marker; additionally reject any `[FEN]` that is not byte-for-byte
    // the standard start. Compare the FULL parsed `Position` (not just the
    // placement field) so a standard-placement-but-black-to-move / altered-
    // castling FEN without `[SetUp]` is still caught; an unparseable FEN is
    // rejected too.
    if tags.setup.as_deref() == Some("1") {
        return false;
    }
    if let Some(fen) = &tags.fen {
        match Position::from_fen(fen) {
            Ok(p) if p == Position::starting_position() => {}
            _ => return false,
        }
    }

    // Minimum length: the result must be connected to real play.
    if (ply_count.saturating_sub(1) as u32) < f.min_plies {
        return false;
    }

    // TC-class gate (Lichess; skipped for CCRL via `require_tc = false`).
    if f.require_tc {
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
            ..Default::default()
        }
    }

    fn default_filter() -> GameFilter {
        GameFilter::default()
    }

    /// Admission with a long mainline (100 positions = 99 plies) so the
    /// min-length gate is a no-op — the existing TC/Elo/Termination tests
    /// predate that gate and assert only their own dimension.
    fn admit(tags: &PgnTags, f: &GameFilter) -> bool {
        game_admitted(tags, 100, f)
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
        assert!(!admit(&tags, &default_filter()));
    }

    #[test]
    fn abandoned_excluded() {
        let mut tags = tags_normal("600+5", 2200, 2200);
        tags.termination = Some("Abandoned".into());
        assert!(!admit(&tags, &default_filter()));
    }

    #[test]
    fn time_forfeit_allowed_when_flag_set() {
        let mut tags = tags_normal("600+5", 2200, 2200);
        tags.termination = Some("Time forfeit".into());
        let f = GameFilter {
            allow_time_forfeit: true,
            ..default_filter()
        };
        assert!(admit(&tags, &f));
    }

    #[test]
    fn normal_termination_admitted() {
        let tags = tags_normal("600+5", 2200, 2200);
        assert!(admit(&tags, &default_filter()));
    }

    // -----------------------------------------------------------------------
    // TC-class boundary
    // -----------------------------------------------------------------------
    #[test]
    fn tc_class_300s_base_admitted() {
        // Exactly 300 s = 5 min: boundary admitted.
        let tags = tags_normal("300+0", 2200, 2200);
        assert!(admit(&tags, &default_filter()));
    }

    #[test]
    fn tc_class_299s_base_rejected() {
        // 299 s < 300: rejected.
        let tags = tags_normal("299+3", 2200, 2200);
        assert!(!admit(&tags, &default_filter()));
    }

    #[test]
    fn tc_class_ccrl_40_15_admitted() {
        // CCRL 40/15 = 15 minutes × 60 = 900 s ≥ 300.
        let tags = tags_normal("40/15", 2200, 2200);
        assert!(admit(&tags, &default_filter()));
    }

    #[test]
    fn tc_class_ccrl_40_2_rejected() {
        // 40/2 = 2 × 60 = 120 s < 300.
        let tags = tags_normal("40/2", 2200, 2200);
        assert!(!admit(&tags, &default_filter()));
    }

    #[test]
    fn tc_class_missing_rejected() {
        let mut tags = tags_normal("600+5", 2200, 2200);
        tags.time_control = None;
        assert!(!admit(&tags, &default_filter()));
    }

    #[test]
    fn tc_class_plain_seconds_600_admitted() {
        // "600" (10 min) with no increment — plain seconds form.
        let tags = tags_normal("600", 2200, 2200);
        assert!(admit(&tags, &default_filter()));
    }

    #[test]
    fn tc_class_plain_seconds_60_rejected() {
        // 60 s (1 min bullet) — rejected.
        let tags = tags_normal("60", 2200, 2200);
        assert!(!admit(&tags, &default_filter()));
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
        assert!(admit(&tags, &f));
    }

    #[test]
    fn elo_white_below_min_rejected() {
        let f = GameFilter {
            min_elo: Some(2000),
            ..default_filter()
        };
        let tags = tags_normal("600+5", 1999, 2200);
        assert!(!admit(&tags, &f));
    }

    #[test]
    fn elo_black_below_min_rejected() {
        let f = GameFilter {
            min_elo: Some(2000),
            ..default_filter()
        };
        let tags = tags_normal("600+5", 2200, 1999);
        assert!(!admit(&tags, &f));
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
        assert!(!admit(&tags, &f));
    }

    #[test]
    fn elo_missing_black_when_min_set_rejected() {
        let f = GameFilter {
            min_elo: Some(2000),
            ..default_filter()
        };
        let mut tags = tags_normal("600+5", 2200, 2200);
        tags.black_elo = None;
        assert!(!admit(&tags, &f));
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
        assert!(admit(&tags, &f));
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

    // -----------------------------------------------------------------------
    // ADR-0036 filter-spec amendment: Rules-infraction, min-length,
    // non-standard-start, CCRL no-TC.
    // -----------------------------------------------------------------------
    #[test]
    fn rules_infraction_excluded() {
        let mut tags = tags_normal("600+5", 2200, 2200);
        tags.termination = Some("Rules infraction".into());
        assert!(!admit(&tags, &default_filter()));
        // Case-insensitive, like the other Termination tokens.
        tags.termination = Some("rules INFRACTION".into());
        assert!(!admit(&tags, &default_filter()));
    }

    #[test]
    fn min_length_gate() {
        let tags = tags_normal("600+5", 2200, 2200);
        // ply_count = positions = plies + 1. MIN_GAME_PLIES = 20.
        assert!(
            !game_admitted(&tags, 20, &default_filter()),
            "19 plies rejected"
        );
        assert!(
            game_admitted(&tags, 21, &default_filter()),
            "20 plies admitted (boundary)"
        );
        assert!(game_admitted(&tags, 200, &default_filter()));
        // A 1-position (0-ply) game is rejected.
        assert!(!game_admitted(&tags, 1, &default_filter()));
    }

    #[test]
    fn setup_tag_rejected() {
        let mut tags = tags_normal("600+5", 2200, 2200);
        tags.setup = Some("1".into());
        assert!(
            !admit(&tags, &default_filter()),
            "[SetUp \"1\"] ⇒ from-position game"
        );
        // SetUp "0" (explicitly standard) is fine.
        tags.setup = Some("0".into());
        assert!(admit(&tags, &default_filter()));
    }

    #[test]
    fn non_startpos_fen_rejected_startpos_fen_ok() {
        let mut tags = tags_normal("600+5", 2200, 2200);
        // A from-position FEN (e.g. an endgame study) ⇒ rejected.
        tags.fen = Some("8/8/8/8/8/8/4K3/4k3 w - - 0 1".into());
        assert!(!admit(&tags, &default_filter()));
        // Standard PLACEMENT but black to move (not the standard start) — would
        // mis-replay (parser applies the first SAN as White) ⇒ rejected by the
        // full-Position comparison even without a `[SetUp]` tag.
        tags.fen = Some("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR b KQkq - 0 1".into());
        assert!(
            !admit(&tags, &default_filter()),
            "standard-placement, black-to-move rejected"
        );
        // An unparseable FEN ⇒ rejected (defensive).
        tags.fen = Some("not a fen".into());
        assert!(!admit(&tags, &default_filter()));
        // A redundant exact-startpos FEN ⇒ admitted.
        tags.fen = Some("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1".into());
        assert!(admit(&tags, &default_filter()));
    }

    #[test]
    fn ccrl_filter_admits_without_timecontrol_tag() {
        // The CCRL case: WhiteElo/BlackElo present (≥2000), Normal/absent
        // Termination, but NO TimeControl tag (CCRL puts the TC in [Event]).
        let mut tags = tags_normal("ignored", 2628, 2749);
        tags.time_control = None;
        // The default (Lichess) filter REQUIRES a TC tag ⇒ rejects.
        assert!(
            !admit(&tags, &default_filter()),
            "default filter requires a TimeControl tag"
        );
        // The CCRL filter skips the TC gate ⇒ admits.
        assert!(
            admit(&tags, &ccrl_filter()),
            "ccrl_filter admits CCRL games (no TC tag)"
        );
        // CCRL filter still enforces min-length + Elo + non-standard-start.
        assert!(
            !game_admitted(&tags, 5, &ccrl_filter()),
            "short CCRL game still rejected"
        );
        tags.setup = Some("1".into());
        assert!(
            !admit(&tags, &ccrl_filter()),
            "from-position CCRL game still rejected"
        );
    }
}
