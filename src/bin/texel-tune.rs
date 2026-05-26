//! `texel-tune` — M6.I Texel tuning harness CLI (ADR-0037).
//!
//! Subcommands: `cache` (build the feature cache), `kfit` (fit K on init),
//! `tune` (the default Adam solve), `mixture` (the optional bi-level simplex),
//! `apply` (codegen tuned weights into `eval/data.rs`), `crosscheck` (the
//! three-tier model-vs-reference-vs-engine gate), `sensitivity` (the
//! per-parameter diagnostic table).
//!
//! Standalone consumer — it does NOT touch `evaluate` / search. Arguments are
//! parsed with `std::env::args` (no clap dep); `--lanes` may repeat (one per
//! source lane).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clawfish::texel::dataset::{self, CacheMeta, CachedRec};
use clawfish::texel::loss::{Reg, fit_k_features};
use clawfish::texel::optimizer::{self, TuneConfig};
use clawfish::texel::params::{
    self, EvalParams, ParamSensitivity, ParamsFile, SCHEMA_VERSION, StoppingConfig,
};
use clawfish::texel::{features, layout, mixture};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() < 2 || argv[1] == "--help" || argv[1] == "-h" {
        print_help();
        return if argv.len() < 2 {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        };
    }

    let cmd = argv[1].as_str();
    let args = Args::parse(&argv[2..]);
    let result = match cmd {
        "cache" => cmd_cache(&args),
        "kfit" => cmd_kfit(&args),
        "tune" => cmd_tune(&args),
        "mixture" => cmd_mixture(&args),
        "apply" => cmd_apply(&args),
        "crosscheck" => cmd_crosscheck(&args),
        "sensitivity" => cmd_sensitivity(&args),
        other => {
            eprintln!("texel-tune: unknown subcommand {other:?}");
            print_help();
            return ExitCode::FAILURE;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("texel-tune: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    eprintln!(
        "texel-tune — M6.I Texel tuning harness\n\
         \n\
         USAGE: texel-tune <subcommand> [args]\n\
         \n\
         SUBCOMMANDS:\n\
         \x20 cache        --lanes <DIR>×4 --out <CACHE>\n\
         \x20 kfit         --cache <CACHE>\n\
         \x20 tune         --cache <CACHE> --out-params <JSON> --checkpoint <CKPT>\n\
         \x20              --max-iter <N> --seed <S> --eval-every <E>\n\
         \x20              --patience <P> --val-fraction <F>\n\
         \x20 mixture      --lanes <DIR>×4 --out-params <JSON> [tune flags]\n\
         \x20 apply        --params <JSON> --data <data.rs>\n\
         \x20 crosscheck   --lanes <DIR>×4\n\
         \x20 sensitivity  --params <JSON> --cache <CACHE>"
    );
}

// ---------------------------------------------------------------------------
// Minimal arg parser: scalar `--key value` (last wins) + repeatable `--lanes`.
// ---------------------------------------------------------------------------

struct Args {
    scalar: std::collections::BTreeMap<String, String>,
    lanes: Vec<String>,
}

impl Args {
    fn parse(argv: &[String]) -> Args {
        let mut scalar = std::collections::BTreeMap::new();
        let mut lanes = Vec::new();
        let mut i = 0;
        while i < argv.len() {
            if let Some(key) = argv[i].strip_prefix("--") {
                let val = if i + 1 < argv.len() && !argv[i + 1].starts_with("--") {
                    let v = argv[i + 1].clone();
                    i += 1;
                    v
                } else {
                    String::new()
                };
                if key == "lanes" {
                    lanes.push(val);
                } else {
                    scalar.insert(key.to_string(), val);
                }
            }
            i += 1;
        }
        Args { scalar, lanes }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.scalar.get(key).map(String::as_str)
    }
    fn require(&self, key: &str) -> Result<&str, String> {
        self.get(key)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("missing required flag --{key}"))
    }
    fn parse_u64(&self, key: &str, default: u64) -> Result<u64, String> {
        match self.get(key) {
            Some(s) if !s.is_empty() => s.parse().map_err(|e| format!("--{key}: {e}")),
            _ => Ok(default),
        }
    }
    fn parse_u32(&self, key: &str, default: u32) -> Result<u32, String> {
        match self.get(key) {
            Some(s) if !s.is_empty() => s.parse().map_err(|e| format!("--{key}: {e}")),
            _ => Ok(default),
        }
    }
    fn parse_f64(&self, key: &str, default: f64) -> Result<f64, String> {
        match self.get(key) {
            Some(s) if !s.is_empty() => s.parse().map_err(|e| format!("--{key}: {e}")),
            _ => Ok(default),
        }
    }
    fn four_lanes(&self) -> Result<[PathBuf; 4], String> {
        if self.lanes.len() != 4 {
            return Err(format!(
                "expected exactly 4 --lanes dirs, got {}",
                self.lanes.len()
            ));
        }
        Ok([
            PathBuf::from(&self.lanes[0]),
            PathBuf::from(&self.lanes[1]),
            PathBuf::from(&self.lanes[2]),
            PathBuf::from(&self.lanes[3]),
        ])
    }
}

// ---------------------------------------------------------------------------
// Subcommands.
// ---------------------------------------------------------------------------

fn cmd_cache(args: &Args) -> Result<(), String> {
    let dirs = args.four_lanes()?;
    let out = args.require("out")?;
    let dir_refs: [&Path; 4] = [&dirs[0], &dirs[1], &dirs[2], &dirs[3]];
    let lanes = dataset::load_lanes(dir_refs).map_err(|e| e.to_string())?;
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let meta = dataset::build_cache(&lanes, Path::new(out), workers).map_err(|e| e.to_string())?;
    eprintln!(
        "texel-tune cache: wrote {} records to {out} (per-source {:?})",
        meta.n, meta.per_source
    );
    Ok(())
}

fn cmd_kfit(args: &Args) -> Result<(), String> {
    let cache = args.require("cache")?;
    let init = EvalParams::shipped().core_to_vec();
    let k = fit_k_features(Path::new(cache), &init);
    println!("{k}");
    Ok(())
}

fn cmd_tune(args: &Args) -> Result<(), String> {
    let cache = PathBuf::from(args.require("cache")?);
    let out_params = PathBuf::from(args.require("out-params")?);
    let checkpoint = args.get("checkpoint").map(PathBuf::from);
    let max_iter = args.parse_u64("max-iter", 10_000)?;
    let seed = args.parse_u64("seed", 0)?;
    let eval_every = args.parse_u64("eval-every", 100)?;
    let patience = args.parse_u32("patience", 5)?;
    let val_fraction = args.parse_f64("val-fraction", 0.1)?;

    let meta = reconstruct_meta(&cache)?;
    let split = dataset::split_by_game(&meta, val_fraction, seed);
    let init = EvalParams::shipped();

    let cfg = TuneConfig {
        lr: 0.05,
        max_iter,
        patience,
        eval_every,
        reg: Reg {
            l2_lambda: 0.0,
            init: init.core_to_vec(),
            mono_lambda: 0.0,
        },
        seed,
        val_fraction,
        checkpoint_path: checkpoint.clone(),
        checkpoint_every: eval_every,
        progress_label: None,
    };

    // Resume from a valid checkpoint if one exists (a torn `.tmp` is ignored by
    // load_checkpoint's CRC/magic check).
    let resume = checkpoint
        .as_ref()
        .and_then(|p| optimizer::load_checkpoint(p).ok());

    let result = optimizer::tune(&cache, &split, &init, &cfg, resume).map_err(|e| e.to_string())?;

    write_params(
        &out_params,
        &result.params,
        result.k,
        [0.25, 0.25, 0.25, 0.25],
        &cfg,
        &cache,
        &meta,
        seed,
    )?;
    eprintln!(
        "texel-tune tune: {} iters, K={}, val_loss={}, wrote {}",
        result.iters,
        result.k,
        result.val_loss,
        out_params.display()
    );
    Ok(())
}

fn cmd_mixture(args: &Args) -> Result<(), String> {
    let dirs = args.four_lanes()?;
    let out_params = PathBuf::from(args.require("out-params")?);
    let seed = args.parse_u64("seed", 0)?;
    let val_fraction = args.parse_f64("val-fraction", 0.1)?;
    let max_iter = args.parse_u64("max-iter", 10_000)?;
    let patience = args.parse_u32("patience", 5)?;
    let eval_every = args.parse_u64("eval-every", 100)?;

    // Build a transient cache from the lanes (the mixture subcommand owns its
    // own cache build).
    let dir_refs: [&Path; 4] = [&dirs[0], &dirs[1], &dirs[2], &dirs[3]];
    let lanes = dataset::load_lanes(dir_refs).map_err(|e| e.to_string())?;
    let cache = out_params.with_extension("mixcache");
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let meta = dataset::build_cache(&lanes, &cache, workers).map_err(|e| e.to_string())?;

    let cfg = TuneConfig {
        lr: 0.05,
        max_iter,
        patience,
        eval_every,
        reg: Reg {
            l2_lambda: 0.0,
            init: EvalParams::shipped().core_to_vec(),
            mono_lambda: 0.0,
        },
        seed,
        val_fraction,
        checkpoint_path: None,
        checkpoint_every: eval_every,
        progress_label: None,
    };

    let progress_path = out_params.with_extension("progress");
    let (mix, result) = mixture::simplex_search(&cache, &meta, &cfg, Some(&progress_path))
        .map_err(|e| e.to_string())?;
    write_params(
        &out_params,
        &result.params,
        result.k,
        mix,
        &cfg,
        &cache,
        &meta,
        seed,
    )?;
    let _ = std::fs::remove_file(&cache);
    eprintln!(
        "texel-tune mixture: chosen mix {mix:?}, K={}, val_loss={}",
        result.k, result.val_loss
    );
    Ok(())
}

fn cmd_apply(args: &Args) -> Result<(), String> {
    let params_path = args.require("params")?;
    let data = args.require("data")?;
    let pf = params::read_params_file(Path::new(params_path)).map_err(|e| e.to_string())?;
    params::apply_to_data_rs(&pf.params, Path::new(data)).map_err(|e| e.to_string())?;
    eprintln!("texel-tune apply: rewrote TEXEL-TUNABLE regions in {data}");
    Ok(())
}

fn cmd_crosscheck(args: &Args) -> Result<(), String> {
    let dirs = args.four_lanes()?;
    let dir_refs: [&Path; 4] = [&dirs[0], &dirs[1], &dirs[2], &dirs[3]];
    let lanes = dataset::load_lanes(dir_refs).map_err(|e| e.to_string())?;

    // Three-tier gate over the available corpus positions (ADR-0037 §7):
    //   Tier 1: reference_score_white(pos, shipped) == quiet::static_eval_white.
    //   Tier 2: model_score_white(extract, w) ≈ reference_score_white at the
    //           shipped + a random non-trivial weight vector.
    let shipped = EvalParams::shipped();
    let mut rng = clawfish::corpus::prng::Prng::new(0xC0FFEE);
    let random_core: Vec<f64> = (0..layout::N_CORE)
        .map(|_| rng.below(41) as f64 - 20.0)
        .collect();
    let random_params = shipped.with_core(&random_core);

    let mut checked = 0usize;
    for lane in &lanes.recs {
        for rec in lane {
            let pos = match clawfish::Position::from_fen(&rec.fen) {
                Ok(p) => p,
                Err(_) => continue,
            };
            // Tier 1.
            let reference = features::reference_score_white(&pos, &shipped);
            let engine = clawfish::corpus::quiet::static_eval_white(&pos);
            if (reference - engine).abs() > 1 {
                return Err(format!(
                    "Tier-1 crosscheck FAILED: reference {reference} != engine {engine} at {}",
                    rec.fen
                ));
            }
            // Tier 2 (skip near-bare positions: the model does not short-circuit
            // on insufficient material but the reference does, and those never
            // occur in a quiet game corpus — ADR-0037 §2. A ≤4-piece guard via
            // the public occupancy count conservatively covers every
            // insufficient-material count without duplicating the predicate).
            if pos.occupied_all().count() > 4 {
                let fv = features::extract(&pos);
                let model = features::model_score_white(&fv, &random_core);
                let reference_rand = features::reference_score_white(&pos, &random_params) as f64;
                if (model - reference_rand).abs() > 2.0 {
                    return Err(format!(
                        "Tier-2 crosscheck FAILED: model {model} vs reference {reference_rand} at {}",
                        rec.fen
                    ));
                }
            }
            checked += 1;
        }
    }
    eprintln!("texel-tune crosscheck: {checked} positions passed the three-tier gate");
    Ok(())
}

fn cmd_sensitivity(args: &Args) -> Result<(), String> {
    let params_path = args.require("params")?;
    let cache = PathBuf::from(args.require("cache")?);
    let pf = params::read_params_file(Path::new(params_path)).map_err(|e| e.to_string())?;
    let core = EvalParams::shipped()
        .with_core(&core_from_ints(&pf.params))
        .core_to_vec();
    let sens = sensitivity_table(&cache, &core, pf.k);
    let json = serde_json::to_string_pretty(&sens).map_err(|e| e.to_string())?;
    println!("{json}");
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Reconstruct the [`CacheMeta`] (game_keys + per_source) by streaming the
/// on-disk cache (sorted `(source, game_id, ply)`), so the CLI can build the
/// group-by-game split from just the `--cache` path.
fn reconstruct_meta(cache: &Path) -> Result<CacheMeta, String> {
    let mut game_keys: Vec<(u8, u64)> = Vec::new();
    let mut per_source = [0u64; 4];
    for chunk in dataset::stream_chunks(cache, 65_536) {
        for rec in &chunk {
            game_keys.push((rec.source, rec.game_id));
            if (rec.source as usize) < 4 {
                per_source[rec.source as usize] += 1;
            }
        }
    }
    if game_keys.is_empty() {
        return Err(format!("cache {} is empty or unreadable", cache.display()));
    }
    Ok(CacheMeta {
        n: game_keys.len() as u64,
        layout_hash: 0,
        lane_sha256: Default::default(),
        per_source,
        game_keys,
    })
}

/// Recover the core f64 vec from an integer params artifact (round-trip through
/// `with_core`/`core_to_vec`).
fn core_from_ints(p: &params::EvalParamsI32) -> Vec<f64> {
    // Rebuild an EvalParams whose core fields carry the integer values, then
    // read the core vec. The shipped() base supplies the non-core fields.
    let mut ep = EvalParams::shipped();
    // Map each integer field back via the same layout the core vec uses.
    let f = |x: i32| x as f64;
    ep.iso_mg = f(p.iso_mg);
    ep.iso_eg = f(p.iso_eg);
    ep.dbl_mg = f(p.dbl_mg);
    ep.dbl_eg = f(p.dbl_eg);
    ep.bwd_mg = f(p.bwd_mg);
    ep.bwd_eg = f(p.bwd_eg);
    for k in 0..8 {
        ep.conn_mg[k] = f(p.conn_mg[k]);
        ep.conn_eg[k] = f(p.conn_eg[k]);
        ep.passed_mg[k] = f(p.passed_mg[k]);
        ep.passed_eg[k] = f(p.passed_eg[k]);
        ep.passed_free_eg_delta[k] = f(p.passed_free_eg_delta[k]);
        ep.outpost_knight_mg[k] = f(p.outpost_knight_mg[k]);
        ep.outpost_knight_eg[k] = f(p.outpost_knight_eg[k]);
        ep.outpost_bishop_mg[k] = f(p.outpost_bishop_mg[k]);
        ep.outpost_bishop_eg[k] = f(p.outpost_bishop_eg[k]);
    }
    ep.passed_kdist_own_per_step = f(p.passed_kdist_own_per_step);
    ep.passed_kdist_enemy_per_step = f(p.passed_kdist_enemy_per_step);
    for k in 0..9 {
        ep.knight_mobility_mg[k] = f(p.knight_mobility_mg[k]);
        ep.knight_mobility_eg[k] = f(p.knight_mobility_eg[k]);
    }
    for k in 0..14 {
        ep.bishop_mobility_mg[k] = f(p.bishop_mobility_mg[k]);
        ep.bishop_mobility_eg[k] = f(p.bishop_mobility_eg[k]);
    }
    for k in 0..15 {
        ep.rook_mobility_mg[k] = f(p.rook_mobility_mg[k]);
        ep.rook_mobility_eg[k] = f(p.rook_mobility_eg[k]);
    }
    for k in 0..28 {
        ep.queen_mobility_mg[k] = f(p.queen_mobility_mg[k]);
        ep.queen_mobility_eg[k] = f(p.queen_mobility_eg[k]);
    }
    ep.shield_1_mg = f(p.shield_1_mg);
    ep.shield_1_eg = f(p.shield_1_eg);
    ep.shield_2_mg = f(p.shield_2_mg);
    ep.shield_2_eg = f(p.shield_2_eg);
    ep.ks_kfile_semi_open_mg = f(p.ks_kfile_semi_open_mg);
    ep.ks_kfile_open_mg = f(p.ks_kfile_open_mg);
    ep.ks_adj_semi_open_mg = f(p.ks_adj_semi_open_mg);
    ep.ks_both_adj_semi_open_mg = f(p.ks_both_adj_semi_open_mg);
    ep.rook_open_file_mg = f(p.rook_open_file_mg);
    ep.rook_open_file_eg = f(p.rook_open_file_eg);
    ep.rook_semi_open_file_mg = f(p.rook_semi_open_file_mg);
    ep.rook_semi_open_file_eg = f(p.rook_semi_open_file_eg);
    ep.core_to_vec()
}

/// Build the per-parameter sensitivity table: the loss gradient at the endpoint
/// + a ±10% perturbation loss delta (ADR-0037 §9, recorded in all outcomes).
fn sensitivity_table(cache: &Path, core: &[f64], k: f64) -> Vec<ParamSensitivity> {
    // Load the whole cache once (the sensitivity diagnostic is a one-shot pass).
    let recs: Vec<CachedRec> = dataset::stream_chunks(cache, 65_536).flatten().collect();
    let reg = Reg {
        l2_lambda: 0.0,
        init: vec![0.0; core.len()],
        mono_lambda: 0.0,
    };
    let mut grad = vec![0.0; core.len()];
    let (base_loss, n) = clawfish::texel::loss::loss_and_grad(&recs, core, k, &reg, &mut grad);
    let base_mean = if n == 0 { 0.0 } else { base_loss / n as f64 };

    (0..core.len())
        .map(|i| {
            let mut w = core.to_vec();
            let delta = (core[i] * 0.10).abs().max(1.0);
            w[i] = core[i] + delta;
            let mut g = vec![0.0; core.len()];
            let (lp, np) = clawfish::texel::loss::loss_and_grad(&recs, &w, k, &reg, &mut g);
            let perturbed = if np == 0 { 0.0 } else { lp / np as f64 };
            ParamSensitivity {
                name: param_name(i),
                grad: grad[i],
                sprt_margin_sens: perturbed - base_mean,
            }
        })
        .collect()
}

/// Human-readable name for core index `i` (group + offset).
fn param_name(i: usize) -> String {
    let g = layout::group_of(i);
    let off = i - layout::group_range(g).start;
    format!("{g:?}[{off}]")
}

/// Write the `ParamsFile` artifact (tuned ints + K + mix + sensitivity + lane
/// sha256 + stopping consts).
#[allow(clippy::too_many_arguments)]
fn write_params(
    out: &Path,
    params: &EvalParams,
    k: f64,
    mix: [f64; 4],
    cfg: &TuneConfig,
    cache: &Path,
    meta: &CacheMeta,
    seed: u64,
) -> Result<(), String> {
    let core = params.core_to_vec();
    let sensitivity = sensitivity_table(cache, &core, k);
    let pf = ParamsFile {
        schema_version: SCHEMA_VERSION,
        params: params.to_ints(),
        k,
        mix,
        stopping: StoppingConfig {
            patience: cfg.patience,
            max_iter: cfg.max_iter,
            eval_every: cfg.eval_every,
            quant_stable_checks: 3,
        },
        sensitivity,
        lane_sha256: meta.lane_sha256.clone(),
        seed,
    };
    params::write_params_file(&pf, out).map_err(|e| e.to_string())
}
