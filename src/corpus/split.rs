//! Game-level train/val split + held-out integrity (research §6.3).
//!
//! Split key is `game_id` (whole game → one split) so dedup-removal can
//! never make a game straddle splits. Game-disjointness is a HARD
//! must-PASS; post-split FEN-leakage ratio is reported and must be ≤ τ
//! (opening-transposition leakage is unavoidable, bounded not zero —
//! research §6.2).
//!
//! Implemented by the M6.G `split` coder slice per `docs/plans/m6.g.md` §3.8.

use super::CorpusRecord;

/// A game-level train/validation partition.
pub struct Split {
    /// Training records.
    pub train: Vec<CorpusRecord>,
    /// Held-out validation records.
    pub val: Vec<CorpusRecord>,
}

/// Integrity report for a [`Split`].
#[derive(Clone, Debug)]
pub struct SplitReport {
    /// `true` iff no `game_id` appears in both splits (HARD must-PASS).
    pub game_disjoint: bool,
    /// Fraction of val FENs also present in train (reported; ≤ τ).
    pub fen_leakage_ratio: f64,
    /// Distinct games in train.
    pub train_games: u64,
    /// Distinct games in val.
    pub val_games: u64,
}

/// Assign whole games to train/val by seeded hash of `game_id`.
pub fn split_by_game(_records: Vec<CorpusRecord>, _val_fraction: f64, _seed: u64) -> Split {
    todo!("M6.G split slice")
}

/// Game-disjointness (hard) + FEN-leakage ratio (reported).
pub fn split_integrity(_split: &Split) -> SplitReport {
    todo!("M6.G split slice")
}
