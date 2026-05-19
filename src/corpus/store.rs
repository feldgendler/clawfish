//! Crash-safe per-game append-block log + checkpoint (R1/R2/R3).
//!
//! Shard frame (the atomic unit): `MAGIC:u32 | game_id:u64 | rec_count:u32
//! | payload_len:u32 | payload | crc32(header‖payload):u32`. A whole game is
//! appended in one `write` + `fsync`. Resume scans frames and accepts only
//! fully-valid ones; a torn final block (partial write / bad CRC after a
//! prior game's fsync) is discarded WHOLESALE by truncating to the last
//! valid byte — never line-by-line. Checkpoint is written `.tmp`→`fsync`→
//! rename AFTER the game block's fsync; resume skips already-present
//! `game_id`s (idempotent — never partial, never double).
//!
//! Implemented by the M6.G `selfplay+store` (Opus) coder slice per
//! `docs/plans/m6.g.md` §3.5/§3.6.

use std::path::Path;

use super::{CorpusError, CorpusRecord};

/// Shard frame magic (`"CWG1"`); rejects foreign/garbage bytes early.
pub const SHARD_MAGIC: u32 = 0x4357_4731;

/// One decoded game block.
pub struct GameBlock {
    /// The game's id (unique; resume skips already-present ids).
    pub game_id: u64,
    /// The game's labeled positions, in game order.
    pub records: Vec<CorpusRecord>,
}

/// Resume checkpoint. `prng_state` pairs with `prng::Prng::from_state`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checkpoint {
    /// Count of games durably committed before this checkpoint.
    pub games_completed: u64,
    /// Raw `Prng` state to resume the seeded stream bit-identically.
    pub prng_state: u64,
    /// Depth-ladder cumulative cursor (R-TC sampling resume).
    pub ladder_cursor: u64,
    /// Per-worker next `game_id` (R7 striping resume).
    pub per_worker_next_game_id: Vec<u64>,
}

/// Encode one game's records into a single CRC-framed block.
pub fn encode_block(_game_id: u64, _records: &[CorpusRecord]) -> Vec<u8> {
    todo!("M6.G store slice")
}

/// Scan a shard: return all fully-valid blocks and the byte length up to
/// (and including) the last valid block. Bytes past that are a torn tail
/// truncated on resume.
pub fn scan_valid_blocks(_path: &Path) -> Result<(Vec<GameBlock>, u64), CorpusError> {
    todo!("M6.G store slice")
}

/// Append one game block to the shard and `fsync`. The atomic unit (R1/R2).
pub fn append_block(
    _path: &Path,
    _game_id: u64,
    _records: &[CorpusRecord],
) -> Result<(), CorpusError> {
    todo!("M6.G store slice")
}

/// Atomic whole-file replace: `path.tmp` → `fsync` → rename → `fsync(dir)`.
/// Used for the checkpoint + manifest, NOT the append-log.
pub fn atomic_write(_path: &Path, _bytes: &[u8]) -> Result<(), CorpusError> {
    todo!("M6.G store slice")
}

/// Persist the resume checkpoint atomically (after the game-block fsync).
pub fn write_checkpoint(_path: &Path, _ckpt: &Checkpoint) -> Result<(), CorpusError> {
    todo!("M6.G store slice")
}

/// Read the resume checkpoint, if present.
pub fn read_checkpoint(_path: &Path) -> Result<Option<Checkpoint>, CorpusError> {
    todo!("M6.G store slice")
}
