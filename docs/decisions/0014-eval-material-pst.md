# ADR-0014 — Evaluation v1: material + PeSTO middlegame PST

**Status:** Accepted (lands with M3.A).

**Context:** The engine needs an evaluation function for the first time. M3.A is the first phase that *evaluates* — the depth-1 `GreedyMover` consumes it; M3.C alpha-beta will too.

## Decision

### 1. Composition: material + piece-square tables, single-phase

`evaluate(&Position) -> i32` returns a side-to-move-relative centipawn score. Two terms:

- **Material** — piece-count × per-kind value. PeSTO middlegame table (P=82, N=337, B=365, R=477, Q=1025, K=0).
- **Piece-square tables** — PeSTO middlegame PSTs vendored verbatim. One `[i32; 64]` table per piece kind.

Both terms are folded together into a single signed value via the precomputed `PSQT[color][kind][square]` lookup table, signed so that white pieces add and black pieces subtract.

Single-phase MG-only is acceptable for M3 per `docs/research/m3-eval-material-pst.md` §3. Tapering, mop-up, bishop pair, mobility, king safety, pawn structure → M6.

### 2. Source: PeSTO MG values, vendored verbatim

`docs/research/m3-eval-material-pst.md` §"Recommended PST data" carries the load-bearing copy. Source: Ronald Friederich's PeSTO TalkChess post via Chess Programming Wiki. Texel-tuned against actual game data; widely adopted as a baseline.

No modification at vendor time. M3.A's `src/eval.rs` reproduces the tables exactly.

### 3. Perspective: side-to-move

`evaluate` returns positive when the side-to-move is winning; negative when losing. Aligns with negamax (M3.C) without per-call sign-flip plumbing.

Internally, the field stored on `Position` is `static_eval_white: i32` (white-perspective canonical). `evaluate` flips it on `if side == Black { -e }`.

### 4. Square indexing: LERF + XOR-56 flip

Engine's internal layout: `a1 = 0`, `h8 = 63` (LERF). PeSTO arrays as printed are `a8 = 0`, `h1 = 63`. The flip from LERF to PeSTO array index is `s ^ 56`.

- White piece on `s`: PSQT contribution = `+(MATERIAL[kind] + MG_PST[kind][s ^ 56])`.
- Black piece on `s`: PSQT contribution = `-(MATERIAL[kind] + MG_PST[kind][s])`.

The `s ^ 56` flip lives only in the compile-time PSQT table builder. Hot-path `evaluate` reads the precomputed table directly.

### 5. Insufficient-material draws — in eval

`evaluate` returns `0` for FIDE-mandatory draws by insufficient material:

- KvK (just kings).
- KvN (king + knight vs. king, either side).
- KvB (king + bishop vs. king, either side).

Not detected in eval at M3 (deferred to M6):
- KBvKB same-color bishops. Rare at depth-1 plus the test surface cost; M6 ships it alongside the bishop-pair term.

### 6. Incremental update — `make_move` / `unmake_move`

`Position` carries `static_eval_white: i32`. Maintained incrementally by `make_move` via per-flag delta (six flag categories, one delta expression per category). `unmake_move` restores from `Undo::prior_static_eval`.

- **Debug builds:** post-make assert `static_eval_white == eval::eval_white_from_scratch(&pos)`.
- **Release builds:** trust the delta. Always-on `make_move_no_eval_recompute_in_release` perf sentinel guards against accidental from-scratch reintroduction (mirrors M1.E's zobrist sentinel).

### 7. NNUE-readiness (consumed from ADR-0004)

The incremental `static_eval_white` field is the same shape as the future NNUE accumulator. When NNUE lands (M9):

- `Position` gains a parallel `accumulator: NNUEAccumulator` field.
- `make_move` / `unmake_move` apply the NNUE delta in the same place this ADR's PST delta lives.
- `evaluate` becomes `nnue::evaluate` instead of the PST/material sum.
- The classical PST/material path is preserved (or removed) at NNUE-land time.

### 8. Tuning — out of scope

PeSTO values vendored as-is. No Texel tuning, no SPRT-driven parameter sweeps at M3. Tuning infrastructure is itself a milestone (M6); applying tuned weights is a downstream consumer.

## Consequences

- **Speed:** ~5–10 ns per `evaluate` call (popcount-based insufficient-material check + one field read + perspective flip). Incremental delta in `make_move` is ~5–8 ns.
- **Strength:** known weakness in bare-king endgames (KQ vs. K, KR vs. K conversion). Mitigated by M3 SPRT target (vs. RandomMover) being far above the conversion threshold; addressed structurally at M6.
- **Underpromotion:** queen always wins on material at depth-1; under-promotions never picked. Search depth (M3.C) introduces situations (e.g. avoiding stalemate) where under-promo would be correct, but M3.A's `GreedyMover` is depth-1 — any underpromotion miss is by design.
- **Test surface:** color-swap and PST-symmetry properties cover the eval invariants compactly. Anchor tests pin specific FENs to known-correct values to catch table-typo regressions.

## Alternatives considered

- **Simplified eval (Michniewski).** Rejected: PeSTO is Texel-tuned and demonstrably stronger (research §2); the data-vendoring cost is identical.
- **Tapered (MG + EG) at M3.** Rejected: research §3 — single-phase is sufficient for the M3 SPRT target, and tapering's blend logic adds debugging surface (per-piece phase weights, blend formula correctness) that's better isolated to M6.
- **Two separate fields (material + PST score).** Rejected: combining them into one `static_eval_white` field eliminates dual-source-of-truth drift risk (same argument as M2.D's "no seed field on Engine"). The hot-path field-read shape is identical.
- **Insufficient-material in `Search` instead of `eval`.** Rejected: insufficient material is a property of the position, not the search depth. Single eval site is the cleanest factoring; analogous to how mate detection lives in movegen, not in search.

## References

- `docs/research/m3-eval-material-pst.md` — full design space + vendored data.
- `docs/decisions/0004-nnue-hooks-from-day-one.md` — incremental update site.
- `docs/decisions/0011-uci-io-threading.md` — `Search` trait that consumes `evaluate`.
- [CPW — PeSTO's Evaluation Function](https://www.chessprogramming.org/PeSTO%27s_Evaluation_Function).
