//! M6.G — reusable game-result-labeled quiet-position corpus.
//!
//! Data-infrastructure module: PGN ingestion + deterministic self-play
//! generation + filtering + dedup + game-level train/val split + the
//! stratified selection objective + a data-quality gate. No `evaluate` /
//! `Position` / search **behavior** change (the only engine-crate touch is
//! the additive, behavior-neutral `search::quiescence_eval_white` seam) —
//! so no bench, no SPRT, no Elo claim. The gate is data-quality checks.
//!
//! Consumed by M6.I (Texel tuning), the tuning-backlog "PST co-tuning"
//! Arm B, future SPSA campaigns, and M10 NNUE data-prep. See
//! `docs/plans/m6.g.md`, `docs/research/m6-corpus-construction.md`,
//! and ADR-0035.

pub mod consumer;
pub mod dedup;
pub mod dispatcher;
/// M6.H on-demand network fetcher (CCRL/Lichess). Gated behind the non-default
/// `corpus-fetch` Cargo feature so the engine binary never links the HTTP /
/// decompression stack (`ureq`/`zstd`/`zip`). See `docs/plans/m6.h.md`,
/// ADR-0036.
#[cfg(feature = "corpus-fetch")]
pub mod fetch;
pub mod filter;
pub mod manifest;
pub mod objective;
pub mod openings;
pub mod pending;
pub mod pgn;
pub mod prng;
pub mod quality_gate;
pub mod quiet;
pub mod selfplay;
pub mod split;
pub mod store;

use std::fmt;

// ---------------------------------------------------------------------------
// Pinned M6.G↔M6.I interface contract.
//
// These constants ARE the interface: the corpus is built against them and
// M6.I must read the same values from `filter_spec.txt`. Changing one is a
// contract break, not a tweak. Provenance: research §11 (recommended
// predicate) — `!in_check ∧ |static_eval − qsearch| < QUIET_MARGIN_CP`,
// opening-ply skip, |eval| cutoff, per-game cap. Predicate B was chosen
// precisely so static_eval ≈ qsearch within the margin, which is what lets
// M6.I tune on `static_eval_white` directly (no qsearch at tune time).
// ---------------------------------------------------------------------------

/// A position is non-quiet if `|static_eval − qsearch| ≥` this (centipawns).
pub const QUIET_MARGIN_CP: i32 = 30;
/// Opening-ply skip: positions with `ply_from_game_start < this` are dropped
/// (0-indexed half-move count). Book/theory positions are over-represented.
pub const OPENING_SKIP_PLIES: u32 = 8;
/// High-score exclusion: `|static_eval| > this` (centipawns) is dropped
/// (near-resignation; result already determined).
pub const HIGH_SCORE_CP: i32 = 600;
/// At most this many retained positions per source game (seeded reservoir).
pub const PER_GAME_CAP: usize = 10;

// ---------------------------------------------------------------------------
// Shared types (the §3.1 interface slice — fixed before the parallel fan-out).
// ---------------------------------------------------------------------------

/// Original game-outcome label, always from White's perspective. NEVER an
/// engine evaluation (ADR-0003 spirit / ADR-0035 label-provenance audit).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub enum Label {
    /// `1-0` → `1.0`.
    WhiteWin,
    /// `1/2-1/2` → `0.5`.
    Draw,
    /// `0-1` → `0.0`.
    BlackWin,
}

impl Label {
    /// Texel target value (White-POV): 1.0 / 0.5 / 0.0.
    pub fn as_f64(self) -> f64 {
        match self {
            Label::WhiteWin => 1.0,
            Label::Draw => 0.5,
            Label::BlackWin => 0.0,
        }
    }

    /// On-disk tag byte (store.rs frame). Stable: never renumber.
    pub fn as_u8(self) -> u8 {
        match self {
            Label::WhiteWin => 0,
            Label::Draw => 1,
            Label::BlackWin => 2,
        }
    }

    /// Inverse of [`Label::as_u8`].
    pub fn from_u8(b: u8) -> Option<Label> {
        match b {
            0 => Some(Label::WhiteWin),
            1 => Some(Label::Draw),
            2 => Some(Label::BlackWin),
            _ => None,
        }
    }

    /// Parse a PGN `Result` / EPD outcome token. `*` (unterminated) → `None`
    /// — the caller drops the whole game (never mislabels).
    pub fn from_result_tag(s: &str) -> Option<Label> {
        match s.trim() {
            "1-0" => Some(Label::WhiteWin),
            "0-1" => Some(Label::BlackWin),
            "1/2-1/2" | "½-½" => Some(Label::Draw),
            _ => None,
        }
    }
}

/// Provenance of a record — the ADR-0003 label-provenance audit input.
/// Zurichess is intentionally absent: its `quiet-labeled.epd` `c9` labels
/// are Stockfish-080916 played-game results, not original outcomes
/// (research §3) → REJECTED by the audit.
///
/// Self-play is split into two variants by **opening regime**:
/// `SelfPlayOnBook` (games seeded from a vendored opening book FEN) and
/// `SelfPlayOffBook` (games seeded from the startpos + random plies). The
/// split lets the M6.I bi-level optimizer treat the book / off-book
/// proportion as a training-time per-source reweighting axis (identical
/// mechanism to the CCRL / Lichess proportions) instead of a corpus-
/// generation knob. Both variants are original game-result-labeled; the
/// audit accepts all four sources.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub enum Source {
    /// clawfish-vs-clawfish deterministic self-play, opening seeded from a
    /// vendored opening-book FEN (original game result).
    SelfPlayOnBook,
    /// clawfish-vs-clawfish deterministic self-play, opening seeded from
    /// startpos + random plies (original game result).
    SelfPlayOffBook,
    /// CCRL PGN snapshot (original game result; embedded engine evals ignored).
    Ccrl,
    /// Lichess open-database PGN (original game result).
    LichessOpen,
}

impl Source {
    /// On-disk tag byte (store.rs frame). Stable: never renumber.
    pub fn as_u8(self) -> u8 {
        match self {
            Source::SelfPlayOnBook => 0,
            Source::SelfPlayOffBook => 1,
            Source::Ccrl => 2,
            Source::LichessOpen => 3,
        }
    }

    /// Inverse of [`Source::as_u8`].
    pub fn from_u8(b: u8) -> Option<Source> {
        match b {
            0 => Some(Source::SelfPlayOnBook),
            1 => Some(Source::SelfPlayOffBook),
            2 => Some(Source::Ccrl),
            3 => Some(Source::LichessOpen),
            _ => None,
        }
    }
}

/// `0xFF` sentinel for `CorpusRecord::depth_rung` on external (non-self-play)
/// sources, which have no fixed-depth stratum.
pub const DEPTH_RUNG_EXTERNAL: u8 = 0xFF;

// Blind-spot stratum membership bits (frozen at corpus-build time; never a
// live `eval::tier1` call — closes the M6.I re-tune circularity). Reused by
// the stratified objective.

/// STS theme #3 ("Knight Outposts") corpus-context analogue stratum.
pub const STRATUM_OUTPOST: u8 = 1 << 0;
/// Endgame stratum: phase-tag below the mop-up threshold.
pub const STRATUM_ENDGAME: u8 = 1 << 1;
// (Bit 2 reserved — previously `STRATUM_BOOK_OPENING`. Book / off-book
// provenance is now a per-`Source` distinction (`SelfPlayOnBook` vs
// `SelfPlayOffBook`), readable through `StratObjective::per_source`. The
// dedicated stratum bit became redundant and was removed when the four-
// source taxonomy landed.)

/// One labeled quiet position. `fen` is the canonical `Position::to_fen()`.
/// `label` is the ORIGINAL game outcome (never an engine score). `game_id`
/// is the game-level split key. `ply` is the 0-indexed half-move count from
/// the game start (opening-skip + the dedup tie-break key). `depth_rung` is
/// the R-TC self-play stratum (`DEPTH_RUNG_EXTERNAL` for PGN sources).
/// `strata` is the frozen blind-spot bitset.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CorpusRecord {
    /// Canonical `Position::to_fen()`.
    pub fen: String,
    /// Original game outcome, White-POV (never an engine score).
    pub label: Label,
    /// Provenance (ADR-0003 audit input).
    pub source: Source,
    /// Game-level train/val split key.
    pub game_id: u64,
    /// 0-indexed half-move count from game start (opening-skip + dedup tie-break).
    pub ply: u32,
    /// R-TC self-play depth stratum (`DEPTH_RUNG_EXTERNAL` for PGN sources).
    pub depth_rung: u8,
    /// Frozen blind-spot stratum bitset (`STRATUM_*`).
    pub strata: u8,
}

impl CorpusRecord {
    /// Deterministic dedup tie-break key: among exact-FEN duplicates the
    /// lexicographically-smallest `(source, game_id, ply)` survives — a
    /// function of the input *multiset*, independent of shard/merge order
    /// (the reproducibility-gate prerequisite).
    pub fn dedup_key(&self) -> (u8, u64, u32) {
        (self.source.as_u8(), self.game_id, self.ply)
    }
}

/// Crate-wide corpus error.
#[derive(Debug)]
pub enum CorpusError {
    /// Underlying I/O failure.
    Io(std::io::Error),
    /// PGN/SAN parse failure — the caller drops the whole game.
    Pgn(String),
    /// FEN parse failure.
    Fen(crate::FenError),
    /// Checkpoint / shard frame corruption beyond the tolerated torn tail.
    Checkpoint(String),
    /// Manifest JSON malformed / schema mismatch.
    Json(String),
    /// A data-quality gate check failed (the landing gate).
    QualityGate(String),
}

impl fmt::Display for CorpusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CorpusError::Io(e) => write!(f, "io: {e}"),
            CorpusError::Pgn(s) => write!(f, "pgn: {s}"),
            CorpusError::Fen(e) => write!(f, "fen: {e:?}"),
            CorpusError::Checkpoint(s) => write!(f, "checkpoint: {s}"),
            CorpusError::Json(s) => write!(f, "json: {s}"),
            CorpusError::QualityGate(s) => write!(f, "quality-gate: {s}"),
        }
    }
}

impl std::error::Error for CorpusError {}

impl From<std::io::Error> for CorpusError {
    fn from(e: std::io::Error) -> Self {
        CorpusError::Io(e)
    }
}

impl From<crate::FenError> for CorpusError {
    fn from(e: crate::FenError) -> Self {
        CorpusError::Fen(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_f64_mapping_white_pov() {
        assert_eq!(Label::WhiteWin.as_f64(), 1.0);
        assert_eq!(Label::Draw.as_f64(), 0.5);
        assert_eq!(Label::BlackWin.as_f64(), 0.0);
    }

    #[test]
    fn label_result_tag_parse_and_star_is_none() {
        assert_eq!(Label::from_result_tag("1-0"), Some(Label::WhiteWin));
        assert_eq!(Label::from_result_tag("0-1"), Some(Label::BlackWin));
        assert_eq!(Label::from_result_tag("1/2-1/2"), Some(Label::Draw));
        assert_eq!(Label::from_result_tag("*"), None);
        assert_eq!(Label::from_result_tag(""), None);
    }

    #[test]
    fn label_source_byte_roundtrip_stable() {
        for l in [Label::WhiteWin, Label::Draw, Label::BlackWin] {
            assert_eq!(Label::from_u8(l.as_u8()), Some(l));
        }
        for s in [
            Source::SelfPlayOnBook,
            Source::SelfPlayOffBook,
            Source::Ccrl,
            Source::LichessOpen,
        ] {
            assert_eq!(Source::from_u8(s.as_u8()), Some(s));
        }
        assert_eq!(Label::from_u8(3), None);
        assert_eq!(Source::from_u8(9), None);
    }

    #[test]
    fn pinned_interface_constants_golden() {
        // The M6.G↔M6.I interface contract. A change here is a contract
        // break — M6.I reads these from filter_spec.txt. Pinned so an
        // accidental edit fails the test, not silently re-defines the corpus.
        assert_eq!(QUIET_MARGIN_CP, 30);
        assert_eq!(OPENING_SKIP_PLIES, 8);
        assert_eq!(HIGH_SCORE_CP, 600);
        assert_eq!(PER_GAME_CAP, 10);
        assert_eq!(DEPTH_RUNG_EXTERNAL, 0xFF);
        assert_eq!(STRATUM_OUTPOST, 1);
        assert_eq!(STRATUM_ENDGAME, 2);
    }

    #[test]
    fn dedup_key_orders_by_source_then_game_then_ply() {
        let r = |src, g, p| CorpusRecord {
            fen: "x".into(),
            label: Label::Draw,
            source: src,
            game_id: g,
            ply: p,
            depth_rung: DEPTH_RUNG_EXTERNAL,
            strata: 0,
        };
        assert!(r(Source::SelfPlayOnBook, 5, 9).dedup_key() < r(Source::Ccrl, 0, 0).dedup_key());
        assert!(r(Source::SelfPlayOffBook, 5, 9).dedup_key() < r(Source::Ccrl, 0, 0).dedup_key());
        assert!(r(Source::Ccrl, 2, 9).dedup_key() < r(Source::Ccrl, 3, 0).dedup_key());
        assert!(r(Source::Ccrl, 2, 4).dedup_key() < r(Source::Ccrl, 2, 5).dedup_key());
    }
}
