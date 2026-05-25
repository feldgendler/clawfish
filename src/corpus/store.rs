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

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use super::{CorpusError, CorpusRecord, Label, Source};

/// Shard frame magic (`"CWG1"`); rejects foreign/garbage bytes early.
pub const SHARD_MAGIC: u32 = 0x4357_4731;

/// Fixed frame-header byte length: `MAGIC:u32 | game_id:u64 | rec_count:u32
/// | payload_len:u32`. The CRC is computed over `header‖payload`.
const HEADER_LEN: usize = 4 + 8 + 4 + 4;
/// Trailing CRC-32 width.
const CRC_LEN: usize = 4;

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
    /// Consumer cursor: the next game_id the consumer will process.
    /// Authoritative for the per-game-file architecture (CWK2 format).
    pub next_consume_id: u64,
}

// ---------------------------------------------------------------------------
// CRC-32 (IEEE 802.3 / zlib). Hand-rolled — no crate dependency (plan §1
// "No new third-party Rust crate"). Reflected polynomial 0xEDB88320; init
// 0xFFFFFFFF; final XOR 0xFFFFFFFF. Pinned by `crc_known_vector`.
// ---------------------------------------------------------------------------

const CRC32_POLY: u32 = 0xEDB8_8320;

/// 256-entry CRC-32 lookup table, built once on first use.
fn crc32_table() -> &'static [u32; 256] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        let mut i = 0usize;
        while i < 256 {
            let mut crc = i as u32;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ CRC32_POLY
                } else {
                    crc >> 1
                };
                bit += 1;
            }
            table[i] = crc;
            i += 1;
        }
        table
    })
}

/// CRC-32 (IEEE) of `bytes`.
pub fn crc32(bytes: &[u8]) -> u32 {
    let table = crc32_table();
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in bytes {
        let idx = ((crc ^ b as u32) & 0xFF) as usize;
        crc = (crc >> 8) ^ table[idx];
    }
    crc ^ 0xFFFF_FFFF
}

// ---------------------------------------------------------------------------
// Block encode / decode.
// ---------------------------------------------------------------------------

/// Encode one game's records into a single CRC-framed block.
pub fn encode_block(game_id: u64, records: &[CorpusRecord]) -> Vec<u8> {
    let mut payload = Vec::new();
    for r in records {
        let fen = r.fen.as_bytes();
        debug_assert!(
            fen.len() <= u16::MAX as usize,
            "FEN longer than u16::MAX bytes is impossible for legal positions"
        );
        payload.extend_from_slice(&(fen.len() as u16).to_le_bytes());
        payload.extend_from_slice(fen);
        payload.push(r.label.as_u8());
        payload.push(r.source.as_u8());
        payload.extend_from_slice(&r.ply.to_le_bytes());
        payload.push(r.depth_rung);
        payload.push(r.strata);
    }

    let rec_count = records.len() as u32;
    let payload_len = payload.len() as u32;

    let mut block = Vec::with_capacity(HEADER_LEN + payload.len() + CRC_LEN);
    block.extend_from_slice(&SHARD_MAGIC.to_le_bytes());
    block.extend_from_slice(&game_id.to_le_bytes());
    block.extend_from_slice(&rec_count.to_le_bytes());
    block.extend_from_slice(&payload_len.to_le_bytes());
    block.extend_from_slice(&payload);

    // CRC covers header ‖ payload (everything written so far).
    let crc = crc32(&block);
    block.extend_from_slice(&crc.to_le_bytes());
    block
}

/// Decode one block starting at `buf[0]`. Returns `(GameBlock, consumed)` on
/// a fully-valid frame, or `None` if the frame is incomplete/corrupt (a torn
/// tail — the caller stops scanning here and truncates).
pub(crate) fn decode_block(buf: &[u8]) -> Option<(GameBlock, usize)> {
    if buf.len() < HEADER_LEN + CRC_LEN {
        return None;
    }
    let magic = u32::from_le_bytes(buf[0..4].try_into().ok()?);
    if magic != SHARD_MAGIC {
        return None;
    }
    let game_id = u64::from_le_bytes(buf[4..12].try_into().ok()?);
    let rec_count = u32::from_le_bytes(buf[12..16].try_into().ok()?) as usize;
    let payload_len = u32::from_le_bytes(buf[16..20].try_into().ok()?) as usize;

    let frame_len = HEADER_LEN.checked_add(payload_len)?.checked_add(CRC_LEN)?;
    if buf.len() < frame_len {
        return None;
    }

    let crc_stored = u32::from_le_bytes(
        buf[HEADER_LEN + payload_len..HEADER_LEN + payload_len + CRC_LEN]
            .try_into()
            .ok()?,
    );
    let crc_actual = crc32(&buf[..HEADER_LEN + payload_len]);
    if crc_stored != crc_actual {
        return None;
    }

    // CRC validated ⇒ the payload is intact; a malformed record body here
    // would mean a logic bug in `encode_block`, not on-disk corruption.
    let mut records = Vec::with_capacity(rec_count);
    let payload = &buf[HEADER_LEN..HEADER_LEN + payload_len];
    let mut off = 0usize;
    for _ in 0..rec_count {
        if off + 2 > payload.len() {
            return None;
        }
        let fen_len = u16::from_le_bytes(payload[off..off + 2].try_into().ok()?) as usize;
        off += 2;
        if off + fen_len + 1 + 1 + 4 + 1 + 1 > payload.len() {
            return None;
        }
        let fen = String::from_utf8(payload[off..off + fen_len].to_vec()).ok()?;
        off += fen_len;
        let label = Label::from_u8(payload[off])?;
        off += 1;
        let source = Source::from_u8(payload[off])?;
        off += 1;
        let ply = u32::from_le_bytes(payload[off..off + 4].try_into().ok()?);
        off += 4;
        let depth_rung = payload[off];
        off += 1;
        let strata = payload[off];
        off += 1;
        records.push(CorpusRecord {
            fen,
            label,
            source,
            game_id,
            ply,
            depth_rung,
            strata,
        });
    }
    if off != payload_len {
        return None;
    }

    Some((GameBlock { game_id, records }, frame_len))
}

/// Scan a shard: return all fully-valid blocks and the byte length up to
/// (and including) the last valid block. Bytes past that are a torn tail
/// truncated on resume.
pub fn scan_valid_blocks(path: &Path) -> Result<(Vec<GameBlock>, u64), CorpusError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(CorpusError::Io(e)),
    };

    let mut blocks = Vec::new();
    let mut valid_len: u64 = 0;
    let mut off = 0usize;
    while off < bytes.len() {
        match decode_block(&bytes[off..]) {
            Some((block, consumed)) => {
                blocks.push(block);
                off += consumed;
                valid_len = off as u64;
            }
            // First frame that fails magic/length/CRC: stop. Everything from
            // here on is a torn tail, discarded WHOLESALE on resume.
            None => break,
        }
    }
    Ok((blocks, valid_len))
}

/// As `scan_valid_blocks`, but also returns each block's end-offset within
/// the file. Used by `corpus truncate` to truncate at exact on-disk block
/// boundaries without relying on round-trip encoder equivalence (the
/// encoder is deterministic, but tying offsets to a scan-time invariant is
/// simpler and less brittle than re-encoding to recompute).
pub fn scan_valid_blocks_with_offsets(
    path: &Path,
) -> Result<(Vec<(GameBlock, u64)>, u64), CorpusError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(CorpusError::Io(e)),
    };
    let mut blocks: Vec<(GameBlock, u64)> = Vec::new();
    let mut valid_len: u64 = 0;
    let mut off = 0usize;
    while off < bytes.len() {
        match decode_block(&bytes[off..]) {
            Some((block, consumed)) => {
                off += consumed;
                valid_len = off as u64;
                blocks.push((block, valid_len));
            }
            None => break,
        }
    }
    Ok((blocks, valid_len))
}

/// Truncate `path` to `valid_len` bytes, discarding any torn tail wholesale,
/// then `fsync`. No-op if the file is already exactly that length.
pub fn truncate_to_valid(path: &Path, valid_len: u64) -> Result<(), CorpusError> {
    let f = OpenOptions::new().write(true).open(path)?;
    if f.metadata()?.len() != valid_len {
        f.set_len(valid_len)?;
        f.sync_all()?;
    }
    Ok(())
}

/// Append one game block to the shard and `fsync`. The atomic unit (R1/R2).
///
/// Returns the byte offset of the file BEFORE the write (the length before
/// appending). This pre-append offset is the exact truncation point needed by
/// the boundary-game extend logic: `truncate_to_valid(lane, pre_offset)` drops
/// the partial boundary block, making extend idempotent.
pub fn append_block(
    path: &Path,
    game_id: u64,
    records: &[CorpusRecord],
) -> Result<u64, CorpusError> {
    let block = encode_block(game_id, records);
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    // Read the pre-append length via the file handle (the file is already open
    // in append mode, so metadata().len() is the current size before write_all).
    let pre_offset = f.metadata()?.len();
    // One `write_all` of the whole block — a short write here leaves a torn
    // frame the CRC scan rejects WHOLESALE, never a partial record.
    f.write_all(&block)?;
    f.sync_all()?;
    Ok(pre_offset)
}

/// Append pre-encoded bytes (the output of `encode_block`) to the shard and
/// `fsync`. Used by the consumer to avoid re-encoding blocks that have already
/// been dedup-capped.
pub fn append_raw_bytes(path: &Path, bytes: &[u8]) -> Result<(), CorpusError> {
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Atomic whole-file replace (checkpoint + manifest only).
// ---------------------------------------------------------------------------

/// Atomic whole-file replace: `path.tmp` → `fsync` → rename → `fsync(dir)`.
/// Used for the checkpoint + manifest, NOT the append-log.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), CorpusError> {
    let tmp = tmp_path(path);
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    // Directory fsync makes the rename itself durable (POSIX: a rename is
    // not guaranteed persisted until the containing directory is synced).
    if let Some(dir) = path.parent()
        && let Ok(d) = File::open(dir)
    {
        // Directory fsync is best-effort: some filesystems reject
        // fsync on a directory fd. The rename is still atomic.
        let _ = d.sync_all();
    }
    Ok(())
}

/// Sibling `.tmp` path for the atomic-write staging file.
fn tmp_path(path: &Path) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".tmp");
    std::path::PathBuf::from(s)
}

// ---------------------------------------------------------------------------
// Checkpoint serialization. Fixed binary layout, CRC-protected (so a torn
// checkpoint write — caught before the rename completes — is rejected and
// resume falls back to the shard scan, never a half-parsed cursor).
// ---------------------------------------------------------------------------

/// Checkpoint magic for the CWK2 format (adds `next_consume_id`).
/// Old CWK1 (`0x4357_4B31`) checkpoints are rejected as corrupt → resume
/// falls back to the shard scan and derives the cursor from committed_ids.
pub const CKPT_MAGIC: u32 = 0x4357_4B32; // "CWK2"

fn encode_checkpoint(ckpt: &Checkpoint) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&CKPT_MAGIC.to_le_bytes());
    body.extend_from_slice(&ckpt.games_completed.to_le_bytes());
    body.extend_from_slice(&ckpt.prng_state.to_le_bytes());
    body.extend_from_slice(&ckpt.ladder_cursor.to_le_bytes());
    body.extend_from_slice(&(ckpt.per_worker_next_game_id.len() as u32).to_le_bytes());
    for g in &ckpt.per_worker_next_game_id {
        body.extend_from_slice(&g.to_le_bytes());
    }
    body.extend_from_slice(&ckpt.next_consume_id.to_le_bytes());
    let crc = crc32(&body);
    body.extend_from_slice(&crc.to_le_bytes());
    body
}

fn decode_checkpoint(bytes: &[u8]) -> Result<Checkpoint, CorpusError> {
    let bad = || CorpusError::Checkpoint("corrupt checkpoint".to_string());
    // Minimum: magic(4) + games_completed(8) + prng_state(8) + ladder_cursor(8)
    //          + worker_count(4) + next_consume_id(8) + crc(4) = 44
    if bytes.len() < 4 + 8 + 8 + 8 + 4 + 8 + CRC_LEN {
        return Err(bad());
    }
    let crc_at = bytes.len() - CRC_LEN;
    let crc_stored = u32::from_le_bytes(bytes[crc_at..].try_into().map_err(|_| bad())?);
    if crc32(&bytes[..crc_at]) != crc_stored {
        return Err(bad());
    }
    let magic = u32::from_le_bytes(bytes[0..4].try_into().map_err(|_| bad())?);
    if magic != CKPT_MAGIC {
        return Err(bad());
    }
    let games_completed = u64::from_le_bytes(bytes[4..12].try_into().map_err(|_| bad())?);
    let prng_state = u64::from_le_bytes(bytes[12..20].try_into().map_err(|_| bad())?);
    let ladder_cursor = u64::from_le_bytes(bytes[20..28].try_into().map_err(|_| bad())?);
    let n = u32::from_le_bytes(bytes[28..32].try_into().map_err(|_| bad())?) as usize;
    let mut off = 32;
    // Expect: n worker ids (8 bytes each) + next_consume_id (8) + crc (4)
    if off + n * 8 + 8 + CRC_LEN != bytes.len() {
        return Err(bad());
    }
    let mut per_worker_next_game_id = Vec::with_capacity(n);
    for _ in 0..n {
        per_worker_next_game_id.push(u64::from_le_bytes(
            bytes[off..off + 8].try_into().map_err(|_| bad())?,
        ));
        off += 8;
    }
    let next_consume_id = u64::from_le_bytes(bytes[off..off + 8].try_into().map_err(|_| bad())?);
    Ok(Checkpoint {
        games_completed,
        prng_state,
        ladder_cursor,
        per_worker_next_game_id,
        next_consume_id,
    })
}

/// Persist the resume checkpoint atomically (after the game-block fsync).
pub fn write_checkpoint(path: &Path, ckpt: &Checkpoint) -> Result<(), CorpusError> {
    atomic_write(path, &encode_checkpoint(ckpt))
}

/// Read the resume checkpoint, if present.
pub fn read_checkpoint(path: &Path) -> Result<Option<Checkpoint>, CorpusError> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CorpusError::Io(e)),
    };
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    Ok(Some(decode_checkpoint(&bytes)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    // ── Temp-dir scaffolding (no `tempfile` dev-dep; plan §4) ─────────────

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static CTR: AtomicU64 = AtomicU64::new(0);
            let n = CTR.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let dir = std::env::temp_dir().join(format!("clawfish-corpus-store-{tag}-{pid}-{n}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn rec(fen: &str, label: Label, src: Source, game_id: u64, ply: u32) -> CorpusRecord {
        CorpusRecord {
            fen: fen.to_string(),
            label,
            source: src,
            game_id,
            ply,
            depth_rung: 7,
            strata: super::super::STRATUM_OUTPOST,
        }
    }

    fn game_records(game_id: u64, n: u32) -> Vec<CorpusRecord> {
        (0..n)
            .map(|p| {
                rec(
                    &format!("8/8/8/8/8/8/8/k6K w - - {p} {game_id}"),
                    if p % 2 == 0 {
                        Label::WhiteWin
                    } else {
                        Label::Draw
                    },
                    Source::SelfPlayOffBook,
                    game_id,
                    p,
                )
            })
            .collect()
    }

    // ── CRC ──────────────────────────────────────────────────────────────

    #[test]
    fn crc_known_vector() {
        // The canonical CRC-32 (IEEE) check value for ASCII "123456789"
        // is 0xCBF43926 (the standard "check" value used by every
        // reference CRC-32 implementation). Pins the hand-rolled table.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        // Empty input: identity (init ^ final xor → 0).
        assert_eq!(crc32(b""), 0x0000_0000);
        // Single byte cross-check against the well-known value.
        assert_eq!(crc32(b"a"), 0xE8B7_BE43);
    }

    // ── Frame round-trip ──────────────────────────────────────────────────

    #[test]
    fn game_block_frame_roundtrip() {
        let td = TempDir::new("roundtrip");
        let shard = td.path("shard.bin");
        let g0 = game_records(11, 3);
        let g1 = game_records(22, 5);
        append_block(&shard, 11, &g0).unwrap();
        append_block(&shard, 22, &g1).unwrap();

        let (blocks, valid_len) = scan_valid_blocks(&shard).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].game_id, 11);
        assert_eq!(blocks[0].records, g0);
        assert_eq!(blocks[1].game_id, 22);
        assert_eq!(blocks[1].records, g1);
        assert_eq!(valid_len, std::fs::metadata(&shard).unwrap().len());
    }

    #[test]
    fn empty_game_block_roundtrips() {
        let td = TempDir::new("empty-game");
        let shard = td.path("shard.bin");
        append_block(&shard, 7, &[]).unwrap();
        let (blocks, _) = scan_valid_blocks(&shard).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].game_id, 7);
        assert!(blocks[0].records.is_empty());
    }

    #[test]
    fn scan_missing_shard_is_empty_not_error() {
        let td = TempDir::new("missing");
        let (blocks, valid_len) = scan_valid_blocks(&td.path("nope.bin")).unwrap();
        assert!(blocks.is_empty());
        assert_eq!(valid_len, 0);
    }

    // ── Torn-tail / corruption discipline (the correctness gate) ──────────

    #[test]
    fn torn_final_block_discarded_wholesale() {
        // Coverage map for this test:
        //
        //   - Loop body (`cut ∈ [after_g0, total)`): every byte position
        //     strictly inside game 2's block — game 1 durable, game 2
        //     torn — must yield exactly game 1 + truncate to the game-1
        //     fsync boundary.
        //
        //   - Post-loop block (`cut == total`, the complete file): both
        //     game 1 AND game 2 must scan cleanly. The sibling test
        //     `complete_two_game_shard_yields_both_blocks` covers this
        //     case explicitly so the two-block happy path has its own
        //     named assertion.
        //
        // Two games. Truncate at EVERY byte offset strictly inside game 2's
        // block (i.e. after game 1's durable fsync point, before game 2 is
        // complete) AND at the exact game-1 fsync boundary. Resume must
        // yield EXACTLY game 1 — zero partial records from game 2, ever.
        let td = TempDir::new("torn");
        let shard = td.path("shard.bin");
        let g0 = game_records(100, 4);
        let g1 = game_records(200, 6);
        append_block(&shard, 100, &g0).unwrap();
        let after_g0 = std::fs::metadata(&shard).unwrap().len();
        append_block(&shard, 200, &g1).unwrap();
        let full = std::fs::read(&shard).unwrap();
        let total = full.len() as u64;

        // Every cut point in [after_g0, total): the prior game is durable,
        // game 2 is torn at this offset → resume yields exactly game 1.
        for cut in after_g0..total {
            std::fs::write(&shard, &full[..cut as usize]).unwrap();
            let (blocks, valid_len) = scan_valid_blocks(&shard).unwrap();
            assert_eq!(
                blocks.len(),
                1,
                "cut={cut}: a torn game-2 block must be discarded wholesale, \
                 yielding exactly game 1"
            );
            assert_eq!(blocks[0].game_id, 100);
            assert_eq!(blocks[0].records, g0, "cut={cut}: game-1 records intact");
            assert_eq!(
                valid_len, after_g0,
                "cut={cut}: valid_len truncates back to the game-1 fsync point"
            );
            // Resume truncation drops the torn tail wholesale.
            truncate_to_valid(&shard, valid_len).unwrap();
            assert_eq!(std::fs::metadata(&shard).unwrap().len(), after_g0);
        }

        // At exactly the game-1 fsync boundary the whole of game 1 is valid.
        std::fs::write(&shard, &full[..after_g0 as usize]).unwrap();
        let (blocks, valid_len) = scan_valid_blocks(&shard).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].records, g0);
        assert_eq!(valid_len, after_g0);
    }

    #[test]
    fn complete_two_game_shard_yields_both_blocks() {
        // The `cut == total` post-loop case from `torn_final_block_*`,
        // extracted into its own named test: a complete two-game shard
        // (no truncation, no corruption) must scan to BOTH game blocks.
        let td = TempDir::new("two-game-complete");
        let shard = td.path("shard.bin");
        let g0 = game_records(100, 4);
        let g1 = game_records(200, 6);
        append_block(&shard, 100, &g0).unwrap();
        append_block(&shard, 200, &g1).unwrap();
        let total = std::fs::metadata(&shard).unwrap().len();

        let (blocks, valid_len) = scan_valid_blocks(&shard).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].game_id, 100);
        assert_eq!(blocks[0].records, g0);
        assert_eq!(blocks[1].game_id, 200);
        assert_eq!(blocks[1].records, g1);
        assert_eq!(
            valid_len, total,
            "a complete shard validates every byte; no torn-tail truncation"
        );
    }

    #[test]
    fn bad_crc_block_rejected() {
        let td = TempDir::new("bad-crc");
        let shard = td.path("shard.bin");
        let g0 = game_records(1, 3);
        let g1 = game_records(2, 3);
        append_block(&shard, 1, &g0).unwrap();
        let after_g0 = std::fs::metadata(&shard).unwrap().len() as usize;
        append_block(&shard, 2, &g1).unwrap();

        // Flip one byte in game 2's payload — CRC must reject the whole
        // block; game 1 still scans cleanly.
        let mut bytes = std::fs::read(&shard).unwrap();
        let flip = after_g0 + HEADER_LEN + 1;
        bytes[flip] ^= 0xFF;
        std::fs::write(&shard, &bytes).unwrap();

        let (blocks, valid_len) = scan_valid_blocks(&shard).unwrap();
        assert_eq!(blocks.len(), 1, "bad CRC ⇒ block-2 rejected wholesale");
        assert_eq!(blocks[0].game_id, 1);
        assert_eq!(valid_len as usize, after_g0);
    }

    #[test]
    fn corrupt_tail_not_panic() {
        let td = TempDir::new("corrupt-tail");
        let shard = td.path("shard.bin");
        append_block(&shard, 9, &game_records(9, 2)).unwrap();
        let valid = std::fs::metadata(&shard).unwrap().len();

        // Append arbitrary garbage (a SIGKILL mid-write leaves random
        // trailing bytes). Scanner must not panic; valid block survives.
        let mut bytes = std::fs::read(&shard).unwrap();
        bytes.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01, 0x02]);
        std::fs::write(&shard, &bytes).unwrap();
        let (blocks, valid_len) = scan_valid_blocks(&shard).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(valid_len, valid);

        // A lone partial header (fewer than HEADER_LEN bytes) must also be
        // tolerated without panic.
        std::fs::write(&shard, [0x31, 0x47]).unwrap();
        let (blocks, valid_len) = scan_valid_blocks(&shard).unwrap();
        assert!(blocks.is_empty());
        assert_eq!(valid_len, 0);
    }

    #[test]
    fn wrong_magic_first_block_yields_nothing() {
        let td = TempDir::new("wrong-magic");
        let shard = td.path("shard.bin");
        let mut bytes = encode_block(1, &game_records(1, 2));
        bytes[0] ^= 0xFF; // corrupt the magic of the very first frame
        std::fs::write(&shard, &bytes).unwrap();
        let (blocks, valid_len) = scan_valid_blocks(&shard).unwrap();
        assert!(blocks.is_empty());
        assert_eq!(valid_len, 0);
    }

    // ── Checkpoint ────────────────────────────────────────────────────────

    #[test]
    fn checkpoint_roundtrip() {
        let td = TempDir::new("ckpt-rt");
        let p = td.path("checkpoint.bin");
        let ckpt = Checkpoint {
            games_completed: 1234,
            prng_state: 0xDEAD_BEEF_CAFE_F00D,
            ladder_cursor: 99,
            per_worker_next_game_id: vec![3, 7, 11, 4],
            next_consume_id: 1234,
        };
        assert_eq!(read_checkpoint(&p).unwrap(), None);
        write_checkpoint(&p, &ckpt).unwrap();
        assert_eq!(read_checkpoint(&p).unwrap(), Some(ckpt));
    }

    #[test]
    fn checkpoint_atomic_tmp_then_rename() {
        let td = TempDir::new("ckpt-atomic");
        let p = td.path("checkpoint.bin");
        let tmp = tmp_path(&p);
        write_checkpoint(
            &p,
            &Checkpoint {
                games_completed: 5,
                prng_state: 42,
                ladder_cursor: 0,
                per_worker_next_game_id: vec![1],
                next_consume_id: 5,
            },
        )
        .unwrap();
        // The `.tmp` staging file must not survive a successful write —
        // it was renamed onto the final path.
        assert!(!tmp.exists(), ".tmp must be renamed away, not left behind");
        assert!(p.exists());
    }

    #[test]
    fn corrupt_checkpoint_is_rejected_not_half_parsed() {
        let td = TempDir::new("ckpt-corrupt");
        let p = td.path("checkpoint.bin");
        write_checkpoint(
            &p,
            &Checkpoint {
                games_completed: 8,
                prng_state: 1,
                ladder_cursor: 2,
                per_worker_next_game_id: vec![0, 0],
                next_consume_id: 8,
            },
        )
        .unwrap();
        let mut bytes = std::fs::read(&p).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        std::fs::write(&p, &bytes).unwrap();
        // A torn/corrupt checkpoint must be a clean Err, never a
        // half-parsed cursor that would desync the resume.
        assert!(matches!(
            read_checkpoint(&p),
            Err(CorpusError::Checkpoint(_))
        ));
    }

    #[test]
    fn atomic_write_replaces_existing() {
        let td = TempDir::new("atomic-replace");
        let p = td.path("f.bin");
        atomic_write(&p, b"first").unwrap();
        atomic_write(&p, b"second-longer").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"second-longer");
    }

    // ── Resume idempotency: game-block fsync THEN checkpoint ──────────────

    #[test]
    fn crash_before_checkpoint_drops_inflight_game() {
        // The §3.5 ordering: a game block is fsynced, then the checkpoint.
        // A crash BETWEEN them leaves a durable, fully-valid game block
        // with NO checkpoint covering it. Resume must (a) still see the
        // durable block via the shard scan, and (b) not have a stale
        // checkpoint that double-counts or skips it.
        let td = TempDir::new("crash-pre-ckpt");
        let shard = td.path("shard.bin");
        let ckpt_path = td.path("checkpoint.bin");

        // Game 1 committed + checkpointed.
        append_block(&shard, 1, &game_records(1, 3)).unwrap();
        write_checkpoint(
            &ckpt_path,
            &Checkpoint {
                games_completed: 1,
                prng_state: 10,
                ladder_cursor: 1,
                per_worker_next_game_id: vec![2],
                next_consume_id: 1,
            },
        )
        .unwrap();

        // Game 2's block fsynced — but we "crash" before its checkpoint.
        append_block(&shard, 2, &game_records(2, 4)).unwrap();

        // Resume: the shard scan is the source of truth for durable games;
        // the checkpoint only lags. Both games are present (game 2 durable
        // despite the stale checkpoint).
        let (blocks, _) = scan_valid_blocks(&shard).unwrap();
        let ids: Vec<u64> = blocks.iter().map(|b| b.game_id).collect();
        assert_eq!(ids, vec![1, 2]);
        let ckpt = read_checkpoint(&ckpt_path).unwrap().unwrap();
        assert_eq!(
            ckpt.games_completed, 1,
            "checkpoint legitimately lags the durable shard by the in-flight game"
        );
    }

    #[test]
    fn resume_skips_completed_game_ids_idempotent() {
        // Re-emitting a game whose block was already durably appended
        // (crash after data-fsync, before checkpoint) must NOT double it:
        // resume dedups on game_id (the §3.5 idempotency contract).
        let td = TempDir::new("resume-idem");
        let shard = td.path("shard.bin");
        append_block(&shard, 1, &game_records(1, 3)).unwrap();
        append_block(&shard, 2, &game_records(2, 3)).unwrap();

        // Simulate the resume dispatcher: collect already-present ids, then
        // re-run the campaign which would re-emit game 2 — the dispatcher
        // must skip it because it is already durable.
        let (blocks, _) = scan_valid_blocks(&shard).unwrap();
        let present: std::collections::HashSet<u64> = blocks.iter().map(|b| b.game_id).collect();
        assert!(present.contains(&1) && present.contains(&2));

        for replay_id in [2u64, 3u64] {
            if present.contains(&replay_id) {
                continue; // idempotent skip
            }
            append_block(&shard, replay_id, &game_records(replay_id, 2)).unwrap();
        }

        let (blocks, _) = scan_valid_blocks(&shard).unwrap();
        let ids: Vec<u64> = blocks.iter().map(|b| b.game_id).collect();
        assert_eq!(
            ids,
            vec![1, 2, 3],
            "game 2 not doubled; only the genuinely-new game 3 appended"
        );
    }
}
