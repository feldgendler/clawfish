//! Gold-standard differential between the magic-bitboard sliding-piece attack
//! lookup (`clawfish::magic`) and the slow ray-walker oracle
//! (`clawfish::slow_attacks`).
//!
//! Why `occ ⊆ mask` is exhaustive: the magic lookup begins by computing
//! `(occ & m.mask)` — any bits in `occ` outside of `m.mask` are masked off and
//! cannot affect the lookup result. Conversely, every subset of `m.mask`
//! reaches a (possibly distinct) slot in the per-square attack table. So
//! enumerating all `2^popcount(mask)` subsets of the relevant-blocker mask via
//! the carry-rippler trick and asserting `magic == slow` at every (square,
//! subset) pair fully covers the lookup contract on the input domain it cares
//! about. Per `docs/research/m1-magic-bitboards.md` §7 this is the gold-standard
//! validation strategy.
//!
//! Inputs with bits outside the mask are not exercised here — they are covered
//! by the spot tests in `magic.rs`'s inline `mod tests` (e.g.
//! `rook_attacks_starting_position_a1_matches_slow`), which pass full-board
//! occupancies through the lookup and thereby exercise the `& m.mask` masking
//! step end-to-end. The differential and the spot tests together cover the
//! full lookup contract.

use clawfish::{Bitboard, Square, magic, slow_attacks};

#[test]
fn rook_magic_matches_slow_for_every_subset() {
    for sq in Square::all() {
        let mask = slow_attacks::rook_mask(sq);
        let mut sub: u64 = 0;
        loop {
            let occ = Bitboard(sub);
            let slow = slow_attacks::rook_attacks(sq, occ);
            let fast = magic::rook_attacks(sq, occ);
            assert_eq!(fast, slow, "rook mismatch on {sq:?} occ {occ:?}");
            // Carry-rippler step: advance to the next subset of `mask`. When
            // `sub` wraps back to 0 we have visited every subset exactly once
            // (the empty subset was checked once at the top of the loop).
            sub = sub.wrapping_sub(mask.0) & mask.0;
            if sub == 0 {
                break;
            }
        }
    }
}

#[test]
fn bishop_magic_matches_slow_for_every_subset() {
    for sq in Square::all() {
        let mask = slow_attacks::bishop_mask(sq);
        let mut sub: u64 = 0;
        loop {
            let occ = Bitboard(sub);
            let slow = slow_attacks::bishop_attacks(sq, occ);
            let fast = magic::bishop_attacks(sq, occ);
            assert_eq!(fast, slow, "bishop mismatch on {sq:?} occ {occ:?}");
            sub = sub.wrapping_sub(mask.0) & mask.0;
            if sub == 0 {
                break;
            }
        }
    }
}
