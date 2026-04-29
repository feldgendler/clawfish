# 0004 — NNUE hooks present from day one

**Status:** Accepted, 2026-04-27

## Context

NNUE is a planned milestone (M9 per `docs/roadmap.md`), not a maybe. NNUE is *not* a drop-in eval function — it is an incrementally updated accumulator that requires hooks in `make_move` and `unmake_move`. Retrofitting these hooks into a search that has open-coded move-application logic at every call site is painful.

The user's guidance is that NNUE-readiness should "exist or be easy to add" from the start.

## Decision

We do **not** pre-build the NNUE accumulator, the eval trait, or any abstraction now. We **do** ensure two minimal properties from day one:

1. **`make_move(&mut Position, Move) -> Undo`** and **`unmake_move(&mut Position, Move, Undo)`** exist as discrete functions. Search code calls these — it does not open-code position mutation inline. This guarantees a single interception point for future accumulator updates.
2. **`Move` and `Undo` carry enough information** to compute the NNUE delta after the fact: from-square, to-square, moving piece, captured piece (if any), promotion target, castling rook movement, en passant capture square. This is information we'd carry anyway for legality and unmake correctness; we just confirm it's all there.

The eval interface in v1 is whatever's simplest — likely a free function `evaluate(&Position) -> Score`. When NNUE arrives, this function is replaced and the make/unmake hooks gain accumulator-update calls. No structural refactor required.

## Consequences

- Cost paid now: effectively zero. Discrete `make_move` / `unmake_move` functions are simply better engineering than inline position mutation.
- Cost saved later: avoiding a search-wide refactor when NNUE arrives.
- We are *not* paying for: an `Eval` trait, an `Accumulator` type, feature-flagged NNUE code paths, or any other speculative abstraction.

## Rationale

Of all the "NNUE-readiness" tasks, only the make/unmake structure is genuinely free and genuinely painful to retrofit. Everything else (the eval interface itself, the accumulator type, the feature transformer) can be added when NNUE actually arrives. This keeps v1 simple while preserving the only invariant that's expensive to fix later.
