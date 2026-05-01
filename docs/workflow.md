# Workflow

How we work together on this project.

## The per-feature loop

The loop is **designed to run unattended**. The user starts a session with a prompt like "plan and implement M1.X," and the agent proceeds through every step to commit without proactively pausing. The user can interject at any point by sending a message — the agent pauses to read and respond, then resumes. But the default trajectory is end-to-end execution.

The blind-review loops at steps 4, 6, and 9 are the **primary quality control**, replacing the per-step user-approval gate of more interactive workflows. Reviewer concerns surface in chat as the loop runs (so the user reading along — synchronously or after the fact — sees what was raised and how it was addressed). The user can override any concern by sending a message; otherwise the agent acts on all reviewer concerns and continues.

**Stuck vs. uncertain.** If the agent gets genuinely stuck (ambiguous spec the research can't resolve, hard tool failure that's not a transient retry, an unforeseen architectural fork that contradicts ADRs), it surfaces the issue in chat and pauses. "Stuck" is the only break in the unattended trajectory. "Uncertain" is not stuck — when uncertain, the agent picks the most defensible path, notes the alternatives in chat, and continues.

The cycle:

1. **Deep prior-art research.** Web search, not training-data recall. Chess Programming Wiki, papers, blog posts, TalkChess threads, articles with illustrative snippets. **Not** engine source code (see restriction below). Devil is in the details. Delegate to a research subagent when it spans more than a few queries. Skippable when the unit is fully covered by prior-art notes already in `docs/prior-art.md` or `docs/research/` — the agent justifies the skip in chat.
2. **Synthesize findings in chat.** Tradeoffs, alternatives, gotchas. Informational; the agent does not wait.
3. **Choose approach.** The agent picks based on the research synthesis, consistency with project decisions (ADRs and `docs/architecture.md`), and the path of least architectural surprise. If multiple defensible options exist, the agent picks one and notes the alternatives in chat for awareness.
4. **Plan with plan-review loop.** Every implementable unit gets a written plan. The plan must identify **parallelization opportunities** — which subtasks can run on parallel coding agents. The plan goes through a blind-review loop until convergent. See "Plan mode and plan-review loop" below.
5. **Write tests** for the entire task scope, where the layer admits TDD (see TDD scope below). This includes both ordinary unit tests and property tests (via `proptest`) where the invariant is more compact than an enumeration — written and reviewed *together*, before implementation. Property tests do not replace specific unit tests; see "Property tests vs. unit tests" below. Parallelizable across coding agents per the plan.
6. **Test-suite review loop.** Independent reviewer checks the test suite for correctness to spec, confirmation bias, adequate checks, corner case coverage. See "Test-suite review loop" below. Implementation does not begin until the test suite passes review.
7. **Implement.** Parallelizable across coding agents per the plan.
8. **All tests pass.** No pre-review checks or commit until they do.
9. **Pre-review mechanical checks.** Orchestrator runs `cargo fmt --check`, `cargo clippy`, `cargo llvm-cov`, `cargo mutants --in-diff` on the unit's diff. Triages each mutation survivor (caught / equivalent / deferred / catchable-by-adding-test) per "Pre-review mechanical checks" under "Final review loop" below. Surfaces the analysis in the reviewer's spawn prompt.
10. **Final review loop on code + tests jointly.** Independent reviewer reads code + tests + plan + the orchestrator's mechanical-check analysis. Judges correctness, corner cases, code quality, readability, simplicity, performance considerations, AND whether the orchestrator's coverage-gap and mutation-survivor classifications are sound. Reviewer does NOT re-run cargo-mutants or cargo-llvm-cov — those are pre-review work. See "Final review loop" below.
11. **Benchmark and profile.** Record results. Compare to previous baseline.
12. **Commit + push.** Final step of the unit's loop. Conventional commit message describing what landed (e.g. `M1.A: project skeleton, square and bitboard primitives`). Stage only the files belonging to this unit; leave any unrelated in-flight work in the working tree alone. Push to `origin/main` after the commit lands; the project is on GitHub and the upstream branch tracks it.

Skipping any review loop strips the project of its primary quality control (the user is no longer the per-step gate; the reviewers are). When the agent skips a step, it must justify in chat.

## Plan mode and plan-review loop

**Every implementable unit goes through plan mode.** No code is written until a plan exists, has survived the plan-review loop, and has user approval.

A "unit" is a phase of a milestone (e.g. M1.A, M1.B), or a discrete feature within a phase if the phase is large. The roadmap decomposes milestones into units sized for **~300–800 lines of resulting code, with ~500 as the typical target**. Review-loop cost scales roughly linearly with artifact size, and smaller units keep the prompt cache warm across passes. Sub-divide larger units unless decomposition would genuinely fragment a coherent change.

The plan itself, **written to a file** (`docs/plans/<unit>.md`) so the reviewer can read it, names:

- Files created/modified.
- The type definitions and function signatures the plan introduces.
- Module boundaries.
- Test coverage strategy and specific test names.
- Order of operations.
- Dependencies on other units.
- **Parallelization map.** Which subtasks (writing tests, implementing modules) can be executed concurrently by separate coding agents, or "none — sequential" if parallelism is not practical for this unit. Honest about dependencies; don't overstate parallelism.

**Target plan length: ~300 lines.** Plans are re-tokenized on every reviewer pass — past ~300 lines, cost grows faster than quality contribution. Keep in the plan: file/function/type signatures, test names, parallelization map, order of operations, dependencies. Push to a referenced `docs/research/<unit>.md` note: empirical probe tables, full re-derivation of already-decided ADRs, speculative future-extension notes, background on alternatives that were considered and rejected.

### The plan-review loop

**Review is done by a fresh subagent, never by the main agent on its own work.** Main-agent self-review fails because the main agent is biased by the context that produced the plan — it can rationalize weaknesses because it remembers *why* the plan ended up that way. A fresh agent reading only the artifact has no such bias.

1. **Main agent writes v1 of the plan** to `docs/plans/<unit>.md`.
2. **Main agent launches a blind reviewer subagent.** The subagent's *only* inputs are:
   - The plan file.
   - Anything else in the project directory it chooses to read (CLAUDE.md, `docs/architecture.md`, the ADRs in `docs/decisions/`, `docs/prior-art.md`, `docs/research/`, the existing source code).
   - It does **not** see the main conversation.
   The reviewer's prompt asks for critique along these dimensions:
   - **Correctness.** Does the plan implement the actual semantics? Are edge cases handled? Are types, signatures, and invariants consistent across the plan?
   - **Simplicity.** Could anything be cut, merged, or deferred without losing the goal?
   - **Performance considerations.** Are there algorithmic or data-layout choices that should be flagged now versus left for later optimization?
   - **Best practices.** Idiomatic Rust, sensible test layout, conventional naming, error-handling style, etc.
   - **Adherence to project decisions.** ADRs in `docs/decisions/`, commitments in `docs/architecture.md`, workflow rules in this file. The reviewer must catch plans that drift from settled decisions.
   - **Parallelization soundness.** Does the parallelization map identify genuinely independent subtasks? Are dependencies between them honest? Or is parallelism overstated?
3. **Reviewer returns a structured critique** plus an explicit verdict. The reviewer's prompt asks for output in a fixed format so the main agent can post it directly:
   - Each concern listed as: `[severity] Title — one-paragraph description.` Severity is one of `must-fix`, `should-fix`, or `nit`.
   - Final verdict: either `no further substantive issues` (loop terminates) or `revisions required`.
4. **Main agent surfaces the critique in chat — every concern, with its substance.** The user does not read code; the reviewer's findings are part of the design conversation he sees. Posting just counts (e.g. "5 should-fix items, 8 nits") is **not** sufficient — the user needs to see what each concern actually *is*, with enough detail to push back if the reviewer is wrong. For `must-fix` and `should-fix`, post the full text; for `nit`s, a compact one-line-per-nit list is fine. The user can override any concern before the main agent acts on it (e.g. "ignore concern 4, the reviewer is wrong about X").
5. **Main agent incorporates the feedback** (with any user overrides from step 4), revises the plan in place, and **continues the same reviewer subagent** (via `SendMessage`) for the next pass — context stays cached (faster) and the reviewer's judgment stays consistent from pass to pass (more stable than spawning a fresh reviewer each iteration, which restarts calibration). **Prerequisite:** `SendMessage` is part of Claude Code's experimental Agent Teams and requires `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1` in the env block of `.claude/settings.json`. Already set in this project; if a future session reports that `SendMessage` is unavailable, that's the first thing to check.
6. **Loop continues** until the reviewer returns "no further substantive issues" — OR after the **3rd reviewer pass** when no `must-fix` items remain. Beyond pass 3, residual `should-fix`/`nit` items are surfaced in chat and the loop terminates; final review and the user reading along are the next safety nets. The pass cap exists because each pass re-tokenizes the plan (and reviewer cache); diminishing returns set in fast once architectural concerns are resolved. The cap applies to all three review loops (plan, test-suite, final).
7. **Execute.** No user-approval gate by default — the reviewer's convergence is the gate. The agent proceeds directly to steps 5–11 of the per-feature loop above (tests → test-suite review → implement → final review → benchmark → commit). The user can override mid-loop at any point by sending a message ("wait, change X"); absent intervention, the agent runs through to commit.

**Loop convergence is the reviewer's call, not the main agent's.** The main agent does not declare a plan done.

The reviewer is the primary architectural review channel. The user reads the chat — synchronously or asynchronously — to see the reviewer's findings and the agent's responses. The blind-review loop's standing instruction is to be adversarial; a plan that the reviewer has signed off on has been pressure-tested against project decisions and best practices.

## Test-suite review loop

After tests are written for the unit (step 6 of the per-feature loop) and **before** any implementation begins, the test suite goes through its own blind-review loop. Same mechanics as the plan-review loop above (fresh subagent, blind to main-conversation context, `SendMessage` continuation, **chat-surfacing of every reviewer concern at each pass**, reviewer-determined convergence).

The reviewer reads the test files plus enough project context to evaluate whether the tests adequately exercise the contract — the plan, the relevant ADRs, the spec being tested (e.g. `docs/reference/rules/` for chess rule semantics, `docs/reference/pgn-spec-1994.txt` for FEN/PGN, the UCI spec for protocol).

The dimensions of test-suite review:

- **Correctness to spec.** Do the tests actually check what the spec says they should? Are the assertions correct, or do they bake in a misreading of the spec?
- **Confirmation bias.** Are the tests suspiciously easy to pass? Do they assume the implementation, or assert behavior independently? Would the test pass against a stub that returns the expected value without doing real work?
- **Adequate checks.** Are there enough assertions per test? Or are tests perfunctory ("call the function, check it doesn't panic") versus genuinely verifying behavior?
- **Corner case coverage.** Are edge cases tested? Empty inputs, max sizes, boundary conditions, failure paths, malformed inputs, ambiguous-spec cases?

Tests pass review **before** any implementation work begins. On reviewer convergence, the agent proceeds directly to implementation — no user-approval gate. Implementation written against unreviewed tests is hard to course-correct: by the time you discover the tests were inadequate, you've shaped the code around them.

## Final review loop

After implementation is complete, **all tests pass** (step 8 of the per-feature loop), AND the orchestrator has run the mechanical pre-review checks (step 9, see "Pre-review mechanical checks" below), the entire task scope (code + tests jointly) goes through a final blind-review loop (step 10). Same mechanics as the others (fresh subagent, blind to main-conversation context, `SendMessage` continuation, **chat-surfacing of every reviewer concern at each pass**, reviewer-determined convergence).

**Review is reading + judgment, not execution.** Reviewers do not run `cargo build`, `cargo test`, `cargo clippy`, `cargo fmt`, `cargo llvm-cov`, `cargo mutants`, or any other mechanical check. Those are orchestrator-side work. The reviewer reads the artifact (code + tests + plan + the orchestrator's pre-review analysis) and applies judgment. Re-running tests duplicates effort, blocks the iteration cycle on long-running mechanism, and adds no signal — `cargo mutants` produces identical output regardless of who invokes it.

The reviewer reads the new/modified code, the tests, the plan that authorized the work, the orchestrator's pre-review analysis (build/test/clippy/fmt status, coverage report, mutation-survivor list + per-survivor classification), and project context.

The dimensions of final review:

- **Correctness.** Does the code actually do what the tests claim it does? Are there situations the tests don't cover where the code would behave incorrectly?
- **Corner cases.** Same dimension as test-suite review, now on the implementation side: are there situations not covered by tests that the code should handle? (If yes, write more tests, then re-implement to satisfy them.)
- **Coverage gap analysis.** Read the coverage report from the orchestrator's pre-review run. For each newly-introduced code path with uncovered lines or branches, judge whether the gap is acceptable: a missing-assertion gap (write more tests), a genuinely-unreachable branch (refactor via `unreachable!()`), or a documented intentional gap (e.g. a `panic!()` on impossible state). No hard percentage threshold — judgment-based.
- **Mutation-survivor analysis.** Read the cargo-mutants survivor list + analysis from the orchestrator's pre-review run. For each surviving mutant: validate the orchestrator's classification (caught / equivalent-with-rationale / real-gap-with-deferred-detection-plan). The reviewer's role is *judging the analysis*, not running the tool. If a survivor's exclusion rationale doesn't hold up, push back; otherwise accept.
- **Code quality.** Idiomatic Rust, error-handling style, no dead code, no premature abstractions, no commented-out blocks. **Provably-unreachable branches use `unreachable!("brief why")`** rather than silent defensive returns. The panic message documents the invariant the code relies on; if a future change breaks the invariant, the program fails loudly instead of returning a misleading error or silently miscomputing. Reserve plain `if`/`return Err(...)` for paths that *can* fire on bad input — these are validation, not defense. **Defensive checks** — invariants the contract guarantees, where re-asserting in release would just slow the hot path — use `unreachable!()` or `debug_assert!`/`debug_assert_eq!` and **compile in debug builds only** (gated by `cfg(debug_assertions)` or via the `debug_assert*!` macros). Release trusts the invariant. Push the validation up to the boundary that creates the invariant (e.g. extend the FEN parser's `validate_post_parse`) rather than re-checking on every consumer call.
- **Readability.** Clear naming, sensible structure, comments only where the *why* is non-obvious. A future reader (including a future Claude session) should be able to follow the code without consulting the conversation that produced it.
- **Simplicity.** Anything overengineered? Anything that could be cut, merged, or deferred without loss?
- **Performance considerations.** Any obvious algorithmic, data-layout, or hot-path inefficiencies that should be flagged now? (Concrete optimization happens at the benchmark step that follows; review just flags candidates.)

The final review's purpose is to catch what the plan didn't anticipate and the tests didn't cover, and to validate the orchestrator's analysis of the mechanical-check output. On reviewer convergence, the agent proceeds directly to benchmarking and commit — no user-approval gate. The user can interject at any point during the loop by sending a message; absent intervention, the work commits when the loop terminates.

### Pre-review mechanical checks

Before invoking the final-reviewer, the orchestrator runs the mechanical checks itself and surfaces the results in the reviewer's spawn prompt. This division of labor exists because:

- Mechanical checks are mechanical — `cargo build`, `cargo test`, `cargo clippy`, `cargo fmt --check`, `cargo llvm-cov`, `cargo mutants` produce identical output regardless of who runs them. There's no judgment to be added by re-running.
- Mechanical-check runtimes can be substantial (mutation testing in particular: hours on the M3+ surface). Bundling them into the reviewer's iteration cycle multiplies the cycle wallclock by N passes; the reviewer's actual value-add (correctness, code-quality, simplicity reasoning) is independent of the mechanical output.
- Survivor triage iterates fastest with manual mutation + targeted tests (per "Triaging a single survivor" in "Mutation-testing scope" below). The orchestrator handles this loop directly, surfaces the residual analysis to the reviewer, and the reviewer judges whether the analysis is sound.

The pre-review mechanical-check sequence:

```sh
cargo fmt --check                                                         # zero tolerance
cargo clippy --all-targets -- -D warnings                                 # zero tolerance
cargo test --release                                                       # all green; surface failures before review
cargo llvm-cov --summary-only --lib --release                              # surface report to reviewer
git add -N $(git ls-files --others --exclude-standard 'src/**/*.rs')      # see "Mutation-testing scope" — load-bearing for new files
git diff HEAD -- 'src/**/*.rs' 'tests/**/*.rs' > /tmp/<unit>.diff
cargo mutants --in-diff /tmp/<unit>.diff                                   # surface survivor list + per-survivor classification to reviewer
```

For each surviving mutant, the orchestrator triages BEFORE invoking the reviewer:

1. **Caught (no action needed)** — re-run `cargo test --lib <test_name>` to confirm.
2. **Equivalent mutant** — prove indistinguishability, add `exclude_re` rule to `.cargo/mutants.toml` with a comment explaining why no input distinguishes original from mutated form.
3. **Real-bug, structurally undetectable at this milestone scope** — document with `exclude_re` rule and an explicit "deferred to M<X>" detection plan; surface in the milestone retrospective (`docs/milestones/<unit>.md`) for the milestone where it should be re-validated.
4. **Real-bug, catchable by adding a test** — add the test, re-run, return to step 1.

The reviewer then verifies the orchestrator's classifications are sound, checks the residual analysis aligns with the test surface, and judges whether the deferred-detection plan is reasonable. The reviewer does NOT re-run cargo-mutants.

(One downstream simplification: the cost-quality calibration of having the reviewer be Opus is also less load-bearing now, since Opus's value-add was largely in the judgment dimensions, not in running tools. The model-tier choice for final-reviewer should be re-evaluated next time the role's calibration is checked.)

## Running a match

Tournament smoke runs are driven by `scripts/match.sh`. As of ELOH.E (ADR-0022), the `self-play` and `vs-stockfish` arms invoke the in-process `elo-iterate` harness; only `compliance` still uses fastchess.

- `scripts/match.sh self-play` — 2-game self-play (Random_Seed=1 vs Random_Seed=2 via `--engine-option`/`--opponent-option`); artifacts in `target/matches/smoke/m2-self-play-<TS>/{summary.txt,match.pgn,games/}`.
- `scripts/match.sh vs-stockfish` — 2-game match against Stockfish 18 capped at `UCI_Elo=1320`; artifacts in `target/matches/smoke/m2-vs-stockfish-<TS>/`.
- `scripts/match.sh compliance` — fastchess `--compliance` UCI check; all 40 steps must pass on fastchess 1.8.0-alpha ("Engine passed all compliance checks.").

Artifacts:

- Per-run output dir under `target/matches/smoke/` (gitignored): `summary.txt`, `match.pgn`, `games/<N>.pgn`.
- Milestone summary → `bench/m2.md` (M2) or `bench/sprt/<dated>.md` (M3+ SPRT).

First-time install: `scripts/install-fastchess.sh` (idempotent; downloads and SHA256-verifies the pinned binary into `vendor/fastchess/`). Required for the `compliance` arm only — the other arms use the in-process harness binary.

See ADR-0012 (amended) and ADR-0022 for layout details, adjudication parameters, and the fresh-clone bootstrap sequence.

## Online Elo iteration

Rating estimation against Stockfish is driven by `cargo run --release --bin elo-iterate -- ...` (the ELOH.B in-process harness). Two entry points:

- **`scripts/elo-iterate.sh [initial] [batches] [games-per-batch]`** — thin wrapper; defaults `initial=2171, batches=30, games-per-batch=4`. Translates to `--max-games BATCHES*GAMES_PER_BATCH --concurrency GAMES_PER_BATCH/2 --initial-elo INITIAL --k0 40 --tau 10 --target-sigma 30 --stop-window 30 --stop-window-confirm 5`. Requires Stockfish on PATH (`brew install stockfish`).
- **`scripts/sprt.sh rating-estimate`** — fixed-anchor measurement (frozen-K + disabled-σ via `--k0 0 --target-sigma 0`). Semantically equivalent to the prior fastchess match at fixed `UCI_Elo`.

Output lives under `target/elo-iterate/run-<TS>/`:
- `games/<N>.pgn` — per-game PGN with Seven Tag Roster + per-move score comments.
- `summary.txt` — one line per game, plus `progress:` lines per completed batch and a final `converged:` line.

K-update cadence shifted from per-batch (the old bash version) to per-game (ELOH.B spec). Total game count and bash CLI surface are preserved.

### Mixed-TC SPRT (ELOH.D)

For mechanisms whose Δ Elo amplifies with depth (history, aspiration, depth-conditional pruning), a fast-TC SPRT can be inconclusive even when the slow-TC version is load-bearing positive (M4.C exhibited this — fast-TC `tc=10+0.1` SPRT inconclusive at +3.47 ± 5.88; slow-TC `tc=60+0.6` decisive at +35.74 ± 27.51). The **mixed-TC SPRT** redefines the game as "draw TC from a distribution D, then play standard chess at that TC." Game outcomes remain i.i.d. under the redefined game, so SPRT applies; the same data also supports a per-(TC, W/L/D) regression for the Δ(TC) curve.

Drive via `--tc-sample <SPEC>` instead of `--tc <TC>`:

```sh
cargo run --release --bin elo-iterate -- \
  --engine target/release/clawfish \
  --opponent target/release/baseline-clawfish \
  --tc-sample 10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1 \
  --max-games 400 \
  --initial-elo 2114 --k0 0 --target-sigma 0 \
  --seed 0xC1AB_F15A_E10D_D000 \
  --concurrency 4 \
  --engine-launch-prefix 'taskpolicy -c utility' \
  --opponent-launch-prefix 'taskpolicy -c utility' \
  --virtual-clock
```

`--tc-sample <SPEC>` and `--tc <TC>` are mutually exclusive at parse time. `--seed N` accepts decimal or `0x`-prefixed hex; default seed is constant so runs without `--seed` are still bit-deterministic for the TC-sampling stream. (Cross-run determinism still depends on subprocess scheduling under N>1 concurrency for the K-update path; the `--seed`-driven determinism is for the per-pair TC sequence only.)

Per-game `summary.txt` lines gain a `tc=<base>+<inc>` field; a `summary-by-tc:` aggregate (input-spec order) is appended after the `converged:` line. PGNs carry the sampled TC in their `[TimeControl ...]` tag.

Decision-rule support is downstream: the harness emits per-game `(TC, W/L/D)` data; mixed-game SPRT verdict computation, Δ(TC) regression fit, and confidence-band visualisation live outside the harness. M4.D's mixed-TC width-tune campaign is the first consumer.

## Static analysis and dependency hygiene

Standing checks that complement the review loops. The review loops catch reasoning errors; these catch mechanical drift.

### Per-unit (pre-review step)

- **`cargo mutants`** — orchestrator runs at the pre-review mechanical-checks step (loop step 9), surfaces survivor list + classification in the reviewer's spawn prompt. Reviewer judges the classification, doesn't re-run. See "Pre-review mechanical checks" under "Final review loop" above for the orchestrator's triage protocol; "Mutation-testing scope" below for the `--in-diff` invocation details.

### Fuzzing

Coverage-guided fuzzing of the project's string→AST parsers (FEN and UCI). Per ADR-0013.

- **Targets.** `fuzz_fen` (`Position::from_fen`) and `fuzz_uci` (`parse_uci_line`) under `fuzz/fuzz_targets/`. Both use `Arbitrary<String>` harnesses.
- **Cadence.** Re-run on parser change (`src/fen.rs`, `src/uci.rs`, or anything they call) → ≥30-min run on the affected target. Per major milestone → 2-hour run on each target as a backstop. **Not** in pre-commit.
- **First-time install.** `cargo install cargo-fuzz`. Nightly toolchain auto-selected inside `fuzz/` via `fuzz/rust-toolchain.toml` — no `+nightly` typing required from inside the directory.
- **Run.** `cd fuzz && cargo fuzz run --sanitizer none <target> -- -max_len=200 -max_total_time=<seconds>`. The `--sanitizer none` is correct for the current targets (no fuzzed path reaches `unsafe`); revisit per ADR-0013 §5 if that changes.
- **Smoke (~10 seconds).** `cd fuzz && cargo fuzz run --sanitizer none <target> -- -runs=1000 -max_len=200`. Useful as a fast post-edit check.
- **Triage.** `cargo fuzz tmin <target> <artifact>` minimizes a crash. Embed minimized literal as a `#[test]` in `src/fen.rs::tests` or `src/uci.rs::tests`; fix parser; re-run target until clean.
- **Seed maintenance.** Adding a fixture to `src/uci.rs::tests` does *not* auto-flow into the seed corpus. To add a seed, write a new file under `fuzz/corpus/<target>/`. To remove one, `git rm`. No extraction tooling.
- **Policy isolation.** The fuzz workspace is independent (`[workspace] members = ["."]` inside `fuzz/Cargo.toml`); `libfuzzer-sys` and its transitives are not in the root `Cargo.lock`. Root `cargo audit` and `cargo deny check` therefore do not see fuzz deps. Run `cd fuzz && cargo audit` separately to check fuzz-side advisories.
- **Campaign results.** Per-campaign summary appended to `docs/research/tooling-fuzzing-results.md`. **Not** in `bench/` — ADR-0010 reserves that for performance baselines.

## Mutation-testing scope

Mutation testing runtime grows roughly linearly with codebase size. A full-repo run is acceptable while the project is small, but doesn't scale — by the time the search layer lands, a full pass would take an hour-plus. Two scoping mechanisms keep the per-unit cost bounded; combine them as needed.

### Default per-unit run: `--in-diff`

`cargo mutants --in-diff <FILE>` accepts a unified diff and only generates mutants on lines the diff touches. At the pre-review step, the unit's work is in the working tree (uncommitted — the commit lands at step 12), so the invocation is:

```sh
git add -N $(git ls-files --others --exclude-standard 'src/**/*.rs')   # see note
git diff HEAD -- 'src/**/*.rs' > /tmp/<unit>.diff
cargo mutants --in-diff /tmp/<unit>.diff
```

**`git add -N` is load-bearing for new files.** `git diff HEAD` shows nothing for *untracked* files (only files git already knows about). When a unit introduces new `.rs` files (every `M1.X` so far has done this), the diff would silently exclude them and `cargo mutants --in-diff` would generate **zero mutants for the new code** — a false-positive clean run. `git add -N` ("intent to add") tells git to track the file's existence without staging contents, so subsequent `git diff` includes the new file in full. The `add -N`-d files can be `git restore --staged` afterward if needed; they're not actually staged.

The runtime is proportional to the *new* surface area, not the cumulative codebase. Lines that haven't changed since the previous unit are not mutated.

This is the default cargo-mutants invocation at the final-review step.

When in-flight work from a parallel agent is sitting in the same working tree, scope the diff to the unit's files explicitly with extra pathspec args, e.g. `git diff HEAD -- src/zobrist.rs src/position.rs > /tmp/m1.d.diff`, so unrelated unstaged work doesn't broaden the mutant set. Combine with `git add -N` on any new files in the unit's scope.

### Iterating: `--iterate`

When working through survivors and re-running cargo-mutants, `--iterate` reads the previous run's `mutants.out/` and skips mutants that were already caught. Combine with `--in-diff` so that fixing a survivor re-tests only the unaddressed mutants in the unit's surface area:

```sh
cargo mutants --in-diff /tmp/<unit>.diff --iterate
```

### Periodic full-suite backstop

The `--in-diff` runs do not exercise mutants on lines that haven't changed since the previous unit. Test additions in the unit might reveal mutants in pre-existing untouched code that the previous run missed; conversely, a refactor elsewhere could move a previously-caught line out of test coverage. To catch this drift, run the **full suite** (`cargo mutants` with no `--in-diff`) once per major milestone (M1, M2, M3, …) — at the milestone's final unit's commit, after that unit's per-unit pass converges. Frequency tradeoff: more often means more confidence and slower commits; the milestone cadence is the balance for now and can be revisited if drift becomes a recurring source of late-discovered survivors.

**Don't kick this off autonomously.** Full-suite runs take hours (~3+ h on the dev machine), tie up the working directory's build cache, and burn interactive cycles. The agent surfaces the next-best moment to run it (typically off-hours, before sleep, so it completes overnight) and waits for explicit user go-ahead. Per-unit `--in-diff` runs (minutes) remain part of the autonomous flow.

### File-glob fallback: `--file`

`-f <GLOB>` limits to a fixed file set, ignoring git history. Useful when the relevant scope is known but a diff isn't (e.g. mutation-testing a single module while debugging it). Not part of the standard per-unit workflow, but available.

### Tactical guidance: see `.cargo/mutants.toml`

Cargo-mutants-specific tips — survivor triage technique (manual mutation + targeted test instead of re-running the suite), `exclude_re` anti-patterns (line-number anchors are fragile; prefer structural refactor) — live as comments in `.cargo/mutants.toml` itself, where they're seen when editing exclusion rules. This file scopes the workflow-level pieces (when to run, where in the loop, full-suite cadence); the config-file scopes the in-the-weeds tactics.

### Continuously enforced (pre-commit hook)

A Claude Code `PreToolUse` hook (`.claude/hooks/pre-commit-check.sh`, wired in `.claude/settings.json`) intercepts `git commit` invocations and runs:

- **`rustfmt --check`** against the **staged `.rs` files only** (not the whole crate). This way, parallel agents' in-flight unstaged or untracked work doesn't block a clean commit in another scope.
- **`cargo clippy --all-targets -- -D warnings`** and **`cargo test`** on the whole crate — but **only if the working tree exactly matches the staged set** (no unstaged tracked changes, no untracked files) **and** `cargo check` confirms it compiles. If a parallel agent has WIP in the tree or the build is mid-edit, clippy + tests are skipped with a note rather than blocking. Solo work stages everything before committing, so this naturally runs the full check.

Failure of any check that does run blocks the commit and surfaces the diagnostic to the agent. The hook is a safety net, not a replacement for running these inside the per-feature loop.

If the hook ever blocks unexpectedly on something *in your scope*, fix the underlying issue rather than bypassing. Never use `--no-verify`.

### Dependency-change checks

Whenever `Cargo.toml` or `Cargo.lock` changes (a dep is added, updated, or removed), run:

- **`cargo audit`** — checks `Cargo.lock` against the [RustSec advisory DB](https://rustsec.org/) for known-vulnerable crates. Surface any advisory in chat before continuing.
- **`cargo deny check`** — enforces `deny.toml` policy: license allowlist, source restriction (crates.io only), advisory denial, duplicate-version warnings, wildcard-version ban.

Project policy (encoded in `deny.toml`):

- License allowlist limited to common permissive licenses (MIT, Apache-2.0, BSD-2/3-Clause, ISC, Zlib, CC0-1.0, Unicode-3.0). Anything else needs an explicit decision.
- Sources: crates.io only. No git deps, no alternative registries.
- Wildcard versions (`"*"`) denied — pin or range.
- Multiple versions of the same crate: warn (informational; transitives sometimes force this).
- Yanked crates: deny.

These tools are **not** in the pre-commit hook because they are slower and only relevant on dep changes. They are agent discipline plus a periodic refresh as new advisories land in the DB.

## Source-code reading restriction

We do not read the source code of existing chess engines, even for prior-art research, even when wiki articles link to specific lines. See `docs/decisions/0003-no-third-party-source-code-reading.md`.

**In bounds:** Chess Programming Wiki articles, academic papers (including their pseudocode and illustrative code fragments), blog posts, forum discussions, README files describing techniques at a high level.

**Out of bounds:** browsing the `src/` of any chess engine repo (Stockfish, Fairy-Stockfish, Leela, any open-source Rust engine, etc.) — even via raw GitHub URLs, even via search snippets that quote engine source.

**Cost to acknowledge:** prose is sometimes ambiguous where the reference implementation would resolve in seconds. We accept slower research in exchange for genuine first-principles understanding. When prose is ambiguous, work it out from first principles or ask the user — don't fall back to engine code.

## TDD scope

A chess engine is overwhelmingly deterministic, so TDD applies far more broadly than just to the rules layer. The framing that matters:

- **Function-level TDD** verifies that a specific function produces the specified output for specific inputs. Almost every component admits this.
- **Strength-level SPRT** verifies that *enabling* a feature, or tuning its parameters, gains Elo in actual play. This is a different question with a different answer.

The two are **orthogonal and both apply.** A pruning decision function has a deterministic correct answer for any input — unit test. Whether enabling that pruning gains Elo — SPRT. Same for every eval term, every ordering heuristic, every search parameter.

| Layer | Function-level TDD | Strength validation |
|---|---|---|
| Bitboard primitives | yes — pure functions, exact outputs | n/a |
| Move generation | per-piece-type unit tests + perft to depth 6–7 (fixtures from Stockfish — `decisions/0006`) | n/a |
| Make/unmake | yes — round-trip: `unmake(make(p, m)) == p` | n/a |
| Zobrist hashing | yes — round-trip + incrementality properties | n/a |
| Eval terms (each) | yes — constructed positions with expected term values | SPRT on whether the term improves play |
| Eval composition | sanity tests (symmetry, color invariance, range bounds) | SPRT for tuning weights |
| Search invariants | property tests via `proptest` (PV stability under re-search; fail-soft consistency) | SPRT |
| Search heuristics (LMR, NMP, futility, …) | yes — the decision function is unit-tested with constructed inputs | SPRT for *enabling* + parameter tuning |
| TT operations (probe, store, replacement) | yes | n/a |
| Move ordering | yes — given a move list and history/killer state, ordering is deterministic | SPRT on whether ordering changes help |
| UCI protocol | yes — input/output strings | n/a |
| NNUE inference | yes — reference output vectors with fixed weights | SPRT vs. classical eval |
| Time management | mocked-clock unit tests for the allocation function | empirical only |
| Skill dial / strength reduction | yes — knob value → effect (depth cap, noise distribution, etc.) | empirical Elo calibration |

### Genuinely non-deterministic (or hard-to-isolate) elements

- **Time management** against a real wall clock. Mockable for tests by injecting a clock.
- **Multi-threaded search.** Race-induced ordering means single-thread reproducibility breaks; tests at the function level still apply, but full search reproducibility doesn't.
- **Match outcomes / strength claims themselves.** These are inherently statistical — SPRT, not unit tests.

NNUE *training* is non-deterministic; NNUE *inference* is deterministic and unit-testable.

## Property tests vs. unit tests

Property tests (via `proptest`) and ordinary unit tests are **complementary**, not interchangeable.

- **Property tests are part of the per-feature test suite from the start.** When a unit's invariants admit a more compact property than an enumerated unit test (set algebra, round-trips, idempotence, monotonicity, etc.), the property is written *together with* the ordinary tests in step 5 of the per-feature loop and reviewed *together with* them in step 6's test-suite review. Properties are not a follow-up exercise after the code ships.
- **Property tests do not replace specific unit tests.** Two reasons:
  1. **Sampling, not enumeration.** Proptest samples (default 256 cases). A property whose strategy spans the same domain as a unit test guarantees the value only probabilistically; the unit test guarantees it deterministically.
  2. **Anchor tests document intent.** Tests added to kill specific cargo-mutants survivors, or to pin a past regression, encode information beyond the assertion — *which* mutant they kill, *which* bug they prevent. A property that subsumes the assertion does not subsume the documentation. **Do not delete anchor tests** when a broader property covers the same assertion. The same applies in reverse: a property exercising a previously-excluded equivalent mutant is good news but does not justify removing the exclusion's `# explanation` comment in `.cargo/mutants.toml`.
- **Use a property where the invariant is more compact than the enumeration.** Use a unit test where you want to anchor a specific input/output pair, document a mutant kill, or pin a regression.

## SPRT

Sequential Probability Ratio Test. The standard for accepting/rejecting engine changes once games are playable (M3+).

- Standard bounds: H0 elo0=0, H1 elo1=10 (default in `scripts/sprt.sh sprt`); tighten to elo0=0/elo1=5 for marginal changes by editing the invocation.
- Run via the in-process `elo-iterate` harness (per ADR-0022). Pentanomial-GSPRT verdict + post-hoc Δ Elo CI both land in `<out-dir>/summary.txt`.
- Fast time controls (e.g. `tc=10+0.1`) for many games — or `--tc-sample` for the mixed-TC SPRT shape (see below).
- Accept if SPRT crosses the upper Wald bound (`H1`); reject on lower (`H0`).

Worked example invocation:

```sh
scripts/sprt.sh sprt baseline/alpha-beta-tt-killer-history-aspiration
```

Output:
- `target/matches/sprt/<TS>-<baseline-slug>-sprt/summary.txt` — `converged:` line, then `sprt: verdict=H1 llr=… elo0=0.0 elo1=10.0 alpha=0.050 beta=0.050 pairs=N ptnml=[...]` and `ci: elo=… [..., ...] pairs=N`.
- `target/matches/sprt/<TS>-<baseline-slug>-sprt/match.pgn` — concatenated PGN of all games.
- `target/matches/sprt/<TS>-<baseline-slug>-sprt/games/<N>.pgn` — per-game PGNs.

### SPRT methodology — baselines from historical commits, not feature flags

The reference for any SPRT match is **a binary built from a prior git commit**, not a feature-flagged version of the current code. Build both binaries (current branch HEAD + the baseline commit's HEAD), pit them via the in-process harness, accept/reject on the SPRT bound.

- **Why not feature flags.** A flag-per-prior-behavior approach grows linearly with milestone count and produces a code surface where every search/eval function carries a "circa-M3" / "circa-M4" / … parametrization. The field universally rejects this pattern (Stockfish/Fishtest, Komodo, Ethereal all use historical-commit builds). The codebase always reflects the *current* engine; prior behaviors live only in git history.
- **Build flow** (wired in `scripts/sprt.sh` since M3.F):
  - `git worktree add target/sprt-baselines/<sha> <sha>` — isolated checkout so cargo's `target/` doesn't conflict.
  - `cargo build --release` in the worktree → cached binary at `target/sprt-baselines/<sha>/target/release/clawfish`.
  - `cargo build --release` in the working tree.
  - In-process harness SPRT match between the two binaries (ELOH.E migration; previously fastchess).
- **Caching.** Built baselines are keyed by SHA; re-runs against the same baseline are free.
- **Refactors don't need SPRT.** Functional equivalence is verified by `cargo test`; the deterministic `bench` UCI command (lands at M3.F) catches "did the search behavior accidentally change" in seconds. Reserve SPRT for changes that intend a strength delta or that touch search/eval where neutrality isn't trivially testable.

### Compliance arm stays on fastchess

`scripts/match.sh compliance` is the only flow that still calls fastchess. The fastchess `--compliance` UCI shake-out has no in-house substitute, so `scripts/install-fastchess.sh` remains the canonical bootstrap step for that one consumer. The in-tree `tests/uci_integration.rs` battery is the engine-side complement; it pins specific UCI invariants but doesn't substitute for the 40-step compliance walk.

### Baseline tag naming convention

SPRT baselines are referenced by **annotated git tags**, not bare SHAs. A bare SHA is forgettable and offers no signal in an SPRT log; an annotated tag is self-documenting.

- **Format**: `baseline/<descriptive-slug>`.
- **Slug**: lowercase, hyphen-separated, describes the engine's *behavior* at the tagged commit — not the milestone the commit landed in. The behavior name is what makes the tag legible years later in an SPRT log; milestone numbers require cross-referencing the roadmap.
- **Annotated, not lightweight** (`git tag -a`). The annotation explains what the tag marks: which behavior, when it was last in production, and why it's a useful reference point. Pre-formatted so `git tag -ln5 baseline/random-mover` reads cleanly without consulting the roadmap.
- **Each tag is the commit when the named behavior was last in production** — i.e., the last commit on `main` before the next phase replaced it. Tagged once and never moved or deleted (immutable historical reference).
- **Pushed to `origin`** (the project is on GitHub at `feldgendler/clawfish`; tags push via `git push --tags` after creation).

Tags created so far:

| Tag | Commit | Marks |
|---|---|---|
| `baseline/random-mover` | `08b980d` (M2.E end) | Last commit shipping uniform-random move selection as production search. The reference point for the M3 exit criterion ("beats the random mover ~100%"). |
| `baseline/material-greedy` | M3.A end | Last commit shipping depth-1 best-eval (PeSTO MG material + PST) as production search. Reference point for any future SPRT match against the depth-1 greedy behavior. |
| `baseline/alpha-beta-no-tt` | M3.F end | Last commit shipping alpha-beta + qsearch + ID + time-management without TT. Reference point for M4.A's SPRT (and M4.B–D thereafter). |

Tags expected to be created at future milestone boundaries (illustrative — not commitments):

- `baseline/alpha-beta-tt` — last commit shipping the bare TT (M4.A end / M4.B start), etc.

Not every commit gets a baseline tag. The criterion: tag a commit if a future SPRT might want to cite it as a fixed reference point (typically end-of-milestone or end-of-substantial-feature). Within-milestone refactors and intermediate sub-phase commits don't get tagged — they're just steps in the history.

## Benchmarking conventions

- Each milestone produces a benchmark baseline at `bench/<milestone>.md` per `docs/decisions/0010-benchmark-baseline-format.md` (committed human-readable table; raw `target/criterion/` artifacts are per-machine and gitignored).
- Standard UCI `bench` command runs a fixed set of positions and reports nodes searched — used as a deterministic regression check across changes.
- Profile hot paths with `samply` or Instruments (macOS, Apple Silicon).
- Use `criterion` for microbenchmarks.
- Never optimize without profiling first; never optimize without a benchmark to compare.

## Model assignment

Subagents inherit the orchestrator's model unless their definition specifies otherwise. Custom agent definitions in `.claude/agents/` set the model per role. The assignment is **tiered to balance cost against the reasoning each role demands**, not flat.

| Role | Agent file | Model | Why this tier |
|---|---|---|---|
| Main orchestrator | (no file — inherited) | Opus | Holds full conversation context; coordinates all subagents. |
| Plan reviewer | `plan-reviewer.md` | Opus | Originally tiered to Sonnet; M2.C calibration found Sonnet missed two must-fix items Opus caught (Box<dyn Search> non-movability into `thread::spawn`; reader EOF / channel-disconnect dead code). Reverted to Opus per the stop-loss. |
| Test-suite reviewer | `test-suite-reviewer.md` | Sonnet | Drops cleanly on confirmation-bias and corner-case dimensions. Haiku too risky on "would this pass against a stub?" reasoning. |
| Final reviewer | `final-reviewer.md` | Sonnet | Was Opus; dropped after the M3.E parallel A/B (2026-04-29 — see calibration log) confirmed Sonnet matches Opus on must-fix coverage in the narrowed read+judgment surface. Opus caught 2 extra documentation/perf nits Sonnet missed; both verdicts were `no further substantive issues`. |
| Research subagent | `chess-researcher.md` | Sonnet | Cross-source synthesis still needs reasoning. Output is reviewed downstream by the user reading chat and by the plan reviewer. |
| Coder (default) | `chess-coder.md` | Sonnet | Plans here are prescriptive (signatures, test names, order spelled out). Transcription is largely model-independent. |
| Coder (architecturally tricky) | (override via `Agent` tool's `model: opus`) | Opus | Plan flags which subtasks need it. See "When to flag a coder slice for Opus" below. Opt-in at spawn time. |

### When to flag a coder slice for Opus

The default Sonnet coder does well on prescriptive transcription (signatures + test names + order spelled out). It is reliably weaker on judgment calls that require *generalizing across files or invariants*. Flag a slice for `model: opus` when it involves any of:

- **Tricky lifetime / trait / generics gymnastics** — the original criterion.
- **Novel invariants** that need to be *invented* during implementation, not just transcribed from the plan.
- **Parallel-precedent application** — adding a helper, field, or test alongside an existing one (M3.A's `update_static_eval_after_make` next to `update_zobrist_after_make`, or M3.A's `make_move_no_from_scratch_in_release` after the M1.E sentinel). Sonnet tends to literal-pattern-match (duplicate the existing helper byte-for-byte with a different name) rather than think through whether the *discipline* of the existing helper applies and whether one helper or two is the right factoring. The plan must spell out: "match existing helper's order-agnostic param discipline; do not duplicate the test, expand its docstring."
- **Project-discipline application across modules** — e.g. when a slice adds vendored data, the coder must remember to add a cargo-mutants exclusion mirroring `src/magic/constants.rs`. Sonnet missed this in M3.A; final-review caught it.
- **Anticipating qualitative behavior shifts that break existing tests** — M3.A made `g1f3` the unique startpos best, breaking two engine tests that asserted "different seeds → different bestmoves from startpos." A careful pre-impl pass would have updated the test surface; Sonnet caught it during impl after the test failures, which is fine but late. Plan should pre-emptively flag tests that depend on the prior phase's non-determinism.

When the plan flags a slice with one of these markers, spawn the coder with `model: opus`. When in doubt, flag — the cost difference is bounded and final-review converges faster on Opus-implemented slices.

### Calibration

The first time this assignment runs on a substantive phase, spawn the plan reviewer **in parallel on both Sonnet and Opus** for the v1 plan. Compare critiques:

- If Sonnet flagged everything Opus did (modulo wording), the drop is confirmed empirically.
- If Sonnet missed something material, you've found the cost-quality boundary; revisit that tier (Opus for plan-review, retry Sonnet for the others).

The calibration pass is one-time per role. Re-run if the workflow changes shape (e.g. new ADR-rich phase, new spec being interpreted) or if the stop-loss fires.

**Calibration log:**

- **2026-04-27 — plan-reviewer (M2.C v1 plan).** Sonnet + Opus reviewed in parallel. Sonnet returned 3 must-fix / 6 should-fix; Opus returned 4 must-fix / 8 should-fix. Two of Opus's must-fix items were absent from Sonnet's critique: (a) `Box<dyn Search>` is not `Clone` and cannot be moved into `thread::spawn`, which would have stalled Coder-B at impl time; (b) reader-loop EOF synthesis vs. orchestrator channel-disconnect handling created an unreachable defensive branch (mutation-test survivor). Outcome: plan-reviewer reverted from Sonnet to Opus.

- **2026-04-28 — M2.E in-anger data points across all four reviewer/coder tiers** (no parallel A/B; observational from a real run):
    - **plan-reviewer (Opus, 3 passes).** Pass 1 caught 3 must-fix items, the most consequential being an empirical `--compliance` Step 12 failure the orchestrator missed: the reviewer ran `vendor/fastchess/fastchess --compliance target/release/clawfish` themselves and found `info`-line emission was missing, forcing a `src/search.rs` change into M2.E's scope mid-review. Also caught: S1–S4 of the proposed in-tree tests duplicated existing E33–E35; `BufReader::lines().flatten()` antipattern. Plus 8 should-fix items including substantive empirical findings (deterministic seeds → duplicate trajectories; `-draw` triggering trivially against a 0-cp default). The empirical-probe behavior is precisely what the Opus tier is paying for.
    - **test-suite-reviewer (Sonnet, 2 passes) — first calibration data point.** Pass 1 caught a should-fix (E37 silence contract not fully pinned: `assert_eq!(lines, vec!["readyok"])` is strictly tighter than separate `any`/`!any` assertions) plus 2 nits (E33 prefix `info ` → `info depth `; temporal-ordering deferral). Confirmation-bias / corner-case reasoning was sharp ("could E37 false-pass against a stub that always emits readyok? against a hypothetical regression that emits info string only with debug=on?"). Sonnet at this tier validated; remove from watchlist.
    - **final-reviewer (Opus, 2 passes).** Pass 1 caught a `cargo fmt --check` violation (load-bearing — pre-commit hook would block) and a should-fix `ulimit -n` runbook gap (would have bitten the next operator). Ran `cargo llvm-cov` and `cargo mutants --in-diff` cleanly. Mutation result: 1 generated, 1 caught.
    - **chess-coder (Sonnet, 2 slices).** Phase 1+2 (src + tests) and Phase 4 (scripts + ADR + runbook) shipped correctly; Phase 4 coder also caught the 12-vs-40 fastchess-compliance-step discrepancy in their report (real observational reasoning). Single workflow gap: neither slice ran `cargo fmt`, leaving final-review to catch it. Closed by adding `cargo fmt` to `chess-coder.md`'s verification checklist.
    - **Outcome:** No tier changes. test-suite-reviewer removed from watchlist; chess-coder verification step extended.

- **2026-04-28 — chess-researcher (M3 search-basics brief, parallel A/B).** Sonnet + Opus spawned on the same brief covering negamax framing / mate scoring / PV recovery / quiescence / move ordering / iterative deepening / cancellation / repetition / draw detection / pitfalls / performance budget. Reports produced at `docs/research/m3-search-basics.md` (Sonnet) and `docs/research/m3-search-basics.opus.md` (Opus).
    - **Substantive convergence on every headline call**: fail-soft negamax; triangular PV table; defer PVS / aspiration windows to M4; qsearch = stand-pat + captures + queen promos + in-check evasions (no checks, no delta pruning at M3); MVV-LVA + movegen-order quiets; ID aborts between iterations only; ~2k–4k node cancellation cadence; repetition via game-history `Vec<u64>` plumbed through `SearchContext`; insufficient-material in eval.
    - **Differences are wording-crispness or minor-parameter only**: score type `i32` (Sonnet) vs `i16` (Opus, citing M9 NNUE SIMD); MATE constant 30000 vs 32000; mate-distance pruning M3 (Sonnet) vs M4 (Opus); performance estimate 5–20 Mnps (Sonnet) vs 0.5–2 Mnps (Opus) — empirical resolution deferred to M3.C bench. Both reports surfaced the PV-update-under-cancellation pitfall (Opus framed it as a named bug taxonomy; Sonnet folded it into the "Common pitfalls" list). No must-fix gap in either direction.
    - **Outcome:** chess-researcher Sonnet tier confirmed; remove from watchlist. Both reports are kept (the Opus parallel pass is preserved as the calibration evidence and as a useful second perspective for plan-mode reference).

- **2026-04-28 — chess-coder (M3.A, observational).** Sonnet ran three slices (test-writing + test-suite-fix + impl); not a parallel A/B. Mechanical execution clean; deviations reported honestly. Final-review pass 1 caught **1 must-fix + 3 should-fix items** that were within Sonnet's nominal scope but reflect *judgment calls Sonnet did not make*:
    - Cargo-mutants exclusion gap on `src/eval/data.rs` (PST data) — Sonnet did not generalize from the existing `src/magic/constants.rs` precedent. ~150 mutants would have shipped uncaught. Required a split-file refactor to address.
    - Duplicate `make_move_no_eval_recompute_in_release` perf sentinel byte-identical to the M1.E zobrist sentinel — pattern-matched the form rather than thinking through factoring.
    - `update_static_eval_after_make` derived `us` from `pos.side_to_move().flip()` rather than the `mover.color` parameter — fragile call-ordering dependency; the parallel `update_zobrist_after_make` uses `mover.color`. Plan §7 prose called out the discipline; Sonnet didn't carry it into code.
    - Stale `TODO M3.A impl` comment on `Undo::prior_static_eval` after the field was populated.
    - **Outcome (no tier change):** all four caught by final-review (Opus); cascade worked. Workflow updated with "When to flag a coder slice for Opus" criteria covering parallel-precedent application, project-discipline application across modules, and qualitative-behavior-shift anticipation. Plans for M3.B+ should explicitly flag any slice meeting these criteria for `model: opus`. **Stop-loss not triggered** — these are should-fix items caught by review, not must-fix items shipped.

- **2026-04-29 — chess-coder (M3.D, parallel Sonnet+Opus A/B).** First clean parallel A/B for the chess-coder tier (deferred from M3.A and M3.C). Both agents spawned with `isolation: "worktree"` from the M3.C-end commit (`4d8917e`), with identical implementation prompt (plan §3 + §4 + §5 + verification checklist). Mostly converged outcome; coder-only signal partially compromised by a workflow surprise.
    - **Worktree-isolation surprise.** The orchestrator's pre-A/B temp commit (`ebd2691 WIP M3.D: test suite + plan + qsearch stubs`) on main was NOT picked up by the worktree spawn — both worktrees materialized at the parent commit (`4d8917e`). Both coders therefore wrote tests AND impl from scratch following plan §6.1 verbatim. The A/B is coder-AND-test-author, not coder-only. Useful but with reduced signal compared to a clean test-suite-shared A/B.
    - **Both produced near-identical implementations** matching plan §3 step ordering exactly. Differences:
        - Sonnet bumped `node_count_is_below_branching_bound` from 8000 to 10_000; Opus kept at 8000 (Sonnet observed 2179 nodes, mine ~30000 in the merged result).
        - Both re-pinned E39 from `b1c3` to `d2d4`.
        - Sonnet patched `integration_position_with_moves_then_go` to `go depth 2`; Opus to `go depth 3`.
        - Both wrote 18 tests; both passed all gates; both clean clippy + fmt; both produced sound code.
    - **Outcome.** Sonnet's diff applied to main per workflow.md's cheaper-model default rule. **chess-coder Sonnet tier confirmed for prescriptive-plan slices** — M3.D's plan §3 was unusually prescriptive thanks to the 3-pass plan-review iteration, and Sonnet handled it cleanly. The categorical "novel invariants" risks the calibration trigger flagged (false-stalemate trap, stand-pat-in-check, mate detection inside qsearch, move-flag filter discipline) were all correctly handled by both Sonnet and Opus; the prescriptive plan body left no room for either to drift.
    - **Future watchlist.** Confirm Sonnet handles less-prescriptive slices comparably; otherwise re-trigger calibration. The next logical trigger is when a coder slice's plan §3 body is *intentionally* less detailed (e.g. an exploratory feature where the plan can't pre-specify the impl shape) — those are the cases where pattern-matching tendency would manifest.
    - **Final-review's mutation gap closed via refactor.** The `delete - in AlphaBetaMover::qsearch` recursive-bound mutation at the open-coded `(-beta, -alpha)` survived v2's 18 M3.D tests. Manual verification (apply mutation + re-run targeted tests in seconds — the user's mid-flight nudge to skip full cargo-mutants iteration cycles) confirmed the mutation was structurally undetectable at M3.D scope at the call site: 1–2 plies of qsearch recursion in unit-test fixtures are too shallow to manifest the broken's degenerate child window's effect on returned scores. Initial proposal to document the gap via `.cargo/mutants.toml` `exclude_re` was rejected after the user pointed out that line-number-anchored regexes are fragile (any unrelated edit shifts the line, the rule silently breaks). v4 fix: extract `(-beta, -alpha)` into a module-level `negate_window(alpha, beta) -> (i32, i32)` helper. The mutation moves to the helper's body, where it's directly catchable by `negate_window_negates_both_bounds_and_swaps_them` (asymmetric + symmetric window cases, both `delete -` mutations verified to fire the assertion). Not a stop-loss trigger; the original gap was a code-shape issue, not a coder-tier issue.

- **2026-04-29 — chess-coder (M3.E, parallel Sonnet+Opus A/B, partial).** Two slices: A (Sonnet, `compute_caps` + `MoveOverhead` UCI option) and B (Opus, ID outer loop + `negamax` ply==0 reorder + `handle_go` wiring). Slice A's surface was narrow + heavily prescriptive; landed cleanly with all 54 tests passing on the slice's own surface. Slice B was flagged for Opus override at spawn time because the slice introduces three load-bearing structural changes (per-iteration vs per-go reset distinction, `last_complete` snapshot semantics, mid-iteration abort discipline) — Opus handled correctly without cascading reviewer iterations.
    - **Test-write-phase oversight surfaced post-impl.** Two pre-existing M3.D tests (`alphabeta_search_aborts_on_should_abort`, `go_with_movetime_emits_bestmove_after_deadline`) embedded M3.D-era semantics that no longer hold under M3.E (the first expected `bestmove=None` from a pre-set stop, but M3.E ID's iter-1 completes naturally; the second expected ≥40ms from `movetime 50`, but M3.E `compute_caps(movetime=50, mo=50) = (1ms, 1ms)`). The test-suite-write phase (orchestrator) failed to update them. Three integration tests (E39/E41/E42) were also affected by a `quit`-arrives-mid-search race: the M3.D test pattern of writing all commands + EOF eagerly + reading stdout works for single-pass M3.D but races with M3.E's inter-iteration `stop` check, breaking after iteration 1 instead of running to the requested depth. Orchestrator fixed all 5 tests.
    - **`aborted_fallback_result` extraction (mid-implementation refactor)**. cargo-mutants flagged 3 mutations on `(self.pv.lengths[0] > 0).then(...)` in `Search::go`'s fallback path. Same problem as M3.D's `negate_window`: the inline expression is unreachable from any chess fixture (iter 1 from any chess position visits < 4096 nodes, below the cancellation cadence). Following the M3.D playbook, extract to `aborted_fallback_result(&PvTable, Option<i32>) -> (u32, Option<Move>, i32)` named helper, add 4 direct unit tests on synthetic PvTable fixtures. Loop replaces over-filtering an `exclude_re` rule.
    - **Hard-deadline mutation `+ → -` on `engine.rs:225`** required a debug-friendly Kiwipete fixture (`go wtime 20000 btime 20000`) where iter-3 visits ~26k nodes (well over the 4096 cadence) and `compute_caps` produces (~950ms, ~2850ms) caps that fit iter-3 in both debug and release. Mutated impl aborts iter-3 on the first 4096-poll; correct impl emits the depth-3 info line. Test naming: `handle_go_with_wtime_btime_reaches_depth_3_on_kiwipete`.
    - **Outcome.** No tier change. Slice A confirmed Sonnet again on prescriptive narrow surfaces. Slice B Opus override worked as designed: implementation passed final-review without bug-fixing iterations on the search-core changes. The mutation work is the orchestrator's domain, not the coder's.

- **2026-04-29 — final-reviewer (M3.E, parallel Sonnet+Opus A/B).** Triggered the watchlist re-eval after the M3.D scope-narrowing (mechanical checks moved to orchestrator). Both reviewers spawned in parallel on the same v1 artifact (M3.E full diff: ~2400 lines across `src/search.rs`, `src/engine.rs`, `tests/uci_integration.rs`, plus 5 doc files).
    - **Sonnet:** 0 must-fix + 0 should-fix + **2 nits** + verdict `no further substantive issues`. The 2 nits were: (a) docstring drift on `alphabeta_partial_completion_under_abort_returns_partial_pv_and_score` (M3.D framing referencing "first root move's subtree" no longer matched M3.E ID semantics where the abort fires between iterations); (b) panic-message typo on `handle_go_with_wtime_btime_reaches_depth_3_on_kiwipete` (`wtime=5000` in error msg vs `wtime=20000` in actual test command — copy-paste from adjacent test).
    - **Opus:** 0 must-fix + 0 should-fix + **4 nits** + verdict `no further substantive issues`. Sonnet's 2 nits PLUS 2 extras: (c) `docs/architecture.md` Lines 141–149 mixed M3.D and M3.E semantic descriptions of `Search::go` (the earlier paragraph at line 133 superseded the M3.D-style numbered steps without removing them); (d) `bench/m3.md` iter-8 nps-drop explanation cited "qsearch's cumulative cancellation-poll cost" but per-node cost shouldn't accumulate across iterations — the doc-claim's mechanism is speculative without re-bench data.
    - **Both reviewers:** validated the orchestrator's mutation-survivor classifications cleanly. Both judged the `aborted_fallback_result` extraction as the right move per the M3.D `negate_window` precedent. Both confirmed coverage gaps (97.6% region) were acceptable. No correctness issues found by either.
    - **Outcome.** Sonnet matched Opus on must-fix coverage (both 0). Opus's 2 extra nits were doc-quality / explanatory only — neither blocked the milestone. **Drop final-reviewer to Sonnet** per the calibration protocol; model-assignment table updated. Watchlist entry retired.
    - **Cost-quality calibration data point:** Sonnet's review pass took ~4 min (40 tool uses) vs Opus's ~7 min (35 tool uses). On the narrowed read+judgment surface, Sonnet hits substantively all the same correctness/mutation/coverage dimensions; the gap is on doc-thoroughness which is downstream of the actual review value-add.

**Watchlist** (tiers without further calibration data — next time the role fires, consider running Sonnet + Opus in parallel before relying on the cheaper tier):

- **chess-coder.** M3.D parallel A/B confirmed Sonnet for prescriptive plans. Re-watch when a coder slice's plan §3 is intentionally less prescriptive.
- ~~final-reviewer (re-eval Sonnet)~~ — calibration fired at M3.E (2026-04-29). Sonnet matched Opus on must-fix coverage; Opus caught 2 extra nits (architecture.md doc-inconsistency + bench/m3.md perf-claim mechanism). Verdict: drop final-reviewer to Sonnet. Removed from watchlist.

### Stop-loss

The tiered drop is a working hypothesis, not a settled commitment. Trigger a re-evaluation if any of these fire:

- **Late-caught regression.** A defect that final review catches — or worse, ships and surfaces in a later phase — that a stronger plan- or test-reviewer would plausibly have caught earlier. "Plausibly" is judgment but not a low bar: the regression has to be the kind of issue that role's listed dimensions explicitly cover.
- **Reviewer rubber-stamping.** Reviewer outputs trend toward "no further substantive issues" on first pass when prior baselines averaged 3+. Could be the agent internalizing past feedback proactively (good) or the reviewer not probing as hard (bad). Spot-check by re-running the same artifact past Opus and comparing.
- **User-as-corrector.** If the user's interjections become a substantive correction channel rather than a sanity check, the reviewer gate is too weak.

When a trigger fires:

- Revert the implicated role to Opus by editing the agent file's `model:` field.
- Document the trigger and the missed defect in the next milestone's `roadmap.md` notes (so the rollback isn't silently forgotten).
- Re-run the calibration step before any future re-attempt at dropping that role.

## Main-agent context discipline

The main agent's context grows across a cycle (subagent return messages, plan revisions, file reads). Sessions past ~150k tokens are materially more expensive even with cache hits. Discipline:

- **Don't re-`Read` files that are already in this conversation's context.** The Read tool re-injects the entire file even when nothing has changed. Use `offset`/`limit` to fetch only the specific lines you need; or rely on the prior read if the relevant content is still in scrollback.
- **`/clear` between M-phases.** Persistent state lives in `CLAUDE.md`, `MEMORY.md`, plan files, and source. A new phase has nothing to gain from the prior phase's transcript.
- **Compact mid-cycle if context drifts past ~120k**, especially before kicking off a parallel coding-agent batch — the orchestrator's spawn payload includes context it considers relevant, so a bloated main context inflates every spawned agent.

## Communication norms

- The user does not read code. Every non-trivial behavioral choice should be summarized in chat.
- When a change crosses architectural boundaries (e.g. modifying make/unmake, changing the eval interface), call it out explicitly.
- When a benchmark shows a regression or a SPRT fails, surface the result and propose what to try next — don't silently roll forward.
- When prior-art research turns up something surprising or that contradicts a prior assumption, raise it before acting on it.

## Documentation maintenance

When a session produces new decisions, update the relevant doc *before* stopping:
- New architectural decision → write `docs/decisions/NNNN-name.md` and reference it from `docs/architecture.md`.
- New load-bearing invariant or design surface that doesn't merit a full ADR (e.g. a phase's data-structure invariant) → add a settled-commitments row + a per-feature section to `docs/architecture.md`. Roadmap/milestone entries are forward-looking or retrospective; they are not the home for invariants future engineers need to consult.
- Milestone progress or revision → update `docs/roadmap.md`.
- New ground rule from user → update `CLAUDE.md`.
- Prior-art research output → append to `docs/prior-art.md` under the relevant component section.
- **New rating estimate or bench signature** → update **`README.md`'s Status block** alongside the per-milestone `bench/m<N>.md` and the milestone retrospective. The README is the externally-facing summary; out-of-date numbers there are misleading to readers who don't drill into `bench/`. Specifically: every milestone whose retrospective publishes a new bench signature or rating estimate must atomically update `README.md`'s "Current strength" / "Current bench" lines with the new numbers, the conditions (TC, hardware, anchor), and the methodology link.

## Documentation style

Write docs as structure, not prose. Concretely:

- **Don't pack multiple distinct facts into one paragraph or one bullet.** A bullet that names a topic and then runs through five comma-separated facts and parentheticals is a list disguised as prose — break it into sub-bullets, one claim per line.
- **Prefer the cheapest structure that fits.** Table > nested bullets > short prose. Use prose only when the points are genuinely linear (narrative, derivation, an argument that builds), not when listing parallel facts.
- **One concept per section; one claim per bullet.** Findings, conventions, phase descriptions, ADR rationales — never a wall of paragraph the reader has to parse manually.
- **Citations and parentheticals are not exempt.** "(per X, because Y, see also Z)" inside a bullet is itself an infodump — split it.

Applies everywhere: `roadmap.md` phase entries and ADR stubs, written ADRs, research reports, status sections in `CLAUDE.md`/`roadmap.md`, plan documents, commit bodies.
