//! Piece taxonomy: `Color`, `PieceKind`, and `Piece`.
//!
//! Both `Color` and `PieceKind` are `#[repr(u8)]` enums whose discriminants
//! are stable: `White = 0`, `Black = 1`, and `Pawn = 0` through `King = 5`.
//! Code elsewhere in the crate indexes color- or kind-keyed arrays via `as
//! usize`. The `PieceKind` ordering (P-N-B-R-Q-K) matches the kind sub-axis
//! of the Polyglot Zobrist piece enumeration so M1.D's hash table doesn't
//! need a kind-axis remap.

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Color {
    White = 0,
    Black = 1,
}

impl Color {
    pub const COUNT: usize = 2;

    #[must_use]
    #[inline]
    pub const fn flip(self) -> Color {
        match self {
            Color::White => Color::Black,
            Color::Black => Color::White,
        }
    }

    #[inline]
    pub const fn index(self) -> usize {
        self as usize
    }

    #[inline]
    pub const fn from_fen_char(c: u8) -> Option<Color> {
        match c {
            b'w' => Some(Color::White),
            b'b' => Some(Color::Black),
            _ => None,
        }
    }

    #[inline]
    pub const fn fen_char(self) -> u8 {
        match self {
            Color::White => b'w',
            Color::Black => b'b',
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum PieceKind {
    Pawn = 0,
    Knight = 1,
    Bishop = 2,
    Rook = 3,
    Queen = 4,
    King = 5,
}

impl PieceKind {
    pub const COUNT: usize = 6;

    pub const ALL: [PieceKind; 6] = [
        PieceKind::Pawn,
        PieceKind::Knight,
        PieceKind::Bishop,
        PieceKind::Rook,
        PieceKind::Queen,
        PieceKind::King,
    ];

    #[inline]
    pub const fn index(self) -> usize {
        self as usize
    }

    #[inline]
    pub const fn san_letter(self) -> u8 {
        match self {
            PieceKind::Pawn => b'p',
            PieceKind::Knight => b'n',
            PieceKind::Bishop => b'b',
            PieceKind::Rook => b'r',
            PieceKind::Queen => b'q',
            PieceKind::King => b'k',
        }
    }

    #[inline]
    pub const fn from_san_letter(c: u8) -> Option<PieceKind> {
        match c {
            b'p' | b'P' => Some(PieceKind::Pawn),
            b'n' | b'N' => Some(PieceKind::Knight),
            b'b' | b'B' => Some(PieceKind::Bishop),
            b'r' | b'R' => Some(PieceKind::Rook),
            b'q' | b'Q' => Some(PieceKind::Queen),
            b'k' | b'K' => Some(PieceKind::King),
            _ => None,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Piece {
    pub color: Color,
    pub kind: PieceKind,
}

impl Piece {
    pub const fn new(color: Color, kind: PieceKind) -> Piece {
        Piece { color, kind }
    }

    #[inline]
    pub const fn fen_char(self) -> u8 {
        // FEN: uppercase for White, lowercase for Black. san_letter() returns
        // the lowercase form; subtract 0x20 to uppercase for White.
        let lower = self.kind.san_letter();
        match self.color {
            Color::White => lower - 0x20,
            Color::Black => lower,
        }
    }

    #[inline]
    pub const fn from_fen_char(c: u8) -> Option<Piece> {
        let color = match c {
            b'A'..=b'Z' => Color::White,
            b'a'..=b'z' => Color::Black,
            _ => return None,
        };
        match PieceKind::from_san_letter(c) {
            Some(kind) => Some(Piece::new(color, kind)),
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_flip_is_involutive() {
        // Each color flips to the other and double-flip is identity.
        assert_eq!(Color::White.flip(), Color::Black);
        assert_eq!(Color::Black.flip(), Color::White);
        assert_eq!(Color::White.flip().flip(), Color::White);
        assert_eq!(Color::Black.flip().flip(), Color::Black);
    }

    #[test]
    fn color_index_is_repr() {
        assert_eq!(Color::White.index(), 0);
        assert_eq!(Color::Black.index(), 1);
        // Pin that index() actually returns the discriminant via `as usize`.
        assert_eq!(Color::White.index(), Color::White as usize);
        assert_eq!(Color::Black.index(), Color::Black as usize);
    }

    #[test]
    fn color_fen_char_round_trip() {
        assert_eq!(Color::from_fen_char(b'w'), Some(Color::White));
        assert_eq!(Color::from_fen_char(b'b'), Some(Color::Black));
        assert_eq!(Color::White.fen_char(), b'w');
        assert_eq!(Color::Black.fen_char(), b'b');
        // Round-trip both directions.
        assert_eq!(
            Color::from_fen_char(Color::White.fen_char()),
            Some(Color::White)
        );
        assert_eq!(
            Color::from_fen_char(Color::Black.fen_char()),
            Some(Color::Black)
        );
        // Case-sensitive per FEN spec §16.1.3.2: uppercase 'W'/'B' must reject.
        assert_eq!(Color::from_fen_char(b'W'), None);
        assert_eq!(Color::from_fen_char(b'B'), None);
    }

    #[test]
    fn color_fen_char_rejects_other() {
        assert_eq!(Color::from_fen_char(b'x'), None);
        assert_eq!(Color::from_fen_char(b' '), None);
        assert_eq!(Color::from_fen_char(b'1'), None);
        assert_eq!(Color::from_fen_char(b'\0'), None);
    }

    #[test]
    fn piecekind_index_distinct() {
        // All six piece kinds map to distinct indices in 0..=5.
        let mut seen = [false; 6];
        for kind in PieceKind::ALL {
            let i = kind.index();
            assert!(i < 6, "index() for {:?} must be < 6, got {}", kind, i);
            assert!(!seen[i], "duplicate index {} for {:?}", i, kind);
            seen[i] = true;
        }
        assert!(seen.iter().all(|&b| b), "every index 0..=5 must appear");
        // Pin that index() returns the discriminant via `as usize`.
        assert_eq!(PieceKind::Pawn.index(), 0);
        assert_eq!(PieceKind::Knight.index(), 1);
        assert_eq!(PieceKind::Bishop.index(), 2);
        assert_eq!(PieceKind::Rook.index(), 3);
        assert_eq!(PieceKind::Queen.index(), 4);
        assert_eq!(PieceKind::King.index(), 5);
    }

    #[test]
    fn piecekind_san_letter_table() {
        // Pinned literal mapping per the plan (lowercase per FEN/SAN convention).
        assert_eq!(PieceKind::Pawn.san_letter(), b'p');
        assert_eq!(PieceKind::Knight.san_letter(), b'n');
        assert_eq!(PieceKind::Bishop.san_letter(), b'b');
        assert_eq!(PieceKind::Rook.san_letter(), b'r');
        assert_eq!(PieceKind::Queen.san_letter(), b'q');
        assert_eq!(PieceKind::King.san_letter(), b'k');
    }

    #[test]
    fn piecekind_from_san_letter_case_insensitive() {
        // For each kind, both the lowercase and uppercase letters parse to it.
        let cases: [(PieceKind, u8, u8); 6] = [
            (PieceKind::Pawn, b'p', b'P'),
            (PieceKind::Knight, b'n', b'N'),
            (PieceKind::Bishop, b'b', b'B'),
            (PieceKind::Rook, b'r', b'R'),
            (PieceKind::Queen, b'q', b'Q'),
            (PieceKind::King, b'k', b'K'),
        ];
        for (kind, lower, upper) in cases {
            assert_eq!(
                PieceKind::from_san_letter(lower),
                Some(kind),
                "lowercase {} should parse to {:?}",
                lower as char,
                kind
            );
            assert_eq!(
                PieceKind::from_san_letter(upper),
                Some(kind),
                "uppercase {} should parse to {:?}",
                upper as char,
                kind
            );
        }
    }

    #[test]
    fn piecekind_from_san_letter_rejects_invalid() {
        assert_eq!(PieceKind::from_san_letter(b'x'), None);
        assert_eq!(PieceKind::from_san_letter(b' '), None);
        assert_eq!(PieceKind::from_san_letter(b'8'), None);
        assert_eq!(PieceKind::from_san_letter(b'\0'), None);
    }

    #[test]
    fn piece_fen_char_white_uppercase() {
        assert_eq!(Piece::new(Color::White, PieceKind::Pawn).fen_char(), b'P');
        assert_eq!(Piece::new(Color::White, PieceKind::Knight).fen_char(), b'N');
        assert_eq!(Piece::new(Color::White, PieceKind::Bishop).fen_char(), b'B');
        assert_eq!(Piece::new(Color::White, PieceKind::Rook).fen_char(), b'R');
        assert_eq!(Piece::new(Color::White, PieceKind::Queen).fen_char(), b'Q');
        assert_eq!(Piece::new(Color::White, PieceKind::King).fen_char(), b'K');
    }

    #[test]
    fn piece_fen_char_black_lowercase() {
        assert_eq!(Piece::new(Color::Black, PieceKind::Pawn).fen_char(), b'p');
        assert_eq!(Piece::new(Color::Black, PieceKind::Knight).fen_char(), b'n');
        assert_eq!(Piece::new(Color::Black, PieceKind::Bishop).fen_char(), b'b');
        assert_eq!(Piece::new(Color::Black, PieceKind::Rook).fen_char(), b'r');
        assert_eq!(Piece::new(Color::Black, PieceKind::Queen).fen_char(), b'q');
        assert_eq!(Piece::new(Color::Black, PieceKind::King).fen_char(), b'k');
    }

    #[test]
    fn piece_from_fen_char_explicit_letters() {
        // Anchor each FEN letter to a specific Piece, independently of the
        // round-trip below. Without these, a symmetric color-swapped bug in
        // both fen_char and from_fen_char would self-cancel in the round-trip.
        assert_eq!(
            Piece::from_fen_char(b'P'),
            Some(Piece::new(Color::White, PieceKind::Pawn))
        );
        assert_eq!(
            Piece::from_fen_char(b'N'),
            Some(Piece::new(Color::White, PieceKind::Knight))
        );
        assert_eq!(
            Piece::from_fen_char(b'B'),
            Some(Piece::new(Color::White, PieceKind::Bishop))
        );
        assert_eq!(
            Piece::from_fen_char(b'R'),
            Some(Piece::new(Color::White, PieceKind::Rook))
        );
        assert_eq!(
            Piece::from_fen_char(b'Q'),
            Some(Piece::new(Color::White, PieceKind::Queen))
        );
        assert_eq!(
            Piece::from_fen_char(b'K'),
            Some(Piece::new(Color::White, PieceKind::King))
        );
        assert_eq!(
            Piece::from_fen_char(b'p'),
            Some(Piece::new(Color::Black, PieceKind::Pawn))
        );
        assert_eq!(
            Piece::from_fen_char(b'n'),
            Some(Piece::new(Color::Black, PieceKind::Knight))
        );
        assert_eq!(
            Piece::from_fen_char(b'b'),
            Some(Piece::new(Color::Black, PieceKind::Bishop))
        );
        assert_eq!(
            Piece::from_fen_char(b'r'),
            Some(Piece::new(Color::Black, PieceKind::Rook))
        );
        assert_eq!(
            Piece::from_fen_char(b'q'),
            Some(Piece::new(Color::Black, PieceKind::Queen))
        );
        assert_eq!(
            Piece::from_fen_char(b'k'),
            Some(Piece::new(Color::Black, PieceKind::King))
        );
    }

    #[test]
    fn piece_from_fen_char_round_trip() {
        // For every color × kind, fen_char then from_fen_char round-trips.
        for &color in &[Color::White, Color::Black] {
            for kind in PieceKind::ALL {
                let p = Piece::new(color, kind);
                let c = p.fen_char();
                assert_eq!(
                    Piece::from_fen_char(c),
                    Some(p),
                    "round-trip failed for {:?}/{:?} via {}",
                    color,
                    kind,
                    c as char
                );
            }
        }
    }

    #[test]
    fn piece_from_fen_char_rejects_invalid() {
        assert_eq!(Piece::from_fen_char(b'1'), None);
        assert_eq!(Piece::from_fen_char(b'/'), None);
        assert_eq!(Piece::from_fen_char(b' '), None);
        assert_eq!(Piece::from_fen_char(b'X'), None);
    }
}
