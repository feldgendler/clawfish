# Plan: ELOH controller boundary tests + pgn fixture sweep

**Unit name.** `tooling-eloh-controller-boundary-tests`
**Source backlog item.** `docs/tooling-backlog.md` §"ELOH controller boundary testing — mock `EngineHandle` test harness"
**Estimated size.** ~200 LOC of test code + ~10–20 LOC of mutants.toml deletions. Zero production code.

## 1. Problem statement

`.cargo/mutants.toml` parks two clusters of survivors as **deferred**, with `docs/tooling-backlog.md` flagging this as the next tooling item:

- `in controller::run_iteration` — function-anchored exclusion. The audit comment enumerates 5 specific suspected survivors (PGN-write call site, summary-line call site, `pd < tp` boundary, `t < max_games` early-break guard, in-flight tracking math), but the audit is **suspicions, not measurements**. The actual surviving-mutant set must be determined empirically by lifting the exclusion and running cargo-mutants. cargo-mutants 27.0.0 lists **71 mutants** in this function (counted via `cargo mutants --config /dev/null --list -f 'src/bin/elo-iterate.rs' | grep -c 'in controller::run_iteration'`); the function-anchored exclusion suppresses all of them.
- `in pgn::format_pgn` — function-anchored exclusion. Existing pgn tests use loose assertions (`pgn.contains(...)`, `pgn.trim_end().ends_with(...)`) that silently absorb spacing-boundary mutations on lines 5376 and 5390. The fix is tighter assertion shape, optionally with a parametric move-count sweep. cargo-mutants 27.0.0 lists **22 mutants** in this function (counted via the same invocation, `grep -c 'in pgn::format_pgn'`).

The 71 + 22 figures are the *total* generated mutant population for each function, not the *surviving* set. Most are likely caught by existing tests once the function-anchored exclusion is lifted (see §4.2 rows P/Q/R/S for examples of expected-caught mutants). The §4.1 survey reveals the actual residual; the implementer should not expect to write 71 new tests.

## 2. Scoping decision — what's IN, what's OUT

The backlog entry frames this as a "mock `EngineHandle`" task. **That framing is partially misleading**: the documented surviving boundary mutants in `controller::run_iteration` live in pure Rust dispatch logic *above* `production_worker_fn`. They do **not** need a faked `EngineHandle`. Only a hypothetical `controller_setoption_before_ucinewgame_pins_order` test would require a mock subprocess; that ordering is already pinned by the inline comment in `production_worker_fn` + the `#[ignore]`-gated end-to-end smoke.

### IN (this unit)

1. **Empirical mutation-survey-then-test workflow** on `controller::run_iteration`: lift the function-anchored exclusion, enumerate survivors, then iteratively kill each via the manual-mutation iteration in `.cargo/mutants.toml` lines 11–28.
2. **Tighten existing pgn tests** + add **parametric `format_pgn` sweep** with strict spacing assertions to kill the boundary survivors at lines 5376 and 5390.
3. **Update `.cargo/mutants.toml`**: replace function-anchored blanket exclusions with **narrower** regex patterns that target only the integration-bound survivors (file-IO call sites, the `production_worker_fn` body), each with refreshed audit prose.

### OUT (explicitly deferred)

- **Fake `EngineHandle` for `production_worker_fn`** — would require either (a) a trait abstraction refactor of `driver::EngineHandle`, invasive across the entire driver module, or (b) a `tests/fixtures/mock-engine` binary target with scripted UCI replies. Either path is 5× the present budget. Deferred to a follow-up backlog item appended in §6.
- **The `controller_setoption_before_ucinewgame_pins_order` test** — strictly requires the mock-engine fixture above. Inline comment + e2e smoke continues to pin the ordering. Deferred with the fixture.
- **`production_worker_fn` exclusion lifting** — stays excluded.
- **A shared "recording synthetic-pool" helper** — YAGNI for 6 tests (the existing `synthetic_pool` already serves; new tests can use bespoke handlers per the existing pattern in `controller_does_not_block_on_slow_worker`).

## 3. Files modified

Only **two**:

- `src/bin/elo-iterate.rs` — add tests under existing `mod controller::tests` (line 7588) and `mod pgn::tests` (line 5400). Tighten the loose assertions in two existing pgn tests if needed (one-line edits). Zero production-code changes.
- `.cargo/mutants.toml` — replace function-anchored exclusions with narrower patterns.

Optionally:

- `docs/tooling-backlog.md` — strike the closed item; append the "Mock-engine fixture" follow-up.

## 4. Test scope

### 4.1 Mutation-survey workflow on `controller::run_iteration`

Per `.cargo/mutants.toml` lines 11–28 ("Triaging a single survivor — manual mutation + targeted test"), the cycle is **edit-test-revert**, not "predict every survivor up front then write tests blind."

**Step-by-step:**

1. Comment out the `"in controller::run_iteration"` line in `.cargo/mutants.toml`.
2. Run `cargo mutants --file src/bin/elo-iterate.rs --regex 'in controller::run_iteration'` to enumerate this function's survivors against the current test suite.
3. For each surviving mutant, manually apply the mutation via `Edit`, run `cargo test --bin elo-iterate -- controller::tests`, observe pass/fail:
   - **Fails (caught)** — the existing tests already kill it; no new test needed. Revert the edit.
   - **Passes (survives)** — new test required. Write a targeted test with a post-condition that distinguishes the mutated code from the original. Revert the source mutation; verify the new test still passes against the original code; verify it fails when the mutation is re-applied. Iterate.
   - **Hangs** — see §4.2 hang-class mutants A, J, L, M, N1 and the watchdog strategy below them. Either (a) kill via test wrapped in `run_iteration_with_watchdog`, or (b) re-exclude with a narrower pattern + audit prose explaining the structural-equivalence argument or the deferred-detection commitment.
   - **Equivalent** — document and re-exclude with a narrower pattern.
4. After all survivors are addressed, run `cargo mutants --file src/bin/elo-iterate.rs --regex 'in controller::run_iteration'` again to confirm the residual is empty (or only contains the explicitly-re-excluded mutants).

### 4.2 Test drafts (subject to revision based on §4.1's actual survivor set)

The following are **expected** tests based on the actual cargo-mutants 27.0.0 mutant list (verified by `cargo mutants --config /dev/null --list`). Each draft cites:
- The exact line in `src/bin/elo-iterate.rs` of the mutated source.
- The exact mutation (cargo-mutants display form).
- The post-condition that distinguishes mutated from original behavior.
- Whether the mutant is **hang-class** (loop never returns under the mutation; requires watchdog).

| ID | Source line | Mutation | Hang? | Post-condition that distinguishes |
|---|---|---|---|---|
| **A** | 7174 (`if pairs_dispatched >= total_pairs { break; }`, bootstrap) | `>= → <` | **YES** | At pd=0, `0 < tp` true → break bootstrap immediately. 0 dispatches → drain loop's `recv()` blocks forever (no worker has a PlayPair to respond to). **Watchdog required.** |
| **B1** | 7191 (`pairs_dispatched += 1`, bootstrap) | `+= → *=` | no | pd stays 0; bootstrap dispatches N copies of `pair_index=0` across N workers. Detection: bootstrap-window dispatch's `pair_index` values across workers must form `{0, 1, …, min(N, tp)-1}`. **Existing** `dispatch_round_robin_one_pair_per_worker` (line 7696) asserts `sorted == vec![0, 1, 2, 3]`; verify by manual mutation before adding a new test. |
| **B2** | 7191 | `+= → -=` | n/a | u32 underflow panic — caught. |
| **C** | 7204 (`if t >= args.max_games { break; }`) | `>= → <` | no | 1 worker, `max_games=4`, script emits 5 GCs (no PC). Original: `4 >= 4` → break at t=4, `games_played=4`. Mutant `<`: `0 < 4` true at t=0 → break immediately, `games_played=0`. Caught by `games_played == max_games`. |
| **D** | 7192 (`*in_flight_slot += 1`, bootstrap) | `+= → *=` | no | pif=[0,…,0] at end of bootstrap. drain_done's all_idle true at first iter check; loop exits before any GC processed. `games_played == 0` (original: max_games). |
| **E** | 7192 | `+= → -=` | n/a | u32 underflow → panic. cargo-mutants treats panic as caught. |
| **F** | 7406 (`pairs_dispatched += 1`, redispatch) | `+= → *=` | no | pd never advances past bootstrap value; redispatch arm keeps firing with the same `pair_tcs[pd]`. Each redispatch reuses the SAME `pair_index`. Total games still reaches max_games (worker emits per cmd), so `games_played` doesn't distinguish. **Detection: per-cmd pair_index sequence assertion**, e.g. `recorded_pair_indices == vec![0, 1, 2, 3]`. New test required. |
| **G** | 7406 | `+= → -=` | n/a | pd underflows / panics. Caught. |
| **H** | 7407 (`pairs_in_flight[wid] += 1`, redispatch) | `+= → *=` | no | At redispatch, pif[wid] *= 1 = 0. drain_done returns true at top of next iter (pd≥tp, all_idle), exits early before draining the redispatched pair's reports. `games_played < max_games`. Existing `controller_terminates_on_max_games` likely catches; verify by manual mutation. |
| **I** | 7317 (`pairs_in_flight[wid].saturating_sub(1)`) | `saturating_sub(1) → saturating_sub(2)` | no | At pif=1 → 0 (saturating). Original sub(1): same. EQUIVALENT in the natural flow; document. |
| **J** | 7317 | `saturating_sub(1) → saturating_sub(0)` | **YES** | pif stays at 1 forever. Drain_done's `all_idle` never true → loop blocks indefinitely on `pool.reports.recv()` after the last PC. **Watchdog required.** |
| **K** | 7394 (`if !terminating && pairs_dispatched < total_pairs && wid < pool.senders.len()`) | `<` → `<=` (pd<tp clause) | no | 1 worker, `max_games=8`, `total_pairs=4`. At pd=4 after redispatching pair_index=3, `4 <= 4` true → tries to dispatch; panics on `pair_tcs[4]` (out of bounds). Caught via panic. |
| **L** | 7394 | `<` → `==` (pd<tp clause) | **YES** | At pd=1 (after first PC), `1 == 2` false → no redispatch; pif decrements to 0; drain_done is `(false || pd≥tp=false) && all_idle=true` = false → loop continues, blocks on recv forever. **Watchdog required.** |
| **M** | 7394 | `<` → `>` (pd<tp clause) | **YES** | At pd=1, `1 > 2` false → never re-dispatches; pif=0; same hang as L. **Watchdog required.** |
| **N1** | 7394 | `delete !` on `!terminating` | **YES** | Gate becomes `terminating && pd<tp && wid<senders.len()`. Happy-path tests have `terminating=false` (no sigma, no SPRT, no failure) → gate always false → no redispatch. After first PC, drain_done false (pd<tp, all_idle=true), recv blocks. **Watchdog required.** |
| **N2** | 7394 | `&&` → `||` (first occurrence) | n/a | Gate `!terminating || pd<tp && wid<senders.len()` — over-dispatches once `pd >= tp` if `!terminating` true → panic on `pair_tcs[pd]`. |
| **N3** | 7394 | `&&` → `||` (second occurrence) | n/a | Gate `!terminating && pd<tp || wid<senders.len()` — similar over-dispatch panic. |
| **O** | 7394 | `<` clauses on `wid < pool.senders.len()` | n/a | The `wid < pool.senders.len()` clause is defensive (wid comes from `WorkerReport::PairComplete` and is always < senders.len()). Likely structurally equivalent at the test surface; document or leave to triage. |
| **P** | 7281–7286 (`wins/losses/draws += 1`) | `+= → *=` | no | Existing `aggregate_wld_handles_clawfish_white_and_black` asserts `outcome.wld == (2,1,1)`; expected to catch. |
| **Q** | 7278 (`t += 1`) | `+= → *=`, `+= → -=` | no | Existing `controller_terminates_on_max_games` asserts `games_played == max_games`; expected to catch. |
| **R** | 7301–7302 (`terminating = true; sigma_fired = true;`) | various | no | Existing `controller_terminates_on_sigma` covers; expected to catch. |
| **S** | 7350–7363 (SPRT branch arms) | various | no | SPRT-active tests in `mod controller::tests` (search for `sprt_acceptH0`/`H1`) cover. |

**Hang-class enumeration.** Mutants **A, J, L, M, N1** are hang-class — they make the drain loop's exit condition unreachable under happy-path test fixtures. They MUST be tested through a watchdog wrapper. Any other hang-class mutants discovered during §4.1 triage get added to this list.

**Strategy for hang-class mutants.**

`std::thread::scope` is **wrong** for the watchdog: `thread::scope`'s closure waits for spawned threads to join before returning, so a hung scoped thread defeats the timeout. The correct pattern uses `std::thread::spawn` (escapes lifetimes via `'static`), moves the `WorkerPool` into the spawned closure by ownership, and recovers the result via `mpsc::channel`. The hung thread is leaked when the watchdog fires; the test process exits soon after, which reaps the OS thread.

```rust
fn run_iteration_with_watchdog(
    pool: WorkerPool,
    args: cli::Args,
    out_dir: PathBuf,
    timeout: Duration,
) -> Result<IterationOutcome, HarnessError> {
    let (tx, rx) = mpsc::channel();
    let _hung_thread = std::thread::spawn(move || {
        let mut pool = pool;
        let r = run_iteration(&mut pool, &args, &out_dir);
        let _ = tx.send(r);
    });
    rx.recv_timeout(timeout)
        .unwrap_or_else(|_| panic!("run_iteration hung past {:?}", timeout))
}
```

`WorkerPool`, `cli::Args`, and `PathBuf` are all `Send + 'static` (no lifetime parameters; all interior types are `Send`). Ownership transfer is the cleanest way to get past Rust's borrow rules without `Mutex` / `Arc` machinery.

`Duration::from_secs(2)` is the recommended timeout — synthetic worker tests typically complete in ~50 ms; flake margin is 40×.

### 4.3 PGN tightening + parametric sweep

The pgn-format survivors are at lines 5376 (`if i < moves.len()` after `i += 2`) and 5390 (`if !moves.is_empty()`).

(PGN mutants are renamed to **PL, PM, PN** to avoid identifier collision with controller mutants L, M, N1–3 in §4.2.)

**Mutant PL (line 5376, `<` → `<=`):** would push a trailing space after the last move pair. Output: `1. e2e4 e7e5  1-0\n` (double space before result marker). Existing `pgn_white_wins_startpos_formats_to_seven_tag_roster_plus_comments` only asserts `pgn.trim_end().ends_with("1-0")` (silently absorbs internal whitespace).

**Mutant PM (line 5376, `<` → `==`):** would push space only when `i == moves.len()` — same observable effect (trailing space after last pair). Same kill as PL.

**Mutant PN (line 5390, `!` removed → `if moves.is_empty()`):** for n=0, would push a leading space before the result marker (`" 1/2-1/2\n"`); for n≥1, would skip the separator (`...lastmove1-0\n` with no space). Existing `pgn_empty_moves_omits_body_but_keeps_result` only asserts `pgn.trim_end().ends_with("1/2-1/2")` (absorbs leading space). Existing `pgn_white_wins_startpos_*` likely catches n≥1 case via `contains` of the move sequence — to be verified.

**Strategy:** add ONE test with strict assertions across multiple move counts:

```rust
#[test]
fn format_pgn_pins_separator_and_result_spacing() {
    let header = make_minimal_header_with_result("1-0");
    for n in [0_usize, 1, 2, 3, 4, 5] {
        let moves: Vec<PgnMove> = (0..n).map(|i| make_pgn_move_uci(i)).collect();
        let pgn = format_pgn(&header, &moves);
        let body = body_section(&pgn);  // lines after the header's blank-line separator

        if n == 0 {
            // n=0: body is exactly the result marker on its own line, no leading space.
            assert_eq!(body, "1-0\n", "n=0 body must be '1-0\\n', got {body:?}");
        } else {
            // n≥1: body must end with " 1-0\n" (single space before result, then newline).
            assert!(body.ends_with(" 1-0\n"), "n={n} body must end with ' 1-0\\n', got {body:?}");
            // No internal double-spaces (kills L376 boundary mutants).
            // We strip the trailing " 1-0\n" first because legitimate single space precedes it.
            let body_without_result = &body[..body.len() - " 1-0\n".len()];
            assert!(!body_without_result.contains("  "),
                "n={n} body has unexpected double-space: {body_without_result:?}");
            // Move-pair separator: at every "N. " token (for N>=2) the preceding char must be ' '.
            // (Kills L376 `<` → `==` case where separator is dropped at all-but-the-boundary positions.)
            for k in 2..=((n + 1) / 2) {
                let sep_substr = format!(" {}. ", k);
                assert!(body_without_result.contains(&sep_substr),
                    "n={n} expected substring '{sep_substr}' as move-pair separator, got {body_without_result:?}");
            }
        }
    }
}
```

`body_section` is a tiny helper that splits on `"\n\n"` and returns the second half. `make_minimal_header_with_result` is a 5-line stub.

This single test pins all three target pgn mutants across the boundary (n=0 distinguishes the L5390 empty-case; n=1 distinguishes the L5390 non-empty case and the L5376 boundary at the single-move ending; n≥2 distinguishes L5376's `<` → `<=` and `<` → `==`).

**Move-pair separator formula `for k in 2..=((n + 1) / 2)`** (intentional unified form): for even n, this gives `k ∈ [2, n/2]` (n/2 pairs total → checks for separators at indices 2..=n/2); for odd n, `k ∈ [2, (n+1)/2]` covers the trailing white-only "pair" at the half-pair index (e.g., n=3 → k=2..=2 → checks for " 2. " before the trailing white move). For n=1, the range `2..=1` is empty (no separators expected); for n=0, the entire `else` branch is skipped. Add a one-line comment in the test body to flag this.

**No need for n=10 or n=11.** The structural property is the same at all `n ≥ 4`; the sweep at n ∈ {0,1,2,3,4,5} covers boundary, odd, even, and "more than one pair" cases.

### 4.4 Mutants.toml updates

Order of operations (this is THE concrete step list):

1. **Comment out** `"in controller::run_iteration"` at line 368. Comment out `"in pgn::format_pgn"` at line 406. Run `cargo mutants -f src/bin/elo-iterate.rs -F 'controller::run_iteration|pgn::format_pgn'` (single regex with alternation; `-F` does not stack) to enumerate residuals.
2. **For each residual**, apply the §4.1 manual-mutation triage and add tests OR document as equivalent.
3. **Re-add narrower exclusions** for residuals classified as integration-only or structurally-equivalent. Concrete expected re-exclusions:
   - **File-IO call sites in `run_iteration`**: `replace std::fs::write(...) with Ok(())` and `replace summary::append_summary_line(...) with Ok(())` and similar IO no-ops. The cargo-mutants display form is generally `replace <path>::<fn> with Ok(())` or `delete ! in <fn>`. Best handled by extracting the IO into a `write_game_artifacts(...)` helper and excluding the helper, or by keeping a tightly-anchored exclusion like `r"std::fs::write in controller::run_iteration"`. Choose at survey time.
   - `"replace summary::append_summary_line ->"` — already present for the callee; keep.
   - The `+= 1 → += 0` and `*= 1 → *= 0` style mutants for `wins/losses/draws` counters at lines 7281–7286 if they survive — likely caught by `outcome.wld == (W, L, D)` assertions in existing tests, but verify.
4. Document each retained exclusion's rationale in `.cargo/mutants.toml` per the file's own conventions (lines 30–71): no line anchors; use function-name regex; explain *why* equivalent or deferred.

## 5. Order of operations

Single-thread, no parallelism worth invoking:

1. Lift the function-anchored exclusions in `.cargo/mutants.toml` (comment them out; do not delete yet — keep the audit prose alongside until the new exclusions land).
2. Run cargo-mutants scoped to the two target functions. cargo-mutants 27.0.0's `-F` / `--re` flag accepts a single regex (later flags overwrite earlier ones), so use **alternation in one regex**, not two flags:
   ```bash
   cargo mutants -f src/bin/elo-iterate.rs -F 'controller::run_iteration|pgn::format_pgn'
   ```
   to enumerate residuals against the current test suite.
3. For each residual: triage per §4.1 (caught / write test / equivalent / re-exclude). For hang-class mutants (A, J, L, M, N1 from §4.2 plus any new ones found during survey), apply the watchdog strategy from §4.2.
4. Write the §4.3 pgn test + the watchdog wrapper helper (§4.2) + the survivor-targeted controller tests. Verify each test catches its target mutant by manual mutation (per `.cargo/mutants.toml` lines 11–28).
5. Re-run `cargo mutants -f src/bin/elo-iterate.rs -F 'controller::run_iteration|pgn::format_pgn'` to verify residuals are either zero or all explicitly re-excluded.
6. Pre-review mechanical checks: `cargo fmt --check`, `cargo clippy --all-targets`, `cargo test --bin elo-iterate`, `cargo llvm-cov` summary. Surface coverage deltas in chat.
7. Final-reviewer blind loop on the diff (code + tests + mutants.toml + plan).
8. Update `docs/tooling-backlog.md`: mark Done, append the Mock-engine follow-up.
9. Commit + push.

## 6. Follow-up backlog item to append

Append to `docs/tooling-backlog.md` (above the active-queue terminator):

> ### Mock-engine fixture for `production_worker_fn` setoption ordering
>
> **Surfaced.** During the `tooling-eloh-controller-boundary-tests` plan (2026-05-08), as the deferred slice of the original "ELOH controller boundary testing" backlog item.
>
> **Purpose.** Close the structural gap that prevents `controller::production_worker_fn` from being unit-testable: the function spawns real engine subprocesses and drives UCI over stdin/stdout pipes. The setoption ordering invariant currently pinned by inline comment + `#[ignore]`-gated e2e smoke would gain a unit-test pin, plus ~5 additional `production_worker_fn` mutants would become catchable.
>
> **Specific ordering to enforce in the fixture's UCI driver.** Per pair: `setoption name UCI_Elo value <N>` → `setoption name UCI_LimitStrength value true` → `isready` → wait for `readyok` → `ucinewgame` → `isready` → wait for `readyok` → `position startpos` → `go ...`. The fixture must record the exact ordering of incoming commands and the test asserts the sequence. Per `docs/research/tooling-stockfish-mid-session-setoption.md`, this ordering is what the harness commits to.
>
> **Scope.** Either (a) a `tests/fixtures/mock-engine` binary target driven by env vars / a TOML script that responds to UCI commands as scripted, plus an `EngineSpec { path: <fixture binary>, ... }` test harness, OR (b) a trait abstraction over `driver::EngineHandle` so the controller can be parameterized over real-vs-mock at test time. Option (a) is preferred (less invasive).
>
> **Estimated size.** ~250–400 LOC (fixture binary + 4–6 tests + harness wiring).
>
> **When to land.** Pull when ELOH-controller mutation coverage becomes a load-bearing blocker on a future controller refactor. Not gating any milestone today.

## 7. Test names (alphabetical, for cross-reference)

Controller (`mod controller::tests`) — final names depend on §4.1's actual survivor set; these are the working drafts:

- `run_iteration_breaks_at_max_games_does_not_overshoot` (mutant C)
- `run_iteration_bootstrap_pair_indices_form_zero_to_n_minus_one` (mutant B1; possibly redundant with the existing `dispatch_round_robin_one_pair_per_worker` — drop if manual mutation confirms existing test catches)
- `run_iteration_bootstrap_in_flight_increment_pins_drain_done` (mutant D)
- `run_iteration_redispatch_pair_indices_form_strictly_ascending_sequence` (mutant F — line 7406's `*= 1` mutation; key new test)
- `run_iteration_redispatch_dispatches_exactly_total_pairs_no_overshoot` (mutant K — caught via panic on out-of-bounds index)
- `run_iteration_does_not_hang_on_bootstrap_break` (mutant A, with watchdog)
- `run_iteration_does_not_hang_on_in_flight_decrement` (mutant J, with watchdog)
- `run_iteration_does_not_hang_on_redispatch_pd_eq_tp_boundary` (mutants L, M, with watchdog)
- `run_iteration_does_not_hang_on_terminating_gate` (mutant N1, with watchdog)

PGN (`mod pgn::tests`):

- `format_pgn_pins_separator_and_result_spacing` (the L5376 and L5390 boundary mutants)

Helper (`mod controller::tests`, test-only):

- `fn run_iteration_with_watchdog(...)` (~15 LOC; reusable across hang-class tests)

## 8. Parallelization map

**None — sequential.** The §4.1 triage workflow is iterative: each survivor's resolution informs the next. Parallel agents on different mutants would step on the source-edit-revert cycle. Total scope is ~200 LOC across two `mod tests` blocks in one file; one agent finishes in one pass.

## 9. Risks and unknowns

- **The audit comment may be wrong.** ELOH.B's claim of 5 specific run_iteration survivors is a 2026-04-30 snapshot; ELOH.C/D/E added tests that may have killed some of them. The §4.1 enumeration step measures the actual residual; the §4.2 draft table is hand-mutation analysis to be verified during survey.
- **Watchdog timing flake.** A 2 s watchdog is generous for run_iteration on synthetic workers (typical wallclock ~50 ms); macOS-14 CI runners tolerate this. If the watchdog fires under the original code on a slow runner, raise to 5 s — flake is on the false-positive side, never silently passing the mutant.
- **`thread::scope` was rejected** as the watchdog primitive because `thread::scope`'s closure waits for spawned threads to join before returning, defeating the timeout. The chosen pattern in §4.2 uses `std::thread::spawn` with ownership transfer (`WorkerPool: Send + 'static`); the hung thread is leaked when the watchdog fires, and the OS reaps it when the test process exits. This is the load-bearing primitive for hang-class mutants A, J, L, M, N1.
- **`cargo mutants -F` syntax.** cargo-mutants 27.0.0 uses `-F PATTERN` / `--re PATTERN` for a single regex. Stacking two `-F` flags causes the later one to overwrite the earlier; use alternation in a single regex (`-F 'A|B'`) to test multiple functions in one run.
- **Bootstrap mutants D, E, K may panic during cargo-mutants run** instead of cleanly producing a "missed" verdict. cargo-mutants treats panics as caught (per its docs) — fine. But local `cargo test` will surface the panic as a failure if I leave the mutation applied; remember to revert before committing.

## 10. Bench / commit

No production-code changes ⇒ no bench delta. Commit message: `tooling: ELOH controller boundary tests + format_pgn sweep`.

Body format (template — fill from §5 step 5's residual report):

```
Mutants closed in controller::run_iteration: <list of mutant IDs from §4.2 that flipped from missed to caught, e.g. "A, B1, C, D, F, H, J, K, L, M, N1, N2, N3">.
Mutants closed in pgn::format_pgn: <list, e.g. "PL, PM, PN">.
Mutants kept excluded as structurally equivalent or integration-only: <list with one-line rationale per pattern>.

`.cargo/mutants.toml` exclusions narrowed: function-anchored → mutant-pattern-anchored. Specifically: <named patterns retained vs removed>.

Backlog item "ELOH controller boundary testing" closed; follow-up "Mock-engine fixture for production_worker_fn" appended to docs/tooling-backlog.md.

LOC: ~200 test + ~10 mutants.toml.
```
