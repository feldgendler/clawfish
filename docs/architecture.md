# Architecture

Current architectural state. Decisions and their rationale live in `docs/decisions/`.

## Settled commitments

| Area | Choice | Decision record |
|---|---|---|
| Language | Rust | (foundational) |
| Scope | Standard chess only; no variant abstraction | `decisions/0001-variant-chess-out-of-scope.md` |
| Board representation | Bitboards, `u64` | implied by 8×8 + variant-out-of-scope |
| Sliding-piece move gen | Magic bitboards | implied by 8×8 |
| Move encoding | 16-bit | implied by standard chess |
| Evaluation v1 | Classical (material, PSTs, structural terms) | (foundational) |
| Evaluation future | NNUE — planned milestone, not optional | `decisions/0004-nnue-hooks-from-day-one.md` |
| Make/unmake structure | Single function calls with clean interception point for future NNUE accumulator updates | `decisions/0004-nnue-hooks-from-day-one.md` |
| Strength dial | Planned milestone (M7); reuses the same eval/move-selection function-call discipline | `decisions/0005-strength-dial-as-planned-milestone.md` |
| Parallelism | Not v1, but designed in (lockless TT, Lazy SMP affinity) | (foundational) |
| Protocol | UCI | (foundational) |
| Primary platform | Apple Silicon (ARM64) macOS | `decisions/0002-target-platform-apple-silicon.md` |
| Mobile | Downstream port; weaker/slower acceptable | `decisions/0002-target-platform-apple-silicon.md` |
| Source-code reading | No third-party chess engine source code as research input | `decisions/0003-no-third-party-source-code-reading.md` |
| Testing | TDD on rules layer (perft); property tests on search; SPRT on strength changes | (see `workflow.md`) |
| Perft oracle | Stockfish (Homebrew install) is the sole external source for perft fixtures (totals + divide) | `decisions/0006-stockfish-as-perft-oracle.md` |
| Position layout | 6 piece-kind bitboards + 2 color-occupancy bitboards + 64-entry `Option<Piece>` mailbox + cached king squares + aux state (side, castling, EP target, halfmove, fullmove). FEN-spec EP semantics (set whenever prior move was a double push); the Polyglot-pseudo-legal-only EP filter applies at Zobrist time only. | `docs/plans/m1.b.md` |
| FEN parsing | Strict per Edwards 1994 §16.1: single-space field separator, strict-decimal integers (no `+`/`-`/whitespace prefix), spec-ordered castling letters. Parse-time structural checks: exactly one king per color, no pawns on rank 1/8, EP target on rank 3 or 6. Deeper semantic validity (own-side-not-in-check, castling-vs-rook consistency) deferred. | `docs/plans/m1.b.md` |

## Hot-path implications of Apple Silicon target

- **No PEXT / BMI2** — but we're not using them anyway (magic bitboards don't need them).
- **ARM NEON** is the relevant SIMD ISA. Matters when NNUE inference arrives; irrelevant for v1 classical eval.
- Apple's `aarch64` is `LITTLE_ENDIAN`, `64-bit`, with strong unaligned load support — no portability guards needed for v1.
- Profiling: `samply` (good flamegraphs), Instruments (Time Profiler), `cargo bench` with `criterion`.

## NNUE-readiness without paying for it now

NNUE is planned. The user accepts "easy to add later" — we do not need to design the full accumulator now. The minimum we owe future-NNUE is a make/unmake structure that is *interceptable*:

- `make_move(&mut Position, Move)` and `unmake_move(&mut Position, Move, Undo)` exist as discrete function calls — not open-coded inline at every call site in search.
- Move and undo info are sufficient to compute the NNUE delta after the fact (from-square, to-square, moving piece, captured piece, promotion, castling, en passant — all are present anyway).
- The eval interface is whatever's simplest in v1; we do not pre-build a trait. Replacing it later is a refactor of one function, not a structural change.

This is essentially zero added cost — it's just clean engineering — and avoids any retrofit pain.

## Things explicitly *not* designed for now

- Variant chess (Tier 1+ of any kind). Future fork.
- Multiple board sizes.
- Fairy pieces, drops, or any non-standard rule.
- Distributed search.
- GPU acceleration.

When a "what about X" question comes up that touches these, the answer is: not now, not even a bit, unless it costs literally nothing.
