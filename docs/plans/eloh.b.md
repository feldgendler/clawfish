# ELOH.B — Online iteration + concurrency + progress + threshold adjudication

The statistical layer on top of ELOH.A's correctness layer. Adds Robbins-Monro K-update at single-game cadence, σ-based stopping, parallel-pair concurrency, threshold adjudication (resign / draw-by-score / max-moves), and convergence-progress output. Replaces `scripts/elo-iterate.sh` and `scripts/sprt.sh rating-estimate` end-to-end.

Spec source: `docs/tooling/elo-iteration-harness.md` ELOH.B section. Validation precedent: `docs/research/tooling-elo-harness-validation.md` — ELOH.A's back-test exposed two structural confounds (concurrency regime, deferred threshold adjudication) that ELOH.B's gate must control.

**No new ADR.** Concurrency primitive (`std::thread` + `std::sync::mpsc`) is captured here plus a `docs/architecture.md` settled-commitments row per workflow.md "New load-bearing invariant that doesn't merit a full ADR." Justification for not promoting to ADR: the choice does not constrain ELOH.C or any future phase (worker-pool is binary-internal; the lib seam at `src/match_clock.rs` is the only ELOH.C contact point and is unchanged). Threshold-adjudication semantics are parameter-level.

## 0. Sizing note

Estimated total: ~400 prod LOC + ~250 test LOC = ~650, against spec's ~150+80 figure and workflow's 800-LOC ceiling. The plan exposes integration glue (controller plumbing, score-history threading, CLI breadth) the spec figure assumed in the floor; same shape as ELOH.A's spec-vs-actual. Mitigation: §8 parallelizes three independent slices.

## 1. Goals

- Robbins-Monro K-update at single-game cadence: `K_t = K_0 / (1 + t/τ)`, `Elo_{t+1} = Elo_t + K_t · (S − E)`, `E = 1 / (1 + 10^((opp − my) / 400))`.
- Mid-run opponent reconfiguration via `setoption name UCI_Elo value <new>` between games (no process restart). Spawn-once contract from ELOH.A holds; preflight probe (`docs/research/tooling-stockfish-mid-session-setoption.md`) confirmed.
- σ-based stopping: stop when trailing-window σ < `--target-sigma` for `--stop-window-confirm` consecutive games. `--target-sigma 0` ⇒ disabled. **`--k0 0` ⇒ K-update no-op (estimate frozen);** the fixed-anchor sentinel for `scripts/sprt.sh rating-estimate`.
- Threshold adjudication matching fastchess defaults: resign `movecount=3 score=600`, draw `movenumber=34 movecount=8 score=20`, configurable `--max-moves` (default 200).
- Concurrency: `--concurrency N` parallel **color-pairs** via `std::thread` + `std::sync::mpsc`; each worker owns its own engine subprocess pair; results merge in arrival order into a single-threaded K-updater. **Color-pair atomicity:** a worker plays both games of a pair (color-swapped, against the same `opponent_uci_elo`) before the controller updates `opponent_uci_elo` and dispatches the next pair.
- Convergence-progress output: one stdout line per completed batch (where a batch = `--concurrency` color-pairs); final `converged: …` line.
- `scripts/sprt.sh rating-estimate` and `scripts/elo-iterate.sh` reroute through the new binary.
- Back-validation gate: Part 1 (full ELOH.B online run vs M3.F's ~2114 ± 2σ); Part 2 (synthetic-Bernoulli σ-stopping unit test).

## 2. Out of scope

- `--go-nodes N` mode and `VirtualClock` UCI option negotiation → ELOH.C.
- Adaptive K from running variance, multi-anchor regression, resume-from-checkpoint.
- Tournament book / opening-positions file. Startpos-only stays.
- SAN move formatter (still UCI long-algebraic per ADR-0020 §6).

## 3. Files modified

| File | Change | LOC est |
|---|---|---|
| `src/bin/elo-iterate.rs` | New sub-modules `estimator`, `sigma`, `thresholds`, `progress`, `controller`. Existing modules extended: `cli` (new flags), `match_loop` (score-history threading + new `GameOutcome` variants), `summary` (progress lines), `main` (controller-driven dispatch). | +650 (incl. ~250 tests) |
| `scripts/elo-iterate.sh` | Replace body with thin wrapper invoking the binary; preserves bash CLI surface (`<initial> [batches] [games-per-batch]`). | -240 / +30 |
| `scripts/sprt.sh` | `rating-estimate` reroutes via `cargo run --release --bin elo-iterate -- --k0 0 --target-sigma 0 --max-games <N> ...` (frozen-K + frozen-σ = bit-equivalent fixed-anchor measurement). | +20 / -10 |
| `docs/tooling/elo-iteration-harness.md` | ELOH.B row → done; scope detail → "Done" prose with actual landing size; cross-link Part 1+2 results. | re-state |
| `docs/tooling-backlog.md` | "Custom in-process Elo-iteration harness" entry → "Done" block. | re-state |
| `docs/workflow.md` | "Online Elo iteration" subsection added under "Running a match", pointing at the binary. | +20 |
| `docs/architecture.md` | Settled-commitments row for harness concurrency model. | +5 |
| `docs/research/tooling-elo-harness-validation.md` | Append "Part 1 — ELOH.B online run revalidation" and "Part 2 — synthetic Bernoulli σ-stopping" sections. Created post-Part-1. | +80 |
| `.cargo/mutants.toml` | Anticipated additions for progress-format float-formatting equivalents and controller integration-only paths. Survivor-driven. | +0..15 |

## 4. Type definitions and key signatures

### 4.1 Estimator (`mod estimator`)

```rust
/// `K_t = K_0 / (1 + t/τ)`. No floor (the bash version's `max(8, ...)` is dropped;
/// if a future operator needs one, `--k-min` is the right place — out of scope).
/// **Sentinel:** `k0 == 0.0` returns `0.0` for all t, freezing the estimate.
pub(crate) fn compute_k(t: u32, k0: f64, tau: f64) -> f64;

/// `1 / (1 + 10^((opp − my) / 400))`.
pub(crate) fn expected_score(my_elo: f64, opp_elo: f64) -> f64;

/// `prior_elo + k * (result - expected_score(prior_elo, opp_elo))`.
/// `result ∈ {0.0, 0.5, 1.0}` from clawfish's POV. With `k == 0.0`, returns `prior_elo`.
pub(crate) fn update_estimate(prior_elo: f64, opp_elo: f64, result: f64, k: f64) -> f64;
```

### 4.2 σ-stopping (`mod sigma`)

```rust
/// Sample stddev (Bessel-corrected, divisor `n-1`); 0.0 for `xs.len() < 2`.
pub(crate) fn sample_stddev(xs: &[f64]) -> f64;

/// Decide whether iteration should terminate. **Per-game cadence:** caller
/// invokes after every game (NOT every batch). The controller calls this once
/// per arrived game-result, after appending the new estimate to the trail.
///
/// Returns `true` iff the last `confirm` consecutive trailing-σ values
/// (each computed over the most recent `window` estimates ending at that
/// position) are all strictly below `target_sigma`. Anti-flap.
///
/// **Sentinel:** `target_sigma == 0.0` ⇒ disabled, returns `false`.
/// **Insufficient data:** `estimates.len() < window + confirm - 1` ⇒ `false`.
pub(crate) fn should_stop(
    estimates: &[f64],
    window: usize,
    target_sigma: f64,
    confirm: usize,
) -> bool;
```

### 4.3 Threshold adjudication (`mod adjudicate`, extended)

```rust
pub(crate) enum GameOver {
    // ELOH.A variants unchanged.
    Checkmate(Color), Stalemate, FiftyMove, ThreefoldRepetition,
    InsufficientMaterial, TimeForfeit(Color),
    // NEW:
    /// The just-moved side resigned — its score reached the threshold.
    /// Carries the *resigning* (= losing) color.
    ResignAdjudicated(Color),
    /// Both sides agreed on a near-zero score after the movenumber floor.
    DrawAdjudicated,
}

/// **Just-moved-side discipline.** Called after the side `mover` plays a move
/// and pushes its score onto its history. Returns `true` if `mover` should
/// resign — its trailing `movecount` scores are all at-or-below
/// `-score_threshold` (Cp) or are losing-mate (`Mate(n)` with `n < 0`).
/// Caller wraps the result as `GameOver::ResignAdjudicated(mover)`.
///
/// `mover_history.len() < movecount` → returns `false`.
/// `None` entries break the streak.
/// `Mate(n)` with `n >= 0` does NOT resign (engine sees winning mate).
pub(crate) fn resign_threshold_check(
    mover_history: &[Option<driver::Score>],
    movecount: u32,
    score_threshold: i32,
) -> bool;

/// Both sides agree on a near-zero score for `movecount` consecutive own-moves
/// each, and the current `move_number` (1-based full-move) is ≥ `movenumber_floor`.
///
/// `Score::Cp(s)` with `|s| ≤ score_threshold` qualifies. `Score::Mate(_)`
/// is treated as a non-balanced score regardless of inner value — mate is
/// by definition not a near-zero evaluation, so the impl matches Cp
/// explicitly and treats Mate(_) as breaking the streak. (Note: `Mate(n)`
/// carries plies-to-mate, not a centipawn-scaled score, so a |inner| ≤ thr
/// shortcut would be wrong for small `n`. Pinned by `draw_mate_score_breaks_streak`.)
/// `None` breaks the streak. Either side's history shorter than `movecount`
/// → returns `false`.
pub(crate) fn draw_threshold_check(
    white_history: &[Option<driver::Score>],
    black_history: &[Option<driver::Score>],
    move_number: u32,
    movenumber_floor: u32,
    movecount: u32,
    score_threshold: i32,
) -> bool;
```

**Per-move sequence in `play_one_game` after each move applied:**
1. Push `last_info.score` onto the *just-moved* side's `ScoreHistory`.
2. `detect_native_game_over(pos, history)` — if `Some(_)`, return.
3. `resign_threshold_check(mover_history, ...)` — if `true`, return `ResignAdjudicated(mover_color)`.
4. `draw_threshold_check(white_hist, black_hist, ...)` — if `true`, return `DrawAdjudicated`.
5. `move_count >= max_plies` — if so, `MaxMovesReached`.

### 4.4 Controller (`mod controller`)

```rust
pub(crate) enum WorkerCmd {
    /// Play one color-pair (2 games, color-swapped, same opp_uci_elo).
    /// `pair_index` is the 0-based pair count for game-index assignment.
    PlayPair { pair_index: u32, opponent_uci_elo: u32 },
    Quit,
}

pub(crate) enum WorkerReport {
    GameComplete {
        game_index: u32,
        opponent_uci_elo: u32,
        clawfish_score: f64,           // 1.0 / 0.5 / 0.0
        outcome: match_loop::GameOutcome,
        pgn_moves: Vec<pgn::PgnMove>,
        white_name: String,
        black_name: String,
    },
    /// Sent at end-of-pair so controller can resume-dispatch this worker.
    PairComplete { worker_id: u32 },
    Failure(String),
}
```

**Why the dispatch unit is the color-pair:** per ADR-0020 §1, color-paired games share `opponent_uci_elo` to control color-bias against the same anchor. Single-game dispatch under concurrency=1 would split a pair across batches with different opp_elo (estimate moves between games, opp_elo is recomputed). Pair dispatch keeps both games of pair `p` against `opp_uci_elo(p)` regardless of `--concurrency`. The K-update *fires* per game (single-game cadence per spec); only the opp-elo broadcast is per-pair. This is also what fastchess `-repeat` does.

**Worker spawn flow (one-time, before any `PlayPair`):**
1. Worker thread starts; calls `driver::spawn_engine` for clawfish + opponent (using `EngineSpec` from `WorkerConfig`).
2. For each engine: send `uci`, drain via `wait_for_uciok` (handshake_timeout=10s — same as ELOH.A's main).
3. Apply static `engine_options` and `opponent_options` from `WorkerConfig` (these are options that don't change per-pair — e.g. `MoveOverhead`, `Hash`, `UCI_LimitStrength`. `UCI_Elo` is *not* applied here; it's broadcast per-pair).
4. Per engine: `wait_for_readyok` after the option block.
5. Enter the `recv` loop on the worker's `WorkerCmd` channel.

**Per-pair flow inside a worker thread:**
1. Receive `PlayPair { pair_index, opponent_uci_elo }`.
2. Send to opponent: `setoption name UCI_Elo value <opponent_uci_elo>` then `setoption name UCI_LimitStrength value true` (idempotent — re-sent each pair to defend against engine-state edge cases) then `isready` (wait for `readyok` per ELOH.A's recv-pump discipline). **Setoption before ucinewgame** — ucinewgame is allowed to reset per-game state but UCI options are persistent; defensive ordering avoids any engine that drops options on `ucinewgame`. Pinned in §6.4 test.
3. For game in [first, swap]: send to both engines `ucinewgame` + `isready`; play the game via `match_loop::play_one_game`; report `GameComplete` over the report channel.
4. After both games: report `PairComplete { worker_id }`.

**Color-pair invariant:** worker plays game `2*pair_index + 1` as clawfish-white, game `2*pair_index + 2` as clawfish-black (1-based game indices match ELOH.A's existing `(game_index - 1) / 2` pair-id pattern; clawfish-white iff `(game_index - 1) % 2 == 0`).

**Controller loop:**
```rust
pub(crate) fn run_iteration(
    pool: &mut WorkerPool, args: &cli::Args, out_dir: &std::path::Path,
) -> Result<IterationOutcome, HarnessError>;

pub(crate) struct IterationOutcome {
    pub final_estimate: f64, pub final_sigma: f64, pub games_played: u32,
    pub stop_reason: StopReason, pub wld: (u32, u32, u32),
}
pub(crate) enum StopReason { Sigma, MaxGames }

pub(crate) struct WorkerPool {
    pub senders: Vec<std::sync::mpsc::Sender<WorkerCmd>>,
    pub reports: std::sync::mpsc::Receiver<WorkerReport>,
    pub join_handles: Vec<std::thread::JoinHandle<()>>,
}

/// Public spawn entry point — production use.
pub(crate) fn spawn_workers(n: u32, cfg: WorkerConfig) -> Result<WorkerPool, HarnessError>;

/// Internal spawn taking the worker-thread function as a parameter, for
/// synthetic-worker substitution in tests (§6.6).
pub(super) fn spawn_workers_with_fn(
    n: u32,
    cfg: WorkerConfig,
    worker_fn: fn(u32, WorkerConfig, std::sync::mpsc::Receiver<WorkerCmd>, std::sync::mpsc::Sender<WorkerReport>),
) -> Result<WorkerPool, HarnessError>;
```

Algorithm:
1. **Bootstrap.** Dispatch one `PlayPair` to each worker at `opponent_uci_elo = round(args.initial_elo)` (clamped to `[1320, 3190]` per Stockfish bounds — same as `scripts/elo-iterate.sh`). `total_pairs = args.max_games / 2` (max_games is even per ADR-0020 §8).
2. **Drain.** Loop on `pool.reports.recv()`:
   - On `GameComplete`: write PGN, append summary line, push clawfish_score → estimator updates `current_estimate`, append to `estimates_trail`, `t += 1`. Run `should_stop(estimates_trail, …)`; if `true`, set `terminating = true`.
   - On `PairComplete { worker_id }`: emit `progress: …` line (cumulative wld + current K + current σ); if `!terminating && pairs_dispatched < total_pairs`, dispatch next `PlayPair` to `worker_id` and `pairs_dispatched += 1`.
   - On `Failure`: send `Quit` to all workers, drain remaining reports, join, return `HarnessError`.
3. **Termination criterion.** Loop exits when `(terminating || pairs_dispatched == total_pairs) && all_workers_idle`, where "all_workers_idle" means every worker has reported `PairComplete` for its most recently dispatched pair (no in-flight pairs). On exit: send `Quit` to all senders, drain `join_handles`, emit `converged: …` line. **In-flight pair completion:** when `terminating` is set, no new pairs are dispatched, but the controller still drains `GameComplete` and `PairComplete` for already-dispatched pairs (their results count toward the final estimate). This keeps every game's K-update applied and avoids a race where `Quit` interrupts an in-flight pair mid-game.
4. `stop_reason` = `Sigma` if `terminating` was set by `should_stop`; `MaxGames` if loop exited because `pairs_dispatched == total_pairs && all_workers_idle` without `terminating`.

**Note: estimate-trail ordering ≠ game-index ordering under concurrency > 1.** With `N` workers, `GameComplete` reports may arrive in any interleaved order across worker pairs. The estimate trail is in arrival order; the K-update remains correct because Robbins-Monro is order-robust (each game's S − E contribution is independent of arrival order, only the K_t coefficient depends on `t` which is incremented in arrival order).

**`WorkerPool::Drop` impl:** drops `senders` (causing workers to see `Disconnected` on `recv` and exit naturally). Does NOT join — joining in `Drop` could block forever on panic-unwinding paths and the OS reaps eventually. The success-path `run_iteration` issues explicit `Quit` + joins (via `pool.join_handles.drain(..).for_each(|h| { let _ = h.join(); })`); panic-time cleanup falls back to disconnection-driven worker exit.

**File-system writes happen on the controller thread** (not workers): `summary.txt` line ordering must be deterministic, and `summary.txt` plus the per-game PGN are written together at `GameComplete` arrival. Channel marshals the `Vec<PgnMove>` move list; ~200 moves * ~20 bytes/move ≈ 4 KB per game → trivial overhead.

### 4.5 Progress format (`mod progress`)

```rust
pub(crate) struct ProgressLine {
    pub t: u32, pub games: u32, pub elo: f64, pub sigma: f64, pub k: f64,
    pub wld: (u32, u32, u32),
}

/// `progress: t=<t> games=<G> elo=<%.2f> sigma=<%.2f> K=<%.3f> wld=<W>-<L>-<D>`
/// Float specs use Rust `{:.2}` / `{:.3}` (rounding to fixed places).
pub(crate) fn format_progress(line: &ProgressLine) -> String;

/// `converged: elo=<%.2f> sigma=<%.2f> games=<G> reason=<sigma|max-games>`
pub(crate) fn format_converged(
    final_elo: f64, final_sigma: f64, games: u32, reason: StopReason,
) -> String;
```

### 4.6 CLI additions (`mod cli`)

```rust
pub(crate) struct Args {
    // ELOH.A fields unchanged.

    pub initial_elo: f64,           // --initial-elo; required, no default.
    pub k0: f64,                    // --k0; default 40.0; 0.0 = freeze-K sentinel.
    pub tau: f64,                   // --tau; default 10.0
    pub target_sigma: f64,          // --target-sigma; default 30.0; 0.0 = disable sentinel.
    pub stop_window: usize,         // --stop-window; default 30
    pub stop_window_confirm: usize, // --stop-window-confirm; default 5
    pub concurrency: u32,           // --concurrency; default 1; counted in pairs.
    pub thresholds: Thresholds,
    pub max_moves: u32,             // --max-moves; default 200 plies.
}

pub(crate) struct Thresholds {
    pub resign_movecount: u32,      // default 3
    pub resign_score: i32,          // default 600 (positive cp; threshold = -value)
    pub draw_movenumber: u32,       // default 34 (full-move number)
    pub draw_movecount: u32,        // default 8
    pub draw_score: i32,            // default 20 (positive cp)
}
```

Validation: `--initial-elo` required; `--concurrency >= 1`; `--stop-window >= 2`; `--stop-window-confirm >= 1`; `--k0 >= 0` (0 = freeze); `--tau > 0`; `--target-sigma >= 0` (0 = disable); thresholds non-negative; `--max-moves >= 2`. Existing `--max-games` parity-and-≥2 rule unchanged.

**Sentinel composition rule:** `--k0 0` requires `--target-sigma 0` (CLI rejects otherwise as `InvalidValue`). With K=0 the estimate trail is constant ⇒ σ=0 ⇒ σ-stopping would fire trivially after `window+confirm-1` games. The `--k0 0` mode is reserved for fixed-anchor measurements where the operator wants `--max-games` as the only termination criterion; pairing both flags is an explicit declaration of fixed-anchor intent. Pinned by `parse_args_k0_zero_requires_target_sigma_zero` in §6.5.

`--target-sigma` near-zero footgun (e.g. `0.0001`): documented in `--help` — "exactly 0 disables σ-stopping; small positive values may never fire in practice."

## 5. Module boundaries

```
src/bin/elo-iterate.rs              <- binary entrypoint
    mod cli                         (extended)
    mod driver                      (untouched)
    mod adjudicate                  (extended w/ thresholds + new variants)
    mod estimator                   (NEW)
    mod sigma                       (NEW)
    mod progress                    (NEW)
    mod match_loop                  (extended w/ score-history threading)
    mod pgn                         (untouched)
    mod summary                     (extended w/ progress-line append)
    mod controller                  (NEW)
    fn main()                       (rewritten)
```

## 6. Test coverage strategy

### 6.1 Estimator (`mod estimator::tests`, ~30 LOC)

| Test | Asserts |
|---|---|
| `compute_k_at_t_zero_returns_k0` | `compute_k(0, 40, 10) == 40.0`. |
| `compute_k_at_t_equals_tau_halves` | `compute_k(10, 40, 10) ≈ 20.0`. |
| `compute_k_decay_at_ten_tau` | `compute_k(100, 40, 10) ≈ 40/11`. |
| `compute_k_monotone_decreasing` | For monotone `t`, K is monotone non-increasing. |
| `compute_k_zero_k0_returns_zero` | `compute_k(t, 0.0, _) == 0.0` for any t (sentinel). |
| `expected_score_equal_elo_returns_half` | `expected_score(2000, 2000) == 0.5`. |
| `expected_score_400_above` | `expected_score(2400, 2000) ≈ 0.909` (±1e-3). |
| `expected_score_400_below` | `expected_score(2000, 2400) ≈ 0.091`. |
| `update_win_against_equal` | At E=0.5, S=1 ⇒ shift +k/2. |
| `update_loss_against_equal` | S=0 ⇒ shift -k/2. |
| `update_draw_against_equal_no_change` | S=0.5 ⇒ unchanged. |
| `update_with_zero_k_freezes_estimate` | `update_estimate(prior, opp, S, 0.0) == prior` for any inputs (sentinel). |

### 6.2 σ-stopping (`mod sigma::tests`, ~50 LOC including Bernoulli back-test)

| Test | Asserts |
|---|---|
| `sample_stddev_constant_series_zero` | `[5.0, 5.0, 5.0]` → `0.0`. |
| `sample_stddev_two_point_uses_bessel` | `[0.0, 2.0]` → `√2`. |
| `sample_stddev_short_returns_zero` | `[]`, `[42.0]` → `0.0`. |
| `should_stop_disabled_when_target_zero` | Constant series, `target=0.0` → `false`. |
| `should_stop_fires_when_recent_window_below` | `vec![2100.0; 50]`, window=30, target=10, confirm=5 → `true`. |
| `should_stop_does_not_fire_with_high_variance` | Series with σ=50 in window → `false`. |
| **`should_stop_anti_flap_concrete_fixture`** | Build `estimates` of length 35 = `[2100; 30]` followed by `[2100, 2050, 2150, 2050, 2150]` (last 5 positions have wide trailing-σ); window=30, target=10, confirm=5 → `false` (the last 5 positions' trailing σ exceeds target). Companion: `[2100; 30]` followed by `[2100; 5]` → `true`. |
| `should_stop_short_estimates_returns_false` | `len < window + confirm - 1` → `false`. |
| **`bernoulli_back_test_gate`** | **Part 2 of back-validation.** Seeded PRNG (xorshift, fixed seed) generates Bernoulli stream with `p = 0.760` (matches +200 Elo gap → expected_score≈0.760). Initial estimate set at the equilibrium (initial_elo for clawfish, opp_elo s.t. expected_score=p). Run online iteration with K_0=40, τ=10, target_sigma=30, window=30, confirm=5. Expected: σ-stopping fires within 100–400 games. Wide tolerance: σ_window ≈ K_t · √(p(1-p)) · √(1-1/window) crosses 30 at K_t ≈ 70 (formally); but K_t crosses 70 well before t=0 with K_0=40 — drift dominates early. Empirical convergence point depends on initial-estimate-vs-equilibrium offset; a wide window absorbs noise without making the test confirmation-biased. ~30 LOC. |

### 6.3 Threshold adjudication (`mod adjudicate::tests`, +~50 LOC)

| Test | Asserts |
|---|---|
| `resign_three_consecutive_below_threshold_fires` | `[Cp(-700), Cp(-650), Cp(-720)]`, mc=3, thr=600 → `true`. |
| `resign_two_below_one_above_does_not_fire` | `[Cp(-700), Cp(-650), Cp(-100)]` → `false`. |
| `resign_negative_mate_score_fires` | `[Mate(-3), Mate(-4), Mate(-5)]` → `true`. |
| `resign_positive_mate_does_not_fire` | `[Mate(3), Mate(2), Mate(1)]` → `false` (winning side, not losing). |
| `resign_none_entry_breaks_streak` | `[Cp(-700), None, Cp(-720)]` → `false`. |
| `resign_short_history_returns_false` | `mover_history.len() < movecount` → `false` (no panic on slice). |
| `resign_exact_threshold_fires` | `[Cp(-600), Cp(-600), Cp(-600)]`, thr=600 → `true` (≤, not <). Pins boundary. |
| `draw_eight_consecutive_balanced_after_movenumber_fires` | Both histories: 8 entries `Cp(±10)`, move 40, floor 34, count 8, thr 20 → `true`. |
| `draw_before_movenumber_does_not_fire` | Same balanced history, move 30, floor 34 → `false`. |
| `draw_one_side_above_threshold` | White balanced; black has `Cp(-50)` once → `false`. |
| `draw_mate_score_breaks_streak` | Otherwise-balanced, `Mate(5)` in last position on either side → `false`. |
| `draw_short_history_either_side_returns_false` | Either side `< movecount` → `false`. |
| `draw_none_entry_breaks_streak` | `None` in trailing window on either side → `false`. |
| `draw_exact_threshold_fires` | All entries `Cp(±20)`, thr=20 → `true` (|s| ≤ thr, not <). |

### 6.4 Progress format (`mod progress::tests`, ~20 LOC)

| Test | Asserts |
|---|---|
| `format_progress_canonical_string` | `ProgressLine { t: 60, games: 60, elo: 2103.45, sigma: 28.7, k: 13.333, wld: (45, 8, 7) }` → `"progress: t=60 games=60 elo=2103.45 sigma=28.70 K=13.333 wld=45-8-7"`. |
| `format_converged_sigma_reason` | Contains `reason=sigma`. |
| `format_converged_max_games_reason` | Contains `reason=max-games`. |
| `format_progress_zero_sigma` | `sigma=0.0` → contains `sigma=0.00`. |
| `format_progress_two_decimal_elo_rounds` | `elo=1999.999` → contains `elo=2000.00`. |

### 6.5 CLI parse (`mod cli::tests`, +~30 LOC)

| Test | Asserts |
|---|---|
| `parse_args_default_thresholds_match_sprt_sh` | Defaults `(3, 600, 34, 8, 20)`. |
| `parse_args_concurrency_default_one` | Defaults to 1. |
| `parse_args_initial_elo_required` | Missing → `Err(MissingFlag)`. |
| `parse_args_target_sigma_zero_valid_sentinel` | `--target-sigma 0` parses. |
| `parse_args_negative_target_sigma_rejected` | `-1` → `Err(InvalidValue)`. |
| `parse_args_stop_window_minimum_two` | `--stop-window 1` → `Err`. |
| `parse_args_concurrency_zero_rejected` | `0` → `Err`. |
| `parse_args_max_moves_default_200` | Omitted → 200. |
| `parse_args_k0_zero_with_target_sigma_zero_valid` | `--k0 0 --target-sigma 0` parses (frozen-anchor mode). |
| `parse_args_k0_zero_requires_target_sigma_zero` | `--k0 0` without `--target-sigma 0` → `Err(InvalidValue)`. Pins the sentinel-composition rule (§4.6). |
| `parse_args_tau_zero_rejected` | `--tau 0` → `Err` (would divide-by-zero). |

### 6.6 Controller (`mod controller::tests`, ~50 LOC)

Synthetic-channel tests (no subprocesses). Test seam: `spawn_workers` is a thin wrapper around an internal `spawn_workers_with_fn(n: u32, cfg, worker_fn: fn(...) -> ()) -> WorkerPool` that takes a worker-thread function as a parameter. Tests substitute a synthetic `worker_fn` that consumes `WorkerCmd` from its receiver and emits canned `WorkerReport`s without spawning subprocesses.

| Test | Asserts |
|---|---|
| `dispatch_round_robin_one_pair_per_worker` | Concurrency=4, after bootstrap, each worker's Sender saw exactly one `PlayPair`; pair_index assignment is round-robin (worker N gets pair N initially). |
| `aggregate_wld_handles_clawfish_white_and_black` | Synthetic reports: 2W (one as white one as black), 1L, 1D → cumulative `(2, 1, 1)`. Pins score-mapping. |
| `controller_terminates_on_max_games` | `--target-sigma 0 --max-games N`, synthetic feed → exits with `StopReason::MaxGames` after exactly N games. |
| `controller_terminates_on_sigma` | Synthetic feed of constant estimates → `should_stop` fires; `StopReason::Sigma`. |
| `controller_setoption_before_ucinewgame_pins_order` | Mock engine handle records the sequence of commands sent within a `PlayPair`. Assertion: index-of(`"setoption name UCI_Elo"`) < index-of(first `"ucinewgame"`). Pins per-game flow §4.4. |
| `controller_freeze_k_holds_initial_estimate` | `--k0 0` synthetic feed of N games (any results) → final estimate equals `initial_elo`. Pins frozen-K sentinel composition with controller. |
| `controller_does_not_block_on_slow_worker` | Spawn 2 synthetic workers; worker 0 sleeps 200 ms before sending each report; controller continues dispatching to worker 1 in the meantime. Assertion: total controller wallclock for 4 pairs is ≤ ~600 ms (not 800+ ms which would indicate serial blocking). Catches a regression where the controller `recv()`s and dispatches under a worker-id-keyed lock. |

`#[ignore]`-gated end-to-end (extends ELOH.A's smoke):

| Test | Asserts |
|---|---|
| `end_to_end_clawfish_self_play_concurrency_2` | `--concurrency 2 --max-games 4 --tc 1+0.05 --target-sigma 0 --initial-elo 2000 --k0 0`. 4 PGNs written, summary has 4 entries + progress lines + 1 converged line, exit 0. |
| `end_to_end_threshold_adjudication_self_play` | Aggressive thresholds (`--resign-score 100 --draw-score 5 --draw-movenumber 1 --draw-movecount 2`) force adjudication; assert ≥1 game's termination string contains `"adjudication: resign"` or `"adjudication: draw-by-score"`. |

## 7. Order of operations

1. **`mod cli` extension.** New `Args` fields + `Thresholds`; extend `parse_args`; §6.5 tests.
2. **Slices A, B, C in parallel** (after step 1 lands):
   - **A.** `mod estimator` + `mod sigma` (incl. Bernoulli back-test gate). Pure fns.
   - **B.** `mod adjudicate` extension (new variants + 2 pure fns + §6.3 tests). Single atomic edit on the enum so it must be one slice.
   - **C.** `mod progress` (formatter + §6.4 tests).
3. **Slice D.** `mod match_loop` extension: thread per-side `ScoreHistory` through `GameContext`; insert per-move `resign_threshold_check` / `draw_threshold_check` calls per §4.3; route new `GameOutcome` arms through `outcome_to_pgn_result` and `outcome_to_termination_reason`. Sequential after B.
4. **Slice E.** `mod controller`. Worker thread + channel plumbing + iteration loop + §6.6 controller-smoke tests. **Opus override** — novel invariants (worker-pool lifecycle, recv-pump at setoption boundaries, pair-atomicity color-balance, frozen-K composition). Sequential after A+B+C+D.
5. **Slice F.** `main()` rewrite + bash-script reroutes. Sequential after E.
6. **Pre-review mechanical checks** (workflow.md step 9).
7. **Final review loop** (workflow.md step 10).
8. **Commit + push** — atomic doc-delta per §9.
9. **Manual back-test (Part 1, post-commit).**
   - `cargo run --release --bin elo-iterate -- --engine target/release/clawfish --opponent $(which stockfish) --engine-launch-prefix 'taskpolicy -c utility' --opponent-launch-prefix 'taskpolicy -c utility' --opponent-option UCI_LimitStrength=true --tc 10+0.1 --max-games 120 --target-sigma 0 --initial-elo 2114 --concurrency 6`. Both engines under taskpolicy controls the concurrency confound.
   - Pass: final estimate ∈ [2044, 2184] (±2σ ≈ ±70 Elo around M3.F's ~2114).
   - **Diagnostic ladder on Part 1 failure:**
     - If estimate matches ELOH.A's ~1752 ± noise → confound-control failed (taskpolicy not effective at concurrency=6, or thresholds not actually wired). Verify by inspecting summary.txt termination strings — should show `adjudication: resign` and `adjudication: draw-by-score`.
     - If estimate matches M3.F's ~2114 ± wallclock-noise → harness is correct, gate may need wider tolerance per the "wallclock-noise carry-over" caveat (`docs/tooling/elo-iteration-harness.md` §"ELOH.B's role in M4.A measurement").
     - Otherwise → suspected harness bug (e.g. score-mapping confused color, K-update sign error). Re-run `--k0 0 --initial-elo 2114 --max-games 200 --concurrency 6` (frozen-K; reduces to ELOH.A-equivalent fixed-anchor measurement at concurrency=6 + thresholds) — should match M3.F's W/L/D directly. If it doesn't, the bug is in the harness.
   - Archive verdict to `docs/research/tooling-elo-harness-validation.md` "Part 1" section.

## 8. Dependencies

- **ELOH.A** for driver/adjudicate/match_loop/pgn/summary stack. Already landed.
- **No M4 dependency** — same as ELOH.A.
- Crate API surface unchanged.

## 9. Parallelization map

After step 1: spawn slices A, B, C in parallel via three coder agents. Honest dependencies:
- A, B, C share no source surface (A: new files; B: new variants on existing enum + new fns; C: new file).
- D needs B's enum variants → sequential.
- E needs A, B, C, D → sequential. **Opus override** flagged.
- F needs E → sequential.

## 10. Risk register

- **Worker-pool lifecycle on engine crash.** Worker reports `Failure(_)`; controller drains, sends `Quit`, joins, returns `HarnessError`. No automatic respawn (out of scope).
- **`mpsc` channel-disconnect on controller drop.** Workers see `Err(Disconnected)` on next `recv` and exit. `WorkerPool::Drop` drops senders; OS reaps engine processes via existing ELOH.A `EngineHandle::Drop` best-effort kill.
- **Mid-pair `setoption UCI_Elo` race.** Setoption sent before ucinewgame within each pair; recv-pump per ELOH.A's discipline. Pinned by `controller_setoption_before_ucinewgame_pins_order`.
- **σ-stopping anti-flap edge.** `--stop-window-confirm 5` requires 5 consecutive in-window observations; absorbs oscillation. Risk of never-stops if `--target-sigma` impossibly tight; mitigated by `--max-games` fallback.
- **`--target-sigma 0` vs near-zero footgun.** Documented in `--help`; only exact-zero disables.
- **Threshold check score-source.** Strictly the just-moved side's history; type signature `mover_history: &[Option<Score>]` makes this a compile-time pin (no cross-color ambiguity at the call site).
- **Validation-note's two confounds.** Part 1 controls both via `--concurrency 6` + thresholds; diagnostic ladder distinguishes structural-but-not-bug from bug.
- **Wallclock-noise carryover.** Per `docs/tooling/elo-iteration-harness.md` §"ELOH.B's role in M4.A measurement" — Part 1's tolerance ±2σ acknowledges irreducible wallclock noise. M4.A's first rating estimate documented as wallclock-noisy until ELOH.C lands.

## 11. Doc-delta — atomic with landing

- `docs/tooling/elo-iteration-harness.md` — ELOH.B row → done; scope detail → "Done" prose with landing size; cross-link Part 1+2.
- `docs/tooling-backlog.md` — "Custom in-process Elo-iteration harness" → "Done" block.
- `docs/workflow.md` — "Online Elo iteration" subsection.
- `docs/architecture.md` — settled-commitments row for std::thread + mpsc concurrency model.
- `scripts/elo-iterate.sh` — thin wrapper; `<initial>` defaults to 2171 (preserves bash default); translates `BATCHES * GAMES_PER_BATCH` → `--max-games`, `GAMES_PER_BATCH` → `--concurrency`, hardcodes `--target-sigma 30 --stop-window 30 --stop-window-confirm 5 --k0 40 --tau 10`. **Behavior delta:** wrapper preserves CLI surface and total game count, but K-update cadence shifts from per-batch (bash) to per-game (new harness, per spec §1). Per-batch averaging in the bash version → per-game increments here. Documented in the wrapper's header comment.
- `scripts/sprt.sh` — `rating-estimate` reroutes to `--k0 0 --target-sigma 0 --max-games <N> --initial-elo <STOCKFISH_ELO>` (frozen-K + frozen-σ ⇒ semantically equivalent to fastchess fixed-count match against `Stockfish UCI_Elo=<STOCKFISH_ELO>` — Stockfish UCI_Elo never moves because K=0 means estimate stays fixed at `initial_elo`).

After Part 1 manual back-test:
- `docs/research/tooling-elo-harness-validation.md` — Part 1 + Part 2 sections appended.

## 12. Verification checklist

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --release`
- `cargo llvm-cov --summary-only --lib --release`
- `cargo mutants --in-diff` against unit's diff
