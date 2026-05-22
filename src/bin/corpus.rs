//! `corpus` — M6.G corpus-construction CLI driver.
//!
//! Subcommands: `calibrate-ladder`, `selfplay`, `ingest-pgn`, `build`,
//! `quality-gate`, `export`, `rerun`. R5/R6/R7: streaming, all-cores
//! workers, graceful SIGTERM/SIGINT, crash-safe via the per-game
//! append-block log. Implemented in the M6.G integration slice per
//! `docs/plans/m6.g.md` §2/§5.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clawfish::corpus::dedup::{dedup_fen, per_game_cap};
use clawfish::corpus::filter::GameFilter;
use clawfish::corpus::manifest::{
    Manifest, hex_digest, read_manifest, sha256_bytes, write_filter_spec, write_manifest,
};
use clawfish::corpus::pgn::{PgnStats, stream_pgn};
use clawfish::corpus::quality_gate::{FEN_LEAKAGE_TAU, QualityReport, run_quality_gate};
use clawfish::corpus::selfplay::{
    OpeningMode, SelfPlayConfig, calibrate_ladder, run as selfplay_run,
};
use clawfish::corpus::split::split_by_game;
use clawfish::corpus::store::{GameBlock, append_block, atomic_write, scan_valid_blocks};
use clawfish::corpus::{
    CorpusRecord, DEPTH_RUNG_EXTERNAL, Label, PER_GAME_CAP, QUIET_MARGIN_CP, Source,
};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() < 2 || argv[1] == "--help" || argv[1] == "-h" {
        print_top_help();
        return if argv.len() < 2 {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        };
    }
    let cmd = argv[1].as_str();
    let rest = &argv[2..];
    let res = match cmd {
        "calibrate-ladder" => cmd_calibrate_ladder(rest),
        "selfplay" => cmd_selfplay(rest),
        "ingest-pgn" => cmd_ingest_pgn(rest),
        "fetch" => cmd_fetch(rest),
        "build" => cmd_build(rest),
        "quality-gate" => cmd_quality_gate(rest),
        "export" => cmd_export(rest),
        "rerun" => cmd_rerun(rest),
        "truncate" => cmd_truncate(rest),
        _ => {
            eprintln!("corpus: unknown subcommand {cmd:?}");
            print_top_help();
            return ExitCode::FAILURE;
        }
    };
    match res {
        Ok(code) => code,
        Err(e) => {
            eprintln!("corpus: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print_top_help() {
    eprintln!(
        "corpus — M6.G corpus-construction CLI.

Usage: corpus <subcommand> [options]

Subcommands:
  calibrate-ladder   Empirically measure the R-TC depth ladder from `bench` movetime
                     buckets and write it to filter_spec.txt + manifest.json.
  selfplay           Run a deterministic fixed-depth self-play campaign.
                     Crash-safe (per-game append-block log + checkpoint).
  ingest-pgn         Stream-parse an on-disk CCRL/Lichess PGN into the shard log.
  fetch              Stream a CCRL/Lichess dump over HTTPS into the shard log
                     (M6.H; needs the `corpus-fetch` build feature).
  build              Apply filter → quiet → strata → dedup → cap → split;
                     emit train+val shards, manifest.json, filter_spec.txt.
  quality-gate       Run the six data-quality checks; exit 0 iff the gate passes.
  export             Convert the binary shard log to human-greppable TSV.
  rerun              Re-execute the stage→build→quality-gate flow from a manifest.
  truncate           Truncate shard.bin + val.bin to ≤ --cap-positions on a per-game
                     boundary; resets checkpoint.bin for a coherent resume.

Use `corpus <subcommand> --help` for details."
    );
}

// ─── argv parsing helpers ──────────────────────────────────────────────────

struct Args {
    flags: BTreeMap<String, String>,
}

fn parse_args(argv: &[String]) -> Args {
    let mut flags = BTreeMap::new();
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        if let Some(key) = a.strip_prefix("--") {
            // Forms: --key=value | --key value | --key (bool)
            if let Some(eq) = key.find('=') {
                let (k, v) = key.split_at(eq);
                flags.insert(k.to_string(), v[1..].to_string());
            } else if i + 1 < argv.len() && !argv[i + 1].starts_with("--") {
                flags.insert(key.to_string(), argv[i + 1].clone());
                i += 1;
            } else {
                flags.insert(key.to_string(), String::new());
            }
        }
        i += 1;
    }
    Args { flags }
}

impl Args {
    fn get(&self, key: &str) -> Option<&str> {
        self.flags.get(key).map(String::as_str)
    }
    fn require(&self, key: &str) -> Result<&str, String> {
        self.get(key)
            .ok_or_else(|| format!("missing required flag --{key}"))
    }
    fn parse_u64(&self, key: &str, default: u64) -> Result<u64, String> {
        self.get(key)
            .map(|s| s.parse::<u64>().map_err(|e| format!("--{key}: {e}")))
            .unwrap_or(Ok(default))
    }
    fn parse_u32(&self, key: &str, default: u32) -> Result<u32, String> {
        self.get(key)
            .map(|s| s.parse::<u32>().map_err(|e| format!("--{key}: {e}")))
            .unwrap_or(Ok(default))
    }
    fn parse_usize(&self, key: &str, default: usize) -> Result<usize, String> {
        self.get(key)
            .map(|s| s.parse::<usize>().map_err(|e| format!("--{key}: {e}")))
            .unwrap_or(Ok(default))
    }
    fn parse_f64(&self, key: &str, default: f64) -> Result<f64, String> {
        self.get(key)
            .map(|s| s.parse::<f64>().map_err(|e| format!("--{key}: {e}")))
            .unwrap_or(Ok(default))
    }
    fn has(&self, key: &str) -> bool {
        self.flags.contains_key(key)
    }
}

// ─── signal handler (graceful SIGTERM/SIGINT → set stop) ───────────────────

fn install_signal_handler(stop: Arc<AtomicBool>) {
    // Background thread that polls the signal-arrived atomic flag set by
    // the OS handler. We register the handler with `libc::signal` so a
    // SIGTERM or SIGINT flips an atomic that this thread observes and
    // forwards into the `stop` flag the campaign reads. No `unsafe`
    // shenanigans inside the handler itself — just a relaxed store.
    static GOT_SIGNAL: AtomicBool = AtomicBool::new(false);

    extern "C" fn handle(_sig: libc::c_int) {
        GOT_SIGNAL.store(true, Ordering::Relaxed);
    }

    unsafe {
        libc::signal(libc::SIGTERM, handle as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, handle as *const () as libc::sighandler_t);
    }

    std::thread::spawn(move || {
        loop {
            if GOT_SIGNAL.load(Ordering::Relaxed) {
                stop.store(true, Ordering::Relaxed);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
}

// ─── calibrate-ladder ──────────────────────────────────────────────────────

fn cmd_calibrate_ladder(argv: &[String]) -> Result<ExitCode, String> {
    let args = parse_args(argv);
    if args.has("help") {
        eprintln!(
            "corpus calibrate-ladder --buckets <ms,ms,...> --out <dir>

Measure clawfish's median completed iterative-deepening depth at each
deployment movetime bucket over the bench corpus. Writes filter_spec.txt
+ a manifest stub recording the calibration.

Flags:
  --buckets <list>   Comma-separated movetime budgets in ms (default: 100,200,400,600)
  --out <dir>        Output directory (created if missing)"
        );
        return Ok(ExitCode::SUCCESS);
    }
    let out = PathBuf::from(args.require("out")?);
    let buckets_str = args.get("buckets").unwrap_or("100,200,400,600");
    let buckets: Vec<u64> = buckets_str
        .split(',')
        .map(|s| {
            s.trim()
                .parse::<u64>()
                .map_err(|e| format!("--buckets: {e}"))
        })
        .collect::<Result<_, _>>()?;
    fs::create_dir_all(&out).map_err(|e| format!("create {}: {e}", out.display()))?;

    eprintln!("corpus: calibrating depth ladder over bucket(s) {buckets:?} ms on bench corpus...");
    let ladder = calibrate_ladder(&buckets);
    eprintln!("corpus: calibrated ladder = {ladder:?}");

    // Write the ladder + the pinned-interface constants to filter_spec.txt
    // (the M6.G↔M6.I contract surface).
    write_filter_spec(&out).map_err(|e| format!("write_filter_spec: {e}"))?;
    // Append the ladder, the FEN-leakage tau, and the bucket→depth mapping.
    let mut spec = fs::read_to_string(out.join("filter_spec.txt"))
        .map_err(|e| format!("read filter_spec.txt: {e}"))?;
    spec.push_str("# R-TC depth ladder (corpus calibrate-ladder).\n");
    for ((depth, weight), bucket) in ladder.iter().zip(buckets.iter()) {
        spec.push_str(&format!("DEPTH_RUNG_{bucket}MS={depth},{weight}\n"));
    }
    spec.push_str(&format!("FEN_LEAKAGE_TAU={FEN_LEAKAGE_TAU}\n"));
    fs::write(out.join("filter_spec.txt"), spec).map_err(|e| format!("write filter_spec: {e}"))?;

    // Write a minimal manifest stub (no corpus bytes yet — just records the
    // calibration so a subsequent `selfplay` can refer to it).
    let manifest = Manifest {
        schema_version: 1,
        created_at: timestamp(),
        engine_commit: engine_commit(),
        total_positions: 0,
        validation_fraction: 0.0,
        sources: Vec::new(),
        self_play_seed: 0,
        games: 0,
        max_plies: 0,
        opening_random_plies: 0,
        workers: 0,
        split_seed: 0,
        val_fraction: 0.0,
        depth_ladder: ladder,
        opening_book_path: None,
        opening_book_sha256: None,
        opening_mode: None,
        corpus_sha256: String::new(),
    };
    write_manifest(&out, &manifest).map_err(|e| format!("write_manifest: {e}"))?;
    eprintln!(
        "corpus: wrote {}/manifest.json + filter_spec.txt",
        out.display()
    );
    Ok(ExitCode::SUCCESS)
}

// ─── selfplay ──────────────────────────────────────────────────────────────

fn cmd_selfplay(argv: &[String]) -> Result<ExitCode, String> {
    let args = parse_args(argv);
    if args.has("help") {
        eprintln!(
            "corpus selfplay --seed <u64> --opening-mode <book|random> [--games <N>] \\
                [--cap-positions <N>] --workers <N> --out <dir> \\
                [--max-plies <N>] [--opening-random-plies <N>] [--val-fraction <f>] \\
                [--opening-book <path>] [--split-seed <u64>]

Run a deterministic fixed-depth self-play campaign. Uses the calibrated
depth ladder from <out>/manifest.json if present, else falls back to
depth 4. Crash-safe (R1/R2/R3). SIGTERM/SIGINT triggers a graceful
drop-in-flight + checkpoint flush.

--games is OPTIONAL: omit it for an unbounded campaign (kill with
Ctrl-C / SIGTERM when satisfied — R2 guarantees zero partial labels).
With --games <N>, the campaign caps at game N-1 (matching R1's
idempotent resume — repeated runs with the same seed and a larger
--games extend the corpus). The manifest records the ACTUAL durable
game count at exit, so re-run.sh reproduces what you have.

--opening-mode (REQUIRED): determines the opening regime AND the
provenance every record carries.
  book    Seed every game from --opening-book <path> (default
          bench/data/openings.epd if vendored). Records ⇒
          Source::SelfPlayOnBook. Operate one campaign per --out dir.
  random  Seed every game from startpos + --opening-random-plies
          random plies. Records ⇒ Source::SelfPlayOffBook. The
          opening book is ignored even if vendored.

The on-book / off-book proportion that the M6.I bi-level optimizer
reweights at training time IS the per-source weight on the
StratObjective; the operator grows the two corpora independently by
running one campaign per mode."
        );
        return Ok(ExitCode::SUCCESS);
    }
    let out = PathBuf::from(args.require("out")?);
    fs::create_dir_all(&out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let seed = args.parse_u64("seed", 0)?;
    // Default unbounded — campaign runs until SIGINT/SIGTERM. Resume on a
    // later run skips already-durable game_ids (R1 idempotent resume).
    let games = args.parse_u64("games", u64::MAX)?;
    let workers = args.parse_usize("workers", num_workers_default())?;
    let max_plies = args.parse_u32("max-plies", 400)?;
    let opening_random_plies = args.parse_u32("opening-random-plies", 8)?;
    let val_fraction = args.parse_f64("val-fraction", 0.1)?;

    // Pick up the depth ladder from a pre-existing manifest if present
    // (calibrate-ladder writes one). Empty ladder ⇒ selfplay defaults to a
    // shallow depth-4 rung (documented in `selfplay::run`).
    let depth_ladder = match read_manifest(&out) {
        Ok(m) if !m.depth_ladder.is_empty() => m.depth_ladder,
        _ => {
            eprintln!("corpus: no pre-existing manifest depth_ladder — defaulting to [(4, 1)]");
            vec![(4, 1)]
        }
    };

    let opening_mode = match args.require("opening-mode")? {
        "book" => OpeningMode::Book,
        "random" => OpeningMode::Random,
        other => {
            return Err(format!(
                "--opening-mode must be `book` or `random`, got {other:?}"
            ));
        }
    };

    // Opening book is required in Book mode, ignored in Random mode. In
    // Book mode the operator may pass --opening-book explicitly; otherwise
    // we try the vendored bench/data/openings.epd default.
    let (opening_book_path, book) = match opening_mode {
        OpeningMode::Book => {
            let path = args
                .get("opening-book")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("bench/data/openings.epd"));
            if !path.exists() {
                return Err(format!(
                    "--opening-mode=book requires an opening book; \
                     pass --opening-book <path> (tried {})",
                    path.display()
                ));
            }
            let book = std::sync::Arc::new(
                clawfish::corpus::openings::Book::load_epd(&path)
                    .map_err(|e| format!("load opening book {}: {e}", path.display()))?,
            );
            (Some(path), Some(book))
        }
        OpeningMode::Random => {
            if args.get("opening-book").is_some() {
                eprintln!(
                    "corpus: --opening-mode=random ignores --opening-book \
                     (every game starts from startpos + random plies)"
                );
            }
            (None, None)
        }
    };

    let split_seed = args.parse_u64("split-seed", 7)?;
    // --cap-positions <N>: graceful exit when durable positions reach N.
    // Distinct from --games (a cap on dispatched game_ids) — the operator's
    // real target for Texel tuning is corpus *size*, and games-per-position
    // varies. Both caps can be set; whichever fires first wins.
    let cap_positions = args
        .get("cap-positions")
        .map(|s| s.parse::<u64>())
        .transpose()
        .map_err(|e| format!("--cap-positions: {e}"))?;
    let cfg = SelfPlayConfig {
        seed,
        games,
        workers,
        depth_ladder: depth_ladder.clone(),
        opening_random_plies,
        max_plies,
        out_dir: out.clone(),
        val_fraction,
        opening_mode,
        book: book.clone(),
        poll_interval_ms: None,
        split_seed,
        cap_positions,
    };
    let stop = Arc::new(AtomicBool::new(false));
    install_signal_handler(Arc::clone(&stop));

    let games_display = if games == u64::MAX {
        "unbounded".to_string()
    } else {
        games.to_string()
    };
    let mode_display = match opening_mode {
        OpeningMode::Book => "book",
        OpeningMode::Random => "random",
    };
    eprintln!(
        "corpus: starting self-play campaign seed={seed} games={games_display} \
         workers={workers} depth_ladder={depth_ladder:?} val_fraction={val_fraction} \
         opening_mode={mode_display} out={}",
        out.display()
    );

    let stats = selfplay_run(&cfg, &stop).map_err(|e| format!("selfplay::run: {e}"))?;
    eprintln!(
        "corpus: campaign done — games_committed={} dropped_inflight={} positions_committed={}",
        stats.games_committed, stats.games_dropped_inflight, stats.positions_committed
    );

    // Record the ACTUAL durable game count at exit (resume-safe across
    // SIGINT-killed runs and across multi-night accumulation). Scan the
    // shard once at end; if absent fall back to this run's increment.
    // `re-run.sh` reads this number from the manifest and passes it to
    // `corpus selfplay --games <N>` → resume + complete-to-N path that
    // byte-reproduces the durable shard.
    let games_durable = scan_valid_blocks(&out.join("shard.bin"))
        .map(|(blocks, _)| blocks.len() as u64)
        .unwrap_or(stats.games_committed);

    // Pin every self-play knob into the manifest so a subsequent `build` +
    // `quality-gate` records them in the frozen artifact (the R-TC
    // reproducibility contract — `re-run.sh` reads each from manifest.json,
    // no shell-default substitution for a pinned knob).
    if let Ok(mut m) = read_manifest(&out) {
        m.self_play_seed = seed;
        m.games = games_durable;
        m.max_plies = max_plies;
        m.opening_random_plies = opening_random_plies;
        m.workers = workers as u32;
        m.val_fraction = val_fraction;
        m.opening_book_path = opening_book_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        m.opening_book_sha256 = book.as_ref().map(|b| b.sha256().to_owned());
        m.opening_mode = Some(match opening_mode {
            OpeningMode::Book => "book".into(),
            OpeningMode::Random => "random".into(),
        });
        write_manifest(&out, &m).map_err(|e| format!("update selfplay knobs: {e}"))?;
    }
    Ok(ExitCode::SUCCESS)
}

fn num_workers_default() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Largest `game_id` present across every shard in `dir` that an
/// `ingest-pgn` invocation could collide with. Scans both `shard.bin`
/// (self-play) and `pgn-shard.bin` (prior `ingest-pgn` runs) — missing
/// files contribute 0, never an error (`scan_valid_blocks` returns
/// `Ok((vec![], 0))` for a non-existent path).
fn max_existing_game_id(dir: &Path) -> u64 {
    let mut hi: u64 = 0;
    for name in ["shard.bin", "pgn-shard.bin"] {
        if let Ok((blocks, _)) = scan_valid_blocks(&dir.join(name))
            && let Some(m) = blocks.iter().map(|b| b.game_id).max()
        {
            hi = hi.max(m);
        }
    }
    hi
}

// ─── ingest-pgn ────────────────────────────────────────────────────────────

fn cmd_ingest_pgn(argv: &[String]) -> Result<ExitCode, String> {
    let args = parse_args(argv);
    if args.has("help") {
        eprintln!(
            "corpus ingest-pgn --path <pgn> --out <dir> [--source ccrl|lichess]

Stream-parse a CCRL or Lichess PGN; apply `GameFilter::default()`;
emit each accepted game as one CRC-framed block in <out>/pgn-shard.bin
(separate from the self-play shard so the two streams remain
distinguishable for the audit). The `--source` tag determines the
provenance recorded on every emitted record."
        );
        return Ok(ExitCode::SUCCESS);
    }
    let path = PathBuf::from(args.require("path")?);
    let out = PathBuf::from(args.require("out")?);
    fs::create_dir_all(&out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let source = match args.get("source").unwrap_or("ccrl") {
        "ccrl" => Source::Ccrl,
        "lichess" => Source::LichessOpen,
        other => {
            return Err(format!(
                "--source must be `ccrl` or `lichess`, got {other:?}"
            ));
        }
    };
    let filter = GameFilter::default();

    let f = fs::File::open(&path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let r = BufReader::new(f);
    let shard = out.join("pgn-shard.bin");

    // Reserve a game-id range for this PGN: start strictly above any
    // existing game-id in BOTH the self-play shard AND any prior
    // ingest-pgn shard. A second `ingest-pgn` invocation would otherwise
    // collide with the first one's ids (which sat in `pgn-shard.bin`).
    let base_id = max_existing_game_id(&out) + 1;

    let mut stats = PgnStats::default();
    let shard_ref = shard.clone();
    let mut on_game = move |gp: clawfish::corpus::pgn::GamePositions| {
        // Game-level admission first (Termination/TC/Elo).
        if !clawfish::corpus::filter::game_admitted(&gp.tags, &filter) {
            return;
        }
        let label = match gp.tags.result {
            Some(l) => l,
            None => return,
        };
        let recs: Vec<CorpusRecord> = gp
            .positions
            .into_iter()
            .map(|(pos, ply)| CorpusRecord {
                fen: pos.to_fen(),
                label,
                source,
                game_id: gp.game_id,
                ply,
                depth_rung: DEPTH_RUNG_EXTERNAL,
                strata: 0,
            })
            .collect();
        if !recs.is_empty()
            && let Err(e) = append_block(&shard_ref, gp.game_id, &recs)
        {
            eprintln!("corpus: append_block failed for game {}: {e}", gp.game_id);
        }
    };

    let next_id =
        stream_pgn(r, base_id, &mut on_game, &mut stats).map_err(|e| format!("stream_pgn: {e}"))?;
    eprintln!(
        "corpus: PGN ingest done — games_seen={} emitted={} parse_failures={} \
         no_result_dropped={} next_id={}",
        stats.games_seen,
        stats.games_emitted,
        stats.parse_failures,
        stats.no_result_dropped,
        next_id
    );
    Ok(ExitCode::SUCCESS)
}

// ─── fetch (M6.H — on-demand network ingestion) ─────────────────────────────

#[cfg(feature = "corpus-fetch")]
fn cmd_fetch(argv: &[String]) -> Result<ExitCode, String> {
    use clawfish::corpus::fetch::{FetchConfig, catalog, stream_to_ingest};

    let args = parse_args(argv);
    if args.has("help") {
        eprintln!(
            "corpus fetch --source ccrl|lichess --out <dir> [--target-positions N]
                   [--url <url>] [--lichess-month YYYY-MM]

Stream a compressed PGN dump over HTTPS, decompress, apply
`GameFilter::default()`, and append each accepted game as one CRC-framed
block in <out>/pgn-shard.bin (the same path/discipline as `ingest-pgn`).
Robust for unattended runs: in-RAM HTTP Range resume, a games-parsed stall
watchdog, infinite exponential backoff, and content-validity gates.

Supported URL extensions: .pgn.zst (streamed), .zip, .7z (downloaded to a
temp file then parsed locally).

--target-positions  Stop after this many NEW positions are appended (default:
                    unbounded — runs to end-of-stream).
--url               Explicit dump URL. Lichess auto-constructs from
                    --lichess-month (default a pinned month) when omitted; CCRL
                    REQUIRES --url to an official CCRL .7z (the count-bearing
                    filename isn't auto-constructible) — see docs/data-catalog.md."
        );
        return Ok(ExitCode::SUCCESS);
    }

    let source = match args.require("source")? {
        "ccrl" => Source::Ccrl,
        "lichess" => Source::LichessOpen,
        other => {
            return Err(format!(
                "--source must be `ccrl` or `lichess`, got {other:?}"
            ));
        }
    };
    let out = PathBuf::from(args.require("out")?);
    let target = args.parse_u64("target-positions", u64::MAX)?;

    let lichess_month = match args.get("lichess-month") {
        Some(s) => Some(parse_year_month(s)?),
        None => None,
    };
    let url = match args.get("url") {
        Some(u) => u.to_string(),
        None => catalog::default_url(source, lichess_month).ok_or_else(|| {
            "this source has no auto-default URL — pass --url to a CCRL archive \
             (.7z/.zip/.pgn.zst). See docs/data-catalog.md"
                .to_string()
        })?,
    };

    let stop = Arc::new(AtomicBool::new(false));
    install_signal_handler(stop.clone());
    let filter = GameFilter::default();
    let cfg = FetchConfig::default();

    eprintln!("corpus: fetch {source:?} target={target} url={url}");
    let outcome = stream_to_ingest(source, &url, target, &out, &filter, &stop, &cfg)
        .map_err(|e| format!("fetch: {e}"))?;
    eprintln!(
        "corpus: fetch done — positions={} games={} bytes_received={} terminated={:?} next_id={}",
        outcome.positions_ingested,
        outcome.games_emitted,
        outcome.bytes_received,
        outcome.terminated,
        outcome.next_game_id,
    );
    Ok(ExitCode::SUCCESS)
}

/// Parse a `YYYY-MM` token into `(year, month)` with a 1..=12 month guard.
#[cfg(feature = "corpus-fetch")]
fn parse_year_month(s: &str) -> Result<(u32, u32), String> {
    let (y, m) = s
        .split_once('-')
        .ok_or_else(|| format!("--lichess-month must be YYYY-MM, got {s:?}"))?;
    let year: u32 = y.parse().map_err(|_| format!("bad year in {s:?}"))?;
    let month: u32 = m.parse().map_err(|_| format!("bad month in {s:?}"))?;
    if !(1..=12).contains(&month) {
        return Err(format!("month out of range in {s:?}"));
    }
    Ok((year, month))
}

/// `corpus-fetch`-feature-gated: without it, the network stack isn't linked.
#[cfg(not(feature = "corpus-fetch"))]
fn cmd_fetch(_argv: &[String]) -> Result<ExitCode, String> {
    Err(
        "`corpus fetch` needs the `corpus-fetch` build feature; rebuild with \
         `cargo build --release --features corpus-fetch`"
            .to_string(),
    )
}

// ─── build ─────────────────────────────────────────────────────────────────

fn cmd_build(argv: &[String]) -> Result<ExitCode, String> {
    let args = parse_args(argv);
    if args.has("help") {
        eprintln!(
            "corpus build --in <dir> [--out <dir>] [--val-fraction <f>] \\
                [--split-seed <u64>] [--max-mem-mb <N>] [--max-spill-gb <N>]

Re-scan the input shard logs (self-play + PGN), apply the pinned
filter→quiet→strata pipeline, run dedup→cap→split, and emit the frozen
corpus to <out>/train.bin + <out>/val.bin + manifest.json + filter_spec.txt
+ corpus_stats.txt. Default --out = --in."
        );
        return Ok(ExitCode::SUCCESS);
    }
    let in_dir = PathBuf::from(args.require("in")?);
    let out = match args.get("out") {
        Some(s) => PathBuf::from(s),
        None => in_dir.clone(),
    };
    fs::create_dir_all(&out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let val_fraction = args.parse_f64("val-fraction", 0.1)?;
    let split_seed = args.parse_u64("split-seed", 7)?;
    let max_mem_mb = args.parse_u64("max-mem-mb", 512)?;
    let max_spill_gb = args.parse_u64("max-spill-gb", 8)?;

    // Load the pre-existing manifest's metadata (depth ladder, self-play
    // seed) if available — `build` does not overwrite the calibration.
    let existing_manifest = read_manifest(&in_dir).ok();

    // 1) Read all shards into memory (R6 streaming would chunk this; the
    //    landing-budget corpus is small enough to fit, and the dedup spill
    //    handles the rest).
    let mut all: Vec<CorpusRecord> = Vec::new();
    for name in ["shard.bin", "pgn-shard.bin"] {
        let p = in_dir.join(name);
        if !p.exists() {
            continue;
        }
        let (blocks, _) =
            scan_valid_blocks(&p).map_err(|e| format!("scan {}: {e}", p.display()))?;
        for b in blocks {
            for r in b.records {
                all.push(r);
            }
        }
    }
    eprintln!(
        "corpus: loaded {} records (already quiet-certified by selfplay)",
        all.len()
    );

    // The shard contains quiet-certified, strata-tagged records emitted by
    // selfplay's inline filter (ply ≥ OPENING_SKIP_PLIES ∧ !in_check ∧
    // |eval| ≤ HIGH_SCORE_CP ∧ quiet predicate, with `strata_for` already
    // applied). Build only does the cross-game work: dedup → per-game
    // cap → game-level train/val split. For ingested-PGN records (when
    // multi-source ingest grows beyond self-play), the build pass should
    // re-apply per-position filtering — left as a follow-up when an
    // operator actually runs `ingest-pgn`. The committed self-play-only
    // path is fully covered by the inline filter.
    let admitted = all;

    // 3) Dedup → per-game cap → game-level split.
    let spill_dir = std::env::temp_dir().join("clawfish-corpus-spill");
    let after_dedup = dedup_fen(admitted.into_iter(), &spill_dir, max_mem_mb, max_spill_gb)
        .map_err(|e| format!("dedup_fen: {e}"))?;
    eprintln!("corpus: after dedup_fen = {} records", after_dedup.len());
    let after_cap = per_game_cap(after_dedup, PER_GAME_CAP, split_seed);
    eprintln!(
        "corpus: after per_game_cap(cap={PER_GAME_CAP}) = {} records",
        after_cap.len()
    );
    let split = split_by_game(after_cap, val_fraction, split_seed);
    let train_n = split.train.len();
    let val_n = split.val.len();
    eprintln!("corpus: split → train={train_n} val={val_n}");

    // 4) Emit the frozen shards. Group by game_id so the on-disk frame is
    //    one block per game (the §3.6 contract). When the val split is
    //    empty (small corpus + low val_fraction) we still emit a zero-byte
    //    val.bin so downstream tooling can scan it unconditionally — the
    //    `scan_valid_blocks` path treats a missing file as empty too.
    let train_path = out.join("train.bin");
    let val_path = out.join("val.bin");
    let _ = fs::remove_file(&train_path);
    let _ = fs::remove_file(&val_path);
    emit_records_per_game(&train_path, &split.train)?;
    emit_records_per_game(&val_path, &split.val)?;
    if !val_path.exists() {
        fs::write(&val_path, b"").map_err(|e| format!("create empty val.bin: {e}"))?;
    }
    if !train_path.exists() {
        fs::write(&train_path, b"").map_err(|e| format!("create empty train.bin: {e}"))?;
    }

    // 5) Compute the frozen-corpus SHA-256 = sha256(train ‖ val).
    let mut bytes =
        fs::read(&train_path).map_err(|e| format!("read {}: {e}", train_path.display()))?;
    let val_bytes = fs::read(&val_path).map_err(|e| format!("read {}: {e}", val_path.display()))?;
    bytes.extend_from_slice(&val_bytes);
    let corpus_digest = hex_digest(&sha256_bytes(&bytes));

    // 6) Re-emit manifest.json with the post-build totals + digest. Every
    //    reproducibility knob from `corpus selfplay` (recorded into the
    //    manifest there) is carried through verbatim so `re-run.sh` reads
    //    them all from one place.
    let manifest = Manifest {
        schema_version: 1,
        created_at: timestamp(),
        engine_commit: engine_commit(),
        total_positions: (train_n + val_n) as u64,
        validation_fraction: val_fraction,
        sources: existing_manifest
            .as_ref()
            .map(|m| m.sources.clone())
            .unwrap_or_default(),
        self_play_seed: existing_manifest
            .as_ref()
            .map(|m| m.self_play_seed)
            .unwrap_or(0),
        games: existing_manifest.as_ref().map(|m| m.games).unwrap_or(0),
        max_plies: existing_manifest.as_ref().map(|m| m.max_plies).unwrap_or(0),
        opening_random_plies: existing_manifest
            .as_ref()
            .map(|m| m.opening_random_plies)
            .unwrap_or(0),
        workers: existing_manifest.as_ref().map(|m| m.workers).unwrap_or(0),
        split_seed,
        val_fraction,
        depth_ladder: existing_manifest
            .as_ref()
            .map(|m| m.depth_ladder.clone())
            .unwrap_or_else(|| vec![(4, 1)]),
        opening_book_path: existing_manifest
            .as_ref()
            .and_then(|m| m.opening_book_path.clone()),
        opening_book_sha256: existing_manifest
            .as_ref()
            .and_then(|m| m.opening_book_sha256.clone()),
        opening_mode: existing_manifest
            .as_ref()
            .and_then(|m| m.opening_mode.clone()),
        corpus_sha256: corpus_digest.clone(),
    };
    write_manifest(&out, &manifest).map_err(|e| format!("write_manifest: {e}"))?;
    write_filter_spec(&out).map_err(|e| format!("write_filter_spec: {e}"))?;

    // Stats file — recorded knobs the gate / a reviewer can inspect.
    let mut stats = String::new();
    stats.push_str(&format!("train_records={train_n}\n"));
    stats.push_str(&format!("val_records={val_n}\n"));
    stats.push_str(&format!("val_fraction={val_fraction}\n"));
    stats.push_str(&format!("split_seed={split_seed}\n"));
    stats.push_str(&format!("corpus_sha256={corpus_digest}\n"));
    // Per-position filter stats moved to selfplay (inline filter); the
    // build pass no longer drops records by position-level predicates.
    stats.push_str(&format!("QUIET_MARGIN_CP={QUIET_MARGIN_CP}\n"));
    atomic_write(&out.join("corpus_stats.txt"), stats.as_bytes())
        .map_err(|e| format!("write corpus_stats: {e}"))?;

    // Promote train.bin → shard.bin (the gate's `Layout::discover` reads
    // `shard.bin` + `val.bin`). `fs::rename` overwrites the existing
    // shard.bin atomically on POSIX — that is exactly what we want here:
    // the raw input shard is replaced by the filtered/dedup'd output.
    let _ = fs::remove_file(out.join("shard.bin"));
    fs::rename(&train_path, out.join("shard.bin"))
        .map_err(|e| format!("rename train.bin → shard.bin: {e}"))?;

    // Write the re-run.sh template + its sister scripts (R5 staging).
    write_rerun_script(&out)?;

    eprintln!(
        "corpus: build complete — total={}, sha256={}",
        train_n + val_n,
        corpus_digest
    );
    Ok(ExitCode::SUCCESS)
}

fn emit_records_per_game(path: &Path, records: &[CorpusRecord]) -> Result<(), String> {
    // Group by game_id (BTreeMap iteration is sorted ⇒ deterministic block
    // order) and sort each game's records by ply. The on-disk shard is then
    // a pure function of the input multiset, independent of worker
    // scheduling — `corpus_sha256` reproduces regardless of `--workers`.
    let mut by_game: BTreeMap<u64, Vec<CorpusRecord>> = BTreeMap::new();
    for r in records {
        by_game.entry(r.game_id).or_default().push(r.clone());
    }
    for (gid, recs) in &mut by_game {
        recs.sort_by(|a, b| a.ply.cmp(&b.ply).then_with(|| a.fen.cmp(&b.fen)));
        append_block(path, *gid, recs).map_err(|e| format!("append_block {gid}: {e}"))?;
    }
    Ok(())
}

fn write_rerun_script(dir: &Path) -> Result<(), String> {
    let body = r#"#!/usr/bin/env bash
# bench/corpus/re-run.sh — operator script to re-derive the corpus bytes.
# Generated by `corpus build`.
#
# Stages:
#   1) (External) stage raw CCRL/Lichess PGNs into target/corpus-raw/ if any
#      are listed in manifest.json. Re-derivability is the weaker contract
#      for those slices (research §1 / plan §1) — re-download then ingest.
#   2) corpus selfplay  — deterministic given (seed, depth_ladder, binary).
#   3) corpus ingest-pgn — once per PGN listed in the manifest.
#   4) corpus build      — apply filter→quiet→dedup→cap→split → frozen bytes.
#   5) corpus quality-gate — must PASS.
#
# Every reproducibility knob is READ FROM manifest.json — no shell-default
# substitution for a knob that is pinned in the manifest. The vendored
# bytes were produced with these exact knobs; re-running with different
# knobs (different seed, games, workers, val_fraction, ...) produces a
# DIFFERENT corpus, not the frozen one.
#
# Tooling: this re-run script's no-network on-disk staging path (operator
# extracts archives, then `corpus ingest-pgn`) may use system `zstd`/`7z`.
# `corpus fetch` itself needs NO system tools — it decompresses .pgn.zst/.zip/.7z
# in-process via Rust deps (zstd / zip / sevenz-rust2, behind `corpus-fetch`).

set -eu

DIR="$(cd "$(dirname "$0")" && pwd)"
BIN="${CORPUS_BIN:-corpus}"

# `manifest_field <name>` extracts a JSON number/string value for a top-level
# scalar field. Trims whitespace, comma, and quote chars. Good enough for the
# small set of pinned knobs (no nested-object collisions because every
# matched key is a top-level scalar — `sources`/`depth_ladder` are arrays
# which we don't read this way).
manifest_field() {
    grep -E "\"$1\"" "$DIR/manifest.json" | head -1 | tr -d ' ,"' | cut -d: -f2
}

SEED="$(manifest_field self_play_seed)"
GAMES="$(manifest_field games)"
MAX_PLIES="$(manifest_field max_plies)"
OPENING_RANDOM_PLIES="$(manifest_field opening_random_plies)"
WORKERS="$(manifest_field workers)"
SPLIT_SEED="$(manifest_field split_seed)"
VAL_FRACTION="$(manifest_field val_fraction)"
OPENING_BOOK="$(manifest_field opening_book_path)"
OPENING_MODE="$(manifest_field opening_mode)"

# Default to `random` when the manifest does not pin a mode — older
# manifests pre-date the on-book / off-book taxonomy and only ever ran
# the random regime.
if [ -z "$OPENING_MODE" ]; then
    OPENING_MODE=random
fi

echo "corpus re-run: using binary '$BIN' against $DIR"
echo "corpus re-run: seed=$SEED games=$GAMES workers=$WORKERS \
max_plies=$MAX_PLIES opening_random_plies=$OPENING_RANDOM_PLIES \
split_seed=$SPLIT_SEED val_fraction=$VAL_FRACTION \
opening_mode=$OPENING_MODE opening_book=${OPENING_BOOK:-<none>}"

# (1) external staging: read manifest.sources and re-download each.
# (Left as an operator step — the manifest pins URLs + SHA-256.)

# (2) self-play. The manifest's opening_mode pins which regime — book
#     campaigns require an opening book; random campaigns ignore it.
if [ "$OPENING_MODE" = "book" ]; then
    "$BIN" selfplay --seed "$SEED" --games "$GAMES" --workers "$WORKERS" \
        --max-plies "$MAX_PLIES" --opening-random-plies "$OPENING_RANDOM_PLIES" \
        --val-fraction "$VAL_FRACTION" \
        --opening-mode book --opening-book "$OPENING_BOOK" \
        --out "$DIR"
else
    "$BIN" selfplay --seed "$SEED" --games "$GAMES" --workers "$WORKERS" \
        --max-plies "$MAX_PLIES" --opening-random-plies "$OPENING_RANDOM_PLIES" \
        --val-fraction "$VAL_FRACTION" \
        --opening-mode random \
        --out "$DIR"
fi

# (3) Each PGN source: corpus ingest-pgn …
# (4) Build the frozen corpus.
"$BIN" build --in "$DIR" --split-seed "$SPLIT_SEED" --val-fraction "$VAL_FRACTION"

# (5) Run the data-quality gate (must PASS).
"$BIN" quality-gate --dir "$DIR"
"#;
    let path = dir.join("re-run.sh");
    fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
    // Mark executable on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(&path)
            .map_err(|e| format!("stat re-run.sh: {e}"))?
            .permissions();
        perm.set_mode(0o755);
        fs::set_permissions(&path, perm).map_err(|e| format!("chmod re-run.sh: {e}"))?;
    }
    Ok(())
}

fn timestamp() -> String {
    // ISO-8601 UTC. We approximate using SystemTime::now -> seconds.
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(std::time::Duration::ZERO);
    let secs = dur.as_secs() as i64;
    let (year, month, day, hour, min, sec) = unix_to_ymd(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// Crude Unix→YMD conversion (no chrono dep). Good for a manifest timestamp.
fn unix_to_ymd(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let hour = (time_of_day / 3600) as u32;
    let min = ((time_of_day % 3600) / 60) as u32;
    let sec = (time_of_day % 60) as u32;
    // Howard Hinnant's date algorithms (public-domain).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i32) + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, hour, min, sec)
}

fn engine_commit() -> String {
    // Best-effort: read CLAWFISH_COMMIT env (set by scripts/corpus.sh) or
    // fall back to "unknown". Avoids running `git rev-parse` from a Rust
    // process (dependency-hygiene-clean; the operator script can stamp it).
    std::env::var("CLAWFISH_COMMIT").unwrap_or_else(|_| "unknown".into())
}

// ─── quality-gate ──────────────────────────────────────────────────────────

fn cmd_quality_gate(argv: &[String]) -> Result<ExitCode, String> {
    let args = parse_args(argv);
    if args.has("help") {
        eprintln!(
            "corpus quality-gate --dir <dir>

Run the six data-quality checks against the frozen corpus at <dir>.
Exit 0 iff the gate passes (the three must-PASS checks all PASSed)."
        );
        return Ok(ExitCode::SUCCESS);
    }
    let dir = PathBuf::from(args.require("dir")?);
    let report = run_quality_gate(&dir).map_err(|e| format!("run_quality_gate: {e}"))?;
    print_report(&report);
    if report.gate_passed() {
        eprintln!("corpus: data-quality gate PASS");
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!("corpus: data-quality gate FAIL");
        Ok(ExitCode::FAILURE)
    }
}

fn print_report(report: &QualityReport) {
    println!("name                                must  passed  detail");
    println!("----                                ----  ------  ------");
    for c in &report.checks {
        let mp = if c.must_pass { "YES" } else { " no" };
        let p = if c.passed { "PASS" } else { "FAIL" };
        println!("{:36} {mp:4}  {p:6}  {}", c.name, c.detail);
    }
}

// ─── export ────────────────────────────────────────────────────────────────

fn cmd_export(argv: &[String]) -> Result<ExitCode, String> {
    let args = parse_args(argv);
    if args.has("help") {
        eprintln!(
            "corpus export --in <dir>

Convert the on-disk binary shard log to TSV on stdout. Columns:
fen\\tlabel\\tsource\\tgame_id\\tply\\tdepth_rung\\tstrata.

Reads <dir>/shard.bin + <dir>/val.bin + <dir>/pgn-shard.bin if present."
        );
        return Ok(ExitCode::SUCCESS);
    }
    let in_dir = PathBuf::from(args.require("in")?);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "fen\tlabel\tsource\tgame_id\tply\tdepth_rung\tstrata")
        .map_err(|e| format!("write tsv header: {e}"))?;
    let mut total = 0u64;
    for name in ["shard.bin", "val.bin", "pgn-shard.bin"] {
        let p = in_dir.join(name);
        if !p.exists() {
            continue;
        }
        let (blocks, _) =
            scan_valid_blocks(&p).map_err(|e| format!("scan {}: {e}", p.display()))?;
        for b in blocks {
            total += dump_block_tsv(&mut out, &b)?;
        }
    }
    eprintln!("corpus: exported {total} records as TSV");
    Ok(ExitCode::SUCCESS)
}

fn dump_block_tsv(out: &mut impl Write, block: &GameBlock) -> Result<u64, String> {
    let mut n = 0u64;
    for r in &block.records {
        writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            r.fen,
            label_name(r.label),
            source_name(r.source),
            r.game_id,
            r.ply,
            r.depth_rung,
            r.strata
        )
        .map_err(|e| format!("write tsv row: {e}"))?;
        n += 1;
    }
    Ok(n)
}

fn label_name(l: Label) -> &'static str {
    match l {
        Label::WhiteWin => "1-0",
        Label::Draw => "1/2-1/2",
        Label::BlackWin => "0-1",
    }
}

fn source_name(s: Source) -> &'static str {
    match s {
        Source::SelfPlayOnBook => "selfplay_book",
        Source::SelfPlayOffBook => "selfplay_random",
        Source::Ccrl => "ccrl",
        Source::LichessOpen => "lichess",
    }
}

// ─── rerun ─────────────────────────────────────────────────────────────────

fn cmd_rerun(argv: &[String]) -> Result<ExitCode, String> {
    let args = parse_args(argv);
    if args.has("help") {
        eprintln!(
            "corpus rerun --manifest <path>

Convenience wrapper: re-execute the stage→build→quality-gate flow
documented in `bench/corpus/re-run.sh`. The vendored script is the
operator-facing artifact; this subcommand simply prints a pointer
to that script so a re-run is always traceable to a committed file."
        );
        return Ok(ExitCode::SUCCESS);
    }
    let m = args.require("manifest")?;
    let p = Path::new(m);
    if !p.exists() {
        return Err(format!("--manifest {}: not found", p.display()));
    }
    let dir = p.parent().unwrap_or(Path::new("."));
    let script = dir.join("re-run.sh");
    if script.exists() {
        eprintln!(
            "corpus: re-run script is at {} — invoke it directly:\n  sh {}",
            script.display(),
            script.display()
        );
        Ok(ExitCode::SUCCESS)
    } else {
        Err(format!(
            "no re-run.sh found next to {}; re-run `corpus build` to regenerate",
            p.display()
        ))
    }
}

// ─── truncate ──────────────────────────────────────────────────────────────

fn cmd_truncate(argv: &[String]) -> Result<ExitCode, String> {
    let args = parse_args(argv);
    if args.has("help") {
        eprintln!(
            "corpus truncate --out <dir> --cap-positions <N>

Truncate `shard.bin` and `val.bin` in <dir> to exactly N positions
(or as close as the block sizes allow). The largest game_id prefix
whose total position count is ≤ N is kept whole; if N falls
mid-game, the boundary game's block is re-encoded with only the
first (N − cumulative) records and appended in place. The resulting
corpus has exactly N positions (or, on an edge case where the cap
lands precisely on a game boundary, exactly N positions with no
partial game). All retained blocks remain CRC-framed and decodable.

`pending/` is left untouched (the operator typically `rm -rf`s it
before truncating to ensure no in-flight games persist into the
truncated artifact)."
        );
        return Ok(ExitCode::SUCCESS);
    }
    let out = PathBuf::from(args.require("out")?);
    let cap = args.parse_u64("cap-positions", 0)?;
    if cap == 0 {
        return Err("--cap-positions must be > 0".into());
    }

    // Collect game blocks from both shard.bin and val.bin, sort by
    // game_id, find the largest whole-game prefix whose position count ≤
    // cap, and (if cap falls mid-game) re-encode the boundary block with
    // only the leading (cap − cumulative) records. Both files are
    // append-only CRC-framed per game; we truncate at the boundary then
    // append the partial block, if any.
    let shard_path = out.join("shard.bin");
    let val_path = out.join("val.bin");
    // Scan-with-offsets: end-of-block byte position is taken directly from
    // the scan, not reconstructed by re-encoding (the encoder is
    // deterministic and round-trips perfectly, but using the scan offsets
    // directly avoids any test-coverage gap on the round-trip and matches
    // the truncate's "truncate at on-disk boundary" contract literally).
    let (shard_blocks_off, shard_valid_len) =
        clawfish::corpus::store::scan_valid_blocks_with_offsets(&shard_path)
            .map_err(|e| format!("scan shard.bin: {e}"))?;
    let (val_blocks_off, val_valid_len) =
        clawfish::corpus::store::scan_valid_blocks_with_offsets(&val_path)
            .map_err(|e| format!("scan val.bin: {e}"))?;
    // Borrow the blocks separately (without consuming the offsets) so the
    // boundary partial-game re-encode below can access block.records.
    let shard_blocks: Vec<&GameBlock> = shard_blocks_off.iter().map(|(b, _)| b).collect();
    let val_blocks: Vec<&GameBlock> = val_blocks_off.iter().map(|(b, _)| b).collect();
    let shard_end_offsets: Vec<u64> = shard_blocks_off.iter().map(|(_, o)| *o).collect();
    let val_end_offsets: Vec<u64> = val_blocks_off.iter().map(|(_, o)| *o).collect();

    // Build (game_id, file_tag, idx_into_file_blocks, position_count)
    // records. idx is the position of the block within its file's
    // shard_blocks/val_blocks; used for the boundary block's records
    // vector and for the on-disk end-offset lookup.
    #[derive(Clone, Copy)]
    enum FileTag {
        Shard,
        Val,
    }
    let mut tagged: Vec<(u64, FileTag, usize, u64)> = Vec::new();
    for (i, b) in shard_blocks.iter().enumerate() {
        tagged.push((b.game_id, FileTag::Shard, i, b.records.len() as u64));
    }
    for (i, b) in val_blocks.iter().enumerate() {
        tagged.push((b.game_id, FileTag::Val, i, b.records.len() as u64));
    }
    tagged.sort_by_key(|t| t.0);

    let total_positions_before: u64 = tagged.iter().map(|t| t.3).sum();
    eprintln!(
        "corpus: before truncate: shard.bin={} bytes ({} games), val.bin={} bytes ({} games), \
         total positions={}",
        shard_valid_len,
        shard_blocks.len(),
        val_valid_len,
        val_blocks.len(),
        total_positions_before
    );

    // Walk blocks in sorted game_id order. For each block:
    //   - if cum + n_positions ≤ cap: keep whole block; advance the
    //     respective file's keep_until offset to this block's end.
    //   - else: this is the boundary block. Stop the walk; remember its
    //     (file, idx, room = cap − cum) so we can re-encode the partial.
    let mut cum: u64 = 0;
    let mut shard_keep_until: u64 = 0;
    let mut val_keep_until: u64 = 0;
    let mut games_kept: u64 = 0;
    let mut partial: Option<(FileTag, usize, u64)> = None; // (file, block_idx, room)
    for (_game_id, tag, idx, n_positions) in &tagged {
        let next = cum + *n_positions;
        if next > cap {
            let room = cap - cum;
            if room > 0 {
                partial = Some((*tag, *idx, room));
            }
            break;
        }
        cum = next;
        games_kept += 1;
        let end_off = match tag {
            FileTag::Shard => shard_end_offsets[*idx],
            FileTag::Val => val_end_offsets[*idx],
        };
        match tag {
            FileTag::Shard => shard_keep_until = end_off,
            FileTag::Val => val_keep_until = end_off,
        }
    }

    let partial_positions = partial.as_ref().map(|p| p.2).unwrap_or(0);
    eprintln!(
        "corpus: truncate target: keep {} whole games + {} partial-game records = {} positions (cap {})",
        games_kept,
        partial_positions,
        cum + partial_positions,
        cap
    );

    // Whole-game truncation first. If a file isn't getting a partial
    // appended below, we set its length to keep_until directly.
    let truncate_file =
        |path: &Path, current_len: u64, new_len: u64, label: &str| -> Result<(), String> {
            if new_len < current_len {
                let f = fs::OpenOptions::new()
                    .write(true)
                    .open(path)
                    .map_err(|e| format!("open {label}: {e}"))?;
                f.set_len(new_len)
                    .map_err(|e| format!("truncate {label}: {e}"))?;
                f.sync_all().map_err(|e| format!("fsync {label}: {e}"))?;
                eprintln!(
                    "corpus: {label} truncated {} → {} bytes",
                    current_len, new_len
                );
            }
            Ok(())
        };
    truncate_file(&shard_path, shard_valid_len, shard_keep_until, "shard.bin")?;
    truncate_file(&val_path, val_valid_len, val_keep_until, "val.bin")?;

    // Partial-game block: re-encode boundary block with first `room`
    // records and append to its file. The block's game_id is preserved
    // (so the dispatcher's gap-list logic sees it as already-played and
    // doesn't re-dispatch it on resume).
    if let Some((tag, idx, room)) = partial {
        let (boundary_block, dest_path, label) = match tag {
            FileTag::Shard => (&shard_blocks[idx], &shard_path, "shard.bin"),
            FileTag::Val => (&val_blocks[idx], &val_path, "val.bin"),
        };
        let mut records = boundary_block.records.clone();
        records.truncate(room as usize);
        let partial_bytes = clawfish::corpus::store::encode_block(boundary_block.game_id, &records);
        clawfish::corpus::store::append_raw_bytes(dest_path, &partial_bytes)
            .map_err(|e| format!("append partial block to {label}: {e}"))?;
        eprintln!(
            "corpus: {label} partial-game appended: game_id={} kept {}/{} records, {} bytes",
            boundary_block.game_id,
            room,
            boundary_block.records.len(),
            partial_bytes.len()
        );
    }

    // Reset the checkpoint so a future selfplay run resumes coherently
    // with the truncated state (next_consume_id matches what survived).
    // We delete it; the resume protocol will fall back to "scan shard +
    // val, derive next_consume_id from max committed_id + 1".
    let ckpt = out.join("checkpoint.bin");
    if ckpt.exists() {
        let _ = fs::remove_file(&ckpt);
        eprintln!("corpus: checkpoint.bin removed (resume will rebuild from shards)");
    }

    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawfish::corpus::store::append_block;
    use clawfish::corpus::{DEPTH_RUNG_EXTERNAL, Label};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static CTR: AtomicU64 = AtomicU64::new(0);
            let n = CTR.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let dir =
                std::env::temp_dir().join(format!("clawfish-corpus-bin-test-{tag}-{pid}-{n}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn rec(game_id: u64, ply: u32) -> CorpusRecord {
        CorpusRecord {
            fen: "8/8/8/8/8/8/8/k6K w - - 0 1".into(),
            label: Label::Draw,
            source: Source::Ccrl,
            game_id,
            ply,
            depth_rung: DEPTH_RUNG_EXTERNAL,
            strata: 0,
        }
    }

    #[test]
    fn max_existing_game_id_missing_dir_is_zero() {
        let td = TempDir::new("missing");
        assert_eq!(max_existing_game_id(td.path()), 0);
    }

    #[test]
    fn max_existing_game_id_picks_max_across_both_shards() {
        // Self-play shard.bin has game_ids up to 42; pgn-shard.bin has
        // game_ids up to 100. `max_existing_game_id` must return 100, so a
        // subsequent `ingest-pgn` starts at 101 (no collision with the
        // prior PGN ingest's range).
        let td = TempDir::new("both-shards");
        for gid in [10u64, 25, 42] {
            append_block(&td.path().join("shard.bin"), gid, &[rec(gid, 0)]).unwrap();
        }
        for gid in [50u64, 75, 100] {
            append_block(&td.path().join("pgn-shard.bin"), gid, &[rec(gid, 0)]).unwrap();
        }
        assert_eq!(
            max_existing_game_id(td.path()),
            100,
            "must scan pgn-shard.bin too — a 2nd ingest-pgn collides without this"
        );
    }

    #[test]
    fn cmd_ingest_pgn_base_id_includes_existing_pgn_shard() {
        // Only pgn-shard.bin exists; max_existing_game_id reports the
        // max from THERE (this is the case the bug-fix targets — a 2nd
        // `ingest-pgn` invocation would re-use base_id 0 without this).
        let td = TempDir::new("pgn-only");
        for gid in [3u64, 7, 19] {
            append_block(&td.path().join("pgn-shard.bin"), gid, &[rec(gid, 0)]).unwrap();
        }
        assert_eq!(max_existing_game_id(td.path()), 19);
    }
}
