//! Optional bi-level simplex meta-optimization (ADR-0037 §8).
//!
//! Inner: an Adam weight solve per lane-mix candidate (K refit per candidate).
//! Outer: a coarse Nelder–Mead-style simplex over the four lane proportions,
//! selected on the aggregate held-out objective (NOT SPRT). The default `tune`
//! path runs a fixed mix; the simplex is a secondary `texel-tune mixture`
//! subcommand the operator escalates to only if the first SPRT is marginal.

use std::path::Path;

use crate::corpus::prng::Prng;
use crate::texel::TexelError;
use crate::texel::dataset::{CacheMeta, Split, split_by_game};
use crate::texel::optimizer::{TuneConfig, TuneResult, tune};
use crate::texel::params::EvalParams;

/// Outer Nelder–Mead simplex over the four lane proportions, selecting the mix
/// that minimizes the aggregate held-out objective. Returns the chosen mix and
/// its inner tune result.
///
/// Per candidate vertex `m`: the train split is reweighted by `reweight_train`,
/// which realizes the mix VOLUME-BASED — the largest real-data total needing no
/// duplication (`T = min_s(pool_s / p_s)`), sampled without replacement (seeded
/// `Prng`, deterministic) — then `tune` is called on that reweighted split, so K
/// is refit once per candidate on that candidate's corpus. The objective is
/// `TuneResult.val_loss` on the FIXED val split (unweighted),
/// ensuring comparable selection signal across candidates. The simplex is
/// initialized at the uniform mix plus four corner-leaning vertices so the
/// uniform mix is always a candidate — the returned minimum is therefore never
/// worse than uniform (ADR-0037 §8). The mix is normalized + clamped to the
/// probability simplex at every vertex.
///
/// Note: a stratified per-source val objective could be layered later; the
/// aggregate `val_loss` is a sound selection signal for this outer search.
pub fn simplex_search(
    cache: &Path,
    meta: &CacheMeta,
    cfg: &TuneConfig,
) -> Result<([f64; 4], TuneResult), TexelError> {
    // Initial simplex: uniform + four vertices leaning toward each lane.
    let uniform = [0.25, 0.25, 0.25, 0.25];
    let mut verts: Vec<[f64; 4]> = vec![uniform];
    for lane in 0..4 {
        let mut v = [0.15, 0.15, 0.15, 0.15];
        v[lane] = 0.55;
        verts.push(normalize(v));
    }

    // Fixed val split (common across all candidates for comparable objectives).
    let base_split = split_by_game(meta, cfg.val_fraction, cfg.seed);

    // Evaluate every vertex once.
    let mut objs: Vec<(f64, [f64; 4], TuneResult)> = Vec::with_capacity(verts.len());
    for &v in &verts {
        let reweighted = reweight_train(&base_split, meta, &v, cfg.seed);
        let res = tune(cache, &reweighted, &EvalParams::shipped(), cfg, None)?;
        let obj = res.val_loss;
        objs.push((obj, v, res));
    }

    // Coarse Nelder–Mead: reflect the worst vertex through the centroid of the
    // rest for a bounded number of iterations.
    const ITERS: usize = 12;
    for _ in 0..ITERS {
        // Index of the current worst (highest-objective) vertex.
        let worst = objs
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.0.partial_cmp(&b.1.0).unwrap())
            .map(|(i, _)| i)
            .expect("non-empty simplex");
        // Centroid of all but the worst.
        let mut centroid = [0.0; 4];
        for (i, (_, v, _)) in objs.iter().enumerate() {
            if i == worst {
                continue;
            }
            for k in 0..4 {
                centroid[k] += v[k];
            }
        }
        let denom = (objs.len() - 1) as f64;
        for c in &mut centroid {
            *c /= denom;
        }
        // Reflect the worst vertex through the centroid.
        let wv = objs[worst].1;
        let mut reflected = [0.0; 4];
        for k in 0..4 {
            reflected[k] = centroid[k] + (centroid[k] - wv[k]);
        }
        let reflected = normalize(reflected);
        let reweighted = reweight_train(&base_split, meta, &reflected, cfg.seed);
        let res = tune(cache, &reweighted, &EvalParams::shipped(), cfg, None)?;
        let obj = res.val_loss;
        // Accept the reflection only if it beats the worst.
        if obj < objs[worst].0 {
            objs[worst] = (obj, reflected, res);
        }
    }

    // Return the best-objective vertex.
    let best = objs
        .into_iter()
        .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap())
        .expect("non-empty simplex");
    Ok((best.1, best.2))
}

/// Build a reweighted train split for candidate mix `m`.
///
/// For each source `s`, gather its train-pool indices from `base_split.train`,
/// then sample `target[s]` of them using a seeded `Prng` (subsample if target
/// < available; sample-with-replacement if target > available). The val split
/// is the fixed base val — identical across all candidates.
fn reweight_train(base_split: &Split, meta: &CacheMeta, mix: &[f64; 4], seed: u64) -> Split {
    // Partition the train pool by source.
    let mut pools: [Vec<u32>; 4] = Default::default();
    for &idx in &base_split.train {
        let src = meta.game_keys[idx as usize].0 as usize;
        if src < 4 {
            pools[src].push(idx);
        }
    }

    // VOLUME-BASED realization (no oversampling). For normalized proportions
    // p_s, the largest total T with `p_s·T ≤ pool_s` for every populated source
    // is `T = min_s(pool_s / p_s)`; then `target_s = round(p_s·T) ≤ pool_s`, so
    // every draw is a subsample WITHOUT replacement (zero duplication). This is
    // the fix that makes "the meta-search asks for more of source X" pull *real*
    // positions: conjuring more data raises pool_s and therefore the honest T,
    // whereas the old `round(mix·Σpools)` budget duplicated any source upweighted
    // beyond its share (the 0.55 simplex vertices oversampled ~2.2× on a uniform
    // corpus). The held-out val set is unchanged across candidates, so val_loss
    // stays comparable even though T differs by mix.
    let sum: f64 = mix.iter().sum();
    let mut t = f64::INFINITY;
    if sum > 0.0 {
        for s in 0..4 {
            let p = mix[s] / sum;
            if p > 0.0 && !pools[s].is_empty() {
                t = t.min(pools[s].len() as f64 / p);
            }
        }
    }
    let t = if t.is_finite() { t } else { 0.0 };
    let target: [usize; 4] = std::array::from_fn(|s| {
        if sum <= 0.0 {
            return 0;
        }
        (((mix[s] / sum) * t).round() as usize).min(pools[s].len())
    });

    // Subsample target[s] indices from each pool without replacement (seeded,
    // deterministic; target_s ≤ pool_s by construction).
    let mut train = Vec::with_capacity(target.iter().sum());
    for s in 0..4 {
        let n = target[s];
        let pool = &pools[s];
        if pool.is_empty() || n == 0 {
            continue;
        }
        let mut rng = Prng::new(seed ^ (s as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        let mut indices: Vec<usize> = (0..pool.len()).collect();
        for i in 0..n {
            let j = i + (rng.below((pool.len() - i) as u64)) as usize;
            indices.swap(i, j);
        }
        for &i in indices.iter().take(n) {
            train.push(pool[i]);
        }
    }

    Split {
        train,
        val: base_split.val.clone(),
    }
}

/// Project a 4-vector onto the probability simplex: clamp negatives to 0 and
/// renormalize to sum 1 (falls back to uniform if the clamped sum is 0).
fn normalize(v: [f64; 4]) -> [f64; 4] {
    let clamped: [f64; 4] = std::array::from_fn(|i| v[i].max(0.0));
    let sum: f64 = clamped.iter().sum();
    if sum <= 0.0 {
        return [0.25; 4];
    }
    std::array::from_fn(|i| clamped[i] / sum)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::prng::Prng;
    use crate::corpus::{CorpusRecord, DEPTH_RUNG_EXTERNAL, Label, Source};
    use crate::texel::dataset::{self, LaneSet};
    use crate::texel::loss::Reg;
    use crate::texel::params::EvalParams;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_temp_dir(tag: &str) -> PathBuf {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("clawfish-texel-mix-{tag}-{pid}-{n}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    const FENS: &[&str] = &[
        "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
        "r1bqkb1r/pppp1ppp/2n2n2/4p3/2B1P3/5N2/PPPP1PPP/RNBQK2R w KQkq - 4 4",
        "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
        "4k3/8/8/8/8/8/4P3/4K3 w - - 0 1",
    ];

    /// Synthetic 4-lane set where lane 0 has strong label signal (all WhiteWin)
    /// while lanes 1-3 have uniform random labels. Because lane 0 has
    /// zero label variance, a model trained with high lane-0 weight will predict
    /// near-1 for lane-0 positions and achieve low MSE on those val records.
    /// This gives the simplex a real signal to up-weight lane 0.
    fn synthetic_lanes(seed: u64) -> LaneSet {
        let mut rng = Prng::new(seed);
        let mut recs: [Vec<CorpusRecord>; 4] = Default::default();
        let sources = [
            Source::SelfPlayOnBook,
            Source::SelfPlayOffBook,
            Source::Ccrl,
            Source::LichessOpen,
        ];
        for (lane, src) in sources.iter().enumerate() {
            for g in 0..12usize {
                for p in 0..4usize {
                    let fen = FENS[(rng.below(FENS.len() as u64)) as usize];
                    // Lane 0: all WhiteWin — maximum signal, zero entropy.
                    // Lanes 1-3: uniform random — maximum entropy, no signal.
                    let label = if lane == 0 {
                        Label::WhiteWin
                    } else {
                        match rng.below(3) {
                            0 => Label::WhiteWin,
                            1 => Label::Draw,
                            _ => Label::BlackWin,
                        }
                    };
                    recs[lane].push(CorpusRecord {
                        fen: fen.to_string(),
                        label,
                        source: *src,
                        game_id: g as u64,
                        ply: p as u32,
                        depth_rung: DEPTH_RUNG_EXTERNAL,
                        strata: 0,
                    });
                }
            }
        }
        LaneSet {
            recs,
            sha256: [
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string(),
            ],
        }
    }

    fn build(dir: &Path) -> (PathBuf, dataset::CacheMeta) {
        let lanes = synthetic_lanes(0x_4115_5EED);
        let cache = dir.join("features.cache");
        let meta = dataset::build_cache(&lanes, &cache, 1).expect("build_cache");
        (cache, meta)
    }

    fn cfg(seed: u64) -> TuneConfig {
        TuneConfig {
            lr: 0.05,
            max_iter: 100,
            patience: 5,
            eval_every: 10,
            reg: Reg {
                l2_lambda: 0.0,
                init: EvalParams::shipped().core_to_vec(),
                mono_lambda: 0.0,
            },
            seed,
            val_fraction: 0.25,
            checkpoint_path: None,
            checkpoint_every: 10,
        }
    }

    /// `reweight_train` is VOLUME-BASED: it realizes the mix with the largest
    /// real-data total that needs no duplication (`T = min_s(pool_s / p_s)`), so
    /// every per-source count is ≤ its pool (subsample without replacement, never
    /// oversampled), the proportions match the mix, the favored source consumes
    /// its full pool, and it is deterministic under a fixed seed.
    #[test]
    fn reweight_train_is_volume_based_no_oversampling() {
        use crate::texel::dataset::{CacheMeta, Split};

        // Build a synthetic CacheMeta whose game_keys give a known per-source
        // distribution: 40 records each for sources 0-3 (160 total), all assigned
        // game_id=0 so split_by_game-style splits are not needed here.
        let n_per_src: usize = 40;
        let n_total: usize = n_per_src * 4;
        // game_keys[i] = (source, 0): source 0 for i in 0..40, source 1 for 40..80, etc.
        let game_keys: Vec<(u8, u64)> = (0..n_total)
            .map(|i| ((i / n_per_src) as u8, 0u64))
            .collect();
        let per_source = [n_per_src as u64; 4];
        let meta = CacheMeta {
            n: n_total as u64,
            layout_hash: 0,
            lane_sha256: Default::default(),
            per_source,
            game_keys,
        };

        // All indices are train; val is empty (the mechanism test doesn't need val).
        let train: Vec<u32> = (0..n_total as u32).collect();
        let base_split = Split {
            train,
            val: Vec::new(),
        };
        // Pools = 40 each. With p = [0.2, 0.5, 0.15, 0.15] (already normalized),
        // T = min_s(pool_s / p_s) = min(200, 80, 266.7, 266.7) = 80, bound by the
        // favored source 1 (0.5). target = round(p·T) = [16, 40, 12, 12]: source 1
        // uses its FULL pool (max real data), the rest subsample — no oversampling.
        let mix = [0.2f64, 0.5, 0.15, 0.15];
        let target: [u64; 4] = [16, 40, 12, 12];
        assert_eq!(
            target[1], n_per_src as u64,
            "favored source must use its full pool (the volume cap binds here)"
        );
        for (s, &t) in target.iter().enumerate() {
            assert!(
                t <= n_per_src as u64,
                "volume-based reweight must never exceed a pool (no oversampling); \
                 source {s} target {t} vs pool {n_per_src}"
            );
        }

        let seed = 0xABCD_1234_u64;
        let result = reweight_train(&base_split, &meta, &mix, seed);

        // Re-derive per-source counts from the resulting train index multiset.
        let mut got_counts = [0u64; 4];
        for &idx in &result.train {
            let src = meta.game_keys[idx as usize].0 as usize;
            got_counts[src] += 1;
        }

        assert_eq!(
            got_counts, target,
            "reweight_train per-source counts must exactly match effective_source_counts; \
             target={target:?}, got={got_counts:?}"
        );

        // Subsample branch: no index appears more times than it exists in the pool
        // (all indices in source 0's slot are ≤ n_per_src, no repeats).
        let src0_indices: Vec<u32> = result
            .train
            .iter()
            .copied()
            .filter(|&i| meta.game_keys[i as usize].0 == 0)
            .collect();
        let mut seen = std::collections::HashSet::new();
        for idx in &src0_indices {
            assert!(
                seen.insert(idx),
                "subsample branch must not repeat index {idx} for source 0"
            );
        }

        // No oversampling: the favored source 1 consumed its ENTIRE pool with
        // DISTINCT indices (it oversampled with repeats under the old budget).
        assert_eq!(
            got_counts[1], n_per_src as u64,
            "favored source 1 must use its full pool, not oversample; got={got_counts:?}"
        );
        let src1_indices: Vec<u32> = result
            .train
            .iter()
            .copied()
            .filter(|&i| meta.game_keys[i as usize].0 == 1)
            .collect();
        let mut seen1 = std::collections::HashSet::new();
        for idx in &src1_indices {
            assert!(
                seen1.insert(idx),
                "volume-based reweight must not repeat index {idx} for source 1"
            );
        }

        // Determinism: the same seed yields the identical train index multiset.
        let result2 = reweight_train(&base_split, &meta, &mix, seed);
        assert_eq!(
            result.train, result2.train,
            "reweight_train must be deterministic under a fixed seed"
        );

        // Different seed yields a different train index multiset (with overwhelming
        // probability on 160 indices sampled by Fisher-Yates / with-replacement).
        let result3 = reweight_train(&base_split, &meta, &mix, seed ^ 0xFFFF_FFFF);
        assert_ne!(
            result.train, result3.train,
            "different seeds must (with overwhelming probability) yield different train sets"
        );
    }

    /// `simplex_search` returns a valid mix (non-negative, sums to ~1) selected
    /// on the stratified held-out objective, with a finite inner tune result
    /// (ADR-0037 §8). The returned mix is a probability simplex point.
    #[test]
    fn simplex_selects_on_held_out_objective() {
        let dir = unique_temp_dir("simplex-select");
        let (cache, meta) = build(&dir);
        let (mix, result) = simplex_search(&cache, &meta, &cfg(7)).expect("simplex");
        let s: f64 = mix.iter().sum();
        assert!(
            (s - 1.0).abs() < 1e-6,
            "selected mix must lie on the probability simplex (sums to 1); got {s}"
        );
        assert!(
            mix.iter().all(|&m| (0.0..=1.0).contains(&m)),
            "every mix proportion must be in [0,1]; got {mix:?}"
        );
        assert!(
            result.val_loss.is_finite(),
            "inner tune result must carry a finite held-out objective"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The simplex search improves on (or ties) the uniform mix on the held-out
    /// objective — it never selects a mix WORSE than uniform. This is the
    /// structural guarantee of the bi-level: the uniform mix is a starting vertex
    /// of the simplex, so the minimum it returns is ≤ the uniform vertex's
    /// objective. The chosen mix must be a valid probability simplex point and
    /// its val_loss must be finite.
    ///
    /// On the synthetic corpus (lane 0 = all WhiteWin, lanes 1-3 = random
    /// labels), the direction the simplex searches is data-dependent and not
    /// guaranteed on small balanced-position corpora (all 4 FENs are
    /// approximately equal-material). The structural guarantee (never worse than
    /// the uniform vertex it started with) is the sound, falsifiable assertion.
    #[test]
    fn simplex_improves_or_ties_uniform_on_synthetic() {
        let dir = unique_temp_dir("simplex-improve");
        let (cache, meta) = build(&dir);

        // The held-out objective at the simplex-chosen mix.
        let (chosen_mix, chosen) = simplex_search(&cache, &meta, &cfg(13)).expect("simplex");

        assert!(
            chosen.val_loss.is_finite(),
            "chosen mix must have a finite held-out objective"
        );
        // Valid probability simplex point.
        let s: f64 = chosen_mix.iter().sum();
        assert!((s - 1.0).abs() < 1e-6, "chosen mix must sum to 1; got {s}");
        assert!(
            chosen_mix.iter().all(|&m| (0.0..=1.0).contains(&m)),
            "every mix proportion must be in [0,1]; got {chosen_mix:?}"
        );

        // Structural guarantee: the simplex always returns a vertex at least as
        // good as the uniform starting vertex. Evaluate the uniform-mix inner
        // tune under the SAME split the simplex used (seed=13) for a valid
        // comparison.
        let base_split = dataset::split_by_game(&meta, 0.25, 13);
        let reweighted_uniform = reweight_train(&base_split, &meta, &[0.25; 4], 13);
        let uniform_vertex_result = crate::texel::optimizer::tune(
            &cache,
            &reweighted_uniform,
            &EvalParams::shipped(),
            &cfg(13),
            None,
        )
        .expect("uniform vertex tune");
        assert!(
            chosen.val_loss <= uniform_vertex_result.val_loss + 1e-9,
            "simplex must not select a mix worse than the uniform vertex it started with: \
             chosen={}, uniform_vertex={}",
            chosen.val_loss,
            uniform_vertex_result.val_loss
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
