//! Search trait and value types.
//!
//! Defined at M2.C so M2.D's random-mover and M3+'s alpha-beta plug into the
//! orchestrator without trait churn. `SearchContext` carries the cancellation
//! flag, deadline, start time, and parsed `SearchLimits`; `Search::go` is
//! polled by the orchestrator's worker thread and must obey `should_abort`
//! (per ADR-0011 and `docs/plans/m2.c.md` §3).
//!
//! M2.C ships the [`Stub`] implementation: deterministic
//! lexicographically-first legal move; honors `infinite` / `movetime` /
//! `ponder` by polling `should_abort` until cancelled. See plan §8.

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
    /// in practice means "search briefly" — Stub treats `Some(<= 0)` the
    /// same as `Some(0)`.
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
}

/// M2.C placeholder. Replaced in M2.D by the random-mover.
///
/// Behavior (per plan §8):
/// - Generates legal moves; filters by `ctx.limits.searchmoves` if `Some`.
/// - Picks the lexicographically-first by UCI notation as the candidate
///   bestmove (`None` if no candidate exists).
/// - If `infinite` / `movetime` / `ponder` is set: spins on `should_abort`
///   with a 1 ms sleep cadence until cancelled / deadline expires, then
///   emits the candidate.
/// - Else: emits the candidate immediately.
pub(crate) struct Stub;

impl Search for Stub {
    fn go(
        &mut self,
        position: &Position,
        ctx: &SearchContext,
        _info_sink: &dyn Fn(&str),
    ) -> SearchResult {
        // Always compute the candidate before checking cancellation. The
        // alternative (early-exit on pre-set stop) creates a race against
        // `handle_quit` — if `quit` flips the flag before this thread is
        // scheduled, the worker would emit `bestmove 0000` instead of the
        // legitimate lex-first move. See plan §8.
        let mut moves = {
            let mut ml = crate::movegen::MoveList::new();
            crate::movegen::generate_moves(position, &mut ml);
            ml.iter().collect::<Vec<_>>()
        };

        if let Some(ref filter) = ctx.limits.searchmoves {
            moves.retain(|mv| filter.contains(mv));
        }

        let candidate = moves
            .iter()
            .min_by(|a, b| a.to_uci().cmp(&b.to_uci()))
            .copied();

        let wait = ctx.limits.infinite || ctx.limits.movetime.is_some() || ctx.limits.ponder;
        if wait {
            while !ctx.should_abort(0) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }

        SearchResult {
            bestmove: candidate,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Position;
    use crate::movegen::{MoveList, generate_moves};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
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

    #[test]
    fn stub_picks_first_legal_move_by_uci() {
        let pos = Position::starting_position();
        let (ctx, _stop) = non_aborting_ctx();

        // Sanity: confirm the lex-first UCI move from startpos is a2a3.
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let expected_uci = ml
            .iter()
            .map(|mv| mv.to_uci())
            .min()
            .expect("startpos must have legal moves");
        assert_eq!(
            expected_uci, "a2a3",
            "sanity: lex-first UCI from startpos must be a2a3"
        );

        let result = Stub.go(&pos, &ctx, &|_| {});
        assert_eq!(
            result.bestmove.map(|m| m.to_uci()),
            Some("a2a3".to_string()),
            "Stub should pick lex-first legal move a2a3 from startpos"
        );
    }

    #[test]
    fn stub_emits_none_in_checkmate() {
        // Fool's mate: black queen delivers checkmate, white is mated.
        let pos =
            Position::from_fen("rnb1kbnr/pppp1ppp/8/4p3/6Pq/5P2/PPPPP2P/RNBQKBNR w KQkq - 1 3")
                .expect("fool's-mate FEN is valid");
        let (ctx, _stop) = non_aborting_ctx();

        let result = Stub.go(&pos, &ctx, &|_| {});
        assert_eq!(
            result.bestmove, None,
            "Stub should return None bestmove when the side to move is checkmated"
        );
    }

    #[test]
    fn stub_returns_candidate_even_with_pre_set_cancellation() {
        // Per plan §8: Stub always computes the lex-first candidate before
        // checking cancellation. Pre-set stop only short-circuits the wait
        // loop (which a bare `go` doesn't enter anyway), not the candidate
        // computation. This is the race-free design — the alternative
        // (early-exit on pre-set stop) creates a quit-vs-go scheduling race
        // that makes integration tests flaky.
        let pos = Position::starting_position();
        let stop = Arc::new(AtomicBool::new(true)); // pre-set before the call
        let ctx = SearchContext {
            stop: Arc::clone(&stop),
            deadline: None,
            start: Instant::now(),
            limits: SearchLimits::default(),
        };

        let wallclock_deadline = Instant::now() + Duration::from_millis(50);
        let result = Stub.go(&pos, &ctx, &|_| {});

        assert!(
            Instant::now() < wallclock_deadline,
            "Stub should return immediately for bare go even with pre-set stop; took too long"
        );
        assert_eq!(
            result.bestmove.map(|m| m.to_uci()),
            Some("a2a3".to_string()),
            "Stub must compute the lex-first candidate even when stop is pre-set"
        );
    }

    #[test]
    fn stub_honors_infinite_until_cancelled() {
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
        let handle = thread::spawn(move || Stub.go(&pos, &ctx, &|_| {}));

        // Give the search time to enter its polling loop before cancelling.
        thread::sleep(Duration::from_millis(20));
        stop_clone.store(true, Ordering::Relaxed);

        // The search should return within 100 ms of cancellation.
        let join_deadline = Instant::now() + Duration::from_millis(100);
        loop {
            if handle.is_finished() {
                break;
            }
            assert!(
                Instant::now() < join_deadline,
                "Stub did not return within 100 ms after stop was set"
            );
            thread::sleep(Duration::from_millis(2));
        }

        let result = handle.join().expect("search thread should not panic");
        assert_eq!(
            result.bestmove.map(|m| m.to_uci()),
            Some("a2a3".to_string()),
            "Stub should return lex-first move a2a3 even when cancelled after infinite loop"
        );
    }

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
}
