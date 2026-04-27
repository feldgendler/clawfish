//! Polyglot Zobrist hashing.
//!
//! Per ADR-0009 (`docs/decisions/0009-polyglot-zobrist.md`), this module
//! implements the **Polyglot** Zobrist key set verbatim — 781 published
//! `u64` constants vendored in [`keys`] — and the **EP-only-when-pseudo-legal**
//! hashing rule from the Polyglot spec.
//!
//! Public surface:
//! - [`piece_key`], [`castling_key`], [`ep_file_key`], [`turn_key`] —
//!   per-component key accessors. M1.E will use these to build incremental
//!   XOR updates for `make_move` / `unmake_move`.
//! - [`ep_file_to_hash`] — the pseudo-legal EP predicate (adjacency only;
//!   NO pin / discovered-check / own-king-check tests, per the Polyglot
//!   spec's verbatim "irrelevant if the potential en passant capturing
//!   move is legal or not").
//! - [`from_scratch`] — full Zobrist hash of a [`Position`]. Called by
//!   `Position::starting_position()` and the FEN parser; M1.E's
//!   `make_move` / `unmake_move` debug-asserts equality against this
//!   after every incremental update.
//!
//! The PolyGlot **adapter tool** (separate from the format) is upstream-
//! abandoned and Homebrew-deprecated — see ADR-0009 §"A note on 'PolyGlot
//! is being retired'". The **format** (which is what this module
//! implements) is frozen and dominant; our hash interoperates with every
//! published Polyglot opening book by construction.

mod keys;

use crate::bitboard::Bitboard;
use crate::piece::{Color, Piece, PieceKind};
use crate::position::{CastlingRights, Position};
use crate::square::Square;

// ---------------------------------------------------------------------------
// Bit-layout pin: the CastlingRights bit positions must match the Polyglot
// castling-key offset order (WK=0, WQ=1, BK=2, BQ=3). Compiles to nothing;
// fails to compile if a future refactor of CastlingRights breaks the
// invariant. See ADR-0009 / plan Decision §4.
// ---------------------------------------------------------------------------

const _: () = {
    assert!(CastlingRights::WHITE_KING.bits() == 1 << 0);
    assert!(CastlingRights::WHITE_QUEEN.bits() == 1 << 1);
    assert!(CastlingRights::BLACK_KING.bits() == 1 << 2);
    assert!(CastlingRights::BLACK_QUEEN.bits() == 1 << 3);
};

// ---------------------------------------------------------------------------
// Polyglot piece-color enumeration.
//
// Polyglot ordering (verbatim from book_format.html):
//   BP=0, WP=1, BN=2, WN=3, BB=4, WB=5, BR=6, WR=7, BQ=8, WQ=9, BK=10, WK=11.
//
// Our PieceKind is Pawn=0..King=5 (matches Polyglot's kind sub-axis).
// Our Color is White=0, Black=1.
//
// Polyglot wants the WHITE entry at the +1 slot within each kind pair, so:
//   2 * kind + (1 - color)  →  White contributes 1, Black contributes 0
// which lands in the Polyglot order above.
// ---------------------------------------------------------------------------

#[inline]
const fn polyglot_piece_index(piece: Piece) -> usize {
    2 * piece.kind.index() + (1 - piece.color.index())
}

// ---------------------------------------------------------------------------
// Per-component key accessors.
// ---------------------------------------------------------------------------

/// Polyglot piece-square key for `(piece, sq)`. XOR this in for each piece
/// on the board.
#[inline]
pub fn piece_key(piece: Piece, sq: Square) -> u64 {
    keys::POLYGLOT_KEYS[64 * polyglot_piece_index(piece) + sq.index() as usize]
}

/// Polyglot castling-right key. `right` must have **exactly one** of the
/// four named flags set (`WHITE_KING`, `WHITE_QUEEN`, `BLACK_KING`,
/// `BLACK_QUEEN`). Debug-asserts on `NONE` or multi-flag input; releases
/// will misbehave silently on misuse.
#[inline]
pub fn castling_key(right: CastlingRights) -> u64 {
    debug_assert_eq!(
        right.bits().count_ones(),
        1,
        "castling_key requires a single-flag CastlingRights",
    );
    keys::POLYGLOT_KEYS[768 + right.bits().trailing_zeros() as usize]
}

/// Polyglot EP-file key for the given file index (`0..8`). XOR this in
/// **only** when [`ep_file_to_hash`] returns `Some(file)`. Debug-asserts
/// if `file >= 8`.
#[inline]
pub fn ep_file_key(file: u8) -> u64 {
    debug_assert!(file < 8, "ep_file_key file must be in 0..8, got {file}");
    keys::POLYGLOT_KEYS[772 + file as usize]
}

/// The single Polyglot turn key. **Asymmetric:** XOR it in **iff WHITE is
/// to move** — Black-to-move contributes 0.
#[inline]
pub const fn turn_key() -> u64 {
    keys::POLYGLOT_KEYS[780]
}

// ---------------------------------------------------------------------------
// Pseudo-legal EP predicate.
// ---------------------------------------------------------------------------

/// Returns `Some(file)` iff `pos.ep_target()` is set AND a pawn of
/// `pos.side_to_move()` sits adjacent (same rank as the would-be capturer,
/// file ±1) to that target — i.e. an EP capture is **pseudo-legally**
/// possible per the Polyglot spec. Returns `None` otherwise.
///
/// "Pseudo-legal" here is **adjacency only** — no pin, discovered-check,
/// or own-king-check tests, per the spec's verbatim "irrelevant if the
/// potential en passant capturing move is legal or not."
///
/// **Trusts the FEN parser's invariant** that `ep_target` is on rank 3 (white
/// just double-pushed; black to move) or rank 6 (black just double-pushed;
/// white to move) — see `docs/architecture.md` "FEN parsing" row. The
/// capturer rank below is derived from `side_to_move` alone; for an
/// undefined cross-mismatched position (e.g. `ep` on rank 3 with
/// white-to-move, which the FEN parser allows but a real game would
/// never produce) this function returns `None` (the wrong rank for the
/// own pawn means no adjacency match), which is the safest outcome.
pub fn ep_file_to_hash(pos: &Position) -> Option<u8> {
    let ep = pos.ep_target()?;
    // The relevant pawn is the side-to-move's would-be CAPTURER, not the
    // opponent's just-pushed pawn. The opponent's pushed pawn sits on the
    // same file as `ep`, one rank toward us; the capturer must sit on the
    // same rank as that pushed pawn, on a file adjacent (±1).
    let stm = pos.side_to_move();
    let capturer_rank: u8 = match stm {
        Color::White => 4, // ep on rank index 5 → capturer on rank 5 (index 4)
        Color::Black => 3, // ep on rank index 2 → capturer on rank 4 (index 3)
    };
    let our_pawns = pos.pieces_colored(stm, PieceKind::Pawn);
    let file = ep.file();
    let mut adjacent = Bitboard::EMPTY;
    if file > 0 {
        adjacent |= Bitboard::from_square(
            Square::from_file_rank(file - 1, capturer_rank).expect("file-1, rank in 0..8"),
        );
    }
    if file < 7 {
        adjacent |= Bitboard::from_square(
            Square::from_file_rank(file + 1, capturer_rank).expect("file+1, rank in 0..8"),
        );
    }
    if (our_pawns & adjacent) != Bitboard::EMPTY {
        Some(file)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// From-scratch hash.
// ---------------------------------------------------------------------------

/// Compute the Polyglot Zobrist hash of `pos` from scratch. Used by
/// `Position::starting_position()`, by the FEN parser via
/// `Position::refresh_zobrist()`, and (M1.E onward) by the debug-build
/// round-trip assert after every `make_move` / `unmake_move`.
///
/// Composition (per the Polyglot spec section ordering):
/// 1. XOR `piece_key` for every occupied square.
/// 2. XOR `castling_key` for each set castling right.
/// 3. XOR `ep_file_key` for the EP file iff [`ep_file_to_hash`] yields one.
/// 4. XOR `turn_key` iff `side_to_move` is `White`.
pub fn from_scratch(pos: &Position) -> u64 {
    let mut h: u64 = 0;
    for sq in Square::all() {
        if let Some(piece) = pos.piece_at(sq) {
            h ^= piece_key(piece, sq);
        }
    }
    let cr = pos.castling_rights();
    if cr.has(CastlingRights::WHITE_KING) {
        h ^= castling_key(CastlingRights::WHITE_KING);
    }
    if cr.has(CastlingRights::WHITE_QUEEN) {
        h ^= castling_key(CastlingRights::WHITE_QUEEN);
    }
    if cr.has(CastlingRights::BLACK_KING) {
        h ^= castling_key(CastlingRights::BLACK_KING);
    }
    if cr.has(CastlingRights::BLACK_QUEEN) {
        h ^= castling_key(CastlingRights::BLACK_QUEEN);
    }
    if let Some(file) = ep_file_to_hash(pos) {
        h ^= ep_file_key(file);
    }
    if pos.side_to_move() == Color::White {
        h ^= turn_key();
    }
    h
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::piece::{Color, Piece, PieceKind};
    use crate::position::{CastlingRights, Position};
    use crate::square::Square;

    // -----------------------------------------------------------------------
    // Vendored-table sanity tests.
    // -----------------------------------------------------------------------

    #[test]
    fn polyglot_keys_first_and_last_pinned() {
        // Sentinel against row-shift or typo in the vendored file.
        assert_eq!(keys::POLYGLOT_KEYS[0], 0x9D39247E33776D41);
        assert_eq!(keys::POLYGLOT_KEYS[780], 0xF8D626AAAF278509);
    }

    #[test]
    fn polyglot_keys_length_781() {
        // Belt-and-suspenders runtime check alongside the compile-time length.
        assert_eq!(keys::POLYGLOT_KEYS.len(), 781);
    }

    #[test]
    fn polyglot_keys_no_duplicates() {
        // Detects copy-paste during vendoring (e.g. a duplicated line).
        let mut seen = std::collections::HashSet::new();
        for (i, &k) in keys::POLYGLOT_KEYS.iter().enumerate() {
            assert!(seen.insert(k), "duplicate key {:#018x} at index {i}", k);
        }
    }

    #[test]
    fn polyglot_keys_no_zero() {
        // The Polyglot spec uses 0 as "no contribution" for unused EP / Black
        // turn.  If a published key were accidentally 0, XORing it in would
        // silently no-op.
        for (i, &k) in keys::POLYGLOT_KEYS.iter().enumerate() {
            assert_ne!(k, 0, "POLYGLOT_KEYS[{i}] must not be zero");
        }
    }

    // -----------------------------------------------------------------------
    // Piece-encoding tests.
    // -----------------------------------------------------------------------

    #[test]
    fn polyglot_piece_index_pinned() {
        // All 12 entries of the Polyglot piece-color enumeration:
        //   BP=0, WP=1, BN=2, WN=3, BB=4, WB=5, BR=6, WR=7, BQ=8, WQ=9, BK=10, WK=11.
        // Catches a `1 - color` vs. `color` flip in the encoding formula and
        // any per-kind arithmetic bug (e.g. swapped Rook/Queen offsets).
        assert_eq!(
            polyglot_piece_index(Piece::new(Color::Black, PieceKind::Pawn)),
            0
        );
        assert_eq!(
            polyglot_piece_index(Piece::new(Color::White, PieceKind::Pawn)),
            1
        );
        assert_eq!(
            polyglot_piece_index(Piece::new(Color::Black, PieceKind::Knight)),
            2
        );
        assert_eq!(
            polyglot_piece_index(Piece::new(Color::White, PieceKind::Knight)),
            3
        );
        assert_eq!(
            polyglot_piece_index(Piece::new(Color::Black, PieceKind::Bishop)),
            4
        );
        assert_eq!(
            polyglot_piece_index(Piece::new(Color::White, PieceKind::Bishop)),
            5
        );
        assert_eq!(
            polyglot_piece_index(Piece::new(Color::Black, PieceKind::Rook)),
            6
        );
        assert_eq!(
            polyglot_piece_index(Piece::new(Color::White, PieceKind::Rook)),
            7
        );
        assert_eq!(
            polyglot_piece_index(Piece::new(Color::Black, PieceKind::Queen)),
            8
        );
        assert_eq!(
            polyglot_piece_index(Piece::new(Color::White, PieceKind::Queen)),
            9
        );
        assert_eq!(
            polyglot_piece_index(Piece::new(Color::Black, PieceKind::King)),
            10
        );
        assert_eq!(
            polyglot_piece_index(Piece::new(Color::White, PieceKind::King)),
            11
        );
    }

    #[test]
    fn piece_key_matches_table_for_known_offsets() {
        // Verify that piece_key indexes POLYGLOT_KEYS correctly.
        let bp = Piece::new(Color::Black, PieceKind::Pawn);
        let wp = Piece::new(Color::White, PieceKind::Pawn);
        let wk = Piece::new(Color::White, PieceKind::King);

        // BP on A1 → POLYGLOT_KEYS[64*0 + 0]
        assert_eq!(piece_key(bp, Square::A1), keys::POLYGLOT_KEYS[0]);
        // WP on A1 → POLYGLOT_KEYS[64*1 + 0]
        assert_eq!(piece_key(wp, Square::A1), keys::POLYGLOT_KEYS[64]);
        // WK on H8 → POLYGLOT_KEYS[64*11 + 63]
        assert_eq!(piece_key(wk, Square::H8), keys::POLYGLOT_KEYS[64 * 11 + 63]);
    }

    // -----------------------------------------------------------------------
    // Castling-key tests.
    // -----------------------------------------------------------------------

    #[test]
    fn castling_key_table_offsets() {
        // Catches a vendoring shift of the four castling slots.
        assert_eq!(
            castling_key(CastlingRights::WHITE_KING),
            keys::POLYGLOT_KEYS[768]
        );
        assert_eq!(
            castling_key(CastlingRights::WHITE_QUEEN),
            keys::POLYGLOT_KEYS[769]
        );
        assert_eq!(
            castling_key(CastlingRights::BLACK_KING),
            keys::POLYGLOT_KEYS[770]
        );
        assert_eq!(
            castling_key(CastlingRights::BLACK_QUEEN),
            keys::POLYGLOT_KEYS[771]
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic]
    fn castling_key_panics_on_none() {
        // Guards the single-flag contract: NONE has no set bits.
        let _ = castling_key(CastlingRights::NONE);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic]
    fn castling_key_panics_on_multi_flag() {
        // Multi-flag input (two bits set) must also panic.
        let _ = castling_key(CastlingRights::WHITE_KING.with(CastlingRights::BLACK_QUEEN));
    }

    // -----------------------------------------------------------------------
    // EP-file-key tests.
    // -----------------------------------------------------------------------

    #[test]
    fn ep_file_key_table_offsets() {
        // Verifies the mapping ep_file_key(f) == POLYGLOT_KEYS[772 + f] for all f.
        for f in 0u8..8 {
            assert_eq!(
                ep_file_key(f),
                keys::POLYGLOT_KEYS[772 + f as usize],
                "ep_file_key({f}) must equal POLYGLOT_KEYS[{}]",
                772 + f as usize,
            );
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic]
    fn ep_file_key_panics_on_oob() {
        // File index 8 is out of range; the function must debug-assert.
        let _ = ep_file_key(8);
    }

    // -----------------------------------------------------------------------
    // Turn-key test.
    // -----------------------------------------------------------------------

    #[test]
    fn turn_key_matches_table() {
        // RandomTurn is the last entry in the 781-key array.
        assert_eq!(turn_key(), keys::POLYGLOT_KEYS[780]);
    }

    // -----------------------------------------------------------------------
    // ep_file_to_hash tests.
    // -----------------------------------------------------------------------

    #[test]
    fn ep_file_to_hash_none_when_no_ep_target() {
        // Any position with ep_target = None must return None, regardless of
        // pawn placement.
        let pos =
            Position::from_fen("4k3/8/8/8/4P3/8/8/4K3 w - - 0 1").expect("test FEN must parse");
        assert_eq!(ep_file_to_hash(&pos), None);
    }

    #[test]
    fn ep_file_to_hash_none_when_no_capturer() {
        // EP target e3 set (as if from 1.e4), black to move.  Black has no
        // pawn on rank 4 at d4 or f4, so no pseudo-legal capturer exists.
        // (Mirrors the condition in Polyglot test vector #2.)
        let pos =
            Position::from_fen("4k3/8/8/8/4P3/8/8/4K3 b - e3 0 1").expect("test FEN must parse");
        assert_eq!(ep_file_to_hash(&pos), None);
    }

    #[test]
    fn ep_file_to_hash_some_for_white_capturer() {
        // EP target f6, white to move.  White pawn on e5 (rank index 4, file 4)
        // is adjacent to f (file 5) — pseudo-legal capture exists.
        // (Mirrors the condition in Polyglot test vector #5.)
        let pos =
            Position::from_fen("4k3/8/8/4Pp2/8/8/8/4K3 w - f6 0 1").expect("test FEN must parse");
        assert_eq!(ep_file_to_hash(&pos), Some(5));
    }

    #[test]
    fn ep_file_to_hash_some_for_black_capturer() {
        // EP target c3, black to move.  Black pawn on b4 (rank index 3, file 1)
        // is adjacent to c (file 2) — pseudo-legal capture exists.
        // (Mirrors the condition in Polyglot test vector #8.)
        let pos =
            Position::from_fen("4k3/8/8/8/1pP5/8/8/4K3 b - c3 0 1").expect("test FEN must parse");
        assert_eq!(ep_file_to_hash(&pos), Some(2));
    }

    #[test]
    fn ep_file_to_hash_a_file_no_left_neighbor() {
        // EP target a6, white to move.  White pawn on b5 is adjacent to the
        // a-file.  The a-file has no left neighbor; the file-edge guard must
        // not underflow and must correctly return Some(0).
        let pos =
            Position::from_fen("4k3/8/8/pP6/8/8/8/4K3 w - a6 0 1").expect("test FEN must parse");
        assert_eq!(ep_file_to_hash(&pos), Some(0));
    }

    #[test]
    fn ep_file_to_hash_h_file_no_right_neighbor() {
        // EP target h6, white to move.  White pawn on g5 is adjacent to the
        // h-file.  The h-file has no right neighbor; the file-edge guard must
        // not overflow and must correctly return Some(7).
        let pos =
            Position::from_fen("4k3/8/8/6Pp/8/8/8/4K3 w - h6 0 1").expect("test FEN must parse");
        assert_eq!(ep_file_to_hash(&pos), Some(7));
    }

    #[test]
    fn ep_file_to_hash_pinned_capturer_still_hashes() {
        // EP target c3, black to move.  Black pawn on b4 is geometrically
        // adjacent to c — but it is pinned along the b-file by a white rook
        // on b1 against the black king on b8.  The Polyglot spec says "irrelevant
        // if the potential en passant capturing move is legal or not" — we must
        // still return Some(2).  This test catches accidental drift toward the
        // legal-only EP-hashing refinement.
        let pos =
            Position::from_fen("1k6/8/8/8/1pP5/8/8/1R5K b - c3 0 1").expect("test FEN must parse");
        assert_eq!(ep_file_to_hash(&pos), Some(2));
    }

    // -----------------------------------------------------------------------
    // from_scratch decomposition tests.
    // -----------------------------------------------------------------------

    #[test]
    fn from_scratch_xor_decomposes() {
        // Two kings only, no castling, no EP, white to move.
        // from_scratch must equal piece_key(WK, E1) ^ piece_key(BK, E8) ^ turn_key().
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/4K3 w - - 0 1").expect("test FEN must parse");
        let wk = Piece::new(Color::White, PieceKind::King);
        let bk = Piece::new(Color::Black, PieceKind::King);
        let expected = piece_key(wk, Square::E1) ^ piece_key(bk, Square::E8) ^ turn_key();
        assert_eq!(from_scratch(&pos), expected);
    }

    #[test]
    fn from_scratch_side_to_move_asymmetry() {
        // Two otherwise-identical positions (two kings + one white pawn, no EP,
        // no castling) differing only in side_to_move.  The NO-EP precondition
        // is deliberate: if EP were set, flipping side-to-move would also retoggle
        // ep_file_to_hash, muddying the turn-key delta.
        let white_to_move = Position::from_fen("4k3/8/8/8/4P3/8/8/4K3 w - - 0 1")
            .expect("white-to-move FEN must parse");
        let black_to_move = Position::from_fen("4k3/8/8/8/4P3/8/8/4K3 b - - 0 1")
            .expect("black-to-move FEN must parse");

        let hw = from_scratch(&white_to_move);
        let hb = from_scratch(&black_to_move);

        // The XOR difference between the two hashes must be exactly turn_key().
        assert_eq!(hw ^ hb, turn_key());
        // Pin the direction: the white-to-move hash IS the one that includes
        // turn_key(), so XORing it out must yield the black hash.
        assert_eq!(hw ^ turn_key(), hb);
    }

    #[test]
    fn from_scratch_starting_position_pinned() {
        // Gold-standard interop test: Polyglot test vector #1.
        // Any vendoring error, piece-color flip, or castling-key mis-shift
        // breaks this test first.
        let pos = Position::starting_position();
        assert_eq!(from_scratch(&pos), 0x463b96181691fc9c);
    }

    #[test]
    fn from_scratch_castling_xor_decomposes() {
        // Two kings + all four rooks on starting squares + all four castling
        // rights, no EP, white to move. Pins the per-right XOR composition
        // path inside from_scratch. A "skipped XOR for one of the four rights"
        // bug breaks this test directly (instead of leaving diagnostics to
        // ~6 of the 9 integration vectors failing simultaneously).
        //
        // Rooks on a1/h1/a8/h8 are required by the FENVAL §13 castling-mailbox
        // check; the test FEN previously omitted them, which the M1.F
        // validation now rejects.
        let pos = Position::from_fen("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1")
            .expect("test FEN must parse");
        let wk = Piece::new(Color::White, PieceKind::King);
        let bk = Piece::new(Color::Black, PieceKind::King);
        let wr = Piece::new(Color::White, PieceKind::Rook);
        let br = Piece::new(Color::Black, PieceKind::Rook);
        let expected = piece_key(wk, Square::E1)
            ^ piece_key(bk, Square::E8)
            ^ piece_key(wr, Square::A1)
            ^ piece_key(wr, Square::H1)
            ^ piece_key(br, Square::A8)
            ^ piece_key(br, Square::H8)
            ^ castling_key(CastlingRights::WHITE_KING)
            ^ castling_key(CastlingRights::WHITE_QUEEN)
            ^ castling_key(CastlingRights::BLACK_KING)
            ^ castling_key(CastlingRights::BLACK_QUEEN)
            ^ turn_key();
        assert_eq!(from_scratch(&pos), expected);
    }

    // -----------------------------------------------------------------------
    // Property tests.
    // -----------------------------------------------------------------------

    use proptest::prelude::*;

    // Strategy: arbitrary (piece, square) pair.
    fn arb_piece() -> impl Strategy<Value = Piece> {
        (0usize..2, 0usize..6).prop_map(|(color_idx, kind_idx)| {
            let color = if color_idx == 0 {
                Color::White
            } else {
                Color::Black
            };
            let kind = PieceKind::ALL[kind_idx];
            Piece::new(color, kind)
        })
    }

    proptest! {
        #[test]
        fn prop_piece_key_xor_idempotence(
            h in any::<u64>(),
            piece in arb_piece(),
            sq_idx in 0u8..64,
        ) {
            // XORing in then XORing out the same piece key is identity.
            // Pins the toggle contract that M1.E's make_move / unmake_move relies on.
            let sq = Square::new(sq_idx).expect("0..64 is a valid square");
            let k = piece_key(piece, sq);
            prop_assert_eq!(h ^ k ^ k, h);
        }

        #[test]
        fn prop_castling_key_xor_idempotence(
            h in any::<u64>(),
            // Pick one of the four valid single-flag castling rights.
            right_idx in 0u8..4,
        ) {
            // XORing in then XORing out the same castling key is identity.
            let right = match right_idx {
                0 => CastlingRights::WHITE_KING,
                1 => CastlingRights::WHITE_QUEEN,
                2 => CastlingRights::BLACK_KING,
                _ => CastlingRights::BLACK_QUEEN,
            };
            let k = castling_key(right);
            prop_assert_eq!(h ^ k ^ k, h);
        }

        #[test]
        fn prop_turn_key_xor_idempotence(h in any::<u64>()) {
            // Toggling the turn key twice is identity.
            let k = turn_key();
            prop_assert_eq!(h ^ k ^ k, h);
        }

        #[test]
        fn prop_ep_file_key_xor_idempotence(
            h in any::<u64>(),
            file in 0u8..8,
        ) {
            // XORing in then XORing out the same EP-file key is identity.
            let k = ep_file_key(file);
            prop_assert_eq!(h ^ k ^ k, h);
        }

        #[test]
        fn prop_from_scratch_pieces_only_decomposes(
            // A vec of (sq_idx, color_idx, kind_idx) triples, up to 32 entries.
            // We use Black-to-move to suppress turn_key, no castling, no EP.
            pieces_raw in prop::collection::vec(
                (0u8..64, 0usize..2, 0usize..6),
                0..=32,
            ),
        ) {
            // Deduplicate squares so we don't place two pieces on the same square.
            let mut used_squares = std::collections::HashSet::new();
            let mut pieces: Vec<(Square, Piece)> = Vec::new();
            for (sq_idx, color_idx, kind_idx) in pieces_raw {
                let sq = Square::new(sq_idx).expect("0..64 is valid");
                // Skip king-kind entries (placing kings via set_piece on occupied
                // squares or overwriting kings would violate debug asserts; the
                // two-kings invariant required by validate_post_parse is also not
                // guaranteed here).  We add the two required kings separately.
                let kind = PieceKind::ALL[kind_idx];
                if kind == PieceKind::King {
                    continue;
                }
                if used_squares.insert(sq_idx) {
                    pieces.push((sq, Piece::new(
                        if color_idx == 0 { Color::White } else { Color::Black },
                        kind,
                    )));
                }
            }

            // Always place kings on squares not already used.
            // Find two free squares: e1 (index 4) and e8 (index 60).
            // If those are taken by the random pieces, skip the property case
            // (use `prop_assume!`).
            prop_assume!(!used_squares.contains(&4));
            prop_assume!(!used_squares.contains(&60));

            // Build the position via set_piece + set_aux_state + refresh_zobrist.
            let mut pos = Position::empty();
            pos.set_piece(Square::E1, Piece::new(Color::White, PieceKind::King));
            pos.set_piece(Square::E8, Piece::new(Color::Black, PieceKind::King));
            for &(sq, piece) in &pieces {
                pos.set_piece(sq, piece);
            }
            // Black to move suppresses turn_key; no castling; no EP.
            pos.set_aux_state(Color::Black, CastlingRights::NONE, None, 0, 1);
            pos.refresh_zobrist();

            // Compute the expected hash manually: XOR of each piece key.
            let wk = Piece::new(Color::White, PieceKind::King);
            let bk = Piece::new(Color::Black, PieceKind::King);
            let mut expected = piece_key(wk, Square::E1) ^ piece_key(bk, Square::E8);
            for &(sq, piece) in &pieces {
                expected ^= piece_key(piece, sq);
            }
            // No castling keys, no EP key, no turn key (Black to move).

            prop_assert_eq!(from_scratch(&pos), expected);
        }
    }

    // -----------------------------------------------------------------------
    // Throughput sanity benchmark (ignored by default — `cargo test --release
    // -- --ignored bench_zobrist_throughput`). One-number sanity floor; the
    // criterion harness lands in M1.G.
    // -----------------------------------------------------------------------

    #[test]
    #[ignore]
    fn bench_zobrist_throughput() {
        use std::hint::black_box;
        use std::time::Instant;

        let pos = Position::starting_position();
        const ITERS: u32 = 1_000_000;

        let t0 = Instant::now();
        for _ in 0..ITERS {
            let _ = black_box(from_scratch(black_box(&pos)));
        }
        let elapsed = t0.elapsed();
        let ns_per = elapsed.as_nanos() as f64 / ITERS as f64;
        println!(
            "from_scratch(starting_position) over {ITERS} iters: {elapsed:?} \
             ({ns_per:.1} ns/op, {:.1} M ops/sec)",
            1000.0 / ns_per,
        );

        // Same again on ep_file_to_hash — pinned starting position has no EP,
        // so this exercises only the early-exit path.
        let t0 = Instant::now();
        for _ in 0..ITERS {
            let _ = black_box(ep_file_to_hash(black_box(&pos)));
        }
        let elapsed = t0.elapsed();
        let ns_per = elapsed.as_nanos() as f64 / ITERS as f64;
        println!(
            "ep_file_to_hash(starting_position, None EP) over {ITERS} iters: {elapsed:?} \
             ({ns_per:.2} ns/op)",
        );
    }
}
