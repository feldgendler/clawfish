//! Butterfly-indexed history table for move ordering in M4.C.
//!
//! Implements the design committed in ADR-0019
//! (`docs/decisions/0019-history-heuristic.md`). Quiet moves that cause beta
//! cutoffs accumulate a `+depth*depth` bonus; quiets searched before the cutter
//! at the same node accumulate a symmetric malus. The combined effect improves
//! the ordering of the quiet-move pool across iterative-deepening iterations.
//!
//! The table is indexed `[side][from][to]` (butterfly + color dimension).
//! `Color::White as usize == 0`, `Color::Black as usize == 1`.
//! 2 × 64 × 64 × 2 bytes = 16 KiB; fits in Apple Silicon M4's 192 KiB L1d.

use crate::piece::Color;
use crate::square::Square;

/// Saturation cap for history scores.
///
/// Chosen at 16384 (= 2^14) per ADR-0019 (literature-standard cap; CPW +
/// MadChess + general practice). The value is strictly less than
/// `i16::MAX` (32767), leaving headroom so that arithmetic temporaries
/// computed in `i32` before the clamp lands cannot silently truncate.
///
/// The score scale of `negamax_move_order_score` is laid out so that:
/// captures (`mvv_lva_score + CAPTURE_OFFSET`) > KILLER0_SCORE >
/// KILLER1_SCORE > `MAX_HISTORY` ≥ history scores ≥ `-MAX_HISTORY`.
/// All four constants are defined in `src/search.rs`.
pub(crate) const MAX_HISTORY: i16 = 16384;

/// Butterfly-indexed history table: `[side][from][to]` → `i16` score.
///
/// Scores are signed. Positive values indicate that the move from `from` to
/// `to` by `side` has historically caused beta cutoffs; negative values
/// indicate that it has been tried and failed at cut-nodes. Both directions
/// are bounded by `[-MAX_HISTORY, MAX_HISTORY]`.
///
/// The table is allocated inline (on whatever heap frame owns the struct, not
/// on the Rust call stack) as part of `AlphaBetaMover`, which is boxed.
pub(crate) struct HistoryTable {
    entries: [[[i16; 64]; 64]; 2],
}

impl HistoryTable {
    /// Allocate a zeroed history table.
    ///
    /// All entries start at 0 — a neutral prior that places unobserved moves
    /// neither above nor below other unobserved moves in the quiet pool.
    pub(crate) fn new() -> Self {
        HistoryTable {
            entries: [[[0i16; 64]; 64]; 2],
        }
    }

    /// Zero every entry, resetting the table to the same state as `new()`.
    ///
    /// Called from `Search::reset()` on `ucinewgame` and between bench
    /// positions so history from one game or position does not bias the next.
    /// The body is `*self = Self::new()` rather than a manual loop so the
    /// compiler can lower it to a single `memset`-equivalent call.
    pub(crate) fn clear(&mut self) {
        *self = Self::new();
    }

    /// Return the history score for the move from `from` to `to` by `side`.
    ///
    /// Returns a value in `[-MAX_HISTORY, MAX_HISTORY]`. A score of 0 means
    /// the move has not yet been observed or that bonuses and maluses have
    /// exactly cancelled.
    #[inline]
    pub(crate) fn score(&self, side: Color, from: Square, to: Square) -> i16 {
        self.entries[side.index()][from.index() as usize][to.index() as usize]
    }

    /// Apply `bonus` to the entry for `(side, from, to)`, clamping the result
    /// to `[-MAX_HISTORY, MAX_HISTORY]`.
    ///
    /// `bonus` is `i32` so callers can pass `depth * depth` or
    /// `-(depth * depth)` (both fit in `i32`) without intermediate clamping.
    /// The clamp lands here, after the widened add, preventing `i16` overflow.
    #[inline]
    pub(crate) fn update(&mut self, side: Color, from: Square, to: Square, bonus: i32) {
        let entry = &mut self.entries[side.index()][from.index() as usize][to.index() as usize];
        let new_value = (*entry as i32 + bonus).clamp(-(MAX_HISTORY as i32), MAX_HISTORY as i32);
        *entry = new_value as i16;
    }
}

impl Default for HistoryTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::piece::Color;
    use crate::square::Square;

    #[test]
    fn history_table_default_is_all_zero() {
        let t = HistoryTable::new();
        // Spot-check: four corners and several interior triples.
        assert_eq!(t.score(Color::White, Square::A1, Square::A1), 0);
        assert_eq!(t.score(Color::White, Square::H8, Square::H8), 0);
        assert_eq!(t.score(Color::Black, Square::A1, Square::A1), 0);
        assert_eq!(t.score(Color::Black, Square::H8, Square::H8), 0);
        // Interior spot-checks across both sides.
        assert_eq!(t.score(Color::White, Square::E2, Square::E4), 0);
        assert_eq!(t.score(Color::White, Square::D7, Square::D5), 0);
        assert_eq!(t.score(Color::Black, Square::E7, Square::E5), 0);
        assert_eq!(t.score(Color::Black, Square::G1, Square::F3), 0);
    }

    #[test]
    fn history_table_update_with_zero_bonus_is_noop() {
        let mut t = HistoryTable::new();
        // Write a nonzero value, then apply a zero bonus; must be unchanged.
        t.update(Color::White, Square::E2, Square::E4, 50);
        t.update(Color::White, Square::E2, Square::E4, 0);
        assert_eq!(t.score(Color::White, Square::E2, Square::E4), 50);
    }

    #[test]
    fn history_table_update_positive_bonus_increments() {
        let mut t = HistoryTable::new();
        t.update(Color::White, Square::E2, Square::E4, 100);
        assert_eq!(t.score(Color::White, Square::E2, Square::E4), 100);
    }

    #[test]
    fn history_table_update_negative_bonus_decrements() {
        let mut t = HistoryTable::new();
        t.update(Color::White, Square::E2, Square::E4, -100);
        assert_eq!(t.score(Color::White, Square::E2, Square::E4), -100);
    }

    #[test]
    fn history_table_update_clamps_to_max_history() {
        let mut t = HistoryTable::new();
        // A wildly large bonus must saturate at MAX_HISTORY, not overflow.
        t.update(Color::White, Square::E2, Square::E4, 1_000_000);
        assert_eq!(t.score(Color::White, Square::E2, Square::E4), MAX_HISTORY);
    }

    #[test]
    fn history_table_update_clamps_to_negative_max_history() {
        let mut t = HistoryTable::new();
        t.update(Color::White, Square::E2, Square::E4, -1_000_000);
        assert_eq!(t.score(Color::White, Square::E2, Square::E4), -MAX_HISTORY);
    }

    #[test]
    fn history_table_update_clamps_after_running_total_overflow() {
        let mut t = HistoryTable::new();
        // Saturate upward, then try to push further — must stay at MAX_HISTORY.
        t.update(Color::White, Square::E2, Square::E4, MAX_HISTORY as i32);
        t.update(Color::White, Square::E2, Square::E4, 1000);
        assert_eq!(t.score(Color::White, Square::E2, Square::E4), MAX_HISTORY);
        // Symmetric: saturate downward, then push further negative.
        let mut t2 = HistoryTable::new();
        t2.update(Color::White, Square::D2, Square::D4, -(MAX_HISTORY as i32));
        t2.update(Color::White, Square::D2, Square::D4, -1000);
        assert_eq!(t2.score(Color::White, Square::D2, Square::D4), -MAX_HISTORY);
    }

    #[test]
    fn history_table_clear_zeroes_all_entries() {
        let mut t = HistoryTable::new();
        t.update(Color::White, Square::E2, Square::E4, 500);
        t.update(Color::Black, Square::E7, Square::E5, 300);
        t.update(Color::White, Square::D1, Square::H5, -200);
        t.update(Color::Black, Square::G8, Square::F6, 100);
        t.clear();
        assert_eq!(t.score(Color::White, Square::E2, Square::E4), 0);
        assert_eq!(t.score(Color::Black, Square::E7, Square::E5), 0);
        assert_eq!(t.score(Color::White, Square::D1, Square::H5), 0);
        assert_eq!(t.score(Color::Black, Square::G8, Square::F6), 0);
    }

    #[test]
    fn history_table_distinct_sides_are_independent() {
        let mut t = HistoryTable::new();
        t.update(Color::White, Square::E2, Square::E4, 100);
        // Black's entry for the same square pair must remain 0.
        assert_eq!(t.score(Color::White, Square::E2, Square::E4), 100);
        assert_eq!(t.score(Color::Black, Square::E2, Square::E4), 0);
    }

    #[test]
    fn history_table_distinct_squares_are_independent() {
        let mut t = HistoryTable::new();
        t.update(Color::White, Square::E2, Square::E4, 100);
        // Neighboring destination square must be unaffected.
        assert_eq!(t.score(Color::White, Square::E2, Square::E4), 100);
        assert_eq!(t.score(Color::White, Square::E2, Square::E3), 0);
    }

    #[test]
    fn history_table_size_is_16_kib() {
        // 2 sides × 64 from-squares × 64 to-squares × 2 bytes (i16) = 16384 bytes.
        assert_eq!(std::mem::size_of::<HistoryTable>(), 2 * 64 * 64 * 2);
    }

    #[test]
    fn history_table_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<HistoryTable>();
    }
}
