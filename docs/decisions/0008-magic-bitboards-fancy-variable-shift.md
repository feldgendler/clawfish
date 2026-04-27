# 0008 — Sliding-piece attacks: fancy magic bitboards, variable shift

**Status:** Accepted, 2026-04-27 (binds at M1.C).

## Context

The roadmap foundationally commits to **bitboards + magic bitboards for sliders** (`docs/architecture.md` "Settled commitments"). M1.C is the phase that lands sliding-piece attack lookup. Three independent variants of magic bitboards exist in the prose literature, plus the BMI2 PEXT alternative; we need to commit to one for M1.C and document the others as deferred.

Full prior-art reasoning lives in `docs/research/m1-magic-bitboards.md` (researched 2026-04-27, ~4400 words). This ADR records the *commitment*; the research is the *justification*.

## Decision

**Fancy magic bitboards with variable shift.** Per-square `Magic` struct holding `mask`, `magic`, `shift = 64 - popcount(mask)`, and `offset` into a single shared backing array per piece-type (rook, bishop). Lookup formula:

```rust
let m = &ROOK_MAGICS[sq.index() as usize];
let blockers = (occ & m.mask).0;
let idx = blockers.wrapping_mul(m.magic) >> m.shift;
Bitboard(ROOK_ATTACK_TABLE[m.offset as usize + idx as usize])
```

Footprint: ~840 KiB total (~800 KiB rook + ~41 KiB bishop). Fits comfortably in M4 L2 (≥4 MiB on every M4 SKU, see `m1-magic-bitboards.md` §5).

**Magic constants are generated, committed, deterministic.** A separate `magicgen` binary at `src/bin/magicgen.rs` runs the search, validates against the slow ray-walker over every `(square, occupancy ⊆ mask)` pair for both pieces, and emits `src/magic/constants.rs` as a checked-in source file. PRNG is SplitMix64 with a literal seed; re-running magicgen produces a byte-identical output file (deterministic-PRNG property). The engine binary itself does no magic search; it reads the committed constants only.

**Attack tables are built at runtime via `LazyLock`.** Each piece-type's table is populated from the committed magics + the slow ray-walker on first use of the corresponding `magic::*_attacks` function. Build cost is single-digit milliseconds on M4 (~108k slot-fills for rook, ~5k for bishop). We do not pursue `const fn` table construction — documented Rust-engine experience cited in `m1-magic-bitboards.md` §4 is that const-eval blows up compile time for negligible runtime benefit.

**The slow ray-walker is permanent.** `slow_attacks` is a public crate-level module exporting the same shape of API as `magic` (`rook_attacks`, `bishop_attacks`, `queen_attacks`, plus `rook_mask`, `bishop_mask`). It is the **oracle** for:
- Building the runtime attack tables (so the source of truth is one walker, not two).
- The differential test in `tests/magic_consistency.rs` — every `(square, occupancy ⊆ mask)` pair, both pieces, asserting `magic == slow`.
- Future debugging when a perft mismatch points at sliding-piece attacks.
- Higher-layer test code that wants an obviously-correct lower-layer reference (pin detection, attack maps, etc., as those land in M1.E and M1.F).

Cost of keeping it: a few dozen lines of Rust, no runtime cost unless explicitly invoked.

## Alternatives considered

**Plain magic** (~2.3 MiB; uniform 4096-entry rook slots, 512-entry bishop slots). Defensible. Discarded because variable shift's per-square slot sizing is the standard described in essentially every prose tutorial; cross-checking against literature is easier with the standard form. Footprint difference (~1.5 MiB) is not load-bearing on M4 (`m1-magic-bitboards.md` §5).

**Fixed-shift fancy magic** (single shift per piece-type; ~1.6% perft win on a 2010 Core i5 per `m1-magic-bitboards.md` §2). Saves one memory load on the lookup. Discarded for M1.C; revisit as a benchmark-driven tweak post-M1.G. The function-level interface (`fn rook_attacks(sq, occ) -> Bitboard`) hides the choice — switching is a magicgen change plus a one-line lookup change.

**Black magic** (Annuss 2017; ~692 KiB via overlapping per-square slots). Saves ~150 KiB. Adds search-space and table-build complexity that we don't need at M4. Defer.

**PEXT** (BMI2 parallel bit extract). Replaces the multiply-shift with a direct gather. Genuinely faster on Intel x86-64 — but **non-existent on ARM64**. Apple Silicon has no equivalent in NEON or SVE. Closed; not architecture-applicable.

The prose research lays out each alternative with its full reasoning; the choice is settled.

## Consequences

- The `magic` module's public surface is `rook_attacks`, `bishop_attacks`, `queen_attacks` — three pure functions on `(Square, Bitboard) -> Bitboard`. The `Magic` struct and the constants are crate-private implementation details. Future variant swaps (fixed-shift, black) are local to the `magic` module.
- `slow_attacks` is also public (so external test code can call it without `pub(crate)` access). It will not appear in the engine's hot path; its raison d'être is being the test oracle.
- The committed `src/magic/constants.rs` ties the engine to a specific mask-construction shape. Any future change to `slow_attacks::rook_mask` or `slow_attacks::bishop_mask` will fail the integration test on the first square checked (mask-mismatch assertion) — the contributor is then directed to re-run `cargo run --release --bin magicgen` and commit the regenerated constants. **The failure mode is loud, not silent.**
- Magicgen is sequential and reproducible. Parallelizing the search would buy seconds at the cost of determinism; not worth it. If we ever parallelize, this ADR needs revisiting and a new seed-handling protocol.
- `LazyLock` is in std as of Rust 1.80 (May 2024). The project's `edition = "2024"` requires ≥1.85, so this is in. We do not pull in `once_cell` or any third-party `lazy_static` crate.
- The slow walker stays in the crate's public API forever. If we ever decide to remove it (for crate-size reasons, say), we lose the differential-test oracle and a major debugging tool. The intent is "permanent."

## How to apply

- All sliding-piece attack queries in M1.E (in-check tests) and M1.F (legal-direct movegen) go through `magic::rook_attacks` / `magic::bishop_attacks` / `magic::queen_attacks`. Direct calls into `slow_attacks` from production code are a smell — gate via review.
- Any change to mask construction (rare) requires running `cargo run --release --bin magicgen` and committing the regenerated `src/magic/constants.rs` in the same change.
- Performance optimizations (fixed-shift, black magic, `get_unchecked` lookups, etc.) are deferred until we have a perft baseline (M1.G) and a profile flagging sliding-piece lookup as a hotspot. Until then, prefer correctness and clarity.
- The differential test in `tests/magic_consistency.rs` is the integrity contract. Don't weaken it (e.g. by sampling occupancies instead of enumerating). It runs in well under a second on M4.

## Sources

- `docs/research/m1-magic-bitboards.md` — full prior-art research; this ADR's reasoning trail.
- `docs/architecture.md` — bitboards + magic-bitboards as foundational commitments.
- `docs/roadmap.md` — M1.C scope listing this ADR as the binding phase.
