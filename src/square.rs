use crate::bitboard::{Bitboard, Squares};
use std::fmt;

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Square(u8);

impl Square {
    /// Total number of squares on a chess board. Useful for sizing arrays
    /// like `[T; Square::COUNT]` indexed by square.
    pub const COUNT: usize = 64;

    pub const A1: Square = Square(0);
    pub const B1: Square = Square(1);
    pub const C1: Square = Square(2);
    pub const D1: Square = Square(3);
    pub const E1: Square = Square(4);
    pub const F1: Square = Square(5);
    pub const G1: Square = Square(6);
    pub const H1: Square = Square(7);
    pub const A2: Square = Square(8);
    pub const B2: Square = Square(9);
    pub const C2: Square = Square(10);
    pub const D2: Square = Square(11);
    pub const E2: Square = Square(12);
    pub const F2: Square = Square(13);
    pub const G2: Square = Square(14);
    pub const H2: Square = Square(15);
    pub const A3: Square = Square(16);
    pub const B3: Square = Square(17);
    pub const C3: Square = Square(18);
    pub const D3: Square = Square(19);
    pub const E3: Square = Square(20);
    pub const F3: Square = Square(21);
    pub const G3: Square = Square(22);
    pub const H3: Square = Square(23);
    pub const A4: Square = Square(24);
    pub const B4: Square = Square(25);
    pub const C4: Square = Square(26);
    pub const D4: Square = Square(27);
    pub const E4: Square = Square(28);
    pub const F4: Square = Square(29);
    pub const G4: Square = Square(30);
    pub const H4: Square = Square(31);
    pub const A5: Square = Square(32);
    pub const B5: Square = Square(33);
    pub const C5: Square = Square(34);
    pub const D5: Square = Square(35);
    pub const E5: Square = Square(36);
    pub const F5: Square = Square(37);
    pub const G5: Square = Square(38);
    pub const H5: Square = Square(39);
    pub const A6: Square = Square(40);
    pub const B6: Square = Square(41);
    pub const C6: Square = Square(42);
    pub const D6: Square = Square(43);
    pub const E6: Square = Square(44);
    pub const F6: Square = Square(45);
    pub const G6: Square = Square(46);
    pub const H6: Square = Square(47);
    pub const A7: Square = Square(48);
    pub const B7: Square = Square(49);
    pub const C7: Square = Square(50);
    pub const D7: Square = Square(51);
    pub const E7: Square = Square(52);
    pub const F7: Square = Square(53);
    pub const G7: Square = Square(54);
    pub const H7: Square = Square(55);
    pub const A8: Square = Square(56);
    pub const B8: Square = Square(57);
    pub const C8: Square = Square(58);
    pub const D8: Square = Square(59);
    pub const E8: Square = Square(60);
    pub const F8: Square = Square(61);
    pub const G8: Square = Square(62);
    pub const H8: Square = Square(63);

    pub const fn new(index: u8) -> Option<Self> {
        if index < 64 {
            Some(Self(index))
        } else {
            None
        }
    }

    pub const fn new_unchecked(index: u8) -> Self {
        debug_assert!(index < 64);
        Self(index)
    }

    pub const fn from_file_rank(file: u8, rank: u8) -> Option<Self> {
        if file >= 8 || rank >= 8 {
            None
        } else {
            Some(Self(rank * 8 + file))
        }
    }

    pub const fn index(self) -> u8 {
        self.0
    }

    pub const fn file(self) -> u8 {
        self.0 % 8
    }

    pub const fn rank(self) -> u8 {
        self.0 / 8
    }

    pub fn parse_uci(s: &str) -> Option<Self> {
        let bytes = s.as_bytes();
        if bytes.len() != 2 {
            return None;
        }
        let file_byte = bytes[0];
        let rank_byte = bytes[1];
        if !(b'a'..=b'h').contains(&file_byte) || !(b'1'..=b'8').contains(&rank_byte) {
            return None;
        }
        let file = file_byte - b'a';
        let rank = rank_byte - b'1';
        Some(Self(rank * 8 + file))
    }

    pub fn all() -> Squares {
        Bitboard::FULL.iter()
    }
}

impl fmt::Display for Square {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let file_char = (b'a' + self.file()) as char;
        let rank_char = (b'1' + self.rank()) as char;
        write!(f, "{}{}", file_char, rank_char)
    }
}

impl fmt::Debug for Square {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_within_bounds() {
        assert!(Square::new(0).is_some());
        assert!(Square::new(63).is_some());
        assert!(Square::new(64).is_none());
    }

    #[test]
    fn index_round_trip() {
        for i in 0..64u8 {
            let sq = Square::new(i).expect("0..64 must be a valid square");
            assert_eq!(sq.index(), i);
        }
    }

    #[test]
    fn from_file_rank_round_trip() {
        for f in 0..8u8 {
            for r in 0..8u8 {
                let sq = Square::from_file_rank(f, r)
                    .expect("0..8 x 0..8 must be a valid square");
                assert_eq!(sq.file(), f);
                assert_eq!(sq.rank(), r);
            }
        }
    }

    #[test]
    fn from_file_rank_rejects_oob() {
        assert!(Square::from_file_rank(8, 0).is_none());
        assert!(Square::from_file_rank(0, 8).is_none());
    }

    #[test]
    fn from_file_rank_lerf_anchored() {
        // Anchors `from_file_rank` to LERF independently of file()/rank().
        // Without these, `from_file_rank` and the decomposers could share a
        // transposed-bug (square = 8*file + rank) and the round-trip would
        // still pass. Pinning the corner squares prevents that.
        assert_eq!(Square::from_file_rank(0, 0), Some(Square::A1));
        assert_eq!(Square::from_file_rank(7, 0), Some(Square::H1));
        assert_eq!(Square::from_file_rank(0, 7), Some(Square::A8));
        assert_eq!(Square::from_file_rank(7, 7), Some(Square::H8));
        // One non-corner mid-board sample to catch swapped-axis bugs.
        assert_eq!(Square::from_file_rank(4, 3), Some(Square::E4));
    }

    #[test]
    fn lerf_corners() {
        assert_eq!(Square::A1.index(), 0);
        assert_eq!(Square::H1.index(), 7);
        assert_eq!(Square::A8.index(), 56);
        assert_eq!(Square::H8.index(), 63);
    }

    #[test]
    fn parse_uci_valid() {
        // Literal anchors covering every file letter (a-h) and every rank
        // digit (1-8) at least once, independent of `Display`. Without these,
        // a symmetric Display+parse_uci bug (e.g. both transpose file/rank)
        // would self-cancel in the round-trip below.
        assert_eq!(Square::parse_uci("a1"), Some(Square::A1));
        assert_eq!(Square::parse_uci("b3"), Some(Square::B3));
        assert_eq!(Square::parse_uci("c5"), Some(Square::C5));
        assert_eq!(Square::parse_uci("d2"), Some(Square::D2));
        assert_eq!(Square::parse_uci("e4"), Some(Square::E4));
        assert_eq!(Square::parse_uci("f6"), Some(Square::F6));
        assert_eq!(Square::parse_uci("g7"), Some(Square::G7));
        assert_eq!(Square::parse_uci("h8"), Some(Square::H8));

        let named: [Square; 64] = [
            Square::A1, Square::B1, Square::C1, Square::D1, Square::E1, Square::F1, Square::G1, Square::H1,
            Square::A2, Square::B2, Square::C2, Square::D2, Square::E2, Square::F2, Square::G2, Square::H2,
            Square::A3, Square::B3, Square::C3, Square::D3, Square::E3, Square::F3, Square::G3, Square::H3,
            Square::A4, Square::B4, Square::C4, Square::D4, Square::E4, Square::F4, Square::G4, Square::H4,
            Square::A5, Square::B5, Square::C5, Square::D5, Square::E5, Square::F5, Square::G5, Square::H5,
            Square::A6, Square::B6, Square::C6, Square::D6, Square::E6, Square::F6, Square::G6, Square::H6,
            Square::A7, Square::B7, Square::C7, Square::D7, Square::E7, Square::F7, Square::G7, Square::H7,
            Square::A8, Square::B8, Square::C8, Square::D8, Square::E8, Square::F8, Square::G8, Square::H8,
        ];
        for sq in named {
            let s = sq.to_string();
            assert_eq!(Square::parse_uci(&s), Some(sq));
        }
    }

    #[test]
    fn parse_uci_invalid() {
        assert_eq!(Square::parse_uci(""), None);
        assert_eq!(Square::parse_uci("e"), None);
        assert_eq!(Square::parse_uci("i4"), None);
        assert_eq!(Square::parse_uci("e9"), None);
        // "a0" — rank below valid range. Symmetric to "e9"; catches off-by-one
        // on the lower bound where, e.g., `digit - b'1'` could wrap to 0xFF.
        assert_eq!(Square::parse_uci("a0"), None);
        assert_eq!(Square::parse_uci("E4"), None);
        assert_eq!(Square::parse_uci("e4 "), None);
        assert_eq!(Square::parse_uci("e4e4"), None);
        assert_eq!(Square::parse_uci("\u{265B}\u{265B}"), None);
    }

    #[test]
    fn display_format() {
        assert_eq!(Square::E4.to_string(), "e4");
    }

    #[test]
    fn all_iterates_64_unique() {
        let collected: Vec<Square> = Square::all().collect();
        assert_eq!(collected.len(), 64);
        let mut sorted = collected.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 64);
    }

    #[test]
    fn ord_is_lerf_index_order() {
        assert!(Square::A1 < Square::H1);
        assert!(Square::H1 < Square::A2);
        assert!(Square::A2 < Square::H8);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic]
    fn new_unchecked_oob_panics_in_debug() {
        let _ = Square::new_unchecked(64);
    }
}
