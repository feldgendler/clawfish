//! In-process deterministic fixed-depth self-play corpus generator
//! (R1–R7, R-TC).
//!
//! Two-pass: this emits EVERY post-opening-skip position with the game
//! label + `depth_rung`, transactionally per game (does NOT apply the quiet
//! predicate — `build` does, so this slice has no `quiet` dependency).
//! Determinism precondition: `SearchLimits{ depth: Some(d), nodes: None,
//! movetime: None, infinite: false }` with `TimeCaps{soft:MAX,hard:MAX}` ⇒
//! `should_abort` only via `ctx.stop`; a `stop`-aborted in-flight game is
//! DROPPED (R2). Fixed-depth ⇒ load/suspend/renice-independent ⇒ R3/R4
//! without `VirtualClock`.
//!
//! Implemented by the M6.G `selfplay+store` (Opus) coder slice per
//! `docs/plans/m6.g.md` §3.5.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use super::CorpusError;

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
    /// Seeded-random opening plies from startpos (diversification).
    pub opening_random_plies: u32,
    /// Max half-moves before adjudicating an over-long game.
    pub max_plies: u32,
    /// Output directory (shard log + checkpoint).
    pub out_dir: PathBuf,
    /// Fraction of games routed to the held-out self-play validation set.
    pub val_fraction: f64,
}

/// Self-play campaign outcome counters.
#[derive(Clone, Debug, Default)]
pub struct SelfPlayStats {
    /// Games that reached a natural terminal result and were committed.
    pub games_completed: u64,
    /// Games abandoned in-flight (interrupt) — contributed ZERO labels.
    pub games_dropped_inflight: u64,
    /// Total positions emitted (pre-filter; `build` does quiet/cap/dedup).
    pub positions_emitted: u64,
}

/// Empirically measure clawfish's median completed iterative-deepening
/// depth at each deployment movetime bucket over `bench::BENCH_POSITIONS`.
/// Pins the R-TC ladder (recorded in the manifest); re-runnable.
pub fn calibrate_ladder(_buckets_ms: &[u64]) -> Vec<(u8, u32)> {
    todo!("M6.G selfplay slice")
}

/// Run the self-play campaign. Crash-safe (R1/R2), resumable (R3),
/// all-cores renice-friendly (R7). `stop` set by SIGTERM/SIGINT (graceful
/// drop+flush).
pub fn run(_cfg: &SelfPlayConfig, _stop: &AtomicBool) -> Result<SelfPlayStats, CorpusError> {
    todo!("M6.G selfplay slice")
}
