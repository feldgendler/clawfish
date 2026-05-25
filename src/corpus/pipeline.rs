//! Shared per-lane commit primitive (M6.H2). Both the fetch and self-play
//! lanes route their dedup → cap → exact-target-truncation → append-block
//! work through [`LaneCommitter`] so the two ingestion paths **cannot
//! diverge** (plan §1.3).
//!
//! The committer owns three things, applied in this fixed order to each
//! game's records (which the caller has ALREADY quiet-certified — skip8 ∧
//! `!in_check` ∧ `|eval| ≤ HIGH_SCORE_CP` ∧ `is_quiet`):
//!   1. per-lane FEN dedup (first-seen-wins, WITHIN the lane only),
//!   2. per-game reservoir cap = [`PER_GAME_CAP`], seeded by
//!      `substream_seed(cap_seed, game_id)` (Knuth/Vitter Algorithm R),
//!   3. EXACT target truncation: the boundary game commits only
//!      `target - committed` records — never an overshoot.
//!
//! Order matters: dedup runs first so the cap reservoir samples among
//! dedup-survivors, byte-for-byte matching the pre-M6.H2 consumer pipeline.
//!
//! Determinism: within a lane, "first-seen" = "first-by-arrival". The caller
//! passes games in `game_id` order and records in `ply` order, so the
//! survivor set is a deterministic function of the (deterministic) arrival
//! stream for a fixed `cap_seed`. The committer does NOT depend on the
//! global `CorpusRecord::dedup_key` (removed in a later M6.H2 slice).
//!
//! The committer does NOT own the quiet filter (that needs `Position` +
//! `QSearcher`, in the lane's game loop) nor checkpoint / pending-file
//! lifecycle (the self-play consumer wraps that around the call).

use std::collections::HashSet;
use std::path::Path;

use super::PER_GAME_CAP;
use super::prng::{Prng, substream_seed};
use super::store::{append_block, scan_valid_blocks};
use super::{CorpusError, CorpusRecord};

/// Per-lane commit state. Persists for the lifetime of one ingestion call
/// (fetch: across byte-0 restarts; self-play: across the consumer loop).
pub struct LaneCommitter {
    /// Per-lane FEN dedup set; first-seen-wins. Rebuilt from `lane.bin` on a
    /// cross-process resume so a re-committed FEN is dropped.
    fen_set: HashSet<String>,
    /// Reservoir-cap base seed; the per-game sub-stream is
    /// `substream_seed(cap_seed, game_id)`.
    cap_seed: u64,
    /// Usable-position target (post dedup/cap). `None` = unbounded.
    target: Option<u64>,
    /// Usable positions committed so far (cumulative, incl. resume).
    committed: u64,
    /// The lane byte-offset BEFORE the boundary game's block was appended.
    /// Set iff the exact-target truncation fired for this game (`Some`), so
    /// `truncate_to_valid(lane, off)` drops the partial boundary block and
    /// extend is idempotent. `None` when the build drained to EOF before
    /// reaching the target, or the boundary game committed whole (no
    /// truncation). Initialized to `None`; set at most once (boundary fires
    /// exactly once per build).
    truncated_boundary_offset: Option<u64>,
}

/// Outcome of one [`LaneCommitter::commit_game`] call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitOutcome {
    /// Records actually appended for this game (post dedup/cap/truncation).
    pub usable_committed: u64,
    /// `true` iff the survivor set was empty → no block was written.
    pub empty_post_dedup: bool,
    /// `true` iff the cumulative committed count has reached `target`.
    pub target_reached: bool,
}

impl LaneCommitter {
    /// FETCH path: scan an existing `lane.bin` (no consumer) and rebuild the
    /// dedup set + committed count. Also returns the set of `game_id`s already
    /// committed so the caller can skip re-ingesting them by game_id.
    ///
    /// There is exactly ONE scan of `lane.bin` on this path; the self-play
    /// path constructs via [`LaneCommitter::from_parts`] from the consumer's
    /// own (single) scan instead.
    pub fn resume(
        lane_path: &Path,
        cap_seed: u64,
        target: Option<u64>,
    ) -> Result<(LaneCommitter, HashSet<u64>), CorpusError> {
        let (blocks, _) = scan_valid_blocks(lane_path)?;
        let mut fen_set = HashSet::new();
        let mut committed_ids = HashSet::new();
        let mut committed: u64 = 0;
        for block in &blocks {
            committed_ids.insert(block.game_id);
            committed += block.records.len() as u64;
            for r in &block.records {
                fen_set.insert(r.fen.clone());
            }
        }
        Ok((
            LaneCommitter {
                fen_set,
                cap_seed,
                target,
                committed,
                truncated_boundary_offset: None,
            },
            committed_ids,
        ))
    }

    /// SELF-PLAY path: construct from the consumer's already-built scan state
    /// (no second read of `lane.bin`). `fen_set` and `committed` come from the
    /// consumer's resume scan; `cap_seed` is the campaign cap seed.
    pub fn from_parts(
        fen_set: HashSet<String>,
        committed: u64,
        cap_seed: u64,
        target: Option<u64>,
    ) -> LaneCommitter {
        LaneCommitter {
            fen_set,
            cap_seed,
            target,
            committed,
            truncated_boundary_offset: None,
        }
    }

    /// Apply per-lane dedup → per-game reservoir cap → exact target truncation
    /// to one game's quiet-certified `records`, then append ONE CRC block to
    /// `lane_path` (unless the survivor set is empty, in which case no block is
    /// written and `empty_post_dedup` is `true`).
    ///
    /// The caller MUST pass quiet-certified records in `ply` order, and games
    /// in `game_id` order, for the within-lane first-seen / determinism
    /// contract to hold.
    pub fn commit_game(
        &mut self,
        lane_path: &Path,
        game_id: u64,
        records: Vec<CorpusRecord>,
    ) -> Result<CommitOutcome, CorpusError> {
        // 1. Per-lane FEN dedup against the COMMITTED set (first-seen-wins),
        //    plus within-this-game dedup. We do NOT insert into `fen_set` here —
        //    only positions actually committed enter it (step 4). So a position
        //    discarded by this game's reservoir cap or exact-target truncation
        //    gets a fair chance in a LATER game (richer corpus, still no
        //    duplicates), and crucially `fen_set` mirrors the on-disk lane
        //    exactly — which is what makes a resume scan reconstruct it byte-
        //    for-byte, the foundation of bit-identical extend (ADR-0035 v2).
        let mut seen_this_game: HashSet<String> = HashSet::new();
        let deduped: Vec<CorpusRecord> = records
            .into_iter()
            .filter(|r| !self.fen_set.contains(&r.fen) && seen_this_game.insert(r.fen.clone()))
            .collect();

        // 2. Per-game reservoir cap among dedup-survivors (samples among
        //    not-yet-committed uniques — the "don't waste cap slots" property
        //    is preserved).
        let mut capped = cap_dedup_survivors(deduped, self.cap_seed, game_id, PER_GAME_CAP);

        // 3. Exact target truncation: never overshoot. The boundary game keeps
        //    only `target - committed` records.
        let truncation_fired = if let Some(target) = self.target {
            let room = target.saturating_sub(self.committed);
            if (capped.len() as u64) > room {
                capped.truncate(room as usize);
                true
            } else {
                false
            }
        } else {
            false
        };

        let usable = capped.len() as u64;
        let empty_post_dedup = capped.is_empty();
        if !empty_post_dedup {
            // 4. ONLY the committed (post-cap, post-truncate) FENs enter the
            //    dedup set, so it stays identical to what is on disk.
            for r in &capped {
                self.fen_set.insert(r.fen.clone());
            }
            let pre_offset = append_block(lane_path, game_id, &capped)?;
            self.committed += usable;
            // Record the pre-append offset iff exact-target truncation fired for
            // this game. This is the idempotent truncation point for extend:
            // `truncate_to_valid(lane, off)` drops the partial boundary block.
            // Set only once: once target_reached, no further commits happen.
            if truncation_fired && self.truncated_boundary_offset.is_none() {
                self.truncated_boundary_offset = Some(pre_offset);
            }
        }

        Ok(CommitOutcome {
            usable_committed: usable,
            empty_post_dedup,
            target_reached: self.target_reached(),
        })
    }

    /// Cumulative usable positions committed (incl. any resumed-from-disk).
    pub fn committed(&self) -> u64 {
        self.committed
    }

    /// `true` iff a target is set and the committed count has reached it.
    pub fn target_reached(&self) -> bool {
        self.target.is_some_and(|t| self.committed >= t)
    }

    /// The lane byte-offset before the boundary game's block was appended, if
    /// the exact-target truncation fired. `Some` only when the build landed
    /// mid-game (partial boundary); `None` when the stream drained before the
    /// target or the boundary game committed whole.
    ///
    /// On extend, the driver calls `truncate_to_valid(lane, off)` to drop the
    /// partial boundary block, making the truncation idempotent. The committer
    /// is then rebuilt from the truncated lane and the boundary game is
    /// re-derived from scratch (whole this time, room ample at the new target).
    pub fn truncated_boundary_offset(&self) -> Option<u64> {
        self.truncated_boundary_offset
    }
}

/// Seeded reservoir cap over dedup-survivors. Copy of the pre-M6.H2 consumer
/// `inline_cap_dedup_survivors` semantics (Knuth/Vitter Algorithm R, per-game
/// sub-stream `substream_seed(cap_seed, game_id)`) so output is deterministic
/// for a fixed `cap_seed` and matches the historical pipeline byte-for-byte.
fn cap_dedup_survivors(
    records: Vec<CorpusRecord>,
    cap_seed: u64,
    game_id: u64,
    cap: usize,
) -> Vec<CorpusRecord> {
    if cap == 0 || records.is_empty() {
        return Vec::new();
    }
    if records.len() <= cap {
        return records;
    }

    let mut rng = Prng::new(substream_seed(cap_seed, game_id));
    let mut reservoir: Vec<CorpusRecord> = records[..cap].to_vec();
    for (k, record) in records[cap..].iter().enumerate() {
        let j = rng.below((cap + k + 1) as u64) as usize;
        if j < cap {
            reservoir[j] = record.clone();
        }
    }
    reservoir
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::store::scan_valid_blocks;
    use crate::corpus::{CorpusRecord, DEPTH_RUNG_EXTERNAL, Label, Source};
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static CTR: AtomicU64 = AtomicU64::new(0);
            let n = CTR.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let dir =
                std::env::temp_dir().join(format!("clawfish-corpus-pipeline-{tag}-{pid}-{n}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }
        fn lane(&self) -> PathBuf {
            self.0.join("lane.bin")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn rec(game_id: u64, ply: u32, fen: &str) -> CorpusRecord {
        CorpusRecord {
            fen: fen.to_string(),
            label: Label::Draw,
            source: Source::SelfPlayOffBook,
            game_id,
            ply,
            depth_rung: DEPTH_RUNG_EXTERNAL,
            strata: 0,
        }
    }

    /// Fresh committer (no resume): empty dedup set, zero committed.
    fn fresh(cap_seed: u64, target: Option<u64>) -> LaneCommitter {
        LaneCommitter::from_parts(HashSet::new(), 0, cap_seed, target)
    }

    fn total_records(lane: &Path) -> u64 {
        let (blocks, _) = scan_valid_blocks(lane).unwrap();
        blocks.iter().map(|b| b.records.len() as u64).sum()
    }

    #[test]
    fn committer_dedup_first_seen_wins_within_lane() {
        // Game 0 has fen-a; game 1 also has fen-a → game 1's copy is dropped.
        let td = TempDir::new("dedup-first");
        let mut c = fresh(7, None);

        let o0 = c
            .commit_game(&td.lane(), 0, vec![rec(0, 0, "fen-a w - - 0 1")])
            .unwrap();
        assert_eq!(o0.usable_committed, 1);
        assert!(!o0.empty_post_dedup);
        // target=None → target_reached always false.
        assert!(!o0.target_reached);
        assert!(!c.target_reached());

        let o1 = c
            .commit_game(&td.lane(), 1, vec![rec(1, 0, "fen-a w - - 0 1")])
            .unwrap();
        assert_eq!(o1.usable_committed, 0, "duplicate FEN dropped");
        assert!(o1.empty_post_dedup);
        assert!(
            !o1.target_reached,
            "target=None never signals target_reached"
        );

        assert_eq!(c.committed(), 1);
        assert!(
            !c.target_reached(),
            "target=None: target_reached never true"
        );
        assert_eq!(total_records(&td.lane()), 1, "only game 0's record on disk");
    }

    #[test]
    fn committer_cap_caps_at_per_game_cap() {
        // 15 unique FENs in one game; PER_GAME_CAP=10 → exactly 10 survive,
        // and the survivor set is deterministic for a fixed cap_seed.
        let td_a = TempDir::new("cap-a");
        let td_b = TempDir::new("cap-b");
        let recs: Vec<CorpusRecord> = (0..15u32)
            .map(|p| rec(0, p, &format!("cap-fen-{p} w - - 0 1")))
            .collect();

        let mut ca = fresh(0xABCD, None);
        let oa = ca.commit_game(&td_a.lane(), 0, recs.clone()).unwrap();
        assert_eq!(oa.usable_committed, PER_GAME_CAP as u64);

        // Determinism: same cap_seed + input → identical survivor FENs.
        let mut cb = fresh(0xABCD, None);
        cb.commit_game(&td_b.lane(), 0, recs).unwrap();
        let (ba, _) = scan_valid_blocks(&td_a.lane()).unwrap();
        let (bb, _) = scan_valid_blocks(&td_b.lane()).unwrap();
        let fens_a: Vec<&str> = ba[0].records.iter().map(|r| r.fen.as_str()).collect();
        let fens_b: Vec<&str> = bb[0].records.iter().map(|r| r.fen.as_str()).collect();
        assert_eq!(fens_a, fens_b, "cap is deterministic per cap_seed");
    }

    #[test]
    fn committer_dedup_then_cap_order() {
        // Pins dedup-FIRST ordering adversarially: construct a game whose raw
        // record list has EXACTLY `PER_GAME_CAP` unique FENs plus `PER_GAME_CAP`
        // duplicate copies of those FENs (2*PER_GAME_CAP = 20 total raw records).
        //
        // Dedup-then-cap (correct):
        //   dedup removes the 10 duplicates → 10 unique survive →
        //   cap(10) keeps all 10 → result: exactly PER_GAME_CAP distinct FENs.
        //
        // Cap-then-dedup (incorrect ordering): reservoir of 10 from 20 records.
        // The 20 records contain pairs (unique[i], dup[i]) for i in 0..10. For
        // MOST seeds the reservoir includes at least one pair where both the
        // original and its duplicate make it in; dedup then yields FEWER than 10.
        // For some seeds both members of a pair land, provably yielding < 10 after
        // dedup (see invariant below). The dedup-first assertion (exactly 10
        // distinct survivors for ANY seed) is the load-bearing check; a cap-first
        // implementation cannot make this guarantee.
        //
        // Additional assertion: all survivors are distinct (no dedup-survivor is
        // itself a dup — only reachable if dedup ran first).
        let td = TempDir::new("dedup-then-cap");
        let mut c = fresh(7, None);

        // Establish 10 shared FENs from game 0 (the "already-seen" set).
        let shared: Vec<String> = (0..10)
            .map(|i| format!("shared-fen-{i} w - - 0 1"))
            .collect();
        let recs0: Vec<CorpusRecord> = shared
            .iter()
            .enumerate()
            .map(|(p, f)| rec(0, p as u32, f))
            .collect();
        let o0 = c.commit_game(&td.lane(), 0, recs0).unwrap();
        assert_eq!(o0.usable_committed, PER_GAME_CAP as u64);

        // Game 1: 10 unique fresh FENs + 10 duplicate copies of the shared FENs
        // (20 total raw records). The duplicates are interleaved with the unique
        // records so the reservoir cannot deterministically exclude all of them
        // for an arbitrary seed.
        let unique1: Vec<String> = (0..10)
            .map(|i| format!("unique1-fen-{i} w - - 0 1"))
            .collect();
        // Layout: [unique1[0], shared[0], unique1[1], shared[1], ...] — a clean
        // alternating sequence that guarantees the reservoir "sees" duplicates
        // at regular intervals regardless of which positions it samples.
        let recs1: Vec<CorpusRecord> = unique1
            .iter()
            .zip(shared.iter())
            .enumerate()
            .flat_map(|(i, (u, s))| [rec(1, (2 * i) as u32, u), rec(1, (2 * i + 1) as u32, s)])
            .collect();
        assert_eq!(recs1.len(), 20, "sanity: 20 raw records for game 1");

        let o1 = c.commit_game(&td.lane(), 1, recs1).unwrap();

        // Dedup-then-cap: 10 shared FENs dropped → 10 unique survive → cap=10.
        assert_eq!(
            o1.usable_committed, PER_GAME_CAP as u64,
            "10 shared dups dropped by dedup; 10 fresh unique survive; cap(10) keeps all"
        );

        // All survivors are distinct (no dup survived dedup).
        let (blocks, _) = scan_valid_blocks(&td.lane()).unwrap();
        let g1_block = blocks
            .iter()
            .find(|b| b.game_id == 1)
            .expect("game 1 block");
        let survivor_fens: Vec<&str> = g1_block.records.iter().map(|r| r.fen.as_str()).collect();
        let survivor_set: HashSet<&&str> = survivor_fens.iter().collect();
        assert_eq!(
            survivor_set.len(),
            PER_GAME_CAP,
            "all survivors are distinct (cap ran among dedup-survivors, not raw records)"
        );
        // Every survivor is one of the unique1 FENs (never a shared/dup FEN).
        for fen in &survivor_fens {
            assert!(
                unique1.iter().any(|u| u.as_str() == *fen),
                "survivor {fen:?} must be a unique1 FEN, not a shared dup"
            );
        }

        assert_eq!(c.committed(), PER_GAME_CAP as u64 * 2);
    }

    #[test]
    fn committer_target_truncates_partial_game() {
        // target=12; game 0 commits 10, game 1 would commit 5 but only 2 fit
        // (12 - 10) → exact truncation, target_reached.
        let td = TempDir::new("target-trunc");
        let mut c = fresh(7, Some(12));

        let recs0: Vec<CorpusRecord> = (0..10u32)
            .map(|p| rec(0, p, &format!("g0-fen-{p} w - - 0 1")))
            .collect();
        let o0 = c.commit_game(&td.lane(), 0, recs0).unwrap();
        assert_eq!(o0.usable_committed, 10);
        assert!(!o0.target_reached);

        let recs1: Vec<CorpusRecord> = (0..5u32)
            .map(|p| rec(1, p, &format!("g1-fen-{p} w - - 0 1")))
            .collect();
        let o1 = c.commit_game(&td.lane(), 1, recs1).unwrap();
        assert_eq!(o1.usable_committed, 2, "boundary game truncated to 12-10=2");
        assert!(o1.target_reached);
        assert!(c.target_reached());
        assert_eq!(c.committed(), 12);
        assert_eq!(total_records(&td.lane()), 12);
    }

    #[test]
    fn committer_empty_post_dedup_no_block() {
        // A game whose every FEN is a dup commits no block but advances nothing.
        let td = TempDir::new("empty-post-dedup");
        let mut c = fresh(7, None);
        c.commit_game(&td.lane(), 0, vec![rec(0, 0, "dup w - - 0 1")])
            .unwrap();

        let o1 = c
            .commit_game(&td.lane(), 1, vec![rec(1, 0, "dup w - - 0 1")])
            .unwrap();
        assert!(o1.empty_post_dedup);
        assert_eq!(o1.usable_committed, 0);

        // Exactly one block on disk (game 0); game 1 wrote nothing.
        let (blocks, _) = scan_valid_blocks(&td.lane()).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].game_id, 0);
    }

    #[test]
    fn committer_resume_reconstructs_fen_set_and_count() {
        // Write a lane.bin via a first committer, resume into a second, and
        // assert: committed count + ids reconstructed, and a re-committed dup
        // FEN is dropped (resume-invariance).
        let td = TempDir::new("resume-fen-set");
        {
            let mut c = fresh(7, None);
            c.commit_game(&td.lane(), 0, vec![rec(0, 0, "fen-x w - - 0 1")])
                .unwrap();
            c.commit_game(
                &td.lane(),
                1,
                vec![rec(1, 0, "fen-y w - - 0 1"), rec(1, 1, "fen-z w - - 0 1")],
            )
            .unwrap();
        }

        let (mut c, ids) = LaneCommitter::resume(&td.lane(), 7, None).unwrap();
        assert_eq!(c.committed(), 3, "3 records across two committed games");
        assert_eq!(ids, [0u64, 1u64].into_iter().collect());

        // Game 2 re-emits fen-x (already on disk) + a fresh fen-w → only the
        // fresh one survives (resume-invariance: dedup set rebuilt from disk).
        let o2 = c
            .commit_game(
                &td.lane(),
                2,
                vec![rec(2, 0, "fen-x w - - 0 1"), rec(2, 1, "fen-w w - - 0 1")],
            )
            .unwrap();
        assert_eq!(o2.usable_committed, 1, "fen-x dropped as a resume dup");
        assert_eq!(c.committed(), 4);
    }

    #[test]
    fn committer_target_resume_invariance() {
        // ~target-3 records already on disk + target → only the missing 3 are
        // committed (the cumulative resume guarantee). Mirrors the 1.99M-on-disk
        // + --cap=2M scenario at tiny scale.
        let td = TempDir::new("target-resume");
        let target = 10u64;
        {
            // First run commits 7 records (target - 3) across one game.
            let mut c = fresh(7, Some(target));
            let recs: Vec<CorpusRecord> = (0..7u32)
                .map(|p| rec(0, p, &format!("seed-fen-{p} w - - 0 1")))
                .collect();
            let o = c.commit_game(&td.lane(), 0, recs).unwrap();
            assert_eq!(o.usable_committed, 7);
            assert!(!o.target_reached);
        }

        let (mut c, _) = LaneCommitter::resume(&td.lane(), 7, Some(target)).unwrap();
        assert_eq!(c.committed(), 7);

        // Game 1 offers 6 fresh FENs but only 3 fit (10 - 7) → exact truncation.
        let recs1: Vec<CorpusRecord> = (0..6u32)
            .map(|p| rec(1, p, &format!("g1-fen-{p} w - - 0 1")))
            .collect();
        let o1 = c.commit_game(&td.lane(), 1, recs1).unwrap();
        assert_eq!(o1.usable_committed, 3, "only the missing 3 committed");
        assert!(o1.target_reached);
        assert_eq!(c.committed(), target);
        assert_eq!(total_records(&td.lane()), target);
    }

    #[test]
    fn committer_ii_cap_discarded_position_committed_by_later_game() {
        // (II) dedup-against-committed: a position discarded by one game's
        // reservoir cap is NOT remembered, so a later game reaching the same
        // position commits it. (Under the old dedup-against-all-seen it was
        // lost forever.)
        let td = TempDir::new("ii-cap-discard");
        let mut c = fresh(0xABCD, None);

        // Game 0: 15 unique FENs → cap keeps 10, discards 5.
        let all: Vec<String> = (0..15u32)
            .map(|p| format!("ii-fen-{p} w - - 0 1"))
            .collect();
        let recs0: Vec<CorpusRecord> = all
            .iter()
            .enumerate()
            .map(|(p, f)| rec(0, p as u32, f))
            .collect();
        c.commit_game(&td.lane(), 0, recs0).unwrap();

        let (blocks, _) = scan_valid_blocks(&td.lane()).unwrap();
        let committed: HashSet<&str> = blocks[0].records.iter().map(|r| r.fen.as_str()).collect();
        assert_eq!(committed.len(), PER_GAME_CAP, "game 0 committed exactly 10");
        let discarded: &str = all
            .iter()
            .map(|s| s.as_str())
            .find(|f| !committed.contains(f))
            .expect("5 FENs were discarded by the cap");

        let o1 = c
            .commit_game(&td.lane(), 1, vec![rec(1, 0, discarded)])
            .unwrap();
        assert_eq!(
            o1.usable_committed, 1,
            "a cap-discarded position is committable by a later game under (II)"
        );
        assert_eq!(c.committed(), PER_GAME_CAP as u64 + 1);
    }

    #[test]
    fn committer_ii_resume_fen_set_is_committed_only() {
        // (II) makes the resumed dedup set == the on-disk set EXACTLY: a
        // cap-discarded FEN re-emitted after a resume is committed (it was never
        // stored, so it is not in the rebuilt fen_set). This exact
        // reconstruction is the foundation of bit-identical extend.
        let td = TempDir::new("ii-resume-committed-only");
        let all: Vec<String> = (0..15u32)
            .map(|p| format!("ii-r-fen-{p} w - - 0 1"))
            .collect();
        {
            let mut c = fresh(0xBEEF, None);
            let recs0: Vec<CorpusRecord> = all
                .iter()
                .enumerate()
                .map(|(p, f)| rec(0, p as u32, f))
                .collect();
            c.commit_game(&td.lane(), 0, recs0).unwrap();
        }

        let (blocks, _) = scan_valid_blocks(&td.lane()).unwrap();
        let committed: HashSet<&str> = blocks[0].records.iter().map(|r| r.fen.as_str()).collect();
        let discarded: &str = all
            .iter()
            .map(|s| s.as_str())
            .find(|f| !committed.contains(f))
            .expect("cap discarded 5");

        let (mut c, _) = LaneCommitter::resume(&td.lane(), 0xBEEF, None).unwrap();
        assert_eq!(c.committed(), PER_GAME_CAP as u64);
        let o = c
            .commit_game(&td.lane(), 1, vec![rec(1, 0, discarded)])
            .unwrap();
        assert_eq!(
            o.usable_committed, 1,
            "resume fen_set is committed-only ⇒ a discarded FEN is committable"
        );
    }
}
