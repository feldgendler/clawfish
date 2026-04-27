//! Integration tests for FEN round-tripping through the public `Position` API.
//!
//! These tests deliberately use only the public surface of the `chess` crate
//! (`Position::from_fen`, `Position::to_fen`, `Position::STARTING_FEN`,
//! `PartialEq`). They prove that a downstream consumer can:
//!
//! 1. Parse the canonical starting FEN and format it back identically.
//! 2. Parse each of the six canonical perft FENs and format them back
//!    identically (string-level equality — every field, including castling
//!    order, EP encoding, and clocks, must survive verbatim).
//! 3. Round-trip every canonical perft FEN twice and observe semantic
//!    equality (`Position == Position`). This catches a class of formatter
//!    bugs where `to_fen` produces a *different but still-parseable* string
//!    that nevertheless decodes to the same position; the second parse is
//!    the witness.

use chess::Position;

/// Canonical perft positions referenced throughout M1. Pinned here as a
/// literal array so the test file is self-contained and an accidental edit
/// to a shared constants module cannot silently weaken these assertions.
const CANONICAL_PERFT_FENS: &[&str] = &[
    "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
    "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
    "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
    "r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1",
    "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
    "r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10",
];

#[test]
fn starting_position_round_trip() {
    let parsed =
        Position::from_fen(Position::STARTING_FEN).expect("STARTING_FEN must parse cleanly");
    assert_eq!(
        parsed.to_fen(),
        Position::STARTING_FEN,
        "starting FEN must format back to itself byte-for-byte"
    );
}

#[test]
fn canonical_perft_positions_to_fen_matches_input() {
    for &fen in CANONICAL_PERFT_FENS {
        let parsed = Position::from_fen(fen)
            .unwrap_or_else(|e| panic!("canonical FEN failed to parse: {fen:?}: {e:?}"));
        assert_eq!(
            parsed.to_fen(),
            fen,
            "canonical FEN must format back to itself byte-for-byte: {fen:?}"
        );
    }
}

#[test]
fn canonical_perft_positions_idempotent_round_trip() {
    for &fen in CANONICAL_PERFT_FENS {
        let first = Position::from_fen(fen)
            .unwrap_or_else(|e| panic!("canonical FEN failed to parse: {fen:?}: {e:?}"));
        let reformatted = first.to_fen();
        let second = Position::from_fen(&reformatted).unwrap_or_else(|e| {
            panic!(
                "reformatted FEN failed to parse on second round-trip: \
                 original={fen:?}, reformatted={reformatted:?}: {e:?}"
            )
        });
        assert_eq!(
            first, second,
            "round-trip must be idempotent at the Position level: \
             original FEN {fen:?} reformatted as {reformatted:?}"
        );
    }
}
