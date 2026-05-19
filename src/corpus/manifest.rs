//! Reproducibility artifacts (research §7): `manifest.json`,
//! `rng_seeds.txt`, `filter_spec.txt`, `corpus_stats.txt`. Hand-rolled
//! JSON + SHA-256 (no serde / sha2 crate dep — dependency-hygiene-clean).
//!
//! Implemented by the M6.G `manifest` coder slice per
//! `docs/plans/m6.g.md` §3.8.

use std::path::Path;

use super::CorpusError;

/// One external source's pinned provenance (ADR-0003 audit input).
#[derive(Clone, Debug)]
pub struct SourceEntry {
    /// Human label (e.g. `"CCRL-4040-2026-04"`).
    pub name: String,
    /// `"ccrl"` / `"lichess"` / `"self-play"`.
    pub kind: String,
    /// Download URL (raw source; empty for self-play).
    pub url: String,
    /// SHA-256 of the raw source file.
    pub sha256: String,
    /// Raw source size in bytes.
    pub size_bytes: u64,
    /// Acquisition date (operator wall clock).
    pub date: String,
}

/// The full corpus manifest (serialized to `bench/corpus/manifest.json`).
#[derive(Clone, Debug, Default)]
pub struct Manifest {
    /// Manifest schema version (bump on format change).
    pub schema_version: u32,
    /// `git rev-parse HEAD` of the building tool.
    pub tool_git_sha: String,
    /// Pinned external-source provenance entries.
    pub sources: Vec<SourceEntry>,
    /// Self-play base seed.
    pub self_play_seed: u64,
    /// Empirically-anchored `(depth, weight)` ladder (R-TC).
    pub depth_ladder: Vec<(u8, u32)>,
    /// Train/val split seed.
    pub split_seed: u64,
    /// Held-out fraction.
    pub val_fraction: f64,
    /// Total records in the frozen corpus.
    pub total_records: u64,
    /// SHA-256 of the frozen corpus artifact (the bytes consumers freeze on).
    pub corpus_sha256: String,
}

/// SHA-256 of a file (hand-rolled; pinned by a known-vector test).
pub fn sha256_file(_path: &Path) -> Result<String, CorpusError> {
    todo!("M6.G manifest slice")
}

/// SHA-256 of a byte slice (hex lowercase).
pub fn sha256_bytes(_bytes: &[u8]) -> String {
    todo!("M6.G manifest slice")
}

/// Serialize the manifest to `dir/manifest.json` (atomic write).
pub fn write_manifest(_m: &Manifest, _dir: &Path) -> Result<(), CorpusError> {
    todo!("M6.G manifest slice")
}

/// Parse `dir/manifest.json`.
pub fn read_manifest(_dir: &Path) -> Result<Manifest, CorpusError> {
    todo!("M6.G manifest slice")
}

/// Echo the pinned interface constants into `dir/filter_spec.txt` (M6.H
/// reads these — the contract surface).
pub fn write_filter_spec(_dir: &Path) -> Result<(), CorpusError> {
    todo!("M6.G manifest slice")
}
