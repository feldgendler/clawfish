//! Bitboard type and bit-manipulation operations. A bitboard is a `u64`
//! with one bit per board square (LSB = a1, MSB = h8 — LERF layout).

use crate::square::Square;
use std::fmt;
use std::ops;

/// A set of up to 64 squares, stored as a `u64` bitmask (LSB = a1, MSB = h8).
#[derive(Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct Bitboard(pub u64);

impl Bitboard {
    /// Bitboard with no bits set (the empty set).
    pub const EMPTY: Bitboard = Bitboard(0);
    /// Bitboard with all 64 bits set.
    pub const FULL: Bitboard = Bitboard(!0);

    /// All 8 squares on the a-file.
    pub const FILE_A: Bitboard = Bitboard(0x0101_0101_0101_0101);
    /// All 8 squares on the b-file.
    pub const FILE_B: Bitboard = Bitboard(0x0202_0202_0202_0202);
    /// All 8 squares on the c-file.
    pub const FILE_C: Bitboard = Bitboard(0x0404_0404_0404_0404);
    /// All 8 squares on the d-file.
    pub const FILE_D: Bitboard = Bitboard(0x0808_0808_0808_0808);
    /// All 8 squares on the e-file.
    pub const FILE_E: Bitboard = Bitboard(0x1010_1010_1010_1010);
    /// All 8 squares on the f-file.
    pub const FILE_F: Bitboard = Bitboard(0x2020_2020_2020_2020);
    /// All 8 squares on the g-file.
    pub const FILE_G: Bitboard = Bitboard(0x4040_4040_4040_4040);
    /// All 8 squares on the h-file.
    pub const FILE_H: Bitboard = Bitboard(0x8080_8080_8080_8080);

    /// All 8 squares on rank 1 (White's back rank).
    pub const RANK_1: Bitboard = Bitboard(0x0000_0000_0000_00FF);
    /// All 8 squares on rank 2 (White's pawn starting rank).
    pub const RANK_2: Bitboard = Bitboard(0x0000_0000_0000_FF00);
    /// All 8 squares on rank 3.
    pub const RANK_3: Bitboard = Bitboard(0x0000_0000_00FF_0000);
    /// All 8 squares on rank 4.
    pub const RANK_4: Bitboard = Bitboard(0x0000_0000_FF00_0000);
    /// All 8 squares on rank 5.
    pub const RANK_5: Bitboard = Bitboard(0x0000_00FF_0000_0000);
    /// All 8 squares on rank 6.
    pub const RANK_6: Bitboard = Bitboard(0x0000_FF00_0000_0000);
    /// All 8 squares on rank 7 (Black's pawn starting rank).
    pub const RANK_7: Bitboard = Bitboard(0x00FF_0000_0000_0000);
    /// All 8 squares on rank 8 (Black's back rank).
    pub const RANK_8: Bitboard = Bitboard(0xFF00_0000_0000_0000);

    /// Light squares in LERF (a1=0). a1 is dark by chess convention, so the
    /// light squares are those where `(file + rank) % 2 == 1` — bits {1, 3,
    /// 5, 7, 8, 10, 12, 14, …}. The rank-1 byte is `0b10101010 = 0xAA`
    /// (b1, d1, f1, h1 light); rank-2 byte is `0b01010101 = 0x55` (a2, c2,
    /// e2, g2 light); the pattern alternates byte-by-byte up the board.
    pub const LIGHT_SQUARES: Bitboard = Bitboard(0x55AA_55AA_55AA_55AA);
    /// Dark squares: complement of `LIGHT_SQUARES`. a1, c1, e1, g1, b2, …
    /// are dark.
    pub const DARK_SQUARES: Bitboard = Bitboard(0xAA55_AA55_AA55_AA55);

    /// Return a bitboard with only `sq` set.
    #[must_use]
    #[inline]
    pub const fn from_square(sq: Square) -> Bitboard {
        Bitboard(1u64 << sq.index())
    }

    /// Return `true` if `sq` is a member of this bitboard.
    #[inline]
    pub const fn contains(self, sq: Square) -> bool {
        (self.0 >> sq.index()) & 1 == 1
    }

    /// Return this bitboard with `sq` added (no change if already set).
    #[must_use]
    #[inline]
    pub const fn with(self, sq: Square) -> Bitboard {
        Bitboard(self.0 | (1u64 << sq.index()))
    }

    /// Return this bitboard with `sq` removed (no change if already clear).
    #[must_use]
    #[inline]
    pub const fn without(self, sq: Square) -> Bitboard {
        Bitboard(self.0 & !(1u64 << sq.index()))
    }

    /// Return the number of squares in this bitboard.
    #[inline]
    pub const fn count(self) -> u32 {
        self.0.count_ones()
    }

    /// Return `true` if the bitboard contains no squares.
    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Return `true` if the bitboard contains at least one square.
    #[inline]
    pub const fn any(self) -> bool {
        self.0 != 0
    }

    /// Return the least-significant set square without modifying the bitboard, or `None` if empty.
    #[inline]
    pub const fn lsb(self) -> Option<Square> {
        if self.0 == 0 {
            None
        } else {
            Some(Square::new_unchecked(self.0.trailing_zeros() as u8))
        }
    }

    /// Remove and return the least-significant set square, or `None` if empty.
    #[inline]
    pub fn pop_lsb(&mut self) -> Option<Square> {
        match self.lsb() {
            None => None,
            Some(sq) => {
                self.0 &= self.0 - 1;
                Some(sq)
            }
        }
    }

    /// Return an iterator over the set squares in LSB-to-MSB (a1-to-h8) order.
    #[inline]
    pub fn iter(self) -> Squares {
        Squares(self)
    }

    /// Shift all squares one rank toward rank 8; squares on rank 8 are lost.
    #[must_use]
    #[inline]
    pub const fn shift_north(self) -> Bitboard {
        Bitboard(self.0 << 8)
    }

    /// Shift all squares one rank toward rank 1; squares on rank 1 are lost.
    #[must_use]
    #[inline]
    pub const fn shift_south(self) -> Bitboard {
        Bitboard(self.0 >> 8)
    }

    /// Shift all squares one file toward the h-file; squares on the h-file are lost.
    #[must_use]
    #[inline]
    pub const fn shift_east(self) -> Bitboard {
        Bitboard((self.0 & !Self::FILE_H.0) << 1)
    }

    /// Shift all squares one file toward the a-file; squares on the a-file are lost.
    #[must_use]
    #[inline]
    pub const fn shift_west(self) -> Bitboard {
        Bitboard((self.0 & !Self::FILE_A.0) >> 1)
    }

    /// Shift all squares one step toward rank 8 and the h-file; edge squares are lost.
    #[must_use]
    #[inline]
    pub const fn shift_north_east(self) -> Bitboard {
        Bitboard((self.0 & !Self::FILE_H.0) << 9)
    }

    /// Shift all squares one step toward rank 8 and the a-file; edge squares are lost.
    #[must_use]
    #[inline]
    pub const fn shift_north_west(self) -> Bitboard {
        Bitboard((self.0 & !Self::FILE_A.0) << 7)
    }

    /// Shift all squares one step toward rank 1 and the h-file; edge squares are lost.
    #[must_use]
    #[inline]
    pub const fn shift_south_east(self) -> Bitboard {
        Bitboard((self.0 & !Self::FILE_H.0) >> 7)
    }

    /// Shift all squares one step toward rank 1 and the a-file; edge squares are lost.
    #[must_use]
    #[inline]
    pub const fn shift_south_west(self) -> Bitboard {
        Bitboard((self.0 & !Self::FILE_A.0) >> 9)
    }
}

// ---------------------------------------------------------------------------
// Fill / front-span primitives (M6.B, ADR-0032). General-purpose: also used
// by M6.C/M6.E. Stubbed for the test-first gate — Slice A implements.
//
// `#[allow(dead_code)]` per fn: the non-test consumers (`eval::pawns` term
// helpers) land in Slice C; the in-module tests exercise them now. Same
// rationale as the plan-mandated allow on `AlphaBetaMover::pawn_hash`.
// ---------------------------------------------------------------------------

/// Northward (toward rank 8) flood fill: every square at or above an input
/// square's rank, on that square's file.
#[allow(dead_code)]
pub(crate) const fn north_fill(bb: Bitboard) -> Bitboard {
    // Kogge-Stone fill: 7 shifts cover all 8 ranks.
    let bb = Bitboard(bb.0 | (bb.0 << 8));
    let bb = Bitboard(bb.0 | (bb.0 << 16));
    Bitboard(bb.0 | (bb.0 << 32))
}

/// Southward (toward rank 1) flood fill.
#[allow(dead_code)]
pub(crate) const fn south_fill(bb: Bitboard) -> Bitboard {
    let bb = Bitboard(bb.0 | (bb.0 >> 8));
    let bb = Bitboard(bb.0 | (bb.0 >> 16));
    Bitboard(bb.0 | (bb.0 >> 32))
}

/// Full-file fill: every square on any file occupied by an input square
/// (`north_fill | south_fill`). Idempotent.
#[allow(dead_code)]
pub(crate) const fn file_fill(bb: Bitboard) -> Bitboard {
    Bitboard(north_fill(bb).0 | south_fill(bb).0)
}

/// White front spans: squares strictly ahead (toward rank 8) of each input
/// pawn on its own file. **Excludes** the pawn's own square (off-by-one
/// guard — e5 → {e6,e7,e8}).
#[allow(dead_code)]
pub(crate) const fn white_front_spans(bb: Bitboard) -> Bitboard {
    // Shift one step north first so the pawn's own square is excluded.
    north_fill(bb.shift_north())
}

/// Black front spans: squares strictly ahead (toward rank 1) of each input
/// pawn on its own file. Excludes the pawn's own square.
#[allow(dead_code)]
pub(crate) const fn black_front_spans(bb: Bitboard) -> Bitboard {
    south_fill(bb.shift_south())
}

/// White attack-front spans: the white front spans widened east and west
/// (the diagonal capture files ahead of each pawn).
#[allow(dead_code)]
pub(crate) const fn white_attack_front_spans(bb: Bitboard) -> Bitboard {
    let spans = white_front_spans(bb);
    // shift_east/west already mask FILE_H/FILE_A — no wrap.
    Bitboard(spans.0 | spans.shift_east().0 | spans.shift_west().0)
}

/// Black attack-front spans: black front spans widened east and west.
#[allow(dead_code)]
pub(crate) const fn black_attack_front_spans(bb: Bitboard) -> Bitboard {
    let spans = black_front_spans(bb);
    Bitboard(spans.0 | spans.shift_east().0 | spans.shift_west().0)
}

impl ops::BitAnd for Bitboard {
    type Output = Bitboard;
    #[inline]
    fn bitand(self, rhs: Bitboard) -> Bitboard {
        Bitboard(self.0 & rhs.0)
    }
}

impl ops::BitOr for Bitboard {
    type Output = Bitboard;
    #[inline]
    fn bitor(self, rhs: Bitboard) -> Bitboard {
        Bitboard(self.0 | rhs.0)
    }
}

impl ops::BitXor for Bitboard {
    type Output = Bitboard;
    #[inline]
    fn bitxor(self, rhs: Bitboard) -> Bitboard {
        Bitboard(self.0 ^ rhs.0)
    }
}

impl ops::Not for Bitboard {
    type Output = Bitboard;
    #[inline]
    fn not(self) -> Bitboard {
        Bitboard(!self.0)
    }
}

impl ops::BitAndAssign for Bitboard {
    #[inline]
    fn bitand_assign(&mut self, rhs: Bitboard) {
        self.0 &= rhs.0;
    }
}

impl ops::BitOrAssign for Bitboard {
    #[inline]
    fn bitor_assign(&mut self, rhs: Bitboard) {
        self.0 |= rhs.0;
    }
}

impl ops::BitXorAssign for Bitboard {
    #[inline]
    fn bitxor_assign(&mut self, rhs: Bitboard) {
        self.0 ^= rhs.0;
    }
}

impl fmt::Debug for Bitboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for rank in (0..8u8).rev() {
            write!(f, "{}", rank + 1)?;
            for file in 0..8u8 {
                let index = rank * 8 + file;
                let cell = if self.0 & (1u64 << index) != 0 {
                    'X'
                } else {
                    '.'
                };
                write!(f, " {}", cell)?;
            }
            writeln!(f)?;
        }
        writeln!(f, "  a b c d e f g h")?;
        Ok(())
    }
}

impl fmt::Display for Bitboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Bitboard(0x{:016x})", self.0)
    }
}

/// Iterator over the squares of a [`Bitboard`], yielded in LSB-to-MSB order.
#[derive(Clone)]
pub struct Squares(Bitboard);

impl Iterator for Squares {
    type Item = Square;
    #[inline]
    fn next(&mut self) -> Option<Square> {
        self.0.pop_lsb()
    }
}

impl std::iter::FusedIterator for Squares {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::square::Square;

    #[test]
    fn empty_full_basics() {
        assert_eq!(Bitboard::EMPTY.count(), 0);
        assert_eq!(Bitboard::FULL.count(), 64);
        assert!(Bitboard::EMPTY.is_empty());
        assert!(!Bitboard::EMPTY.any());
        assert!(!Bitboard::FULL.is_empty());
        assert!(Bitboard::FULL.any());
    }

    #[test]
    fn default_is_empty() {
        assert_eq!(Bitboard::default(), Bitboard::EMPTY);
    }

    #[test]
    fn set_clear_test() {
        assert!(Bitboard::EMPTY.with(Square::E4).contains(Square::E4));
        assert!(!Bitboard::FULL.without(Square::E4).contains(Square::E4));
    }

    #[test]
    fn with_is_idempotent_on_set_bit() {
        // Pins `|`-not-`^` in `with`: applying to an already-set bit must
        // leave the bit set. Under `^`, the second call would clear it.
        let bb = Bitboard::from_square(Square::E4);
        let twice = bb.with(Square::E4);
        assert!(twice.contains(Square::E4));
        assert_eq!(bb, twice);
    }

    #[test]
    fn from_square_count() {
        for i in 0..64u8 {
            let sq = Square::new(i).expect("0..64 must be a valid square");
            let bb = Bitboard::from_square(sq);
            assert_eq!(bb.count(), 1, "from_square({}) should have count 1", i);
            assert!(
                bb.contains(sq),
                "from_square({}) should contain that square",
                i
            );
        }
    }

    #[test]
    fn from_square_isolation() {
        for i in 0..64u8 {
            let sq = Square::new(i).expect("0..64 must be a valid square");
            let bb = Bitboard::from_square(sq);
            for j in 0..64u8 {
                if i == j {
                    continue;
                }
                let other = Square::new(j).expect("0..64 must be a valid square");
                assert!(
                    !bb.contains(other),
                    "from_square({}) must not contain other square {}",
                    i,
                    j
                );
            }
        }
    }

    #[test]
    fn popcount_known_patterns() {
        assert_eq!(Bitboard::FILE_A.count(), 8);
        assert_eq!(Bitboard::RANK_1.count(), 8);
        assert_eq!((Bitboard::FILE_A | Bitboard::RANK_1).count(), 15);
    }

    #[test]
    fn lsb_empty_returns_none() {
        assert_eq!(Bitboard::EMPTY.lsb(), None);
    }

    #[test]
    fn lsb_single_bit() {
        assert_eq!(Bitboard::from_square(Square::E4).lsb(), Some(Square::E4));
    }

    #[test]
    fn pop_lsb_order() {
        let mut bb = Bitboard::from_square(Square::A1)
            | Bitboard::from_square(Square::H1)
            | Bitboard::from_square(Square::A8)
            | Bitboard::from_square(Square::H8);
        assert_eq!(bb.pop_lsb(), Some(Square::A1));
        assert_eq!(bb.pop_lsb(), Some(Square::H1));
        assert_eq!(bb.pop_lsb(), Some(Square::A8));
        assert_eq!(bb.pop_lsb(), Some(Square::H8));
        assert_eq!(bb.pop_lsb(), None);
    }

    #[test]
    fn pop_lsb_drains() {
        let mut bb = Bitboard::FULL;
        let mut seen: Vec<Square> = Vec::with_capacity(64);
        for _ in 0..64 {
            let sq = bb.pop_lsb().expect("FULL should drain to 64 squares");
            seen.push(sq);
        }
        assert_eq!(bb.pop_lsb(), None);
        assert_eq!(seen.len(), 64);
        let mut sorted = seen.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 64, "all 64 popped squares must be distinct");
    }

    #[test]
    fn iter_yields_set_bits() {
        let bb = Bitboard::FILE_A | Bitboard::RANK_1;
        let collected: Vec<Square> = bb.iter().collect();
        let expected: Vec<Square> = vec![
            Square::A1,
            Square::B1,
            Square::C1,
            Square::D1,
            Square::E1,
            Square::F1,
            Square::G1,
            Square::H1,
            Square::A2,
            Square::A3,
            Square::A4,
            Square::A5,
            Square::A6,
            Square::A7,
            Square::A8,
        ];
        assert_eq!(collected.len(), 15);
        let mut got = collected.clone();
        got.sort();
        let mut want = expected.clone();
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn iter_empty() {
        assert_eq!(Bitboard::EMPTY.iter().next(), None);
    }

    #[test]
    fn iter_order_lsb_first() {
        // Pins iter() ordering as LSB-ascending. The set-membership test above
        // sorts both sides, so order is invisible there. This test fails if
        // iter() yields the right set in the wrong order.
        let bb = Bitboard::from_square(Square::H1) | Bitboard::from_square(Square::A1);
        let collected: Vec<Square> = bb.iter().collect();
        assert_eq!(collected, vec![Square::A1, Square::H1]);
    }

    #[test]
    fn set_op_identities() {
        let bb = Bitboard::FILE_A | Bitboard::RANK_1;
        assert_eq!(bb & Bitboard::EMPTY, Bitboard::EMPTY);
        assert_eq!(bb | Bitboard::EMPTY, bb);
        assert_eq!(bb ^ bb, Bitboard::EMPTY);
        assert_eq!(!Bitboard::EMPTY, Bitboard::FULL);
        assert_eq!(!Bitboard::FULL, Bitboard::EMPTY);
    }

    #[test]
    fn bitand_assign_matches_bitand() {
        let a = Bitboard::FILE_A | Bitboard::RANK_1;
        let b = Bitboard::FILE_H | Bitboard::RANK_4;
        let mut acc = a;
        acc &= b;
        assert_eq!(acc, a & b);
    }

    #[test]
    fn bitor_assign_matches_bitor() {
        let a = Bitboard::from_square(Square::E4);
        let b = Bitboard::from_square(Square::D5);
        let mut acc = a;
        acc |= b;
        assert_eq!(acc, a | b);
    }

    #[test]
    fn bitxor_assign_matches_bitxor() {
        let a = Bitboard::FILE_A | Bitboard::RANK_1;
        let b = Bitboard::FILE_A | Bitboard::RANK_8;
        let mut acc = a;
        acc ^= b;
        assert_eq!(acc, a ^ b);
    }

    #[test]
    fn de_morgan() {
        let cases: [(Bitboard, Bitboard); 4] = [
            (Bitboard::FILE_A, Bitboard::RANK_1),
            (Bitboard::FILE_H, Bitboard::RANK_8),
            (Bitboard::EMPTY, Bitboard::FULL),
            (
                Bitboard::from_square(Square::E4) | Bitboard::from_square(Square::D5),
                Bitboard::FILE_C | Bitboard::RANK_4,
            ),
        ];
        for (a, b) in cases {
            assert_eq!(!(a & b), !a | !b);
        }
    }

    #[test]
    fn shift_cardinals_isolated() {
        let e4 = Bitboard::from_square(Square::E4);
        assert_eq!(e4.shift_north(), Bitboard::from_square(Square::E5));
        assert_eq!(e4.shift_south(), Bitboard::from_square(Square::E3));
        assert_eq!(e4.shift_east(), Bitboard::from_square(Square::F4));
        assert_eq!(e4.shift_west(), Bitboard::from_square(Square::D4));
        // Second non-edge sample so an impl with hardcoded E4 answers fails.
        let c2 = Bitboard::from_square(Square::C2);
        assert_eq!(c2.shift_north(), Bitboard::from_square(Square::C3));
        assert_eq!(c2.shift_south(), Bitboard::from_square(Square::C1));
        assert_eq!(c2.shift_east(), Bitboard::from_square(Square::D2));
        assert_eq!(c2.shift_west(), Bitboard::from_square(Square::B2));
    }

    #[test]
    fn shift_diagonals_isolated() {
        let d4 = Bitboard::from_square(Square::D4);
        assert_eq!(d4.shift_north_east(), Bitboard::from_square(Square::E5));
        assert_eq!(d4.shift_north_west(), Bitboard::from_square(Square::C5));
        assert_eq!(d4.shift_south_east(), Bitboard::from_square(Square::E3));
        assert_eq!(d4.shift_south_west(), Bitboard::from_square(Square::C3));
        // Second non-edge sample.
        let f5 = Bitboard::from_square(Square::F5);
        assert_eq!(f5.shift_north_east(), Bitboard::from_square(Square::G6));
        assert_eq!(f5.shift_north_west(), Bitboard::from_square(Square::E6));
        assert_eq!(f5.shift_south_east(), Bitboard::from_square(Square::G4));
        assert_eq!(f5.shift_south_west(), Bitboard::from_square(Square::E4));
    }

    #[test]
    fn shift_north_off_top_rank() {
        assert_eq!(
            Bitboard::from_square(Square::E8).shift_north(),
            Bitboard::EMPTY
        );
        assert_eq!(
            Bitboard::from_square(Square::A8).shift_north(),
            Bitboard::EMPTY
        );
        assert_eq!(
            Bitboard::from_square(Square::H8).shift_north(),
            Bitboard::EMPTY
        );
    }

    #[test]
    fn shift_south_off_bottom_rank() {
        assert_eq!(
            Bitboard::from_square(Square::E1).shift_south(),
            Bitboard::EMPTY
        );
        assert_eq!(
            Bitboard::from_square(Square::A1).shift_south(),
            Bitboard::EMPTY
        );
        assert_eq!(
            Bitboard::from_square(Square::H1).shift_south(),
            Bitboard::EMPTY
        );
    }

    #[test]
    fn shift_e_singleton_h_file() {
        assert_eq!(
            Bitboard::from_square(Square::H4).shift_east(),
            Bitboard::EMPTY
        );
    }

    #[test]
    fn shift_w_singleton_a_file() {
        assert_eq!(
            Bitboard::from_square(Square::A4).shift_west(),
            Bitboard::EMPTY
        );
    }

    #[test]
    fn shift_e_clears_h_file_bulk() {
        assert_eq!(Bitboard::FILE_H.shift_east(), Bitboard::EMPTY);
    }

    #[test]
    fn shift_w_clears_a_file_bulk() {
        assert_eq!(Bitboard::FILE_A.shift_west(), Bitboard::EMPTY);
    }

    #[test]
    fn shift_ne_h_file_clears() {
        assert_eq!(
            Bitboard::from_square(Square::H4).shift_north_east(),
            Bitboard::EMPTY
        );
    }

    #[test]
    fn shift_nw_a_file_clears() {
        assert_eq!(
            Bitboard::from_square(Square::A4).shift_north_west(),
            Bitboard::EMPTY
        );
    }

    #[test]
    fn shift_se_h_file_clears() {
        assert_eq!(
            Bitboard::from_square(Square::H4).shift_south_east(),
            Bitboard::EMPTY
        );
    }

    #[test]
    fn shift_sw_a_file_clears() {
        assert_eq!(
            Bitboard::from_square(Square::A4).shift_south_west(),
            Bitboard::EMPTY
        );
    }

    #[test]
    fn shift_ne_corner_h8() {
        assert_eq!(
            Bitboard::from_square(Square::H8).shift_north_east(),
            Bitboard::EMPTY
        );
    }

    #[test]
    fn shift_nw_corner_a8() {
        assert_eq!(
            Bitboard::from_square(Square::A8).shift_north_west(),
            Bitboard::EMPTY
        );
    }

    #[test]
    fn shift_se_corner_h1() {
        assert_eq!(
            Bitboard::from_square(Square::H1).shift_south_east(),
            Bitboard::EMPTY
        );
    }

    #[test]
    fn shift_sw_corner_a1() {
        assert_eq!(
            Bitboard::from_square(Square::A1).shift_south_west(),
            Bitboard::EMPTY
        );
    }

    #[test]
    fn file_masks_partition_full() {
        let files = [
            Bitboard::FILE_A,
            Bitboard::FILE_B,
            Bitboard::FILE_C,
            Bitboard::FILE_D,
            Bitboard::FILE_E,
            Bitboard::FILE_F,
            Bitboard::FILE_G,
            Bitboard::FILE_H,
        ];
        let union = files
            .iter()
            .copied()
            .fold(Bitboard::EMPTY, |acc, f| acc | f);
        assert_eq!(union, Bitboard::FULL);
        for i in 0..files.len() {
            for j in (i + 1)..files.len() {
                assert_eq!(
                    files[i] & files[j],
                    Bitboard::EMPTY,
                    "files {} and {} must be disjoint",
                    i,
                    j
                );
            }
        }
    }

    #[test]
    fn rank_masks_partition_full() {
        let ranks = [
            Bitboard::RANK_1,
            Bitboard::RANK_2,
            Bitboard::RANK_3,
            Bitboard::RANK_4,
            Bitboard::RANK_5,
            Bitboard::RANK_6,
            Bitboard::RANK_7,
            Bitboard::RANK_8,
        ];
        let union = ranks
            .iter()
            .copied()
            .fold(Bitboard::EMPTY, |acc, r| acc | r);
        assert_eq!(union, Bitboard::FULL);
        for i in 0..ranks.len() {
            for j in (i + 1)..ranks.len() {
                assert_eq!(
                    ranks[i] & ranks[j],
                    Bitboard::EMPTY,
                    "ranks {} and {} must be disjoint",
                    i,
                    j
                );
            }
        }
    }

    #[test]
    fn pretty_print_grid() {
        let expected = concat!(
            "8 X . . . . . . .\n",
            "7 X . . . . . . .\n",
            "6 X . . . . . . .\n",
            "5 X . . . . . . .\n",
            "4 X . . . . . . .\n",
            "3 X . . . . . . .\n",
            "2 X . . . . . . .\n",
            "1 X X X X X X X X\n",
            "  a b c d e f g h\n",
        );
        let actual = format!("{:?}", Bitboard::FILE_A | Bitboard::RANK_1);
        assert_eq!(actual, expected);
    }

    #[test]
    fn display_hex_format() {
        // Pins width-16, lowercase, zero-padded format. A1 has only the LSB
        // set, so the leading 15 zeros prove the padding; FULL covers every
        // hex letter (a-f) and pins the lowercase choice.
        assert_eq!(
            format!("{}", Bitboard::from_square(Square::A1)),
            "Bitboard(0x0000000000000001)"
        );
        assert_eq!(
            format!("{}", Bitboard::EMPTY),
            "Bitboard(0x0000000000000000)"
        );
        assert_eq!(
            format!("{}", Bitboard::FULL),
            "Bitboard(0xffffffffffffffff)"
        );
    }

    // ------------------------------------------------------------------------
    // Property tests (proptest).
    //
    // The unit tests above pin specific identities on hand-picked operands
    // (set_op_identities, de_morgan, file_masks_partition_full, …). The
    // properties below run those identities (and a few more) on randomly
    // generated u64s, exercising bit patterns the unit tests don't reach
    // (mid-range values, sparse-bit positions, sibling-bit interactions).
    // ------------------------------------------------------------------------

    use proptest::prelude::*;

    proptest! {
        /// Boolean-algebra identities on randomly generated bitboards.
        /// Exercises commutativity, associativity, identity elements,
        /// idempotence, self-XOR collapse, double-negation, De Morgan, and
        /// absorption. Also checks that the assigning ops match their non-
        /// assigning counterparts.
        #[test]
        fn prop_set_algebra(a in any::<u64>(), b in any::<u64>(), c in any::<u64>()) {
            let a = Bitboard(a);
            let b = Bitboard(b);
            let c = Bitboard(c);

            // Commutativity.
            prop_assert_eq!(a & b, b & a);
            prop_assert_eq!(a | b, b | a);
            prop_assert_eq!(a ^ b, b ^ a);

            // Associativity.
            prop_assert_eq!((a & b) & c, a & (b & c));
            prop_assert_eq!((a | b) | c, a | (b | c));
            prop_assert_eq!((a ^ b) ^ c, a ^ (b ^ c));

            // Identity / annihilator elements.
            prop_assert_eq!(a & Bitboard::FULL, a);
            prop_assert_eq!(a | Bitboard::EMPTY, a);
            prop_assert_eq!(a ^ Bitboard::EMPTY, a);
            prop_assert_eq!(a & Bitboard::EMPTY, Bitboard::EMPTY);
            prop_assert_eq!(a | Bitboard::FULL, Bitboard::FULL);

            // Idempotence and self-XOR.
            prop_assert_eq!(a & a, a);
            prop_assert_eq!(a | a, a);
            prop_assert_eq!(a ^ a, Bitboard::EMPTY);

            // Double-negation, De Morgan, absorption.
            prop_assert_eq!(!!a, a);
            prop_assert_eq!(!(a & b), !a | !b);
            prop_assert_eq!(!(a | b), !a & !b);
            prop_assert_eq!(a & (a | b), a);
            prop_assert_eq!(a | (a & b), a);

            // count() agrees with the underlying popcount; is_empty is the
            // negation of any().
            prop_assert_eq!(a.count(), a.0.count_ones());
            prop_assert_eq!(a.is_empty(), !a.any());

            // Assigning operators agree with the non-assigning forms.
            let mut acc = a;
            acc &= b;
            prop_assert_eq!(acc, a & b);
            let mut acc = a;
            acc |= b;
            prop_assert_eq!(acc, a | b);
            let mut acc = a;
            acc ^= b;
            prop_assert_eq!(acc, a ^ b);
        }

        /// Membership invariants: with/without/contains/from_square must agree
        /// with each other across an arbitrary base bitboard and an arbitrary
        /// square. Drains pop_lsb to confirm count() squares come out and the
        /// iterator is fused.
        #[test]
        fn prop_membership(base in any::<u64>(), i in 0u8..64) {
            let a = Bitboard(base);
            let sq = Square::new(i).expect("0..64 is in range");

            // with sets the bit; without clears it.
            prop_assert!(a.with(sq).contains(sq));
            prop_assert!(!a.without(sq).contains(sq));

            // from_square is a singleton bitboard.
            let single = Bitboard::from_square(sq);
            prop_assert_eq!(single.count(), 1);
            prop_assert!(single.contains(sq));

            // count after with(sq) is original count or original+1, depending
            // on whether the bit was already set.
            let new_count = a.with(sq).count();
            if a.contains(sq) {
                prop_assert_eq!(new_count, a.count());
            } else {
                prop_assert_eq!(new_count, a.count() + 1);
            }

            // pop_lsb drains exactly count() squares in strictly LSB-ascending
            // order (each popped index strictly greater than the previous);
            // subsequent calls return None. Pinning order on random u64s kills
            // any "swap LSB for MSB" mutant on the underlying trailing_zeros
            // call — the unit tests (`pop_lsb_order`, `iter_order_lsb_first`)
            // pin this only on hand-picked operands.
            let mut bb = a;
            let mut popped: u32 = 0;
            let mut prev: Option<u8> = None;
            while let Some(sq) = bb.pop_lsb() {
                popped += 1;
                if let Some(p) = prev {
                    prop_assert!(
                        sq.index() > p,
                        "pop_lsb must return strictly LSB-ascending squares; got {} after {}",
                        sq.index(),
                        p,
                    );
                }
                prev = Some(sq.index());
            }
            prop_assert_eq!(popped, a.count());
            prop_assert_eq!(bb.pop_lsb(), None);

            // iter() yields the same sequence of squares as draining via
            // pop_lsb. Comparing as Vecs (not multisets) pins the order too.
            let by_iter: Vec<Square> = a.iter().collect();
            let mut bb = a;
            let mut by_pop: Vec<Square> = Vec::with_capacity(a.count() as usize);
            while let Some(sq) = bb.pop_lsb() {
                by_pop.push(sq);
            }
            prop_assert_eq!(by_iter, by_pop);
        }
    }

    // -----------------------------------------------------------------------
    // M6.A §10.2 — LIGHT_SQUARES / DARK_SQUARES constants.
    // -----------------------------------------------------------------------

    /// Light and dark squares together cover the full board with no overlap.
    ///
    /// Pins the exact constant values against the LERF layout (a1=dark):
    ///   `LIGHT_SQUARES = 0x55AA_55AA_55AA_55AA`
    ///   `DARK_SQUARES  = 0xAA55_AA55_AA55_AA55`
    #[test]
    fn light_dark_squares_partition_full_board() {
        assert_eq!(
            Bitboard::LIGHT_SQUARES.0,
            0x55AA_55AA_55AA_55AA,
            "LIGHT_SQUARES constant must equal 0x55AA_55AA_55AA_55AA"
        );
        assert_eq!(
            Bitboard::LIGHT_SQUARES | Bitboard::DARK_SQUARES,
            Bitboard::FULL,
            "LIGHT_SQUARES | DARK_SQUARES must equal FULL"
        );
        assert_eq!(
            Bitboard::LIGHT_SQUARES & Bitboard::DARK_SQUARES,
            Bitboard::EMPTY,
            "LIGHT_SQUARES & DARK_SQUARES must equal EMPTY (no square is both)"
        );
    }

    /// a1 is a dark square by chess convention.
    ///
    /// LERF bit 0 (a1) must be in DARK_SQUARES and absent from LIGHT_SQUARES.
    /// This pins that the masks are not accidentally swapped.
    #[test]
    fn a1_is_dark_square() {
        let a1 = Bitboard::from_square(Square::A1);
        assert!(
            (Bitboard::DARK_SQUARES & a1).any(),
            "a1 must be a dark square (LERF bit 0 in DARK_SQUARES)"
        );
        assert!(
            !(Bitboard::LIGHT_SQUARES & a1).any(),
            "a1 must not be in LIGHT_SQUARES"
        );
    }

    /// a2 is a light square (file=0, rank=1, sum=1 odd → light).
    ///
    /// LERF bit 8 (a2) must be in LIGHT_SQUARES. Pins rank-2 byte pattern
    /// `0x55` (a2, c2, e2, g2 light).
    #[test]
    fn a2_is_light_square() {
        let a2 = Bitboard::from_square(Square::A2);
        assert!(
            (Bitboard::LIGHT_SQUARES & a2).any(),
            "a2 must be a light square (file=0, rank=1, sum=1 odd → light)"
        );
        assert!(
            !(Bitboard::DARK_SQUARES & a2).any(),
            "a2 must not be in DARK_SQUARES"
        );
    }

    /// h1 is a light square (file=7, rank=0, sum=7 odd → light).
    ///
    /// LERF bit 7 (h1) must be in LIGHT_SQUARES. Pins rank-1 byte pattern
    /// `0xAA` (b1, d1, f1, h1 light).
    #[test]
    fn h1_is_light_square() {
        let h1 = Bitboard::from_square(Square::H1);
        assert!(
            (Bitboard::LIGHT_SQUARES & h1).any(),
            "h1 must be a light square (file=7, rank=0, sum=7 odd → light)"
        );
        assert!(
            !(Bitboard::DARK_SQUARES & h1).any(),
            "h1 must not be in DARK_SQUARES"
        );
    }

    // -----------------------------------------------------------------------
    // M6.B Slice A — fill / front-span primitives (ADR-0032).
    //
    // Hand-derived expected sets in every comment. These fail with
    // `unimplemented!` until Slice A — the test-first gate; assertions are
    // correct against the research-note definitions.
    // -----------------------------------------------------------------------

    use super::{
        black_attack_front_spans, black_front_spans, file_fill, north_fill, south_fill,
        white_attack_front_spans, white_front_spans,
    };

    /// north_fill of a single square fills that square and every square
    /// above it on the same file. e4 (rank idx 3, e-file) → {e4,e5,e6,e7,e8}.
    #[test]
    fn north_fill_single_square() {
        let got = north_fill(Bitboard::from_square(Square::E4));
        let want = Bitboard::from_square(Square::E4)
            | Bitboard::from_square(Square::E5)
            | Bitboard::from_square(Square::E6)
            | Bitboard::from_square(Square::E7)
            | Bitboard::from_square(Square::E8);
        assert_eq!(got, want, "north_fill(e4) = {{e4..e8}}");
    }

    /// south_fill of a single square fills that square and every square
    /// below it. e4 → {e1,e2,e3,e4}.
    #[test]
    fn south_fill_single_square() {
        let got = south_fill(Bitboard::from_square(Square::E4));
        let want = Bitboard::from_square(Square::E1)
            | Bitboard::from_square(Square::E2)
            | Bitboard::from_square(Square::E3)
            | Bitboard::from_square(Square::E4);
        assert_eq!(got, want, "south_fill(e4) = {{e1..e4}}");
    }

    /// file_fill of a single square fills the entire file. e4 → FILE_E.
    #[test]
    fn file_fill_single_square_fills_whole_file() {
        assert_eq!(
            file_fill(Bitboard::from_square(Square::E4)),
            Bitboard::FILE_E,
            "file_fill(e4) = FILE_E (whole e-file)"
        );
    }

    /// Fills of the empty set are empty.
    #[test]
    fn fills_of_empty_are_empty() {
        assert_eq!(north_fill(Bitboard::EMPTY), Bitboard::EMPTY);
        assert_eq!(south_fill(Bitboard::EMPTY), Bitboard::EMPTY);
        assert_eq!(file_fill(Bitboard::EMPTY), Bitboard::EMPTY);
    }

    /// north_fill of a full file is that full file (already saturated).
    #[test]
    fn north_fill_full_file_is_idempotent_set() {
        assert_eq!(
            north_fill(Bitboard::FILE_A),
            Bitboard::FILE_A,
            "north_fill(FILE_A) = FILE_A"
        );
        assert_eq!(
            south_fill(Bitboard::FILE_H),
            Bitboard::FILE_H,
            "south_fill(FILE_H) = FILE_H"
        );
    }

    /// file_fill is idempotent: file_fill(file_fill(x)) == file_fill(x).
    /// Property over random u64 bit patterns.
    #[test]
    fn file_fill_idempotent_property() {
        use proptest::prelude::*;
        proptest!(|(bits in any::<u64>())| {
            let x = Bitboard(bits);
            let once = file_fill(x);
            prop_assert_eq!(file_fill(once), once, "file_fill must be idempotent");
        });
    }

    /// white_front_spans EXCLUDES the pawn's own square (off-by-one guard).
    /// White e5 → {e6,e7,e8} — NOT e5.
    #[test]
    fn white_front_spans_excludes_own_square() {
        let got = white_front_spans(Bitboard::from_square(Square::E5));
        let want = Bitboard::from_square(Square::E6)
            | Bitboard::from_square(Square::E7)
            | Bitboard::from_square(Square::E8);
        assert_eq!(
            got, want,
            "white_front_spans(e5) = {{e6,e7,e8}} (excludes e5)"
        );
        assert!(
            !got.contains(Square::E5),
            "white front span must NOT include the pawn's own square"
        );
    }

    /// black_front_spans excludes own square; ahead for black is toward
    /// rank 1. Black e5 → {e4,e3,e2,e1} — NOT e5.
    #[test]
    fn black_front_spans_excludes_own_square() {
        let got = black_front_spans(Bitboard::from_square(Square::E5));
        let want = Bitboard::from_square(Square::E4)
            | Bitboard::from_square(Square::E3)
            | Bitboard::from_square(Square::E2)
            | Bitboard::from_square(Square::E1);
        assert_eq!(
            got, want,
            "black_front_spans(e5) = {{e4,e3,e2,e1}} (excludes e5)"
        );
        assert!(
            !got.contains(Square::E5),
            "black front span must NOT include the pawn's own square"
        );
    }

    /// white front span of a pawn already on rank 8 is empty (nothing ahead).
    #[test]
    fn white_front_span_rank8_empty() {
        assert_eq!(
            white_front_spans(Bitboard::from_square(Square::E8)),
            Bitboard::EMPTY,
            "no squares ahead of a rank-8 white pawn"
        );
    }

    /// black front span of a pawn on rank 1 is empty.
    #[test]
    fn black_front_span_rank1_empty() {
        assert_eq!(
            black_front_spans(Bitboard::from_square(Square::E1)),
            Bitboard::EMPTY,
            "no squares ahead of a rank-1 black pawn"
        );
    }

    /// white_attack_front_spans = white front spans widened E/W (the
    /// diagonal capture files ahead). White d4 → front span {d5,d6,d7,d8},
    /// widened east/west ⇒ also the c- and e-file squares at ranks 5..8.
    /// Expected = {c,d,e files} × {ranks 5..8}.
    #[test]
    fn white_attack_front_spans_cover_diagonal_files() {
        let got = white_attack_front_spans(Bitboard::from_square(Square::D4));
        let mut want = Bitboard::EMPTY;
        for sq in [
            // d-file ahead
            Square::D5,
            Square::D6,
            Square::D7,
            Square::D8,
            // c-file (west widen)
            Square::C5,
            Square::C6,
            Square::C7,
            Square::C8,
            // e-file (east widen)
            Square::E5,
            Square::E6,
            Square::E7,
            Square::E8,
        ] {
            want |= Bitboard::from_square(sq);
        }
        assert_eq!(
            got, want,
            "white_attack_front_spans(d4) = c/d/e files at ranks 5..8"
        );
    }

    /// black_attack_front_spans = black front spans widened E/W. Black d5 →
    /// front span {d4,d3,d2,d1}, widen E/W ⇒ also c- and e-file at ranks
    /// 1..4. Expected = {c,d,e files} × {ranks 1..4}.
    #[test]
    fn black_attack_front_spans_cover_diagonal_files() {
        let got = black_attack_front_spans(Bitboard::from_square(Square::D5));
        let mut want = Bitboard::EMPTY;
        for sq in [
            Square::D4,
            Square::D3,
            Square::D2,
            Square::D1,
            Square::C4,
            Square::C3,
            Square::C2,
            Square::C1,
            Square::E4,
            Square::E3,
            Square::E2,
            Square::E1,
        ] {
            want |= Bitboard::from_square(sq);
        }
        assert_eq!(
            got, want,
            "black_attack_front_spans(d5) = c/d/e files at ranks 1..4"
        );
    }

    /// a-file edge: white_attack_front_spans must not wrap to the h-file.
    /// White a4 → front span {a5..a8}, east-widen ⇒ b-file {b5..b8}; the
    /// west-widen off the a-file is discarded (no wrap). Expected = a/b
    /// files ranks 5..8 only.
    #[test]
    fn white_attack_front_spans_a_file_no_wrap() {
        let got = white_attack_front_spans(Bitboard::from_square(Square::A4));
        let mut want = Bitboard::EMPTY;
        for sq in [
            Square::A5,
            Square::A6,
            Square::A7,
            Square::A8,
            Square::B5,
            Square::B6,
            Square::B7,
            Square::B8,
        ] {
            want |= Bitboard::from_square(sq);
        }
        assert_eq!(
            got, want,
            "a-file attack-front-span must not wrap onto the h-file"
        );
    }

    /// h-file edge: white_attack_front_spans must not wrap to the a-file.
    /// White h4 → front span {h5..h8}, west-widen ⇒ g-file {g5..g8}; the
    /// east-widen off the h-file is discarded (no wrap). Expected = g/h
    /// files ranks 5..8 only.
    #[test]
    fn white_attack_front_spans_h_file_no_wrap() {
        let got = white_attack_front_spans(Bitboard::from_square(Square::H4));
        let mut want = Bitboard::EMPTY;
        for sq in [
            Square::G5,
            Square::G6,
            Square::G7,
            Square::G8,
            Square::H5,
            Square::H6,
            Square::H7,
            Square::H8,
        ] {
            want |= Bitboard::from_square(sq);
        }
        assert_eq!(
            got, want,
            "h-file attack-front-span must not wrap onto the a-file"
        );
    }

    /// a-file edge: black_attack_front_spans must not wrap to the h-file.
    /// Black a5 → front span (toward rank 1) {a4..a1}, east-widen ⇒
    /// b-file {b4..b1}; west-widen off a-file discarded. Expected = a/b
    /// files ranks 1..4 only.
    #[test]
    fn black_attack_front_spans_a_file_no_wrap() {
        let got = black_attack_front_spans(Bitboard::from_square(Square::A5));
        let mut want = Bitboard::EMPTY;
        for sq in [
            Square::A1,
            Square::A2,
            Square::A3,
            Square::A4,
            Square::B1,
            Square::B2,
            Square::B3,
            Square::B4,
        ] {
            want |= Bitboard::from_square(sq);
        }
        assert_eq!(
            got, want,
            "a-file black attack-front-span must not wrap onto the h-file"
        );
    }

    /// h-file edge: black_attack_front_spans must not wrap to the a-file.
    /// Black h5 → front span {h4..h1}, west-widen ⇒ g-file {g4..g1}; the
    /// east-widen off the h-file is discarded (no wrap). Expected = g/h
    /// files ranks 1..4 only.
    #[test]
    fn black_attack_front_spans_h_file_no_wrap() {
        let got = black_attack_front_spans(Bitboard::from_square(Square::H5));
        let mut want = Bitboard::EMPTY;
        for sq in [
            Square::G1,
            Square::G2,
            Square::G3,
            Square::G4,
            Square::H1,
            Square::H2,
            Square::H3,
            Square::H4,
        ] {
            want |= Bitboard::from_square(sq);
        }
        assert_eq!(
            got, want,
            "h-file black attack-front-span must not wrap onto the a-file"
        );
    }

    /// Overlapping-operand union for `white_attack_front_spans` — adjacent
    /// input files force the three union terms to overlap on shared squares,
    /// exposing any `|→^` substitution.
    ///
    /// Input: white pawns c2 and d2.
    ///
    /// Step-by-step derivation:
    ///   white_front_spans(c2|d2) = north_fill(c3|d3) = {c3..c8} | {d3..d8}.
    ///   Let `spans` = c3..c8 ∪ d3..d8.
    ///   spans.shift_east()  = d3..d8 ∪ e3..e8   (c→d, d→e)
    ///   spans.shift_west()  = b3..b8 ∪ c3..c8   (c→b, d→c)
    ///
    ///   spans ∩ spans.shift_east()  = d3..d8  (non-empty — d-file overlap)
    ///   spans ∩ spans.shift_west()  = c3..c8  (non-empty — c-file overlap)
    ///
    ///   Correct union ( | ): b3..b8 ∪ c3..c8 ∪ d3..d8 ∪ e3..e8  (24 squares)
    ///   Under `|→^` at first  `|`: (spans ^ east) | west
    ///     spans ^ east = c3..c8 ∪ e3..e8  (d cancels) → | west = b3..b8 ∪ c3..c8 ∪ e3..e8
    ///     (d-file absent — 6 squares dropped)
    ///   Under `|→^` at second `|`: spans | (east ^ west)
    ///     east ^ west = b3..b8 ∪ e3..e8  (c,d cancel? No — c is in west only
    ///     [from d→c], d is in east only [from c→d]; e is in east only; b is in
    ///     west only → east ^ west = b3..b8 ∪ d3..d8 ∪ e3..e8)
    ///     wait: east = d3..d8 | e3..e8; west = b3..b8 | c3..c8 → no overlap
    ///     east ^ west = b3..b8 | c3..c8 | d3..d8 | e3..e8. Then spans | above
    ///     = correct. (The second `^` mutant is invisible here — this fixture
    ///     is designed to kill only the first-`|` mutants; the exact-equality
    ///     assertion still pins the full 24-square result against any accidental
    ///     output change.)
    ///
    ///   LERF hex: b/c/d/e files on ranks 3–8 each contribute 0x1E in their
    ///   rank byte (b=bit1, c=bit2, d=bit3, e=bit4 within the byte → 0b0001_1110).
    ///   Bytes 0–1 (ranks 1–2) = 0x00; bytes 2–7 (ranks 3–8) = 0x1E each.
    ///   Expected = 0x1E1E_1E1E_1E1E_0000.
    #[test]
    fn white_attack_front_spans_adjacent_files_union_not_xor() {
        let input = Bitboard::from_square(Square::C2) | Bitboard::from_square(Square::D2);
        let got = white_attack_front_spans(input);

        // Hand-enumerated expected set: b/c/d/e files, ranks 3–8.
        let mut want = Bitboard::EMPTY;
        for sq in [
            // b-file ranks 3–8
            Square::B3,
            Square::B4,
            Square::B5,
            Square::B6,
            Square::B7,
            Square::B8,
            // c-file ranks 3–8
            Square::C3,
            Square::C4,
            Square::C5,
            Square::C6,
            Square::C7,
            Square::C8,
            // d-file ranks 3–8
            Square::D3,
            Square::D4,
            Square::D5,
            Square::D6,
            Square::D7,
            Square::D8,
            // e-file ranks 3–8
            Square::E3,
            Square::E4,
            Square::E5,
            Square::E6,
            Square::E7,
            Square::E8,
        ] {
            want |= Bitboard::from_square(sq);
        }
        // LERF hex: 0x1E1E_1E1E_1E1E_0000 (b/c/d/e bits set in each rank byte
        // for ranks 3–8; ranks 1–2 zero).
        assert_eq!(
            want,
            Bitboard(0x1E1E_1E1E_1E1E_0000),
            "hand-derived hex sanity check"
        );
        assert_eq!(
            got, want,
            "white_attack_front_spans(c2|d2): spans and east-shift overlap on the \
             d-file, west-shift overlaps on the c-file; correct | keeps all 24 squares, \
             |→^ drops the 6 d-file squares (first mutant) or 6 c-file squares (second)"
        );
    }

    /// Overlapping-operand union for `black_attack_front_spans` — adjacent
    /// input files force all three union terms to share squares.
    ///
    /// Input: black pawns c7 and d7.
    ///
    /// Step-by-step derivation:
    ///   black_front_spans(c7|d7) = south_fill(c6|d6) = {c1..c6} | {d1..d6}.
    ///   Let `spans` = c1..c6 ∪ d1..d6.
    ///   spans.shift_east()  = d1..d6 ∪ e1..e6   (c→d, d→e)
    ///   spans.shift_west()  = b1..b6 ∪ c1..c6   (c→b, d→c)
    ///
    ///   spans ∩ spans.shift_east()  = d1..d6  (d-file overlap, non-empty)
    ///   spans ∩ spans.shift_west()  = c1..c6  (c-file overlap, non-empty)
    ///
    ///   Correct union ( | ): b1..b6 ∪ c1..c6 ∪ d1..d6 ∪ e1..e6  (24 squares)
    ///   Under `|→^` at first  `|`: (spans ^ east) | west
    ///     spans ^ east = c1..c6 ∪ e1..e6  (d cancels) → | west = b1..b6 ∪ c1..c6 ∪ e1..e6
    ///     (d-file missing — 6 squares dropped)
    ///   Under `|→^` at second `|`: spans | (east ^ west)
    ///     east = d1..d6 | e1..e6; west = b1..b6 | c1..c6 — no overlap
    ///     east ^ west = east | west → spans | above = correct. (Same argument
    ///     as white case: second `^` is invisible here; exact-equality still
    ///     guards the full 24-square result.)
    ///
    ///   LERF hex: b/c/d/e files on ranks 1–6 each contribute 0x1E in their
    ///   rank byte. Bytes 0–5 (ranks 1–6) = 0x1E; bytes 6–7 (ranks 7–8) = 0x00.
    ///   Expected = 0x0000_1E1E_1E1E_1E1E.
    #[test]
    fn black_attack_front_spans_adjacent_files_union_not_xor() {
        let input = Bitboard::from_square(Square::C7) | Bitboard::from_square(Square::D7);
        let got = black_attack_front_spans(input);

        // Hand-enumerated expected set: b/c/d/e files, ranks 1–6.
        let mut want = Bitboard::EMPTY;
        for sq in [
            // b-file ranks 1–6
            Square::B1,
            Square::B2,
            Square::B3,
            Square::B4,
            Square::B5,
            Square::B6,
            // c-file ranks 1–6
            Square::C1,
            Square::C2,
            Square::C3,
            Square::C4,
            Square::C5,
            Square::C6,
            // d-file ranks 1–6
            Square::D1,
            Square::D2,
            Square::D3,
            Square::D4,
            Square::D5,
            Square::D6,
            // e-file ranks 1–6
            Square::E1,
            Square::E2,
            Square::E3,
            Square::E4,
            Square::E5,
            Square::E6,
        ] {
            want |= Bitboard::from_square(sq);
        }
        // LERF hex: 0x0000_1E1E_1E1E_1E1E (b/c/d/e bits set in rank bytes 1–6;
        // ranks 7–8 zero).
        assert_eq!(
            want,
            Bitboard(0x0000_1E1E_1E1E_1E1E),
            "hand-derived hex sanity check"
        );
        assert_eq!(
            got, want,
            "black_attack_front_spans(c7|d7): spans and east-shift overlap on the \
             d-file, west-shift overlaps on the c-file; correct | keeps all 24 squares, \
             |→^ drops the 6 d-file squares"
        );
    }

    /// Skip-file union for `white_attack_front_spans` — non-adjacent input
    /// files force the east-shift and west-shift to overlap on an intermediate
    /// file that is absent from `spans`, exposing `|→^` at the second `|`.
    ///
    /// Input: white pawns b2 and d2 (skipping the c-file).
    ///
    /// Step-by-step derivation:
    ///   white_front_spans(b2|d2) = north_fill(b3|d3) = {b3..b8} | {d3..d8}.
    ///   Let `spans` = b3..b8 ∪ d3..d8  (c-file absent from spans).
    ///   spans.shift_east()  = c3..c8 ∪ e3..e8   (b→c, d→e)
    ///   spans.shift_west()  = a3..a8 ∪ c3..c8   (b→a, d→c)
    ///
    ///   spans ∩ spans.shift_east()  = ∅  (b,d vs c,e — disjoint)
    ///   east  ∩ west                = c3..c8  (non-empty; c NOT in spans!)
    ///
    ///   Correct union ( | ): a3..a8 ∪ b3..b8 ∪ c3..c8 ∪ d3..d8 ∪ e3..e8  (30 squares)
    ///   Under `|→^` at second `|`: spans | (east ^ west)
    ///     east ^ west = (c3..c8 | e3..e8) ^ (a3..a8 | c3..c8)
    ///                 = a3..a8 | e3..e8  (c cancels!)
    ///     spans | above = b3..b8 | d3..d8 | a3..a8 | e3..e8  (c-file missing!)
    ///   Under `|→^` at first  `|`: (spans ^ east) | west
    ///     spans ^ east = b3..b8 | c3..c8 | d3..d8 | e3..e8  (no overlap → ^ = |)
    ///     | west       = a3..a8 | b3..b8 | c3..c8 | d3..d8 | e3..e8  (= correct)
    ///     So this fixture is invisible to the first-`|` mutant — see the
    ///     adjacent-files test for that coverage.
    ///
    ///   LERF hex: a/b/c/d/e files on ranks 3–8 each contribute 0x1F in their
    ///   rank byte (a=bit0, b=bit1, c=bit2, d=bit3, e=bit4 → 0b0001_1111 = 0x1F).
    ///   Bytes 0–1 (ranks 1–2) = 0x00; bytes 2–7 (ranks 3–8) = 0x1F each.
    ///   Expected = 0x1F1F_1F1F_1F1F_0000.
    #[test]
    fn white_attack_front_spans_skip_files_second_union_not_xor() {
        let input = Bitboard::from_square(Square::B2) | Bitboard::from_square(Square::D2);
        let got = white_attack_front_spans(input);

        // Hand-enumerated expected set: a/b/c/d/e files, ranks 3–8.
        let mut want = Bitboard::EMPTY;
        for sq in [
            // a-file ranks 3–8
            Square::A3,
            Square::A4,
            Square::A5,
            Square::A6,
            Square::A7,
            Square::A8,
            // b-file ranks 3–8
            Square::B3,
            Square::B4,
            Square::B5,
            Square::B6,
            Square::B7,
            Square::B8,
            // c-file ranks 3–8 (present in east∩west but not in spans — drops under ^)
            Square::C3,
            Square::C4,
            Square::C5,
            Square::C6,
            Square::C7,
            Square::C8,
            // d-file ranks 3–8
            Square::D3,
            Square::D4,
            Square::D5,
            Square::D6,
            Square::D7,
            Square::D8,
            // e-file ranks 3–8
            Square::E3,
            Square::E4,
            Square::E5,
            Square::E6,
            Square::E7,
            Square::E8,
        ] {
            want |= Bitboard::from_square(sq);
        }
        // LERF hex: 0x1F1F_1F1F_1F1F_0000 (a/b/c/d/e bits in rank bytes 3–8).
        assert_eq!(
            want,
            Bitboard(0x1F1F_1F1F_1F1F_0000),
            "hand-derived hex sanity check"
        );
        assert_eq!(
            got, want,
            "white_attack_front_spans(b2|d2): east and west shifts both land on the \
             c-file (absent from spans); correct | keeps c-file squares, |→^ at the \
             second operator cancels them"
        );
    }

    /// Skip-file union for `black_attack_front_spans` — non-adjacent input
    /// files expose `|→^` at the second `|` via an intermediate-file overlap.
    ///
    /// Input: black pawns b7 and d7 (skipping the c-file).
    ///
    /// Step-by-step derivation:
    ///   black_front_spans(b7|d7) = south_fill(b6|d6) = {b1..b6} | {d1..d6}.
    ///   Let `spans` = b1..b6 ∪ d1..d6  (c-file absent from spans).
    ///   spans.shift_east()  = c1..c6 ∪ e1..e6   (b→c, d→e)
    ///   spans.shift_west()  = a1..a6 ∪ c1..c6   (b→a, d→c)
    ///
    ///   spans ∩ spans.shift_east()  = ∅  (b,d vs c,e — disjoint)
    ///   east  ∩ west                = c1..c6  (non-empty; c NOT in spans!)
    ///
    ///   Correct union ( | ): a1..a6 ∪ b1..b6 ∪ c1..c6 ∪ d1..d6 ∪ e1..e6  (30 squares)
    ///   Under `|→^` at second `|`: spans | (east ^ west)
    ///     east ^ west = a1..a6 | e1..e6  (c cancels!)
    ///     spans | above = a1..a6 | b1..b6 | d1..d6 | e1..e6  (c-file missing!)
    ///   Under `|→^` at first `|`: result equals correct (no spans∩east overlap).
    ///
    ///   LERF hex: a/b/c/d/e files on ranks 1–6 each contribute 0x1F in their
    ///   rank byte. Bytes 0–5 (ranks 1–6) = 0x1F; bytes 6–7 = 0x00.
    ///   Expected = 0x0000_1F1F_1F1F_1F1F.
    #[test]
    fn black_attack_front_spans_skip_files_second_union_not_xor() {
        let input = Bitboard::from_square(Square::B7) | Bitboard::from_square(Square::D7);
        let got = black_attack_front_spans(input);

        // Hand-enumerated expected set: a/b/c/d/e files, ranks 1–6.
        let mut want = Bitboard::EMPTY;
        for sq in [
            // a-file ranks 1–6
            Square::A1,
            Square::A2,
            Square::A3,
            Square::A4,
            Square::A5,
            Square::A6,
            // b-file ranks 1–6
            Square::B1,
            Square::B2,
            Square::B3,
            Square::B4,
            Square::B5,
            Square::B6,
            // c-file ranks 1–6 (east∩west but not spans — drops under ^)
            Square::C1,
            Square::C2,
            Square::C3,
            Square::C4,
            Square::C5,
            Square::C6,
            // d-file ranks 1–6
            Square::D1,
            Square::D2,
            Square::D3,
            Square::D4,
            Square::D5,
            Square::D6,
            // e-file ranks 1–6
            Square::E1,
            Square::E2,
            Square::E3,
            Square::E4,
            Square::E5,
            Square::E6,
        ] {
            want |= Bitboard::from_square(sq);
        }
        // LERF hex: 0x0000_1F1F_1F1F_1F1F (a/b/c/d/e bits in rank bytes 1–6).
        assert_eq!(
            want,
            Bitboard(0x0000_1F1F_1F1F_1F1F),
            "hand-derived hex sanity check"
        );
        assert_eq!(
            got, want,
            "black_attack_front_spans(b7|d7): east and west shifts both land on the \
             c-file (absent from spans); correct | keeps c-file squares, |→^ at the \
             second operator cancels them"
        );
    }
}
