//! In-process deterministic fixed-depth self-play corpus generator
//! (R1–R7, R-TC).
//!
//! Per-game-file architecture: each worker writes its completed game to
//! `pending/game-{game_id:020}.bin`; a single consumer thread polls for the
//! next-expected game_id in strict order, routes dedup → cap → exact-target
//! through the shared `LaneCommitter`, and appends to `lane.bin` (M6.H2: a
//! flat per-lane corpus, no train/val split — that moves to M6.I). This makes
//! the lane bytes a deterministic function of `(seed, cap_seed, game_id)`,
//! independent of worker count K (the primary acceptance property).
//!
//! Determinism precondition: `SearchLimits{ depth: Some(d), nodes: None,
//! movetime: None, infinite: false }` with `TimeCaps{soft:MAX,hard:MAX}` ⇒
//! `should_abort` only via `ctx.stop`; a `stop`-aborted in-flight game is
//! DROPPED (R2). Fixed-depth ⇒ load/suspend/renice-independent ⇒ R3/R4
//! without `VirtualClock`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use super::consumer::{ConsumerStats, consumer_loop, resume};
use super::dispatcher::{ClaimGuard, Dispatcher};
use super::objective::strata_for;
use super::openings::Book;
use super::pending::write_pending;
use super::prng::{Prng, substream_seed};
use super::quiet::{is_quiet, static_eval_white};
use super::store::encode_block;
use super::{CorpusError, CorpusRecord, HIGH_SCORE_CP, Label, OPENING_SKIP_PLIES, Source};
use crate::search::{
    AlphaBetaMover, QSearcher, Search, SearchContext, SearchLimits, SearchResult, TimeCaps,
};
use crate::search::{is_fifty_move_draw, is_repetition};
use crate::{Color, MoveList, PieceKind, Position, generate_moves, in_check};

/// Opening regime for a self-play campaign. A campaign is constrained to
/// a single regime end-to-end so every game it commits is tagged with the
/// matching `Source` variant. The M6.I bi-level optimizer reweights
/// between on-book and off-book corpora at training time via the per-
/// source loss in `StratObjective`; the operator runs one campaign per
/// regime and grows each independently.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OpeningMode {
    /// Seed every game from a sampled position in the vendored opening
    /// book. Records emit `Source::SelfPlayOnBook`. Requires `book =
    /// Some(_)` in `SelfPlayConfig`; an empty book is a configuration
    /// error caught at the CLI.
    Book,
    /// Seed every game from `startpos + opening_random_plies` random
    /// plies. Records emit `Source::SelfPlayOffBook`. Ignores the
    /// `book` field even when one is loaded.
    Random,
}

impl OpeningMode {
    /// `Source` variant every record from a campaign in this mode carries.
    pub fn source(self) -> Source {
        match self {
            OpeningMode::Book => Source::SelfPlayOnBook,
            OpeningMode::Random => Source::SelfPlayOffBook,
        }
    }
}

/// Self-play campaign config. `depth_ladder` rungs come from
/// `corpus calibrate-ladder` (empirically-anchored, NOT plan literals).
#[derive(Clone, Debug)]
pub struct SelfPlayConfig {
    /// Base RNG seed (deterministic campaign).
    pub seed: u64,
    /// Number of games to generate.
    pub games: u64,
    /// Worker thread count (default = all cores; R7).
    pub workers: usize,
    /// `(depth, weight)` rungs; weights = deployment mixed-TC profile.
    pub depth_ladder: Vec<(u8, u32)>,
    /// Seeded-random plies added *after* the opening seed (book FEN or
    /// startpos) for intra-seed decorrelation.
    pub opening_random_plies: u32,
    /// Max half-moves before adjudicating an over-long game.
    pub max_plies: u32,
    /// Output directory (`lane.bin` + checkpoint).
    pub out_dir: PathBuf,
    /// Opening regime — every game in the campaign uses this regime, and
    /// every committed record carries `opening_mode.source()`. Replaces
    /// the previous per-game `book_fraction` coin-flip; the book vs off-
    /// book mix is now a training-time reweighting axis over two distinct
    /// corpora (ADR-0035 §10).
    pub opening_mode: OpeningMode,
    /// Vendored opening book — required when `opening_mode = Book`,
    /// ignored otherwise. Shared via `Arc` so all workers reuse the
    /// in-RAM positions without cloning.
    pub book: Option<Arc<Book>>,
    /// Consumer poll interval override (ms). `None` ⇒ use the default
    /// `POLL_INTERVAL_MS` constant. Overridable per `cfg` so tests can
    /// drive a fast poll without sleeping 10 ms per iteration. Has no
    /// effect on byte-identity — only on commit latency (§2.6 /
    /// Substantive #7).
    pub poll_interval_ms: Option<u64>,
    /// Seed for the per-game reservoir-cap sampling (Knuth/Vitter Algorithm R,
    /// per-game sub-stream `substream_seed(cap_seed, game_id)`). Deterministic
    /// given `(cap_seed, game_id)`; K-independent (the cap depends only on the
    /// seed, not on worker scheduling). Pinned in the manifest for
    /// reproducibility. (M6.H2: renamed from `split_seed` — the train/val split
    /// it also drove moves to M6.I; only the cap role remains.)
    pub cap_seed: u64,
    /// Optional cap on TOTAL durable usable positions in `lane.bin`. When the
    /// committer's cumulative count reaches this cap, the consumer signals
    /// `stop`, the workers drain their last game, and the campaign exits
    /// gracefully. The cap is in *positions* (not games) because the operator's
    /// actual target is corpus size for Texel tuning, and games-per-position
    /// varies. The committer's exact truncation lands the lane on the cap
    /// exactly. `None` ⇒ unbounded.
    pub cap_positions: Option<u64>,
}

/// Self-play campaign outcome counters.
///
/// Field provenance (§4.4):
/// - `games_emitted` / `games_dropped_inflight`: worker-side counters.
/// - `games_committed` / `games_empty_post_dedup` / `positions_committed`:
///   consumer-side counters (per-game-file architecture). In the legacy
///   channel architecture these are derived from the writer thread.
#[derive(Clone, Debug, Default)]
pub struct SelfPlayStats {
    /// Worker: completed-game pending files written (or in the legacy arch,
    /// games sent to the writer thread).
    pub games_emitted: u64,
    /// Worker: games abandoned in-flight (stop-abort) — contributed ZERO records.
    pub games_dropped_inflight: u64,
    /// Consumer: pending files processed (games committed, incl. empty ones).
    pub games_committed: u64,
    /// Consumer: games that committed zero records (all FENs were
    /// dedup-duplicates or post-dedup-cap count was zero).
    pub games_empty_post_dedup: u64,
    /// Consumer: records appended to `lane.bin` after dedup + cap (cumulative,
    /// including the resumed-from-disk count).
    pub positions_committed: u64,
    /// The lane byte-offset before the boundary game's block was appended, if
    /// the exact-target truncation fired this run. Forwarded from the consumer's
    /// `ConsumerStats`. Used by the extend driver to persist the offset.
    pub truncated_boundary_offset: Option<u64>,
}

/// One completed game's transactional payload: the game id + every
/// post-opening-skip position with the White-POV game label. Buffered
/// entirely in RAM; flushed as ONE block only on a natural terminal (R1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameRecord {
    /// Unique game id (R7 striping key; per-lane dedup arrival order).
    pub game_id: u64,
    /// Post-opening-skip labeled positions, in game order.
    pub records: Vec<CorpusRecord>,
}

// ---------------------------------------------------------------------------
// Game-result detection (own copy — deliberately NOT the `elo_iterate`
// adjudicate module, which is the SPRT-critical harness; plan §3.7 keeps
// M6.G off it). White-POV `Label`. Order: mate/stalemate → 50-move →
// threefold → insufficient material.
// ---------------------------------------------------------------------------

/// True iff neither side can force mate: KK / KBK / KNK / KBKB-same-colour.
fn is_insufficient_material(pos: &Position) -> bool {
    if pos.pieces(PieceKind::Pawn).any()
        || pos.pieces(PieceKind::Rook).any()
        || pos.pieces(PieceKind::Queen).any()
    {
        return false;
    }
    let wb = pos.pieces_colored(Color::White, PieceKind::Bishop);
    let bb = pos.pieces_colored(Color::Black, PieceKind::Bishop);
    let wn = pos.pieces_colored(Color::White, PieceKind::Knight);
    let bn = pos.pieces_colored(Color::Black, PieceKind::Knight);
    let w_minors = wb.count() + wn.count();
    let b_minors = bb.count() + bn.count();
    match (w_minors, b_minors) {
        (0, 0) => true,
        (1, 0) | (0, 1) => true,
        (1, 1) if wb.count() == 1 && bb.count() == 1 && wn.is_empty() && bn.is_empty() => {
            // Square colour parity = (file XOR rank) & 1 = (idx XOR idx>>3) & 1.
            let parity = |sq: crate::Square| {
                let idx = sq.index();
                (idx ^ (idx >> 3)) & 1
            };
            let w = wb.lsb().expect("count==1 guarantees a set bit");
            let b = bb.lsb().expect("count==1 guarantees a set bit");
            parity(w) == parity(b)
        }
        _ => false,
    }
}

/// Native game-result detection. `hist` is the full-game Zobrist trail
/// including `pos` as the last entry. White-POV `Label`. Returns `None`
/// when the game is not over.
fn game_result(pos: &Position, hist: &[u64]) -> Option<Label> {
    let mut moves = MoveList::new();
    generate_moves(pos, &mut moves);
    if moves.is_empty() {
        return Some(if in_check(pos) {
            // Side to move is mated; the side that just moved won.
            match pos.side_to_move() {
                Color::White => Label::BlackWin,
                Color::Black => Label::WhiteWin,
            }
        } else {
            Label::Draw // stalemate
        });
    }
    if is_fifty_move_draw(pos.halfmove_clock()) {
        return Some(Label::Draw);
    }
    // Threefold-as-draw via the engine's repetition detector (plan §3.5
    // names `is_repetition` explicitly — the engine forces/claims the draw
    // on a repetition, the standard engine-internal convention).
    if is_repetition(hist, pos.halfmove_clock()) {
        return Some(Label::Draw);
    }
    if is_insufficient_material(pos) {
        return Some(Label::Draw);
    }
    None
}

// ---------------------------------------------------------------------------
// Single-game play (deterministic, pure-ish given seed+depth).
// ---------------------------------------------------------------------------

/// Search the position to fixed `depth` and return the best move, or `None`
/// if the search was `stop`-aborted (R2: the caller drops the game) or no
/// move exists. The `TimeCaps{MAX,MAX}` precondition makes the only abort
/// path `ctx.stop` ⇒ a non-aborted result is a pure function of
/// `(pos, depth)`.
fn search_best_move(
    searcher: &mut AlphaBetaMover,
    pos: &Position,
    history: &[u64],
    depth: u8,
    stop: &Arc<AtomicBool>,
) -> Option<crate::Move> {
    let limits = SearchLimits {
        depth: Some(depth as u32),
        nodes: None,
        movetime: None,
        infinite: false,
        ..SearchLimits::default()
    };
    let ctx = SearchContext {
        stop: Arc::clone(stop),
        caps: TimeCaps {
            soft: Duration::MAX,
            hard: Duration::MAX,
        },
        virtual_clock: false,
        limits,
        history: history.to_vec(),
        tt: None,
    };
    let sink = |_: &str| {};
    let result: SearchResult = searcher.go(pos, &ctx, &sink);
    // A `stop`-aborted search may still return a (defensive) bestmove; the
    // game is dropped wholesale by the caller checking `stop` itself, so
    // here we only need "no legal move" to map to None.
    result.bestmove
}

/// Outcome of `play_one_game`: a completed game's transactional record, or
/// `None` if interrupted in-flight (R2: contributes ZERO records).
#[allow(clippy::too_many_arguments)] // play/quiet searchers + per-game knobs is what it is
fn play_one_game(
    searcher: &mut AlphaBetaMover,
    qsearcher: &mut QSearcher,
    book: Option<&Book>,
    opening_mode: OpeningMode,
    game_id: u64,
    depth: u8,
    opening_random_plies: u32,
    max_plies: u32,
    rng: &mut Prng,
    stop: &Arc<AtomicBool>,
) -> Option<GameRecord> {
    // R3 hardening: enforce the "this game starts with cold searcher state"
    // contract by construction, so the output depends ONLY on (game_id, depth,
    // opening_mode, seed) and not on whatever TT / history / killers /
    // pawn-hash entries linger from a prior `play_one_game` on the same
    // `&mut` searchers. `selfplay::run` already constructs fresh searchers per
    // game; this reset makes the *inner* function honour the same invariant
    // unconditionally, so `fresh_vs_warm_searcher_same_seed_same_game_identical`
    // is true by construction rather than by lucky search-state alignment at a
    // particular eval landscape (which is how it passed at M6.I and broke at
    // M6.J once the weights shifted the search trajectory).
    *searcher = AlphaBetaMover::new();
    *qsearcher = QSearcher::new();

    // Opening seed: `Book` mode samples from the vendored book (which
    // MUST be loaded — the CLI rejects `Book + book = None` before we
    // reach here); `Random` mode starts from the startpos. Every record
    // emitted by this call is tagged with `opening_mode.source()`.
    let mut pos = match opening_mode {
        OpeningMode::Book => *book
            .expect("OpeningMode::Book requires book = Some(_) (CLI invariant)")
            .sample(rng),
        OpeningMode::Random => Position::starting_position(),
    };
    let mut history = vec![pos.zobrist()];
    let mut ply: u32 = 0;

    // Seeded-random opening plies (diversification) appended AFTER the
    // opening seed (book FEN or startpos). Reject a seed that reaches an
    // early game-over inside the opening: replay from startpos is
    // impossible (the opening is fixed by the rng draw), so a dead
    // opening just yields a short game — we treat the natural terminal as
    // the result rather than discarding (still a valid game-result label,
    // just rare). The plan's "reject seeds reaching early game-over" is
    // handled by `game_result` returning a terminal; the loop below stops.
    while ply < opening_random_plies {
        if game_result(&pos, &history).is_some() {
            break;
        }
        let mut moves = MoveList::new();
        generate_moves(&pos, &mut moves);
        if moves.is_empty() {
            break;
        }
        let pick = rng.below(moves.len() as u64) as usize;
        let mv = moves.as_slice()[pick];
        pos.make_move(mv);
        history.push(pos.zobrist());
        ply += 1;
    }

    let mut records: Vec<CorpusRecord> = Vec::new();
    // Each emitted entry has its strata bit pre-tagged at emit time so the
    // shard carries quiet-certified, build-ready records. Label is back-
    // filled once the game terminates.
    let mut emitted: Vec<(String, u32, u8)> = Vec::new();

    let label = loop {
        if stop.load(Ordering::Relaxed) {
            // R2: an interrupted in-flight game contributes ZERO records.
            return None;
        }
        if let Some(l) = game_result(&pos, &history) {
            break l;
        }
        if ply >= max_plies {
            // max_plies adjudication-as-draw.
            break Label::Draw;
        }
        // Inline per-position filter: ply ≥ OPENING_SKIP_PLIES ∧ !in_check ∧
        // |static_eval| ≤ HIGH_SCORE_CP ∧ quiet predicate ⇒ admitted; strata
        // pre-tagged. Yields build-ready records — the consumer then routes
        // them through the LaneCommitter for per-lane dedup → per-game cap →
        // exact target (the cross-game work; M6.H2: no split here).
        if ply >= OPENING_SKIP_PLIES && !in_check(&pos) {
            let se = static_eval_white(&pos);
            if se.abs() <= HIGH_SCORE_CP {
                let qs = qsearcher.eval_white(&pos);
                if is_quiet(&pos, se, qs) {
                    emitted.push((pos.to_fen(), ply, strata_for(&pos)));
                }
            }
        }
        // No legal move would have been caught by `game_result`; a None
        // here means a stop-abort raced the loop guard — drop the game.
        let mv = search_best_move(searcher, &pos, &history, depth, stop)?;
        pos.make_move(mv);
        history.push(pos.zobrist());
        ply += 1;
    };

    let source = opening_mode.source();
    for (fen, p, strata) in emitted {
        records.push(CorpusRecord {
            fen,
            label,
            source,
            game_id,
            ply: p,
            depth_rung: depth,
            strata,
        });
    }

    Some(GameRecord { game_id, records })
}

// ---------------------------------------------------------------------------
// R-TC depth ladder.
// ---------------------------------------------------------------------------

/// Pure rung-construction from per-bucket measured median completed depths.
/// Weights are the deployment mixed-TC profile (equal here — the canonical
/// 4-bucket SPRT profile). Deterministic given the median vector; the test
/// `calibrate_ladder_deterministic_on_fixture` drives this directly with a
/// fixed (mocked) median table.
fn ladder_from_medians(medians: &[u8]) -> Vec<(u8, u32)> {
    medians.iter().map(|&d| (d, 1u32)).collect()
}

/// Median of completed iterative-deepening depths measured at one movetime
/// budget across the bench corpus. `samples` is one completed-depth reading
/// per bench position.
fn median_depth(mut samples: Vec<u8>) -> u8 {
    debug_assert!(
        !samples.is_empty(),
        "median of an empty sample is undefined"
    );
    samples.sort_unstable();
    samples[samples.len() / 2]
}

/// Empirically measure clawfish's median completed iterative-deepening
/// depth at each deployment movetime bucket over `bench::BENCH_POSITIONS`.
/// Pins the R-TC ladder (recorded in the manifest); re-runnable.
///
/// Movetime-driven measurement is wall-clock dependent by nature (that is
/// the point — it calibrates the depth↔TC proxy on THIS machine, per
/// ADR-0035's owned residual caveat). The pure `ladder_from_medians` /
/// `median_depth` split keeps the rung-construction deterministically
/// testable.
pub fn calibrate_ladder(buckets_ms: &[u64]) -> Vec<(u8, u32)> {
    use crate::bench::BENCH_POSITIONS;

    let mut medians = Vec::with_capacity(buckets_ms.len());
    for &ms in buckets_ms {
        let mut depths = Vec::with_capacity(BENCH_POSITIONS.len());
        for fen in BENCH_POSITIONS.iter() {
            let pos = Position::from_fen(fen)
                .unwrap_or_else(|e| panic!("BENCH_POSITIONS parse: {fen:?} ({e})"));
            let mut searcher = AlphaBetaMover::new();
            let limits = SearchLimits {
                movetime: Some(ms as i64),
                ..SearchLimits::default()
            };
            let caps = TimeCaps {
                soft: Duration::from_millis(ms.max(1)),
                hard: Duration::from_millis(ms.max(1)),
            };
            let ctx = SearchContext {
                stop: Arc::new(AtomicBool::new(false)),
                caps,
                virtual_clock: false,
                limits,
                history: vec![pos.zobrist()],
                tt: None,
            };
            let sink = |_: &str| {};
            let r = searcher.go(&pos, &ctx, &sink);
            depths.push(r.depth.min(u8::MAX as u32) as u8);
        }
        medians.push(median_depth(depths));
    }
    ladder_from_medians(&medians)
}

// ---------------------------------------------------------------------------
// Campaign driver — per-game-file architecture.
//
// Workers write completed games to `pending/game-{id:020}.bin`; a single
// consumer thread commits them in strict game_id order, routing dedup → cap →
// exact-target through the shared `LaneCommitter` into `lane.bin`. This makes
// the lane bytes a deterministic function of (seed, cap_seed, game_id),
// independent of worker count K.
// ---------------------------------------------------------------------------

/// Per-game depth rung, sampled deterministically from the seeded stream by
/// a cumulative-weight draw over the ladder. Deterministic given
/// `(rng-state, ladder)`.
fn sample_rung(ladder: &[(u8, u32)], rng: &mut Prng) -> u8 {
    debug_assert!(!ladder.is_empty(), "depth ladder must be non-empty");
    let total: u64 = ladder.iter().map(|&(_, w)| w as u64).sum();
    debug_assert!(total > 0, "ladder weights must sum to > 0");
    let mut draw = rng.below(total);
    for &(depth, w) in ladder {
        if draw < w as u64 {
            return depth;
        }
        draw -= w as u64;
    }
    unreachable!("cumulative draw < total always selects a rung")
}

/// RAII guard that decrements `alive_workers` on drop (covers panics and clean
/// exits). Declared BEFORE `ClaimGuard` in the worker scope so it drops AFTER
/// `ClaimGuard` (Rust drops in reverse declaration order — §11a.3).
pub struct AliveGuard {
    alive_workers: Arc<AtomicUsize>,
}

impl AliveGuard {
    /// Create an `AliveGuard` that decrements `alive_workers` when dropped.
    pub fn new(alive_workers: Arc<AtomicUsize>) -> Self {
        Self { alive_workers }
    }
}

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.alive_workers.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Per-worker outcome counters.
#[derive(Default)]
pub struct WorkerStats {
    /// Pending files successfully written (one per completed game).
    pub games_emitted: u64,
    /// Games abandoned in-flight due to stop signal (zero records emitted).
    pub games_dropped_inflight: u64,
    /// First fatal I/O error from this worker, if any; `None` on clean exit.
    pub fatal_io_error: Option<String>,
}

/// Single worker loop body.
///
/// Drop order (§11a.3): `_alive_guard` is declared first in the signature so
/// it drops AFTER the local `claim_guard` (last-declared drops first). This
/// ensures any released claim reaches `gap_queue` before `alive_workers`
/// decrements, preventing a spurious consumer-wedge race.
fn worker_loop(
    _worker_id: usize,
    dispatcher: &Dispatcher,
    cfg: &SelfPlayConfig,
    stop: &AtomicBool,
    pending_dir: &std::path::Path,
    _alive_guard: AliveGuard, // drops AFTER claim_guard (declared first)
    _alive_workers: &AtomicUsize,
) -> WorkerStats {
    let ladder = if cfg.depth_ladder.is_empty() {
        &[(4u8, 1u32)] as &[(u8, u32)]
    } else {
        &cfg.depth_ladder
    };
    let book_ref: Option<&Book> = cfg.book.as_deref();
    // Per-worker stop Arc so we can pass &Arc<AtomicBool> to play_one_game.
    let stop_arc = Arc::new(AtomicBool::new(false));
    let mut stats = WorkerStats::default();

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // claim_guard declared AFTER _alive_guard → drops FIRST (§11a.3).
        let claim_guard: ClaimGuard<'_> = match dispatcher.claim_next() {
            Some(g) => g,
            None => break,
        };
        let game_id = claim_guard.game_id();

        // Mirror the campaign stop.
        stop_arc.store(stop.load(Ordering::Relaxed), Ordering::Relaxed);

        let mut rng = Prng::new(substream_seed(cfg.seed, game_id));
        let depth = sample_rung(ladder, &mut rng);
        // Fresh searcher per game (R3: resumed game must not depend on TT
        // state from prior games on this worker).
        let mut searcher = AlphaBetaMover::new();
        let mut qsearcher = QSearcher::new();

        match play_one_game(
            &mut searcher,
            &mut qsearcher,
            book_ref,
            cfg.opening_mode,
            game_id,
            depth,
            cfg.opening_random_plies,
            cfg.max_plies,
            &mut rng,
            &stop_arc,
        ) {
            None => {
                // Stop-aborted in-flight game: R2 — zero records.
                // Drop claim_guard → release_unclaimed via Drop.
                drop(claim_guard);
                stats.games_dropped_inflight += 1;
            }
            Some(gr) => {
                let block = encode_block(game_id, &gr.records);
                match write_pending(pending_dir, game_id, &block) {
                    Ok(()) => {
                        claim_guard.notify_completed();
                        stats.games_emitted += 1;
                    }
                    Err(e) => {
                        // Fatal I/O: release claim via Drop, exit the worker.
                        drop(claim_guard);
                        stats.fatal_io_error = Some(e.to_string());
                        return stats;
                    }
                }
            }
        }
    }

    stats
}

/// Run the self-play campaign. Crash-safe (R1/R2), resumable (R3),
/// K-independent byte-identical output (the per-game-file property).
/// `stop` set by SIGTERM/SIGINT (graceful drop-in-flight + checkpoint flush).
pub fn run(cfg: &SelfPlayConfig, stop: &AtomicBool) -> Result<SelfPlayStats, CorpusError> {
    std::fs::create_dir_all(&cfg.out_dir)?;
    let pending_dir = cfg.out_dir.join("pending");
    std::fs::create_dir_all(&pending_dir)?;

    // Resume: reconstruct ConsumerState from shards + checkpoint + pending.
    let mut consumer_state = resume(&cfg.out_dir, &pending_dir)?;

    // Compute dispatcher seed from the resume state.
    // gap_list = [next_consume_id..upper) \ (committed_ids ∪ pending_ids).
    // The checkpoint is authoritative: anything < next_consume_id is already
    // "consumed" (either committed-with-records and present in committed_ids,
    // or committed-as-empty-post-dedup and tracked only by the checkpoint
    // cursor). Re-dispatching ids < next_consume_id would have workers
    // re-create ghost pending files that the consumer (starting at
    // next_consume_id) never processes — the file would persist in pending/
    // forever (violates ghost-pending-self-heal contract per plan §2.7).
    let pending_scan = super::pending::scan_pending(&pending_dir)?;
    let pending_ids = &pending_scan.ids;
    let committed_ids = &consumer_state.committed_ids_at_resume;
    let next = consumer_state.next_consume_id;
    let upper = {
        let max_committed = committed_ids.iter().copied().max().unwrap_or(0);
        let max_pending = pending_ids.iter().copied().max().unwrap_or(0);
        max_committed.max(max_pending).max(next.saturating_sub(1)) + 1
    };
    let next_dispatch_id = upper;
    let accounted: std::collections::HashSet<u64> = committed_ids
        .iter()
        .chain(pending_ids.iter())
        .copied()
        .collect();
    let gap_list: Vec<u64> = (next..upper).filter(|id| !accounted.contains(id)).collect();

    let dispatcher = Dispatcher::new(gap_list, next_dispatch_id, cfg.games);

    let n_workers = cfg.workers.max(1);
    let alive_workers = Arc::new(AtomicUsize::new(n_workers));

    // All threads borrow from the enclosing scope via plain references
    // (thread::scope guarantees the scope outlives all spawned threads).
    let alive_workers_ref = &alive_workers;
    let dispatcher_ref = &dispatcher;
    let pending_dir_ref = &pending_dir;
    let consumer_state_ref = &mut consumer_state;

    let (worker_stats_all, consumer_stats): (Vec<WorkerStats>, Result<ConsumerStats, CorpusError>) =
        std::thread::scope(|scope| {
            // Spawn consumer first (ready before workers start producing).
            let consumer_handle = scope.spawn(|| {
                consumer_loop(
                    cfg,
                    consumer_state_ref,
                    dispatcher_ref,
                    Arc::clone(alive_workers_ref),
                    stop,
                    &cfg.out_dir,
                )
            });

            // Spawn N workers.
            let mut worker_handles = Vec::with_capacity(n_workers);
            for w in 0..n_workers {
                let alive_guard = AliveGuard::new(Arc::clone(alive_workers_ref));
                let h = scope.spawn(move || {
                    worker_loop(
                        w,
                        dispatcher_ref,
                        cfg,
                        stop,
                        pending_dir_ref,
                        alive_guard,
                        alive_workers_ref,
                    )
                });
                worker_handles.push(h);
            }

            // Join workers first.
            let worker_results: Vec<WorkerStats> = worker_handles
                .into_iter()
                .map(|h| {
                    h.join().unwrap_or_else(|_| WorkerStats {
                        fatal_io_error: Some("worker thread panicked".into()),
                        ..WorkerStats::default()
                    })
                })
                .collect();

            // Join consumer.
            let cs = consumer_handle.join().unwrap_or_else(|_| {
                Err(CorpusError::Checkpoint("consumer thread panicked".into()))
            });

            (worker_results, cs)
        });

    // Propagate the first fatal worker I/O error.
    for ws in &worker_stats_all {
        if let Some(ref e) = ws.fatal_io_error {
            return Err(CorpusError::Checkpoint(format!("worker fatal error: {e}")));
        }
    }

    let consumer_stats = consumer_stats?;

    let games_emitted: u64 = worker_stats_all.iter().map(|w| w.games_emitted).sum();
    let games_dropped: u64 = worker_stats_all
        .iter()
        .map(|w| w.games_dropped_inflight)
        .sum();

    Ok(SelfPlayStats {
        games_emitted,
        games_dropped_inflight: games_dropped,
        games_committed: consumer_stats.games_committed,
        games_empty_post_dedup: consumer_stats.games_empty_post_dedup,
        positions_committed: consumer_stats.positions_committed,
        truncated_boundary_offset: consumer_stats.truncated_boundary_offset,
    })
}

#[cfg(test)]
mod tests {
    use super::super::store::scan_valid_blocks;
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static CTR: AtomicU64 = AtomicU64::new(0);
            let n = CTR.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let dir =
                std::env::temp_dir().join(format!("clawfish-corpus-selfplay-{tag}-{pid}-{n}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn cfg(out: PathBuf, games: u64, workers: usize) -> SelfPlayConfig {
        SelfPlayConfig {
            seed: 0xC0FFEE,
            games,
            workers,
            // Shallow depth keeps the test fast; the determinism property
            // is depth-independent.
            depth_ladder: vec![(2, 1)],
            opening_random_plies: 4,
            max_plies: 40,
            out_dir: out,
            opening_mode: OpeningMode::Random,
            book: None,
            poll_interval_ms: None,
            cap_seed: 7,
            cap_positions: None,
        }
    }

    fn lane_multiset(dir: &std::path::Path) -> Vec<(u64, String, u8, u32, u8, u8)> {
        let (blocks, _) = scan_valid_blocks(&dir.join("lane.bin")).unwrap();
        // Include depth_rung + strata in the comparison tuple so a
        // worker-count-dependent bug in rung sampling or stratum
        // computation surfaces as a 1-vs-N divergence (SF4).
        let mut v: Vec<(u64, String, u8, u32, u8, u8)> = blocks
            .iter()
            .flat_map(|b| {
                b.records.iter().map(|r| {
                    (
                        r.game_id,
                        r.fen.clone(),
                        r.label.as_u8(),
                        r.ply,
                        r.depth_rung,
                        r.strata,
                    )
                })
            })
            .collect();
        v.sort();
        v
    }

    // ── Determinism ────────────────────────────────────────────────────

    #[test]
    fn same_seed_same_game_bit_identical() {
        let mut sa = AlphaBetaMover::new();
        let mut sb = AlphaBetaMover::new();
        let mut qa = QSearcher::new();
        let mut qb = QSearcher::new();
        let stop = Arc::new(AtomicBool::new(false));
        let mut ra = Prng::new(substream_seed(123, 5));
        let mut rb = Prng::new(substream_seed(123, 5));
        let a = play_one_game(
            &mut sa,
            &mut qa,
            None,
            OpeningMode::Random,
            5,
            3,
            4,
            60,
            &mut ra,
            &stop,
        )
        .unwrap();
        let b = play_one_game(
            &mut sb,
            &mut qb,
            None,
            OpeningMode::Random,
            5,
            3,
            4,
            60,
            &mut rb,
            &stop,
        )
        .unwrap();
        assert_eq!(
            a, b,
            "same seed + depth ⇒ bit-identical game (fixed-depth determinism)"
        );
        // Inline filter is aggressive (in-check + |eval|≤600 + quiet); a short
        // shallow game may yield zero admitted records. The determinism
        // invariant is the load-bearing assertion above.
    }

    #[test]
    fn fresh_vs_warm_searcher_same_seed_same_game_identical() {
        // R3 invariant: `selfplay::run` constructs a fresh `AlphaBetaMover`
        // per game so a resumed game cannot depend on the TT/history/killers
        // accumulated by prior games on the warm worker (which a cold-start
        // resume would lack). Pin it: a `play_one_game(seed=N)` on a WARM
        // searcher (one already used for a prior game) must return the
        // IDENTICAL records as a `play_one_game(seed=N)` on a COLD one.
        let game_id: u64 = 5;
        let depth: u8 = 3;
        let opening: u32 = 4;
        let max_plies: u32 = 60;

        // Warm searcher + QSearcher: play a prior game first to populate
        // TT/history on both (the R3 analog now applies to the QSearcher too
        // because qsearch consults the TT — M5.F qsearch-in-TT).
        let mut warm = AlphaBetaMover::new();
        let mut warm_q = QSearcher::new();
        let stop = Arc::new(AtomicBool::new(false));
        let mut rng_warmup = Prng::new(substream_seed(123, 0));
        let _ = play_one_game(
            &mut warm,
            &mut warm_q,
            None,
            OpeningMode::Random,
            0,
            depth,
            opening,
            max_plies,
            &mut rng_warmup,
            &stop,
        )
        .expect("warmup game completes");
        let mut rng_warm = Prng::new(substream_seed(123, game_id));
        let g_warm = play_one_game(
            &mut warm,
            &mut warm_q,
            None,
            OpeningMode::Random,
            game_id,
            depth,
            opening,
            max_plies,
            &mut rng_warm,
            &stop,
        )
        .expect("warm-searcher game completes");

        // Cold searcher + QSearcher: same game_id, fresh state on both.
        let mut cold = AlphaBetaMover::new();
        let mut cold_q = QSearcher::new();
        let mut rng_cold = Prng::new(substream_seed(123, game_id));
        let g_cold = play_one_game(
            &mut cold,
            &mut cold_q,
            None,
            OpeningMode::Random,
            game_id,
            depth,
            opening,
            max_plies,
            &mut rng_cold,
            &stop,
        )
        .expect("cold-searcher game completes");

        assert_eq!(
            g_warm, g_cold,
            "game N on a warm `AlphaBetaMover` must equal game N on a fresh one — \
             a divergence here means TT/history/killers leaked across games, breaking \
             R3 bit-identical resume (the `selfplay::run` fresh-searcher-per-game fix)"
        );
    }

    #[test]
    fn worker_count_does_not_change_corpus() {
        let td1 = TempDir::new("w1");
        let td4 = TempDir::new("w4");
        let s1 = AtomicBool::new(false);
        let s4 = AtomicBool::new(false);
        run(&cfg(td1.0.clone(), 6, 1), &s1).unwrap();
        run(&cfg(td4.0.clone(), 6, 4), &s4).unwrap();
        assert_eq!(
            lane_multiset(&td1.0),
            lane_multiset(&td4.0),
            "1-worker and 4-worker runs must produce the IDENTICAL record \
             multiset (R7: per-game substream seed ⇒ scheduler-independent)"
        );
    }

    #[test]
    fn depth_rung_sampling_deterministic() {
        let ladder = vec![(4u8, 1u32), (6, 1), (8, 1), (10, 1)];
        let mut a = Prng::new(substream_seed(99, 0));
        let mut b = Prng::new(substream_seed(99, 0));
        let xs: Vec<u8> = (0..50).map(|_| sample_rung(&ladder, &mut a)).collect();
        let ys: Vec<u8> = (0..50).map(|_| sample_rung(&ladder, &mut b)).collect();
        assert_eq!(xs, ys, "rung sampling is a pure function of the seed");
        assert!(
            xs.iter().all(|d| [4, 6, 8, 10].contains(d)),
            "every sampled rung is a ladder rung"
        );
    }

    // ── Game-over labelling ────────────────────────────────────────────

    fn play_from_fen_to_terminal(fen: &str) -> Option<Label> {
        let pos = Position::from_fen(fen).unwrap();
        let hist = vec![pos.zobrist()];
        game_result(&pos, &hist)
    }

    #[test]
    fn game_over_mate_labeled_correctly() {
        // Fool's mate final position: White to move, White is mated ⇒
        // BlackWin (the side that just moved won).
        let fen = "rnb1kbnr/pppp1ppp/8/4p3/6Pq/5P2/PPPPP2P/RNBQKBNR w KQkq - 1 3";
        assert_eq!(play_from_fen_to_terminal(fen), Some(Label::BlackWin));
        // Mirror: Black to move and mated ⇒ WhiteWin.
        let scholar = "r1bqkb1r/pppp1Qpp/2n2n2/4p3/2B1P3/8/PPPP1PPP/RNB1K1NR b KQkq - 0 4";
        assert_eq!(play_from_fen_to_terminal(scholar), Some(Label::WhiteWin));
    }

    #[test]
    fn game_over_stalemate_labeled_correctly() {
        // Classic K+P stalemate, Black to move, not in check, no moves.
        let fen = "k7/P7/1K6/8/8/8/8/8 b - - 0 1";
        assert_eq!(play_from_fen_to_terminal(fen), Some(Label::Draw));
    }

    #[test]
    fn game_over_fifty_labeled_correctly() {
        // KQK with halfmove_clock=100 and legal moves ⇒ fifty-move draw.
        let fen = "8/8/8/4k3/8/4K3/8/7Q w - - 100 50";
        assert_eq!(play_from_fen_to_terminal(fen), Some(Label::Draw));
    }

    #[test]
    fn game_over_threefold_labeled_correctly() {
        // Synthetic history where the current position has occurred before
        // within the halfmove window ⇒ repetition draw (engine-internal
        // convention; plan §3.5 names `is_repetition`).
        let pos = Position::from_fen("8/8/8/4k3/8/4K3/8/7Q w - - 8 50").unwrap();
        let z = pos.zobrist();
        // The `1, 2, 3` middle entries are PLACEHOLDER Zobrist values for
        // the intervening positions — this test exercises `is_repetition`'s
        // algorithmic counting (same zobrist 4 plies back, within the
        // halfmove window), not the real-Zobrist-validity of those plies.
        let hist = vec![z, 1, 2, 3, z];
        assert_eq!(game_result(&pos, &hist), Some(Label::Draw));
    }

    #[test]
    fn game_over_insufficient_labeled_correctly() {
        // KK ⇒ insufficient material draw. (KQK is NOT insufficient.)
        let kk = "8/8/8/4k3/8/4K3/8/8 w - - 0 1";
        assert_eq!(play_from_fen_to_terminal(kk), Some(Label::Draw));
        let kqk = "8/8/8/4k3/8/4K3/8/7Q w - - 0 1";
        assert_eq!(play_from_fen_to_terminal(kqk), None);
    }

    #[test]
    fn game_over_maxplies_labeled_correctly() {
        // A game forced to hit max_plies adjudicates as a draw and still
        // emits its post-opening-skip positions.
        let mut s = AlphaBetaMover::new();
        let mut q = QSearcher::new();
        let stop = Arc::new(AtomicBool::new(false));
        let mut rng = Prng::new(substream_seed(7, 1));
        // max_plies just above the opening skip ⇒ a few emitted positions
        // then a forced draw adjudication. (The inline filter may drop some
        // — in-check / non-quiet / |eval|>HIGH — but the label-correctness
        // contract is what we assert.)
        let g = play_one_game(
            &mut s,
            &mut q,
            None,
            OpeningMode::Random,
            1,
            2,
            4,
            OPENING_SKIP_PLIES + 3,
            &mut rng,
            &stop,
        )
        .expect("max-plies game still completes (draw adjudication)");
        assert!(
            g.records.iter().all(|r| r.label == Label::Draw),
            "max-plies adjudication labels every emitted position a draw"
        );
        assert!(
            g.records.iter().all(|r| r.ply >= OPENING_SKIP_PLIES),
            "opening-skip honored: no position below OPENING_SKIP_PLIES"
        );
    }

    // ── OpeningMode + per-source tagging ────────────────────────────────

    fn synth_book(name: &str) -> super::super::openings::Book {
        // 3 hand-picked principled openings; sufficient for the OpeningMode
        // tagging tests below. Per-test unique dir to avoid cargo-test's
        // parallel-execution race when two book-using tests share a path.
        let dir = std::env::temp_dir().join(format!("clawfish-selfplay-book-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("book.epd");
        std::fs::write(
            &p,
            "rnbqkbnr/pp1ppppp/2p5/8/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2\n\
             rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2\n\
             rnbqkbnr/ppp1pppp/8/3p4/3P4/8/PPP1PPPP/RNBQKBNR w KQkq - 0 2\n",
        )
        .unwrap();
        super::super::openings::Book::load_epd(&p).expect("synth book loads")
    }

    #[test]
    fn opening_mode_source_matches_enum_variant() {
        // Lock the OpeningMode → Source mapping. Re-tagging the on-disk
        // taxonomy without updating this test will trip the assertion.
        assert_eq!(OpeningMode::Book.source(), Source::SelfPlayOnBook);
        assert_eq!(OpeningMode::Random.source(), Source::SelfPlayOffBook);
    }

    #[test]
    fn opening_mode_book_tags_records_as_on_book() {
        let mut s = AlphaBetaMover::new();
        let mut q = QSearcher::new();
        let stop = Arc::new(AtomicBool::new(false));
        let mut rng = Prng::new(substream_seed(0xB00C, 1));
        let book = synth_book("mode-book");
        let g = play_one_game(
            &mut s,
            &mut q,
            Some(&book),
            OpeningMode::Book,
            1,
            3,
            2,
            60,
            &mut rng,
            &stop,
        )
        .expect("book-seeded game completes");
        // The game may emit zero records on a shallow run; we assert the
        // source-tag invariant on every emitted record.
        for r in &g.records {
            assert_eq!(
                r.source,
                Source::SelfPlayOnBook,
                "OpeningMode::Book ⇒ every record carries Source::SelfPlayOnBook"
            );
        }
    }

    #[test]
    fn opening_mode_random_tags_records_as_off_book() {
        // Even with a book PROVIDED, OpeningMode::Random ignores it and
        // every record carries Source::SelfPlayOffBook.
        let mut s = AlphaBetaMover::new();
        let mut q = QSearcher::new();
        let stop = Arc::new(AtomicBool::new(false));
        let mut rng = Prng::new(substream_seed(0xB00C, 2));
        let book = synth_book("mode-random");
        let g = play_one_game(
            &mut s,
            &mut q,
            Some(&book),
            OpeningMode::Random,
            2,
            3,
            2,
            60,
            &mut rng,
            &stop,
        )
        .expect("game completes");
        for r in &g.records {
            assert_eq!(
                r.source,
                Source::SelfPlayOffBook,
                "OpeningMode::Random ⇒ every record carries Source::SelfPlayOffBook"
            );
        }
    }

    // ── Interrupt: zero records ─────────────────────────────────────────

    #[test]
    fn interrupted_game_emits_zero_records() {
        let mut s = AlphaBetaMover::new();
        let mut q = QSearcher::new();
        // stop already set before the game starts ⇒ the game is dropped
        // wholesale: ZERO records (R2).
        let stop = Arc::new(AtomicBool::new(true));
        let mut rng = Prng::new(substream_seed(1, 1));
        let g = play_one_game(
            &mut s,
            &mut q,
            None,
            OpeningMode::Random,
            1,
            4,
            4,
            80,
            &mut rng,
            &stop,
        );
        assert!(
            g.is_none(),
            "a stop-aborted in-flight game contributes ZERO records"
        );
    }

    #[test]
    fn interrupted_campaign_writes_only_complete_games() {
        // A campaign stopped immediately: no game can complete (the per-game
        // loop sees stop=true at its first guard) ⇒ the lane has zero
        // partial blocks.
        let td = TempDir::new("interrupt-campaign");
        let stop = AtomicBool::new(true);
        let stats = run(&cfg(td.0.clone(), 8, 2), &stop).unwrap();
        assert_eq!(stats.positions_committed, 0);
        let (blocks, _) = scan_valid_blocks(&td.0.join("lane.bin")).unwrap();
        assert!(
            blocks.is_empty(),
            "an immediately-stopped campaign commits zero (never partial) games"
        );
    }

    #[test]
    fn resume_skips_completed_game_ids_idempotent() {
        // First run completes some games; a second run over the same out_dir
        // must not re-emit (double) any already-durable game_id.
        let td = TempDir::new("resume-idem");
        let s = AtomicBool::new(false);
        run(&cfg(td.0.clone(), 4, 2), &s).unwrap();
        let first = lane_multiset(&td.0);
        let s2 = AtomicBool::new(false);
        run(&cfg(td.0.clone(), 4, 2), &s2).unwrap();
        let second = lane_multiset(&td.0);
        assert_eq!(
            first, second,
            "re-running the same campaign is idempotent (game_id-deduped)"
        );
    }

    // ── Ladder calibration ──────────────────────────────────────────────

    #[test]
    fn calibrate_ladder_deterministic_on_fixture() {
        // Pure rung-construction from a mocked per-bucket median table is
        // deterministic and order-preserving.
        let medians = [4u8, 6, 9, 12];
        let l1 = ladder_from_medians(&medians);
        let l2 = ladder_from_medians(&medians);
        assert_eq!(l1, l2);
        assert_eq!(l1, vec![(4, 1), (6, 1), (9, 1), (12, 1)]);
        // median_depth is a deterministic order statistic.
        assert_eq!(median_depth(vec![3, 1, 9, 5, 7]), 5);
        assert_eq!(median_depth(vec![8, 2]), 8); // upper-median on even n
    }

    #[test]
    fn calibrate_ladder_runs_and_is_structural() {
        // `calibrate_ladder` is wall-clock-dependent BY DESIGN (the
        // documented residual caveat in ADR-0035 — it measures completed
        // iterative-deepening depth at a movetime budget on the host).
        // Run-to-run jitter ±1 ply at small budgets is expected; that is
        // why the FROZEN dev-machine ladder ships in `manifest.json` and
        // the gate verifies AGAINST the frozen ladder, not a re-calibration.
        //
        // What we CAN pin here is structural: the function runs without
        // panicking, returns one rung per bucket, every rung has depth ≥ 1
        // and weight ≥ 1. The pure `ladder_from_medians` /
        // `median_depth` split (above) carries the determinism assertion.
        let buckets = [10u64, 20];
        let l = calibrate_ladder(&buckets);
        assert!(!l.is_empty(), "ladder must be non-empty");
        assert_eq!(l.len(), buckets.len(), "one rung per bucket");
        for (depth, weight) in &l {
            assert!(*depth >= 1, "every rung depth must be ≥ 1; got {depth}");
            assert!(*weight >= 1, "every rung weight must be ≥ 1; got {weight}");
        }
    }

    #[test]
    fn depth_rung_weights_match_deployment_profile() {
        // The deployment mixed-TC profile is equal-weighted across the
        // canonical 4 buckets (plan §3.5 "equal here").
        let l = ladder_from_medians(&[4, 6, 8, 10]);
        assert!(
            l.iter().all(|&(_, w)| w == 1),
            "all rungs equal-weighted (deployment mixed-TC profile)"
        );
        assert_eq!(l.len(), 4, "one rung per deployment TC bucket");
    }
}
