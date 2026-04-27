//! Integration tests for legal move generation (M1.F).
//!
//! Per `docs/plans/m1.f.md` §"Integration tests" and §"Property tests".
//! These tests cover the public crate-root API only (`chess::*`):
//! - depth-1 perft equivalents on the canonical six positions,
//! - checkmate / stalemate detection,
//! - the EP horizontal-pin trap,
//! - the EP-causing-double-check fixture,
//! - underpromotion fan-out plus check delivery,
//! - round-trip make_move / unmake_move over every emitted move,
//! - property tests over a seeded random-walk corpus.
//!
//! The expected move counts in `move_count_canonical_six_at_depth_1` are
//! Stockfish-confirmed depth-1 perft values (per ADR-0006). The plan-execute
//! step verifies these against `go perft 1` from a reference Stockfish build.
//!
//! `prop_no_legal_move_leaves_us_in_check` is **not** here — it requires the
//! crate-private `masks::checkers_of` (the public `in_check` is hard-wired to
//! the side-to-move and cannot answer "did the side that just moved leave
//! itself in check?"). That property lives alongside the module's other
//! `proptest!` tests inside `src/movegen/masks.rs` (or a sibling submodule);
//! this file is the integration-surface harness only.
//!
//! Implementation is stubbed at the time of writing; these tests are red
//! until the M1.F impl pass lands.

use chess::{
    Color, Move, MoveFlag, MoveList, Position, Square, generate_moves, in_check,
    mov::{make_move, unmake_move},
};
use proptest::prelude::*;

// -----------------------------------------------------------------------
// Canonical-six perft FENs and their Stockfish-confirmed depth-1 counts.
//
// Source: Chess Programming Wiki "Perft Results" canonical positions,
// re-verified at plan time against Stockfish (`go perft 1`). These are the
// project's perft oracle per ADR-0006.
// -----------------------------------------------------------------------

const STARTING_FEN: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
const KIWIPETE_FEN: &str = "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1";
const POSITION_3_FEN: &str = "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1";
const POSITION_4_FEN: &str = "r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1";
const POSITION_5_FEN: &str = "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8";
const POSITION_6_FEN: &str =
    "r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10";

/// `(fen, expected_depth_1_legal_move_count)`. The counts are
/// Stockfish-confirmed depth-1 perft values; the plan-execute step
/// verifies them against `stockfish -- "go perft 1"` for each FEN.
const CANONICAL_SIX: [(&str, usize); 6] = [
    (STARTING_FEN, 20),
    (KIWIPETE_FEN, 48),
    (POSITION_3_FEN, 14),
    (POSITION_4_FEN, 6),
    (POSITION_5_FEN, 44),
    (POSITION_6_FEN, 46),
];

// Custom edge fixtures used by the round-trip integration test and the
// property-test corpus. Sourced from `docs/research/m1-engine-architecture.md`
// §6 (the eleven well-known bug-class fixtures).
const MATE_FEN: &str = "4k3/4Q3/3K4/8/8/8/8/8 b - - 0 1";
const STALEMATE_FEN: &str = "7k/5Q2/6K1/8/8/8/8/8 b - - 0 1";
const EP_HORIZONTAL_PIN_FEN: &str = "3k4/8/8/K1Pp3r/8/8/8/8 w - d6 0 2";
const EP_DOUBLE_CHECK_FEN: &str = "4r2k/8/8/8/3Pp3/8/4K3/8 b - d3 0 1";

// -----------------------------------------------------------------------
// Helpers.
// -----------------------------------------------------------------------

fn parse(fen: &str) -> Position {
    Position::from_fen(fen).unwrap_or_else(|e| panic!("FEN must parse: {fen} ({e:?})"))
}

fn move_set(pos: &Position) -> Vec<Move> {
    let mut ml = MoveList::new();
    generate_moves(pos, &mut ml);
    ml.as_slice().to_vec()
}

// -----------------------------------------------------------------------
// Integration tests — direct fixtures.
// -----------------------------------------------------------------------

#[test]
fn move_count_canonical_six_at_depth_1() {
    // Stockfish-confirmed depth-1 perft values; the plan-execute step
    // verifies them against `go perft 1` for each FEN. At depth 1 the
    // perft count is exactly the legal-move-list length.
    for (fen, expected) in CANONICAL_SIX {
        let pos = parse(fen);
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        assert_eq!(
            ml.len(),
            expected,
            "depth-1 perft mismatch for {fen}: got {}, expected {expected}",
            ml.len()
        );
    }
}

#[test]
fn checkmate_no_moves_and_in_check() {
    // White Kd6 + Qe7 delivers mate to black king on e8.
    // - Queen on e7 attacks e8 (file), d8 and f8 (diagonals), f7 (rank).
    // - White king on d6 covers d7 and e7 (so the queen capture would be
    //   into check).
    // - Black king has no escape: e8 (in check), d8/f8 (queen), d7/e7 (king),
    //   f7 (queen). No blocker fits on a queen+king mate net.
    let pos = parse(MATE_FEN);
    assert!(in_check(&pos), "mated side must be in check ({MATE_FEN})");
    let moves = move_set(&pos);
    assert!(
        moves.is_empty(),
        "mate: expected zero legal moves, got {} for {MATE_FEN}",
        moves.len()
    );
}

#[test]
fn stalemate_no_moves_not_in_check() {
    // Black king h8 with white queen f7 + white king g6: black is not in
    // check, but every black king move lands on a white-attacked square.
    let pos = parse(STALEMATE_FEN);
    assert!(
        !in_check(&pos),
        "stalemated side must NOT be in check ({STALEMATE_FEN})"
    );
    let moves = move_set(&pos);
    assert!(
        moves.is_empty(),
        "stalemate: expected zero legal moves, got {} for {STALEMATE_FEN}",
        moves.len()
    );
}

#[test]
fn ep_horizontal_pin_filtered() {
    // `3k4/8/8/K1Pp3r/8/8/8/8 w - d6 0 2`: white king a5, white pawn c5,
    // black pawn d5 (just double-pushed; EP target d6), black rook h5.
    // Both pawns vacate rank 5 simultaneously on `c5xd6 e.p.`, exposing
    // the white king to the rook on h5. The EP move must NOT be emitted.
    let pos = parse(EP_HORIZONTAL_PIN_FEN);
    let illegal_ep = Move::new(Square::C5, Square::D6, MoveFlag::EnPassant);
    let moves = move_set(&pos);
    assert!(
        !moves.contains(&illegal_ep),
        "horizontal-pin EP move c5xd6 must NOT be in the move list (Position-3 trap)"
    );
}

#[test]
fn ep_causing_double_check() {
    // `4r2k/8/8/8/3Pp3/8/4K3/8 b - d3 0 1`: black plays e4xd3 e.p. as the
    // unique single-move class that delivers double check (§6.3 taxonomy).
    //
    // Geometry post-EP:
    // - black pawn lands on d3, attacks (as black) c2 and e2 — checks white
    //   king on e2.
    // - black rook on e8 sees down a now-empty e-file (the moving black pawn
    //   vacated e4) all the way to e2 — discovered check.
    //
    // ASSERTION CAVEAT: `checkers_of` is `pub(crate)` — not reachable from
    // an integration-test crate. We assert the weaker `in_check(post_pos)`
    // bound. The true `count == 2` claim is pinned by an in-crate unit test
    // alongside `src/movegen/masks.rs` (see plan §"Integration tests").
    let mut pos = parse(EP_DOUBLE_CHECK_FEN);

    let ep_move = Move::new(Square::E4, Square::D3, MoveFlag::EnPassant);
    let moves = move_set(&pos);
    assert!(
        moves.contains(&ep_move),
        "EP move e4xd3 must be legal for black (own king h8 not threatened)"
    );

    // Apply EP. `make_move` flips side-to-move to white; `in_check` then
    // queries white's king status — which is what we want.
    let _undo = make_move(&mut pos, ep_move);
    assert_eq!(
        pos.side_to_move(),
        Color::White,
        "post-make_move, side to move must be white"
    );
    assert!(
        in_check(&pos),
        "after black's EP move e4xd3, white must be in (double) check"
    );
}

#[test]
fn ep_causing_single_discovered_check_via_bishop() {
    // Position-3 mini fixture from `docs/research/m1-engine-architecture.md`
    // §6.3: `8/8/1k6/2b5/2pP4/8/5K2/8 b - d3 0 1`. Black pawn on c4 plays
    // c4xd3 e.p., capturing the white pawn on d4. The black bishop on c5
    // had been x-rayed through the d4 pawn at the white king on f2 (along
    // the c5-d4-e3-f2 diagonal). Once d4 vacates, the bishop discovers
    // single check on f2.
    //
    // This is *not* double check (the pawn lands on d3 and attacks c2/e2,
    // not f2), distinguishing it from the EP-causing-double-check fixture.
    let fen = "8/8/1k6/2b5/2pP4/8/5K2/8 b - d3 0 1";
    let mut pos = parse(fen);
    let ep_move = Move::new(Square::C4, Square::D3, MoveFlag::EnPassant);

    let moves = move_set(&pos);
    assert!(
        moves.contains(&ep_move),
        "EP move c4xd3 must be legal for black (own king b6 not threatened)"
    );

    let _undo = make_move(&mut pos, ep_move);
    assert!(
        in_check(&pos),
        "after black's EP move c4xd3, white must be in (single discovered) check"
    );
}

#[test]
fn promotion_plus_check_all_four_underpromotions() {
    // `4k3/1P6/8/8/8/8/K7/8 w - - 0 1`. Pawn on b7; b8 empty; black king on
    // e8; white king on a2. All four pawn-promotion-on-b8 moves must be
    // present (knight-promotion gives check, exercising §6.8 taxonomy).
    let fen = "4k3/1P6/8/8/8/8/K7/8 w - - 0 1";
    let pos = parse(fen);
    let moves = move_set(&pos);

    let expected_flags = [
        MoveFlag::KnightPromo,
        MoveFlag::BishopPromo,
        MoveFlag::RookPromo,
        MoveFlag::QueenPromo,
    ];
    for flag in expected_flags {
        let mv = Move::new(Square::B7, Square::B8, flag);
        assert!(
            moves.contains(&mv),
            "expected b7-b8 with flag {flag:?} in move list ({fen})"
        );
    }
}

#[test]
fn round_trip_make_unmake_for_each_emitted_move() {
    // Curated set: canonical six + four edge fixtures. For each emitted
    // move, make_move then unmake_move must restore the position byte-equal
    // to the original (Position derives PartialEq, which checks every field
    // including the zobrist hash).
    let fens: Vec<&str> = CANONICAL_SIX
        .iter()
        .map(|&(f, _)| f)
        .chain([
            MATE_FEN,
            STALEMATE_FEN,
            EP_HORIZONTAL_PIN_FEN,
            EP_DOUBLE_CHECK_FEN,
        ])
        .collect();

    for fen in fens {
        let original = parse(fen);
        let moves = move_set(&original);

        for mv in moves {
            let mut pos = original;
            let undo = make_move(&mut pos, mv);
            unmake_move(&mut pos, mv, undo);
            assert_eq!(
                pos, original,
                "round-trip failed for move {mv:?} from {fen}"
            );
        }
    }
}

// -----------------------------------------------------------------------
// Property tests.
//
// Strategy: a small set of seed FENs (canonical six + edge fixtures), a
// random-walk length 0..=50, and a deterministic SplitMix64 PRNG seed.
// At run time we resolve the seed FEN to a `Position`, then walk forward
// up to `walk_len` plies by picking a uniformly-random legal move on each
// ply (or stop early on mate / stalemate). The resulting `Position` is
// the property-test input.
//
// We don't use `rand::thread_rng` — proptest must be deterministic so a
// failing case can be replayed. SplitMix64 is implemented inline (no
// external dep): a single state, mix-and-step on each call.
// -----------------------------------------------------------------------

const SEED_FENS: &[&str] = &[
    STARTING_FEN,
    KIWIPETE_FEN,
    POSITION_3_FEN,
    POSITION_4_FEN,
    POSITION_5_FEN,
    POSITION_6_FEN,
    MATE_FEN,
    STALEMATE_FEN,
    EP_HORIZONTAL_PIN_FEN,
    EP_DOUBLE_CHECK_FEN,
];

/// Deterministic SplitMix64 PRNG. Single state, single step. Used to pick
/// random legal moves during the proptest random walk.
#[derive(Copy, Clone)]
struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> SplitMix64 {
        SplitMix64(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Resolve a `(seed_idx, walk_len, rng_seed)` triple into a `Position` by
/// parsing `SEED_FENS[seed_idx]` and walking up to `walk_len` random legal
/// plies. Stops early if a position has no legal moves (mate / stalemate).
fn walk_position(seed_idx: usize, walk_len: usize, rng_seed: u64) -> Position {
    let mut pos = parse(SEED_FENS[seed_idx]);
    let mut rng = SplitMix64::new(rng_seed);
    let mut ml = MoveList::new();
    for _ in 0..walk_len {
        generate_moves(&pos, &mut ml);
        if ml.is_empty() {
            break;
        }
        let pick = ml.as_slice()[(rng.next_u64() as usize) % ml.len()];
        let _undo = make_move(&mut pos, pick);
    }
    pos
}

fn walk_strategy() -> impl Strategy<Value = Position> {
    (0..SEED_FENS.len(), 0usize..=50, any::<u64>())
        .prop_map(|(seed_idx, walk_len, rng_seed)| walk_position(seed_idx, walk_len, rng_seed))
}

proptest! {
    /// Within a single `generate_moves` call, no two emitted moves share
    /// the same encoded bits. Promotion fan-out (four `*Promo` moves on
    /// the same from/to) must produce four *distinct* `Move::bits()`
    /// values because the flag nibble differs.
    #[test]
    fn prop_no_duplicate_moves(pos in walk_strategy()) {
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let mut bits: Vec<u16> = ml.iter().map(|m| m.bits()).collect();
        let len_before = bits.len();
        bits.sort_unstable();
        bits.dedup();
        prop_assert_eq!(
            bits.len(),
            len_before,
            "duplicate emitted move(s) in {} ({:?})",
            pos.to_fen(),
            ml.as_slice(),
        );
    }

    /// Make then unmake every emitted move; the resulting position must be
    /// byte-equal (Position::PartialEq covers every field, including the
    /// incrementally-maintained Zobrist hash).
    #[test]
    fn prop_round_trip_make_unmake(pos in walk_strategy()) {
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        for mv in ml.iter() {
            let mut working = pos;
            let undo = make_move(&mut working, mv);
            unmake_move(&mut working, mv, undo);
            prop_assert_eq!(
                working, pos,
                "round-trip mismatch for {:?} from {}", mv, pos.to_fen(),
            );
        }
    }

    /// Every position emits at most 218 legal moves (Petrović 1964;
    /// integer-programming proof 2025). A regression that emits a 219th
    /// move would either be a `MoveList` capacity bug or an illegal
    /// duplicate; the property catches both.
    #[test]
    fn prop_legal_move_count_within_218(pos in walk_strategy()) {
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        prop_assert!(
            ml.len() <= 218,
            "emitted {} moves (exceeds 218-move ceiling) from {}",
            ml.len(), pos.to_fen(),
        );
    }
}

// -----------------------------------------------------------------------
// Double-check property — hand-built seeds, NOT random walk.
//
// Random walks almost never reach a double-check position, so this test
// is anchored on explicit seeds:
//   - Seed A: rook + knight double check on king e1.
//   - Seed B: bishop + knight double check on king e1.
//   - Seed C: post-EP-double-check (apply e4xd3 from EP_DOUBLE_CHECK_FEN).
//
// For each seed, every emitted move must have `from_square ==
// king_square(side_to_move)` — in double check, only king moves are legal.
// -----------------------------------------------------------------------

#[test]
fn prop_double_check_emits_only_king_moves() {
    // Seed A: rook on e8 + knight on f3 both check king on e1.
    let seed_a = parse("4r2k/8/8/8/8/5n2/8/4K3 w - - 0 1");

    // Seed B: bishop on a5 + knight on c2 both check king on e1.
    let seed_b = parse("4k3/8/8/b7/8/8/2n5/4K3 w - - 0 1");

    // Seed C: parse the EP-double-check FEN and APPLY the EP move at
    // setup time, so the seed used here is the post-EP position with
    // white to move and white in double check.
    let seed_c = {
        let mut pos = parse(EP_DOUBLE_CHECK_FEN);
        let ep = Move::new(Square::E4, Square::D3, MoveFlag::EnPassant);
        let _undo = make_move(&mut pos, ep);
        pos
    };

    for pos in [seed_a, seed_b, seed_c] {
        let king_sq = pos.king_square(pos.side_to_move());
        let moves = move_set(&pos);
        for mv in moves {
            assert_eq!(
                mv.from_square(),
                king_sq,
                "double check must emit king moves only, but got {mv:?} from {}",
                pos.to_fen(),
            );
        }
    }
}
