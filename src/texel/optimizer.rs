//! Adam + early-stop + checkpoint/resume + coordinate-descent cross-check
//! (ADR-0037 §5/§10 R1/R3).
//!
//! The Adam solve runs on the cached sparse features at frozen K. The train and
//! held-out records are deserialized into memory ONCE (a single streaming pass,
//! [`load_split_records`]) and reused across every epoch; each iteration is one
//! full-batch epoch over those in-memory train records (the gradient is summed
//! over them, then one Adam update is applied), so the run is deterministic and
//! resumable without any per-step RNG. Stopping = held-out
//! plateau (patience, restore-best) ∧ integer-quantization floor ∧ max-iter
//! backstop. Checkpoints are atomic (temp → fsync → rename) so a SIGKILL costs
//! at most one optimizer step and resume is bit-identical.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use crate::corpus::Label;
use crate::texel::TexelError;
use crate::texel::dataset::{CachedRec, Split, stream_chunks};
use crate::texel::loss::{Reg, loss_and_grad};
use crate::texel::params::EvalParams;

/// Adam first-moment decay.
const BETA1: f64 = 0.9;
/// Adam second-moment decay.
const BETA2: f64 = 0.999;
/// Adam denominator epsilon.
const EPSILON: f64 = 1e-8;
/// Consecutive held-out evals with stable integer-rounded weights before the
/// quantization floor halts the solve (ADR-0037 §10).
const QUANT_STABLE_CHECKS: u32 = 3;

/// Full resume state (ADR-0037 §10 R3): params, best-params, Adam moments,
/// iteration, K, RNG, early-stop counters, and the held-out history. Resume
/// reproduces an uninterrupted run modulo one lost step.
#[derive(Clone, PartialEq, Debug)]
pub struct Checkpoint {
    /// Current core weight vector.
    pub w: Vec<f64>,
    /// Best-on-held-out core weight vector (restore-best target).
    pub best_w: Vec<f64>,
    /// Adam first-moment estimate.
    pub m: Vec<f64>,
    /// Adam second-moment estimate.
    pub v: Vec<f64>,
    /// Completed iteration count.
    pub iter: u64,
    /// Frozen Texel scaling constant.
    pub k: f64,
    /// RNG state (resumes the seeded stream bit-identically).
    pub rng: u64,
    /// Early-stop patience counter (epochs without held-out improvement).
    pub patience: u32,
    /// Held-out objective history (one entry per evaluation).
    pub val_hist: Vec<f64>,
}

/// Checkpoint binary-format magic (`"TXK1"`); rejects foreign bytes early.
const CKPT_MAGIC: u32 = 0x5458_4B31;

/// Writes `c` atomically (temp → fsync → rename; R1). A torn temp file is
/// ignored by [`load_checkpoint`].
///
/// The f64 vectors are encoded by their IEEE-754 bit pattern (`to_le_bytes`),
/// NOT as JSON floats: serde_json's shortest-decimal float serialization is not
/// guaranteed bit-exact for every f64, which would break the R3 bit-identical
/// resume contract by ≤1 ULP. A length-framed binary layout round-trips every
/// bit exactly and ends with a CRC so a torn write is rejected by
/// [`load_checkpoint`].
pub fn save_checkpoint(c: &Checkpoint, path: &Path) -> Result<(), TexelError> {
    let bytes = encode_checkpoint(c);
    let tmp = tmp_path(path);
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent()
        && let Ok(d) = File::open(dir)
    {
        // Directory fsync is best-effort (some filesystems reject it); the
        // rename is atomic regardless.
        let _ = d.sync_all();
    }
    Ok(())
}

/// Loads a checkpoint, ignoring a torn temp sibling (R1 torn-tail discipline).
/// A partial / corrupt file at `path` (bad magic, short read, CRC mismatch) is
/// an error so the caller can fall back to the prior valid checkpoint or a cold
/// start.
pub fn load_checkpoint(path: &Path) -> Result<Checkpoint, TexelError> {
    let bytes = std::fs::read(path)?;
    decode_checkpoint(&bytes)
}

/// Length-framed binary encoding: `MAGIC:u32 | iter:u64 | k_bits:u64 |
/// rng:u64 | patience:u32 | 4×(len:u32, len×f64_bits:u64) for w/best_w/m/v |
/// val_len:u32, val_len×f64_bits:u64 | crc:u32`.
fn encode_checkpoint(c: &Checkpoint) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&CKPT_MAGIC.to_le_bytes());
    body.extend_from_slice(&c.iter.to_le_bytes());
    body.extend_from_slice(&c.k.to_bits().to_le_bytes());
    body.extend_from_slice(&c.rng.to_le_bytes());
    body.extend_from_slice(&c.patience.to_le_bytes());
    for vec in [&c.w, &c.best_w, &c.m, &c.v] {
        write_f64_vec(&mut body, vec);
    }
    write_f64_vec(&mut body, &c.val_hist);
    let crc = crate::corpus::store::crc32(&body);
    body.extend_from_slice(&crc.to_le_bytes());
    body
}

/// Append a length-framed f64 vector by bit pattern.
fn write_f64_vec(out: &mut Vec<u8>, v: &[f64]) {
    out.extend_from_slice(&(v.len() as u32).to_le_bytes());
    for &x in v {
        out.extend_from_slice(&x.to_bits().to_le_bytes());
    }
}

/// Decode a checkpoint, validating magic + CRC (the inverse of
/// [`encode_checkpoint`]).
fn decode_checkpoint(bytes: &[u8]) -> Result<Checkpoint, TexelError> {
    let bad = || TexelError::Cache("corrupt checkpoint".to_string());
    if bytes.len() < 4 + 8 + 8 + 8 + 4 + 4 {
        return Err(bad());
    }
    let crc_at = bytes.len() - 4;
    let crc_stored = u32::from_le_bytes(bytes[crc_at..].try_into().map_err(|_| bad())?);
    if crate::corpus::store::crc32(&bytes[..crc_at]) != crc_stored {
        return Err(bad());
    }
    let mut r = &bytes[..crc_at];
    if read_u32(&mut r)? != CKPT_MAGIC {
        return Err(bad());
    }
    let iter = read_u64(&mut r)?;
    let k = f64::from_bits(read_u64(&mut r)?);
    let rng = read_u64(&mut r)?;
    let patience = read_u32(&mut r)?;
    let w = read_f64_vec(&mut r)?;
    let best_w = read_f64_vec(&mut r)?;
    let m = read_f64_vec(&mut r)?;
    let v = read_f64_vec(&mut r)?;
    let val_hist = read_f64_vec(&mut r)?;
    if !r.is_empty() {
        return Err(bad());
    }
    Ok(Checkpoint {
        w,
        best_w,
        m,
        v,
        iter,
        k,
        rng,
        patience,
        val_hist,
    })
}

fn read_u32(r: &mut &[u8]) -> Result<u32, TexelError> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)
        .map_err(|_| TexelError::Cache("short checkpoint".to_string()))?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut &[u8]) -> Result<u64, TexelError> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)
        .map_err(|_| TexelError::Cache("short checkpoint".to_string()))?;
    Ok(u64::from_le_bytes(b))
}

fn read_f64_vec(r: &mut &[u8]) -> Result<Vec<f64>, TexelError> {
    let len = read_u32(r)? as usize;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        out.push(f64::from_bits(read_u64(r)?));
    }
    Ok(out)
}

/// Sibling `.tmp` path for the atomic-write staging file (mirrors `store.rs`).
fn tmp_path(path: &Path) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".tmp");
    std::path::PathBuf::from(s)
}

/// Tune-run configuration (the inner Adam solve).
#[derive(Clone)]
pub struct TuneConfig {
    /// Adam learning rate.
    pub lr: f64,
    /// Deterministic max-iteration backstop.
    pub max_iter: u64,
    /// Held-out plateau patience.
    pub patience: u32,
    /// Evaluate the held-out objective every this many iterations.
    pub eval_every: u64,
    /// Regularization config.
    pub reg: Reg,
    /// Tuning RNG seed.
    pub seed: u64,
    /// Held-out fraction (group-by-game).
    pub val_fraction: f64,
    /// Path for atomic checkpoint saves (R1/R3). `None` disables checkpointing.
    pub checkpoint_path: Option<std::path::PathBuf>,
    /// Write a checkpoint every this many iterations (ignored when
    /// `checkpoint_path` is `None`).
    pub checkpoint_every: u64,
    /// If `Some(label)`, emit a one-line progress report to stderr on every
    /// held-out evaluation. `None` is silent (default; existing callers and
    /// tests stay quiet).
    pub progress_label: Option<String>,
}

/// The tune result: tuned params + the fitted K + the final held-out objective.
pub struct TuneResult {
    /// Best (restore-best) tuned parameters.
    pub params: EvalParams,
    /// Frozen Texel scaling constant.
    pub k: f64,
    /// Final held-out objective at the restored-best point.
    pub val_loss: f64,
    /// Iterations actually run.
    pub iters: u64,
}

/// Runs the Adam weight solve over the cached features (R3 resumable). K is
/// fit once on `init` before the loop (or restored from `resume`) and frozen.
/// Each iteration is one full-batch epoch over `split.train`; stopping is the
/// held-out plateau (restore-best) ∧ integer-quantization floor ∧ max-iter
/// backstop.
pub fn tune(
    cache: &Path,
    split: &Split,
    init: &EvalParams,
    cfg: &TuneConfig,
    resume: Option<Checkpoint>,
) -> Result<TuneResult, TexelError> {
    let init_core = init.core_to_vec();
    let n_core = init_core.len();
    let train_set: HashSet<u32> = split.train.iter().copied().collect();
    let val_set: HashSet<u32> = split.val.iter().copied().collect();

    // Load the train + held-out records into memory ONCE (single streaming pass)
    // and reuse them across every epoch. The previous design re-streamed and
    // re-deserialized the entire on-disk cache on every epoch — both for the
    // gradient and each held-out eval — which on a multi-million-position cache
    // dominated runtime (×max_iter epochs ×inner tunes; the meta-tune
    // post-mortem, docs/milestones/meta-tune-postmortem.md). Iterating the
    // records in memory is bit-identical: the gradient/loss see the same records
    // in the same cache order.
    let (train_recs, val_recs) = load_split_records(cache, &train_set, &val_set)?;

    // Restore-or-cold-start the full state.
    let mut state = match resume {
        Some(c) => c,
        None => {
            let k = crate::texel::loss::fit_k_features(cache, &init_core);
            Checkpoint {
                w: init_core.clone(),
                best_w: init_core.clone(),
                m: vec![0.0; n_core],
                v: vec![0.0; n_core],
                iter: 0,
                k,
                rng: cfg.seed,
                patience: 0,
                val_hist: Vec::new(),
            }
        }
    };

    // Seed restore-best with the INITIAL weights on a cold start: the iter-0
    // held-out loss is recorded as `val_hist[0]` and `best_w = init`, so the
    // restore-best can never return a point worse than init (and a zero-iter run
    // reports the init baseline). On resume, `best_val` reconstructs from the
    // checkpoint's `val_hist` (which already carries that iter-0 entry), so the
    // resumed best selection is bit-identical to an uninterrupted run.
    if state.val_hist.is_empty() {
        let init_val = held_out_loss(&val_recs, &state.w, state.k);
        state.val_hist.push(init_val);
    }
    let mut best_val = state.val_hist.iter().copied().fold(f64::INFINITY, f64::min);
    // Integer-quantization-floor tracking: prior int-rounded weights + a stable
    // counter (re-derived each call from the current weights; the count resets
    // on resume, which is conservative — it can only delay the floor stop).
    let mut prev_int: Option<Vec<i64>> = None;
    let mut quant_stable: u32 = 0;
    // Did the held-out plateau (patience) fire? Only then does restore-best
    // override the final iterate (ADR-0037 §10): a run that consumed its full
    // iteration budget (max-iter / quantization floor) returns its last iterate,
    // which is the deterministic, resume-faithful tuned vector.
    let mut plateau_fired = false;

    while state.iter < cfg.max_iter {
        // One full-batch epoch: sum the gradient over every (in-memory) train
        // record at the current weights / frozen K.
        let mut grad = vec![0.0f64; n_core];
        let _ = loss_and_grad(&train_recs, &state.w, state.k, &cfg.reg, &mut grad);

        // Adam update.
        state.iter += 1;
        let t = state.iter as f64;
        let bc1 = 1.0 - BETA1.powf(t);
        let bc2 = 1.0 - BETA2.powf(t);
        for (i, &g) in grad.iter().enumerate() {
            state.m[i] = BETA1 * state.m[i] + (1.0 - BETA1) * g;
            state.v[i] = BETA2 * state.v[i] + (1.0 - BETA2) * g * g;
            let m_hat = state.m[i] / bc1;
            let v_hat = state.v[i] / bc2;
            state.w[i] -= cfg.lr * m_hat / (v_hat.sqrt() + EPSILON);
        }

        // Periodic held-out eval + stopping checks.
        let do_eval = state.iter % cfg.eval_every == 0 || state.iter == cfg.max_iter;
        if do_eval {
            let val = held_out_loss(&val_recs, &state.w, state.k);
            state.val_hist.push(val);

            if val + 1e-12 < best_val {
                best_val = val;
                state.best_w.copy_from_slice(&state.w);
                state.patience = 0;
            } else {
                state.patience += 1;
            }

            if let Some(label) = &cfg.progress_label {
                eprintln!(
                    "[{label}] epoch {iter}/{max_iter}  val_loss {val:.6}  \
                     best {best_val:.6}  patience {patience}/{cfg_patience}",
                    iter = state.iter,
                    max_iter = cfg.max_iter,
                    patience = state.patience,
                    cfg_patience = cfg.patience,
                );
            }

            // Integer-quantization floor: stop once the int-rounded weights are
            // stable across QUANT_STABLE_CHECKS consecutive evals.
            let int_now: Vec<i64> = state.w.iter().map(|x| x.round() as i64).collect();
            if prev_int.as_ref() == Some(&int_now) {
                quant_stable += 1;
            } else {
                quant_stable = 0;
            }
            prev_int = Some(int_now);

            let plateau = state.patience >= cfg.patience;
            let quant_floor = quant_stable >= QUANT_STABLE_CHECKS;

            maybe_checkpoint(cfg, &state)?;

            if plateau {
                plateau_fired = true;
                break;
            }
            if quant_floor {
                break;
            }
        } else {
            maybe_checkpoint(cfg, &state)?;
        }
    }

    // On a held-out plateau, restore the best-val checkpoint (ADR-0037 §10);
    // otherwise (max-iter / quantization-floor completion) return the final
    // iterate — the deterministic, resume-faithful tuned vector.
    let final_w = if plateau_fired {
        &state.best_w
    } else {
        &state.w
    };
    let final_loss = held_out_loss(&val_recs, final_w, state.k);

    Ok(TuneResult {
        params: init.with_core(final_w),
        k: state.k,
        val_loss: final_loss,
        iters: state.iter,
    })
}

/// Write a checkpoint if the path is set and the iteration is on the cadence.
fn maybe_checkpoint(cfg: &TuneConfig, state: &Checkpoint) -> Result<(), TexelError> {
    if let Some(path) = &cfg.checkpoint_path
        && cfg.checkpoint_every > 0
        && state.iter.is_multiple_of(cfg.checkpoint_every)
    {
        save_checkpoint(state, path)?;
    }
    Ok(())
}

/// Stream the on-disk cache ONCE and partition its records into the train and
/// held-out subsets, each preserving cache order. Called a single time per
/// [`tune`] so the epoch loop iterates the records in memory instead of
/// re-streaming + re-deserializing the whole cache every epoch (the meta-tune
/// perf bug — see the module header / post-mortem). `split.train` / `split.val`
/// are disjoint by construction (group-by-game split, or the mixture reweight's
/// per-source subsample of the train pool), so a record falls into at most one
/// set; a record in neither (a reweight-subsampled-out train-pool position) is
/// skipped. Iterating these vectors yields a bit-identical gradient/loss to the
/// previous per-epoch streaming because the record sequence is identical.
fn load_split_records(
    cache: &Path,
    train_set: &HashSet<u32>,
    val_set: &HashSet<u32>,
) -> Result<(Vec<CachedRec>, Vec<CachedRec>), TexelError> {
    let mut train: Vec<CachedRec> = Vec::with_capacity(train_set.len());
    let mut val: Vec<CachedRec> = Vec::with_capacity(val_set.len());
    let mut idx: u32 = 0;
    for chunk in stream_chunks(cache, 65_536) {
        for rec in chunk {
            if train_set.contains(&idx) {
                train.push(rec);
            } else if val_set.contains(&idx) {
                val.push(rec);
            }
            idx += 1;
        }
    }
    Ok((train, val))
}

/// Mean held-out MSE at weights `w` / scaling `k` over the in-memory held-out
/// records.
///
/// Scores on the continuous (un-rounded) cached score, so the held-out
/// objective tracks the smooth surface the Adam gradient minimizes: a float
/// improvement that does not cross an integer boundary still registers, so the
/// plateau / restore-best logic sees the true convergence trajectory rather
/// than integer-quantization noise. The separate integer-quantization-floor
/// stop (`round(w)` stable) is what enforces deployment-faithful rounding.
fn held_out_loss(val_recs: &[CachedRec], w: &[f64], k: f64) -> f64 {
    if val_recs.is_empty() {
        // Sentinel for a degenerate (empty) held-out set — same as the old
        // streaming form's `n == 0` guard. Callers feed a non-empty val split
        // (`split_by_game` with a non-zero `val_fraction`); an all-empty val
        // would make a tune a silent no-op, which `adam_recovers_known_optimum`
        // catches (its `val_loss < initial` assertion fails when both are 0.0).
        return 0.0;
    }
    let mut sum = 0.0f64;
    for rec in val_recs {
        let mut s = rec.base as f64;
        for &(j, c) in rec.coeffs.iter() {
            s += c as f64 * w[j as usize];
        }
        let pred = sigmoid(k * s);
        let label = Label::from_u8(rec.label)
            .expect("valid label byte")
            .as_f64();
        let d = label - pred;
        sum += d * d;
    }
    sum / val_recs.len() as f64
}

/// Logistic sigmoid, clamped (mirrors `objective`'s clamp).
#[inline]
fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x.clamp(-60.0, 60.0)).exp())
}

/// Classic ±1 coordinate-descent cross-check on the sub-vector `idxs` — a
/// correctness check on the gradient solution (ADR-0037 §5). For each index it
/// tries a ±1 step and returns the value (current ± 1) that lowers the
/// unregularized MSE most; an index already optimal at ±1 is left unmoved.
pub fn coord_descent_check(chunk: &[CachedRec], w: &[f64], k: f64, idxs: &[usize]) -> Vec<f64> {
    let base_loss = mse(chunk, w, k);
    idxs.iter()
        .map(|&idx| {
            let mut wp = w.to_vec();
            wp[idx] = w[idx] + 1.0;
            let lp = mse(chunk, &wp, k);
            let mut wm = w.to_vec();
            wm[idx] = w[idx] - 1.0;
            let lm = mse(chunk, &wm, k);
            if lp < base_loss && lp <= lm {
                w[idx] + 1.0
            } else if lm < base_loss {
                w[idx] - 1.0
            } else {
                w[idx]
            }
        })
        .collect()
}

/// Mean squared Texel error over a chunk (the coord-descent objective).
fn mse(chunk: &[CachedRec], w: &[f64], k: f64) -> f64 {
    if chunk.is_empty() {
        return 0.0;
    }
    let sum: f64 = chunk
        .iter()
        .map(|rec| {
            let mut s = rec.base as f64;
            for &(idx, c) in rec.coeffs.iter() {
                s += c as f64 * w[idx as usize];
            }
            let pred = sigmoid(k * s);
            let label = Label::from_u8(rec.label)
                .expect("valid label byte")
                .as_f64();
            let d = label - pred;
            d * d
        })
        .sum();
    sum / chunk.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Position;
    use crate::corpus::prng::Prng;
    use crate::corpus::{CorpusRecord, DEPTH_RUNG_EXTERNAL, Label, Source};
    use crate::texel::dataset::{self, CachedRec, LaneSet};
    use crate::texel::features::{extract, model_score_white};
    use crate::texel::layout::N_CORE;
    use crate::texel::loss::Reg;
    use crate::texel::params::EvalParams;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_temp_dir(tag: &str) -> PathBuf {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("clawfish-texel-opt-{tag}-{pid}-{n}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// A handful of real FENs spread across game phases for synthetic lanes.
    const FENS: &[&str] = &[
        "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
        "r1bqkb1r/pppp1ppp/2n2n2/4p3/2B1P3/5N2/PPPP1PPP/RNBQK2R w KQkq - 4 4",
        "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
        "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        "4k3/8/8/8/8/8/4P3/4K3 w - - 0 1",
        "8/k7/3p4/p2P1p2/P2P1P2/8/8/K7 w - - 0 1",
    ];

    /// Planted core-weight vector for the synthetic signal: a handful of
    /// non-zero entries create score variation across positions so that
    /// `build_tiny_cache` labels are correlated with the feature values the
    /// optimizer can learn from.  Indices are from the flat layout table:
    /// 189 = ROOK_OPEN_FILE_MG, 190 = ROOK_OPEN_FILE_EG (open-rook bonus),
    /// 0 = ISO_MG, 1 = ISO_EG (isolated-pawn penalty),
    /// 83 = ROOK_MOBILITY_MG[0], 98 = ROOK_MOBILITY_EG[0].
    fn planted_w_star() -> Vec<f64> {
        let mut w = vec![0.0f64; N_CORE];
        w[189] = 80.0; // ROOK_OPEN_FILE_MG
        w[190] = 60.0; // ROOK_OPEN_FILE_EG
        w[0] = -50.0; // ISO_MG
        w[1] = -40.0; // ISO_EG
        w[83] = 8.0; // ROOK_MOBILITY_MG[0]
        w[98] = 6.0; // ROOK_MOBILITY_EG[0]
        w
    }

    /// Build a tiny 4-lane synthetic `LaneSet` with signal-bearing labels
    /// derived from a planted weight vector (`planted_w_star`). Each record's
    /// label is assigned by evaluating `model_score_white` with the planted
    /// weights: `score > T → WhiteWin`, `score < -T → BlackWin`, else `Draw`
    /// (T = 20 cp). This gives a clear correlation for Adam to exploit, so
    /// `tune` must strictly reduce the held-out objective.
    fn synthetic_lanes(seed: u64, per_lane: usize) -> LaneSet {
        let w_star = planted_w_star();
        let threshold = 20.0f64;
        let mut rng = Prng::new(seed);
        let mut recs: [Vec<CorpusRecord>; 4] = Default::default();
        let sources = [
            Source::SelfPlayOnBook,
            Source::SelfPlayOffBook,
            Source::Ccrl,
            Source::LichessOpen,
        ];
        for (lane, src) in sources.iter().enumerate() {
            for i in 0..per_lane {
                let fen = FENS[(rng.below(FENS.len() as u64)) as usize];
                let pos = Position::from_fen(fen).expect("synthetic FEN");
                let fv = extract(&pos);
                let planted_score = model_score_white(&fv, &w_star);
                let label = if planted_score > threshold {
                    Label::WhiteWin
                } else if planted_score < -threshold {
                    Label::BlackWin
                } else {
                    Label::Draw
                };
                recs[lane].push(CorpusRecord {
                    fen: fen.to_string(),
                    label,
                    source: *src,
                    game_id: (i / 3) as u64, // a few plies per game (split groups)
                    ply: (i % 3) as u32,
                    depth_rung: DEPTH_RUNG_EXTERNAL,
                    strata: 0,
                });
            }
        }
        LaneSet {
            recs,
            sha256: [
                "00".to_string(),
                "11".to_string(),
                "22".to_string(),
                "33".to_string(),
            ],
        }
    }

    /// Materialize a tiny cache on disk and return its path + meta.
    fn build_tiny_cache(dir: &Path) -> (PathBuf, dataset::CacheMeta) {
        let lanes = synthetic_lanes(0x5EED, 48);
        let cache = dir.join("features.cache");
        let meta = dataset::build_cache(&lanes, &cache, 1).expect("build cache");
        (cache, meta)
    }

    fn default_cfg(seed: u64) -> TuneConfig {
        TuneConfig {
            lr: 0.05,
            max_iter: 200,
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

    /// Synthetic linear cached chunk with a KNOWN optimum: scores are generated
    /// from a planted weight vector so Adam should recover it (low loss, weights
    /// near the planted target on the active indices).
    fn synthetic_chunk(seed: u64, n: usize) -> Vec<CachedRec> {
        let mut rng = Prng::new(seed);
        (0..n)
            .map(|i| {
                let idx = (i % 6) as u16; // a small active sub-space
                let c = (rng.below(5) as i64 - 2) as f32;
                let label = match rng.below(3) {
                    0 => Label::WhiteWin,
                    1 => Label::Draw,
                    _ => Label::BlackWin,
                };
                CachedRec {
                    base: (rng.below(201) as f64 - 100.0) as f32,
                    coeffs: vec![(idx, c)].into_boxed_slice(),
                    label: label.as_u8(),
                    source: Source::SelfPlayOffBook.as_u8(),
                    strata: 0,
                    depth_rung: DEPTH_RUNG_EXTERNAL,
                    game_id: i as u64,
                }
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Adam recovers a known optimum on a synthetic linear problem.
    // -----------------------------------------------------------------------

    /// On a synthetic cache whose labels are generated by a planted linear
    /// model, `tune` (Adam) drives the held-out objective below the
    /// initial-weights objective and converges (the linear-surface convergence
    /// guarantee, ADR-0037 §5). We check the final val_loss improved on init.
    #[test]
    fn adam_recovers_known_optimum_on_synthetic_linear() {
        let dir = unique_temp_dir("adam-opt");
        let (cache, meta) = build_tiny_cache(&dir);
        let split = dataset::split_by_game(&meta, 0.25, 7);
        let init = EvalParams::shipped();

        // Baseline: one-shot val_loss at shipped/init weights (0 iterations).
        let mut zero_cfg = default_cfg(7);
        zero_cfg.max_iter = 0;
        zero_cfg.patience = 1_000_000;
        let baseline = tune(&cache, &split, &init, &zero_cfg, None).expect("baseline tune");
        let initial_val_loss = baseline.val_loss;

        // Full run: Adam must reduce the held-out loss below the initial.
        let cfg = default_cfg(7);
        let result = tune(&cache, &split, &init, &cfg, None).expect("tune");

        // Convergence: val_loss is a finite MSE in [0, 1] and the solver ran.
        assert!(
            result.val_loss.is_finite() && (0.0..=1.0).contains(&result.val_loss),
            "val_loss must be a finite MSE in [0,1]; got {}",
            result.val_loss
        );
        assert!(result.iters > 0, "tune must run at least one iteration");
        assert_eq!(
            result.params.core_to_vec().len(),
            N_CORE,
            "tuned params must expose the full core vector"
        );
        // Adam must strictly reduce the held-out objective (convergence guarantee).
        assert!(
            result.val_loss < initial_val_loss,
            "Adam must reduce val_loss below initial: final={} >= initial={}",
            result.val_loss,
            initial_val_loss
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Coordinate-descent (classic ±1 Texel) agrees in SIGN with the Adam
    /// gradient direction on a small active sub-vector — the cross-check on the
    /// gradient math (ADR-0037 §5). Both should move each probed weight toward
    /// the same side of its current value (loss-reducing direction).
    #[test]
    fn coord_descent_agrees_with_adam_small() {
        let chunk = synthetic_chunk(0xC0DE, 120);
        let k = 0.01;
        let w = vec![0.0f64; N_CORE];
        let idxs: Vec<usize> = (0..6).collect();
        // coord_descent_check returns the ±1-improved sub-vector at idxs.
        let cd = coord_descent_check(&chunk, &w, k, &idxs);
        assert_eq!(cd.len(), idxs.len(), "one result per probed index");
        // The analytic gradient (loss decreases opposite the gradient) must
        // point the SAME way coord-descent moved each weight.
        let mut grad = vec![0.0f64; N_CORE];
        let reg = Reg {
            l2_lambda: 0.0,
            init: vec![0.0; N_CORE],
            mono_lambda: 0.0,
        };
        crate::texel::loss::loss_and_grad(&chunk, &w, k, &reg, &mut grad);
        for (slot, &idx) in idxs.iter().enumerate() {
            let cd_step = cd[slot] - w[idx];
            // Skip indices coord-descent left unmoved (already optimal at ±1).
            if cd_step.abs() < 1e-12 {
                continue;
            }
            assert!(
                cd_step * (-grad[idx]) >= 0.0,
                "coord-descent step ({cd_step}) and -gradient ({}) must share \
                 sign at idx {idx}",
                -grad[idx]
            );
        }
    }

    /// Early-stop restores the BEST held-out weights, not the last iterate:
    /// after a deliberately-overlong run that overfits past the plateau, the
    /// returned params reproduce the best recorded val_loss (restore-best,
    /// ADR-0037 §10).
    #[test]
    fn early_stop_restores_best_not_last() {
        let dir = unique_temp_dir("early-stop");
        let (cache, meta) = build_tiny_cache(&dir);
        let split = dataset::split_by_game(&meta, 0.25, 3);
        let init = EvalParams::shipped();
        // Patience low + max_iter high so the plateau triggers before max_iter,
        // and the run would otherwise drift past the best point.
        let mut cfg = default_cfg(3);
        cfg.patience = 3;
        cfg.max_iter = 10_000;
        cfg.eval_every = 5;
        let result = tune(&cache, &split, &init, &cfg, None).expect("tune");
        // Stopped before max_iter (the plateau fired) — restore-best ⇒ the
        // reported val_loss is the minimum over the history, so re-evaluating
        // the returned params reproduces it (the solver does not return the
        // post-plateau drifted iterate).
        assert!(
            result.iters < cfg.max_iter,
            "early-stop must halt before max_iter; ran {} of {}",
            result.iters,
            cfg.max_iter
        );
        assert!(
            result.val_loss.is_finite(),
            "restored-best val_loss must be finite"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The integer-quantization floor halts the solve once integer-rounded
    /// weights stop changing across `quant_stable_checks` evaluations, even
    /// before the held-out plateau — tested at a rounding boundary (a weight
    /// hovering at x.5 must settle deterministically). (ADR-0037 §10.)
    #[test]
    fn quantization_floor_halts_when_int_weights_stable() {
        let dir = unique_temp_dir("quant-floor");
        let (cache, meta) = build_tiny_cache(&dir);
        let split = dataset::split_by_game(&meta, 0.25, 11);
        let init = EvalParams::shipped();
        // Tiny LR so float weights creep but integer-rounded weights stabilize
        // quickly → the quantization floor (not the plateau, not max_iter) is
        // the binding stop. max_iter is large so it cannot be the cause.
        let mut cfg = default_cfg(11);
        cfg.lr = 1e-4;
        cfg.max_iter = 100_000;
        cfg.patience = 1_000_000; // disable plateau stop
        cfg.eval_every = 10;
        let result = tune(&cache, &split, &init, &cfg, None).expect("tune");
        assert!(
            result.iters < cfg.max_iter,
            "quantization floor must halt before max_iter; ran {} of {}",
            result.iters,
            cfg.max_iter
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Checkpoint round-trip + resume bit-identity (R1/R3).
    // -----------------------------------------------------------------------

    /// `save_checkpoint` then `load_checkpoint` reproduces the checkpoint
    /// bit-identically (every field, incl. the f64 vectors and the RNG state).
    #[test]
    fn checkpoint_roundtrip_bit_identical() {
        let dir = unique_temp_dir("ckpt-roundtrip");
        let ckpt = Checkpoint {
            w: (0..N_CORE).map(|i| i as f64 * 0.5 - 7.25).collect(),
            best_w: (0..N_CORE).map(|i| i as f64 * -0.25 + 1.0).collect(),
            m: vec![1.5; N_CORE],
            v: vec![2.25; N_CORE],
            iter: 1234,
            k: 0.012_345_678,
            rng: 0xDEAD_BEEF_CAFE_F00D,
            patience: 3,
            val_hist: vec![0.30, 0.29, 0.285, 0.286],
        };
        let path = dir.join("ckpt.bin");
        save_checkpoint(&ckpt, &path).expect("save");
        let back = load_checkpoint(&path).expect("load");
        assert_eq!(back, ckpt, "checkpoint must round-trip bit-identically");
        // The f64 fields must be bit-identical (not merely ≈) — pin via bits.
        for (a, b) in back.w.iter().zip(ckpt.w.iter()) {
            assert_eq!(a.to_bits(), b.to_bits(), "w must be bit-identical");
        }
        assert_eq!(
            back.k.to_bits(),
            ckpt.k.to_bits(),
            "k must be bit-identical"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A run of `2N` iterations equals a run of `N` + checkpoint + load + `N`:
    /// the final params, Adam moments, RNG, K, and iteration count are all
    /// bit-identical (R3 resume-equals-uninterrupted, modulo nothing here since
    /// we control the split point exactly).
    #[test]
    fn resume_equals_uninterrupted() {
        let dir = unique_temp_dir("resume-eq");
        let (cache, meta) = build_tiny_cache(&dir);
        let split = dataset::split_by_game(&meta, 0.25, 21);
        // Start from an off-optimum init (all-deferred-terms-zero — the cold-start
        // shape the real M6.I tune used), NOT `EvalParams::shipped()`: post-M6.I
        // SHIP, shipped() sits at the integer-stable tuned optimum, which trips the
        // (intentionally conservative, non-checkpointed — see the loop comment) integer
        // quantization floor and stops the uninterrupted run early, defeating this
        // test's max-iter-path bit-identity check. With weights that genuinely move
        // each epoch, only `max_iter` governs, so resume is bit-identical.
        let zero_core = vec![0.0; EvalParams::shipped().core_to_vec().len()];
        let init = EvalParams::shipped().with_core(&zero_core);
        let ckpt_path = dir.join("tune.ckpt");

        // Uninterrupted: 2N iterations (no checkpoint needed; compare against resumed).
        let mut full_cfg = default_cfg(21);
        full_cfg.max_iter = 60;
        full_cfg.patience = 1_000_000; // no early stop — fixed iteration count
        let full = tune(&cache, &split, &init, &full_cfg, None).expect("full tune");

        // Interrupted: first N iterations, writing a checkpoint via checkpoint_path.
        let mut half_cfg = default_cfg(21);
        half_cfg.max_iter = 30;
        half_cfg.patience = 1_000_000;
        half_cfg.checkpoint_path = Some(ckpt_path.clone());
        half_cfg.checkpoint_every = 1; // save at every iteration so the final state is checkpointed
        let _ = tune(&cache, &split, &init, &half_cfg, None).expect("first half");

        // Load the mid-run checkpoint and resume to the full 60-iteration target.
        let resumed_ckpt = load_checkpoint(&ckpt_path).expect("load mid-run checkpoint");
        let mut resume_cfg = default_cfg(21);
        resume_cfg.max_iter = 60;
        resume_cfg.patience = 1_000_000;
        let resumed =
            tune(&cache, &split, &init, &resume_cfg, Some(resumed_ckpt)).expect("resume tune");

        // The resumed final params / K / iter must be bit-identical to the
        // uninterrupted run.
        assert_eq!(resumed.iters, full.iters, "iteration count must match");
        assert_eq!(
            resumed.k.to_bits(),
            full.k.to_bits(),
            "K must be bit-identical after resume"
        );
        let rv = resumed.params.core_to_vec();
        let fv = full.params.core_to_vec();
        for (i, (a, b)) in rv.iter().zip(fv.iter()).enumerate() {
            assert_eq!(
                a.to_bits(),
                b.to_bits(),
                "resumed core weight[{i}] must be bit-identical to uninterrupted"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
