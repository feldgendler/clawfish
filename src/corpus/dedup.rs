//! FEN dedup + per-game cap. Pipeline order is `dedup → cap → split`
//! (research §2.5/§7.2). Dedup survivor among exact-FEN duplicates =
//! lexicographically-smallest `CorpusRecord::dedup_key()` ⇒ a deterministic
//! function of the input MULTISET, independent of shard/merge order (the
//! reproducibility-gate prerequisite). Bounded-memory external sort-merge:
//! RAM ceiling `max_mem_mb` + disk-spill ceiling under
//! `target/corpus-spill/` (git-ignored), both documented (R6).
//!
//! Implemented by the M6.G `dedup` coder slice per `docs/plans/m6.g.md` §3.8.

use std::path::Path;

use super::{CorpusError, CorpusRecord};

/// Remove exact-FEN duplicates keeping the min-`dedup_key` survivor.
/// Streaming + spill-bounded; `spill_dir` under `target/`.
pub fn dedup_fen(
    _records: impl Iterator<Item = CorpusRecord>,
    _spill_dir: &Path,
    _max_mem_mb: u64,
    _max_spill_gb: u64,
) -> Result<Vec<CorpusRecord>, CorpusError> {
    todo!("M6.G dedup slice")
}

/// At most `cap` records per `game_id` (seeded reservoir — deterministic
/// from `seed`).
pub fn per_game_cap(_records: Vec<CorpusRecord>, _cap: usize, _seed: u64) -> Vec<CorpusRecord> {
    todo!("M6.G dedup slice")
}
