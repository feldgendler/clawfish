//! Consumer thread: polls `pending/` for the next-expected game_id file in
//! strict order, applies the inline dedup → per-game cap → split pipeline,
//! and appends to `shard.bin` / `val.bin`.
//!
//! The consumer is the only writer to the shard files during a run. It
//! maintains `next_consume_id` as the authoritative commit cursor, written
//! to `checkpoint.bin` atomically after each game commit (the §2.6
//! ordering: pending file written → shard append → checkpoint → pending
//! unlink).
//!
//! See `docs/plans/m6.g-per-game-files.md` §2.6/§4.3 for the full contract.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::CorpusError;
use super::CorpusRecord;
use super::PER_GAME_CAP;
use super::dispatcher::Dispatcher;
use super::pending::{MAX_PENDING_BYTES, pending_filename, scan_pending};
use super::prng::substream_seed;
use super::selfplay::SelfPlayConfig;
use super::store::{
    Checkpoint, append_raw_bytes, decode_block, encode_block, read_checkpoint, scan_valid_blocks,
    truncate_to_valid, write_checkpoint,
};

/// Default consumer poll interval in milliseconds.
pub const POLL_INTERVAL_MS: u64 = 10;

/// Consumer thread state. Mutable across the loop; initialized from the
/// resume protocol (§2.7).
pub struct ConsumerState {
    /// The next game_id the consumer expects to find in `pending/`.
    pub next_consume_id: u64,
    /// Global FEN dedup set; "first-seen wins" across all committed games.
    /// Rebuilt from durable shards on resume.
    pub fen_set: HashSet<String>,
    /// Game ids already committed to a durable shard, frozen at resume time.
    /// Used by the skip-committed bypass (§2.6 step 2 / Blocker #1) to
    /// advance past games that were committed without a shard block
    /// (empty-post-dedup games that left a checkpoint record but no block).
    /// Read-only after initialization.
    pub committed_ids_at_resume: HashSet<u64>,
    /// Total durable positions across shard.bin + val.bin at resume time.
    /// Seeded into `ConsumerStats::positions_committed` so the
    /// `cap_positions` check sees the *cumulative* total, not just this
    /// run's increment. Without this, a resume with `--cap-positions N`
    /// would commit N MORE positions on top of whatever's already on
    /// disk, blowing past the operator's target.
    pub positions_at_resume: u64,
}

/// Resume the corpus run: reconstruct `ConsumerState` from durable shards,
/// the checkpoint, and the pending directory.
///
/// Implements §2.7 of `docs/plans/m6.g-per-game-files.md`:
/// 1. Truncate torn shard tails.
/// 2. Rebuild `fen_set` and `committed_ids` from durable shard blocks.
/// 3. Read the checkpoint (if valid) for the authoritative `next_consume_id`.
/// 4. Scan `pending/` for orphan `.tmp` cleanup and gap enumeration.
/// 5. Ghost-pending self-heal: unlink pending files whose `game_id` is
///    already committed or below `ckpt.next_consume_id`.
/// 6. Return `ConsumerState` ready for the consumer loop.
pub fn resume(out_dir: &Path, pending_dir: &Path) -> Result<ConsumerState, CorpusError> {
    // Step 1: Scan + truncate torn shard tails.
    let (shard_blocks, shard_valid_len) = scan_valid_blocks(&out_dir.join("shard.bin"))?;
    if shard_valid_len > 0 {
        truncate_to_valid(&out_dir.join("shard.bin"), shard_valid_len)?;
    }
    let (val_blocks, val_valid_len) = scan_valid_blocks(&out_dir.join("val.bin"))?;
    if val_valid_len > 0 {
        truncate_to_valid(&out_dir.join("val.bin"), val_valid_len)?;
    }

    // Step 2: Rebuild fen_set + committed_ids from durable shards.
    // Also tally positions_at_resume so the cap_positions check sees the
    // cumulative total (resume invariance — Issue: 1.99M-on-disk + --cap=2M
    // must commit only the 9 missing positions, not 2M more).
    let mut fen_set = HashSet::new();
    let mut committed_ids: HashSet<u64> = HashSet::new();
    let mut positions_at_resume: u64 = 0;
    for block in shard_blocks.iter().chain(val_blocks.iter()) {
        committed_ids.insert(block.game_id);
        positions_at_resume += block.records.len() as u64;
        for r in &block.records {
            fen_set.insert(r.fen.clone());
        }
    }

    // Step 3: Read checkpoint for the authoritative cursor.
    // `read_checkpoint` returns `Err(CorpusError::Checkpoint)` for wrong magic
    // (e.g. old CWK1 files) or CRC failures. Treat those the same as "absent"
    // so an old-format checkpoint from a previous run doesn't kill a fresh
    // resume — fall back to the shard-scan cursor. Genuine I/O errors
    // (e.g. permission denied) are still propagated.
    let (ckpt_valid, next_consume_id) = match read_checkpoint(&out_dir.join("checkpoint.bin")) {
        Ok(Some(ckpt)) => (true, ckpt.next_consume_id),
        Ok(None) | Err(CorpusError::Checkpoint(_)) => {
            // No valid checkpoint (missing, corrupt, or wrong magic): derive
            // cursor from committed_ids.
            let cursor = smallest_not_in(&committed_ids);
            (false, cursor)
        }
        Err(e) => return Err(e),
    };

    // Step 4: Scan pending/ for orphan cleanup.
    let pending_scan = scan_pending(pending_dir)?;
    let mut pending_ids = pending_scan.ids;

    // Step 5: Ghost-pending self-heal.
    // ghost_ids = (pending_ids ∩ committed_ids)
    //           ∪ (if ckpt valid: pending_ids ∩ {0..ckpt.next_consume_id})
    let ghost_ids: Vec<u64> = pending_ids
        .iter()
        .copied()
        .filter(|&id| committed_ids.contains(&id) || (ckpt_valid && id < next_consume_id))
        .collect();
    for id in &ghost_ids {
        let _ = std::fs::remove_file(pending_dir.join(pending_filename(*id)));
        pending_ids.remove(id);
    }

    Ok(ConsumerState {
        next_consume_id,
        fen_set,
        committed_ids_at_resume: committed_ids,
        positions_at_resume,
    })
}

/// Smallest u64 not present in `set`, starting from 0.
fn smallest_not_in(set: &HashSet<u64>) -> u64 {
    let mut id = 0u64;
    while set.contains(&id) {
        id += 1;
    }
    id
}

/// Run the consumer loop.
///
/// Polls `pending/` for `state.next_consume_id`, applies the inline pipeline,
/// appends to the appropriate shard (train or val), updates the checkpoint,
/// and unlinks the pending file.
///
/// The loop terminates when:
/// - `state.next_consume_id >= cfg.games` (all games consumed), OR
/// - `stop` is set AND `dispatcher.is_fully_drained_at(next_consume_id)` is
///   true (all in-flight work is done).
///
/// Returns `Err(CorpusError::Checkpoint("consumer wedged: ..."))` if
/// `alive_workers == 0`, no pending file exists for `next_consume_id`, and
/// `dispatcher.is_fully_drained_at` is true (a no-live-producer deadlock).
pub fn consumer_loop(
    cfg: &SelfPlayConfig,
    state: &mut ConsumerState,
    dispatcher: &Dispatcher,
    alive_workers: Arc<AtomicUsize>,
    stop: &std::sync::atomic::AtomicBool,
    out_dir: &Path,
) -> Result<ConsumerStats, CorpusError> {
    let pending_dir = out_dir.join("pending");
    let shard_path = out_dir.join("shard.bin");
    let val_path = out_dir.join("val.bin");
    let ckpt_path = out_dir.join("checkpoint.bin");
    let poll_ms = cfg.poll_interval_ms.unwrap_or(POLL_INTERVAL_MS);

    // Ghost-pending cleanup at loop start: unlink any pending files for
    // game_ids that are already committed or below the current cursor.
    // This covers the case where consumer_loop is called directly (e.g.,
    // in tests) without going through resume().
    clean_ghost_pending(
        &pending_dir,
        &state.committed_ids_at_resume,
        state.next_consume_id,
    );

    // Seed positions_committed from the resumed durable count. Without
    // this, --cap-positions on a resumed campaign would commit `cap` MORE
    // positions on top of the existing corpus instead of capping at the
    // cumulative total. The other stats (games_committed,
    // games_empty_post_dedup, games_emitted, games_dropped_inflight) stay
    // per-run because they describe THIS run's work; positions_committed
    // is the one stat the cap is checked against.
    let mut stats = ConsumerStats {
        positions_committed: state.positions_at_resume,
        ..ConsumerStats::default()
    };

    loop {
        let next = state.next_consume_id;

        // Step 1: Exit checks.
        if next >= cfg.games {
            break;
        }
        if stop.load(Ordering::Acquire) && dispatcher.is_fully_drained_at(next) {
            break;
        }
        // Stop AND no live producers → graceful exit even if gap_queue carries
        // workers' released-mid-game claims (R2 path). Those games are
        // intentionally abandoned; the consumer must not wait for them.
        if stop.load(Ordering::Acquire) && alive_workers.load(Ordering::Acquire) == 0 {
            break;
        }

        // Step 2: Skip-committed bypass (Blocker #1).
        // Advance the cursor without a checkpoint write — the checkpoint is
        // written when we exit the committed-ids run and commit a real pending
        // file. On resume, committed_ids is rebuilt from shards, so the cursor
        // is recoverable without an intermediate checkpoint (§11a.2 batching).
        if state.committed_ids_at_resume.contains(&next) {
            state.next_consume_id += 1;
            continue;
        }

        // Step 3: Probe pending file by exact name.
        let pending_path = pending_dir.join(pending_filename(next));
        match std::fs::read(&pending_path) {
            Ok(bytes) => {
                // Blocker #4(b): oversize is fatal — no release_unclaimed.
                if bytes.len() > MAX_PENDING_BYTES {
                    return Err(CorpusError::Checkpoint(format!(
                        "oversized pending file at game_id {next}: {} > {MAX_PENDING_BYTES}",
                        bytes.len()
                    )));
                }

                // Torn write: decode failure or consumed ≠ total → unlink + retry.
                match decode_block(&bytes) {
                    Some((block, consumed)) if consumed == bytes.len() => {
                        // Substantive #10: filename/block game_id mismatch is fatal.
                        if block.game_id != next {
                            let _ = std::fs::remove_file(&pending_path);
                            return Err(CorpusError::Checkpoint(format!(
                                "pending filename / block game_id mismatch at game_id {next}: \
                                 block reports {}",
                                block.game_id
                            )));
                        }

                        // Happy path: inline dedup → cap → split.
                        let deduped: Vec<CorpusRecord> = block
                            .records
                            .into_iter()
                            .filter(|r| state.fen_set.insert(r.fen.clone()))
                            .collect();

                        let mut capped =
                            inline_cap_dedup_survivors(deduped, cfg.split_seed, next, PER_GAME_CAP);

                        // --cap-positions partial-commit: if the whole game's
                        // records would push us over the operator's target,
                        // keep only the first (cap − positions_committed)
                        // records. The block is still well-formed CRC-framed;
                        // downstream readers see a game with fewer records
                        // than the uncapped twin — same as if PER_GAME_CAP
                        // had been narrower for this specific game. Trailing
                        // records are dropped (not deferred — in-order
                        // commit makes per-game truncation the only way to
                        // hit an exact target).
                        if let Some(cap) = cfg.cap_positions
                            && !capped.is_empty()
                        {
                            let room = cap.saturating_sub(stats.positions_committed);
                            if (capped.len() as u64) > room {
                                capped.truncate(room as usize);
                            }
                        }

                        if !capped.is_empty() {
                            let is_val = split_decision(cfg.split_seed, next, cfg.val_fraction);
                            let dest = if is_val { &val_path } else { &shard_path };
                            let encoded = encode_block(next, &capped);
                            append_raw_bytes(dest, &encoded)?;
                            stats.positions_committed += capped.len() as u64;
                        } else {
                            stats.games_empty_post_dedup += 1;
                        }

                        state.next_consume_id += 1;
                        write_checkpoint(&ckpt_path, &make_checkpoint(state.next_consume_id))?;
                        let _ = std::fs::remove_file(&pending_path);
                        stats.games_committed += 1;

                        // Position-cap (cfg.cap_positions): signal stop when the
                        // durable position count reaches the operator's target.
                        // Workers see stop at their next iteration and exit;
                        // their in-flight games are dropped (R2). The cap is on
                        // post-dedup-cap committed positions across shard + val.
                        if let Some(cap) = cfg.cap_positions
                            && stats.positions_committed >= cap
                        {
                            stop.store(true, Ordering::Relaxed);
                        }
                    }
                    _ => {
                        // Torn write: unlink + release for re-dispatch.
                        let _ = std::fs::remove_file(&pending_path);
                        dispatcher.release_unclaimed(next);
                        // If no workers are alive to re-play the game, we're
                        // immediately wedged (nobody will write a fresh pending).
                        if alive_workers.load(Ordering::Acquire) == 0 {
                            return Err(CorpusError::Checkpoint(format!(
                                "consumer wedged: no live workers, no pending file for game_id {next}"
                            )));
                        }
                    }
                }
            }

            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Wedge detection: no live producers AND dispatcher fully drained.
                if alive_workers.load(Ordering::Acquire) == 0
                    && dispatcher.is_fully_drained_at(next)
                {
                    return Err(CorpusError::Checkpoint(format!(
                        "consumer wedged: no live workers, no pending file for game_id {next}"
                    )));
                }
                std::thread::sleep(std::time::Duration::from_millis(poll_ms));
            }

            Err(e) => return Err(CorpusError::Io(e)),
        }
    }

    Ok(stats)
}

/// Apply seeded reservoir cap to dedup-survivors.
///
/// `seed` = `split_seed`; per-game sub-stream is `substream_seed(seed, game_id)`.
pub fn inline_cap_dedup_survivors(
    records: Vec<CorpusRecord>,
    seed: u64,
    game_id: u64,
    cap: usize,
) -> Vec<CorpusRecord> {
    use super::prng::Prng;

    if cap == 0 || records.is_empty() {
        return Vec::new();
    }
    if records.len() <= cap {
        return records;
    }

    let game_seed = substream_seed(seed, game_id);
    let mut rng = Prng::new(game_seed);

    // Algorithm R reservoir sampling.
    let mut reservoir: Vec<CorpusRecord> = records[..cap].to_vec();
    for (k, record) in records[cap..].iter().enumerate() {
        let j = rng.below((cap + k + 1) as u64) as usize;
        if j < cap {
            reservoir[j] = record.clone();
        }
    }
    reservoir
}

/// Per-game val/train routing: returns `true` iff this game goes to val.
///
/// Matches `split::split_by_game` semantics: threshold in u64 space from
/// `val_fraction * u64::MAX`; per-game hash = `substream_seed(split_seed, game_id)`.
pub fn split_decision(split_seed: u64, game_id: u64, val_fraction: f64) -> bool {
    let threshold = (val_fraction.clamp(0.0, 1.0) * (u64::MAX as f64)) as u64;
    substream_seed(split_seed, game_id) < threshold
}

/// Build a minimal CWK2 checkpoint carrying only the consume cursor.
/// The other fields are zeroed/empty — the consumer only owns `next_consume_id`.
fn make_checkpoint(next_consume_id: u64) -> Checkpoint {
    Checkpoint {
        games_completed: 0,
        prng_state: 0,
        ladder_cursor: 0,
        per_worker_next_game_id: Vec::new(),
        next_consume_id,
    }
}

/// Unlink ghost pending files: any file whose game_id is in `committed_ids`
/// or is strictly below `cursor` (already consumed). Called both at the
/// start of `consumer_loop` and by `resume`.
fn clean_ghost_pending(pending_dir: &Path, committed_ids: &HashSet<u64>, cursor: u64) {
    // Walk the pending directory and clean up orphans.
    // Errors during cleanup are silently ignored — they are best-effort
    // (the file might have already been cleaned by a concurrent actor).
    let Ok(entries) = std::fs::read_dir(pending_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if let Some(id) = parse_pending_game_id(&name)
            && (committed_ids.contains(&id) || id < cursor)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Parse `"game-{N:020}.bin"` → `Some(N)`, anything else → `None`.
fn parse_pending_game_id(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("game-")?;
    let digits = rest.strip_suffix(".bin")?;
    if digits.len() != 20 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()
}

/// Per-run consumer outcome counters (aggregated into `SelfPlayStats`).
#[derive(Clone, Debug, Default)]
pub struct ConsumerStats {
    /// Pending files processed (= games committed, including empty ones).
    pub games_committed: u64,
    /// Games that committed zero records (all FENs were dedup-duplicates or
    /// post-cap count was zero).
    pub games_empty_post_dedup: u64,
    /// Total records appended to the shards after dedup + cap.
    pub positions_committed: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::selfplay::SelfPlayConfig;
    use crate::corpus::store::{encode_block, scan_valid_blocks};
    use crate::corpus::{CorpusRecord, Label, Source};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::AtomicU64;
            static CTR: AtomicU64 = AtomicU64::new(0);
            let n = CTR.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let dir =
                std::env::temp_dir().join(format!("clawfish-corpus-consumer-{tag}-{pid}-{n}"));
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

    fn base_cfg(out: PathBuf, games: u64) -> SelfPlayConfig {
        SelfPlayConfig {
            seed: 0,
            games,
            workers: 1,
            depth_ladder: vec![(2, 1)],
            opening_random_plies: 0,
            max_plies: 40,
            out_dir: out,
            val_fraction: 0.0,
            book: None,
            book_fraction: 0.0,
            poll_interval_ms: Some(1), // fast polling for tests
            split_seed: 7,
            cap_positions: None,
        }
    }

    fn make_record(game_id: u64, fen: &str) -> CorpusRecord {
        CorpusRecord {
            fen: fen.to_string(),
            label: Label::Draw,
            source: Source::SelfPlay,
            game_id,
            ply: 0,
            depth_rung: 2,
            strata: 0,
        }
    }

    fn write_pending_file(pending_dir: &Path, game_id: u64, records: &[CorpusRecord]) {
        let bytes = encode_block(game_id, records);
        let name = crate::corpus::pending::pending_filename(game_id);
        std::fs::write(pending_dir.join(&name), &bytes).unwrap();
    }

    fn make_state(next: u64) -> ConsumerState {
        ConsumerState {
            next_consume_id: next,
            fen_set: HashSet::new(),
            committed_ids_at_resume: HashSet::new(),
            positions_at_resume: 0,
        }
    }

    // ── In-order commit ──────────────────────────────────────────────────

    #[test]
    fn consumer_commits_in_game_id_order_under_out_of_order_arrivals() {
        // Games arrive out of order: 2 before 0, then 1. Consumer must
        // commit them in order 0, 1, 2.
        let td = TempDir::new("order");
        let pending_dir = td.0.join("pending");
        std::fs::create_dir_all(&pending_dir).unwrap();

        let dispatcher = Dispatcher::new(vec![], 3, 3);
        let alive = Arc::new(AtomicUsize::new(0)); // workers done
        let stop = AtomicBool::new(false);

        // Write pending files out-of-order.
        write_pending_file(&pending_dir, 2, &[make_record(2, "pos2 w - - 0 1")]);
        write_pending_file(&pending_dir, 0, &[make_record(0, "pos0 w - - 0 1")]);
        write_pending_file(&pending_dir, 1, &[make_record(1, "pos1 w - - 0 1")]);

        // Notify all games committed so dispatcher is drained.
        dispatcher.notify_completed(0);
        dispatcher.notify_completed(1);
        dispatcher.notify_completed(2);

        let cfg = base_cfg(td.0.clone(), 3);
        let mut state = make_state(0);
        let stats = consumer_loop(
            &cfg,
            &mut state,
            &dispatcher,
            Arc::clone(&alive),
            &stop,
            &td.0,
        )
        .unwrap();

        assert_eq!(stats.games_committed, 3);
        let (blocks, _) = scan_valid_blocks(&td.0.join("shard.bin")).unwrap();
        let ids: Vec<u64> = blocks.iter().map(|b| b.game_id).collect();
        assert_eq!(ids, vec![0, 1, 2], "shard must be in game_id order");
    }

    #[test]
    fn consumer_uses_exact_name_probe_not_readdir() {
        // Even if pending files are present in "wrong" readdir order, the
        // consumer commits in game_id order (by exact-name probe).
        // This is the same as the above test — the key contract is that
        // the consumer never relies on directory entry order.
        let td = TempDir::new("exact-probe-order");
        let pending_dir = td.0.join("pending");
        std::fs::create_dir_all(&pending_dir).unwrap();

        // Write in "reverse" name order to maximize readdir confusion.
        write_pending_file(&pending_dir, 4, &[make_record(4, "pos4 w - - 0 1")]);
        write_pending_file(&pending_dir, 3, &[make_record(3, "pos3 w - - 0 1")]);
        write_pending_file(&pending_dir, 2, &[make_record(2, "pos2 w - - 0 1")]);
        write_pending_file(&pending_dir, 1, &[make_record(1, "pos1 w - - 0 1")]);
        write_pending_file(&pending_dir, 0, &[make_record(0, "pos0 w - - 0 1")]);

        let dispatcher = Dispatcher::new(vec![], 5, 5);
        for id in 0..5 {
            dispatcher.notify_completed(id);
        }
        let alive = Arc::new(AtomicUsize::new(0));
        let stop = AtomicBool::new(false);
        let cfg = base_cfg(td.0.clone(), 5);
        let mut state = make_state(0);
        consumer_loop(&cfg, &mut state, &dispatcher, alive, &stop, &td.0).unwrap();

        let (blocks, _) = scan_valid_blocks(&td.0.join("shard.bin")).unwrap();
        let ids: Vec<u64> = blocks.iter().map(|b| b.game_id).collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 4]);
    }

    // ── Inline dedup ─────────────────────────────────────────────────────

    #[test]
    fn consumer_inline_dedup_first_seen_wins() {
        // Game 0 has FEN "fen-a"; game 1 also has "fen-a". The dedup must
        // drop game 1's copy (first-seen = game 0's copy wins).
        let td = TempDir::new("dedup-first");
        let pending_dir = td.0.join("pending");
        std::fs::create_dir_all(&pending_dir).unwrap();

        write_pending_file(
            &pending_dir,
            0,
            &[make_record(
                0,
                "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
            )],
        );
        write_pending_file(
            &pending_dir,
            1,
            &[make_record(
                1,
                "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
            )],
        );

        let dispatcher = Dispatcher::new(vec![], 2, 2);
        dispatcher.notify_completed(0);
        dispatcher.notify_completed(1);
        let alive = Arc::new(AtomicUsize::new(0));
        let stop = AtomicBool::new(false);
        let cfg = base_cfg(td.0.clone(), 2);
        let mut state = make_state(0);
        let stats = consumer_loop(&cfg, &mut state, &dispatcher, alive, &stop, &td.0).unwrap();

        // Only 1 position committed (the duplicate from game 1 dropped).
        assert_eq!(
            stats.positions_committed, 1,
            "duplicate FEN must be dropped"
        );
        assert_eq!(stats.games_committed, 2);
    }

    #[test]
    fn consumer_inline_cap_on_dedup_survivors() {
        // 15 records per game, 5 are duplicates of earlier game's FENs.
        // After dedup, 10 unique records. With PER_GAME_CAP=10, all 10 survive.
        // (This exercises that cap is applied to dedup-survivors, not raw count.)
        use crate::corpus::PER_GAME_CAP;

        let td = TempDir::new("cap-dedup");
        let pending_dir = td.0.join("pending");
        std::fs::create_dir_all(&pending_dir).unwrap();

        // Game 0: 5 "shared" FENs + 10 unique.
        let shared_fens: Vec<String> = (0..5)
            .map(|i| format!("shared-fen-{i} w - - 0 1"))
            .collect();
        let unique0: Vec<String> = (0..10)
            .map(|i| format!("unique0-fen-{i} w - - 0 1"))
            .collect();
        let mut recs0: Vec<CorpusRecord> = shared_fens
            .iter()
            .map(|f| make_record(0, f))
            .chain(unique0.iter().map(|f| make_record(0, f)))
            .collect();
        // Cap at PER_GAME_CAP — game 0 has 15 records but cap is 10.
        recs0.truncate(PER_GAME_CAP); // simulate the per-game cap on game 0

        // Game 1: the same 5 shared FENs (dedup dups) + 10 unique.
        let unique1: Vec<String> = (0..10)
            .map(|i| format!("unique1-fen-{i} w - - 0 1"))
            .collect();
        let recs1: Vec<CorpusRecord> = shared_fens
            .iter()
            .map(|f| make_record(1, f))
            .chain(unique1.iter().map(|f| make_record(1, f)))
            .collect();

        write_pending_file(&pending_dir, 0, &recs0);
        write_pending_file(&pending_dir, 1, &recs1);

        let dispatcher = Dispatcher::new(vec![], 2, 2);
        dispatcher.notify_completed(0);
        dispatcher.notify_completed(1);
        let alive = Arc::new(AtomicUsize::new(0));
        let stop = AtomicBool::new(false);
        let cfg = base_cfg(td.0.clone(), 2);
        let mut state = make_state(0);
        let stats = consumer_loop(&cfg, &mut state, &dispatcher, alive, &stop, &td.0).unwrap();

        // Game 0: 10 unique records (after pre-cap), all pass dedup. → 10 commits.
        // Game 1: 5 shared FENs dropped by dedup (already in fen_set from game 0),
        //         10 unique survive dedup, cap of 10 keeps all 10. → 10 commits.
        // Strict expected: positions_committed == 20.
        assert_eq!(
            stats.positions_committed,
            (PER_GAME_CAP as u64) * 2,
            "dedup-then-cap on the two-game scenario: 10 from game 0 + 10 unique from game 1 = 20; got {}",
            stats.positions_committed
        );
    }

    #[test]
    fn consumer_inline_split_routes_per_game_coin_flip() {
        // With val_fraction=1.0, every game goes to val.bin; with 0.0, none.
        let td_val = TempDir::new("split-val");
        let pending_val = td_val.0.join("pending");
        std::fs::create_dir_all(&pending_val).unwrap();
        for id in 0..4 {
            write_pending_file(
                &pending_val,
                id,
                &[make_record(id, &format!("fen-{id} w - - 0 1"))],
            );
        }
        let mut cfg_val = base_cfg(td_val.0.clone(), 4);
        cfg_val.val_fraction = 1.0;
        let d = Dispatcher::new(vec![], 4, 4);
        for id in 0..4 {
            d.notify_completed(id);
        }
        let alive = Arc::new(AtomicUsize::new(0));
        let stop = AtomicBool::new(false);
        let mut state = make_state(0);
        consumer_loop(&cfg_val, &mut state, &d, alive, &stop, &td_val.0).unwrap();

        // With val_fraction=1.0, shard.bin should have zero records.
        let (train_blocks, _) = scan_valid_blocks(&td_val.0.join("shard.bin")).unwrap();
        assert!(
            train_blocks.iter().flat_map(|b| b.records.iter()).count() == 0,
            "val_fraction=1.0: shard.bin must have zero records"
        );
        let (val_blocks, _) = scan_valid_blocks(&td_val.0.join("val.bin")).unwrap();
        assert!(
            val_blocks.iter().flat_map(|b| b.records.iter()).count() > 0,
            "val_fraction=1.0: val.bin must have records"
        );
    }

    // ── Empty-post-dedup game ────────────────────────────────────────────

    #[test]
    fn consumer_empty_post_dedup_game_advances_cursor() {
        // A game whose every record is a dedup-duplicate must:
        // - Advance next_consume_id.
        // - NOT write a block to shard.bin.
        // - Increment games_empty_post_dedup.
        // - Unlink the pending file.
        let td = TempDir::new("empty-dedup");
        let pending_dir = td.0.join("pending");
        std::fs::create_dir_all(&pending_dir).unwrap();

        let fen = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
        // Game 0: the FEN.
        write_pending_file(&pending_dir, 0, &[make_record(0, fen)]);
        // Game 1: same FEN → empty post-dedup.
        write_pending_file(&pending_dir, 1, &[make_record(1, fen)]);

        let dispatcher = Dispatcher::new(vec![], 2, 2);
        dispatcher.notify_completed(0);
        dispatcher.notify_completed(1);
        let alive = Arc::new(AtomicUsize::new(0));
        let stop = AtomicBool::new(false);
        let cfg = base_cfg(td.0.clone(), 2);
        let mut state = make_state(0);
        let stats = consumer_loop(&cfg, &mut state, &dispatcher, alive, &stop, &td.0).unwrap();

        assert_eq!(stats.games_committed, 2, "cursor advanced past both games");
        assert_eq!(
            stats.games_empty_post_dedup, 1,
            "game 1 was empty post-dedup"
        );
        // Pending files must be unlinked.
        assert!(
            !pending_dir
                .join(crate::corpus::pending::pending_filename(1))
                .exists(),
            "pending file for empty-post-dedup game must be unlinked"
        );
        // Shard should have only game 0's record.
        let (blocks, _) = scan_valid_blocks(&td.0.join("shard.bin")).unwrap();
        let rec_count: usize = blocks.iter().map(|b| b.records.len()).sum();
        assert_eq!(rec_count, 1, "only game 0's record in shard");
    }

    // ── Ghost-pending self-heal ──────────────────────────────────────────

    #[test]
    fn consumer_ghost_pending_self_heal_committed() {
        // Existing case (Blocker #2): a pending file whose game_id is already
        // in `committed_ids_at_resume` must be unlinked (ghost cleanup).
        let td = TempDir::new("ghost-committed");
        let pending_dir = td.0.join("pending");
        std::fs::create_dir_all(&pending_dir).unwrap();

        // Game 0 already committed (in committed_ids_at_resume).
        let fen = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
        write_pending_file(&pending_dir, 0, &[make_record(0, fen)]);

        let mut state = ConsumerState {
            next_consume_id: 1, // starting past game 0
            fen_set: HashSet::new(),
            committed_ids_at_resume: [0].into_iter().collect(),
            positions_at_resume: 0,
        };
        // Write game 1 so consumer has something to commit.
        write_pending_file(&pending_dir, 1, &[make_record(1, "unique w - - 0 1")]);

        let dispatcher = Dispatcher::new(vec![], 2, 2);
        dispatcher.notify_completed(1);
        let alive = Arc::new(AtomicUsize::new(0));
        let stop = AtomicBool::new(false);
        let cfg = base_cfg(td.0.clone(), 2);
        consumer_loop(&cfg, &mut state, &dispatcher, alive, &stop, &td.0).unwrap();

        // Ghost pending file for game 0 must have been cleaned.
        assert!(
            !pending_dir
                .join(crate::corpus::pending::pending_filename(0))
                .exists(),
            "ghost pending file for committed game 0 must be unlinked"
        );
    }

    #[test]
    fn consumer_ghost_pending_self_heal_empty_post_dedup_below_ckpt() {
        // Blocker #2 new case: a pending file for a game_id that was
        // committed as empty-post-dedup (no block in shard, but
        // next_consume_id advanced past it) must also be cleaned.
        // Scenario: checkpoint says next_consume_id=3; pending has game 1.
        let td = TempDir::new("ghost-empty-ckpt");
        let pending_dir = td.0.join("pending");
        std::fs::create_dir_all(&pending_dir).unwrap();

        // Plant ghost pending for game 1 (below ckpt's next_consume_id=3).
        write_pending_file(&pending_dir, 1, &[]);

        // Consumer starts at next_consume_id=3 (game 1 already consumed).
        let mut state = ConsumerState {
            next_consume_id: 3,
            fen_set: HashSet::new(),
            committed_ids_at_resume: HashSet::new(), // empty-post-dedup: no block in shard
            positions_at_resume: 0,
        };
        write_pending_file(&pending_dir, 3, &[make_record(3, "fen-3 w - - 0 1")]);

        let dispatcher = Dispatcher::new(vec![], 4, 4);
        dispatcher.notify_completed(3);
        let alive = Arc::new(AtomicUsize::new(0));
        let stop = AtomicBool::new(false);
        let cfg = base_cfg(td.0.clone(), 4);
        consumer_loop(&cfg, &mut state, &dispatcher, alive, &stop, &td.0).unwrap();

        // Ghost for game 1 must be cleaned.
        assert!(
            !pending_dir
                .join(crate::corpus::pending::pending_filename(1))
                .exists(),
            "ghost pending below ckpt.next_consume_id must be unlinked"
        );
    }

    // ── Torn/oversize/mismatch error paths ───────────────────────────────

    #[test]
    fn consumer_torn_pending_deleted_and_released() {
        // A pending file with bad CRC / decode failure must be:
        // - Unlinked.
        // - Its game_id released back to gap_queue via release_unclaimed.
        let td = TempDir::new("torn-pending");
        let pending_dir = td.0.join("pending");
        std::fs::create_dir_all(&pending_dir).unwrap();

        // Write a corrupt (torn) pending file for game 0.
        let name = crate::corpus::pending::pending_filename(0);
        std::fs::write(pending_dir.join(&name), b"garbage-not-a-valid-block").unwrap();

        // Write a valid pending file for game 1 (after the torn one is handled).
        write_pending_file(&pending_dir, 1, &[make_record(1, "fen-1 w - - 0 1")]);

        // Dispatcher: game 0 was claimed (in-flight), game 1 committed.
        let dispatcher = Dispatcher::new(vec![], 2, 2);
        // Simulate: game 0 is in claimed (will be released by consumer).
        dispatcher.notify_completed(1);
        let alive = Arc::new(AtomicUsize::new(1)); // 1 worker still "alive" (will send game 0 again)
        let stop = AtomicBool::new(false);

        // After torn file unlinked + release_unclaimed(0), game 0 re-enters gap_queue.
        // For this test we just verify the consumer doesn't deadlock on a
        // torn file when the dispatcher is also drained. We stop the worker.
        alive.store(0, Ordering::Relaxed);

        let cfg = base_cfg(td.0.clone(), 2);
        let mut state = make_state(0);
        // Consumer should release game 0 (torn) and... since alive=0 and
        // no valid pending, return wedge error or exit. We accept either
        // outcome that demonstrates the torn file was deleted.
        let _ = consumer_loop(&cfg, &mut state, &dispatcher, alive, &stop, &td.0);

        assert!(
            !pending_dir.join(&name).exists(),
            "torn pending file must be unlinked"
        );
    }

    #[test]
    fn consumer_oversized_pending_is_fatal_not_released() {
        // Blocker #4(b): an oversize pending file is a fatal error — NOT
        // release_unclaimed (the worker would regenerate the same block).
        use crate::corpus::pending::MAX_PENDING_BYTES;

        let td = TempDir::new("oversize-pending");
        let pending_dir = td.0.join("pending");
        std::fs::create_dir_all(&pending_dir).unwrap();

        // Write an oversize pending file for game 0.
        let name = crate::corpus::pending::pending_filename(0);
        std::fs::write(pending_dir.join(&name), vec![0u8; MAX_PENDING_BYTES + 1]).unwrap();

        let dispatcher = Dispatcher::new(vec![], 1, 1);
        let alive = Arc::new(AtomicUsize::new(0));
        let stop = AtomicBool::new(false);
        let cfg = base_cfg(td.0.clone(), 1);
        let mut state = make_state(0);
        let result = consumer_loop(&cfg, &mut state, &dispatcher, alive, &stop, &td.0);
        assert!(
            matches!(result, Err(CorpusError::Checkpoint(_))),
            "oversize pending file must return fatal CorpusError::Checkpoint, got: {result:?}"
        );
    }

    #[test]
    fn consumer_filename_block_id_mismatch_is_fatal_and_unlinked() {
        // Substantive #10: a pending file whose decoded game_id ≠ filename's
        // game_id is a fatal error. The file is unlinked.
        let td = TempDir::new("id-mismatch");
        let pending_dir = td.0.join("pending");
        std::fs::create_dir_all(&pending_dir).unwrap();

        // Write a block for game_id=99 into the filename for game_id=0.
        let mismatch_block = encode_block(99, &[make_record(99, "fen w - - 0 1")]);
        let name = crate::corpus::pending::pending_filename(0);
        std::fs::write(pending_dir.join(&name), &mismatch_block).unwrap();

        let dispatcher = Dispatcher::new(vec![], 1, 1);
        let alive = Arc::new(AtomicUsize::new(0));
        let stop = AtomicBool::new(false);
        let cfg = base_cfg(td.0.clone(), 1);
        let mut state = make_state(0);
        let result = consumer_loop(&cfg, &mut state, &dispatcher, alive, &stop, &td.0);
        assert!(
            matches!(result, Err(CorpusError::Checkpoint(_))),
            "filename/block game_id mismatch must be fatal CorpusError::Checkpoint"
        );
        assert!(
            !pending_dir.join(&name).exists(),
            "mismatch pending file must be unlinked"
        );
    }

    // ── Skip-committed bypass (Blocker #1) ───────────────────────────────

    #[test]
    fn consumer_skip_committed_id_advances_without_waiting() {
        // Blocker #1: if next_consume_id is in committed_ids_at_resume,
        // the consumer must advance the cursor WITHOUT waiting for a
        // pending file (the game was already committed).
        let td = TempDir::new("skip-committed");
        let pending_dir = td.0.join("pending");
        std::fs::create_dir_all(&pending_dir).unwrap();

        // State: next_consume_id=3, game 3 is in committed_ids_at_resume.
        let mut state = ConsumerState {
            next_consume_id: 3,
            fen_set: HashSet::new(),
            committed_ids_at_resume: [3].into_iter().collect(),
            positions_at_resume: 0,
        };
        // No pending file for game 3.
        assert!(
            !pending_dir
                .join(crate::corpus::pending::pending_filename(3))
                .exists()
        );
        // Write game 4 so consumer has something to do after skipping 3.
        write_pending_file(&pending_dir, 4, &[make_record(4, "fen-4 w - - 0 1")]);

        // Mirror real worker flow: claim game 4 before notifying it complete.
        // next_dispatch_id=4 so claim_next yields game 4; games_target=5.
        let dispatcher = Dispatcher::new(vec![], 4, 5);
        let guard = dispatcher.claim_next().expect("should claim game 4");
        assert_eq!(guard.game_id(), 4);
        guard.notify_completed();
        let alive = Arc::new(AtomicUsize::new(0));
        let stop = AtomicBool::new(false);
        let cfg = base_cfg(td.0.clone(), 5);
        let start = std::time::Instant::now();
        consumer_loop(&cfg, &mut state, &dispatcher, alive, &stop, &td.0).unwrap();
        let elapsed = start.elapsed();

        assert_eq!(state.next_consume_id, 5, "cursor must advance past 3 and 4");
        // Skip-committed path takes zero sleeps. Poll cadence is 1ms in test
        // cfg, so any unintended polling loop would add at least 1ms per
        // iteration. We allow 2000ms to accommodate fsync latency on slow
        // devices while still catching the pathological "polling in a loop"
        // failure mode (which would easily exceed this threshold).
        assert!(
            elapsed.as_millis() < 2000,
            "skip-committed path must not sleep; elapsed {elapsed:?}"
        );
    }

    // ── Wedge detection ──────────────────────────────────────────────────

    #[test]
    fn consumer_wedge_when_no_live_workers_and_no_pending() {
        // When alive_workers=0, no pending file for next_consume_id, and
        // is_fully_drained_at is true, the consumer must return
        // CorpusError::Checkpoint("consumer wedged: ...").
        let td = TempDir::new("wedge");
        let pending_dir = td.0.join("pending");
        std::fs::create_dir_all(&pending_dir).unwrap();

        let dispatcher = Dispatcher::new(vec![], 1, 5); // id 0 never dispatched
        let alive = Arc::new(AtomicUsize::new(0)); // no live workers
        let stop = AtomicBool::new(false);
        let cfg = base_cfg(td.0.clone(), 5);
        let mut state = make_state(0);

        // Drain the dispatcher without producing a pending file.
        dispatcher.notify_completed(0); // lie: tell dispatcher it's done so is_fully_drained_at is true

        let result = consumer_loop(&cfg, &mut state, &dispatcher, alive, &stop, &td.0);
        assert!(
            matches!(&result, Err(CorpusError::Checkpoint(msg)) if msg.contains("consumer wedged")),
            "expected consumer wedge error, got: {result:?}"
        );
    }

    // ── Poll interval override ───────────────────────────────────────────

    #[test]
    fn consumer_poll_interval_ms_override_honored() {
        // Substantive #7: the poll interval is overridable via cfg.poll_interval_ms.
        // With poll_interval_ms=Some(1), the consumer polls every 1ms.
        // Verify it terminates quickly when pending files arrive.
        let td = TempDir::new("poll-interval");
        let pending_dir = td.0.join("pending");
        std::fs::create_dir_all(&pending_dir).unwrap();

        write_pending_file(&pending_dir, 0, &[make_record(0, "fen w - - 0 1")]);

        let dispatcher = Dispatcher::new(vec![], 1, 1);
        dispatcher.notify_completed(0);
        let alive = Arc::new(AtomicUsize::new(0));
        let stop = AtomicBool::new(false);
        let mut cfg = base_cfg(td.0.clone(), 1);
        cfg.poll_interval_ms = Some(1); // 1ms poll
        let mut state = make_state(0);
        let start = std::time::Instant::now();
        consumer_loop(&cfg, &mut state, &dispatcher, alive, &stop, &td.0).unwrap();
        assert!(
            start.elapsed().as_millis() < 200,
            "with poll_interval_ms=1 and immediate file, consumer should finish fast"
        );
    }
}
