# Workflow

How we work together on this project.

## The per-feature loop

Every feature or major component follows the same cycle:

1. **Deep prior-art research.** Web search, not training-data recall. Chess Programming Wiki, papers, blog posts, TalkChess threads, articles with illustrative snippets. **Not** engine source code (see restriction below). Devil is in the details. Delegate to a research subagent when it spans more than a few queries.
2. **Explain findings in chat.** Tradeoffs, alternatives, gotchas. Pre-implementation, before any code.
3. **Discuss and converge.** User pushes back, asks for alternatives, picks an approach.
4. **Plan with plan-review loop.** Every implementable unit gets a written plan. The plan must identify **parallelization opportunities** — which subtasks can run on parallel coding agents. The plan goes through a blind-review loop until convergent. See "Plan mode and plan-review loop" below.
5. **Write tests** for the entire task scope, where the layer admits TDD (see TDD scope below). Parallelizable across coding agents per the plan.
6. **Test-suite review loop.** Independent reviewer checks the test suite for correctness to spec, confirmation bias, adequate checks, corner case coverage. See "Test-suite review loop" below. Implementation does not begin until tests pass review.
7. **Implement.** Parallelizable across coding agents per the plan.
8. **All tests pass.** No final review or commit until they do.
9. **Final review loop on code + tests jointly.** Independent reviewer checks correctness, corner cases, code quality, readability, simplicity, performance considerations. See "Final review loop" below.
10. **Benchmark and profile.** Record results. Compare to previous baseline.

Skipping research/discussion or any review loop strips the user of his only architectural review channels. The user does not read code; chat, reviewed plans, reviewed tests, and reviewed final artifacts are how he understands what's being built. When uncertain, propose before implementing.

## Plan mode and plan-review loop

**Every implementable unit goes through plan mode.** No code is written until a plan exists, has survived the plan-review loop, and has user approval.

A "unit" is a phase of a milestone (e.g. M1.A, M1.B), or a discrete feature within a phase if the phase is large. The roadmap decomposes milestones into units sized for ~500–1500 lines of resulting code each. Larger should be sub-divided.

The plan itself, **written to a file** (`docs/plans/<unit>.md`) so the reviewer can read it, names:

- Files created/modified.
- The type definitions and function signatures the plan introduces.
- Module boundaries.
- Test coverage strategy and specific test names.
- Order of operations.
- Dependencies on other units.
- **Parallelization map.** Which subtasks (writing tests, implementing modules) can be executed concurrently by separate coding agents, or "none — sequential" if parallelism is not practical for this unit. Honest about dependencies; don't overstate parallelism.

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
3. **Reviewer returns a structured critique** plus an explicit verdict — either "no further substantive issues" (loop terminates) or a list of specific concerns with severity.
4. **Main agent incorporates the feedback**, revises the plan in place, and **continues the same reviewer subagent** (via `SendMessage`) for the next pass — context stays cached (faster) and the reviewer's judgment stays consistent from pass to pass (more stable than spawning a fresh reviewer each iteration, which restarts calibration). **Prerequisite:** `SendMessage` is part of Claude Code's experimental Agent Teams and requires `CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1` in the env block of `.claude/settings.json`. Already set in this project; if a future session reports that `SendMessage` is unavailable, that's the first thing to check.
5. **Loop continues** until the reviewer returns "no further substantive issues."
6. **User approves the converged plan.**
7. **Execute** (steps 5–10 of the per-feature loop above: tests → test-suite review → implement → final review → benchmark).

**Loop convergence is the reviewer's call, not the main agent's.** The main agent does not declare a plan done.

The user, who does not read code, gets to review the plan as the primary architectural review channel — and the blind-review loop ensures the plan that reaches the user has already been pressure-tested by an independent reader.

## Test-suite review loop

After tests are written for the unit (step 6 of the per-feature loop) and **before** any implementation begins, the test suite goes through its own blind-review loop. Same mechanics as the plan-review loop above (fresh subagent, blind to main-conversation context, `SendMessage` continuation, reviewer-determined convergence).

The reviewer reads the test files plus enough project context to evaluate whether the tests adequately exercise the contract — the plan, the relevant ADRs, the spec being tested (e.g. `docs/reference/rules/` for chess rule semantics, `docs/reference/pgn-spec-1994.txt` for FEN/PGN, the UCI spec for protocol).

The dimensions of test-suite review:

- **Correctness to spec.** Do the tests actually check what the spec says they should? Are the assertions correct, or do they bake in a misreading of the spec?
- **Confirmation bias.** Are the tests suspiciously easy to pass? Do they assume the implementation, or assert behavior independently? Would the test pass against a stub that returns the expected value without doing real work?
- **Adequate checks.** Are there enough assertions per test? Or are tests perfunctory ("call the function, check it doesn't panic") versus genuinely verifying behavior?
- **Corner case coverage.** Are edge cases tested? Empty inputs, max sizes, boundary conditions, failure paths, malformed inputs, ambiguous-spec cases?

Tests pass review **before** any implementation work begins. Implementation written against unreviewed tests is hard to course-correct — by the time you discover the tests were inadequate, you've shaped the code around them.

## Final review loop

After implementation is complete and **all tests pass** (step 9 of the per-feature loop), the entire task scope (code + tests jointly) goes through a final blind-review loop. Same mechanics as the others.

The reviewer reads the new/modified code, the tests, the plan that authorized the work, and any project context relevant to the unit.

The dimensions of final review:

- **Correctness.** Does the code actually do what the tests claim it does? Are there situations the tests don't cover where the code would behave incorrectly?
- **Corner cases.** Same dimension as test-suite review, now on the implementation side: are there situations not covered by tests that the code should handle? (If yes, write more tests, then re-implement to satisfy them.)
- **Code quality.** Idiomatic Rust, error-handling style, no dead code, no premature abstractions, no commented-out blocks.
- **Readability.** Clear naming, sensible structure, comments only where the *why* is non-obvious. A future reader (including a future Claude session) should be able to follow the code without consulting the conversation that produced it.
- **Simplicity.** Anything overengineered? Anything that could be cut, merged, or deferred without loss?
- **Performance considerations.** Any obvious algorithmic, data-layout, or hot-path inefficiencies that should be flagged now? (Concrete optimization happens at the benchmark step that follows; review just flags candidates.)

The final review's purpose is to catch what the plan didn't anticipate and the tests didn't cover. The user, who again does not read code, gets the final review's report as his last opportunity to push back before the work commits.

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
| Search invariants | property tests (PV stability under re-search; fail-soft consistency) | SPRT |
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

## SPRT

Sequential Probability Ratio Test. The standard for accepting/rejecting engine changes once games are playable (M3+).

- Standard bounds: H0 elo0=0, H1 elo1=5 (conservative); or elo0=-3, elo1=3 for marginal changes.
- Run via `cutechess-cli` or `fastchess`.
- Fast time controls (e.g. 10+0.1) for many games.
- Accept if SPRT crosses the upper bound; reject on lower.

## Benchmarking conventions

- Each milestone produces a benchmark baseline saved to `bench/` (TBD format).
- Standard UCI `bench` command runs a fixed set of positions and reports nodes searched — used as a deterministic regression check across changes.
- Profile hot paths with `samply` or Instruments (macOS, Apple Silicon).
- Use `criterion` for microbenchmarks.
- Never optimize without profiling first; never optimize without a benchmark to compare.

## Communication norms

- The user does not read code. Every non-trivial behavioral choice should be summarized in chat.
- When a change crosses architectural boundaries (e.g. modifying make/unmake, changing the eval interface), call it out explicitly.
- When a benchmark shows a regression or a SPRT fails, surface the result and propose what to try next — don't silently roll forward.
- When prior-art research turns up something surprising or that contradicts a prior assumption, raise it before acting on it.

## Documentation maintenance

When a session produces new decisions, update the relevant doc *before* stopping:
- New architectural decision → write `docs/decisions/NNNN-name.md` and reference it from `docs/architecture.md`.
- Milestone progress or revision → update `docs/roadmap.md`.
- New ground rule from user → update `CLAUDE.md`.
- Prior-art research output → append to `docs/prior-art.md` under the relevant component section.
