//! Corpus-mixture meta-tuning via the **full Nelder–Mead** simplex method in
//! **softmax-reparameterized z-space**.
//!
//! The four lane proportions live on the probability 3-simplex (sum-to-1,
//! non-negative). Searching directly in `[f64; 4]` mix space has two structural
//! problems for Nelder–Mead: the constraint surface is bounded (forcing a
//! `clamp + renormalize` projection, which is a discontinuity right where N-M
//! most often probes), and the simplex is rank-deficient by one dimension (the
//! sum-to-1 constraint). Both are fixed by reparameterizing through the natural
//! 3-DOF coordinate system: `z ∈ R³`, with `z₄ ≡ 0` anchoring a softmax map
//! `p_i = exp(z_i) / Σ_j exp(z_j)`. N-M then operates on full-rank R³ with no
//! constraints, no clamping, and dimensionally honest about the simplex.
//!
//! The seed simplex is four asymmetric vertices, each pushing one lane toward
//! a 3:1:1:1 ratio. Uniform mix is *not* a seed vertex — it is the centroid of
//! the seed simplex in mix-space, naturally reachable via contraction if it is
//! optimal. The corresponding z-triples are
//! `[ln 3, 0, 0]`, `[0, ln 3, 0]`, `[0, 0, ln 3]`, `[−ln 3, −ln 3, −ln 3]`.
//!
//! All four N-M moves are implemented (standard coefficients):
//! - **Reflect** (α=1): mirror the worst vertex through the centroid of the
//!   rest.
//! - **Expand** (γ=2): if the reflection is the new best, push further in the
//!   same direction.
//! - **Contract** (β=0.5): outside contraction when the reflection beats the
//!   worst (the simplex overshot but the direction is right); inside
//!   contraction when it does not (the direction itself was wrong).
//! - **Shrink** (σ=0.5): when contraction also fails, pull every non-best
//!   vertex halfway toward the best.
//!
//! **Flattening mitigation** via Kelley's sufficient-decrease restart
//! (*"Detection and Remediation of Stagnation in the Nelder–Mead Algorithm
//! Using a Sufficient Decrease Condition"*, 1999): if over the most recent
//! `KELLEY_WINDOW` iterations the best vertex's objective has not decreased by
//! at least `KELLEY_TAU · simplex_size²`, the simplex is replaced with a fresh
//! axis-aligned simplex around the current best (capped to `MAX_RESTARTS`).
//! Guards against the canonical N-M failure mode (McKinnon 1998) where
//! repeated contractions drive the simplex into a lower-dimensional subspace.
//!
//! **Stopping** on the first of: objective spread `max(f) − min(f) < EPS_F`,
//! z-space parameter spread `max ‖z − centroid‖ < EPS_Z`, or `iter == NM_MAX_ITER`.
//! Returns the **best vertex** — we already paid to evaluate it; the centroid is
//! an unevaluated interpolation that would need an extra inner solve and is no
//! safer than a real measurement.
//!
//! **Checkpoint format `MIX2`** (per-iteration granularity, bit-exact resume).
//! A kill mid-iteration loses at most one outer N-M iteration's worth of inner
//! tunes (1–3, or up to 4 on the rare shrink). Every `f64` (z-coords,
//! objectives, tuned cores, Kelley window) round-trips through its `to_bits`
//! encoding — JSON float serialization would drop ≤1 ULP per number, enough to
//! steer a resumed simplex onto a different vertex.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::corpus::prng::Prng;
use crate::texel::TexelError;
use crate::texel::dataset::{CacheMeta, Split, split_by_game};
use crate::texel::optimizer::{TuneConfig, TuneResult, tune};
use crate::texel::params::EvalParams;

// ---------------------------------------------------------------------------
// Algorithm constants.
// ---------------------------------------------------------------------------

/// Free z-space dimension (`z₄ ≡ 0` is anchored, so 3 free coords for a
/// 4-component mix on the 3-simplex).
const N_Z: usize = 3;
/// Simplex vertex count (n+1 in n=3-D space).
const N_VERTS: usize = 4;
/// Hard cap on outer N-M iterations.
const NM_MAX_ITER: usize = 30;
/// Reflection coefficient (standard α).
const NM_ALPHA: f64 = 1.0;
/// Expansion coefficient (standard γ).
const NM_GAMMA: f64 = 2.0;
/// Contraction coefficient (standard β), used for both outside and inside.
const NM_BETA: f64 = 0.5;
/// Shrink coefficient (standard σ).
const NM_SIGMA: f64 = 0.5;
/// Sliding-window length for Kelley's sufficient-decrease check.
const KELLEY_WINDOW: usize = 5;
/// Sufficient-decrease threshold coefficient: stagnation if
/// `(oldest − newest) < KELLEY_TAU · z_spread²` over the window.
const KELLEY_TAU: f64 = 1e-6;
/// Maximum Kelley restarts before terminating exploration.
const MAX_RESTARTS: usize = 2;
/// Axis-step magnitude (z-space units) for restart simplices.
const RESTART_STEP: f64 = 0.5;
/// Objective-spread stopping tolerance: `max(f) − min(f) < EPS_F` terminates.
const EPS_F: f64 = 1e-6;
/// Z-space spread stopping tolerance: `max ‖v.z − centroid‖ < EPS_Z` terminates.
const EPS_Z: f64 = 0.005;

// ---------------------------------------------------------------------------
// Softmax reparameterization (R³ ↔ probability 3-simplex).
// ---------------------------------------------------------------------------

/// Map `z ∈ R³` to a mix on the probability 3-simplex via softmax with the
/// fourth coordinate anchored at 0:
/// `p_i = exp(z_i) / (exp(z_0) + exp(z_1) + exp(z_2) + exp(0))`.
///
/// Numerically stable by subtracting the per-call max (including the implicit
/// `z₄ = 0`) before each `exp`.
pub fn mix_from_z(z: [f64; N_Z]) -> [f64; 4] {
    // The anchor z_4 = 0 always contributes to the max.
    let mut max_z = 0.0_f64;
    for &zi in &z {
        if zi > max_z {
            max_z = zi;
        }
    }
    let e0 = (z[0] - max_z).exp();
    let e1 = (z[1] - max_z).exp();
    let e2 = (z[2] - max_z).exp();
    let e3 = (0.0 - max_z).exp();
    let sum = e0 + e1 + e2 + e3;
    [e0 / sum, e1 / sum, e2 / sum, e3 / sum]
}

/// Seed simplex: four z-triples mapping to lane-dominant mixes 3:1:1:1,
/// 1:3:1:1, 1:1:3:1, 1:1:1:3 (each normalized to sum 1).
///
/// First three: `z_i = ln 3` on axis `i`, others 0 — softmax gives mix
/// `[3/6, 1/6, 1/6, 1/6]` (in the right rotation).
/// Fourth (lane-3 dominant via anchor): `z = (−ln 3, −ln 3, −ln 3)` — softmax
/// gives `[1/6, 1/6, 1/6, 3/6]`. Affinely independent in R³ (verified
/// algebraically and by [`seed_simplex_is_full_rank`]).
fn seed_simplex_z() -> [[f64; N_Z]; N_VERTS] {
    let l3 = 3.0_f64.ln();
    [
        [l3, 0.0, 0.0],
        [0.0, l3, 0.0],
        [0.0, 0.0, l3],
        [-l3, -l3, -l3],
    ]
}

// ---------------------------------------------------------------------------
// Nelder–Mead vertex moves in z-space.
// ---------------------------------------------------------------------------

/// Reflection: `r = c + α·(c − worst)`.
fn nm_reflect(c: [f64; N_Z], worst: [f64; N_Z]) -> [f64; N_Z] {
    std::array::from_fn(|k| c[k] + NM_ALPHA * (c[k] - worst[k]))
}

/// Expansion: `e = c + γ·(r − c)` — push further along the reflection
/// direction.
fn nm_expand(c: [f64; N_Z], r: [f64; N_Z]) -> [f64; N_Z] {
    std::array::from_fn(|k| c[k] + NM_GAMMA * (r[k] - c[k]))
}

/// Outside contraction: `oc = c + β·(r − c)` — halfway between centroid and the
/// reflected point (used when reflection beat the worst).
fn nm_outside_contract(c: [f64; N_Z], r: [f64; N_Z]) -> [f64; N_Z] {
    std::array::from_fn(|k| c[k] + NM_BETA * (r[k] - c[k]))
}

/// Inside contraction: `ic = c + β·(worst − c)` — halfway between centroid and
/// the worst vertex (used when reflection was worse than the worst, i.e. the
/// reflection direction itself is bad).
fn nm_inside_contract(c: [f64; N_Z], worst: [f64; N_Z]) -> [f64; N_Z] {
    std::array::from_fn(|k| c[k] + NM_BETA * (worst[k] - c[k]))
}

/// Shrink: `s = best + σ·(v − best)` — pull a non-best vertex halfway toward
/// the best.
fn nm_shrink(best: [f64; N_Z], v: [f64; N_Z]) -> [f64; N_Z] {
    std::array::from_fn(|k| best[k] + NM_SIGMA * (v[k] - best[k]))
}

/// Centroid of all vertices except the one at `worst_idx`.
fn z_centroid_excluding(verts: &[Vertex; N_VERTS], worst_idx: usize) -> [f64; N_Z] {
    let denom = (N_VERTS - 1) as f64;
    std::array::from_fn(|k| {
        let mut s = 0.0;
        for (i, v) in verts.iter().enumerate() {
            if i != worst_idx {
                s += v.z[k];
            }
        }
        s / denom
    })
}

/// Euclidean distance in R³.
fn z_distance(a: [f64; N_Z], b: [f64; N_Z]) -> f64 {
    let mut s = 0.0;
    for k in 0..N_Z {
        let d = a[k] - b[k];
        s += d * d;
    }
    s.sqrt()
}

/// Maximum vertex distance from the centroid of all vertices (the parameter-space
/// "diameter" used for the stopping tolerance and Kelley's size² normalization).
fn simplex_z_spread(verts: &[Vertex; N_VERTS]) -> f64 {
    let denom = N_VERTS as f64;
    let full_centroid: [f64; N_Z] = std::array::from_fn(|k| {
        let s: f64 = verts.iter().map(|v| v.z[k]).sum();
        s / denom
    });
    verts
        .iter()
        .map(|v| z_distance(v.z, full_centroid))
        .fold(0.0, f64::max)
}

// ---------------------------------------------------------------------------
// Vertex + checkpoint state.
// ---------------------------------------------------------------------------

/// One evaluated simplex vertex: its z-coordinate, its held-out objective, and
/// the full inner-tune result (so the best vertex can be returned without a
/// re-evaluation).
#[derive(Clone)]
struct Vertex {
    z: [f64; N_Z],
    obj: f64,
    result: TuneResult,
}

/// Persisted meta-search state — enough to resume the simplex at an outer
/// iteration boundary without redoing any completed iteration's inner tunes.
/// Bit-exact: every `f64` is encoded by its IEEE-754 bit pattern (NOT JSON;
/// see module doc).
#[derive(Clone, PartialEq, Debug)]
struct Checkpoint {
    /// Fingerprint of `(cfg, corpus)` the persisted objectives were computed
    /// under (see [`config_fingerprint`]). A resume whose current fingerprint
    /// differs is treated as a cold start.
    config_hash: u64,
    /// Outer iterations completed so far.
    iter_done: usize,
    /// Kelley restarts already performed (`≤ MAX_RESTARTS`).
    restarts_done: usize,
    /// Total inner tunes completed (seeds + per-iteration moves), for logging
    /// and progress display.
    tunes_done: usize,
    /// All four current simplex vertices.
    verts: [CkptVertex; N_VERTS],
    /// Best-objective sliding window for Kelley's sufficient-decrease check.
    best_window: Vec<f64>,
}

/// One vertex as persisted: z + objective + full tuned-core weight vector +
/// frozen K + inner-tune iteration count. Rebuilds into a [`Vertex`] via
/// `EvalParams::shipped().with_core(&core)` on resume (non-core fields come
/// from `shipped`, matching the original solve since the inner tune does not
/// touch them).
#[derive(Clone, PartialEq, Debug)]
struct CkptVertex {
    z: [f64; N_Z],
    obj: f64,
    core: Vec<f64>,
    k: f64,
    iters: u64,
}

// ---------------------------------------------------------------------------
// Public entry point.
// ---------------------------------------------------------------------------

/// Outer Nelder–Mead search over the four lane proportions, selecting the mix
/// that minimizes the aggregate held-out objective. Returns the chosen mix
/// (on the probability 3-simplex) and the inner tune result evaluated at that
/// mix.
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

/// Inner implementation. `stop_after_iters` is a test hook: when `Some(n)`,
/// the search returns `Ok(None)` (after writing the checkpoint) as soon as `n`
/// outer iterations have completed, simulating an interruption at an iteration
/// boundary. Production passes `None` and always gets `Ok(Some(_))`.
fn simplex_search_impl(
    cache: &Path,
    meta: &CacheMeta,
    cfg: &TuneConfig,
    progress_path: Option<&Path>,
    checkpoint_path: Option<&Path>,
    stop_after_iters: Option<usize>,
) -> Result<Option<([f64; 4], TuneResult)>, TexelError> {
    let base_split = split_by_game(meta, cfg.val_fraction, cfg.seed);
    let config_hash = config_fingerprint(cfg, meta);

    // One inner Adam solve at the mix corresponding to z, labelled for the log.
    let run_inner = |z: &[f64; N_Z], label: String| -> Result<TuneResult, TexelError> {
        let mut inner_cfg = cfg.clone();
        inner_cfg.progress_label = Some(label);
        let mix = mix_from_z(*z);
        let reweighted = reweight_train(&base_split, meta, &mix, cfg.seed);
        tune(cache, &reweighted, &EvalParams::shipped(), &inner_cfg, None)
    };

    // Restore or cold-start. A checkpoint whose config fingerprint mismatches
    // the current run is discarded — its objectives are incomparable.
    let restored = checkpoint_path
        .and_then(load_checkpoint)
        .filter(|c| c.config_hash == config_hash);

    let (mut verts, mut iter_start, mut restarts_done, mut best_window, mut tunes_done) =
        match restored {
            Some(c) => {
                let verts = ckpt_verts_to_runtime(&c.verts);
                (
                    verts,
                    c.iter_done,
                    c.restarts_done,
                    c.best_window,
                    c.tunes_done,
                )
            }
            None => {
                let seeds = seed_simplex_z();
                let mut built: Vec<Vertex> = Vec::with_capacity(N_VERTS);
                let mut tunes = 0usize;
                for (k, z) in seeds.iter().enumerate() {
                    let mix = mix_from_z(*z);
                    let label = format!("nm seed {}/{} mix={:.3?}", k + 1, N_VERTS, mix);
                    let result = run_inner(z, label)?;
                    let obj = result.val_loss;
                    eprintln!(
                        "[meta] seed {}/{} mix={:.3?}  val_loss={:.6}",
                        k + 1,
                        N_VERTS,
                        mix,
                        obj
                    );
                    built.push(Vertex { z: *z, obj, result });
                    tunes += 1;
                }
                let verts: [Vertex; N_VERTS] = built
                    .try_into()
                    .map_err(|_| TexelError::Cache("seed simplex size".into()))?;
                let best_obj = verts.iter().map(|v| v.obj).fold(f64::INFINITY, f64::min);
                let best_window = vec![best_obj];
                record_progress(
                    progress_path,
                    checkpoint_path,
                    config_hash,
                    0,
                    0,
                    tunes,
                    &verts,
                    &best_window,
                )?;
                (verts, 0usize, 0usize, best_window, tunes)
            }
        };

    // Main N-M loop.
    while iter_start < NM_MAX_ITER {
        // Sort vertex indices ascending by objective (deterministic on ties via total_cmp).
        let order = sort_indices_by_obj(&verts);
        let best_idx = order[0];
        let second_worst_idx = order[N_VERTS - 2];
        let worst_idx = order[N_VERTS - 1];
        let best_obj = verts[best_idx].obj;
        let second_worst_obj = verts[second_worst_idx].obj;
        let worst_obj = verts[worst_idx].obj;

        // Stopping checks (objective spread / parameter spread).
        let obj_spread = worst_obj - best_obj;
        let z_spread = simplex_z_spread(&verts);
        if obj_spread < EPS_F || z_spread < EPS_Z {
            eprintln!(
                "[meta] terminate at iter {}: obj_spread={:.3e}, z_spread={:.3e}",
                iter_start, obj_spread, z_spread
            );
            break;
        }

        let centroid = z_centroid_excluding(&verts, worst_idx);

        // Reflection.
        let r_z = nm_reflect(centroid, verts[worst_idx].z);
        let r_label = format!(
            "nm iter {}/{} reflect mix={:.3?}",
            iter_start + 1,
            NM_MAX_ITER,
            mix_from_z(r_z)
        );
        let r_result = run_inner(&r_z, r_label)?;
        let r_obj = r_result.val_loss;
        tunes_done += 1;

        let move_label;
        if best_obj <= r_obj && r_obj < second_worst_obj {
            // Plain reflection: better than worst, not the new best.
            verts[worst_idx] = Vertex {
                z: r_z,
                obj: r_obj,
                result: r_result,
            };
            move_label = "reflect";
        } else if r_obj < best_obj {
            // Expansion: reflection is the new best — push further.
            let e_z = nm_expand(centroid, r_z);
            let e_label = format!(
                "nm iter {}/{} expand mix={:.3?}",
                iter_start + 1,
                NM_MAX_ITER,
                mix_from_z(e_z)
            );
            let e_result = run_inner(&e_z, e_label)?;
            let e_obj = e_result.val_loss;
            tunes_done += 1;
            if e_obj < r_obj {
                verts[worst_idx] = Vertex {
                    z: e_z,
                    obj: e_obj,
                    result: e_result,
                };
                move_label = "expand";
            } else {
                verts[worst_idx] = Vertex {
                    z: r_z,
                    obj: r_obj,
                    result: r_result,
                };
                move_label = "reflect (expand-failed)";
            }
        } else {
            // Contraction.
            let (c_z, accept_threshold, kind) = if r_obj < worst_obj {
                // Reflection beat the worst but tied or beat 2nd-worst: outside contract.
                (
                    nm_outside_contract(centroid, r_z),
                    r_obj,
                    "outside-contract",
                )
            } else {
                // Reflection failed to beat the worst: inside contract.
                (
                    nm_inside_contract(centroid, verts[worst_idx].z),
                    worst_obj,
                    "inside-contract",
                )
            };
            let c_label = format!(
                "nm iter {}/{} {} mix={:.3?}",
                iter_start + 1,
                NM_MAX_ITER,
                kind,
                mix_from_z(c_z)
            );
            let c_result = run_inner(&c_z, c_label)?;
            let c_obj = c_result.val_loss;
            tunes_done += 1;
            // Strict `<` for both outside and inside contraction: a tie with
            // the acceptance threshold (`r_obj` outside / `worst_obj` inside)
            // falls through to shrink. The shrink is safe (never worsens the
            // best, makes the simplex tighter), so on the rare double-precision
            // tie this is a slightly more cautious but valid variant of the
            // textbook rule (some references use `≤` for outside contraction).
            if c_obj < accept_threshold {
                verts[worst_idx] = Vertex {
                    z: c_z,
                    obj: c_obj,
                    result: c_result,
                };
                move_label = kind;
            } else {
                // Shrink: pull every non-best vertex halfway toward best.
                let best_z = verts[best_idx].z;
                for (i, v) in verts.iter_mut().enumerate() {
                    if i == best_idx {
                        continue;
                    }
                    let s_z = nm_shrink(best_z, v.z);
                    let s_label = format!(
                        "nm iter {}/{} shrink mix={:.3?}",
                        iter_start + 1,
                        NM_MAX_ITER,
                        mix_from_z(s_z)
                    );
                    let s_result = run_inner(&s_z, s_label)?;
                    let s_obj = s_result.val_loss;
                    tunes_done += 1;
                    *v = Vertex {
                        z: s_z,
                        obj: s_obj,
                        result: s_result,
                    };
                }
                move_label = "shrink";
            }
        }

        // Update best-objective window for Kelley's sufficient-decrease test.
        let curr_best = verts.iter().map(|v| v.obj).fold(f64::INFINITY, f64::min);
        best_window.push(curr_best);
        if best_window.len() > KELLEY_WINDOW {
            best_window.remove(0);
        }

        eprintln!(
            "[meta] iter {}/{} {}  worst→({:.6})  best={:.6}  obj_spread={:.3e}  z_spread={:.3e}",
            iter_start + 1,
            NM_MAX_ITER,
            move_label,
            verts[worst_idx].obj,
            curr_best,
            obj_spread,
            z_spread,
        );

        // Kelley restart: if the best objective has not decreased by at least
        // KELLEY_TAU · simplex_size² over the full window, restart with a
        // fresh axis-aligned simplex around the current best.
        if best_window.len() == KELLEY_WINDOW && restarts_done < MAX_RESTARTS {
            let oldest = best_window[0];
            let newest = best_window[KELLEY_WINDOW - 1];
            let decrease = oldest - newest;
            let size_sq = z_spread * z_spread;
            if decrease < KELLEY_TAU * size_sq {
                eprintln!(
                    "[meta] Kelley restart #{} at iter {}: decrease {:.3e} < tau·size² {:.3e}",
                    restarts_done + 1,
                    iter_start + 1,
                    decrease,
                    KELLEY_TAU * size_sq
                );
                let new_best_idx = sort_indices_by_obj(&verts)[0];
                let best_z = verts[new_best_idx].z;
                let preserved_best = verts[new_best_idx].clone();
                let mut new_verts: Vec<Vertex> = Vec::with_capacity(N_VERTS);
                new_verts.push(preserved_best);
                for axis in 0..N_Z {
                    let mut new_z = best_z;
                    new_z[axis] += RESTART_STEP;
                    let r_label = format!(
                        "nm restart {}/{} axis {} mix={:.3?}",
                        restarts_done + 1,
                        MAX_RESTARTS,
                        axis,
                        mix_from_z(new_z)
                    );
                    let r_result = run_inner(&new_z, r_label)?;
                    let r_obj = r_result.val_loss;
                    tunes_done += 1;
                    new_verts.push(Vertex {
                        z: new_z,
                        obj: r_obj,
                        result: r_result,
                    });
                }
                verts = new_verts
                    .try_into()
                    .map_err(|_| TexelError::Cache("restart vertex count".into()))?;
                restarts_done += 1;
                best_window.clear();
                let restart_best = verts.iter().map(|v| v.obj).fold(f64::INFINITY, f64::min);
                best_window.push(restart_best);
            }
        }

        iter_start += 1;
        record_progress(
            progress_path,
            checkpoint_path,
            config_hash,
            iter_start,
            restarts_done,
            tunes_done,
            &verts,
            &best_window,
        )?;

        if stop_after_iters == Some(iter_start) {
            return Ok(None);
        }
    }

    // Return the best vertex (we already paid to evaluate it).
    let best_idx = sort_indices_by_obj(&verts)[0];
    let best = verts[best_idx].clone();
    Ok(Some((mix_from_z(best.z), best.result)))
}

/// Sort vertex indices by objective ascending. Uses `total_cmp` so ordering is
/// deterministic on ties (`partial_cmp` is None-on-NaN; vertices' objectives
/// are finite MSE values in practice, but `total_cmp` removes the failure mode
/// entirely and keeps resume bit-exact).
fn sort_indices_by_obj(verts: &[Vertex; N_VERTS]) -> [usize; N_VERTS] {
    let mut order: [usize; N_VERTS] = [0, 1, 2, 3];
    order.sort_by(|&a, &b| verts[a].obj.total_cmp(&verts[b].obj));
    order
}

/// Rebuild runtime vertices from checkpointed ones (rehydrate `TuneResult`s
/// with `EvalParams::shipped().with_core(&core)`).
fn ckpt_verts_to_runtime(ckpt: &[CkptVertex; N_VERTS]) -> [Vertex; N_VERTS] {
    std::array::from_fn(|i| {
        let cv = &ckpt[i];
        let params = EvalParams::shipped().with_core(&cv.core);
        Vertex {
            z: cv.z,
            obj: cv.obj,
            result: TuneResult {
                params,
                k: cv.k,
                val_loss: cv.obj,
                iters: cv.iters,
            },
        }
    })
}

// ---------------------------------------------------------------------------
// Progress + checkpoint persistence.
// ---------------------------------------------------------------------------

/// Write the progress file and (if enabled) the resume checkpoint. The
/// progress file is best-effort observability; the checkpoint write is
/// correctness-critical for resume, so its errors propagate.
#[allow(clippy::too_many_arguments)]
fn record_progress(
    progress_path: Option<&Path>,
    checkpoint_path: Option<&Path>,
    config_hash: u64,
    iter_done: usize,
    restarts_done: usize,
    tunes_done: usize,
    verts: &[Vertex; N_VERTS],
    best_window: &[f64],
) -> Result<(), TexelError> {
    let best_idx = sort_indices_by_obj(verts)[0];
    let best = &verts[best_idx];
    write_progress_file(
        progress_path,
        tunes_done,
        iter_done,
        mix_from_z(best.z),
        best.result.k,
        best.obj,
    );
    if let Some(path) = checkpoint_path {
        let ckpt_verts: [CkptVertex; N_VERTS] = std::array::from_fn(|i| CkptVertex {
            z: verts[i].z,
            obj: verts[i].obj,
            core: verts[i].result.params.core_to_vec(),
            k: verts[i].result.k,
            iters: verts[i].result.iters,
        });
        let c = Checkpoint {
            config_hash,
            iter_done,
            restarts_done,
            tunes_done,
            verts: ckpt_verts,
            best_window: best_window.to_vec(),
        };
        save_checkpoint(path, &c)?;
    }
    Ok(())
}

/// Format the meta-tuning status for the progress file (grep-friendly, no
/// tooling needed to read).
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

/// Atomically overwrite the progress file (temp → rename). Silently ignores
/// errors — observability, not correctness.
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

/// Meta-search checkpoint binary-format magic (`"MIX2"`, the full-N-M format
/// that supersedes the reflect-only `MIX1` from the post-mortem-era harness).
const MIX_CKPT_MAGIC: u32 = 0x4D49_5832;

/// Atomically write the checkpoint (temp → fsync → rename). A torn `.tmp` is
/// never renamed into place, so `load_checkpoint` only ever sees a complete
/// file.
fn save_checkpoint(path: &Path, c: &Checkpoint) -> Result<(), TexelError> {
    let bytes = encode_checkpoint(c);
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

/// Load a checkpoint (missing/malformed → `None`, i.e. cold start). Resume is
/// always an optimization, never a hard dependency.
fn load_checkpoint(path: &Path) -> Option<Checkpoint> {
    let bytes = std::fs::read(path).ok()?;
    decode_checkpoint(&bytes)
}

/// Bit-exact binary encoding. Every `f64` is `to_bits` → LE u64; every `u32`,
/// `u64`, and length field is LE; trailing 4-byte CRC of everything before it.
fn encode_checkpoint(c: &Checkpoint) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&MIX_CKPT_MAGIC.to_le_bytes());
    body.extend_from_slice(&c.config_hash.to_le_bytes());
    body.extend_from_slice(&(c.iter_done as u64).to_le_bytes());
    body.extend_from_slice(&(c.restarts_done as u64).to_le_bytes());
    body.extend_from_slice(&(c.tunes_done as u64).to_le_bytes());
    // Vertices: fixed count N_VERTS.
    for cv in &c.verts {
        for x in &cv.z {
            body.extend_from_slice(&x.to_bits().to_le_bytes());
        }
        body.extend_from_slice(&cv.obj.to_bits().to_le_bytes());
        body.extend_from_slice(&(cv.core.len() as u32).to_le_bytes());
        for x in &cv.core {
            body.extend_from_slice(&x.to_bits().to_le_bytes());
        }
        body.extend_from_slice(&cv.k.to_bits().to_le_bytes());
        body.extend_from_slice(&cv.iters.to_le_bytes());
    }
    // Kelley window.
    body.extend_from_slice(&(c.best_window.len() as u32).to_le_bytes());
    for x in &c.best_window {
        body.extend_from_slice(&x.to_bits().to_le_bytes());
    }
    let crc = crate::corpus::store::crc32(&body);
    body.extend_from_slice(&crc.to_le_bytes());
    body
}

/// Decode + validate. Returns `None` on any inconsistency (bad magic, short
/// read, CRC mismatch, trailing junk).
fn decode_checkpoint(bytes: &[u8]) -> Option<Checkpoint> {
    // minimum: magic(4) + config_hash(8) + 3×u64 counters(24) + crc(4)
    if bytes.len() < 4 + 8 + 24 + 4 {
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
    let iter_done = read_u64(&mut r)? as usize;
    let restarts_done = read_u64(&mut r)? as usize;
    let tunes_done = read_u64(&mut r)? as usize;
    let mut verts_vec: Vec<CkptVertex> = Vec::with_capacity(N_VERTS);
    for _ in 0..N_VERTS {
        let mut z = [0.0_f64; N_Z];
        for x in &mut z {
            *x = read_f64(&mut r)?;
        }
        let obj = read_f64(&mut r)?;
        let n_core = read_u32(&mut r)? as usize;
        // Bounds-check before allocating: a corrupted byte at `n_core` could
        // ask for a multi-GB Vec long before the per-element `read_f64` runs
        // out of input. The current `N_CORE` (~200) is far below this cap;
        // the cap is set generously to not constrain future growth.
        const MAX_CORE: usize = 65_536;
        if n_core > MAX_CORE {
            return None;
        }
        let mut core = Vec::with_capacity(n_core);
        for _ in 0..n_core {
            core.push(read_f64(&mut r)?);
        }
        let k_scale = read_f64(&mut r)?;
        let iters = read_u64(&mut r)?;
        verts_vec.push(CkptVertex {
            z,
            obj,
            core,
            k: k_scale,
            iters,
        });
    }
    let verts: [CkptVertex; N_VERTS] = verts_vec.try_into().ok()?;
    let n_win = read_u32(&mut r)? as usize;
    // Bounds-check (same rationale as `n_core` above).
    if n_win > KELLEY_WINDOW * 16 {
        return None;
    }
    let mut best_window = Vec::with_capacity(n_win);
    for _ in 0..n_win {
        best_window.push(read_f64(&mut r)?);
    }
    if !r.is_empty() {
        return None;
    }
    Some(Checkpoint {
        config_hash,
        iter_done,
        restarts_done,
        tunes_done,
        verts,
        best_window,
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

/// Fingerprint of the configuration + corpus an inner-tune objective depends
/// on, so a resume can reject a checkpoint written under a different
/// `(cfg, corpus)`. Folds the meta-search-affecting knobs (`seed`,
/// `val_fraction`, `max_iter`, `patience`, `eval_every`) and the corpus
/// identity (`layout_hash` + the per-lane sha256s) into a 64-bit CRC pair.
///
/// `lr`/`reg` are deliberately omitted because `cmd_mixture` hardcodes them;
/// if `texel-tune mixture` ever exposes an `--lr`/regularization flag, add it
/// here too, or a stale checkpoint under a different value would wrongly
/// match.
fn config_fingerprint(cfg: &TuneConfig, meta: &CacheMeta) -> u64 {
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

// ---------------------------------------------------------------------------
// Reweight train (unchanged from the previous mixture path).
// ---------------------------------------------------------------------------

/// Build a reweighted train split for candidate mix `m`.
///
/// For each source `s`, gather its train-pool indices from `base_split.train`,
/// then sample `target[s]` of them using a seeded `Prng`. VOLUME-BASED (no
/// oversampling): for normalized proportions `p_s`, the largest total
/// `T` with `p_s·T ≤ pool_s` for every populated source is
/// `T = min_s(pool_s / p_s)`; then `target_s = round(p_s·T) ≤ pool_s`, so
/// every draw is a subsample WITHOUT replacement (zero duplication). The
/// held-out val set is unchanged across candidates, so `val_loss` stays
/// comparable even though `T` differs by mix.
fn reweight_train(base_split: &Split, meta: &CacheMeta, mix: &[f64; 4], seed: u64) -> Split {
    let mut pools: [Vec<u32>; 4] = Default::default();
    for &idx in &base_split.train {
        let src = meta.game_keys[idx as usize].0 as usize;
        if src < 4 {
            pools[src].push(idx);
        }
    }

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

// ===========================================================================
// Tests.
// ===========================================================================

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

    /// Synthetic 4-lane set where lane 0 has strong label signal (all
    /// `WhiteWin`) while lanes 1–3 have uniform random labels. A model trained
    /// with high lane-0 weight will predict near-1 for lane-0 positions and
    /// achieve low MSE on those val records — giving the simplex a real signal
    /// to up-weight lane 0.
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
            sign_project: false,
        }
    }

    // -----------------------------------------------------------------------
    // Softmax + seed simplex.
    // -----------------------------------------------------------------------

    /// `mix_from_z` is a partition of unity: outputs sum to 1, every component
    /// is strictly positive (the softmax interior of the probability simplex).
    #[test]
    fn mix_from_z_partition_of_unity() {
        let samples: [[f64; N_Z]; 7] = [
            [0.0, 0.0, 0.0],
            [1.0, -1.0, 0.5],
            [5.0, 0.0, -5.0],
            [-3.0, -3.0, -3.0],
            [10.0, 0.0, 0.0],
            [0.0, 0.0, 10.0],
            [100.0, 100.0, 100.0], // stress softmax stability
        ];
        for z in samples {
            let p = mix_from_z(z);
            let sum: f64 = p.iter().sum();
            assert!(
                (sum - 1.0).abs() < 1e-12,
                "mix_from_z must partition unity; z={z:?}, p={p:?}, sum={sum}"
            );
            for (i, &pi) in p.iter().enumerate() {
                assert!(
                    pi > 0.0 && pi < 1.0,
                    "every p_i must lie in (0,1); z={z:?}, p[{i}]={pi}"
                );
            }
        }
    }

    /// The seed simplex `seed_simplex_z()` maps to the user-requested mixes
    /// 3:1:1:1, 1:3:1:1, 1:1:3:1, 1:1:1:3 (each normalized to sum 1), within
    /// double-precision tolerance.
    #[test]
    fn seed_simplex_yields_asked_mixes() {
        let seeds = seed_simplex_z();
        let expected: [[f64; 4]; N_VERTS] = [
            [0.5, 1.0 / 6.0, 1.0 / 6.0, 1.0 / 6.0],
            [1.0 / 6.0, 0.5, 1.0 / 6.0, 1.0 / 6.0],
            [1.0 / 6.0, 1.0 / 6.0, 0.5, 1.0 / 6.0],
            [1.0 / 6.0, 1.0 / 6.0, 1.0 / 6.0, 0.5],
        ];
        for (i, z) in seeds.iter().enumerate() {
            let p = mix_from_z(*z);
            for j in 0..4 {
                assert!(
                    (p[j] - expected[i][j]).abs() < 1e-12,
                    "seed {i} mix[{j}]: expected {}, got {} (z={z:?})",
                    expected[i][j],
                    p[j]
                );
            }
        }
    }

    /// The seed simplex is full-rank in R³ (affinely independent): the three
    /// edge vectors from any one vertex span R³, so the simplex is not
    /// degenerate from the start.
    #[test]
    fn seed_simplex_is_full_rank() {
        let s = seed_simplex_z();
        let v0 = s[0];
        let e1: [f64; 3] = std::array::from_fn(|k| s[1][k] - v0[k]);
        let e2: [f64; 3] = std::array::from_fn(|k| s[2][k] - v0[k]);
        let e3: [f64; 3] = std::array::from_fn(|k| s[3][k] - v0[k]);
        // 3×3 determinant via cofactor expansion along the first row.
        let det = e1[0] * (e2[1] * e3[2] - e2[2] * e3[1]) - e1[1] * (e2[0] * e3[2] - e2[2] * e3[0])
            + e1[2] * (e2[0] * e3[1] - e2[1] * e3[0]);
        assert!(
            det.abs() > 1e-9,
            "edge-vector determinant must be non-zero; got {det}"
        );
    }

    // -----------------------------------------------------------------------
    // N-M move arithmetic.
    // -----------------------------------------------------------------------

    /// All four N-M moves implement their textbook formulas with the standard
    /// coefficients (α=1, γ=2, β=0.5, σ=0.5).
    #[test]
    fn nm_moves_match_textbook_formulas() {
        let c = [1.0, 2.0, 3.0];
        let worst = [4.0, 4.0, 4.0];
        // reflect: c + α(c − worst) = 2c − worst (since α=1).
        let r = nm_reflect(c, worst);
        for k in 0..N_Z {
            assert!(
                (r[k] - (2.0 * c[k] - worst[k])).abs() < 1e-15,
                "reflect[{k}]: expected {}, got {}",
                2.0 * c[k] - worst[k],
                r[k]
            );
        }
        // expand: c + γ(r − c) = -c + 2r (since γ=2).
        let e = nm_expand(c, r);
        for k in 0..N_Z {
            assert!(
                (e[k] - (2.0 * r[k] - c[k])).abs() < 1e-15,
                "expand[{k}]: expected {}, got {}",
                2.0 * r[k] - c[k],
                e[k]
            );
        }
        // outside contract: c + β(r − c) = 0.5(c + r) (since β=0.5).
        let oc = nm_outside_contract(c, r);
        for k in 0..N_Z {
            assert!(
                (oc[k] - 0.5 * (c[k] + r[k])).abs() < 1e-15,
                "outside_contract[{k}]: expected {}, got {}",
                0.5 * (c[k] + r[k]),
                oc[k]
            );
        }
        // inside contract: c + β(worst − c) = 0.5(c + worst).
        let ic = nm_inside_contract(c, worst);
        for k in 0..N_Z {
            assert!(
                (ic[k] - 0.5 * (c[k] + worst[k])).abs() < 1e-15,
                "inside_contract[{k}]: expected {}, got {}",
                0.5 * (c[k] + worst[k]),
                ic[k]
            );
        }
        // shrink: best + σ(v − best) = 0.5(best + v).
        let v = [10.0, 20.0, 30.0];
        let s = nm_shrink(c, v);
        for k in 0..N_Z {
            assert!(
                (s[k] - 0.5 * (c[k] + v[k])).abs() < 1e-15,
                "shrink[{k}]: expected {}, got {}",
                0.5 * (c[k] + v[k]),
                s[k]
            );
        }
    }

    /// The centroid-excluding helper averages all vertices except the worst —
    /// it is the geometric center the reflection direction is drawn from.
    #[test]
    fn z_centroid_excludes_worst_correctly() {
        // Hand-built vertices; objective values don't matter for centroid.
        let verts: [Vertex; N_VERTS] = std::array::from_fn(|i| Vertex {
            z: match i {
                0 => [0.0, 0.0, 0.0],
                1 => [3.0, 0.0, 0.0],
                2 => [0.0, 3.0, 0.0],
                _ => [99.0, 99.0, 99.0],
            },
            obj: i as f64,
            result: TuneResult {
                params: EvalParams::shipped(),
                k: 0.005,
                val_loss: i as f64,
                iters: 1,
            },
        });
        // Exclude vertex 3 (the obviously-worst one).
        let c = z_centroid_excluding(&verts, 3);
        let expected = [(0.0 + 3.0 + 0.0) / 3.0, (0.0 + 0.0 + 3.0) / 3.0, 0.0_f64];
        for k in 0..N_Z {
            assert!(
                (c[k] - expected[k]).abs() < 1e-15,
                "centroid[{k}]: expected {}, got {}",
                expected[k],
                c[k]
            );
        }
    }

    // -----------------------------------------------------------------------
    // End-to-end simplex behavior on a synthetic corpus.
    // -----------------------------------------------------------------------

    /// `simplex_search` returns a valid mix on the probability simplex
    /// (non-negative components summing to 1) with a finite val_loss.
    #[test]
    fn simplex_selects_valid_mix_on_synthetic() {
        let dir = unique_temp_dir("nm-valid");
        let (cache, meta) = build(&dir);
        let (mix, result) = simplex_search(&cache, &meta, &cfg(7), None, None).expect("nm search");
        let s: f64 = mix.iter().sum();
        assert!(
            (s - 1.0).abs() < 1e-6,
            "selected mix must sum to 1; got {s} ({mix:?})"
        );
        assert!(
            mix.iter().all(|&m| (0.0..=1.0).contains(&m)),
            "every mix proportion must lie in [0,1]; got {mix:?}"
        );
        assert!(
            result.val_loss.is_finite(),
            "inner tune result must carry a finite held-out objective"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// On the lane-0-favored synthetic corpus (zero entropy in lane 0, full
    /// entropy in lanes 1–3), the search **prefers** lane 0 strictly above
    /// uniform.
    ///
    /// Falsifiable structural assertion: `mix[0] > 0.25`. A degenerate N-M
    /// that returned the centroid of the seed simplex would land at uniform
    /// (≈0.25 each); one that picked the worst seed (a lane-{1,2,3}-dominant
    /// vertex) would land at `mix[0] ≈ 1/6 ≈ 0.167`. Only a working N-M that
    /// preserves *or* improves on the lane-0-dominant seed (`mix[0] = 0.5`)
    /// passes — much stronger than the previous "improves-or-ties uniform"
    /// test (which trivially held because uniform was itself a seed vertex;
    /// uniform is no longer a seed here).
    #[test]
    fn nelder_mead_prefers_lane_0_on_lane0_signal() {
        let dir = unique_temp_dir("nm-prefers-lane0");
        let (cache, meta) = build(&dir);
        let (mix, result) = simplex_search(&cache, &meta, &cfg(11), None, None).expect("nm search");
        assert!(
            result.val_loss.is_finite(),
            "result must carry a finite val_loss"
        );
        assert!(
            mix[0] > 0.25,
            "chosen mix must strictly prefer lane 0 above uniform on this synthetic \
             (lane 0 is the only zero-entropy lane); got mix={mix:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Checkpoint roundtrip + resume.
    // -----------------------------------------------------------------------

    /// The meta-search checkpoint round-trips bit-exactly through the binary
    /// codec — every f64 (z-coords, objectives, tuned cores, K, Kelley window)
    /// survives `save → load` with an identical bit pattern. This is what
    /// makes a resumed search select the identical worst/best vertex as an
    /// uninterrupted one.
    #[test]
    fn checkpoint_roundtrips_bit_exact() {
        let dir = unique_temp_dir("mixckpt-roundtrip");
        let mk_v = |z: [f64; N_Z], obj: f64, core: Vec<f64>, k: f64, iters: u64| CkptVertex {
            z,
            obj,
            core,
            k,
            iters,
        };
        let c = Checkpoint {
            config_hash: 0xDEAD_BEEF_1234_5678,
            iter_done: 7,
            restarts_done: 1,
            tunes_done: 17,
            verts: [
                mk_v(
                    [1.0_f64.ln(), 0.0, 0.0],
                    0.234_567_891_234,
                    vec![-50.25, 0.0, 7.0 / 3.0, 1e-9],
                    0.005_712_345_6,
                    100,
                ),
                mk_v(
                    [0.0, 3.0_f64.ln(), 0.0],
                    0.198_765_432_109,
                    vec![1.0, 2.0, 3.0, 4.0, 5.0],
                    0.005_690_000_0,
                    200,
                ),
                mk_v(
                    [0.0, 0.0, 3.0_f64.ln()],
                    0.301_111_222_333,
                    vec![-3.062_500_125, 11.0, -22.0],
                    0.005_500_000_0,
                    300,
                ),
                mk_v(
                    [-(3.0_f64.ln()), -(3.0_f64.ln()), -(3.0_f64.ln())],
                    0.211_222_333_444,
                    vec![0.0; 16],
                    0.005_900_000_0,
                    400,
                ),
            ],
            best_window: vec![0.198_765_432_109, 0.198_500, 0.198_400, 0.198_3, 0.1982],
        };
        let path = dir.join("search.mixckpt");
        save_checkpoint(&path, &c).expect("save");
        let back = load_checkpoint(&path).expect("load");
        assert_eq!(back, c, "checkpoint must round-trip structurally");
        // f64 fields must be bit-identical (not merely ≈) — pin via bits.
        for (a, b) in back.verts.iter().zip(c.verts.iter()) {
            for k in 0..N_Z {
                assert_eq!(
                    a.z[k].to_bits(),
                    b.z[k].to_bits(),
                    "z[{k}] must be bit-identical"
                );
            }
            assert_eq!(a.obj.to_bits(), b.obj.to_bits(), "obj bit-identical");
            assert_eq!(a.k.to_bits(), b.k.to_bits(), "K bit-identical");
            for (i, (x, y)) in a.core.iter().zip(b.core.iter()).enumerate() {
                assert_eq!(x.to_bits(), y.to_bits(), "core[{i}] must be bit-identical");
            }
        }
        for (x, y) in back.best_window.iter().zip(c.best_window.iter()) {
            assert_eq!(
                x.to_bits(),
                y.to_bits(),
                "Kelley window entry bit-identical"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A simplex search interrupted at an iteration boundary and resumed from
    /// its checkpoint produces a **bit-identical** final result to an
    /// uninterrupted run: same chosen mix, same tuned weights, same K, same
    /// held-out objective, same inner-tune iteration count.
    ///
    /// The `assert!(stopped.is_none())` below also implicitly pins that the
    /// N-M main loop **actually runs** beyond the seed phase: `stop_after_iters`
    /// only fires when the main-loop counter reaches its bound, so a return of
    /// `None` is direct evidence that at least 3 outer N-M iterations were
    /// executed (the search did not short-circuit at the seed simplex via an
    /// `EPS_F` / `EPS_Z` tolerance). Combined with the bit-identical
    /// uninterrupted run, that's a guard against accidentally bypassing the
    /// outer N-M loop in a future refactor.
    #[test]
    fn simplex_resume_equals_uninterrupted() {
        let dir = unique_temp_dir("simplex-resume");
        let (cache, meta) = build(&dir);
        let cfg = cfg(31);

        // Uninterrupted run.
        let (mix_full, res_full) =
            super::simplex_search_impl(&cache, &meta, &cfg, None, None, None)
                .expect("full")
                .expect("completes");

        // Interrupted run: stop after 3 outer iterations, writing the checkpoint.
        let ckpt = dir.join("search.mixckpt");
        let stopped = super::simplex_search_impl(&cache, &meta, &cfg, None, Some(&ckpt), Some(3))
            .expect("stopped");
        // `stop_after_iters` only fires after the main loop increments the
        // iteration counter, so `None` here directly proves the main loop ran.
        assert!(
            stopped.is_none(),
            "stop_after must return None (proves the N-M main loop executed ≥3 iters)"
        );
        assert!(ckpt.exists(), "interrupted run must leave a checkpoint");

        // Resume.
        let (mix_resume, res_resume) =
            super::simplex_search_impl(&cache, &meta, &cfg, None, Some(&ckpt), None)
                .expect("resume")
                .expect("completes");

        // Identical chosen mix, tuned weights, K, val_loss, iter count.
        for (a, b) in mix_resume.iter().zip(mix_full.iter()) {
            assert_eq!(a.to_bits(), b.to_bits(), "mix bit-identical");
        }
        assert_eq!(
            res_resume.val_loss.to_bits(),
            res_full.val_loss.to_bits(),
            "val_loss bit-identical"
        );
        assert_eq!(
            res_resume.k.to_bits(),
            res_full.k.to_bits(),
            "K bit-identical"
        );
        assert_eq!(res_resume.iters, res_full.iters, "iters bit-identical");
        let rc = res_resume.params.core_to_vec();
        let fc = res_full.params.core_to_vec();
        for (i, (a, b)) in rc.iter().zip(fc.iter()).enumerate() {
            assert_eq!(a.to_bits(), b.to_bits(), "core[{i}] bit-identical");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A checkpoint written under one config must NOT be resumed under a
    /// different config (different seed here): its objectives were computed on
    /// a different split and are incomparable. The mismatch is treated as a
    /// cold start, so the resumed run equals a *fresh* full run under the new
    /// config — not a corrupted blend.
    #[test]
    fn simplex_rejects_checkpoint_from_different_config() {
        let dir = unique_temp_dir("simplex-cfg-mismatch");
        let (cache, meta) = build(&dir);

        // Write a checkpoint under seed 31 (stop partway).
        let ckpt = dir.join("search.mixckpt");
        let _ = super::simplex_search_impl(&cache, &meta, &cfg(31), None, Some(&ckpt), Some(3))
            .expect("seed-31 partial");
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

    /// Foreign-magic / corrupted bytes are rejected (treated as no checkpoint).
    /// Forms a hard guard against accidentally resuming from a stale `MIX1`
    /// (reflect-only) checkpoint into the new full-N-M format.
    #[test]
    fn decode_rejects_foreign_magic_and_corruption() {
        // Empty / too short.
        assert!(decode_checkpoint(&[]).is_none());
        assert!(decode_checkpoint(&[0; 8]).is_none());
        // Foreign magic ("MIX1" instead of "MIX2"): synthesize a minimally
        // well-formed envelope with a *valid* CRC over the whole body so the
        // CRC gate passes, then confirm decode rejects on the magic check
        // alone (the realistic stale-checkpoint scenario — a `MIX1` file from
        // the previous reflect-only era has a valid CRC over its own body).
        let mut bad = vec![0x31u8, 0x58, 0x49, 0x4D]; // "MIX1" LE
        bad.extend_from_slice(&[0u8; 8 + 24]); // satisfy the min-length check
        bad.extend_from_slice(&crate::corpus::store::crc32(&bad).to_le_bytes());
        assert!(decode_checkpoint(&bad).is_none());
    }

    // -----------------------------------------------------------------------
    // Progress format + reweight_train (carried over).
    // -----------------------------------------------------------------------

    #[test]
    fn format_meta_progress_canonical_string() {
        let s = format_meta_progress(7, 3, [0.4, 0.3, 0.2, 0.1], 0.005700, 0.234567);
        assert_eq!(
            s,
            "tunes_done=7\n\
             simplex_iter=3\n\
             best_mix=[0.4000,0.3000,0.2000,0.1000]\n\
             best_k=0.005700\n\
             best_val_loss=0.234567\n"
        );
    }

    /// `reweight_train` is VOLUME-BASED: it realizes the mix with the largest
    /// real-data total that needs no duplication (`T = min_s(pool_s / p_s)`),
    /// so every per-source count is ≤ its pool (subsample without replacement,
    /// never oversampled), the proportions match the mix, the favored source
    /// consumes its full pool, and it is deterministic under a fixed seed.
    #[test]
    fn reweight_train_is_volume_based_no_oversampling() {
        use crate::texel::dataset::{CacheMeta, Split};

        let n_per_src: usize = 40;
        let n_total: usize = n_per_src * 4;
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

        let train: Vec<u32> = (0..n_total as u32).collect();
        let base_split = Split {
            train,
            val: Vec::new(),
        };
        let mix = [0.2f64, 0.5, 0.15, 0.15];
        let target: [u64; 4] = [16, 40, 12, 12];
        assert_eq!(
            target[1], n_per_src as u64,
            "favored source must use its full pool (the volume cap binds here)"
        );

        let seed = 0xABCD_1234_u64;
        let result = reweight_train(&base_split, &meta, &mix, seed);

        let mut got_counts = [0u64; 4];
        for &idx in &result.train {
            let src = meta.game_keys[idx as usize].0 as usize;
            got_counts[src] += 1;
        }
        assert_eq!(got_counts, target);

        // Determinism: same seed yields the same train index multiset.
        let result2 = reweight_train(&base_split, &meta, &mix, seed);
        assert_eq!(result.train, result2.train);

        // Different seed yields a different result with overwhelming probability.
        let result3 = reweight_train(&base_split, &meta, &mix, seed ^ 0xFFFF_FFFF);
        assert_ne!(result.train, result3.train);
    }
}
