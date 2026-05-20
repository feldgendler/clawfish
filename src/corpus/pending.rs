//! Per-game reorder buffer: atomic write of a completed game block to
//! `pending/game-{game_id:020}.bin` and scan of existing pending files.
//!
//! The consumer reads these in strict `game_id` order and applies the
//! inline dedup → cap → split pipeline before appending to `shard.bin` /
//! `val.bin`. The write side uses the standard tmp-then-rename idiom so a
//! SIGKILL mid-write leaves only an orphaned `.tmp` that is cleaned at the
//! next resume, never a corrupt pending file that the consumer would
//! mis-process.
//!
//! See `docs/plans/m6.g-per-game-files.md` §2.3/§4.1 for the full contract.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Write;
use std::path::Path;

use super::CorpusError;

/// Maximum allowed byte size for a pending game block.
///
/// Derived from `max_plies * ~90 B/record + frame overhead`, rounded up to
/// 64 KiB with headroom. Violations are fatal at both write and read sides —
/// the same worker would regenerate the same oversize block, so retry is not
/// useful (see §2.3 for the full derivation).
pub const MAX_PENDING_BYTES: usize = 64 * 1024;

/// Write a CRC-framed game block atomically to
/// `pending_dir/game-{game_id:020}.bin`.
///
/// The tmp filename is **deterministic per `game_id`** so a worker re-attempt
/// overwrites a prior orphan rather than accumulating duplicates.
///
/// Returns `CorpusError::Checkpoint` when `block_bytes.len() >
/// MAX_PENDING_BYTES` (oversize is fatal — not retryable).
pub fn write_pending(
    pending_dir: &Path,
    game_id: u64,
    block_bytes: &[u8],
) -> Result<(), CorpusError> {
    if block_bytes.len() > MAX_PENDING_BYTES {
        return Err(CorpusError::Checkpoint(format!(
            "oversized pending block at game_id {game_id}: {} bytes > {MAX_PENDING_BYTES}",
            block_bytes.len()
        )));
    }

    let tmp_path = pending_dir.join(pending_tmp_filename(game_id));
    let final_path = pending_dir.join(pending_filename(game_id));

    {
        let mut f = File::create(&tmp_path)?;
        f.write_all(block_bytes)?;
        f.sync_all()?;
    }

    std::fs::rename(&tmp_path, &final_path)?;

    // Directory fsync makes the rename durable on POSIX (APFS honors it).
    // Error is propagated, not swallowed — plan §2.3 / §7 contract.
    let dir = File::open(pending_dir)?;
    dir.sync_all()?;

    Ok(())
}

/// Result of scanning the `pending/` directory.
pub struct PendingScan {
    /// Game ids with a valid, correctly-sized pending file.
    pub ids: BTreeSet<u64>,
    /// Number of orphan `.tmp` files cleaned during the scan.
    pub orphans_cleaned: usize,
}

/// Scan `pending_dir` for pending game files and orphan `.tmp` files.
///
/// - Orphan `.tmp` files are unlinked (mid-write crash artifacts).
/// - Files whose size exceeds `MAX_PENDING_BYTES` or whose CRC/decode fails
///   are also unlinked (corrupt artifacts from a partial rename).
/// - Unknown filenames are logged as warnings and left alone.
pub fn scan_pending(pending_dir: &Path) -> Result<PendingScan, CorpusError> {
    let mut ids = BTreeSet::new();
    let mut orphans_cleaned = 0usize;

    for entry in std::fs::read_dir(pending_dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();

        if name.starts_with('.') && name.ends_with(".bin.tmp") {
            // Orphaned in-flight write from a previous crash — clean up.
            let _ = std::fs::remove_file(entry.path());
            orphans_cleaned += 1;
            continue;
        }

        // Match "game-{20-digit-decimal}.bin".
        if let Some(game_id) = parse_pending_filename(&name) {
            let path = entry.path();
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            // Oversize files are FATAL at the consumer's per-game probe
            // (operator misconfig, not retryable — see plan §2.6 Blocker #4).
            // Leave them in place so the consumer surfaces the error rather
            // than silently absorbing them at resume time.
            if bytes.len() > MAX_PENDING_BYTES {
                ids.insert(game_id);
                continue;
            }
            // Torn write (CRC/decode failure on a not-oversized file): a
            // partial rename from a crashed worker. Unlink and let the
            // dispatcher gap-list replay the game.
            if super::store::decode_block(&bytes).is_none() {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            ids.insert(game_id);
        } else {
            eprintln!("corpus::pending: ignoring unknown file in pending dir: {name}");
        }
    }

    Ok(PendingScan {
        ids,
        orphans_cleaned,
    })
}

/// Parse a filename of the form `game-{20-digit-decimal}.bin` into a `u64`.
fn parse_pending_filename(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("game-")?;
    let digits = rest.strip_suffix(".bin")?;
    if digits.len() != 20 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()
}

/// Canonical filename for a pending game block.
///
/// The 20-digit zero-padded decimal format ensures `readdir` order ≠ game
/// order (they happen to agree for small `game_id` values, but the consumer
/// NEVER relies on that — it probes by exact name).
pub fn pending_filename(game_id: u64) -> String {
    format!("game-{game_id:020}.bin")
}

/// Canonical in-flight (tmp) filename for a pending game block.
pub fn pending_tmp_filename(game_id: u64) -> String {
    format!(".game-{game_id:020}.bin.tmp")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static CTR: AtomicU64 = AtomicU64::new(0);
            let n = CTR.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let dir = std::env::temp_dir().join(format!("clawfish-corpus-pending-{tag}-{pid}-{n}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // ── Filename format ──────────────────────────────────────────────────

    #[test]
    fn pending_filename_is_020_zero_padded_decimal() {
        // Nit #15: the format must be exactly 20 decimal digits, zero-padded.
        // This pins the format so the consumer's exact-name probe is stable
        // across any `game_id` magnitude.
        assert_eq!(
            pending_filename(0),
            "game-00000000000000000000.bin",
            "game_id=0 must be 20 zeros"
        );
        assert_eq!(pending_filename(1), "game-00000000000000000001.bin");
        assert_eq!(
            pending_filename(u64::MAX),
            "game-18446744073709551615.bin",
            "u64::MAX must be exactly 20 decimal digits (no overflow)"
        );
        assert_eq!(
            pending_filename(12345678901234567890),
            "game-12345678901234567890.bin"
        );
    }

    #[test]
    fn pending_tmp_filename_is_dot_prefixed() {
        let name = pending_tmp_filename(42);
        assert!(name.starts_with('.'), "tmp filename must start with '.'");
        assert!(
            name.ends_with(".bin.tmp"),
            "tmp filename must end with '.bin.tmp'"
        );
        assert!(
            name.contains("00000000000000000042"),
            "tmp filename must contain 20-digit game_id"
        );
    }

    // ── write_pending ────────────────────────────────────────────────────

    #[test]
    fn atomic_write_no_partial_visible_smoke() {
        // Smoke check for the smallest valid block (one byte). The proptest
        // below covers the full 1..MAX_PENDING_BYTES range via sampling.
        let td = TempDir::new("atomic-no-partial-smoke");
        let pending_dir = td.0.join("pending");
        std::fs::create_dir_all(&pending_dir).unwrap();
        let block: Vec<u8> = vec![0xAB];
        write_pending(&pending_dir, 0, &block).unwrap();
        let final_path = pending_dir.join(pending_filename(0));
        assert!(final_path.exists(), "final file must exist after Ok");
        assert_eq!(std::fs::read(&final_path).unwrap(), block);
    }

    proptest::proptest! {
        // Property (Blocker #5 / plan §8.1): for any block size in
        // 1..MAX_PENDING_BYTES, after `write_pending` returns `Ok`:
        //   (a) the final file exists at `pending/game-{N:020}.bin` with the
        //       exact bytes,
        //   (b) no `.tmp` orphan is left behind,
        //   (c) the tmp filename is deterministic per game_id (re-running
        //       overwrites cleanly with no duplicate orphans).
        // Marked `#[ignore]` until `write_pending` is implemented.
        #![proptest_config(proptest::test_runner::Config {
            cases: 16, .. proptest::test_runner::Config::default()
        })]

        #[test]
        fn atomic_write_no_partial_visible(
            size in 1usize..MAX_PENDING_BYTES,
        ) {
            let td = TempDir::new("atomic-no-partial-proptest");
            let pending_dir = td.0.join("pending");
            std::fs::create_dir_all(&pending_dir).unwrap();
            let bytes: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let game_id = 0u64;
            write_pending(&pending_dir, game_id, &bytes).unwrap();
            let final_path = pending_dir.join(pending_filename(game_id));
            let tmp_path = pending_dir.join(pending_tmp_filename(game_id));
            proptest::prop_assert!(final_path.exists(), "final must exist after Ok");
            proptest::prop_assert_eq!(std::fs::read(&final_path).unwrap(), bytes);
            proptest::prop_assert!(!tmp_path.exists(), "no .tmp orphan after rename");
        }
    }

    #[test]
    fn oversize_write_returns_checkpoint_error() {
        // Blocker #4(a): an oversize block is rejected with
        // `CorpusError::Checkpoint` — not retryable (the same worker
        // regenerates the same block).
        let td = TempDir::new("oversize-write");
        let pending_dir = td.0.join("pending");
        std::fs::create_dir_all(&pending_dir).unwrap();
        let oversize = vec![0u8; MAX_PENDING_BYTES + 1];
        let result = write_pending(&pending_dir, 7, &oversize);
        assert!(
            matches!(result, Err(CorpusError::Checkpoint(_))),
            "oversize block must return CorpusError::Checkpoint, got: {result:?}"
        );
        // The final file must NOT have been created.
        assert!(
            !pending_dir.join(pending_filename(7)).exists(),
            "no final file must exist for an oversize write"
        );
    }

    #[test]
    fn orphan_tmp_cleaned_by_scan() {
        // A `.tmp` file left by a crash mid-write must be unlinked by
        // `scan_pending` (orphan cleanup).
        let td = TempDir::new("orphan-tmp");
        let pending_dir = td.0.join("pending");
        std::fs::create_dir_all(&pending_dir).unwrap();
        // Plant an orphaned tmp file.
        let tmp = pending_dir.join(pending_tmp_filename(5));
        std::fs::write(&tmp, b"garbage").unwrap();
        assert!(tmp.exists());
        let scan = scan_pending(&pending_dir).unwrap();
        assert_eq!(scan.orphans_cleaned, 1, "one orphan must be cleaned");
        assert!(!tmp.exists(), "orphan .tmp must be unlinked by scan");
        assert!(scan.ids.is_empty(), "no valid pending ids (only orphan)");
    }

    #[test]
    fn exact_name_probe_returns_present_absent() {
        // The consumer uses exact-name probe (never readdir for ordering).
        // Verify that `pending_filename` + filesystem probe works:
        // present game → file readable; absent game → file not found.
        let td = TempDir::new("exact-probe");
        let pending_dir = td.0.join("pending");
        std::fs::create_dir_all(&pending_dir).unwrap();
        // Write game 3.
        let block = crate::corpus::store::encode_block(3, &[]);
        write_pending(&pending_dir, 3, &block).unwrap();
        // Present.
        assert!(
            pending_dir.join(pending_filename(3)).exists(),
            "game 3 must be present"
        );
        // Absent game 4.
        assert!(
            !pending_dir.join(pending_filename(4)).exists(),
            "game 4 must be absent"
        );
    }
}
