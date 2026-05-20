//! Work dispatcher for the per-game-file architecture.
//!
//! Hands out `game_id`s to workers via `claim_next`, tracking which ids are
//! in-flight (`claimed`) and which need replaying (`gap_queue`). The
//! `ClaimGuard` RAII wrapper ensures every claimed id is either committed
//! (via `notify_completed`) or returned to `gap_queue` (via `Drop`), so
//! panics and early-return error paths never leak a claim.
//!
//! See `docs/plans/m6.g-per-game-files.md` §2.5 for the full contract
//! (including the single-lock invariant and the drop-order note §11a.3).

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;

/// Dispatcher: hands out `game_id`s to workers in gap-first, then sequential
/// order. Thread-safe via an internal `Mutex` (contended only at game
/// boundaries, ~100 ms+, so overhead is negligible).
pub struct Dispatcher {
    games_target: u64,
    inner: Mutex<DispatcherInner>,
}

struct DispatcherInner {
    /// Plain `u64` under the lock; an `AtomicU64` is redundant (§2.5
    /// substantive #5 — the gap-vs-steady decision and cursor advance are
    /// inside the same critical section).
    next_dispatch_id: u64,
    gap_queue: VecDeque<u64>,
    claimed: HashSet<u64>,
}

impl Dispatcher {
    /// Create a new dispatcher.
    ///
    /// `gap_list`: game ids that were in-flight at resume and must be replayed
    /// before dispatching fresh ids. `next_dispatch_id`: the lowest fresh id
    /// not yet dispatched (= `upper` from the resume protocol §2.7 step 6).
    /// `games_target`: exclusive upper bound (dispatch returns `None` when all
    /// ids `[0, games_target)` are accounted for).
    ///
    /// **Contract:** the constructor sorts `gap_list` in ascending order;
    /// `claim_next`'s gap-queue `pop_front` therefore yields the smallest gap
    /// first (plan §2.5 — Spec decision 1).
    pub fn new(mut gap_list: Vec<u64>, next_dispatch_id: u64, games_target: u64) -> Self {
        gap_list.sort_unstable();
        Dispatcher {
            games_target,
            inner: Mutex::new(DispatcherInner {
                next_dispatch_id,
                gap_queue: gap_list.into_iter().collect(),
                claimed: HashSet::new(),
            }),
        }
    }

    /// Claim the next game id. Returns a `ClaimGuard` whose `Drop` releases
    /// the id back to `gap_queue` unless `notify_completed` was called.
    ///
    /// Returns `None` when all ids in `[0, games_target)` are accounted for
    /// (either committed or currently claimed).
    pub fn claim_next(&self) -> Option<ClaimGuard<'_>> {
        let mut inner = self.inner.lock().unwrap();
        let game_id = if let Some(g) = inner.gap_queue.pop_front() {
            g
        } else if inner.next_dispatch_id < self.games_target {
            let id = inner.next_dispatch_id;
            inner.next_dispatch_id += 1;
            id
        } else {
            return None;
        };
        inner.claimed.insert(game_id);
        Some(ClaimGuard {
            dispatcher: self,
            game_id,
            notified: false,
        })
    }

    /// Mark `game_id` as durably committed. Removes it from `claimed`.
    ///
    /// Called only from `ClaimGuard::notify_completed`; not intended for
    /// direct use.
    pub fn notify_completed(&self, game_id: u64) {
        self.inner.lock().unwrap().claimed.remove(&game_id);
    }

    /// Return `game_id` to `gap_queue` (it was claimed but not completed).
    ///
    /// Called from `ClaimGuard::Drop` on the non-happy path (panic, early
    /// return, stop abort). Also called by the consumer when it detects a torn
    /// pending file (CRC failure / decode error) — the consumer unlinks the
    /// file and enqueues the id for re-dispatch.
    pub fn release_unclaimed(&self, game_id: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.claimed.remove(&game_id);
        inner.gap_queue.push_back(game_id);
    }

    /// Returns `true` iff `gap_queue` is empty AND no `claimed` entry is
    /// `<= target_id`.
    ///
    /// Used by the consumer to detect wedge: if this returns `true` AND
    /// `alive_workers == 0` AND no pending file exists for `next_consume_id`,
    /// the consumer is wedged (a pending file will never appear).
    pub fn is_fully_drained_at(&self, target_id: u64) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.gap_queue.is_empty() && !inner.claimed.iter().any(|&c| c <= target_id)
    }

    /// Number of ids currently in the gap queue.
    ///
    /// Exposed for test inspection only. Not part of the production API.
    #[cfg(test)]
    pub fn gap_queue_len(&self) -> usize {
        self.inner.lock().unwrap().gap_queue.len()
    }
}

/// RAII claim guard. Borrowing from `&'d Dispatcher` so it composes with
/// `std::thread::scope` (no `Arc` needed — the dispatcher is stack-scoped
/// and its lifetime exceeds all worker threads).
///
/// On `Drop` (panic, early return, stop-abort): calls
/// `Dispatcher::release_unclaimed` to return the id to `gap_queue`.
/// After `notify_completed`: `std::mem::forget` suppresses `Drop`, so
/// `release_unclaimed` is NOT called.
///
/// **Drop order matters** (§11a.3): declare `ClaimGuard` AFTER `AliveGuard`
/// in the worker's local scope so it drops first (Rust drops in reverse
/// declaration order). This ensures the claim is released to `gap_queue`
/// before `alive_workers` is decremented, preventing a consumer wedge-check
/// race.
pub struct ClaimGuard<'d> {
    dispatcher: &'d Dispatcher,
    game_id: u64,
    notified: bool,
}

impl<'d> ClaimGuard<'d> {
    /// The claimed game id.
    pub fn game_id(&self) -> u64 {
        self.game_id
    }

    /// Consume the guard, marking the game as durably committed and
    /// suppressing the `Drop` release. Must be called exactly once after the
    /// pending file has been written and synced.
    pub fn notify_completed(mut self) {
        self.dispatcher.notify_completed(self.game_id);
        self.notified = true;
        std::mem::forget(self);
    }
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        if !self.notified {
            self.dispatcher.release_unclaimed(self.game_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Basic dispatch ───────────────────────────────────────────────────

    #[test]
    fn smallest_gap_dispatched_first() {
        // Gaps must be dispatched before fresh ids.
        let d = Dispatcher::new(vec![3, 1, 7], 10, 20);
        let id = d.claim_next().unwrap().game_id();
        // The smallest gap (1) must come out first.
        assert_eq!(id, 1, "smallest gap must be dispatched first");
    }

    #[test]
    fn budget_exhaustion_returns_none() {
        // When all ids are accounted for, claim_next returns None.
        // next_dispatch_id == games_target → all ids accounted for.
        let d = Dispatcher::new(vec![], 5, 5); // ids 0..5 all dispatched already
        assert!(
            d.claim_next().is_none(),
            "claim_next must return None when budget is exhausted (next_dispatch_id == games_target)"
        );
    }

    #[test]
    fn no_two_workers_same_id_concurrent() {
        // Property (via threads, not proptest): K concurrent workers never
        // receive the same game_id. Run 1000 games with 10 workers.
        use std::sync::{Arc, Mutex};
        let games: u64 = 1000;
        let workers = 10;
        let d = Arc::new(Dispatcher::new(vec![], 0, games));
        let seen = Arc::new(Mutex::new(std::collections::HashSet::<u64>::new()));
        let duplicates = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        std::thread::scope(|scope| {
            for _ in 0..workers {
                let d = Arc::clone(&d);
                let seen = Arc::clone(&seen);
                let dups = Arc::clone(&duplicates);
                scope.spawn(move || {
                    while let Some(guard) = d.claim_next() {
                        let id = guard.game_id();
                        {
                            let mut s = seen.lock().unwrap();
                            if !s.insert(id) {
                                dups.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        guard.notify_completed();
                    }
                });
            }
        });

        assert_eq!(
            duplicates.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "no two workers must receive the same game_id"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.len(),
            games as usize,
            "all game ids dispatched exactly once"
        );
    }

    #[test]
    fn release_reinserts_into_gap_queue() {
        // Dropping a ClaimGuard without notify_completed must re-enqueue
        // the id so it can be reclaimed.
        let d = Dispatcher::new(vec![], 0, 5);
        let id = {
            let guard = d.claim_next().unwrap();
            guard.game_id()
            // guard drops here → release_unclaimed
        };
        // The released id must be reclaimable again.
        let reclaimed = d.claim_next().unwrap().game_id();
        assert_eq!(
            reclaimed, id,
            "released id must be re-dispatched (gap preference)"
        );
    }

    #[test]
    fn notify_removes_from_claimed() {
        // After notify_completed, the id must not remain in `claimed`
        // and must not be re-dispatched (it is permanently committed).
        let d = Dispatcher::new(vec![], 0, 3);
        let guard = d.claim_next().unwrap();
        let id = guard.game_id();
        guard.notify_completed();
        // is_fully_drained_at should not see id in claimed.
        assert!(
            d.is_fully_drained_at(id),
            "after notify_completed, id must not remain in claimed"
        );
    }

    #[test]
    fn is_fully_drained_at_semantics() {
        // Returns true iff gap_queue is empty AND no claimed entry <= target.
        let d = Dispatcher::new(vec![], 0, 10);
        // Nothing dispatched yet → drained at any id? No, gap_queue is empty
        // but claimed is also empty, so yes.
        assert!(d.is_fully_drained_at(0));
        // Claim id 0.
        let guard = d.claim_next().unwrap();
        assert_eq!(guard.game_id(), 0);
        // Now 0 is claimed → not fully drained at 0.
        assert!(!d.is_fully_drained_at(0));
        // But drained at a higher id? No — claimed={0} which is <= 5.
        assert!(!d.is_fully_drained_at(5));
        guard.notify_completed();
        // Now claimed is empty again.
        assert!(d.is_fully_drained_at(0));
    }

    // ── ClaimGuard RAII ──────────────────────────────────────────────────

    #[test]
    fn claim_guard_drop_releases_on_panic() {
        // Blocker #3: if the worker panics mid-game, the ClaimGuard's Drop
        // must call release_unclaimed via stack-unwinding — not just an
        // explicit drop(). This test exercises the actual unwind path via
        // std::panic::catch_unwind so the assertion is meaningful.
        let d = Dispatcher::new(vec![], 0, 10);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let guard = d.claim_next().expect("first claim");
            let game_id = guard.game_id();
            assert_eq!(game_id, 0);
            panic!("simulated mid-game panic");
        }));
        assert!(result.is_err(), "catch_unwind must catch the panic");
        // After unwinding, the claim must have been returned to gap_queue
        // via ClaimGuard::drop → release_unclaimed. A subsequent claim_next
        // must yield the same id again.
        let next_guard = d.claim_next().expect("second claim after panic unwind");
        assert_eq!(
            next_guard.game_id(),
            0,
            "panicked claim must be re-dispatched via Drop-triggered release_unclaimed"
        );
    }

    #[test]
    fn claim_guard_drop_releases_on_early_return() {
        // Blocker #3 (early-return path, e.g. write_pending I/O error).
        let d = Dispatcher::new(vec![], 0, 3);
        let released = std::cell::Cell::new(u64::MAX);

        {
            let guard = d.claim_next().unwrap();
            released.set(guard.game_id());
            // Early return: guard dropped here without notify_completed.
        }

        // The released id must be the next dispatch.
        let reclaimed = d.claim_next().unwrap().game_id();
        assert_eq!(
            reclaimed,
            released.get(),
            "early-return drop releases claim"
        );
    }

    #[test]
    fn claim_guard_notify_completed_does_not_release() {
        // After notify_completed, mem::forget suppresses Drop. The id must
        // NOT be re-enqueued to gap_queue.
        let d = Dispatcher::new(vec![], 0, 2);
        let guard = d.claim_next().unwrap();
        let id = guard.game_id();
        guard.notify_completed();
        // Claim the next id — it must NOT be `id` again (id is committed,
        // not released).
        if let Some(next) = d.claim_next() {
            assert_ne!(
                next.game_id(),
                id,
                "notify_completed must NOT re-enqueue the id"
            );
        }
    }

    #[test]
    fn concurrent_claim_vs_release_upholds_gap_preference() {
        // Push a gap mid-flight and assert it's preferred over fresh ids.
        // This is a single-threaded simulation of the race: claim a fresh
        // id, inject a gap, drop the guard, verify gap is dispatched next.
        let d = Dispatcher::new(vec![], 5, 20);
        // Claim id 5 (the first fresh).
        let guard = d.claim_next().unwrap();
        assert_eq!(guard.game_id(), 5);
        // Inject gap 2 (simulating a resume-discovered gap).
        d.release_unclaimed(2); // normally called by Drop, but we call directly to simulate
        // Drop the guard for id 5 (also releases to gap).
        drop(guard);
        // Next dispatch must prefer the smallest gap.
        let next = d.claim_next().unwrap().game_id();
        assert_eq!(next, 2, "gap 2 must be preferred over fresh ids");
    }
}
