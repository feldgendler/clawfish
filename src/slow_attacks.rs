//! Slow ray-walker oracle for sliding-piece attacks.
//!
//! Permanent module — never replaced by the magic-bitboard fast path. Used as:
//! - the source of truth when the runtime `magic` module builds its attack
//!   tables on first use,
//! - the reference oracle in `tests/magic_consistency.rs` (every (sq, occ ⊆
//!   mask) pair, both pieces),
//! - a debugging aid when a perft mismatch points at sliding-piece attacks,
//! - a reference for future higher-layer test code (pin detection, attack
//!   maps, etc., as those land in M1.E and M1.F).
//!
//! The walker is deliberately simple: for each direction from `sq`, step
//! square-by-square until the edge of the board or an occupied square is
//! reached. The first blocker on each ray IS in the attack set (it's a
//! capture target); the slider's own square is NOT. See ADR-0008 for the
//! commitment that this module stays around forever.

use crate::bitboard::Bitboard;
use crate::square::Square;

#[inline]
fn ray_attack(sq: Square, occ: Bitboard, dfile: i8, drank: i8) -> Bitboard {
    let mut acc = Bitboard::EMPTY;
    let f0 = sq.file() as i8;
    let r0 = sq.rank() as i8;
    let mut k: i8 = 1;
    loop {
        let nf = f0 + k * dfile;
        let nr = r0 + k * drank;
        if !(0..8).contains(&nf) || !(0..8).contains(&nr) {
            return acc;
        }
        let next = Square::from_file_rank(nf as u8, nr as u8)
            .expect("file/rank already bounds-checked to 0..8");
        acc = acc.with(next);
        if occ.contains(next) {
            return acc;
        }
        k += 1;
    }
}

#[inline]
fn ray_mask(sq: Square, dfile: i8, drank: i8) -> Bitboard {
    let mut acc = Bitboard::EMPTY;
    let f0 = sq.file() as i8;
    let r0 = sq.rank() as i8;
    let mut k: i8 = 1;
    loop {
        let nf = f0 + k * dfile;
        let nr = r0 + k * drank;
        if !(0..8).contains(&nf) || !(0..8).contains(&nr) {
            return acc;
        }
        let beyond_f = nf + dfile;
        let beyond_r = nr + drank;
        if !(0..8).contains(&beyond_f) || !(0..8).contains(&beyond_r) {
            return acc;
        }
        let next = Square::from_file_rank(nf as u8, nr as u8)
            .expect("file/rank already bounds-checked to 0..8");
        acc = acc.with(next);
        k += 1;
    }
}

/// Relevant-blocker mask for a rook on `sq`. Excludes the rook's own square
/// and the last square of every ray (per `docs/research/m1-magic-bitboards.md`
/// §3 — those bits don't affect the attack set, so including them would
/// double the magic-bitboard table for no information gain).
#[inline]
pub fn rook_mask(sq: Square) -> Bitboard {
    ray_mask(sq, 0, 1) | ray_mask(sq, 0, -1) | ray_mask(sq, 1, 0) | ray_mask(sq, -1, 0)
}

/// Relevant-blocker mask for a bishop on `sq`. Excludes the bishop's own
/// square and the last square of every ray (per
/// `docs/research/m1-magic-bitboards.md` §3).
#[inline]
pub fn bishop_mask(sq: Square) -> Bitboard {
    ray_mask(sq, 1, 1) | ray_mask(sq, 1, -1) | ray_mask(sq, -1, 1) | ray_mask(sq, -1, -1)
}

/// Squares the rook on `sq` attacks given full-board occupancy `occ`. Includes
/// the first blocker on each ray; excludes `sq` itself. The walker ignores
/// piece colors — color-aware filtering happens at the legal-move layer
/// (M1.F).
#[inline]
pub fn rook_attacks(sq: Square, occ: Bitboard) -> Bitboard {
    ray_attack(sq, occ, 0, 1)
        | ray_attack(sq, occ, 0, -1)
        | ray_attack(sq, occ, 1, 0)
        | ray_attack(sq, occ, -1, 0)
}

/// Squares the bishop on `sq` attacks given full-board occupancy `occ`.
/// Includes the first blocker on each ray; excludes `sq` itself.
#[inline]
pub fn bishop_attacks(sq: Square, occ: Bitboard) -> Bitboard {
    ray_attack(sq, occ, 1, 1)
        | ray_attack(sq, occ, 1, -1)
        | ray_attack(sq, occ, -1, 1)
        | ray_attack(sq, occ, -1, -1)
}

/// `rook_attacks(sq, occ) | bishop_attacks(sq, occ)`. Provided as a public
/// helper so callers can stay symmetric with the `magic` module's API.
#[inline]
pub fn queen_attacks(sq: Square, occ: Bitboard) -> Bitboard {
    rook_attacks(sq, occ) | bishop_attacks(sq, occ)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a Bitboard from a slice of squares.
    fn bb_of(squares: &[Square]) -> Bitboard {
        let mut bb = Bitboard::EMPTY;
        for &sq in squares {
            bb = bb.with(sq);
        }
        bb
    }

    // -------- ray_attack direct tests (pin per-axis step semantics) --------
    //
    // The four-direction `rook_attacks` / `bishop_attacks` callers are
    // symmetric in {dfile, drank} sign, so a sign-flip on the step (`f0 +
    // k*dfile` → `f0 - k*dfile`) cycles east↔west / north↔south and the
    // overall four-direction union is identical. Pin each axis independently
    // here so the per-axis step is verifiable in isolation.

    #[test]
    fn ray_attack_east_pins_dfile_step() {
        // From a4 walking east on an empty board: yields b4..h4.
        let got = ray_attack(Square::A4, Bitboard::EMPTY, 1, 0);
        let expected = bb_of(&[
            Square::B4,
            Square::C4,
            Square::D4,
            Square::E4,
            Square::F4,
            Square::G4,
            Square::H4,
        ]);
        assert_eq!(got, expected);
    }

    #[test]
    fn ray_attack_north_pins_drank_step() {
        // From d1 walking north on an empty board: yields d2..d8.
        let got = ray_attack(Square::D1, Bitboard::EMPTY, 0, 1);
        let expected = bb_of(&[
            Square::D2,
            Square::D3,
            Square::D4,
            Square::D5,
            Square::D6,
            Square::D7,
            Square::D8,
        ]);
        assert_eq!(got, expected);
    }

    // -------- rook_mask --------

    #[test]
    fn rook_mask_a1_pinned_bits() {
        // a2..a7 (file ray, excluding a8 endpoint and a1 own square)
        // plus b1..g1 (rank ray, excluding h1 endpoint and a1 own square).
        let expected = bb_of(&[
            Square::A2,
            Square::A3,
            Square::A4,
            Square::A5,
            Square::A6,
            Square::A7,
            Square::B1,
            Square::C1,
            Square::D1,
            Square::E1,
            Square::F1,
            Square::G1,
        ]);
        let got = rook_mask(Square::A1);
        assert_eq!(got, expected);
        assert_eq!(got.count(), 12);
    }

    #[test]
    fn rook_mask_h8_pinned_bits() {
        // h2..h7 plus b8..g8. Symmetric to A1.
        let expected = bb_of(&[
            Square::H2,
            Square::H3,
            Square::H4,
            Square::H5,
            Square::H6,
            Square::H7,
            Square::B8,
            Square::C8,
            Square::D8,
            Square::E8,
            Square::F8,
            Square::G8,
        ]);
        let got = rook_mask(Square::H8);
        assert_eq!(got, expected);
        assert_eq!(got.count(), 12);
    }

    #[test]
    fn rook_mask_d4_pinned_bits() {
        // d-file: d2..d7 (excludes d1 endpoint, d8 endpoint, d4 own square).
        // rank-4: b4..g4 (excludes a4 endpoint, h4 endpoint, d4 own square).
        let expected = bb_of(&[
            Square::D2,
            Square::D3,
            Square::D5,
            Square::D6,
            Square::D7,
            Square::B4,
            Square::C4,
            Square::E4,
            Square::F4,
            Square::G4,
        ]);
        let got = rook_mask(Square::D4);
        assert_eq!(got, expected);
        assert_eq!(got.count(), 10);
    }

    #[test]
    fn rook_mask_a4_edge_file() {
        // A4 is on the a-file: both file-ray endpoints (a1, a8) AND a4 own
        // square excluded — 5 file bits (a2, a3, a5, a6, a7). Rank-4 ray
        // excludes h4 endpoint and a4 own square — 6 rank bits (b4..g4).
        let expected = bb_of(&[
            Square::A2,
            Square::A3,
            Square::A5,
            Square::A6,
            Square::A7,
            Square::B4,
            Square::C4,
            Square::D4,
            Square::E4,
            Square::F4,
            Square::G4,
        ]);
        let got = rook_mask(Square::A4);
        assert_eq!(got, expected);
        assert_eq!(got.count(), 11);
    }

    #[test]
    fn rook_mask_d1_edge_rank() {
        // D1 is on rank 1: both rank-ray endpoints (a1, h1) AND d1 own square
        // excluded — 5 rank bits (b1, c1, e1, f1, g1). d-file ray excludes d8
        // endpoint and d1 own square — 6 file bits (d2..d7).
        let expected = bb_of(&[
            Square::B1,
            Square::C1,
            Square::E1,
            Square::F1,
            Square::G1,
            Square::D2,
            Square::D3,
            Square::D4,
            Square::D5,
            Square::D6,
            Square::D7,
        ]);
        let got = rook_mask(Square::D1);
        assert_eq!(got, expected);
        assert_eq!(got.count(), 11);
    }

    #[test]
    fn rook_mask_excludes_own_square() {
        for sq in Square::all() {
            assert!(
                !rook_mask(sq).contains(sq),
                "rook_mask({:?}) must not contain {:?}",
                sq,
                sq
            );
        }
    }

    #[test]
    fn rook_mask_total_popcount() {
        // Per docs/research/m1-magic-bitboards.md §3:
        //   4 corners × 12 bits + 24 edges × 11 bits + 36 interior × 10 bits
        //   = 48 + 264 + 360 = 672.
        let total: u32 = Square::all().map(|sq| rook_mask(sq).count()).sum();
        assert_eq!(total, 672);
    }

    // -------- bishop_mask --------

    #[test]
    fn bishop_mask_a1_pinned_bits() {
        // The single NE diagonal from a1, excluding the h8 endpoint and a1
        // itself: b2, c3, d4, e5, f6, g7.
        let expected = bb_of(&[
            Square::B2,
            Square::C3,
            Square::D4,
            Square::E5,
            Square::F6,
            Square::G7,
        ]);
        let got = bishop_mask(Square::A1);
        assert_eq!(got, expected);
        assert_eq!(got.count(), 6);
    }

    #[test]
    fn bishop_mask_h1_pinned_bits() {
        // Single NW diagonal from h1, excluding a8 endpoint and h1 own square:
        // g2, f3, e4, d5, c6, b7.
        let expected = bb_of(&[
            Square::G2,
            Square::F3,
            Square::E4,
            Square::D5,
            Square::C6,
            Square::B7,
        ]);
        let got = bishop_mask(Square::H1);
        assert_eq!(got, expected);
        assert_eq!(got.count(), 6);
    }

    #[test]
    fn bishop_mask_d4_pinned_bits() {
        // From D4, four diagonals; excludes endpoints (a1, h8 on the NE-SW
        // diagonal; a7, g1 on the NW-SE diagonal — these are the last on-board
        // squares of each ray) and d4 own square.
        // NE: e5, f6, g7  (h8 excluded as endpoint)
        // SW: c3, b2     (a1 excluded as endpoint)
        // NW: c5, b6     (a7 excluded as endpoint)
        // SE: e3, f2     (g1 excluded as endpoint)
        let expected = bb_of(&[
            Square::E5,
            Square::F6,
            Square::G7,
            Square::C3,
            Square::B2,
            Square::C5,
            Square::B6,
            Square::E3,
            Square::F2,
        ]);
        let got = bishop_mask(Square::D4);
        assert_eq!(got, expected);
        assert_eq!(got.count(), 9);
    }

    #[test]
    fn bishop_mask_excludes_own_square() {
        for sq in Square::all() {
            assert!(
                !bishop_mask(sq).contains(sq),
                "bishop_mask({:?}) must not contain {:?}",
                sq,
                sq
            );
        }
    }

    #[test]
    fn bishop_mask_disjoint_from_rook_mask() {
        // Rook mask covers file/rank rays, bishop mask covers diagonals;
        // they must never share a bit. Pinned by m1-magic-bitboards.md §8.
        for sq in Square::all() {
            assert_eq!(
                rook_mask(sq) & bishop_mask(sq),
                Bitboard::EMPTY,
                "rook_mask & bishop_mask must be empty at {:?}",
                sq
            );
        }
    }

    #[test]
    fn bishop_mask_a4_edge_file() {
        // a4 is on the a-file. NE diagonal: b5, c6, d7, e8 (4 squares; e8 endpoint
        // excluded → 3 mask bits). SE: b3, c2, d1 (3 squares; d1 endpoint excluded
        // → 2 bits). NW and SW are zero-length (no squares west of a4). Total: 5.
        let expected = bb_of(&[Square::B5, Square::C6, Square::D7, Square::B3, Square::C2]);
        let got = bishop_mask(Square::A4);
        assert_eq!(got, expected);
        assert_eq!(got.count(), 5);
    }

    #[test]
    fn bishop_mask_b1_edge_rank() {
        // b1 is on rank 1. NE diagonal: c2, d3, e4, f5, g6, h7 (6 squares; h7
        // endpoint excluded → 5 mask bits). NW: a2 (1 square, endpoint excluded
        // → 0 bits). SE / SW: zero-length. Total: 5. Pins the asymmetric-diagonal
        // 5-bit case (long single ray) distinct from a4's two-short-rays 5-bit
        // case.
        let expected = bb_of(&[Square::C2, Square::D3, Square::E4, Square::F5, Square::G6]);
        let got = bishop_mask(Square::B1);
        assert_eq!(got, expected);
        assert_eq!(got.count(), 5);
    }

    #[test]
    fn bishop_mask_c3_inner_ring() {
        // c3 is on the inner 4×4 ring. NE: d4, e5, f6, g7, h8 → 4 bits (excludes
        // h8). NW: b4, a5 → 1 bit (excludes a5). SE: d2, e1 → 1 bit (excludes
        // e1). SW: b2, a1 → 1 bit (excludes a1). Total: 7 bits.
        let expected = bb_of(&[
            Square::D4,
            Square::E5,
            Square::F6,
            Square::G7,
            Square::B4,
            Square::D2,
            Square::B2,
        ]);
        let got = bishop_mask(Square::C3);
        assert_eq!(got, expected);
        assert_eq!(got.count(), 7);
    }

    #[test]
    fn bishop_mask_total_popcount() {
        // Sum of bishop-mask popcounts over all 64 squares, derived from the
        // closed-form per-square formula
        //   bits(f, r) = Σ_diag max(0, len_diag - 1)
        // where len_diag is the on-board diagonal length from (f, r). Symmetric
        // across the two main diagonals; per-rank sums are 42, 40, 48, 52, 52,
        // 48, 40, 42 for ranks 0..7. Grand total: 2·(42+40+48+52) = 364.
        let total: u32 = Square::all().map(|sq| bishop_mask(sq).count()).sum();
        assert_eq!(total, 364);
    }

    // -------- rook_attacks --------

    #[test]
    fn rook_attacks_empty_board_a1() {
        // a2..a8 (full a-file minus a1) plus b1..h1 (full rank-1 minus a1).
        let expected = bb_of(&[
            Square::A2,
            Square::A3,
            Square::A4,
            Square::A5,
            Square::A6,
            Square::A7,
            Square::A8,
            Square::B1,
            Square::C1,
            Square::D1,
            Square::E1,
            Square::F1,
            Square::G1,
            Square::H1,
        ]);
        let got = rook_attacks(Square::A1, Bitboard::EMPTY);
        assert_eq!(got, expected);
        assert_eq!(got.count(), 14);
    }

    #[test]
    fn rook_attacks_empty_board_d4() {
        // d-file (rank 1..8 minus d4) ∪ rank-4 (a..h minus d4): 7 + 7 = 14.
        let expected = bb_of(&[
            Square::D1,
            Square::D2,
            Square::D3,
            Square::D5,
            Square::D6,
            Square::D7,
            Square::D8,
            Square::A4,
            Square::B4,
            Square::C4,
            Square::E4,
            Square::F4,
            Square::G4,
            Square::H4,
        ]);
        let got = rook_attacks(Square::D4, Bitboard::EMPTY);
        assert_eq!(got, expected);
        assert_eq!(got.count(), 14);
    }

    #[test]
    fn rook_attacks_blocker_d4_d6() {
        // Rook on d4, blocker on d6. Walker stops at d6 inclusive on the N
        // ray (d5, d6); S ray unblocked (d3, d2, d1); E and W rays unblocked
        // (full rank-4 minus d4). Blocker IS in the attack set.
        let occ = Bitboard::from_square(Square::D6);
        let expected = bb_of(&[
            // N ray, stops at d6 inclusive
            Square::D5,
            Square::D6,
            // S ray
            Square::D3,
            Square::D2,
            Square::D1,
            // rank-4 minus d4
            Square::A4,
            Square::B4,
            Square::C4,
            Square::E4,
            Square::F4,
            Square::G4,
            Square::H4,
        ]);
        let got = rook_attacks(Square::D4, occ);
        assert_eq!(got, expected);
        // Blocker explicitly in the set — pin the blocker-as-target contract.
        assert!(got.contains(Square::D6));
    }

    #[test]
    fn rook_attacks_blocker_d4_d6_d2() {
        // Rook on d4, blockers on d6 AND d2. N ray: d5, d6. S ray: d3, d2.
        // E/W rays unblocked.
        let occ = Bitboard::from_square(Square::D6).with(Square::D2);
        let expected = bb_of(&[
            Square::D5,
            Square::D6,
            Square::D3,
            Square::D2,
            Square::A4,
            Square::B4,
            Square::C4,
            Square::E4,
            Square::F4,
            Square::G4,
            Square::H4,
        ]);
        let got = rook_attacks(Square::D4, occ);
        assert_eq!(got, expected);
        assert!(got.contains(Square::D6));
        assert!(got.contains(Square::D2));
        // d1 is past the blocker on d2 — must be excluded.
        assert!(!got.contains(Square::D1));
    }

    #[test]
    fn rook_attacks_blocker_h1_corner() {
        // Rook on h1, blocker on h7. W ray unblocked: a1..g1. N ray stops at
        // h7: h2..h7. S and E rays off-board.
        let occ = Bitboard::from_square(Square::H7);
        let expected = bb_of(&[
            Square::A1,
            Square::B1,
            Square::C1,
            Square::D1,
            Square::E1,
            Square::F1,
            Square::G1,
            Square::H2,
            Square::H3,
            Square::H4,
            Square::H5,
            Square::H6,
            Square::H7,
        ]);
        let got = rook_attacks(Square::H1, occ);
        assert_eq!(got, expected);
        assert!(got.contains(Square::H7));
        // h8 is past the blocker on h7 — must be excluded.
        assert!(!got.contains(Square::H8));
    }

    #[test]
    fn rook_attacks_excludes_own_square() {
        // Small fixed set of occupancies: empty, full, and one nontrivial
        // subset (the four central squares).
        let occupancies = [
            Bitboard::EMPTY,
            Bitboard::FULL,
            Bitboard::from_square(Square::D4)
                .with(Square::E4)
                .with(Square::D5)
                .with(Square::E5),
        ];
        for sq in Square::all() {
            for &occ in &occupancies {
                assert!(
                    !rook_attacks(sq, occ).contains(sq),
                    "rook_attacks({:?}, {:?}) must not contain {:?}",
                    sq,
                    occ,
                    sq
                );
            }
        }
    }

    // -------- bishop_attacks --------

    #[test]
    fn bishop_attacks_empty_board_d4() {
        // From D4 all four diagonals run to the board edge with no blockers.
        // NE: e5, f6, g7, h8.
        // SW: c3, b2, a1.
        // NW: c5, b6, a7.
        // SE: e3, f2, g1.
        // Total 4 + 3 + 3 + 3 = 13.
        let expected = bb_of(&[
            Square::E5,
            Square::F6,
            Square::G7,
            Square::H8,
            Square::C3,
            Square::B2,
            Square::A1,
            Square::C5,
            Square::B6,
            Square::A7,
            Square::E3,
            Square::F2,
            Square::G1,
        ]);
        let got = bishop_attacks(Square::D4, Bitboard::EMPTY);
        assert_eq!(got, expected);
        assert_eq!(got.count(), 13);
    }

    #[test]
    fn bishop_attacks_empty_board_a1() {
        // Long a1-h8 diagonal minus a1.
        let expected = bb_of(&[
            Square::B2,
            Square::C3,
            Square::D4,
            Square::E5,
            Square::F6,
            Square::G7,
            Square::H8,
        ]);
        let got = bishop_attacks(Square::A1, Bitboard::EMPTY);
        assert_eq!(got, expected);
        assert_eq!(got.count(), 7);
    }

    #[test]
    fn bishop_attacks_blocker_d4_f6() {
        // Bishop on d4, blocker on f6. NE ray stops at f6: e5, f6 (g7, h8
        // excluded). Other three diagonals empty-board.
        let occ = Bitboard::from_square(Square::F6);
        let expected = bb_of(&[
            // NE, stops at f6 inclusive
            Square::E5,
            Square::F6,
            // SW
            Square::C3,
            Square::B2,
            Square::A1,
            // NW
            Square::C5,
            Square::B6,
            Square::A7,
            // SE
            Square::E3,
            Square::F2,
            Square::G1,
        ]);
        let got = bishop_attacks(Square::D4, occ);
        assert_eq!(got, expected);
        assert!(got.contains(Square::F6));
        // g7 and h8 are past the blocker — must be excluded.
        assert!(!got.contains(Square::G7));
        assert!(!got.contains(Square::H8));
    }

    #[test]
    fn bishop_attacks_excludes_own_square() {
        let occupancies = [
            Bitboard::EMPTY,
            Bitboard::FULL,
            Bitboard::from_square(Square::D4)
                .with(Square::E4)
                .with(Square::D5)
                .with(Square::E5),
        ];
        for sq in Square::all() {
            for &occ in &occupancies {
                assert!(
                    !bishop_attacks(sq, occ).contains(sq),
                    "bishop_attacks({:?}, {:?}) must not contain {:?}",
                    sq,
                    occ,
                    sq
                );
            }
        }
    }

    #[test]
    fn bishop_attacks_empty_board_h8() {
        // Bishop on h8 (corner); the only on-board diagonal goes SW. Expected:
        // a1..g7, popcount 7.
        let expected = bb_of(&[
            Square::G7,
            Square::F6,
            Square::E5,
            Square::D4,
            Square::C3,
            Square::B2,
            Square::A1,
        ]);
        let got = bishop_attacks(Square::H8, Bitboard::EMPTY);
        assert_eq!(got, expected);
        assert_eq!(got.count(), 7);
    }

    #[test]
    fn bishop_attacks_empty_board_h1() {
        // Bishop on h1 (corner); only diagonal goes NW. Expected: a8..g2,
        // popcount 7.
        let expected = bb_of(&[
            Square::G2,
            Square::F3,
            Square::E4,
            Square::D5,
            Square::C6,
            Square::B7,
            Square::A8,
        ]);
        let got = bishop_attacks(Square::H1, Bitboard::EMPTY);
        assert_eq!(got, expected);
        assert_eq!(got.count(), 7);
    }

    #[test]
    fn bishop_attacks_blocker_h8_corner() {
        // Bishop on h8 with blocker on c3. Walker steps SW: g7, f6, e5, d4, c3
        // (blocker, included), stop. Other rays off-board. Pinned to confirm
        // first-blocker-on-each-ray semantics from a corner — symmetric to
        // `rook_attacks_blocker_h1_corner`.
        let occ = Bitboard::from_square(Square::C3);
        let expected = bb_of(&[Square::G7, Square::F6, Square::E5, Square::D4, Square::C3]);
        let got = bishop_attacks(Square::H8, occ);
        assert_eq!(got, expected);
        assert!(
            got.contains(Square::C3),
            "blocker square must be in attack set"
        );
        assert!(
            !got.contains(Square::B2),
            "square past the blocker must not be in attack set"
        );
    }

    // -------- queen_attacks --------

    #[test]
    fn queen_attacks_equals_rook_or_bishop() {
        // For several pinned (sq, occ) pairs, queen = rook | bishop.
        let cases: [(Square, Bitboard); 4] = [
            (Square::D4, Bitboard::EMPTY),
            (Square::D4, Bitboard::FULL),
            (
                Square::D4,
                Bitboard::from_square(Square::D6).with(Square::F6),
            ),
            (
                Square::A1,
                Bitboard::from_square(Square::A4).with(Square::D4),
            ),
        ];
        for (sq, occ) in cases {
            let q = queen_attacks(sq, occ);
            let combined = rook_attacks(sq, occ) | bishop_attacks(sq, occ);
            assert_eq!(
                q, combined,
                "queen_attacks({:?}, {:?}) must equal rook|bishop",
                sq, occ
            );
        }
    }

    #[test]
    fn queen_attacks_starting_position_d1() {
        // Full starting-position occupancy: ranks 1, 2, 7, 8 fully populated.
        // White queen on d1 (already in the rank-1 occupancy). Every
        // immediate neighbour on every ray is blocked.
        // Rook rays: N hits d2, S off-board, W hits c1, E hits e1.
        // Bishop rays: NE hits e2, NW hits c2, SE off-board, SW off-board.
        // Attack set is exactly {c1, e1, d2, c2, e2}.
        let occ = Bitboard::RANK_1 | Bitboard::RANK_2 | Bitboard::RANK_7 | Bitboard::RANK_8;
        let expected = bb_of(&[Square::C1, Square::E1, Square::D2, Square::C2, Square::E2]);
        let got = queen_attacks(Square::D1, occ);
        assert_eq!(got, expected);
        assert_eq!(got.count(), 5);
    }
}
