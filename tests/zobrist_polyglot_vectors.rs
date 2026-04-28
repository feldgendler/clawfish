//! Integration tests for Polyglot Zobrist hashing — the 9 published
//! `(FEN, key)` test vectors from the Polyglot spec
//! (`docs/reference/polyglot-book-format.md`).
//!
//! Each test exercises a different sub-axis: piece-color encoding,
//! castling-key order, EP-pseudo-legal rule (yes/no cases, both colors),
//! side-to-move asymmetry. Together they constitute the gold-standard
//! correctness check for the Polyglot Zobrist implementation.

use clawfish::Position;

// ---------------------------------------------------------------------------
// The 9 published Polyglot test vectors.
//
// Source: docs/reference/polyglot-book-format.md (vendored from book_format.html).
//
// Each FEN is defined as a named constant so the assertion message is
// actionable: a failing test prints the exact FEN that broke.
// ---------------------------------------------------------------------------

/// Vector 1: starting position.
/// Exercises: all four castle keys (WK, WQ, BK, BQ), turn=White.
const FEN1: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";

/// Vector 2: after 1.e4 — EP target e3 set, but Black has no adjacent pawn.
/// Exercises: EP target present but no pseudo-legal capturer → EP key NOT hashed;
/// turn=Black (turn key not XORed).
const FEN2: &str = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq e3 0 1";

/// Vector 3: after 1.e4 d5 — EP target d6, white-to-move; white pawn on e4
/// (rank index 3, not rank index 4) — wrong rank, so not adjacent.
/// Exercises: EP target present but capturer is on wrong rank → EP key NOT hashed.
const FEN3: &str = "rnbqkbnr/ppp1pppp/8/3p4/4P3/8/PPPP1PPP/RNBQKBNR w KQkq d6 0 2";

/// Vector 4: no EP target at all.
/// Exercises: no EP key contribution.
const FEN4: &str = "rnbqkbnr/ppp1pppp/8/3pP3/8/8/PPPP1PPP/RNBQKBNR b KQkq - 0 2";

/// Vector 5: white pawn on e5 adjacent to EP target f6, white-to-move.
/// Exercises: EP-pseudo-legal, white side — EP key f IS hashed.
const FEN5: &str = "rnbqkbnr/ppp1p1pp/8/3pPp2/8/8/PPPP1PPP/RNBQKBNR w KQkq f6 0 3";

/// Vector 6: white king has moved (Ke1→e2) — both white castle keys gone.
/// Exercises: partial castling rights (only BK, BQ remain).
const FEN6: &str = "rnbqkbnr/ppp1p1pp/8/3pPp2/8/8/PPPPKPPP/RNBQ1BNR b kq - 0 3";

/// Vector 7: both kings have moved — all castle keys gone.
/// Exercises: zero castling rights; no castling contribution to hash.
const FEN7: &str = "rnbq1bnr/ppp1pkpp/8/3pPp2/8/8/PPPPKPPP/RNBQ1BNR w - - 0 4";

/// Vector 8: EP target c3, black-to-move; black pawn on b4 is adjacent.
/// Exercises: EP-pseudo-legal, black side — EP key c IS hashed.
const FEN8: &str = "rnbqkbnr/p1pppppp/8/8/PpP4P/8/1P1PPPP1/RNBQKBNR b KQkq c3 0 3";

/// Vector 9: EP consumed (bxc3); white queenside rook moved (Ra1→a3) — WQ castle
/// key gone.
/// Exercises: mixed castling (WK absent, WQ absent, BK present, BQ present).
const FEN9: &str = "rnbqkbnr/p1pppppp/8/8/P6P/R1p5/1P1PPPP1/1NBQKBNR b Kkq - 0 4";

#[test]
fn polyglot_vector_1_starting_position() {
    let pos = Position::from_fen(FEN1).expect("test FEN must parse");
    assert_eq!(pos.zobrist(), 0x463b96181691fc9c, "FEN: {FEN1}");
}

#[test]
fn polyglot_vector_2_ep_target_no_capturer() {
    let pos = Position::from_fen(FEN2).expect("test FEN must parse");
    assert_eq!(pos.zobrist(), 0x823c9b50fd114196, "FEN: {FEN2}");
}

#[test]
fn polyglot_vector_3_ep_target_capturer_wrong_rank() {
    let pos = Position::from_fen(FEN3).expect("test FEN must parse");
    assert_eq!(pos.zobrist(), 0x0756b94461c50fb0, "FEN: {FEN3}");
}

#[test]
fn polyglot_vector_4_no_ep_target() {
    let pos = Position::from_fen(FEN4).expect("test FEN must parse");
    assert_eq!(pos.zobrist(), 0x662fafb965db29d4, "FEN: {FEN4}");
}

#[test]
fn polyglot_vector_5_ep_pseudo_legal_white() {
    let pos = Position::from_fen(FEN5).expect("test FEN must parse");
    assert_eq!(pos.zobrist(), 0x22a48b5a8e47ff78, "FEN: {FEN5}");
}

#[test]
fn polyglot_vector_6_white_king_moved() {
    let pos = Position::from_fen(FEN6).expect("test FEN must parse");
    assert_eq!(pos.zobrist(), 0x652a607ca3f242c1, "FEN: {FEN6}");
}

#[test]
fn polyglot_vector_7_both_kings_moved() {
    let pos = Position::from_fen(FEN7).expect("test FEN must parse");
    assert_eq!(pos.zobrist(), 0x00fdd303c946bdd9, "FEN: {FEN7}");
}

#[test]
fn polyglot_vector_8_ep_pseudo_legal_black() {
    let pos = Position::from_fen(FEN8).expect("test FEN must parse");
    assert_eq!(pos.zobrist(), 0x3c8123ea7b067637, "FEN: {FEN8}");
}

#[test]
fn polyglot_vector_9_ep_consumed_rook_moved() {
    let pos = Position::from_fen(FEN9).expect("test FEN must parse");
    assert_eq!(pos.zobrist(), 0x5c3f9b829b279560, "FEN: {FEN9}");
}
