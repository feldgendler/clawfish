//! King move emission. See `docs/plans/m1.f.md` §11.
//!
//! Two paths:
//! - Regular king moves filtered by `!pos.occupied(us)` and `!info.king_danger`
//!   (the king-flee gotcha — `king_danger` is computed on `occ ^ king_bb`).
//! - Castling — only when not in check, with all transit / final squares
//!   not in `king_danger`. The king-and-rook-on-starting-square invariant
//!   is enforced at FEN parse time (`validate_post_parse`); movegen
//!   `debug_assert!`s it; release trusts.

use crate::mov::{Move, MoveFlag};
use crate::movegen::masks::{self, MaskInfo};
use crate::movegen::move_list::MoveList;
use crate::piece::{Color, Piece, PieceKind};
use crate::position::{CastlingRights, Position};
use crate::square::Square;

pub(crate) fn emit_king_moves(
    pos: &Position,
    us: Color,
    king_sq: Square,
    info: &MaskInfo,
    out: &mut MoveList,
) {
    let them = us.flip();
    let own = pos.occupied(us);
    let enemy = pos.occupied(them);

    // Regular king moves: 8-direction attacks, filtered by own occupancy and
    // king_danger (computed on `occ ^ king_bb`, so flee-along-ray is correct).
    let targets = masks::king_attacks(king_sq) & !own & !info.king_danger;
    for to in targets.iter() {
        let flag = if enemy.contains(to) {
            MoveFlag::Capture
        } else {
            MoveFlag::Quiet
        };
        out.push(Move::new(king_sq, to, flag));
    }

    // Castling — never legal while in check.
    if !info.checkers.is_empty() {
        return;
    }

    let rights = pos.castling_rights();
    let occ = pos.occupied_all();

    match us {
        Color::White => {
            if rights.has(CastlingRights::WHITE_KING) {
                debug_assert_eq!(
                    pos.piece_at(Square::E1),
                    Some(Piece::new(Color::White, PieceKind::King)),
                );
                debug_assert_eq!(
                    pos.piece_at(Square::H1),
                    Some(Piece::new(Color::White, PieceKind::Rook)),
                );
                let path_clear = !occ.contains(Square::F1) && !occ.contains(Square::G1);
                let path_safe = !info.king_danger.contains(Square::E1)
                    && !info.king_danger.contains(Square::F1)
                    && !info.king_danger.contains(Square::G1);
                if path_clear && path_safe {
                    out.push(Move::new(Square::E1, Square::G1, MoveFlag::KingCastle));
                }
            }
            if rights.has(CastlingRights::WHITE_QUEEN) {
                debug_assert_eq!(
                    pos.piece_at(Square::E1),
                    Some(Piece::new(Color::White, PieceKind::King)),
                );
                debug_assert_eq!(
                    pos.piece_at(Square::A1),
                    Some(Piece::new(Color::White, PieceKind::Rook)),
                );
                let path_clear = !occ.contains(Square::B1)
                    && !occ.contains(Square::C1)
                    && !occ.contains(Square::D1);
                let path_safe = !info.king_danger.contains(Square::E1)
                    && !info.king_danger.contains(Square::D1)
                    && !info.king_danger.contains(Square::C1);
                if path_clear && path_safe {
                    out.push(Move::new(Square::E1, Square::C1, MoveFlag::QueenCastle));
                }
            }
        }
        Color::Black => {
            if rights.has(CastlingRights::BLACK_KING) {
                debug_assert_eq!(
                    pos.piece_at(Square::E8),
                    Some(Piece::new(Color::Black, PieceKind::King)),
                );
                debug_assert_eq!(
                    pos.piece_at(Square::H8),
                    Some(Piece::new(Color::Black, PieceKind::Rook)),
                );
                let path_clear = !occ.contains(Square::F8) && !occ.contains(Square::G8);
                let path_safe = !info.king_danger.contains(Square::E8)
                    && !info.king_danger.contains(Square::F8)
                    && !info.king_danger.contains(Square::G8);
                if path_clear && path_safe {
                    out.push(Move::new(Square::E8, Square::G8, MoveFlag::KingCastle));
                }
            }
            if rights.has(CastlingRights::BLACK_QUEEN) {
                debug_assert_eq!(
                    pos.piece_at(Square::E8),
                    Some(Piece::new(Color::Black, PieceKind::King)),
                );
                debug_assert_eq!(
                    pos.piece_at(Square::A8),
                    Some(Piece::new(Color::Black, PieceKind::Rook)),
                );
                let path_clear = !occ.contains(Square::B8)
                    && !occ.contains(Square::C8)
                    && !occ.contains(Square::D8);
                let path_safe = !info.king_danger.contains(Square::E8)
                    && !info.king_danger.contains(Square::D8)
                    && !info.king_danger.contains(Square::C8);
                if path_clear && path_safe {
                    out.push(Move::new(Square::E8, Square::C8, MoveFlag::QueenCastle));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! King + castling emission tests. Per `docs/plans/m1.f.md` §11 and
    //! §"Test coverage strategy" → "Unit tests" → "king.rs".
    //!
    //! Implementation is stubbed (`unimplemented!`); these tests will be
    //! red until the M1.F implementation pass lands. That is the expected
    //! state at plan-write time — the tests pin the contract that the
    //! implementation must satisfy.
    //!
    //! Every FEN in this module satisfies the strengthened
    //! `validate_post_parse` from §13: if a castling right is set, the
    //! matching king and rook are on their starting squares (e1/h1 for
    //! white kingside, e1/a1 for white queenside, mirror for black). For
    //! tests that exercise zero castling rights, the castling field is
    //! `-`.
    //!
    //! Test calls go through `MaskInfo::compute(&pos, us, king_sq)` to
    //! build the per-call mask bundle and then `emit_king_moves(&pos, us,
    //! king_sq, &info, &mut out)`. The tests never reach into the per-piece
    //! emit functions for non-king pieces, so they isolate king + castling
    //! emission specifically.
    use super::*;
    use crate::mov::{Move, MoveFlag};
    use crate::position::Position;

    /// Build the per-call mask bundle for `pos` with the side-to-move's
    /// king square.
    fn mask_info(pos: &Position) -> (Color, Square, MaskInfo) {
        let us = pos.side_to_move();
        let king_sq = pos.king_square(us);
        let info = MaskInfo::compute(pos, us, king_sq);
        (us, king_sq, info)
    }

    /// Run `emit_king_moves` against `pos` and return the emitted moves.
    fn emit(pos: &Position) -> MoveList {
        let (us, king_sq, info) = mask_info(pos);
        let mut out = MoveList::new();
        emit_king_moves(pos, us, king_sq, &info, &mut out);
        out
    }

    /// True iff `moves` contains a move with the given `(from, to, flag)`.
    fn contains_move(moves: &MoveList, from: Square, to: Square, flag: MoveFlag) -> bool {
        let target = Move::new(from, to, flag);
        moves.iter().any(|m| m == target)
    }

    /// True iff `moves` contains any move with the given `flag`.
    fn contains_flag(moves: &MoveList, flag: MoveFlag) -> bool {
        moves.iter().any(|m| m.flag() == flag)
    }

    /// True iff `moves` contains any move whose `to_square` is `to`.
    fn contains_to(moves: &MoveList, to: Square) -> bool {
        moves.iter().any(|m| m.to_square() == to)
    }

    // ---------------------------------------------------------------------
    // Regular king moves.
    // ---------------------------------------------------------------------

    #[test]
    fn king_quiet_moves_starting_position() {
        // Standard initial position: white king on e1, every adjacent
        // square occupied by a friendly piece (d1=Q, d2=P, e2=P, f2=P,
        // f1=B). The king has zero legal moves.
        let pos = Position::starting_position();
        let moves = emit(&pos);
        assert_eq!(
            moves.len(),
            0,
            "king on starting square is hemmed in by own pieces; expected 0 moves, got {} ({:?})",
            moves.len(),
            moves.as_slice(),
        );
    }

    #[test]
    fn king_capture_moves() {
        // White king on e1 with an adjacent, undefended black knight on
        // e2. The knight does not attack e1 (knight on e2 attacks
        // c1/c3/d4/f4/g3/g1 — note: not e1), so the white king is not in
        // check; the king can quietly capture e2. Test asserts that a
        // `Capture` from e1 to e2 is emitted.
        let pos = Position::from_fen("4k3/8/8/8/8/8/4n3/4K3 w - - 0 1")
            .expect("FEN must parse: white king e1, black knight e2, black king e8, no rights");
        let moves = emit(&pos);
        assert!(
            contains_move(&moves, Square::E1, Square::E2, MoveFlag::Capture),
            "expected Capture from e1 to e2 in {:?}",
            moves.as_slice(),
        );
    }

    #[test]
    fn king_cannot_move_into_attacked_square() {
        // White king on b8 in check from black rook on a8 (rook attacks
        // the a-file and rank 8, including a8/c8/d8/.../h8 and a7/a6/...
        // — but NOT b7 or further into rank 7 along non-a files).
        //
        // The "into attacked square" subject of this test is the capture
        // square a8: even though a8 is the rook's own square, capturing
        // into a8 would still leave the white king in check from the
        // (no longer present) rook — except the rook IS captured, so
        // a8 is not in check after the move. Wait — a8 is occupied by
        // the rook itself. The king cannot move to a8 only if a8 is
        // attacked by some other opponent piece. In this position, no
        // other opponent attacker exists, so capturing the rook on a8
        // IS legal — that's not what this test is named after.
        //
        // Re-reading the plan: "king_cannot_move_into_attacked_square —
        // opponent rook on a8, our king on b8: cannot move to a8 (capture
        // into check)". This phrasing only makes sense if some second
        // attacker defends a8. Without a defender, capturing a8 is
        // legal (the rook is undefended).
        //
        // To pin the "cannot move into attacked square" concept clearly,
        // we add a defender — black bishop on h1 attacks a8 along the
        // h1-a8 diagonal. Wait — h1 isn't on the a8-h1 diagonal? Yes it
        // is: a8, b7, c6, d5, e4, f3, g2, h1. So bishop on h1 covers a8.
        //
        // Setup: black rook on a8 (giving check), black bishop on h1
        // defending a8 along the long diagonal. White king on b8. Black
        // king somewhere out of the way (e.g. h6). Castling `-`.
        //
        // Expected: no king move targeting a8 is emitted (capture would
        // leave king on a8 in check from the bishop on h1, since the
        // bishop's diagonal is uninterrupted once the rook is gone).
        let pos = Position::from_fen("rK6/8/7k/8/8/8/8/7b w - - 0 1")
            .expect("FEN must parse: rook a8, white king b8, defender bishop h1");
        let moves = emit(&pos);
        assert!(
            !contains_to(&moves, Square::A8),
            "king must not move to a8 (capture-into-check); got {:?}",
            moves.as_slice(),
        );
    }

    #[test]
    fn king_can_capture_undefended_attacker() {
        // White king on e1 in check from a black pawn on f2 (the pawn
        // attacks e1 and g1). The pawn is not defended by any other
        // black piece. The king can legally capture f2.
        //
        // Test asserts that a `Capture` from e1 to f2 is emitted.
        let pos = Position::from_fen("4k3/8/8/8/8/8/5p2/4K3 w - - 0 1")
            .expect("FEN must parse: white king e1 in check from black pawn f2, no rights");
        let moves = emit(&pos);
        assert!(
            contains_move(&moves, Square::E1, Square::F2, MoveFlag::Capture),
            "expected Capture from e1 to f2 (capture undefended attacker) in {:?}",
            moves.as_slice(),
        );
    }

    #[test]
    fn king_cannot_capture_defended_attacker() {
        // White king on e1 in check from a black rook on e2. A second
        // black rook on e8 defends the e2 rook along the e-file (with
        // the e-file blockers between e2 and e8 empty). If the king
        // captures e2, it lands on e2 — and the e8 rook now attacks the
        // (newly empty between) e-file all the way to e2: the white
        // king on e2 is in check from e8.
        //
        // Test asserts that no king move targeting e2 (the capture)
        // is emitted.
        //
        // Black king parked on a8 to keep the position structurally
        // valid (one king per side). Castling `-` because no king or
        // rook is on a starting square paired with a starting-square
        // king of the same color (white king on e1, but white has no
        // rooks here).
        let pos = Position::from_fen("k3r3/8/8/8/8/8/4r3/4K3 w - - 0 1")
            .expect("FEN must parse: defended rook on e2, defender on e8");
        let moves = emit(&pos);
        assert!(
            !contains_to(&moves, Square::E2),
            "king must not capture defended rook on e2 (king-on-e2 still in check from e8); \
             got {:?}",
            moves.as_slice(),
        );
    }

    #[test]
    fn king_flee_along_check_ray_blocked() {
        // White king on a1 in check from black rook on a8 (the a-file is
        // empty of any other piece). The "flee along the ray" gotcha:
        // moving the king from a1 to a2 stays on the a-file. Without
        // the `occ ^ king_bb` correction in the king-danger map, the
        // rook's attack ray would stop at a1 (the king's current
        // square), so a2 would not be flagged as attacked, and the
        // emitter would (wrongly) emit `a1a2` as a legal move.
        //
        // With the correction, a2 IS attacked, and the king cannot
        // flee to a2.
        //
        // Test asserts that no king move targets a2.
        //
        // Black king parked far from the action (e8). Castling `-`
        // (white king is not on e1, so no white rights; no black rook
        // on a8/h8 in matching positions for black rights).
        let pos = Position::from_fen("r3k3/8/8/8/8/8/8/K7 w - - 0 1")
            .expect("FEN must parse: white king a1 vs black rook a8");
        let moves = emit(&pos);
        assert!(
            !contains_to(&moves, Square::A2),
            "king must not flee to a2 (still on the a-file ray after `occ ^ king_bb`); \
             got {:?}",
            moves.as_slice(),
        );
    }

    // ---------------------------------------------------------------------
    // Castling.
    // ---------------------------------------------------------------------

    #[test]
    fn castling_kingside_legal_when_path_clear() {
        // White king on e1, white rook on h1, no pieces on f1 or g1, no
        // attacker covering e1/f1/g1. White is not in check. Castling
        // right `K` set. The kingside castle move (`KingCastle` flag,
        // from e1 to g1) must be emitted.
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/4K2R w K - 0 1")
            .expect("FEN must parse: kingside castling-ready position");
        let moves = emit(&pos);
        assert!(
            contains_move(&moves, Square::E1, Square::G1, MoveFlag::KingCastle),
            "expected kingside castle (e1→g1, KingCastle) in {:?}",
            moves.as_slice(),
        );
    }

    #[test]
    fn castling_queenside_legal_when_path_clear() {
        // White king on e1, white rook on a1, no pieces on b1/c1/d1, no
        // attacker covering c1/d1/e1. White is not in check. Castling
        // right `Q` set. The queenside castle move (`QueenCastle` flag,
        // from e1 to c1) must be emitted.
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/R3K3 w Q - 0 1")
            .expect("FEN must parse: queenside castling-ready position");
        let moves = emit(&pos);
        assert!(
            contains_move(&moves, Square::E1, Square::C1, MoveFlag::QueenCastle),
            "expected queenside castle (e1→c1, QueenCastle) in {:?}",
            moves.as_slice(),
        );
    }

    #[test]
    fn castling_blocked_by_own_piece_between() {
        // White knight on f1 occupies a square between king (e1) and rook
        // (h1). Even with castling right `K` set, the kingside path is
        // blocked. No `KingCastle` move may be emitted.
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/4KN1R w K - 0 1")
            .expect("FEN must parse: own knight on f1 blocks kingside castling");
        let moves = emit(&pos);
        assert!(
            !contains_flag(&moves, MoveFlag::KingCastle),
            "kingside castle must not emit when own knight blocks f1; got {:?}",
            moves.as_slice(),
        );
    }

    #[test]
    fn castling_queenside_blocked_by_piece_on_b1() {
        // White rook a1 + king e1 + own knight on b1 (between rook and king
        // on the queenside path). The knight occupies a path square the
        // rook must transit; even though king's transit is c1/d1/e1 only,
        // the implementation's path-clear check covers b1 too. Pins the
        // path-clear `&&` chain on its first conjunct.
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/RN2K3 w Q - 0 1")
            .expect("FEN must parse: own knight on b1 blocks queenside path");
        let moves = emit(&pos);
        assert!(
            !contains_flag(&moves, MoveFlag::QueenCastle),
            "queenside castle must not emit when b1 has an own piece; got {:?}",
            moves.as_slice(),
        );
    }

    #[test]
    fn castling_queenside_blocked_by_piece_on_c1() {
        // Own knight on c1 — the king's transit-final square. Pins the
        // path-clear `&&` chain on its second conjunct.
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/R1N1K3 w Q - 0 1")
            .expect("FEN must parse: own knight on c1 blocks queenside path");
        let moves = emit(&pos);
        assert!(
            !contains_flag(&moves, MoveFlag::QueenCastle),
            "queenside castle must not emit when c1 has an own piece; got {:?}",
            moves.as_slice(),
        );
    }

    #[test]
    fn castling_queenside_blocked_by_piece_on_d1() {
        // Own knight on d1. Pins the path-clear chain on its third conjunct.
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/R2NK3 w Q - 0 1")
            .expect("FEN must parse: own knight on d1 blocks queenside path");
        let moves = emit(&pos);
        assert!(
            !contains_flag(&moves, MoveFlag::QueenCastle),
            "queenside castle must not emit when d1 has an own piece; got {:?}",
            moves.as_slice(),
        );
    }

    #[test]
    fn castling_queenside_blocked_by_attack_on_d1() {
        // Black rook on d8 attacks d1 (king's transit square between e1
        // and c1). king_danger contains d1, so the path-safe `&&` chain
        // must reject this. Pins the path-safe chain's middle conjunct.
        let pos = Position::from_fen("3r4/8/k7/8/8/8/8/R3K3 w Q - 0 1")
            .expect("FEN must parse: black rook attacks d1");
        let moves = emit(&pos);
        assert!(
            !contains_flag(&moves, MoveFlag::QueenCastle),
            "queenside castle must not emit when d1 is attacked; got {:?}",
            moves.as_slice(),
        );
    }

    #[test]
    fn castling_queenside_blocked_by_attack_on_c1() {
        // Black rook on c8 attacks c1 (king's transit-final). Pins the
        // path-safe chain's third conjunct.
        let pos = Position::from_fen("2r5/8/k7/8/8/8/8/R3K3 w Q - 0 1")
            .expect("FEN must parse: black rook attacks c1");
        let moves = emit(&pos);
        assert!(
            !contains_flag(&moves, MoveFlag::QueenCastle),
            "queenside castle must not emit when c1 is attacked; got {:?}",
            moves.as_slice(),
        );
    }

    #[test]
    fn castling_blocked_by_opponent_attack_on_transit() {
        // Black rook on f8 attacks down the f-file (no blockers); the
        // white king's transit square f1 is therefore attacked. The
        // king cannot pass through an attacked square, so kingside
        // castle must not be emitted — even though the king-from
        // (e1) and king-to (g1) squares are not attacked.
        //
        // Black king parked on a6 (out of the action). Castling `K`
        // (white kingside only); no black rights claimed (e8 vacant).
        let pos = Position::from_fen("5r2/8/k7/8/8/8/8/4K2R w K - 0 1")
            .expect("FEN must parse: black rook attacks f1 (kingside transit)");
        let moves = emit(&pos);
        assert!(
            !contains_flag(&moves, MoveFlag::KingCastle),
            "kingside castle must not emit when transit square f1 is attacked; got {:?}",
            moves.as_slice(),
        );
    }

    #[test]
    fn castling_blocked_by_opponent_attack_on_destination() {
        // Black rook on g8 attacks down the g-file; the king's destination
        // square g1 is therefore attacked. The king cannot land on an
        // attacked square. Kingside castle must not be emitted — even
        // though e1 and f1 are not attacked.
        let pos = Position::from_fen("6r1/8/k7/8/8/8/8/4K2R w K - 0 1")
            .expect("FEN must parse: black rook attacks g1 (kingside destination)");
        let moves = emit(&pos);
        assert!(
            !contains_flag(&moves, MoveFlag::KingCastle),
            "kingside castle must not emit when destination square g1 is attacked; got {:?}",
            moves.as_slice(),
        );
    }

    #[test]
    fn castling_blocked_when_in_check() {
        // White king on e1 in check from black rook on e2. White has both
        // castling rights (`KQ`); both kingside and queenside paths are
        // structurally clear (b1/c1/d1/f1/g1 all empty). But because
        // the king is currently in check, neither castle is legal:
        // castling out of check is forbidden by the FIDE rules.
        let pos = Position::from_fen("4k3/8/8/8/8/8/4r3/R3K2R w KQ - 0 1")
            .expect("FEN must parse: in-check king with both castling rights and clear paths");
        let moves = emit(&pos);
        assert!(
            !contains_flag(&moves, MoveFlag::KingCastle),
            "kingside castle must not emit when king is in check; got {:?}",
            moves.as_slice(),
        );
        assert!(
            !contains_flag(&moves, MoveFlag::QueenCastle),
            "queenside castle must not emit when king is in check; got {:?}",
            moves.as_slice(),
        );
    }

    #[test]
    fn castling_blocked_when_no_right() {
        // Path is clear for kingside (f1 and g1 empty) AND for queenside,
        // and the king is not in check. The castling-rights field
        // (`Q` only — kingside right `K` not set) is the *only* reason
        // kingside castle must not be emitted. White rook on h1 is
        // present but its kingside-castling-right was lost (e.g., the
        // king once moved and returned to e1, then an `Q`-only state
        // is not a real chess derivation, but FEN allows it as a
        // start-from-arbitrary-position).
        //
        // Test asserts that no `KingCastle` move is emitted even though
        // the path is otherwise clear.
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/R3K2R w Q - 0 1")
            .expect("FEN must parse: kingside path clear but kingside right cleared");
        let moves = emit(&pos);
        assert!(
            !contains_flag(&moves, MoveFlag::KingCastle),
            "kingside castle must not emit when castling right K is cleared; got {:?}",
            moves.as_slice(),
        );
    }

    #[test]
    fn castling_b1_attacked_kingside_irrelevant() {
        // Black rook on b8 attacks the b-file (b1, b2, ..., b8). For a
        // white kingside castle, the king transits e1 → f1 → g1; b1 is
        // not in this path. The b1 attacker is therefore irrelevant to
        // kingside legality. Kingside castle must STILL be emitted.
        //
        // Sanity: f1, g1, h1 are not on the b-file, so the b8 rook does
        // not attack any king-transit square. The white king on e1 is
        // not in check.
        let pos = Position::from_fen("1r2k3/8/8/8/8/8/8/4K2R w K - 0 1")
            .expect("FEN must parse: black rook attacks b1 only (kingside test)");
        let moves = emit(&pos);
        assert!(
            contains_move(&moves, Square::E1, Square::G1, MoveFlag::KingCastle),
            "kingside castle must emit when only b1 is attacked (kingside doesn't transit b1); \
             got {:?}",
            moves.as_slice(),
        );
    }

    #[test]
    fn castling_b1_attacked_queenside_legal() {
        // Black rook on b8 attacks the b-file. For a white queenside
        // castle:
        // - The king transits e1 → d1 → c1.
        // - The rook transits a1 → d1 → … but the rook's own transit
        //   squares are b1 and c1 and d1 (specifically: rook lands on
        //   d1, passing across b1 on its way).
        //
        // Castling legality cares about KING transit squares only:
        // c1, d1, e1 must not be attacked (and e1 must not be in
        // check — covered by the "not in check" precondition; here
        // the king is not in check). The rook crosses b1, but the
        // chess rule is silent about rook transit squares — only the
        // king's path matters.
        //
        // This test makes the *why* explicit: queenside castle is
        // LEGAL even though b1 is attacked, because b1 is the rook's
        // transit-only square (the king never visits b1).
        let pos = Position::from_fen("1r2k3/8/8/8/8/8/8/R3K3 w Q - 0 1")
            .expect("FEN must parse: black rook attacks b1 only (queenside test)");
        let moves = emit(&pos);
        assert!(
            contains_move(&moves, Square::E1, Square::C1, MoveFlag::QueenCastle),
            "queenside castle MUST emit when only b1 is attacked: b1 is the rook's transit \
             square, not the king's; the king's transit squares are c1/d1/e1, none of which \
             are attacked; got {:?}",
            moves.as_slice(),
        );
    }
}
