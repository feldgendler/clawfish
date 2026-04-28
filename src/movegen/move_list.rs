//! `MoveList` — stack-allocated fixed-capacity buffer for legal moves.
//!
//! 256-slot capacity, covering the proven 218-move ceiling (Petrović 1964;
//! integer-programming proof 2025) with comfortable headroom. `MaybeUninit<Move>`
//! storage avoids the 256× `Move::default()` initialization cost on every
//! `MoveList::new()` call.
//!
//! See `docs/plans/m1.f.md` §2 for the full design rationale.

use std::fmt;
use std::mem::MaybeUninit;

use crate::mov::Move;

/// Stack-allocated fixed-capacity move buffer. Capacity 256 covers the
/// proven max legal-move count of 218 plus headroom — though we only emit
/// legal moves so 218 is the true ceiling.
///
/// `len: u16` (not `u8`): 256 is not representable in a `u8`, so a `u8 len`
/// would be off-by-one. `u16` is two bytes, costs nothing relative to the
/// 512-byte `moves` array, and admits 0..=256 cleanly.
///
/// **Soundness contract:** indices `0..len` are always initialized via
/// `push`. `as_slice` reads through `MaybeUninit::slice_assume_init_ref`
/// over `&self.moves[..len]`. `push` and `clear` are `pub(crate)` so the
/// soundness contract stays inside the crate; external consumers read via
/// `as_slice` / `iter` / `len` / `is_empty` only.
pub struct MoveList {
    moves: [MaybeUninit<Move>; 256],
    len: u16,
}

impl MoveList {
    /// New empty `MoveList`. `len = 0`; the `moves` backing array contains
    /// uninitialized bytes (sound — only indices `0..len` are ever read).
    pub const fn new() -> MoveList {
        MoveList {
            moves: [const { MaybeUninit::uninit() }; 256],
            len: 0,
        }
    }

    /// Append `mv`. Debug-asserts `len < 256`; release-build over-push is
    /// UB by contract (the 218-move ceiling makes over-push unreachable in
    /// practice).
    pub(crate) fn push(&mut self, mv: Move) {
        debug_assert!(
            (self.len as usize) < 256,
            "MoveList::push over-capacity: len={} (218-move legal ceiling exceeded)",
            self.len
        );
        self.moves[self.len as usize].write(mv);
        self.len += 1;
    }

    /// Number of moves currently stored.
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// Returns `true` if no moves have been pushed.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// All pushed moves, in push order. Empty for a fresh / cleared list.
    pub fn as_slice(&self) -> &[Move] {
        let initialized = &self.moves[..self.len as usize];
        // SAFETY: `push` is the sole producer of `moves[i]` for `i < len`,
        // and writes a fully-initialized `Move` via `MaybeUninit::write`
        // before bumping `len`. `clear` only ever decreases `len` (to 0),
        // never re-exposing uninitialized slots through `as_slice`. The
        // debug-asserted invariant `len <= 256` keeps the slice index in
        // bounds; release-build over-push is the documented UB-by-contract
        // surface (see type-level docstring). `MaybeUninit<T>` is
        // guaranteed by the standard library to have the same layout as
        // `T`, so `&[MaybeUninit<T>]` and `&[T]` are layout-compatible at
        // the slice level — the cast below is the equivalent of the
        // (unstable) `MaybeUninit::slice_assume_init_ref`.
        unsafe { &*(initialized as *const [MaybeUninit<Move>] as *const [Move]) }
    }

    /// Iterator over pushed moves by value. `Move` is `Copy + 2 bytes`;
    /// the by-value form matches `Bitboard::iter`'s `Square`-by-value
    /// convention.
    pub fn iter(&self) -> impl Iterator<Item = Move> + '_ {
        self.as_slice().iter().copied()
    }

    pub(crate) fn clear(&mut self) {
        self.len = 0;
    }
}

impl Default for MoveList {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for MoveList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MoveList")
            .field("len", &self.len)
            .field("moves", &self.as_slice())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mov::{Move, MoveFlag};
    use crate::square::Square;

    /// Helper: construct a quiet `Move` from two square indices in 0..64.
    /// Used to fabricate distinct, syntactically valid moves for the
    /// capacity test without caring about chess legality.
    fn fake_move(from_idx: u8, to_idx: u8) -> Move {
        let from = Square::new(from_idx % 64).expect("0..64 is in range");
        // Ensure to != from so encoding round-trip is unambiguous.
        let to = Square::new((to_idx % 64).wrapping_add(1) % 64).expect("0..64 is in range");
        Move::new(from, to, MoveFlag::Quiet)
    }

    #[test]
    fn move_list_empty_on_new() {
        let ml = MoveList::new();
        assert_eq!(ml.len(), 0, "fresh list len must be 0");
        assert!(ml.is_empty(), "fresh list must report is_empty");
        assert_eq!(ml.as_slice().len(), 0, "fresh list slice must be empty");
    }

    #[test]
    fn move_list_push_increments_len() {
        let mut ml = MoveList::new();
        let m1 = Move::new(Square::E2, Square::E4, MoveFlag::DoublePush);
        let m2 = Move::new(Square::G1, Square::F3, MoveFlag::Quiet);

        ml.push(m1);
        assert_eq!(ml.len(), 1, "len after one push must be 1");
        assert!(!ml.is_empty(), "list with one move must not be empty");

        ml.push(m2);
        assert_eq!(ml.len(), 2, "len after two pushes must be 2");
    }

    #[test]
    fn move_list_push_round_trips() {
        let mut ml = MoveList::new();
        let m1 = Move::new(Square::E2, Square::E4, MoveFlag::DoublePush);
        let m2 = Move::new(Square::G1, Square::F3, MoveFlag::Quiet);
        let m3 = Move::new(Square::B1, Square::C3, MoveFlag::Quiet);

        ml.push(m1);
        ml.push(m2);
        ml.push(m3);

        let slice = ml.as_slice();
        assert_eq!(slice.len(), 3, "slice length must equal pushed count");
        assert_eq!(slice[0], m1, "index 0 must hold the first pushed move");
        assert_eq!(slice[1], m2, "index 1 must hold the second pushed move");
        assert_eq!(slice[2], m3, "index 2 must hold the third pushed move");
    }

    #[test]
    fn move_list_clear_resets_len() {
        let mut ml = MoveList::new();
        ml.push(Move::new(Square::E2, Square::E4, MoveFlag::DoublePush));
        ml.push(Move::new(Square::G1, Square::F3, MoveFlag::Quiet));
        assert_eq!(ml.len(), 2);

        ml.clear();
        assert_eq!(ml.len(), 0, "clear must reset len to 0");
        assert!(ml.is_empty(), "clear must make is_empty return true");
        assert_eq!(ml.as_slice().len(), 0, "slice after clear must be empty");
    }

    #[test]
    fn move_list_iter_yields_all_pushed() {
        let mut ml = MoveList::new();
        let moves = [
            Move::new(Square::E2, Square::E4, MoveFlag::DoublePush),
            Move::new(Square::G1, Square::F3, MoveFlag::Quiet),
            Move::new(Square::B1, Square::C3, MoveFlag::Quiet),
            Move::new(Square::D2, Square::D4, MoveFlag::DoublePush),
        ];
        for &m in &moves {
            ml.push(m);
        }

        let collected: Vec<Move> = ml.iter().collect();
        assert_eq!(
            collected.len(),
            moves.len(),
            "iter must yield exactly len() items"
        );
        for (i, &m) in moves.iter().enumerate() {
            assert_eq!(collected[i], m, "iter item {} must equal pushed move", i);
        }
    }

    #[test]
    fn move_list_full_capacity_holds_256_moves() {
        let mut ml = MoveList::new();
        let mut pushed: Vec<Move> = Vec::with_capacity(256);
        for i in 0..256u32 {
            // Generate 256 distinct moves: walk through (from, to) combinations
            // such that distinct iterations yield distinct Move bit patterns.
            let from_idx = (i % 64) as u8;
            let to_idx = ((i / 64) * 16 + (i % 16)) as u8 % 64;
            let mv = fake_move(from_idx, to_idx);
            ml.push(mv);
            pushed.push(mv);
        }
        assert_eq!(ml.len(), 256, "len must equal 256 after 256 pushes");
        assert_eq!(
            ml.as_slice().len(),
            256,
            "as_slice().len() must equal 256 after 256 pushes"
        );
        // Content round-trip: each pushed move at index i must be readable
        // at as_slice()[i]. Catches a `MaybeUninit`/`assume_init` bug that
        // would still pass the length-only assertions above.
        let slice = ml.as_slice();
        for (i, &mv) in pushed.iter().enumerate() {
            assert_eq!(
                slice[i], mv,
                "as_slice()[{i}] must match the i-th pushed move"
            );
        }
    }

    #[test]
    fn move_list_debug_snapshot() {
        // Exercise the `Debug` impl. The exact format isn't load-bearing,
        // but rendering once with known content pins it as non-panicking
        // and closes a coverage gap on the impl.
        let mut ml = MoveList::new();
        ml.push(Move::new(Square::E2, Square::E4, MoveFlag::DoublePush));
        let s = format!("{:?}", ml);
        assert!(
            s.contains("MoveList"),
            "Debug should mention type name: {s}"
        );
        assert!(s.contains("len: 1"), "Debug should include len field: {s}");
    }

    #[test]
    fn move_list_default_equals_new() {
        let from_default = MoveList::default();
        let from_new = MoveList::new();
        assert_eq!(
            from_default.len(),
            from_new.len(),
            "default() must produce an empty list (len matches new())"
        );
        assert!(
            from_default.is_empty(),
            "default() must produce an empty list"
        );
        assert_eq!(
            from_default.as_slice().len(),
            0,
            "default() slice must be empty"
        );
    }
}
