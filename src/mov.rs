//! `Move`, `Undo`, and `make_move` / `unmake_move`.
//!
//! Per ADR-0004 (`docs/decisions/0004-nnue-hooks-from-day-one.md`) and the
//! M1.E plan (`docs/plans/m1.e.md`), this module implements the project's
//! sole supported state-transition surface. Move generation (M1.F) is the
//! only producer; search (M3+) is the only consumer.
//!
//! Module name is `mov` rather than `move` because `move` is a Rust
//! keyword. Crate-root re-exports (`Move`, `MoveFlag`, `Undo`) make this
//! invisible to user code.
//!
//! ## Encoding (16-bit `Move`)
//!
//! ```text
//!  15 14 13 12   11 10  9  8  7  6   5  4  3  2  1  0
//! [ flag (4)  ][ to-square (6)    ][ from-square (6) ]
//! ```
//!
//! The 4-bit flag nibble decomposes per CPW's [Encoding Moves] convention:
//! `[promotion][capture][special1][special0]`. 14 valid codes; codes 6 and
//! 7 are deliberately absent — they cannot be produced by any public
//! constructor.
//!
//! [Encoding Moves]: https://www.chessprogramming.org/Encoding_Moves

use std::fmt;

use crate::movegen::{MoveList, generate_moves};
use crate::piece::{Color, Piece, PieceKind};
use crate::position::{CastlingRights, Position};
use crate::square::Square;
use crate::zobrist;

// ---------------------------------------------------------------------------
// Const-time invariant pinning (Decision §11).
// ---------------------------------------------------------------------------

const _: () = {
    // Move flag discriminants — pinned so a future renumber breaks the build.
    assert!(MoveFlag::Quiet as u8 == 0);
    assert!(MoveFlag::DoublePush as u8 == 1);
    assert!(MoveFlag::KingCastle as u8 == 2);
    assert!(MoveFlag::QueenCastle as u8 == 3);
    assert!(MoveFlag::Capture as u8 == 4);
    assert!(MoveFlag::EnPassant as u8 == 5);
    assert!(MoveFlag::KnightPromo as u8 == 8);
    assert!(MoveFlag::BishopPromo as u8 == 9);
    assert!(MoveFlag::RookPromo as u8 == 10);
    assert!(MoveFlag::QueenPromo as u8 == 11);
    assert!(MoveFlag::KnightPromoCapture as u8 == 12);
    assert!(MoveFlag::BishopPromoCapture as u8 == 13);
    assert!(MoveFlag::RookPromoCapture as u8 == 14);
    assert!(MoveFlag::QueenPromoCapture as u8 == 15);
};

const _: () = {
    // Capture-flag bit (bit 2 of the 4-bit flag nibble).
    assert!((MoveFlag::Capture as u8) & 0b0100 != 0);
    assert!((MoveFlag::EnPassant as u8) & 0b0100 != 0);
    assert!((MoveFlag::KnightPromoCapture as u8) & 0b0100 != 0);
    assert!((MoveFlag::QueenPromoCapture as u8) & 0b0100 != 0);
    assert!((MoveFlag::Quiet as u8) & 0b0100 == 0);
    assert!((MoveFlag::DoublePush as u8) & 0b0100 == 0);
    assert!((MoveFlag::KnightPromo as u8) & 0b0100 == 0);
};

const _: () = {
    // Promotion-flag bit (bit 3 of the 4-bit flag nibble).
    assert!((MoveFlag::KnightPromo as u8) & 0b1000 != 0);
    assert!((MoveFlag::QueenPromo as u8) & 0b1000 != 0);
    assert!((MoveFlag::QueenPromoCapture as u8) & 0b1000 != 0);
    assert!((MoveFlag::Capture as u8) & 0b1000 == 0);
    assert!((MoveFlag::EnPassant as u8) & 0b1000 == 0);
};

// ---------------------------------------------------------------------------
// MoveFlag — 4-bit flag nibble.
// ---------------------------------------------------------------------------

/// Flag nibble of a [`Move`]. 14 valid variants; codes 6 and 7 are
/// deliberately absent — there is no public path to construct a `Move`
/// with those bits.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum MoveFlag {
    Quiet = 0,
    DoublePush = 1,
    KingCastle = 2,
    QueenCastle = 3,
    Capture = 4,
    EnPassant = 5,
    // Codes 6 and 7 deliberately absent.
    KnightPromo = 8,
    BishopPromo = 9,
    RookPromo = 10,
    QueenPromo = 11,
    KnightPromoCapture = 12,
    BishopPromoCapture = 13,
    RookPromoCapture = 14,
    QueenPromoCapture = 15,
}

impl MoveFlag {
    /// Decode a 4-bit nibble. Returns `None` for codes 6 and 7.
    #[inline]
    const fn from_bits(b: u8) -> Option<MoveFlag> {
        match b {
            0 => Some(MoveFlag::Quiet),
            1 => Some(MoveFlag::DoublePush),
            2 => Some(MoveFlag::KingCastle),
            3 => Some(MoveFlag::QueenCastle),
            4 => Some(MoveFlag::Capture),
            5 => Some(MoveFlag::EnPassant),
            8 => Some(MoveFlag::KnightPromo),
            9 => Some(MoveFlag::BishopPromo),
            10 => Some(MoveFlag::RookPromo),
            11 => Some(MoveFlag::QueenPromo),
            12 => Some(MoveFlag::KnightPromoCapture),
            13 => Some(MoveFlag::BishopPromoCapture),
            14 => Some(MoveFlag::RookPromoCapture),
            15 => Some(MoveFlag::QueenPromoCapture),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Move — 16-bit packed (from, to, flag).
// ---------------------------------------------------------------------------

/// 16-bit packed encoding of a chess move: bits 0–5 from-square,
/// bits 6–11 to-square, bits 12–15 flag nibble. See module docs.
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Move(u16);

impl Move {
    /// Encode `(from, to, flag)`.
    #[inline]
    pub const fn new(from: Square, to: Square, flag: MoveFlag) -> Move {
        let bits = (flag as u16) << 12 | (to.index() as u16) << 6 | (from.index() as u16);
        Move(bits)
    }

    /// Convenience: `Move::new(from, to, MoveFlag::Quiet)`.
    #[inline]
    pub const fn quiet(from: Square, to: Square) -> Move {
        Move::new(from, to, MoveFlag::Quiet)
    }

    /// Convenience: `Move::new(from, to, MoveFlag::Capture)`. Use
    /// [`MoveFlag::EnPassant`] for EP captures and the four
    /// `*PromoCapture` flags for promotion-captures — those have different
    /// flag bits than plain `Capture`.
    #[inline]
    pub const fn capture(from: Square, to: Square) -> Move {
        Move::new(from, to, MoveFlag::Capture)
    }

    #[inline]
    pub const fn from_square(self) -> Square {
        // Square::new_unchecked is OK because the bottom 6 bits are always in 0..64.
        Square::new_unchecked((self.0 & 0x3F) as u8)
    }

    #[inline]
    pub const fn to_square(self) -> Square {
        Square::new_unchecked(((self.0 >> 6) & 0x3F) as u8)
    }

    #[inline]
    pub const fn flag(self) -> MoveFlag {
        let bits = ((self.0 >> 12) & 0xF) as u8;
        match MoveFlag::from_bits(bits) {
            Some(f) => f,
            // No-arg `unreachable!()` is const-callable since Rust 1.57
            // (`panic!` with a static literal). Format-arg interpolation
            // (`unreachable!("…{bits}…")`) is NOT yet const-callable on
            // stable, so the no-arg form is the only option here. The
            // panic message is "internal error: entered unreachable code"
            // plus the file:line location — sufficient diagnostics for an
            // arm that's provably unreachable (Move::new is the only
            // public constructor and takes a typed MoveFlag).
            None => unreachable!(),
        }
    }

    /// `true` for [`MoveFlag::Capture`], [`MoveFlag::EnPassant`], and the
    /// four `*PromoCapture` flags. Equivalent to "the flag has bit 2 set"
    /// per the CPW capture-bit convention.
    #[inline]
    pub const fn is_capture(self) -> bool {
        // Bit 14 of the 16-bit move = bit 2 of the 4-bit flag nibble.
        (self.0 & (1 << 14)) != 0
    }

    /// `true` for the four `*Promo` and four `*PromoCapture` flags.
    #[inline]
    pub const fn is_promotion(self) -> bool {
        // Bit 15 of the 16-bit move = bit 3 of the 4-bit flag nibble.
        (self.0 & (1 << 15)) != 0
    }

    /// `true` for [`MoveFlag::KingCastle`] and [`MoveFlag::QueenCastle`].
    #[inline]
    pub const fn is_castling(self) -> bool {
        matches!(self.flag(), MoveFlag::KingCastle | MoveFlag::QueenCastle)
    }

    /// `Some(N|B|R|Q)` iff this move is a promotion (with or without
    /// capture); `None` for all non-promotion flags.
    #[inline]
    pub const fn promotion_kind(self) -> Option<PieceKind> {
        match self.flag() {
            MoveFlag::KnightPromo | MoveFlag::KnightPromoCapture => Some(PieceKind::Knight),
            MoveFlag::BishopPromo | MoveFlag::BishopPromoCapture => Some(PieceKind::Bishop),
            MoveFlag::RookPromo | MoveFlag::RookPromoCapture => Some(PieceKind::Rook),
            MoveFlag::QueenPromo | MoveFlag::QueenPromoCapture => Some(PieceKind::Queen),
            _ => None,
        }
    }

    /// Raw 16-bit encoding — for stable comparison and tests.
    #[inline]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// UCI long-algebraic encoding of this move. Examples: `"e2e4"`,
    /// `"e1g1"` (king-side castle), `"e7e8q"` (queen promotion).
    /// Produces identical bytes to the `Display` impl on `Move`;
    /// `Display::fmt` is the canonical writer (zero-alloc when piped to
    /// a `Write`), and this method exists for self-documenting call
    /// sites in M2 protocol code.
    pub fn to_uci(self) -> String {
        self.to_string()
    }

    /// Parse a UCI long-algebraic move string in the context of `pos`.
    ///
    /// Returns `IllegalForPosition` if no legal move in `pos` matches the
    /// requested `(from, to, promotion_kind)` triple — this subsumes the
    /// many ways a syntactically valid string can fail to identify a real
    /// legal move (no piece on `from`, opposite-color piece on `from`,
    /// blocked target, missing castling rights, EP target not set, promo
    /// where none is legal, missing promo where one is required, would
    /// leave the king in check, etc.).
    ///
    /// Rejects the literal string `"0000"` with `NullMove`. The UCI spec
    /// describes `0000` as engine→GUI only; we have no `Move::NULL`
    /// sentinel today (deferred to null-move pruning, M5).
    ///
    /// Rejects strings of any length other than 4 or 5, non-ASCII input,
    /// and malformed square substrings with `Malformed`. Rejects 5-char
    /// strings whose promo letter is not one of `n`/`b`/`r`/`q` (lowercase)
    /// with `IllegalPromotionPiece`.
    pub fn from_uci(s: &str, pos: &Position) -> Result<Move, UciMoveError> {
        // 1. Reject the null move explicitly.
        if s == "0000" {
            return Err(UciMoveError::NullMove);
        }

        // 2. ASCII-only guard. Without this, the byte slices below can
        //    panic on non-char-boundary indices. Concretely: the input
        //    `"e2e°"` is 5 bytes (`e`/`2`/`e` = 3 ASCII bytes plus `°`
        //    = 2 UTF-8 bytes 0xC2 0xB0). It passes the length-5 check
        //    in step 3, then `&s[2..4]` would end at byte 4 — the
        //    middle of the `°` codepoint — and panic.
        if !s.is_ascii() {
            return Err(UciMoveError::Malformed);
        }

        // 3. Length: only 4 or 5 are accepted.
        let bytes = s.as_bytes();
        if bytes.len() != 4 && bytes.len() != 5 {
            return Err(UciMoveError::Malformed);
        }

        // 4. Parse the squares first, then the promo letter. Order
        //    matters for error categorization: a malformed square
        //    dominates a malformed promo letter.
        let from = Square::parse_uci(&s[0..2]).ok_or(UciMoveError::Malformed)?;
        let to = Square::parse_uci(&s[2..4]).ok_or(UciMoveError::Malformed)?;
        let promo: Option<PieceKind> = if bytes.len() == 5 {
            match bytes[4] {
                b'n' => Some(PieceKind::Knight),
                b'b' => Some(PieceKind::Bishop),
                b'r' => Some(PieceKind::Rook),
                b'q' => Some(PieceKind::Queen),
                _ => return Err(UciMoveError::IllegalPromotionPiece),
            }
        } else {
            None
        };

        // 5. Generate-and-match. `MoveList` may be empty (checkmate /
        //    stalemate); `find` on an empty iterator returns None
        //    cleanly. The (from, to, promotion_kind) match key uniquely
        //    identifies a legal move; see the M2.A plan for the
        //    chess-rules derivation of that uniqueness invariant.
        let mut moves = MoveList::new();
        generate_moves(pos, &mut moves);
        let slice = moves.as_slice();
        let pred = |mv: &&Move| {
            mv.from_square() == from && mv.to_square() == to && mv.promotion_kind() == promo
        };
        let result = slice.iter().find(pred).copied();
        // Defense-in-depth: the uniqueness invariant is pinned externally
        // by `prop_at_most_one_legal_move_per_from_to_promo`, but if it
        // ever silently broke (movegen refactor, etc.), `find` would
        // pick the first match and `from_uci` would parse the wrong
        // move. Debug-only check so a regression fires loudly at the
        // consumer site, not just in the property test. Release trusts
        // the invariant — no overhead on the protocol path.
        debug_assert!(
            result.is_none() || slice.iter().filter(pred).count() == 1,
            "(from, to, promo) uniqueness violated for from={from:?}, to={to:?}, promo={promo:?} in pos={}",
            pos.to_fen(),
        );
        result.ok_or(UciMoveError::IllegalForPosition)
    }
}

// ---------------------------------------------------------------------------
// UciMoveError — error type for `Move::from_uci`.
// ---------------------------------------------------------------------------

/// Reasons `Move::from_uci` can reject an input string.
///
/// See `Move::from_uci` for the categorization rules. The four buckets
/// give M2.B/C readable error messages without duplicating chunks of
/// `generate_moves` for diagnostic-only purposes.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum UciMoveError {
    /// Length not 4 or 5; non-ASCII input; or one of the square
    /// substrings is not a valid algebraic square.
    Malformed,
    /// Length is 5 but the promo letter is not one of `n`, `b`, `r`,
    /// `q` (lowercase per spec). Pawn `p` and king `k` are
    /// syntactically possible SAN piece letters but illegal as
    /// promotion targets per FIDE rules. Uppercase `N`/`B`/`R`/`Q`
    /// also rejects here.
    IllegalPromotionPiece,
    /// The literal string `"0000"`. UCI describes `0000` as engine→GUI
    /// only; we have no `Move::NULL` sentinel today.
    NullMove,
    /// The string parsed cleanly but no legal move in the position
    /// matches the requested `(from, to, promotion_kind)` triple.
    IllegalForPosition,
}

impl fmt::Display for UciMoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            UciMoveError::Malformed => {
                "malformed UCI move (expected 4 or 5 ASCII chars: `from-to[promo]`)"
            }
            UciMoveError::IllegalPromotionPiece => {
                "illegal UCI promotion piece (expected lowercase `n`, `b`, `r`, or `q`)"
            }
            UciMoveError::NullMove => {
                "null move `0000` not accepted (UCI defines it as engine→GUI only)"
            }
            UciMoveError::IllegalForPosition => "UCI move not legal in the given position",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for UciMoveError {}

impl fmt::Display for Move {
    /// Long algebraic, lowercase. Quiet/capture/castle/EP all format as
    /// `from-to` (4 chars, e.g. `e2e4`, `e1g1`, `e5d6`). Promotions append
    /// the lowercase promotion-kind letter (`e7e8q`, `e7f8n`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.from_square(), self.to_square())?;
        if let Some(kind) = self.promotion_kind() {
            // san_letter() returns lowercase ASCII (`p`/`n`/`b`/`r`/`q`/`k`).
            // Pawns and kings cannot be promotion targets, so the letter is
            // always one of n/b/r/q here.
            write!(f, "{}", kind.san_letter() as char)?;
        }
        Ok(())
    }
}

impl fmt::Debug for Move {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Move({}-{}, {:?})",
            self.from_square(),
            self.to_square(),
            self.flag()
        )
    }
}

// ---------------------------------------------------------------------------
// Undo — token returned by `make_move` and consumed by `unmake_move`.
// ---------------------------------------------------------------------------

/// Reversal token for a previous `make_move` call. Carries the minimum
/// state to undo the change byte-for-byte. ~16 bytes after alignment.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Undo {
    /// Piece removed from the board, or `None` for non-captures. For en
    /// passant, the captured pawn was on a different square than `mv.to`
    /// (recoverable from `mv` alone via the captured-square arithmetic in
    /// `make_move`).
    pub captured: Option<Piece>,
    pub prior_castling: CastlingRights,
    pub prior_ep: Option<Square>,
    pub prior_halfmove: u8,
    pub prior_zobrist: u64,
}

// ---------------------------------------------------------------------------
// Castling-rights mask table (Decision §6).
// ---------------------------------------------------------------------------

/// `new_castling = old_castling & MASK[from] & MASK[to]`. The mask is
/// `ALL` everywhere except the six rights-affecting squares (a1, e1, h1,
/// a8, e8, h8). See plan Decision §6 for soundness argument.
const CASTLING_MASK: [CastlingRights; 64] = build_castling_mask();

const fn build_castling_mask() -> [CastlingRights; 64] {
    let mut mask = [CastlingRights::ALL; 64];
    mask[Square::A1.index() as usize] = CastlingRights::ALL.without(CastlingRights::WHITE_QUEEN);
    mask[Square::H1.index() as usize] = CastlingRights::ALL.without(CastlingRights::WHITE_KING);
    mask[Square::E1.index() as usize] = CastlingRights::ALL
        .without(CastlingRights::WHITE_KING)
        .without(CastlingRights::WHITE_QUEEN);
    mask[Square::A8.index() as usize] = CastlingRights::ALL.without(CastlingRights::BLACK_QUEEN);
    mask[Square::H8.index() as usize] = CastlingRights::ALL.without(CastlingRights::BLACK_KING);
    mask[Square::E8.index() as usize] = CastlingRights::ALL
        .without(CastlingRights::BLACK_KING)
        .without(CastlingRights::BLACK_QUEEN);
    mask
}

// ---------------------------------------------------------------------------
// Castling rook squares (Decision §12).
// ---------------------------------------------------------------------------

#[inline]
const fn castle_rook_squares(us: Color, flag: MoveFlag) -> (Square, Square) {
    match (us, flag) {
        (Color::White, MoveFlag::KingCastle) => (Square::H1, Square::F1),
        (Color::White, MoveFlag::QueenCastle) => (Square::A1, Square::D1),
        (Color::Black, MoveFlag::KingCastle) => (Square::H8, Square::F8),
        (Color::Black, MoveFlag::QueenCastle) => (Square::A8, Square::D8),
        // No-arg `unreachable!()` for the same reason as `Move::flag`'s
        // catch-all — formatted-message interpolation in const fn is not
        // yet stable. Both call sites of this function are inside `match`
        // arms gated on `flag` being `KingCastle` or `QueenCastle`, so
        // this arm is provably unreachable.
        _ => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// make_move / unmake_move — STUBBED in pass 5.0 (M1.E plan §"Step 5.0").
// Filled in at step 7.B / 7.C / 7.D.
// ---------------------------------------------------------------------------

/// Apply `mv` to `pos`, returning an [`Undo`] sufficient to reverse it.
///
/// **Trusts the caller.** This function does not validate move legality
/// (or even pseudo-legality). Movegen (M1.F) is responsible for emitting
/// only legal moves. Debug builds add cross-field consistency assertions
/// and an incremental-Zobrist round-trip check; release builds trust the
/// incremental delta exclusively.
pub fn make_move(pos: &mut Position, mv: Move) -> Undo {
    let from = mv.from_square();
    let to = mv.to_square();
    let flag = mv.flag();
    let mover = pos
        .piece_at(from)
        .expect("make_move: source square is empty");
    let us = mover.color;
    let them = us.flip();

    // Determine the captured square (differs from `to` only on EP). The
    // captured pawn for EP sits on the same file as `to`, one rank back
    // toward us. EP target is rank 6 (idx 5) for white, rank 3 (idx 2)
    // for black, so the captured pawn is on rank 5 (idx 4) or rank 4
    // (idx 3) respectively.
    let capture_sq: Square = if flag == MoveFlag::EnPassant {
        let rank = match us {
            Color::White => to.rank() - 1,
            Color::Black => to.rank() + 1,
        };
        Square::from_file_rank(to.file(), rank).expect("EP capture square in range")
    } else {
        to
    };

    let captured: Option<Piece> = if mv.is_capture() {
        let p = pos.piece_at(capture_sq);
        debug_assert!(
            p.map(|p| p.color == them && p.kind != PieceKind::King)
                .unwrap_or(false),
            "capture flag requires opponent non-king piece on capture square; mv={mv:?}, capture_sq={capture_sq:?}",
        );
        p
    } else {
        debug_assert!(
            pos.piece_at(to).is_none(),
            "non-capture must land on empty square; mv={mv:?}, occupant={:?}",
            pos.piece_at(to),
        );
        None
    };

    let undo = Undo {
        captured,
        prior_castling: pos.castling_rights(),
        prior_ep: pos.ep_target(),
        prior_halfmove: pos.halfmove_clock(),
        prior_zobrist: pos.zobrist(),
    };

    // Stash prior position's EP-key contribution before mutation. The
    // predicate inspects side-to-move's pawn bitboard, which our move is
    // about to perturb. Plan §9 step 2.
    let prior_ep_file: Option<u8> = zobrist::ep_file_to_hash(pos);

    // ----- Mutate bitboards / mailbox -----
    match flag {
        MoveFlag::Quiet | MoveFlag::DoublePush => {
            pos.clear_square(from);
            pos.set_piece(to, mover);
        }
        MoveFlag::Capture => {
            pos.clear_square(from);
            pos.clear_square(to); // capture_sq == to here
            pos.set_piece(to, mover);
        }
        MoveFlag::EnPassant => {
            pos.clear_square(from);
            pos.clear_square(capture_sq); // captured pawn NOT on `to`
            pos.set_piece(to, mover);
        }
        MoveFlag::KingCastle | MoveFlag::QueenCastle => {
            pos.clear_square(from);
            let (rook_from, rook_to) = castle_rook_squares(us, flag);
            pos.clear_square(rook_from);
            pos.set_piece(to, mover);
            pos.set_piece(rook_to, Piece::new(us, PieceKind::Rook));
        }
        MoveFlag::KnightPromo
        | MoveFlag::BishopPromo
        | MoveFlag::RookPromo
        | MoveFlag::QueenPromo => {
            pos.clear_square(from);
            let promo_kind = mv.promotion_kind().expect("promo flag has kind");
            pos.set_piece(to, Piece::new(us, promo_kind));
        }
        MoveFlag::KnightPromoCapture
        | MoveFlag::BishopPromoCapture
        | MoveFlag::RookPromoCapture
        | MoveFlag::QueenPromoCapture => {
            pos.clear_square(from);
            pos.clear_square(to); // capture_sq == to
            let promo_kind = mv.promotion_kind().expect("promo flag has kind");
            pos.set_piece(to, Piece::new(us, promo_kind));
        }
    }

    // ----- Aux state -----
    let new_castling = undo.prior_castling
        & CASTLING_MASK[from.index() as usize]
        & CASTLING_MASK[to.index() as usize];

    let new_ep: Option<Square> = if flag == MoveFlag::DoublePush {
        // Skipped square is at the file of from/to and the rank between
        // them. White: from rank-idx 1 → skip rank-idx 2. Black: from
        // rank-idx 6 → skip rank-idx 5.
        let rank = match us {
            Color::White => from.rank() + 1,
            Color::Black => from.rank() - 1,
        };
        Some(Square::from_file_rank(from.file(), rank).expect("double-push skip square in range"))
    } else {
        None
    };

    let new_halfmove: u8 = if mover.kind == PieceKind::Pawn || mv.is_capture() {
        0
    } else {
        // Saturating add — engines won't reach 256-ply quiet runs without
        // hitting the 75-move automatic draw, but saturate defensively.
        undo.prior_halfmove.saturating_add(1)
    };

    let new_fullmove: u16 = if us == Color::Black {
        pos.fullmove_number() + 1
    } else {
        pos.fullmove_number()
    };

    pos.set_aux_state(them, new_castling, new_ep, new_halfmove, new_fullmove);

    // After set_aux_state, pos.side_to_move() is `them` and
    // pos.ep_target() is `new_ep` — exactly the inputs
    // zobrist::ep_file_to_hash needs to compute the post-make EP
    // contribution. Ordering is load-bearing: this read must follow
    // set_aux_state.
    let new_ep_file: Option<u8> = zobrist::ep_file_to_hash(pos);

    // ----- Incremental Zobrist -----
    update_zobrist_after_make(
        pos,
        mv,
        mover,
        &undo,
        captured,
        capture_sq,
        prior_ep_file,
        new_ep_file,
        new_castling,
    );

    // ----- Debug assertions -----
    // `from_scratch` runs in debug only; the release path trusts the
    // incremental delta exclusively (validated by the always-on
    // `make_move_no_from_scratch_in_release` perf sentinel).
    #[cfg(debug_assertions)]
    {
        pos.debug_assert_consistent();
        debug_assert_eq!(
            pos.zobrist(),
            zobrist::from_scratch(pos),
            "incremental zobrist diverged after make_move {mv:?}",
        );
    }

    undo
}

/// Reverse a previous `make_move`. Trusts that `(mv, undo)` came from a
/// matching `make_move` call on the current `pos`.
pub fn unmake_move(pos: &mut Position, mv: Move, undo: Undo) {
    let from = mv.from_square();
    let to = mv.to_square();
    let flag = mv.flag();

    // After make, `pos.side_to_move()` is `them` (the side that didn't
    // move). The mover was the *opposite* color. Recover `us` by
    // flipping.
    let us = pos.side_to_move().flip();

    let capture_sq: Square = if flag == MoveFlag::EnPassant {
        let rank = match us {
            Color::White => to.rank() - 1,
            Color::Black => to.rank() + 1,
        };
        Square::from_file_rank(to.file(), rank).expect("EP capture square in range")
    } else {
        to
    };

    match flag {
        MoveFlag::Quiet | MoveFlag::DoublePush => {
            let mover = pos
                .piece_at(to)
                .expect("unmake: `to` square must be occupied post-make");
            pos.clear_square(to);
            pos.set_piece(from, mover);
        }
        MoveFlag::Capture => {
            let mover = pos
                .piece_at(to)
                .expect("unmake capture: `to` empty post-make");
            pos.clear_square(to);
            pos.set_piece(from, mover);
            pos.set_piece(
                capture_sq,
                undo.captured.expect("capture flag has captured piece"),
            );
        }
        MoveFlag::EnPassant => {
            let mover = pos.piece_at(to).expect("unmake EP: `to` empty post-make");
            pos.clear_square(to);
            pos.set_piece(from, mover);
            pos.set_piece(capture_sq, undo.captured.expect("EP has captured pawn"));
        }
        MoveFlag::KingCastle | MoveFlag::QueenCastle => {
            let king = pos
                .piece_at(to)
                .expect("unmake castle: king missing on `to`");
            let (rook_from, rook_to) = castle_rook_squares(us, flag);
            let rook = pos
                .piece_at(rook_to)
                .expect("unmake castle: rook missing on rook_to");
            pos.clear_square(to);
            pos.clear_square(rook_to);
            pos.set_piece(from, king);
            pos.set_piece(rook_from, rook);
        }
        MoveFlag::KnightPromo
        | MoveFlag::BishopPromo
        | MoveFlag::RookPromo
        | MoveFlag::QueenPromo => {
            // `to` holds the promoted piece; replace with a pawn on `from`.
            pos.clear_square(to);
            pos.set_piece(from, Piece::new(us, PieceKind::Pawn));
        }
        MoveFlag::KnightPromoCapture
        | MoveFlag::BishopPromoCapture
        | MoveFlag::RookPromoCapture
        | MoveFlag::QueenPromoCapture => {
            pos.clear_square(to);
            pos.set_piece(from, Piece::new(us, PieceKind::Pawn));
            pos.set_piece(
                capture_sq,
                undo.captured.expect("promo-capture has captured piece"),
            );
        }
    }

    // Restore aux state. Fullmove is recomputable from `us`: if the side
    // that just moved was Black, make incremented fullmove; we decrement.
    let restored_fullmove = if us == Color::Black {
        pos.fullmove_number() - 1
    } else {
        pos.fullmove_number()
    };
    pos.set_aux_state(
        us,
        undo.prior_castling,
        undo.prior_ep,
        undo.prior_halfmove,
        restored_fullmove,
    );

    // Restore zobrist directly from undo. unmake's zobrist correctness
    // is structural: we wrote prior_zobrist into Undo at make-time.
    pos.refresh_zobrist_from(undo.prior_zobrist);

    #[cfg(debug_assertions)]
    {
        pos.debug_assert_consistent();
        debug_assert_eq!(
            pos.zobrist(),
            zobrist::from_scratch(pos),
            "unmake restored zobrist disagrees with from_scratch",
        );
    }
}

/// Apply the incremental Zobrist delta after `make_move` has completed
/// the bitboard/mailbox/aux-state mutations. Plan §9.
///
/// The helper is **order-agnostic at its boundary**: every relevant
/// prior/new value is an explicit by-value parameter, so the helper reads
/// no mutable state from `pos` other than to write the new zobrist via
/// `refresh_zobrist_from`. The caller's only ordering constraint is that
/// `prior_ep_file` is captured pre-mutation and `new_ep_file` is captured
/// post-`set_aux_state`.
#[allow(clippy::too_many_arguments)]
fn update_zobrist_after_make(
    pos: &mut Position,
    mv: Move,
    mover: Piece,
    undo: &Undo,
    captured: Option<Piece>,
    capture_sq: Square,
    prior_ep_file: Option<u8>,
    new_ep_file: Option<u8>,
    new_castling: CastlingRights,
) {
    let from = mv.from_square();
    let to = mv.to_square();
    let flag = mv.flag();
    let us = mover.color;
    let mut z = undo.prior_zobrist;

    // (a) Mover keys: XOR out from `from`, XOR in on `to`. Promotions
    //     XOR in the *promoted* piece, NOT the pawn.
    z ^= zobrist::piece_key(mover, from);
    let lands_as: Piece = match mv.promotion_kind() {
        Some(kind) => Piece::new(us, kind),
        None => mover,
    };
    z ^= zobrist::piece_key(lands_as, to);

    // (b) Captured key: XOR out the captured piece on its actual square
    //     (capture_sq, which equals `to` except for EP).
    if let Some(victim) = captured {
        z ^= zobrist::piece_key(victim, capture_sq);
    }

    // (c) Castling rook movement: also toggle rook-from and rook-to keys.
    if flag == MoveFlag::KingCastle || flag == MoveFlag::QueenCastle {
        let (rook_from, rook_to) = castle_rook_squares(us, flag);
        let rook = Piece::new(us, PieceKind::Rook);
        z ^= zobrist::piece_key(rook, rook_from);
        z ^= zobrist::piece_key(rook, rook_to);
    }

    // (d) Castling rights delta. XOR each right that changed. Computed
    //     from explicit values — does NOT read pos.castling_rights().
    let prior_bits = undo.prior_castling.bits();
    let new_bits = new_castling.bits();
    let delta = prior_bits ^ new_bits;
    if delta != 0 {
        for &right in &[
            CastlingRights::WHITE_KING,
            CastlingRights::WHITE_QUEEN,
            CastlingRights::BLACK_KING,
            CastlingRights::BLACK_QUEEN,
        ] {
            if delta & right.bits() != 0 {
                z ^= zobrist::castling_key(right);
            }
        }
    }

    // (e) EP file delta — explicit values, no reads on pos.
    if let Some(file) = prior_ep_file {
        z ^= zobrist::ep_file_key(file);
    }
    if let Some(file) = new_ep_file {
        z ^= zobrist::ep_file_key(file);
    }

    // (f) Turn key: side-to-move flipped, so toggle. Polyglot's
    //     side-to-move asymmetry (XOR iff WHITE) is preserved by this
    //     unconditional toggle: from white-to-move, we XOR out (turn_key
    //     was contributed); from black-to-move, we XOR in. Both via the
    //     same XOR.
    z ^= zobrist::turn_key();

    pos.refresh_zobrist_from(z);
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::piece::{Color, Piece, PieceKind};
    use crate::position::{CastlingRights, Position};
    use crate::square::Square;

    // -----------------------------------------------------------------------
    // Curated test corpus (Decision §"CASES table shape" in plan).
    //
    // Tuple shape: (starting_fen, mv, expected_after_fen).
    // The expected_after_fen is hand-computed and verified against Stockfish
    // 18 via:  printf 'position fen <FEN> moves <UCI>\nd\nquit\n' | stockfish
    // The verification is a pre-commit step; the tests themselves don't run
    // Stockfish.
    // -----------------------------------------------------------------------

    /// One curated case.
    struct Case {
        before: &'static str,
        mv: Move,
        after: &'static str,
        label: &'static str,
    }

    /// Quiet, double-push, single-pawn-push, capture, EP, castling, promotion,
    /// promo-capture cases plus rook-corner-castling-loss edges. Used by both
    /// the curated unit tests below and the round-trip property tests.
    const CASES: &[Case] = &[
        // ---------- Quiet moves ----------
        Case {
            before: "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
            mv: Move::quiet(Square::E2, Square::E3),
            after: "rnbqkbnr/pppppppp/8/8/8/4P3/PPPP1PPP/RNBQKBNR b KQkq - 0 1",
            label: "quiet_pawn_single_push_white",
        },
        Case {
            before: "rnbqkbnr/pppppppp/8/8/8/4P3/PPPP1PPP/RNBQKBNR b KQkq - 0 1",
            mv: Move::quiet(Square::E7, Square::E6),
            after: "rnbqkbnr/pppp1ppp/4p3/8/8/4P3/PPPP1PPP/RNBQKBNR w KQkq - 0 2",
            label: "quiet_pawn_single_push_black",
        },
        Case {
            before: "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
            mv: Move::quiet(Square::G1, Square::F3),
            after: "rnbqkbnr/pppppppp/8/8/8/5N2/PPPPPPPP/RNBQKB1R b KQkq - 1 1",
            label: "quiet_knight_move",
        },
        Case {
            before: "4k3/8/8/8/8/8/8/4K2R w K - 0 1",
            mv: Move::quiet(Square::E1, Square::E2),
            after: "4k3/8/8/8/8/8/4K3/7R b - - 1 1",
            label: "quiet_white_king_clears_castling",
        },
        // Castling-rights field uses only the rights actually held. `KQ`
        // with no h1 rook is rejected as inconsistent by stricter parsers
        // (Stockfish strips it on output), so we use rights consistent
        // with the rooks present.
        Case {
            before: "4k3/8/8/8/8/8/8/R3K3 w Q - 0 1",
            mv: Move::quiet(Square::A1, Square::A2),
            after: "4k3/8/8/8/8/8/R7/4K3 b - - 1 1",
            label: "quiet_a1_rook_clears_white_queen",
        },
        Case {
            before: "4k3/8/8/8/8/8/8/4K2R w K - 0 1",
            mv: Move::quiet(Square::H1, Square::H2),
            after: "4k3/8/8/8/8/8/7R/4K3 b - - 1 1",
            label: "quiet_h1_rook_clears_white_king",
        },
        Case {
            before: "r3k3/8/8/8/8/8/8/4K3 b q - 0 1",
            mv: Move::quiet(Square::E8, Square::E7),
            after: "r7/4k3/8/8/8/8/8/4K3 w - - 1 2",
            label: "quiet_black_king_clears_castling",
        },
        Case {
            before: "r3k3/8/8/8/8/8/8/4K3 b q - 0 1",
            mv: Move::quiet(Square::A8, Square::A7),
            after: "4k3/r7/8/8/8/8/8/4K3 w - - 1 2",
            label: "quiet_a8_rook_clears_black_queen",
        },
        Case {
            before: "4k2r/8/8/8/8/8/8/4K3 b k - 0 1",
            mv: Move::quiet(Square::H8, Square::H7),
            after: "4k3/7r/8/8/8/8/8/4K3 w - - 1 2",
            label: "quiet_h8_rook_clears_black_king",
        },
        // ---------- Double pushes ----------
        // The first two cases use positions WITH a capturer present so
        // Stockfish (X-FEN: drop EP if no capturer) and our engine (FEN-spec
        // literal: always set EP after a double-push) produce identical
        // expected FENs — these are Stockfish-verifiable on `after`. The
        // `double_push_no_capturer` case below intentionally diverges from
        // Stockfish on the EP-target field; that's the deliberate
        // architecture commitment from `docs/architecture.md` (EP target
        // stored on every double-push; Polyglot pseudo-legal filter applies
        // only at zobrist-XOR time per ADR-0009).
        Case {
            before: "4k3/8/8/8/5p2/8/4P3/4K3 w - - 0 1",
            mv: Move::new(Square::E2, Square::E4, MoveFlag::DoublePush),
            after: "4k3/8/8/8/4Pp2/8/8/4K3 b - e3 0 1",
            label: "double_push_white_sets_ep_e3",
        },
        Case {
            before: "4k3/4p3/8/5P2/8/8/8/4K3 b - - 0 1",
            mv: Move::new(Square::E7, Square::E5, MoveFlag::DoublePush),
            after: "4k3/8/8/4pP2/8/8/8/4K3 w - e6 0 2",
            label: "double_push_black_sets_ep_e6",
        },
        // White double-push with NO black capturer adjacent — Polyglot
        // pseudo-legal rule means the EP key is NOT XORed in zobrist.
        // Our engine still sets the EP target in the position state per
        // FEN spec; only the zobrist XOR is suppressed. NOT Stockfish-
        // verifiable on `after`: Stockfish strips the `e3` field. The
        // round-trip test asserts equality against our parser's reading
        // of this expected FEN, which is consistent with our spec stance.
        Case {
            before: "4k3/8/8/8/8/8/4P3/4K3 w - - 0 1",
            mv: Move::new(Square::E2, Square::E4, MoveFlag::DoublePush),
            after: "4k3/8/8/8/4P3/8/8/4K3 b - e3 0 1",
            label: "double_push_no_capturer",
        },
        // White double-push with a black pawn ON RANK 4 adjacent — capturer
        // exists, EP key IS XORed.
        Case {
            before: "4k3/8/8/8/3p4/8/4P3/4K3 w - - 0 1",
            mv: Move::new(Square::E2, Square::E4, MoveFlag::DoublePush),
            after: "4k3/8/8/8/3pP3/8/8/4K3 b - e3 0 1",
            label: "double_push_with_capturer",
        },
        // ---------- Captures ----------
        Case {
            before: "rnbqkbnr/pppp1ppp/8/4p3/8/5N2/PPPPPPPP/RNBQKB1R w KQkq - 0 2",
            mv: Move::capture(Square::F3, Square::E5),
            after: "rnbqkbnr/pppp1ppp/8/4N3/8/8/PPPPPPPP/RNBQKB1R b KQkq - 0 2",
            label: "capture_basic_knight_takes_pawn",
        },
        // Kiwipete-style: black bishop on g2 captures white rook on h1 →
        // WHITE_KING castling lost. (Captor must be one diagonal step from
        // h1; using a bishop on g2 is the simplest legal setup.)
        Case {
            before: "4k3/8/8/8/8/8/6b1/4K2R b K - 0 1",
            mv: Move::capture(Square::G2, Square::H1),
            after: "4k3/8/8/8/8/8/8/4K2b w - - 0 2",
            label: "capture_clears_castling_on_to_h1",
        },
        // The canonical Kiwipete bxa1 line: black bishop on b2 captures
        // white rook on a1 → WHITE_QUEEN castling lost.
        Case {
            before: "4k3/8/8/8/8/8/1b6/R3K3 b Q - 0 1",
            mv: Move::capture(Square::B2, Square::A1),
            after: "4k3/8/8/8/8/8/8/b3K3 w - - 0 2",
            label: "capture_clears_castling_on_to_a1",
        },
        // White rook on a1 captures black rook on a8 → BLACK_QUEEN castling lost.
        // (The capturing piece must be a rook, not a bishop, to legally slide
        // along the a-file.)
        Case {
            before: "r3k3/8/8/8/8/8/8/R3K3 w q - 0 1",
            mv: Move::capture(Square::A1, Square::A8),
            after: "R3k3/8/8/8/8/8/8/4K3 b - - 0 1",
            label: "capture_clears_castling_on_to_a8",
        },
        // White rook on h1 captures black rook on h8 → BLACK_KING castling lost.
        Case {
            before: "4k2r/8/8/8/8/8/8/4K2R w k - 0 1",
            mv: Move::capture(Square::H1, Square::H8),
            after: "4k2R/8/8/8/8/8/8/4K3 b - - 0 1",
            label: "capture_clears_castling_on_to_h8",
        },
        // ---------- En passant ----------
        // White EP capture: position after `1.e4 d6 2.e5 d5` then white plays exd6 EP.
        Case {
            before: "4k3/8/8/3pP3/8/8/8/4K3 w - d6 0 1",
            mv: Move::new(Square::E5, Square::D6, MoveFlag::EnPassant),
            after: "4k3/8/3P4/8/8/8/8/4K3 b - - 0 1",
            label: "ep_white_captures_black",
        },
        // Black EP capture: white played e2-e4 with a black pawn on d4, black plays dxe3 EP.
        Case {
            before: "4k3/8/8/8/3pP3/8/8/4K3 b - e3 0 1",
            mv: Move::new(Square::D4, Square::E3, MoveFlag::EnPassant),
            after: "4k3/8/8/8/8/4p3/8/4K3 w - - 0 2",
            label: "ep_black_captures_white",
        },
        // ---------- Promotions (no capture) ----------
        // Black king on f8 (not e8) so the e7-e8 promotion path is unblocked.
        Case {
            before: "5k2/4P3/8/8/8/8/8/4K3 w - - 0 1",
            mv: Move::new(Square::E7, Square::E8, MoveFlag::QueenPromo),
            after: "4Qk2/8/8/8/8/8/8/4K3 b - - 0 1",
            label: "promo_white_queen",
        },
        Case {
            before: "5k2/4P3/8/8/8/8/8/4K3 w - - 0 1",
            mv: Move::new(Square::E7, Square::E8, MoveFlag::KnightPromo),
            after: "4Nk2/8/8/8/8/8/8/4K3 b - - 0 1",
            label: "promo_white_knight",
        },
        Case {
            before: "5k2/4P3/8/8/8/8/8/4K3 w - - 0 1",
            mv: Move::new(Square::E7, Square::E8, MoveFlag::BishopPromo),
            after: "4Bk2/8/8/8/8/8/8/4K3 b - - 0 1",
            label: "promo_white_bishop",
        },
        Case {
            before: "5k2/4P3/8/8/8/8/8/4K3 w - - 0 1",
            mv: Move::new(Square::E7, Square::E8, MoveFlag::RookPromo),
            after: "4Rk2/8/8/8/8/8/8/4K3 b - - 0 1",
            label: "promo_white_rook",
        },
        Case {
            before: "4k3/8/8/8/8/8/4p3/7K b - - 0 1",
            mv: Move::new(Square::E2, Square::E1, MoveFlag::QueenPromo),
            after: "4k3/8/8/8/8/8/8/4q2K w - - 0 2",
            label: "promo_black_queen",
        },
        // ---------- Promotion-captures ----------
        Case {
            before: "4kn2/4P3/8/8/8/8/8/4K3 w - - 0 1",
            mv: Move::new(Square::E7, Square::F8, MoveFlag::QueenPromoCapture),
            after: "4kQ2/8/8/8/8/8/8/4K3 b - - 0 1",
            label: "promo_capture_white_queen",
        },
        // Promo-capture onto a8 must clear BLACK_QUEEN castling right.
        Case {
            before: "r3k3/1P6/8/8/8/8/8/4K3 w q - 0 1",
            mv: Move::new(Square::B7, Square::A8, MoveFlag::QueenPromoCapture),
            after: "Q3k3/8/8/8/8/8/8/4K3 b - - 0 1",
            label: "promo_capture_clears_castling_on_to_a8",
        },
        // Mirror: black promo-capture onto h1 must clear WHITE_KING right.
        Case {
            before: "4k3/8/8/8/8/8/6p1/4K2R b K - 0 1",
            mv: Move::new(Square::G2, Square::H1, MoveFlag::QueenPromoCapture),
            after: "4k3/8/8/8/8/8/8/4K2q w - - 0 2",
            label: "promo_capture_clears_castling_on_to_h1",
        },
        // ---------- Castling ----------
        Case {
            before: "4k3/8/8/8/8/8/8/4K2R w K - 0 1",
            mv: Move::new(Square::E1, Square::G1, MoveFlag::KingCastle),
            after: "4k3/8/8/8/8/8/8/5RK1 b - - 1 1",
            label: "castle_white_kingside",
        },
        Case {
            before: "4k3/8/8/8/8/8/8/R3K3 w Q - 0 1",
            mv: Move::new(Square::E1, Square::C1, MoveFlag::QueenCastle),
            after: "4k3/8/8/8/8/8/8/2KR4 b - - 1 1",
            label: "castle_white_queenside",
        },
        Case {
            before: "4k2r/8/8/8/8/8/8/4K3 b k - 0 1",
            mv: Move::new(Square::E8, Square::G8, MoveFlag::KingCastle),
            after: "5rk1/8/8/8/8/8/8/4K3 w - - 1 2",
            label: "castle_black_kingside",
        },
        Case {
            before: "r3k3/8/8/8/8/8/8/4K3 b q - 0 1",
            mv: Move::new(Square::E8, Square::C8, MoveFlag::QueenCastle),
            after: "2kr4/8/8/8/8/8/8/4K3 w - - 1 2",
            label: "castle_black_queenside",
        },
    ];

    // Helper: find a case by label.
    fn case(label: &str) -> &'static Case {
        CASES
            .iter()
            .find(|c| c.label == label)
            .unwrap_or_else(|| panic!("no case named {label}"))
    }

    // -----------------------------------------------------------------------
    // Move-encoding tests (pass 5.A).
    // -----------------------------------------------------------------------

    #[test]
    fn move_new_round_trips_components() {
        // Cover every flag with a distinct (from, to) pair.
        let triples: &[(Square, Square, MoveFlag)] = &[
            (Square::E2, Square::E3, MoveFlag::Quiet),
            (Square::E2, Square::E4, MoveFlag::DoublePush),
            (Square::E1, Square::G1, MoveFlag::KingCastle),
            (Square::E1, Square::C1, MoveFlag::QueenCastle),
            (Square::E4, Square::F5, MoveFlag::Capture),
            (Square::E5, Square::D6, MoveFlag::EnPassant),
            (Square::E7, Square::E8, MoveFlag::KnightPromo),
            (Square::E7, Square::E8, MoveFlag::BishopPromo),
            (Square::E7, Square::E8, MoveFlag::RookPromo),
            (Square::E7, Square::E8, MoveFlag::QueenPromo),
            (Square::E7, Square::F8, MoveFlag::KnightPromoCapture),
            (Square::E7, Square::F8, MoveFlag::BishopPromoCapture),
            (Square::E7, Square::F8, MoveFlag::RookPromoCapture),
            (Square::E7, Square::F8, MoveFlag::QueenPromoCapture),
        ];
        for &(from, to, flag) in triples {
            let mv = Move::new(from, to, flag);
            assert_eq!(mv.from_square(), from, "from for {flag:?}");
            assert_eq!(mv.to_square(), to, "to for {flag:?}");
            assert_eq!(mv.flag(), flag, "flag");
        }
    }

    #[test]
    fn move_layout_bits() {
        // QueenPromoCapture (15 = 0xF) on A1->H8: flag<<12 | to<<6 | from
        // = 0xF000 | (63 << 6) | 0 = 0xF000 | 0x0FC0 | 0 = 0xFFC0.
        assert_eq!(
            Move::new(Square::A1, Square::H8, MoveFlag::QueenPromoCapture).bits(),
            0xFFC0
        );
        // Quiet A1->A1: all zero.
        assert_eq!(Move::new(Square::A1, Square::A1, MoveFlag::Quiet).bits(), 0);
        // Distinct (from, to, flag) → distinct bits.
        let m1 = Move::quiet(Square::E2, Square::E3);
        let m2 = Move::quiet(Square::E2, Square::E4);
        assert_ne!(m1.bits(), m2.bits());

        // Asymmetric (from, to, flag) — pins the bit-shift directions
        // (catches a swap of the from-shift and to-shift positions).
        // B1 = 1 (file b, rank 1), C2 = 10 (file c, rank 2), Capture flag = 4.
        // Expected: (4 << 12) | (10 << 6) | 1 = 0x4000 | 0x280 | 1 = 0x4281.
        assert_eq!(
            Move::new(Square::B1, Square::C2, MoveFlag::Capture).bits(),
            0x4281,
            "from in bits 0-5; to in bits 6-11; flag in bits 12-15"
        );
    }

    #[test]
    fn move_quiet_helper() {
        assert_eq!(
            Move::quiet(Square::E2, Square::E3),
            Move::new(Square::E2, Square::E3, MoveFlag::Quiet)
        );
    }

    #[test]
    fn move_capture_helper() {
        assert_eq!(
            Move::capture(Square::E4, Square::F5),
            Move::new(Square::E4, Square::F5, MoveFlag::Capture)
        );
    }

    #[test]
    fn move_is_capture_table() {
        // Every flag → expected is_capture boolean.
        let cases: &[(MoveFlag, bool)] = &[
            (MoveFlag::Quiet, false),
            (MoveFlag::DoublePush, false),
            (MoveFlag::KingCastle, false),
            (MoveFlag::QueenCastle, false),
            (MoveFlag::Capture, true),
            (MoveFlag::EnPassant, true),
            (MoveFlag::KnightPromo, false),
            (MoveFlag::BishopPromo, false),
            (MoveFlag::RookPromo, false),
            (MoveFlag::QueenPromo, false),
            (MoveFlag::KnightPromoCapture, true),
            (MoveFlag::BishopPromoCapture, true),
            (MoveFlag::RookPromoCapture, true),
            (MoveFlag::QueenPromoCapture, true),
        ];
        for &(flag, expected) in cases {
            let mv = Move::new(Square::A1, Square::A2, flag);
            assert_eq!(mv.is_capture(), expected, "is_capture for {flag:?}");
        }
    }

    #[test]
    fn move_is_promotion_table() {
        let promo_flags = [
            MoveFlag::KnightPromo,
            MoveFlag::BishopPromo,
            MoveFlag::RookPromo,
            MoveFlag::QueenPromo,
            MoveFlag::KnightPromoCapture,
            MoveFlag::BishopPromoCapture,
            MoveFlag::RookPromoCapture,
            MoveFlag::QueenPromoCapture,
        ];
        let non_promo_flags = [
            MoveFlag::Quiet,
            MoveFlag::DoublePush,
            MoveFlag::KingCastle,
            MoveFlag::QueenCastle,
            MoveFlag::Capture,
            MoveFlag::EnPassant,
        ];
        for flag in promo_flags {
            assert!(
                Move::new(Square::A1, Square::A2, flag).is_promotion(),
                "{flag:?} should be promotion"
            );
        }
        for flag in non_promo_flags {
            assert!(
                !Move::new(Square::A1, Square::A2, flag).is_promotion(),
                "{flag:?} should not be promotion"
            );
        }
    }

    #[test]
    fn move_is_castling_table() {
        for flag in [MoveFlag::KingCastle, MoveFlag::QueenCastle] {
            assert!(Move::new(Square::E1, Square::G1, flag).is_castling());
        }
        let non_castle = [
            MoveFlag::Quiet,
            MoveFlag::DoublePush,
            MoveFlag::Capture,
            MoveFlag::EnPassant,
            MoveFlag::KnightPromo,
            MoveFlag::QueenPromoCapture,
        ];
        for flag in non_castle {
            assert!(!Move::new(Square::E1, Square::G1, flag).is_castling());
        }
    }

    #[test]
    fn move_promotion_kind_table() {
        let mapping: &[(MoveFlag, Option<PieceKind>)] = &[
            (MoveFlag::Quiet, None),
            (MoveFlag::DoublePush, None),
            (MoveFlag::KingCastle, None),
            (MoveFlag::QueenCastle, None),
            (MoveFlag::Capture, None),
            (MoveFlag::EnPassant, None),
            (MoveFlag::KnightPromo, Some(PieceKind::Knight)),
            (MoveFlag::BishopPromo, Some(PieceKind::Bishop)),
            (MoveFlag::RookPromo, Some(PieceKind::Rook)),
            (MoveFlag::QueenPromo, Some(PieceKind::Queen)),
            (MoveFlag::KnightPromoCapture, Some(PieceKind::Knight)),
            (MoveFlag::BishopPromoCapture, Some(PieceKind::Bishop)),
            (MoveFlag::RookPromoCapture, Some(PieceKind::Rook)),
            (MoveFlag::QueenPromoCapture, Some(PieceKind::Queen)),
        ];
        for &(flag, expected) in mapping {
            let mv = Move::new(Square::A1, Square::A2, flag);
            assert_eq!(mv.promotion_kind(), expected, "for {flag:?}");
        }
    }

    #[test]
    fn move_display_long_algebraic() {
        assert_eq!(Move::quiet(Square::E2, Square::E4).to_string(), "e2e4");
        assert_eq!(Move::capture(Square::E4, Square::F5).to_string(), "e4f5");
        // Castle: long algebraic encodes king move squares.
        assert_eq!(
            Move::new(Square::E1, Square::G1, MoveFlag::KingCastle).to_string(),
            "e1g1"
        );
        // Promotion: lowercase suffix.
        assert_eq!(
            Move::new(Square::E7, Square::E8, MoveFlag::QueenPromo).to_string(),
            "e7e8q"
        );
        assert_eq!(
            Move::new(Square::E7, Square::E8, MoveFlag::KnightPromo).to_string(),
            "e7e8n"
        );
        assert_eq!(
            Move::new(Square::E7, Square::E8, MoveFlag::BishopPromo).to_string(),
            "e7e8b"
        );
        assert_eq!(
            Move::new(Square::E7, Square::E8, MoveFlag::RookPromo).to_string(),
            "e7e8r"
        );
        // Promotion-capture: same shape.
        assert_eq!(
            Move::new(Square::E7, Square::F8, MoveFlag::QueenPromoCapture).to_string(),
            "e7f8q"
        );
    }

    #[test]
    fn move_display_promotion_letter_lowercase() {
        // UCI conformance: promotion suffixes must be ASCII lowercase.
        let promo_flags = [
            (MoveFlag::KnightPromo, 'n'),
            (MoveFlag::BishopPromo, 'b'),
            (MoveFlag::RookPromo, 'r'),
            (MoveFlag::QueenPromo, 'q'),
            (MoveFlag::KnightPromoCapture, 'n'),
            (MoveFlag::BishopPromoCapture, 'b'),
            (MoveFlag::RookPromoCapture, 'r'),
            (MoveFlag::QueenPromoCapture, 'q'),
        ];
        for (flag, expected_letter) in promo_flags {
            let mv = Move::new(Square::E7, Square::E8, flag);
            let s = mv.to_string();
            assert!(
                s.chars().all(|c| !c.is_ascii_uppercase()),
                "Display must not emit uppercase: {s} for {flag:?}",
            );
            assert!(
                s.ends_with(expected_letter),
                "Display for {flag:?} must end with {expected_letter}, got {s}",
            );
        }
    }

    #[test]
    fn move_debug_format() {
        let s = format!("{:?}", Move::quiet(Square::E2, Square::E4));
        assert!(s.contains("e2"), "Debug should include from-square: {s}");
        assert!(s.contains("e4"), "Debug should include to-square: {s}");
        assert!(s.contains("Quiet"), "Debug should include flag name: {s}");

        let s2 = format!(
            "{:?}",
            Move::new(Square::E7, Square::F8, MoveFlag::QueenPromoCapture)
        );
        assert!(
            s2.contains("QueenPromoCapture"),
            "Debug includes flag: {s2}"
        );
    }

    #[test]
    fn move_flag_unused_codes_absent() {
        // from_bits returns None for codes 6 and 7; valid codes 0..=5 and 8..=15
        // round-trip.
        assert_eq!(MoveFlag::from_bits(6), None);
        assert_eq!(MoveFlag::from_bits(7), None);
        assert_eq!(MoveFlag::from_bits(0), Some(MoveFlag::Quiet));
        assert_eq!(MoveFlag::from_bits(15), Some(MoveFlag::QueenPromoCapture));
        // Out-of-range values (>=16) also yield None — we only call from_bits
        // with the bottom nibble, so this is purely defensive.
        assert_eq!(MoveFlag::from_bits(16), None);
    }

    // -----------------------------------------------------------------------
    // Curated unit tests for make/unmake — pass 5.B (non-special) + 5.C (special).
    //
    // Each test parses the `before` FEN, applies the move, asserts the
    // post-make Position equals the parsed `after` FEN, then unmakes and
    // asserts byte-equality with the original. Position derives PartialEq
    // over all fields including zobrist, so the post-unmake comparison is
    // a full round-trip check including Zobrist.
    //
    // Per-field diagnostic asserts are added on the first quiet test as a
    // diagnostic aid for aux-state-only failures (per the plan §"Test
    // coverage" → "Quiet moves" row).
    // -----------------------------------------------------------------------

    /// Generic case driver. Parses before/after; applies make; checks
    /// post-make equality; unmakes; checks byte-equality with the original.
    fn run_case(c: &Case) {
        let original = Position::from_fen(c.before).expect("before FEN must parse");
        let expected_after = Position::from_fen(c.after).expect("after FEN must parse");

        let mut pos = original;
        let undo = make_move(&mut pos, c.mv);

        assert_eq!(
            pos,
            expected_after,
            "post-make differs from expected for {label}",
            label = c.label,
        );

        unmake_move(&mut pos, c.mv, undo);
        assert_eq!(
            pos,
            original,
            "post-unmake differs from original for {label}",
            label = c.label,
        );
    }

    #[test]
    fn quiet_pawn_single_push_white() {
        run_case(case("quiet_pawn_single_push_white"));
    }

    #[test]
    fn quiet_pawn_single_push_black() {
        run_case(case("quiet_pawn_single_push_black"));
    }

    #[test]
    fn quiet_knight_move() {
        run_case(case("quiet_knight_move"));
    }

    #[test]
    fn quiet_white_king_clears_castling() {
        run_case(case("quiet_white_king_clears_castling"));
    }

    #[test]
    fn quiet_a1_rook_clears_white_queen_right() {
        run_case(case("quiet_a1_rook_clears_white_queen"));
    }

    #[test]
    fn quiet_h1_rook_clears_white_king_right() {
        run_case(case("quiet_h1_rook_clears_white_king"));
    }

    #[test]
    fn quiet_black_king_clears_castling() {
        run_case(case("quiet_black_king_clears_castling"));
    }

    #[test]
    fn quiet_a8_rook_clears_black_queen_right() {
        run_case(case("quiet_a8_rook_clears_black_queen"));
    }

    #[test]
    fn quiet_h8_rook_clears_black_king_right() {
        run_case(case("quiet_h8_rook_clears_black_king"));
    }

    /// Per-field diagnostic aid (plan §"Test coverage" → quiet row).
    /// In addition to the structural round-trip, explicitly compare the
    /// aux-state fields so an aux-only regression produces a one-line
    /// diagnostic without needing to compare debug grids.
    #[test]
    fn quiet_round_trip_per_field_diagnostics() {
        let c = case("quiet_pawn_single_push_white");
        let original = Position::from_fen(c.before).expect("before FEN parses");
        let mut pos = original;

        let undo = make_move(&mut pos, c.mv);
        unmake_move(&mut pos, c.mv, undo);

        assert_eq!(pos, original, "structural round-trip");
        // Diagnostic aids:
        assert_eq!(pos.fullmove_number(), original.fullmove_number());
        assert_eq!(pos.halfmove_clock(), original.halfmove_clock());
        assert_eq!(pos.castling_rights(), original.castling_rights());
        assert_eq!(pos.ep_target(), original.ep_target());
        assert_eq!(pos.side_to_move(), original.side_to_move());
        assert_eq!(pos.zobrist(), original.zobrist());
    }

    #[test]
    fn double_push_white_sets_ep_e3() {
        run_case(case("double_push_white_sets_ep_e3"));
        // Also: post-make EP target is e3.
        let c = case("double_push_white_sets_ep_e3");
        let mut pos = Position::from_fen(c.before).unwrap();
        let _undo = make_move(&mut pos, c.mv);
        assert_eq!(pos.ep_target(), Some(Square::E3));
        assert_eq!(pos.halfmove_clock(), 0);
    }

    #[test]
    fn double_push_black_sets_ep_e6() {
        run_case(case("double_push_black_sets_ep_e6"));
        let c = case("double_push_black_sets_ep_e6");
        let mut pos = Position::from_fen(c.before).unwrap();
        let _undo = make_move(&mut pos, c.mv);
        assert_eq!(pos.ep_target(), Some(Square::E6));
    }

    #[test]
    fn double_push_zobrist_no_ep_capturer() {
        // With no opponent capturer, the post-make zobrist must NOT have the
        // EP-file key XORed in. The structural round-trip in run_case checks
        // this implicitly (zobrist is part of Position equality), but an
        // explicit assertion against from_scratch documents the invariant.
        let c = case("double_push_no_capturer");
        let mut pos = Position::from_fen(c.before).unwrap();
        let _undo = make_move(&mut pos, c.mv);
        assert_eq!(pos.zobrist(), crate::zobrist::from_scratch(&pos));
        // ep_file_to_hash should return None on this position.
        assert_eq!(crate::zobrist::ep_file_to_hash(&pos), None);
    }

    #[test]
    fn double_push_zobrist_with_ep_capturer() {
        let c = case("double_push_with_capturer");
        let mut pos = Position::from_fen(c.before).unwrap();
        let _undo = make_move(&mut pos, c.mv);
        assert_eq!(pos.zobrist(), crate::zobrist::from_scratch(&pos));
        // The black pawn on d4 IS adjacent to the new EP target e3.
        assert_eq!(crate::zobrist::ep_file_to_hash(&pos), Some(4));
    }

    #[test]
    fn capture_basic() {
        run_case(case("capture_basic_knight_takes_pawn"));
        // Diagnostic: Undo carries the captured pawn.
        let c = case("capture_basic_knight_takes_pawn");
        let mut pos = Position::from_fen(c.before).unwrap();
        let undo = make_move(&mut pos, c.mv);
        assert_eq!(
            undo.captured,
            Some(Piece::new(Color::Black, PieceKind::Pawn))
        );
        assert_eq!(pos.halfmove_clock(), 0, "capture resets halfmove");
    }

    #[test]
    fn capture_clears_castling_on_to_h1() {
        run_case(case("capture_clears_castling_on_to_h1"));
    }

    #[test]
    fn capture_clears_castling_on_to_a1() {
        // The canonical Kiwipete depth-4 perft-bug exposure point.
        run_case(case("capture_clears_castling_on_to_a1"));
    }

    #[test]
    fn capture_clears_castling_on_to_a8() {
        run_case(case("capture_clears_castling_on_to_a8"));
    }

    #[test]
    fn capture_clears_castling_on_to_h8() {
        run_case(case("capture_clears_castling_on_to_h8"));
    }

    #[test]
    fn ep_white_captures_black() {
        run_case(case("ep_white_captures_black"));
        // Specific assertion: the captured pawn was on d5, NOT on d6 (the EP target).
        let c = case("ep_white_captures_black");
        let mut pos = Position::from_fen(c.before).unwrap();
        let undo = make_move(&mut pos, c.mv);
        assert_eq!(
            undo.captured,
            Some(Piece::new(Color::Black, PieceKind::Pawn))
        );
        assert_eq!(
            pos.piece_at(Square::D5),
            None,
            "captured pawn removed from d5"
        );
        assert_eq!(
            pos.piece_at(Square::D6),
            Some(Piece::new(Color::White, PieceKind::Pawn)),
            "white pawn on d6 (the EP-capture destination)"
        );
        assert_eq!(pos.ep_target(), None, "EP target cleared after EP capture");
    }

    #[test]
    fn ep_black_captures_white() {
        run_case(case("ep_black_captures_white"));
        let c = case("ep_black_captures_white");
        let mut pos = Position::from_fen(c.before).unwrap();
        let undo = make_move(&mut pos, c.mv);
        assert_eq!(
            undo.captured,
            Some(Piece::new(Color::White, PieceKind::Pawn))
        );
        assert_eq!(
            pos.piece_at(Square::E4),
            None,
            "captured pawn removed from e4"
        );
        assert_eq!(
            pos.piece_at(Square::E3),
            Some(Piece::new(Color::Black, PieceKind::Pawn))
        );
    }

    /// Pin the **side-to-move asymmetry** of the Polyglot turn key
    /// (ADR-0009 §4: "RandomTurn[780] is XORed iff White is to move —
    /// asymmetric — Black-to-move contributes 0"). A symmetric "XOR turn
    /// key on every flip" implementation would pass round-trip and
    /// `from_scratch` consistency checks (because `from_scratch` follows
    /// the same convention) but break Polyglot interop.
    ///
    /// Strategy: take a position with white-to-move, no EP, no castling
    /// changes (so the only delta is the moving piece + turn key). Compute
    /// the expected zobrist delta manually, including XORing OUT the turn
    /// key (since we're flipping from white-to-move to black-to-move).
    /// Assert the actual delta matches.
    #[test]
    fn turn_key_asymmetry_pinned_by_make() {
        // Two kings + a single white knight, no castling, no EP, white to
        // move. After Nb1-c3 (a quiet knight move, no capture, no special
        // case), side flips to black, no EP target, no castling change.
        // The only zobrist deltas are: piece keys for the knight on b1/c3,
        // and the turn key (XORed OUT since we're transitioning from
        // white-to-move to black-to-move).
        let mut pos = Position::from_fen("4k3/8/8/8/8/8/8/1N2K3 w - - 0 1").unwrap();
        let prior = pos.zobrist();
        let knight = Piece::new(Color::White, PieceKind::Knight);
        let mv = Move::quiet(Square::B1, Square::C3);

        let _undo = make_move(&mut pos, mv);

        let new = pos.zobrist();
        let expected_new = prior
            ^ crate::zobrist::piece_key(knight, Square::B1)
            ^ crate::zobrist::piece_key(knight, Square::C3)
            ^ crate::zobrist::turn_key();
        assert_eq!(
            new, expected_new,
            "incremental zobrist did not XOR turn_key on white→black transition",
        );

        // Symmetric direction: same construction but black-to-move,
        // black knight moves from a natural starting square. The
        // transition is black-to-move → white-to-move; turn_key is XORed.
        // (XOR is its own inverse, so XORing turn_key is correct in both
        // directions — the asymmetry pinned here is that the increment
        // XORs it exactly once per move, regardless of direction.)
        let mut pos = Position::from_fen("1n2k3/8/8/8/8/8/8/4K3 b - - 0 1").unwrap();
        let prior = pos.zobrist();
        let knight = Piece::new(Color::Black, PieceKind::Knight);
        let mv = Move::quiet(Square::B8, Square::C6);

        let _undo = make_move(&mut pos, mv);

        let new = pos.zobrist();
        let expected_new = prior
            ^ crate::zobrist::piece_key(knight, Square::B8)
            ^ crate::zobrist::piece_key(knight, Square::C6)
            ^ crate::zobrist::turn_key();
        assert_eq!(
            new, expected_new,
            "incremental zobrist did not XOR turn_key on black→white transition",
        );
    }

    #[test]
    fn ep_zobrist_consistency() {
        for label in ["ep_white_captures_black", "ep_black_captures_white"] {
            let c = case(label);
            let mut pos = Position::from_fen(c.before).unwrap();
            let _undo = make_move(&mut pos, c.mv);
            assert_eq!(
                pos.zobrist(),
                crate::zobrist::from_scratch(&pos),
                "post-EP zobrist disagrees with from_scratch for {label}",
            );
        }
    }

    #[test]
    fn promo_white_queen() {
        run_case(case("promo_white_queen"));
    }

    #[test]
    fn promo_white_knight() {
        run_case(case("promo_white_knight"));
    }

    #[test]
    fn promo_white_bishop() {
        run_case(case("promo_white_bishop"));
    }

    #[test]
    fn promo_white_rook() {
        run_case(case("promo_white_rook"));
    }

    #[test]
    fn promo_black_queen() {
        run_case(case("promo_black_queen"));
    }

    #[test]
    fn promo_capture_white_queen() {
        run_case(case("promo_capture_white_queen"));
        let c = case("promo_capture_white_queen");
        let mut pos = Position::from_fen(c.before).unwrap();
        let undo = make_move(&mut pos, c.mv);
        assert_eq!(
            undo.captured,
            Some(Piece::new(Color::Black, PieceKind::Knight))
        );
    }

    #[test]
    fn promo_capture_clears_castling_on_to_a8() {
        run_case(case("promo_capture_clears_castling_on_to_a8"));
    }

    #[test]
    fn promo_capture_clears_castling_on_to_h1() {
        run_case(case("promo_capture_clears_castling_on_to_h1"));
    }

    #[test]
    fn castle_white_kingside() {
        run_case(case("castle_white_kingside"));
        let c = case("castle_white_kingside");
        let mut pos = Position::from_fen(c.before).unwrap();
        let _undo = make_move(&mut pos, c.mv);
        // King on g1, rook on f1; both white castling rights cleared.
        assert_eq!(
            pos.piece_at(Square::G1),
            Some(Piece::new(Color::White, PieceKind::King))
        );
        assert_eq!(
            pos.piece_at(Square::F1),
            Some(Piece::new(Color::White, PieceKind::Rook))
        );
        assert!(!pos.castling_rights().has(CastlingRights::WHITE_KING));
        assert!(!pos.castling_rights().has(CastlingRights::WHITE_QUEEN));
        assert_eq!(pos.halfmove_clock(), 1, "king move != pawn move != capture");
    }

    #[test]
    fn castle_white_queenside() {
        run_case(case("castle_white_queenside"));
        let c = case("castle_white_queenside");
        let mut pos = Position::from_fen(c.before).unwrap();
        let _undo = make_move(&mut pos, c.mv);
        assert_eq!(
            pos.piece_at(Square::C1),
            Some(Piece::new(Color::White, PieceKind::King))
        );
        assert_eq!(
            pos.piece_at(Square::D1),
            Some(Piece::new(Color::White, PieceKind::Rook))
        );
    }

    #[test]
    fn castle_black_kingside() {
        run_case(case("castle_black_kingside"));
        let c = case("castle_black_kingside");
        let mut pos = Position::from_fen(c.before).unwrap();
        let _undo = make_move(&mut pos, c.mv);
        assert_eq!(
            pos.piece_at(Square::G8),
            Some(Piece::new(Color::Black, PieceKind::King))
        );
        assert_eq!(
            pos.piece_at(Square::F8),
            Some(Piece::new(Color::Black, PieceKind::Rook))
        );
    }

    #[test]
    fn castle_black_queenside() {
        run_case(case("castle_black_queenside"));
        let c = case("castle_black_queenside");
        let mut pos = Position::from_fen(c.before).unwrap();
        let _undo = make_move(&mut pos, c.mv);
        assert_eq!(
            pos.piece_at(Square::C8),
            Some(Piece::new(Color::Black, PieceKind::King))
        );
        assert_eq!(
            pos.piece_at(Square::D8),
            Some(Piece::new(Color::Black, PieceKind::Rook))
        );
    }

    // -----------------------------------------------------------------------
    // Property tests (pass 5.D).
    //
    // Strategy: pick a curated case from CASES, apply make then unmake,
    // assert byte-equality with the original. Plus zobrist == from_scratch
    // assertions on the post-make state. Plus a 2-ply round-trip across
    // curated multi-ply transitions (especially cross-ply EP).
    // -----------------------------------------------------------------------

    use proptest::prelude::*;

    fn arb_case_index() -> impl Strategy<Value = usize> {
        0..CASES.len()
    }

    /// Curated 2-ply sequences exercising cross-ply EP transitions and other
    /// chained-state cases. Each entry is (FEN, mv1, mv2, label).
    /// Verified by hand; second move must be legal in the position after
    /// the first move.
    struct TwoPlyCase {
        before: &'static str,
        mv1: Move,
        mv2: Move,
        label: &'static str,
    }

    const TWO_PLY_CASES: &[TwoPlyCase] = &[
        // (a) Some→Some on a different file: white double-push e4 (EP target e3),
        //     then black double-push d5 (EP target d6).
        TwoPlyCase {
            before: "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
            mv1: Move::new(Square::E2, Square::E4, MoveFlag::DoublePush),
            mv2: Move::new(Square::D7, Square::D5, MoveFlag::DoublePush),
            label: "ep_some_to_some_different_file",
        },
        // (b) Some→None: white double-push e4 (EP e3), then black plays a quiet move.
        TwoPlyCase {
            before: "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
            mv1: Move::new(Square::E2, Square::E4, MoveFlag::DoublePush),
            mv2: Move::quiet(Square::G8, Square::F6),
            label: "ep_some_to_none_via_quiet",
        },
        // (c) Some→None via EP capture: black double-push d5, white plays exd6 EP.
        TwoPlyCase {
            before: "4k3/3p4/8/4P3/8/8/8/4K3 b - - 0 1",
            mv1: Move::new(Square::D7, Square::D5, MoveFlag::DoublePush),
            mv2: Move::new(Square::E5, Square::D6, MoveFlag::EnPassant),
            label: "ep_some_consumed_by_ep_capture",
        },
        // Double-push followed by quiet: ensures EP is cleared.
        TwoPlyCase {
            before: "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
            mv1: Move::new(Square::E2, Square::E4, MoveFlag::DoublePush),
            mv2: Move::quiet(Square::E7, Square::E6),
            label: "ep_then_single_pawn_push",
        },
        // (d) Prior-EP-target-set-but-no-capturer cross-ply transition.
        //     Ply 1: white double-push e2-e4 with NO black pawn anywhere
        //     adjacent → ep_target = Some(e3), but pseudo-legal predicate
        //     yields None (no capturer), so the EP-file key was NOT XORed
        //     into the zobrist. Ply 2: black quiet king move. The
        //     incremental update for ply 2 must use ep_file_to_hash(pos)
        //     (= None) for prior_ep_file, NOT ep_target.is_some() (which
        //     would falsely yield Some(e_file) and produce a spurious XOR
        //     out of an EP key that was never XORed in). A buggy
        //     implementation using ep_target.is_some() instead of the
        //     pseudo-legal predicate would corrupt the zobrist by
        //     ep_file_key(4) after ply 2; this case catches that.
        TwoPlyCase {
            before: "4k3/8/8/8/8/8/4P3/4K3 w - - 0 1",
            mv1: Move::new(Square::E2, Square::E4, MoveFlag::DoublePush),
            mv2: Move::quiet(Square::E8, Square::D8),
            label: "ep_some_no_capturer_then_quiet",
        },
    ];

    fn arb_two_ply_index() -> impl Strategy<Value = usize> {
        0..TWO_PLY_CASES.len()
    }

    proptest! {
        /// Round-trip: for every curated case, make then unmake yields
        /// byte-equality with the original.
        #[test]
        fn prop_make_unmake_round_trips_curated(idx in arb_case_index()) {
            let c = &CASES[idx];
            let original = Position::from_fen(c.before).unwrap();
            let mut pos = original;
            let undo = make_move(&mut pos, c.mv);
            unmake_move(&mut pos, c.mv, undo);
            prop_assert_eq!(pos, original, "round-trip failed for {}", c.label);
        }

        /// After make_move, the incremental zobrist must equal the from-scratch
        /// computation. Runs in release too — the debug-build assert in
        /// make_move runs only in debug.
        #[test]
        fn prop_make_then_zobrist_matches_from_scratch(idx in arb_case_index()) {
            let c = &CASES[idx];
            let mut pos = Position::from_fen(c.before).unwrap();
            let _undo = make_move(&mut pos, c.mv);
            prop_assert_eq!(
                pos.zobrist(),
                crate::zobrist::from_scratch(&pos),
                "zobrist diverged after make for {}", c.label,
            );
        }

        /// Symmetric: after unmake, zobrist must agree with from-scratch.
        /// Subsumed by the round-trip assertion (Position equality includes
        /// zobrist) but stating it explicitly aids diagnosis.
        #[test]
        fn prop_unmake_then_zobrist_matches_from_scratch(idx in arb_case_index()) {
            let c = &CASES[idx];
            let mut pos = Position::from_fen(c.before).unwrap();
            let undo = make_move(&mut pos, c.mv);
            unmake_move(&mut pos, c.mv, undo);
            prop_assert_eq!(
                pos.zobrist(),
                crate::zobrist::from_scratch(&pos),
                "zobrist diverged after unmake for {}", c.label,
            );
        }

        /// Two-ply round-trip: apply mv1, then mv2; unmake mv2, then mv1.
        /// Byte-equality with the starting position. Catches cross-ply
        /// state-chaining bugs (especially EP transitions).
        #[test]
        fn prop_two_move_round_trip(idx in arb_two_ply_index()) {
            let c = &TWO_PLY_CASES[idx];
            let original = Position::from_fen(c.before).unwrap();
            let mut pos = original;
            let undo1 = make_move(&mut pos, c.mv1);
            // Zobrist consistency after ply 1.
            prop_assert_eq!(
                pos.zobrist(),
                crate::zobrist::from_scratch(&pos),
                "zobrist diverged after ply 1 for {}", c.label,
            );
            let undo2 = make_move(&mut pos, c.mv2);
            // Zobrist consistency after ply 2.
            prop_assert_eq!(
                pos.zobrist(),
                crate::zobrist::from_scratch(&pos),
                "zobrist diverged after ply 2 for {}", c.label,
            );
            unmake_move(&mut pos, c.mv2, undo2);
            unmake_move(&mut pos, c.mv1, undo1);
            prop_assert_eq!(pos, original, "two-ply round-trip failed for {}", c.label);
        }

        /// Capture moves carry a captured piece in Undo.
        #[test]
        fn prop_capture_undo_captured_some(idx in arb_case_index()) {
            let c = &CASES[idx];
            // Filter to capture cases.
            prop_assume!(c.mv.is_capture());
            let mut pos = Position::from_fen(c.before).unwrap();
            let prior_stm = pos.side_to_move();
            let undo = make_move(&mut pos, c.mv);
            let captured = undo.captured.expect("capture move must have captured piece");
            prop_assert_eq!(captured.color, prior_stm.flip(), "captured piece must be opponent's");
            prop_assert_ne!(captured.kind, PieceKind::King, "kings cannot be captured");
        }

        /// Non-capture moves leave Undo.captured = None.
        #[test]
        fn prop_quiet_move_undo_captured_none(idx in arb_case_index()) {
            let c = &CASES[idx];
            prop_assume!(!c.mv.is_capture());
            let mut pos = Position::from_fen(c.before).unwrap();
            let undo = make_move(&mut pos, c.mv);
            prop_assert_eq!(undo.captured, None, "non-capture must have None captured for {}", c.label);
        }
    }

    // -----------------------------------------------------------------------
    // Throughput sanity benches (ignored by default).
    // -----------------------------------------------------------------------

    #[test]
    #[ignore]
    fn bench_make_unmake_throughput() {
        use std::hint::black_box;
        use std::time::Instant;

        // One representative case from each major branch: quiet pawn,
        // capture, EP, castle, promo. Pre-parse positions outside the loop.
        let test_cases = [
            "quiet_pawn_single_push_white",
            "capture_basic_knight_takes_pawn",
            "ep_white_captures_black",
            "castle_white_kingside",
            "promo_white_queen",
        ];

        const ITERS: u32 = 1_000_000;

        for label in test_cases {
            let c = case(label);
            let pos0 = Position::from_fen(c.before).unwrap();

            let t0 = Instant::now();
            let mut pos = pos0;
            for _ in 0..ITERS {
                let undo = make_move(&mut pos, black_box(c.mv));
                unmake_move(&mut pos, black_box(c.mv), undo);
            }
            let elapsed = t0.elapsed();
            let ns_per = elapsed.as_nanos() as f64 / ITERS as f64;
            println!(
                "make/unmake [{label}] over {ITERS} iters: {elapsed:?} ({ns_per:.1} ns/cycle, {:.1} M cycles/sec)",
                1000.0 / ns_per,
            );
            assert_eq!(pos, pos0, "round-trip should leave position unchanged");
        }
    }

    /// Always-on release-build perf sentinel. Compiled out in debug builds
    /// (where the round-trip assert against `from_scratch` adds 50–100ns
    /// per call and would skew the threshold). In release, runs 1M
    /// (make, unmake) cycles on a quiet pawn push and asserts elapsed time
    /// is below a generous threshold. A reintroduced from-scratch call on
    /// the release path (~50 ns/op overhead = ~5–10× slowdown) would push
    /// past this threshold and fail.
    ///
    /// Run via `cargo test --release` (no filter). The plan's Verification
    /// step 5 invokes this.
    #[test]
    #[cfg(not(debug_assertions))]
    fn make_move_no_from_scratch_in_release() {
        use std::hint::black_box;
        use std::time::Instant;

        let c = case("quiet_pawn_single_push_white");
        let pos0 = Position::from_fen(c.before).unwrap();

        const ITERS: u32 = 1_000_000;
        let mut pos = pos0;
        let t0 = Instant::now();
        for _ in 0..ITERS {
            let undo = make_move(&mut pos, black_box(c.mv));
            unmake_move(&mut pos, black_box(c.mv), undo);
        }
        let elapsed = t0.elapsed();
        let ns_per = elapsed.as_nanos() as f64 / ITERS as f64;

        // Threshold: 100 ns/cycle. Targeted baseline is ~10–30 ns/cycle
        // (research §7 baselines: 5–20 ns search-time on Apple Silicon).
        // Even a single-sided from_scratch reintroduction on the make path
        // adds ~50 ns/op (per the M1.D throughput sanity number for
        // from_scratch ≈ 50.8 ns), bringing the cycle to ~60–80 ns —
        // detectable at this threshold but not at the looser 300 ns.
        // Threshold may need re-tuning post-step-10 benchmarks once the
        // empirical baseline is known on this hardware.
        const THRESHOLD_NS_PER_CYCLE: f64 = 100.0;
        assert!(
            ns_per < THRESHOLD_NS_PER_CYCLE,
            "make/unmake performance regression: {ns_per:.1} ns/cycle exceeds {THRESHOLD_NS_PER_CYCLE} threshold — possible accidental from_scratch on release path",
        );
        // Sanity: the loop did run.
        assert_eq!(pos, pos0);
    }

    // ===========================================================================
    // M2.A — UCI move encoding tests.
    //
    // Layout per docs/plans/m2.a.md §"Test coverage strategy":
    //   - to_uci anchor tests (one per flag/form)
    //   - from_uci positive anchor tests (one per disambiguation)
    //   - from_uci negative-parse table (one test, table-driven)
    //   - from_uci position-dependent rejection tests
    //   - round-trip on the curated CASES corpus
    //   - round-trip on every D1 legal move from the canonical 6
    //   - proptest: round-trip on D2-reachable positions
    //   - proptest: uniqueness of (from, to, promotion_kind) per position
    // ===========================================================================

    use crate::mov::UciMoveError;

    // -----------------------------------------------------------------------
    // to_uci anchor tests — one per flag/form.
    //
    // These intentionally duplicate the literals pinned by the existing
    // `move_display_*` tests above. Both sets survive: the duplication
    // pins `to_uci` independently of `Display`, so a future refactor
    // splitting them apart cannot break the wire format silently.
    // -----------------------------------------------------------------------

    #[test]
    fn to_uci_quiet_pawn() {
        assert_eq!(Move::quiet(Square::E2, Square::E3).to_uci(), "e2e3");
    }

    #[test]
    fn to_uci_double_push() {
        assert_eq!(
            Move::new(Square::E2, Square::E4, MoveFlag::DoublePush).to_uci(),
            "e2e4"
        );
    }

    #[test]
    fn to_uci_quiet_knight() {
        assert_eq!(Move::quiet(Square::G1, Square::F3).to_uci(), "g1f3");
    }

    #[test]
    fn to_uci_capture() {
        assert_eq!(Move::capture(Square::F3, Square::E5).to_uci(), "f3e5");
    }

    #[test]
    fn to_uci_en_passant() {
        assert_eq!(
            Move::new(Square::E5, Square::D6, MoveFlag::EnPassant).to_uci(),
            "e5d6"
        );
    }

    #[test]
    fn to_uci_kingside_castle_white() {
        assert_eq!(
            Move::new(Square::E1, Square::G1, MoveFlag::KingCastle).to_uci(),
            "e1g1"
        );
    }

    #[test]
    fn to_uci_queenside_castle_white() {
        assert_eq!(
            Move::new(Square::E1, Square::C1, MoveFlag::QueenCastle).to_uci(),
            "e1c1"
        );
    }

    #[test]
    fn to_uci_kingside_castle_black() {
        assert_eq!(
            Move::new(Square::E8, Square::G8, MoveFlag::KingCastle).to_uci(),
            "e8g8"
        );
    }

    #[test]
    fn to_uci_queenside_castle_black() {
        assert_eq!(
            Move::new(Square::E8, Square::C8, MoveFlag::QueenCastle).to_uci(),
            "e8c8"
        );
    }

    #[test]
    fn to_uci_promotion_each_kind() {
        let promos: &[(MoveFlag, &str)] = &[
            (MoveFlag::KnightPromo, "e7e8n"),
            (MoveFlag::BishopPromo, "e7e8b"),
            (MoveFlag::RookPromo, "e7e8r"),
            (MoveFlag::QueenPromo, "e7e8q"),
        ];
        for &(flag, expected) in promos {
            let mv = Move::new(Square::E7, Square::E8, flag);
            assert_eq!(mv.to_uci(), expected, "flag={flag:?}");
        }
    }

    #[test]
    fn to_uci_promotion_capture_each_kind() {
        let promos: &[(MoveFlag, &str)] = &[
            (MoveFlag::KnightPromoCapture, "e7f8n"),
            (MoveFlag::BishopPromoCapture, "e7f8b"),
            (MoveFlag::RookPromoCapture, "e7f8r"),
            (MoveFlag::QueenPromoCapture, "e7f8q"),
        ];
        for &(flag, expected) in promos {
            let mv = Move::new(Square::E7, Square::F8, flag);
            assert_eq!(mv.to_uci(), expected, "flag={flag:?}");
        }
    }

    // -----------------------------------------------------------------------
    // to_uci agrees with Display.
    //
    // Pins the structural relationship: to_uci's contract is "identical
    // bytes to Display::fmt." Today the implementation is literally
    // `self.to_string()`, so this assertion is a tautology — the value
    // is catching a future refactor that splits them apart silently.
    // -----------------------------------------------------------------------

    #[test]
    fn to_uci_matches_display_for_every_curated_case() {
        for c in CASES {
            assert_eq!(c.mv.to_uci(), c.mv.to_string(), "case {}", c.label);
        }
    }

    // -----------------------------------------------------------------------
    // from_uci positive anchor tests — one per disambiguation.
    //
    // Each test asserts both that parsing succeeds AND that the returned
    // Move's flag is the one we expect (because flag inference is the
    // whole point of from_uci needing &Position).
    // -----------------------------------------------------------------------

    #[test]
    fn from_uci_quiet_pawn_starting_position() {
        let pos = Position::starting_position();
        let mv = Move::from_uci("e2e3", &pos).expect("e2e3 is a legal quiet pawn push");
        assert_eq!(mv.from_square(), Square::E2);
        assert_eq!(mv.to_square(), Square::E3);
        assert_eq!(mv.flag(), MoveFlag::Quiet);
    }

    #[test]
    fn from_uci_double_push_recognized() {
        let pos = Position::starting_position();
        let mv = Move::from_uci("e2e4", &pos).expect("e2e4 is a legal double push");
        assert_eq!(mv.from_square(), Square::E2);
        assert_eq!(mv.to_square(), Square::E4);
        assert_eq!(mv.flag(), MoveFlag::DoublePush);
    }

    #[test]
    fn from_uci_capture_inferred() {
        // White knight on f3 captures black pawn on e5.
        let pos =
            Position::from_fen("rnbqkbnr/pppp1ppp/8/4p3/8/5N2/PPPPPPPP/RNBQKB1R w KQkq - 0 2")
                .unwrap();
        let mv = Move::from_uci("f3e5", &pos).expect("f3xe5 is a legal capture");
        assert_eq!(mv.from_square(), Square::F3);
        assert_eq!(mv.to_square(), Square::E5);
        assert_eq!(mv.flag(), MoveFlag::Capture);
    }

    #[test]
    fn from_uci_en_passant_inferred() {
        // White pawn on e5, black pawn on d5, EP target d6.
        let pos = Position::from_fen("4k3/8/8/3pP3/8/8/8/4K3 w - d6 0 1").unwrap();
        let mv = Move::from_uci("e5d6", &pos).expect("e5xd6 e.p. is legal");
        assert_eq!(mv.from_square(), Square::E5);
        assert_eq!(mv.to_square(), Square::D6);
        assert_eq!(mv.flag(), MoveFlag::EnPassant);
    }

    #[test]
    fn from_uci_castle_kingside_white() {
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/4K2R w K - 0 1").unwrap();
        let mv = Move::from_uci("e1g1", &pos).expect("e1g1 is castling");
        assert_eq!(mv.from_square(), Square::E1);
        assert_eq!(mv.to_square(), Square::G1);
        assert_eq!(mv.flag(), MoveFlag::KingCastle);
    }

    #[test]
    fn from_uci_castle_queenside_white() {
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/R3K3 w Q - 0 1").unwrap();
        let mv = Move::from_uci("e1c1", &pos).expect("e1c1 is castling");
        assert_eq!(mv.from_square(), Square::E1);
        assert_eq!(mv.to_square(), Square::C1);
        assert_eq!(mv.flag(), MoveFlag::QueenCastle);
    }

    #[test]
    fn from_uci_castle_kingside_black() {
        let pos = Position::from_fen("4k2r/8/8/8/8/8/8/4K3 b k - 0 1").unwrap();
        let mv = Move::from_uci("e8g8", &pos).expect("e8g8 is castling");
        assert_eq!(mv.from_square(), Square::E8);
        assert_eq!(mv.to_square(), Square::G8);
        assert_eq!(mv.flag(), MoveFlag::KingCastle);
    }

    #[test]
    fn from_uci_promotion_each_kind() {
        let pos = Position::from_fen("5k2/4P3/8/8/8/8/8/4K3 w - - 0 1").unwrap();
        let cases: &[(&str, MoveFlag)] = &[
            ("e7e8q", MoveFlag::QueenPromo),
            ("e7e8r", MoveFlag::RookPromo),
            ("e7e8b", MoveFlag::BishopPromo),
            ("e7e8n", MoveFlag::KnightPromo),
        ];
        for &(uci, expected_flag) in cases {
            let mv = Move::from_uci(uci, &pos).expect("legal promotion must parse");
            assert_eq!(mv.from_square(), Square::E7, "uci={uci}");
            assert_eq!(mv.to_square(), Square::E8, "uci={uci}");
            assert_eq!(mv.flag(), expected_flag, "uci={uci}");
        }
    }

    #[test]
    fn from_uci_promotion_capture_inferred() {
        // Black knight on f8; white pawn on e7 promotes by capturing.
        let pos = Position::from_fen("4kn2/4P3/8/8/8/8/8/4K3 w - - 0 1").unwrap();
        let mv = Move::from_uci("e7f8q", &pos).expect("e7xf8=Q is legal");
        assert_eq!(mv.from_square(), Square::E7);
        assert_eq!(mv.to_square(), Square::F8);
        assert_eq!(mv.flag(), MoveFlag::QueenPromoCapture);
    }

    #[test]
    fn from_uci_under_check_evasion() {
        // Black bishop on b4 checks white king on e1 via the
        // a5-e1 diagonal. White's only legal moves are: king moves
        // to d1/d2/e2/f1/f2 (the bishop attack-line covers e1, so
        // king must vacate or block). A king step out of check that
        // doesn't fall back into the diagonal: e1f1 (f1 is not on
        // the diagonal). Pins that flag inference correctly handles
        // an in-check position — flag must be Quiet, not anything
        // mistakenly inferred from a "looks like a king step" path
        // that bypassed `generate_moves`.
        let pos = Position::from_fen("4k3/8/8/8/1b6/8/8/4K3 w - - 0 1").unwrap();
        let mv = Move::from_uci("e1f1", &pos).expect("e1f1 is a legal check evasion");
        assert_eq!(mv.from_square(), Square::E1);
        assert_eq!(mv.to_square(), Square::F1);
        assert_eq!(mv.flag(), MoveFlag::Quiet);

        // Belt-and-suspenders: e1d2 lands the king back on the b4-e1
        // diagonal (still attacked by the bishop) — must reject.
        // Materially the same code path as `from_uci_rejects_move_into_check`,
        // but spelled out here so the in-check test pins both directions
        // (legal evasion accepted, illegal "evasion" rejected) on the
        // same position.
        assert_eq!(
            Move::from_uci("e1d2", &pos),
            Err(UciMoveError::IllegalForPosition),
        );
    }

    // -----------------------------------------------------------------------
    // from_uci negative-parse tests — table-driven.
    //
    // The error categorization is intrinsic to the input string's
    // length/character classes (or the explicit `0000` literal), so the
    // position is irrelevant for these. Use the starting position.
    //
    // **Categorization granularity is a project-level commitment, NOT
    // a UCI-spec requirement.** The spec only requires "reject
    // malformed input"; it doesn't dictate which error variant maps to
    // which input. This table pins the granularity chosen in the M2.A
    // plan §"Decisions §2" / §4 (parse from-square → parse to-square →
    // parse promo letter). A correctly-spec'd alternative
    // implementation that examined characters in a different order
    // could conform to UCI but disagree with this table on the
    // borderline rows. The borderline rows below use `is_err()` rather
    // than a specific variant; the rest use the variant.
    // -----------------------------------------------------------------------

    #[test]
    fn from_uci_negative_parse_table() {
        let pos = Position::starting_position();
        let cases: &[(&str, UciMoveError)] = &[
            // Length-based malformed inputs.
            ("", UciMoveError::Malformed),
            ("e", UciMoveError::Malformed),
            ("e2e", UciMoveError::Malformed),
            ("e2e4qq", UciMoveError::Malformed),
            // Bad square chars.
            ("i2e4", UciMoveError::Malformed),
            ("e2i4", UciMoveError::Malformed),
            ("e9e4", UciMoveError::Malformed),
            ("e2e9", UciMoveError::Malformed),
            ("E2E4", UciMoveError::Malformed),
            ("a0a1", UciMoveError::Malformed),
            // Non-ASCII rejected by the is_ascii() guard before slicing
            // (without the guard the &s[2..4] slice would panic on a
            // non-char-boundary index).
            ("e2e°", UciMoveError::Malformed),
            ("\u{265B}\u{265B}", UciMoveError::Malformed),
            // Length-5 inputs that pass the square parse but have a
            // semantically-illegal promo letter — categorized as
            // IllegalPromotionPiece (no spec-borderline ambiguity).
            ("e7e8p", UciMoveError::IllegalPromotionPiece),
            ("e7e8k", UciMoveError::IllegalPromotionPiece),
            ("e7e8x", UciMoveError::IllegalPromotionPiece),
            ("e7e8Q", UciMoveError::IllegalPromotionPiece),
            // Null move.
            ("0000", UciMoveError::NullMove),
        ];
        for &(input, expected) in cases {
            let result = Move::from_uci(input, &pos);
            assert_eq!(
                result,
                Err(expected),
                "input {input:?} should reject as {expected:?}, got {result:?}",
            );
        }

        // Borderline rows — spec doesn't dictate the variant. Pin only
        // that we reject. (For the record, the project's chosen
        // ordering routes them as listed in the comments.)
        for input in [
            "e2 e4", // → Malformed: Square::parse_uci("e ") rejects before promo is read
            "e2e4 ", // → IllegalPromotionPiece: squares parse, byte 4 = ' ' hits the catch-all
            "e2e44", // → IllegalPromotionPiece: squares parse, byte 4 = '4' hits the catch-all
        ] {
            let result = Move::from_uci(input, &pos);
            assert!(
                result.is_err(),
                "input {input:?} must reject (spec doesn't pin the variant), got {result:?}",
            );
        }
    }

    // -----------------------------------------------------------------------
    // from_uci position-dependent rejection tests.
    //
    // The string parses cleanly; the position rejects.
    // -----------------------------------------------------------------------

    #[test]
    fn from_uci_rejects_no_piece_on_from() {
        let pos = Position::starting_position();
        // No piece on e3 in the starting position.
        assert_eq!(
            Move::from_uci("e3e4", &pos),
            Err(UciMoveError::IllegalForPosition),
        );
    }

    #[test]
    fn from_uci_rejects_opposite_color_on_from() {
        // White to move; e7 has a black pawn.
        let pos = Position::starting_position();
        assert_eq!(
            Move::from_uci("e7e6", &pos),
            Err(UciMoveError::IllegalForPosition),
        );
    }

    #[test]
    fn from_uci_rejects_blocked_pawn_push() {
        // White pawn on e2 blocked by black pawn on e3.
        let pos = Position::from_fen("4k3/8/8/8/8/4p3/4P3/4K3 w - - 0 1").unwrap();
        assert_eq!(
            Move::from_uci("e2e3", &pos),
            Err(UciMoveError::IllegalForPosition),
        );
    }

    #[test]
    fn from_uci_rejects_castling_when_no_rights() {
        // Same geometry as a legal kingside castle, but the rights field
        // is empty — castling must reject.
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/4K2R w - - 0 1").unwrap();
        assert_eq!(
            Move::from_uci("e1g1", &pos),
            Err(UciMoveError::IllegalForPosition),
        );
    }

    #[test]
    fn from_uci_rejects_castling_when_in_check() {
        // Black rook on e4 attacks white king on e1 — castling is
        // illegal when in check, even with rights present.
        let pos = Position::from_fen("4k3/8/8/8/4r3/8/8/4K2R w K - 0 1").unwrap();
        assert_eq!(
            Move::from_uci("e1g1", &pos),
            Err(UciMoveError::IllegalForPosition),
        );
    }

    #[test]
    fn from_uci_rejects_promo_when_not_promotion_rank() {
        // e2-e4 is not a promotion. Specifying a promo letter here is
        // illegal-for-position (no legal e2-e4-with-promotion exists).
        let pos = Position::starting_position();
        assert_eq!(
            Move::from_uci("e2e4q", &pos),
            Err(UciMoveError::IllegalForPosition),
        );
    }

    #[test]
    fn from_uci_rejects_missing_promo_when_required() {
        // e7-e8 is a promotion-only square; a length-4 string omits the
        // required promo letter, so no legal move matches.
        let pos = Position::from_fen("5k2/4P3/8/8/8/8/8/4K3 w - - 0 1").unwrap();
        assert_eq!(
            Move::from_uci("e7e8", &pos),
            Err(UciMoveError::IllegalForPosition),
        );
    }

    #[test]
    fn from_uci_rejects_ep_when_target_not_set() {
        // Same piece geometry as the EP-positive test, but EP target is
        // not set — no EP move is legal, no plain capture is legal
        // either (d6 is empty).
        let pos = Position::from_fen("4k3/8/8/3pP3/8/8/8/4K3 w - - 0 1").unwrap();
        assert_eq!(
            Move::from_uci("e5d6", &pos),
            Err(UciMoveError::IllegalForPosition),
        );
    }

    #[test]
    fn from_uci_rejects_move_into_check() {
        // Black rook on e4 absolutely pins white knight on e2 to king
        // on e1; any knight move leaves the e-file and exposes the
        // king. e2-c3 is therefore illegal-for-position. Pins that
        // from_uci defers to generate_moves's legality filter, not
        // just pseudo-legality.
        //
        // (A pawn pin along the same file would NOT work — pushing
        // e2-e3 keeps the pawn on the e-file, still blocking the rook.
        // A knight has no "stay on line" moves, so every knight move
        // is illegal under the pin.)
        let pos = Position::from_fen("3k4/8/8/8/4r3/8/4N3/4K3 w - - 0 1").unwrap();
        assert_eq!(
            Move::from_uci("e2c3", &pos),
            Err(UciMoveError::IllegalForPosition),
        );
    }

    #[test]
    fn from_uci_rejects_when_no_legal_moves_exist() {
        // Stalemate position: black king on h8 is not in check but has
        // no legal moves. generate_moves returns an empty list; find
        // returns None cleanly. Pins the empty-MoveList code path so
        // no future fast-path can panic on it. Multiple inputs assert
        // that rejection comes from "no legal moves at all," not from
        // "this specific from-square has no candidate."
        let pos = Position::from_fen("7k/5Q2/6K1/8/8/8/8/8 b - - 0 1").unwrap();
        for input in ["h8h7", "h8g8", "h8g7", "a1a2"] {
            assert_eq!(
                Move::from_uci(input, &pos),
                Err(UciMoveError::IllegalForPosition),
                "input {input}",
            );
        }
    }

    // -----------------------------------------------------------------------
    // Round-trip tests.
    //
    // For every move that can arise from a legal play, to_uci then
    // from_uci must recover the original move byte-for-byte.
    // -----------------------------------------------------------------------

    #[test]
    fn round_trip_curated_cases() {
        for c in CASES {
            let pos = Position::from_fen(c.before).expect("before FEN parses");
            let uci = c.mv.to_uci();
            let parsed = Move::from_uci(&uci, &pos)
                .unwrap_or_else(|e| panic!("round-trip failed for {} ({uci}): {e:?}", c.label));
            assert_eq!(
                parsed,
                c.mv,
                "round-trip mismatch for {} (uci={uci}): parsed flag {:?}, expected flag {:?}",
                c.label,
                parsed.flag(),
                c.mv.flag(),
            );
        }
    }

    /// Seed FENs for D1 enumeration and the M2.A property tests below.
    /// Mirrors `src/movegen.rs::SEED_FENS` (canonical 6 + edge-class
    /// fixtures from `docs/research/m1-engine-architecture.md` §6).
    /// Reusing the broader edge-class list — not just the canonical 6 —
    /// raises the bar for `prop_at_most_one_legal_move_per_from_to_promo`
    /// and `round_trip_canonical_six_d2_proptest`: bugs in EP-discovery
    /// or EP-horizontal-pin handling are most likely to surface in those
    /// edge positions, not in the well-trodden canonical-6 starts.
    const UCI_SEED_FENS: &[&str] = &[
        // Canonical 6.
        "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
        "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
        "r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1",
        "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
        "r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10",
        // Edge fixtures.
        "3k4/8/8/K1Pp3r/8/8/8/8 w - d6 0 2", // EP horizontal pin.
        "4r2k/8/8/8/3Pp3/8/4K3/8 b - d3 0 1", // EP-double-check parent.
        "4k3/4Q3/3K4/8/8/8/8/8 b - - 0 1",   // Mate.
        "7k/5Q2/6K1/8/8/8/8/8 b - - 0 1",    // Stalemate.
    ];

    #[test]
    fn round_trip_seed_positions_d1() {
        for fen in UCI_SEED_FENS {
            let pos = Position::from_fen(fen).expect("seed FEN parses");
            let mut moves = MoveList::new();
            generate_moves(&pos, &mut moves);
            for mv in moves.as_slice() {
                let uci = mv.to_uci();
                let parsed = Move::from_uci(&uci, &pos)
                    .unwrap_or_else(|e| panic!("D1 round-trip failed for {uci} from {fen}: {e:?}"));
                assert_eq!(
                    parsed, *mv,
                    "D1 round-trip mismatch from {fen}: uci={uci}, expected={mv:?}, parsed={parsed:?}",
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // M2.A property tests.
    //
    // Random walk from one of `UCI_SEED_FENS` (canonical 6 + edge-class
    // fixtures: EP-horizontal-pin, EP-double-check parent, mate,
    // stalemate), then exercise to_uci/from_uci round-trip and the
    // (from, to, promo) uniqueness invariant on the reached position.
    // Mirrors the M1.F `SEED_FENS` + walk pattern from `src/movegen.rs`.
    // -----------------------------------------------------------------------

    /// Tiny SplitMix64 — same constants as `src/movegen.rs::next_u64`.
    /// Re-emitted here rather than exported to keep the random-walk
    /// strategy local to its consumer.
    fn uci_next_u64(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Walk a Position from a seed FEN through `walk_len` legal plies.
    /// Stops early on terminal positions.
    fn uci_walk(seed: &str, walk_len: usize, rng_seed: u64) -> Position {
        let mut pos = Position::from_fen(seed).expect("seed FEN must parse");
        let mut state = rng_seed.wrapping_add(1);
        for _ in 0..walk_len {
            let mut ml = MoveList::new();
            generate_moves(&pos, &mut ml);
            if ml.is_empty() {
                break;
            }
            let pick = (uci_next_u64(&mut state) as usize) % ml.len();
            let mv = ml.as_slice()[pick];
            pos.make_move(mv);
        }
        pos
    }

    fn uci_arb_position() -> impl Strategy<Value = Position> {
        // Walk length 0..=4 plies. 0 = the seed itself (D0 coverage,
        // important for the edge-class seeds in `UCI_SEED_FENS` that
        // already encode the exotic positions); 4 = enough to surface
        // mid-game positions with EP targets, mid-game castling rights,
        // and pawn-promotion candidates from the canonical-6 seeds.
        (0..UCI_SEED_FENS.len(), 0usize..=4, any::<u64>())
            .prop_map(|(idx, walk_len, rng_seed)| uci_walk(UCI_SEED_FENS[idx], walk_len, rng_seed))
    }

    proptest! {
        /// For any reachable position, every legal move round-trips
        /// through to_uci → from_uci correctly. Subsumes
        /// `round_trip_seed_positions_d1` for a strict superset of
        /// positions (D0..=D4 from each seed in `UCI_SEED_FENS`).
        #[test]
        fn round_trip_seed_positions_d2_proptest(pos in uci_arb_position()) {
            let mut moves = MoveList::new();
            generate_moves(&pos, &mut moves);
            for mv in moves.iter() {
                let uci = mv.to_uci();
                let parsed = Move::from_uci(&uci, &pos);
                prop_assert_eq!(
                    parsed,
                    Ok(mv),
                    "round-trip failed for uci={} (mv={:?}) from {}",
                    uci,
                    mv,
                    pos.to_fen(),
                );
            }
        }

        /// Uniqueness of (from, to, promotion_kind) per legal move.
        ///
        /// `from_uci`'s generate-and-match strategy depends on this
        /// invariant. The plan §"Decisions §2" derives it from chess
        /// rules; this property pins it empirically against the
        /// movegen surface across reachable positions. If a future
        /// movegen change ever emits two legal moves with the same
        /// triple, `from_uci` would silently pick the first — this
        /// test catches the regression before that.
        #[test]
        fn prop_at_most_one_legal_move_per_from_to_promo(pos in uci_arb_position()) {
            let mut moves = MoveList::new();
            generate_moves(&pos, &mut moves);
            // O(n²) pairwise check — n ≤ 218 in any chess position
            // (Whittington's max), so ~50k comparisons worst case.
            // Trivial.
            let slice = moves.as_slice();
            for (i, a) in slice.iter().enumerate() {
                for (j_offset, b) in slice[i + 1..].iter().enumerate() {
                    let j = i + 1 + j_offset;
                    let same = a.from_square() == b.from_square()
                        && a.to_square() == b.to_square()
                        && a.promotion_kind() == b.promotion_kind();
                    prop_assert!(
                        !same,
                        "uniqueness violated in {}: pair (i={}, j={}) — {:?} and {:?} share (from, to, promo)",
                        pos.to_fen(),
                        i,
                        j,
                        a,
                        b,
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // UciMoveError surface tests.
    //
    // The Display/Error impls are tiny but worth a smoke test so a
    // future renaming or removal of a variant doesn't silently break
    // diagnostics in M2.B/C.
    // -----------------------------------------------------------------------

    #[test]
    fn uci_move_error_display_messages() {
        // Each variant produces non-empty human-readable output. The
        // exact wording isn't pinned (it's diagnostic, not protocol),
        // but each must be distinct and informative.
        let messages: Vec<String> = [
            UciMoveError::Malformed,
            UciMoveError::IllegalPromotionPiece,
            UciMoveError::NullMove,
            UciMoveError::IllegalForPosition,
        ]
        .iter()
        .map(|e| e.to_string())
        .collect();
        for m in &messages {
            assert!(!m.is_empty(), "Display must produce non-empty text");
        }
        // Distinct messages — no two variants share text.
        for i in 0..messages.len() {
            for j in (i + 1)..messages.len() {
                assert_ne!(
                    messages[i], messages[j],
                    "variants at indices {i} and {j} share a Display message",
                );
            }
        }
    }

    #[test]
    fn uci_move_error_implements_std_error() {
        // Compile-time check: UciMoveError satisfies the `std::error::Error`
        // trait bound, so it's usable with `?`-propagation through
        // `Box<dyn Error>` downstream in M2.B/C.
        fn assert_error<E: std::error::Error>(_: &E) {}
        assert_error(&UciMoveError::Malformed);
    }
}
