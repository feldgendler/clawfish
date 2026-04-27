//! Knight move emission. See `docs/plans/m1.f.md` §9.
//!
//! Pinned knights emit nothing — knight L-jumps cannot lie along any pin
//! ray, so a pinned knight has no legal move. Non-pinned knights filter
//! their attack set through `capture_mask | push_mask`.

use crate::mov::{Move, MoveFlag};
use crate::movegen::masks::{self, MaskInfo};
use crate::movegen::move_list::MoveList;
use crate::piece::{Color, PieceKind};
use crate::position::Position;

pub(crate) fn emit_knight_moves(pos: &Position, us: Color, info: &MaskInfo, out: &mut MoveList) {
    let them = us.flip();
    let own_occ = pos.occupied(us);
    let enemy_occ = pos.occupied(them);
    let evasion_mask = info.capture_mask | info.push_mask;

    for from in pos.pieces_colored(us, PieceKind::Knight).iter() {
        // Pinned knights have no legal move: a knight L-jump never lies along
        // the king-pinner ray.
        if info.pinned.contains(from) {
            continue;
        }
        let targets = masks::knight_attacks(from) & !own_occ & evasion_mask;
        for to in targets.iter() {
            let flag = if enemy_occ.contains(to) {
                MoveFlag::Capture
            } else {
                MoveFlag::Quiet
            };
            out.push(Move::new(from, to, flag));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mov::{Move, MoveFlag};
    use crate::square::Square;

    /// Generate knight moves for the side-to-move of `fen` and return them.
    /// Tests are red until `MaskInfo::compute` and `emit_knight_moves` land.
    fn knight_moves_for(fen: &str) -> Vec<Move> {
        let pos = Position::from_fen(fen).expect("test FEN must parse");
        let us = pos.side_to_move();
        let king_sq = pos.king_square(us);
        let info = MaskInfo::compute(&pos, us, king_sq);
        let mut out = MoveList::new();
        emit_knight_moves(&pos, us, &info, &mut out);
        out.iter().collect()
    }

    /// Filter a move list down to moves whose `from` square equals `sq`.
    fn moves_from(moves: &[Move], sq: Square) -> Vec<Move> {
        moves
            .iter()
            .copied()
            .filter(|m| m.from_square() == sq)
            .collect()
    }

    #[test]
    fn knight_moves_starting_position_white() {
        let moves = knight_moves_for(Position::STARTING_FEN);
        assert_eq!(
            moves.len(),
            4,
            "starting position must emit exactly 4 knight moves; got {moves:?}"
        );

        // Expected knight moves: Nb1c3, Nb1a3, Ng1f3, Ng1h3, all quiet.
        let expected = [
            Move::new(Square::B1, Square::C3, MoveFlag::Quiet),
            Move::new(Square::B1, Square::A3, MoveFlag::Quiet),
            Move::new(Square::G1, Square::F3, MoveFlag::Quiet),
            Move::new(Square::G1, Square::H3, MoveFlag::Quiet),
        ];
        for m in &expected {
            assert!(
                moves.contains(m),
                "expected starting-position knight move {m:?} missing from {moves:?}",
            );
        }
    }

    #[test]
    fn knight_quiet_and_capture_distinction() {
        // White knight on d4. Own pawns occupy two of its eight L-squares
        // (b3 and f3); two opponent pieces sit on two more (e6 and c6).
        // Remaining four targets (b5, c2, e2, f5) are empty.
        //
        // Expectations:
        //   own pawns on b3, f3 → filtered out (no move emitted)
        //   opponent pieces on e6, c6 → emitted as Capture
        //   empty squares b5, c2, e2, f5 → emitted as Quiet (4 quiets)
        // Total: 6 moves.
        let fen = "4k3/8/2p1p3/8/3N4/1P3P2/8/4K3 w - - 0 1";
        let moves = knight_moves_for(fen);
        let from_d4 = moves_from(&moves, Square::D4);
        assert_eq!(
            from_d4.len(),
            6,
            "knight on d4 with two own + two enemy in pattern emits 6 moves; got {from_d4:?}"
        );

        let captures: Vec<Move> = from_d4.iter().copied().filter(|m| m.is_capture()).collect();
        assert_eq!(
            captures.len(),
            2,
            "expected exactly 2 capture moves; got {captures:?}"
        );
        assert!(
            captures.contains(&Move::new(Square::D4, Square::E6, MoveFlag::Capture)),
            "expected Nd4xe6 capture; got {captures:?}"
        );
        assert!(
            captures.contains(&Move::new(Square::D4, Square::C6, MoveFlag::Capture)),
            "expected Nd4xc6 capture; got {captures:?}"
        );

        // None of the own-piece squares should appear as destinations.
        for own in [Square::B3, Square::F3] {
            assert!(
                !from_d4.iter().any(|m| m.to_square() == own),
                "own piece on {own:?} must filter the knight move; got {from_d4:?}"
            );
        }
    }

    #[test]
    fn knight_pinned_emits_nothing() {
        // White king on e1, white knight on e4, black rook on e8: knight
        // pinned along the e-file. A pinned knight has no legal move (no
        // L-jump aligns with any pin ray).
        let fen = "4r2k/8/8/8/4N3/8/8/4K3 w - - 0 1";
        let moves = knight_moves_for(fen);
        assert!(
            moves.is_empty(),
            "pinned knight must emit zero moves; got {moves:?}"
        );
    }

    #[test]
    fn knight_check_evasion_can_capture_checker() {
        // White king on e1; black knight on f3 gives check (Nf3 attacks e1).
        // White's own knight on g1 attacks f3 (only one of the two starting-
        // square knights does — the b1 knight is too far). The legal evasion
        // for that knight is the capture-the-checker move Ng1xf3.
        //
        // Note: white king moves are not in scope for this test (handled by
        // king.rs); we assert only that the knight emits the capture-the-
        // checker move and no quiet moves.
        let fen = "4k3/8/8/8/8/5n2/8/1N2K1N1 w - - 0 1";
        let moves = knight_moves_for(fen);

        // The g1 knight should emit exactly one move: Ng1xf3 capture.
        let from_g1 = moves_from(&moves, Square::G1);
        assert_eq!(
            from_g1.len(),
            1,
            "g1 knight in single check must emit only the capture-the-checker move; got {from_g1:?}"
        );
        assert_eq!(
            from_g1[0],
            Move::new(Square::G1, Square::F3, MoveFlag::Capture),
            "g1 knight evasion must be Ng1xf3 capture; got {:?}",
            from_g1[0]
        );

        // The b1 knight does not attack f3 and cannot block a knight check;
        // it must emit zero moves.
        let from_b1 = moves_from(&moves, Square::B1);
        assert!(
            from_b1.is_empty(),
            "b1 knight cannot evade the f3 knight check; got {from_b1:?}"
        );
    }

    #[test]
    fn knight_check_evasion_cannot_block_knight_check() {
        // White king on e1; black knight on f3 gives check.
        // White's own knight on c3 does NOT attack f3 (c3 → f3 is not an
        // L-jump). It also cannot "block" a knight check (knight checks are
        // unblockable: there are no squares between attacker and target).
        //
        // Therefore the c3 knight emits zero moves.
        let fen = "4k3/8/8/8/8/2N2n2/8/4K3 w - - 0 1";
        let moves = knight_moves_for(fen);
        let from_c3 = moves_from(&moves, Square::C3);
        assert!(
            from_c3.is_empty(),
            "c3 knight cannot capture or block the f3 knight check; got {from_c3:?}"
        );
    }
}
