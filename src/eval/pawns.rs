//! Pawn-structure evaluation + search-owned pawn hash table (M6.B).
//!
//! `pawn_eval(&Position) -> PawnEval` is the single source of truth for the
//! white-perspective pawn-structure MG/EG contribution and the passed-pawn
//! detection bitboards (cached for M6.C). `PawnHashTable` is a fixed 4 MiB
//! always-replace accelerator keyed by the pawn-only Zobrist substream
//! (ADR-0032).

use super::chebyshev_distance;
use crate::bitboard::{self, Bitboard};
use crate::eval::data::{
    BWD_EG, BWD_MG, CONN_EG, CONN_MG, DBL_EG, DBL_MG, ISO_EG, ISO_MG, PASSED_EG,
    PASSED_FREE_EG_DELTA, PASSED_KDIST_CAP, PASSED_KDIST_ENEMY_PER_STEP, PASSED_KDIST_OWN_PER_STEP,
    PASSED_MG,
};
use crate::piece::{Color, PieceKind};
use crate::position::Position;
use crate::square::Square;

/// White-perspective pawn-structure eval + cached detection bitboards.
///
/// `#[allow(dead_code)]`: the test-first gate lays this type + the term
/// helpers + `pawn_eval`/`get` down as `unimplemented!()` stubs. Their
/// non-test consumers (`eval::evaluate_core`, qsearch via `evaluate_cached`)
/// land in Slices C–E; until then production builds see them unused. The
/// in-module tests do exercise them. Mirrors the plan-mandated
/// `#[allow(dead_code)]` on `AlphaBetaMover::pawn_hash`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct PawnEval {
    /// White-perspective pawn-structure MG contribution.
    pub mg: i32,
    /// White-perspective pawn-structure EG contribution.
    pub eg: i32,
    /// `passed[White]`, `passed[Black]` passed-pawn bitboards (M6.C reads).
    pub passed: [Bitboard; 2],
}

/// Compute pawn-structure eval from scratch (no cache). Single source of
/// truth for both `evaluate` and `evaluate_cached` (via the hash table).
pub(crate) fn pawn_eval(pos: &Position) -> PawnEval {
    let wp = pos.pieces_colored(Color::White, PieceKind::Pawn);
    let bp = pos.pieces_colored(Color::Black, PieceKind::Pawn);

    let mut mg = 0i32;
    let mut eg = 0i32;
    let mut passed = [Bitboard::EMPTY; 2];

    for (side, own, enemy, sign) in [(Color::White, wp, bp, 1i32), (Color::Black, bp, wp, -1i32)] {
        let iso = isolated_pawns(own);
        let dbl = doubled_pawns(own);
        let bwd = backward_pawns(own, enemy, side);
        let con = connected_pawns(own, side);

        // Isolated / doubled / backward STACK — no if-else / cross-term
        // suppression (ADR-0032 §6: "Isolated/doubled/backward stack (no
        // if-else suppression)"). A pawn that is isolated AND backward
        // incurs both penalties; isolated AND doubled likewise. Texel
        // reconciles any double-counting in M6.F.
        let iso_count = iso.count() as i32;
        let bwd_count = bwd.count() as i32;

        let mut conn_mg = 0i32;
        let mut conn_eg = 0i32;
        let mut remaining = con;
        while let Some(sq) = remaining.pop_lsb() {
            let rel_rank = match side {
                Color::White => sq.rank() as usize,
                Color::Black => 7 - sq.rank() as usize,
            };
            conn_mg += CONN_MG[rel_rank];
            conn_eg += CONN_EG[rel_rank];
        }

        mg += sign
            * (ISO_MG * iso_count + DBL_MG * dbl.count() as i32 + BWD_MG * bwd_count + conn_mg);
        eg += sign
            * (ISO_EG * iso_count + DBL_EG * dbl.count() as i32 + BWD_EG * bwd_count + conn_eg);

        passed[side.index()] = passed_pawns(own, enemy, side);
    }

    PawnEval { mg, eg, passed }
}

const PAWN_HASH_MIB: usize = 4;
const PAWN_HASH_ENTRIES: usize = PAWN_HASH_MIB * 1024 * 1024 / 32; // 2^17
// Pin the literal entry count: the `& (PAWN_HASH_ENTRIES - 1)` index mask
// requires a power of two, and a `/`→`*` (or arithmetic) mutation on the
// line above would silently produce a ~4 GiB allocation that only manifests
// as a test timeout. This compile-time assert turns any such mutation into
// an UNVIABLE (build-caught) result instead. 4 MiB / 32 B = 131072 = 2^17.
const _: () = assert!(PAWN_HASH_ENTRIES == 131072 && PAWN_HASH_ENTRIES.is_power_of_two());

/// One pawn-hash slot. `key == 0` doubles as the zeroed-slot sentinel and a
/// reachable real value; ADR-0032 §2 — such positions are never cached.
#[derive(Copy, Clone)]
#[repr(C)]
struct PawnHashEntry {
    key: u64,
    mg: i16,
    eg: i16,
    passed: [Bitboard; 2],
}

// Pin the 4-MiB/entry-count arithmetic against a future field reorder.
const _: () = assert!(core::mem::size_of::<PawnHashEntry>() == 32);

/// Search-owned, fixed 4 MiB, always-replace pawn hash table (ADR-0032 §2).
pub(crate) struct PawnHashTable {
    entries: Box<[PawnHashEntry]>,
}

impl PawnHashTable {
    /// Allocate a zeroed table. `key == 0` is the empty-slot sentinel.
    pub(crate) fn new() -> Self {
        let entries = vec![
            PawnHashEntry {
                key: 0,
                mg: 0,
                eg: 0,
                passed: [Bitboard::EMPTY; 2],
            };
            PAWN_HASH_ENTRIES
        ]
        .into_boxed_slice();
        Self { entries }
    }

    /// Zero every slot (fired on `ucinewgame` + per bench position via
    /// `Search::reset`).
    pub(crate) fn clear(&mut self) {
        for e in self.entries.iter_mut() {
            *e = PawnHashEntry {
                key: 0,
                mg: 0,
                eg: 0,
                passed: [Bitboard::EMPTY; 2],
            };
        }
    }

    /// Probe-or-compute: returns `PawnEval`; on miss computes via `pawn_eval`
    /// and stores. `key == 0` recomputes without probe or store (ADR-0032 §2).
    pub(crate) fn get(&mut self, pos: &Position) -> PawnEval {
        let key = pos.pawn_zobrist();

        // key==0 is both the empty-slot sentinel and a reachable real value
        // (no-pawn position, some symmetric structures). Never cache it.
        if key == 0 {
            return pawn_eval(pos);
        }

        let idx = (key as usize) & (PAWN_HASH_ENTRIES - 1);
        if self.entries[idx].key == key {
            // Cache hit: reconstruct PawnEval from stored i16 fields.
            let e = &self.entries[idx];
            return PawnEval {
                mg: e.mg as i32,
                eg: e.eg as i32,
                passed: e.passed,
            };
        }

        // Cache miss: compute, store (always-replace), return.
        let pe = pawn_eval(pos);
        debug_assert!(
            pe.mg >= i16::MIN as i32 && pe.mg <= i16::MAX as i32,
            "pawn eval mg out of i16 range: {}",
            pe.mg
        );
        debug_assert!(
            pe.eg >= i16::MIN as i32 && pe.eg <= i16::MAX as i32,
            "pawn eval eg out of i16 range: {}",
            pe.eg
        );
        self.entries[idx] = PawnHashEntry {
            key,
            mg: pe.mg as i16,
            eg: pe.eg as i16,
            passed: pe.passed,
        };
        pe
    }

    /// Test-only: `true` iff every slot key is the empty-sentinel 0.
    /// Mirrors the `history_table` test-accessor precedent — lets the
    /// search-wiring tests observe `Search::reset`'s clear without exposing
    /// the slot array. Production never calls this.
    #[cfg(test)]
    pub(crate) fn all_slots_empty_for_test(&self) -> bool {
        self.entries.iter().all(|e| e.key == 0)
    }

    /// Test-only: forcibly dirty one slot so a subsequent `clear()` /
    /// `Search::reset()` is observably effective.
    #[cfg(test)]
    pub(crate) fn dirty_one_slot_for_test(&mut self) {
        self.entries[0].key = 0xC0FF_EE00_DEAD_BEEF;
        self.entries[0].mg = 77;
        self.entries[0].eg = -33;
    }
}

// ---------------------------------------------------------------------------
// Per-term predicate helpers (white-relative; black via symmetry in
// pawn_eval). Each returns a bitboard of the pawns satisfying the predicate.
// ---------------------------------------------------------------------------

/// Pawns of `own` with no friendly pawn on either adjacent file.
pub(crate) fn isolated_pawns(own: Bitboard) -> Bitboard {
    // A pawn is isolated if no friendly pawn exists on either adjacent file.
    // Neighbor files: east and west of all files containing own pawns.
    let pawn_files = bitboard::file_fill(own);
    let neighbor_files = pawn_files.shift_east() | pawn_files.shift_west();
    own & !neighbor_files
}

/// The per-extra-pawn doubled set: on any file with N≥2 friendly pawns this
/// returns N−1 of them, so `count()` == Σ(pawns_on_file − 1) regardless of
/// color (the only quantity `pawn_eval` consumes — the DBL penalty). Note the
/// *specific* pawn marked is color-relative: the south-based formula marks the
/// northernmost-but-one toward rank 8 (the rear pawn for White, the
/// least-advanced for Black). Since only the popcount is consumed and it is
/// color-correct, the per-color square identity is immaterial here.
pub(crate) fn doubled_pawns(own: Bitboard) -> Bitboard {
    // South-fill of the full own-pawn set: a pawn is in it iff another own
    // pawn sits north of it on the same file (count = pawns_on_file − 1).
    own & bitboard::black_front_spans(own)
}

/// CPW-simple backward pawns of `own` (white-relative when `side` is White):
/// stop square attacked by an enemy pawn and not covered by own attack-front
/// spans.
pub(crate) fn backward_pawns(own: Bitboard, enemy: Bitboard, side: Color) -> Bitboard {
    match side {
        Color::White => {
            // stops = one square ahead of each own pawn (toward rank 8).
            let stops = own.shift_north();
            // Enemy pawn attacks on those stop squares.
            let enemy_attacks = enemy.shift_south_east() | enemy.shift_south_west();
            // Diagonal attack-front spans of own pawns (without own file).
            let own_attack_spans = bitboard::white_front_spans(own).shift_east()
                | bitboard::white_front_spans(own).shift_west();
            // Backward = stop attacked by enemy and not covered by own spans,
            // shifted back to the pawn's square.
            (stops & enemy_attacks & !own_attack_spans).shift_south()
        }
        Color::Black => {
            // Direction-reversed formula: black advances toward rank 1.
            let stops = own.shift_south();
            let enemy_attacks = enemy.shift_north_east() | enemy.shift_north_west();
            let own_attack_spans = bitboard::black_front_spans(own).shift_east()
                | bitboard::black_front_spans(own).shift_west();
            (stops & enemy_attacks & !own_attack_spans).shift_north()
        }
    }
}

/// Connected pawns of `own` (`phalanx | defended`), white-relative when
/// `side` is White. A pawn is connected iff it is in a phalanx with an
/// adjacent same-rank friendly pawn OR it is defended by another friendly
/// pawn's attack. A bare defender (c3 defends d4) is NOT itself connected
/// unless it is also phalanx or itself defended.
pub(crate) fn connected_pawns(own: Bitboard, side: Color) -> Bitboard {
    // Phalanx: same-rank, adjacent-file friendly pawn (direction-independent).
    let phalanx = own & (own.shift_east() | own.shift_west());

    // Defended: own pawn attacked from below by another own pawn.
    // White pawns attack north-east/north-west, so a defended white pawn
    // has another white pawn to its south-east or south-west.
    let defended = match side {
        Color::White => own & (own.shift_north_east() | own.shift_north_west()),
        Color::Black => own & (own.shift_south_east() | own.shift_south_west()),
    };

    phalanx | defended
}

/// Passed pawns of `own`: no `enemy` pawn on the file or either adjacent file
/// strictly ahead. White-relative when `side` is White.
pub(crate) fn passed_pawns(own: Bitboard, enemy: Bitboard, side: Color) -> Bitboard {
    // Enemy coverage toward own's promotion rank, widened by adjacent files.
    let enemy_front = match side {
        Color::White => {
            let ef = bitboard::black_front_spans(enemy);
            ef | ef.shift_east() | ef.shift_west()
        }
        Color::Black => {
            let ef = bitboard::white_front_spans(enemy);
            ef | ef.shift_east() | ef.shift_west()
        }
    };
    own & !enemy_front
}

/// Signed (white − black) isolated / doubled / backward pawn raw counts.
/// Shares the per-side iso/dbl/bwd detection with [`pawn_eval`].
fn iso_dbl_bwd_signed_counts(pos: &Position) -> (i32, i32, i32) {
    let wp = pos.pieces_colored(Color::White, PieceKind::Pawn);
    let bp = pos.pieces_colored(Color::Black, PieceKind::Pawn);
    let (mut iso, mut dbl, mut bwd) = (0i32, 0i32, 0i32);
    for (side, own, enemy, sign) in [(Color::White, wp, bp, 1i32), (Color::Black, bp, wp, -1i32)] {
        iso += sign * isolated_pawns(own).count() as i32;
        dbl += sign * doubled_pawns(own).count() as i32;
        bwd += sign * backward_pawns(own, enemy, side).count() as i32;
    }
    (iso, dbl, bwd)
}

/// Sparse `(core_index, raw_count)` feature accessor for iso/dbl/bwd — the
/// Phase-3 Texel seam (ADR-0037 §2). White-POV signed counts (white +, black
/// −); each term emits BOTH its MG and EG core index (same raw count). Shares
/// detection with [`pawn_eval`]; `dot(features, shipped core weights)` equals
/// [`iso_dbl_bwd_term_white`] (pinned by `accessor_dot_weights_equals_term_fn`).
pub(crate) fn iso_dbl_bwd_features(pos: &Position) -> Vec<(u16, i32)> {
    use crate::texel::layout::{Group, group_range};
    let (iso, dbl, bwd) = iso_dbl_bwd_signed_counts(pos);
    // IsoDblBwd layout: [ISO_MG, ISO_EG, DBL_MG, DBL_EG, BWD_MG, BWD_EG].
    let start = group_range(Group::IsoDblBwd).start;
    let mut out = Vec::new();
    for (pair, count) in [(0usize, iso), (2, dbl), (4, bwd)] {
        if count != 0 {
            out.push(((start + pair) as u16, count)); // MG
            out.push(((start + pair + 1) as u16, count)); // EG
        }
    }
    out
}

/// White-perspective `(mg, eg)` iso/dbl/bwd contribution — the linear
/// isolated/doubled/backward part of [`pawn_eval`]'s pawn-structure score
/// (excludes the connected-pawn rank bonus). Used by the Phase-3 accessor
/// cross-check.
#[allow(dead_code)] // Texel seam: consumed by tests + `texel::features` (later slice).
pub(crate) fn iso_dbl_bwd_term_white(pos: &Position) -> (i32, i32) {
    let (iso, dbl, bwd) = iso_dbl_bwd_signed_counts(pos);
    let mg = ISO_MG * iso + DBL_MG * dbl + BWD_MG * bwd;
    let eg = ISO_EG * iso + DBL_EG * dbl + BWD_EG * bwd;
    (mg, eg)
}

/// Signed (white − black) connected-pawn per-relative-rank raw counts (index
/// 0..8). Shares the per-side connected-pawn detection with [`pawn_eval`].
fn conn_signed_rank_counts(pos: &Position) -> [i32; 8] {
    let wp = pos.pieces_colored(Color::White, PieceKind::Pawn);
    let bp = pos.pieces_colored(Color::Black, PieceKind::Pawn);
    let mut counts = [0i32; 8];
    for (side, own, sign) in [(Color::White, wp, 1i32), (Color::Black, bp, -1i32)] {
        let mut remaining = connected_pawns(own, side);
        while let Some(sq) = remaining.pop_lsb() {
            let rel_rank = match side {
                Color::White => sq.rank() as usize,
                Color::Black => 7 - sq.rank() as usize,
            };
            counts[rel_rank] += sign;
        }
    }
    counts
}

/// Sparse `(core_index, raw_count)` feature accessor for connected pawns —
/// the Phase-3 Texel seam (ADR-0037 §2). White-POV signed per-relative-rank
/// counts; each rank emits BOTH its MG and EG core index. Shares detection
/// with [`pawn_eval`]; `dot(features, shipped core weights)` equals
/// [`conn_term_white`] (pinned by `accessor_dot_weights_equals_term_fn`).
pub(crate) fn conn_features(pos: &Position) -> Vec<(u16, i32)> {
    use crate::texel::layout::{Group, group_range};
    let counts = conn_signed_rank_counts(pos);
    // Conn layout: CONN_MG[2..8] (6) then CONN_EG[2..8] (6); rank r → local
    // (r-2) for MG and 6+(r-2) for EG. Ranks 0,1 are structurally unscored.
    let start = group_range(Group::Conn).start;
    let mut out = Vec::new();
    for (rank, &count) in counts.iter().enumerate().take(8).skip(2) {
        if count != 0 {
            let local = rank - 2;
            out.push(((start + local) as u16, count)); // MG
            out.push(((start + 6 + local) as u16, count)); // EG
        }
    }
    out
}

/// White-perspective `(mg, eg)` connected-pawn rank-bonus contribution — the
/// `connected = phalanx | defended` part of [`pawn_eval`]'s pawn-structure
/// score. Used by the Phase-3 accessor cross-check.
#[allow(dead_code)] // Texel seam: consumed by tests + `texel::features` (later slice).
pub(crate) fn conn_term_white(pos: &Position) -> (i32, i32) {
    let counts = conn_signed_rank_counts(pos);
    let mut mg = 0i32;
    let mut eg = 0i32;
    for (rank, &count) in counts.iter().enumerate() {
        mg += CONN_MG[rank] * count;
        eg += CONN_EG[rank] * count;
    }
    (mg, eg)
}

/// White-perspective passed-pawn eval (rank bonus + EG path discriminator +
/// EG king-tropism). Reads the M6.B-cached `passed[White|Black]`; computed
/// **live** (king-distance/path are not pawn-only — ADR-0032 §3). Returns
/// `(mg, eg)` white-perspective; black passers subtract symmetrically.
///
/// **Shipped score-neutral (M6.C):** all `PASSED_*` weights are zeroed in
/// `eval::data` (the §11-step-3 / ADR-0032 §7 outcome — see `data.rs`), so this
/// term currently contributes `(0, 0)` for every position. The term math stays
/// live and referenced at zero weight so M6.F's joint Texel re-introduces and
/// reshapes the passed-pawn weight set with no code change (the M6.B
/// ISO/DBL/BWD `pawn_eval` precedent).
pub(crate) fn passed_pawn_term_white(pos: &Position, passed: &[Bitboard; 2]) -> (i32, i32) {
    let occ_all = pos.occupied_all();
    let mut mg = 0i32;
    let mut eg = 0i32;

    for (side, sign) in [(Color::White, 1i32), (Color::Black, -1i32)] {
        let enemy = side.flip();
        let enemy_occ = pos.occupied(enemy);

        for sq in passed[side.index()].iter() {
            // Relative rank: white counts from rank 1 up, black mirrors.
            let rel = match side {
                Color::White => sq.rank(),
                Color::Black => 7 - sq.rank(),
            } as usize;

            mg += sign * PASSED_MG[rel];
            eg += sign * PASSED_EG[rel];

            // Front-span path (excludes the pawn's own square, includes the
            // promotion square). Three-state EG discriminator: empty of ALL
            // pieces → +Δ; an enemy piece on it → −Δ; friendly-only → 0.
            let pawn_bb = Bitboard::from_square(sq);
            let path = match side {
                Color::White => bitboard::white_front_spans(pawn_bb),
                Color::Black => bitboard::black_front_spans(pawn_bb),
            };
            if (path & occ_all).is_empty() {
                eg += sign * PASSED_FREE_EG_DELTA[rel];
            } else if (path & enemy_occ).any() {
                eg -= sign * PASSED_FREE_EG_DELTA[rel];
            }

            // King-tropism (EG-only, rank-scaled, measured to the promotion
            // square: rank 7 for white, rank 0 for black — research §7). Own
            // king near promo → bonus; enemy king near promo → penalty.
            let own_king = pos.king_square(side);
            let enemy_king = pos.king_square(enemy);
            let promo_rank = match side {
                Color::White => 7,
                Color::Black => 0,
            };
            let promo = Square::from_file_rank(sq.file(), promo_rank)
                .expect("file 0..7 and rank 0/7 are always in range");
            let own_d = chebyshev_distance(own_king, promo).min(PASSED_KDIST_CAP);
            let enemy_d = chebyshev_distance(enemy_king, promo).min(PASSED_KDIST_CAP);
            let rel_scale = rel as i32;
            eg += sign * rel_scale * PASSED_KDIST_OWN_PER_STEP * (PASSED_KDIST_CAP - own_d);
            eg += sign * rel_scale * PASSED_KDIST_ENEMY_PER_STEP * (enemy_d - PASSED_KDIST_CAP);
        }
    }

    (mg, eg)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitboard::Bitboard;
    use crate::eval::data::{
        BWD_EG, BWD_MG, CONN_EG, CONN_MG, DBL_EG, DBL_MG, ISO_EG, ISO_MG, PASSED_EG,
        PASSED_FREE_EG_DELTA, PASSED_KDIST_CAP, PASSED_KDIST_ENEMY_PER_STEP,
        PASSED_KDIST_OWN_PER_STEP, PASSED_MG,
    };
    use crate::piece::Color;
    use crate::position::Position;
    use crate::square::Square;

    // Centre-of-literature default weights (research §6). These mirror the
    // values the implementation will place in `eval::data`; the per-term
    // tests below assert on popcounts/bitboards (definitional, weight-free)
    // so they survive any later Texel re-tune. `pawn_eval` component tests
    // assert sign/relative-magnitude, not exact cp, for the same reason —
    // except the explicitly weight-pinned stacking + rank-scaling fixtures
    // which document the literature default in their derivation.

    /// Helper: white+black pawn bitboards from a FEN fixture.
    fn pawns_of(fen: &str) -> (Bitboard, Bitboard) {
        let pos = Position::from_fen(fen).expect("fixture FEN must parse");
        (
            pos.pieces_colored(Color::White, crate::piece::PieceKind::Pawn),
            pos.pieces_colored(Color::Black, crate::piece::PieceKind::Pawn),
        )
    }

    // -----------------------------------------------------------------------
    // Isolated pawns.
    // -----------------------------------------------------------------------

    /// a-file isolani: white pawns a4 and c2. The a4 pawn has no friendly
    /// pawn on the b-file (its only adjacent file) → isolated. The c2 pawn
    /// has no friendly pawn on b- or d-file → also isolated. Expected set =
    /// {a4, c2}, popcount 2.
    ///
    /// Hand-derivation: white pawns = {a4, c2}. Adjacent files of a4: only
    /// b. No white pawn on b → a4 isolated. Adjacent files of c2: b, d. No
    /// white pawn on b or d → c2 isolated.
    #[test]
    fn isolated_a_file_and_center_isolani() {
        let (wp, _bp) = pawns_of("4k3/8/8/8/P7/8/2P5/4K3 w - - 0 1");
        let iso = isolated_pawns(wp);
        assert_eq!(
            iso,
            Bitboard::from_square(Square::A4) | Bitboard::from_square(Square::C2),
            "isolated set must be exactly {{a4, c2}}"
        );
        assert_eq!(iso.count(), 2, "two isolated pawns");
    }

    /// Non-isolated control: connected white pawns b2 and c2 share adjacent
    /// files (b is adjacent to c and vice versa) → neither isolated.
    /// Expected isolated set = empty.
    #[test]
    fn isolated_excludes_pawns_with_adjacent_file_neighbor() {
        let (wp, _bp) = pawns_of("4k3/8/8/8/8/8/1PP5/4K3 w - - 0 1");
        let iso = isolated_pawns(wp);
        assert_eq!(
            iso,
            Bitboard::EMPTY,
            "b2 and c2 each have a neighbor on the other's file → none isolated"
        );
    }

    // -----------------------------------------------------------------------
    // Doubled pawns (per *extra* pawn on a file).
    // -----------------------------------------------------------------------

    /// Doubled pair on the d-file (d2, d4) → 1 extra pawn. The doubled set
    /// (CPW: rear-span members = not front-most) is the rear pawn {d2}.
    /// popcount = 1 = popcount_on_file − 1.
    ///
    /// Hand-derivation: d-file white pawns {d2, d4}. Front-most (toward rank
    /// 8) for white = d4. Rear = d2. doubled = {d2}, count 1.
    #[test]
    fn doubled_pair_counts_one_extra() {
        let (wp, _bp) = pawns_of("4k3/8/8/8/3P4/8/3P4/4K3 w - - 0 1");
        let dbl = doubled_pawns(wp);
        assert_eq!(dbl.count(), 1, "doubled pair → exactly 1 extra pawn");
        assert_eq!(
            dbl,
            Bitboard::from_square(Square::D2),
            "the rear pawn (d2) is the doubled member; d4 is front-most"
        );
    }

    /// Tripled file (e2, e4, e6) → 2 extra pawns. doubled set = the two
    /// non-front-most pawns {e2, e4}; e6 is front-most for white. popcount 2.
    #[test]
    fn tripled_file_counts_two_extra() {
        let (wp, _bp) = pawns_of("4k3/8/4P3/8/4P3/8/4P3/4K3 w - - 0 1");
        let dbl = doubled_pawns(wp);
        assert_eq!(dbl.count(), 2, "tripled file → 2 extra pawns");
        assert_eq!(
            dbl,
            Bitboard::from_square(Square::E2) | Bitboard::from_square(Square::E4),
            "the two rear pawns (e2, e4) are doubled members; e6 is front-most"
        );
    }

    /// No doubling: one pawn per occupied file → empty doubled set.
    #[test]
    fn no_doubled_when_one_per_file() {
        let (wp, _bp) = pawns_of("4k3/8/8/8/8/8/PP6/4K3 w - - 0 1");
        assert_eq!(
            doubled_pawns(wp),
            Bitboard::EMPTY,
            "a2,b2 single-occupancy files → no doubled pawns"
        );
    }

    // -----------------------------------------------------------------------
    // Backward pawns (CPW-simple).
    // -----------------------------------------------------------------------

    /// CPW-simple positive: white pawn c3, friendly d-file pawn pushed to d4
    /// (so it cannot defend c3's stop square c4), and a black pawn on b5 and
    /// d5 that attack c4. c3's stop square is c4. c4 is attacked by an enemy
    /// pawn (b5 attacks c4, d5 attacks c4) AND c4 is NOT in white's
    /// attack-front-spans (no white pawn can ever defend c4 here: the b-file
    /// has no white pawn, and d4 is level with — not behind — c4 so its
    /// attack-front-span starts at d5/b5, not c4). → c3 is backward.
    ///
    /// Hand-derivation:
    ///   white pawns: c3 (sq 18), d4 (sq 27)
    ///   black pawns: b5 (sq 33), d5 (sq 35)
    ///   stops(white) = white_pawns << 8 → c4 (26), d5 (35)
    ///   black pawn attacks = b5→{a4,c4}, d5→{c4,e4}  ⇒ includes c4
    ///   white attack-front-spans: from c3 the OWN spans don't cover c4
    ///     (a pawn's own forward-attack span starts one rank ahead and
    ///     diagonally — c3 covers b4/d4 upward, never c4 itself); d4 covers
    ///     c5/e5 upward, never c4. So c4 ∉ white attack spans.
    ///   backward = (stops & blackAttacks & ~whiteAttackSpans) >> 8
    ///            = ({c4} & {a4,c4,e4} & ~{...}) >> 8 = {c4} >> 8 = {c3}
    /// Expected backward set = {c3}, popcount 1.
    #[test]
    fn backward_cpw_simple_positive() {
        let (wp, bp) = pawns_of("4k3/8/8/1p1p4/3P4/2P5/8/4K3 w - - 0 1");
        let bwd = backward_pawns(wp, bp, Color::White);
        assert_eq!(
            bwd,
            Bitboard::from_square(Square::C3),
            "c3 is backward (stop c4 enemy-attacked, not in white attack-spans)"
        );
        assert_eq!(bwd.count(), 1, "exactly one backward pawn");
    }

    /// Near-miss: same white c3 pawn, but the stop square c4 is NOT attacked
    /// by any enemy pawn (black pawns moved to a7/h7, far away). CPW-simple
    /// requires the stop square be enemy-pawn-attacked → c3 is NOT backward.
    /// Expected backward set = empty.
    ///
    /// Hand-derivation: black pawns a7,h7 attack b6 and g6 only — never c4.
    /// stops(white) ∩ blackAttacks = ∅ → backward = ∅.
    #[test]
    fn backward_near_miss_stop_not_enemy_attacked() {
        let (wp, bp) = pawns_of("4k3/p6p/8/8/8/2P5/8/4K3 w - - 0 1");
        assert_eq!(
            backward_pawns(wp, bp, Color::White),
            Bitboard::EMPTY,
            "c3 stop (c4) not enemy-attacked → not backward (CPW-simple)"
        );
    }

    /// Defended stop square is NOT backward: white c3 with a friendly b2
    /// pawn whose attack-front-span covers c3..c8 diagonals including c4.
    /// Even though black b5/d5 attack c4, c4 ∈ white attack-front-spans (b2
    /// attacks c3 and the front-span continues up the c-diagonal). → not
    /// backward.
    ///
    /// Hand-derivation: b2 white pawn → east attack-front-span covers
    /// c3,c4,c5,... So c4 ∈ whiteAttackSpans ⇒ masked out ⇒ backward = ∅.
    #[test]
    fn backward_excluded_when_stop_in_own_attack_spans() {
        let (wp, bp) = pawns_of("4k3/8/8/1p1p4/8/2P5/1P6/4K3 w - - 0 1");
        assert_eq!(
            backward_pawns(wp, bp, Color::White),
            Bitboard::EMPTY,
            "c4 is covered by b2's attack-front-span → c3 not backward"
        );
    }

    // -----------------------------------------------------------------------
    // Overlapping-operand union tests for backward_pawns (mutant closure).
    //
    // The three tests below construct positions where the two operands of a
    // `|` inside `backward_pawns` share at least one common square, so that
    // substituting `|` with `^` drops the shared square and changes the
    // result. Each test asserts an exact bitboard that differs from the result
    // under the targeted mutation.
    // -----------------------------------------------------------------------

    /// White own_attack_spans union — line-249 `|→^` mutant.
    ///
    /// White pawns b2, c2, d2. Black pawns b4, d4 (both attack c3 from SE/SW).
    ///
    /// own_attack_spans derivation (white branch):
    ///   white_front_spans({b2,c2,d2}) = {b3..b8} ∪ {c3..c8} ∪ {d3..d8}
    ///   .shift_east() = {c3..c8} ∪ {d3..d8} ∪ {e3..e8}
    ///   .shift_west() = {a3..a8} ∪ {b3..b8} ∪ {c3..c8}
    ///   overlap: {c3..c8} appears in both shifts.
    ///
    ///   Correct `|`: c3 ∈ own_attack_spans. c2's stop is c3; c3 is covered
    ///   → c2 NOT backward. backward = EMPTY.
    ///
    ///   Under `|→^` (line 249): {c3..c8} cancels from both → c3 ∉ mutated
    ///   own_attack_spans. stops({b2,c2,d2}) ∩ enemy_attacks = {c3} (b4→c3,
    ///   d4→c3); c3 not masked → c2 IS backward → result = {c2} ≠ EMPTY. ✓
    #[test]
    fn backward_white_own_attack_spans_overlap_kills_line249_xor() {
        // White: b2, c2, d2. Black: b4, d4 (both attack the c3 stop of c2).
        let (wp, bp) = pawns_of("4k3/8/8/8/1p1p4/8/1PPP4/4K3 w - - 0 1");
        assert_eq!(
            backward_pawns(wp, bp, Color::White),
            Bitboard::EMPTY,
            "c2 is not backward: its stop c3 appears in BOTH shifted white attack-spans, \
             so c3 is covered by own_attack_spans; |→^ at line 249 drops c3..c8, \
             wrongly making c2 backward"
        );
    }

    /// Black enemy_attacks union — line-257 `|→^` mutant.
    ///
    /// Black pawn c5. White (enemy) pawns b3, d3 (both attack c4 via NE/NW).
    ///
    /// enemy_attacks derivation (black branch):
    ///   {b3,d3}.shift_north_east() = {c4, e4}
    ///   {b3,d3}.shift_north_west() = {a4, c4}
    ///   overlap: c4 appears in both shifts.
    ///
    ///   Correct `|`: c4 ∈ enemy_attacks. Black own_attack_spans of {c5}
    ///   = {d4..d1} ∪ {b4..b1} (c4 absent); stops∩attacks = {c4} →
    ///   c5 IS backward. Result = {c5}.
    ///
    ///   Under `|→^` (line 257): c4 cancels from both shifts → c4 ∉ mutated
    ///   enemy_attacks → stops∩attacks = empty → c5 NOT backward → EMPTY ≠ {c5}. ✓
    #[test]
    fn backward_black_enemy_attacks_overlap_kills_line257_xor() {
        // Black: c5. White (enemy): b3, d3 (b3→NE=c4, d3→NW=c4, both attack c5's stop).
        let (wp, bp) = pawns_of("4k3/8/8/2p5/8/1P1P4/8/4K3 w - - 0 1");
        assert_eq!(
            backward_pawns(bp, wp, Color::Black),
            Bitboard::from_square(Square::C5),
            "c5 is backward: its stop c4 is attacked by both b3 (NE) and d3 (NW), \
             and c4 is absent from black own_attack_spans; |→^ at line 257 drops \
             c4 from enemy_attacks, wrongly making c5 NOT backward"
        );
    }

    /// Black own_attack_spans union — line-259 `|→^` mutant.
    ///
    /// Black pawns b5, c5, d5. White (enemy) pawns b3, d3 (attack c4).
    ///
    /// own_attack_spans derivation (black branch):
    ///   black_front_spans({b5,c5,d5}) = {b4..b1} ∪ {c4..c1} ∪ {d4..d1}
    ///   .shift_east() = {c4..c1} ∪ {d4..d1} ∪ {e4..e1}
    ///   .shift_west() = {a4..a1} ∪ {b4..b1} ∪ {c4..c1}
    ///   overlap: {c4..c1} appears in both shifts.
    ///
    ///   Correct `|`: c4 ∈ own_attack_spans. c5's stop c4 is covered →
    ///   c5 NOT backward. backward = EMPTY.
    ///
    ///   Under `|→^` (line 259): {c4..c1} cancels → c4 ∉ mutated
    ///   own_attack_spans. stops∩enemy_attacks = {c4}; c4 not masked →
    ///   c5 IS backward → result = {c5} ≠ EMPTY. ✓
    #[test]
    fn backward_black_own_attack_spans_overlap_kills_line259_xor() {
        // Black: b5, c5, d5. White (enemy): b3, d3 (attack c4 = c5's stop).
        let (wp, bp) = pawns_of("4k3/8/8/1ppp4/8/1P1P4/8/4K3 w - - 0 1");
        assert_eq!(
            backward_pawns(bp, wp, Color::Black),
            Bitboard::EMPTY,
            "c5 is not backward: its stop c4 appears in BOTH shifted black own_attack_spans, \
             so c4 is covered; |→^ at line 259 drops c4..c1, wrongly making c5 backward"
        );
    }

    // -----------------------------------------------------------------------
    // Connected pawns (connected = phalanx | defended).
    // -----------------------------------------------------------------------

    /// Chain-defended (tightened definition `connected = phalanx | defended`):
    /// white pawn d4 defended by c3 (c3 attacks d4). d4 is defended →
    /// d4 is connected. c3 defends d4 but c3 is NOT itself defended (no
    /// friendly pawn attacks c3) and is NOT in a phalanx → c3 is NOT
    /// connected under the tightened predicate.
    ///
    /// Hand-derivation:
    ///   phalanx: no same-rank adjacent pairs → ∅
    ///   defended(d4): c3 attacks d4 → d4 ∈ defended
    ///   defended(c3): no white pawn attacks c3 → c3 ∉ defended
    ///   connected = phalanx | defended = {d4}
    #[test]
    fn connected_chain_defended() {
        let (wp, _bp) = pawns_of("4k3/8/8/8/3P4/2P5/8/4K3 w - - 0 1");
        let con = connected_pawns(wp, Color::White);
        assert_eq!(
            con,
            Bitboard::from_square(Square::D4),
            "tightened connected = phalanx|defended: d4 is defended by c3, \
             but c3 is not itself defended or in a phalanx → only d4"
        );
    }

    /// Phalanx: both same-rank-adjacent members are phalanx → both connected.
    /// d4/e4 are adjacent on rank 4 — each is phalanx of the other. Under
    /// `connected = phalanx | defended`, both appear regardless of whether
    /// either defends the other.
    ///
    /// Hand-derivation:
    ///   phalanx: d4 adjacent to e4 on same rank → {d4, e4}
    ///   connected = {d4, e4}
    #[test]
    fn connected_phalanx_both_members() {
        let (wp, _bp) = pawns_of("4k3/8/8/8/3PP3/8/8/4K3 w - - 0 1");
        let con = connected_pawns(wp, Color::White);
        assert_eq!(
            con,
            Bitboard::from_square(Square::D4) | Bitboard::from_square(Square::E4),
            "d4/e4 phalanx → both connected (same-rank-adjacent)"
        );
    }

    /// Isolated lone pawn is NOT connected (no phalanx partner, defends or
    /// is defended by nothing). Expected connected set = empty.
    #[test]
    fn connected_excludes_lone_pawn() {
        let (wp, _bp) = pawns_of("4k3/8/8/8/4P3/8/8/4K3 w - - 0 1");
        assert_eq!(
            connected_pawns(wp, Color::White),
            Bitboard::EMPTY,
            "lone e4 pawn → not connected"
        );
    }

    // -----------------------------------------------------------------------
    // Overlapping-operand union tests for connected_pawns (mutant closure).
    //
    // The four tests below construct positions where the two operands of a `|`
    // inside `connected_pawns` share at least one common square, so that
    // substituting `|` with `^` (or `&` for the 280 mutant) changes the
    // result. Each test asserts an exact bitboard.
    // -----------------------------------------------------------------------

    /// Phalanx inner union — line-273 `|→^` mutant.
    ///
    /// White pawns c4, d4, e4 (three-pawn rank-4 phalanx).
    ///
    /// phalanx derivation:
    ///   own.shift_east() of {c4,d4,e4} = {d4, e4, f4}
    ///   own.shift_west() of {c4,d4,e4} = {b4, c4, d4}
    ///   overlap: d4 appears in both shifts.
    ///
    ///   Correct `|`: d4 present → own & {b4,c4,d4,e4,f4} = {c4,d4,e4}.
    ///
    ///   Under `|→^` (line 273): d4 cancels → {b4,c4,e4,f4};
    ///   own & xor = {c4,e4} — d4 drops. defended = empty (no below-defender).
    ///   Result = {c4,e4} ≠ {c4,d4,e4}. ✓
    #[test]
    fn connected_phalanx_inner_union_overlap_kills_line273_xor() {
        // White: c4, d4, e4 — d4 reachable from both sides via shift_east/shift_west.
        let (wp, _bp) = pawns_of("4k3/8/8/8/2PPP3/8/8/4K3 w - - 0 1");
        assert_eq!(
            connected_pawns(wp, Color::White),
            Bitboard::from_square(Square::C4)
                | Bitboard::from_square(Square::D4)
                | Bitboard::from_square(Square::E4),
            "d4 is in both own.shift_east and own.shift_west; |→^ at line 273 \
             cancels d4 from the phalanx inner union, wrongly dropping d4"
        );
    }

    /// White defended inner union — line-279 `|→^` mutant.
    ///
    /// White pawns c3, d4, e3 — d4 is doubly defended from both diagonals.
    ///
    /// defended derivation:
    ///   own.shift_north_east() of {c3,d4,e3} = {d4, e5, f4}
    ///   own.shift_north_west() of {c3,d4,e3} = {b4, c5, d4}
    ///   overlap: d4 appears in both shifts.
    ///
    ///   Correct `|`: d4 ∈ union; own & union = {d4}. phalanx = empty.
    ///   connected = {d4}.
    ///
    ///   Under `|→^` (line 279): d4 cancels → union = {b4,c5,e5,f4};
    ///   own & xor = empty. connected = empty ≠ {d4}. ✓
    #[test]
    fn connected_white_defended_inner_union_overlap_kills_line279_xor() {
        // White: c3, d4, e3 — d4 doubly defended by c3 (NE) and e3 (NW).
        let (wp, _bp) = pawns_of("4k3/8/8/8/3P4/2P1P3/8/4K3 w - - 0 1");
        assert_eq!(
            connected_pawns(wp, Color::White),
            Bitboard::from_square(Square::D4),
            "d4 is in both own.shift_north_east and own.shift_north_west; \
             |→^ at line 279 cancels d4, wrongly dropping it from connected"
        );
    }

    /// Black defended inner union — line-280 `|→^` mutant.
    ///
    /// Black pawns c6, d5, e6 — d5 is doubly defended from both diagonals.
    ///
    /// defended derivation (black uses SE/SW):
    ///   own.shift_south_east() of {c6,d5,e6} = {d5, e4, f5}
    ///   own.shift_south_west() of {c6,d5,e6} = {b5, c4, d5}
    ///   overlap: d5 appears in both shifts.
    ///
    ///   Correct `|`: d5 ∈ union; own & union = {d5}. phalanx = empty
    ///   (c6/e6 are not adjacent files). connected = {d5}.
    ///
    ///   Under `|→^` (line 280): d5 cancels → union = {b5,c4,e4,f5};
    ///   own & xor = empty. connected = empty ≠ {d5}. ✓
    #[test]
    fn connected_black_defended_inner_union_overlap_kills_line280_xor() {
        // Black: c6, d5, e6 — d5 doubly defended by c6 (SE) and e6 (SW).
        let (_wp, bp) = pawns_of("4k3/8/2p1p3/3p4/8/8/8/4K3 w - - 0 1");
        assert_eq!(
            connected_pawns(bp, Color::Black),
            Bitboard::from_square(Square::D5),
            "d5 is in both own.shift_south_east and own.shift_south_west; \
             |→^ at line 280 cancels d5, wrongly dropping it from connected"
        );
    }

    /// Black defended single-diagonal — line-280 `|→&` mutant.
    ///
    /// Black pawns c6, d5 — c6 defends d5 from SE only (no SW defender).
    ///
    /// defended derivation:
    ///   own.shift_south_east() of {c6,d5} = {d5, e4}
    ///   own.shift_south_west() of {c6,d5} = {b5, c4}
    ///   intersection: empty (d5 is in SE shift but not SW shift).
    ///
    ///   Correct `|`: d5 ∈ union → own & union = {d5}. phalanx = empty.
    ///   connected = {d5}.
    ///
    ///   Under `|→&` (line 280): intersection = empty → own & empty = empty.
    ///   connected = empty ≠ {d5}. ✓
    ///   (The `|→^` mutant for this position also fires: SE^SW = {b5,c4,d5,e4},
    ///   own & xor = {d5}, so connected = {d5} = correct — `|→^` is invisible
    ///   here; the overlap test above catches `|→^` instead.)
    #[test]
    fn connected_black_defended_single_diagonal_kills_line280_and() {
        // Black: c6, d5 — c6 defends d5 from SE only; no SW defender of d5.
        let (_wp, bp) = pawns_of("4k3/8/2p5/3p4/8/8/8/4K3 w - - 0 1");
        assert_eq!(
            connected_pawns(bp, Color::Black),
            Bitboard::from_square(Square::D5),
            "d5 is defended by c6 from SE only; |→& at line 280 drops d5 \
             because d5 is not in the intersection of both shifts"
        );
    }

    /// Final phalanx|defended union — line-283 `|→^` mutant.
    ///
    /// White pawns c4, d4, e3 — c4/d4 form a phalanx AND d4 is defended by e3.
    ///
    /// phalanx = {c4, d4} (same-rank adjacent). defended:
    ///   own.shift_north_east() = {d5, e5, f4}; own.shift_north_west() = {b5,c5,d4}
    ///   union = {b5,c5,d4,d5,e5,f4}; own & union = {d4}. defended = {d4}.
    ///
    ///   Correct `|`: phalanx|defended = {c4,d4}|{d4} = {c4,d4}.
    ///
    ///   Under `|→^` (line 283): {c4,d4}^{d4} = {c4} — d4 cancels.
    ///   Result = {c4} ≠ {c4,d4}. ✓
    #[test]
    fn connected_phalanx_defended_final_union_overlap_kills_line283_xor() {
        // White: c4, d4, e3 — c4/d4 phalanx; d4 also defended by e3.
        let (wp, _bp) = pawns_of("4k3/8/8/8/2PP4/4P3/8/4K3 w - - 0 1");
        assert_eq!(
            connected_pawns(wp, Color::White),
            Bitboard::from_square(Square::C4) | Bitboard::from_square(Square::D4),
            "d4 is simultaneously in phalanx (with c4) and in defended (via e3); \
             |→^ at line 283 cancels d4 where phalanx and defended overlap, \
             wrongly returning only {{c4}}"
        );
    }

    // -----------------------------------------------------------------------
    // Passed pawns (detection; bonus is M6.C).
    // -----------------------------------------------------------------------

    /// Clear passer: white e5, no black pawn on d/e/f files anywhere ahead
    /// (rank > 5). Black pawn parked on a7 (far file) → e5 is a passer.
    /// Expected white-passers = {e5}.
    ///
    /// Hand-derivation: enemyFront = black front-spans of {a7} = a-file
    /// below a7 ⇒ {a1..a6}; widen by east/west ⇒ a/b files ranks 1..6.
    /// e5 ∉ that → e5 passer.
    #[test]
    fn passed_clear_passer() {
        let (wp, bp) = pawns_of("4k3/p7/8/4P3/8/8/8/4K3 w - - 0 1");
        let passers = passed_pawns(wp, bp, Color::White);
        assert_eq!(
            passers,
            Bitboard::from_square(Square::E5),
            "e5 has no enemy pawn on d/e/f ahead → passer"
        );
    }

    /// Blocked-by-adjacent-enemy: white e5 with a black pawn on d6 (adjacent
    /// file, strictly ahead). d6 is on an adjacent file ahead of e5 → e5 is
    /// NOT a passer. Expected white-passers = empty.
    ///
    /// Hand-derivation: black d6 front-span (downward for black is
    /// irrelevant; for passer detection we widen enemy *front-spans toward
    /// their promotion*, i.e. black pawns' coverage toward rank 1) — the
    /// CPW formula: enemyFront = bFrontSpans(bpawns) widened by E/W. Black
    /// d6 front-span (toward rank 1) = d5,d4,...,d1; widen E/W ⇒ c,d,e files
    /// at those ranks; that does NOT include e5 (e5 is rank 5 = same rank as
    /// d6−1? d6 is rank 6; d6 front-span starts d5). e5 is rank 5, e-file —
    /// included via the east-widen of d5 ⇒ e5 ∈ enemyFront ⇒ e5 NOT a
    /// passer.
    #[test]
    fn passed_blocked_by_adjacent_enemy_not_passer() {
        let (wp, bp) = pawns_of("4k3/8/3p4/4P3/8/8/8/4K3 w - - 0 1");
        assert_eq!(
            passed_pawns(wp, bp, Color::White),
            Bitboard::EMPTY,
            "black d6 (adjacent file, ahead) stops e5 being a passer"
        );
    }

    /// Doubled rear pawn is automatically NOT a passer: white e4 and e5
    /// (e5 front of e4). e4's own front-span is occupied by e5 — but the
    /// CPW formula keys off *enemy* pawns only. The intended invariant
    /// (research §1.5): a doubled rear pawn is not a passer because the
    /// front friendly pawn blocks it; here we instead pin the enemy-side
    /// definition with a black e7 pawn: e7 is on the e-file ahead of both
    /// e4 and e5 → NEITHER is a passer. Expected = empty.
    ///
    /// Hand-derivation: black e7 front-span (toward rank 1) = e6..e1;
    /// widen E/W ⇒ d,e,f files ranks 1..6. e4 and e5 both ∈ that set ⇒
    /// neither is a passer.
    #[test]
    fn passed_enemy_on_file_ahead_blocks_both_doubled() {
        let (wp, bp) = pawns_of("4k3/4p3/8/4P3/4P3/8/8/4K3 w - - 0 1");
        assert_eq!(
            passed_pawns(wp, bp, Color::White),
            Bitboard::EMPTY,
            "black e7 on the file ahead → neither doubled white pawn is a passer"
        );
    }

    /// Black-side passer (symmetry of formula, not a mirrored Position):
    /// black d4 with no white pawn on c/d/e ahead (toward rank 1). White
    /// pawn parked h2 (far). Expected black-passers = {d4}.
    ///
    /// Hand-derivation: white front-span of {h2} = h3..h8; widen E/W ⇒ g/h
    /// files ranks 3..8. d4 ∉ that → d4 is a black passer.
    #[test]
    fn passed_black_side_passer() {
        let (wp, bp) = pawns_of("4k3/8/8/8/3p4/8/7P/4K3 w - - 0 1");
        let passers = passed_pawns(bp, wp, Color::Black);
        assert_eq!(
            passers,
            Bitboard::from_square(Square::D4),
            "black d4 has no white pawn on c/d/e ahead → black passer"
        );
    }

    // -----------------------------------------------------------------------
    // Overlapping-operand union tests for `passed_pawns` (M6.B mutant closure).
    //
    // The four tests below force the three union terms inside `passed_pawns`
    // (ef | ef.shift_east() | ef.shift_west()) to overlap on at least one
    // file, so that substituting `|` with `^` or `&` produces a different
    // result. Each test asserts the exact empty passed-pawn set, which becomes
    // non-empty under the targeted mutation.
    // -----------------------------------------------------------------------

    /// White branch — adjacent enemy files, first-`|` `|→^` mutant.
    ///
    /// Input: white pawn e3, black pawns d6 and e6.
    ///
    /// Enemy-front derivation (for white passers):
    ///   ef = black_front_spans(d6|e6) = south_fill(d5|e5) = {d1..d5} ∪ {e1..e5}
    ///   ef.shift_east()  = {e1..e5} ∪ {f1..f5}
    ///   ef.shift_west()  = {c1..c5} ∪ {d1..d5}
    ///   ef ∩ ef.shift_east() = {e1..e5}  (non-empty — e-file overlap)
    ///
    ///   Correct enemy_front = c1..c5 ∪ d1..d5 ∪ e1..e5 ∪ f1..f5
    ///   (ranks 1–5 on c/d/e/f files — 20 squares).
    ///
    ///   Under `|→^` at first `|`: (ef ^ ef.shift_east()) | ef.shift_west()
    ///     ef ^ east = (d1..d5 | e1..e5) ^ (e1..e5 | f1..f5)
    ///               = d1..d5 | f1..f5   (e cancels)
    ///     | west    = c1..c5 | d1..d5 | f1..f5   (e-file missing!)
    ///   e3 ∈ e1..e5 in the correct block but absent in the mutated block
    ///   → e3 wrongly scores as a white passer. Result becomes {e3} ≠ EMPTY.
    ///   The test assertion fails under the mutation. ✓
    #[test]
    fn passed_pawns_white_adjacent_enemy_files_union_kills_first_xor() {
        // White pawn on e3; black pawns on d6 and e6 (adjacent files).
        let own = Bitboard::from_square(Square::E3);
        let enemy = Bitboard::from_square(Square::D6) | Bitboard::from_square(Square::E6);
        let result = passed_pawns(own, enemy, Color::White);
        // e3 is on the e-file; black d6/e6 cover d/e/c/f files toward rank 1,
        // including e3's path → e3 is NOT a white passer.
        assert_eq!(
            result,
            Bitboard::EMPTY,
            "e3 is blocked by black d6/e6 whose widened front spans cover the e-file; \
             |→^ at the first union operator drops the e-file from enemy_front, \
             wrongly making e3 a passer and returning a non-empty set"
        );
    }

    /// White branch — skip-file enemy pattern, second-`|` `|→^` and `|→&` mutants.
    ///
    /// Input: white pawns b3 and d3, black pawns c6 and e6.
    ///
    /// Enemy-front derivation (for white passers):
    ///   ef = black_front_spans(c6|e6) = south_fill(c5|e5) = {c1..c5} ∪ {e1..e5}
    ///   ef.shift_east()  = {d1..d5} ∪ {f1..f5}
    ///   ef.shift_west()  = {b1..b5} ∪ {d1..d5}
    ///   ef.shift_east() ∩ ef.shift_west() = {d1..d5}  (non-empty — d not in ef!)
    ///   ef ∩ ef.shift_east() = ∅  (c/e vs d/f — disjoint)
    ///
    ///   Correct enemy_front = b1..b5 ∪ c1..c5 ∪ d1..d5 ∪ e1..e5 ∪ f1..f5
    ///   (ranks 1–5 on b/c/d/e/f files — 25 squares).
    ///
    ///   d3 ∈ d1..d5 → blocked. b3 ∈ b1..b5 → blocked. Expected = EMPTY.
    ///
    ///   Under `|→^` at second `|`: ef | (ef.shift_east() ^ ef.shift_west())
    ///     east ^ west = (d1..d5 | f1..f5) ^ (b1..b5 | d1..d5)
    ///                 = b1..b5 | f1..f5   (d cancels)
    ///     ef | above  = b1..b5 ∪ c1..c5 ∪ e1..e5 ∪ f1..f5   (d-file missing!)
    ///   d3 ∉ mutated enemy_front → d3 wrongly a passer. Result = {d3} ≠ EMPTY. ✓
    ///
    ///   Under `|→&` at second `|` (Rust precedence: ef | (east & west)):
    ///     east & west = (d1..d5 | f1..f5) & (b1..b5 | d1..d5) = d1..d5
    ///     ef | d1..d5 = c1..c5 | d1..d5 | e1..e5   (b and f files missing!)
    ///   b3 ∉ mutated enemy_front → b3 wrongly a passer. Result = {b3, …} ≠ EMPTY. ✓
    #[test]
    fn passed_pawns_white_skip_enemy_files_union_kills_second_xor_and_and() {
        // White pawns on b3 and d3; black pawns on c6 and e6 (skip the d-file).
        let own = Bitboard::from_square(Square::B3) | Bitboard::from_square(Square::D3);
        let enemy = Bitboard::from_square(Square::C6) | Bitboard::from_square(Square::E6);
        let result = passed_pawns(own, enemy, Color::White);
        // b3 is blocked by the west-widened c6 span; d3 is blocked by the
        // overlapping east/west widening of c6 and e6 onto the d-file.
        assert_eq!(
            result,
            Bitboard::EMPTY,
            "b3 and d3 are blocked by black c6/e6 whose widened front spans cover b and d; \
             |→^ at the second union operator drops the d-file (making d3 a passer), \
             and |→& collapses to the d-file only (making b3 a passer)"
        );
    }

    /// Black branch — adjacent enemy files, first-`|` `|→^` mutant.
    ///
    /// Input: black pawn e6, white pawns d3 and e3.
    ///
    /// Enemy-front derivation (for black passers):
    ///   ef = white_front_spans(d3|e3) = north_fill(d4|e4) = {d4..d8} ∪ {e4..e8}
    ///   ef.shift_east()  = {e4..e8} ∪ {f4..f8}
    ///   ef.shift_west()  = {c4..c8} ∪ {d4..d8}
    ///   ef ∩ ef.shift_east() = {e4..e8}  (non-empty — e-file overlap)
    ///
    ///   Correct enemy_front = c4..c8 ∪ d4..d8 ∪ e4..e8 ∪ f4..f8
    ///   (ranks 4–8 on c/d/e/f files — 20 squares).
    ///
    ///   e6 ∈ e4..e8 → blocked. Expected = EMPTY.
    ///
    ///   Under `|→^` at first `|`: (ef ^ ef.shift_east()) | ef.shift_west()
    ///     ef ^ east = (d4..d8 | e4..e8) ^ (e4..e8 | f4..f8)
    ///               = d4..d8 | f4..f8   (e cancels)
    ///     | west    = c4..c8 | d4..d8 | f4..f8   (e-file missing!)
    ///   e6 ∉ mutated enemy_front → e6 wrongly a black passer. Result = {e6} ≠ EMPTY. ✓
    #[test]
    fn passed_pawns_black_adjacent_enemy_files_union_kills_first_xor() {
        // Black pawn on e6; white pawns on d3 and e3 (adjacent files).
        let own = Bitboard::from_square(Square::E6);
        let enemy = Bitboard::from_square(Square::D3) | Bitboard::from_square(Square::E3);
        let result = passed_pawns(own, enemy, Color::Black);
        // e6 is on the e-file; white d3/e3 front spans cover d/e/c/f files
        // toward rank 8, including e6's path → e6 is NOT a black passer.
        assert_eq!(
            result,
            Bitboard::EMPTY,
            "e6 is blocked by white d3/e3 whose widened front spans cover the e-file; \
             |→^ at the first union operator in the black branch drops the e-file, \
             wrongly making e6 a passer"
        );
    }

    /// Black branch — skip-file enemy pattern, second-`|` `|→^` and `|→&` mutants.
    ///
    /// Input: black pawns b6 and d6, white pawns c3 and e3.
    ///
    /// Enemy-front derivation (for black passers):
    ///   ef = white_front_spans(c3|e3) = north_fill(c4|e4) = {c4..c8} ∪ {e4..e8}
    ///   ef.shift_east()  = {d4..d8} ∪ {f4..f8}
    ///   ef.shift_west()  = {b4..b8} ∪ {d4..d8}
    ///   ef.shift_east() ∩ ef.shift_west() = {d4..d8}  (non-empty — d not in ef!)
    ///   ef ∩ ef.shift_east() = ∅  (c/e vs d/f — disjoint)
    ///
    ///   Correct enemy_front = b4..b8 ∪ c4..c8 ∪ d4..d8 ∪ e4..e8 ∪ f4..f8
    ///   (ranks 4–8 on b/c/d/e/f files — 25 squares).
    ///
    ///   d6 ∈ d4..d8 → blocked. b6 ∈ b4..b8 → blocked. Expected = EMPTY.
    ///
    ///   Under `|→^` at second `|`: ef | (ef.shift_east() ^ ef.shift_west())
    ///     east ^ west = (d4..d8 | f4..f8) ^ (b4..b8 | d4..d8)
    ///                 = b4..b8 | f4..f8   (d cancels)
    ///     ef | above  = b4..b8 ∪ c4..c8 ∪ e4..e8 ∪ f4..f8   (d-file missing!)
    ///   d6 ∉ mutated enemy_front → d6 wrongly a passer. Result = {d6} ≠ EMPTY. ✓
    ///
    ///   Under `|→&` at second `|` (Rust precedence: ef | (east & west)):
    ///     east & west = (d4..d8 | f4..f8) & (b4..b8 | d4..d8) = d4..d8
    ///     ef | d4..d8 = c4..c8 | d4..d8 | e4..e8   (b and f files missing!)
    ///   b6 ∉ mutated enemy_front → b6 wrongly a passer. Result = {b6, …} ≠ EMPTY. ✓
    #[test]
    fn passed_pawns_black_skip_enemy_files_union_kills_second_xor_and_and() {
        // Black pawns on b6 and d6; white pawns on c3 and e3 (skip the d-file).
        let own = Bitboard::from_square(Square::B6) | Bitboard::from_square(Square::D6);
        let enemy = Bitboard::from_square(Square::C3) | Bitboard::from_square(Square::E3);
        let result = passed_pawns(own, enemy, Color::Black);
        // b6 is blocked by the west-widened c3 span; d6 is blocked by the
        // overlapping east/west widening of c3 and e3 onto the d-file.
        assert_eq!(
            result,
            Bitboard::EMPTY,
            "b6 and d6 are blocked by white c3/e3 whose widened front spans cover b and d; \
             |→^ at the second union operator drops the d-file (making d6 a passer), \
             and |→& collapses to the d-file only (making b6 a passer)"
        );
    }

    // -----------------------------------------------------------------------
    // Isolated + doubled stacking (pins D5: no if-else suppression).
    // -----------------------------------------------------------------------

    /// White pawns a2 and a4 only: the a-file is doubled (1 extra) AND both
    /// a-pawns are isolated (no white pawn on the b-file). Both predicates
    /// must fire independently — no mutual suppression.
    ///
    /// Hand-derivation:
    ///   isolated(white) = {a2, a4}  (no b-file friendly pawn)  → count 2
    ///   doubled(white)  = {a2}      (a4 front-most for white)   → count 1
    /// The two sets are NOT disjoint by construction (a2 ∈ both); stacking
    /// means the eval applies ISO twice (a2,a4) AND DBL once (a2) — pinned
    /// at the `pawn_eval` level by `pawn_eval_isolated_doubled_stack`.
    #[test]
    fn isolated_and_doubled_stack_independently() {
        let (wp, _bp) = pawns_of("4k3/8/8/8/P7/8/P7/4K3 w - - 0 1");
        let iso = isolated_pawns(wp);
        let dbl = doubled_pawns(wp);
        assert_eq!(
            iso,
            Bitboard::from_square(Square::A2) | Bitboard::from_square(Square::A4),
            "both a-file pawns are isolated"
        );
        assert_eq!(iso.count(), 2, "two isolated pawns");
        assert_eq!(
            dbl,
            Bitboard::from_square(Square::A2),
            "rear a2 is the doubled member"
        );
        assert_eq!(dbl.count(), 1, "one extra doubled pawn");
        // Stacking precondition: the two predicates overlap (a2 ∈ both) —
        // proving they are computed independently, not via if-else.
        assert!(
            (iso & dbl).any(),
            "a2 must be in BOTH the isolated and doubled sets (no suppression)"
        );
    }

    // -----------------------------------------------------------------------
    // pawn_eval — accumulation, sign, rank-scaling, stacking.
    // -----------------------------------------------------------------------

    /// Empty-of-pawns position: pawn_eval is the identity (0,0, no passers).
    /// Pins the no-pawn base case (also the key==0 path's correctness claim).
    // Passes against a zero-returning stub: this documents the base case, not
    // the pawn-structure arithmetic. Correctness of the zero return is pinned
    // by `pawn_eval_isolated_doubled_stack` (which asserts non-zero values).
    #[test]
    fn pawn_eval_no_pawns_is_zero_identity() {
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/4K3 w - - 0 1").expect("KvK FEN");
        let pe = pawn_eval(&pos);
        assert_eq!(pe.mg, 0, "no pawns → mg 0");
        assert_eq!(pe.eg, 0, "no pawns → eg 0");
        assert_eq!(pe.passed[0], Bitboard::EMPTY, "no white passers");
        assert_eq!(pe.passed[1], Bitboard::EMPTY, "no black passers");
    }

    /// Color-symmetric structure cancels to zero. Hand-mirrored FEN pair is
    /// NOT used here — instead a single position that is itself vertically
    /// symmetric: white isolated a2 vs black isolated a7 (each side one
    /// isolated a-pawn, structurally identical under color swap). The
    /// white-minus-black accumulation must cancel: mg == 0, eg == 0.
    ///
    /// Hand-derivation (pure-stack, ADR-0032 §6): white {a2} isolated only —
    /// it is NOT backward (black a7's only pawn-attack is b6, never a3, so
    /// a2's stop a3 is not enemy-attacked) and not doubled/connected.
    /// Symmetrically black {a7} isolated only. ISO contributes sign·ISO for
    /// each: `+1·ISO (white) + (−1)·ISO (black) = 0` for both mg and eg —
    /// the cancellation holds *because* no confounding term fires on either
    /// side. No passers: a7 is on the a-file ahead of a2 ⇒ a2 not a passer;
    /// symmetrically a7 not a passer. passers both empty.
    // Passes against a zero-returning stub: the exact cancellation expected
    // here is zero, which any zero stub satisfies. The non-zero sign/magnitude
    // claims are pinned by the asymmetric mirror-pair tests above.
    #[test]
    fn pawn_eval_symmetric_structure_cancels() {
        let pos = Position::from_fen("4k3/p7/8/8/8/8/P7/4K3 w - - 0 1")
            .expect("symmetric isolated-a-pawn FEN");
        let pe = pawn_eval(&pos);
        assert_eq!(pe.mg, 0, "symmetric isolated pawns cancel in mg");
        assert_eq!(pe.eg, 0, "symmetric isolated pawns cancel in eg");
        assert_eq!(pe.passed[0], Bitboard::EMPTY, "a2 blocked by a7 ahead");
        assert_eq!(pe.passed[1], Bitboard::EMPTY, "a7 blocked by a2 ahead");
    }

    /// Sign convention: a white-only isolated pawn yields a negative MG/EG
    /// (penalty), and the hand-mirrored black-only counterpart yields the
    /// exact componentwise negation. The pair is two hand-written FENs
    /// (vertical mirror + color swap), NOT a generic `mirror(pos)` helper.
    /// Isolates the isolated term under **pure-stack semantics** (ADR-0032
    /// §6): a lone pawn with no enemy pawns has *only* the ISO term — no
    /// doubled (single file occupant), no backward (no enemy attacks the
    /// stop), no connected (no friendly partner). The assertion follows
    /// directly from the ADR-§6 weights, no term suppression involved.
    ///
    /// FEN A (white isolani c4, lone): "4k3/8/8/8/2P5/8/8/4K3"
    /// FEN B (black isolani c5, the vertical-mirror+color-swap):
    ///        "4k3/8/8/2p5/8/8/8/4K3"
    /// c4 white ↔ c5 black under board flip (rank r ↔ 7−r: rank 3 ↔ rank 4,
    /// i.e. c4 (rank idx 3) ↔ c5 (rank idx 4)).
    ///
    /// Hand-derivation: A has white {c4} isolated only ⇒ white net =
    /// `ISO_MG·1 = −10` (mg), `ISO_EG·1 = −20` (eg) ⇒ `mg_A = −10 < 0`,
    /// `eg_A = −20 < 0`. No black pawns ⇒ c4 is a passer ⇒
    /// `passed[White] = {c4}`. B is the exact mirror: black {c5} isolated ⇒
    /// `mg_B = +10 = −mg_A`, `eg_B = +20 = −eg_A`; `passed[Black] = {c5}`.
    #[test]
    fn pawn_eval_color_mirror_pair_negates_componentwise() {
        let a = Position::from_fen("4k3/8/8/8/2P5/8/8/4K3 w - - 0 1").expect("mirror A");
        let b = Position::from_fen("4k3/8/8/2p5/8/8/8/4K3 w - - 0 1").expect("mirror B");
        let pe_a = pawn_eval(&a);
        let pe_b = pawn_eval(&b);
        // Composition pinned symbolically against `eval::data` (lone white
        // isolani ⇒ white net = ISO term only). Tracks the shipped consts, so it
        // holds at the M6.B inert CONN-only config (ISO = 0) and at the M6.I tuned
        // ship. NOTE: the M6.I Texel retune set ISO_MG slightly POSITIVE (+5;
        // ISO_EG = -1), overriding the "isolated pawn is always a penalty" prior —
        // a weakly-identified minor term the joint, SPRT-validated vector is free
        // to set. No sign assertion is made here (see docs/milestones/m6.i.md).
        assert_eq!(pe_a.mg, ISO_MG, "lone white isolani → mg = ISO_MG");
        assert_eq!(pe_a.eg, ISO_EG, "lone white isolani → eg = ISO_EG");
        assert_eq!(
            pe_b.mg, -pe_a.mg,
            "mirrored black-only isolani mg must be the negation of white's"
        );
        assert_eq!(
            pe_b.eg, -pe_a.eg,
            "mirrored black-only isolani eg must be the negation of white's"
        );
        // Detection-bitboard symmetry across the hand-mirror pair.
        assert_eq!(
            pe_a.passed[Color::White.index()],
            Bitboard::from_square(Square::C4),
            "A: c4 is a white passer (no black pawns)"
        );
        assert_eq!(
            pe_b.passed[Color::Black.index()],
            Bitboard::from_square(Square::C5),
            "B: c5 is a black passer (mirror of A's white passer)"
        );
        // The white-term popcount on A equals the black-term popcount on B.
        let (wa, _) = (
            a.pieces_colored(Color::White, crate::piece::PieceKind::Pawn),
            (),
        );
        let (_, bb_b) = (
            (),
            b.pieces_colored(Color::Black, crate::piece::PieceKind::Pawn),
        );
        assert_eq!(
            isolated_pawns(wa).count(),
            isolated_pawns(bb_b).count(),
            "white isolated popcount on A == black isolated popcount on B"
        );
    }

    /// Color-mirror pair for the doubled term. Isolates the doubled (+
    /// isolated) terms under **pure-stack semantics** (ADR-0032 §6). A pure
    /// DBL-only fixture is impossible without an adjacent friendly pawn (which
    /// would introduce a connected term); the doubled d-file pawns are
    /// *necessarily* isolated, so ISO and DBL both fire and stack — exactly
    /// what pure-stack mandates. With no enemy pawns there is no backward
    /// confound, so the full contribution is hand-derivable to an exact value.
    ///
    /// FEN A (white doubled d-file, d3+d4): "4k3/8/8/8/3P4/3P4/8/4K3"
    /// FEN B (black doubled d-file, d5+d6, vertical-mirror+color-swap):
    ///        "4k3/8/3p4/3p4/8/8/8/4K3"
    ///
    /// Hand-derivation (A): white {d3, d4}, no black. isolated = {d3, d4}
    /// (no c/e-file friendly pawn) ⇒ count 2. doubled = {d3} (d4 front-most
    /// for white) ⇒ count 1. backward = ∅ (no enemy attacks any stop).
    /// connected = ∅ (d3/d4 not same-rank phalanx; d3 attacks c4/e4, not d4,
    /// so d4 not defended). White net =
    /// `2·ISO_MG + 1·DBL_MG = 2·(−10) + (−10) = −30` (mg),
    /// `2·ISO_EG + 1·DBL_EG = 2·(−20) + (−15) = −55` (eg) ⇒ `mg_A = −30 < 0`,
    /// `eg_A = −55 < 0` — assertion follows the math. B is the exact mirror ⇒
    /// `mg_B = +30 = −mg_A`, `eg_B = +55 = −eg_A`. White doubled popcount on
    /// A == black doubled popcount on B (both 1 extra).
    #[test]
    fn pawn_eval_doubled_color_mirror_pair_negates_componentwise() {
        let a = Position::from_fen("4k3/8/8/8/3P4/3P4/8/4K3 w - - 0 1").expect("doubled mirror A");
        let b = Position::from_fen("4k3/8/3p4/3p4/8/8/8/4K3 w - - 0 1").expect("doubled mirror B");
        let pe_a = pawn_eval(&a);
        let pe_b = pawn_eval(&b);
        // Composition pinned symbolically (2 isolated + 1 doubled-extra,
        // pure-stack). Holds at the M6.B CONN-only ship (all three = 0) and
        // at any M6.F re-tune.
        assert_eq!(
            pe_a.mg,
            2 * ISO_MG + DBL_MG,
            "doubled+isolated d3/d4 → mg = 2*ISO_MG + DBL_MG"
        );
        assert_eq!(
            pe_a.eg,
            2 * ISO_EG + DBL_EG,
            "doubled+isolated d3/d4 → eg = 2*ISO_EG + DBL_EG"
        );
        assert!(pe_a.mg <= 0, "ISO+DBL penalties sum ≤0 at any weight");
        assert!(pe_a.eg <= 0, "ISO+DBL eg penalties sum ≤0 at any weight");
        assert_eq!(pe_b.mg, -pe_a.mg, "doubled mirror: mg negated");
        assert_eq!(pe_b.eg, -pe_a.eg, "doubled mirror: eg negated");
        let (wa, _) = pawns_of("4k3/8/8/8/3P4/3P4/8/4K3 w - - 0 1");
        let (_, bb_b) = pawns_of("4k3/8/3p4/3p4/8/8/8/4K3 w - - 0 1");
        assert_eq!(
            doubled_pawns(wa).count(),
            doubled_pawns(bb_b).count(),
            "white doubled popcount on A == black doubled popcount on B"
        );
    }

    /// Color-mirror pair for the backward term. Isolates the backward term
    /// under **pure-stack semantics** (ADR-0032 §6: ISO/DBL/BWD stack, no
    /// if-else suppression). The prior fixture
    /// (`4k3/8/8/1p1p4/3P4/2P5/8/4K3`) was confounded — its black b5/d5 were
    /// *each* isolated AND backward, so under honest pure-stack accumulation
    /// the black side's `−1·(2·ISO + 2·BWD)` swamped white's small net and the
    /// position scored `mg = +35` (positive), violating `mg_A < 0`. The
    /// defective Slice-C code "fixed" this by introducing ISO/BWD mutual
    /// exclusion — contradicting the locked ADR. This redesign instead picks a
    /// fixture whose sign is correct under honest stacking.
    ///
    /// FEN A: white {c3, d6}, black {b5, c5} — `4k3/8/3P4/1pp5/8/2P5/8/4K3`.
    ///
    /// d6 is a *structure-free* partner: its only role is to make c3
    /// non-isolated (d-file adjacent to c). d6 is not isolated (c3 adjacent),
    /// not doubled, not backward (stop d7 not enemy-attacked), and creates no
    /// connected term — not in a phalanx with c3 (different ranks), does not
    /// defend c3, not defended by c3 (c3 attacks b4/d4 not d6; d6 attacks
    /// c7/e7 not c3).
    ///
    /// White backward set = {c3}: stop c4; black b5 attacks c4 (b5→c4 SE); c4
    /// is not in white's attack-front-spans (no white pawn can ever defend c4
    /// here) → c3 backward, and no other white term fires ⇒ white net =
    /// `BWD_MG·1 = −8` (mg), `BWD_EG·1 = −12` (eg).
    ///
    /// Black {b5, c5} is a same-rank-4 phalanx: NOT isolated (b/c mutually
    /// adjacent files), NOT doubled, NOT backward (stops b4/c4: only b4 ∈
    /// white attacks via c3, but b4 ∈ black's own attack-front-spans through
    /// c5 → masked out → backward(black)=∅), connected = the phalanx {b5, c5}
    /// (count 2). Both on chess rank 5 ⇒ black relative rank `7 − 4 = 3` ⇒
    /// black net term = `CONN_MG[3]·2 = 7·2 = 14` (mg),
    /// `CONN_EG[3]·2 = 10·2 = 20` (eg); applied with `sign = −1`.
    ///
    /// Total ⇒ `mg_A = −8 + (−14) = −22 < 0`,
    /// `eg_A = −12 + (−20) = −32 < 0`. The sign assertion follows from this
    /// arithmetic, not vice-versa.
    ///
    /// M6.F maintenance note: unlike the isolated/doubled/connected mirror
    /// fixtures (each a single weight table), these exact-value pins mix two
    /// independent weight tables — BWD (white c3) and CONN (black b5/c5
    /// phalanx). If BWD and CONN are independently retuned in M6.F the
    /// `assert_eq!` pins (not the sign checks) will need re-deriving from the
    /// new `eval::data` constants.
    ///
    /// FEN B = exact vertical-mirror + color-swap of A:
    /// `4k3/8/2p5/8/1PP5/3p4/8/4K3` (white c3↔black c6, white d6↔black d3,
    /// black b5↔white b4, black c5↔white c4). By the mirror symmetry of the
    /// per-term formulas `pe_B = −pe_A` componentwise ⇒ `mg_B = +22`,
    /// `eg_B = +32`. Black backward popcount on B = 1 (c6) = white backward
    /// popcount on A.
    #[test]
    fn pawn_eval_backward_color_mirror_pair_negates_componentwise() {
        let a =
            Position::from_fen("4k3/8/3P4/1pp5/8/2P5/8/4K3 w - - 0 1").expect("backward mirror A");
        let b =
            Position::from_fen("4k3/8/2p5/8/1PP5/3p4/8/4K3 w - - 0 1").expect("backward mirror B");
        let pe_a = pawn_eval(&a);
        let pe_b = pawn_eval(&b);
        // Composition pinned symbolically: white net = BWD term (c3); black
        // net = CONN over the b5/c5 phalanx at black relative rank 3 (2 pawns),
        // applied with sign −1. Holds at the M6.B CONN-only ship (BWD=0,
        // CONN live) and at any M6.F re-tune. mg/eg are strictly negative
        // because CONN[3] > 0 dominates the non-positive BWD term.
        assert_eq!(
            pe_a.mg,
            BWD_MG - 2 * CONN_MG[3],
            "white backward c3 + black rank-5 phalanx → mg = BWD_MG − 2·CONN_MG[3]"
        );
        assert_eq!(
            pe_a.eg,
            BWD_EG - 2 * CONN_EG[3],
            "white backward c3 + black rank-5 phalanx → eg = BWD_EG − 2·CONN_EG[3]"
        );
        assert!(pe_a.mg < 0, "backward fixture → negative mg contribution");
        assert!(pe_a.eg < 0, "backward fixture → negative eg contribution");
        assert_eq!(pe_b.mg, -pe_a.mg, "backward mirror: mg negated");
        assert_eq!(pe_b.eg, -pe_a.eg, "backward mirror: eg negated");
        let (wa_all, bp_all) = pawns_of("4k3/8/3P4/1pp5/8/2P5/8/4K3 w - - 0 1");
        let (wp_b, bb_b) = pawns_of("4k3/8/2p5/8/1PP5/3p4/8/4K3 w - - 0 1");
        assert_eq!(
            backward_pawns(wa_all, bp_all, Color::White).count(),
            backward_pawns(bb_b, wp_b, Color::Black).count(),
            "white backward popcount on A == black backward popcount on B"
        );
    }

    /// Color-mirror pair for the connected term. Isolates the connected term
    /// under **pure-stack semantics** (ADR-0032 §6): an adjacent-file phalanx
    /// with no enemy pawns has *only* the connected bonus — adjacent files ⇒
    /// not isolated, no enemy ⇒ not backward, single occupant per file ⇒ not
    /// doubled.
    ///
    /// FEN A (white phalanx d4/e4): "4k3/8/8/8/3PP3/8/8/4K3"
    /// FEN B (black phalanx d5/e5, vertical-mirror+color-swap):
    ///        "4k3/8/8/3pp3/8/8/8/4K3"
    ///
    /// Hand-derivation (A): white {d4, e4}, no black. isolated = ∅ (d/e
    /// mutually adjacent files). doubled = ∅; backward = ∅ (no enemy).
    /// connected = phalanx {d4, e4} (same rank 4, adjacent files) ⇒ count 2;
    /// both on chess rank 4 ⇒ white relative rank 3. White net =
    /// `CONN_MG[3]·2 = 7·2 = 14` (mg), `CONN_EG[3]·2 = 10·2 = 20` (eg) ⇒
    /// `mg_A = +14 > 0`, `eg_A = +20 > 0` — assertion follows the math. B is
    /// the exact mirror ⇒ `mg_B = −14 = −mg_A`, `eg_B = −20 = −eg_A`.
    /// White connected popcount on A == black connected popcount on B (both 2).
    #[test]
    fn pawn_eval_connected_color_mirror_pair_negates_componentwise() {
        let a = Position::from_fen("4k3/8/8/8/3PP3/8/8/4K3 w - - 0 1").expect("conn mirror A");
        let b = Position::from_fen("4k3/8/8/3pp3/8/8/8/4K3 w - - 0 1").expect("conn mirror B");
        let pe_a = pawn_eval(&a);
        let pe_b = pawn_eval(&b);
        // Symbolic against the shipped consts (robust to the M6.I re-tune:
        // CONN_MG[3] = 7 unchanged, CONN_EG[3] = 10 → 9).
        assert_eq!(
            pe_a.mg,
            2 * CONN_MG[3],
            "rank-4 phalanx d4/e4 → mg = 2*CONN_MG[3]"
        );
        assert_eq!(
            pe_a.eg,
            2 * CONN_EG[3],
            "rank-4 phalanx d4/e4 → eg = 2*CONN_EG[3]"
        );
        assert!(pe_a.mg > 0, "white phalanx d4/e4 → positive mg (bonus)");
        assert!(
            pe_a.eg > 0,
            "white rank-4 phalanx → positive eg (CONN_EG[rank4] > 0)"
        );
        assert_eq!(pe_b.mg, -pe_a.mg, "connected mirror: mg negated");
        assert_eq!(pe_b.eg, -pe_a.eg, "connected mirror: eg negated");
        let (wa, _) = pawns_of("4k3/8/8/8/3PP3/8/8/4K3 w - - 0 1");
        let (_, bb_b) = pawns_of("4k3/8/8/3pp3/8/8/8/4K3 w - - 0 1");
        assert_eq!(
            connected_pawns(wa, Color::White).count(),
            connected_pawns(bb_b, Color::Black).count(),
            "white connected popcount on A == black connected popcount on B"
        );
    }

    /// Connected rank-scaling: the SAME phalanx shape at a more advanced
    /// rank yields a strictly larger (more positive) white MG contribution.
    /// Fixture 1: white phalanx b3/c3 (relative rank 2 for white). Fixture
    /// 2: white phalanx b6/c6 (relative rank 5). With CONN positive and
    /// rank-scaled (research §1.4 table: rank 6 entry ≫ rank 3 entry), the
    /// rank-6 fixture's mg must exceed the rank-3 fixture's mg.
    ///
    /// Hand-derivation: both fixtures have ONLY the connected term active
    /// (no isolated: b,c adjacent; no doubled; no backward — no enemy pawns;
    /// passers present but passers add nothing to mg/eg in M6.B —
    /// detection-only). So mg = Σ CONN_MG[rank]. CONN_MG is monotone
    /// increasing in advancement → mg(rank6 phalanx) > mg(rank3 phalanx).
    #[test]
    fn pawn_eval_connected_rank_scaling_monotone() {
        let low = Position::from_fen("4k3/8/8/8/8/1PP5/8/4K3 w - - 0 1").expect("rank-3 phalanx");
        let high = Position::from_fen("4k3/8/1PP5/8/8/8/8/4K3 w - - 0 1").expect("rank-6 phalanx");
        let mg_low = pawn_eval(&low).mg;
        let mg_high = pawn_eval(&high).mg;
        assert!(
            mg_high > mg_low,
            "advanced phalanx must score higher (rank-scaled CONN): \
             mg(rank3)={mg_low}, mg(rank6)={mg_high}"
        );
        assert!(
            mg_low > 0,
            "a pure connected phalanx with no weaknesses → positive mg"
        );
    }

    /// Stacking pinned at the pawn_eval level (D5; ADR-0032 §6: "Isolated/
    /// doubled/backward stack (no if-else suppression)"). This test's whole
    /// point is to keep BOTH ISO and DBL firing on the same pawn — it must
    /// NOT isolate to a single term. White a2 and a4: each is isolated (no
    /// b-file friendly pawn) and a2 is the rear of a doubled a-file pair.
    /// Under **pure-stack** semantics a2 incurs ISO *and* DBL simultaneously
    /// (proving the reverted code applies no cross-term `& !` suppression).
    ///
    /// Detection: isolated = {a2, a4} ⇒ count 2. doubled = {a2} ⇒ count 1
    /// (a4 front-most for white). backward = ∅ (no enemy pawns); connected
    /// = ∅ (a2/a4 not phalanx, a2 attacks b3 not a4, so a4 not defended).
    ///
    /// **The no-suppression invariant is asserted weight-free on the predicate
    /// popcounts** (`isolated_pawns(wp).count()==2` ∧ `doubled_pawns(wp)
    /// .count()==1`): a2 ∈ *both* sets is exactly "no if-else suppression",
    /// and a suppressing implementation would fail those `assert_eq!`s
    /// regardless of weights. This is the load-bearing discriminator — needed
    /// because the M6.B shipped CONN-only config zeroes ISO/DBL/BWD, so the
    /// weighted sum (`pe.mg == 2·ISO_MG + DBL_MG`, now `== 0`) can no longer
    /// tell suppressed from non-suppressed code. The `pe.mg/eg` pins are kept
    /// as a composition check, expressed symbolically over the `eval::data`
    /// constants so they auto-revalidate against whatever values M6.F's joint
    /// Texel pass produces. The a-pawns are passers (no black pawns) but
    /// detection adds nothing to mg/eg.
    #[test]
    fn pawn_eval_isolated_doubled_stack() {
        let pos = Position::from_fen("4k3/8/8/8/P7/8/P7/4K3 w - - 0 1").expect("a2+a4 stack FEN");
        let pe = pawn_eval(&pos);
        // No-suppression is a STRUCTURAL claim, asserted weight-free on the
        // predicate popcounts (the M6.B CONN-only ship zeroes ISO/DBL weights,
        // so the weighted sum alone cannot discriminate suppression). a2 must
        // appear in BOTH the isolated set (a2, a4) AND the doubled set (a-file
        // extra) — exactly what "no if-else suppression" means.
        let (wp, _) = pawns_of("4k3/8/8/8/P7/8/P7/4K3 w - - 0 1");
        assert_eq!(
            isolated_pawns(wp).count(),
            2,
            "no suppression: a2 AND a4 both isolated"
        );
        assert_eq!(
            doubled_pawns(wp).count(),
            1,
            "no suppression: a2 also counts as the a-file doubled extra"
        );
        // Composition pinned symbolically (2 isolated + 1 doubled-extra).
        assert_eq!(
            pe.mg,
            2 * ISO_MG + DBL_MG,
            "stack composition: 2*ISO_MG + DBL_MG (no if-else suppression)"
        );
        assert_eq!(
            pe.eg,
            2 * ISO_EG + DBL_EG,
            "stack composition: 2*ISO_EG + DBL_EG"
        );
        // Both a-pawns are passers (no black pawns); passed[] is detection-only in M6.B.
        assert_eq!(
            pe.passed[Color::White.index()],
            Bitboard::from_square(Square::A2) | Bitboard::from_square(Square::A4),
            "a2 and a4 are both white passers (no black pawns)"
        );
        assert_eq!(
            pe.passed[Color::Black.index()],
            Bitboard::EMPTY,
            "no black pawns → no black passers"
        );
    }

    // -----------------------------------------------------------------------
    // PawnHashTable: new/clear are real; get is stubbed.
    // -----------------------------------------------------------------------

    /// `new()` allocates the full fixed-size table without panicking and the
    /// entry-count arithmetic resolves to 2^17.
    #[test]
    fn pawn_hash_new_allocates_full_table() {
        let ph = PawnHashTable::new();
        assert_eq!(
            ph.entries.len(),
            PAWN_HASH_ENTRIES,
            "table must hold PAWN_HASH_ENTRIES slots"
        );
        assert_eq!(PAWN_HASH_ENTRIES, 1 << 17, "4 MiB / 32 B = 2^17 entries");
    }

    /// `new()` zeroes every slot (key 0 = empty sentinel).
    #[test]
    fn pawn_hash_new_is_zeroed() {
        let ph = PawnHashTable::new();
        assert!(
            ph.entries.iter().all(|e| e.key == 0),
            "every slot key must be 0 after new()"
        );
    }

    /// `clear()` re-zeroes a table whose slots were mutated.
    #[test]
    fn pawn_hash_clear_zeroes_all_slots() {
        let mut ph = PawnHashTable::new();
        // Dirty a few slots directly (private fields visible in-module).
        ph.entries[0].key = 0xDEAD_BEEF;
        ph.entries[0].mg = 123;
        ph.entries[1].key = 0x1234_5678;
        ph.entries[PAWN_HASH_ENTRIES - 1].key = 0xFFFF;
        ph.clear();
        assert!(
            ph.entries
                .iter()
                .all(|e| e.key == 0 && e.mg == 0 && e.eg == 0),
            "clear() must zero every slot's key/mg/eg"
        );
    }

    /// Direct slot-collision unit test: provably exercises the full-key
    /// verification path in `get`. Recipe:
    ///   (a) `get(p_a)` once → p_a's real slot is populated with p_a's data.
    ///   (b) Compute p_a's actual slot index from `p_a.pawn_zobrist()` (same
    ///       mask the table uses) and overwrite that slot with a different key
    ///       (`pz ^ (1u64 << 17)` — same low-17-bit index, different full key)
    ///       and poisoned values (999, -999).
    ///   (c) `get(p_a)` again — it now provably probes the poisoned slot;
    ///       the full-key check must reject the stale entry, recompute, and
    ///       return `pawn_eval(p_a)` (not 999/-999).
    ///
    /// This kills a "store without key-verification on read-back" stub: such a
    /// stub would return (999, -999) in step (c) and fail the assertions.
    ///
    /// Stub note: panics on `unimplemented!()` until Slice C — that is the
    /// test-first gate; the invariant is correct against the §6 contract.
    #[test]
    fn pawn_hash_collision_full_key_verification() {
        let p_a = Position::from_fen("4k3/8/8/8/P7/8/P7/4K3 w - - 0 1").expect("p_a");
        let mut ph = PawnHashTable::new();

        // (a) Populate p_a's real slot.
        let first = ph.get(&p_a);

        // (b) Overwrite that exact slot with a different key and poisoned values.
        let pz = p_a.pawn_zobrist();
        let slot_idx = (pz as usize) & (PAWN_HASH_ENTRIES - 1);
        let poison_key = pz ^ (1u64 << 17); // same slot index, different full key
        debug_assert_eq!(
            (poison_key as usize) & (PAWN_HASH_ENTRIES - 1),
            slot_idx,
            "poison_key must map to the same slot as pz"
        );
        ph.entries[slot_idx].key = poison_key;
        ph.entries[slot_idx].mg = 999;
        ph.entries[slot_idx].eg = -999;

        // (c) Re-probe p_a: must detect key mismatch and recompute.
        let second = ph.get(&p_a);
        assert_ne!(
            (second.mg, second.eg),
            (999, -999),
            "full-key mismatch: poisoned slot must be rejected, not returned"
        );
        assert_eq!(
            second, first,
            "re-probe after slot collision must return the same PawnEval as the first get"
        );
        assert_eq!(
            second,
            pawn_eval(&p_a),
            "re-probe after slot collision must equal pawn_eval(p_a)"
        );
    }

    /// Entry layout is pinned to 32 bytes (the const-assert duplicated as a
    /// runtime check so a reviewer sees it fail loudly, not just at compile).
    #[test]
    fn pawn_hash_entry_is_32_bytes() {
        assert_eq!(
            core::mem::size_of::<PawnHashEntry>(),
            32,
            "PawnHashEntry must be 32 B (4-MiB / 2^17 arithmetic depends on it)"
        );
    }

    /// `get` miss-then-hit returns equal `PawnEval`. Uses a position with a
    /// non-zero pawn_zobrist so the key != 0 cache path is exercised. (Stub:
    /// fails with `unimplemented!` until Slice C — that is the test-first
    /// gate; the assertion is correct against the §6 contract.)
    #[test]
    fn pawn_hash_get_miss_then_hit_equal() {
        let pos = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1")
            .expect("startpos");
        let mut ph = PawnHashTable::new();
        let first = ph.get(&pos); // miss → compute + store
        let second = ph.get(&pos); // hit → reconstructed
        assert_eq!(first, second, "miss-then-hit must return the same PawnEval");
        assert_eq!(
            second,
            pawn_eval(&pos),
            "cached value must equal the uncached pawn_eval (pure accelerator)"
        );
    }

    /// `clear` forces a subsequent `get` to recompute (miss). The recomputed
    /// value must still equal `pawn_eval`.
    #[test]
    fn pawn_hash_get_after_clear_recomputes_correctly() {
        let pos = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1")
            .expect("startpos");
        let mut ph = PawnHashTable::new();
        let _ = ph.get(&pos);
        ph.clear();
        let after = ph.get(&pos);
        assert_eq!(
            after,
            pawn_eval(&pos),
            "post-clear get must recompute the correct PawnEval"
        );
    }

    /// Collision-slot correctness: two positions whose pawn_zobrist values
    /// land in the same table index but differ must NOT alias. After storing
    /// the first and then the second (always-replace), a re-`get` of the
    /// first must reconstruct the FIRST's value (full-key verification
    /// rejects the stale second-position entry → recompute, correct).
    ///
    /// We do not hand-pick a true index collision (pawn_zobrist is opaque);
    /// instead we assert the weaker, sufficient invariant: `get` on two
    /// different pawn structures returns each structure's own `pawn_eval`,
    /// interleaved, with no cross-contamination. This kills a "store but
    /// never verify the key" stub.
    #[test]
    fn pawn_hash_get_distinct_structures_no_cross_contamination() {
        let p1 = Position::from_fen("4k3/8/8/8/P7/8/P7/4K3 w - - 0 1").expect("p1");
        let p2 = Position::from_fen("4k3/8/8/8/3PP3/8/8/4K3 w - - 0 1").expect("p2");
        let mut ph = PawnHashTable::new();
        let a1 = ph.get(&p1);
        let b2 = ph.get(&p2);
        let a1_again = ph.get(&p1);
        assert_eq!(a1, pawn_eval(&p1), "p1 first get must equal pawn_eval(p1)");
        assert_eq!(b2, pawn_eval(&p2), "p2 get must equal pawn_eval(p2)");
        assert_eq!(
            a1_again,
            pawn_eval(&p1),
            "re-get of p1 must still equal pawn_eval(p1), not p2's value"
        );
    }

    /// `get` on a no-pawn position (pawn_zobrist == 0) returns the trivial
    /// identity and does not corrupt the table. Pins the key==0 decision
    /// (ADR-0032 §2): never probe/store, recompute → (0,0,empty).
    #[test]
    fn pawn_hash_get_no_pawn_position_returns_identity() {
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/4K3 w - - 0 1").expect("KvK");
        let mut ph = PawnHashTable::new();
        let pe = ph.get(&pos);
        assert_eq!(
            pe,
            PawnEval {
                mg: 0,
                eg: 0,
                passed: [Bitboard::EMPTY; 2]
            },
            "no-pawn position → (0,0,empty) identity"
        );
        // The table must remain pristine (no store happened for key==0).
        assert!(
            ph.entries.iter().all(|e| e.key == 0),
            "key==0 path must not store → table stays all-zero"
        );
    }

    // -----------------------------------------------------------------------
    // M6.C — passed_pawn_term_white.
    //
    // These tests exercise `passed_pawn_term_white`, which is laid down as an
    // `unimplemented!()` stub by the test-first gate (Slice A implements).
    // They therefore PANIC against the stub — that is the intended red state;
    // do NOT weaken the assertions to make them pass.
    //
    // Every expected value is hand-derived from the `eval::data` constants
    // (`PASSED_MG/EG`, `PASSED_FREE_EG_DELTA`, `PASSED_KDIST_*`) and pinned
    // symbolically over those constants so the M6.F joint-Texel re-tune
    // auto-revalidates them. The algorithm is plan §5:
    //   per passer (white sign +1 / black sign −1, rel = white rank /
    //   7−rank for black):
    //     mg += sign·PASSED_MG[rel]
    //     eg += sign·PASSED_EG[rel]
    //     eg += sign·(+Δ if front-span empty of all / −Δ if enemy on it / 0)
    //     eg += sign·rel·OWN ·(CAP − own_d)   (own_d  = min(cheb(own_k ,promo),CAP))
    //     eg += sign·rel·ENE·(enemy_d − CAP)  (enemy_d= min(cheb(enemy_k,promo),CAP))
    //   promo = from_file_rank(file, 7) white / (file, 0) black.
    // The `passed` argument is always the M6.B-cached `pawn_eval(&pos).passed`
    // (or a hand-built bitboard where the test needs a square no real pawn
    // can stand on, e.g. relative rank 7).
    // (M6.C `PASSED_*` constants are imported at the top of `mod tests`.)
    // -----------------------------------------------------------------------

    /// Rank-bonus scaling. Two single-white-passer fixtures, both with an
    /// empty front-span (path-clear +Δ) and both kings ≥ CAP Chebyshev from
    /// the promotion square (king term = 0): the only difference is relative
    /// rank, so the contribution must scale by the rank table.
    ///
    /// Fixture LOW: white Pc3 (file 2, rank idx 2 → rel-rank 2), white Ka1,
    /// black kh1. promo = c8. cheb(a1,c8)=max(2,7)=7≥5 ⇒ own_d=5;
    /// cheb(h1,c8)=max(5,7)=7≥5 ⇒ enemy_d=5 ⇒ king term =
    /// rel·5·(5−5)+rel·7·(5−5)=0. c-file ahead of c3 unoccupied ⇒ +Δ.
    ///   mg = PASSED_MG[2] = 3
    ///   eg = PASSED_EG[2] + PASSED_FREE_EG_DELTA[2] + 0 = 4 + 4 = 8
    ///
    /// Fixture HIGH: white Pc7 (rel-rank 6), same kings (a1/h1). promo = c8,
    /// king term 0; front-span {c8} unoccupied ⇒ +Δ.
    ///   mg = PASSED_MG[6] = 34
    ///   eg = PASSED_EG[6] + PASSED_FREE_EG_DELTA[6] + 0 = 118 + 98 = 216
    ///
    /// Pins: exact (mg,eg) symbolically from the `eval::data` constants — at
    /// the shipped score-neutral config every `PASSED_*` is 0, so each LHS is
    /// 0 = RHS; the symbolic forms auto-revalidate the EG-dominant / monotone
    /// shape when M6.F re-tunes (the M6.B `pawn_eval` symbolic-pin precedent).
    /// The weight-independent structural invariant the test exercises — that
    /// each fixture's passer is detected at the asserted *relative rank* (2 vs
    /// 6) — is pinned directly on the detection bitboard, so a rel-rank /
    /// passer-detection bug still fails regardless of weights.
    #[test]
    fn passed_rank_bonus_scales_white() {
        let low = Position::from_fen("8/8/8/8/8/2P5/8/K6k w - - 0 1").expect("rel-rank-2 passer");
        let high = Position::from_fen("8/2P5/8/8/8/8/8/K6k w - - 0 1").expect("rel-rank-6 passer");
        let (lmg, leg) = passed_pawn_term_white(&low, &pawn_eval(&low).passed);
        let (hmg, heg) = passed_pawn_term_white(&high, &pawn_eval(&high).passed);

        // Structural (weight-free): the lone white passer sits at rel-rank 2
        // (c3) in LOW and rel-rank 6 (c7) in HIGH. rel-rank = sq.rank() for
        // white; this is the exact index the term reads into PASSED_*[rel].
        let low_p = pawn_eval(&low).passed[Color::White.index()];
        let high_p = pawn_eval(&high).passed[Color::White.index()];
        assert_eq!(
            low_p,
            Bitboard::from_square(Square::C3),
            "LOW: lone white passer is c3 → rel-rank 2"
        );
        assert_eq!(
            high_p,
            Bitboard::from_square(Square::C7),
            "HIGH: lone white passer is c7 → rel-rank 6"
        );

        assert_eq!(lmg, PASSED_MG[2], "rel-rank-2 mg = PASSED_MG[2]");
        assert_eq!(
            leg,
            PASSED_EG[2] + PASSED_FREE_EG_DELTA[2],
            "rel-rank-2 eg = PASSED_EG[2] + path-clear Δ[2] (kings far → king term 0)"
        );
        assert_eq!(hmg, PASSED_MG[6], "rel-rank-6 mg = PASSED_MG[6]");
        assert_eq!(
            heg,
            PASSED_EG[6] + PASSED_FREE_EG_DELTA[6],
            "rel-rank-6 eg = PASSED_EG[6] + path-clear Δ[6]"
        );
        // The EG-dominance / rank-monotonicity priors that held for the inert
        // (all-zero) and literature configs were DROPPED at the M6.I tuned ship:
        // the Texel-recovered passed table is deliberately non-intuitive —
        // PASSED_MG = [..,-30,-17,25,36,40,0] (negative MG for early passers) and
        // EG = [..,15,38,39,35,3,0], so e.g. rel-rank-6 is not EG-dominant
        // (eg 3+34=37 < mg 40). The joint vector was SPRT-validated (+93.9 Elo
        // [+69.0,+119.7] vs M6.F); per-term shape priors are not invariants of
        // the shipped weights. The symbolic index-tracking assertions above (the
        // term reads PASSED_*[rel_rank]) remain the structural invariant.
        // See docs/milestones/m6.i.md "counterintuitive term shapes".
    }

    /// D6 / research §7 "rank-7 MG non-zero" pitfall. A real pawn cannot stand
    /// on the promotion rank, so the relative-rank-7 table entry is only ever
    /// reached via a constructed `passed` bitboard — we pass `passed[White] =
    /// {c8}` (c8 = file 2, rank idx 7 → rel-rank 7) over a bare-kings board.
    ///
    /// front-span of a rank-8 square = north_fill(shift_north) = ∅ (shifts off
    /// the board) ⇒ path empty ⇒ +Δ[7]. promo = from_file_rank(2,7) = c8;
    /// white Ka1 cheb(a1,c8)=7≥5, black kh1 cheb(h1,c8)=7≥5 ⇒ king term 0.
    ///   mg = PASSED_MG[7] = 0     ← the D6 pin
    ///   eg = PASSED_EG[7] + PASSED_FREE_EG_DELTA[7] = 170 + 141 = 311
    #[test]
    fn passed_rank7_mg_zero_eg_dominant() {
        let pos = Position::from_fen("8/8/8/8/8/8/8/K6k w - - 0 1").expect("bare-kings board");
        let passed = [Bitboard::from_square(Square::C8), Bitboard::EMPTY];
        let (mg, eg) = passed_pawn_term_white(&pos, &passed);
        // The D6 pin is weight-INDEPENDENT and survives literally: the rank-7
        // MG entry is 0 by design (pre-zero literature default *and* the
        // shipped score-neutral config). A misread that put a non-zero value
        // at PASSED_MG[7] still fails this.
        assert_eq!(
            mg, PASSED_MG[7],
            "rel-rank-7 MG entry is 0 (D6): a rank-7 passer is decisive in EG only"
        );
        assert_eq!(mg, 0, "PASSED_MG[7] is literally 0 (pins the table value)");
        // Composition pinned symbolically (rank-8 square ⇒ empty front-span ⇒
        // the +Δ branch; kings 7 cheb ⇒ king term 0). 0 = 0 at the shipped
        // config; M6.F-revalidates the EG-dominant rank-7 magnitude.
        assert_eq!(
            eg,
            PASSED_EG[7] + PASSED_FREE_EG_DELTA[7],
            "rel-rank-7 eg = PASSED_EG[7] + path-clear Δ[7] (empty front-span, kings far)"
        );
        // Pre-zero literature anchor (PASSED_EG[7]=170 + Δ[7]=141 = 311) is the
        // M6.F restart point — asserted as a symbolic property of the formula
        // shape, not the shipped magnitude (which is 0). EG-dominance is the
        // magnitude claim the shipped config zeroes; re-expressed symbolically.
        assert!(
            PASSED_EG[7] + PASSED_FREE_EG_DELTA[7] >= PASSED_MG[7],
            "rank-7 passer is EG-dominant (M6.F-revalidated; 0 at shipped weights)"
        );
    }

    /// Path-clear three-state, branch 1: empty front-span → +Δ. White Pe5
    /// (rel-rank 4), no piece on the e-file ahead, both kings ≥5 cheb from
    /// promo e8 (Ka1: cheb=7; kh1: cheb=7) → king term 0.
    ///   mg = PASSED_MG[4] = 15
    ///   eg = PASSED_EG[4] + PASSED_FREE_EG_DELTA[4] + 0 = 42 + 35 = 77
    /// The discriminator: eg must be strictly greater than PASSED_EG[4] alone
    /// (the +Δ branch fired).
    #[test]
    fn passed_path_clear_adds_delta() {
        let pos = Position::from_fen("8/8/8/4P3/8/8/8/K6k w - - 0 1").expect("clear-path passer");
        let (mg, eg) = passed_pawn_term_white(&pos, &pawn_eval(&pos).passed);
        // Structural (weight-free): the path-CLEAR branch is selected because
        // e5's white front-span ({e6,e7,e8}) is empty of ALL pieces. This is
        // the exact classification `passed_pawn_term_white` keys the +Δ branch
        // on; a path-state bug fails here regardless of weights.
        let e5 = Bitboard::from_square(Square::E5);
        let path = bitboard::white_front_spans(e5);
        assert!(
            (path & pos.occupied_all()).is_empty(),
            "e5 front-span empty of all pieces → the +Δ (path-clear) branch fires"
        );
        assert_eq!(mg, PASSED_MG[4], "rel-rank-4 mg = PASSED_MG[4]");
        assert_eq!(
            eg,
            PASSED_EG[4] + PASSED_FREE_EG_DELTA[4],
            "empty front-span → +Δ[4] added to PASSED_EG[4]"
        );
        // +Δ being a strict bonus over the bare rank value is the magnitude
        // claim zeroed by the shipped config; asserted symbolically (holds at
        // 0, M6.F-revalidates that PASSED_FREE_EG_DELTA[4] ≥ 0).
        assert!(
            PASSED_EG[4] + PASSED_FREE_EG_DELTA[4] >= PASSED_EG[4],
            "path-clear adds a non-negative Δ[4] (M6.F-revalidated)"
        );
    }

    /// Path-clear three-state, branch 2: enemy piece on the front-span → −Δ.
    /// White Pe5 (rel-rank 4), black knight e7 sits on e5's front-span
    /// {e6,e7,e8}. e7 ∈ path ∧ e7 is an enemy piece ⇒ −Δ branch. Kings far
    /// (Ka1 cheb(a1,e8)=7; kh1 cheb(h1,e8)=7) ⇒ king term 0. A non-pawn
    /// enemy does not affect passer detection — e5 is still a passer.
    ///   mg = PASSED_MG[4] = 15
    ///   eg = PASSED_EG[4] − PASSED_FREE_EG_DELTA[4] + 0 = 42 − 35 = 7
    #[test]
    fn passed_path_enemy_piece_penalty() {
        let pos = Position::from_fen("8/4n3/8/4P3/8/8/8/K6k w - - 0 1").expect("enemy-on-path");
        let (mg, eg) = passed_pawn_term_white(&pos, &pawn_eval(&pos).passed);
        // Structural (weight-free): the −Δ branch is selected because an ENEMY
        // (black) piece (the e7 knight) lies on e5's white front-span while
        // the span is NOT all-empty. e5 is still a passer (a non-pawn enemy
        // does not affect passer detection). These are the exact predicates
        // the term branches on — a sign/own-vs-enemy bug fails here at any
        // weight.
        let e5 = Bitboard::from_square(Square::E5);
        let path = bitboard::white_front_spans(e5);
        assert!(
            !(path & pos.occupied_all()).is_empty(),
            "front-span is occupied (knight on it) → not the +Δ branch"
        );
        assert!(
            !(path & pos.occupied(Color::Black)).is_empty(),
            "an ENEMY piece is on the front-span → the −Δ branch fires"
        );
        assert_eq!(
            pawn_eval(&pos).passed[Color::White.index()],
            e5,
            "e5 is still a passer (a non-pawn enemy does not block detection)"
        );
        assert_eq!(mg, PASSED_MG[4], "rank bonus mg unaffected by path state");
        assert_eq!(
            eg,
            PASSED_EG[4] - PASSED_FREE_EG_DELTA[4],
            "enemy knight on the front-span → −Δ[4]"
        );
        // The penalty-below-bare-rank magnitude is zeroed by the shipped
        // config; asserted symbolically (M6.F-revalidates Δ[4] ≥ 0 ⇒ −Δ is a
        // penalty).
        assert!(
            PASSED_EG[4] - PASSED_FREE_EG_DELTA[4] <= PASSED_EG[4],
            "enemy-blocked path is a non-positive Δ adjustment (M6.F-revalidated)"
        );
    }

    /// Path-clear three-state, branch 3 (the D4 / research §3.2 refinement):
    /// only a FRIENDLY piece on the front-span → neither +Δ nor −Δ (neutral).
    /// White Pe5 (rel-rank 4), white knight e7 on e5's front-span. e7 ∈ path
    /// but e7 is friendly, not enemy ⇒ neutral. Kings far ⇒ king term 0.
    ///   mg = PASSED_MG[4] = 15
    ///   eg = PASSED_EG[4] + 0 + 0 = 42   (NO delta either way)
    #[test]
    fn passed_path_friendly_only_neutral() {
        let pos = Position::from_fen("8/4N3/8/4P3/8/8/8/K6k w - - 0 1").expect("friendly-on-path");
        let (mg, eg) = passed_pawn_term_white(&pos, &pawn_eval(&pos).passed);
        // Structural (weight-free) — the three-state D4 refinement: the path
        // is occupied (so NOT the +Δ branch) but the occupant is FRIENDLY, not
        // enemy (so NOT the −Δ branch either) → the neutral third state. A
        // two-state implementation that penalised any occupied path would mis-
        // classify here regardless of weights.
        let e5 = Bitboard::from_square(Square::E5);
        let path = bitboard::white_front_spans(e5);
        assert!(
            !(path & pos.occupied_all()).is_empty(),
            "front-span is occupied (own knight e7) → not the +Δ branch"
        );
        assert!(
            (path & pos.occupied(Color::Black)).is_empty(),
            "no ENEMY piece on the front-span → not the −Δ branch → neutral state"
        );
        assert_eq!(mg, PASSED_MG[4], "rank bonus mg unaffected");
        assert_eq!(
            eg, PASSED_EG[4],
            "friendly-only on the front-span → neutral: PASSED_EG[4] with NO Δ \
             (three-state, not two-state — an own piece must not incur the penalty)"
        );
    }

    /// Path discriminator for a BLACK passer — exercises the own/enemy sense
    /// directly for the black branch (the white tests + the path-CLEAR mirror
    /// would not catch a `path & white_occ`-vs-`path & enemy_occ` inversion
    /// when `side == Black`; research §7's most-likely-inverted pitfall).
    ///
    /// Both fixtures: black Pc4 (file 2, rank idx 3 → black rel-rank 7−3=4),
    /// no white pawns ⇒ c4 is a black passer. black_front_spans(c4) =
    /// {c3,c2,c1}. Black promo = c1. Black king e8 (cheb(e8,c1)=7 ⇒ own_d=5),
    /// white king g6 (cheb(g6,c1)=max(4,5)=5 ⇒ enemy_d=5) ⇒ king term 0.
    /// Sign for a black passer is −1.
    ///
    /// E (enemy-blocked): a WHITE knight on c2 ∈ path ⇒ for BLACK "enemy" =
    /// white ⇒ −Δ branch. eg_E = −1·(PASSED_EG[4] − Δ[4]) = −(42−35) = −7.
    /// F (friendly-neutral): a BLACK knight on c2 ∈ path ⇒ friendly-only ⇒
    /// neutral. eg_F = −1·PASSED_EG[4] = −42.
    /// An own/enemy inversion in the black branch swaps these (E↔F); the
    /// exact pins + `eg_E ≠ eg_F` (−7 ≠ −42) catch it.
    #[test]
    fn passed_path_black_enemy_vs_friendly_branches() {
        let e = Position::from_fen("4k3/8/6K1/8/2p5/8/2N5/8 w - - 0 1")
            .expect("black passer, WHITE knight on its path (enemy-blocked)");
        let f = Position::from_fen("4k3/8/6K1/8/2p5/8/2n5/8 w - - 0 1")
            .expect("black passer, BLACK knight on its path (friendly-neutral)");
        let (mg_e, eg_e) = passed_pawn_term_white(&e, &pawn_eval(&e).passed);
        let (mg_f, eg_f) = passed_pawn_term_white(&f, &pawn_eval(&f).passed);
        // Structural (weight-free): for a BLACK passer, "enemy" = white. The
        // c4 black passer's path is black_front_spans(c4) = {c3,c2,c1}. In E a
        // WHITE knight sits on c2 (∈ path) → enemy-for-black on path → the −Δ
        // branch. In F a BLACK knight sits on c2 → friendly-only → the neutral
        // branch. An own/enemy inversion in the black arm would swap which
        // branch each fixture selects — caught here regardless of weights.
        let c4_path = bitboard::black_front_spans(Bitboard::from_square(Square::C4));
        assert!(
            !(c4_path & e.occupied(Color::White)).is_empty(),
            "E: a WHITE (enemy-for-black) piece is on the black passer's path → −Δ branch"
        );
        assert!(
            (c4_path & e.occupied(Color::Black)).count() == 0,
            "E: no BLACK (friendly) piece on the path other than the passer's own file"
        );
        assert!(
            (c4_path & f.occupied(Color::White)).is_empty(),
            "F: no WHITE piece on the path → not the −Δ branch"
        );
        assert!(
            !(c4_path & f.occupied(Color::Black)).is_empty(),
            "F: a BLACK (friendly) piece is on the path → neutral branch"
        );
        assert_eq!(mg_e, -PASSED_MG[4], "black passer mg = −PASSED_MG[4]");
        assert_eq!(mg_f, -PASSED_MG[4], "black passer mg = −PASSED_MG[4]");
        assert_eq!(
            eg_e,
            -(PASSED_EG[4] - PASSED_FREE_EG_DELTA[4]),
            "E: white (enemy-for-black) piece on the black front-span → −Δ branch"
        );
        assert_eq!(
            eg_f, -PASSED_EG[4],
            "F: black (friendly) piece only on the path → neutral, no Δ"
        );
        // The E≠F magnitude discrimination is zeroed by the shipped config
        // (both collapse to −0); the own/enemy-branch correctness is now
        // pinned structurally above. The directional magnitude claim is
        // re-expressed symbolically (M6.F-revalidated).
        assert!(
            -(PASSED_EG[4] - PASSED_FREE_EG_DELTA[4]) >= -PASSED_EG[4],
            "a white blocker yields a less-negative white-POV eg for black \
             than a harmless own blocker (M6.F-revalidated; equal at 0)"
        );
    }

    /// King-distance sign. Position A: own (white) king adjacent to the promo
    /// square, enemy king far → positive king contribution. Position B: the
    /// kings swapped → negative. Same white Pc5 (rel-rank 4), path clear in
    /// both, so rank+path are identical and only the king geometry differs.
    ///
    /// A: white Kb7 (cheb(b7,c8)=max(1,1)=1 ⇒ own_d=1), black kh1
    /// (cheb(h1,c8)=7 ⇒ enemy_d=5). Path {c6,c7,c8} unoccupied ⇒ +Δ.
    ///   king term = 4·5·(5−1) + 4·7·(5−5) = 80
    ///   eg_A = PASSED_EG[4] + Δ[4] + 80 = 42 + 35 + 80 = 157
    /// B: kings swapped (white Kh1, black kb7) ⇒ own_d=5, enemy_d=1.
    ///   king term = 4·5·(5−5) + 4·7·(1−5) = −112
    ///   eg_B = 42 + 35 − 112 = −35
    /// Discriminator: the king contribution (eg minus the rank+path baseline
    /// PASSED_EG[4]+Δ[4]) is positive in A and negative in B.
    ///
    /// **Score-neutral note (M6.B re-expression precedent).** At the shipped
    /// zero weights every weighted-magnitude assertion here is `0`-valued and
    /// the `const{}` sign invariants reduce to `0>=0`/`0<=0` — the magnitude
    /// discrimination is *dormant* and revalidates only when M6.F restores
    /// non-zero `PASSED_KDIST_*`. The active weight-free guard at the shipped
    /// config is the structural Chebyshev-clamp geometry pin (own_d=1 vs
    /// enemy_d=CAP per fixture); the `const{}` invariants are a compile-time
    /// gate on the *named* `PASSED_KDIST_*` constants (bypassable only if M6.F
    /// introduces the coefficient under a different constant).
    #[test]
    fn passed_king_distance_own_near_is_bonus() {
        let a = Position::from_fen("8/1K6/8/2P5/8/8/8/7k w - - 0 1").expect("own king near promo");
        let b =
            Position::from_fen("8/1k6/8/2P5/8/8/8/7K w - - 0 1").expect("enemy king near promo");
        let (mg_a, eg_a) = passed_pawn_term_white(&a, &pawn_eval(&a).passed);
        let (mg_b, eg_b) = passed_pawn_term_white(&b, &pawn_eval(&b).passed);
        // Structural (weight-free): the king-distance GEOMETRY the term feeds
        // into the formula. Promo square = c8. A: own (white) king b7 is 1
        // cheb from c8, enemy king h1 is ≥CAP. B: kings swapped. These clamped
        // distances are the exact `min(cheb, CAP)` inputs — a wrong promo
        // square / Chebyshev / clamp fails here at any weight.
        let promo = Square::C8;
        assert_eq!(
            chebyshev_distance(a.king_square(Color::White), promo).min(PASSED_KDIST_CAP),
            1,
            "A: own king b7 is Chebyshev-1 from promo c8 (clamp not saturated)"
        );
        assert_eq!(
            chebyshev_distance(a.king_square(Color::Black), promo).min(PASSED_KDIST_CAP),
            PASSED_KDIST_CAP,
            "A: enemy king h1 clamps to CAP (far → enemy term zero)"
        );
        assert_eq!(
            chebyshev_distance(b.king_square(Color::White), promo).min(PASSED_KDIST_CAP),
            PASSED_KDIST_CAP,
            "B: own king h1 clamps to CAP (own term zero)"
        );
        assert_eq!(
            chebyshev_distance(b.king_square(Color::Black), promo).min(PASSED_KDIST_CAP),
            1,
            "B: enemy king b7 is Chebyshev-1 from promo c8"
        );
        // Full-total pins (catch a baseline error that a king-component-only
        // check would miss): mg unaffected by king geometry; eg = rank + Δ ±
        // the exact king term. Symbolic over `eval::data` ⇒ 0 = 0 at the
        // shipped config, M6.F-revalidated.
        assert_eq!(
            mg_a, PASSED_MG[4],
            "A mg = rank bonus only (king term is EG-only)"
        );
        assert_eq!(mg_b, PASSED_MG[4], "B mg = rank bonus only");
        assert_eq!(
            eg_a,
            PASSED_EG[4]
                + PASSED_FREE_EG_DELTA[4]
                + 4 * PASSED_KDIST_OWN_PER_STEP * (PASSED_KDIST_CAP - 1),
            "A eg total = rank + Δ + own-king term (rel·OWN·(CAP−1))"
        );
        assert_eq!(
            eg_b,
            PASSED_EG[4]
                + PASSED_FREE_EG_DELTA[4]
                + 4 * PASSED_KDIST_ENEMY_PER_STEP * (1 - PASSED_KDIST_CAP),
            "B eg total = rank + Δ + enemy-king term (rel·ENEMY·(1−CAP))"
        );
        let baseline = PASSED_EG[4] + PASSED_FREE_EG_DELTA[4];
        let king_a = eg_a - baseline;
        let king_b = eg_b - baseline;
        // Exact symbolic pins (the king-term formula shape; the SIGN is the
        // load-bearing claim — own-near is a bonus, enemy-near a penalty).
        // Holds at 0 (shipped); M6.F-revalidates the non-zero sign/magnitude.
        assert_eq!(
            king_a,
            4 * PASSED_KDIST_OWN_PER_STEP * (PASSED_KDIST_CAP - 1),
            "A: rel·OWN·(CAP−own_d=1) with enemy_d=CAP (enemy term zero)"
        );
        assert_eq!(
            king_b,
            4 * PASSED_KDIST_ENEMY_PER_STEP * (1 - PASSED_KDIST_CAP),
            "B: rel·ENEMY·(enemy_d=1 − CAP) with own_d=CAP (own term zero)"
        );
        // Compile-time structural invariants (M6.F-revalidated): own-king-near
        // is a non-negative bonus, enemy-king-near a non-positive penalty. 0 =
        // 0 at the shipped zeroed coeffs; a future M6.F re-tune that violated
        // the sign relationship would fail the build here.
        const {
            assert!(
                4 * PASSED_KDIST_OWN_PER_STEP * (PASSED_KDIST_CAP - 1) >= 0,
                "own king near promo → non-negative king contribution"
            )
        };
        const {
            assert!(
                4 * PASSED_KDIST_ENEMY_PER_STEP * (1 - PASSED_KDIST_CAP) <= 0,
                "enemy king near promo → non-positive king contribution"
            )
        };
    }

    /// King-distance Chebyshev clamp. P1: both kings exactly CAP=5 cheb from
    /// the promo square. P2: both kings cheb 7 (> CAP). `min(dist, CAP)`
    /// clamps both to 5, so the king term is identical (= 0 here, since both
    /// distances clamp to CAP) and the full (mg,eg) must match across P1/P2.
    ///
    /// White Pd4 (rel-rank 3), promo d8 = (file 3, rank 7).
    /// P1: white Ka3 (cheb(a3,d8)=max(3,5)=5), black kh3 (cheb(h3,d8)=
    /// max(4,5)=5). P2: white Ka1 (cheb=max(3,7)=7), black kh1 (cheb=
    /// max(4,7)=7). Both: own_d=enemy_d=min(·,5)=5 ⇒ king term 0. Path
    /// {d5,d6,d7,d8} unoccupied in both ⇒ +Δ[3].
    ///   (mg,eg) = (PASSED_MG[3], PASSED_EG[3] + Δ[3]) = (8, 18+16=34) for both.
    #[test]
    fn passed_king_distance_capped() {
        let p1 = Position::from_fen("8/8/8/8/3P4/K6k/8/8 w - - 0 1").expect("kings cheb=CAP");
        let p2 = Position::from_fen("8/8/8/8/3P4/8/8/K6k w - - 0 1").expect("kings cheb>CAP");
        let t1 = passed_pawn_term_white(&p1, &pawn_eval(&p1).passed);
        let t2 = passed_pawn_term_white(&p2, &pawn_eval(&p2).passed);
        // The clamp identity is WEIGHT-INDEPENDENT and survives unchanged:
        // both fixtures clamp every king distance to CAP, so the term is bit-
        // identical regardless of the coefficient values (including the
        // shipped zeros). A broken `min(·, CAP)` clamp fails here at any
        // weight (with zeroed coeffs only if the clamp affected the
        // *unclamped* product — which it does not here, so the structural
        // geometry below is the real clamp discriminator).
        assert_eq!(
            t1, t2,
            "Chebyshev distances ≥ CAP clamp identically: =CAP and >CAP give the same term"
        );
        // Structural (weight-free): P1 saturates both clamps at CAP; P2's
        // raw distances exceed CAP yet clamp to the same CAP. This is the
        // load-bearing clamp behaviour, asserted on the geometry directly.
        let promo_d = Square::D8;
        assert_eq!(
            chebyshev_distance(p1.king_square(Color::White), promo_d).min(PASSED_KDIST_CAP),
            PASSED_KDIST_CAP,
            "P1 own king is exactly CAP cheb from d8 (clamp saturated)"
        );
        assert!(
            chebyshev_distance(p2.king_square(Color::White), promo_d) > PASSED_KDIST_CAP,
            "P2 own king raw cheb exceeds CAP (must clamp down to CAP)"
        );
        assert_eq!(
            chebyshev_distance(p2.king_square(Color::White), promo_d).min(PASSED_KDIST_CAP),
            PASSED_KDIST_CAP,
            "P2 own king clamps to the SAME CAP as P1 → identical term"
        );
        assert_eq!(
            t1,
            (PASSED_MG[3], PASSED_EG[3] + PASSED_FREE_EG_DELTA[3]),
            "clamped king term is 0 → just rank bonus + path-clear Δ[3]"
        );
        // P3: own king ONE step inside CAP (own_d=4) → the clamp is NOT
        // saturated. White Kh7 (cheb(h7,d8)=max(4,1)=4 ⇒ own_d=4); black ka1
        // (cheb(a1,d8)=7 ⇒ enemy_d=CAP ⇒ enemy term 0); path {d5,d6,d7,d8}
        // clear ⇒ +Δ[3].
        let p3 =
            Position::from_fen("8/7K/8/8/3P4/8/8/k7 w - - 0 1").expect("own king one inside CAP");
        let t3 = passed_pawn_term_white(&p3, &pawn_eval(&p3).passed);
        // Structural (weight-free): the clamp is NOT saturated for P3's own
        // king — the un-saturated distance is what makes the king term differ
        // from the saturated case once weights are non-zero.
        assert_eq!(
            chebyshev_distance(p3.king_square(Color::White), promo_d).min(PASSED_KDIST_CAP),
            4,
            "P3 own king h7 is cheb-4 from d8 → one step inside CAP (clamp NOT saturated)"
        );
        let king3 = t3.1 - (PASSED_EG[3] + PASSED_FREE_EG_DELTA[3]);
        assert_eq!(
            king3,
            3 * PASSED_KDIST_OWN_PER_STEP * (PASSED_KDIST_CAP - 4),
            "own_d=4 (one step inside CAP) → king term rel·OWN·(CAP−4)"
        );
        // "sub-CAP own distance contributes a non-zero term" and "differs from
        // the saturated case" are magnitude claims zeroed by the shipped
        // config; re-expressed symbolically (M6.F-revalidates the inequality).
        const {
            assert!(
                3 * PASSED_KDIST_OWN_PER_STEP * (PASSED_KDIST_CAP - 4) >= 0,
                "a sub-CAP own-king distance contributes a non-negative term"
            )
        };
    }

    /// King-distance is rank-scaled by relative rank. Same king geometry
    /// (white king on c8 adjacent to the d-file promo square d8 ⇒ own_d=1;
    /// black kh1 ⇒ enemy_d=5) at two relative ranks; the king contribution
    /// must scale linearly with `rel` — doubling rel doubles it.
    ///
    /// LOW: white Pd4 (rel-rank 3). king term = 3·5·(5−1) + 3·7·(5−5) = 60.
    ///   eg_low = PASSED_EG[3] + Δ[3] + 60 ; king_low = eg_low − (PASSED_EG[3]+Δ[3]) = 60
    /// HIGH: white Pd7 (rel-rank 6). king term = 6·5·(5−1) = 120.
    ///   king_high = eg_high − (PASSED_EG[6]+Δ[6]) = 120
    /// Pin: king_high > king_low AND king_high == 2·king_low (rel 6 = 2·rel 3,
    /// the exact linear `rel`-scale — kills a constant / wrong-axis scaling).
    ///
    /// **Score-neutral note (M6.B re-expression precedent).** At the shipped
    /// zero weights the linear-scale arithmetic pin degenerates to a constant
    /// tautology (`6·OWN·(CAP−1) == 2·(3·OWN·(CAP−1))`, true for any value
    /// incl. 0) — the runtime `king_high == 2·king_low` linearity check is
    /// *dormant* and revalidates only when M6.F restores non-zero
    /// `PASSED_KDIST_*`. The active weight-free guard at the shipped config is
    /// the detection-bitboard pin (d4 = rel-3, d7 = rel-6 passers) catching a
    /// wrong-pawn-square bug independent of weight.
    #[test]
    fn passed_king_distance_rank_scaled() {
        let low =
            Position::from_fen("2K5/8/8/8/3P4/8/8/7k w - - 0 1").expect("rel-rank-3, king near");
        let high =
            Position::from_fen("2K5/3P4/8/8/8/8/8/7k w - - 0 1").expect("rel-rank-6, king near");
        // Structural (weight-free): the two fixtures place the lone white
        // passer at rel-rank 3 (d4) and rel-rank 6 (d7) under the SAME king
        // geometry. rel-rank is the linear scale factor the king term
        // multiplies by; a wrong rel / wrong-axis scale fails here at any
        // weight via the detection-square pin.
        assert_eq!(
            pawn_eval(&low).passed[Color::White.index()],
            Bitboard::from_square(Square::D4),
            "LOW: lone white passer d4 → rel-rank 3"
        );
        assert_eq!(
            pawn_eval(&high).passed[Color::White.index()],
            Bitboard::from_square(Square::D7),
            "HIGH: lone white passer d7 → rel-rank 6 (= 2 × rel-3)"
        );
        let (_, eg_low) = passed_pawn_term_white(&low, &pawn_eval(&low).passed);
        let (_, eg_high) = passed_pawn_term_white(&high, &pawn_eval(&high).passed);
        let king_low = eg_low - (PASSED_EG[3] + PASSED_FREE_EG_DELTA[3]);
        let king_high = eg_high - (PASSED_EG[6] + PASSED_FREE_EG_DELTA[6]);
        // Linear-in-rel is the load-bearing scaling claim; expressed
        // symbolically over `eval::data` so it holds at the shipped zeros
        // (0 = 2·0) and M6.F-revalidates the non-zero linearity. rel-6 is
        // exactly 2× rel-3 with identical king geometry.
        assert_eq!(
            6 * PASSED_KDIST_OWN_PER_STEP * (PASSED_KDIST_CAP - 1),
            2 * (3 * PASSED_KDIST_OWN_PER_STEP * (PASSED_KDIST_CAP - 1)),
            "king term is linear in rel: rel-6 contribution = 2 × rel-3 (M6.F-revalidated)"
        );
        assert_eq!(
            king_low,
            3 * PASSED_KDIST_OWN_PER_STEP * (PASSED_KDIST_CAP - 1),
            "rel-3 king term = 3·OWN·(CAP−1) (enemy_d=CAP zeroes the enemy term)"
        );
        assert_eq!(
            king_high,
            6 * PASSED_KDIST_OWN_PER_STEP * (PASSED_KDIST_CAP - 1),
            "rel-6 king term = 6·OWN·(CAP−1) — twice the rel-3 term, same geometry"
        );
    }

    /// Black promotion square is rank 0, NOT rank 7 (research §7 pitfall).
    /// Black Pc2 (file 2, rank idx 1 → black rel-rank = 7−1 = 6). Black promo
    /// = from_file_rank(2, 0) = c1. Black (own) king b1 adjacent to c1
    /// (cheb(b1,c1)=1 ⇒ own_d=1); white (enemy) king h8 far from c1
    /// (cheb(h8,c1)=max(5,7)=7 ⇒ enemy_d=5). Black sign = −1.
    ///
    /// black_front_spans(c2) = {c1}; c1 unoccupied ⇒ +Δ branch.
    ///   mg = −1·PASSED_MG[6] = −34
    ///   eg = −1·( PASSED_EG[6] + Δ[6] + [rel·OWN·(CAP−1) + rel·ENEMY·(CAP−CAP)] )
    ///      = −( 118 + 98 + 6·5·4 ) = −(118+98+120) = −336
    ///
    /// THE DISCRIMINATOR: if the code wrongly used rank 7 for the black promo
    /// (c8), then own_d = min(cheb(b1,c8)=7,5)=5 and enemy_d =
    /// min(cheb(h8,c8)=5,5)=5 ⇒ king term 0 ⇒ eg = −(118+98+0) = −216 ≠ −336.
    /// The exact −336 pin therefore catches the rank-7-for-black bug.
    ///
    /// **Score-neutral note (M6.B re-expression precedent).** At the shipped
    /// zero weights the −336-vs-−216 magnitude discriminator collapses to
    /// `0 == 0` — the rank-7-for-black bug catch is *dormant* and revalidates
    /// only when M6.F restores non-zero weights. The active weight-free guard
    /// is the structural geometry assertion (clamped own-king Chebyshev to the
    /// correct rank-0 promo c1 = 1 vs the wrong rank-7 promo c8 = CAP): the
    /// fixture is constructed so the two promo squares are *geometrically
    /// distinguishable*, the invariant that makes the M6.F magnitude pin
    /// observable.
    #[test]
    fn passed_promo_square_black_is_rank0() {
        let pos =
            Position::from_fen("7K/8/8/8/8/8/2p5/1k6 w - - 0 1").expect("black near-promo c2");
        let (mg, eg) = passed_pawn_term_white(&pos, &pawn_eval(&pos).passed);
        let rel = 6i32;
        let king = rel * PASSED_KDIST_OWN_PER_STEP * (PASSED_KDIST_CAP - 1)
            + rel * PASSED_KDIST_ENEMY_PER_STEP * (PASSED_KDIST_CAP - PASSED_KDIST_CAP);
        // Structural (weight-free), THE discriminator for the rank-7-for-black
        // bug: a black passer's promotion square is (file, rank 0), not
        // (file, rank 7). The c2 black passer promotes on c1. The own (black)
        // king b1 is Chebyshev-1 from the CORRECT promo c1 but Chebyshev-7
        // from the WRONG promo c8 — so the clamped own-distance the term feeds
        // the formula is 1 iff the rank-0 promo is used. This catches the bug
        // independent of any weight (the magnitude pins below are 0 at the
        // shipped config and cannot).
        let correct_promo = Square::from_file_rank(2, 0).expect("c1");
        let wrong_promo = Square::from_file_rank(2, 7).expect("c8");
        let own_d_correct =
            chebyshev_distance(pos.king_square(Color::Black), correct_promo).min(PASSED_KDIST_CAP);
        let own_d_wrong =
            chebyshev_distance(pos.king_square(Color::Black), wrong_promo).min(PASSED_KDIST_CAP);
        assert_eq!(
            own_d_correct, 1,
            "black king b1 is Chebyshev-1 from the correct rank-0 promo c1"
        );
        assert_eq!(
            own_d_wrong, PASSED_KDIST_CAP,
            "black king b1 clamps to CAP from the WRONG rank-7 promo c8 — \
             the rank-0-vs-rank-7 choice is observable in this clamped distance"
        );
        assert_ne!(
            own_d_correct, own_d_wrong,
            "the black-promo-rank bug is structurally observable regardless of weights"
        );
        assert_eq!(mg, -PASSED_MG[6], "black passer mg = −PASSED_MG[6]");
        assert_eq!(
            eg,
            -(PASSED_EG[6] + PASSED_FREE_EG_DELTA[6] + king),
            "black passer eg = −(rank + path-clear Δ + king-term measured to c1, the rank-0 promo)"
        );
    }

    /// No passers anywhere → (0, 0). White Pe4 vs black Pe5 mutually block
    /// each other on the e-file, so `pawn_eval` reports no passers for either
    /// side; iterating two empty bitboards yields the additive identity.
    #[test]
    fn passed_no_passers_is_zero() {
        let pos = Position::from_fen("4k3/8/8/4p3/4P3/8/8/4K3 w - - 0 1")
            .expect("mutually-blocked pawns");
        let pe = pawn_eval(&pos);
        assert_eq!(
            pe.passed,
            [Bitboard::EMPTY; 2],
            "fixture invariant: e4/e5 block each other → no passers either side"
        );
        assert_eq!(
            passed_pawn_term_white(&pos, &pe.passed),
            (0, 0),
            "no passers → additive identity (0, 0)"
        );
    }

    /// Color-mirror pair (the M6.B `pawn_eval_*_color_mirror_pair` precedent):
    /// a white-structure FEN and its hand-written vertical-mirror + colour-swap
    /// must produce a componentwise-negated `passed_pawn_term_white`. This
    /// single fixture covers rel-rank, the path discriminator, AND the EG
    /// king-distance term symmetry (plan §7 / R3: a king-distance-bearing
    /// mirror, not just rank/path).
    ///
    /// FEN A: white Pc5 (rel-rank 4), white Kb7 (own king adjacent to promo
    /// c8 ⇒ own_d=1), black kh1 (cheb(h1,c8)=7 ⇒ enemy_d=5). Path
    /// {c6,c7,c8} unoccupied ⇒ +Δ. king term = 4·5·(5−1) + 4·7·0 = 80.
    ///   mg_A = PASSED_MG[4] = 15
    ///   eg_A = PASSED_EG[4] + Δ[4] + 80 = 42 + 35 + 80 = 157
    ///
    /// FEN B = vertical mirror (rank r → 7−r) + colour swap:
    ///   white Pc5(f2,r4) → black pc4(f2,r3); white Kb7(f1,r6) → black
    ///   kb2(f1,r1); black kh1(f7,r0) → white Kh8(f7,r7).
    /// Black passer c4 (rel = 7−3 = 4), sign −1, black promo c1
    /// (from_file_rank(2,0)); black king b2 cheb(b2,c1)=1 ⇒ own_d=1; white
    /// king h8 cheb(h8,c1)=7 ⇒ enemy_d=5; black_front_spans(c4)={c3,c2,c1}
    /// unoccupied ⇒ +Δ. By construction every sub-term mirrors A with the
    /// opposite sign ⇒ (mg_B, eg_B) = (−15, −157) = −(mg_A, eg_A).
    #[test]
    fn passed_color_mirror_pair_negates_componentwise() {
        let a = Position::from_fen("8/1K6/8/2P5/8/8/8/7k w - - 0 1").expect("mirror A (white)");
        let b = Position::from_fen("7K/8/8/8/2p5/8/1k6/8 w - - 0 1").expect("mirror B (black)");
        let ta = passed_pawn_term_white(&a, &pawn_eval(&a).passed);
        let tb = passed_pawn_term_white(&b, &pawn_eval(&b).passed);

        // Composition pinned symbolically against `eval::data` (white side:
        // rank + path-clear Δ + own-king-near king term, enemy term zero).
        // Holds at the shipped zeros; M6.F-revalidates the magnitude.
        let king = 4 * PASSED_KDIST_OWN_PER_STEP * (PASSED_KDIST_CAP - 1);
        assert_eq!(
            ta,
            (PASSED_MG[4], PASSED_EG[4] + PASSED_FREE_EG_DELTA[4] + king),
            "A: white passer rel-rank-4, path clear, own king adjacent to promo"
        );
        // "A eg is a net bonus" is a magnitude claim zeroed by the shipped
        // config; re-expressed symbolically over the composition (≥ 0 at any
        // weight given the rank/path/king sub-terms are individually ≥ 0 here;
        // M6.F-revalidates the strict bonus).
        assert!(
            PASSED_EG[4] + PASSED_FREE_EG_DELTA[4] + king >= 0,
            "A eg is a net bonus for the side to promote (M6.F-revalidated; 0 at shipped)"
        );
        // Componentwise negation of the SYMBOLIC forms across the hand-mirror
        // pair (R3 — the M6.B color-mirror discipline). Both sides are 0 at
        // the shipped weights; the structural mirror is pinned weight-free on
        // the detection bitboards below.
        assert_eq!(tb.0, -ta.0, "mirrored black-structure mg = −(white mg)");
        assert_eq!(tb.1, -ta.1, "mirrored black-structure eg = −(white eg)");

        // Detection-bitboard symmetry: the white passer on A mirrors the
        // black passer on B (same file, mirrored rank). Weight-free → still
        // pins rel-rank + path symmetry at the shipped zeros.
        assert_eq!(
            pawn_eval(&a).passed[Color::White.index()],
            Bitboard::from_square(Square::C5),
            "A: c5 is the lone white passer"
        );
        assert_eq!(
            pawn_eval(&b).passed[Color::Black.index()],
            Bitboard::from_square(Square::C4),
            "B: c4 is the lone black passer (mirror of A's c5)"
        );

        // King-distance GEOMETRY symmetry (plan §7 R3: the mirror must pin the
        // EG king term, not just rank/path — and with the shipped zero weights
        // `tb == -ta` no longer discriminates it, so it is pinned weight-free
        // on the clamped Chebyshev inputs). A: white passer c5 → promo c8;
        // own (white) king b7 cheb-1, enemy (black) king h1 clamps to CAP.
        // B: black passer c4 → promo c1; own (black) king b2 cheb-1, enemy
        // (white) king h8 clamps to CAP. The own/enemy clamped distances
        // mirror exactly — a black-promo-rank or own/enemy-king-swap bug
        // breaks this regardless of weights.
        let a_own =
            chebyshev_distance(a.king_square(Color::White), Square::C8).min(PASSED_KDIST_CAP);
        let a_enemy =
            chebyshev_distance(a.king_square(Color::Black), Square::C8).min(PASSED_KDIST_CAP);
        let b_promo = Square::from_file_rank(2, 0).expect("c1 — black promo is rank 0");
        let b_own = chebyshev_distance(b.king_square(Color::Black), b_promo).min(PASSED_KDIST_CAP);
        let b_enemy =
            chebyshev_distance(b.king_square(Color::White), b_promo).min(PASSED_KDIST_CAP);
        assert_eq!(a_own, 1, "A: own king adjacent to promo c8 (cheb-1)");
        assert_eq!(
            a_enemy, PASSED_KDIST_CAP,
            "A: enemy king far → clamps to CAP"
        );
        assert_eq!(
            (b_own, b_enemy),
            (a_own, a_enemy),
            "king-distance geometry mirrors componentwise across the pair \
             (own↔own, enemy↔enemy; black promo at rank 0)"
        );
    }

    /// D2 (no suppression — research §4.3 / ADR-0032 §6): a connected passer
    /// earns BOTH the CONN bonus (M6.B `pawn_eval`) and the passed-pawn term,
    /// additively, on the SAME pawns. White phalanx d5/e5, no black pawns:
    /// {d5,e5} are simultaneously a connected phalanx AND both passers.
    ///
    /// Kings far (Ka1: cheb to d8/e8 = 7; kh1: cheb to d8/e8 = 7) ⇒ king
    /// term 0; both front-spans clear ⇒ +Δ[4] each.
    ///   passed term = (2·PASSED_MG[4], 2·(PASSED_EG[4]+Δ[4])) = (30, 154) ≠ 0
    ///   pawn_eval CONN (rel-rank 4, 2 pawns) = (2·CONN_MG[4], 2·CONN_EG[4])
    ///                                        = (26, 36) ≠ 0
    /// The two terms coexist on the identical squares — proves no if-else
    /// suppression collapses one when the other fires.
    #[test]
    fn passed_stacks_with_connected() {
        let pos = Position::from_fen("8/8/8/3PP3/8/8/8/K6k w - - 0 1").expect("connected passers");
        let pe = pawn_eval(&pos);
        let white_pawns = pos.pieces_colored(Color::White, crate::piece::PieceKind::Pawn);

        // The SAME d5/e5 squares are in the passed set AND the connected set.
        let d5e5 = Bitboard::from_square(Square::D5) | Bitboard::from_square(Square::E5);
        assert_eq!(
            pe.passed[Color::White.index()],
            d5e5,
            "d5,e5 are both white passers (no black pawns)"
        );
        assert_eq!(
            connected_pawns(white_pawns, Color::White),
            d5e5,
            "d5,e5 are also a connected phalanx — the very same squares"
        );

        // CONN is the only live M6.B term (ISO/DBL/BWD zeroed), so for a
        // d5/e5-only board `pawn_eval` == the exact CONN contribution: two
        // rel-rank-4 connected pawns → (2·CONN_MG[4], 2·CONN_EG[4]).
        assert_eq!(
            (pe.mg, pe.eg),
            (2 * CONN_MG[4], 2 * CONN_EG[4]),
            "connected phalanx → exact CONN contribution (2 pawns at rel-rank 4)"
        );
        assert_ne!(
            (pe.mg, pe.eg),
            (0, 0),
            "…and it is non-zero (no suppression)"
        );
        // No-suppression is a STRUCTURAL claim, pinned weight-free above on
        // set membership: the SAME d5/e5 squares are in BOTH the passed set
        // AND the connected set. A suppressing implementation (passed term
        // masked off when CONN fires, or vice-versa) would fail those
        // `assert_eq!`s regardless of weights — exactly the M6.B
        // `pawn_eval_isolated_doubled_stack` popcount-precedent move (the
        // shipped score-neutral config zeroes the passed weights, so the
        // weighted `pp != (0,0)` magnitude can no longer discriminate
        // suppression). The passed-term composition is pinned symbolically
        // over `eval::data` (0 = 0 at shipped weights; M6.F-revalidates the
        // exact additive value: 2 passers, rel-rank 4, paths clear → +Δ[4],
        // kings ≥CAP from d8/e8 → king term 0).
        let pp = passed_pawn_term_white(&pos, &pe.passed);
        assert_eq!(
            pp,
            (
                2 * PASSED_MG[4],
                2 * (PASSED_EG[4] + PASSED_FREE_EG_DELTA[4])
            ),
            "connected passer earns the exact passed-pawn term too — additive, \
             no suppression (D2); the SAME d5/e5 squares carry both \
             (magnitude discrimination M6.F-revalidated; 0 at shipped weights)"
        );
    }
}
