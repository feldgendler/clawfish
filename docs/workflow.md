# Workflow

How we work together on this project.

## The per-feature loop

Every feature or major component follows the same cycle:

1. **Deep prior-art research.** Web search, not training-data recall. Chess Programming Wiki, papers, blog posts, TalkChess threads, articles with illustrative snippets. **Not** engine source code (see restriction below). Devil is in the details. Delegate to a research subagent when it spans more than a few queries.
2. **Explain findings in chat.** Tradeoffs, alternatives, gotchas. Pre-implementation, before any code.
3. **Discuss and converge.** User pushes back, asks for alternatives, picks an approach.
4. **Write tests first** where the layer admits it (see TDD scope below).
5. **Implement.**
6. **Benchmark and profile.** Record results. Compare to previous baseline.

Skipping research-and-discussion strips the user of his only review channel. When uncertain, propose before implementing.

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
