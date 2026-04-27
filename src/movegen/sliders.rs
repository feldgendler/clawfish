//! Bishop / rook / queen move emission. See `docs/plans/m1.f.md` §10.
//!
//! Unified inner loop with a `kind` switch on the attack lookup. Pinned
//! sliders restrict their destination set to `pin_rays[from]`.

use crate::magic;
use crate::mov::{Move, MoveFlag};
use crate::movegen::masks::MaskInfo;
use crate::movegen::move_list::MoveList;
use crate::piece::{Color, PieceKind};
use crate::position::Position;

pub(crate) fn emit_slider_moves(pos: &Position, us: Color, info: &MaskInfo, out: &mut MoveList) {
    let all_occ = pos.occupied_all();
    let own = pos.occupied(us);
    let them_bb = pos.occupied(us.flip());
    let target_mask = info.capture_mask | info.push_mask;

    for kind in [PieceKind::Bishop, PieceKind::Rook, PieceKind::Queen] {
        for from in pos.pieces_colored(us, kind).iter() {
            let attacks = match kind {
                PieceKind::Bishop => magic::bishop_attacks(from, all_occ),
                PieceKind::Rook => magic::rook_attacks(from, all_occ),
                PieceKind::Queen => magic::queen_attacks(from, all_occ),
                _ => unreachable!(),
            };
            let mut dests = attacks & !own & target_mask;
            if info.pinned.contains(from) {
                dests &= info.pin_rays[from.index() as usize];
            }
            for to in dests.iter() {
                let flag = if them_bb.contains(to) {
                    MoveFlag::Capture
                } else {
                    MoveFlag::Quiet
                };
                out.push(Move::new(from, to, flag));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mov::{Move, MoveFlag};
    use crate::square::Square;

    /// Generate slider moves for the side-to-move of `fen` and return them.
    fn slider_moves_for(fen: &str) -> Vec<Move> {
        let pos = Position::from_fen(fen).expect("test FEN must parse");
        let us = pos.side_to_move();
        let king_sq = pos.king_square(us);
        let info = MaskInfo::compute(&pos, us, king_sq);
        let mut out = MoveList::new();
        emit_slider_moves(&pos, us, &info, &mut out);
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
    fn bishop_quiet_and_capture() {
        // White bishop on d4, black piece (knight) on g7. Diagonal d4 →
        // g7: e5, f6 (quiet), g7 (capture). h8 is BEYOND the captured
        // piece on the same diagonal and must NOT be emitted.
        //
        // Other diagonals from d4: a1-h8 down (c3, b2, a1 — empty), a7-g1
        // diagonal (c5, b6, a7 — empty; e3, f2, g1 — empty). All emitted as
        // quiet so we focus assertion on the e5/f6/g7/h8 ray.
        let fen = "4k3/6n1/8/8/3B4/8/8/4K3 w - - 0 1";
        let moves = slider_moves_for(fen);
        let from_d4 = moves_from(&moves, Square::D4);

        // Verify the e5-f6-g7 diagonal emissions.
        assert!(
            from_d4.contains(&Move::new(Square::D4, Square::E5, MoveFlag::Quiet)),
            "expected quiet Bd4-e5 on the way to g7; got {from_d4:?}"
        );
        assert!(
            from_d4.contains(&Move::new(Square::D4, Square::F6, MoveFlag::Quiet)),
            "expected quiet Bd4-f6 on the way to g7; got {from_d4:?}"
        );
        assert!(
            from_d4.contains(&Move::new(Square::D4, Square::G7, MoveFlag::Capture)),
            "expected capture Bd4xg7; got {from_d4:?}"
        );
        // h8 must not be emitted: the bishop cannot leap over the captured
        // piece.
        assert!(
            !from_d4.iter().any(|m| m.to_square() == Square::H8),
            "h8 must NOT be emitted (beyond captured piece on g7); got {from_d4:?}"
        );
    }

    #[test]
    fn rook_quiet_and_capture() {
        // White rook on a1, black piece (knight) on a5. File a1 → a5: a2,
        // a3, a4 (quiet), a5 (capture). a6/a7/a8 are beyond the captured
        // piece on the same file and must NOT be emitted.
        let fen = "4k3/8/8/n7/8/8/8/R3K3 w - - 0 1";
        let moves = slider_moves_for(fen);
        let from_a1 = moves_from(&moves, Square::A1);

        for sq in [Square::A2, Square::A3, Square::A4] {
            assert!(
                from_a1.contains(&Move::new(Square::A1, sq, MoveFlag::Quiet)),
                "expected quiet Ra1-{sq:?} on the way to a5; got {from_a1:?}"
            );
        }
        assert!(
            from_a1.contains(&Move::new(Square::A1, Square::A5, MoveFlag::Capture)),
            "expected capture Ra1xa5; got {from_a1:?}"
        );
        for sq in [Square::A6, Square::A7, Square::A8] {
            assert!(
                !from_a1.iter().any(|m| m.to_square() == sq),
                "{sq:?} must NOT be emitted (beyond captured piece on a5); got {from_a1:?}"
            );
        }
    }

    #[test]
    fn queen_equals_rook_or_bishop_emission() {
        // Fixed position. Run three FENs sharing geometry: a queen on d4, a
        // rook on d4, a bishop on d4 — same surrounding pieces in each case.
        // Assert: the destination squares emitted by the queen-FEN equal the
        // union of those emitted by the rook-FEN and the bishop-FEN.
        //
        // Surroundings: white king on e1, black king on e8, no other pieces.
        // No checks, no pins, no captures — pure quiet emission.
        let queen_fen = "4k3/8/8/8/3Q4/8/8/4K3 w - - 0 1";
        let rook_fen = "4k3/8/8/8/3R4/8/8/4K3 w - - 0 1";
        let bishop_fen = "4k3/8/8/8/3B4/8/8/4K3 w - - 0 1";

        let queen_moves = slider_moves_for(queen_fen);
        let rook_moves = slider_moves_for(rook_fen);
        let bishop_moves = slider_moves_for(bishop_fen);

        let queen_dests: std::collections::BTreeSet<Square> =
            queen_moves.iter().map(|m| m.to_square()).collect();
        let rook_dests: std::collections::BTreeSet<Square> =
            rook_moves.iter().map(|m| m.to_square()).collect();
        let bishop_dests: std::collections::BTreeSet<Square> =
            bishop_moves.iter().map(|m| m.to_square()).collect();

        let union: std::collections::BTreeSet<Square> =
            rook_dests.union(&bishop_dests).copied().collect();

        assert_eq!(
            queen_dests, union,
            "queen destinations from d4 must equal rook ∪ bishop destinations from d4"
        );

        // Sanity: each set is non-empty (config check).
        assert!(
            !rook_dests.is_empty(),
            "rook on d4 has at least 14 destinations"
        );
        assert!(
            !bishop_dests.is_empty(),
            "bishop on d4 has at least 13 destinations"
        );
    }

    #[test]
    fn slider_pinned_along_pin_ray_emits_along_ray_only() {
        // White king on e1, white bishop on c3, black bishop on a5.
        // The a5-e1 diagonal: a5, b4, c3, d2, e1. White bishop on c3 is
        // pinned. Pinned-along-diagonal bishop may move on the ray:
        //   - to b4 (quiet, between king and pinner — but actually past king)
        //     wait, let's think: the diagonal a5 → e1 goes a5, b4, c3, d2, e1.
        //     The bishop on c3 can move to b4 (toward pinner), capture a5
        //     (the pinner), or move to d2 (toward king, between king and
        //     pinned piece).
        // Total: b4 quiet, d2 quiet, a5 capture = 3 moves.
        // Off-ray destinations (e.g. b2, a1, d4, e5) must be filtered out.
        let fen = "4k3/8/8/b7/8/2B5/8/4K3 w - - 0 1";
        let moves = slider_moves_for(fen);
        let from_c3 = moves_from(&moves, Square::C3);

        assert_eq!(
            from_c3.len(),
            3,
            "pinned bishop on c3 must emit exactly 3 moves (b4, d2, a5xcapture); got {from_c3:?}"
        );
        assert!(
            from_c3.contains(&Move::new(Square::C3, Square::B4, MoveFlag::Quiet)),
            "expected quiet Bc3-b4 on the pin ray; got {from_c3:?}"
        );
        assert!(
            from_c3.contains(&Move::new(Square::C3, Square::D2, MoveFlag::Quiet)),
            "expected quiet Bc3-d2 on the pin ray; got {from_c3:?}"
        );
        assert!(
            from_c3.contains(&Move::new(Square::C3, Square::A5, MoveFlag::Capture)),
            "expected capture Bc3xa5 (the pinner); got {from_c3:?}"
        );

        // Off-diagonal squares the bishop would normally see must NOT be
        // emitted. b2, a1 are on the c3-a1 diagonal (off the pin ray);
        // d4, e5, f6, g7, h8 are on the c3-h8 diagonal (off the pin ray).
        for off in [Square::B2, Square::A1, Square::D4, Square::E5] {
            assert!(
                !from_c3.iter().any(|m| m.to_square() == off),
                "{off:?} is off the pin ray and must NOT be emitted; got {from_c3:?}"
            );
        }
    }

    #[test]
    fn slider_pinned_perpendicular_emits_nothing() {
        // White king on e1, white bishop on e2, black rook on e8 (with black
        // king on h8 — the FEN's third field is castling rights `-` so no
        // need for the rook to be on h8). The bishop is pinned along the
        // e-file. Bishops can't move along files at all (their move
        // directions are pure diagonals), so pinned-perpendicular bishop
        // emits zero moves.
        let fen = "4r2k/8/8/8/8/8/4B3/4K3 w - - 0 1";
        let moves = slider_moves_for(fen);
        let from_e2 = moves_from(&moves, Square::E2);
        assert!(
            from_e2.is_empty(),
            "bishop pinned perpendicular to its move directions emits zero moves; got {from_e2:?}"
        );
    }

    #[test]
    fn slider_blocked_by_own_piece() {
        // White bishop on d4, white pawn on f6. The bishop's d4-h8 diagonal
        // hits the own pawn on f6: f6 is NOT in the bishop's move set
        // (capture own piece is illegal); g7, h8 (beyond) also not.
        let fen = "4k3/8/5P2/8/3B4/8/8/4K3 w - - 0 1";
        let moves = slider_moves_for(fen);
        let from_d4 = moves_from(&moves, Square::D4);

        // e5 should still be emitted (in front of the blocker).
        assert!(
            from_d4.contains(&Move::new(Square::D4, Square::E5, MoveFlag::Quiet)),
            "expected quiet Bd4-e5 (before the f6 blocker); got {from_d4:?}"
        );
        // f6 not in attack set (own piece blocks).
        assert!(
            !from_d4.iter().any(|m| m.to_square() == Square::F6),
            "f6 must NOT be emitted (own pawn there); got {from_d4:?}"
        );
        // Beyond the own-piece blocker.
        for sq in [Square::G7, Square::H8] {
            assert!(
                !from_d4.iter().any(|m| m.to_square() == sq),
                "{sq:?} must NOT be emitted (beyond own-piece blocker on f6); got {from_d4:?}"
            );
        }
    }

    #[test]
    fn slider_blocked_by_opponent_piece_emits_capture() {
        // White bishop on d4, black piece (knight) on f6. Opponent piece on
        // f6 is captured; squares beyond (g7, h8) NOT emitted.
        let fen = "4k3/8/5n2/8/3B4/8/8/4K3 w - - 0 1";
        let moves = slider_moves_for(fen);
        let from_d4 = moves_from(&moves, Square::D4);

        assert!(
            from_d4.contains(&Move::new(Square::D4, Square::E5, MoveFlag::Quiet)),
            "expected quiet Bd4-e5 (before the f6 capture target); got {from_d4:?}"
        );
        assert!(
            from_d4.contains(&Move::new(Square::D4, Square::F6, MoveFlag::Capture)),
            "expected capture Bd4xf6; got {from_d4:?}"
        );
        for sq in [Square::G7, Square::H8] {
            assert!(
                !from_d4.iter().any(|m| m.to_square() == sq),
                "{sq:?} must NOT be emitted (beyond captured piece on f6); got {from_d4:?}"
            );
        }
    }

    #[test]
    fn slider_check_evasion_blocks_slider_check() {
        // White king on e1, black rook on e8 (black king on h8 for FEN
        // validity) gives check along the e-file. White rook on a3 can
        // interpose by moving to e3 (block the check). Verify the block
        // move Ra3-e3 is emitted as a quiet (e3 is empty).
        let fen = "4r2k/8/8/8/8/R7/8/4K3 w - - 0 1";
        let moves = slider_moves_for(fen);
        let from_a3 = moves_from(&moves, Square::A3);

        assert!(
            from_a3.contains(&Move::new(Square::A3, Square::E3, MoveFlag::Quiet)),
            "expected quiet Ra3-e3 to block the e-file check; got {from_a3:?}"
        );

        // Off-block-ray destinations must not be emitted in single check
        // (check evasion restricts non-king moves to capture_mask | push_mask).
        // Specifically, a3-a2 / a3-b3 / a3-h3 etc. are NOT in the push_mask
        // (which is the e-file squares between king and checker) and NOT in
        // the capture_mask (the rook on e8 — only e8 itself is a capture).
        for sq in [Square::A2, Square::B3, Square::H3, Square::A4] {
            assert!(
                !from_a3.iter().any(|m| m.to_square() == sq),
                "{sq:?} not on push_mask|capture_mask under single check; \
                 must NOT be emitted; got {from_a3:?}"
            );
        }
    }
}
