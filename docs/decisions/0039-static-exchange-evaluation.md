# ADR-0039 — Static Exchange Evaluation (SEE) + good/bad capture-ordering split

**Status:** Accepted (lands with M7.A, 2026-06-15).

## Context

M7.A introduces Static Exchange Evaluation: resolving a capture sequence on one
square to its net material outcome. SEE is a **search** primitive (it needs only
piece values, not the hand-crafted eval terms M6 tuned), so — unlike the
classical eval — **NNUE does not obsolete it**: SEE-based capture ordering and
qsearch pruning persist under a learned eval. It is slotted before NNUE (M11) so
its benefit is reaped across every later milestone, and it unblocks the ADR-0030
§9 deferred good/bad capture split (and the future ADR-0026 SEE capture-futility
and qsearch SEE-pruning, M7.B/M7.C).

This ADR records the SEE *semantics*, the *value scale*, and the *move-ordering
tier* that M7.A commits. Plan: `docs/plans/m7.a.md`. Prior-art / test positions:
`docs/research/m7-see-test-positions.md`.

## Decision

### 1. Semantics (the ambiguities pinned)

SEE has several edge cases where engines legitimately differ
(`docs/research/m7-see-test-positions.md` §2). We commit to one explicit
definition so the fast resolver, the slow oracle, and the curated tests agree:

- **Pins are IGNORED.** A pinned defender still participates in the exchange.
  This is the mainstream bitboard-SEE definition and is what an attacker-bitboard
  resolver computes for free. (Pin-aware SEE has no published prose algorithm and
  the community consensus is that pins/overloads accumulate too fast to be
  meaningful — HGM, TalkChess t=48609.)
- **Least-valuable-attacker (LVA) recapture order**, iterating piece kinds
  Pawn → Knight → Bishop → Rook → Queen → King (the `PieceKind as usize` order).
- **X-ray / batteries:** when an attacker leaves a square, a slider behind it on
  the same line is revealed and joins the exchange (rescan against the shrinking
  occupancy). Pawns/knights/kings are never revealed.
- **King recapture:** the king may recapture only when the opponent has no
  remaining attacker on the square. Enforced structurally by the sentinel king
  value (§2) plus an explicit break in the swap loop.
- **Promotion-on-capture:** queen-promo assumption — a pawn capturing onto the
  last rank gains `(SEE_VALUE[Queen] − SEE_VALUE[Pawn])` and the piece occupying
  the square for the next recapture has the queen value. Applies both to a
  promo-capture as the *initial* move and to a pawn *recapture* that lands on the
  last rank mid-sequence. **Under-promotion captures are over-valued** as queens
  — an accepted standard simplification (the engine rarely searches an
  under-promo-capture as the principal line).
- **En passant:** the captured pawn is not on the destination square. Initial
  victim value = pawn; the starting occupancy removes **both** the moving pawn
  (from-square) and the EP-captured pawn (the square sharing the from-rank and
  to-file), so x-ray rescans see through the vacated EP-victim square.

### 2. Value scale — independent of eval

`SEE_VALUE = [82, 337, 365, 477, 1025, 30000]` (P, N, B, R, Q, K), defined in
`src/see.rs`. The first five mirror PeSTO MG `MATERIAL` for consistency with the
existing MVV-LVA tier, but are defined **independently** so SEE is decoupled from
any future eval re-tuning (SEE is a search primitive; NNUE does not touch it).
King = `30000` is a sentinel larger than the maximum material one side can hold
(`9Q + 2R + 2B + 2N = 11583`, pinned by a compile-time assert), so a king
recapture is only ever spent as the last resort and never makes a losing capture
look winning.

Consequence — **B (365) ≠ N (337)**: a few near-equal exchanges score
differently than a B==N scale (e.g. the Arasan MA-1 position scores −28 on our
scale vs the suite's symbolic 0). This is correct for our values, not a bug; it
is documented at the test.

### 3. Algorithm — allocation-free swap-list

`see(pos, mv) -> i32` returns the net material outcome on the `SEE_VALUE` scale
from the perspective of the side making `mv` (≥ 0 ⟺ the capture at least breaks
even). It is **only valid on capture moves** (`debug_assert!(mv.is_capture())`).
The implementation is allocation-free (stack `[i32; 32]`), maintains an
incremental running attacker set (pawn/knight/king computed once, only revealed
sliders OR'd in per step), and uses the **raw-gain-array** back-up
`gain[i-1] -= max(0, gain[i])` (the iterative form of
`f(i) = captured(i) − max(0, f(i+1))` with the initial capture forced).

> **Two formula bugs were caught during implementation by hand-computed curated
> values** (recorded here because the lesson is load-bearing): (a) the back-up
> was first transcribed without CPW's outer negation (always-non-negative
> result); (b) a promoting *recapture* under-counted the captured piece
> (`SEE_VALUE[Queen]` instead of `piece_on_sq + (Q−P)`). Both were **invisible to
> the differential proptest** because the fast resolver and the slow oracle share
> the formula — only externally-grounded hand-computed values (and third-party
> Arasan positions) distinguished them. **Lesson: a differential test between two
> implementations that share a value table / formula proves agreement, not
> correctness.**

### 4. Verification — correctness is the landing gate

Per the M5.E correctness-only-gate precedent (SEE's per-game Elo at SPRT TCs is
real but the *correctness* is the hard gate):

- **Curated suite** — hand-computed expected values on the `SEE_VALUE` scale plus
  21 third-party Arasan positions (prose-sourced per ADR-0003, asserted as ground
  truth) covering x-ray, promotion (initial + mid-sequence), en passant,
  multi-defender LVA order, king-stop, and the pin-ignoring case.
- **Differential proptest** — `see()` ≡ a structurally-independent `slow_see`
  oracle (full attacker recompute each step vs. incremental x-ray) over ≥ 2048
  random positions × all their captures (the M1.C magic / slow-ray-walker
  precedent). The oracle derives EP/promo square-arithmetic inline, not via a
  shared helper.
- **King-recapture-stop equivalent mutant.** The stop `break` is an equivalent
  mutant: the sentinel (30000) + the minimax back-up produce the identical return
  value with or without it (a king-into-attack recapture back-propagates a
  hugely-negative value that the prior `max(0,·)` floors to 0). It is kept as a
  performance optimization, covered by an execution-path test, and triaged in
  `.cargo/mutants.toml` (the M5.H1 equivalent-mutant precedent).

### 5. Move-ordering tier (the ADR-0030 §9 deferral) — EXPLORED, SHELVED

**The capture-ordering split was the planned first consumer of `see()`, but it
SPRT'd flat and is NOT in production.** Two variants were measured vs `M5.K`
(mixed-TC + virtual-clock, elo0=0/elo1=5, 400 games each):

- **v1** — demote every `SEE < 0` capture to a bottom tier below history quiets
  (`BAD_CAPTURE_BASE = −2_000_000`): **Δ Elo −10.43 [−34.69, +13.74]**.
- **v2** — demote only `SEE < −SEE_ORDER_MARGIN` (margin = 100 cp, ~1 pawn), to
  avoid over-demoting marginally-losing / pin-or-tactic-misclassified captures:
  **Δ Elo +2.61 [−22.39, +27.62]**.

The retune moved the point estimate +13 Elo (confirming the over-demotion
hypothesis) but landed **flat** — neither variant clears any ADR-0037 ship rung.
**Conclusion: ordering-by-SEE is neutral for this engine** — the existing
MVV-LVA + TT + killer + history ordering is already strong enough that reordering
captures by SEE adds nothing measurable. SEE's Elo is in *pruning* (M7.B/M7.C),
not ordering. The experiment (both variants + CIs + per-TC) is documented in
`bench/m7.md`; the hook (`BAD_CAPTURE_BASE`, `SEE_ORDER_MARGIN`,
`see_nonneg_fastout`, the `negamax_move_order_score` split) is **reverted** on
`main` and the split *code* is not retained (the result is conclusive enough that
re-deriving it from this record is cheaper than carrying the code). The tier
ladder it implemented, for the record:

```
TT > good captures (SEE≥0) > killer0 > killer1 > history quiets > bad captures
```
- A **fast-out** (`SEE_VALUE[victim] ≥ SEE_VALUE[attacker]` ⟹ SEE ≥ 0) keeps the
  full resolver off the common cheap-takes-expensive captures; correctness-neutral
  (a strict SEE≥0 subset). It does **not** bound worst-case per-node cost
  (`attacker > victim` captures still run the full `see()`).
- Non-capture promotions keep the top capture tier (never routed through `see()`).
- **qsearch ordering is untouched** (ADR-0030 §9 keeps qsearch out of the stager);
  qsearch SEE-pruning is M7.B.

## Disposition — landed INERT (M7.A-infra, 2026-06-15)

The SEE **evaluator** (`src/see.rs` + `mod see`) lands on `main` as verified,
tested infrastructure with **no production caller** — `evaluate`/search are
**byte-identical to `M5.K`** (bench d7 `1326598` / d4 `112020`, deterministic),
so the landing is provably inert and carries **no SPRT gate** (the M6
inert-landing precedent; correctness is the gate, met per §4). The capture-
ordering split that would have consumed it was explored and shelved (§5). The
module-wide `dead_code` allow is removed when **M7.B (qsearch SEE-pruning)**
wires the first real consumer.

## Consequences

### Positive
- **Permanent reusable search primitive** — `see()` survives NNUE and is the
  foundation M7.B/M7.C consume.
- **Inert landing is zero-risk** — byte-identical to `M5.K`, no behavior change.
- The exchange-resolver correctness work (the two formula bugs, the third-party
  validation discipline) is done once and reused by every SEE consumer.

### Negative / costs
- **No Elo at M7.A** — the only consumer attempted (capture ordering) was neutral
  (§5). The strength payoff is deferred to M7.B/M7.C (pruning).
- A fully-`dead_code`-allowed module sits on `main` until M7.B lands its consumer
  (honest "infra ahead of consumer", scoped by the M7.B follow-up).

### Open / deferred
- **Quiet-move SEE** (SEE of a non-capture move to a defended empty square) — out
  of scope for the capture-only `see()`; three Arasan x-ray positions (XR-3/4/5)
  that use it are not portable here. A future extension if a pruning consumer
  needs it.
- **Under-promotion captures** valued as queens (accepted simplification).
- **Incremental-direction-only x-ray rescan** — micro-opt deferred.
- **Capture-ordering split** — shelved after two flat SPRT variants (§5); could be
  revisited but ordering-by-SEE looks structurally neutral for this engine.

### Lesson
A search technique that is *correct and reduces nodes* (the v1/v2 ordering cut
−7.6% nodes) does not necessarily *gain Elo* — fewer nodes at a worse-ordered
frontier can wash out. The node count is a diagnostic, not a strength proxy; only
the SPRT decides. SEE's leverage in a modern engine is concentrated in pruning,
not ordering, once MVV-LVA + TT + killers + history are in place.
