use crate::square::Square;
use std::fmt;
use std::ops;

#[derive(Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct Bitboard(pub u64);

impl Bitboard {
    pub const EMPTY: Bitboard = Bitboard(0);
    pub const FULL: Bitboard = Bitboard(!0);

    pub const FILE_A: Bitboard = Bitboard(0x0101_0101_0101_0101);
    pub const FILE_B: Bitboard = Bitboard(0x0202_0202_0202_0202);
    pub const FILE_C: Bitboard = Bitboard(0x0404_0404_0404_0404);
    pub const FILE_D: Bitboard = Bitboard(0x0808_0808_0808_0808);
    pub const FILE_E: Bitboard = Bitboard(0x1010_1010_1010_1010);
    pub const FILE_F: Bitboard = Bitboard(0x2020_2020_2020_2020);
    pub const FILE_G: Bitboard = Bitboard(0x4040_4040_4040_4040);
    pub const FILE_H: Bitboard = Bitboard(0x8080_8080_8080_8080);

    pub const RANK_1: Bitboard = Bitboard(0x0000_0000_0000_00FF);
    pub const RANK_2: Bitboard = Bitboard(0x0000_0000_0000_FF00);
    pub const RANK_3: Bitboard = Bitboard(0x0000_0000_00FF_0000);
    pub const RANK_4: Bitboard = Bitboard(0x0000_0000_FF00_0000);
    pub const RANK_5: Bitboard = Bitboard(0x0000_00FF_0000_0000);
    pub const RANK_6: Bitboard = Bitboard(0x0000_FF00_0000_0000);
    pub const RANK_7: Bitboard = Bitboard(0x00FF_0000_0000_0000);
    pub const RANK_8: Bitboard = Bitboard(0xFF00_0000_0000_0000);

    #[must_use]
    #[inline]
    pub const fn from_square(sq: Square) -> Bitboard {
        Bitboard(1u64 << sq.index())
    }

    #[inline]
    pub const fn contains(self, sq: Square) -> bool {
        (self.0 >> sq.index()) & 1 == 1
    }

    #[must_use]
    #[inline]
    pub const fn with(self, sq: Square) -> Bitboard {
        Bitboard(self.0 | (1u64 << sq.index()))
    }

    #[must_use]
    #[inline]
    pub const fn without(self, sq: Square) -> Bitboard {
        Bitboard(self.0 & !(1u64 << sq.index()))
    }

    #[inline]
    pub const fn count(self) -> u32 {
        self.0.count_ones()
    }

    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub const fn any(self) -> bool {
        self.0 != 0
    }

    #[inline]
    pub const fn lsb(self) -> Option<Square> {
        if self.0 == 0 {
            None
        } else {
            Some(Square::new_unchecked(self.0.trailing_zeros() as u8))
        }
    }

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

    #[inline]
    pub fn iter(self) -> Squares {
        Squares(self)
    }

    #[must_use]
    #[inline]
    pub const fn shift_north(self) -> Bitboard {
        Bitboard(self.0 << 8)
    }

    #[must_use]
    #[inline]
    pub const fn shift_south(self) -> Bitboard {
        Bitboard(self.0 >> 8)
    }

    #[must_use]
    #[inline]
    pub const fn shift_east(self) -> Bitboard {
        Bitboard((self.0 & !Self::FILE_H.0) << 1)
    }

    #[must_use]
    #[inline]
    pub const fn shift_west(self) -> Bitboard {
        Bitboard((self.0 & !Self::FILE_A.0) >> 1)
    }

    #[must_use]
    #[inline]
    pub const fn shift_north_east(self) -> Bitboard {
        Bitboard((self.0 & !Self::FILE_H.0) << 9)
    }

    #[must_use]
    #[inline]
    pub const fn shift_north_west(self) -> Bitboard {
        Bitboard((self.0 & !Self::FILE_A.0) << 7)
    }

    #[must_use]
    #[inline]
    pub const fn shift_south_east(self) -> Bitboard {
        Bitboard((self.0 & !Self::FILE_H.0) >> 7)
    }

    #[must_use]
    #[inline]
    pub const fn shift_south_west(self) -> Bitboard {
        Bitboard((self.0 & !Self::FILE_A.0) >> 9)
    }
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
}
