//! `Position` — the canonical chess position representation.
//!
//! Layout: 6 piece-kind bitboards + 2 color-occupancy bitboards + a 64-entry
//! mailbox + cached king squares + auxiliary state (side to move, castling
//! rights, EP target, halfmove clock, fullmove number).
//!
//! `Display` formats the position as a FEN string (single-line, six fields).
//! `Debug` formats as an 8×8 grid with FEN piece letters and a one-line
//! summary of auxiliary state. This mirrors `Bitboard`'s convention.
//!
//! Source of truth: bitboards plus auxiliary fields. The mailbox and
//! `king_sq` cache are derived state, kept in sync by the `set_piece`
//! mutator (which debug-asserts on king-overwrite).
//!
//! EP target is stored exactly as FEN encodes it (set after every double
//! pawn push, regardless of whether capture is possible). The Polyglot
//! "EP-only-when-pseudo-legal" filter is applied at Zobrist-XOR time in
//! M1.D — it does not affect storage.

use std::fmt;

use crate::bitboard::Bitboard;
use crate::fen::FenError;
use crate::piece::{Color, Piece, PieceKind};
use crate::square::Square;

/// Castling rights as a 4-bit field. Bit layout matches the Polyglot
/// Zobrist castling-key convention (bit 0 = white kingside, bit 1 = white
/// queenside, bit 2 = black kingside, bit 3 = black queenside) so M1.D
/// can read this field directly without remapping.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct CastlingRights(u8);

impl CastlingRights {
    pub const NONE: CastlingRights = CastlingRights(0);
    pub const WHITE_KING: CastlingRights = CastlingRights(0b0001);
    pub const WHITE_QUEEN: CastlingRights = CastlingRights(0b0010);
    pub const BLACK_KING: CastlingRights = CastlingRights(0b0100);
    pub const BLACK_QUEEN: CastlingRights = CastlingRights(0b1000);
    pub const ALL: CastlingRights = CastlingRights(0b1111);

    /// Test whether all bits of `flag` are set in `self`. Returns `false`
    /// when `flag == NONE` (the empty query) — `has` is intended for
    /// single-flag or multi-flag presence queries, not for "any rights at
    /// all," which `bits() != 0` answers more clearly.
    pub const fn has(self, flag: CastlingRights) -> bool {
        (self.0 & flag.0) == flag.0 && flag.0 != 0
    }

    #[must_use]
    pub const fn with(self, flag: CastlingRights) -> CastlingRights {
        CastlingRights(self.0 | flag.0)
    }

    #[must_use]
    pub const fn without(self, flag: CastlingRights) -> CastlingRights {
        CastlingRights(self.0 & !flag.0)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }
}

impl std::ops::BitAnd for CastlingRights {
    type Output = CastlingRights;
    /// Bitwise intersection — used by M1.E's make_move to apply the
    /// castling-mask table: `new_castling = prior & MASK[from] & MASK[to]`.
    #[inline]
    fn bitand(self, rhs: CastlingRights) -> CastlingRights {
        CastlingRights(self.0 & rhs.0)
    }
}

impl fmt::Display for CastlingRights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            return f.write_str("-");
        }
        if self.has(CastlingRights::WHITE_KING) {
            f.write_str("K")?;
        }
        if self.has(CastlingRights::WHITE_QUEEN) {
            f.write_str("Q")?;
        }
        if self.has(CastlingRights::BLACK_KING) {
            f.write_str("k")?;
        }
        if self.has(CastlingRights::BLACK_QUEEN) {
            f.write_str("q")?;
        }
        Ok(())
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub struct Position {
    piece_bb: [Bitboard; PieceKind::COUNT],
    color_bb: [Bitboard; Color::COUNT],
    mailbox: [Option<Piece>; 64],
    king_sq: [Square; Color::COUNT],
    side_to_move: Color,
    castling: CastlingRights,
    ep_target: Option<Square>,
    halfmove_clock: u8,
    fullmove_number: u16,
    /// Polyglot Zobrist hash. Maintained from-scratch via [`refresh_zobrist`]
    /// after parse / construction; M1.E will switch this to incremental
    /// XOR updates inside `make_move` / `unmake_move`. Private — cross-module
    /// mutation goes through `refresh_zobrist`. See ADR-0009.
    zobrist: u64,
}

impl Position {
    pub const STARTING_FEN: &'static str =
        "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";

    /// Empty board, white to move, no castling, no EP, halfmove=0, fullmove=1.
    /// Has zero kings — produced positions WILL FAIL `validate_post_parse`;
    /// this constructor is only suitable as a starting point that the FEN
    /// parser (or a test) immediately fills in via `set_piece` /
    /// `set_aux_state`.
    pub(crate) const fn empty() -> Position {
        Position {
            piece_bb: [Bitboard::EMPTY; PieceKind::COUNT],
            color_bb: [Bitboard::EMPTY; Color::COUNT],
            mailbox: [None; 64],
            king_sq: [Square::A1, Square::A1],
            side_to_move: Color::White,
            castling: CastlingRights::NONE,
            ep_target: None,
            halfmove_clock: 0,
            fullmove_number: 1,
            zobrist: 0,
        }
    }

    pub fn starting_position() -> Position {
        // White pieces.
        let white_pawns = Bitboard::RANK_2;
        let white_rooks =
            Bitboard(Bitboard::from_square(Square::A1).0 | Bitboard::from_square(Square::H1).0);
        let white_knights =
            Bitboard(Bitboard::from_square(Square::B1).0 | Bitboard::from_square(Square::G1).0);
        let white_bishops =
            Bitboard(Bitboard::from_square(Square::C1).0 | Bitboard::from_square(Square::F1).0);
        let white_queens = Bitboard::from_square(Square::D1);
        let white_kings = Bitboard::from_square(Square::E1);

        // Black pieces.
        let black_pawns = Bitboard::RANK_7;
        let black_rooks =
            Bitboard(Bitboard::from_square(Square::A8).0 | Bitboard::from_square(Square::H8).0);
        let black_knights =
            Bitboard(Bitboard::from_square(Square::B8).0 | Bitboard::from_square(Square::G8).0);
        let black_bishops =
            Bitboard(Bitboard::from_square(Square::C8).0 | Bitboard::from_square(Square::F8).0);
        let black_queens = Bitboard::from_square(Square::D8);
        let black_kings = Bitboard::from_square(Square::E8);

        let pawns = Bitboard(white_pawns.0 | black_pawns.0);
        let knights = Bitboard(white_knights.0 | black_knights.0);
        let bishops = Bitboard(white_bishops.0 | black_bishops.0);
        let rooks = Bitboard(white_rooks.0 | black_rooks.0);
        let queens = Bitboard(white_queens.0 | black_queens.0);
        let kings = Bitboard(white_kings.0 | black_kings.0);

        let white_occ = Bitboard(Bitboard::RANK_1.0 | Bitboard::RANK_2.0);
        let black_occ = Bitboard(Bitboard::RANK_7.0 | Bitboard::RANK_8.0);

        // Mailbox: 64 entries, ordered by LERF index (a1=0..h8=63).
        const WP: Option<Piece> = Some(Piece::new(Color::White, PieceKind::Pawn));
        const WN: Option<Piece> = Some(Piece::new(Color::White, PieceKind::Knight));
        const WB: Option<Piece> = Some(Piece::new(Color::White, PieceKind::Bishop));
        const WR: Option<Piece> = Some(Piece::new(Color::White, PieceKind::Rook));
        const WQ: Option<Piece> = Some(Piece::new(Color::White, PieceKind::Queen));
        const WK: Option<Piece> = Some(Piece::new(Color::White, PieceKind::King));
        const BP: Option<Piece> = Some(Piece::new(Color::Black, PieceKind::Pawn));
        const BN: Option<Piece> = Some(Piece::new(Color::Black, PieceKind::Knight));
        const BB: Option<Piece> = Some(Piece::new(Color::Black, PieceKind::Bishop));
        const BR: Option<Piece> = Some(Piece::new(Color::Black, PieceKind::Rook));
        const BQ: Option<Piece> = Some(Piece::new(Color::Black, PieceKind::Queen));
        const BK: Option<Piece> = Some(Piece::new(Color::Black, PieceKind::King));
        const __: Option<Piece> = None;

        let mailbox: [Option<Piece>; 64] = [
            // rank 1: a1..h1
            WR, WN, WB, WQ, WK, WB, WN, WR, // rank 2: a2..h2
            WP, WP, WP, WP, WP, WP, WP, WP, // rank 3
            __, __, __, __, __, __, __, __, // rank 4
            __, __, __, __, __, __, __, __, // rank 5
            __, __, __, __, __, __, __, __, // rank 6
            __, __, __, __, __, __, __, __, // rank 7
            BP, BP, BP, BP, BP, BP, BP, BP, // rank 8
            BR, BN, BB, BQ, BK, BB, BN, BR,
        ];

        // piece_bb is indexed by `PieceKind as usize` (P=0, N=1, B=2, R=3, Q=4, K=5).
        // color_bb is indexed by `Color as usize` (White=0, Black=1).
        // king_sq mirrors the color order. This dependency is load-bearing
        // across `pieces`, `pieces_colored`, `occupied`, `king_square`.
        let mut p = Position {
            piece_bb: [pawns, knights, bishops, rooks, queens, kings],
            color_bb: [white_occ, black_occ],
            mailbox,
            king_sq: [Square::E1, Square::E8],
            side_to_move: Color::White,
            castling: CastlingRights::ALL,
            ep_target: None,
            halfmove_clock: 0,
            fullmove_number: 1,
            zobrist: 0,
        };
        p.refresh_zobrist();
        p
    }

    pub fn from_fen(s: &str) -> Result<Position, FenError> {
        crate::fen::parse(s)
    }

    pub fn to_fen(&self) -> String {
        format!("{}", self)
    }

    pub const fn side_to_move(&self) -> Color {
        self.side_to_move
    }

    pub const fn castling_rights(&self) -> CastlingRights {
        self.castling
    }

    pub const fn ep_target(&self) -> Option<Square> {
        self.ep_target
    }

    pub const fn halfmove_clock(&self) -> u8 {
        self.halfmove_clock
    }

    pub const fn fullmove_number(&self) -> u16 {
        self.fullmove_number
    }

    /// Polyglot Zobrist hash of the position (per ADR-0009). Maintained
    /// from-scratch by [`Position::refresh_zobrist`] after parse and
    /// construction; M1.E will switch to incremental XOR updates inside
    /// `make_move` / `unmake_move`.
    pub const fn zobrist(&self) -> u64 {
        self.zobrist
    }

    pub const fn piece_at(&self, sq: Square) -> Option<Piece> {
        self.mailbox[sq.index() as usize]
    }

    pub const fn pieces(&self, kind: PieceKind) -> Bitboard {
        self.piece_bb[kind.index()]
    }

    pub const fn pieces_colored(&self, color: Color, kind: PieceKind) -> Bitboard {
        Bitboard(self.piece_bb[kind.index()].0 & self.color_bb[color.index()].0)
    }

    pub const fn occupied(&self, color: Color) -> Bitboard {
        self.color_bb[color.index()]
    }

    pub const fn occupied_all(&self) -> Bitboard {
        Bitboard(self.color_bb[0].0 | self.color_bb[1].0)
    }

    pub const fn king_square(&self, color: Color) -> Square {
        self.king_sq[color.index()]
    }

    /// Cross-field consistency check used by tests and (in M1.E) by the
    /// post-make-move debug assert. Panics on violation in debug builds.
    /// In release builds the body compiles out — see the `cfg(not(debug_assertions))`
    /// stub below — so callers can invoke it unconditionally.
    #[cfg(debug_assertions)]
    #[allow(dead_code)] // Called from tests only at M1.B; M1.E adds a non-test caller.
    pub(crate) fn debug_assert_consistent(&self) {
        // 1. Piece bitboards are pairwise disjoint.
        for i in 0..PieceKind::COUNT {
            for j in (i + 1)..PieceKind::COUNT {
                assert_eq!(
                    self.piece_bb[i] & self.piece_bb[j],
                    Bitboard::EMPTY,
                    "piece bitboards {i} and {j} must be disjoint",
                );
            }
        }

        // 2. Color bitboards are disjoint.
        assert_eq!(
            self.color_bb[Color::White.index()] & self.color_bb[Color::Black.index()],
            Bitboard::EMPTY,
            "color bitboards must be disjoint",
        );

        // 3. Piece-bitboard union equals color-bitboard union.
        let mut piece_union = Bitboard::EMPTY;
        for bb in self.piece_bb.iter() {
            piece_union |= *bb;
        }
        let color_union = self.color_bb[Color::White.index()] | self.color_bb[Color::Black.index()];
        assert_eq!(
            piece_union, color_union,
            "piece-bitboard union must equal color-bitboard union",
        );

        // 4. Mailbox agrees with bitboards on every square.
        for sq in Square::all() {
            match self.mailbox[sq.index() as usize] {
                Some(piece) => {
                    assert!(
                        self.piece_bb[piece.kind.index()].contains(sq),
                        "mailbox has {piece:?} at {sq:?} but piece bitboard disagrees",
                    );
                    assert!(
                        self.color_bb[piece.color.index()].contains(sq),
                        "mailbox has {piece:?} at {sq:?} but color bitboard disagrees",
                    );
                }
                None => {
                    assert!(
                        !piece_union.contains(sq),
                        "mailbox empty at {sq:?} but bitboards have a piece there",
                    );
                }
            }
        }

        // 5. king_sq matches the King bitboard (single bit per color).
        for color in [Color::White, Color::Black] {
            let king_bb = self.pieces_colored(color, PieceKind::King);
            assert_eq!(
                king_bb.count(),
                1,
                "{color:?} must have exactly one king for cache consistency",
            );
            let cached = self.king_sq[color.index()];
            assert!(
                king_bb.contains(cached),
                "{color:?} king cache {cached:?} not in king bitboard",
            );
        }
    }

    /// Place a piece at `sq`, updating bitboards, mailbox, and king cache.
    /// Debug-asserts if the previous occupant was a king (overwriting a
    /// king would silently invalidate `king_sq`). The FEN parser walks
    /// each square exactly once so it never triggers the assert.
    ///
    /// **Does not refresh `zobrist`.** Callers that need a consistent hash
    /// after a sequence of `set_piece` / `set_aux_state` mutations must
    /// call [`refresh_zobrist`](Self::refresh_zobrist) afterward. The FEN
    /// parser does this once after all set_* calls complete; M1.E's
    /// `make_move` will use incremental XOR updates instead.
    pub(crate) fn set_piece(&mut self, sq: Square, piece: Piece) {
        let prev = self.mailbox[sq.index() as usize];
        debug_assert!(
            prev.map(|p| p.kind != PieceKind::King).unwrap_or(true),
            "set_piece would overwrite a king at {sq:?}",
        );

        if let Some(prev_piece) = prev {
            self.piece_bb[prev_piece.kind.index()] =
                self.piece_bb[prev_piece.kind.index()].without(sq);
            self.color_bb[prev_piece.color.index()] =
                self.color_bb[prev_piece.color.index()].without(sq);
        }

        self.piece_bb[piece.kind.index()] = self.piece_bb[piece.kind.index()].with(sq);
        self.color_bb[piece.color.index()] = self.color_bb[piece.color.index()].with(sq);
        self.mailbox[sq.index() as usize] = Some(piece);

        if matches!(piece.kind, PieceKind::King) {
            self.king_sq[piece.color.index()] = sq;
        }
    }

    /// Remove the piece (if any) at `sq`. Symmetric to [`set_piece`](Self::set_piece).
    /// Used by `make_move` / `unmake_move` to vacate a square; the matching
    /// `set_piece` call that places the moving piece on its new square
    /// follows immediately.
    ///
    /// **Clearing a king square is allowed** — every king move (quiet,
    /// capture, castling) decomposes into `clear_square(old_sq)` followed
    /// by `set_piece(new_sq, king)`. The `set_piece` call refreshes
    /// `king_sq[color]` to the new square, so the king-cache invariant
    /// holds again the moment both calls have run. Callers must not
    /// invoke [`debug_assert_consistent`](Self::debug_assert_consistent)
    /// between the `clear_square` and the matching `set_piece` for a king
    /// move; `make_move` / `unmake_move` invoke it only after both have
    /// completed.
    ///
    /// Calling on an empty square is a no-op.
    ///
    /// **Does not refresh `zobrist`** — same contract as `set_piece`. M1.E's
    /// `make_move` / `unmake_move` maintain `zobrist` via incremental XOR
    /// updates around the bitboard mutations.
    pub(crate) fn clear_square(&mut self, sq: Square) {
        let prev = self.mailbox[sq.index() as usize];
        if let Some(prev_piece) = prev {
            self.piece_bb[prev_piece.kind.index()] =
                self.piece_bb[prev_piece.kind.index()].without(sq);
            self.color_bb[prev_piece.color.index()] =
                self.color_bb[prev_piece.color.index()].without(sq);
            self.mailbox[sq.index() as usize] = None;
            // The king cache is intentionally NOT updated here. See the doc
            // comment: a `clear_square` of a king square is always followed
            // by a `set_piece` placing that king on a new square, which
            // updates `king_sq`. Between the two calls the cache is stale,
            // which is why `debug_assert_consistent` must not be invoked.
        }
    }

    /// Set side-to-move, castling, EP, halfmove, and fullmove fields in one go.
    ///
    /// **Does not refresh `zobrist`.** Same contract as [`set_piece`](Self::set_piece):
    /// callers that need a consistent hash must call
    /// [`refresh_zobrist`](Self::refresh_zobrist) afterward.
    pub(crate) fn set_aux_state(
        &mut self,
        side: Color,
        castling: CastlingRights,
        ep: Option<Square>,
        halfmove: u8,
        fullmove: u16,
    ) {
        self.side_to_move = side;
        self.castling = castling;
        self.ep_target = ep;
        self.halfmove_clock = halfmove;
        self.fullmove_number = fullmove;
    }

    /// Recompute and store the Polyglot Zobrist hash from the current
    /// position state via [`crate::zobrist::from_scratch`]. Called by
    /// `starting_position()`, by the FEN parser after `validate_post_parse`,
    /// and by tests that mutate via `set_piece` / `set_aux_state` and want
    /// a consistent hash. M1.E's `make_move` / `unmake_move` use the
    /// `refresh_zobrist_from` setter below for incremental XOR updates.
    pub(crate) fn refresh_zobrist(&mut self) {
        self.zobrist = crate::zobrist::from_scratch(self);
    }

    /// Write `value` into the zobrist field directly, bypassing
    /// [`refresh_zobrist`](Self::refresh_zobrist)'s from-scratch
    /// recomputation. Used by `mov::make_move` to install an
    /// incrementally-computed hash and by `mov::unmake_move` to restore
    /// the pre-make hash from `Undo`. The caller is responsible for the
    /// value being correct; debug assertions in `make_move` / `unmake_move`
    /// validate against `from_scratch`.
    pub(crate) fn refresh_zobrist_from(&mut self, value: u64) {
        self.zobrist = value;
    }

    /// Apply `mv` to `self`, returning an [`Undo`](crate::mov::Undo) token
    /// sufficient to reverse the change. Equivalent to
    /// [`crate::mov::make_move(self, mv)`](crate::mov::make_move).
    pub fn make_move(&mut self, mv: crate::mov::Move) -> crate::mov::Undo {
        crate::mov::make_move(self, mv)
    }

    /// Reverse a previous `make_move`. Equivalent to
    /// [`crate::mov::unmake_move(self, mv, undo)`](crate::mov::unmake_move).
    pub fn unmake_move(&mut self, mv: crate::mov::Move, undo: crate::mov::Undo) {
        crate::mov::unmake_move(self, mv, undo)
    }

    pub(crate) fn validate_post_parse(&self) -> Result<(), FenError> {
        let white_kings = self.pieces_colored(Color::White, PieceKind::King).count();
        match white_kings {
            0 => return Err(FenError::MissingKing(Color::White)),
            1 => {}
            _ => return Err(FenError::TooManyKings(Color::White)),
        }
        let black_kings = self.pieces_colored(Color::Black, PieceKind::King).count();
        match black_kings {
            0 => return Err(FenError::MissingKing(Color::Black)),
            1 => {}
            _ => return Err(FenError::TooManyKings(Color::Black)),
        }
        Ok(())
    }

    /// Release-mode no-op stub for `debug_assert_consistent`. The function
    /// exists in both modes so callers (tests, parser post-conditions) can
    /// invoke it without `cfg`-gating each call site; the body is only
    /// present under `cfg(debug_assertions)`.
    #[cfg(not(debug_assertions))]
    #[allow(dead_code)]
    pub(crate) fn debug_assert_consistent(&self) {}
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        crate::fen::format(self, f)
    }
}

impl fmt::Debug for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 8 rank rows, rank 8 at top.
        for rank in (0..8u8).rev() {
            write!(f, "{}", rank + 1)?;
            for file in 0..8u8 {
                let sq = Square::from_file_rank(file, rank).expect("file and rank are in 0..8");
                let cell = match self.piece_at(sq) {
                    Some(piece) => piece.fen_char() as char,
                    None => '.',
                };
                write!(f, " {}", cell)?;
            }
            writeln!(f)?;
        }
        // File-label row.
        writeln!(f, "  a b c d e f g h")?;
        // Summary line.
        let color_word = match self.side_to_move {
            Color::White => "White",
            Color::Black => "Black",
        };
        let ep = match self.ep_target {
            Some(sq) => format!("{}", sq),
            None => "-".to_string(),
        };
        writeln!(
            f,
            "{} to move, halfmove={}, fullmove={}, castling={}, ep={}",
            color_word, self.halfmove_clock, self.fullmove_number, self.castling, ep
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitboard::Bitboard;
    use crate::piece::{Color, Piece, PieceKind};
    use crate::square::Square;

    // Convenience aliases for piece literals used throughout these tests.
    // Some are referenced in only a subset of tests; `#[allow(dead_code)]`
    // keeps the full set declared for readability without warnings.
    #[allow(dead_code)]
    const WHITE_PAWN: Piece = Piece::new(Color::White, PieceKind::Pawn);
    #[allow(dead_code)]
    const WHITE_KNIGHT: Piece = Piece::new(Color::White, PieceKind::Knight);
    #[allow(dead_code)]
    const WHITE_BISHOP: Piece = Piece::new(Color::White, PieceKind::Bishop);
    #[allow(dead_code)]
    const WHITE_ROOK: Piece = Piece::new(Color::White, PieceKind::Rook);
    #[allow(dead_code)]
    const WHITE_QUEEN: Piece = Piece::new(Color::White, PieceKind::Queen);
    #[allow(dead_code)]
    const WHITE_KING: Piece = Piece::new(Color::White, PieceKind::King);
    #[allow(dead_code)]
    const BLACK_PAWN: Piece = Piece::new(Color::Black, PieceKind::Pawn);
    #[allow(dead_code)]
    const BLACK_KNIGHT: Piece = Piece::new(Color::Black, PieceKind::Knight);
    #[allow(dead_code)]
    const BLACK_BISHOP: Piece = Piece::new(Color::Black, PieceKind::Bishop);
    #[allow(dead_code)]
    const BLACK_ROOK: Piece = Piece::new(Color::Black, PieceKind::Rook);
    #[allow(dead_code)]
    const BLACK_QUEEN: Piece = Piece::new(Color::Black, PieceKind::Queen);
    #[allow(dead_code)]
    const BLACK_KING: Piece = Piece::new(Color::Black, PieceKind::King);

    #[test]
    fn castling_none_display() {
        assert_eq!(format!("{}", CastlingRights::NONE), "-");
    }

    #[test]
    fn castling_all_display() {
        assert_eq!(format!("{}", CastlingRights::ALL), "KQkq");
    }

    #[test]
    fn castling_partial_displays() {
        let kq = CastlingRights::WHITE_KING.with(CastlingRights::BLACK_QUEEN);
        assert_eq!(format!("{}", kq), "Kq");

        let qk = CastlingRights::WHITE_QUEEN.with(CastlingRights::BLACK_KING);
        assert_eq!(format!("{}", qk), "Qk");

        assert_eq!(format!("{}", CastlingRights::BLACK_KING), "k");
    }

    #[test]
    fn castling_with_is_idempotent_on_set_flag() {
        // Pins `|`-not-`^` in CastlingRights::with: re-adding a flag that's
        // already set must leave the rights unchanged.
        let r = CastlingRights::WHITE_KING;
        assert_eq!(
            r.with(CastlingRights::WHITE_KING),
            CastlingRights::WHITE_KING
        );
    }

    #[test]
    fn castling_has_with_without() {
        assert!(
            CastlingRights::NONE
                .with(CastlingRights::WHITE_KING)
                .has(CastlingRights::WHITE_KING)
        );
        assert!(
            !CastlingRights::ALL
                .without(CastlingRights::BLACK_QUEEN)
                .has(CastlingRights::BLACK_QUEEN)
        );
        assert!(CastlingRights::ALL.has(CastlingRights::WHITE_KING));
    }

    #[test]
    fn castling_bits_layout() {
        assert_eq!(CastlingRights::WHITE_KING.bits(), 1);
        assert_eq!(CastlingRights::WHITE_QUEEN.bits(), 2);
        assert_eq!(CastlingRights::BLACK_KING.bits(), 4);
        assert_eq!(CastlingRights::BLACK_QUEEN.bits(), 8);
    }

    #[test]
    fn starting_position_basics() {
        let p = Position::starting_position();
        assert_eq!(p.side_to_move(), Color::White);
        assert_eq!(p.castling_rights(), CastlingRights::ALL);
        assert_eq!(p.ep_target(), None);
        assert_eq!(p.halfmove_clock(), 0);
        assert_eq!(p.fullmove_number(), 1);
        assert_eq!(p.king_square(Color::White), Square::E1);
        assert_eq!(p.king_square(Color::Black), Square::E8);

        assert_eq!(p.pieces_colored(Color::White, PieceKind::Pawn).count(), 8);
        assert_eq!(p.pieces_colored(Color::Black, PieceKind::Pawn).count(), 8);

        // Pawn ranks.
        assert_eq!(
            p.pieces_colored(Color::White, PieceKind::Pawn),
            Bitboard::RANK_2
        );
        assert_eq!(
            p.pieces_colored(Color::Black, PieceKind::Pawn),
            Bitboard::RANK_7
        );

        // Back-rank piece counts.
        assert_eq!(p.pieces_colored(Color::White, PieceKind::Knight).count(), 2);
        assert_eq!(p.pieces_colored(Color::White, PieceKind::Bishop).count(), 2);
        assert_eq!(p.pieces_colored(Color::White, PieceKind::Rook).count(), 2);
        assert_eq!(p.pieces_colored(Color::White, PieceKind::Queen).count(), 1);
        assert_eq!(p.pieces_colored(Color::White, PieceKind::King).count(), 1);

        assert_eq!(p.pieces_colored(Color::Black, PieceKind::Knight).count(), 2);
        assert_eq!(p.pieces_colored(Color::Black, PieceKind::Bishop).count(), 2);
        assert_eq!(p.pieces_colored(Color::Black, PieceKind::Rook).count(), 2);
        assert_eq!(p.pieces_colored(Color::Black, PieceKind::Queen).count(), 1);
        assert_eq!(p.pieces_colored(Color::Black, PieceKind::King).count(), 1);

        // Spot-check specific squares for piece presence.
        assert!(
            p.pieces_colored(Color::White, PieceKind::Rook)
                .contains(Square::A1)
        );
        assert!(
            p.pieces_colored(Color::White, PieceKind::Rook)
                .contains(Square::H1)
        );
        assert!(
            p.pieces_colored(Color::Black, PieceKind::Rook)
                .contains(Square::A8)
        );
        assert!(
            p.pieces_colored(Color::Black, PieceKind::Rook)
                .contains(Square::H8)
        );
        assert!(
            p.pieces_colored(Color::White, PieceKind::Queen)
                .contains(Square::D1)
        );
        assert!(
            p.pieces_colored(Color::Black, PieceKind::Queen)
                .contains(Square::D8)
        );
    }

    #[test]
    fn starting_position_consistency() {
        let p = Position::starting_position();
        p.debug_assert_consistent();

        // Piece bitboards are pairwise disjoint.
        let kinds = PieceKind::ALL;
        for i in 0..kinds.len() {
            for j in (i + 1)..kinds.len() {
                assert_eq!(
                    p.pieces(kinds[i]) & p.pieces(kinds[j]),
                    Bitboard::EMPTY,
                    "piece bitboards for {:?} and {:?} must be disjoint",
                    kinds[i],
                    kinds[j]
                );
            }
        }

        // Color bitboards are disjoint.
        assert_eq!(
            p.occupied(Color::White) & p.occupied(Color::Black),
            Bitboard::EMPTY
        );

        // Piece-bitboard union equals color-bitboard union.
        let piece_union = kinds
            .iter()
            .copied()
            .fold(Bitboard::EMPTY, |acc, k| acc | p.pieces(k));
        let color_union = p.occupied(Color::White) | p.occupied(Color::Black);
        assert_eq!(piece_union, color_union);
        assert_eq!(piece_union, p.occupied_all());

        // Mailbox matches bitboards on every square.
        for sq in Square::all() {
            let mailbox_piece = p.piece_at(sq);
            // Cross-check via bitboards: find the piece whose bitboard covers sq.
            let mut bb_piece: Option<Piece> = None;
            for &kind in &PieceKind::ALL {
                for color in [Color::White, Color::Black] {
                    if p.pieces_colored(color, kind).contains(sq) {
                        assert!(bb_piece.is_none(), "two bitboards both claim {:?}", sq);
                        bb_piece = Some(Piece::new(color, kind));
                    }
                }
            }
            assert_eq!(
                mailbox_piece, bb_piece,
                "mailbox vs bitboards diverge at {:?}",
                sq
            );
        }

        // King cache matches the King bitboard.
        assert!(
            p.pieces_colored(Color::White, PieceKind::King)
                .contains(p.king_square(Color::White))
        );
        assert!(
            p.pieces_colored(Color::Black, PieceKind::King)
                .contains(p.king_square(Color::Black))
        );
    }

    #[test]
    fn piece_at_starting_position() {
        let p = Position::starting_position();
        assert_eq!(p.piece_at(Square::E1), Some(WHITE_KING));
        assert_eq!(p.piece_at(Square::E2), Some(WHITE_PAWN));
        assert_eq!(p.piece_at(Square::E4), None);
        assert_eq!(p.piece_at(Square::D8), Some(BLACK_QUEEN));
    }

    #[test]
    fn occupied_partition_starting_position() {
        let p = Position::starting_position();
        assert_eq!(
            p.occupied(Color::White) & p.occupied(Color::Black),
            Bitboard::EMPTY
        );
        assert_eq!(
            p.occupied_all(),
            p.occupied(Color::White) | p.occupied(Color::Black)
        );
        assert_eq!(p.occupied_all().count(), 32);
    }

    #[test]
    fn pieces_kind_aggregate() {
        let p = Position::starting_position();
        for &kind in &PieceKind::ALL {
            assert_eq!(
                p.pieces(kind),
                p.pieces_colored(Color::White, kind) | p.pieces_colored(Color::Black, kind),
                "pieces({:?}) must be the union of the two color-keyed bitboards",
                kind
            );
        }
    }

    #[test]
    fn display_starting_position_emits_starting_fen() {
        assert_eq!(
            format!("{}", Position::starting_position()),
            Position::STARTING_FEN
        );
    }

    #[test]
    fn debug_grid_starting_position() {
        let expected = concat!(
            "8 r n b q k b n r\n",
            "7 p p p p p p p p\n",
            "6 . . . . . . . .\n",
            "5 . . . . . . . .\n",
            "4 . . . . . . . .\n",
            "3 . . . . . . . .\n",
            "2 P P P P P P P P\n",
            "1 R N B Q K B N R\n",
            "  a b c d e f g h\n",
            "White to move, halfmove=0, fullmove=1, castling=KQkq, ep=-\n",
        );
        let actual = format!("{:?}", Position::starting_position());
        assert_eq!(actual, expected);
    }

    #[test]
    fn equality_is_structural() {
        let p1 = Position::starting_position();
        let p2 = Position::starting_position();
        assert_eq!(p1, p2);

        // Flipping side-to-move alone should make them unequal.
        let mut p_flipped = Position::starting_position();
        p_flipped.set_aux_state(Color::Black, CastlingRights::ALL, None, 0, 1);
        assert_ne!(p1, p_flipped);

        // Bumping fullmove alone should make them unequal.
        let mut p_bumped = Position::starting_position();
        p_bumped.set_aux_state(Color::White, CastlingRights::ALL, None, 0, 2);
        assert_ne!(p1, p_bumped);
    }

    #[test]
    fn position_is_copy_clone() {
        fn assert_copy_clone<T: Copy + Clone>() {}
        assert_copy_clone::<Position>();
    }

    #[test]
    fn starting_position_matches_from_fen() {
        let from_const = Position::starting_position();
        let from_parser =
            Position::from_fen(Position::STARTING_FEN).expect("STARTING_FEN must parse");
        assert_eq!(from_const, from_parser);
    }

    // Zobrist field tests — prove the field is populated correctly, not just
    // that `zobrist::from_scratch` returns the right value.

    /// Starting-position Polyglot hash (test vector #1 from the published spec).
    #[test]
    fn zobrist_starting_position_matches_polyglot() {
        assert_eq!(Position::starting_position().zobrist(), 0x463b96181691fc9c,);
    }

    /// `empty()` initialises the zobrist field to zero; the parser fills it in.
    #[test]
    fn zobrist_empty_is_zero() {
        assert_eq!(Position::empty().zobrist(), 0);
    }

    /// FEN parser hooks `from_scratch`; result equals the same Polyglot vector #1.
    #[test]
    fn zobrist_field_present_after_parse() {
        assert_eq!(
            Position::from_fen(Position::STARTING_FEN)
                .unwrap()
                .zobrist(),
            0x463b96181691fc9c,
        );
    }

    #[test]
    fn to_fen_matches_display() {
        // The canonical six perft positions, pinned per docs/plans/m1.b.md.
        let canonical_six = [
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
            "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
            "r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1",
            "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
            "r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10",
        ];
        for s in canonical_six {
            let p = Position::from_fen(s).expect("canonical FEN must parse");
            assert_eq!(p.to_fen(), format!("{}", p));
        }
    }

    #[test]
    fn unit_round_trip_format_then_parse() {
        // Build a contrived but valid endgame:
        //   White: King e1, Rook a1, Pawn e4
        //   Black: King e8, Knight g8
        // White to move, no castling, no EP, halfmove 3, fullmove 17.
        let mut p = Position::empty();
        p.set_piece(Square::E1, WHITE_KING);
        p.set_piece(Square::A1, WHITE_ROOK);
        p.set_piece(Square::E4, WHITE_PAWN);
        p.set_piece(Square::E8, BLACK_KING);
        p.set_piece(Square::G8, BLACK_KNIGHT);
        p.set_aux_state(Color::White, CastlingRights::NONE, None, 3, 17);
        // set_piece/set_aux_state don't refresh the Zobrist field; do it once
        // at the end so structural equality with the FEN-roundtripped position
        // (whose parser refreshes zobrist) holds.
        p.refresh_zobrist();

        let formatted = format!("{}", p);
        let parsed = Position::from_fen(&formatted).expect("formatted FEN must parse");
        assert_eq!(parsed, p);
    }

    #[test]
    fn unit_round_trip_with_castling_and_ep() {
        // Companion to unit_round_trip_format_then_parse, but exercising the
        // castling and EP fields. Without this, a Display bug that mishandles
        // partial-castling output (e.g. emits "Kk" as "kK") or a non-trivial
        // EP square would not be caught at the unit-test layer.
        //
        // Setup (chess-coherent: white just played a d2-d4 double-push, leaving
        // black to move with rank-3 EP):
        //   White: King e1, Rook a1, Rook h1, Pawn d4
        //   Black: King e8, Rook a8
        // Black to move, white-king-side + black-queen-side castling only,
        // EP target d3, halfmove 0, fullmove 8.
        let mut p = Position::empty();
        p.set_piece(Square::E1, WHITE_KING);
        p.set_piece(Square::A1, WHITE_ROOK);
        p.set_piece(Square::H1, WHITE_ROOK);
        p.set_piece(Square::E8, BLACK_KING);
        p.set_piece(Square::A8, BLACK_ROOK);
        p.set_piece(Square::D4, WHITE_PAWN);
        let castling = CastlingRights::WHITE_KING.with(CastlingRights::BLACK_QUEEN);
        p.set_aux_state(Color::Black, castling, Some(Square::D3), 0, 8);
        // set_piece/set_aux_state don't refresh the Zobrist field; do it once
        // at the end so structural equality with the FEN-roundtripped position
        // holds.
        p.refresh_zobrist();

        let formatted = format!("{}", p);
        let parsed = Position::from_fen(&formatted).expect("formatted castle/EP FEN must parse");
        assert_eq!(parsed, p);
        // Sanity-check the formatted string actually carries the expected
        // castling and EP tokens (so the round-trip wasn't a self-cancelling
        // double Display+parse bug).
        let fields: Vec<&str> = formatted.split(' ').collect();
        assert_eq!(fields.len(), 6);
        assert_eq!(fields[2], "Kq", "castling field must use FEN order Kq");
        assert_eq!(fields[3], "d3", "EP field must serialize to d3");
    }

    #[test]
    fn debug_grid_midgame_snapshot() {
        // Pin the grid printer for a non-default position: black to move,
        // partial castling (white kingside only), EP square set, halfmove and
        // fullmove non-default. Catches summary-line bugs that the
        // starting-position snapshot misses. The rook on h1 plus king on e1
        // makes the white-kingside-castling rights coherent (visually).
        let mut p = Position::empty();
        p.set_piece(Square::E1, WHITE_KING);
        p.set_piece(Square::H1, WHITE_ROOK);
        p.set_piece(Square::E4, WHITE_PAWN);
        p.set_piece(Square::A8, BLACK_KING);
        p.set_aux_state(
            Color::Black,
            CastlingRights::WHITE_KING,
            Some(Square::E3),
            5,
            12,
        );
        let expected = concat!(
            "8 k . . . . . . .\n",
            "7 . . . . . . . .\n",
            "6 . . . . . . . .\n",
            "5 . . . . . . . .\n",
            "4 . . . . P . . .\n",
            "3 . . . . . . . .\n",
            "2 . . . . . . . .\n",
            "1 . . . . K . . R\n",
            "  a b c d e f g h\n",
            "Black to move, halfmove=5, fullmove=12, castling=K, ep=e3\n",
        );
        assert_eq!(format!("{:?}", p), expected);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic]
    fn set_piece_panics_on_king_overwrite() {
        let mut p = Position::empty();
        p.set_piece(Square::E1, WHITE_KING);
        p.set_piece(Square::E1, WHITE_QUEEN);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic]
    fn set_piece_panics_on_king_replaced_by_pawn() {
        let mut p = Position::empty();
        p.set_piece(Square::E1, WHITE_KING);
        p.set_piece(Square::E1, WHITE_PAWN);
    }

    // -------------------------------------------------------------------------
    // Coverage backfill: paths not exercised by the contract-driven tests above.
    // -------------------------------------------------------------------------

    /// `set_piece` has a branch that removes the previous piece when a square
    /// is overwritten by a non-king piece. All existing tests only place pieces
    /// on empty squares or trigger the debug-assert panic. This test exercises
    /// the "previous non-king piece is replaced" path (lines 349-352).
    #[test]
    fn set_piece_overwrites_non_king_updates_bitboards() {
        let mut p = Position::empty();
        // Place a white rook, then overwrite it with a white bishop.
        p.set_piece(Square::E4, WHITE_ROOK);
        // Verify rook is present before overwrite.
        assert!(
            p.pieces_colored(Color::White, PieceKind::Rook)
                .contains(Square::E4)
        );

        p.set_piece(Square::E4, WHITE_BISHOP);

        // After overwrite: bishop must be on e4, rook must not be.
        assert!(
            p.pieces_colored(Color::White, PieceKind::Bishop)
                .contains(Square::E4),
            "bishop must appear on e4 after overwrite"
        );
        assert!(
            !p.pieces_colored(Color::White, PieceKind::Rook)
                .contains(Square::E4),
            "rook must be removed from e4 after overwrite"
        );
        assert_eq!(
            p.piece_at(Square::E4),
            Some(WHITE_BISHOP),
            "mailbox must reflect the new piece"
        );
        // Color bitboard should still claim e4 for White.
        assert!(p.occupied(Color::White).contains(Square::E4));
    }

    /// `debug_assert_consistent` is called from many tests as a sanity gate
    /// but its body has never been observed to panic — every existing call
    /// site presents a consistent position. The next two tests poke private
    /// fields directly to construct broken states and confirm the function
    /// fires. Without them, replacing the body with `()` silently passes.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic]
    fn debug_assert_consistent_panics_on_broken_king_cache() {
        let mut p = Position::starting_position();
        // Corrupt the cached king square without touching the king bitboard.
        p.king_sq[Color::White.index()] = Square::A1;
        p.debug_assert_consistent();
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic]
    fn debug_assert_consistent_panics_on_mailbox_bitboard_drift() {
        let mut p = Position::starting_position();
        // Drop the white king from the mailbox without touching bitboards.
        p.mailbox[Square::E1.index() as usize] = None;
        p.debug_assert_consistent();
    }

    /// `validate_post_parse` checks for TooManyKings for both colors, but only
    /// the White path was reachable via `Position::from_fen` in the existing
    /// tests (the two-White-kings FEN). Cover the Black path directly.
    #[test]
    fn parse_rejects_two_black_kings() {
        // Two black kings: one on e8, one on d8. White king on e1.
        let r = Position::from_fen("3kk3/8/8/8/8/8/8/4K3 w - - 0 1");
        assert!(
            matches!(r, Err(FenError::TooManyKings(Color::Black))),
            "two black kings must yield TooManyKings(Black), got {r:?}"
        );
    }

    // ------------------------------------------------------------------------
    // Property tests (proptest).
    // ------------------------------------------------------------------------

    use proptest::prelude::*;

    /// All four `CastlingRights` named flags, paired with their target bit
    /// position. Used by the bit-packing property to cover every flag without
    /// repeating the list.
    const CR_FLAGS: [(CastlingRights, u8); 4] = [
        (CastlingRights::WHITE_KING, 0b0001),
        (CastlingRights::WHITE_QUEEN, 0b0010),
        (CastlingRights::BLACK_KING, 0b0100),
        (CastlingRights::BLACK_QUEEN, 0b1000),
    ];

    /// Build a CastlingRights from a 4-bit mask by `with`-ing each set flag.
    /// Mirrors how `parse_castling` constructs rights from FEN letters.
    fn rights_from_mask(mask: u8) -> CastlingRights {
        let mut r = CastlingRights::NONE;
        for (flag, bit) in CR_FLAGS {
            if mask & bit != 0 {
                r = r.with(flag);
            }
        }
        r
    }

    proptest! {
        /// Bit packing: a 4-bit mask routed through `with()` round-trips
        /// through `bits()`, every per-flag `has()` answers correctly, and
        /// `with`/`without` are idempotent on their respective set/clear
        /// state. Pins the bit layout to `bit 0 = WK, bit 1 = WQ, bit 2 = BK,
        /// bit 3 = BQ`, which is load-bearing for the Polyglot Zobrist
        /// castling-key convention (per the doc comment on CastlingRights).
        #[test]
        fn prop_castling_rights_bit_packing(mask in 0u8..16) {
            let r = rights_from_mask(mask);

            // bits() reflects exactly the constructed mask.
            prop_assert_eq!(r.bits(), mask);

            // has() answers correctly for every single flag.
            for (flag, bit) in CR_FLAGS {
                prop_assert_eq!(r.has(flag), mask & bit != 0);
            }

            // The "all bits clear" query (`has(NONE)`) returns false by
            // definition (the empty query is meaningless), regardless of mask.
            prop_assert!(!r.has(CastlingRights::NONE));

            // with(flag) sets the flag; without(flag) clears it. Both are
            // idempotent (asserted directly on the random base `r`, which
            // kills `|`-vs-`^` mutants on `with` and `&!`-vs-`^` on `without`
            // regardless of whether `r` already had the flag).
            for (flag, _) in CR_FLAGS {
                prop_assert!(r.with(flag).has(flag));
                prop_assert!(!r.without(flag).has(flag));

                prop_assert_eq!(r.with(flag).with(flag), r.with(flag));
                prop_assert_eq!(r.without(flag).without(flag), r.without(flag));

                // with then without unconditionally clears.
                prop_assert!(!r.with(flag).without(flag).has(flag));
                // without then with unconditionally sets.
                prop_assert!(r.without(flag).with(flag).has(flag));
            }
        }

        /// Multi-flag `has` query: `has(combo)` is true iff EVERY bit of
        /// `combo` is set in `self`. This pins the AND-not-OR semantics of
        /// `has` against the multi-flag query that the unit tests only check
        /// on a single hand-picked combo (`ALL.has(WK)`).
        #[test]
        fn prop_castling_has_multi_flag(mask in 0u8..16, combo in 0u8..16) {
            let r = rights_from_mask(mask);
            let combo_rights = rights_from_mask(combo);
            let expected = combo != 0 && (mask & combo) == combo;
            prop_assert_eq!(r.has(combo_rights), expected);
        }
    }
}
