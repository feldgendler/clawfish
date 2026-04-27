//! Per-call mask bundle for legal-direct movegen.
//!
//! See `docs/plans/m1.f.md` §4–§7 for the full design.
//!
//! - `MaskInfo::compute(pos, us, king_sq)` produces the per-call bundle:
//!   `checkers`, `pinned`, `capture_mask`, `push_mask`, `king_danger`,
//!   `pin_rays[64]`.
//! - `checkers_of(pos, us, king_sq, occ)` — pieces of `us.flip()` whose
//!   attack set contains `king_sq`. Parameterized on `occ` so the king-flee
//!   path can pass `occ ^ king_bb`.
//! - `compute_attacked_squares(pos, attacker, occ)` — union of every
//!   square at least one `attacker`-color piece attacks (used for
//!   `king_danger`). Distinct from `checkers_of`: enumerates *every*
//!   opponent piece kind (including the king for adjacency).
//! - Pre-computed leaf-piece attack tables (`PAWN_ATTACKS`, `KNIGHT_ATTACKS`,
//!   `KING_ATTACKS`) — `const fn`-built `static` tables. `PAWN_ATTACKS` is
//!   indexed by **attacker color**: `PAWN_ATTACKS[us][sq]` is the squares an
//!   own-color pawn on `sq` would attack.

use crate::bitboard::Bitboard;
use crate::magic;
use crate::piece::{Color, PieceKind};
use crate::position::Position;
use crate::square::Square;

/// Per-call mask bundle. Recomputed at the top of every `generate_moves`
/// call (no cache on `Position` — see ADR-0007 §"Why recompute pin/checker
/// per-call").
pub(crate) struct MaskInfo {
    pub checkers: Bitboard,
    pub pinned: Bitboard,
    /// `enemy_occupied` (no check), `checkers` (single check), or
    /// `Bitboard::EMPTY` (double check).
    pub capture_mask: Bitboard,
    /// `~all_occupancy` (no check), `ray_between(king, slider_checker)`
    /// (single slider check), or `Bitboard::EMPTY`.
    pub push_mask: Bitboard,
    /// Squares attacked by the opponent on `occupancy ^ king_bb`. Used by
    /// king-move filtering (the king cannot move to any square in this
    /// set). Computed against `occ ^ king_bb` so a slider that's checking
    /// through the king's current square still attacks the square the
    /// king would flee to.
    pub king_danger: Bitboard,
    /// For each pinned own piece, the king-pinner ray. Indexed by
    /// `Square::index()`. Entries for non-pinned squares are
    /// `Bitboard::EMPTY` (untouched by `compute`); callers must check
    /// `pinned.contains(sq)` first.
    pub pin_rays: [Bitboard; 64],
}

impl MaskInfo {
    /// Compute the mask bundle for `pos` with `us` to move and king on
    /// `king_sq`. ~~~30 attack lookups + pin-loop. Sub-microsecond on
    /// Apple Silicon.
    pub(crate) fn compute(pos: &Position, us: Color, king_sq: Square) -> MaskInfo {
        let them = us.flip();
        let all_occ = pos.occupied_all();
        let king_bb = Bitboard::from_square(king_sq);

        let checkers = checkers_of(pos, us, king_sq, all_occ);

        let mut pinned = Bitboard::EMPTY;
        let mut pin_rays = [Bitboard::EMPTY; 64];

        // Rook-direction pinner candidates: opponent rook+queen on king's
        // rank/file ray. We sweep candidates via empty-board rook attacks
        // from king_sq, intersected with opponent rook+queen.
        let opp_rq =
            pos.pieces_colored(them, PieceKind::Rook) | pos.pieces_colored(them, PieceKind::Queen);
        let rook_xray = magic::rook_attacks(king_sq, Bitboard::EMPTY) & opp_rq;
        for slider_sq in rook_xray.iter() {
            let between = between_bb(king_sq, slider_sq) & all_occ;
            if between.count() == 1 {
                let mid = between.lsb().expect("count == 1 implies lsb is Some");
                if pos.occupied(us).contains(mid) {
                    pinned = pinned.with(mid);
                    pin_rays[mid.index() as usize] = line_bb(king_sq, slider_sq);
                }
            }
        }

        // Bishop-direction pinner candidates.
        let opp_bq = pos.pieces_colored(them, PieceKind::Bishop)
            | pos.pieces_colored(them, PieceKind::Queen);
        let bishop_xray = magic::bishop_attacks(king_sq, Bitboard::EMPTY) & opp_bq;
        for slider_sq in bishop_xray.iter() {
            let between = between_bb(king_sq, slider_sq) & all_occ;
            if between.count() == 1 {
                let mid = between.lsb().expect("count == 1 implies lsb is Some");
                if pos.occupied(us).contains(mid) {
                    pinned = pinned.with(mid);
                    pin_rays[mid.index() as usize] = line_bb(king_sq, slider_sq);
                }
            }
        }

        let n_checkers = checkers.count();
        let (capture_mask, push_mask) = match n_checkers {
            0 => (pos.occupied(them), !all_occ),
            1 => {
                let checker_sq = checkers.lsb().expect("count == 1 implies lsb is Some");
                let push = if is_slider_at(pos, them, checker_sq) {
                    between_bb(king_sq, checker_sq)
                } else {
                    Bitboard::EMPTY
                };
                (checkers, push)
            }
            _ => (Bitboard::EMPTY, Bitboard::EMPTY),
        };

        let king_danger = compute_attacked_squares(pos, them, all_occ ^ king_bb);

        MaskInfo {
            checkers,
            pinned,
            capture_mask,
            push_mask,
            king_danger,
            pin_rays,
        }
    }
}

/// Whether the piece at `sq` (assumed to be of color `color`) is a slider
/// (bishop, rook, or queen). Used to pick `push_mask` in the single-check
/// branch.
#[inline]
fn is_slider_at(pos: &Position, color: Color, sq: Square) -> bool {
    pos.pieces_colored(color, PieceKind::Bishop).contains(sq)
        || pos.pieces_colored(color, PieceKind::Rook).contains(sq)
        || pos.pieces_colored(color, PieceKind::Queen).contains(sq)
}

/// Pieces of `us.flip()` whose attack set contains `king_sq` under
/// occupancy `occupancy`. Parameterized on `occupancy` so the king-flee
/// path can pass `occupancy ^ king_bb` (see ADR-0007 §"King-flee gotcha").
pub(crate) fn checkers_of(
    pos: &Position,
    us: Color,
    king_sq: Square,
    occupancy: Bitboard,
) -> Bitboard {
    let them = us.flip();
    let mut chk = Bitboard::EMPTY;
    // Pawns: pretend our own-color pawn stands on king_sq and shoots its
    // attack pattern; the squares it would land on are exactly the squares
    // an opponent pawn would attack king_sq from.
    chk |= pawn_attacks_from(king_sq, us) & pos.pieces_colored(them, PieceKind::Pawn);
    chk |= knight_attacks(king_sq) & pos.pieces_colored(them, PieceKind::Knight);
    let diag =
        pos.pieces_colored(them, PieceKind::Bishop) | pos.pieces_colored(them, PieceKind::Queen);
    chk |= magic::bishop_attacks(king_sq, occupancy) & diag;
    let orth =
        pos.pieces_colored(them, PieceKind::Rook) | pos.pieces_colored(them, PieceKind::Queen);
    chk |= magic::rook_attacks(king_sq, occupancy) & orth;
    chk
}

/// Union of every square at least one `attacker`-color piece attacks under
/// `occupancy`. Enumerates every piece kind including the **king** (kings
/// attack their 8 neighbors; we cannot move our king there). Distinct from
/// `checkers_of` — `checkers_of` returns *attacking pieces whose attack
/// hits one square*; this returns *every square attacked by at least one
/// piece*.
pub(crate) fn compute_attacked_squares(
    pos: &Position,
    attacker: Color,
    occupancy: Bitboard,
) -> Bitboard {
    let mut attacked = Bitboard::EMPTY;

    // Pawns: shift the whole pawn bitboard by the two attack diagonals.
    let pawns = pos.pieces_colored(attacker, PieceKind::Pawn);
    let pawn_attacks = match attacker {
        Color::White => pawns.shift_north_east() | pawns.shift_north_west(),
        Color::Black => pawns.shift_south_east() | pawns.shift_south_west(),
    };
    attacked |= pawn_attacks;

    // Knights.
    for sq in pos.pieces_colored(attacker, PieceKind::Knight).iter() {
        attacked |= knight_attacks(sq);
    }

    // Bishops + queens (diagonal sliders).
    let diag = pos.pieces_colored(attacker, PieceKind::Bishop)
        | pos.pieces_colored(attacker, PieceKind::Queen);
    for sq in diag.iter() {
        attacked |= magic::bishop_attacks(sq, occupancy);
    }

    // Rooks + queens (orthogonal sliders).
    let orth = pos.pieces_colored(attacker, PieceKind::Rook)
        | pos.pieces_colored(attacker, PieceKind::Queen);
    for sq in orth.iter() {
        attacked |= magic::rook_attacks(sq, occupancy);
    }

    // Opponent king — its 8 neighbors. Required so our king cannot move
    // adjacent to the opposing king.
    let king_sq = pos.king_square(attacker);
    attacked |= king_attacks(king_sq);

    attacked
}

/// Squares an own-color pawn standing on `sq` would attack. Read by
/// `PAWN_ATTACKS[us.index()][sq.index()]`. The own-color-attacker
/// orientation is the load-bearing pin: ANDing the result against the
/// opponent's pawn bitboard yields opponent pawns whose attack ray hits
/// `sq` (used by `checkers_of`).
#[inline]
pub(crate) fn pawn_attacks_from(sq: Square, us: Color) -> Bitboard {
    PAWN_ATTACKS[us.index()][sq.index() as usize]
}

#[inline]
pub(crate) fn knight_attacks(sq: Square) -> Bitboard {
    KNIGHT_ATTACKS[sq.index() as usize]
}

#[inline]
pub(crate) fn king_attacks(sq: Square) -> Bitboard {
    KING_ATTACKS[sq.index() as usize]
}

/// `[white_attacks; black_attacks]` indexed by attacker color, then square.
/// Built at compile time via `const fn`.
pub(crate) static PAWN_ATTACKS: [[Bitboard; 64]; 2] = build_pawn_attacks();

pub(crate) static KNIGHT_ATTACKS: [Bitboard; 64] = build_knight_attacks();

pub(crate) static KING_ATTACKS: [Bitboard; 64] = build_king_attacks();

const fn build_pawn_attacks() -> [[Bitboard; 64]; 2] {
    let mut tables = [[Bitboard::EMPTY; 64]; 2];
    let mut i = 0u8;
    while i < 64 {
        let file = i % 8;
        let rank = i / 8;
        // White pawn on `i` attacks NW (file-1, rank+1) and NE (file+1, rank+1).
        let mut white = Bitboard::EMPTY;
        if file > 0 && rank < 7 {
            // NW = i + 7
            white = white.with(Square::new_unchecked(i + 7));
        }
        if file < 7 && rank < 7 {
            // NE = i + 9
            white = white.with(Square::new_unchecked(i + 9));
        }
        tables[Color::White.index()][i as usize] = white;

        // Black pawn on `i` attacks SW (file-1, rank-1) and SE (file+1, rank-1).
        let mut black = Bitboard::EMPTY;
        if file > 0 && rank > 0 {
            // SW = i - 9
            black = black.with(Square::new_unchecked(i - 9));
        }
        if file < 7 && rank > 0 {
            // SE = i - 7
            black = black.with(Square::new_unchecked(i - 7));
        }
        tables[Color::Black.index()][i as usize] = black;

        i += 1;
    }
    tables
}

const fn build_knight_attacks() -> [Bitboard; 64] {
    let mut table = [Bitboard::EMPTY; 64];
    // 8 L-jump deltas: (df, dr) pairs.
    let deltas: [(i8, i8); 8] = [
        (-2, -1),
        (-2, 1),
        (-1, -2),
        (-1, 2),
        (1, -2),
        (1, 2),
        (2, -1),
        (2, 1),
    ];
    let mut i = 0u8;
    while i < 64 {
        let file = (i % 8) as i8;
        let rank = (i / 8) as i8;
        let mut bb = Bitboard::EMPTY;
        let mut k = 0;
        while k < 8 {
            let (df, dr) = deltas[k];
            let nf = file + df;
            let nr = rank + dr;
            if nf >= 0 && nf < 8 && nr >= 0 && nr < 8 {
                bb = bb.with(Square::new_unchecked((nr * 8 + nf) as u8));
            }
            k += 1;
        }
        table[i as usize] = bb;
        i += 1;
    }
    table
}

const fn build_king_attacks() -> [Bitboard; 64] {
    let mut table = [Bitboard::EMPTY; 64];
    let deltas: [(i8, i8); 8] = [
        (-1, -1),
        (-1, 0),
        (-1, 1),
        (0, -1),
        (0, 1),
        (1, -1),
        (1, 0),
        (1, 1),
    ];
    let mut i = 0u8;
    while i < 64 {
        let file = (i % 8) as i8;
        let rank = (i / 8) as i8;
        let mut bb = Bitboard::EMPTY;
        let mut k = 0;
        while k < 8 {
            let (df, dr) = deltas[k];
            let nf = file + df;
            let nr = rank + dr;
            if nf >= 0 && nf < 8 && nr >= 0 && nr < 8 {
                bb = bb.with(Square::new_unchecked((nr * 8 + nf) as u8));
            }
            k += 1;
        }
        table[i as usize] = bb;
        i += 1;
    }
    table
}

/// Squares strictly between `from` and `to` along their shared rank, file,
/// or diagonal. `Bitboard::EMPTY` if `from` and `to` are not collinear by
/// rook or bishop semantics, or if they are equal.
#[inline]
pub(crate) fn between_bb(from: Square, to: Square) -> Bitboard {
    if from.index() == to.index() {
        return Bitboard::EMPTY;
    }
    let from_file = from.file() as i8;
    let from_rank = from.rank() as i8;
    let to_file = to.file() as i8;
    let to_rank = to.rank() as i8;
    let df = to_file - from_file;
    let dr = to_rank - from_rank;
    // Collinear iff: same rank (dr == 0), same file (df == 0), or |df| == |dr|.
    let step_f;
    let step_r;
    if df == 0 && dr != 0 {
        step_f = 0i8;
        step_r = dr.signum();
    } else if dr == 0 && df != 0 {
        step_f = df.signum();
        step_r = 0i8;
    } else if df.abs() == dr.abs() && df != 0 {
        step_f = df.signum();
        step_r = dr.signum();
    } else {
        return Bitboard::EMPTY;
    }
    let mut bb = Bitboard::EMPTY;
    let mut f = from_file + step_f;
    let mut r = from_rank + step_r;
    while !(f == to_file && r == to_rank) {
        // Safe by construction: `from` and `to` are on-board, and stepping
        // a unit step at a time toward `to` keeps coordinates in 0..8.
        bb = bb.with(Square::new_unchecked((r * 8 + f) as u8));
        f += step_f;
        r += step_r;
    }
    bb
}

/// Squares on the line through `from` and `to` (inclusive of both
/// endpoints, extended to the board edges). Used to compute pin rays:
/// when a slider pins a piece, the pinned piece may move along
/// `line_bb(king_sq, slider_sq)`.
///
/// `Bitboard::EMPTY` if the squares are not collinear.
#[inline]
pub(crate) fn line_bb(from: Square, to: Square) -> Bitboard {
    if from.index() == to.index() {
        return Bitboard::EMPTY;
    }
    let from_file = from.file() as i8;
    let from_rank = from.rank() as i8;
    let to_file = to.file() as i8;
    let to_rank = to.rank() as i8;
    let df = to_file - from_file;
    let dr = to_rank - from_rank;
    let step_f;
    let step_r;
    if df == 0 && dr != 0 {
        step_f = 0i8;
        step_r = 1i8;
    } else if dr == 0 && df != 0 {
        step_f = 1i8;
        step_r = 0i8;
    } else if df.abs() == dr.abs() && df != 0 {
        // Diagonal: pick a canonical direction; we walk both ways below.
        step_f = df.signum();
        step_r = dr.signum();
    } else {
        return Bitboard::EMPTY;
    }
    // Sweep both directions from `from` collecting on-board squares.
    let mut bb = Bitboard::EMPTY.with(from);
    // Forward.
    let mut f = from_file + step_f;
    let mut r = from_rank + step_r;
    while (0..8).contains(&f) && (0..8).contains(&r) {
        bb = bb.with(Square::new_unchecked((r * 8 + f) as u8));
        f += step_f;
        r += step_r;
    }
    // Backward.
    let mut f = from_file - step_f;
    let mut r = from_rank - step_r;
    while (0..8).contains(&f) && (0..8).contains(&r) {
        bb = bb.with(Square::new_unchecked((r * 8 + f) as u8));
        f -= step_f;
        r -= step_r;
    }
    bb
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::piece::Color;
    use crate::position::Position;
    use crate::square::Square;

    /// Helper: build a `Bitboard` from a list of squares.
    fn bb(squares: &[Square]) -> Bitboard {
        let mut b = Bitboard::EMPTY;
        for &sq in squares {
            b = b.with(sq);
        }
        b
    }

    // ------------------------------------------------------------------------
    // Pawn-attack pre-computed table tests.
    // ------------------------------------------------------------------------

    #[test]
    fn pawn_attacks_starting_rank() {
        // White pawn on a2 attacks b3 (only).
        assert_eq!(
            pawn_attacks_from(Square::A2, Color::White),
            bb(&[Square::B3]),
            "white pawn on a2 should attack only b3"
        );
        // Black pawn on a7 attacks b6 (only). Pin attacker-color orientation:
        // PAWN_ATTACKS[us][sq] is the squares an own-color pawn on sq attacks.
        assert_eq!(
            pawn_attacks_from(Square::A7, Color::Black),
            bb(&[Square::B6]),
            "black pawn on a7 should attack only b6"
        );
    }

    #[test]
    fn pawn_attacks_corner_files() {
        // a-file white pawn cannot attack to its left (off-board).
        assert_eq!(
            pawn_attacks_from(Square::A2, Color::White),
            bb(&[Square::B3]),
            "a-file white pawn should have only one attack target"
        );
        // h-file white pawn cannot attack to its right (off-board).
        assert_eq!(
            pawn_attacks_from(Square::H2, Color::White),
            bb(&[Square::G3]),
            "h-file white pawn should have only one attack target"
        );
        // Mirror for black.
        assert_eq!(
            pawn_attacks_from(Square::A7, Color::Black),
            bb(&[Square::B6]),
            "a-file black pawn should have only one attack target"
        );
        assert_eq!(
            pawn_attacks_from(Square::H7, Color::Black),
            bb(&[Square::G6]),
            "h-file black pawn should have only one attack target"
        );
    }

    #[test]
    fn pawn_attacks_central() {
        // White pawn on d4 attacks c5 and e5.
        assert_eq!(
            pawn_attacks_from(Square::D4, Color::White),
            bb(&[Square::C5, Square::E5]),
            "white pawn on d4 should attack c5 and e5"
        );
        // Black pawn on d4 attacks c3 and e3.
        assert_eq!(
            pawn_attacks_from(Square::D4, Color::Black),
            bb(&[Square::C3, Square::E3]),
            "black pawn on d4 should attack c3 and e3"
        );
    }

    // ------------------------------------------------------------------------
    // Knight-attack pre-computed table tests.
    // ------------------------------------------------------------------------

    #[test]
    fn knight_attacks_central() {
        // d4 knight has 8 targets.
        let expected = bb(&[
            Square::B3,
            Square::C2,
            Square::E2,
            Square::F3,
            Square::F5,
            Square::E6,
            Square::C6,
            Square::B5,
        ]);
        assert_eq!(
            knight_attacks(Square::D4),
            expected,
            "central knight on d4 should attack 8 squares"
        );
        assert_eq!(knight_attacks(Square::D4).count(), 8);
    }

    #[test]
    fn knight_attacks_corner() {
        // a1 knight has 2 targets: b3, c2.
        let expected = bb(&[Square::B3, Square::C2]);
        assert_eq!(
            knight_attacks(Square::A1),
            expected,
            "corner knight on a1 should attack only b3 and c2"
        );
        assert_eq!(knight_attacks(Square::A1).count(), 2);
    }

    // ------------------------------------------------------------------------
    // King-attack pre-computed table tests.
    // ------------------------------------------------------------------------

    #[test]
    fn king_attacks_central() {
        let expected = bb(&[
            Square::C3,
            Square::D3,
            Square::E3,
            Square::C4,
            Square::E4,
            Square::C5,
            Square::D5,
            Square::E5,
        ]);
        assert_eq!(
            king_attacks(Square::D4),
            expected,
            "central king on d4 should attack 8 neighbors"
        );
        assert_eq!(king_attacks(Square::D4).count(), 8);
    }

    #[test]
    fn king_attacks_corner() {
        let expected = bb(&[Square::A2, Square::B1, Square::B2]);
        assert_eq!(
            king_attacks(Square::A1),
            expected,
            "corner king on a1 should attack only a2, b1, b2"
        );
        assert_eq!(king_attacks(Square::A1).count(), 3);
    }

    // ------------------------------------------------------------------------
    // `between_bb` / `line_bb` direct unit tests.
    //
    // These exercise the geometry helpers in isolation. The `MaskInfo::compute`
    // tests above call them indirectly with king-pinner pairs anchored at
    // e1/h1, which doesn't exercise the mid-board backward-sweep path of
    // `line_bb` or the non-collinear early-return of either helper. These
    // direct tests close those mutation-testing gaps.
    // ------------------------------------------------------------------------

    #[test]
    fn between_bb_same_square_empty() {
        for sq in [Square::A1, Square::D4, Square::H8] {
            assert_eq!(
                between_bb(sq, sq),
                Bitboard::EMPTY,
                "between_bb({sq}, {sq}) must be EMPTY",
            );
        }
    }

    #[test]
    fn between_bb_non_collinear_empty() {
        // Knight-jump distances and other non-rank/file/diagonal pairs must
        // return EMPTY, NOT step into the diagonal branch.
        let pairs = [
            (Square::B1, Square::C3), // knight jump
            (Square::A1, Square::B3), // knight jump
            (Square::D4, Square::F7), // 2 files, 3 ranks
            (Square::A1, Square::G8), // 6 files, 7 ranks
        ];
        for (from, to) in pairs {
            assert_eq!(
                between_bb(from, to),
                Bitboard::EMPTY,
                "non-collinear ({from}, {to}) must yield EMPTY",
            );
        }
    }

    #[test]
    fn between_bb_rank_diagonal_file() {
        // Rank: between b3 and g3 → c3, d3, e3, f3.
        assert_eq!(
            between_bb(Square::B3, Square::G3),
            bb(&[Square::C3, Square::D3, Square::E3, Square::F3]),
        );
        // File: between c2 and c7 → c3, c4, c5, c6.
        assert_eq!(
            between_bb(Square::C2, Square::C7),
            bb(&[Square::C3, Square::C4, Square::C5, Square::C6]),
        );
        // NE diagonal: between b2 and g7 → c3, d4, e5, f6.
        assert_eq!(
            between_bb(Square::B2, Square::G7),
            bb(&[Square::C3, Square::D4, Square::E5, Square::F6]),
        );
        // NW diagonal: between h1 and a8 → b7, c6, d5, e4, f3, g2.
        assert_eq!(
            between_bb(Square::H1, Square::A8),
            bb(&[
                Square::B7,
                Square::C6,
                Square::D5,
                Square::E4,
                Square::F3,
                Square::G2,
            ]),
        );
    }

    #[test]
    fn between_bb_adjacent_empty() {
        // Adjacent squares (df=±1, dr=0/±1, etc.) have nothing strictly
        // between them.
        for (from, to) in [
            (Square::A1, Square::B1),
            (Square::A1, Square::A2),
            (Square::D4, Square::E5),
        ] {
            assert_eq!(
                between_bb(from, to),
                Bitboard::EMPTY,
                "adjacent ({from}, {to}) has nothing strictly between",
            );
        }
    }

    #[test]
    fn line_bb_same_square_empty() {
        for sq in [Square::A1, Square::D4, Square::H8] {
            assert_eq!(line_bb(sq, sq), Bitboard::EMPTY);
        }
    }

    #[test]
    fn line_bb_non_collinear_empty() {
        let pairs = [
            (Square::B1, Square::C3),
            (Square::A1, Square::B3),
            (Square::D4, Square::F7),
        ];
        for (from, to) in pairs {
            assert_eq!(
                line_bb(from, to),
                Bitboard::EMPTY,
                "non-collinear ({from}, {to}) must yield EMPTY",
            );
        }
    }

    #[test]
    fn line_bb_file_full_through_midboard_from() {
        // `from` mid-file: line_bb(e4, e8) must include squares both
        // forward (e5..e8) AND backward (e1..e3) of the from square,
        // plus from itself. Pins the backward-sweep in line_bb.
        let got = line_bb(Square::E4, Square::E8);
        let expected = bb(&[
            Square::E1,
            Square::E2,
            Square::E3,
            Square::E4,
            Square::E5,
            Square::E6,
            Square::E7,
            Square::E8,
        ]);
        assert_eq!(
            got, expected,
            "line_bb(e4, e8) should be the full e-file (forward + backward)",
        );
    }

    #[test]
    fn line_bb_rank_full_through_midboard_from() {
        // Rank version: line_bb(d4, g4) must include a4-h4.
        let got = line_bb(Square::D4, Square::G4);
        let expected = bb(&[
            Square::A4,
            Square::B4,
            Square::C4,
            Square::D4,
            Square::E4,
            Square::F4,
            Square::G4,
            Square::H4,
        ]);
        assert_eq!(got, expected, "line_bb(d4, g4) should be the full rank-4");
    }

    #[test]
    fn line_bb_diagonal_full_through_midboard_from() {
        // Diagonal: line_bb(d4, g7) — both ends mid-board. Result is the
        // full a1-h8 diagonal: a1, b2, c3, d4, e5, f6, g7, h8.
        let got = line_bb(Square::D4, Square::G7);
        let expected = bb(&[
            Square::A1,
            Square::B2,
            Square::C3,
            Square::D4,
            Square::E5,
            Square::F6,
            Square::G7,
            Square::H8,
        ]);
        assert_eq!(
            got, expected,
            "line_bb(d4, g7) should be the full a1-h8 diagonal (forward + backward)",
        );
    }

    #[test]
    fn line_bb_anti_diagonal_full_through_midboard_from() {
        // Anti-diagonal: line_bb(d5, f3) — full h1-a8 line: a8, b7, c6, d5, e4, f3, g2, h1.
        let got = line_bb(Square::D5, Square::F3);
        let expected = bb(&[
            Square::A8,
            Square::B7,
            Square::C6,
            Square::D5,
            Square::E4,
            Square::F3,
            Square::G2,
            Square::H1,
        ]);
        assert_eq!(
            got, expected,
            "line_bb(d5, f3) should be the full h1-a8 anti-diagonal",
        );
    }

    // ------------------------------------------------------------------------
    // `checkers_of` tests.
    // ------------------------------------------------------------------------

    #[test]
    fn checkers_no_check_starting_position() {
        let pos = Position::starting_position();
        let king_sq = pos.king_square(pos.side_to_move());
        let checkers = checkers_of(&pos, pos.side_to_move(), king_sq, pos.occupied_all());
        assert!(
            checkers.is_empty(),
            "starting position should have no checkers, got {:?}",
            checkers
        );
    }

    #[test]
    fn checkers_pawn_check() {
        // White king on e4, black pawn on d5. White to move; black pawn checks.
        let pos = Position::from_fen("4k3/8/8/3p4/4K3/8/8/8 w - - 0 1").expect("FEN must parse");
        let king_sq = pos.king_square(Color::White);
        let checkers = checkers_of(&pos, Color::White, king_sq, pos.occupied_all());
        assert_eq!(
            checkers,
            bb(&[Square::D5]),
            "checker bitboard should be exactly {{d5}}"
        );
    }

    #[test]
    fn checkers_knight_check() {
        // White king on e4, black knight on c3.
        let pos = Position::from_fen("4k3/8/8/8/4K3/2n5/8/8 w - - 0 1").expect("FEN must parse");
        let king_sq = pos.king_square(Color::White);
        let checkers = checkers_of(&pos, Color::White, king_sq, pos.occupied_all());
        assert_eq!(
            checkers,
            bb(&[Square::C3]),
            "checker bitboard should be exactly {{c3}}"
        );
    }

    #[test]
    fn checkers_slider_check() {
        // White king on e4, black rook on e8, no blockers.
        let pos = Position::from_fen("4r3/8/8/8/4K3/8/8/4k3 w - - 0 1").expect("FEN must parse");
        let king_sq = pos.king_square(Color::White);
        let checkers = checkers_of(&pos, Color::White, king_sq, pos.occupied_all());
        assert_eq!(
            checkers,
            bb(&[Square::E8]),
            "checker bitboard should be exactly {{e8}}"
        );
    }

    #[test]
    fn checkers_double_check() {
        // White king on e1, black rook on e8, black knight on f3, black king
        // tucked at h8 to satisfy "exactly one king per color" — both rook
        // and knight deliver check.
        let pos = Position::from_fen("4r2k/8/8/8/8/5n2/8/4K3 w - - 0 1").expect("FEN must parse");
        let king_sq = pos.king_square(Color::White);
        let checkers = checkers_of(&pos, Color::White, king_sq, pos.occupied_all());
        assert_eq!(
            checkers.count(),
            2,
            "double-check position should yield 2 checkers, got {:?}",
            checkers
        );
        // Sanity: both attackers are present.
        assert!(
            checkers.contains(Square::E8),
            "rook on e8 should be in checker set"
        );
        assert!(
            checkers.contains(Square::F3),
            "knight on f3 should be in checker set"
        );
    }

    // ------------------------------------------------------------------------
    // Pin-computation tests via `MaskInfo::compute`.
    //
    // These tests exercise the post-impl contract — `MaskInfo::compute` is
    // currently `unimplemented!()`, so they will fail until the impl pass
    // lands. That is expected per the project's TDD pattern.
    // ------------------------------------------------------------------------

    #[test]
    fn pinned_no_pinners() {
        let pos = Position::starting_position();
        let king_sq = pos.king_square(pos.side_to_move());
        let info = MaskInfo::compute(&pos, pos.side_to_move(), king_sq);
        assert!(
            info.pinned.is_empty(),
            "starting position should have no pinned pieces, got {:?}",
            info.pinned
        );
    }

    #[test]
    fn pinned_one_rook_pinner() {
        // White king on e1, white bishop on e4, black rook on e8. The bishop
        // is pinned along the e-file.
        let pos = Position::from_fen("4r3/8/8/8/4B3/8/8/4K2k w - - 0 1").expect("FEN must parse");
        let king_sq = pos.king_square(Color::White);
        let info = MaskInfo::compute(&pos, Color::White, king_sq);
        assert!(
            info.pinned.contains(Square::E4),
            "bishop on e4 should be pinned, pinned={:?}",
            info.pinned
        );
        // pin_rays[E4] should include the e-file from e1 (king) through e8 (pinner).
        let ray = info.pin_rays[Square::E4.index() as usize];
        for sq in [
            Square::E1,
            Square::E2,
            Square::E3,
            Square::E4,
            Square::E5,
            Square::E6,
            Square::E7,
            Square::E8,
        ] {
            assert!(
                ray.contains(sq),
                "pin_ray for e4 should include {} (king-pinner line), ray={:?}",
                sq,
                ray
            );
        }
    }

    #[test]
    fn pinned_two_pieces_between_means_no_pin() {
        // White king on e1, white bishop on e3, white knight on e4, black
        // rook on e8: two pieces between => neither is pinned.
        let pos = Position::from_fen("4r3/8/8/8/4N3/4B3/8/4K2k w - - 0 1").expect("FEN must parse");
        let king_sq = pos.king_square(Color::White);
        let info = MaskInfo::compute(&pos, Color::White, king_sq);
        assert!(
            !info.pinned.contains(Square::E3),
            "bishop on e3 should NOT be pinned (two pieces between king and rook)"
        );
        assert!(
            !info.pinned.contains(Square::E4),
            "knight on e4 should NOT be pinned (two pieces between king and rook)"
        );
    }

    #[test]
    fn pinned_opposite_color_piece_between_does_not_pin_us() {
        // White king on e1, black knight on e3 (the only piece between),
        // black rook on e8. No own-side piece is between => no own-side pin.
        let pos = Position::from_fen("4r3/8/8/8/8/4n3/8/4K2k w - - 0 1").expect("FEN must parse");
        let king_sq = pos.king_square(Color::White);
        let info = MaskInfo::compute(&pos, Color::White, king_sq);
        assert!(
            info.pinned.is_empty(),
            "no own-side piece is between king and rook, so pinned must be empty; got {:?}",
            info.pinned
        );
    }

    #[test]
    fn pinned_diagonal_bishop_pinner() {
        // White king on e1, white knight on c3, black bishop on a5: knight
        // pinned along a5-e1 diagonal.
        let pos = Position::from_fen("7k/8/8/b7/8/2N5/8/4K3 w - - 0 1").expect("FEN must parse");
        let king_sq = pos.king_square(Color::White);
        let info = MaskInfo::compute(&pos, Color::White, king_sq);
        assert!(
            info.pinned.contains(Square::C3),
            "knight on c3 should be pinned along a5-e1 diagonal, pinned={:?}",
            info.pinned
        );
        // pin_rays[C3] should include the a5-e1 diagonal squares.
        let ray = info.pin_rays[Square::C3.index() as usize];
        for sq in [Square::E1, Square::D2, Square::C3, Square::B4, Square::A5] {
            assert!(
                ray.contains(sq),
                "pin_ray for c3 should include {} (king-pinner line), ray={:?}",
                sq,
                ray
            );
        }
    }

    #[test]
    fn pinned_queen_as_rook_direction_pinner() {
        // Queen pinning along a file (rook-direction). The pin computation
        // must recognize the queen as a rook-axis attacker; missing this
        // case would let an own piece move off the e-file pin ray.
        //
        // White king on e1, white knight on e4, black queen on e8.
        let pos = Position::from_fen("4q3/8/8/8/4N3/8/8/4K2k w - - 0 1").expect("FEN must parse");
        let king_sq = pos.king_square(Color::White);
        let info = MaskInfo::compute(&pos, Color::White, king_sq);
        assert!(
            info.pinned.contains(Square::E4),
            "queen-as-rook should pin own knight on e4 along e-file; pinned={:?}",
            info.pinned
        );
        let ray = info.pin_rays[Square::E4.index() as usize];
        for sq in [Square::E1, Square::E4, Square::E8] {
            assert!(
                ray.contains(sq),
                "pin_ray for e4 should include {sq} on the e-file; ray={ray:?}",
            );
        }
    }

    #[test]
    fn pinned_queen_as_bishop_direction_pinner() {
        // Queen pinning along a diagonal (bishop-direction). Symmetric
        // failure mode to the rook-direction case above.
        //
        // White king on e1, white knight on c3, black queen on a5 (along
        // the a5-e1 diagonal).
        let pos = Position::from_fen("7k/8/8/q7/8/2N5/8/4K3 w - - 0 1").expect("FEN must parse");
        let king_sq = pos.king_square(Color::White);
        let info = MaskInfo::compute(&pos, Color::White, king_sq);
        assert!(
            info.pinned.contains(Square::C3),
            "queen-as-bishop should pin own knight on c3 along a5-e1 diagonal; pinned={:?}",
            info.pinned
        );
        let ray = info.pin_rays[Square::C3.index() as usize];
        for sq in [Square::E1, Square::D2, Square::C3, Square::B4, Square::A5] {
            assert!(
                ray.contains(sq),
                "pin_ray for c3 should include {sq} on the diagonal; ray={ray:?}",
            );
        }
    }

    // ------------------------------------------------------------------------
    // King-danger / `compute_attacked_squares` tests.
    // ------------------------------------------------------------------------

    #[test]
    fn king_danger_through_king_square() {
        // White king on a1, black rook on a8, empty file. With `occ ^ king_bb`,
        // the rook's ray extends through a1; in particular a2 is attacked.
        // Without the `^ king_bb`, a2 would be missing because the rook's
        // attack ray would terminate at a1.
        let pos = Position::from_fen("r6k/8/8/8/8/8/8/K7 w - - 0 1").expect("FEN must parse");
        let king_bb = Bitboard::from_square(pos.king_square(Color::White));
        let occ = pos.occupied_all();
        let attacked = compute_attacked_squares(&pos, Color::Black, occ ^ king_bb);
        assert!(
            attacked.contains(Square::A2),
            "with occ ^ king_bb, the rook on a8 should attack a2 (through the vacated king square); got {:?}",
            attacked
        );
    }

    #[test]
    fn compute_attacked_squares_includes_pawn_pattern() {
        // Black pawn on e4 — we ask for squares attacked by Black; pawns on
        // e4 (as black) attack d3 and f3.
        let pos = Position::from_fen("4k3/8/8/8/4p3/8/8/4K3 w - - 0 1").expect("FEN must parse");
        let attacked = compute_attacked_squares(&pos, Color::Black, pos.occupied_all());
        assert!(
            attacked.contains(Square::D3),
            "black pawn on e4 should attack d3, got {:?}",
            attacked
        );
        assert!(
            attacked.contains(Square::F3),
            "black pawn on e4 should attack f3, got {:?}",
            attacked
        );
    }

    #[test]
    fn compute_attacked_squares_includes_opponent_king() {
        // Black king on e4 — we ask for squares attacked by Black; the black
        // king attacks its 8 neighbors. (Required so our king never moves
        // adjacent to the opponent king.)
        let pos = Position::from_fen("8/8/8/8/4k3/8/8/4K3 w - - 0 1").expect("FEN must parse");
        let attacked = compute_attacked_squares(&pos, Color::Black, pos.occupied_all());
        for sq in [
            Square::D3,
            Square::E3,
            Square::F3,
            Square::D4,
            Square::F4,
            Square::D5,
            Square::E5,
            Square::F5,
        ] {
            assert!(
                attacked.contains(sq),
                "black king on e4 should attack {}, got {:?}",
                sq,
                attacked
            );
        }
    }
}
