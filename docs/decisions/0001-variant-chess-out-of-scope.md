# 0001 — Variant chess is out of scope

**Status:** Accepted, 2026-04-27

## Context

Earlier conversation suggested variant chess (exotic pieces, different board geometry, different rules) might be a long-term goal. This would force significant architectural overhead from day one: parameterized board sizes, multi-word or trait-based bitboards, abstract move encoding, dispatch strategy for variant-specific rules, etc.

## Decision

Variant chess is **explicitly out of scope** for this project. If/when it happens, it will be a future *fork* of this codebase, drawing on whatever parts turn out to be useful.

The current project codes for standard chess only.

## Consequences

**Costs paid for variant support: zero.** The architecture is allowed to assume:
- 8×8 board.
- `u64` bitboards.
- Magic bitboards for sliding pieces (8×8-specific).
- 16-bit move encoding.
- Hardcoded piece set: pawn, knight, bishop, rook, queen, king.
- Standard chess rules: castling, en passant, promotion to QRBN, 50-move, threefold repetition.
- No `Variant` enum, no trait-object dispatch, no abstraction layer.

**Free-win exception.** If a design choice is *literally* free now and might make a future fork easier, take it. But "literally free" is the bar — no upfront cost is acceptable, even small.

## Rationale

The user's primary goal is a strong standard-chess engine that can grow toward GM level. Variant chess is a separate, future, optional project. Designing for variants now would slow standard-chess strength development, complicate every layer, and most likely produce abstractions that don't fit the actual variant requirements when they're known.
