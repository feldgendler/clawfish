//! FEN dedup + per-game cap. Pipeline order is `dedup → cap → split`
//! (research §2.5/§7.2). Dedup survivor among exact-FEN duplicates =
//! lexicographically-smallest `CorpusRecord::dedup_key()` ⇒ a deterministic
//! function of the input MULTISET, independent of shard/merge order (the
//! reproducibility-gate prerequisite). Bounded-memory external sort-merge:
//! RAM ceiling `max_mem_mb` + disk-spill ceiling under
//! `target/corpus-spill/` (git-ignored), both documented (R6).
//!
//! Implemented by the M6.G `dedup` coder slice per `docs/plans/m6.g.md` §3.8.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use super::{CorpusError, CorpusRecord, Label, Source};

// ---------------------------------------------------------------------------
// Serialization helpers for spill files.
//
// Each spill record is a length-prefixed row. Format (little-endian):
//   fen_len: u16
//   fen:     [u8; fen_len]
//   label:   u8
//   source:  u8
//   game_id: u64
//   ply:     u32
//   depth_rung: u8
//   strata:  u8
//
// This is compact and easy to parse without a full serde stack.
// ---------------------------------------------------------------------------

fn write_record(w: &mut impl Write, r: &CorpusRecord) -> io::Result<()> {
    let fen = r.fen.as_bytes();
    let fen_len = u16::try_from(fen.len()).expect("FEN longer than 65535 bytes — impossible");
    w.write_all(&fen_len.to_le_bytes())?;
    w.write_all(fen)?;
    w.write_all(&[r.label.as_u8(), r.source.as_u8()])?;
    w.write_all(&r.game_id.to_le_bytes())?;
    w.write_all(&r.ply.to_le_bytes())?;
    w.write_all(&[r.depth_rung, r.strata])?;
    Ok(())
}

fn read_record(r: &mut impl Read) -> io::Result<Option<CorpusRecord>> {
    let mut buf2 = [0u8; 2];
    match r.read_exact(&mut buf2) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let fen_len = u16::from_le_bytes(buf2) as usize;
    let mut fen_bytes = vec![0u8; fen_len];
    r.read_exact(&mut fen_bytes)?;
    let fen =
        String::from_utf8(fen_bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let mut tag = [0u8; 2];
    r.read_exact(&mut tag)?;
    let label = Label::from_u8(tag[0])
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad label byte"))?;
    let source = Source::from_u8(tag[1])
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad source byte"))?;

    let mut u64_buf = [0u8; 8];
    r.read_exact(&mut u64_buf)?;
    let game_id = u64::from_le_bytes(u64_buf);

    let mut u32_buf = [0u8; 4];
    r.read_exact(&mut u32_buf)?;
    let ply = u32::from_le_bytes(u32_buf);

    let mut misc = [0u8; 2];
    r.read_exact(&mut misc)?;
    let depth_rung = misc[0];
    let strata = misc[1];

    Ok(Some(CorpusRecord {
        fen,
        label,
        source,
        game_id,
        ply,
        depth_rung,
        strata,
    }))
}

// ---------------------------------------------------------------------------
// Sort key helpers.
// ---------------------------------------------------------------------------

/// Compare two records for sorting: primary key = FEN, tie-break = dedup_key.
fn record_sort_cmp(a: &CorpusRecord, b: &CorpusRecord) -> Ordering {
    a.fen
        .cmp(&b.fen)
        .then_with(|| a.dedup_key().cmp(&b.dedup_key()))
}

// ---------------------------------------------------------------------------
// Estimated in-memory size of a CorpusRecord (rough upper bound).
// Used to decide when to spill a run. The true size is always ≤ this.
// ---------------------------------------------------------------------------

fn record_size_estimate(r: &CorpusRecord) -> usize {
    // Struct overhead + heap String
    std::mem::size_of::<CorpusRecord>() + r.fen.capacity()
}

// ---------------------------------------------------------------------------
// Spill run writer: sorts a batch in-RAM and writes to a temp file.
// ---------------------------------------------------------------------------

fn write_spill_run(
    batch: &mut [CorpusRecord],
    spill_dir: &Path,
    run_index: usize,
) -> Result<PathBuf, CorpusError> {
    batch.sort_unstable_by(record_sort_cmp);
    let path = spill_dir.join(format!("run-{run_index:06}.tmp"));
    let file = fs::File::create(&path).map_err(CorpusError::Io)?;
    let mut w = BufWriter::new(file);
    for r in batch.iter() {
        write_record(&mut w, r).map_err(CorpusError::Io)?;
    }
    w.flush().map_err(CorpusError::Io)?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// External sort-merge: merge sorted run files, emitting the min-dedup_key
// survivor among each group of exact-FEN duplicates.
// ---------------------------------------------------------------------------

/// Heap entry for the k-way merge. `Reverse`-ordered so BinaryHeap is a min-heap.
struct HeapEntry {
    record: CorpusRecord,
    /// Which run file this entry came from.
    run_idx: usize,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for HeapEntry {}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Min-heap: flip the comparison so smallest FEN+key is at the top.
        record_sort_cmp(&other.record, &self.record)
    }
}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn merge_spill_runs(run_paths: &[PathBuf]) -> Result<Vec<CorpusRecord>, CorpusError> {
    let mut readers: Vec<BufReader<fs::File>> = run_paths
        .iter()
        .map(|p| {
            fs::File::open(p)
                .map(BufReader::new)
                .map_err(CorpusError::Io)
        })
        .collect::<Result<_, _>>()?;

    let mut heap = BinaryHeap::new();

    // Prime the heap with the first record from each run.
    for (idx, reader) in readers.iter_mut().enumerate() {
        if let Some(rec) = read_record(reader).map_err(CorpusError::Io)? {
            heap.push(HeapEntry {
                record: rec,
                run_idx: idx,
            });
        }
    }

    let mut output: Vec<CorpusRecord> = Vec::new();
    // `current` accumulates candidates sharing the same FEN.
    let mut current_fen: Option<String> = None;
    let mut current_best: Option<CorpusRecord> = None;

    while let Some(entry) = heap.pop() {
        let HeapEntry { record, run_idx } = entry;

        // Advance the reader for this run.
        if let Some(next) = read_record(&mut readers[run_idx]).map_err(CorpusError::Io)? {
            heap.push(HeapEntry {
                record: next,
                run_idx,
            });
        }

        // Check if this is a new FEN group.
        let same_group = current_fen.as_deref() == Some(record.fen.as_str());
        if !same_group {
            // Emit the best survivor from the completed group.
            if let Some(best) = current_best.take() {
                output.push(best);
            }
            current_fen = Some(record.fen.clone());
            current_best = Some(record);
        } else {
            // Same FEN — keep the min-dedup_key survivor.
            let best = current_best
                .as_ref()
                .expect("current_best is Some when same_group");
            if record.dedup_key() < best.dedup_key() {
                current_best = Some(record);
            }
        }
    }
    // Emit the final group's survivor.
    if let Some(best) = current_best {
        output.push(best);
    }

    Ok(output)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Remove exact-FEN duplicates keeping the min-`dedup_key` survivor.
///
/// Among duplicates the survivor is the record with the
/// lexicographically-smallest `(source.as_u8(), game_id, ply)` — a
/// deterministic function of the input **multiset**, independent of
/// shard/merge order (the reproducibility-gate prerequisite).
///
/// **Bounded memory.** When the in-RAM working set would exceed
/// `max_mem_mb` megabytes, sorted runs are spilled to temp files under
/// `spill_dir` and a k-way merge is done on the sorted runs. Total spill
/// is bounded by `max_spill_gb`; if it would be exceeded,
/// [`CorpusError::QualityGate`] is returned. Spill files are cleaned on
/// success.
///
/// For small inputs (below `max_mem_mb`) an in-memory path is used; the
/// external path is exercised by the `dedup_external_spill_matches_in_memory`
/// test.
pub fn dedup_fen(
    records: impl Iterator<Item = CorpusRecord>,
    spill_dir: &Path,
    max_mem_mb: u64,
    max_spill_gb: u64,
) -> Result<Vec<CorpusRecord>, CorpusError> {
    let max_bytes = max_mem_mb.saturating_mul(1024 * 1024) as usize;
    let max_spill_bytes = max_spill_gb.saturating_mul(1024 * 1024 * 1024);

    // Collect records into RAM batches; spill sorted runs when the ceiling is hit.
    let mut batch: Vec<CorpusRecord> = Vec::new();
    let mut batch_bytes: usize = 0;
    let mut run_paths: Vec<PathBuf> = Vec::new();
    let mut total_spill_bytes: u64 = 0;

    for record in records {
        let sz = record_size_estimate(&record);
        // If adding this record would exceed RAM budget AND we already have
        // records, spill the current batch as a sorted run.
        if batch_bytes + sz > max_bytes && !batch.is_empty() {
            // Estimate spill size (each record is roughly fen_len + 18 bytes on disk).
            let run_disk_est: u64 = batch.iter().map(|r| r.fen.len() as u64 + 18).sum();
            total_spill_bytes = total_spill_bytes.saturating_add(run_disk_est);
            if total_spill_bytes > max_spill_bytes {
                return Err(CorpusError::QualityGate(format!(
                    "dedup spill exceeded {max_spill_gb} GB ceiling"
                )));
            }
            fs::create_dir_all(spill_dir).map_err(CorpusError::Io)?;
            let path = write_spill_run(&mut batch, spill_dir, run_paths.len())?;
            run_paths.push(path);
            batch.clear();
            batch_bytes = 0;
        }
        batch_bytes += sz;
        batch.push(record);
    }

    // Handle the in-memory-only path (no spills).
    if run_paths.is_empty() {
        // Sort in-place, then deduplicate by keeping min-dedup_key per FEN.
        batch.sort_unstable_by(record_sort_cmp);
        let output = deduplicate_sorted(batch);
        return Ok(output);
    }

    // Spill path: flush the remaining batch as the last run.
    if !batch.is_empty() {
        let run_disk_est: u64 = batch.iter().map(|r| r.fen.len() as u64 + 18).sum();
        total_spill_bytes = total_spill_bytes.saturating_add(run_disk_est);
        if total_spill_bytes > max_spill_bytes {
            return Err(CorpusError::QualityGate(format!(
                "dedup spill exceeded {max_spill_gb} GB ceiling"
            )));
        }
        fs::create_dir_all(spill_dir).map_err(CorpusError::Io)?;
        let path = write_spill_run(&mut batch, spill_dir, run_paths.len())?;
        run_paths.push(path);
    }

    let result = merge_spill_runs(&run_paths)?;

    // Clean up spill files on success.
    for path in &run_paths {
        let _ = fs::remove_file(path);
    }

    Ok(result)
}

/// Deduplicate an already-sorted record slice (sorted by FEN then dedup_key).
/// The first record in each FEN group is the min-dedup_key survivor.
fn deduplicate_sorted(sorted: Vec<CorpusRecord>) -> Vec<CorpusRecord> {
    let mut output: Vec<CorpusRecord> = Vec::with_capacity(sorted.len());
    let mut last_fen: Option<String> = None;
    for record in sorted {
        match last_fen.as_deref() {
            Some(fen) if fen == record.fen.as_str() => {
                // Duplicate FEN: `record` has ≥ dedup_key of the one we already
                // kept (since sorted), so discard it.
            }
            _ => {
                last_fen = Some(record.fen.clone());
                output.push(record);
            }
        }
    }
    output
}

/// At most `cap` records per `game_id`, selected by seeded reservoir sampling.
///
/// The reservoir sample is deterministic from `seed` and the input ordering:
/// the same input slice and seed always yield the same kept set.
/// Uses `corpus::prng::Prng` (SplitMix64).
pub fn per_game_cap(records: Vec<CorpusRecord>, cap: usize, seed: u64) -> Vec<CorpusRecord> {
    use super::prng::Prng;

    if cap == 0 {
        return Vec::new();
    }

    // Group by game_id in a stable, order-preserving way.
    // We process groups by scanning linearly (preserves input order within each group).
    // Build an index of record → group membership first so we can sample per-group.
    //
    // Strategy: collect per-game groups, reservoir-sample each group with a
    // per-game-deterministic seed derived from `seed ^ game_id`, then
    // re-interleave the survivors in their original relative order.

    // Map: game_id → list of (original_index, &record).
    use std::collections::HashMap;
    let mut game_indices: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, r) in records.iter().enumerate() {
        game_indices.entry(r.game_id).or_default().push(i);
    }

    // Reservoir sample each game's indices.
    let mut kept: Vec<bool> = vec![false; records.len()];
    for (game_id, indices) in &game_indices {
        // Per-game deterministic seed: mix `seed` with `game_id` so each game
        // uses an independent stream regardless of iteration order over the HashMap.
        let game_seed = super::prng::substream_seed(seed, *game_id);
        let mut rng = Prng::new(game_seed);

        if indices.len() <= cap {
            // Keep all.
            for &idx in indices {
                kept[idx] = true;
            }
        } else {
            // Knuth/Vitter Algorithm R: fill reservoir with first `cap` items,
            // then replace with decreasing probability.
            let mut reservoir: Vec<usize> = indices[..cap].to_vec();
            for (k, &idx) in indices[cap..].iter().enumerate() {
                // k is 0-indexed offset after the initial cap.
                let j = rng.below((cap + k + 1) as u64) as usize;
                if j < cap {
                    reservoir[j] = idx;
                }
            }
            for idx in reservoir {
                kept[idx] = true;
            }
        }
    }

    // Collect survivors in original order (stable).
    records
        .into_iter()
        .enumerate()
        .filter_map(|(i, r)| if kept[i] { Some(r) } else { None })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{DEPTH_RUNG_EXTERNAL, Label, Source};
    use proptest::prelude::*;

    fn make_record(fen: &str, source: Source, game_id: u64, ply: u32) -> CorpusRecord {
        CorpusRecord {
            fen: fen.to_string(),
            label: Label::Draw,
            source,
            game_id,
            ply,
            depth_rung: DEPTH_RUNG_EXTERNAL,
            strata: 0,
        }
    }

    fn make_record_label(
        fen: &str,
        source: Source,
        game_id: u64,
        ply: u32,
        label: Label,
    ) -> CorpusRecord {
        CorpusRecord {
            fen: fen.to_string(),
            label,
            source,
            game_id,
            ply,
            depth_rung: DEPTH_RUNG_EXTERNAL,
            strata: 0,
        }
    }

    /// Run dedup_fen in-memory (large max_mem_mb, no spill).
    fn dedup_in_mem(records: Vec<CorpusRecord>) -> Vec<CorpusRecord> {
        let spill_dir = std::env::temp_dir().join("clawfish-dedup-nospill");
        dedup_fen(records.into_iter(), &spill_dir, 512, 8).expect("dedup_in_mem failed")
    }

    // -----------------------------------------------------------------------
    // Named tests from plan §4 "dedup"
    // -----------------------------------------------------------------------

    #[test]
    fn fen_exact_dup_removed() {
        let records = vec![
            make_record(
                "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
                Source::Ccrl,
                1,
                10,
            ),
            make_record(
                "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
                Source::Ccrl,
                2,
                10,
            ),
            make_record("different/fen w - - 0 1", Source::Ccrl, 3, 10),
        ];
        let out = dedup_in_mem(records);
        assert_eq!(out.len(), 2, "two distinct FENs → two survivors");
        assert!(out.iter().all(|r| {
            r.fen
                != out
                    .iter()
                    .skip(1)
                    .find(|x| x.fen == r.fen)
                    .map(|_| "")
                    .unwrap_or("MISMATCH")
        }));
        // The FEN "rnbqkbnr..." appears exactly once.
        let start_count = out.iter().filter(|r| r.fen.starts_with("rnbqkbnr")).count();
        assert_eq!(start_count, 1);
    }

    #[test]
    fn dedup_survivor_is_min_source_gameid_ply_independent_of_input_order() {
        // Build a multiset: two duplicates of the same FEN from different sources/games.
        // The survivor must always be the one with the smallest dedup_key.
        let fen = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
        // dedup_key = (source.as_u8(), game_id, ply)
        // SelfPlay=0, Ccrl=1, LichessOpen=2
        let r_min = make_record(fen, Source::SelfPlay, 1, 8); // key = (0,1,8) ← min
        let r_mid = make_record(fen, Source::Ccrl, 1, 8); // key = (1,1,8)
        let r_max = make_record(fen, Source::LichessOpen, 5, 9); // key = (2,5,9)

        let orderings: &[&[CorpusRecord]] = &[
            &[r_min.clone(), r_mid.clone(), r_max.clone()],
            &[r_mid.clone(), r_min.clone(), r_max.clone()],
            &[r_max.clone(), r_mid.clone(), r_min.clone()],
            &[r_mid.clone(), r_max.clone(), r_min.clone()],
            &[r_min.clone(), r_max.clone(), r_mid.clone()],
            &[r_max.clone(), r_min.clone(), r_mid.clone()],
        ];

        for &ordering in orderings {
            let out = dedup_in_mem(ordering.to_vec());
            assert_eq!(out.len(), 1, "three dups → one survivor");
            assert_eq!(
                out[0].dedup_key(),
                r_min.dedup_key(),
                "survivor must be min-dedup_key regardless of input order"
            );
        }
    }

    #[test]
    fn per_game_cap_le_cap_and_seed_deterministic() {
        // 20 records in game 1, 5 records in game 2; cap=10.
        let mut records: Vec<CorpusRecord> = (0..20u32)
            .map(|p| make_record_label(&format!("fen-g1-{p}"), Source::Ccrl, 1, p, Label::Draw))
            .collect();
        records.extend((0..5u32).map(|p| {
            make_record_label(&format!("fen-g2-{p}"), Source::Ccrl, 2, p, Label::WhiteWin)
        }));

        let seed = 0xDEAD_BEEF;
        let out = per_game_cap(records.clone(), 10, seed);

        // At most 10 per game.
        let g1_count = out.iter().filter(|r| r.game_id == 1).count();
        let g2_count = out.iter().filter(|r| r.game_id == 2).count();
        assert!(g1_count <= 10, "game 1: {g1_count} > 10");
        assert_eq!(g2_count, 5, "game 2 has fewer than cap; all kept");

        // Deterministic: same input + seed → same output.
        let out2 = per_game_cap(records, 10, seed);
        assert_eq!(
            out, out2,
            "per_game_cap must be deterministic for the same seed"
        );
    }

    #[test]
    fn dedup_external_spill_matches_in_memory() {
        // Force the external-spill path by using max_mem_mb = 0 (any batch > 0 bytes triggers a spill).
        // We need a unique temp dir per test run.
        let spill_dir =
            std::env::temp_dir().join(format!("clawfish-dedup-spill-{}", std::process::id()));

        let records: Vec<CorpusRecord> = (0..50u32)
            .flat_map(|i| {
                // Each FEN appears twice from different game_ids.
                let fen = format!("position-{}", i % 25);
                vec![
                    make_record(&fen, Source::Ccrl, i as u64, 10),
                    make_record(&fen, Source::LichessOpen, (i + 100) as u64, 10),
                ]
            })
            .collect();

        // In-memory reference.
        let in_mem = dedup_in_mem(records.clone());

        // Spill path with a 0-MB ceiling to force every record to be spilled immediately.
        let spill_out =
            dedup_fen(records.into_iter(), &spill_dir, 0, 8).expect("spill dedup failed");

        // Clean up spill dir (files already removed by dedup_fen on success,
        // but the directory may remain).
        let _ = fs::remove_dir_all(&spill_dir);

        assert_eq!(
            in_mem.len(),
            spill_out.len(),
            "spill and in-memory paths must produce same count"
        );

        // Sort both by FEN for comparison (order may differ between paths).
        let mut im = in_mem;
        let mut sp = spill_out;
        im.sort_unstable_by(|a, b| a.fen.cmp(&b.fen));
        sp.sort_unstable_by(|a, b| a.fen.cmp(&b.fen));
        for (a, b) in im.iter().zip(sp.iter()) {
            assert_eq!(a.fen, b.fen);
            assert_eq!(
                a.dedup_key(),
                b.dedup_key(),
                "spill path must pick same survivor"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Proptest: reservoir_size_invariant
    // -----------------------------------------------------------------------

    proptest! {
        #[test]
        fn reservoir_size_invariant(
            n_games in 1usize..=10,
            records_per_game in 1usize..=20,
            cap in 1usize..=10,
            seed: u64,
        ) {
            let mut records: Vec<CorpusRecord> = Vec::new();
            for g in 0..n_games as u64 {
                for p in 0..records_per_game as u32 {
                    records.push(make_record(
                        &format!("fen-{g}-{p}"),
                        Source::SelfPlay,
                        g,
                        p,
                    ));
                }
            }

            let input_set: std::collections::HashSet<String> =
                records.iter().map(|r| r.fen.clone()).collect();

            let out = per_game_cap(records, cap, seed);

            // Output ≤ cap per game.
            for g in 0..n_games as u64 {
                let cnt = out.iter().filter(|r| r.game_id == g).count();
                prop_assert!(cnt <= cap, "game {g} has {cnt} records, cap={cap}");
            }

            // Every kept record is in the input.
            for r in &out {
                prop_assert!(
                    input_set.contains(&r.fen),
                    "output record not in input set"
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // Proptest: dedup_deterministic_under_input_permutation
    // -----------------------------------------------------------------------

    proptest! {
        #[test]
        fn dedup_deterministic_under_input_permutation(
            n_fens in 1usize..=10,
            dup_factor in 1usize..=3,
            seed: u64,
        ) {
            // Build a multiset: each FEN repeated dup_factor times with
            // varying (source, game_id, ply) values.
            let mut records: Vec<CorpusRecord> = Vec::new();
            for f in 0..n_fens as u64 {
                for d in 0..dup_factor as u64 {
                    let src = match (f + d) % 3 {
                        0 => Source::SelfPlay,
                        1 => Source::Ccrl,
                        _ => Source::LichessOpen,
                    };
                    records.push(make_record(
                        &format!("fen-{f}"),
                        src,
                        f * 100 + d,
                        (d * 5) as u32,
                    ));
                }
            }

            let canonical = dedup_in_mem(records.clone());

            // Permute the input a few times and assert identical dedup output.
            let mut rng = crate::corpus::prng::Prng::new(seed);
            for _ in 0..4 {
                let mut shuffled = records.clone();
                // Fisher-Yates shuffle.
                for i in (1..shuffled.len()).rev() {
                    let j = rng.below((i + 1) as u64) as usize;
                    shuffled.swap(i, j);
                }
                let out = dedup_in_mem(shuffled);

                prop_assert_eq!(
                    canonical.len(),
                    out.len(),
                    "dedup output length changed under permutation"
                );

                // Sort both by FEN to compare — output order may differ.
                let mut c = canonical.clone();
                let mut o = out;
                c.sort_unstable_by(|a, b| a.fen.cmp(&b.fen));
                o.sort_unstable_by(|a, b| a.fen.cmp(&b.fen));
                for (a, b) in c.iter().zip(o.iter()) {
                    prop_assert_eq!(&a.fen, &b.fen);
                    prop_assert_eq!(
                        a.dedup_key(),
                        b.dedup_key(),
                        "permutation changed the survivor"
                    );
                }
            }
        }
    }
}
