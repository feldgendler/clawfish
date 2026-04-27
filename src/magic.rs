//! Fancy magic-bitboard sliding-piece attack lookup.
//!
//! Public surface: `rook_attacks`, `bishop_attacks`, `queen_attacks`. Each
//! takes a `Square` and an occupancy `Bitboard` and returns the attack-set
//! bitboard, semantically identical to `slow_attacks::*_attacks` but with
//! constant-time lookup.
//!
//! Per ADR-0008 we use **fancy magic with variable shift**. The per-square
//! `Magic` struct (mask, magic, shift, offset) is generated at codegen time
//! by `src/bin/magicgen.rs` and committed in `src/magic/constants.rs`. The
//! attack tables themselves are built at runtime via `LazyLock` on first
//! call to `*_attacks`.

use std::sync::LazyLock;

use crate::bitboard::Bitboard;
use crate::slow_attacks;
use crate::square::Square;

mod constants;

/// Per-square magic-bitboard parameters. Values are produced by `magicgen`
/// and committed in `src/magic/constants.rs`.
///
/// Crate-private: the function-level API (`rook_attacks`, etc.) is the entire
/// public surface for the magic module. `Magic` exists so the codegen-emitted
/// constants file has a typed shape to point at.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Magic {
    pub(crate) mask: Bitboard,
    pub(crate) magic: u64,
    pub(crate) shift: u8,
    pub(crate) offset: u32,
}

/// Squares the rook on `sq` attacks given board occupancy `occ`. Includes
/// the first blocker on each ray; excludes `sq` itself. Same semantics as
/// `slow_attacks::rook_attacks`.
#[inline]
pub fn rook_attacks(sq: Square, occ: Bitboard) -> Bitboard {
    let m = &constants::ROOK_MAGICS[sq.index() as usize];
    let blockers = (occ & m.mask).0;
    // `wrapping_mul` is load-bearing: magic constants routinely overflow u64,
    // and `*` would panic in debug builds. See research §8.
    let idx = blockers.wrapping_mul(m.magic) >> m.shift;
    Bitboard(ROOK_ATTACK_TABLE[m.offset as usize + idx as usize])
}

/// Squares the bishop on `sq` attacks given board occupancy `occ`. Includes
/// the first blocker on each ray; excludes `sq` itself. Same semantics as
/// `slow_attacks::bishop_attacks`.
#[inline]
pub fn bishop_attacks(sq: Square, occ: Bitboard) -> Bitboard {
    let m = &constants::BISHOP_MAGICS[sq.index() as usize];
    let blockers = (occ & m.mask).0;
    let idx = blockers.wrapping_mul(m.magic) >> m.shift;
    Bitboard(BISHOP_ATTACK_TABLE[m.offset as usize + idx as usize])
}

/// `rook_attacks(sq, occ) | bishop_attacks(sq, occ)`. Two table lookups, one OR.
#[inline]
pub fn queen_attacks(sq: Square, occ: Bitboard) -> Bitboard {
    rook_attacks(sq, occ) | bishop_attacks(sq, occ)
}

static ROOK_ATTACK_TABLE: LazyLock<Box<[u64]>> = LazyLock::new(build_rook_table);
static BISHOP_ATTACK_TABLE: LazyLock<Box<[u64]>> = LazyLock::new(build_bishop_table);

fn build_rook_table() -> Box<[u64]> {
    let mut table = vec![0u64; constants::ROOK_TABLE_LEN];
    for sq in Square::all() {
        let m = &constants::ROOK_MAGICS[sq.index() as usize];
        let mask = m.mask.0;
        // Carry-rippler subset enumeration. Starts at sub == 0 (the empty
        // subset) and wraps back to 0 after visiting every subset of `mask`.
        //
        // `existing == 0` as the "slot not yet filled" sentinel is safe because
        // a slider's attack set is never empty for any blocker subset: every
        // square has at least one accessible ray neighbour, and that neighbour
        // is in the attack set whether it's a blocker or not.
        let mut sub: u64 = 0;
        loop {
            let attack = slow_attacks::rook_attacks(sq, Bitboard(sub)).0;
            let idx = sub.wrapping_mul(m.magic) >> m.shift;
            let slot = m.offset as usize + idx as usize;
            let existing = table[slot];
            if existing == 0 {
                table[slot] = attack;
            } else if existing != attack {
                panic!(
                    "magic table destructive collision at slot {} for square {:?} subset {:#x} (existing {:#x}, new {:#x}) — constants/source likely corrupted",
                    slot, sq, sub, existing, attack,
                );
            }
            // existing == attack is a constructive collision — fine, no-op.
            sub = sub.wrapping_sub(mask) & mask;
            if sub == 0 {
                break;
            }
        }
    }
    table.into_boxed_slice()
}

fn build_bishop_table() -> Box<[u64]> {
    let mut table = vec![0u64; constants::BISHOP_TABLE_LEN];
    for sq in Square::all() {
        let m = &constants::BISHOP_MAGICS[sq.index() as usize];
        let mask = m.mask.0;
        let mut sub: u64 = 0;
        loop {
            let attack = slow_attacks::bishop_attacks(sq, Bitboard(sub)).0;
            let idx = sub.wrapping_mul(m.magic) >> m.shift;
            let slot = m.offset as usize + idx as usize;
            let existing = table[slot];
            if existing == 0 {
                table[slot] = attack;
            } else if existing != attack {
                panic!(
                    "magic table destructive collision at slot {} for square {:?} subset {:#x} (existing {:#x}, new {:#x}) — constants/source likely corrupted",
                    slot, sq, sub, existing, attack,
                );
            }
            sub = sub.wrapping_sub(mask) & mask;
            if sub == 0 {
                break;
            }
        }
    }
    table.into_boxed_slice()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slow_attacks;
    use std::thread;

    /// Build a Bitboard from a slice of squares. Mirrors the helper in
    /// `slow_attacks`'s test module.
    fn bb_of(squares: &[Square]) -> Bitboard {
        let mut bb = Bitboard::EMPTY;
        for &sq in squares {
            bb = bb.with(sq);
        }
        bb
    }

    // -------- spot checks against slow_attacks (lookup body) --------

    #[test]
    fn rook_attacks_starting_position_a1_matches_slow() {
        // Starting-position occupancy: ranks 1, 2, 7, 8 fully populated. White
        // rook on a1. Immediate blockers on a2 (north — own pawn) and b1 (east
        // — own knight). No south or west ray (a1 is the corner). Attack set
        // is exactly {a2, b1}.
        let occ = Bitboard::RANK_1 | Bitboard::RANK_2 | Bitboard::RANK_7 | Bitboard::RANK_8;
        let expected = bb_of(&[Square::A2, Square::B1]);
        let got = rook_attacks(Square::A1, occ);
        assert_eq!(got, expected);
        // Cross-check against the oracle so a sign-error in the literal would
        // still surface as a divergence.
        assert_eq!(got, slow_attacks::rook_attacks(Square::A1, occ));
    }

    #[test]
    fn bishop_attacks_d4_blocker_matches_slow() {
        // Bishop on d4 with one blocker on f6. NE ray stops at f6 inclusive
        // (e5, f6); g7 and h8 are excluded. The other three diagonals run to
        // the board edge: NW = c5, b6, a7; SW = c3, b2, a1; SE = e3, f2, g1.
        // 2 + 3 + 3 + 3 = 11 squares.
        let occ = Bitboard::from_square(Square::F6);
        let expected = bb_of(&[
            Square::E5,
            Square::F6,
            Square::C5,
            Square::B6,
            Square::A7,
            Square::C3,
            Square::B2,
            Square::A1,
            Square::E3,
            Square::F2,
            Square::G1,
        ]);
        let got = bishop_attacks(Square::D4, occ);
        assert_eq!(got, expected);
        assert_eq!(got, slow_attacks::bishop_attacks(Square::D4, occ));
    }

    #[test]
    fn queen_attacks_d4_combines_rook_and_bishop() {
        // For several pinned occupancies, queen = rook | bishop. Three
        // distinct occupancies — empty, full, and a starting-position
        // occupancy — exercise different blocker patterns.
        let starting_occ =
            Bitboard::RANK_1 | Bitboard::RANK_2 | Bitboard::RANK_7 | Bitboard::RANK_8;
        let cases: [Bitboard; 3] = [Bitboard::EMPTY, Bitboard::FULL, starting_occ];
        for occ in cases {
            let q = queen_attacks(Square::D4, occ);
            let combined = rook_attacks(Square::D4, occ) | bishop_attacks(Square::D4, occ);
            assert_eq!(
                q, combined,
                "queen_attacks(D4, {:?}) must equal rook|bishop",
                occ
            );
        }
    }

    // -------- structural invariants on the constants tables --------

    #[test]
    fn magic_struct_mask_matches_slow_attacks_mask() {
        for sq in Square::all() {
            let i = sq.index() as usize;
            assert_eq!(
                constants::ROOK_MAGICS[i].mask,
                slow_attacks::rook_mask(sq),
                "ROOK_MAGICS[{:?}].mask must equal slow_attacks::rook_mask({:?})",
                sq,
                sq
            );
            assert_eq!(
                constants::BISHOP_MAGICS[i].mask,
                slow_attacks::bishop_mask(sq),
                "BISHOP_MAGICS[{:?}].mask must equal slow_attacks::bishop_mask({:?})",
                sq,
                sq
            );
        }
    }

    #[test]
    fn magic_struct_shift_matches_mask_popcount() {
        for sq in Square::all() {
            let i = sq.index() as usize;
            let rook_expected = 64 - slow_attacks::rook_mask(sq).count() as u8;
            let bishop_expected = 64 - slow_attacks::bishop_mask(sq).count() as u8;
            assert_eq!(
                constants::ROOK_MAGICS[i].shift,
                rook_expected,
                "ROOK_MAGICS[{:?}].shift must equal 64 - mask.count()",
                sq
            );
            assert_eq!(
                constants::BISHOP_MAGICS[i].shift,
                bishop_expected,
                "BISHOP_MAGICS[{:?}].shift must equal 64 - mask.count()",
                sq
            );
        }
    }

    #[test]
    fn magic_offsets_partition_table() {
        // Strict cumulative-arithmetic invariant per `m1-magic-bitboards.md` §8:
        // for every i ≥ 1, offset[i] == offset[i-1] + slot_size[i-1]. This is
        // tighter than "strictly increasing" — strict-increase plus the global
        // sum-of-sizes-equals-table-len would still admit configurations with
        // simultaneous overlap+gap (e.g. offsets [0,2,8] with sizes [4,4,4]
        // sums to 12 with internal overlap at [2,4) and gap at [4,8)). The
        // cumulative form rules that out by construction.

        // Rook
        assert_eq!(
            constants::ROOK_MAGICS[0].offset,
            0,
            "ROOK_MAGICS[0].offset must be 0"
        );
        for i in 1..64usize {
            let m_prev = &constants::ROOK_MAGICS[i - 1];
            let m = &constants::ROOK_MAGICS[i];
            let prev_size = 1u64 << (64 - m_prev.shift as u32);
            let expected_offset = m_prev.offset as u64 + prev_size;
            let sq = Square::new(i as u8).expect("0..64 is always a valid square");
            assert_eq!(
                m.offset as u64,
                expected_offset,
                "ROOK_MAGICS[{:?}].offset must equal ROOK_MAGICS[{}].offset + 2^(64-shift) = {}",
                sq,
                i - 1,
                expected_offset
            );
        }
        // Last slot ends exactly at the table length — coverage with no gap.
        let last_rook = &constants::ROOK_MAGICS[63];
        let last_size_rook = 1usize << (64 - last_rook.shift as u32);
        assert_eq!(
            last_rook.offset as usize + last_size_rook,
            constants::ROOK_TABLE_LEN,
            "ROOK_MAGICS[63].offset + slot size must equal ROOK_TABLE_LEN"
        );

        // Bishop — symmetric.
        assert_eq!(
            constants::BISHOP_MAGICS[0].offset,
            0,
            "BISHOP_MAGICS[0].offset must be 0"
        );
        for i in 1..64usize {
            let m_prev = &constants::BISHOP_MAGICS[i - 1];
            let m = &constants::BISHOP_MAGICS[i];
            let prev_size = 1u64 << (64 - m_prev.shift as u32);
            let expected_offset = m_prev.offset as u64 + prev_size;
            let sq = Square::new(i as u8).expect("0..64 is always a valid square");
            assert_eq!(
                m.offset as u64,
                expected_offset,
                "BISHOP_MAGICS[{:?}].offset must equal BISHOP_MAGICS[{}].offset + 2^(64-shift) = {}",
                sq,
                i - 1,
                expected_offset
            );
        }
        let last_bishop = &constants::BISHOP_MAGICS[63];
        let last_size_bishop = 1usize << (64 - last_bishop.shift as u32);
        assert_eq!(
            last_bishop.offset as usize + last_size_bishop,
            constants::BISHOP_TABLE_LEN,
            "BISHOP_MAGICS[63].offset + slot size must equal BISHOP_TABLE_LEN"
        );
    }

    #[test]
    fn rook_table_len_matches_slot_size_sum() {
        let total: usize = (0..64usize)
            .map(|i| 1usize << (64 - constants::ROOK_MAGICS[i].shift as u32))
            .sum();
        assert_eq!(
            total,
            constants::ROOK_TABLE_LEN,
            "sum of rook per-square slot sizes must equal ROOK_TABLE_LEN"
        );
    }

    #[test]
    fn bishop_table_len_matches_slot_size_sum() {
        let total: usize = (0..64usize)
            .map(|i| 1usize << (64 - constants::BISHOP_MAGICS[i].shift as u32))
            .sum();
        assert_eq!(
            total,
            constants::BISHOP_TABLE_LEN,
            "sum of bishop per-square slot sizes must equal BISHOP_TABLE_LEN"
        );
    }

    // -------- exhaustive smoke tests over the lookup --------

    #[test]
    fn attack_lookup_does_not_panic_on_empty_occ() {
        // Every square's slot is reachable with the empty occupancy. Catches
        // an offset-out-of-bounds bug that the hand-picked spot tests above
        // would miss.
        for sq in Square::all() {
            let _ = rook_attacks(sq, Bitboard::EMPTY);
            let _ = bishop_attacks(sq, Bitboard::EMPTY);
        }
    }

    #[test]
    fn attack_lookup_does_not_panic_on_full_occ() {
        // Symmetric: every square × FULL. Exercises a different part of each
        // square's table than the empty case.
        for sq in Square::all() {
            let _ = rook_attacks(sq, Bitboard::FULL);
            let _ = bishop_attacks(sq, Bitboard::FULL);
        }
    }

    // -------- concurrent-read consistency --------

    #[test]
    fn concurrent_reads_consistent() {
        // Spawn 8 threads that each compute rook_attacks + bishop_attacks on
        // the same (sq, occ) and assert all returns agree. Pins concurrent
        // reads against the attack table return consistent values and don't
        // deadlock or panic. Does NOT pin first-init thread-safety: cargo's
        // shared-static-state runner means the LazyLock is almost certainly
        // already forced by an earlier sibling test by the time this runs.
        let sq = Square::D4;
        let occ = Bitboard::from_square(Square::D6).with(Square::F6);
        let mut handles = Vec::with_capacity(8);
        for _ in 0..8 {
            handles.push(thread::spawn(move || {
                let r = rook_attacks(sq, occ);
                let b = bishop_attacks(sq, occ);
                (r, b)
            }));
        }
        let results: Vec<(Bitboard, Bitboard)> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        let first = results[0];
        for (i, &res) in results.iter().enumerate() {
            assert_eq!(
                res, first,
                "thread {} produced a different (rook, bishop) tuple",
                i
            );
        }
        // Anchor the threaded result against the slow oracle so a "consistently
        // wrong" lookup couldn't pass this test on self-agreement alone.
        assert_eq!(
            first.0,
            slow_attacks::rook_attacks(sq, occ),
            "concurrent rook_attacks result must match slow oracle"
        );
        assert_eq!(
            first.1,
            slow_attacks::bishop_attacks(sq, occ),
            "concurrent bishop_attacks result must match slow oracle"
        );
    }

    // -------- throughput sanity (run with --ignored) --------

    /// Loose throughput sanity check, not a proper benchmark — that lands in
    /// M1.G via criterion. Reports rook lookups/sec on this machine; intended
    /// to surface "is the lookup in the right order of magnitude?" rather
    /// than to gate any release.
    ///
    /// Run with `cargo test --release -- --ignored bench_rook_attacks_throughput`.
    #[test]
    #[ignore]
    fn bench_rook_attacks_throughput() {
        use std::time::Instant;

        // Force the LazyLock to populate before timing.
        let _ = rook_attacks(Square::A1, Bitboard::EMPTY);

        // Eight diverse (sq, occ) pairs the loop cycles through. Different
        // squares + occupancies hit different cache lines, so the timing
        // reflects realistic mixed-access patterns rather than a single
        // hot slot.
        let cases: [(Square, Bitboard); 8] = [
            (Square::A1, Bitboard::EMPTY),
            (Square::H8, Bitboard::FULL),
            (Square::D4, Bitboard::RANK_2 | Bitboard::RANK_7),
            (Square::E1, Bitboard::FILE_E),
            (Square::A8, Bitboard::from_square(Square::A4)),
            (
                Square::H1,
                Bitboard::RANK_1 | Bitboard::RANK_2 | Bitboard::RANK_7 | Bitboard::RANK_8,
            ),
            (Square::D5, Bitboard::FILE_D | Bitboard::RANK_5),
            (Square::C6, Bitboard::from_square(Square::C2)),
        ];

        const ITERS: u32 = 10_000_000;
        let mut sink: u64 = 0;
        let start = Instant::now();
        for i in 0..ITERS {
            let (sq, occ) = cases[(i & 7) as usize];
            // wrapping_add into a u64 sink defeats dead-code elimination
            // without the XOR-parity-cancellation pitfall (over 10M iters of
            // 8 cycling cases, each attack-set XOR'd an even number of times
            // collapses to zero — wrapping_add of always-non-empty attack
            // sets accumulates instead).
            sink = sink.wrapping_add(rook_attacks(sq, occ).0);
        }
        let elapsed = start.elapsed();
        // Touch sink to keep it live.
        assert_ne!(
            sink, 0,
            "sink unexpectedly zero — DCE may have eaten the loop"
        );

        let nanos_per_call = elapsed.as_nanos() as f64 / ITERS as f64;
        let mnps = (ITERS as f64 / elapsed.as_secs_f64()) / 1_000_000.0;
        println!(
            "rook_attacks throughput: {:.2} ns/call, {:.1} M lookups/sec ({} iters in {:?})",
            nanos_per_call, mnps, ITERS, elapsed,
        );

        // Sanity floor: > 10 Mnps would be very surprising; > 1 Gnps would
        // mean LLVM constant-folded everything. These bounds are loose; the
        // real benchmark is M1.G's criterion harness.
        assert!(
            mnps > 10.0,
            "rook_attacks throughput {:.1} Mnps is below the 10-Mnps sanity floor",
            mnps
        );
        assert!(
            mnps < 5_000.0,
            "rook_attacks throughput {:.1} Mnps exceeds 5 Gnps — likely LLVM constant-folded the loop",
            mnps
        );
    }
}
