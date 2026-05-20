//! Game-level train/val split + held-out integrity (research §6.3).
//!
//! Split key is `game_id` (whole game → one split) so dedup-removal can
//! never make a game straddle splits. Game-disjointness is a HARD
//! must-PASS; post-split FEN-leakage ratio is reported and must be ≤ τ
//! (opening-transposition leakage is unavoidable, bounded not zero —
//! research §6.2).
//!
//! Implemented by the M6.G `split` coder slice per `docs/plans/m6.g.md` §3.8.

use std::collections::{HashMap, HashSet};

use super::{CorpusRecord, prng::substream_seed};

/// A game-level train/validation partition.
pub struct Split {
    /// Training records.
    pub train: Vec<CorpusRecord>,
    /// Held-out validation records.
    pub val: Vec<CorpusRecord>,
}

/// Integrity report for a [`Split`].
#[derive(Clone, Debug)]
pub struct SplitReport {
    /// `true` iff no `game_id` appears in both splits (HARD must-PASS).
    pub game_disjoint: bool,
    /// Fraction of val FENs also present in train (reported; ≤ τ).
    pub fen_leakage_ratio: f64,
    /// Distinct games in train.
    pub train_games: u64,
    /// Distinct games in val.
    pub val_games: u64,
}

/// Assign whole games to train/val by seeded hash of `game_id`.
///
/// For each distinct `game_id`, the assignment is determined by
/// `substream_seed(seed, game_id)` mapped into `[0, u64::MAX]` and compared
/// to `val_fraction * u64::MAX`. If the hash falls below the threshold the
/// game goes to val; otherwise train. All records sharing a `game_id` go to
/// exactly one side — a game can never straddle the splits.
///
/// The split is deterministic from `(seed, val_fraction)` and independent of
/// the order records appear in `records`.
pub fn split_by_game(records: Vec<CorpusRecord>, val_fraction: f64, seed: u64) -> Split {
    debug_assert!(
        (0.0..=1.0).contains(&val_fraction),
        "val_fraction must be in [0, 1]"
    );

    // Threshold in u64 space: a game_id's hash < threshold → val.
    // Clamp to avoid edge-case panics at val_fraction == 1.0 which would
    // produce threshold > u64::MAX.
    let threshold = (val_fraction * (u64::MAX as f64)) as u64;

    let mut train = Vec::new();
    let mut val = Vec::new();

    // Cache per-game assignments so each game_id is only hashed once.
    let mut assignments: HashMap<u64, bool> = HashMap::new();

    for record in records {
        let is_val = *assignments.entry(record.game_id).or_insert_with(|| {
            let hash = substream_seed(seed, record.game_id);
            hash < threshold
        });

        if is_val {
            val.push(record);
        } else {
            train.push(record);
        }
    }

    Split { train, val }
}

/// Game-disjointness (hard) + FEN-leakage ratio (reported).
///
/// `game_disjoint` is a HARD invariant for any valid split: every `game_id`
/// appears in exactly one side. `fen_leakage_ratio` measures the fraction of
/// distinct val FENs that also appear in train — opening-transposition leakage
/// makes this nonzero by construction but it should be small.
pub fn split_integrity(split: &Split) -> SplitReport {
    let train_game_ids: HashSet<u64> = split.train.iter().map(|r| r.game_id).collect();
    let val_game_ids: HashSet<u64> = split.val.iter().map(|r| r.game_id).collect();

    let game_disjoint = train_game_ids.is_disjoint(&val_game_ids);

    let train_fens: HashSet<&str> = split.train.iter().map(|r| r.fen.as_str()).collect();
    let val_fens: HashSet<&str> = split.val.iter().map(|r| r.fen.as_str()).collect();

    let fen_leakage_ratio = if val_fens.is_empty() {
        0.0
    } else {
        let leaked = val_fens.iter().filter(|f| train_fens.contains(*f)).count();
        leaked as f64 / val_fens.len() as f64
    };

    SplitReport {
        game_disjoint,
        fen_leakage_ratio,
        train_games: train_game_ids.len() as u64,
        val_games: val_game_ids.len() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{DEPTH_RUNG_EXTERNAL, Label, Source};
    use proptest::prelude::*;

    fn make_record(game_id: u64, fen: &str) -> CorpusRecord {
        CorpusRecord {
            fen: fen.to_string(),
            label: Label::Draw,
            source: Source::SelfPlay,
            game_id,
            ply: 10,
            depth_rung: DEPTH_RUNG_EXTERNAL,
            strata: 0,
        }
    }

    fn make_records_for_game(game_id: u64, count: u32) -> Vec<CorpusRecord> {
        (0..count)
            .map(|i| make_record(game_id, &format!("fen_g{game_id}_p{i}")))
            .collect()
    }

    /// Verifies the documented pipeline discipline: split operates on an
    /// arbitrary record set and makes no dedup/cap assumptions of its own.
    /// The caller (corpus build) must run dedup → cap → split in that order;
    /// split is correct on any input.
    #[test]
    fn pipeline_order_dedup_then_cap_then_split() {
        // Build records that already look like post-dedup-and-cap output:
        // each FEN unique, at most PER_GAME_CAP per game. Split must not
        // further deduplicate, cap, or otherwise filter the record set.
        let mut records = Vec::new();
        for game_id in 0..10u64 {
            for pos in 0..5u32 {
                records.push(make_record(game_id, &format!("unique_fen_{game_id}_{pos}")));
            }
        }
        let total_in = records.len();

        let split = split_by_game(records, 0.2, 42);
        // Split must be lossless: all records land in exactly one side.
        assert_eq!(split.train.len() + split.val.len(), total_in);
    }

    #[test]
    fn no_game_spans_train_and_val() {
        // 20 games × 5 records each.
        let records: Vec<CorpusRecord> = (0..20u64)
            .flat_map(|g| make_records_for_game(g, 5))
            .collect();

        let split = split_by_game(records, 0.2, 0xDEAD_BEEF);
        let report = split_integrity(&split);

        assert!(
            report.game_disjoint,
            "game_disjoint must be true — every game_id must appear in exactly one split"
        );
    }

    #[test]
    fn ratio_within_tolerance() {
        // With 500 games we expect the val fraction to be within ±5% of 0.2.
        const N_GAMES: u64 = 500;
        const VAL_FRACTION: f64 = 0.2;
        const TOLERANCE: f64 = 0.05;

        let records: Vec<CorpusRecord> = (0..N_GAMES)
            .flat_map(|g| make_records_for_game(g, 3))
            .collect();

        let split = split_by_game(records, VAL_FRACTION, 12345);
        let report = split_integrity(&split);

        let observed_val = report.val_games as f64 / N_GAMES as f64;
        assert!(
            (observed_val - VAL_FRACTION).abs() <= TOLERANCE,
            "observed val fraction {observed_val:.3} should be within {TOLERANCE} of {VAL_FRACTION}"
        );
    }

    /// Construct a transposition (same FEN) shared across a train game and a
    /// val game, assert the FEN-leakage ratio is computed and reported.
    #[test]
    fn fen_leakage_ratio_reported_le_tau() {
        // We need at least one game in each split under the SAME seed
        // we'll use for `split_by_game` — otherwise the routing the
        // pair-finder predicts and the split's own hashing disagree, and
        // the FEN we plant in the "train" game might land in val (or
        // vice versa). Use seed=999 in both calls.
        const SHARED_FEN: &str = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1";

        let (train_game, val_game) = find_split_pair(0.5, 999, 10);

        // The shared FEN appears in both a train game and a val game (simulating
        // a common opening transposition).
        let records = vec![
            make_record(train_game, SHARED_FEN),
            make_record(train_game, "unique_train_fen_1"),
            make_record(val_game, SHARED_FEN),
            make_record(val_game, "unique_val_fen_1"),
        ];

        let split = split_by_game(records, 0.5, 999);
        let report = split_integrity(&split);

        // The leakage ratio must be computed (not panicked, not NaN).
        assert!(report.fen_leakage_ratio.is_finite());
        // The shared FEN leaks from train to val, so ratio > 0.
        assert!(
            report.fen_leakage_ratio > 0.0,
            "expected at least one FEN to leak (the shared opening position)"
        );
        // The ratio must be ≤ 1.0 (trivially bounded).
        assert!(report.fen_leakage_ratio <= 1.0);
    }

    /// Given `val_fraction` and a seed, find two distinct game_ids in `0..limit`
    /// that land in different splits. Returns `(train_game_id, val_game_id)`.
    fn find_split_pair(val_fraction: f64, seed: u64, limit: u64) -> (u64, u64) {
        let threshold = (val_fraction * (u64::MAX as f64)) as u64;
        let mut train_id = None;
        let mut val_id = None;
        for id in 0..limit {
            let hash = substream_seed(seed, id);
            if hash < threshold {
                val_id = Some(id);
            } else {
                train_id = Some(id);
            }
            if train_id.is_some() && val_id.is_some() {
                break;
            }
        }
        (
            train_id.expect("no train game found in range"),
            val_id.expect("no val game found in range"),
        )
    }

    proptest! {
        /// For any seed and any val_fraction in [0,1], split_by_game must
        /// always produce a game-disjoint split.
        #[test]
        fn split_always_game_disjoint(
            seed in any::<u64>(),
            val_fraction in 0.0f64..=1.0f64,
            // Generate 0..20 games with 1..5 records each.
            game_count in 0usize..20,
            records_per_game in 1usize..6,
        ) {
            let records: Vec<CorpusRecord> = (0..game_count as u64)
                .flat_map(|g| {
                    (0..records_per_game as u32).map(move |p| {
                        make_record(g, &format!("fen_{g}_{p}"))
                    })
                })
                .collect();

            let split = split_by_game(records, val_fraction, seed);
            let report = split_integrity(&split);

            prop_assert!(
                report.game_disjoint,
                "game_disjoint violated: seed={seed}, val_fraction={val_fraction}"
            );
        }
    }
}
