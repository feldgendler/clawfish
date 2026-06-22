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
// **LIVE since M6.I — all of ISO/DBL/BWD/CONN are active, joint-Texel-tuned
// weights; the values below are the M6.J meta-tune output (`texel-tune
// apply`). They are NOT zeroed.** Some Texel-fit shapes are counterintuitive
// (e.g. ISO_MG positive) — a sign/monotonicity-constrained retune is
// backlogged (CLAUDE.md M6).
//
// History (why they once landed zeroed): at M6.B only the connected-pawn term
// shipped — a per-term SPRT screen vs `M6.A` proved each term individually
// positive-to-neutral (ISO +82.7, DBL +31.4, BWD −7.5, CONN +103.1) yet every
// multi-term subset collapsed via a connectivity-axis double-count (ISO×CONN
// −197.9; all-four −99.9), so ISO/DBL/BWD landed zeroed (kept as named
// constants, term math live at zero weight) pending a joint re-tune. M6.I's
// joint Texel (SPRT +93.86 vs `M6.F`) reshaped and activated the full set;
// M6.J retuned to the current values. ADR-0032 §7;
// docs/milestones/{m6.b,m6.i,m6.j}.md.
//
// These are data constants, not logic. They are excluded from `cargo mutants`
// per `.cargo/mutants.toml` alongside the PST arrays.
// ---------------------------------------------------------------------------

// TEXEL-TUNABLE-BEGIN: iso_dbl_bwd
/// Isolated pawn penalty, middlegame (per pawn). Live (M6.J-tuned).
pub(crate) const ISO_MG: i32 = 4;
/// Isolated pawn penalty, endgame (per pawn). Live — see `ISO_MG`.
pub(crate) const ISO_EG: i32 = -1;

/// Doubled pawn penalty, middlegame (per *extra* pawn on a file). Live.
pub(crate) const DBL_MG: i32 = -23;
/// Doubled pawn penalty, endgame. Live — see `DBL_MG`.
pub(crate) const DBL_EG: i32 = -14;

/// Backward pawn penalty, middlegame (CPW-simple predicate). Live.
pub(crate) const BWD_MG: i32 = -7;
/// Backward pawn penalty, endgame. Live — see `BWD_MG`.
pub(crate) const BWD_EG: i32 = -7;
// TEXEL-TUNABLE-END: iso_dbl_bwd

// TEXEL-TUNABLE-BEGIN: conn
/// Connected pawn bonus table, middlegame. Indexed by relative rank (0..7).
/// Relative rank = `sq.rank()` for white; `7 - sq.rank()` for black.
/// Indices 0 and 1 are 0: rank-0 is the back rank (unreachable for a pawn),
/// rank-1 is the starting rank (no advancement bonus). Indices 2..7 hold the
/// literature-default rank-scaled bonuses from research §1.4 (chess ranks 3–8
/// / relative ranks 2–7). Index 7 = promotion rank, kept in the table for
/// completeness but never hit mid-search.
#[rustfmt::skip]
pub(crate) const CONN_MG: [i32; 8] = [0, 0, 3, 7, 7, 32, 37, 0];

/// Connected pawn bonus table, endgame. Same indexing as `CONN_MG`.
#[rustfmt::skip]
pub(crate) const CONN_EG: [i32; 8] = [0, 0, 11, 9, 15, 27, 33, 0];
// TEXEL-TUNABLE-END: conn

// ---------------------------------------------------------------------------
// M6.C passed-pawn weight constants. Source: docs/research/m6-passed-pawns.md
// §8 (literature defaults — MadChess 3.0 Beta Build 103 rank+path, placeholder
// king-distance).
//
// **LIVE since M6.I — the full passed-pawn weight set is active and
// joint-Texel-reshaped; the values below are the M6.J meta-tune output. They
// are NOT zeroed.** (Note the negative early-rank PASSED_MG entries — a
// counterintuitive but SPRT-validated Texel shape; sign/monotonicity-
// constrained retune backlogged.)
//
// History (why they once landed zeroed): a three-screen SPRT vs `M6.B`
// (env-mask subset filter, no value tuning — the M6.B "Subset rework" method)
// proved the *literature-default* passed-pawn weights had a *scale-invariant
// structural* mismatch with this engine, with no global scalar that recovered
// Elo:
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
// The failure was scale-invariant and structural, not a magnitude knob → a
// joint *reshape* (not a rescale) was required. That reshape landed: M6.I's
// joint Texel re-derived and activated the entire passed-pawn weight set, and
// M6.J retuned to the current values. ADR-0032 §7; docs/milestones/m6.c.md +
// m6.i.md.
//
// `PASSED_KDIST_CAP` is a **structural clamp, not a weight** (it bounds the
// Chebyshev distance regardless of coefficient magnitude); the king-tropism
// coefficients it clamps are now live (non-zero), so the clamp is active.
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

// TEXEL-TUNABLE-BEGIN: passed
/// Passed pawn rank bonus, middlegame. Indexed by relative rank (0..7).
/// Live (M6.J-tuned); see block comment.
#[rustfmt::skip]
pub(crate) const PASSED_MG: [i32; 8] = [0, 0, -28, -14, 23, 36, 40, 0];

/// Passed pawn rank bonus, endgame. Same indexing as `PASSED_MG`.
/// Live — see `PASSED_MG`.
#[rustfmt::skip]
pub(crate) const PASSED_EG: [i32; 8] = [0, 0, 15, 37, 38, 35, 5, 0];

/// EG-only path-clear delta (MadChess free-path minus base EG). `+` when the
/// front-span is empty of all pieces, `−` when an enemy piece is on it, `0`
/// when only a friendly piece is on it (three-state — research §3 / ADR-0032
/// §6). Same indexing as `PASSED_MG`. Live — see block comment.
#[rustfmt::skip]
pub(crate) const PASSED_FREE_EG_DELTA: [i32; 8] = [0, 0, -1, 12, 22, 28, 35, 0];

/// EG-only king-tropism: cp per Chebyshev step the **own** king is closer to
/// the passer's promotion square (rank-scaled by relative rank). Live
/// (M6.J-tuned); see block comment.
pub(crate) const PASSED_KDIST_OWN_PER_STEP: i32 = 6;

/// EG-only king-tropism: cp per Chebyshev step the **enemy** king is closer
/// to the promotion square (rank-scaled; a near enemy king is a penalty).
/// Live — see `PASSED_KDIST_OWN_PER_STEP`.
pub(crate) const PASSED_KDIST_ENEMY_PER_STEP: i32 = 3;
// TEXEL-TUNABLE-END: passed

/// Chebyshev-distance clamp for the king-tropism term: beyond 5 steps king
/// intervention is too slow to matter at shallow depth (research §2.2). A
/// **structural clamp, not a weight** — kept as a named tunable structural
/// constant; active now that the king-tropism coefficients are live.
pub(crate) const PASSED_KDIST_CAP: i32 = 5;

// ---------------------------------------------------------------------------
// M6.D piece-mobility weight constants. Source: Stockfish classical-HCE
// MobilityBonus (TalkChess quote — docs/research/m6-mobility.md §6;
// ADR-0003-clean, not engine source). Single co-calibrated Texel set,
// index = popcount(piece_attacks ∩ mobility_area).
//
// **LIVE since M6.I — all four mobility tables are active, joint-Texel-
// reshaped weights; the values below are the M6.J meta-tune output. They are
// NOT zeroed.**
//
// History (why they once landed zeroed): a §11 per-kind env-mask screen ladder
// vs `M6.C` (single-TC 10+0.1, elo0=0/elo1=10, RUN ALONE) proved the
// *literature defaults* had a *scale-invariant structural mismatch* with this
// engine's PeSTO PSTs — no global scalar recovered Elo:
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
// The literature tables were mis-*shaped* for this engine, not mis-*scaled*,
// so a joint *reshape* (not a rescale) was required. That reshape landed:
// M6.I's joint Texel re-derived and activated the entire mobility weight set
// against our PeSTO PSTs (jointly with the pawn-structure and passed-pawn
// terms), and M6.J retuned to the current values. **No separate ADR** — the
// mobility-area semantic is committed in the roadmap M6.D row +
// docs/milestones/m6.d.md + m6.i.md (ADR-0032 is pawn-structure-scoped;
// mobility is a distinct concern, roadmap-committed).
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
// TEXEL-TUNABLE-BEGIN: mobility
#[rustfmt::skip]
pub(crate) const KNIGHT_MOBILITY_MG: [i32; 9] = [-28, -10, 1, 5, 10, 13, 17, 24, 30];
#[rustfmt::skip]
pub(crate) const KNIGHT_MOBILITY_EG: [i32; 9] = [-35, -27, -11, 4, 14, 24, 21, 11, -10];
#[rustfmt::skip]
pub(crate) const BISHOP_MOBILITY_MG: [i32; 14] = [-30, -19, -9, -3, 3, 5, 9, 11, 14, 18, 22, 30, 21, 21];
#[rustfmt::skip]
pub(crate) const BISHOP_MOBILITY_EG: [i32; 14] = [-38, -31, -25, -14, 3, 17, 22, 24, 26, 25, 23, 19, 23, 18];
#[rustfmt::skip]
pub(crate) const ROOK_MOBILITY_MG: [i32; 15] = [-33, -18, -13, -5, -4, 4, 8, 15, 20, 24, 26, 28, 28, 29, 47];
#[rustfmt::skip]
pub(crate) const ROOK_MOBILITY_EG: [i32; 15] = [-36, -29, -25, -21, -6, 2, 7, 11, 17, 22, 25, 25, 26, 23, -15];
#[rustfmt::skip]
pub(crate) const QUEEN_MOBILITY_MG: [i32; 28] = [-8, -8, -13, -13, -12, -12, -11, -6, -2, 2, 6, 10, 17, 20, 23, 28, 29, 30, 35, 37, 38, 37, 34, 32, 21, 14, 2, -3];
#[rustfmt::skip]
pub(crate) const QUEEN_MOBILITY_EG: [i32; 28] = [-28, -34, -34, -33, -31, -24, -8, -3, 11, 20, 26, 28, 29, 33, 35, 37, 39, 40, 40, 40, 40, 37, 35, 21, 15, 3, 1, -12];
// TEXEL-TUNABLE-END: mobility

// ---------------------------------------------------------------------------
// M6.E king-safety weight constants. Source: CPW-Engine/Fruit/Glaurung
// lineage (docs/research/m6-king-safety.md §2/§6; ADR-0003-clean — not
// engine source). Castled-king pawn-shield bonuses (two rank tiers, three
// files); open/semi-open file MG-only penalties. The attacker S-curve was
// removed after both the g=1 and g=0.5 SPRT probes regressed vs `M6.J`
// (M6.K; ADR-0038). Shield + open-file terms were Texel-tuned at M6.I
// (ADR-0037).
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

// TEXEL-TUNABLE-BEGIN: king_safety_shield_file
/// Pawn-shield bonus: friendly pawn on king's 2nd rank, middlegame. Live (M6.I).
pub(crate) const SHIELD_1_MG: i32 = 7; // lit  10
/// Pawn-shield bonus: friendly pawn on king's 2nd rank, endgame. Live (M6.I).
pub(crate) const SHIELD_1_EG: i32 = 1; // lit   5
/// Pawn-shield bonus: friendly pawn on king's 3rd rank, middlegame. Live (M6.I).
pub(crate) const SHIELD_2_MG: i32 = 4; // lit   5
/// Pawn-shield bonus: friendly pawn on king's 3rd rank, endgame. Live (M6.I).
pub(crate) const SHIELD_2_EG: i32 = -7; // lit   3

/// MG penalty: king file is semi-open (no own pawn, enemy pawn present). Live.
pub(crate) const KS_KFILE_SEMI_OPEN_MG: i32 = -20; // lit -15
/// MG penalty: king file is fully open (no pawn of either color). Live.
pub(crate) const KS_KFILE_OPEN_MG: i32 = -42; // lit -20
/// MG penalty per adjacent file that is semi-open (no own pawn). Live.
pub(crate) const KS_ADJ_SEMI_OPEN_MG: i32 = -15; // lit -10
/// MG amplification: both adjacent files semi-open (no-cover threshold). Live.
pub(crate) const KS_BOTH_ADJ_SEMI_OPEN_MG: i32 = -23; // lit -10
// TEXEL-TUNABLE-END: king_safety_shield_file

// ---------------------------------------------------------------------------
// M6.F Tier-1 HCE feature constants. Sources: CPW Outposts, CPW Rook on Open
// File, CPW Bishops of Opposite Colors, MadChess dev-blog Elo figures (Knight
// Outpost +25, Endgame Eval Scaling +12). ADR-0003-clean — not engine source.
//
// **MIXED state — read carefully:**
//   - **Additive terms (outpost, rook-file): LIVE since M6.I.** Joint-Texel-
//     activated; the values below are the M6.J meta-tune output. NOT zeroed.
//   - **Endgame-scale tunables (OCB / pawnless / 50-move floor): STILL at
//     identity** — each = EG_SCALE_DEN = 64 ⇒ scale ≡ 1, i.e. inert. The
//     M6.I/M6.J quiet-position Texel objective does not exercise the draw-scale
//     terms, so they were left at the identity landing value. See each scale
//     const's own note. ADR-0034 §2.
//
// At the M6.F landing ALL these terms shipped inert (additive weight = 0 ⇒
// term ≡ (0,0); every scale = EG_SCALE_DEN ⇒ blended * D / D == blended
// exactly in i32 ⇒ eval byte-identical to M6.E). M6.I's joint Texel then
// activated and reshaped the additive set; the scales remain inert (above).
//
// **Literature defaults (M6.I starting point, recorded verbatim):**
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
// scale a known per-position constant that folds into the M6.I cached feature
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
// TEXEL-TUNABLE-BEGIN: outpost
/// Knight outpost bonus, middlegame, indexed by relative rank (0..7). Live
/// (M6.J-tuned); literature defaults ~[0,0,0,18,28,16,0,0]. Used from tier1.rs.
pub(crate) const OUTPOST_KNIGHT_MG: [i32; 8] = [0, 0, 0, 11, 19, 18, 0, 0]; // lit ~[0,0,0,18,28,16,0,0]
/// Knight outpost bonus, endgame. Live — see `OUTPOST_KNIGHT_MG`.
pub(crate) const OUTPOST_KNIGHT_EG: [i32; 8] = [0, 0, 0, 14, 16, 23, 0, 0]; // lit ~[0,0,0,12,18,10,0,0]
/// Bishop outpost bonus, middlegame. Live — see `OUTPOST_KNIGHT_MG`.
pub(crate) const OUTPOST_BISHOP_MG: [i32; 8] = [0, 0, 0, 22, 22, 12, 0, 0]; // lit ~[0,0,0,12,18,10,0,0]
/// Bishop outpost bonus, endgame. Live — see `OUTPOST_KNIGHT_MG`.
pub(crate) const OUTPOST_BISHOP_EG: [i32; 8] = [0, 0, 0, 6, 1, -1, 0, 0]; // lit ~[0,0,0, 8,12, 6,0,0]
// TEXEL-TUNABLE-END: outpost

// TEXEL-TUNABLE-BEGIN: rook_file
/// Rook on fully open file (no pawn either color) bonus, middlegame. Live.
pub(crate) const ROOK_OPEN_FILE_MG: i32 = 28; // lit  20
/// Rook on fully open file bonus, endgame. Live — see `ROOK_OPEN_FILE_MG`.
pub(crate) const ROOK_OPEN_FILE_EG: i32 = 16; // lit  10
/// Rook on semi-open file (own pawn absent, enemy present) bonus, middlegame. Live.
pub(crate) const ROOK_SEMI_OPEN_FILE_MG: i32 = 19; // lit  10
/// Rook on semi-open file bonus, endgame. Live — see `ROOK_SEMI_OPEN_FILE_MG`.
pub(crate) const ROOK_SEMI_OPEN_FILE_EG: i32 = 6; // lit   5
// TEXEL-TUNABLE-END: rook_file

/// Endgame draw-scale fixed-point denominator. STRUCTURAL — not a tunable;
/// the per-position scale = numerator / EG_SCALE_DEN ∈ [0, 1].
/// Every scale tunable = EG_SCALE_DEN ⇒ scale ≡ identity (still the case).
pub(crate) const EG_SCALE_DEN: i32 = 64; // structural (not tuned)
// TEXEL-TUNABLE-BEGIN: scale
/// OCB-with-pawns draw-scale numerator. Texel-tunable but STILL at identity =
/// EG_SCALE_DEN (the quiet-position objective doesn't exercise it); lit ~32.
pub(crate) const OCB_WITH_PAWNS_SCALE: i32 = 64; // = DEN ⇒ identity; lit ~32 (0.5)
/// Pawnless drawish endgame scale numerator. Texel-tunable but STILL at
/// identity = EG_SCALE_DEN (see `OCB_WITH_PAWNS_SCALE`); literature ~8.
pub(crate) const PAWNLESS_DRAW_SCALE: i32 = 64; // = DEN ⇒ identity; lit ~8
// TEXEL-TUNABLE-END: scale
/// 50-move proximity taper onset in halfmoves. STRUCTURAL — not a tunable
/// (semantically correct onset; tuning changes meaning, not magnitude).
pub(crate) const FIFTY_MOVE_TAPER_FROM: i32 = 80; // structural onset (halfmoves)
// TEXEL-TUNABLE-BEGIN: scale
/// 50-move taper floor: scale value at halfmove 100. Texel-tunable but STILL
/// at identity = EG_SCALE_DEN (ramp flat; see `OCB_WITH_PAWNS_SCALE`); lit ~16.
pub(crate) const FIFTY_MOVE_FLOOR: i32 = 64; // = DEN ⇒ identity; lit ~16
// TEXEL-TUNABLE-END: scale

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
