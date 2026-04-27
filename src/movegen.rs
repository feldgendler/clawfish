//! Legal move generation — the engine's sole supported move enumeration
//! surface. Per ADR-0007 (`docs/decisions/0007-legal-direct-movegen.md`).
//!
//! ## Public API
//!
//! ```ignore
//! use chess::{Position, MoveList, generate_moves, in_check};
//!
//! let pos = Position::starting_position();
//! let mut moves = MoveList::new();
//! generate_moves(&pos, &mut moves);
//! assert_eq!(moves.len(), 20);    // 16 pawn moves + 4 knight moves
//! ```
//!
//! `generate_moves` emits the **legal** move set: every move passes the
//! "leaves own king not in check" test by construction (mask-based
//! filtering plus check-evasion specialization). No post-emission legality
//! filter is required.
//!
//! See `docs/plans/m1.f.md` for the full design.

pub mod masks;
pub mod move_list;

mod king;
mod knight;
mod pawn;
mod sliders;

pub use move_list::MoveList;

use crate::position::Position;

/// Emit every legal move in `pos` into `out`.
///
/// `out` is cleared first; on return, `out.as_slice()` is the legal move
/// set. The order is: king moves, then (when not in double check) pawn,
/// knight, slider moves. The exact within-piece-kind order is unspecified
/// and may shift across implementation revisions; do not rely on it.
pub fn generate_moves(pos: &Position, out: &mut MoveList) {
    out.clear();
    let us = pos.side_to_move();
    let king_sq = pos.king_square(us);
    let info = masks::MaskInfo::compute(pos, us, king_sq);

    king::emit_king_moves(pos, us, king_sq, &info, out);
    if info.checkers.count() <= 1 {
        pawn::emit_pawn_moves(pos, us, &info, out);
        knight::emit_knight_moves(pos, us, &info, out);
        sliders::emit_slider_moves(pos, us, &info, out);
    }
}

/// Whether the side-to-move is currently in check.
pub fn in_check(pos: &Position) -> bool {
    let us = pos.side_to_move();
    let king_sq = pos.king_square(us);
    !masks::checkers_of(pos, us, king_sq, pos.occupied_all()).is_empty()
}

#[cfg(test)]
mod tests {
    //! Crate-private property tests for `generate_moves`.
    //!
    //! Property tests that need access to `checkers_of` (which is
    //! `pub(crate)`) live here, not in `tests/movegen.rs`. The integration
    //! test crate cannot see crate-private symbols.
    //!
    //! Tests will be RED until the M1.F implementation pass lands; that's
    //! the project's TDD posture.

    use super::*;
    use crate::mov::Move;
    use crate::movegen::masks::checkers_of;
    use crate::piece::Color;

    use proptest::prelude::*;

    /// Edge-class seed FENs from `docs/research/m1-engine-architecture.md`
    /// §6 plus the canonical 6 perft positions. The strategy below walks
    /// 0..50 random legal plies from one of these seeds, returning the
    /// reached `Position`.
    const SEED_FENS: &[&str] = &[
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

    /// Tiny SplitMix64 — deterministic PRNG. Used to drive a random walk
    /// from a seed FEN through legal moves.
    fn next_u64(state: &mut u64) -> u64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Random-walk a Position from a seed FEN. Picks legal moves from the
    /// `generate_moves` output. Stops early on mate/stalemate.
    fn walk(seed: &str, walk_len: usize, rng_seed: u64) -> Position {
        let mut pos = Position::from_fen(seed).expect("seed FEN must parse");
        let mut state = rng_seed.wrapping_add(1);
        for _ in 0..walk_len {
            let mut ml = MoveList::new();
            generate_moves(&pos, &mut ml);
            if ml.is_empty() {
                break;
            }
            let pick = (next_u64(&mut state) as usize) % ml.len();
            let mv = ml.as_slice()[pick];
            pos.make_move(mv);
        }
        pos
    }

    fn arb_position() -> impl Strategy<Value = Position> {
        (0..SEED_FENS.len(), 0usize..50, any::<u64>())
            .prop_map(|(idx, walk_len, rng_seed)| walk(SEED_FENS[idx], walk_len, rng_seed))
    }

    proptest! {
        /// Per ADR-0007 §"Why legal-direct": `generate_moves` emits the
        /// legal move set — every emitted move, after `make_move`, leaves
        /// the mover's king NOT in check. Implemented via crate-private
        /// `checkers_of` against the pre-move side and post-move king
        /// square (which moves only when the move itself is a king move).
        #[test]
        fn prop_no_legal_move_leaves_us_in_check(pos in arb_position()) {
            let mut ml = MoveList::new();
            generate_moves(&pos, &mut ml);
            for mv in ml.iter() {
                let us = pos.side_to_move();
                let mut p = pos;
                let _undo = p.make_move(mv);
                let king_sq = p.king_square(us);
                let chk = checkers_of(&p, us, king_sq, p.occupied_all());
                prop_assert!(
                    chk.is_empty(),
                    "move {:?} from {} left {:?} king on {:?} in check from {:?}",
                    mv, pos.to_fen(), us, king_sq, chk,
                );
            }
        }
    }

    /// Hand-built post-EP-double-check assertion that the integration
    /// suite couldn't make (count == 2) because `checkers_of` is
    /// `pub(crate)`. This pins the §6.3 taxonomy point exactly.
    #[test]
    fn ep_double_check_post_position_has_two_checkers() {
        // Parse the parent fixture, locate the EP move (e4xd3 e.p.),
        // apply it, and assert the post-move position has two distinct
        // checkers on the white king on e2 (rook on e-file via
        // discovered check, plus the moving black pawn now on d3
        // attacking e2 diagonally).
        use crate::mov::MoveFlag;
        use crate::square::Square;

        let mut pos = Position::from_fen("4r2k/8/8/8/3Pp3/8/4K3/8 b - d3 0 1").expect("FEN parses");
        let ep = Move::new(Square::E4, Square::D3, MoveFlag::EnPassant);
        pos.make_move(ep);

        let king_sq = pos.king_square(Color::White);
        let chk = checkers_of(&pos, Color::White, king_sq, pos.occupied_all());
        assert_eq!(
            chk.count(),
            2,
            "EP capture must deliver double check (rook discovery + moving pawn); \
             got checker bitboard {chk:?}"
        );
    }
}
