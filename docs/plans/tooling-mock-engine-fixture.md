# Plan: Mock-engine fixture for `production_worker_fn` setoption ordering

**Unit name.** `tooling-mock-engine-fixture`
**Source backlog item.** `docs/tooling-backlog.md` §"Mock-engine fixture for `production_worker_fn` setoption ordering"
**Estimated size.** ~280–380 LOC: ~100–120 LOC mock-engine binary + ~150–200 LOC tests + ~20 LOC of `.cargo/mutants.toml` adjustments + ~5 LOC `Cargo.toml` `[[bin]]` declaration.

## 1. Problem statement

`controller::production_worker_fn` (the per-worker thread that drives one engine + one opponent through the UCI handshake and per-pair flow) is currently structurally untestable: it spawns real engine subprocesses and drives UCI over stdin/stdout pipes. Two consequences:

1. **`.cargo/mutants.toml` parks the entire function as integration-only** (lines 338–339):
   ```
   "in controller::production_worker_fn",
   "replace controller::production_worker_fn with",
   ```
   Both patterns are function-anchored blanket exclusions. cargo-mutants 27.0.0 lists ~70+ generated mutants in the function (we'll measure exactly during §4.1); none are exercised by `cargo mutants`.
2. **The setoption-before-ucinewgame ordering invariant is pinned only by an inline comment** at lines 7019–7034 plus the `#[ignore]`-gated end-to-end smoke (`end_to_end_clawfish_self_play_4_games`). Per `docs/research/tooling-stockfish-mid-session-setoption.md` (the 2026-04-29 Stockfish 18 probe), this ordering is a **load-bearing UCI-protocol hygiene point**: while Stockfish 18 honors mid-session `UCI_Elo`, the harness defends against engines that may reset state on `ucinewgame` by always sending `setoption` *before* `ucinewgame` within a pair. A single off-by-one in the order would silently produce wrong-strength play, invisible to the unit-tested `synthetic_pool` fixtures (which simulate worker outputs without driving real UCI commands).

The mock-engine fixture closes both gaps: a scripted UCI mock binary records every command it receives, the controller drives the full `production_worker_fn` flow against two mock instances, and tests assert the recorded UCI command sequence.

## 2. Approach choice — option (a): fixture binary

The backlog item names two options:
- (a) `tests/fixtures/mock-engine` binary target driven by env vars / a TOML script that responds to UCI commands as scripted, plus an `EngineSpec { path: <fixture binary>, ... }` test harness.
- (b) Trait abstraction over `driver::EngineHandle` so the controller can be parameterized over real-vs-mock at test time.

**Choosing (a).** Rationale:

- (a) is **non-invasive** to the production driver: zero changes to `mod driver`, zero changes to `EngineHandle`, zero changes to the spawn / send / recv plumbing. A real subprocess driving a real UCI flow is what production runs.
- (b) would require a trait ~10 method surface (spawn, send_line, recv_until_bestmove, wait_for_uciok, wait_for_readyok, shutdown, plus accessors for `last_info`/`shutting_down`/`name`) propagated through every call site in `production_worker_fn`, `match_loop::play_one_game`, and the controller. Touch surface: 30+ sites. Invasive across half of `elo-iterate.rs`.
- (a) tests the *real* subprocess plumbing (Cargo-built binary, real stdin/stdout pipes, real reader thread) — same code path as production. (b) would test a trait-mocked path that diverges from production.
- The `EngineSpec.launch_prefix: Option<Vec<String>>` field already accepts a list of words to prepend to argv. By using `launch_prefix = Some(["/usr/bin/env", "MOCK_ENGINE_RECORD_PATH=/tmp/foo"])`, the test injects per-instance environment variables without modifying `EngineSpec` or `spawn_engine`. **Zero production-code changes.**

## 3. Files modified

Five files:

- **`Cargo.toml`** — declare `[[bin]] name = "mock-engine"` (~5 lines).
- **`src/bin/mock_engine.rs`** — new file (~100–120 LOC). Test fixture binary; minimal UCI mock that records received commands.
- **`src/bin/elo-iterate.rs`** — add tests under `mod controller::tests` (~150–200 LOC). Zero production-code changes in this file. (Add a small recording-file path resolver helper; use the existing `resolve_bin` walk pattern from `mod e2e_smoke`.)
- **`.cargo/mutants.toml`** — narrow the `production_worker_fn` exclusions; add `src/bin/mock_engine.rs` to `exclude_globs` (test fixture, not engine code).
- **`docs/tooling-backlog.md`** — strike the closed item, move to "Done since the 2026-04-27 review" with a one-paragraph closure summary.

## 4. Mock-engine binary design

`src/bin/mock_engine.rs`. Pure Rust, std-only, ~80 LOC. Reads one env var, reads UCI commands from stdin, appends them to the recording file, replies per a minimal protocol subset.

### 4.1 Environment variables

- **`MOCK_ENGINE_RECORD_PATH`** *(required)*: path to the recording file. Mock appends one line per received UCI command. Each test creates two distinct recording paths (one per mock instance) under a unique temp-dir per test. **Failure mode:** if the env var is unset or empty, the mock prints a clear diagnostic to stderr and `std::process::exit(2)` immediately on startup. If the path is unwriteable on first open, the mock panics with the underlying I/O error. Fixture code; loud failure beats silent recording loss.
- **`MOCK_ENGINE_VIRTUAL_CLOCK_ADVERTISED`** *(optional, default `"0"`)*: when `"1"`, the mock includes `option name VirtualClock type check default false` as a discrete line in its `uci` response. Used by the VirtualClock-negotiation test.

No other inputs. The mock has no scripted-move file, no error-injection mode — minimal scope.

**Permissiveness to unknown commands.** The mock is intentionally permissive: any UCI command not in the §4.2 dispatch table is recorded and silently ignored (no reply, no error). Rationale: future evolution of `production_worker_fn` may add new sends (e.g. `setoption name Hash value 64`) that should not break this fixture. Per-test assertions key on **substring or per-line presence**, not on strict line counts, so an extra recorded line does not flake a green test.

### 4.2 UCI command protocol (mock side)

Each "Reply" cell below describes a sequence of **discrete output lines**, each emitted via a separate `writeln!(stdout, "…")` followed by `stdout.flush()`. The reader thread on the harness side splits incoming bytes on `\n` (via `BufReader::lines`), so each reply line must be its own write. The recorder side mirrors this: every received UCI line is a separate line in the recording file.

| Received | Recorded? | Reply lines |
|---|---|---|
| `uci` | yes | (1) `id name MockEngine` &nbsp;(2) `id author clawfish-test` &nbsp;(3) `option name UCI_Elo type spin default 1320 min 1320 max 3190` &nbsp;(4) `option name UCI_LimitStrength type check default false` &nbsp;(5, conditional) `option name VirtualClock type check default false` *(emitted only when `MOCK_ENGINE_VIRTUAL_CLOCK_ADVERTISED=1`)* &nbsp;(6) `uciok` |
| `setoption …` | yes | (no reply — UCI spec) |
| `isready` | yes | `readyok` |
| `ucinewgame` | yes | (no reply) |
| `position …` | yes | (no reply) |
| `go …` | yes | `bestmove 0000` (single line, emitted immediately) |
| `quit` | yes | (no reply, then `std::process::exit(0)` after recording the line and flushing) |
| anything else | yes | (no reply; mock is permissive — see §4.1) |

**Why `bestmove 0000`?** `match_loop::play_one_game` calls `Move::from_uci(...)` on the `bestmove` payload (line 6459). `0000` is the UCI null-move encoding, which `Move::from_uci` rejects on a real position → `play_one_game` returns `IllegalMove(active_color)` at line 6469 **before** advancing the side-to-move. So each game ends after exactly **one ply** at the side-to-move's handle. This is the shortest legal way to terminate a game without giving the mock real chess knowledge.

**Per-game UCI footprint** (consequence of the one-ply termination):
- For each game, `production_worker_fn` sends `ucinewgame` + (after `wait_for_readyok`) `isready` to **both** engine and opponent (lines 7058–7063).
- Then `play_one_game` sends `position …` + `go …` to **only the side-to-move's handle** for the first ply, receives `bestmove 0000`, and returns `IllegalMove(active_color)` immediately. The non-side-to-move handle receives nothing inside `play_one_game`.
- For game 1 of a pair (`clawfish_white=true`): engine plays White → engine receives `position startpos` + `go …`; opponent receives nothing inside `play_one_game`.
- For game 2 (`clawfish_white=false`): opponent plays White → opponent receives `position startpos` + `go …`; engine receives nothing inside `play_one_game`.

This per-game-asymmetry shape is what tests T5a and T5b assert (see §5.4).

### 4.3 Recording file format

One UCI command per line, exactly as received (after `\n`-trimming). The mock's main loop reads one line, opens the file in append mode, writes the line + `\n`, flushes, closes. Concurrent writes from a single mock instance are serialized by the loop; no two instances share the same recording file (test wires them separately).

### 4.4 Test fixture is excluded from mutation testing

The mock is test fixture code, not engine code. Add `"src/bin/mock_engine.rs"` to `.cargo/mutants.toml`'s `exclude_globs`, alongside `magicgen.rs` (already excluded as a codegen tool, not engine code).

## 5. Test scope

All tests live under `mod controller::tests` in `src/bin/elo-iterate.rs`, in a new test cluster (separate from existing `synthetic_pool`-based tests).

### 5.1 Helper: `resolve_mock_engine_bin()`

Mirror the existing `e2e_smoke::resolve_bin` pattern (line 10722). Walk from `current_exe()` up two parents to `target/<profile>/`, look for `mock-engine`. The binary is built automatically by `cargo test` because it's declared as `[[bin]]` in `Cargo.toml`.

### 5.2 Helper: `build_engine_spec_for_mock(name, record_path, advertise_vc)`

```rust
fn build_engine_spec_for_mock(
    name: &str,
    mock_path: &str,
    record_path: &std::path::Path,
    advertise_vc: bool,
) -> driver::EngineSpec {
    let mut prefix = vec![
        "/usr/bin/env".to_string(),
        format!("MOCK_ENGINE_RECORD_PATH={}", record_path.display()),
    ];
    if advertise_vc {
        prefix.push("MOCK_ENGINE_VIRTUAL_CLOCK_ADVERTISED=1".to_string());
    }
    driver::EngineSpec {
        name: name.to_string(),
        path: mock_path.to_string(),
        launch_prefix: Some(prefix),
    }
}
```

### 5.3 Helper: `run_one_pair_against_mocks(...)` → `(engine_log, opponent_log)`

Sets up a fresh temp dir, builds two `EngineSpec`s, builds a `WorkerConfig`, calls `controller::spawn_workers(1, cfg)`, sends one `WorkerCmd::PlayPair`, drains reports until `PairComplete` (with watchdog), **drops the senders, then explicitly joins the worker thread** so the mock has finished recording its `quit` line and `driver::shutdown` has reaped both child processes, **then** reads the two recording files. Returns the line vectors.

```rust
fn run_one_pair_against_mocks(
    engine_options: Vec<(String, String)>,
    opponent_options: Vec<(String, String)>,
    virtual_clock: bool,
    advertise_vc_engine: bool,
    advertise_vc_opponent: bool,
    opponent_uci_elo: u32,
) -> (Vec<String>, Vec<String>) { ... }
```

The two `advertise_vc_*` booleans exist only for T7's asymmetric-advertisement scenario; T1–T6 and T8 pass `(false, false)`.

**Worker-thread join is load-bearing.** `WorkerPool::drop` only clears the cmd senders by design (joining in Drop could block forever on a panicking worker); `JoinHandle::drop` does NOT join either. Without an explicit join, the test reads the recording file while the mock is still mid-`quit` (or the worker is mid-`driver::shutdown`'s up-to-1 s child-poll). The fix is to take ownership of the pool's `join_handles` (`std::mem::take(&mut pool.join_handles)`) after dropping the senders, then `for h in handles { let _ = h.join(); }`. Same primitive as the `tooling-eloh-controller-boundary-tests` `run_iteration_with_watchdog` precedent (ownership transfer of pool to a thread we then join), adapted here for a synchronous join with a timeout.

For the timeout path: spawn a small "join with timeout" wrapper using a one-shot `mpsc::channel` to bound the join — same pattern as the boundary-tests' watchdog. Recommended timeout: `Duration::from_secs(10)` (mock pair completes in ~50 ms typical; 10 s is 200× margin to absorb CI scheduler stalls).

The watchdog catches mocks that hang on a malformed reply path (none expected; defense-in-depth — mocks could crash on a future production change to `production_worker_fn` and we want a test failure, not a hung CI).

### 5.4 Tests

Each test asserts a specific load-bearing property of the recorded UCI sequence. **Symmetry assertions** (negative checks on the *other* log) appear in T1, T3, T6, and T7 — these close the wrong-side / engine↔opponent swap mutants that positive-only assertions would miss.

| # | Test name | What it pins | Mutants closed (predicted) |
|---|---|---|---|
| **T1** | `production_worker_fn_emits_setoption_uci_elo_before_ucinewgame_per_pair` | (a) In opponent_log: every `setoption name UCI_Elo …` line precedes the next `ucinewgame` that follows it. (b) **Negative-symmetry:** engine_log contains NO `setoption name UCI_Elo` line. | The reorder mutants on the per-pair phase (anything hoisting `ucinewgame` above the setoption block); the engine↔opponent swap mutant on the per-pair `setoption UCI_Elo` send. |
| **T2** | `production_worker_fn_emits_uci_elo_with_correct_value` | In opponent_log: the `setoption name UCI_Elo value <N>` line carries `<N>` exactly equal to `opponent_uci_elo` from the cmd payload (test uses `2400`). | `format!`-payload mutants on the value-substitution (`{opponent_uci_elo}` → `{}`/`0`/etc). |
| **T3** | `production_worker_fn_emits_setoption_limitstrength_after_uci_elo_per_pair` | (a) In opponent_log: `setoption name UCI_LimitStrength value true` is the **first** opponent_log line after the per-pair `setoption name UCI_Elo …`, with no UCI command between them. (b) **Negative-symmetry:** engine_log contains NO `setoption name UCI_LimitStrength` line. | The reorder-within-pair mutants; the `delete` mutant on the LimitStrength send; the engine↔opponent swap mutant. |
| **T4** | `production_worker_fn_emits_isready_after_setoption_block_before_ucinewgame` | Per pair, in opponent_log: an `isready` line follows the per-pair setoption block and precedes the next `ucinewgame`. | The `delete wait_for_readyok` mutant on the post-setoption sync (line 7044). |
| **T5a** | `production_worker_fn_emits_ucinewgame_then_isready_per_game_for_both_engines` | Per game (2 per pair), BOTH engine_log and opponent_log contain `ucinewgame` followed by `isready` (with no UCI command between them in their respective logs). Concretely: engine_log contains a sub-sequence `ucinewgame, isready, ucinewgame, isready`; opponent_log contains the same sub-sequence — both pinning the per-game send order at lines 7058–7063 of `production_worker_fn`. | The `delete ucinewgame` / `delete wait_for_readyok` mutants in the per-game arm; reorder mutants. |
| **T5b** | `production_worker_fn_routes_position_and_go_to_side_to_move_per_game` | engine_log contains exactly **one** `position startpos` line and exactly **one** line beginning `go ` (game 1 of pair, where engine plays white). opponent_log contains exactly **one** `position startpos` line and exactly **one** `go ` line (game 2 of pair, where opponent plays white). The strict count is a **deliberate exception** to §4.1's loose-count guidance: `position` and `go` are sent exactly once per ply by `play_one_game`, and the game ends after one ply via `IllegalMove(White)`, so any deviation from the count of 1 per side-to-move log is a routing bug, not future-evolution noise. | The `clawfish_white = game_in_pair == 0` color-assignment mutant; the `(side == Color::White) == (ctx.white_engine_index == 0)` swap mutants in `play_one_game`'s active-handle selection at line 6393. |
| **T6** | `production_worker_fn_applies_engine_options_during_handshake_not_per_pair` | engine_log contains `setoption name <K> value <V>` for each `engine_options` entry, all appearing **before** the first `ucinewgame`. opponent_log similarly for `opponent_options`. **Negative-symmetry:** engine_log does NOT contain any `setoption` line for an `opponent_options` entry, and vice versa. | The mutants that swap the two option-loops, drop one, or move them inside the per-pair PlayPair branch. |
| **T7** | `production_worker_fn_negotiates_virtual_clock_with_advertising_engine_only` | Configure mock-A to advertise VirtualClock and mock-B to NOT advertise; set `virtual_clock=true`. With clawfish-engine = mock-A, opponent = mock-B: engine_log contains `setoption name VirtualClock value true`; opponent_log does NOT. | The `&&` → `\|\|` mutant on the `cfg.virtual_clock && engine_caps.supports_virtual_clock` gate at line 6979; the engine↔opponent swap mutant on the VirtualClock send. |
| **T8** | `production_worker_fn_emits_quit_to_both_engines_on_shutdown` | After the helper's worker-join (§5.3), both recording files contain a final `quit` line. | The `let _ = super::driver::shutdown(engine);` / `… shutdown(opponent);` deletion mutants at lines 7143–7144 of `production_worker_fn` (NOT `driver::shutdown` itself, which remains integration-only-excluded — T8 is testing the **invocation** of shutdown by `production_worker_fn`). |

**Per-pair count.** All tests run with `pair_index=0, opponent_uci_elo=2400` (a non-default value to distinguish from the default 1320 / 0). T6 sets engine/opponent options. T7 sets virtual_clock=true. The PlayPair sends 1 pair = 2 games; in both games the side-to-move (White) plays `bestmove 0000` and the game ends as `IllegalMove(White)` after 1 ply (game 1: clawfish-white loses; game 2: clawfish-black, opponent-white loses).

### 5.5 What's explicitly out of scope

- **Multi-pair / dispatch-loop tests.** Already covered by `synthetic_pool`-based controller tests (`run_iteration_*` in `mod controller::tests`). The mock fixture is for `production_worker_fn`'s UCI-emission contract, not the controller's dispatch logic.
- **Concurrency=N tests.** A single PlayPair against a single worker is sufficient to pin the per-pair UCI ordering. Concurrency adds no signal at this layer (the pair phase is per-worker-local).
- **Failure-path tests.** Mock crash mid-pair, mock timeout, mock returns unparseable response — these would test `production_worker_fn`'s failure handling, not the happy-path UCI ordering. Out of scope per the backlog item; deferred.
- **`controller::run_iteration` mutant survey re-run.** That work was completed in `tooling-eloh-controller-boundary-tests` (2026-05-08). This unit only addresses `production_worker_fn`.

## 6. `.cargo/mutants.toml` updates

After tests pass, run `cargo mutants -f src/bin/elo-iterate.rs -F 'controller::production_worker_fn'` to enumerate residuals, then narrow the existing function-anchored exclusions:

1. **Lift** the two `production_worker_fn` blanket exclusions:
   ```
   "in controller::production_worker_fn",
   "replace controller::production_worker_fn with",
   ```
2. **Run survey.** Manually triage residual survivors per `.cargo/mutants.toml` lines 11–28's edit-test-revert workflow.
3. **Re-add narrow exclusions** for the genuinely integration-only or structurally-equivalent residuals. Expected categories:
   - File-IO call sites inside the per-pair `GameComplete` arm (the harness writes per-game PGNs in `controller::run_iteration`, not in `production_worker_fn`, so this should not survive in `production_worker_fn`).
   - The `wait_for_uciok` / `wait_for_readyok` outer-wrapper functions (already excluded; kept).
   - Any equivalent mutants in the `let _ = …` ignored-error pattern; document.
4. **Add `src/bin/mock_engine.rs` to `exclude_globs`** alongside `magicgen.rs`.

The expected post-pass exclusion shape: a few specific narrow patterns (e.g. `replace driver::send_line with` for the in-loop sends that mocks don't observe; or `replace ... with Result::Err(...)` mutants that would only matter under engine-process failure injection). The function-anchored blanket gives way to mutant-pattern-anchored.

## 7. Order of operations

Single-thread execution. The §5 tests share a `resolve_mock_engine_bin` + `run_one_pair_against_mocks` helper; writing them in sequence reuses common scaffolding.

1. **Add `[[bin]] name = "mock-engine"` to `Cargo.toml`** with `path = "src/bin/mock_engine.rs"`.
2. **Write `src/bin/mock_engine.rs`** (the mock binary). Verify it builds via `cargo build --bin mock-engine`.
3. **Write tests T1–T8** under `mod controller::tests` in `src/bin/elo-iterate.rs`. Each test self-sufficient; helpers (`resolve_mock_engine_bin`, `build_engine_spec_for_mock`, `run_one_pair_against_mocks`) shared. The `run_one_pair_against_mocks` helper performs the load-bearing worker-join described in §5.3 before reading recordings.
4. **Test-suite review loop** (blind reviewer reads the mock + tests + plan + `docs/research/tooling-stockfish-mid-session-setoption.md` + the inline comment block at `production_worker_fn:7019–7034`).
5. **Run `cargo test --bin elo-iterate -- production_worker_fn`** to confirm tests pass against current code.
6. **Pre-review mechanical checks**: `cargo fmt --check`, `cargo clippy --all-targets`, `cargo test`, `cargo mutants -f src/bin/elo-iterate.rs -F 'controller::production_worker_fn'` (after temporarily commenting out the function-anchored exclusions). Triage residuals per §6. **Re-entry of the test-suite review loop is required if survey-driven new tests are added** (continue the same reviewer subagent via `SendMessage` per `docs/workflow.md` step 5 of the plan-review loop).
7. **Update `.cargo/mutants.toml`** per §6 — narrow exclusions, add fixture to `exclude_globs`.
8. **Final-review loop** — blind reviewer reads code + tests + plan + mutation-survivor analysis.
9. **Update `docs/tooling-backlog.md`** — strike the closed item, move to "Done since the 2026-04-27 review" with a one-paragraph closure summary.
10. **Commit** on a feature branch (`tooling/mock-engine-fixture`). No bench delta — pure tooling work, no production code change.

## 8. Test names (alphabetical, for cross-reference)

- `production_worker_fn_applies_engine_options_during_handshake_not_per_pair` (T6)
- `production_worker_fn_emits_isready_after_setoption_block_before_ucinewgame` (T4)
- `production_worker_fn_emits_quit_to_both_engines_on_shutdown` (T8)
- `production_worker_fn_emits_setoption_limitstrength_after_uci_elo_per_pair` (T3)
- `production_worker_fn_emits_setoption_uci_elo_before_ucinewgame_per_pair` (T1)
- `production_worker_fn_emits_uci_elo_with_correct_value` (T2)
- `production_worker_fn_emits_ucinewgame_then_isready_per_game_for_both_engines` (T5a)
- `production_worker_fn_negotiates_virtual_clock_with_advertising_engine_only` (T7)
- `production_worker_fn_routes_position_and_go_to_side_to_move_per_game` (T5b)

## 9. Parallelization map

**None — sequential.** The mock binary, the test helpers, and the eight tests are tightly coupled: every test reuses the same `run_one_pair_against_mocks` helper, and the helper depends on the mock binary's recording protocol. Parallel coding agents would step on each other's helper edits. Total ~280–380 LOC across five files (per §1 and §3); one agent in one pass.

## 10. Risks and unknowns

- **`/usr/bin/env` portability.** macOS-14 (CI primary) and Ubuntu (CI secondary) both have `/usr/bin/env` as a POSIX-standard utility. The launch_prefix uses the absolute path explicitly to avoid PATH lookup variance. **Mitigation if a future machine lacks it:** add an `envs: Vec<(String, String)>` field to `EngineSpec` (one-line additive change) — a follow-up that only fires if the portability assumption breaks.
- **`bestmove 0000` parse path.** Verified at `Move::from_uci` in `src/mov.rs` (or wherever the parser lives) — null-move encoding is not a valid move for any real position, so `from_uci` returns `Err(_)`, and `play_one_game` line 6469 returns `IllegalMove(active_color)`. Confirmed by reading the existing parser code; no surprise behavior expected.
- **Cargo build cost of the new bin.** The mock-engine binary is ~80 LOC, std-only. Compile time at `cargo test`: < 1 s on Apple Silicon. Negligible.
- **`cargo mutants` and the new bin.** cargo-mutants does NOT mutate test fixtures listed in `exclude_globs` — so `mock_engine.rs` doesn't add mutant-survey time. The narrowed `production_worker_fn` exclusions DO add survey time (the function-anchored blanket suppressed all 70+ mutants). Estimated added wallclock: ~5–10 min on first re-run (cargo-mutants caches across runs). Acceptable for the coverage gain.
- **Per-test temp-dir cleanup.** Tests use `std::env::temp_dir().join(...)`, with `remove_dir_all` at test start (mirrors the existing `e2e_smoke` pattern). On a single failure leaving stale files, the next run cleans them up. No leak on success.
- **Recording file flush ordering.** The mock flushes after each line write. The §5.3 helper joins each worker `JoinHandle` after dropping senders and *before* reading the recording files. The worker's `production_worker_fn` calls `driver::shutdown(engine)` and `shutdown(opponent)` at function exit, which sends `quit\n` to each child, polls for exit, and joins the reader thread. Once the worker join returns, the mock has recorded its `quit` line and exited. So the read-after-join sees a fully-flushed file. (Without the join, this is racy — see §5.3's "Worker-thread join is load-bearing" note.)
- **Watchdog tuning.** The pair completes in ~50 ms typical; 10 s watchdog is 200× margin. Same flake stance as the controller boundary tests' 2 s watchdog.

## 11. Bench / commit

No production-code change ⇒ no bench delta. Commit message:

```
tooling: mock-engine fixture for production_worker_fn UCI ordering

Mock binary at src/bin/mock_engine.rs that records every received UCI
command. Eight tests in mod controller::tests pin the per-pair UCI
sequence (setoption UCI_Elo before UCI_LimitStrength before isready
before ucinewgame; per-game ucinewgame/isready/position/go), the
handshake-time application of engine_options/opponent_options, the
VirtualClock negotiation gate, and the shutdown quit.

Mutants closed in controller::production_worker_fn: <list from §6 survey>.
Mutants kept excluded as integration-only or structurally-equivalent: <list>.

`.cargo/mutants.toml`: function-anchored production_worker_fn exclusions
narrowed to mutant-pattern-anchored. src/bin/mock_engine.rs added to
exclude_globs as test-fixture code.

Backlog item "Mock-engine fixture for production_worker_fn setoption
ordering" closed.

LOC: ~100–120 mock binary + ~150–200 tests + ~5 Cargo.toml + ~20 mutants.toml ≈ ~280–380 total.
```
