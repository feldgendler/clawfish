//! Integration tests for `make_move` / `unmake_move`.
//!
//! Per `docs/plans/m1.e.md` §"`tests/make_unmake.rs` — integration":
//! - Multi-ply sequence applied + unmade, exercising capture, double-push,
//!   castle, EP, and promotion-capture in one chain.
//! - Quiet move + immediate unmake against each of the canonical six
//!   perft FENs as a public-API surface sentinel.
//!
//! Uses only the public crate-root API.

use clawfish::{
    Color, Move, MoveFlag, Piece, PieceKind, Position, Square,
    mov::{make_move, unmake_move},
    zobrist::from_scratch,
};

/// Six-ply hand-curated sequence exercising the special-case spectrum:
/// castle, double-push, EP capture, quiet, promotion-capture, and a
/// king-move escaping check — each in one ply. After every ply, the
/// incremental zobrist is asserted equal to `from_scratch(&pos)`. After
/// the make-loop, the full FEN is asserted equal to the Stockfish-derived
/// expected FEN as an external oracle anchor — without this, internal
/// consistency could be satisfied by chess-incorrect mutations. After all
/// six unmakes, byte-equality with the original starting position is
/// asserted.
///
/// Setup (white to move): `r3k3/1P1p3p/8/4P3/8/8/8/4K2R w K - 0 1`
///   - White: K e1, R h1, pawn e5, pawn b7. Kingside-castle eligible.
///   - Black: k e8, r a8, pawn d7, pawn h7.
///
/// Sequence (each move hand-validated as legal vs. Stockfish):
///   ply 0 (white): e1-g1 KingCastle         → king g1, rook f1; K right cleared
///   ply 1 (black): d7-d5 DoublePush         → ep target d6; black pawn d5
///   ply 2 (white): e5-d6 EnPassant          → white pawn d6, black pawn (was d5) removed
///   ply 3 (black): h7-h6 Quiet              → quiet pawn move
///   ply 4 (white): b7-a8 QueenPromoCapture  → white queen a8; black rook captured; black king now in check
///   ply 5 (black): e8-d7 Quiet              → king escapes check (d7 not attacked: pawn on d6 hits e7/c7, queen on a8 doesn't see d7)
///
/// Stockfish-derived final FEN: `Q7/3k4/3P3p/8/8/8/8/5RK1 w - - 1 4`
#[test]
fn play_and_unwind_special_case_sequence() {
    const START: &str = "r3k3/1P1p3p/8/4P3/8/8/8/4K2R w K - 0 1";
    let original = Position::from_fen(START).expect("setup FEN parses");

    let sequence: &[Move] = &[
        Move::new(Square::E1, Square::G1, MoveFlag::KingCastle),
        Move::new(Square::D7, Square::D5, MoveFlag::DoublePush),
        Move::new(Square::E5, Square::D6, MoveFlag::EnPassant),
        Move::quiet(Square::H7, Square::H6),
        Move::new(Square::B7, Square::A8, MoveFlag::QueenPromoCapture),
        Move::quiet(Square::E8, Square::D7),
    ];

    let mut pos = original;
    let mut undos: Vec<clawfish::Undo> = Vec::with_capacity(sequence.len());

    for (i, &mv) in sequence.iter().enumerate() {
        let undo = make_move(&mut pos, mv);
        assert_eq!(
            pos.zobrist(),
            from_scratch(&pos),
            "incremental zobrist diverged at ply {i} after {mv:?}",
        );
        undos.push(undo);
    }

    // External oracle anchor: the final FEN matches Stockfish's
    // interpretation. Catches a make_move whose internal consistency is
    // satisfied but whose mutations are chess-incorrect (e.g. EP removing
    // the wrong pawn).
    assert_eq!(
        pos.to_fen(),
        "Q7/3k4/3P3p/8/8/8/8/5RK1 w - - 1 4",
        "post-make FEN must match Stockfish-validated final state",
    );

    // Unmake in reverse order. Use a plain reverse loop — pair sequence[i]
    // with undos[i] explicitly via index.
    for i in (0..sequence.len()).rev() {
        let mv = sequence[i];
        let undo = undos[i];
        unmake_move(&mut pos, mv, undo);
        assert_eq!(
            pos.zobrist(),
            from_scratch(&pos),
            "incremental zobrist diverged after unmake of ply {i}",
        );
    }

    assert_eq!(pos, original, "byte-equality after full unwind");
}

/// For each of the six canonical perft FENs, apply one curated quiet move
/// and immediately unmake. Asserts byte-equality and zobrist consistency
/// after make. Sentinel that the public API isn't broken across the
/// FEN spectrum.
#[test]
fn canonical_six_perft_positions_round_trip_smoke() {
    let cases: &[(&str, Move)] = &[
        // Starting position.
        (
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
            Move::quiet(Square::G1, Square::F3),
        ),
        // Kiwipete: bishop e2-d1.
        (
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
            Move::quiet(Square::E2, Square::D1),
        ),
        // Position 3.
        (
            "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
            Move::quiet(Square::A5, Square::A6),
        ),
        // Position 4.
        (
            "r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1",
            Move::quiet(Square::F3, Square::E5),
        ),
        // Position 5: white knight b1 → a3 (a3 is empty; knight moves are
        // quiet here). The earlier choice (king e1-f2) was a capture, not
        // a quiet move — f2 holds a black knight.
        (
            "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
            Move::quiet(Square::B1, Square::A3),
        ),
        // Position 6.
        (
            "r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10",
            Move::quiet(Square::C3, Square::D5),
        ),
    ];

    for (fen, mv) in cases {
        let original =
            Position::from_fen(fen).unwrap_or_else(|e| panic!("FEN {fen} parses: {e:?}"));
        let mut pos = original;
        let undo = make_move(&mut pos, *mv);
        assert_eq!(
            pos.zobrist(),
            from_scratch(&pos),
            "incremental zobrist diverged for FEN {fen}",
        );
        unmake_move(&mut pos, *mv, undo);
        assert_eq!(pos, original, "round-trip failed for FEN {fen}");
    }
}

/// Diagnostic: EP capture works end-to-end through the `Position::make_move`
/// method delegate (vs the free function). Distinct surface check.
#[test]
fn ep_capture_via_position_method() {
    let mut pos = Position::from_fen("4k3/8/8/3pP3/8/8/8/4K3 w - d6 0 1").unwrap();
    let mv = Move::new(Square::E5, Square::D6, MoveFlag::EnPassant);

    let undo = pos.make_move(mv);
    assert_eq!(
        pos.piece_at(Square::D6),
        Some(Piece::new(Color::White, PieceKind::Pawn))
    );
    assert_eq!(pos.piece_at(Square::D5), None);
    assert_eq!(pos.piece_at(Square::E5), None);
    assert_eq!(pos.ep_target(), None);
    assert_eq!(pos.side_to_move(), Color::Black);

    pos.unmake_move(mv, undo);
    let original = Position::from_fen("4k3/8/8/3pP3/8/8/8/4K3 w - d6 0 1").unwrap();
    assert_eq!(pos, original);
}
