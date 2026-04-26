# 0003 — No reading of third-party chess engine source code

**Status:** Accepted, 2026-04-27

## Context

The user does not want me influenced by existing engine implementations. Beyond the existing rule that we write all chess-domain code from scratch, this extends the prohibition to the *research* phase: I do not read engine source code as a reference, even for ideas.

## Decision

**Out of bounds:** browsing the source repositories of any chess engine — Stockfish, Fairy-Stockfish, Leela, any open-source Rust engine, etc. Even via raw GitHub URLs, even when wiki articles link to specific lines, even via search-result snippets that quote engine source.

**In bounds:**
- Chess Programming Wiki articles (the prose, not their source links).
- Academic papers, including pseudocode and small illustrative code fragments published for exposition.
- Blog posts, tutorial articles.
- TalkChess and other forum discussions.
- README files describing techniques at a high level.

The dividing line: **prose with code as illustration is fine; engine source code as a reference is not.**

## Clarification: runtime use of third-party engines

Running a third-party chess engine as a binary and consuming its **output** is data consumption, not source reading, and is **allowed**. Specifically:

- Using Stockfish as a perft oracle (see `0006-stockfish-as-perft-oracle.md`).
- Playing matches against Stockfish or any other engine for SPRT calibration, sparring, strength estimation, or as a tournament opponent.
- Parsing PGN games produced by other engines as training/test data.
- Reading evaluations or principal variations from other engines as test fixtures.

The constraint is on reading the *implementation* (source code), not on observing the *behavior* (output, moves played, evaluations).

## Consequences

- **Slower research at the margins.** Prose is sometimes ambiguous where a glance at the reference implementation would resolve in seconds. We accept this cost.
- **When prose is ambiguous,** I work it out from first principles, or surface the ambiguity to the user. I do not fall back to reading engine source as a tiebreaker.
- **Subagents inherit this restriction.** When delegating research to a subagent, I include the constraint in the prompt.

## Rationale

The user wants the engine's design to emerge from understanding, not imitation. Reading existing engines' code is the fastest way to converge on the same design choices for the same reasons — sometimes good ones, sometimes path-dependent ones. Working from prose forces genuine reasoning about tradeoffs and reduces the risk of carrying over assumptions that don't fit our actual constraints (e.g. our single-platform, single-variant, NNUE-eventual-but-not-now profile differs from any specific public engine's).
