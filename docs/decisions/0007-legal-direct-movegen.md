# 0007 — Move generation: legal-direct, mask-based, with check-evasion specialization

**Status:** Accepted, 2026-04-27 (binds at M1.F).

## Context

M1.F is the move-generation phase. The biggest architectural fork is **legal-direct** vs **pseudo-legal-and-filter**:

- **Pseudo-legal-and-filter** (CPW [Move Generation], several TalkChess threads): emit moves ignoring whether they leave own king in check, then test legality post-hoc — typically by playing the move and looking for own-king-in-check.
- **Legal-direct** (Peter Ellis Jones, Pradu Kannan, Analog Hors): up-front compute checkers, pinned pieces, capture-mask, push-mask, then emit only legal moves.

Full prior-art reasoning lives in `docs/research/m1-engine-architecture.md` §3 (researched 2026-04-27). This ADR records the *commitment*; the research is the *justification*. Note also that `docs/prior-art.md`'s "Headline calls" already names legal-direct-with-evasion-specialization, so this ADR is codification, not a fresh decision.

## Decision

**Legal-direct, mask-based, with check-evasion specialization.**

- **Up-front per-call computation** of the following bitboards each time `generate_moves` is invoked (i.e., **not cached on `Position`**):
  - `checkers` — set of opponent pieces directly attacking the side-to-move's king.
  - `pinned` — set of own pieces pinned to the king by an opponent slider.
  - `capture_mask` and `push_mask` — masks every non-king destination must intersect, derived from the check state. See §"Masks" below.
- **Per-piece-type generation routines** that filter their output through these masks. Pinned pieces only emit moves along the king-pinner ray. Knights cannot move at all when pinned (no diagonal/orthogonal that aligns with both the knight's L-step pattern and a king-pinner ray exists).
- **Check-evasion specialization** with three branches:
  - **No check:** regular per-piece-type generation; `capture_mask = enemy_occupied`, `push_mask = empty_squares`.
  - **Single check:** `capture_mask = checkers`; `push_mask = ray_between(king, checker)` for sliding checkers, else `Bitboard::EMPTY` (knight/pawn checks cannot be blocked). King moves filtered separately.
  - **Double check:** king moves only. `capture_mask = push_mask = Bitboard::EMPTY` for non-king pieces.
- **King-flee gotcha pinned in code.** When generating king moves, opponent attacks must be computed against `occupancy ^ king_bb` so a slider giving check through the king's *current* square doesn't fail to attack the square the king flees to along the same ray. Test fixtures must include this case.
- **Direct emission to a stack-allocated move list.** Public surface emits into a `MoveList` (alias for an inline `[Move; 256]` plus length, per `docs/research/m1-engine-architecture.md` §2). No `Vec` allocation on the hot path.
- **EP legality is checked at emission time** with the horizontal-pin sub-case handled explicitly (Position 3's `8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8` line). The standard pinned-mask filter is **insufficient** because both pawns vacate the rank simultaneously — neither is individually pinned, but the move exposes the king. Mechanically: after computing the post-EP occupancy, run a rook/queen-attack test on the king from the relevant horizontal direction; reject the EP move if the king is attacked.

## Why legal-direct (and not pseudo-legal-and-filter)

Three considerations specific to this project, in order of weight:

1. **TDD invariant clarity.** "The output of `generate_moves` is the set of legal moves" is a strict invariant a TDD-first project can rely on. With pseudo-legal-and-filter, the invariant is "after `generate_moves` plus a separate filter step, the result is legal" — the seam between gen and filter becomes a place for bugs the user, who does not read code, cannot catch by reading. The reviewer's job stays simpler.
2. **Mask infrastructure is wanted anyway.** The pin / checker / capture-mask / push-mask machinery is reused in:
   - **M1.F itself** for evasion specialization (single check → capture-or-block; double check → king-only).
   - **Search (M3+)** for SEE (Static Exchange Evaluation) and check-extension decisions.
   - **Eval (M3+)** for king-safety and pin-related terms.
   So the up-front cost is amortized over many milestones.
3. **Stockfish perft fixtures are legal-move counts.** Our oracle (per ADR-0006) emits legal-move totals. Generating legal moves directly removes the "is the bug in gen, or in the legality filter?" question from M1.G's debugging surface.

The performance argument is **not** a load-bearing reason. Peter Ellis Jones reports 106 Mnps for his pure legal-direct generator vs. ~190 Mnps for Qperft (tuned pseudo-legal). Legal-direct is therefore not free, but is well within our ≥100 Mnps perft target band. Stockfish at the very top of the field uses pseudo-legal+filter; we are not Stockfish, and our perft target is the 100–200 Mnps band, not the 300+ Mnps band where the difference matters.

## Why recompute pin/checker per-call (and not cache on `Position`)

Caching invites invalidation bugs — a `Position` mutation has to remember to update the cache, and a second slot in `Undo` has to roll it back. Recomputing at the top of `generate_moves` is:

- **Cheap.** Pin computation is one rook-attack + one bishop-attack from the king square against the opponent's slider sets, plus a per-pinner ray-between intersection. Checker computation is a superset of in-check testing (which we will need anyway). Sub-microsecond on the hot path.
- **Robust.** No cache-invalidation surface area; no question about whether an `Undo`-replayed position has stale pin info.

Future optimization to cache is open if profiling shows repeated computation across hot search loops, but the cache lives there (hidden behind a TT entry / search-stack slot), not on `Position`. Per `docs/research/m1-engine-architecture.md` §4, this is the recommended posture.

## Masks

Crisp definition so the implementation has nothing to interpret:

- `checkers: Bitboard` — opponent pieces whose attack set includes our king square. Includes pawns (using opponent's pawn-attack pattern from the king square), knights, sliders.
- `pinned: Bitboard` — own pieces such that, removing the piece, an opponent slider would attack our king through the now-empty square along the slider's natural direction. Pawns attacking the king do not pin (pin requires a slider behind the candidate pinned piece). Knight attackers do not pin (knights don't slide).
- `capture_mask: Bitboard` —
  - no check: `opponent_occupancy`
  - single check: `checkers` (a single set bit)
  - double check: `EMPTY`
- `push_mask: Bitboard` —
  - no check: `~all_occupancy` (empty squares)
  - single check, slider: `ray_between(king_sq, checker_sq)` — the squares strictly between, exclusive of both endpoints
  - single check, knight or pawn: `EMPTY` (cannot be blocked)
  - double check: `EMPTY`

A non-king move is legal **only if** its destination is in `capture_mask | push_mask`. (Pinned-piece restriction is an additional filter on the moving piece's allowed direction, not on the destination set.) King moves are filtered separately against the opponent's attack map computed on `occupancy ^ king_bb`.

EP captures get a per-move legality test on top of the standard mask check, per the Decision §"EP legality" wording above.

## Public surface

The `movegen` module (placed in `src/movegen.rs`, mirroring `src/mov.rs`) exposes:

```rust
pub fn generate_moves(pos: &Position, out: &mut MoveList);
pub fn in_check(pos: &Position) -> bool;
```

`MoveList` is a thin newtype wrapper over `[Move; 256]` plus a length, with `push`, `len`, `as_slice`, and an iterator. Crate-private internals (per-piece-type emit functions, mask computation) live in submodules but do not appear in the public API.

`generate_moves` is the only supported way to enumerate legal moves. There is no `generate_pseudo_legal` companion — pseudo-legal generation is not on the project's roadmap as a public surface. (If a future search heuristic wants captures-only or evasions-only, those become additional shapes of the same legal-direct pipeline, not pseudo-legal-and-filter.)

## Alternatives considered

- **Pseudo-legal-and-filter.** Faster in absolute terms by a meaningful constant factor in the literature, but loses the TDD invariant and forces a separate legality-filter codepath. The mask infrastructure would still need to be built for SEE / check extensions in search, so the supposed simplicity advantage is partial. Discarded.
- **Hybrid: legal for non-pinned + filter for pinned.** A real engine could blend these, emitting legal-direct for the common case and falling back to make/unmake-and-test for the rare pinned-EP corner. Discarded for M1.F because the mask infrastructure is being built anyway and the EP-horizontal-pin test is a single rook-attack call.
- **Caching `pinned` / `checkers` on `Position`.** Discarded for the cache-invalidation reasons above.

## Consequences

- The `magic` module's sliding-piece attack functions are now consumed by movegen, not just by tests. Any bug in `magic` will be visible as a perft mismatch in M1.G.
- `slow_attacks` continues to exist as the differential-test oracle (per ADR-0008) and is now also a candidate reference implementation for movegen tests when an obviously-correct slower version is helpful (e.g., a brute-force pin computation can be tested against a per-square ray-walker oracle).
- Pin / checker / mask routines become a stable internal API. Future search code may reuse them; future eval code may reuse pin queries. The shapes are documented in the M1.F plan.
- Test fixtures for movegen include all of the canonical edge-case taxonomy from `docs/research/m1-engine-architecture.md` §6, including the EP-horizontal-pin position, EP-causing-double-check, castling-through/out-of/into-check, and king-fleeing-along-a-slider's-check-ray.
- The performance ceiling on legal-direct is ~100–200 Mnps in the prose. We will know our concrete number when M1.G's perft + benchmark harness lands; below 50 Mnps is structurally wrong, 100+ is the floor target.

## How to apply

- All move enumeration in M2+ (UCI random-mover, search, etc.) goes through `generate_moves`. No call site re-implements legality.
- Per-call recomputation of `pinned` / `checkers`. No `Position` field for either; if a future profile screams cache, the cache lives in the search stack or TT entry, not on `Position`.
- King-move filtering uses `occupancy ^ king_bb`, not `occupancy`, when computing the squares the opponent's sliders attack. This is a one-line convention with a one-line comment; the test suite includes a fixture that fails if it's missed.
- EP legality has its own test (the horizontal-pin case) that fails if the standard pinned-mask filter is the only check applied.

## Sources

- `docs/research/m1-engine-architecture.md` §3, §6 — full prior-art research and the edge-case taxonomy this ADR builds on.
- `docs/prior-art.md` "Move generation (M1)" — synthesis naming legal-direct as the headline call.
- `docs/architecture.md` — sliding-piece attacks, make/unmake, position layout commitments that movegen consumes.
- `docs/decisions/0008-magic-bitboards-fancy-variable-shift.md` — the attack-lookup substrate.
- `docs/decisions/0006-stockfish-as-perft-oracle.md` — the legal-move-count oracle that M1.G validates against.
