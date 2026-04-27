//! Pawn move emission. See `docs/plans/m1.f.md` §8.
//!
//! Eight sub-cases, color-symmetric:
//! - Single push (with promotion-rank fanout to 4 `*Promo` flags).
//! - Double push (only from starting rank; never a promotion).
//! - Capture left / right (with promotion-rank fanout to 4 `*PromoCapture` flags).
//! - En passant — gated on `pos.ep_target()`, with the post-EP horizontal-pin
//!   filter applied at emission time (Position-3 trap).

use crate::bitboard::Bitboard;
use crate::magic;
use crate::mov::{Move, MoveFlag};
use crate::movegen::masks::{MaskInfo, pawn_attacks_from};
use crate::movegen::move_list::MoveList;
use crate::piece::{Color, PieceKind};
use crate::position::Position;
use crate::square::Square;

pub(crate) fn emit_pawn_moves(pos: &Position, us: Color, info: &MaskInfo, out: &mut MoveList) {
    let them = us.flip();
    let all_occ = pos.occupied_all();
    let empty = !all_occ;
    let enemy = pos.occupied(them);
    let pawns = pos.pieces_colored(us, PieceKind::Pawn);
    let king_sq = pos.king_square(us);

    // Per-color geometry constants.
    let (start_rank, promo_rank, push_dr) = match us {
        Color::White => (1u8, 7u8, 1i8),
        Color::Black => (6u8, 0u8, -1i8),
    };

    for from in pawns.iter() {
        let from_file = from.file();
        let from_rank = from.rank();
        let pinned = info.pinned.contains(from);
        let pin_ray = if pinned {
            info.pin_rays[from.index() as usize]
        } else {
            Bitboard::FULL
        };

        // Single push.
        let push_rank = (from_rank as i8 + push_dr) as u8;
        if let Some(push_sq) = Square::from_file_rank(from_file, push_rank) {
            let push_bb = Bitboard::from_square(push_sq);
            if (empty & push_bb).any() {
                // Single push allowed (target empty).
                if (info.push_mask & push_bb & pin_ray).any() {
                    if push_rank == promo_rank {
                        out.push(Move::new(from, push_sq, MoveFlag::KnightPromo));
                        out.push(Move::new(from, push_sq, MoveFlag::BishopPromo));
                        out.push(Move::new(from, push_sq, MoveFlag::RookPromo));
                        out.push(Move::new(from, push_sq, MoveFlag::QueenPromo));
                    } else {
                        out.push(Move::new(from, push_sq, MoveFlag::Quiet));
                    }
                }

                // Double push (only from starting rank, never a promotion).
                if from_rank == start_rank {
                    let dbl_rank = (from_rank as i8 + 2 * push_dr) as u8;
                    if let Some(dbl_sq) = Square::from_file_rank(from_file, dbl_rank) {
                        let dbl_bb = Bitboard::from_square(dbl_sq);
                        if (empty & dbl_bb).any() && (info.push_mask & dbl_bb & pin_ray).any() {
                            out.push(Move::new(from, dbl_sq, MoveFlag::DoublePush));
                        }
                    }
                }
            }
        }

        // Diagonal captures (left + right) via attack table.
        let cap_targets = pawn_attacks_from(from, us) & enemy;
        for to in cap_targets.iter() {
            let to_bb = Bitboard::from_square(to);
            if (info.capture_mask & to_bb & pin_ray).is_empty() {
                continue;
            }
            if to.rank() == promo_rank {
                out.push(Move::new(from, to, MoveFlag::KnightPromoCapture));
                out.push(Move::new(from, to, MoveFlag::BishopPromoCapture));
                out.push(Move::new(from, to, MoveFlag::RookPromoCapture));
                out.push(Move::new(from, to, MoveFlag::QueenPromoCapture));
            } else {
                out.push(Move::new(from, to, MoveFlag::Capture));
            }
        }
    }

    // En passant — handled separately: attacker scan + post-EP horizontal-pin
    // filter.
    if let Some(ep_sq) = pos.ep_target() {
        // The EP-captured pawn sits on the rank one step *back* from `ep_sq`
        // from our perspective.
        let captured_rank = (ep_sq.rank() as i8 - push_dr) as u8;
        let captured_sq = Square::from_file_rank(ep_sq.file(), captured_rank)
            .expect("EP captured-pawn square is on-board by construction");
        let ep_bb = Bitboard::from_square(ep_sq);
        let captured_bb = Bitboard::from_square(captured_sq);

        // Find own pawns whose attack pattern includes `ep_sq`.
        // Use the mirror-symmetry trick: opponent-color pawn attacks from
        // `ep_sq` lands on exactly the squares from which our pawns can
        // attack `ep_sq`.
        let attackers = pawn_attacks_from(ep_sq, us.flip()) & pawns;

        // Mask gating for EP under check: allow if either the EP-captured
        // pawn is in capture_mask (the checker IS the captured pawn) OR
        // the EP target square is in push_mask (rare discovered-check
        // block scenario).
        let mask_ok = (info.capture_mask & captured_bb).any() || (info.push_mask & ep_bb).any();
        if !mask_ok {
            return;
        }

        for from in attackers.iter() {
            // Standard pin filter applies to EP destination.
            if info.pinned.contains(from) {
                let pr = info.pin_rays[from.index() as usize];
                if (pr & ep_bb).is_empty() {
                    continue;
                }
            }

            // Post-EP-occupancy horizontal-pin check (Position-3 trap):
            // remove our pawn from `from`, remove opp pawn from
            // captured_sq, add our pawn at ep_sq, and verify our king
            // isn't attacked by an opposing rook/queen along the resulting
            // ray.
            let from_bb = Bitboard::from_square(from);
            let post_ep_occ = (all_occ ^ from_bb ^ captured_bb) | ep_bb;
            let opp_rq = pos.pieces_colored(them, PieceKind::Rook)
                | pos.pieces_colored(them, PieceKind::Queen);
            if (magic::rook_attacks(king_sq, post_ep_occ) & opp_rq).any() {
                continue;
            }
            // Also guard against discovered diagonal checks — same trick
            // as horizontal but for bishops/queens. Removing our pawn and
            // the opp pawn could expose a diagonal attacker.
            //
            // This guard exceeds plan §8 (which calls only for the
            // horizontal-rook check, motivated by Position-3). The
            // diagonal case is symmetric: the standard pin filter doesn't
            // catch it because our capturer pawn isn't on the
            // king-pinner diagonal — only the post-EP-occupancy check
            // does. See `pawn_ep_diagonal_pin_filtered` in tests below.
            let opp_bq = pos.pieces_colored(them, PieceKind::Bishop)
                | pos.pieces_colored(them, PieceKind::Queen);
            if (magic::bishop_attacks(king_sq, post_ep_occ) & opp_bq).any() {
                continue;
            }

            out.push(Move::new(from, ep_sq, MoveFlag::EnPassant));
        }
    }
}

#[cfg(test)]
mod tests {
    //! Pawn-emission unit tests. Each test constructs a `Position` via
    //! `Position::from_fen`, computes a `MaskInfo`, calls `emit_pawn_moves`,
    //! and inspects the resulting `MoveList`.
    //!
    //! All tests are RED until the M1.F implementation pass lands (the
    //! emitter and `MaskInfo::compute` are both `unimplemented!()`). That
    //! is the project's TDD discipline.
    //!
    //! FEN choices: castling rights are kept as `-` everywhere unless the
    //! test specifically exercises castling (none of these do). Both kings
    //! are always present and located so as to satisfy the FEN parser's
    //! structural checks but stay clear of the squares the test cares
    //! about.
    use super::*;
    use crate::mov::{Move, MoveFlag};
    use crate::movegen::masks::MaskInfo;
    use crate::movegen::move_list::MoveList;
    use crate::piece::{Piece, PieceKind};
    use crate::position::Position;
    use crate::square::Square;

    /// Drive the emitter for `pos` and return the resulting `MoveList`.
    /// Centralizes the boilerplate so each test reads as a fixture + an
    /// assertion against the emitted slice.
    fn emit(pos: &Position) -> MoveList {
        let us = pos.side_to_move();
        let king_sq = pos.king_square(us);
        let info = MaskInfo::compute(pos, us, king_sq);
        let mut moves = MoveList::new();
        emit_pawn_moves(pos, us, &info, &mut moves);
        moves
    }

    /// Filter to only those moves whose `from_square` holds an own-color
    /// pawn in `pos`. Used by the starting-position test where the public
    /// API would return the full mixed move list, but `emit_pawn_moves`
    /// already only produces pawn moves — so this is a defensive filter
    /// in case the implementation later widens scope, and a self-doc anchor
    /// in the tests.
    fn pawn_moves_only(pos: &Position, moves: &[Move]) -> Vec<Move> {
        let us = pos.side_to_move();
        moves
            .iter()
            .copied()
            .filter(|mv| {
                let from = mv.from_square();
                pos.piece_at(from) == Some(Piece::new(us, PieceKind::Pawn))
            })
            .collect()
    }

    #[test]
    fn pawn_pushes_starting_position_white() {
        // Standard initial position. Each of the 8 white pawns has a
        // single-push (rank 2 → rank 3) and a double-push (rank 2 → rank 4)
        // available — 16 pawn moves total. No captures, no EP, no
        // promotions.
        let pos = Position::starting_position();
        let moves = emit(&pos);
        let pawn_only = pawn_moves_only(&pos, moves.as_slice());
        assert_eq!(
            pawn_only.len(),
            16,
            "starting position should emit 16 pawn moves (8 single + 8 double pushes)"
        );

        // Spot-check a representative single + double push.
        let single = Move::new(Square::E2, Square::E3, MoveFlag::Quiet);
        let double = Move::new(Square::E2, Square::E4, MoveFlag::DoublePush);
        assert!(
            pawn_only.contains(&single),
            "expected e2e3 single push in starting position"
        );
        assert!(
            pawn_only.contains(&double),
            "expected e2e4 double push in starting position"
        );
    }

    #[test]
    fn pawn_pushes_starting_position_black() {
        // Symmetric to `pawn_pushes_starting_position_white`. The
        // starting position with white-to-move flipped to black-to-move
        // (FEN field 2 = `b`) gives black 16 pawn moves: 8 single-pushes
        // (rank 7 → rank 6) plus 8 double-pushes (rank 7 → rank 5).
        //
        // Pins the per-color `push_dr = -1` for black: a `delete -`
        // mutation on the `Color::Black => (..., -1i8)` tuple element
        // would flip black pushes to advance UP (rank 7 → rank 8) which
        // is occupied by black pieces — black would emit zero pushes.
        let pos =
            Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR b KQkq - 0 1").unwrap();
        let moves = emit(&pos);
        let pawn_only = pawn_moves_only(&pos, moves.as_slice());
        assert_eq!(
            pawn_only.len(),
            16,
            "starting position (black to move) should emit 16 pawn moves; got {}",
            pawn_only.len()
        );

        // Spot-check a representative single + double push for black.
        let single = Move::new(Square::E7, Square::E6, MoveFlag::Quiet);
        let double = Move::new(Square::E7, Square::E5, MoveFlag::DoublePush);
        assert!(
            pawn_only.contains(&single),
            "expected e7e6 single push for black"
        );
        assert!(
            pawn_only.contains(&double),
            "expected e7e5 double push for black"
        );
    }

    #[test]
    fn pawn_pushes_blocked_by_own_piece() {
        // White pawn on a2, white knight on a3, kings out of the way.
        // The pawn cannot single-push (a3 occupied by own piece) and
        // therefore cannot double-push either.
        let pos = Position::from_fen("4k3/8/8/8/8/N7/P7/4K3 w - - 0 1").unwrap();
        let moves = emit(&pos);
        let pawn_only = pawn_moves_only(&pos, moves.as_slice());
        // No a-pawn moves.
        for mv in &pawn_only {
            assert_ne!(
                mv.from_square(),
                Square::A2,
                "a2 pawn must not emit any move when blocked by own knight on a3: {mv:?}"
            );
        }
    }

    #[test]
    fn pawn_pushes_blocked_by_opponent_piece() {
        // White pawn on a2, BLACK knight on a3. a3 is straight ahead, not
        // a diagonal capture target, so no capture either.
        let pos = Position::from_fen("4k3/8/8/8/8/n7/P7/4K3 w - - 0 1").unwrap();
        let moves = emit(&pos);
        let pawn_only = pawn_moves_only(&pos, moves.as_slice());
        for mv in &pawn_only {
            assert_ne!(
                mv.from_square(),
                Square::A2,
                "a2 pawn must not emit any move when blocked by opponent knight on a3 \
                 (a3 is straight ahead, not a capture target): {mv:?}"
            );
        }
    }

    #[test]
    fn pawn_double_push_only_from_starting_rank() {
        // White pawn on a3 (NOT a2). Only single push to a4 is legal; no
        // double push allowed since the pawn isn't on its starting rank.
        let pos = Position::from_fen("4k3/8/8/8/8/P7/8/4K3 w - - 0 1").unwrap();
        let moves = emit(&pos);
        let pawn_only = pawn_moves_only(&pos, moves.as_slice());
        let single = Move::new(Square::A3, Square::A4, MoveFlag::Quiet);
        assert!(
            pawn_only.contains(&single),
            "pawn on a3 should single-push to a4"
        );
        // No double-push to a5.
        for mv in &pawn_only {
            if mv.from_square() == Square::A3 {
                assert_ne!(
                    mv.flag(),
                    MoveFlag::DoublePush,
                    "pawn on a3 must not emit a DoublePush flag: {mv:?}"
                );
                assert_ne!(
                    mv.to_square(),
                    Square::A5,
                    "pawn on a3 must not reach a5 (double-push from non-starting rank): {mv:?}"
                );
            }
        }
    }

    #[test]
    fn pawn_double_push_blocked_by_intermediate_square() {
        // White pawn on a2, opponent knight on a3. Single-push blocked
        // (a3 occupied), so double-push must not be emitted either even
        // though a4 itself is empty.
        let pos = Position::from_fen("4k3/8/8/8/8/n7/P7/4K3 w - - 0 1").unwrap();
        let moves = emit(&pos);
        let pawn_only = pawn_moves_only(&pos, moves.as_slice());
        for mv in &pawn_only {
            if mv.from_square() == Square::A2 {
                assert_ne!(
                    mv.to_square(),
                    Square::A3,
                    "no single push when a3 occupied: {mv:?}"
                );
                assert_ne!(
                    mv.to_square(),
                    Square::A4,
                    "no double push when a3 (intermediate) occupied: {mv:?}"
                );
            }
        }
    }

    #[test]
    fn pawn_capture_left_right() {
        // White pawn on d4 with black knights on c5 (capture-left) and
        // e5 (capture-right). Both diagonal captures must emit. Kings
        // tucked into corners so they don't interfere.
        let pos = Position::from_fen("4k3/8/8/2n1n3/3P4/8/8/4K3 w - - 0 1").unwrap();
        let moves = emit(&pos);
        let pawn_only = pawn_moves_only(&pos, moves.as_slice());
        let cap_left = Move::new(Square::D4, Square::C5, MoveFlag::Capture);
        let cap_right = Move::new(Square::D4, Square::E5, MoveFlag::Capture);
        assert!(
            pawn_only.contains(&cap_left),
            "expected d4xc5 capture-left, got: {pawn_only:?}"
        );
        assert!(
            pawn_only.contains(&cap_right),
            "expected d4xe5 capture-right, got: {pawn_only:?}"
        );
    }

    #[test]
    fn pawn_capture_corner_files() {
        // White pawn on a2. There is no "a-file capture-left" target; the
        // file-A pawn only has a capture-right diagonal (to b3). Place a
        // black knight on b3 and assert exactly the b3 capture is emitted
        // (and no off-board "h3" or wrap-around target).
        let pos = Position::from_fen("4k3/8/8/8/8/1n6/P7/4K3 w - - 0 1").unwrap();
        let moves = emit(&pos);
        let pawn_only = pawn_moves_only(&pos, moves.as_slice());
        let cap_right = Move::new(Square::A2, Square::B3, MoveFlag::Capture);
        assert!(
            pawn_only.contains(&cap_right),
            "expected a2xb3 capture-right, got: {pawn_only:?}"
        );
        // No capture from a2 with destination off the a- or b-file (no
        // wrap-around to the h-file).
        for mv in &pawn_only {
            if mv.from_square() == Square::A2 && mv.flag() == MoveFlag::Capture {
                let to_file = mv.to_square().file();
                assert!(
                    to_file == 1,
                    "a2 capture destination file must be b (file 1), got file {to_file} \
                     in {mv:?}"
                );
            }
        }
    }

    #[test]
    fn pawn_promotion_emits_four_flags_per_target() {
        // White pawn on a7 with empty a8. Four promotion flags must be
        // emitted (Knight/Bishop/Rook/Queen).  Kings out of the way:
        // white king on h1, black king on h8.
        let pos = Position::from_fen("7k/P7/8/8/8/8/8/7K w - - 0 1").unwrap();
        let moves = emit(&pos);
        let pawn_only = pawn_moves_only(&pos, moves.as_slice());

        for flag in [
            MoveFlag::KnightPromo,
            MoveFlag::BishopPromo,
            MoveFlag::RookPromo,
            MoveFlag::QueenPromo,
        ] {
            let expected = Move::new(Square::A7, Square::A8, flag);
            assert!(
                pawn_only.contains(&expected),
                "expected promotion flag {flag:?} for a7a8, got: {pawn_only:?}"
            );
        }

        // And no plain-Quiet move to a8 (promotion replaces the plain
        // push on the promotion rank).
        let plain = Move::new(Square::A7, Square::A8, MoveFlag::Quiet);
        assert!(
            !pawn_only.contains(&plain),
            "a7a8 must NOT be emitted as plain Quiet (promotion fans out): \
             {pawn_only:?}"
        );
    }

    #[test]
    fn pawn_promotion_capture_emits_four_flags_per_target() {
        // White pawn on b7. a8 has a black knight (capture-promotion
        // target); b8 is empty (push-promotion target); c8 is empty.
        // Expect four `*Promo` push-promotions on b7→b8 plus four
        // `*PromoCapture` on b7→a8 = 8 pawn moves.
        let pos = Position::from_fen("n6k/1P6/8/8/8/8/8/7K w - - 0 1").unwrap();
        let moves = emit(&pos);
        let pawn_only = pawn_moves_only(&pos, moves.as_slice());

        for flag in [
            MoveFlag::KnightPromo,
            MoveFlag::BishopPromo,
            MoveFlag::RookPromo,
            MoveFlag::QueenPromo,
        ] {
            let push = Move::new(Square::B7, Square::B8, flag);
            assert!(
                pawn_only.contains(&push),
                "expected push-promotion {flag:?} for b7b8, got: {pawn_only:?}"
            );
        }
        for flag in [
            MoveFlag::KnightPromoCapture,
            MoveFlag::BishopPromoCapture,
            MoveFlag::RookPromoCapture,
            MoveFlag::QueenPromoCapture,
        ] {
            let cap = Move::new(Square::B7, Square::A8, flag);
            assert!(
                pawn_only.contains(&cap),
                "expected capture-promotion {flag:?} for b7xa8, got: {pawn_only:?}"
            );
        }
    }

    #[test]
    fn pawn_ep_when_target_is_set() {
        // Minimal EP-available position: white pawn on e5, black pawn on
        // d5 (just double-pushed), EP target d6, white to move. The EP
        // capture e5xd6 must be emitted with the `EnPassant` flag.
        let pos = Position::from_fen("4k3/8/8/3pP3/8/8/8/4K3 w - d6 0 1").unwrap();
        let moves = emit(&pos);
        let pawn_only = pawn_moves_only(&pos, moves.as_slice());
        let ep = Move::new(Square::E5, Square::D6, MoveFlag::EnPassant);
        assert!(
            pawn_only.contains(&ep),
            "expected EP move e5xd6 with EnPassant flag, got: {pawn_only:?}"
        );
    }

    #[test]
    fn pawn_ep_horizontal_pin_filtered() {
        // Position-3-style trap: white king on a5, white pawn on c5,
        // black pawn on d5 (just double-pushed; EP target d6), black
        // rook on h5. The EP capture c5xd6 simultaneously vacates c5 and
        // d5, exposing the white king on a5 to the rook on h5 along
        // rank 5. The EP move MUST NOT be emitted.
        let pos = Position::from_fen("3k4/8/8/K1Pp3r/8/8/8/8 w - d6 0 2").unwrap();
        let moves = emit(&pos);
        let pawn_only = pawn_moves_only(&pos, moves.as_slice());
        let ep = Move::new(Square::C5, Square::D6, MoveFlag::EnPassant);
        assert!(
            !pawn_only.contains(&ep),
            "EP c5xd6 must NOT be emitted under horizontal-pin trap, but it was: \
             {pawn_only:?}"
        );
    }

    #[test]
    fn pawn_ep_when_pinned_along_capture_ray() {
        // A pawn pinned along the EP-capture diagonal can still EP — its
        // move stays on the pin ray.
        //
        // Setup: white king on a1, white pawn on d4, black bishop on h8.
        // The d4 pawn is pinned along the a1-h8 diagonal (king-pawn-pinner
        // ray). Black pawn on e4 (just double-pushed), EP target e3? No —
        // EP target reflects black's most recent double push, so we need
        // it from black's perspective. Re-frame:
        //
        // Use BLACK to move. Black king on h8, black pawn on d4 (pinned
        // by white bishop on a1 through the a1-h8 diagonal), white king
        // on h1 out of the way. White pawn just moved e2-e4 (double
        // push), EP target e3. Black's d4xe3 EP capture stays on the
        // a1-h8 diagonal? d4 is on the a1-h8 diagonal; e3 is also on
        // a1-h8 diagonal (a1, b2, c3, d4, e5, f6, g7, h8 — wait, e3 is
        // NOT on a1-h8).
        //
        // Geometry recheck: the pawn-EP-capture move for a black pawn on
        // d4 capturing toward e3 is along the a8-h1 ANTI-diagonal? No,
        // d4→e3 is south-east one square; the line through d4 and e3 is
        // the a7-g1 anti-diagonal direction (slope -1). Pinner candidate:
        // a white bishop at the far end of that anti-diagonal in the
        // direction *away* from the king, with the king at the opposite
        // far end.
        //
        // Place: black king on a7, black pawn on d4 (pinned along a7-d4-g1
        // anti-diagonal), white bishop on g1 (the pinner), white king on
        // h2 (clear of interest), white pawn on e4 (just double-pushed,
        // EP target e3). The EP move d4xe3 stays on the a7-g1 anti-
        // diagonal, so it is legal. Assert it's emitted.
        let pos = Position::from_fen("8/k7/8/8/3pP3/8/7K/6B1 b - e3 0 1").unwrap();
        let moves = emit(&pos);
        let pawn_only = pawn_moves_only(&pos, moves.as_slice());
        let ep = Move::new(Square::D4, Square::E3, MoveFlag::EnPassant);
        assert!(
            pawn_only.contains(&ep),
            "EP d4xe3 along the pin diagonal must be emitted, got: {pawn_only:?}"
        );
    }

    #[test]
    fn pawn_ep_horizontal_pin_anchor_a8_king() {
        // Anchor variant of the EP horizontal-pin trap with a different
        // black-king location (a8 rather than d8). Tests that the
        // EP-horizontal-pin filter is independent of where the *other*
        // king sits — only the geometry on rank 5 (own king + own pawn +
        // opp pawn + opp rook all on the same rank) matters.
        //
        // Note: this is NOT a strict "rank-pinned pawn" case in the
        // standard pin sense — there are two pieces (c5 and d5) between
        // own king (a5) and the rook (h5), so neither is individually
        // pinned. The EP move is suppressed by the post-EP-occupancy
        // horizontal-rook check, not by the standard pin_rays filter.
        // The test pins this distinction: even when the pawn is not
        // pinned in the static sense, EP is still rejected because
        // both pawns vacate the rank simultaneously.
        let pos = Position::from_fen("k7/8/8/K1Pp3r/8/8/8/8 w - d6 0 1").unwrap();
        let moves = emit(&pos);
        let pawn_only = pawn_moves_only(&pos, moves.as_slice());
        let ep = Move::new(Square::C5, Square::D6, MoveFlag::EnPassant);
        assert!(
            !pawn_only.contains(&ep),
            "EP c5xd6 must NOT be emitted when post-EP occupancy exposes king to rook \
             (different black-king anchor than the primary test), got: {pawn_only:?}"
        );
    }

    #[test]
    fn pawn_ep_diagonal_pin_filtered() {
        // Symmetric companion to the horizontal-pin trap, on the **diagonal**
        // axis. White king on a3, white pawn on d5 (the EP capturer), black
        // pawn on c5 (just double-pushed c7-c5; EP target c6), black bishop
        // on f8 along the a3-b4-c5-d6-e7-f8 diagonal, black king on h8.
        //
        // Pre-EP: c5 (black pawn) shields white king from bishop f8 (the
        // only diagonal blocker). White is not in check.
        // Post-EP: white pawn moves to c6, black pawn at c5 is captured
        // and removed; c5 vacates. The bishop's f8→a3 diagonal is now
        // unblocked: white king on a3 IS attacked.
        //
        // The white pawn on d5 is NOT pinned in the standard sense —
        // d5 is not on the king-pinner diagonal — so the standard pin
        // filter (pin_rays) does NOT reject this move. Only the
        // post-EP-occupancy bishop guard in `emit_pawn_moves` catches
        // the discovery. This test pins that guard.
        let pos = Position::from_fen("5b1k/8/8/2pP4/8/K7/8/8 w - c6 0 1").unwrap();
        let moves = emit(&pos);
        let pawn_only = pawn_moves_only(&pos, moves.as_slice());
        let illegal_ep = Move::new(Square::D5, Square::C6, MoveFlag::EnPassant);
        assert!(
            !pawn_only.contains(&illegal_ep),
            "EP d5xc6 must NOT be emitted (post-EP exposes king on a3 to \
             bishop on f8 along vacated c5 square), got: {pawn_only:?}"
        );
    }

    #[test]
    fn pawn_ep_target_set_but_no_capturer() {
        // EP target d6 is set in the FEN but the only white pawn is on
        // e5 → wait, that *is* a capturer. Use a pawn that is NOT
        // adjacent: white pawn on a5 only. d6 EP target is meaningless
        // since no white pawn is on c5 or e5.
        let pos = Position::from_fen("4k3/8/8/P7/8/8/8/4K3 w - d6 0 1").unwrap();
        let moves = emit(&pos);
        let pawn_only = pawn_moves_only(&pos, moves.as_slice());
        for mv in &pawn_only {
            assert_ne!(
                mv.flag(),
                MoveFlag::EnPassant,
                "no friendly pawn adjacent to EP target ⇒ no EnPassant move: {mv:?}"
            );
        }
    }

    #[test]
    fn pawn_pinned_along_file_can_only_push() {
        // White king on e1, white pawn on e2, black rook on e8 (with black
        // king on a8 to satisfy validate_post_parse). The pawn is pinned
        // along the e-file. It can push (e2→e3) and double-push (e2→e4) —
        // both stay on the pin ray — but cannot capture off-file. Place a
        // black pawn on d3 to confirm the diagonal capture is filtered
        // out by the pin. (A knight on d3 would itself attack e1, turning
        // the position into a check rather than a pure pin.)
        let pos = Position::from_fen("k3r3/8/8/8/8/3p4/4P3/4K3 w - - 0 1").unwrap();
        let moves = emit(&pos);
        let pawn_only = pawn_moves_only(&pos, moves.as_slice());
        let push = Move::new(Square::E2, Square::E3, MoveFlag::Quiet);
        let dbl = Move::new(Square::E2, Square::E4, MoveFlag::DoublePush);
        assert!(
            pawn_only.contains(&push),
            "pin-along-file pawn must still emit single push along the pin ray, \
             got: {pawn_only:?}"
        );
        assert!(
            pawn_only.contains(&dbl),
            "pin-along-file pawn must still emit double push along the pin ray, \
             got: {pawn_only:?}"
        );
        let illegal_cap = Move::new(Square::E2, Square::D3, MoveFlag::Capture);
        assert!(
            !pawn_only.contains(&illegal_cap),
            "pin-filtered capture e2xd3 must NOT be emitted (pinned along e-file), \
             got: {pawn_only:?}"
        );
    }

    #[test]
    fn pawn_pinned_along_diagonal_can_only_capture_along_pin() {
        // White king on a1, white pawn on b2 pinned by black bishop on c3
        // (a1-b2-c3 diagonal — exactly one piece, b2, between king and
        // pinner). Black king on e8 for parser validity.
        //
        // The pawn's only legal move is to capture the pinner: b2xc3
        // (along the pin ray). All four other "moves" are illegal:
        //   - b2-b3 push: leaves the diagonal (b-file);
        //   - b2-b4 double-push: leaves the diagonal;
        //   - b2-a3 capture-left: leaves the diagonal (a3 is on the
        //     b2-a3 NW diagonal, not the a1-h8 NE pin ray).
        //
        // To keep the assertion narrow, we don't put a black piece on a3,
        // so capture-left isn't even attempted by the emitter — focusing
        // the test on the push-suppression case.
        let pos = Position::from_fen("4k3/8/8/8/8/2b5/1P6/K7 w - - 0 1").unwrap();
        let moves = emit(&pos);
        let pawn_only = pawn_moves_only(&pos, moves.as_slice());

        // Capture along pin diagonal (the pinner itself) IS emitted.
        let cap_along_pin = Move::new(Square::B2, Square::C3, MoveFlag::Capture);
        assert!(
            pawn_only.contains(&cap_along_pin),
            "pawn pinned along a1-h8 diagonal must emit capture along the pin (b2xc3), \
             got: {pawn_only:?}"
        );

        // Push b2→b3 leaves the diagonal — must NOT emit.
        let illegal_push = Move::new(Square::B2, Square::B3, MoveFlag::Quiet);
        assert!(
            !pawn_only.contains(&illegal_push),
            "diagonally-pinned pawn must not emit a forward push that leaves the pin: \
             {pawn_only:?}"
        );
        // Double-push b2→b4 also leaves the diagonal — must NOT emit.
        let illegal_dbl = Move::new(Square::B2, Square::B4, MoveFlag::DoublePush);
        assert!(
            !pawn_only.contains(&illegal_dbl),
            "diagonally-pinned pawn must not emit a double-push that leaves the pin: \
             {pawn_only:?}"
        );
    }

    #[test]
    fn pawn_pinned_along_rank_emits_nothing() {
        // White king on a4, white pawn on c4, black rook on h4 (rank 4
        // pin). Pawn cannot move along a rank, so every pawn move would
        // leave the pin ray → emit zero pawn moves from c4.
        let pos = Position::from_fen("4k3/8/8/8/K1P4r/8/8/8 w - - 0 1").unwrap();
        let moves = emit(&pos);
        let pawn_only = pawn_moves_only(&pos, moves.as_slice());
        for mv in &pawn_only {
            assert_ne!(
                mv.from_square(),
                Square::C4,
                "rank-pinned pawn on c4 must emit zero moves, got: {mv:?}"
            );
        }
    }

    #[test]
    fn pawn_block_check_via_push_mask() {
        // Single check from a slider; a pawn push that lands in the
        // `push_mask` (between king and slider) is emitted.
        //
        // Setup: white king on e1, black rook on e8 (checking along the
        // e-file), white pawn on d2 — its push to d3 is irrelevant; we
        // need a pawn whose push lands on the e-file between e1 and e8.
        //
        // Place white pawn on e2: pushing e2→e3 (a Quiet flag) blocks
        // the rook's check. (Double-push e2→e4 also blocks; both should
        // emit.) The white pawn on e2 itself BLOCKS the check from being
        // a check at all (rook on e8, pawn on e2 between)! So that won't
        // work.
        //
        // Use a pawn that's NOT on the king-rook ray but whose push
        // lands ON it. Place white pawn on d6: push to d7 doesn't help.
        //
        // Re-frame: black bishop on h4 checking white king on e1 along
        // the e1-h4 diagonal? e1, f2, g3, h4 — yes, that's a diagonal.
        // To block, a piece must land on f2, g3 (not e1 itself). White
        // pawn on g2: g2→g3 blocks at g3.
        //
        // Need: white king on e1, black bishop on h4 (delivering check),
        // white pawn on g2 (whose g2→g3 single push blocks at g3). And
        // black king parked safely. Also need to ensure no other piece
        // blocks the diagonal already — f2 must be empty (otherwise it's
        // not a check at all).
        let pos = Position::from_fen("4k3/8/8/8/7b/8/6P1/4K3 w - - 0 1").unwrap();
        let moves = emit(&pos);
        let pawn_only = pawn_moves_only(&pos, moves.as_slice());
        let block = Move::new(Square::G2, Square::G3, MoveFlag::Quiet);
        assert!(
            pawn_only.contains(&block),
            "pawn push g2g3 blocks check from h4 bishop and must be emitted, got: \
             {pawn_only:?}"
        );
    }
}
