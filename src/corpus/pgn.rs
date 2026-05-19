//! Streaming PGN reader (CCRL / Lichess shape): Seven-Tag-Roster + SAN
//! movetext → per-game position stream + original game-result label.
//!
//! SAN disambiguation resolves against the LEGAL move set (`generate_moves`
//! is legal-direct — ADR-0007, no pseudo-legal companion), so the
//! pinned-piece case is correct. ANY parse failure drops the WHOLE game and
//! increments `PgnStats.parse_failures` — never "skip the move and
//! continue" (that desynchronizes the stream and mislabels every later
//! position: the Texel-poisoning the roadmap warns of). Streaming, one game
//! buffered at a time (R6).
//!
//! Implemented by the M6.G `pgn` (Opus) coder slice per
//! `docs/plans/m6.g.md` §3.2.

use std::io::BufRead;

use crate::Position;

use super::{CorpusError, Label};

/// Extracted tag-roster fields relevant to filtering.
#[derive(Clone, Debug, Default)]
pub struct PgnTags {
    /// `Result` tag → label (`*`/missing ⇒ `None`, game dropped).
    pub result: Option<Label>,
    /// `Termination` tag (e.g. `"Normal"`, `"Time forfeit"`).
    pub termination: Option<String>,
    /// `WhiteElo` tag, parsed.
    pub white_elo: Option<u32>,
    /// `BlackElo` tag, parsed.
    pub black_elo: Option<u32>,
    /// `TimeControl` tag (PGN spec form, e.g. `"600+5"`).
    pub time_control: Option<String>,
}

/// Counters surfaced into `corpus_stats.txt` + the quality gate.
#[derive(Clone, Debug, Default)]
pub struct PgnStats {
    /// Games encountered (parsed or not).
    pub games_seen: u64,
    /// Games that parsed end-to-end with a usable result and were emitted.
    pub games_emitted: u64,
    /// Games dropped because a SAN/structure parse failed mid-game.
    pub parse_failures: u64,
    /// Games dropped because the result was `*`/missing.
    pub no_result_dropped: u64,
}

/// All positions of one fully-parsed game (label resolved, non-`*`).
pub struct GamePositions {
    /// Assigned game id (game-level split key).
    pub game_id: u64,
    /// The game's tag roster.
    pub tags: PgnTags,
    /// `(position, ply_from_start)` in game order.
    pub positions: Vec<(Position, u32)>,
}

/// Parse one SAN token against the legal move set of `pos`.
pub fn parse_san(
    _token: &str,
    _pos: &Position,
    _legal: &crate::MoveList,
) -> Result<crate::Move, CorpusError> {
    todo!("M6.G pgn slice")
}

/// Stream games from a PGN reader. `on_game` is invoked ONCE per game that
/// parses end-to-end with a non-`*` `Result`; a parse failure or `*`/
/// missing result drops the whole game (counted in `stats`). Returns the
/// next free `game_id`.
pub fn stream_pgn<R: BufRead>(
    _r: R,
    _base_game_id: u64,
    _on_game: &mut dyn FnMut(GamePositions),
    _stats: &mut PgnStats,
) -> Result<u64, CorpusError> {
    todo!("M6.G pgn slice")
}
