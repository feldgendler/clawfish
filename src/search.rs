//! Search trait and value types.
//!
//! Defined at M2.C so M2.D's random-mover and M3+'s alpha-beta plug into the
//! orchestrator without trait churn. `SearchContext` carries the cancellation
//! flag, deadline, start time, and parsed `SearchLimits`; `Search::go` is
//! polled by the orchestrator's worker thread and must obey `should_abort`
//! (per ADR-0011 and `docs/plans/m2.c.md` §3).
//!
//! M3.A ships the [`GreedyMover`] implementation: a depth-1 best-eval mover
//! with reservoir-sampled tie-break PRNG. See plan §10.

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
    /// in practice means "search briefly" — GreedyMover treats `Some(<= 0)`
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
    /// no-op. GreedyMover resets `state` to `seed` here. M3+ alpha-beta
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

// ---------------------------------------------------------------------------
// GreedyMover — depth-1 best-eval with reservoir-sampled tie-break (M3.A).
// ---------------------------------------------------------------------------

/// Depth-1 best-eval mover. Evaluates every legal move at depth 1 via
/// `-evaluate(post_make)`, picks the move with the highest score, and
/// breaks ties with SplitMix64 reservoir sampling (one PRNG step per tied
/// move). Production `Search` impl introduced in M3.A.
pub(crate) struct GreedyMover {
    /// The seed configured via `Random_Seed` (or the default, 0).
    seed: u64,
    /// PRNG state. Advances by one SplitMix64 step per tied move encountered.
    state: u64,
}

impl GreedyMover {
    pub(crate) fn new(seed: u64) -> Self {
        Self { seed, state: seed }
    }
}

impl Search for GreedyMover {
    fn go(
        &mut self,
        position: &Position,
        ctx: &SearchContext,
        info_sink: &dyn Fn(&str),
    ) -> SearchResult {
        use crate::eval::evaluate;

        let mut moves = {
            let mut ml = crate::movegen::MoveList::new();
            crate::movegen::generate_moves(position, &mut ml);
            ml.iter().collect::<Vec<_>>()
        };

        if let Some(ref filter) = ctx.limits.searchmoves {
            moves.retain(|mv| filter.contains(mv));
        }

        if moves.is_empty() {
            let elapsed_ms = ctx.start.elapsed().as_millis();
            info_sink(&format!(
                "info depth 0 score cp 0 nodes 0 time {elapsed_ms} pv 0000"
            ));
            return SearchResult {
                bestmove: None,
                depth: 0,
                score_cp: Some(0),
                nodes: 0,
                ponder: None,
            };
        }

        // Reservoir-sampled best-eval scan. One pos_clone mutated in place via
        // make/unmake — not N separate clones. Required to keep the debug-build
        // round-trip assert hot loop sane.
        let mut pos_clone = *position;

        let undo0 = pos_clone.make_move(moves[0]);
        // Negate: post-make perspective is the opponent; we want our perspective.
        let mut best_score = -evaluate(&pos_clone);
        pos_clone.unmake_move(moves[0], undo0);
        let mut best_mv = moves[0];
        let mut tie_count: u32 = 1;

        for &mv in &moves[1..] {
            let undo = pos_clone.make_move(mv);
            let score = -evaluate(&pos_clone);
            pos_clone.unmake_move(mv, undo);

            if score > best_score {
                best_score = score;
                best_mv = mv;
                tie_count = 1;
            } else if score == best_score {
                tie_count += 1;
                let r = splitmix64_next(&mut self.state);
                // Modulo bias for tie_count ≤ 218 is < 4×10⁻¹⁸ — no rejection needed.
                if (r as usize).is_multiple_of(tie_count as usize) {
                    best_mv = mv;
                }
            }
        }

        let n = moves.len() as u64;

        // Emit the info line BEFORE the wait loop — the score is real and
        // a GUI watching `info score cp` should see it immediately.
        let elapsed_ms = ctx.start.elapsed().as_millis();
        info_sink(&format!(
            "info depth 1 score cp {best_score} nodes {n} time {elapsed_ms} pv {}",
            best_mv.to_uci()
        ));

        // Wait loop: honor infinite / movetime / ponder.
        let wait = ctx.limits.infinite || ctx.limits.movetime.is_some() || ctx.limits.ponder;
        if wait {
            while !ctx.should_abort(0) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }

        SearchResult {
            bestmove: Some(best_mv),
            depth: 1,
            score_cp: Some(best_score),
            nodes: n,
            ponder: None,
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

    // -----------------------------------------------------------------------
    // D1 — SplitMix64 reference-output anchor.
    // -----------------------------------------------------------------------

    /// Pin the first 8 outputs of `splitmix64_next` from `state = 0` against
    /// hand-computed reference values. Catches any constant-typo regression in
    /// the PRNG without needing the full `GreedyMover` machinery.
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

    // ===========================================================================
    // M3.A — GreedyMover tests (D13–D22, E1, F1).
    //
    // All tests in this section will fail until the GreedyMover::go impl ships
    // (Slice 3), because the stub returns SearchResult::default() (bestmove=None).
    // ===========================================================================

    fn go_greedy_no_sink(
        gm: &mut GreedyMover,
        pos: &Position,
        ctx: &SearchContext,
    ) -> SearchResult {
        gm.go(pos, ctx, &|_| {})
    }

    // -----------------------------------------------------------------------
    // D13 — GreedyMover picks a legal move from startpos.
    // -----------------------------------------------------------------------

    #[test]
    fn greedy_picks_a_legal_move_from_startpos() {
        let pos = Position::starting_position();
        let (ctx, _stop) = non_aborting_ctx();
        let mut gm = GreedyMover::new(0);
        let result = go_greedy_no_sink(&mut gm, &pos, &ctx);

        let mv = result
            .bestmove
            .expect("startpos has 20 legal moves — bestmove must be Some");

        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        assert!(
            ml.iter().any(|legal| legal == mv),
            "greedy bestmove {} is not in generate_moves output for startpos",
            mv.to_uci()
        );
    }

    // -----------------------------------------------------------------------
    // D14 — GreedyMover emits None in checkmate.
    // -----------------------------------------------------------------------

    #[test]
    fn greedy_emits_none_in_checkmate() {
        // Fool's mate — white is in checkmate (no legal moves).
        let fen = "rnb1kbnr/pppp1ppp/8/4p3/6Pq/5P2/PPPPP2P/RNBQKBNR w KQkq - 1 3";
        let pos = Position::from_fen(fen).expect("Fool's-mate FEN must parse");

        let (ctx, _stop) = non_aborting_ctx();
        let result = GreedyMover::new(0).go(&pos, &ctx, &|_| {});

        assert_eq!(
            result.bestmove, None,
            "bestmove must be None in a checkmate position"
        );
        assert_eq!(
            result.score_cp,
            Some(0),
            "score_cp must be Some(0) in a checkmate position (empty move list)"
        );
        assert_eq!(
            result.nodes, 0,
            "nodes must be 0 when no moves are evaluated"
        );
    }

    // -----------------------------------------------------------------------
    // D15 — GreedyMover returns candidate even with pre-set cancellation.
    // -----------------------------------------------------------------------

    #[test]
    fn greedy_returns_candidate_even_with_pre_set_cancellation() {
        let pos = Position::starting_position();
        let stop = Arc::new(AtomicBool::new(true)); // pre-set
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            deadline: None,
            start: Instant::now(),
            limits: SearchLimits::default(), // no infinite/movetime/ponder
        };

        let result = GreedyMover::new(0).go(&pos, &ctx, &|_| {});
        assert!(
            result.bestmove.is_some(),
            "bestmove must be Some even when the stop flag is pre-set (no wait loop)"
        );
    }

    // -----------------------------------------------------------------------
    // D16 — GreedyMover honors `infinite` until cancelled.
    // -----------------------------------------------------------------------

    #[test]
    fn greedy_honors_infinite_until_cancelled() {
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
            std::thread::spawn(move || GreedyMover::new(0).go(&pos, &ctx, &|_| {}).bestmove);

        std::thread::sleep(Duration::from_millis(20));
        stop_clone.store(true, Ordering::Relaxed);

        let result = handle.join().expect("worker thread must not panic");
        assert!(
            result.is_some(),
            "bestmove must be Some even after cancellation from infinite mode"
        );

        let elapsed = spawn_time.elapsed();
        assert!(
            elapsed >= Duration::from_millis(15),
            "infinite go must block until cancelled (elapsed {elapsed:?} < 15 ms — \
            worker likely returned immediately without polling should_abort)"
        );
    }

    // -----------------------------------------------------------------------
    // D17 — GreedyMover seed is deterministic for tied positions.
    // -----------------------------------------------------------------------

    /// Uses KvK with white king on e4. All 8 white-king moves leave a KvK
    /// position with kings symmetrically placed → all 8 candidate scores are
    /// equal. Two same-seed instances must produce identical bestmove
    /// sequences across 5 calls. Pins reservoir-sampling determinism. (The
    /// "evaluates to 0" insufficient-material claim is pinned by A4, not here;
    /// D17 only requires that the 8 scores tie, which holds whether `evaluate`
    /// returns 0 or some non-zero color-symmetric value.)
    #[test]
    fn greedy_seed_is_deterministic_for_tied_positions() {
        // KvK: white king on e4, black king on e1. All white king moves tie.
        let pos =
            Position::from_fen("8/8/8/8/4K3/8/8/4k3 w - - 0 1").expect("KvK (ties) FEN must parse");
        let (ctx, _stop) = non_aborting_ctx();

        let mut gm_a = GreedyMover::new(42);
        let mut gm_b = GreedyMover::new(42);

        for call in 0..5 {
            let mv_a = go_greedy_no_sink(&mut gm_a, &pos, &ctx).bestmove;
            let mv_b = go_greedy_no_sink(&mut gm_b, &pos, &ctx).bestmove;
            assert_eq!(
                mv_a, mv_b,
                "D17: determinism violated at call {call}: gm_a={mv_a:?}, gm_b={mv_b:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // D18 — GreedyMover takes a hanging queen.
    // -----------------------------------------------------------------------

    /// White knight on c4, black queen on e5, white to move. The knight can
    /// capture the queen with Nxe5 (c4e5) — a gain of +1025 material (queen
    /// value) at depth 1. All other legal moves (king moves) leave the queen
    /// alive. Nxe5 is therefore clearly the depth-1 best move. Assert
    /// bestmove == c4e5 (knight captures the queen), which proves GreedyMover
    /// actually evaluates positions rather than picking at random.
    #[test]
    fn greedy_takes_hanging_queen() {
        let pos = Position::from_fen("4k3/8/8/4q3/2N5/8/8/4K3 w - - 0 1")
            .expect("FEN with hanging queen must parse");
        let (ctx, _stop) = non_aborting_ctx();
        let mut gm = GreedyMover::new(0);
        let result = go_greedy_no_sink(&mut gm, &pos, &ctx);

        let mv = result
            .bestmove
            .expect("position has legal moves — bestmove must be Some");
        let expected = crate::mov::Move::from_uci("c4e5", &pos).expect("c4e5 must be legal (Nxe5)");
        assert_eq!(
            mv,
            expected,
            "D18: greedy must take the hanging queen (c4e5 = Nxe5); got {}",
            mv.to_uci()
        );
    }

    // -----------------------------------------------------------------------
    // D19 — empty searchmoves yields None, no PRNG advance.
    // -----------------------------------------------------------------------

    #[test]
    fn greedy_searchmoves_empty_yields_none_no_prng_advance() {
        let pos = Position::starting_position();
        let (ctx_no_filter, _stop_nf) = non_aborting_ctx();
        let (ctx_empty_filter, _stop_ef) = ctx_with_searchmoves(vec![]);

        let mut tested = GreedyMover::new(7);
        let mut reference = GreedyMover::new(7);

        // Empty filter must return None.
        let empty_result = go_greedy_no_sink(&mut tested, &pos, &ctx_empty_filter).bestmove;
        assert_eq!(
            empty_result, None,
            "D19: empty searchmoves must yield bestmove None"
        );

        // Subsequent unrestricted calls must agree (no PRNG state advanced).
        let m1 = go_greedy_no_sink(&mut tested, &pos, &ctx_no_filter).bestmove;
        let m2 = go_greedy_no_sink(&mut reference, &pos, &ctx_no_filter).bestmove;
        assert_eq!(
            m1, m2,
            "D19: PRNG state must not advance on empty-filter go: tested={m1:?}, reference={m2:?}"
        );
    }

    // -----------------------------------------------------------------------
    // D20 — GreedyMover emits an info depth 1 line with score cp.
    // -----------------------------------------------------------------------

    #[test]
    fn greedy_emits_info_depth_1_with_score_cp() {
        use crate::eval::evaluate;
        use crate::movegen::{MoveList, generate_moves};

        let pos = Position::starting_position();
        let (ctx, _stop) = non_aborting_ctx();
        let mut gm = GreedyMover::new(0);

        let info_lines: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        gm.go(&pos, &ctx, &|line| {
            info_lines.borrow_mut().push(line.to_string())
        });
        let info_lines = info_lines.into_inner();

        assert_eq!(
            info_lines.len(),
            1,
            "D20: exactly one info line must be emitted; got: {info_lines:?}"
        );
        let line = &info_lines[0];
        assert!(
            line.starts_with("info depth 1 score cp "),
            "D20: info line must start with 'info depth 1 score cp '; got: {line:?}"
        );
        assert!(
            line.contains("nodes"),
            "D20: info line must contain 'nodes'; got: {line:?}"
        );
        assert!(
            line.contains("time"),
            "D20: info line must contain 'time'; got: {line:?}"
        );
        assert!(
            line.contains("pv"),
            "D20: info line must contain 'pv'; got: {line:?}"
        );

        // Extract the score integer and verify it matches the independently-computed
        // depth-1 best score. An impl that always emits `score cp 0` (the M2.D
        // placeholder pattern) would fail this check for startpos.
        let score_str = line
            .strip_prefix("info depth 1 score cp ")
            .expect("already checked prefix above");
        let score_in_line: i32 = score_str
            .split_whitespace()
            .next()
            .expect("token after 'score cp' must exist")
            .parse()
            .expect("score must be an integer");

        // Independently compute the depth-1 best score: for each legal move,
        // score = -evaluate(after make_move). The max over all legal moves is
        // the expected value.
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let mut pos_clone = pos;
        let mut best = i32::MIN;
        for mv in ml.iter() {
            let undo = pos_clone.make_move(mv);
            let s = -evaluate(&pos_clone);
            pos_clone.unmake_move(mv, undo);
            if s > best {
                best = s;
            }
        }

        assert_eq!(
            score_in_line, best,
            "D20: score cp in info line ({score_in_line}) must match \
            independently-computed depth-1 best score ({best})"
        );
    }

    // -----------------------------------------------------------------------
    // D21 — set_seed resets PRNG state.
    // -----------------------------------------------------------------------

    #[test]
    fn greedy_set_seed_resets_state() {
        const SEED: u64 = 42;
        // KvK tied position for reliable determinism across seeds.
        let pos = Position::from_fen("8/8/8/8/4K3/8/8/4k3 w - - 0 1").expect("KvK FEN must parse");
        let (ctx, _stop) = non_aborting_ctx();

        // Capture the bestmove from a fresh instance.
        let first_mv_fresh = go_greedy_no_sink(&mut GreedyMover::new(SEED), &pos, &ctx).bestmove;

        // Advance a second instance 5 times, then call set_seed.
        let mut gm = GreedyMover::new(SEED);
        for _ in 0..5 {
            go_greedy_no_sink(&mut gm, &pos, &ctx);
        }
        gm.set_seed(SEED);

        let mv_after_set_seed = go_greedy_no_sink(&mut gm, &pos, &ctx).bestmove;
        assert_eq!(
            mv_after_set_seed, first_mv_fresh,
            "D21: set_seed must reset state: expected {first_mv_fresh:?}, got {mv_after_set_seed:?}"
        );
    }

    // -----------------------------------------------------------------------
    // D22 — reset() returns to seed.
    // -----------------------------------------------------------------------

    #[test]
    fn greedy_reset_returns_to_seed() {
        const SEED: u64 = 42;
        let pos = Position::from_fen("8/8/8/8/4K3/8/8/4k3 w - - 0 1").expect("KvK FEN must parse");
        let (ctx, _stop) = non_aborting_ctx();

        let first_mv_fresh = go_greedy_no_sink(&mut GreedyMover::new(SEED), &pos, &ctx).bestmove;

        let mut gm = GreedyMover::new(SEED);
        for _ in 0..5 {
            go_greedy_no_sink(&mut gm, &pos, &ctx);
        }
        gm.reset();

        let mv_after_reset = go_greedy_no_sink(&mut gm, &pos, &ctx).bestmove;
        assert_eq!(
            mv_after_reset, first_mv_fresh,
            "D22: reset must restore state to seed: expected {first_mv_fresh:?}, got {mv_after_reset:?}"
        );
    }

    // -----------------------------------------------------------------------
    // E1 — proptest: GreedyMover picks the move with max -evaluate(post_make).
    // -----------------------------------------------------------------------

    mod proptest_e1 {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig {
                cases: 256,
                ..ProptestConfig::default()
            })]
            /// For any reachable position with ≥1 legal move, the bestmove
            /// GreedyMover returns must have `-evaluate(post_make)` equal to
            /// `max(-evaluate(post_make_for_each_legal))`. Pins the depth-1
            /// best-eval contract.
            #[test]
            fn prop_greedy_picks_max_score_among_legal_moves(
                seed in 0u64..u64::MAX,
            ) {
                use crate::eval::evaluate;
                use crate::movegen::{MoveList, generate_moves};

                const FENS: &[&str] = &[
                    "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
                    "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
                    "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
                    "r2q1rk1/pP1p2pp/Q4n2/bbp1p3/Np6/1B3NBn/pPPP1PPP/R3K2R b KQ - 0 1",
                    "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
                    "r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10",
                ];

                let fen = FENS[(seed as usize) % FENS.len()];
                let pos = Position::from_fen(fen).unwrap();

                let mut ml = MoveList::new();
                generate_moves(&pos, &mut ml);
                prop_assume!(!ml.is_empty());

                // Compute the max score across all legal moves.
                let mut pos_clone = pos;
                let max_score = {
                    let mut best = i32::MIN;
                    for mv in ml.iter() {
                        let undo = pos_clone.make_move(mv);
                        let score = -evaluate(&pos_clone);
                        pos_clone.unmake_move(mv, undo);
                        if score > best {
                            best = score;
                        }
                    }
                    best
                };

                // Ask GreedyMover for its choice.
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
                let result = GreedyMover::new(seed).go(&pos, &ctx, &|_| {});
                let chosen = result.bestmove.expect("position has legal moves");

                // Verify the chosen move achieves the max score.
                let undo = pos_clone.make_move(chosen);
                let chosen_score = -evaluate(&pos_clone);
                pos_clone.unmake_move(chosen, undo);

                prop_assert_eq!(
                    chosen_score,
                    max_score,
                    "E1: greedy chose move {} with score {} but max available is {}",
                    chosen.to_uci(),
                    chosen_score,
                    max_score
                );
            }
        }
    }
}
