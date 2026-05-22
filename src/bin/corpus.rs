//! `corpus` — M6.G/M6.H2 corpus-lane-construction CLI driver.
//!
//! Subcommands: `calibrate-ladder`, `selfplay`, `ingest-pgn`, `fetch`,
//! `finalize`, `quality-gate`, `export`, `rerun`, `truncate`. R5/R6/R7:
//! streaming, all-cores workers, graceful SIGTERM/SIGINT, crash-safe via the
//! per-game append-block log. M6.H2: each lane is an independent generator
//! producing one flat build-ready `lane.bin` (no train/val split — that moves
//! to M6.I); the build-era `corpus build` transform is replaced by the thin
//! `corpus finalize` (manifest/stats/digest, no record transformation). See
//! `docs/plans/m6.h2-corpus-lanes.md` §1.1/§1.2/§5, `docs/plans/m6.g.md`.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use clawfish::corpus::filter::GameFilter;
use clawfish::corpus::manifest::{
    Manifest, hex_digest, read_manifest, sha256_bytes, write_filter_spec, write_manifest,
};
use clawfish::corpus::objective::strata_for;
use clawfish::corpus::pgn::{PgnStats, stream_pgn};
use clawfish::corpus::pipeline::LaneCommitter;
use clawfish::corpus::quality_gate::{QualityReport, run_quality_gate};
use clawfish::corpus::quiet::{is_quiet, static_eval_white};
use clawfish::corpus::selfplay::{
    OpeningMode, SelfPlayConfig, calibrate_ladder, run as selfplay_run,
};
use clawfish::corpus::store::{GameBlock, atomic_write, scan_valid_blocks};
use clawfish::corpus::{
    CorpusRecord, DEPTH_RUNG_EXTERNAL, HIGH_SCORE_CP, Label, OPENING_SKIP_PLIES, QUIET_MARGIN_CP,
    Source,
};
use clawfish::movegen::in_check;
use clawfish::search::QSearcher;

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
        "finalize" => cmd_finalize(rest),
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
        "corpus — M6.G/M6.H2 corpus-lane-construction CLI.

Usage: corpus <subcommand> [options]

Each lane is an INDEPENDENT generator producing one flat build-ready
`lane.bin` (per-lane FEN dedup + per-game cap applied inline; no train/val
split — that moves to M6.I).

Subcommands:
  calibrate-ladder   Empirically measure the R-TC depth ladder from `bench` movetime
                     buckets and write it to filter_spec.txt + manifest.json.
  selfplay           Run a deterministic fixed-depth self-play campaign.
                     Crash-safe (per-game append-block log + checkpoint); emits lane.bin.
  ingest-pgn         Stream-parse an on-disk CCRL/Lichess PGN through the full inline
                     pipeline (skip8 + quiet + dedup + cap) into lane.bin.
  fetch              Stream a CCRL/Lichess dump over HTTPS through the inline pipeline
                     into lane.bin (M6.H; needs the `corpus-fetch` build feature).
  finalize           Recompute corpus_sha256 over lane.bin; write/update manifest.json
                     + filter_spec.txt + corpus_stats.txt + re-run.sh. No record transform.
  quality-gate       Run the five data-quality checks; exit 0 iff the gate passes.
  export             Convert lane.bin to human-greppable TSV.
  rerun              Re-execute the generate→finalize→quality-gate flow from a manifest.
  truncate           Truncate lane.bin to ≤ --cap-positions on a per-game boundary;
                     resets checkpoint.bin for a coherent resume.

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
    // Append the ladder + the bucket→depth mapping.
    let mut spec = fs::read_to_string(out.join("filter_spec.txt"))
        .map_err(|e| format!("read filter_spec.txt: {e}"))?;
    spec.push_str("# R-TC depth ladder (corpus calibrate-ladder).\n");
    for ((depth, weight), bucket) in ladder.iter().zip(buckets.iter()) {
        spec.push_str(&format!("DEPTH_RUNG_{bucket}MS={depth},{weight}\n"));
    }
    fs::write(out.join("filter_spec.txt"), spec).map_err(|e| format!("write filter_spec: {e}"))?;

    // Write a minimal manifest stub (no corpus bytes yet — just records the
    // calibration so a subsequent `selfplay` can refer to it).
    let manifest = Manifest {
        schema_version: 2,
        created_at: timestamp(),
        engine_commit: engine_commit(),
        total_positions: 0,
        sources: Vec::new(),
        self_play_seed: 0,
        games: 0,
        max_plies: 0,
        opening_random_plies: 0,
        workers: 0,
        cap_seed: 0,
        source_url: None,
        target_usable: None,
        source: None,
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
                [--max-plies <N>] [--opening-random-plies <N>] \\
                [--opening-book <path>] [--cap-seed <u64>]

Run a deterministic fixed-depth self-play campaign. Uses the calibrated
depth ladder from <out>/manifest.json if present, else falls back to
depth 4. Crash-safe (R1/R2/R3). SIGTERM/SIGINT triggers a graceful
drop-in-flight + checkpoint flush. Emits one flat build-ready <out>/lane.bin
(per-lane FEN dedup + per-game cap applied inline; no train/val split — that
moves to M6.I).

--games is OPTIONAL: omit it for an unbounded campaign (kill with
Ctrl-C / SIGTERM when satisfied — R2 guarantees zero partial labels).
With --games <N>, the campaign caps at game N-1 (matching R1's
idempotent resume — repeated runs with the same seed and a larger
--games extend the corpus). The manifest records the ACTUAL durable
game count at exit, so re-run.sh reproduces what you have.

--cap-positions <N> counts USABLE (post quiet-filter → dedup → cap)
positions; the committer's exact truncation lands lane.bin on N exactly.
--cap-seed seeds the per-game reservoir cap (deterministic, K-independent).

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

    let cap_seed = args.parse_u64("cap-seed", 7)?;
    // --cap-positions <N>: graceful exit when durable USABLE positions reach N
    // (post quiet-filter → dedup → cap). Distinct from --games (a cap on
    // dispatched game_ids) — the operator's real target for Texel tuning is
    // corpus *size*, and usable-positions-per-game varies. Both caps can be
    // set; whichever fires first wins. The committer's exact truncation lands
    // lane.bin on N exactly.
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
        opening_mode,
        book: book.clone(),
        poll_interval_ms: None,
        cap_seed,
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
         workers={workers} depth_ladder={depth_ladder:?} cap_seed={cap_seed} \
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
    // lane once at end; if absent fall back to this run's increment.
    // `re-run.sh` reads this number from the manifest and passes it to
    // `corpus selfplay --games <N>` → resume + complete-to-N path that
    // byte-reproduces the durable lane.
    let games_durable = scan_valid_blocks(&out.join("lane.bin"))
        .map(|(blocks, _)| blocks.len() as u64)
        .unwrap_or(stats.games_committed);

    // Pin every self-play knob into the manifest so a subsequent `finalize` +
    // `quality-gate` records them in the frozen artifact (the R-TC
    // reproducibility contract — `re-run.sh` reads each from manifest.json,
    // no shell-default substitution for a pinned knob). The manifest may not
    // exist yet (no prior `calibrate-ladder`), so start from any existing one
    // or a stub and write it back unconditionally.
    let mut m = read_manifest(&out).unwrap_or_else(|_| stub_manifest(depth_ladder.clone()));
    m.self_play_seed = seed;
    m.games = games_durable;
    m.max_plies = max_plies;
    m.opening_random_plies = opening_random_plies;
    m.workers = workers as u32;
    m.cap_seed = cap_seed;
    m.target_usable = cap_positions;
    m.opening_book_path = opening_book_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    m.opening_book_sha256 = book.as_ref().map(|b| b.sha256().to_owned());
    m.opening_mode = Some(match opening_mode {
        OpeningMode::Book => "book".into(),
        OpeningMode::Random => "random".into(),
    });
    // Self-play lanes carry a source tag too (the four-source taxonomy) — it
    // mirrors the record `Source`, so re-run.sh and the audit can read one
    // canonical knob regardless of lane kind.
    m.source = Some(match opening_mode {
        OpeningMode::Book => "selfplay-on-book".into(),
        OpeningMode::Random => "selfplay-off-book".into(),
    });
    write_manifest(&out, &m).map_err(|e| format!("update selfplay knobs: {e}"))?;
    Ok(ExitCode::SUCCESS)
}

/// A fresh schema-v2 manifest stub (no corpus bytes; digest left for
/// `finalize`). Used by `selfplay`/`fetch`/`ingest-pgn` when no prior manifest
/// exists in the lane dir.
fn stub_manifest(depth_ladder: Vec<(u8, u32)>) -> Manifest {
    Manifest {
        schema_version: 2,
        created_at: timestamp(),
        engine_commit: engine_commit(),
        total_positions: 0,
        sources: Vec::new(),
        self_play_seed: 0,
        games: 0,
        max_plies: 0,
        opening_random_plies: 0,
        workers: 0,
        cap_seed: 0,
        source_url: None,
        target_usable: None,
        source: None,
        depth_ladder,
        opening_book_path: None,
        opening_book_sha256: None,
        opening_mode: None,
        corpus_sha256: clawfish::corpus::quality_gate::CORPUS_SHA256_PENDING_SENTINEL.to_string(),
    }
}

fn num_workers_default() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Largest `game_id` present in this lane's `lane.bin` that an `ingest-pgn`
/// invocation could collide with (M6.H2: each lane lives in its own dir, so its
/// `game_id` space is private — only `lane.bin` is scanned). A missing file
/// contributes 0, never an error (`scan_valid_blocks` returns `Ok((vec![], 0))`
/// for a non-existent path).
fn max_existing_game_id(dir: &Path) -> u64 {
    let mut hi: u64 = 0;
    if let Ok((blocks, _)) = scan_valid_blocks(&dir.join("lane.bin"))
        && let Some(m) = blocks.iter().map(|b| b.game_id).max()
    {
        hi = hi.max(m);
    }
    hi
}

// ─── ingest-pgn ────────────────────────────────────────────────────────────

fn cmd_ingest_pgn(argv: &[String]) -> Result<ExitCode, String> {
    let args = parse_args(argv);
    if args.has("help") {
        eprintln!(
            "corpus ingest-pgn --path <pgn> --out <dir> [--source ccrl|lichess] \\
                [--target-positions N] [--cap-seed <u64>]

Stream-parse a local CCRL or Lichess PGN through the FULL inline pipeline —
game-level admission (`GameFilter`), then per-position skip8 ∧ !in_check ∧
|static_eval| ≤ HIGH_SCORE_CP ∧ is_quiet — then per-lane FEN dedup → per-game
cap → exact target via the shared LaneCommitter, appending each surviving game
as one CRC-framed block to <out>/lane.bin (the same build-ready discipline as
`corpus fetch`). The `--source` tag determines the provenance recorded on every
emitted record.

--target-positions  Stop after this many NEW usable positions are appended
                    (default: unbounded — ingest the whole file).
--cap-seed          Per-game reservoir-cap seed (default 0)."
        );
        return Ok(ExitCode::SUCCESS);
    }
    let path = PathBuf::from(args.require("path")?);
    let out = PathBuf::from(args.require("out")?);
    fs::create_dir_all(&out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let source_tag = args.get("source").unwrap_or("ccrl").to_string();
    let source = match source_tag.as_str() {
        "ccrl" => Source::Ccrl,
        "lichess" => Source::LichessOpen,
        other => {
            return Err(format!(
                "--source must be `ccrl` or `lichess`, got {other:?}"
            ));
        }
    };
    let target_positions = args.parse_u64("target-positions", u64::MAX)?;
    let cap_seed = args.parse_u64("cap-seed", 0)?;
    // CCRL has no `[TimeControl]` tag (its TC is in `[Event]`), so it needs the
    // TC-relaxed filter; Lichess uses the default (TC-required) band filter.
    let filter = match source {
        Source::Ccrl => clawfish::corpus::filter::ccrl_filter(),
        _ => GameFilter::default(),
    };

    let f = fs::File::open(&path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let r = BufReader::new(f);
    let lane = out.join("lane.bin");

    // Reserve a game-id range strictly above any existing game-id in this
    // lane's lane.bin so re-runs / multiple ingests never collide. A one-shot
    // LaneCommitter (single scan via `resume`) owns dedup → cap → exact target;
    // a per-call QSearcher backs the inline quiet check (single-threaded).
    let base_id = max_existing_game_id(&out) + 1;
    let target = (target_positions != u64::MAX).then_some(target_positions);
    let (mut committer, _committed_ids) =
        LaneCommitter::resume(&lane, cap_seed, target).map_err(|e| format!("resume lane: {e}"))?;
    let mut qsearcher = QSearcher::new();

    let mut stats = PgnStats::default();
    let mut usable_committed: u64 = 0;
    let mut games_committed: u64 = 0;
    {
        let lane_ref = &lane;
        let committer = &mut committer;
        let qsearcher = &mut qsearcher;
        let usable = &mut usable_committed;
        let games = &mut games_committed;
        let mut on_game = |gp: clawfish::corpus::pgn::GamePositions| {
            // Stop once the usable target is met (exact truncation already landed
            // the lane on `target`).
            if committer.target_reached() {
                return;
            }
            // Game-level admission first (Termination / non-standard-start /
            // length / TC / Elo). `positions.len()` = mainline plies + 1.
            if !clawfish::corpus::filter::game_admitted(&gp.tags, gp.positions.len(), &filter) {
                return;
            }
            let Some(label) = gp.tags.result else {
                return;
            };
            // Per-position inline pipeline (the build-ready certificate every
            // lane shares): ply ≥ OPENING_SKIP_PLIES ∧ !in_check ∧ |static_eval|
            // ≤ HIGH_SCORE_CP ∧ is_quiet. The committer then handles dedup → cap
            // → exact target.
            let recs: Vec<CorpusRecord> = gp
                .positions
                .into_iter()
                .filter_map(|(pos, ply)| {
                    if ply < OPENING_SKIP_PLIES || in_check(&pos) {
                        return None;
                    }
                    let se = static_eval_white(&pos);
                    if se.abs() > HIGH_SCORE_CP {
                        return None;
                    }
                    let qs = qsearcher.eval_white(&pos);
                    if !is_quiet(&pos, se, qs) {
                        return None;
                    }
                    Some(CorpusRecord {
                        fen: pos.to_fen(),
                        label,
                        source,
                        game_id: gp.game_id,
                        ply,
                        depth_rung: DEPTH_RUNG_EXTERNAL,
                        strata: strata_for(&pos),
                    })
                })
                .collect();
            if recs.is_empty() {
                return;
            }
            match committer.commit_game(lane_ref, gp.game_id, recs) {
                Ok(outcome) if outcome.usable_committed > 0 => {
                    *usable += outcome.usable_committed;
                    *games += 1;
                }
                Ok(_) => {}
                Err(e) => eprintln!("corpus: commit_game failed for game {}: {e}", gp.game_id),
            }
        };
        stream_pgn(r, base_id, &mut on_game, &mut stats).map_err(|e| format!("stream_pgn: {e}"))?;
    }
    eprintln!(
        "corpus: PGN ingest done — games_seen={} parsed_emitted={} parse_failures={} \
         no_result_dropped={} usable_committed={usable_committed} games_committed={games_committed}",
        stats.games_seen, stats.games_emitted, stats.parse_failures, stats.no_result_dropped,
    );

    // Pin the lane knobs into the manifest so `finalize` carries them.
    let mut m = read_manifest(&out).unwrap_or_else(|_| stub_manifest(vec![(4, 1)]));
    m.cap_seed = cap_seed;
    m.target_usable = target;
    m.source = Some(source_tag);
    write_manifest(&out, &m).map_err(|e| format!("update ingest knobs: {e}"))?;
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
                   [--url <url>] [--lichess-month YYYY-MM] [--cap-seed <u64>]

Stream a compressed PGN dump over HTTPS, decompress, run the FULL inline
pipeline (game-level `GameFilter`, then per-position skip8 ∧ !in_check ∧
|static_eval| ≤ HIGH_SCORE_CP ∧ is_quiet), then per-lane FEN dedup → per-game
cap → exact target via the shared LaneCommitter, appending each surviving game
as one CRC-framed block to <out>/lane.bin. Robust for unattended runs: in-RAM
HTTP Range resume, a games-parsed stall watchdog, infinite exponential backoff,
and content-validity gates.

Supported URL extensions: .pgn.zst (streamed), .zip, .7z (downloaded to a
temp file then parsed locally).

--target-positions  Stop after this many NEW USABLE positions (post quiet-filter
                    → dedup → cap) are appended (default: unbounded — runs to
                    end-of-stream). The committer's exact truncation lands
                    lane.bin on N exactly.
--url               Explicit dump URL. Lichess auto-constructs from
                    --lichess-month (default a pinned month) when omitted; CCRL
                    REQUIRES --url to an official CCRL .7z (the count-bearing
                    filename isn't auto-constructible) — see docs/data-catalog.md.
                    The resolved URL is pinned into the lane manifest's source_url
                    for byte-re-derivability.
--cap-seed          Per-game reservoir-cap seed (default 0)."
        );
        return Ok(ExitCode::SUCCESS);
    }

    // Keep the resolved source string verbatim — it is the canonical lane tag
    // persisted in the manifest and emitted as `fetch --source <tag>` in
    // re-run.sh (it also selects the game-level filter below).
    let source_tag = args.require("source")?.to_string();
    let source = match source_tag.as_str() {
        "ccrl" => Source::Ccrl,
        "lichess" => Source::LichessOpen,
        other => {
            return Err(format!(
                "--source must be `ccrl` or `lichess`, got {other:?}"
            ));
        }
    };
    let out = PathBuf::from(args.require("out")?);
    fs::create_dir_all(&out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let target = args.parse_u64("target-positions", u64::MAX)?;
    let cap_seed = args.parse_u64("cap-seed", 0)?;

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
    // CCRL needs the TC-relaxed filter (no `[TimeControl]` tag); Lichess uses
    // the default band filter.
    let filter = match source {
        Source::Ccrl => clawfish::corpus::filter::ccrl_filter(),
        _ => GameFilter::default(),
    };
    let cfg = FetchConfig {
        cap_seed,
        ..FetchConfig::default()
    };

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

    // Pin the resolved source URL + the cap seed + the usable target into the
    // lane manifest so `finalize` carries them (byte-re-derivability: a fresh
    // uninterrupted fetch from `source_url` reproduces the lane — plan §1.6).
    let mut m = read_manifest(&out).unwrap_or_else(|_| stub_manifest(vec![(4, 1)]));
    m.source_url = Some(url);
    m.cap_seed = cap_seed;
    m.target_usable = (target != u64::MAX).then_some(target);
    m.source = Some(source_tag);
    write_manifest(&out, &m).map_err(|e| format!("update fetch knobs: {e}"))?;
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

// ─── finalize ────────────────────────────────────────────────────────────────

fn cmd_finalize(argv: &[String]) -> Result<ExitCode, String> {
    let args = parse_args(argv);
    if args.has("help") {
        eprintln!(
            "corpus finalize --in <lane-dir>

Finalize a lane directory: recompute corpus_sha256 over the (already
build-ready) lane.bin, write/update manifest.json + filter_spec.txt +
corpus_stats.txt, and write the per-lane re-run.sh. Does NO record
transformation — the lane was made build-ready inline by selfplay / fetch /
ingest-pgn (per-lane FEN dedup → per-game cap → exact target). M6.H2 replaces
the build-era `corpus build` (whose dedup/cap/split transform is now inline)."
        );
        return Ok(ExitCode::SUCCESS);
    }
    let dir = PathBuf::from(args.require("in")?);
    let lane = dir.join("lane.bin");
    if !lane.exists() {
        return Err(format!(
            "{}: no lane.bin to finalize — run selfplay / fetch / ingest-pgn first",
            dir.display()
        ));
    }

    // Recompute the lane digest over lane.bin as-is (no transform). Count usable
    // positions from the same scan so the manifest's total_positions matches the
    // bytes the digest covers.
    let (blocks, _) = scan_valid_blocks(&lane).map_err(|e| format!("scan lane.bin: {e}"))?;
    let total_positions: u64 = blocks.iter().map(|b| b.records.len() as u64).sum();
    let bytes = fs::read(&lane).map_err(|e| format!("read {}: {e}", lane.display()))?;
    let corpus_digest = hex_digest(&sha256_bytes(&bytes));

    // Carry every knob from the existing manifest (written by selfplay / fetch /
    // ingest-pgn). If none exists, start from a stub.
    let mut m = read_manifest(&dir).unwrap_or_else(|_| stub_manifest(vec![(4, 1)]));
    m.schema_version = 2;
    m.created_at = timestamp();
    m.engine_commit = engine_commit();
    m.total_positions = total_positions;
    m.corpus_sha256 = corpus_digest.clone();
    write_manifest(&dir, &m).map_err(|e| format!("write_manifest: {e}"))?;
    write_filter_spec(&dir).map_err(|e| format!("write_filter_spec: {e}"))?;

    // Stats file — recorded knobs the gate / a reviewer can inspect.
    let mut stats = String::new();
    stats.push_str(&format!("total_positions={total_positions}\n"));
    stats.push_str(&format!("games={}\n", blocks.len()));
    stats.push_str(&format!("cap_seed={}\n", m.cap_seed));
    if let Some(t) = m.target_usable {
        stats.push_str(&format!("target_usable={t}\n"));
    }
    if let Some(url) = &m.source_url {
        stats.push_str(&format!("source_url={url}\n"));
    }
    stats.push_str(&format!("corpus_sha256={corpus_digest}\n"));
    stats.push_str(&format!("QUIET_MARGIN_CP={QUIET_MARGIN_CP}\n"));
    atomic_write(&dir.join("corpus_stats.txt"), stats.as_bytes())
        .map_err(|e| format!("write corpus_stats: {e}"))?;

    write_rerun_script(&dir, &m)?;

    eprintln!(
        "corpus: finalize complete — total_positions={total_positions}, games={}, sha256={corpus_digest}",
        blocks.len()
    );
    Ok(ExitCode::SUCCESS)
}

/// Write the per-lane `re-run.sh` (M6.H2). The lane type is read from the
/// manifest: a non-empty `source_url` ⇒ a FETCH lane (`corpus fetch --url … &&
/// finalize && quality-gate`); otherwise a SELF-PLAY lane (`corpus selfplay …
/// --cap-seed … --cap-positions … && finalize && quality-gate`). No `corpus
/// build` step, no `--val-fraction`/`--split-seed` (the split moved to M6.I).
fn write_rerun_script(dir: &Path, manifest: &Manifest) -> Result<(), String> {
    let cap_positions_arg = match manifest.target_usable {
        Some(t) => format!(" --cap-positions {t}"),
        None => String::new(),
    };
    let body = if let Some(url) = &manifest.source_url {
        // FETCH lane. The source tag is load-bearing: `cmd_fetch` requires
        // `--source` (it selects the CCRL-vs-Lichess game filter), and it is
        // unrecoverable from anything else in the manifest. Fail loudly rather
        // than emit a broken script.
        let source = manifest.source.as_deref().ok_or_else(|| {
            format!(
                "{}: FETCH lane manifest has source_url but no `source` tag — \
                 cannot generate a runnable re-run.sh (re-run `corpus fetch` to \
                 repopulate the manifest)",
                dir.display()
            )
        })?;
        // Fetch reads `--target-positions` (NOT `--cap-positions`, which it
        // silently ignores → an unbounded re-fetch). Emit the recorded target.
        let target_arg = match manifest.target_usable {
            Some(t) => format!(" --target-positions {t}"),
            None => String::new(),
        };
        format!(
            r#"#!/usr/bin/env bash
# re-run.sh — operator script to re-derive this FETCH lane's lane.bin.
# Generated by `corpus finalize` (M6.H2).
#
# A *fresh uninterrupted* fetch from the pinned source_url reproduces lane.bin
# byte-for-byte (ADR-0036 / plan §1.6). A *resumed* fetch is content/label-
# equivalent (same FEN+label multiset) but its game_id provenance tags may
# shift — the pre-existing ADR-0035 §8 "weaker / re-derivable" external-source
# tier. The re-run.sh checks nothing about upstream drift beyond what the fetch
# content-validity gates enforce; an upstream URL change is an operator concern.
#
# `corpus fetch` decompresses .pgn.zst/.zip/.7z in-process (zstd / zip /
# sevenz-rust2, behind the `corpus-fetch` build feature) — no system tools.

set -eu

DIR="$(cd "$(dirname "$0")" && pwd)"
BIN="${{CORPUS_BIN:-corpus}}"
URL="{url}"

echo "corpus re-run (fetch lane): using binary '$BIN' against $DIR; url=$URL"

# (1) Fetch the lane through the full inline pipeline → lane.bin.
"$BIN" fetch --source {source} --url "$URL" --out "$DIR"{target_arg}

# (2) Finalize: recompute corpus_sha256 + manifest/stats/re-run.sh.
"$BIN" finalize --in "$DIR"

# (3) Run the data-quality gate (must PASS).
"$BIN" quality-gate --dir "$DIR"
"#
        )
    } else {
        // SELF-PLAY lane.
        format!(
            r#"#!/usr/bin/env bash
# re-run.sh — operator script to re-derive this SELF-PLAY lane's lane.bin.
# Generated by `corpus finalize` (M6.H2).
#
# Deterministic given (seed, cap_seed, games, workers, max_plies,
# opening_random_plies, opening_mode, binary). Every reproducibility knob is
# READ FROM manifest.json — no shell-default substitution for a pinned knob;
# re-running with different knobs produces a DIFFERENT lane, not the frozen one.

set -eu

DIR="$(cd "$(dirname "$0")" && pwd)"
BIN="${{CORPUS_BIN:-corpus}}"

# `manifest_field <name>` extracts a top-level JSON scalar (number/string).
# Trims whitespace, comma, and quote chars.
manifest_field() {{
    grep -E "\"$1\"" "$DIR/manifest.json" | head -1 | tr -d ' ,"' | cut -d: -f2
}}

SEED="$(manifest_field self_play_seed)"
GAMES="$(manifest_field games)"
MAX_PLIES="$(manifest_field max_plies)"
OPENING_RANDOM_PLIES="$(manifest_field opening_random_plies)"
WORKERS="$(manifest_field workers)"
CAP_SEED="$(manifest_field cap_seed)"
OPENING_BOOK="$(manifest_field opening_book_path)"
OPENING_MODE="$(manifest_field opening_mode)"

# Default to `random` when the manifest does not pin a mode.
if [ -z "$OPENING_MODE" ]; then
    OPENING_MODE=random
fi

echo "corpus re-run (self-play lane): using binary '$BIN' against $DIR"
echo "corpus re-run: seed=$SEED games=$GAMES workers=$WORKERS \
max_plies=$MAX_PLIES opening_random_plies=$OPENING_RANDOM_PLIES \
cap_seed=$CAP_SEED opening_mode=$OPENING_MODE opening_book=${{OPENING_BOOK:-<none>}}"

# (1) self-play through the full inline pipeline → lane.bin. The manifest's
#     opening_mode pins which regime — book campaigns require an opening book.
if [ "$OPENING_MODE" = "book" ]; then
    "$BIN" selfplay --seed "$SEED" --games "$GAMES" --workers "$WORKERS" \
        --max-plies "$MAX_PLIES" --opening-random-plies "$OPENING_RANDOM_PLIES" \
        --cap-seed "$CAP_SEED"{cap_positions_arg} \
        --opening-mode book --opening-book "$OPENING_BOOK" \
        --out "$DIR"
else
    "$BIN" selfplay --seed "$SEED" --games "$GAMES" --workers "$WORKERS" \
        --max-plies "$MAX_PLIES" --opening-random-plies "$OPENING_RANDOM_PLIES" \
        --cap-seed "$CAP_SEED"{cap_positions_arg} \
        --opening-mode random \
        --out "$DIR"
fi

# (2) Finalize: recompute corpus_sha256 + manifest/stats/re-run.sh.
"$BIN" finalize --in "$DIR"

# (3) Run the data-quality gate (must PASS).
"$BIN" quality-gate --dir "$DIR"
"#
        )
    };
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

Run the five data-quality checks against the frozen lane at <dir>.
Exit 0 iff the gate passes (the three must-PASS checks — ADR-0003
audit, reproducibility re-run, per-lane dedup integrity — all PASSed)."
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

Convert the lane's flat lane.bin to TSV on stdout. Columns:
fen\\tlabel\\tsource\\tgame_id\\tply\\tdepth_rung\\tstrata.

Reads <dir>/lane.bin."
        );
        return Ok(ExitCode::SUCCESS);
    }
    let in_dir = PathBuf::from(args.require("in")?);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "fen\tlabel\tsource\tgame_id\tply\tdepth_rung\tstrata")
        .map_err(|e| format!("write tsv header: {e}"))?;
    let mut total = 0u64;
    let p = in_dir.join("lane.bin");
    if p.exists() {
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

Convenience wrapper: re-execute the generate→finalize→quality-gate flow
documented in the lane dir's `re-run.sh`. The script is the operator-facing
artifact; this subcommand simply prints a pointer to it so a re-run is always
traceable to a committed file."
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
            "no re-run.sh found next to {}; re-run `corpus finalize` to regenerate",
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

Truncate `lane.bin` in <dir> to exactly N positions (or as close as the
block sizes allow). The largest game_id prefix whose total position
count is ≤ N is kept whole; if N falls mid-game, the boundary game's
block is re-encoded with only the first (N − cumulative) records and
appended in place. The resulting lane has exactly N positions (or, on an
edge case where the cap lands precisely on a game boundary, exactly N
positions with no partial game). All retained blocks remain CRC-framed
and decodable.

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

    // Scan lane.bin's game blocks (in game_id order), find the largest
    // whole-game prefix whose position count ≤ cap, and (if cap falls mid-game)
    // re-encode the boundary block with only the leading (cap − cumulative)
    // records. The file is append-only CRC-framed per game; we truncate at the
    // boundary then append the partial block, if any.
    let lane_path = out.join("lane.bin");
    // Scan-with-offsets: end-of-block byte position is taken directly from the
    // scan, not reconstructed by re-encoding — matching the truncate's
    // "truncate at on-disk boundary" contract literally.
    let (blocks_off, valid_len) =
        clawfish::corpus::store::scan_valid_blocks_with_offsets(&lane_path)
            .map_err(|e| format!("scan lane.bin: {e}"))?;
    let blocks: Vec<&GameBlock> = blocks_off.iter().map(|(b, _)| b).collect();
    let end_offsets: Vec<u64> = blocks_off.iter().map(|(_, o)| *o).collect();

    let total_positions_before: u64 = blocks.iter().map(|b| b.records.len() as u64).sum();
    eprintln!(
        "corpus: before truncate: lane.bin={} bytes ({} games), total positions={}",
        valid_len,
        blocks.len(),
        total_positions_before
    );

    // Walk blocks in (scan = game_id) order. For each block:
    //   - if cum + n_positions ≤ cap: keep whole block; advance keep_until.
    //   - else: this is the boundary block. Stop; remember its (idx, room =
    //     cap − cum) so we can re-encode the partial.
    let mut cum: u64 = 0;
    let mut keep_until: u64 = 0;
    let mut games_kept: u64 = 0;
    let mut partial: Option<(usize, u64)> = None; // (block_idx, room)
    for (idx, b) in blocks.iter().enumerate() {
        let n_positions = b.records.len() as u64;
        let next = cum + n_positions;
        if next > cap {
            let room = cap - cum;
            if room > 0 {
                partial = Some((idx, room));
            }
            break;
        }
        cum = next;
        games_kept += 1;
        keep_until = end_offsets[idx];
    }

    let partial_positions = partial.as_ref().map(|p| p.1).unwrap_or(0);
    eprintln!(
        "corpus: truncate target: keep {} whole games + {} partial-game records = {} positions (cap {})",
        games_kept,
        partial_positions,
        cum + partial_positions,
        cap
    );

    // Whole-game truncation: set the file length to keep_until directly.
    if keep_until < valid_len {
        let f = fs::OpenOptions::new()
            .write(true)
            .open(&lane_path)
            .map_err(|e| format!("open lane.bin: {e}"))?;
        f.set_len(keep_until)
            .map_err(|e| format!("truncate lane.bin: {e}"))?;
        f.sync_all().map_err(|e| format!("fsync lane.bin: {e}"))?;
        eprintln!("corpus: lane.bin truncated {valid_len} → {keep_until} bytes");
    }

    // Partial-game block: re-encode the boundary block with the first `room`
    // records and append. The block's game_id is preserved (so the
    // dispatcher's gap-list logic sees it as already-played and doesn't
    // re-dispatch it on resume).
    if let Some((idx, room)) = partial {
        let boundary_block = &blocks[idx];
        let mut records = boundary_block.records.clone();
        records.truncate(room as usize);
        let partial_bytes = clawfish::corpus::store::encode_block(boundary_block.game_id, &records);
        clawfish::corpus::store::append_raw_bytes(&lane_path, &partial_bytes)
            .map_err(|e| format!("append partial block to lane.bin: {e}"))?;
        eprintln!(
            "corpus: lane.bin partial-game appended: game_id={} kept {}/{} records, {} bytes",
            boundary_block.game_id,
            room,
            boundary_block.records.len(),
            partial_bytes.len()
        );
    }

    // Reset the checkpoint so a future selfplay run resumes coherently with
    // the truncated state (next_consume_id matches what survived). We delete
    // it; the resume protocol falls back to "scan lane.bin, derive
    // next_consume_id from max committed_id + 1".
    let ckpt = out.join("checkpoint.bin");
    if ckpt.exists() {
        let _ = fs::remove_file(&ckpt);
        eprintln!("corpus: checkpoint.bin removed (resume will rebuild from lane.bin)");
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
    fn max_existing_game_id_picks_max_in_lane() {
        // M6.H2: each lane lives in its own dir, so its game_id space is
        // private — `max_existing_game_id` scans only this lane's lane.bin.
        // A subsequent `ingest-pgn` into the SAME dir starts at max+1 (no
        // collision with already-committed games).
        let td = TempDir::new("lane");
        for gid in [10u64, 25, 42] {
            append_block(&td.path().join("lane.bin"), gid, &[rec(gid, 0)]).unwrap();
        }
        assert_eq!(
            max_existing_game_id(td.path()),
            42,
            "max game_id in lane.bin; a follow-on ingest reserves 43+"
        );
    }

    #[test]
    fn cmd_ingest_pgn_base_id_reserves_above_lane_max() {
        // A prior generator left games up to id 19 in lane.bin; a 2nd
        // `ingest-pgn` into the same dir must start strictly above (base 20).
        let td = TempDir::new("ingest-base");
        for gid in [3u64, 7, 19] {
            append_block(&td.path().join("lane.bin"), gid, &[rec(gid, 0)]).unwrap();
        }
        assert_eq!(max_existing_game_id(td.path()), 19);
    }

    // ── re-run.sh generation ─────────────────────────────────────────────

    /// A bare manifest with overridable lane-type fields for re-run.sh tests.
    fn rerun_manifest() -> Manifest {
        Manifest {
            schema_version: 2,
            created_at: "2026-05-22T00:00:00Z".into(),
            engine_commit: "test".into(),
            total_positions: 0,
            sources: Vec::new(),
            self_play_seed: 1,
            games: 0,
            max_plies: 400,
            opening_random_plies: 8,
            workers: 1,
            cap_seed: 7,
            source_url: None,
            target_usable: None,
            source: None,
            depth_ladder: vec![(4, 1)],
            opening_book_path: None,
            opening_book_sha256: None,
            opening_mode: None,
            corpus_sha256: "deadbeef".into(),
        }
    }

    #[test]
    fn rerun_script_fetch_lane_emits_source_url_and_target_positions() {
        // A FETCH lane's re-run.sh must invoke `corpus fetch` with the flags
        // `cmd_fetch` actually reads: `--source` (required, selects the game
        // filter), `--url`, `--target-positions` (NOT `--cap-positions`, which
        // fetch silently ignores → an unbounded re-fetch), and `--out`.
        let td = TempDir::new("rerun-fetch");
        let mut m = rerun_manifest();
        m.source_url = Some("https://ccrl.example/games.pgn.7z".into());
        m.source = Some("ccrl".into());
        m.target_usable = Some(2_000_000);

        write_rerun_script(td.path(), &m).unwrap();
        let script = std::fs::read_to_string(td.path().join("re-run.sh")).unwrap();

        // Required fetch flags present.
        assert!(
            script.contains("--source ccrl"),
            "fetch re-run.sh must pass --source (cmd_fetch requires it):\n{script}"
        );
        assert!(
            script.contains("--url"),
            "fetch re-run.sh must pass --url:\n{script}"
        );
        assert!(
            script.contains("--target-positions 2000000"),
            "fetch re-run.sh must pass --target-positions (the flag cmd_fetch reads):\n{script}"
        );
        assert!(
            script.contains("--out"),
            "fetch re-run.sh must pass --out:\n{script}"
        );
        // The wrong flag (cap-positions) is silently ignored by cmd_fetch and
        // would run an UNBOUNDED re-fetch — it must NOT appear in a fetch lane.
        assert!(
            !script.contains("--cap-positions"),
            "fetch re-run.sh must NOT use --cap-positions (cmd_fetch ignores it):\n{script}"
        );

        // Stronger check: the emitted `fetch` argv satisfies cmd_fetch's
        // required flags (source + url + out all present on the fetch line).
        let fetch_line = script
            .lines()
            .find(|l| l.contains("fetch --"))
            .expect("re-run.sh has a `corpus fetch` line");
        assert!(fetch_line.contains("--source"));
        assert!(fetch_line.contains("--url"));
        assert!(fetch_line.contains("--out"));
    }

    #[test]
    fn rerun_script_fetch_lane_without_source_fails_loudly() {
        // A FETCH lane (source_url set) but no `source` tag is unrecoverable —
        // cmd_fetch requires --source. write_rerun_script must error, not emit
        // a broken script that re-fetches without --source.
        let td = TempDir::new("rerun-fetch-no-source");
        let mut m = rerun_manifest();
        m.source_url = Some("https://ccrl.example/games.pgn.7z".into());
        m.source = None;
        m.target_usable = Some(100);

        let err = write_rerun_script(td.path(), &m)
            .expect_err("missing source on a fetch lane must fail loudly");
        assert!(err.contains("source"), "error must mention source: {err}");
        assert!(
            !td.path().join("re-run.sh").exists(),
            "no script must be written when source is missing"
        );
    }

    #[test]
    fn rerun_script_selfplay_lane_emits_cap_positions_and_cap_seed() {
        // A SELF-PLAY lane (no source_url) must use `--cap-positions` (the flag
        // cmd_selfplay reads) and carry `--cap-seed`.
        let td = TempDir::new("rerun-selfplay");
        let mut m = rerun_manifest();
        m.source_url = None;
        m.source = Some("selfplay-off-book".into());
        m.opening_mode = Some("random".into());
        m.target_usable = Some(500_000);

        write_rerun_script(td.path(), &m).unwrap();
        let script = std::fs::read_to_string(td.path().join("re-run.sh")).unwrap();

        assert!(
            script.contains("--cap-positions 500000"),
            "self-play re-run.sh must pass --cap-positions (cmd_selfplay reads it):\n{script}"
        );
        assert!(
            script.contains("--cap-seed"),
            "self-play re-run.sh must pass --cap-seed:\n{script}"
        );
        // A self-play lane never uses fetch's --target-positions flag.
        assert!(
            !script.contains("--target-positions"),
            "self-play re-run.sh must NOT use --target-positions:\n{script}"
        );
    }
}
