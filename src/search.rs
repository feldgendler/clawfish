//! Search trait and value types.
//!
//! Defined at M2.C so M2.D's random-mover and M3+'s alpha-beta plug into the
//! orchestrator without trait churn. `SearchContext` carries the cancellation
//! flag, deadline, start time, and parsed `SearchLimits`; `Search::go` is
//! polled by the orchestrator's worker thread and must obey `should_abort`
//! (per ADR-0011 and `docs/plans/m2.c.md` §3).
//!
//! M2.D ships the [`RandomMover`] implementation: a SplitMix64-seeded uniform
//! random pick from the legal move list. See plan §5.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::{Move, Position};

/// Parsed `go` parameters routed into search. Constructed by `handle_go` from
/// `GoParams`; `searchmoves` is already validated against the current
/// position (bad entries silently dropped — plan §6).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchLimits {
    pub depth: Option<u32>,
    pub nodes: Option<u64>,
    /// `go movetime <ms>`. Signed to mirror `wtime`/`btime` (the UCI clock
    /// can go negative under time-trouble overshoot); negative `movetime`
    /// in practice means "search briefly" — RandomMover treats `Some(<= 0)`
    /// the same as `Some(0)`.
    pub movetime: Option<i64>,
    pub mate: Option<u32>,
    pub infinite: bool,
    pub ponder: bool,
    pub wtime: Option<i64>,
    pub btime: Option<i64>,
    pub winc: Option<u64>,
    pub binc: Option<u64>,
    pub movestogo: Option<u32>,
    /// Restrict candidate moves to this set. `None` = no restriction.
    /// `Some(empty)` is a degenerate case — search should emit `bestmove
    /// 0000` since no candidate exists.
    pub searchmoves: Option<Vec<Move>>,
}

/// Per-`go` context. Cloned into the worker thread.
#[derive(Clone)]
pub struct SearchContext {
    /// Flipped by the orchestrator on `stop` / time expiry. Polled by
    /// `should_abort`. Cleared by the orchestrator at the start of each
    /// `go`. See plan §7 for the cleared-then-spawned ordering.
    pub stop: Arc<AtomicBool>,
    /// Wallclock cap, computed from `movetime` (M2.D / M3 will compute
    /// from `wtime`/`btime`/`winc` etc.). `None` = no time cap.
    pub deadline: Option<Instant>,
    /// `Instant::now()` at the moment `handle_go` built the context. Used
    /// by future `info time` emission (M3+).
    pub start: Instant,
    pub limits: SearchLimits,
}

impl SearchContext {
    /// `true` ⇒ cancel this iteration immediately. `nodes_searched` is the
    /// caller's running node count; compared against `self.limits.nodes`
    /// (the cap from `go nodes <N>`). `Relaxed` ordering is sufficient
    /// per ADR-0011 §"Ordering and safety".
    #[inline]
    pub fn should_abort(&self, nodes_searched: u64) -> bool {
        if self.stop.load(Ordering::Relaxed) {
            return true;
        }
        if let Some(d) = self.deadline
            && Instant::now() >= d
        {
            return true;
        }
        if let Some(cap) = self.limits.nodes
            && nodes_searched >= cap
        {
            return true;
        }
        false
    }
}

/// Result of one `go` invocation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchResult {
    /// `None` ⇒ the orchestrator emits `bestmove 0000` (spec line 49).
    /// `Some(mv)` ⇒ the orchestrator emits `bestmove <uci>`.
    pub bestmove: Option<Move>,
    pub ponder: Option<Move>,
    pub depth: u32,
    pub score_cp: Option<i32>,
    pub nodes: u64,
}

/// Search interface every implementation honors. `Send` is required because
/// implementations are moved into per-`go` worker threads via
/// `thread::spawn`. See ADR-0011 §"`Search` trait — committed at M2".
pub trait Search: Send {
    /// Run a search. Must obey `ctx`: poll cancellation, respect deadline,
    /// emit `info` lines via `info_sink`, return cleanly on cancellation.
    /// Must not write stdout directly. Must not read stdin.
    fn go(
        &mut self,
        position: &Position,
        ctx: &SearchContext,
        info_sink: &dyn Fn(&str),
    ) -> SearchResult;

    /// Notify the search of a new `Random_Seed` value. Default: no-op
    /// (most search impls don't have configurable seeds). M3+ alpha-beta
    /// will not implement this; it stays a no-op.
    fn set_seed(&mut self, _seed: u64) {}

    /// Notify the search that a new game has started (`ucinewgame`). Default:
    /// no-op. RandomMover resets `state` to `seed` here. M3+ alpha-beta
    /// will eventually clear killer/history/TT here.
    fn reset(&mut self) {}
}

/// SplitMix64 PRNG step. Constants vendored verbatim from prng.di.unimi.it
/// (research §3.1, plan §3). Public-domain; same provenance as the Polyglot
/// Zobrist table. `wrapping_*` is required — overflow is intentional.
#[inline]
pub(crate) fn splitmix64_next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// SplitMix64-seeded uniform random-mover. Replaces `Stub` in M2.D.
///
/// Behavior (per plan §5):
/// - Generates legal moves; filters by `ctx.limits.searchmoves` if `Some`.
/// - Picks one move uniformly at random via one SplitMix64 step.
/// - `splitmix64_next` is called only when the (post-filter) move list is
///   non-empty, so empty filters do not consume PRNG state.
/// - If `infinite` / `movetime` / `ponder` is set: spins on `should_abort`
///   with a 1 ms sleep cadence until cancelled / deadline expires.
/// - Candidate is always computed before the cancellation wait — race-free
///   under quit-immediately-after-go (M2.C precedent, plan §8).
pub(crate) struct RandomMover {
    /// The seed configured via `Random_Seed` (or the default, 0). Reset
    /// target for `reset()`.
    seed: u64,
    /// PRNG state. Initially equal to `seed`; advances by one SplitMix64
    /// step per `go` call (when moves are non-empty).
    state: u64,
}

impl RandomMover {
    pub(crate) fn new(seed: u64) -> Self {
        Self { seed, state: seed }
    }
}

impl Search for RandomMover {
    fn go(
        &mut self,
        position: &Position,
        ctx: &SearchContext,
        info_sink: &dyn Fn(&str),
    ) -> SearchResult {
        // Always compute the candidate before checking cancellation. The
        // alternative (early-exit on pre-set stop) creates a race against
        // `handle_quit` — if `quit` flips the flag before this thread is
        // scheduled, the worker would emit `bestmove 0000` instead of a
        // legitimate move. See plan §8.
        let mut moves = {
            let mut ml = crate::movegen::MoveList::new();
            crate::movegen::generate_moves(position, &mut ml);
            ml.iter().collect::<Vec<_>>()
        };

        if let Some(ref filter) = ctx.limits.searchmoves {
            moves.retain(|mv| filter.contains(mv));
        }

        let candidate = if moves.is_empty() {
            None
        } else {
            let r = splitmix64_next(&mut self.state);
            // Modulo bias for N ≤ 218 (legal-move max) is < 4×10⁻¹⁸
            // (plan §3.1) — no rejection sampling needed.
            let idx = (r as usize) % moves.len();
            Some(moves[idx])
        };

        let wait = ctx.limits.infinite || ctx.limits.movetime.is_some() || ctx.limits.ponder;
        if wait {
            while !ctx.should_abort(0) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }

        // Post-wait: time reflects the full wall-clock spent in this go, symmetric
        // with how M3+ iterative-deepening will report info time on its final iteration.
        let elapsed_ms = ctx.start.elapsed().as_millis();
        let pv_token = candidate
            .map(|m| m.to_uci())
            .unwrap_or_else(|| "0000".to_string());
        info_sink(&format!(
            "info depth 0 score cp 0 nodes 1 time {elapsed_ms} pv {pv_token}"
        ));

        SearchResult {
            bestmove: candidate,
            ..Default::default()
        }
    }

    fn set_seed(&mut self, seed: u64) {
        self.seed = seed;
        self.state = seed;
    }

    fn reset(&mut self) {
        self.state = self.seed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::movegen::{MoveList, generate_moves};
    use crate::{Move, Position};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn non_aborting_ctx() -> (SearchContext, Arc<AtomicBool>) {
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            deadline: None,
            start: Instant::now(),
            limits: SearchLimits::default(),
        };
        (ctx, stop)
    }

    fn ctx_with_searchmoves(moves: Vec<Move>) -> (SearchContext, Arc<AtomicBool>) {
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            deadline: None,
            start: Instant::now(),
            limits: SearchLimits {
                searchmoves: Some(moves),
                ..SearchLimits::default()
            },
        };
        (ctx, stop)
    }

    fn go_no_sink(rm: &mut RandomMover, pos: &Position, ctx: &SearchContext) -> Option<Move> {
        rm.go(pos, ctx, &|_| {}).bestmove
    }

    // -----------------------------------------------------------------------
    // D1 — SplitMix64 reference-output anchor.
    // -----------------------------------------------------------------------

    /// Pin the first 8 outputs of `splitmix64_next` from `state = 0` against
    /// hand-computed reference values. Catches any constant-typo regression in
    /// the PRNG without needing the full `RandomMover` machinery.
    ///
    /// Reference computed offline by running the same algorithm in a standalone
    /// Rust binary; values verified stable across two independent runs.
    #[test]
    fn splitmix64_first_8_outputs_from_seed_0_match_reference() {
        // Each value is the output *before* any masking — the raw 64-bit PRNG
        // output. The state starts at 0 and advances by the golden-ratio
        // constant (0x9E3779B97F4A7C15) on each step before the mixing.
        #[rustfmt::skip]
        let expected: [u64; 8] = [
            0xE220A8397B1DCDAF, // step 0 (state after = 0x9E3779B97F4A7C15)
            0x6E789E6AA1B965F4, // step 1
            0x06C45D188009454F, // step 2
            0xF88BB8A8724C81EC, // step 3
            0x1B39896A51A8749B, // step 4
            0x53CB9F0C747EA2EA, // step 5
            0x2C829ABE1F4532E1, // step 6
            0xC584133AC916AB3C, // step 7
        ];

        let mut state = 0u64;
        for (i, &want) in expected.iter().enumerate() {
            let got = splitmix64_next(&mut state);
            assert_eq!(
                got, want,
                "splitmix64_next output mismatch at step {i}: got 0x{got:016X}, want 0x{want:016X}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // D2 — RandomMover picks a legal move from startpos.
    // -----------------------------------------------------------------------

    #[test]
    fn random_mover_picks_a_legal_move_from_startpos() {
        let pos = Position::starting_position();
        let (ctx, _stop) = non_aborting_ctx();
        let mut rm = RandomMover::new(0);
        let result = rm.go(&pos, &ctx, &|_| {});

        let mv = result
            .bestmove
            .expect("startpos has 20 legal moves — bestmove must be Some");

        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        assert!(
            ml.iter().any(|legal| legal == mv),
            "bestmove {mv} is not in generate_moves output for startpos",
            mv = mv.to_uci()
        );
    }

    // -----------------------------------------------------------------------
    // D3 — RandomMover emits None in checkmate.
    // -----------------------------------------------------------------------

    #[test]
    fn random_mover_emits_none_in_checkmate() {
        // Fool's mate — white is in checkmate (no legal moves).
        let fen = "rnb1kbnr/pppp1ppp/8/4p3/6Pq/5P2/PPPPP2P/RNBQKBNR w KQkq - 1 3";
        let pos = Position::from_fen(fen).expect("Fool's-mate FEN must parse");

        let (ctx, _stop) = non_aborting_ctx();
        let result = RandomMover::new(0).go(&pos, &ctx, &|_| {});

        assert_eq!(
            result.bestmove, None,
            "bestmove must be None in a checkmate position"
        );
    }

    // -----------------------------------------------------------------------
    // D4 — Candidate computed even with pre-set cancellation.
    // -----------------------------------------------------------------------

    /// Pins the "always compute candidate first" invariant (plan §5 / M2.C
    /// precedent). With no infinite/movetime/ponder, the wait loop is skipped
    /// entirely, so a pre-set stop flag must not suppress the bestmove.
    #[test]
    fn random_mover_returns_candidate_even_with_pre_set_cancellation() {
        let pos = Position::starting_position();
        let stop = Arc::new(AtomicBool::new(true)); // pre-set
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            deadline: None,
            start: Instant::now(),
            limits: SearchLimits::default(), // no infinite/movetime/ponder
        };

        let result = RandomMover::new(0).go(&pos, &ctx, &|_| {});
        assert!(
            result.bestmove.is_some(),
            "bestmove must be Some even when the stop flag is pre-set (no wait loop)"
        );
    }

    // -----------------------------------------------------------------------
    // D5 — RandomMover honors `infinite` until cancelled.
    // -----------------------------------------------------------------------

    #[test]
    fn random_mover_honors_infinite_until_cancelled() {
        let pos = Position::starting_position();
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            deadline: None,
            start: Instant::now(),
            limits: SearchLimits {
                infinite: true,
                ..SearchLimits::default()
            },
        };

        let stop_clone = Arc::clone(&stop);
        let spawn_time = Instant::now();
        let handle =
            std::thread::spawn(move || RandomMover::new(0).go(&pos, &ctx, &|_| {}).bestmove);

        std::thread::sleep(Duration::from_millis(20));
        stop_clone.store(true, Ordering::Relaxed);

        // Must return within 100 ms after cancellation (1 ms sleep cadence).
        let result = handle.join().expect("worker thread must not panic");
        assert!(
            result.is_some(),
            "bestmove must be Some even after cancellation from infinite mode"
        );

        // The worker must have been in the polling loop for at least 15 ms —
        // catches a RandomMover::go that ignores `infinite=true` and returns
        // immediately instead of waiting. We sleep 20 ms before cancelling, so
        // the elapsed time must be at least ~18 ms; 15 ms absorbs scheduling
        // jitter.
        let elapsed = spawn_time.elapsed();
        assert!(
            elapsed >= Duration::from_millis(15),
            "infinite go must block until cancelled (elapsed {elapsed:?} < 15 ms — \
            worker likely returned immediately without polling should_abort)"
        );
    }

    // -----------------------------------------------------------------------
    // D6 — seed 42 is deterministic across two independent instances.
    // -----------------------------------------------------------------------

    #[test]
    fn random_mover_seed_42_is_deterministic() {
        // Use a varied sequence of positions so different PRNG branches are
        // exercised. Two instances with the same seed must produce identical
        // bestmoves on the same sequence.
        let positions: Vec<Position> = [
            "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
            "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq e3 0 1",
            "r1bqkbnr/pppp1ppp/2n5/4p3/4P3/5N2/PPPP1PPP/RNBQKB1R w KQkq - 2 3",
            "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
            "r2q1rk1/pP1p2pp/Q4n2/bbp1p3/Np6/1B3NBn/pPPP1PPP/R3K2R b KQ - 0 1",
            "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
            "r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10",
            "rnbqkbnr/pp1ppppp/8/2p5/4P3/8/PPPP1PPP/RNBQKBNR w KQkq c6 0 2",
            "5k2/8/8/8/8/8/8/4K2R w K - 0 1",
        ]
        .iter()
        .map(|fen| {
            Position::from_fen(fen).unwrap_or_else(|e| panic!("FEN must parse: {fen} ({e:?})"))
        })
        .collect();

        let mut rm_a = RandomMover::new(42);
        let mut rm_b = RandomMover::new(42);
        let (ctx, _stop) = non_aborting_ctx();

        for (i, pos) in positions.iter().enumerate() {
            let mv_a = go_no_sink(&mut rm_a, pos, &ctx);
            let mv_b = go_no_sink(&mut rm_b, pos, &ctx);
            assert_eq!(
                mv_a, mv_b,
                "determinism violated at position {i}: rm_a={mv_a:?}, rm_b={mv_b:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // D7 — set_seed resets PRNG state.
    // -----------------------------------------------------------------------

    #[test]
    fn random_mover_set_seed_resets_state() {
        let seed = 7u64;
        let pos = Position::starting_position();
        let (ctx, _stop) = non_aborting_ctx();

        // Capture the bestmove produced by a fresh instance at seed 7.
        let first_mv_fresh = go_no_sink(&mut RandomMover::new(seed), &pos, &ctx);

        // Advance the tested instance 5 times, then reset via set_seed.
        let mut rm = RandomMover::new(seed);
        for _ in 0..5 {
            go_no_sink(&mut rm, &pos, &ctx);
        }
        rm.set_seed(seed);

        // The next go must reproduce the first move of a fresh seed-7 instance.
        let mv_after_set_seed = go_no_sink(&mut rm, &pos, &ctx);
        assert_eq!(
            mv_after_set_seed, first_mv_fresh,
            "set_seed must reset state: expected {first_mv_fresh:?}, got {mv_after_set_seed:?}"
        );
    }

    // -----------------------------------------------------------------------
    // D8 — reset() returns to seed.
    // -----------------------------------------------------------------------

    #[test]
    fn random_mover_reset_returns_to_seed() {
        let seed = 7u64;
        let pos = Position::starting_position();
        let (ctx, _stop) = non_aborting_ctx();

        // Capture the bestmove produced by a fresh instance at seed 7.
        let first_mv_fresh = go_no_sink(&mut RandomMover::new(seed), &pos, &ctx);

        // Advance the tested instance 5 times, then reset.
        let mut rm = RandomMover::new(seed);
        for _ in 0..5 {
            go_no_sink(&mut rm, &pos, &ctx);
        }
        rm.reset();

        let mv_after_reset = go_no_sink(&mut rm, &pos, &ctx);
        assert_eq!(
            mv_after_reset, first_mv_fresh,
            "reset must restore state to seed: expected {first_mv_fresh:?}, got {mv_after_reset:?}"
        );
    }

    // -----------------------------------------------------------------------
    // D9 — empty searchmoves yields None and does not advance PRNG state.
    // -----------------------------------------------------------------------

    /// Pins that `splitmix64_next` is only called when the post-filter move
    /// list is non-empty. An empty `searchmoves` filter must not consume any
    /// PRNG state, so the subsequent unrestricted `go` must produce the same
    /// bestmove as if the empty-filter call had never happened.
    #[test]
    fn random_mover_empty_searchmoves_yields_none_and_does_not_advance_state() {
        let pos = Position::starting_position();
        let (ctx_no_filter, _stop_nf) = non_aborting_ctx();
        let (ctx_empty_filter, _stop_ef) = ctx_with_searchmoves(vec![]);

        let mut tested = RandomMover::new(7);
        let mut reference = RandomMover::new(7);

        // Step 1: empty-filter call must return None.
        let empty_result = go_no_sink(&mut tested, &pos, &ctx_empty_filter);
        assert_eq!(
            empty_result, None,
            "empty searchmoves must yield bestmove None"
        );

        // Steps 2 & 3: subsequent unrestricted calls must agree (proving no
        // PRNG state was consumed in step 1).
        let m1 = go_no_sink(&mut tested, &pos, &ctx_no_filter);
        let m2 = go_no_sink(&mut reference, &pos, &ctx_no_filter);
        assert_eq!(
            m1, m2,
            "PRNG state must not advance on empty-filter go: tested={m1:?}, reference={m2:?}"
        );
    }

    // -----------------------------------------------------------------------
    // D10 — searchmoves filter restricts candidates to the specified subset.
    // -----------------------------------------------------------------------

    #[test]
    fn random_mover_filter_restricts_to_searchmoves_subset() {
        let pos = Position::starting_position();
        let a2a4 = Move::from_uci("a2a4", &pos).expect("a2a4 must be a legal startpos move");
        let b2b4 = Move::from_uci("b2b4", &pos).expect("b2b4 must be a legal startpos move");

        // Build once; reuse across all seeds (avoids repeated parsing).
        let filter = vec![a2a4, b2b4];

        for seed in 0u64..100 {
            let (ctx, _stop) = ctx_with_searchmoves(filter.clone());
            let mv = go_no_sink(&mut RandomMover::new(seed), &pos, &ctx)
                .unwrap_or_else(|| panic!("seed {seed}: searchmoves [a2a4,b2b4] must yield Some"));
            assert!(
                mv == a2a4 || mv == b2b4,
                "seed {seed}: bestmove {uci} not in {{a2a4, b2b4}}",
                uci = mv.to_uci()
            );
        }
    }

    // -----------------------------------------------------------------------
    // D11 — should_abort three sub-cases (carried over from M2.C verbatim).
    // -----------------------------------------------------------------------

    // D11 is carried over verbatim from M2.C — it tests SearchContext, not Search.
    #[test]
    fn should_abort_three_subcases() {
        // Sub-case 1: stop flag set.
        {
            let stop = Arc::new(AtomicBool::new(false));
            let ctx = SearchContext {
                stop: Arc::clone(&stop),
                deadline: None,
                start: Instant::now(),
                limits: SearchLimits::default(),
            };
            assert!(!ctx.should_abort(0), "should not abort before stop is set");
            stop.store(true, Ordering::Relaxed);
            assert!(ctx.should_abort(0), "should abort after stop flag is set");
        }

        // Sub-case 2: deadline already expired.
        {
            let stop = Arc::new(AtomicBool::new(false));
            let expired = Instant::now() - Duration::from_millis(1);
            let ctx = SearchContext {
                stop: Arc::clone(&stop),
                deadline: Some(expired),
                start: Instant::now(),
                limits: SearchLimits::default(),
            };
            assert!(
                ctx.should_abort(0),
                "should abort when deadline is already in the past"
            );
        }

        // Sub-case 3: node cap.
        {
            let stop = Arc::new(AtomicBool::new(false));
            let ctx = SearchContext {
                stop: Arc::clone(&stop),
                deadline: None,
                start: Instant::now(),
                limits: SearchLimits {
                    nodes: Some(500),
                    ..SearchLimits::default()
                },
            };
            assert!(
                ctx.should_abort(1_000),
                "should abort when nodes_searched (1000) >= cap (500)"
            );
            assert!(
                !ctx.should_abort(100),
                "should not abort when nodes_searched (100) < cap (500)"
            );
        }
    }

    // -----------------------------------------------------------------------
    // D12 — chi-square uniformity proptest.
    // -----------------------------------------------------------------------

    /// Verify that `RandomMover` picks legal moves with an approximately
    /// uniform distribution. Uses startpos (K = 20 legal moves). Draws M =
    /// 2000 samples per seed, computes chi-square, asserts χ² < 43.82.
    ///
    /// Critical value at α = 0.001, df = K−1 = 19: **43.82** (from standard
    /// chi-square tables, e.g. NIST Handbook §1.3.6.7.4).
    ///
    /// The bound is intentionally loose (α = 0.001, not 0.05) to keep
    /// per-seed false-fail probability below ~1/1000, and proptest is capped
    /// at 8 cases to keep wallclock low (8 seeds × 2000 picks ≈ 16k PRNG
    /// steps, negligible).
    ///
    /// Each sample reuses the same `RandomMover` instance (state advances
    /// sample-to-sample) to measure genuine PRNG dispersion, not restarts.
    #[cfg(test)]
    mod proptest_d12 {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig {
                cases: 8,
                ..ProptestConfig::default()
            })]
            #[test]
            fn prop_random_mover_chi_square_within_tolerance_for_uniform_pick(
                seed in any::<u64>(),
            ) {
                const K: usize = 20;     // legal moves from startpos
                const M: usize = 2000;   // samples
                // Critical value at α = 0.001, df = K−1 = 19 (NIST table).
                const CHI2_CRIT: f64 = 43.82;

                let pos = Position::starting_position();
                let (ctx, _stop) = {
                    let stop = Arc::new(AtomicBool::new(false));
                    let ctx = SearchContext {
                        stop: Arc::clone(&stop),
                        deadline: None,
                        start: std::time::Instant::now(),
                        limits: SearchLimits::default(),
                    };
                    (ctx, stop)
                };

                // Build a stable move-index map using MoveList order.
                let mut ml = MoveList::new();
                generate_moves(&pos, &mut ml);
                prop_assert_eq!(
                    ml.len(),
                    K,
                    "startpos must have exactly 20 legal moves (got {})",
                    ml.len()
                );
                let move_list: Vec<Move> = ml.iter().collect();

                let mut counts = vec![0u64; K];
                let mut rm = RandomMover::new(seed);

                for _ in 0..M {
                    let mv = rm.go(&pos, &ctx, &|_| {})
                        .bestmove
                        .expect("startpos always has legal moves");
                    let idx = move_list
                        .iter()
                        .position(|&m| m == mv)
                        .expect("bestmove must be in the MoveList");
                    counts[idx] += 1;
                }

                let expected = M as f64 / K as f64;
                let chi2: f64 = counts.iter().map(|&c| {
                    let diff = c as f64 - expected;
                    diff * diff / expected
                }).sum();

                prop_assert!(
                    chi2 < CHI2_CRIT,
                    "seed {seed}: chi-square {chi2:.2} >= {CHI2_CRIT} (α=0.001, df=19); counts={counts:?}"
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // Seed-pair finder (documentation / one-off helper).
    // -----------------------------------------------------------------------

    /// Helper: run RandomMover::go from startpos with the given seed and no
    /// restrictions, returning the bestmove UCI string.
    fn bestmove_from_startpos(seed: u64) -> Option<String> {
        let pos = Position::starting_position();
        let (ctx, _stop) = non_aborting_ctx();
        let result = RandomMover::new(seed).go(&pos, &ctx, &|_| {});
        result.bestmove.map(|m| m.to_uci())
    }

    /// Find the first seed that terminates (reaches checkmate or stalemate)
    /// within 500 ply of startpos self-play. Used for E36 anchor calibration.
    /// Gated #[ignore] — run manually; leave in place as documentation.
    #[test]
    #[ignore]
    fn find_terminating_seed_for_e36() {
        use crate::movegen::{MoveList, generate_moves};
        let max_ply = 500usize;

        for seed in 0u64..200 {
            let mut rm = RandomMover::new(seed);
            let mut pos = Position::starting_position();
            let stop = Arc::new(AtomicBool::new(false));

            let mut terminated = false;
            let mut final_ply = 0usize;
            let mut final_mv = String::new();

            for ply in 0..max_ply {
                let mut ml = MoveList::new();
                generate_moves(&pos, &mut ml);
                if ml.is_empty() {
                    final_ply = ply;
                    terminated = true;
                    break;
                }
                let ctx = SearchContext {
                    stop: Arc::clone(&stop),
                    deadline: None,
                    start: std::time::Instant::now(),
                    limits: SearchLimits::default(),
                };
                let result = rm.go(&pos, &ctx, &|_| {});
                let mv = result.bestmove.expect("must have bestmove");
                final_mv = mv.to_uci();
                pos.make_move(mv);
            }

            if terminated {
                println!("FOUND: seed={seed} final_ply={final_ply} last_bestmove={final_mv:?}");
                return;
            } else {
                println!("seed={seed}: cycled at {max_ply} ply");
            }
        }
        println!("No terminating seed found in 0..200");
    }

    /// One-off seed-pair finder for plan §B.2.e. Gated #[ignore] — run
    /// manually during Phase 1 to capture SEED_A / SEED_B, then leave in
    /// place as documentation.
    #[test]
    #[ignore]
    fn find_seed_pair_for_b2e() {
        let mut found: Option<(u64, u64, String, String)> = None;
        let mut cache: Vec<String> = Vec::new();
        for s in 0u64..1000 {
            let mv = bestmove_from_startpos(s).expect("startpos always has legal moves");
            cache.push(mv);
        }
        'outer: for s_a in 0u64..1000 {
            for s_b in (s_a + 1)..1000 {
                if cache[s_a as usize] != cache[s_b as usize] {
                    found = Some((
                        s_a,
                        s_b,
                        cache[s_a as usize].clone(),
                        cache[s_b as usize].clone(),
                    ));
                    break 'outer;
                }
            }
        }
        match found {
            Some((a, b, mv_a, mv_b)) => {
                println!("SEED_A={a} bestmove={mv_a}  SEED_B={b} bestmove={mv_b}");
            }
            None => println!("No differing pair found in 0..1000"),
        }
    }
}
