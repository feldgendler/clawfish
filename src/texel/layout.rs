//! Canonical flat index map for the linear-core tunable weights (ADR-0037 §3).
//!
//! The core vector is the optimizer's parameter vector: a flat `[f64; N_CORE]`
//! (length [`N_CORE`]) whose layout is fixed once here and shared by
//! [`crate::texel::params`] (vec view ⇄ struct), [`crate::texel::features`]
//! (the `(core_index, count)` sparse feature coeffs + the MG/EG blend), and the
//! feature cache header (layout hash). A silent reorder would mis-attribute
//! every tuned weight, so this map is the single source of truth and is pinned
//! by [`tests::layout_ranges_contiguous_no_overlap_cover_n_core`].
//!
//! **Groups are laid out contiguously in [`Group`] enum order.** Each entry's
//! middlegame-vs-endgame parity ([`is_mg`]) is fixed here because the fast
//! model accumulates two f64 sums (MG / EG) and divides by the phase once
//! (ADR-0037 §2); `features` reads the parity from this module, never re-derives
//! it.
//!
//! ## Per-group index ranges (N_CORE = 193)
//!
//! | Group | Range | Count | Contents (in order) |
//! |---|---|---|---|
//! | `IsoDblBwd`    | `0..6`     | 6   | ISO_MG, ISO_EG, DBL_MG, DBL_EG, BWD_MG, BWD_EG |
//! | `Conn`         | `6..18`    | 12  | CONN_MG[2..8] (6), then CONN_EG[2..8] (6) |
//! | `Passed`       | `18..37`   | 19  | PASSED_MG[2..7] (5), PASSED_EG[2..8] (6), PASSED_FREE_EG_DELTA[2..8] (6), KDIST_OWN, KDIST_ENEMY |
//! | `MobN`         | `37..55`   | 18  | KNIGHT_MOBILITY_MG[0..9] (9), then KNIGHT_MOBILITY_EG[0..9] (9) |
//! | `MobB`         | `55..83`   | 28  | BISHOP_MOBILITY_MG[0..14] (14), then BISHOP_MOBILITY_EG[0..14] (14) |
//! | `MobR`         | `83..113`  | 30  | ROOK_MOBILITY_MG[0..15] (15), then ROOK_MOBILITY_EG[0..15] (15) |
//! | `MobQ`         | `113..169` | 56  | QUEEN_MOBILITY_MG[0..28] (28), then QUEEN_MOBILITY_EG[0..28] (28) |
//! | `KsShieldFile` | `169..177` | 8   | SHIELD_1_MG, SHIELD_1_EG, SHIELD_2_MG, SHIELD_2_EG, KS_KFILE_SEMI_OPEN_MG, KS_KFILE_OPEN_MG, KS_ADJ_SEMI_OPEN_MG, KS_BOTH_ADJ_SEMI_OPEN_MG |
//! | `Outpost`      | `177..189` | 12  | OUTPOST_KNIGHT_MG[3..6] (3), OUTPOST_KNIGHT_EG[3..6] (3), OUTPOST_BISHOP_MG[3..6] (3), OUTPOST_BISHOP_EG[3..6] (3) |
//! | `RookFile`     | `189..193` | 4   | ROOK_OPEN_FILE_MG, ROOK_OPEN_FILE_EG, ROOK_SEMI_OPEN_FILE_MG, ROOK_SEMI_OPEN_FILE_EG |
//!
//! ### Exclusions / non-core (held at `shipped()`, NOT in the vector)
//! - `PASSED_MG[7]` (structurally 0 — the passer bonus is EG-dominant; ADR-0034).
//! - Outpost ranks outside `3..=5` (the predicate gates rel-rank ∈ 3..=5).
//! - King-safety S-curve table (100) + the 4 attacker multipliers (deferred —
//!   quiet corpus gives no king-attack signal, ADR-0037 §3).
//! - The 3 endgame-scale numerators (outer-swept via the reference scorer, §4).
//! - Material / 12 PSTs / bishop-pair / mop-up / `EG_SCALE_DEN` /
//!   `FIFTY_MOVE_TAPER_FROM` / `PASSED_KDIST_CAP` (the frozen reference frame).

use std::ops::Range;

/// Flat linear-core tunable count (ADR-0037 §3 "Linear-core total ~193").
pub const N_CORE: usize = 193;

/// MG-vs-EG parity of a core index (the single-division blend selector).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Parity {
    /// Middlegame term: contributes to the `mg` accumulator.
    Mg,
    /// Endgame term: contributes to the `eg` accumulator.
    Eg,
}

/// A contiguous block of core indices owned by one tunable term family. Laid
/// out in declaration order; see the module-level table for the per-group
/// ranges.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Group {
    /// Pawn isolated / doubled / backward (MG+EG): 6.
    IsoDblBwd,
    /// Connected-pawn rank table (MG+EG, ranks 2..8): 12.
    Conn,
    /// Passed-pawn rank / free-path delta / king-distance: 19.
    Passed,
    /// Knight mobility (MG+EG): 18.
    MobN,
    /// Bishop mobility (MG+EG): 28.
    MobB,
    /// Rook mobility (MG+EG): 30.
    MobR,
    /// Queen mobility (MG+EG): 56.
    MobQ,
    /// King-safety pawn-shield + open-file: 8.
    KsShieldFile,
    /// Knight / bishop outpost (MG+EG, ranks 3..6): 12.
    Outpost,
    /// Rook on open / semi-open file (MG+EG): 4.
    RookFile,
}

/// Per-group core-index count, in [`Group`] order. The cumulative sums give the
/// contiguous ranges returned by [`group_range`].
const GROUP_LEN: [usize; 10] = [6, 12, 19, 18, 28, 30, 56, 8, 12, 4];

/// Groups in flat-layout order (matches [`GROUP_LEN`]).
const GROUP_ORDER: [Group; 10] = [
    Group::IsoDblBwd,
    Group::Conn,
    Group::Passed,
    Group::MobN,
    Group::MobB,
    Group::MobR,
    Group::MobQ,
    Group::KsShieldFile,
    Group::Outpost,
    Group::RookFile,
];

fn group_ordinal(g: Group) -> usize {
    match g {
        Group::IsoDblBwd => 0,
        Group::Conn => 1,
        Group::Passed => 2,
        Group::MobN => 3,
        Group::MobB => 4,
        Group::MobR => 5,
        Group::MobQ => 6,
        Group::KsShieldFile => 7,
        Group::Outpost => 8,
        Group::RookFile => 9,
    }
}

/// The flat core-index range owned by `g`. Ranges are contiguous, non-
/// overlapping, and together cover `0..N_CORE` (pinned by the unit test).
pub fn group_range(g: Group) -> Range<usize> {
    let ord = group_ordinal(g);
    let start: usize = GROUP_LEN[..ord].iter().sum();
    start..start + GROUP_LEN[ord]
}

/// The group that owns flat core index `idx`.
///
/// # Panics
/// Debug-asserts `idx < N_CORE` — callers pass layout-valid indices.
pub fn group_of(idx: usize) -> Group {
    debug_assert!(
        idx < N_CORE,
        "core index {idx} out of range (N_CORE={N_CORE})"
    );
    let mut acc = 0usize;
    for (k, &len) in GROUP_LEN.iter().enumerate() {
        acc += len;
        if idx < acc {
            return GROUP_ORDER[k];
        }
    }
    unreachable!("idx < N_CORE guarantees a covering group")
}

/// MG-vs-EG parity of flat core index `idx` (the blend-accumulator selector).
///
/// Within each group the MG entries precede the EG entries (per the module
/// table); the king-safety and rook-file groups interleave per their named
/// `*_MG` / `*_EG` const order.
///
/// # Panics
/// Debug-asserts `idx < N_CORE`.
pub fn is_mg(idx: usize) -> bool {
    parity(idx) == Parity::Mg
}

/// [`Parity`] of flat core index `idx`. See [`is_mg`].
pub fn parity(idx: usize) -> Parity {
    debug_assert!(
        idx < N_CORE,
        "core index {idx} out of range (N_CORE={N_CORE})"
    );
    let g = group_of(idx);
    let local = idx - group_range(g).start;
    let mg = match g {
        // 3 MG/EG scalar pairs, interleaved (ISO, DBL, BWD): even = MG.
        Group::IsoDblBwd => local.is_multiple_of(2),
        // MG block (first half) then EG block (second half).
        Group::Conn | Group::MobN | Group::MobB | Group::MobR | Group::MobQ => {
            local < GROUP_LEN[group_ordinal(g)] / 2
        }
        // PASSED_MG[2..7] (5, MG) | PASSED_EG (6, EG) | FREE_EG_DELTA (6, EG)
        // | KDIST_OWN (EG) | KDIST_ENEMY (EG). Only the first 5 are MG.
        Group::Passed => local < 5,
        // SHIELD_1_MG, SHIELD_1_EG, SHIELD_2_MG, SHIELD_2_EG (interleaved
        // pairs), then 4 MG-only open-file penalties.
        Group::KsShieldFile => match local {
            0 | 2 => true,  // SHIELD_1_MG, SHIELD_2_MG
            1 | 3 => false, // SHIELD_1_EG, SHIELD_2_EG
            4..=7 => true,  // open-file penalties (MG-only)
            _ => unreachable!("KsShieldFile has 8 entries"),
        },
        // KNIGHT_MG(3), KNIGHT_EG(3), BISHOP_MG(3), BISHOP_EG(3): MG when the
        // 6-cycle offset is in the first half.
        Group::Outpost => local % 6 < 3,
        // ROOK_OPEN_MG, ROOK_OPEN_EG, ROOK_SEMI_MG, ROOK_SEMI_EG.
        Group::RookFile => local.is_multiple_of(2),
    };
    if mg { Parity::Mg } else { Parity::Eg }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_ranges_contiguous_no_overlap_cover_n_core() {
        let mut cursor = 0usize;
        for &g in &GROUP_ORDER {
            let r = group_range(g);
            assert_eq!(
                r.start, cursor,
                "group {g:?} starts at {} not {cursor}",
                r.start
            );
            assert!(r.end > r.start, "group {g:?} is empty");
            cursor = r.end;
        }
        assert_eq!(cursor, N_CORE, "groups do not cover exactly 0..N_CORE");

        // Every index maps back to the group whose range contains it.
        for idx in 0..N_CORE {
            let g = group_of(idx);
            assert!(
                group_range(g).contains(&idx),
                "idx {idx} not in group_of(idx)={g:?}"
            );
        }
    }

    #[test]
    fn group_lengths_sum_to_n_core() {
        assert_eq!(GROUP_LEN.iter().sum::<usize>(), N_CORE);
    }

    #[test]
    fn parity_total_is_documented_split() {
        // Sanity pin: the MG/EG split is fixed; a parity edit fails this.
        let mg = (0..N_CORE).filter(|&i| is_mg(i)).count();
        let eg = N_CORE - mg;
        assert_eq!(mg + eg, N_CORE);
        // MG entries: IsoDblBwd 3, Conn 6, Passed 5, MobN 9, MobB 14, MobR 15,
        // MobQ 28, KsShieldFile 6, Outpost 6, RookFile 2 = 94.
        assert_eq!(mg, 94, "MG-parity count drifted");
    }
}
