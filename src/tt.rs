//! Transposition table (TT): Zobrist-keyed cache of previously-searched positions.
//!
//! Implements the design committed in ADR-0018 (`docs/decisions/0018-transposition-table.md`)
//! and detailed in the M4.A plan (`docs/plans/m4.a.md` §3.1–3.4).
//!
//! # Thread-safety contract
//!
//! `TranspositionTable` stores its backing vector behind `UnsafeCell` rather
//! than `Mutex` to avoid ~10 ns lock-acquisition overhead at ~23 M probes/sec.
//! The `unsafe impl Sync` is sound under the ADR-0011 single-mutator invariant:
//! only the per-`go` worker thread probes/stores during a search; the
//! orchestrator mutates (via `resize` / `clear`) only after
//! `Engine::join_in_flight_worker()` returns. No cross-thread access to the
//! vector ever overlaps. The `generation` and `mask` fields use `AtomicU8` /
//! `AtomicUsize` respectively so the orchestrator can read them under `Sync`
//! without unsafe.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering::Relaxed};

use crate::search::MATE_IN_MAX_PLY;

// ---------------------------------------------------------------------------
// TtBound
// ---------------------------------------------------------------------------

/// Bound type stored with each TT entry.
///
/// - `Exact`: the stored score is the position's true minimax value.
/// - `Lower`: the search produced a beta-cutoff; the score is a lower bound.
/// - `Upper`: no move improved alpha (fail-low); the score is an upper bound.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum TtBound {
    Exact = 0,
    Lower = 1,
    Upper = 2,
}

impl TtBound {
    /// Decode a 2-bit field into a `TtBound`. Only used on values packed by
    /// `TtEntry::pack_age_bound`, so only 0–2 are ever produced.
    fn from_u8(b: u8) -> TtBound {
        debug_assert!(b <= 2, "TtBound discriminant out of range: {b}");
        match b {
            0 => TtBound::Exact,
            1 => TtBound::Lower,
            2 => TtBound::Upper,
            _ => unreachable!("TtBound discriminant > 2; pack_age_bound masks to 2 bits"),
        }
    }
}

// ---------------------------------------------------------------------------
// TtEntry
// ---------------------------------------------------------------------------

/// 16-byte transposition-table entry. 4 entries per 64-byte cache line.
///
/// Field layout per ADR-0018 §3:
/// - `key` (8 bytes): full Zobrist key for collision detection.
/// - `score` (2 bytes): mate-adjusted score (see `score_to_tt`).
/// - `depth` (1 byte): depth at which the entry was stored.
/// - `age_and_bound` (1 byte): bits [7..2] = age (6-bit generation), bits [1..0] = [`TtBound`].
/// - `best_move` (2 bytes): packed [`Move`] bits; 0 = no move stored.
/// - `_pad` (2 bytes): structural padding to reach 16 bytes; always zero.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub(crate) struct TtEntry {
    pub key: u64,
    pub score: i16,
    pub depth: u8,
    pub age_and_bound: u8,
    pub best_move: u16,
    pub _pad: u16,
}

impl TtEntry {
    /// True when the slot has never been written (all fields zero).
    pub fn is_empty(&self) -> bool {
        self.key == 0 && self.depth == 0 && self.age_and_bound == 0 && self.best_move == 0
    }

    /// Extract the bound from `age_and_bound` bits [1..0].
    pub fn bound(&self) -> TtBound {
        TtBound::from_u8(self.age_and_bound & 0b11)
    }

    /// Extract the 6-bit generation from `age_and_bound` bits [7..2].
    pub fn age(&self) -> u8 {
        self.age_and_bound >> 2
    }

    /// Pack a 6-bit `age` and a [`TtBound`] into the `age_and_bound` byte.
    pub fn pack_age_bound(age: u8, bound: TtBound) -> u8 {
        (age & 0x3F) << 2 | bound as u8
    }
}

// ---------------------------------------------------------------------------
// TtData — call-site bundle for store()
// ---------------------------------------------------------------------------

/// Input bundle for [`TranspositionTable::store`].
///
/// Groups the four store parameters so callers don't pass a wide positional list.
pub(crate) struct TtData {
    pub score: i16,
    pub depth: u8,
    pub bound: TtBound,
    /// Packed `Move` bits; 0 means "no best move to store" (triggering
    /// best-move preservation from the old entry when keys match).
    pub best_move: u16,
}

// ---------------------------------------------------------------------------
// TranspositionTable
// ---------------------------------------------------------------------------

/// Power-of-two-sized direct-mapped transposition table.
///
/// Indexed by `key & mask`. Single-threaded for M4–M7 under the ADR-0011
/// invariant; the lockless M8 variant replaces `UnsafeCell<Vec<TtEntry>>`
/// with `Vec<AtomicU64>` pairs.
pub(crate) struct TranspositionTable {
    entries: UnsafeCell<Vec<TtEntry>>,
    /// `entries.len() - 1`; power-of-two mask. Stored as `AtomicUsize` so
    /// `entry_count()` and `index_for()` are callable under `&self` without
    /// entering the `UnsafeCell`.
    mask: AtomicUsize,
    /// 6-bit search generation. Advanced by `new_search()`; reset by `clear()`.
    generation: AtomicU8,
}

// SAFETY: ADR-0011 + ADR-0018 §2 invariant — only the per-`go` worker thread
// accesses `entries` during a search; the orchestrator accesses it only after
// `Engine::join_in_flight_worker()` returns. No concurrent access ever
// overlaps. `mask` and `generation` are atomic.
unsafe impl Sync for TranspositionTable {}

impl TranspositionTable {
    /// Allocate a table of approximately `size_mib` MiB.
    ///
    /// The actual entry count is the largest power of two that fits in the
    /// requested byte budget (16 bytes per entry). Panics if `size_mib == 0`.
    pub fn new(size_mib: usize) -> Self {
        assert!(
            size_mib > 0,
            "TranspositionTable size must be at least 1 MiB"
        );
        let n = entry_count_for_mib(size_mib);
        let entries = vec![TtEntry::default(); n];
        TranspositionTable {
            entries: UnsafeCell::new(entries),
            mask: AtomicUsize::new(n - 1),
            generation: AtomicU8::new(0),
        }
    }

    /// Rebuild the table at a new size. All prior entries are discarded.
    ///
    /// Must only be called after `Engine::join_in_flight_worker()` returns
    /// (ADR-0011 / ADR-0018 §2 invariant).
    pub fn resize(&self, size_mib: usize) {
        assert!(
            size_mib > 0,
            "TranspositionTable size must be at least 1 MiB"
        );
        let n = entry_count_for_mib(size_mib);
        // SAFETY: caller upholds the single-mutator invariant.
        let entries = unsafe { &mut *self.entries.get() };
        *entries = vec![TtEntry::default(); n];
        self.mask.store(n - 1, Relaxed);
        self.generation.store(0, Relaxed);
    }

    /// Zero all entries and reset the generation counter to 0.
    ///
    /// Must only be called after `Engine::join_in_flight_worker()` returns.
    pub fn clear(&self) {
        // SAFETY: caller upholds the single-mutator invariant.
        let entries = unsafe { &mut *self.entries.get() };
        entries.fill(TtEntry::default());
        self.generation.store(0, Relaxed);
    }

    /// Advance the generation counter once per `go` command.
    ///
    /// The 6-bit counter wraps at 64: `gen = (gen + 1) & 0x3F`.
    pub fn new_search(&self) {
        let g = self.generation.load(Relaxed);
        self.generation.store((g + 1) & 0x3F, Relaxed);
    }

    /// Look up `key` in the table.
    ///
    /// Returns `Some(entry)` on a key match; `None` on a miss or an empty slot.
    pub fn probe(&self, key: u64) -> Option<TtEntry> {
        let idx = self.index_for(key);
        // SAFETY: single-mutator invariant; no concurrent write.
        let entry = unsafe { (&*self.entries.get())[idx] };
        if entry.is_empty() || entry.key != key {
            None
        } else {
            Some(entry)
        }
    }

    /// Store search results for `key`, applying the depth-preferred + age-bias
    /// replacement scheme from ADR-0018 §1.
    ///
    /// When the new entry has no best move (`data.best_move == 0`) and the
    /// current slot holds the same position (`old.key == key`) with a known
    /// move, the old move is preserved (ADR-0018 §7).
    pub fn store(&self, key: u64, data: TtData) {
        // Invariant: stored entries always have `depth >= 1`. The
        // `is_empty()` check distinguishes stored from default by leaving
        // `score` and `_pad` out of the equality test; if a future caller
        // ever stores `depth == 0`, an entry with `key == 0 && bound ==
        // Exact && best_move == 0` would alias with a default entry on
        // probe. Negamax never stores at `depth == 0` (qsearch handles
        // the horizon), so the invariant holds today; pin it explicitly
        // so a future qsearch-in-TT (M5) adopts a different empty-slot
        // discriminator before going to depth=0.
        debug_assert!(
            data.depth >= 1,
            "TT store with depth=0 breaks is_empty() distinguishability"
        );
        let idx = self.index_for(key);
        // SAFETY: single-mutator invariant; no concurrent write.
        let entries = unsafe { &mut *self.entries.get() };
        let old = entries[idx];
        let current_gen = self.generation();

        let replace = old.is_empty() || old.age() != current_gen || old.depth <= data.depth;

        if replace {
            let preserved_move = if data.best_move == 0 && old.key == key && old.best_move != 0 {
                old.best_move
            } else {
                data.best_move
            };
            entries[idx] = TtEntry {
                key,
                score: data.score,
                depth: data.depth,
                age_and_bound: TtEntry::pack_age_bound(current_gen, data.bound),
                best_move: preserved_move,
                _pad: 0,
            };
        }
    }

    /// Number of entries in the table (always a power of two).
    #[cfg(test)]
    pub fn entry_count(&self) -> usize {
        self.mask.load(Relaxed) + 1
    }

    /// Current generation counter value, in `[0, 63]`.
    pub fn generation(&self) -> u8 {
        self.generation.load(Relaxed)
    }

    /// Compute the table index for `key`.
    pub fn index_for(&self, key: u64) -> usize {
        (key as usize) & self.mask.load(Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Size computation helper
// ---------------------------------------------------------------------------

/// Compute the entry count for a table of `size_mib` MiB, rounded down to the
/// nearest power of two.
fn entry_count_for_mib(size_mib: usize) -> usize {
    let raw = size_mib * 1024 * 1024 / 16;
    debug_assert!(raw >= 1, "size_mib too small to hold even one entry");
    // ilog2 rounds down, so 1 << ilog2(raw) is the floor power of two.
    1usize << raw.ilog2()
}

// ---------------------------------------------------------------------------
// Mate-score store/probe adjustments
// ---------------------------------------------------------------------------

/// Adjust a score for storage in the TT.
///
/// Mate scores encode distance to mate from the *root*. To make the stored
/// score position-independent (so it can be reused when the same position is
/// reached at a different ply), we encode it as distance from the *node* by
/// adding `ply` to a positive mate score and subtracting `ply` from a negative
/// one. Non-mate scores are left unchanged.
pub(crate) fn score_to_tt(score: i32, ply: i32) -> i32 {
    if score > MATE_IN_MAX_PLY {
        score + ply
    } else if score < -MATE_IN_MAX_PLY {
        score - ply
    } else {
        score
    }
}

/// Recover the root-relative score when probing the TT at `ply`.
///
/// Inverse of [`score_to_tt`]: subtract `ply` from a positive stored mate
/// score and add `ply` to a negative one.
pub(crate) fn score_from_tt(score: i32, ply: i32) -> i32 {
    if score > MATE_IN_MAX_PLY {
        score - ply
    } else if score < -MATE_IN_MAX_PLY {
        score + ply
    } else {
        score
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // Constants mirrored from search for readability in tests.
    const MATE: i32 = 30_000;
    const INF: i32 = 30_001;
    const MAX_PLY: i32 = 64;

    // -----------------------------------------------------------------------
    // T1 — default entry is empty
    // -----------------------------------------------------------------------

    #[test]
    fn tt_entry_default_is_empty() {
        let e = TtEntry::default();
        assert_eq!(e.key, 0);
        assert_eq!(e.depth, 0);
        assert_eq!(e.age_and_bound, 0);
        assert_eq!(e.best_move, 0);
        assert!(e.is_empty());
    }

    // -----------------------------------------------------------------------
    // T2 — age/bound round-trip for several (age, bound) combinations
    // -----------------------------------------------------------------------

    #[test]
    fn tt_entry_pack_age_bound_round_trip() {
        let cases: &[(u8, TtBound)] = &[
            (0, TtBound::Exact),
            (0, TtBound::Lower),
            (0, TtBound::Upper),
            (1, TtBound::Exact),
            (1, TtBound::Lower),
            (1, TtBound::Upper),
            (32, TtBound::Exact),
            (32, TtBound::Lower),
            (32, TtBound::Upper),
            (63, TtBound::Exact),
            (63, TtBound::Lower),
            (63, TtBound::Upper),
        ];

        for &(age, bound) in cases {
            let packed = TtEntry::pack_age_bound(age, bound);
            let entry = TtEntry {
                age_and_bound: packed,
                ..TtEntry::default()
            };
            assert_eq!(
                entry.age(),
                age,
                "age mismatch for age={age} bound={bound:?}"
            );
            assert_eq!(
                entry.bound(),
                bound,
                "bound mismatch for age={age} bound={bound:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // T3 — 16 MiB → 1,048,576 entries (exact power of two)
    // -----------------------------------------------------------------------

    #[test]
    fn tt_new_size_is_power_of_two_entries() {
        let tt = TranspositionTable::new(16);
        let count = tt.entry_count();
        assert_eq!(count, 16 * 1024 * 1024 / 16);
        assert!(count.is_power_of_two());
    }

    // -----------------------------------------------------------------------
    // T4 — non-power-of-two MiB still yields a power-of-two entry count
    // -----------------------------------------------------------------------

    #[test]
    fn tt_new_size_2_mib_yields_131072_entries() {
        // 2 MiB / 16 bytes = 131,072 entries, which is exactly 2^17.
        let tt = TranspositionTable::new(2);
        let count = tt.entry_count();
        assert_eq!(count, 131_072);
        assert!(count.is_power_of_two());

        // 3 MiB / 16 = 196,608 → floor to 2^17 = 131,072.
        let tt3 = TranspositionTable::new(3);
        assert_eq!(tt3.entry_count(), 131_072);
        assert!(tt3.entry_count().is_power_of_two());
    }

    // -----------------------------------------------------------------------
    // T5 — new(0) panics
    // -----------------------------------------------------------------------

    #[test]
    #[should_panic]
    fn tt_new_panics_on_zero_size() {
        let _ = TranspositionTable::new(0);
    }

    // -----------------------------------------------------------------------
    // T6 — resize clears entries
    // -----------------------------------------------------------------------

    #[test]
    fn tt_resize_clears_entries() {
        let tt = TranspositionTable::new(1);
        let key = 0xDEAD_BEEF_1234_5678_u64;
        tt.store(
            key,
            TtData {
                score: 42,
                depth: 3,
                bound: TtBound::Exact,
                best_move: 0,
            },
        );
        assert!(tt.probe(key).is_some());

        tt.resize(8);

        assert!(tt.probe(key).is_none(), "entries must be cleared on resize");
    }

    // -----------------------------------------------------------------------
    // T7 — clear zeroes all entries
    // -----------------------------------------------------------------------

    #[test]
    fn tt_clear_zeroes_all_entries() {
        let tt = TranspositionTable::new(1);
        let keys: &[u64] = &[1, 2, 0xFFFF_0000_1111_AAAA, 99];
        for &key in keys {
            tt.store(
                key,
                TtData {
                    score: 10,
                    depth: 4,
                    bound: TtBound::Lower,
                    best_move: 0,
                },
            );
        }

        tt.clear();

        for &key in keys {
            assert!(
                tt.probe(key).is_none(),
                "key {key:#x} should be gone after clear"
            );
        }
    }

    // -----------------------------------------------------------------------
    // T7b — clear resets generation to 0
    // -----------------------------------------------------------------------

    #[test]
    fn tt_clear_resets_generation_to_zero() {
        let tt = TranspositionTable::new(1);
        tt.new_search();
        tt.new_search();
        tt.new_search();
        assert_eq!(tt.generation(), 3);

        tt.clear();

        assert_eq!(tt.generation(), 0);
    }

    // -----------------------------------------------------------------------
    // T8 — probe on a never-stored key returns None
    // -----------------------------------------------------------------------

    #[test]
    fn tt_probe_miss_returns_none() {
        let tt = TranspositionTable::new(1);
        assert!(tt.probe(0xABCD_1234_5678_9ABC).is_none());
    }

    // -----------------------------------------------------------------------
    // T9 — store + probe round-trip preserves all fields
    // -----------------------------------------------------------------------

    #[test]
    fn tt_probe_hit_returns_stored_entry() {
        let tt = TranspositionTable::new(1);
        let key = 0x1111_2222_3333_4444_u64;
        let data = TtData {
            score: -500,
            depth: 7,
            bound: TtBound::Upper,
            best_move: 0xAB12,
        };

        tt.store(key, data);
        let hit = tt.probe(key).expect("should hit after store");

        assert_eq!(hit.key, key);
        assert_eq!(hit.score, -500);
        assert_eq!(hit.depth, 7);
        assert_eq!(hit.bound(), TtBound::Upper);
        assert_eq!(hit.age(), tt.generation());
        assert_eq!(hit.best_move, 0xAB12);
        assert_eq!(hit._pad, 0);
    }

    // -----------------------------------------------------------------------
    // T10 — two keys colliding to the same slot: second probe returns None
    // -----------------------------------------------------------------------

    #[test]
    fn tt_probe_collision_returns_none() {
        // Use a 1-entry table so any two distinct keys collide.
        let entries_ptr: Vec<TtEntry> = vec![TtEntry::default(); 1];
        drop(entries_ptr); // We'll use the TranspositionTable API instead.

        // Construct a 1 MiB table (131072 entries). Find two keys that hash
        // to the same index by picking keys that differ only in bits above
        // the mask.
        let tt = TranspositionTable::new(1); // 65536 entries, mask = 65535
        let mask = tt.entry_count() - 1;
        let key_a: u64 = 7; // index = 7
        let key_b: u64 = key_a | ((mask as u64 + 1) * 3); // index = 7 as well

        assert_eq!(
            key_a as usize & mask,
            key_b as usize & mask,
            "keys must collide"
        );

        tt.store(
            key_a,
            TtData {
                score: 100,
                depth: 5,
                bound: TtBound::Exact,
                best_move: 0,
            },
        );
        // probe key_b — slot is occupied by key_a, key mismatch → None
        assert!(tt.probe(key_b).is_none());
    }

    // -----------------------------------------------------------------------
    // T11 — depth-preferred replacement (>= not >)
    // -----------------------------------------------------------------------

    #[test]
    fn tt_store_replaces_when_new_depth_geq_old_depth() {
        let tt = TranspositionTable::new(1);
        let key = 0xFACE_CAFE_BABE_1234_u64;

        // Store depth=5
        tt.store(
            key,
            TtData {
                score: 50,
                depth: 5,
                bound: TtBound::Exact,
                best_move: 0x0101,
            },
        );
        assert_eq!(tt.probe(key).unwrap().depth, 5);

        // Same generation, same depth (5) → replaced (<=)
        tt.store(
            key,
            TtData {
                score: 60,
                depth: 5,
                bound: TtBound::Lower,
                best_move: 0x0202,
            },
        );
        {
            let e = tt.probe(key).unwrap();
            assert_eq!(e.depth, 5);
            assert_eq!(e.score, 60, "same-depth store should replace");
        }

        // depth 6 → replaced
        tt.store(
            key,
            TtData {
                score: 70,
                depth: 6,
                bound: TtBound::Exact,
                best_move: 0x0303,
            },
        );
        assert_eq!(
            tt.probe(key).unwrap().score,
            70,
            "deeper store should replace"
        );

        // depth 4, same generation → NOT replaced (4 <= 6 is false)
        tt.store(
            key,
            TtData {
                score: 0,
                depth: 4,
                bound: TtBound::Upper,
                best_move: 0,
            },
        );
        assert_eq!(
            tt.probe(key).unwrap().score,
            70,
            "shallower same-gen store must not replace"
        );
    }

    // -----------------------------------------------------------------------
    // T12 — old-generation entry is always replaced regardless of depth
    // -----------------------------------------------------------------------

    #[test]
    fn tt_store_replaces_when_old_age_differs() {
        let tt = TranspositionTable::new(1);
        let key = 0x0ACE_DEAD_0000_0001_u64;

        // Store at generation 0 with depth=10
        tt.store(
            key,
            TtData {
                score: 999,
                depth: 10,
                bound: TtBound::Exact,
                best_move: 0,
            },
        );
        assert_eq!(tt.probe(key).unwrap().depth, 10);

        // Advance generation
        tt.new_search();
        assert_eq!(tt.generation(), 1);

        // Store new entry with depth=2 — old age (0) != current (1), so replaced
        tt.store(
            key,
            TtData {
                score: 1,
                depth: 2,
                bound: TtBound::Lower,
                best_move: 0,
            },
        );
        let e = tt.probe(key).unwrap();
        assert_eq!(
            e.depth, 2,
            "old-generation entry must be replaced even if new depth is shallower"
        );
        assert_eq!(e.score, 1);
    }

    // -----------------------------------------------------------------------
    // T13 — best-move preserved on fail-low when key matches
    // -----------------------------------------------------------------------

    #[test]
    fn tt_store_preserves_old_best_move_on_fail_low_with_no_move() {
        let tt = TranspositionTable::new(1);
        let key = 0xBEEF_CAFE_1234_5678_u64;
        let m1: u16 = 0x1A2B;
        let m2: u16 = 0x3C4D;

        // Store with best_move=M1, depth=5
        tt.store(
            key,
            TtData {
                score: 100,
                depth: 5,
                bound: TtBound::Exact,
                best_move: m1,
            },
        );
        assert_eq!(tt.probe(key).unwrap().best_move, m1);

        // Store with best_move=0 (fail-low / Upper), same key, depth=6 → replaces but preserves M1
        tt.store(
            key,
            TtData {
                score: -50,
                depth: 6,
                bound: TtBound::Upper,
                best_move: 0,
            },
        );
        assert_eq!(
            tt.probe(key).unwrap().best_move,
            m1,
            "old best_move must be preserved when new entry has no move and key matches"
        );

        // But when a real best_move is provided, it overwrites M1
        tt.store(
            key,
            TtData {
                score: 200,
                depth: 7,
                bound: TtBound::Lower,
                best_move: m2,
            },
        );
        assert_eq!(
            tt.probe(key).unwrap().best_move,
            m2,
            "new best_move must overwrite old when explicitly provided"
        );
    }

    // -----------------------------------------------------------------------
    // T14 — best-move NOT preserved on key mismatch (different position)
    // -----------------------------------------------------------------------

    #[test]
    fn tt_store_does_not_preserve_best_move_on_key_mismatch() {
        let tt = TranspositionTable::new(1);
        // Use a 1-entry table equivalent by choosing colliding keys.
        // Re-use the collision construction from T10.
        let mask = tt.entry_count() - 1;
        let key_a: u64 = 42;
        let key_b: u64 = key_a | ((mask as u64 + 1) * 5);
        assert_eq!(key_a as usize & mask, key_b as usize & mask);

        let m1: u16 = 0xAAAA;

        // Store key_a with best_move=M1
        tt.store(
            key_a,
            TtData {
                score: 50,
                depth: 5,
                bound: TtBound::Exact,
                best_move: m1,
            },
        );

        // Store key_b at the same slot with best_move=0 — different key, no preservation
        tt.store(
            key_b,
            TtData {
                score: 0,
                depth: 6,
                bound: TtBound::Upper,
                best_move: 0,
            },
        );
        let e = tt.probe(key_b).unwrap();
        assert_eq!(
            e.best_move, 0,
            "best_move must NOT be preserved when the colliding slot had a different key"
        );
    }

    // -----------------------------------------------------------------------
    // T15 — new_search wraps generation at 64
    // -----------------------------------------------------------------------

    #[test]
    fn tt_new_search_advances_generation_modulo_64() {
        let tt = TranspositionTable::new(1);
        assert_eq!(tt.generation(), 0);

        for _ in 0..65 {
            tt.new_search();
        }

        // 65 increments from 0: 65 mod 64 = 1
        assert_eq!(tt.generation(), 1);
    }

    // -----------------------------------------------------------------------
    // T16 — score_to_tt is a no-op for normal (non-mate) scores
    // -----------------------------------------------------------------------

    #[test]
    fn score_to_tt_no_op_for_normal_score() {
        assert_eq!(score_to_tt(150, 8), 150);
        assert_eq!(score_to_tt(-300, 12), -300);
        assert_eq!(score_to_tt(0, 0), 0);
    }

    // -----------------------------------------------------------------------
    // T17 — score_to_tt adjusts positive mate scores
    // -----------------------------------------------------------------------

    #[test]
    fn score_to_tt_adjusts_positive_mate() {
        // MATE - 3 = 29997 > MATE_IN_MAX_PLY = 29936 → mate score
        assert_eq!(score_to_tt(MATE - 3, 5), MATE - 3 + 5); // = MATE + 2
        assert_eq!(score_to_tt(MATE - 3, 5), MATE + 2);
    }

    // -----------------------------------------------------------------------
    // T18 — score_to_tt adjusts negative mate scores
    // -----------------------------------------------------------------------

    #[test]
    fn score_to_tt_adjusts_negative_mate() {
        // -(MATE - 3) = -29997 < -MATE_IN_MAX_PLY = -29936 → negative mate score
        assert_eq!(score_to_tt(-(MATE - 3), 5), -(MATE - 3) - 5); // = -(MATE + 2)
        assert_eq!(score_to_tt(-(MATE - 3), 5), -(MATE + 2));
    }

    // -----------------------------------------------------------------------
    // T19 — score_from_tt inverts score_to_tt (proptest round-trip)
    // -----------------------------------------------------------------------

    proptest! {
        #[test]
        fn score_from_tt_inverts_score_to_tt(
            score in -INF..=INF,
            ply in 0..=MAX_PLY,
        ) {
            let stored = score_to_tt(score, ply);
            let recovered = score_from_tt(stored, ply);
            prop_assert_eq!(recovered, score);
        }
    }

    // -----------------------------------------------------------------------
    // T20 — worked example matching ADR-0018 §5
    // -----------------------------------------------------------------------

    #[test]
    fn score_from_tt_returns_correct_distance_at_different_plies() {
        // Mate-in-2 found at ply 0 → search returns MATE - 4 (4 plies to mate).
        // ply 0 → no adjustment (MATE - 4 is NOT > MATE_IN_MAX_PLY? Let's check:
        // MATE_IN_MAX_PLY = 29936; MATE - 4 = 29996 > 29936 → IS a mate score!
        // But at ply 0: score_to_tt(29996, 0) = 29996 + 0 = 29996. No-op only
        // because ply = 0.

        // Assertion 1: ply=0 no-op (adding 0 doesn't change the value).
        let mate4 = MATE - 4; // 29996
        assert!(mate4 > MATE_IN_MAX_PLY, "sanity: MATE-4 is a mate score");
        assert_eq!(score_to_tt(mate4, 0), mate4);

        // Assertion 2: store mate-in-3 (MATE-3=29997) at ply 5.
        // score_to_tt(MATE - 3, 5) = (MATE - 3) + 5 = MATE + 2 = 30002.
        let stored = score_to_tt(MATE - 3, 5);
        assert_eq!(stored, MATE + 2);

        // Assertion 3: probe at ply 5 round-trips.
        assert_eq!(score_from_tt(stored, 5), MATE - 3);

        // Assertion 4: the same stored value probed at a different ply gives the
        // correct mate distance from that ply.
        // Stored = MATE + 2 (distance from the actual mate node = 0 + 2 plies
        // of adjustment). At ply 3: score_from_tt(MATE + 2, 3) = (MATE + 2) - 3 = MATE - 1.
        // This represents "mate in 1 more ply from this position", which is correct
        // if the same absolute mate node is 1 ply away from ply-3's position.
        assert_eq!(score_from_tt(MATE + 2, 3), MATE - 1);
    }

    // -----------------------------------------------------------------------
    // T20b — Negative-mate quadrant deterministic worked example.
    //
    // T19's proptest covers the (negative score, high ply) probe quadrant
    // statistically; T20b pins the same quadrant deterministically so the
    // sign convention is documented in the test surface, not buried in
    // sampled-only assertions.
    // -----------------------------------------------------------------------

    #[test]
    fn score_to_tt_and_score_from_tt_negative_mate_quadrant_round_trip() {
        // "Mated in 2" found at ply 5 → negamax returns -(MATE - 4) = -29996.
        let mated = -(MATE - 4); // -29996
        assert!(
            mated < -MATE_IN_MAX_PLY,
            "sanity: -(MATE-4) is a negative mate score"
        );
        // Store at ply 5: score_to_tt(-29996, 5) = -29996 - 5 = -30001.
        let stored = score_to_tt(mated, 5);
        assert_eq!(stored, -(MATE + 1));
        // Probe at ply 5: round-trip → -29996.
        assert_eq!(score_from_tt(stored, 5), mated);
        // Probe the same stored value at a different ply (ply 8) yields a
        // ply-shifted view of the same absolute mate node:
        // score_from_tt(-30001, 8) = -30001 + 8 = -29993 = -(MATE - 7).
        assert_eq!(score_from_tt(-(MATE + 1), 8), -(MATE - 7));
    }

    // -----------------------------------------------------------------------
    // T20c — Boundary case at exactly ±MATE_IN_MAX_PLY.
    //
    // The mate-score predicates use strict inequality: `score > MATE_IN_MAX_PLY`
    // and `score < -MATE_IN_MAX_PLY`. The boundary case `score == ±MATE_IN_MAX_PLY`
    // exactly is non-mate (centipawn) and must NOT be ply-adjusted. Without
    // this test, mutations like `>` → `>=` (or `<` → `<=`) would survive — at
    // the boundary value the mutated predicate flips, but no test in T17/T18/
    // T19/T20/T20b/T19-proptest hits the exact boundary integer (proptest's
    // i32 sampling has ~1/2^32 chance of hitting it).
    // -----------------------------------------------------------------------

    #[test]
    fn score_to_tt_boundary_at_mate_in_max_ply_is_no_op() {
        // score == MATE_IN_MAX_PLY is the largest non-mate cp score.
        // Must not be ply-adjusted regardless of ply.
        assert_eq!(score_to_tt(MATE_IN_MAX_PLY, 0), MATE_IN_MAX_PLY);
        assert_eq!(score_to_tt(MATE_IN_MAX_PLY, 5), MATE_IN_MAX_PLY);
        assert_eq!(score_to_tt(MATE_IN_MAX_PLY, 32), MATE_IN_MAX_PLY);
        // Symmetric negative-boundary case.
        assert_eq!(score_to_tt(-MATE_IN_MAX_PLY, 0), -MATE_IN_MAX_PLY);
        assert_eq!(score_to_tt(-MATE_IN_MAX_PLY, 5), -MATE_IN_MAX_PLY);
        assert_eq!(score_to_tt(-MATE_IN_MAX_PLY, 32), -MATE_IN_MAX_PLY);
    }

    #[test]
    fn score_from_tt_boundary_at_mate_in_max_ply_is_no_op() {
        // Symmetric to score_to_tt: probe-side adjustment must also leave
        // the boundary unchanged.
        assert_eq!(score_from_tt(MATE_IN_MAX_PLY, 0), MATE_IN_MAX_PLY);
        assert_eq!(score_from_tt(MATE_IN_MAX_PLY, 5), MATE_IN_MAX_PLY);
        assert_eq!(score_from_tt(MATE_IN_MAX_PLY, 32), MATE_IN_MAX_PLY);
        assert_eq!(score_from_tt(-MATE_IN_MAX_PLY, 0), -MATE_IN_MAX_PLY);
        assert_eq!(score_from_tt(-MATE_IN_MAX_PLY, 5), -MATE_IN_MAX_PLY);
        assert_eq!(score_from_tt(-MATE_IN_MAX_PLY, 32), -MATE_IN_MAX_PLY);
    }

    #[test]
    fn score_to_tt_just_above_mate_in_max_ply_does_adjust() {
        // The point just above the boundary IS a mate score and must be adjusted.
        // This pins that we adjust at MATE_IN_MAX_PLY + 1, not at MATE_IN_MAX_PLY.
        assert_eq!(score_to_tt(MATE_IN_MAX_PLY + 1, 5), MATE_IN_MAX_PLY + 1 + 5);
        assert_eq!(
            score_from_tt(MATE_IN_MAX_PLY + 1, 5),
            MATE_IN_MAX_PLY + 1 - 5
        );
        assert_eq!(
            score_to_tt(-(MATE_IN_MAX_PLY + 1), 5),
            -(MATE_IN_MAX_PLY + 1) - 5
        );
        assert_eq!(
            score_from_tt(-(MATE_IN_MAX_PLY + 1), 5),
            -(MATE_IN_MAX_PLY + 1) + 5
        );
    }

    // -----------------------------------------------------------------------
    // T21 — TtEntry is exactly 16 bytes
    // -----------------------------------------------------------------------

    #[test]
    fn tt_entry_size_is_16_bytes() {
        assert_eq!(std::mem::size_of::<TtEntry>(), 16);
    }

    // -----------------------------------------------------------------------
    // T22 — index_for uses low bits (mask-based)
    // -----------------------------------------------------------------------

    #[test]
    fn tt_index_for_uses_low_bits() {
        let tt = TranspositionTable::new(1);
        let mask = tt.entry_count() - 1;

        // Two keys that share the same low bits but differ in high bits
        let key_a: u64 = 0x0000_0000_0000_00AB;
        let key_b: u64 = 0xFFFF_FFFF_FFFF_00AB;

        let idx_a = tt.index_for(key_a);
        let idx_b = tt.index_for(key_b);

        // Both must equal key & mask
        assert_eq!(idx_a, key_a as usize & mask);
        assert_eq!(idx_b, key_b as usize & mask);

        // And they collide (same low bits)
        assert_eq!(
            idx_a, idx_b,
            "keys with identical low bits must map to the same index"
        );
    }
}
