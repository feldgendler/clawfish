//! Optional bi-level simplex meta-optimization (ADR-0037 §8).
//!
//! Inner: an Adam weight solve per lane-mix candidate (K refit per candidate).
//! Outer: a coarse Nelder–Mead-style simplex over the four lane proportions,
//! selected on the aggregate held-out objective (NOT SPRT). The default `tune`
//! path runs a fixed mix; the simplex is a secondary `texel-tune mixture`
//! subcommand the operator escalates to only if the first SPRT is marginal.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

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
    progress_path: Option<&Path>,
    checkpoint_path: Option<&Path>,
) -> Result<([f64; 4], TuneResult), TexelError> {
    Ok(
        simplex_search_impl(cache, meta, cfg, progress_path, checkpoint_path, None)?
            .expect("simplex_search without a stop bound always runs to completion"),
    )
}

/// Seed vertices in the initial simplex (uniform + one corner-leaning vertex
/// per lane).
const SEED_COUNT: usize = 5;
/// Coarse Nelder–Mead reflection iterations after the seed vertices.
const REFLECT_ITERS: usize = 12;

/// Global-best vertex found so far, with the full inner-tune result needed both
/// to return it and to persist/restore it across a checkpoint.
struct BestVertex {
    val: f64,
    mix: [f64; 4],
    result: TuneResult,
}

/// Persisted meta-search state — enough to resume the simplex at an inner-tune
/// boundary without redoing any completed inner tune (the meta-tune post-mortem,
/// `docs/milestones/meta-tune-postmortem.md`). Written atomically after every
/// inner tune. Encoded as a length-framed BINARY layout with each `f64` written
/// by its IEEE-754 bit pattern (`to_bits`) — NOT JSON: serde_json's shortest-
/// decimal float serialization is not bit-exact for every `f64` (it can differ
/// by ≤1 ULP), which would let a resumed search select a different worst/best
/// vertex. The bit-pattern layout (mirroring `optimizer::save_checkpoint`)
/// round-trips every bit exactly and ends with a CRC so a torn write is
/// rejected. Inner-tune granularity is sufficient now that `optimizer::tune` no
/// longer re-streams the cache per epoch (a kill loses at most the one in-flight
/// inner tune, which restarts from scratch).
#[derive(Clone, PartialEq, Debug)]
struct MixCheckpoint {
    /// Fingerprint of the `(cfg, corpus)` the persisted objectives were computed
    /// under (see [`mix_config_fingerprint`]). A resume whose current fingerprint
    /// differs is treated as a cold start — the restored `objs`/`best` were
    /// computed under a different split/corpus and would make the simplex compare
    /// incomparable objectives. Mirrors the `layout_hash`/`lane_sha256` staleness
    /// guard the feature cache uses.
    config_hash: u64,
    /// Current simplex: `(held-out objective, mix)` per vertex.
    objs: Vec<(f64, [f64; 4])>,
    /// Global-best vertex mix.
    best_mix: [f64; 4],
    /// Global-best vertex tuned core weight vector (rebuilds the `TuneResult`).
    best_core: Vec<f64>,
    /// Global-best frozen K.
    best_k: f64,
    /// Global-best held-out objective.
    best_val: f64,
    /// Global-best inner-tune iteration count.
    best_iters: u64,
    /// Seed vertices evaluated (`0..=SEED_COUNT`).
    seeds_done: usize,
    /// Reflection iterations completed (`0..=REFLECT_ITERS`).
    reflections_done: usize,
    /// Total inner tunes completed (`= seeds_done + reflections_done`).
    tunes_done: usize,
}

/// Inner implementation of [`simplex_search`]. `stop_after` is a test hook:
/// when `Some(n)`, the search returns `Ok(None)` (after writing the checkpoint)
/// as soon as `n` inner tunes have completed, simulating an interruption at an
/// inner-tune boundary. Production passes `None` and always gets `Ok(Some(_))`.
///
/// The returned vertex is the global best, tracked in `best` *independently* of
/// the simplex `objs` (so it survives even though `objs` drops the per-vertex
/// `TuneResult`). Its objective equals the old `min_by(objs)` objective: the
/// best vertex is never the Nelder–Mead *worst*, so it is never the one a
/// reflection replaces, hence it stays in the simplex throughout. The *mix* it
/// returns can differ from the old `min_by` only under tied objectives (the old
/// `min_by` kept the first equal-minimum vertex; `best` keeps the first to
/// *reach* the minimum) — an immaterial tiebreak, the held-out objective is
/// identical and any min-objective mix is an equally valid SPRT candidate.
fn simplex_search_impl(
    cache: &Path,
    meta: &CacheMeta,
    cfg: &TuneConfig,
    progress_path: Option<&Path>,
    checkpoint_path: Option<&Path>,
    stop_after: Option<usize>,
) -> Result<Option<([f64; 4], TuneResult)>, TexelError> {
    // Initial simplex: uniform + one vertex leaning toward each lane.
    let uniform = [0.25, 0.25, 0.25, 0.25];
    let mut verts: Vec<[f64; 4]> = vec![uniform];
    for lane in 0..4 {
        let mut v = [0.15, 0.15, 0.15, 0.15];
        v[lane] = 0.55;
        verts.push(normalize(v));
    }
    debug_assert_eq!(verts.len(), SEED_COUNT);

    // Fixed val split (common across all candidates for comparable objectives).
    let base_split = split_by_game(meta, cfg.val_fraction, cfg.seed);

    // One inner Adam solve at lane-mix `mix`, labelled for the progress log.
    let run_inner = |mix: &[f64; 4], label: String| -> Result<TuneResult, TexelError> {
        let mut inner_cfg = cfg.clone();
        inner_cfg.progress_label = Some(label);
        let reweighted = reweight_train(&base_split, meta, mix, cfg.seed);
        tune(cache, &reweighted, &EvalParams::shipped(), &inner_cfg, None)
    };

    // Restore prior state (resume) or cold-start. A checkpoint whose config
    // fingerprint does not match this run's (different seed / val-fraction /
    // iteration knobs / corpus) is discarded — its objectives are incomparable
    // to what this run will compute.
    let config_hash = mix_config_fingerprint(cfg, meta);
    let restored = checkpoint_path
        .and_then(load_mix_checkpoint)
        .filter(|c| c.config_hash == config_hash);
    let (mut objs, mut best, seeds_done, reflections_done, mut tunes_done) = match restored {
        Some(c) => {
            let result = TuneResult {
                params: EvalParams::shipped().with_core(&c.best_core),
                k: c.best_k,
                val_loss: c.best_val,
                iters: c.best_iters,
            };
            let best = Some(BestVertex {
                val: c.best_val,
                mix: c.best_mix,
                result,
            });
            (c.objs, best, c.seeds_done, c.reflections_done, c.tunes_done)
        }
        None => (Vec::<(f64, [f64; 4])>::new(), None, 0usize, 0usize, 0usize),
    };

    // Evaluate the remaining seed vertices.
    for k in seeds_done..verts.len() {
        let v = verts[k];
        let label = format!("meta seed {}/{} mix={v:.3?}", k + 1, verts.len());
        let res = run_inner(&v, label)?;
        let obj = res.val_loss;
        objs.push((obj, v));
        tunes_done += 1;
        if best.as_ref().is_none_or(|b| obj < b.val) {
            best = Some(BestVertex {
                val: obj,
                mix: v,
                result: res,
            });
        }
        let b = best.as_ref().expect("best set after first seed");
        eprintln!(
            "[meta] seed tune {tunes_done}/{} mix={v:.3?}  val_loss={obj:.6}  \
             best_mix={:.3?}  best_val={:.6}",
            SEED_COUNT + REFLECT_ITERS,
            b.mix,
            b.val,
        );
        record_progress(
            progress_path,
            checkpoint_path,
            config_hash,
            &objs,
            b,
            k + 1,
            reflections_done,
            tunes_done,
        )?;
        if stop_after == Some(tunes_done) {
            return Ok(None);
        }
    }

    // Coarse Nelder–Mead: reflect the worst vertex through the centroid of the
    // rest for a bounded number of iterations.
    for i in reflections_done..REFLECT_ITERS {
        // Index of the current worst (highest-objective) vertex.
        let worst = objs
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.0.partial_cmp(&b.1.0).unwrap())
            .map(|(i, _)| i)
            .expect("non-empty simplex");
        // Centroid of all but the worst.
        let mut centroid = [0.0; 4];
        for (j, (_, v)) in objs.iter().enumerate() {
            if j == worst {
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
        let label = format!(
            "meta iter {}/{REFLECT_ITERS} reflect mix={reflected:.3?}",
            i + 1
        );
        let res = run_inner(&reflected, label)?;
        let obj = res.val_loss;
        // Accept the reflection into the simplex only if it beats the worst.
        let accepted = accept_reflection(&mut objs, worst, obj, reflected);
        tunes_done += 1;
        if best.as_ref().is_none_or(|b| obj < b.val) {
            best = Some(BestVertex {
                val: obj,
                mix: reflected,
                result: res,
            });
        }
        let b = best.as_ref().expect("best set after seeds");
        eprintln!(
            "[meta] iter {}/{REFLECT_ITERS} reflect mix={reflected:.3?}  val_loss={obj:.6}  \
             accepted={accepted}  best_mix={:.3?}  best_val={:.6}",
            i + 1,
            b.mix,
            b.val,
        );
        record_progress(
            progress_path,
            checkpoint_path,
            config_hash,
            &objs,
            b,
            verts.len(),
            i + 1,
            tunes_done,
        )?;
        if stop_after == Some(tunes_done) {
            return Ok(None);
        }
    }

    let best = best.expect("at least one inner tune ran");
    Ok(Some((best.mix, best.result)))
}

/// Accept a reflected vertex into the simplex iff its objective beats the
/// current worst vertex's; returns whether it was accepted. Extracted as a pure
/// helper so the accept/replace rule (the core Nelder–Mead trajectory driver)
/// is unit-testable in isolation.
fn accept_reflection(objs: &mut [(f64, [f64; 4])], worst: usize, obj: f64, mix: [f64; 4]) -> bool {
    let accepted = obj < objs[worst].0;
    if accepted {
        objs[worst] = (obj, mix);
    }
    accepted
}

/// Fingerprint of the configuration + corpus an inner-tune objective depends on,
/// so a resume can reject a checkpoint written under a different `(cfg, corpus)`
/// (see [`MixCheckpoint::config_hash`]). Folds the meta-search-affecting knobs
/// (`seed`, `val_fraction`, `max_iter`, `patience`, `eval_every`) and the corpus
/// identity (`layout_hash` + the per-lane sha256s) into a 64-bit CRC pair.
///
/// `lr`/`reg` are deliberately omitted because `cmd_mixture` hardcodes them; if
/// the `mixture` subcommand ever exposes an `--lr`/regularization flag, add it
/// here too, or a stale checkpoint under a different value would wrongly match.
fn mix_config_fingerprint(cfg: &TuneConfig, meta: &CacheMeta) -> u64 {
    let mut buf = Vec::new();
    buf.extend_from_slice(&cfg.seed.to_le_bytes());
    buf.extend_from_slice(&cfg.val_fraction.to_bits().to_le_bytes());
    buf.extend_from_slice(&cfg.max_iter.to_le_bytes());
    buf.extend_from_slice(&(cfg.patience as u64).to_le_bytes());
    buf.extend_from_slice(&cfg.eval_every.to_le_bytes());
    buf.extend_from_slice(&meta.layout_hash.to_le_bytes());
    for sha in &meta.lane_sha256 {
        buf.extend_from_slice(&(sha.len() as u64).to_le_bytes());
        buf.extend_from_slice(sha.as_bytes());
    }
    let lo = crate::corpus::store::crc32(&buf) as u64;
    buf.push(0xA5);
    let hi = crate::corpus::store::crc32(&buf) as u64;
    (hi << 32) | lo
}

/// Write the progress file and (if enabled) the resume checkpoint after an
/// inner tune. The progress file is best-effort observability; the checkpoint
/// write is correctness-critical for resume, so its errors propagate.
#[allow(clippy::too_many_arguments)]
fn record_progress(
    progress_path: Option<&Path>,
    checkpoint_path: Option<&Path>,
    config_hash: u64,
    objs: &[(f64, [f64; 4])],
    best: &BestVertex,
    seeds_done: usize,
    reflections_done: usize,
    tunes_done: usize,
) -> Result<(), TexelError> {
    write_progress_file(
        progress_path,
        tunes_done,
        reflections_done,
        best.mix,
        best.result.k,
        best.val,
    );
    if let Some(path) = checkpoint_path {
        let c = MixCheckpoint {
            config_hash,
            objs: objs.to_vec(),
            best_mix: best.mix,
            best_core: best.result.params.core_to_vec(),
            best_k: best.result.k,
            best_val: best.val,
            best_iters: best.result.iters,
            seeds_done,
            reflections_done,
            tunes_done,
        };
        save_mix_checkpoint(path, &c)?;
    }
    Ok(())
}

/// Meta-search checkpoint binary-format magic (`"MIX1"`); rejects foreign bytes.
const MIX_CKPT_MAGIC: u32 = 0x4D49_5831;

/// Atomically write the meta-search checkpoint (temp → fsync → rename). A torn
/// `.tmp` is never renamed into place, so `load_mix_checkpoint` only ever sees
/// a complete file.
fn save_mix_checkpoint(path: &Path, c: &MixCheckpoint) -> Result<(), TexelError> {
    let bytes = encode_mix_checkpoint(c);
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp_path = PathBuf::from(&tmp);
    {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Load a meta-search checkpoint, treating a missing or malformed file (bad
/// magic, short read, CRC mismatch) as "no checkpoint" (cold start) — resume is
/// an optimization, never a hard dependency.
fn load_mix_checkpoint(path: &Path) -> Option<MixCheckpoint> {
    let bytes = std::fs::read(path).ok()?;
    decode_mix_checkpoint(&bytes)
}

/// Length-framed binary encoding with every `f64` written by `to_bits`, ending
/// in a CRC (mirrors `optimizer::encode_checkpoint`'s bit-exact discipline).
fn encode_mix_checkpoint(c: &MixCheckpoint) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&MIX_CKPT_MAGIC.to_le_bytes());
    body.extend_from_slice(&c.config_hash.to_le_bytes());
    body.extend_from_slice(&(c.objs.len() as u32).to_le_bytes());
    for (obj, mix) in &c.objs {
        body.extend_from_slice(&obj.to_bits().to_le_bytes());
        for x in mix {
            body.extend_from_slice(&x.to_bits().to_le_bytes());
        }
    }
    for x in &c.best_mix {
        body.extend_from_slice(&x.to_bits().to_le_bytes());
    }
    body.extend_from_slice(&(c.best_core.len() as u32).to_le_bytes());
    for x in &c.best_core {
        body.extend_from_slice(&x.to_bits().to_le_bytes());
    }
    body.extend_from_slice(&c.best_k.to_bits().to_le_bytes());
    body.extend_from_slice(&c.best_val.to_bits().to_le_bytes());
    body.extend_from_slice(&c.best_iters.to_le_bytes());
    body.extend_from_slice(&(c.seeds_done as u64).to_le_bytes());
    body.extend_from_slice(&(c.reflections_done as u64).to_le_bytes());
    body.extend_from_slice(&(c.tunes_done as u64).to_le_bytes());
    let crc = crate::corpus::store::crc32(&body);
    body.extend_from_slice(&crc.to_le_bytes());
    body
}

/// Decode + validate (magic + CRC) — the inverse of [`encode_mix_checkpoint`].
/// Any inconsistency yields `None` (treated as no checkpoint).
fn decode_mix_checkpoint(bytes: &[u8]) -> Option<MixCheckpoint> {
    // magic(4) + config_hash(8) + n_objs(4) + crc(4)
    if bytes.len() < 4 + 8 + 4 + 4 {
        return None;
    }
    let crc_at = bytes.len() - 4;
    let crc_stored = u32::from_le_bytes(bytes[crc_at..].try_into().ok()?);
    if crate::corpus::store::crc32(&bytes[..crc_at]) != crc_stored {
        return None;
    }
    let mut r = &bytes[..crc_at];
    if read_u32(&mut r)? != MIX_CKPT_MAGIC {
        return None;
    }
    let config_hash = read_u64(&mut r)?;
    let n_objs = read_u32(&mut r)? as usize;
    let mut objs = Vec::with_capacity(n_objs);
    for _ in 0..n_objs {
        let obj = read_f64(&mut r)?;
        let mix = read_mix(&mut r)?;
        objs.push((obj, mix));
    }
    let best_mix = read_mix(&mut r)?;
    let n_core = read_u32(&mut r)? as usize;
    let mut best_core = Vec::with_capacity(n_core);
    for _ in 0..n_core {
        best_core.push(read_f64(&mut r)?);
    }
    let best_k = read_f64(&mut r)?;
    let best_val = read_f64(&mut r)?;
    let best_iters = read_u64(&mut r)?;
    let seeds_done = read_u64(&mut r)? as usize;
    let reflections_done = read_u64(&mut r)? as usize;
    let tunes_done = read_u64(&mut r)? as usize;
    if !r.is_empty() {
        return None;
    }
    Some(MixCheckpoint {
        config_hash,
        objs,
        best_mix,
        best_core,
        best_k,
        best_val,
        best_iters,
        seeds_done,
        reflections_done,
        tunes_done,
    })
}

fn read_u32(r: &mut &[u8]) -> Option<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).ok()?;
    Some(u32::from_le_bytes(b))
}

fn read_u64(r: &mut &[u8]) -> Option<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b).ok()?;
    Some(u64::from_le_bytes(b))
}

fn read_f64(r: &mut &[u8]) -> Option<f64> {
    Some(f64::from_bits(read_u64(r)?))
}

fn read_mix(r: &mut &[u8]) -> Option<[f64; 4]> {
    Some([read_f64(r)?, read_f64(r)?, read_f64(r)?, read_f64(r)?])
}

/// Format the meta-tuning status for the progress file.
///
/// Produces a human-readable, grep-friendly plain-text summary so the operator
/// can `tail -f` it or `cat` it mid-run without any tooling.
pub fn format_meta_progress(
    tunes_done: usize,
    simplex_iter: usize,
    best_mix: [f64; 4],
    best_k: f64,
    best_val: f64,
) -> String {
    format!(
        "tunes_done={tunes_done}\n\
         simplex_iter={simplex_iter}\n\
         best_mix=[{:.4},{:.4},{:.4},{:.4}]\n\
         best_k={best_k:.6}\n\
         best_val_loss={best_val:.6}\n",
        best_mix[0], best_mix[1], best_mix[2], best_mix[3],
    )
}

/// Atomically overwrite the progress file at `path` (temp → rename).
/// Silently ignores errors — this is observability, not correctness.
fn write_progress_file(
    path: Option<&Path>,
    tunes_done: usize,
    simplex_iter: usize,
    best_mix: [f64; 4],
    best_k: f64,
    best_val: f64,
) {
    let Some(path) = path else { return };
    let content = format_meta_progress(tunes_done, simplex_iter, best_mix, best_k, best_val);
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp_path = std::path::Path::new(&tmp);
    let write_ok = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(tmp_path)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
        Ok(())
    })();
    if write_ok.is_ok() {
        let _ = std::fs::rename(tmp_path, path);
    }
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
            progress_label: None,
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
        let (mix, result) = simplex_search(&cache, &meta, &cfg(7), None, None).expect("simplex");
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
        let (chosen_mix, chosen) =
            simplex_search(&cache, &meta, &cfg(13), None, None).expect("simplex");

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

    /// `format_meta_progress` produces a stable, human-readable plain-text block
    /// suitable for writing to a status file and tailing mid-run.
    #[test]
    fn format_meta_progress_canonical_string() {
        let s = super::format_meta_progress(7, 3, [0.4, 0.3, 0.2, 0.1], 0.005700, 0.234567);
        assert_eq!(
            s,
            "tunes_done=7\n\
             simplex_iter=3\n\
             best_mix=[0.4000,0.3000,0.2000,0.1000]\n\
             best_k=0.005700\n\
             best_val_loss=0.234567\n"
        );
    }

    /// The meta-search checkpoint round-trips through the on-disk binary codec
    /// bit-exactly — every `f64` (the simplex objectives, the best vertex's
    /// objective/K, and its tuned core weights) survives `save → load` with an
    /// identical bit pattern, which is what makes a resumed search select the
    /// identical worst/best vertex as an uninterrupted one.
    #[test]
    fn mix_checkpoint_roundtrips_bit_exact() {
        let dir = unique_temp_dir("mixckpt-roundtrip");
        let c = MixCheckpoint {
            config_hash: 0xDEAD_BEEF_1234_5678,
            objs: vec![
                (0.234_567_891_234, [0.25, 0.25, 0.25, 0.25]),
                (0.198_765_432_109, [0.55, 0.15, 0.15, 0.15]),
                (0.301_111_222_333, [0.15, 0.55, 0.15, 0.15]),
            ],
            best_mix: [0.55, 0.15, 0.15, 0.15],
            best_core: vec![-50.25, 0.0, 7.0 / 3.0, 1e-9, -3.062_500_125],
            best_k: 0.005_712_345_6,
            best_val: 0.198_765_432_109,
            best_iters: 4321,
            seeds_done: 5,
            reflections_done: 2,
            tunes_done: 7,
        };
        let path = dir.join("search.mixckpt");
        super::save_mix_checkpoint(&path, &c).expect("save");
        let back = super::load_mix_checkpoint(&path).expect("load");
        assert_eq!(back, c, "checkpoint must round-trip structurally");
        // f64 fields must be bit-identical (not merely ≈) — pin via bits.
        for (a, b) in back.objs.iter().zip(c.objs.iter()) {
            assert_eq!(
                a.0.to_bits(),
                b.0.to_bits(),
                "objective must be bit-identical"
            );
            for (x, y) in a.1.iter().zip(b.1.iter()) {
                assert_eq!(
                    x.to_bits(),
                    y.to_bits(),
                    "mix proportion must be bit-identical"
                );
            }
        }
        assert_eq!(back.best_k.to_bits(), c.best_k.to_bits(), "K bit-identical");
        assert_eq!(
            back.best_val.to_bits(),
            c.best_val.to_bits(),
            "best_val bit-identical"
        );
        for (a, b) in back.best_core.iter().zip(c.best_core.iter()) {
            assert_eq!(a.to_bits(), b.to_bits(), "core weight bit-identical");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A simplex search interrupted at an inner-tune boundary and then resumed
    /// from its checkpoint produces a **bit-identical** final result to an
    /// uninterrupted run: same chosen mix, same tuned weights, same K, same
    /// held-out objective, same inner-tune iteration count. This is the
    /// resume-equals-uninterrupted contract for the meta-search (the inner Adam
    /// solve's own resume contract is covered by
    /// `optimizer::resume_equals_uninterrupted`).
    #[test]
    fn simplex_resume_equals_uninterrupted() {
        let dir = unique_temp_dir("simplex-resume");
        let (cache, meta) = build(&dir);
        let cfg = cfg(31);

        // Uninterrupted full run (no checkpoint).
        let (mix_full, res_full) =
            super::simplex_search_impl(&cache, &meta, &cfg, None, None, None)
                .expect("full run")
                .expect("full run completes");

        // Interrupted run: stop after 7 inner tunes (5 seeds + 2 reflections),
        // writing the checkpoint.
        let ckpt = dir.join("search.mixckpt");
        let stopped = super::simplex_search_impl(&cache, &meta, &cfg, None, Some(&ckpt), Some(7))
            .expect("stopped run");
        assert!(
            stopped.is_none(),
            "stop_after must return None (interrupted)"
        );
        assert!(ckpt.exists(), "interrupted run must leave a checkpoint");

        // Resume from the checkpoint and complete.
        let (mix_resume, res_resume) =
            super::simplex_search_impl(&cache, &meta, &cfg, None, Some(&ckpt), None)
                .expect("resume run")
                .expect("resume run completes");

        // Identical chosen mix (bit-exact), tuned weights, K, objective, iters.
        for (a, b) in mix_resume.iter().zip(mix_full.iter()) {
            assert_eq!(
                a.to_bits(),
                b.to_bits(),
                "resumed mix must match uninterrupted"
            );
        }
        assert_eq!(
            res_resume.val_loss.to_bits(),
            res_full.val_loss.to_bits(),
            "resumed val_loss must match uninterrupted"
        );
        assert_eq!(
            res_resume.k.to_bits(),
            res_full.k.to_bits(),
            "resumed K must match uninterrupted"
        );
        assert_eq!(
            res_resume.iters, res_full.iters,
            "resumed inner-tune iters must match uninterrupted"
        );
        let rc = res_resume.params.core_to_vec();
        let fc = res_full.params.core_to_vec();
        for (i, (a, b)) in rc.iter().zip(fc.iter()).enumerate() {
            assert_eq!(
                a.to_bits(),
                b.to_bits(),
                "resumed core weight[{i}] must match uninterrupted"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A checkpoint written under one config must NOT be resumed under a
    /// different config (different seed here): its objectives were computed on a
    /// different split and are incomparable. The mismatch is treated as a cold
    /// start, so the resumed run equals a *fresh* full run under the new config —
    /// not a corrupted blend of the two. Guards the unattended-re-run footgun of
    /// reusing `--out-params` across configs.
    #[test]
    fn simplex_rejects_checkpoint_from_different_config() {
        let dir = unique_temp_dir("simplex-cfg-mismatch");
        let (cache, meta) = build(&dir);

        // Write a checkpoint under seed 31 (stop partway so it persists).
        let ckpt = dir.join("search.mixckpt");
        let _ = super::simplex_search_impl(&cache, &meta, &cfg(31), None, Some(&ckpt), Some(7))
            .expect("seed-31 partial run");
        assert!(ckpt.exists(), "partial run must leave a checkpoint");

        // "Resume" under a DIFFERENT seed (99): the stale checkpoint must be
        // discarded, so the result equals a fresh seed-99 run from scratch.
        let (mix_resume, res_resume) =
            super::simplex_search_impl(&cache, &meta, &cfg(99), None, Some(&ckpt), None)
                .expect("seed-99 resume")
                .expect("completes");
        let (mix_fresh, res_fresh) =
            super::simplex_search_impl(&cache, &meta, &cfg(99), None, None, None)
                .expect("seed-99 fresh")
                .expect("completes");
        for (a, b) in mix_resume.iter().zip(mix_fresh.iter()) {
            assert_eq!(
                a.to_bits(),
                b.to_bits(),
                "stale-config checkpoint must be ignored (cold start under new config)"
            );
        }
        assert_eq!(
            res_resume.val_loss.to_bits(),
            res_fresh.val_loss.to_bits(),
            "stale-config checkpoint must not influence the new-config result"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `accept_reflection` is the Nelder–Mead trajectory driver: it replaces the
    /// worst vertex iff the reflection strictly beats it, and reports acceptance.
    /// A vertex that does not beat the worst is rejected and leaves the simplex
    /// untouched.
    #[test]
    fn accept_reflection_replaces_only_on_strict_improvement() {
        let mut objs = vec![
            (0.30, [0.25, 0.25, 0.25, 0.25]),
            (0.50, [0.55, 0.15, 0.15, 0.15]), // worst (index 1)
            (0.40, [0.15, 0.55, 0.15, 0.15]),
        ];

        // Worse than the worst (0.60 > 0.50): rejected, simplex unchanged.
        let before = objs.clone();
        let accepted = super::accept_reflection(&mut objs, 1, 0.60, [0.1, 0.2, 0.3, 0.4]);
        assert!(
            !accepted,
            "a reflection worse than the worst must be rejected"
        );
        assert_eq!(
            objs, before,
            "a rejected reflection must not mutate the simplex"
        );

        // Tie (0.50 == 0.50): NOT a strict improvement → rejected.
        let accepted = super::accept_reflection(&mut objs, 1, 0.50, [0.1, 0.2, 0.3, 0.4]);
        assert!(
            !accepted,
            "a reflection tying the worst must be rejected (strict <)"
        );
        assert_eq!(objs, before, "a tie must not mutate the simplex");

        // Better than the worst (0.45 < 0.50): accepted, replaces index 1.
        let accepted = super::accept_reflection(&mut objs, 1, 0.45, [0.7, 0.1, 0.1, 0.1]);
        assert!(accepted, "a reflection beating the worst must be accepted");
        assert_eq!(
            objs[1],
            (0.45, [0.7, 0.1, 0.1, 0.1]),
            "an accepted reflection must replace the worst slot"
        );
    }
}
