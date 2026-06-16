//! Static Exchange Evaluation (SEE).
//!
//! `see(pos, mv)` returns the net material outcome of the capture sequence
//! starting with `mv`, assuming both sides recapture with their least-valuable
//! attacker (LVA) and stop when continuing would lose material.  Positive =
//! the capturing side comes out ahead; negative = the capture loses material.
//!
//! Semantics committed in M7.A (ADR at landing):
//! - Pins are **ignored**: a pinned defender still participates.
//! - LVA order: Pawn → Knight → Bishop → Rook → Queen → King.
//! - X-ray / batteries: sliders behind a spent attacker are revealed.
//! - King recaptures only when the opponent has no remaining attacker.
//! - Promotion-on-capture: best-promo = queen assumption.
//! - En passant: the captured pawn is on the square sharing `from`'s rank
//!   and `to`'s file; both that square and the mover's `from` are vacated
//!   before the x-ray rescan.
//!
//! **Landed inert (M7.A-infra, 2026-06-15).** The evaluator is fully built and
//! tested but has **no production caller yet** — the M7.A capture-ordering split
//! that consumed it SPRT'd flat (v1 −10.4, v2 +2.6 Elo; neither cleared the ship
//! ladder — the engine's MVV-LVA+TT+killer+history ordering is already strong, so
//! ordering-by-SEE is neutral here) and was shelved. `see()` / `attackers_to` /
//! `SEE_VALUE` are the reusable search primitive that **M7.B (qsearch
//! SEE-pruning)** and M7.C consume; landing it inert ahead of its consumer
//! follows the M6 inert-landing precedent (eval byte-identical to M5.K, bench
//! unchanged). The module-wide `dead_code` allow was removed when M7.B wired the
//! first production caller (`qsearch_see_pruneable` in `search.rs`).

use crate::bitboard::Bitboard;
use crate::magic::{bishop_attacks, rook_attacks};
use crate::mov::{Move, MoveFlag};
use crate::movegen::masks::{king_attacks, knight_attacks, pawn_attacks_from};
use crate::piece::{Color, PieceKind};
use crate::position::Position;
use crate::square::Square;

// ---------------------------------------------------------------------------
// Value scale
// ---------------------------------------------------------------------------

/// SEE material values, indexed by `PieceKind as usize` (P=0 … K=5).
///
/// The first five mirror PeSTO MG `MATERIAL` in `src/eval/data.rs` for
/// consistency with the existing MVV-LVA tier, but are defined **here**
/// independently so SEE is decoupled from future eval re-tuning.  SEE is a
/// search primitive; NNUE does not obsolete it.
pub(crate) const SEE_VALUE: [i32; 6] = [82, 337, 365, 477, 1025, 30000];

// King sentinel must dominate the maximum material one side can accumulate
// via promotions (9 queens + 2 rooks + 2 bishops + 2 knights).
// 9*1025 + 2*477 + 2*365 + 2*337 = 11583 < 30000 ✓
const _SEE_KING_SENTINEL_DOMINATES: () = {
    assert!(
        SEE_VALUE[PieceKind::King as usize]
            > 9 * SEE_VALUE[PieceKind::Queen as usize]
                + 2 * SEE_VALUE[PieceKind::Rook as usize]
                + 2 * SEE_VALUE[PieceKind::Bishop as usize]
                + 2 * SEE_VALUE[PieceKind::Knight as usize]
    );
};

/// Maximum number of attacker-removal steps in one SEE exchange.  The real
/// maximum is 16 per side (8 pawns + 2 knights + 2 bishops + 2 rooks +
/// 1 queen + 1 king) × 2 sides = 32; this is a safe over-bound.
const MAX_SEE_DEPTH: usize = 32;

// ---------------------------------------------------------------------------
// attackers_to
// ---------------------------------------------------------------------------

/// Union of all pieces of **both** colors that attack `sq` under occupancy
/// `occ`.  Callers mask by `pos.occupied(side)` to isolate one side's
/// attackers.
///
/// Uses `occ` for slider rescans so callers can progressively shrink
/// occupancy to reveal x-ray batteries.
pub(crate) fn attackers_to(pos: &Position, sq: Square, occ: Bitboard) -> Bitboard {
    let white_pawns = pos.pieces_colored(Color::White, PieceKind::Pawn);
    let black_pawns = pos.pieces_colored(Color::Black, PieceKind::Pawn);

    // A pawn of color C attacks `sq` from the squares the *opposite*-color
    // pawn-attack pattern of `sq` points to.
    // pawn_attacks_from(sq, Black) → squares a Black pawn on `sq` attacks
    // (i.e. below-diagonally) = exactly the squares a White pawn could stand
    // on to attack `sq` upward.
    let pawn_attackers = (pawn_attacks_from(sq, Color::Black) & white_pawns)
        | (pawn_attacks_from(sq, Color::White) & black_pawns);

    let knight_attackers = knight_attacks(sq) & pos.pieces(PieceKind::Knight);

    let bishop_like = pos.pieces(PieceKind::Bishop) | pos.pieces(PieceKind::Queen);
    let rook_like = pos.pieces(PieceKind::Rook) | pos.pieces(PieceKind::Queen);

    let diag_attackers = bishop_attacks(sq, occ) & bishop_like;
    let orth_attackers = rook_attacks(sq, occ) & rook_like;

    let king_attackers = king_attacks(sq) & pos.pieces(PieceKind::King);

    pawn_attackers | knight_attackers | diag_attackers | orth_attackers | king_attackers
}

// ---------------------------------------------------------------------------
// see — fast swap-list resolver
// ---------------------------------------------------------------------------

/// Static Exchange Evaluation of capture move `mv`.
///
/// Returns the net material outcome (on the `SEE_VALUE` scale) from the
/// perspective of the side making `mv`.  Positive = capturer gains material;
/// negative = capturer loses material.
///
/// **Only valid on capture moves** (`mv.is_capture()` must hold).
///
/// Algorithm: allocation-free raw gain array.  `gain[d]` stores the raw
/// value of what the depth-`d` capturer takes (= value of the piece placed
/// on `to` by the depth-`(d-1)` capturer).  Backup folds from deepest to
/// shallowest: `gain[i-1] -= max(0, gain[i])`, meaning each side stops when
/// continuing would yield a negative return.
pub(crate) fn see(pos: &Position, mv: Move) -> i32 {
    debug_assert!(mv.is_capture(), "see() called on non-capture move");

    let from = mv.from_square();
    let to = mv.to_square();
    let flag = mv.flag();

    // Determine initial victim value, starting occupancy, and the value of
    // the moving piece now sitting on `to` (= gain[1]'s "raw captured value").
    let (gain0, mut occ, mut piece_on_sq) = match flag {
        MoveFlag::EnPassant => {
            // The EP-captured pawn sits at rank_of(from), file_of(to).
            // Computed inline (independence from slow_see — §5.2).
            let ep_victim_sq = Square::new_unchecked(from.rank() * 8 + to.file());
            let occ = pos.occupied_all().without(ep_victim_sq);
            let mover_kind = pos
                .piece_at(from)
                .expect("see: from-square must be occupied")
                .kind;
            (
                SEE_VALUE[PieceKind::Pawn as usize],
                occ,
                SEE_VALUE[mover_kind as usize],
            )
        }
        MoveFlag::KnightPromoCapture
        | MoveFlag::BishopPromoCapture
        | MoveFlag::RookPromoCapture
        | MoveFlag::QueenPromoCapture => {
            let victim_val = SEE_VALUE[pos
                .piece_at(to)
                .expect("see: promo-capture requires a piece on to-square")
                .kind as usize];
            // The pawn promotes to a queen; queen-promo assumption.
            // gain[0] includes the promotion bonus; piece_on_sq = queen value.
            let promo_bonus =
                SEE_VALUE[PieceKind::Queen as usize] - SEE_VALUE[PieceKind::Pawn as usize];
            (
                victim_val + promo_bonus,
                pos.occupied_all(),
                SEE_VALUE[PieceKind::Queen as usize],
            )
        }
        MoveFlag::Capture => {
            let victim_val = SEE_VALUE[pos
                .piece_at(to)
                .expect("see: capture requires a piece on to-square")
                .kind as usize];
            let mover_kind = pos
                .piece_at(from)
                .expect("see: from-square must be occupied")
                .kind;
            (
                victim_val,
                pos.occupied_all(),
                SEE_VALUE[mover_kind as usize],
            )
        }
        _ => unreachable!("see: unexpected non-capture flag on a capture move"),
    };

    // Remove the mover from occupancy (it has vacated `from`).
    occ = occ.without(from);

    let bishop_like = pos.pieces(PieceKind::Bishop) | pos.pieces(PieceKind::Queen);
    let rook_like = pos.pieces(PieceKind::Rook) | pos.pieces(PieceKind::Queen);

    // Initialize running attacker set against the post-mover occupancy.
    let mut attackers = attackers_to(pos, to, occ);

    // gain[0] is already set; gain[1..] are filled in the loop.
    let mut gain = [0i32; MAX_SEE_DEPTH];
    gain[0] = gain0;

    let mut side = pos.side_to_move().flip();
    let mut d = 1usize;

    loop {
        let side_attackers = attackers & occ & pos.occupied(side);
        if side_attackers.is_empty() {
            break;
        }

        let (lva_sq, lva_kind) = match find_lva(side_attackers, pos) {
            Some(x) => x,
            None => break,
        };

        // King-recapture legality: king cannot step into a defended square.
        if lva_kind == PieceKind::King {
            let opponent_still_attacks = (attackers & occ & pos.occupied(side.flip())).any();
            if opponent_still_attacks {
                break;
            }
        }

        // Raw gain: value of the piece currently on `to` (what this capturer takes) plus
        // the promotion bonus if this pawn lands on the back rank.  The bonus
        // (`queen - pawn`) is added to `piece_on_sq` (the piece being taken) because
        // the pawn upgrades its own future trade-value — analogous to how the initial
        // promo-capture path computes `victim_val + promo_bonus` at gain[0].
        let promo = lva_kind == PieceKind::Pawn && is_promotion_rank(to, side);
        gain[d] = piece_on_sq
            + if promo {
                SEE_VALUE[PieceKind::Queen as usize] - SEE_VALUE[PieceKind::Pawn as usize]
            } else {
                0
            };

        // The piece now sitting on `to` is worth queen value after promotion,
        // or the mover's own value otherwise.
        piece_on_sq = if promo {
            SEE_VALUE[PieceKind::Queen as usize]
        } else {
            SEE_VALUE[lva_kind as usize]
        };

        occ = occ.without(lva_sq);
        attackers = attackers.without(lva_sq);

        // Reveal x-ray sliders through the vacated square.
        attackers |=
            (bishop_attacks(to, occ) & bishop_like | rook_attacks(to, occ) & rook_like) & occ;

        side = side.flip();
        d += 1;
        if d >= MAX_SEE_DEPTH {
            break;
        }
    }

    // Backup: each side stops when continuing yields a negative return.
    // gain[i-1] -= max(0, gain[i]) propagates from deepest to shallowest.
    for i in (1..d).rev() {
        gain[i - 1] -= gain[i].max(0);
    }

    gain[0]
}

/// Find the square and kind of the lowest-SEE_VALUE piece in `side_attackers`.
#[inline]
fn find_lva(side_attackers: Bitboard, pos: &Position) -> Option<(Square, PieceKind)> {
    for kind in PieceKind::ALL {
        let candidates = side_attackers & pos.pieces(kind);
        if let Some(sq) = candidates.lsb() {
            return Some((sq, kind));
        }
    }
    None
}

/// Whether `to` is the promotion rank for a pawn of color `side` landing there.
#[inline]
fn is_promotion_rank(to: Square, side: Color) -> bool {
    match side {
        Color::White => to.rank() == 7,
        Color::Black => to.rank() == 0,
    }
}

// ---------------------------------------------------------------------------
// slow_see — independent reference oracle for property testing
// ---------------------------------------------------------------------------

/// Structurally-independent SEE oracle used by the differential proptest.
///
/// Recomputes the full attacker set from scratch via `attackers_to` after
/// every removal (no incremental x-ray bookkeeping).  Uses the same LVA
/// order, same `SEE_VALUE`, same pin-ignoring semantics, and the same raw
/// gain + `gain[i-1] -= max(0, gain[i])` backup.  The EP-victim square and
/// promotion-rank trigger are derived inline (not from a shared helper) to
/// satisfy §5.2 independence.
#[cfg(test)]
fn slow_see(pos: &Position, mv: Move) -> i32 {
    debug_assert!(mv.is_capture(), "slow_see() called on non-capture move");

    let from = mv.from_square();
    let to = mv.to_square();
    let flag = mv.flag();

    let (gain0, mut occ, mut piece_on_sq) = match flag {
        MoveFlag::EnPassant => {
            // EP victim: rank of `from`, file of `to` — inline, not a shared helper.
            let ep_sq = Square::new_unchecked(from.rank() * 8 + to.file());
            let occ = pos.occupied_all().without(ep_sq);
            let mover = pos.piece_at(from).expect("slow_see: from occupied").kind;
            (
                SEE_VALUE[PieceKind::Pawn as usize],
                occ,
                SEE_VALUE[mover as usize],
            )
        }
        MoveFlag::KnightPromoCapture
        | MoveFlag::BishopPromoCapture
        | MoveFlag::RookPromoCapture
        | MoveFlag::QueenPromoCapture => {
            let vtgt = SEE_VALUE[pos
                .piece_at(to)
                .expect("slow_see: to occupied for promo-cap")
                .kind as usize];
            let bonus = SEE_VALUE[PieceKind::Queen as usize] - SEE_VALUE[PieceKind::Pawn as usize];
            (
                vtgt + bonus,
                pos.occupied_all(),
                SEE_VALUE[PieceKind::Queen as usize],
            )
        }
        MoveFlag::Capture => {
            let vtgt = SEE_VALUE[pos
                .piece_at(to)
                .expect("slow_see: to occupied for capture")
                .kind as usize];
            let mover = pos.piece_at(from).expect("slow_see: from occupied").kind;
            (vtgt, pos.occupied_all(), SEE_VALUE[mover as usize])
        }
        _ => unreachable!("slow_see: unexpected non-capture flag"),
    };

    occ = occ.without(from);

    let mut gain = [0i32; MAX_SEE_DEPTH];
    gain[0] = gain0;
    let mut side = pos.side_to_move().flip();
    let mut d = 1usize;

    loop {
        // Full recompute every step (independence from fast path x-ray logic).
        let full_attackers = attackers_to(pos, to, occ);
        let side_attackers = full_attackers & occ & pos.occupied(side);

        if side_attackers.is_empty() {
            break;
        }

        let mut lva_sq_opt: Option<Square> = None;
        let mut lva_kind_opt: Option<PieceKind> = None;
        for kind in PieceKind::ALL {
            let cands = side_attackers & pos.pieces(kind);
            if let Some(sq) = cands.lsb() {
                lva_sq_opt = Some(sq);
                lva_kind_opt = Some(kind);
                break;
            }
        }
        let (lva_sq, lva_kind) = match (lva_sq_opt, lva_kind_opt) {
            (Some(sq), Some(k)) => (sq, k),
            _ => break,
        };

        if lva_kind == PieceKind::King {
            let opp_still = (full_attackers & occ & pos.occupied(side.flip())).any();
            if opp_still {
                break;
            }
        }

        // Promotion check — inline from mv's to-square rank and recapturing side.
        let promo = lva_kind == PieceKind::Pawn
            && match side {
                Color::White => to.rank() == 7,
                Color::Black => to.rank() == 0,
            };

        // Mirror of the fix in see(): gain includes piece_on_sq plus promotion
        // bonus; piece_on_sq becomes queen value after promotion.
        gain[d] = piece_on_sq
            + if promo {
                SEE_VALUE[PieceKind::Queen as usize] - SEE_VALUE[PieceKind::Pawn as usize]
            } else {
                0
            };

        piece_on_sq = if promo {
            SEE_VALUE[PieceKind::Queen as usize]
        } else {
            SEE_VALUE[lva_kind as usize]
        };

        occ = occ.without(lva_sq);
        side = side.flip();
        d += 1;
        if d >= MAX_SEE_DEPTH {
            break;
        }
    }

    for i in (1..d).rev() {
        gain[i - 1] -= gain[i].max(0);
    }

    gain[0]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mov::Move;
    use crate::movegen::test_strategies::arb_position;
    use crate::movegen::{MoveList, generate_moves};
    use crate::position::Position;
    use proptest::prelude::*;

    /// Parse a position and find the capture move matching the UCI string.
    fn parse_capture(fen: &str, uci: &str) -> (Position, Move) {
        let pos = Position::from_fen(fen).expect("FEN must parse");
        let mv = Move::from_uci(uci, &pos).expect("move must be legal");
        assert!(mv.is_capture(), "move must be a capture: {uci}");
        (pos, mv)
    }

    // -----------------------------------------------------------------------
    // attackers_to unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn attackers_to_pawn_orientation() {
        // White pawn on e4 attacks d5 and f5.
        // Black pawn on d5 can be found via pawn_attacks_from(d5, White) & white_pawns.
        // FEN: white Pe4, black pd5, kings.
        let pos = Position::from_fen("4k3/8/8/3p4/4P3/8/8/4K3 w - - 0 1").unwrap();
        let atk = attackers_to(&pos, Square::D5, pos.occupied_all());
        // White pawn e4 attacks d5 (pawn_attacks_from(d5, Black) & white_pawns = e4).
        assert!(atk.contains(Square::E4), "white Pe4 should attack d5");
        // Black pawn d5 does NOT attack d5 (it occupies it).
        assert!(!atk.contains(Square::D5), "d5 not in its own attacker set");
    }

    #[test]
    fn attackers_to_knight_attacks() {
        // White knight on f3 attacks e5.
        let pos =
            Position::from_fen("r1bqkbnr/pppp1ppp/2n5/4p3/4P3/5N2/PPPP1PPP/RNBQKB1R w KQkq - 2 3")
                .unwrap();
        let atk = attackers_to(&pos, Square::E5, pos.occupied_all());
        assert!(atk.contains(Square::F3), "Nf3 attacks e5");
    }

    #[test]
    fn attackers_to_xray_rook_battery() {
        // Two white rooks on a1 and a5; black rook on a8.
        // With full occ: Ra1 is blocked by Ra5 when looking from a8 perspective.
        // After removing Ra5 from occ: Ra1 is revealed.
        let pos = Position::from_fen("r6k/8/8/R7/8/8/8/R6K w - - 0 1").unwrap();
        let full_occ = pos.occupied_all();

        let atk_full = attackers_to(&pos, Square::A8, full_occ);
        assert!(atk_full.contains(Square::A5), "Ra5 attacks a8 in full occ");
        assert!(
            !atk_full.contains(Square::A1),
            "Ra1 blocked by Ra5 in full occ"
        );

        // Simulate Ra5 having moved to a8 (remove a5 from occ).
        let occ_after = full_occ.without(Square::A5);
        let atk_after = attackers_to(&pos, Square::A8, occ_after);
        assert!(
            atk_after.contains(Square::A1),
            "Ra1 revealed after Ra5 removed"
        );
    }

    #[test]
    fn attackers_to_bishop_and_queen() {
        // White bishop on c4 and white queen on b3 both on the b3-e6 diagonal.
        // From b3: b3→c4→d5→e6. So both Bc4 and Qb3 attack d5 in the right occ.
        // If Bc4 is present, Qb3 is blocked. After removing Bc4: Qb3 revealed.
        let pos = Position::from_fen("4k3/8/4n3/3p4/2B5/1Q6/8/4K3 w - - 0 1").unwrap();
        let full_occ = pos.occupied_all();

        let atk_full = attackers_to(&pos, Square::D5, full_occ);
        assert!(atk_full.contains(Square::C4), "Bc4 attacks d5");
        // Qb3 is blocked by Bc4 on b3-d5 diagonal.
        assert!(
            !atk_full.contains(Square::B3),
            "Qb3 blocked by Bc4 in full occ"
        );

        // Remove Bc4: Qb3 should be revealed.
        let occ_no_bc4 = full_occ.without(Square::C4);
        let atk_after = attackers_to(&pos, Square::D5, occ_no_bc4);
        assert!(
            atk_after.contains(Square::B3),
            "Qb3 revealed after Bc4 removed"
        );
    }

    // -----------------------------------------------------------------------
    // Curated SEE correctness suite
    //
    // All expected values computed from SEE_VALUE = [82, 337, 365, 477, 1025, 30000].
    // Exchange notation in comments shows the sequence and manual calculation.
    // -----------------------------------------------------------------------

    #[test]
    fn see_hanging_piece_free_capture() {
        // White queen takes undefended black rook on a5; no recapture.
        // FEN: white Qa1, Ka8 side; black Ra5, Kh8. Queen on a1 takes rook on a5.
        // Black king on h8 cannot reach a5. No defenders. SEE = 477.
        let (pos, mv) = parse_capture("7k/8/8/r7/8/8/8/Q3K3 w - - 0 1", "a1a5");
        assert_eq!(see(&pos, mv), 477, "Q takes hanging R = +477");
        assert_eq!(slow_see(&pos, mv), 477);
    }

    #[test]
    fn see_pawn_takes_hanging_queen() {
        // White pawn takes undefended black queen on d5; no recapture.
        // Sequence: Pxq. gain[0]=1025.
        // SEE = 1025.
        let (pos, mv) = parse_capture("4k3/8/8/3q4/4P3/8/8/4K3 w - - 0 1", "e4d5");
        assert_eq!(see(&pos, mv), 1025, "P takes hanging Q = +1025");
        assert_eq!(slow_see(&pos, mv), 1025);
    }

    #[test]
    fn see_knight_takes_hanging_bishop() {
        // White Ne3 takes undefended black Bd5; no recapture.
        // Ne3 attacks d5 (e3=file4,rank2 → d5=file3,rank4: delta=(-1,+2) ✓ knight move).
        // No black defenders of d5 other than the bishop itself.
        // Sequence: Nxb. gain[0]=365. SEE = 365.
        let (pos, mv) = parse_capture("4k3/8/8/3b4/8/4N3/8/4K3 w - - 0 1", "e3d5");
        assert_eq!(see(&pos, mv), 365, "N takes hanging B = +365");
        assert_eq!(slow_see(&pos, mv), 365);
    }

    #[test]
    fn see_knight_takes_pawn_defended_by_pawn_is_negative() {
        // White Ne3 takes black Pd5, black Pc6 recaptures.
        // Ne3→d5: delta=(-1,+2) ✓ valid knight move.
        // Sequence: Nxp(82), Pxn(337). Backup: gain[0]=82-max(0,337)=82-337=-255.
        // SEE = -255.
        let (pos, mv) = parse_capture("4k3/8/2p5/3p4/8/4N3/8/4K3 w - - 0 1", "e3d5");
        assert_eq!(see(&pos, mv), -255, "NxP/P should be -255");
        assert_eq!(slow_see(&pos, mv), -255);
    }

    #[test]
    fn see_rook_takes_pawn_defended_by_pawn_is_negative() {
        // White Rd1 takes black Pd4, black Pc5 recaptures.
        // Sequence: Rxp(82), Pxr(477). Backup: gain[0]=82-477=-395.
        // SEE = -395.
        let (pos, mv) = parse_capture("4k3/8/8/2p5/3p4/8/8/3RK3 w - - 0 1", "d1d4");
        assert_eq!(see(&pos, mv), -395, "RxP/P should be -395");
        assert_eq!(slow_see(&pos, mv), -395);
    }

    #[test]
    fn see_queen_takes_pawn_defended_by_rook_is_negative() {
        // White Qd4 takes black Pd5, black Rd7 recaptures white queen.
        // Sequence: Qxp(82), Rxq(1025). Backup: gain[0]=82-1025=-943.
        // SEE = -943.
        let (pos, mv) = parse_capture("4k3/3r4/8/3p4/3Q4/8/8/4K3 w - - 0 1", "d4d5");
        assert_eq!(see(&pos, mv), -943, "QxP/R should be -943");
        assert_eq!(slow_see(&pos, mv), -943);
    }

    #[test]
    fn see_pawn_takes_pawn_defended_by_pawn() {
        // White Pe4 takes black Pd5, black Pc6 recaptures.
        // Sequence: Pxp(82), Pxp(82). Backup: gain[0]=82-82=0.
        // SEE = 0 (equal trade).
        let (pos, mv) = parse_capture("4k3/8/2p5/3p4/4P3/8/8/4K3 w - - 0 1", "e4d5");
        assert_eq!(see(&pos, mv), 0, "PxP/P should be 0 (equal trade)");
        assert_eq!(slow_see(&pos, mv), 0);
    }

    #[test]
    fn see_xray_rook_battery() {
        // White Ra1, Ra3 on the a-file. Black Pa7, Ra8.
        // White Ra1 takes Pa7. Black Ra8 recaptures (gets Ra1=477).
        // White Ra3 is revealed through the vacated a1 square, recaptures (gets Ra8=477).
        // Sequence: Ra1xPa7(82), Ra8xRa7(477), Ra3xRa8_on_a7(477).
        // gain[0]=82, gain[1]=477 (Ra1 taken), gain[2]=477 (Ra8 taken).
        // Backup: gain[1]=477-max(0,477)=0. gain[0]=82-max(0,0)=82.
        // SEE = 82.
        //
        // Alternative (black stops at d=1 before white re-recaptures?):
        // The LVA at d=2 is white Ra3; black has no further recapture at d=3.
        // gain[2]=477 (white captures black rook). d=3: no black. Break. d=3.
        // Backup: gain[1]=477-max(0,477)=0. gain[0]=82-max(0,0)=82.
        // SEE = 82 (white netting the pawn value after the full rook exchange).
        // White Ra5 takes Pa7 (a5→a7, a6 is empty → legal).
        // Black Ra8 recaptures Ra5 (now on a7).
        // White Ra1 revealed: Ra5 vacated a5, so rook_attacks(a7, occ) south sees a1.
        // Ra1 recaptures Ra8 (now on a7). No more black. d=3.
        // gain[0]=82 (pawn), gain[1]=477 (Ra5), gain[2]=477 (Ra8).
        // Backup: gain[1]=477-max(0,477)=0. gain[0]=82-max(0,0)=82.
        // SEE = 82.
        let (pos, mv) = parse_capture("r6k/p7/8/R7/8/8/8/R6K w - - 0 1", "a5a7");
        assert_eq!(see(&pos, mv), 82, "rook battery SEE = pawn value 82");
        assert_eq!(slow_see(&pos, mv), 82);
    }

    #[test]
    fn see_xray_bishop_queen_battery() {
        // White Bc4, Qb3 on the b3-c4-d5 diagonal. Black Pd5, Ne6.
        // White Bc4 takes Pd5. Black Ne6 recaptures (gets Bc4=365).
        // White Qb3 revealed, recaptures (gets Ne6=337).
        // Sequence: Bxp(82), Nxb(365), Qxn(337).
        // gain[0]=82, gain[1]=365 (Bc4 taken by Ne6), gain[2]=337 (Ne6 taken by Qb3).
        // Backup: gain[1]=365-max(0,337)=365-337=28. gain[0]=82-max(0,28)=82-28=54.
        // SEE = 54.
        // Ne7 attacks d5: e7→d5 = (-1,-2) ✓ valid knight jump.
        let (pos, mv) = parse_capture("4k3/4n3/8/3p4/2B5/1Q6/8/4K3 w - - 0 1", "c4d5");
        assert_eq!(see(&pos, mv), 54, "bishop+queen battery: SEE = 54");
        assert_eq!(slow_see(&pos, mv), 54);
    }

    #[test]
    fn see_xray_stacked_defenders_outgun_single_attacker() {
        // White Re8 takes black Pe4; black has a rook battery — Re3 visible and
        // Re1 hidden behind Re3 on the e-file.  White has no further attacker.
        // Re1 is revealed as an x-ray defender after Re3 recaptures and vacates e3.
        //
        // FEN: `4R2k/8/8/8/4p3/4r3/7K/4r3 w - - 0 1`
        //   white Re8, Kh2;  black Pe4, Re3, Re1, Kh8.
        //   (Kh2, not h1: Re1 on e1 would give check along rank 1 to Kh1.)
        //
        // Move e8e4 (Re8 takes Pe4):
        //   d=0  Rxp:         gain[0]=82,  piece_on_sq=477 (white rook on e4)
        //         Re3 is visible; Re1 blocked by Re3 (e1→e2→e3=Re3).
        //   d=1  Re3xRe4:     gain[1]=477, piece_on_sq=477 (Re3 now on e4)
        //         X-ray: Re1 revealed (e3 vacated → e1→e2→e3(empty)→e4).
        //   d=2  no more white attackers → break.
        //
        // Backup: gain[0]=82−max(0,477)=−395. SEE = −395.
        //
        // The x-ray Re1 enters the attacker set correctly but white breaks first;
        // it would matter if white had a third attacker (the proptest covers that
        // space exhaustively; this test pins the negative SEE for the human reader
        // and confirms the rescan doesn't corrupt the result).
        let (pos, mv) = parse_capture("4R2k/8/8/8/4p3/4r3/7K/4r3 w - - 0 1", "e8e4");
        assert_eq!(
            see(&pos, mv),
            82 - 477,
            "single rook vs pawn defended by x-ray rook stack: SEE = −395"
        );
        assert_eq!(see(&pos, mv), slow_see(&pos, mv));
    }

    #[test]
    fn see_xray_attacker_flips_losing_to_winning() {
        // White Rc5 takes black Nc6; black Rc8 defends; white Rc3 is an x-ray
        // attacker hidden behind Rc5 on the c-file.  After Rc5 vacates c5,
        // Rc3 is immediately visible (c3→c4→c5(now empty)→c6).
        //
        // Without Rc3: Rxn(337), Rc8xRc6(477) → backup: 337−477 = −140 (losing).
        // With Rc3 x-ray: Rc8 recaptures but Rc3 then recaptures Rc8 — white nets
        //   a full knight at the cost of a rook-for-rook exchange (net = +knight).
        //
        // Position: `2r4k/8/2n5/2R5/8/2R5/8/7K w - - 0 1`
        //   Rank 8: 2r4k → c8=r(black), h8=k
        //   Rank 6: 2n5  → c6=n(black)
        //   Rank 5: 2R5  → c5=R(white)
        //   Rank 3: 2R5  → c3=R(white)
        //   Rank 1: 7K   → h1=K(white)
        //
        // Move c5c6 (Rc5 takes Nc6):
        //   d=0  Rxn:          gain[0]=337, piece_on_sq=477
        //         Remove Rc5; Rc3 revealed (c3→c4→c5(empty)→c6). ✓
        //   d=1  Rc8 (LVA):    gain[1]=477, piece_on_sq=477.  Remove Rc8.
        //   d=2  Rc3:          gain[2]=477, piece_on_sq=477.  Remove Rc3.
        //   d=3  no black → break.
        //
        // Backup: gain[1]=477−max(0,477)=0. gain[0]=337−max(0,0)=337. SEE = +337.
        let (pos, mv) = parse_capture("2r4k/8/2n5/2R5/8/2R5/8/7K w - - 0 1", "c5c6");
        assert_eq!(
            see(&pos, mv),
            337,
            "xray Rc3 flips RxN/Rc8 from −140 to +337"
        );
        assert_eq!(see(&pos, mv), slow_see(&pos, mv));
    }

    #[test]
    fn see_multi_attacker_lva_order() {
        // White Pd4, Nc3, Bb2 all attack e5 (hypothetical; verify geometry).
        // Actually: Pd4 attacks e5 (✓), Nc3 attacks d5/e4/b5/a4 — NOT e5.
        // Let's use Pd4 (attacks e5), Nf3 (attacks e5), Bb2 (attacks e5 via b2-e5 diagonal? b2=file1,rank1; e5=file4,rank4; diff=(3,3) yes diagonal).
        // Black Re5 (hanging). LVA order: pawn first, then knight, then bishop.
        // Sequence: Pxr(477). No recapture. SEE = 477.
        let (pos, mv) = parse_capture("4k3/8/8/4r3/3P4/5N2/1B6/4K3 w - - 0 1", "d4e5");
        assert_eq!(
            see(&pos, mv),
            477,
            "P takes hanging R with N,B available = 477"
        );
        assert_eq!(slow_see(&pos, mv), 477);
    }

    #[test]
    fn see_king_stop_branch_fires() {
        // Position designed so that after one exchange, white's ONLY remaining
        // attacker on d5 is its king (Kd4, adjacent), but black still has Qe6
        // on the diagonal — so the king-break branch fires.
        //
        // White: Ne3 (takes Pd5), Kd4.  Black: Pd5, Rd8, Qe6, Kh8.
        //
        //   • Ne3xPd5: gain[0]=82.  piece_on_sq=337 (knight on d5).
        //   • Black LVA = Rd8 (477 < Qe6=1025). gain[1]=337. piece_on_sq=477.
        //     Remove Rd8.
        //   • White: Kd4 adjacent to d5. King-stop check: Qe6 still attacks
        //     d5 (e6→d5 diagonal). Opponent has attacker → break.
        //   • d=2. Backup: gain[0]=82−max(0,337)=−255. SEE = −255.
        //
        // Kd4 legality: Rd8 blocked by Pd5; Qe6→d5 blocked by Pd5 (doesn't
        // reach d4); Ne3 doesn't attack d4. Position is legal.
        let (pos, mv) = parse_capture("3r3k/8/4q3/3p4/3K4/4N3/8/8 w - - 0 1", "e3d5");
        assert_eq!(
            see(&pos, mv),
            -255,
            "king-stop fires: Ne3xPd5/Rd8 Kd4-break"
        );
        assert_eq!(slow_see(&pos, mv), -255);
    }

    #[test]
    fn see_king_can_recapture_undefended_square() {
        // White king Ke4 takes black pawn on d5. No black defenders of d5.
        // Sequence: Kxp(82). No recapture. SEE = 82.
        let (pos, mv) = parse_capture("4k3/8/8/3p4/4K3/8/8/8 w - - 0 1", "e4d5");
        assert_eq!(see(&pos, mv), 82, "K takes undefended P = 82");
        assert_eq!(slow_see(&pos, mv), 82);
    }

    #[test]
    fn see_en_passant_basic() {
        // White Pd5 takes black Pe5 en passant, landing on e6. No defenders.
        // EP victim Pc5 removed from occ. gain[0]=82. SEE = 82.
        // FEN: white Pd5, black pe5; EP target e6.
        let (pos, mv) = parse_capture("4k3/8/8/3Pp3/8/8/8/4K3 w - e6 0 1", "d5e6");
        assert_eq!(see(&pos, mv), 82, "EP basic = 82");
        assert_eq!(slow_see(&pos, mv), 82);
    }

    #[test]
    fn see_en_passant_xray_opens_through_vacated_ep_square() {
        // White Pd5 takes black Pc5 EP → lands on c6.
        // After EP: c5 vacated (ep-victim) and d5 vacated (mover).
        // Black Rc8 attacks c6 (through empty c7). White Rc1 is revealed through
        // the vacated c5 (c1→c2→c3→c4→c5(now empty)→c6).
        //
        // Sequence:
        //   d=0  Pxp_ep:          gain[0]=82,  piece_on_sq=82  (white pawn on c6)
        //   d=1  Rc8xPc6:         gain[1]=82,  piece_on_sq=477 (rook on c6)
        //   d=2  Rc1xRc6 (xray):  gain[2]=477, piece_on_sq=477
        //   d=3  no black → break
        //
        // Backup: gain[1]=82−max(0,477)=−395. gain[0]=82−max(0,−395)=82.
        // SEE = 82.
        let (pos, mv) = parse_capture("2rk4/8/8/2pP4/8/8/8/2R1K3 w - c6 0 1", "d5c6");
        assert_eq!(see(&pos, mv), 82, "EP x-ray through c5: SEE = 82");
        assert_eq!(slow_see(&pos, mv), 82);
    }

    #[test]
    fn see_en_passant_black_captures_distinct_geometry() {
        // Black Ph4 captures white Pg4 EP, landing on g3 (b - side capture on
        // the g/h files — distinct from the two white EP tests on c/d files).
        //
        // EP victim formula: ep_victim_sq = rank_of(from)*8 + file_of(to).
        //   from=h4 (rank=3, file=7), to=g3 (rank=2, file=6).
        //   ep_victim_sq = 3*8 + 6 = 30 = g4. ✓  (white pawn on g4)
        //
        // A from/to swap bug would give rank_of(to)*8+file_of(from) = 2*8+7 = 23 = h3
        // (empty) — the white pawn on g4 would not be removed from occupancy, so the
        // x-ray white Rg1 would remain blocked and not enter the exchange, yielding a
        // wrong SEE (82 instead of 0).
        //
        // Position: black Ph4, white Pg4, white Rg1 (x-ray on g-file revealed when
        // g4 is vacated), black Ke8, white Kh8-side king on h1.
        //
        // Sequence:
        //   d=0  Ph4xPg4_EP:      gain[0]=82,  piece_on_sq=82  (black pawn on g3)
        //         g4 vacated → Rg1 now sees g3 via g1→g2→g3.
        //   d=1  Rg1 recaptures:  gain[1]=82,  piece_on_sq=477 (rook on g3)
        //   d=2  no black attacker → break
        //
        // Backup: gain[0]=82−max(0,82)=0. SEE = 0.
        let (pos, mv) = parse_capture("4k3/8/8/8/6Pp/8/8/4K1R1 b - g3 0 1", "h4g3");
        assert_eq!(see(&pos, mv), 0, "black EP Ph4xPg4/Rg1: SEE = 0");
        assert_eq!(slow_see(&pos, mv), 0);
    }

    #[test]
    fn see_promotion_capture_no_recapture() {
        // White Pg7 takes black Rh8, promotes to queen. No recapture.
        // gain[0] = 477 (rook) + (1025-82) (promo bonus) = 477+943 = 1420.
        // SEE = 1420.
        let (pos, mv) = parse_capture("7r/6P1/8/8/8/8/8/4K2k w - - 0 1", "g7h8q");
        assert_eq!(see(&pos, mv), 1420, "promo-capture no recapture = 1420");
        assert_eq!(slow_see(&pos, mv), 1420);
    }

    #[test]
    fn see_promotion_capture_with_recapture_still_good() {
        // White Pg7 takes black Rh8 (promo to queen). Black Rh1 recaptures queen.
        // gain[0]=1420 (rook+promo_bonus). piece_on_sq=1025 (queen).
        // d=1: Black Rh1. gain[1]=1025 (the queen). Backup: gain[0]=1420-max(0,1025)=395.
        // SEE = 395 (still positive: took rook+promo, lost queen, net = 477+943-1025 = 395).
        // Wait: 477 (rook taken) + 943 (promo bonus) - 1025 (queen lost) = 395. ✓
        // Black Rh8 is the victim. Black Rh5 recaptures on h8 (through empty h6,h7).
        // White Pg7 takes Rh8 (promo-cap to queen, gain[0]=1420).
        // Black Rh5 recaptures white queen on h8 (gain[1]=1025).
        // Backup: gain[0]=1420-1025=395. SEE = 395.
        let (pos, mv) = parse_capture("7r/6P1/8/8/7r/8/8/4K2k w - - 0 1", "g7h8q");
        assert_eq!(see(&pos, mv), 395, "promo-capture + recapture = 395");
        assert_eq!(slow_see(&pos, mv), 395);
    }

    #[test]
    fn see_pinned_defender_still_participates() {
        // Documents pin-ignoring semantics (§2).
        // Black bishop Bc6 is "pinned" on the diagonal to Ke8 by white Ba4
        // (a4→b3→c2 diagonal does NOT go through c6→e8, so this test needs
        // a proper pin setup).
        //
        // Instead: use a simpler arrangement where the "pinned" piece
        // participates as a recapturer. Both fast and slow ignore pins
        // (both use attackers_to which has no legal-move filter), so the
        // key property is fast == slow and both agree on participation.
        //
        // Setup: White Pe4 takes black Pd5. Black Bc6 is a potential
        // recapturer (c6 attacks d5 diagonally). Regardless of any "pin"
        // to black king, SEE includes Bc6 as an attacker.
        // Black king on e8 (not on the c6-d5 diagonal, so no real pin here,
        // but the point is the test documents that we don't check legality).
        // White pawn e4 takes black pawn d5. Black Bc6 recaptures.
        // gain[0]=82 (Pd5). gain[1]=82 (Pe4, taken by Bc6). Backup: gain[0]=82-82=0.
        // SEE = 0 (equal trade).
        // Sequence:
        //   d=0  Pe4xPd5:   gain[0]=82, piece_on_sq=82 (white pawn on d5)
        //   d=1  Bc6xPd5:   gain[1]=82, piece_on_sq=365 (bishop on d5)
        //   d=2  no white attacker → break
        //
        // Backup: gain[0]=82−max(0,82)=0. SEE = 0 (equal pawn trade; bishop
        // doesn't recapture again because white has no piece left).
        let (pos, mv) = parse_capture("4k3/8/2b5/3p4/4P3/8/8/4K3 w - - 0 1", "e4d5");
        let fast = see(&pos, mv);
        let slow = slow_see(&pos, mv);
        assert_eq!(fast, slow, "pinned defender: fast == slow");
        assert_eq!(
            fast, 0,
            "PxP/B = 0 (white P for black P, bishop doesn't recapture again)"
        );
    }

    #[test]
    fn see_equal_knight_trade() {
        // White Nc4 takes black Nd5. Black Ne7 (or another knight) recaptures.
        // Sequence: Nxn(337), Nxn(337). gain[0]=337, gain[1]=337.
        // Backup: gain[0]=337-337=0. SEE = 0.
        // White Ne3 takes black Nd5, black Ne7 recaptures (e7→d5 = (-1,-2) ✓).
        let (pos, mv) = parse_capture("4k3/4n3/8/3n4/8/4N3/8/4K3 w - - 0 1", "e3d5");
        assert_eq!(see(&pos, mv), 0, "NxN/N = 0 (equal trade)");
        assert_eq!(slow_see(&pos, mv), 0);
    }

    #[test]
    fn see_rook_takes_rook_equal_trade() {
        // White Ra5 takes black Ra7. Black Ra8 recaptures.
        // Sequence: Rxr(477), Rxr(477). gain[0]=477, gain[1]=477.
        // Backup: gain[0]=477-477=0. SEE = 0.
        let (pos, mv) = parse_capture("r6k/r7/8/R7/8/8/8/4K3 w - - 0 1", "a5a7");
        assert_eq!(see(&pos, mv), 0, "RxR/R = 0");
        assert_eq!(slow_see(&pos, mv), 0);
    }

    #[test]
    fn see_bishop_takes_knight_positive() {
        // White Bc4 takes black Nd5 (undefended). gain[0]=337. SEE = 337.
        let (pos, mv) = parse_capture("4k3/8/8/3n4/2B5/8/8/4K3 w - - 0 1", "c4d5");
        assert_eq!(see(&pos, mv), 337, "BxN undefended = 337");
        assert_eq!(slow_see(&pos, mv), 337);
    }

    #[test]
    fn see_rook_takes_knight_defended_by_bishop() {
        // White Rd4 takes black Nd5. Black Bc6 recaptures white rook.
        // Sequence: Rxn(337), Bxr(477). gain[0]=337, gain[1]=477.
        // Backup: gain[0]=337-477=-140. SEE = -140.
        let (pos, mv) = parse_capture("4k3/8/2b5/3n4/3R4/8/8/4K3 w - - 0 1", "d4d5");
        assert_eq!(see(&pos, mv), -140, "RxN/B = -140");
        assert_eq!(slow_see(&pos, mv), -140);
    }

    #[test]
    fn see_mid_sequence_promotion_bug_regression() {
        // Regression: a pawn promoting mid-sequence (not the initial move) must
        // credit the piece it captures PLUS the promotion bonus, not just queen
        // value alone.
        //
        // FEN: `rn5k/P7/8/8/8/8/7b/1R5K w - - 0 1`
        //   White: Rb1, Pa7, Kh1.   Black: Ra8, Nb8, Bh2, Kh8.
        //
        // Move: b1b8  (Rb1 captures Nb8).
        //
        // Sequence (piece values: P=82 N=337 B=365 R=477 Q=1025):
        //   d=0  Rxn:       gain[0] = 337          piece_on_sq = 477 (R on b8)
        //   d=1  Bh2xRb8:   gain[1] = 477 (R)      piece_on_sq = 365 (B on b8)
        //   d=2  Pa7xBb8=Q: gain[2] = 365+943=1308  piece_on_sq = 1025 (Q)
        //   d=3  Ra8xQb8:   gain[3] = 1025          piece_on_sq = 477 (R)
        //   d=4  no more white attackers → break.
        //
        // Backup: gain[2]=1308−max(0,1025)=283
        //         gain[1]=477−max(0,283)=194
        //         gain[0]=337−max(0,194)=143
        // SEE = 143.
        //
        // Bug (pre-fix): gain[2] was set to SEE_VALUE[Queen]=1025 instead of
        // piece_on_sq + promo_bonus = 365+943=1308, yielding SEE = −140.
        let (pos, mv) = parse_capture("rn5k/P7/8/8/8/8/7b/1R5K w - - 0 1", "b1b8");
        assert_eq!(see(&pos, mv), 143, "mid-sequence promo: SEE = 143");
        assert_eq!(slow_see(&pos, mv), 143);
    }

    // -----------------------------------------------------------------------
    // Arasan / TalkChess t=69052 validation suite
    //
    // 22 of the 25 Arasan positions are capture moves compatible with our see()
    // interface (see() requires is_capture()).  PR-1, PR-2, PR-3 are
    // non-capture promotions (pawn to empty back-rank square) — they require a
    // SEE variant for quiet promotions that is out of scope for M7.A.
    //
    // Expected values are converted from symbolic to our SEE_VALUE scale:
    //   P=82, N=337, B=365, R=477, Q=1025, K=30000.
    //
    // Source: TalkChess t=69052 (Jon Dart / Arasan suite).
    //         ADR-0003: positions reproduced from forum prose, no source read.
    // -----------------------------------------------------------------------

    // --- X-Ray positions ---

    #[test]
    fn arasan_xr1_pawn_captures_pawn_xray() {
        // XR-1: hxg4; multiple sliders involved; net = 0.
        let (pos, mv) = parse_capture(
            "4R3/2r3p1/5bk1/1p1r3p/p2PR1P1/P1BK1P2/1P6/8 b - - 0 1",
            "h5g4",
        );
        assert_eq!(see(&pos, mv), 0, "XR-1 expected 0");
        assert_eq!(see(&pos, mv), slow_see(&pos, mv));
    }

    #[test]
    fn arasan_xr2_pawn_captures_pawn_xray_variant() {
        // XR-2: same structure, different pawn; net = 0.
        let (pos, mv) = parse_capture(
            "4R3/2r3p1/5bk1/1p1r1p1p/p2PR1P1/P1BK1P2/1P6/8 b - - 0 1",
            "h5g4",
        );
        assert_eq!(see(&pos, mv), 0, "XR-2 expected 0");
        assert_eq!(see(&pos, mv), slow_see(&pos, mv));
    }

    // XR-3, XR-4, XR-5: Re1→e8, but e8 is EMPTY in all three FENs — these are
    // non-capture rook moves, not valid inputs for see() (which requires
    // is_capture()).  Reported to orchestrator for adjudication; positions likely
    // have a transcription error in the source suite (the attacking rook may
    // intend a different target square where a piece sits).  Skipped pending
    // corrected FENs.

    #[test]
    fn arasan_xr6_rook_captures_bishop_xray_rook() {
        // XR-6: Rc3xBc5; Rc2 revealed via x-ray; net = +value[bishop] = +365.
        let (pos, mv) = parse_capture(
            "2r4k/2r4p/p7/2b2p1b/4pP2/1BR5/P1R3PP/2Q4K w - - 0 1",
            "c3c5",
        );
        assert_eq!(see(&pos, mv), 365, "XR-6 expected +365");
        assert_eq!(see(&pos, mv), slow_see(&pos, mv));
    }

    #[test]
    fn arasan_xr7_bishop_captures_pawn_net_negative() {
        // XR-7: Bg2xPc6; second defender tips it negative;
        // net = value[pawn]−value[bishop] = 82−365 = −283.
        let (pos, mv) = parse_capture("8/pp6/2pkp3/4bp2/2R3b1/2P5/PP4B1/1K6 w - - 0 1", "g2c6");
        assert_eq!(see(&pos, mv), 82 - 365, "XR-7 expected −283");
        assert_eq!(see(&pos, mv), slow_see(&pos, mv));
    }

    #[test]
    fn arasan_xr8_rook_captures_pawn_negative() {
        // XR-8: Re6xPe4; net = value[pawn]−value[rook] = 82−477 = −395.
        let (pos, mv) = parse_capture(
            "4q3/1p1pr1k1/1B2rp2/6p1/p3PP2/P3R1P1/1P2R1K1/4Q3 b - - 0 1",
            "e6e4",
        );
        assert_eq!(see(&pos, mv), 82 - 477, "XR-8 expected −395");
        assert_eq!(see(&pos, mv), slow_see(&pos, mv));
    }

    #[test]
    fn arasan_xr9_bishop_captures_pawn_positive() {
        // XR-9: Bh7xPe4; different first attacker changes outcome to +value[pawn]=+82.
        let (pos, mv) = parse_capture(
            "4q3/1p1pr1kb/1B2rp2/6p1/p3PP2/P3R1P1/1P2R1K1/4Q3 b - - 0 1",
            "h7e4",
        );
        assert_eq!(see(&pos, mv), 82, "XR-9 expected +82");
        assert_eq!(see(&pos, mv), slow_see(&pos, mv));
    }

    // --- Promotion-capture positions (PR-1, PR-2, PR-3 are non-capture
    //     promotions and require a quiet-promotion SEE extension — skipped.) ---

    #[test]
    fn arasan_pr4_rook_captures_rook_promo_threat() {
        // PR-4: Rc3xRc8; Pb7 can promote after exchange; net = +value[rook] = +477.
        let (pos, mv) = parse_capture(
            "2r4r/1P4pk/p2p1b1p/7n/BB3p2/2R2p2/P1P2P2/4RK2 w - - 0 1",
            "c3c8",
        );
        assert_eq!(see(&pos, mv), 477, "PR-4 expected +477");
        assert_eq!(see(&pos, mv), slow_see(&pos, mv));
    }

    #[test]
    fn arasan_pr5_rook_captures_rook_promo_threat_no_second_rook() {
        // PR-5: Rc3xRc8 (no black Rh8); promotion threat still wins a rook = +477.
        let (pos, mv) = parse_capture(
            "2r5/1P4pk/p2p1b1p/5b1n/BB3p2/2R2p2/P1P2P2/4RK2 w - - 0 1",
            "c3c8",
        );
        assert_eq!(see(&pos, mv), 477, "PR-5 expected +477");
        assert_eq!(see(&pos, mv), slow_see(&pos, mv));
    }

    #[test]
    fn arasan_pr6_queen_captures_knight_promo_rescues() {
        // PR-6: Qb2xNb8; Ra8 recaptures queen; Pa7 promotes on b8 rescuing the
        // exchange.  Expected = +value[knight] = +337 (on our scale).
        //
        // Sequence (fixed code):
        //   d=0  Qxn: gain[0]=337,   piece_on_sq=1025
        //   d=1  Ra8xQ: gain[1]=1025, piece_on_sq=477
        //   d=2  Pa7xR=Q (promo on b8): gain[2]=477+943=1420, piece_on_sq=1025
        //   d=3  no more black → break.
        //   Backup: gain[1]=1025−max(0,1420)=−395 → gain[0]=337−max(0,−395)=337.
        // SEE = 337.
        let (pos, mv) = parse_capture("rn6/P7/K7/8/8/6k1/1Q6/8 w - - 0 1", "b2b8");
        assert_eq!(see(&pos, mv), 337, "PR-6 expected +337");
        assert_eq!(see(&pos, mv), slow_see(&pos, mv));
    }

    // --- En passant positions ---

    #[test]
    fn arasan_ep1_en_passant_one_defender() {
        // EP-1: g5xh6 en passant; one defender; net = 0.
        let (pos, mv) = parse_capture(
            "3q2nk/pb1r1p2/np6/3P2Pp/2p1P3/2R4B/PQ3P1P/3R2K1 w - h6 0 1",
            "g5h6",
        );
        assert_eq!(see(&pos, mv), 0, "EP-1 expected 0");
        assert_eq!(see(&pos, mv), slow_see(&pos, mv));
    }

    #[test]
    fn arasan_ep2_en_passant_extra_bishop_defender() {
        // EP-2: g5xh6 en passant; extra white bishop enters; net = +value[pawn] = +82.
        let (pos, mv) = parse_capture(
            "3q2nk/pb1r1p2/np6/3P2Pp/2p1P3/2R1B2B/PQ3P1P/3R2K1 w - h6 0 1",
            "g5h6",
        );
        assert_eq!(see(&pos, mv), 82, "EP-2 expected +82");
        assert_eq!(see(&pos, mv), slow_see(&pos, mv));
    }

    // --- Multi-attacker positions ---

    #[test]
    fn arasan_ma1_bishop_captures_knight_scale_divergence() {
        // MA-1 (Arasan): Bg4xNf3. The Arasan suite gives the SYMBOLIC answer 0
        // ("equal trade"), which assumes Bishop == Knight (the CPW 325/325
        // illustrative scale). Our SEE_VALUE is PeSTO-derived with B=365 > N=337,
        // so the answer is correctly −28 (= N − B = 337 − 365) on OUR scale, not
        // a bug. Adjudicated 2026-06-15: assert the value correct for our scale.
        //
        // Sequence on f3: Bg4xN (gain N=337), g2-pawn xB (gain B=365), Qh5xP
        // (gain P=82), Qe2xQ (gain Q=1025). Raw gains [337,365,82,1025] →
        // backup: gain[2]=82−1025→−943; gain[1]=365−0=365; gain[0]=337−365=−28.
        // With B==N the gain[0] step is 0; the entire divergence is the B−N gap.
        let (pos, mv) = parse_capture(
            "4r1k1/5pp1/nbp4p/1p2p2q/1P2P1b1/1BP2N1P/1B2QPPK/3R4 b - - 0 1",
            "g4f3",
        );
        assert_eq!(
            see(&pos, mv),
            -28,
            "MA-1 = −28 on our B≠N scale (Arasan's symbolic 0 assumes B==N)"
        );
        assert_eq!(see(&pos, mv), slow_see(&pos, mv));
    }

    #[test]
    fn arasan_ma2_pawn_captures_pawn_multi_attacker() {
        // MA-2: d6xe5; Black wins a pawn; net = +value[pawn] = +82.
        let (pos, mv) = parse_capture(
            "2r1r1k1/pp1bppbp/3p1np1/q3P3/2P2P2/1P2B3/P1N1B1PP/2RQ1RK1 b - - 0 1",
            "d6e5",
        );
        assert_eq!(see(&pos, mv), 82, "MA-2 expected +82");
        assert_eq!(see(&pos, mv), slow_see(&pos, mv));
    }

    #[test]
    fn arasan_ma3_knight_captures_pawn_multi_attacker() {
        // MA-3: Nf3xe5; White wins a pawn; net = +value[pawn] = +82.
        let (pos, mv) = parse_capture(
            "r2qk1nr/pp2ppbp/2b3p1/2p1p3/8/2N2N2/PPPP1PPP/R1BQR1K1 w kq - 0 1",
            "f3e5",
        );
        assert_eq!(see(&pos, mv), 82, "MA-3 expected +82");
        assert_eq!(see(&pos, mv), slow_see(&pos, mv));
    }

    #[test]
    fn arasan_ma4_bishop_captures_pawn_balanced() {
        // MA-4: Bf4xe5; balanced exchange; net = 0.
        let (pos, mv) = parse_capture(
            "6r1/4kq2/b2p1p2/p1pPb3/p1P2B1Q/2P4P/2B1R1P1/6K1 w - - 0 1",
            "f4e5",
        );
        assert_eq!(see(&pos, mv), 0, "MA-4 expected 0");
        assert_eq!(see(&pos, mv), slow_see(&pos, mv));
    }

    #[test]
    fn arasan_ma5_queen_captures_rook_two_rooks_for_queen() {
        // MA-5: Qc5xRc1; full exchange nets 2R−Q = 2×477−1025 = −71.
        // (On our scale Q > 2R, so this is negative even though the note says
        // "positive for Black" in a 100/500/900 scale.)
        let (pos, mv) = parse_capture(
            "2r2r1k/6bp/p7/2q2p1Q/3PpP2/1B6/P5PP/2RR3K b - - 0 1",
            "c5c1",
        );
        assert_eq!(see(&pos, mv), 2 * 477 - 1025, "MA-5 expected −71");
        assert_eq!(see(&pos, mv), slow_see(&pos, mv));
    }

    #[test]
    fn arasan_ma6_rook_captures_rook_with_queen_present() {
        // MA-6: Rh1xRf1; equal rook trade despite queen nearby; net = 0.
        let (pos, mv) = parse_capture(
            "8/4kp2/2npp3/1Nn5/1p2PQP1/7q/1PP1B3/4KR1r b - - 0 1",
            "h1f1",
        );
        assert_eq!(see(&pos, mv), 0, "MA-6 expected 0");
        assert_eq!(see(&pos, mv), slow_see(&pos, mv));
    }

    #[test]
    fn arasan_ma7_rook_captures_rook_no_queen() {
        // MA-7: same as MA-6 minus White queen; result still 0.
        let (pos, mv) = parse_capture(
            "8/4kp2/2npp3/1Nn5/1p2P1P1/7q/1PP1B3/4KR1r b - - 0 1",
            "h1f1",
        );
        assert_eq!(see(&pos, mv), 0, "MA-7 expected 0");
        assert_eq!(see(&pos, mv), slow_see(&pos, mv));
    }

    // -----------------------------------------------------------------------
    // Differential proptest: see() == slow_see() for all captures in random positions
    // -----------------------------------------------------------------------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2048))]
        #[test]
        fn prop_see_equals_slow_see(pos in arb_position()) {
            let mut ml = MoveList::new();
            generate_moves(&pos, &mut ml);
            for mv in ml.iter() {
                if !mv.is_capture() {
                    continue;
                }
                let fast = see(&pos, mv);
                let slow = slow_see(&pos, mv);
                prop_assert_eq!(
                    fast,
                    slow,
                    "see vs slow_see mismatch on {} move {:?}: fast={} slow={}",
                    pos.to_fen(),
                    mv,
                    fast,
                    slow,
                );
            }
        }
    }
}
