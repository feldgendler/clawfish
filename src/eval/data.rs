//! Vendored PeSTO middlegame evaluation data — material values + six PSTs.
//!
//! Source: PeSTO's Evaluation Function (Ronald Friederich), via Chess
//! Programming Wiki and the rofchade.nl post linked from
//! `docs/research/m3-eval-material-pst.md`. Texel-tuned; we vendor verbatim.
//!
//! This file is data, not logic. It is excluded from `cargo mutants` per
//! `.cargo/mutants.toml` (mirrors the `src/magic/constants.rs` precedent for
//! vendored magic constants). Validation lives at higher layers — the
//! published source is authoritative; future SPRT (M3.F) and the smoke games
//! in `scripts/match.sh` exercise the data end-to-end.
//!
//! Index convention for the PST arrays: PeSTO's a8-origin layout (index 0 =
//! a8, index 63 = h1). The `build_psqt` function in the parent module
//! applies the LERF ↔ PeSTO flip via `sq ^ 56` before populating `PSQT`.

// ---------------------------------------------------------------------------
// M6.B pawn-structure weight constants. Source: docs/research/m6-pawn-
// structure.md §6 (literature defaults).
//
// **Shipped config: connected-pawn term only.** ISO/DBL/BWD weights are
// **zeroed** — a per-term SPRT screen + confirmation vs `M6.A` proved each
// term individually positive-to-neutral (ISO +82.7, DBL +31.4, BWD −7.5,
// CONN +103.1) yet every multi-term subset collapses via a connectivity-axis
// double-count (ISO×CONN −197.9; all-four −99.9). CONN-only is the largest
// confirmed gain (+103.1 [+65.5, +143.1] H1) and is interaction-immune.
// ISO/DBL/BWD are kept as named tunable constants (the term math in
// `pawns::pawn_eval` still references them at zero weight, M6.F-ready);
// **M6.F re-introduces them via joint Texel** against the CONN-only baseline.
// ADR-0032 §7; docs/milestones/m6.b.md.
//
// These are data constants, not logic. They are excluded from `cargo mutants`
// per `.cargo/mutants.toml` alongside the PST arrays.
// ---------------------------------------------------------------------------

/// Isolated pawn penalty, middlegame (per pawn). Negative = penalty.
/// Zeroed in the shipped CONN-only config (M6.F re-tunes); see block comment.
pub(crate) const ISO_MG: i32 = 0;
/// Isolated pawn penalty, endgame (per pawn). Zeroed — see `ISO_MG`.
pub(crate) const ISO_EG: i32 = 0;

/// Doubled pawn penalty, middlegame (per *extra* pawn on a file).
/// Zeroed in the shipped CONN-only config (M6.F re-tunes); see block comment.
pub(crate) const DBL_MG: i32 = 0;
/// Doubled pawn penalty, endgame. Zeroed — see `DBL_MG`.
pub(crate) const DBL_EG: i32 = 0;

/// Backward pawn penalty, middlegame (CPW-simple predicate).
/// Zeroed in the shipped CONN-only config (M6.F re-tunes); see block comment.
pub(crate) const BWD_MG: i32 = 0;
/// Backward pawn penalty, endgame. Zeroed — see `BWD_MG`.
pub(crate) const BWD_EG: i32 = 0;

/// Connected pawn bonus table, middlegame. Indexed by relative rank (0..7).
/// Relative rank = `sq.rank()` for white; `7 - sq.rank()` for black.
/// Indices 0 and 1 are 0: rank-0 is the back rank (unreachable for a pawn),
/// rank-1 is the starting rank (no advancement bonus). Indices 2..7 hold the
/// literature-default rank-scaled bonuses from research §1.4 (chess ranks 3–8
/// / relative ranks 2–7). Index 7 = promotion rank, kept in the table for
/// completeness but never hit mid-search.
#[rustfmt::skip]
pub(crate) const CONN_MG: [i32; 8] = [0, 0, 3, 7, 13, 22, 40, 70];

/// Connected pawn bonus table, endgame. Same indexing as `CONN_MG`.
#[rustfmt::skip]
pub(crate) const CONN_EG: [i32; 8] = [0, 0, 5, 10, 18, 30, 55, 95];

// ---------------------------------------------------------------------------
// M6.C passed-pawn weight constants. Source: docs/research/m6-passed-pawns.md
// §8 (literature defaults — MadChess 3.0 Beta Build 103 rank+path, placeholder
// king-distance).
//
// **Shipped config: all passed-pawn weights zeroed (score-neutral landing).**
// A three-screen SPRT vs `M6.B` (env-mask subset filter, no value tuning —
// the M6.B "Subset rework" method) proved the literature-default passed-pawn
// weights have a *scale-invariant structural* mismatch with this engine, with
// no global scalar that recovers Elo:
//   - **all-three** literature default: Δ Elo **−21.74** — the 60+0.6 bucket
//     collapsed W15-L57 (KDIST slow-TC-toxic: distance-proportional, rank-
//     scaled placeholder coeffs over-help shallow ordering and reverse with
//     depth — plan §9 R2 / ADR-0032 §7 pattern).
//   - **{RANK+PATH}** (KDIST out): Δ Elo **+4.34 [−22.33, +31.07]** — flat;
//     a fast-TC W30-L59 over-magnitude vs the PeSTO EG pawn PST (R1: MadChess
//     was Texel-tuned against a different PST).
//   - **{RANK+PATH}/2** (uniform ½-co-scale, the M6.B `(ISO+CONN)/2` probe):
//     Δ Elo **−0.87 [−23.68, +21.94]** — the scale-invariant plateau
//     signature; halving did not recover Elo, it merely migrated the failure
//     to the 20+0.2 bucket. No global multiplier fixes a wrong-*shape* set.
//
// Conclusion: the failure is scale-invariant and structural, not a magnitude
// knob → a joint *reshape* (not a rescale) is required. **M6.F joint Texel
// re-introduces and reshapes the entire passed-pawn weight set** against the
// shipped (CONN-only, passed-zeroed) baseline. The constants stay named and
// referenced — `pawns::passed_pawn_term_white` exercises the full term math at
// zero weight (M6.F-ready), the M6.B ISO/DBL/BWD `pawn_eval` precedent.
// ADR-0032 §7; docs/milestones/m6.c.md.
//
// `PASSED_KDIST_CAP` is a **structural clamp, not a weight** (it bounds the
// Chebyshev distance regardless of coefficient magnitude); it is inert at zero
// coeffs but kept as the named M6.F-tunable structural constant, mirroring how
// M6.B kept its term math live.
//
// Index convention matches `CONN_*`: relative rank 0..7 (`sq.rank()` for
// white, `7 - sq.rank()` for black). Indices 0/1 are 0 (back rank / starting
// rank — unreachable for a passer). The rank-7 MG entry is 0 (the passer
// bonus is EG-dominant — research §1.2/§7 "rank-7 MG non-zero" pitfall; D6).
// The pre-zero literature defaults (M6.F starting point) were:
//   PASSED_MG            = [0, 0,  3,  8, 15, 24,  34,   0]
//   PASSED_EG            = [0, 0,  4, 18, 42, 75, 118, 170]
//   PASSED_FREE_EG_DELTA = [0, 0,  4, 16, 35, 63,  98, 141]
//   PASSED_KDIST_OWN_PER_STEP = 5   PASSED_KDIST_ENEMY_PER_STEP = 7
//
// These are data constants, not logic — excluded from `cargo mutants` per
// `.cargo/mutants.toml` alongside the PST/CONN arrays.
// ---------------------------------------------------------------------------

/// Passed pawn rank bonus, middlegame. Indexed by relative rank (0..7).
/// Zeroed in the shipped score-neutral config (M6.F re-tunes); see block
/// comment.
#[rustfmt::skip]
pub(crate) const PASSED_MG: [i32; 8] = [0, 0, 0, 0, 0, 0, 0, 0];

/// Passed pawn rank bonus, endgame. Same indexing as `PASSED_MG`.
/// Zeroed — see `PASSED_MG`.
#[rustfmt::skip]
pub(crate) const PASSED_EG: [i32; 8] = [0, 0, 0, 0, 0, 0, 0, 0];

/// EG-only path-clear delta (MadChess free-path minus base EG). `+` when the
/// front-span is empty of all pieces, `−` when an enemy piece is on it, `0`
/// when only a friendly piece is on it (three-state — research §3 / ADR-0032
/// §6). Same indexing as `PASSED_MG`. Zeroed — see block comment.
#[rustfmt::skip]
pub(crate) const PASSED_FREE_EG_DELTA: [i32; 8] = [0, 0, 0, 0, 0, 0, 0, 0];

/// EG-only king-tropism: cp per Chebyshev step the **own** king is closer to
/// the passer's promotion square (rank-scaled by relative rank). Zeroed in the
/// shipped score-neutral config (M6.F re-tunes); see block comment.
pub(crate) const PASSED_KDIST_OWN_PER_STEP: i32 = 0;

/// EG-only king-tropism: cp per Chebyshev step the **enemy** king is closer
/// to the promotion square (rank-scaled; a near enemy king is a penalty).
/// Zeroed — see `PASSED_KDIST_OWN_PER_STEP`.
pub(crate) const PASSED_KDIST_ENEMY_PER_STEP: i32 = 0;

/// Chebyshev-distance clamp for the king-tropism term: beyond 5 steps king
/// intervention is too slow to matter at shallow depth (research §2.2). A
/// **structural clamp, not a weight** — kept as the named M6.F-tunable
/// constant even though inert at the zeroed coeffs (see block comment).
pub(crate) const PASSED_KDIST_CAP: i32 = 5;

// ---------------------------------------------------------------------------
// M6.D piece-mobility weight constants. Source: Stockfish classical-HCE
// MobilityBonus (TalkChess quote — docs/research/m6-mobility.md §6;
// ADR-0003-clean, not engine source). Single co-calibrated Texel set,
// index = popcount(piece_attacks ∩ mobility_area).
//
// **Shipped config: ALL mobility weights zeroed (score-neutral landing).**
// A §11 per-kind env-mask screen ladder vs `M6.C` (single-TC 10+0.1,
// elo0=0/elo1=10, RUN ALONE) proved the literature defaults have a
// *scale-invariant structural mismatch* with this engine's PeSTO PSTs —
// no global scalar recovers Elo:
//   - all-four literature default: Δ Elo **−131.62** [−165, −100] H0.
//   - per-kind: {R} −77.60 H0 / {Q} −55.88 H0 / {N} −10.43 (≈flat,
//     CI [−36,+15]) / {B} −136.30 H0 — **no positive interaction-immune
//     subset** (contrast M6.B's clean CONN-only +103). The slider EG tables
//     (ROOK→169, QUEEN→199 ≈1.7–2 pawns) dominate the over-magnitude; the
//     bishop table triple-counts against PeSTO's bishop PST + bishop-pair.
//   - uniform ×0.5 co-scale probe (the M6.B `(ISO+CONN)/2` / M6.C
//     `{RANK+PATH}/2` discriminator): Δ Elo **−220.18** — *worsened*, not
//     rescued. A global multiplier cannot make a wrong-shaped term
//     non-negative; the failure is scale-invariant and structural.
//
// Conclusion (the M6.C disposition reproduced for mobility): the literature
// tables are mis-*shaped* for this engine, not mis-*scaled*. **M6.F joint
// Texel re-derives and reshapes the entire mobility weight set** against our
// PeSTO PSTs jointly with the M6.B ISO/DBL/BWD + CONN rescale and the M6.C
// passed-pawn reshape. The constants stay named and referenced —
// `mobility::mobility_term_white` exercises the full term math at zero weight
// (M6.F-ready), the M6.B/M6.C `*_IN_EVAL` precedent. **No separate ADR** —
// the mobility-area semantic + the score-neutral disposition are committed in
// the roadmap M6.D row + docs/milestones/m6.d.md (ADR-0032 is
// pawn-structure-scoped; mobility is a distinct concern, roadmap-committed).
//
// The pre-zero literature defaults (the M6.F starting point) were:
//   KNIGHT_MG = [-75,-56,-9,-2,6,15,22,30,36]
//   KNIGHT_EG = [-76,-54,-26,-10,5,11,26,28,29]
//   BISHOP_MG = [-48,-21,16,26,37,51,54,63,65,71,79,81,92,97]
//   BISHOP_EG = [-58,-19,-2,12,22,42,54,58,63,70,74,86,90,94]
//   ROOK_MG   = [-56,-25,-11,-5,-4,-1,8,14,21,23,31,32,43,49,59]
//   ROOK_EG   = [-78,-18,26,55,70,81,109,120,128,143,154,160,165,168,169]
//   QUEEN_MG  = [-40,-25,2,4,14,24,25,40,43,47,54,56,60,70,72,73,75,77,
//                85,94,99,108,112,113,118,119,123,128]
//   QUEEN_EG  = [-35,-12,7,19,37,55,62,76,79,87,94,102,111,116,118,122,
//                128,130,133,136,140,157,158,161,174,177,191,199]
//
// Data, not logic — excluded from cargo mutants per .cargo/mutants.toml.
#[rustfmt::skip]
pub(crate) const KNIGHT_MOBILITY_MG: [i32; 9] = [0, 0, 0, 0, 0, 0, 0, 0, 0];
#[rustfmt::skip]
pub(crate) const KNIGHT_MOBILITY_EG: [i32; 9] = [0, 0, 0, 0, 0, 0, 0, 0, 0];
#[rustfmt::skip]
pub(crate) const BISHOP_MOBILITY_MG: [i32; 14] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
#[rustfmt::skip]
pub(crate) const BISHOP_MOBILITY_EG: [i32; 14] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
#[rustfmt::skip]
pub(crate) const ROOK_MOBILITY_MG: [i32; 15] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
#[rustfmt::skip]
pub(crate) const ROOK_MOBILITY_EG: [i32; 15] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
#[rustfmt::skip]
pub(crate) const QUEEN_MOBILITY_MG: [i32; 28] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
#[rustfmt::skip]
pub(crate) const QUEEN_MOBILITY_EG: [i32; 28] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

// ---------------------------------------------------------------------------
// M6.E king-safety weight constants. Source: CPW-Engine/Fruit/Glaurung
// lineage (docs/research/m6-king-safety.md §2/§6; ADR-0003-clean — not
// engine source). Per-kind attacker-unit multipliers; 100-entry S-curve
// SafetyTable; castled-king pawn-shield bonuses (two rank tiers, three
// files); open/semi-open file MG-only penalties.
//
// **Shipped config: ALL king-safety weights zeroed (score-neutral landing).**
// Research §8 verdict: HIGH transfer risk — the single highest-risk M6 term:
//   - PeSTO's MG king PST already prices ~30–50 cp of castled-king safety
//     (the dominant double-count axis, stronger than M6.D's PST double-count).
//   - The SafetyTable S-curve saturates at 500 cp (calibrated against engines
//     with jointly-tuned PSTs; our zeroed mobility/pawn-structure baseline is
//     not that baseline).
//   - King safety is the noisiest term to SPRT (TC-dependent, opening-pool-
//     dominated variance; mixed-TC screens ~5× M6.B cost).
//   - Components are coupled by design (shield weakness feeds S-curve;
//     open-file and S-curve price the same king-danger state): no
//     interaction-immune subset (contrast M6.B's clean CONN-only +103).
// The M6.C/M6.D three-phase "co-calibrated-elsewhere ≠ transfers here" law
// (M6.B −197.94 / M6.C −21.74 / M6.D −131.62 / co-scale *worsened* −220.18)
// applies with strongest force here. **M6.E skips the diagnostic screen ladder
// entirely** (ADR-0033 §8 / plan §10) on the justified prediction that the
// screen's only outcome is "defer" at the highest screen cost of any M6 term.
// Inert landing: zeroed weights ⇒ term ≡ (0,0) ⇒ evaluate byte-identical to
// M6.D ⇒ bench 1213649 / depth-4 90591 byte-for-byte ⇒ no SPRT needed.
//
// **M6.F joint Texel obligation (extended).** Re-derives the entire king-safety
// weight set (S-curve table shape + per-kind attacker multipliers + shield +
// open-file + MG/EG split) *jointly* with M6.B ISO/DBL/BWD + CONN rescale,
// M6.C passed-pawn reshape, and M6.D mobility reshape — one joint pass. The
// §7 double-count axes (pawn-shield × PeSTO king MG PST — dominant; attack
// S-curve × king PST — low-moderate; CONN × shield — low) are the load-
// bearing inputs to that pass. ADR-0033 §7/§8.
//
// **Literature defaults (the M6.F starting point, recorded verbatim):**
//
// Per-kind attacker multipliers (CPW-Engine form: units = weight × attacked
// king-zone squares, summed to SAFETY_TABLE index). Source: CPW-Engine eval
// (Glaurung/Fruit lineage, the one complete citable set outside engine source
// repos — ADR-0003-clean):
//   KING_ATTACK_WEIGHT_N = 2   KING_ATTACK_WEIGHT_B = 2
//   KING_ATTACK_WEIGHT_R = 3   KING_ATTACK_WEIGHT_Q = 4
//
// SAFETY_TABLE (100-entry CPW-Engine S-curve, Glaurung 1.2 lineage verbatim;
// same values reproduced on CPW King Safety page and CPW-Engine eval page;
// S-curve: slow rise 0–10, steeper 10–60, flat 500 from index 61+):
//   [  0,  0,  1,  2,  3,  5,  7,  9, 12, 15,
//     18, 22, 26, 30, 35, 39, 44, 50, 56, 62,
//     68, 75, 82, 85, 89, 97,105,113,122,131,
//    140,150,169,180,191,202,213,225,237,248,
//    260,272,283,295,307,319,330,342,354,366,
//    377,389,401,412,424,436,448,459,471,483,
//    494,500,500,500,500,500,500,500,500,500,
//    500,500,500,500,500,500,500,500,500,500,
//    500,500,500,500,500,500,500,500,500,500,
//    500,500,500,500,500,500,500,500,500,500]
//
// Gating (CPW-Engine): attack_units = 0 if < 2 distinct attackers OR enemy
// has no queen (conservative at M6.E; M6.F may relax). Index clamped to
// min(units, 99) — the saturating tail is by design (S-curve flat at 500).
// Attack penalty MG-only (EG = 0; king walks in EG, fights PeSTO EG king PST
// centralization incentive — ADR-0033 §2/§5; research §5).
//
// Pawn-shield defaults (center of literature range; two-tier: SHIELD_1 >
// SHIELD_2, unmoved rank-2 pawn strictly better than rank-3):
//   SHIELD_1_MG = 10   SHIELD_1_EG = 5   (rank-2 pawn)
//   SHIELD_2_MG =  5   SHIELD_2_EG = 3   (rank-3 pawn)
//
// Open/semi-open file defaults (MG-only; research §4):
//   KS_KFILE_SEMI_OPEN_MG = -15   KS_KFILE_OPEN_MG = -20
//   KS_ADJ_SEMI_OPEN_MG  = -10   (per adjacent file)
//   KS_BOTH_ADJ_SEMI_OPEN_MG = -10  (extra when both adj semi-open)
//
// These are data constants, not logic — excluded from `cargo mutants` per
// `.cargo/mutants.toml` alongside the PST/mobility arrays.
// ---------------------------------------------------------------------------

/// King-zone attacker-unit multiplier, knight. Zeroed (score-neutral, M6.F re-tunes).
pub(crate) const KING_ATTACK_WEIGHT_N: i32 = 0; // lit 2
/// King-zone attacker-unit multiplier, bishop. Zeroed — see `KING_ATTACK_WEIGHT_N`.
pub(crate) const KING_ATTACK_WEIGHT_B: i32 = 0; // lit 2
/// King-zone attacker-unit multiplier, rook. Zeroed — see `KING_ATTACK_WEIGHT_N`.
pub(crate) const KING_ATTACK_WEIGHT_R: i32 = 0; // lit 3
/// King-zone attacker-unit multiplier, queen. Zeroed — see `KING_ATTACK_WEIGHT_N`.
pub(crate) const KING_ATTACK_WEIGHT_Q: i32 = 0; // lit 4

/// CPW-Engine 100-entry S-curve SafetyTable. Indexed by accumulated attack
/// units; clamped to [0, 99]. Zeroed — see block comment (M6.F re-derives).
/// The literature defaults are recorded verbatim in the block comment above.
#[rustfmt::skip]
pub(crate) const KING_SAFETY_TABLE: [i32; 100] = [0; 100];

/// Pawn-shield bonus: friendly pawn on king's 2nd rank, middlegame. Zeroed.
pub(crate) const SHIELD_1_MG: i32 = 0; // lit  10
/// Pawn-shield bonus: friendly pawn on king's 2nd rank, endgame. Zeroed.
pub(crate) const SHIELD_1_EG: i32 = 0; // lit   5
/// Pawn-shield bonus: friendly pawn on king's 3rd rank, middlegame. Zeroed.
pub(crate) const SHIELD_2_MG: i32 = 0; // lit   5
/// Pawn-shield bonus: friendly pawn on king's 3rd rank, endgame. Zeroed.
pub(crate) const SHIELD_2_EG: i32 = 0; // lit   3

/// MG penalty: king file is semi-open (no own pawn, enemy pawn present). Zeroed.
pub(crate) const KS_KFILE_SEMI_OPEN_MG: i32 = 0; // lit -15
/// MG penalty: king file is fully open (no pawn of either color). Zeroed.
pub(crate) const KS_KFILE_OPEN_MG: i32 = 0; // lit -20
/// MG penalty per adjacent file that is semi-open (no own pawn). Zeroed.
pub(crate) const KS_ADJ_SEMI_OPEN_MG: i32 = 0; // lit -10
/// MG amplification: both adjacent files semi-open (no-cover threshold). Zeroed.
pub(crate) const KS_BOTH_ADJ_SEMI_OPEN_MG: i32 = 0; // lit -10

// ---------------------------------------------------------------------------
// M6.F Tier-1 HCE feature constants. Sources: CPW Outposts, CPW Rook on Open
// File, CPW Bishops of Opposite Colors, MadChess dev-blog Elo figures (Knight
// Outpost +25, Endgame Eval Scaling +12). ADR-0003-clean — not engine source.
//
// **Shipped config: ALL Tier-1 additive weights ZEROED; ALL endgame-scale
// tunables = EG_SCALE_DEN (identity).** Score-neutral inert landing per the
// M6.C/M6.D/M6.E precedent (ADR-0034 §2); the full term math is live and
// M6.H-ready; M6.H joint Texel re-derives the entire weight/coefficient set.
//
// Inert-identity construction:
//   - Additive terms: weight = 0 ⇒ term ≡ (0, 0).
//   - Multiplicative scale: every tunable = EG_SCALE_DEN ⇒ the min/taper
//     returns EG_SCALE_DEN for all inputs ⇒ blended * D / D == blended exactly
//     in i32. ⇒ evaluate byte-identical to M6.E ⇒ bench 1213649 / depth-4
//     90591 byte-for-byte. ADR-0034 §2.
//
// **Literature defaults (M6.H starting point, recorded verbatim):**
//
// Outpost per-kind per-relative-rank MG/EG bonuses. Relative rank 0..7; only
// ranks 3..=5 (chess ranks 4–6) are scored (the classic outpost band). Values
// from CPW Outposts page + MadChess dev-blog, adjusted to centipawn scale:
//   Knight outpost: ~[0,0,0, 18, 28, 16, 0, 0] MG / ~[0,0,0, 12, 18, 10, 0, 0] EG
//   Bishop outpost: ~[0,0,0, 12, 18, 10, 0, 0] MG / ~[0,0,0,  8, 12,  6, 0, 0] EG
//
// Rook on open / semi-open file (CPW Rook on Open File / Crazyhouse eval
// survey; common literature range MG 15–25, EG 5–15):
//   ROOK_OPEN_FILE_MG = 20    ROOK_OPEN_FILE_EG = 10
//   ROOK_SEMI_OPEN_FILE_MG = 10   ROOK_SEMI_OPEN_FILE_EG = 5
//
// Endgame draw-scale (CPW Bishops of Opposite Colors, MadChess +12 Elo;
// literature draw-probability fraction: OCB ~0.5, pawnless ~0.125):
//   OCB_WITH_PAWNS_SCALE  ≈ 32  (0.5 × DEN)
//   PAWNLESS_DRAW_SCALE   ≈  8  (0.125 × DEN)
//   FIFTY_MOVE_FLOOR      ≈ 16  (0.25 × DEN — scale at halfmove 100)
//   FIFTY_MOVE_TAPER_FROM = 80  structural onset (not tuned)
//
// EG_SCALE_DEN = 64: a STRUCTURAL fixed-point denominator (NOT a tunable —
// the PASSED_KDIST_CAP structural-constant precedent). Keeps the per-position
// scale a known per-position constant that folds into the M6.H cached feature
// mask, preserving optimizer linearity (ADR-0034 §4 / §8).
// FIFTY_MOVE_TAPER_FROM = 80: structural onset (not tuned — it is the
// semantically correct onset for 50-move proximity; tuning it would change the
// meaning of the taper, not just its magnitude).
//
// These are data constants, not logic — excluded from `cargo mutants` per
// `.cargo/mutants.toml` alongside the PST/mobility/king-safety arrays.
// ---------------------------------------------------------------------------

// Outpost — per-kind, per-relative-rank MG/EG (rel-rank 0..7; predicate gates
// rel-rank ∈ 3..=5 i.e. chess ranks 4–6 so out-of-band entries never index).
/// Knight outpost bonus, middlegame, indexed by relative rank (0..7). Zeroed
/// (M6.H re-derives); literature defaults ~[0,0,0,18,28,16,0,0].
// Used from tier1.rs; #[allow] covers the stub-slice gap until impl is wired.
#[allow(dead_code)]
pub(crate) const OUTPOST_KNIGHT_MG: [i32; 8] = [0; 8]; // lit ~[0,0,0,18,28,16,0,0]
/// Knight outpost bonus, endgame. Zeroed — see `OUTPOST_KNIGHT_MG`.
#[allow(dead_code)]
pub(crate) const OUTPOST_KNIGHT_EG: [i32; 8] = [0; 8]; // lit ~[0,0,0,12,18,10,0,0]
/// Bishop outpost bonus, middlegame. Zeroed — see `OUTPOST_KNIGHT_MG`.
#[allow(dead_code)]
pub(crate) const OUTPOST_BISHOP_MG: [i32; 8] = [0; 8]; // lit ~[0,0,0,12,18,10,0,0]
/// Bishop outpost bonus, endgame. Zeroed — see `OUTPOST_KNIGHT_MG`.
#[allow(dead_code)]
pub(crate) const OUTPOST_BISHOP_EG: [i32; 8] = [0; 8]; // lit ~[0,0,0, 8,12, 6,0,0]

/// Rook on fully open file (no pawn either color) bonus, middlegame. Zeroed.
#[allow(dead_code)]
pub(crate) const ROOK_OPEN_FILE_MG: i32 = 0; // lit  20
/// Rook on fully open file bonus, endgame. Zeroed — see `ROOK_OPEN_FILE_MG`.
#[allow(dead_code)]
pub(crate) const ROOK_OPEN_FILE_EG: i32 = 0; // lit  10
/// Rook on semi-open file (own pawn absent, enemy present) bonus, middlegame. Zeroed.
#[allow(dead_code)]
pub(crate) const ROOK_SEMI_OPEN_FILE_MG: i32 = 0; // lit  10
/// Rook on semi-open file bonus, endgame. Zeroed — see `ROOK_SEMI_OPEN_FILE_MG`.
#[allow(dead_code)]
pub(crate) const ROOK_SEMI_OPEN_FILE_EG: i32 = 0; // lit   5

/// Endgame draw-scale fixed-point denominator. STRUCTURAL — not a tunable;
/// the per-position scale = numerator / EG_SCALE_DEN ∈ [0, 1].
/// Every scale tunable at ship = EG_SCALE_DEN ⇒ scale ≡ identity.
#[allow(dead_code)]
pub(crate) const EG_SCALE_DEN: i32 = 64; // structural (not tuned)
/// OCB-with-pawns draw-scale numerator. M6.H tunable; identity value =
/// EG_SCALE_DEN. Shipped = EG_SCALE_DEN (identity); literature ~32 (0.5×DEN).
#[allow(dead_code)]
pub(crate) const OCB_WITH_PAWNS_SCALE: i32 = 64; // = DEN ⇒ identity; lit ~32 (0.5)
/// Pawnless drawish endgame scale numerator. M6.H tunable; identity =
/// EG_SCALE_DEN. Shipped = EG_SCALE_DEN (identity); literature ~8 (0.125×DEN).
#[allow(dead_code)]
pub(crate) const PAWNLESS_DRAW_SCALE: i32 = 64; // = DEN ⇒ identity; lit ~8
/// 50-move proximity taper onset in halfmoves. STRUCTURAL — not a tunable
/// (semantically correct onset; tuning changes meaning, not magnitude).
#[allow(dead_code)]
pub(crate) const FIFTY_MOVE_TAPER_FROM: i32 = 80; // structural onset (halfmoves)
/// 50-move taper floor: scale value at halfmove 100. M6.H tunable; identity =
/// EG_SCALE_DEN. Shipped = EG_SCALE_DEN (identity, ramp flat); lit ~16 (0.25).
#[allow(dead_code)]
pub(crate) const FIFTY_MOVE_FLOOR: i32 = 64; // = DEN ⇒ identity; lit ~16

/// Centipawn material values indexed by `PieceKind::index()` (P=0 … K=5).
/// King material is 0 — kings are never captured; the king PST term is
/// purely positional.
pub(crate) const MATERIAL: [i32; 6] = [82, 337, 365, 477, 1025, 0];

/// PeSTO MG pawn PST.
#[rustfmt::skip]
pub(super) const MG_PAWN_PST: [i32; 64] = [
      0,   0,   0,   0,   0,   0,   0,   0,
     98, 134,  61,  95,  68, 126,  34, -11,
     -6,   7,  26,  31,  65,  56,  25, -20,
    -14,  13,   6,  21,  23,  12,  17, -23,
    -27,  -2,  -5,  12,  17,   6,  10, -25,
    -26,  -4,  -4, -10,   3,   3,  33, -12,
    -35,  -1, -20, -23, -15,  24,  38, -22,
      0,   0,   0,   0,   0,   0,   0,   0,
];

/// PeSTO MG knight PST.
#[rustfmt::skip]
pub(super) const MG_KNIGHT_PST: [i32; 64] = [
    -167, -89, -34, -49,  61, -97, -15, -107,
     -73, -41,  72,  36,  23,  62,   7,  -17,
     -47,  60,  37,  65,  84, 129,  73,   44,
      -9,  17,  19,  53,  37,  69,  18,   22,
     -13,   4,  16,  13,  28,  19,  21,   -8,
     -23,  -9,  12,  10,  19,  17,  25,  -16,
     -29, -53, -12,  -3,  -1,  18, -14,  -19,
    -105, -21, -58, -33, -17, -28, -19,  -23,
];

/// PeSTO MG bishop PST.
#[rustfmt::skip]
pub(super) const MG_BISHOP_PST: [i32; 64] = [
    -29,   4, -82, -37, -25, -42,   7,  -8,
    -26,  16, -18, -13,  30,  59,  18, -47,
    -16,  37,  43,  40,  35,  50,  37,  -2,
     -4,   5,  19,  50,  37,  37,   7,  -2,
     -6,  13,  13,  26,  34,  12,  10,   4,
      0,  15,  15,  15,  14,  27,  18,  10,
      4,  15,  16,   0,   7,  21,  33,   1,
    -33,  -3, -14, -21, -13, -12, -39, -21,
];

/// PeSTO MG rook PST.
#[rustfmt::skip]
pub(super) const MG_ROOK_PST: [i32; 64] = [
     32,  42,  32,  51,  63,   9,  31,  43,
     27,  32,  58,  62,  80,  67,  26,  44,
     -5,  19,  26,  36,  17,  45,  61,  16,
    -24, -11,   7,  26,  24,  35,  -8, -20,
    -36, -26, -12,  -1,   9,  -7,   6, -23,
    -45, -25, -16, -17,   3,   0,  -5, -33,
    -44, -16, -20,  -9,  -1,  11,  -6, -71,
    -19, -13,   1,  17,  16,   7, -37, -26,
];

/// PeSTO MG queen PST.
#[rustfmt::skip]
pub(super) const MG_QUEEN_PST: [i32; 64] = [
    -28,   0,  29,  12,  59,  44,  43,  45,
    -24, -39,  -5,   1, -16,  57,  28,  54,
    -13, -17,   7,   8,  29,  56,  47,  57,
    -27, -27, -16, -16,  -1,  17,  -2,   1,
     -9, -26,  -9, -10,  -2,  -4,   3,  -3,
    -14,   2, -11,  -2,  -5,   2,  14,   5,
    -35,  -8,  11,   2,   8,  15,  -3,   1,
     -1, -18,  -9,  10, -15, -25, -31, -50,
];

/// PeSTO MG king PST. Non-zero despite king material = 0 — the PST captures
/// middlegame king-safety preferences (tucked king > centralized king).
#[rustfmt::skip]
pub(super) const MG_KING_PST: [i32; 64] = [
    -65,  23,  16, -15, -56, -34,   2,  13,
     29,  -1, -20,  -7,  -8,  -4, -38, -29,
     -9,  24,   2, -16, -20,   6,  22, -22,
    -17, -20, -12, -27, -30, -25, -14, -36,
    -49,  -1, -27, -39, -46, -44, -33, -51,
    -14, -14, -22, -46, -44, -30, -15, -27,
      1,   7,  -8, -64, -43, -16,   9,   8,
    -15,  36,  12, -54,   8, -28,  24,  14,
];

// ---------------------------------------------------------------------------
// PeSTO endgame data — EG material values + six EG PSTs.
//
// Same a8-origin indexing convention as the MG arrays above. The
// `build_psqt` function in the parent module applies the LERF ↔ PeSTO
// flip via `sq ^ 56` for white-piece lookups.
// ---------------------------------------------------------------------------

/// Endgame centipawn material values indexed by `PieceKind::index()` (P=0 … K=5).
/// King EG material is 0 (kings are never captured; PST captures centrality).
pub(crate) const MATERIAL_EG: [i32; 6] = [94, 281, 297, 512, 936, 0];

/// Per-piece phase contribution. Sum at full material = 24.
/// King contributes 0 (always present; non-zero would add a constant offset
/// to every phase computation without discriminating phase).
pub(crate) const PHASE_DELTA: [u8; 6] = [0, 1, 1, 2, 4, 0];

/// PeSTO EG pawn PST.
#[rustfmt::skip]
pub(super) const EG_PAWN_PST: [i32; 64] = [
      0,   0,   0,   0,   0,   0,   0,   0,
    178, 173, 158, 134, 147, 132, 165, 187,
     94, 100,  85,  67,  56,  53,  82,  84,
     32,  24,  13,   5,  -2,   4,  17,  17,
     13,   9,  -3,  -7,  -7,  -8,   3,  -1,
      4,   7,  -6,   1,   0,  -5,  -1,  -8,
     13,   8,   8,  10,  13,   0,   2,  -7,
      0,   0,   0,   0,   0,   0,   0,   0,
];

/// PeSTO EG knight PST.
#[rustfmt::skip]
pub(super) const EG_KNIGHT_PST: [i32; 64] = [
    -58, -38, -13, -28, -31, -27, -63, -99,
    -25,  -8, -25,  -2,  -9, -25, -24, -52,
    -24, -20,  10,   9,  -1,  -9, -19, -41,
    -17,   3,  22,  22,  22,  11,   8, -18,
    -18,  -6,  16,  25,  16,  17,   4, -18,
    -23,  -3,  -1,  15,  10,  -3, -20, -22,
    -42, -20, -10,  -5,  -2, -20, -23, -44,
    -29, -51, -23, -15, -22, -18, -50, -64,
];

/// PeSTO EG bishop PST.
#[rustfmt::skip]
pub(super) const EG_BISHOP_PST: [i32; 64] = [
    -14, -21, -11,  -8,  -7,  -9, -17, -24,
     -8,  -4,   7, -12,  -3, -13,  -4, -14,
      2,  -8,   0,  -1,  -2,   6,   0,   4,
     -3,   9,  12,   9,  14,  10,   3,   2,
     -6,   3,  13,  19,   7,  10,  -3,  -9,
    -12,  -3,   8,  10,  13,   3,  -7, -15,
    -14, -18,  -7,  -1,   4,  -9, -15, -27,
    -23,  -9, -23,  -5,  -9, -16,  -5, -17,
];

/// PeSTO EG rook PST.
#[rustfmt::skip]
pub(super) const EG_ROOK_PST: [i32; 64] = [
    13,  10,  18,  15,  12,  12,   8,   5,
    11,  13,  13,  11,  -3,   3,   8,   3,
     7,   7,   7,   5,   4,  -3,  -5,  -3,
     4,   3,  13,   1,   2,   1,  -1,   2,
     3,   5,   8,   4,  -5,  -6,  -8, -11,
    -4,   0,  -5,  -1,  -7, -12,  -8, -16,
    -6,  -6,   0,   2,  -9,  -9, -11,  -3,
    -9,   2,   3,  -1,  -5, -13,   4, -20,
];

/// PeSTO EG queen PST.
#[rustfmt::skip]
pub(super) const EG_QUEEN_PST: [i32; 64] = [
     -9,  22,  22,  27,  27,  19,  10,  20,
    -17,  20,  32,  41,  58,  25,  30,   0,
    -20,   6,   9,  49,  47,  35,  19,   9,
      3,  22,  24,  45,  57,  40,  57,  36,
    -18,  28,  19,  47,  31,  34,  39,  23,
    -16, -27,  15,   6,   9,  17,  10,   5,
    -22, -23, -30, -16, -16, -23, -36, -32,
    -33, -28, -22, -43,  -5, -32, -20, -41,
];

/// PeSTO EG king PST. Large positive central values reflect the endgame
/// king's role as an active piece — opposite of the MG king PST.
#[rustfmt::skip]
pub(super) const EG_KING_PST: [i32; 64] = [
    -74, -35, -18, -18, -11,  15,   4, -17,
    -12,  17,  14,  17,  17,  38,  23,  11,
     10,  17,  23,  15,  20,  45,  44,  13,
     -8,  22,  24,  27,  26,  33,  26,   3,
    -18,  -4,  21,  24,  27,  23,   9, -11,
    -19,  -3,  11,  21,  23,  16,   7,  -9,
    -27, -11,   4,  13,  14,   4,  -5, -17,
    -53, -34, -21, -11, -28, -14, -24, -43,
];
