# ADR-0040 — Quiescence SEE-pruning: gate set, threshold, fast-out, TT policy

**Status:** Proposed (lands with M7.B, pending the confirmation SPRT vs production HEAD `M6.J`/`M5.K`).

## Context

M7.B wires the **first production consumer** of the SEE evaluator that landed
inert in M7.A (`src/see.rs`; ADR-0039). Inside the quiescence move loop, before
recursing into a capture, the engine now skips captures that are *statically
losing* per Static Exchange Evaluation (`see(pos, mv) < 0`). This is the
ADR-0016 §"SEE-based capture filter" deferral ("M5+") finally realized, and the
mechanism ADR-0039 named as where SEE's Elo actually lives:

> SEE's leverage in a modern engine is concentrated in pruning, not ordering,
> once MVV-LVA + TT + killers + history are in place. (ADR-0039)

M7.A's *ordering* split (good/bad capture reorder) was explored and **shelved**
as neutral — reordering captures by SEE added nothing measurable (two flat SPRTs;
ADR-0039 §, `bench/m7.md`). Pruning is a different mechanism: it removes subtrees
rather than reorders them, so it is expected to convert to Elo where reordering
did not.

This phase layers on top of:
- ADR-0039 (the SEE evaluator: `see(pos, mv) -> i32`, `SEE_VALUE`, pin-ignoring
  semantics, the allocation-free swap-list).
- ADR-0016 (search structure: qsearch is captures + queen-promos; the SEE
  capture-filter deferral recorded there).
- ADR-0026 (frontier futility pruning: the fail-soft, gate-set, `!in_check`
  discipline this mirrors in the negamax loop).
- M5.E qsearch structure (the in-check evasion triage, the queen-promo-stalemate
  / under-promo extension this must not perturb).

Plan and test surface: `docs/plans/m7.b.md`. Prior-art / SEE positions:
`docs/research/m7-see-test-positions.md`.

## Decision

### 1. The rule

Inside the qsearch main move loop, before `make_move`, skip move `mv` iff **all**
of:

1. **Not in check** (`!in_chk`). When in check, the loop holds evasions (incl.
   quiet king moves and blocks) and the position demands a response — pruning any
   evasion is unsound. Mirrors the ADR-0026 §3 FFP `!in_check` gate. **Load-bearing.**
2. **Non-promotion capture** (`mv.is_capture() && !mv.is_promotion()`), i.e. flag
   ∈ {`Capture`, `EnPassant`}. Non-capture queen-promos (admitted by the qsearch
   filter) are ~+940 cp material events we always search; promotion-captures are
   excluded to keep the M5.E #3 queen-promo-stalemate / under-promo-extension
   logic intact and because they are essentially never SEE-negative (they gain a
   queen). See §4.
3. **`see(pos, mv) < QS_SEE_PRUNE_THRESHOLD`** with `QS_SEE_PRUNE_THRESHOLD = 0`
   (prune strictly-losing captures).

On a fire: `continue` — no make/unmake, no `best`/`alpha`/`cutoff_move` update.

### 2. Threshold

`const QS_SEE_PRUNE_THRESHOLD: i32 = 0`. This is the SPRT/SPSA tuning lever
(plan §10). `0` prunes only strictly-losing captures. A **negative** margin
(prune only clearly-losing) keeps the fast-out valid; a **positive** margin
(prune marginally-even captures too) would invalidate the fast-out (§3) and
requires reworking it — pinned by a compile-time `assert!(QS_SEE_PRUNE_THRESHOLD
<= 0)`.

### 3. Fast-out (perf, behaviour-preserving)

A capture whose victim is worth ≥ the attacker can never be SEE-negative (worst
case the attacker is traded off, netting ≥ 0). So `qsearch_see_pruneable`
short-circuits `victim >= attacker ⇒ not pruned` **without** running the full
`see()` resolver. This keeps `see()` off the hot path for the common
winning/equal captures (PxN, RxQ, equal trades, all en-passant PxP); only
`attacker > victim` captures (QxP-class — exactly the prune candidates) pay for
the resolver. The fast-out reads the victim as `SEE_VALUE[Pawn]` for en-passant
(the to-square is empty for EP — reading `piece_at(to)` would be a bug) and
`SEE_VALUE[piece_at(to).kind]` otherwise. Valid only while the threshold ≤ 0
(compile-time assert).

### 4. Promotion exclusion is an equivalent guard at threshold 0

Under-promotion-captures (`{Knight,Bishop,Rook}PromoCapture`) never reach the
gate — `qsearch_move_filter` admits only captures + queen-promos, so the only
promotion-capture in the loop is `QueenPromoCapture`. A queen-promo-capture has a
pawn attacker (82) ≤ any victim (≥ 82), so the fast-out always fires and the
`< threshold` comparison is never reached. Hence **dropping `!mv.is_promotion()`
from the gate is an equivalent mutant** at `QS_SEE_PRUNE_THRESHOLD = 0` (it
changes no observable behaviour). The guard is retained for clarity/robustness
and to document the dependency on `qsearch_move_filter` admitting no
under-promo-captures; recorded as a documented equivalent mutant for the
`mutants --in-diff` triage (alongside the `< vs <=` boundary, §6).

### 5. TT-move: no exemption

The TT move is promoted to `moves_vec[0]` (ordering, step 7), then pruned on the
**same predicate** as any other move — there is no TT exemption. A winning/equal
TT capture hits the fast-out (never pruned); a *losing* TT capture at a depth-0
qsearch entry is low-confidence (qsearch TT entries are depth 0; ADR-0028) and
statically losing, so pruning it is consistent and the stand-pat floor still
bounds the node. Pinned by `qsearch_tt_move_pruned_when_see_losing_capture`
(losing TT capture pruned, while the position's legal QxQ is still searched). If
the SPRT regresses traceably to over-pruning trusted moves, a TT-move exemption
is the first remediation (plan §11).

### 6. Soundness & fail-soft

Qsearch's floor is stand-pat. A `see < 0` capture is statically expected to lose
material; searching it can raise `best` above stand-pat only if the recursive
line refutes the static SEE verdict (a real but rare tactical exception). Pruning
accepts that bounded accuracy loss for a large node saving — the textbook qsearch
SEE-prune. The loss is bounded: never in check, never promotions, stand-pat floor
preserved, so pruning all captures returns the (sound) stand-pat Upper bound —
exactly as a captureless quiet node already does. Net effect is validated by
**SPRT**, not asserted; node count is a diagnostic, not a strength proxy
(ADR-0039 lesson).

Documented equivalent mutants for `mutants --in-diff`: the `!mv.is_promotion()`
guard (§4) and `< vs <=` at threshold 0 (no `attacker > victim` exchange yields
`see() == 0` exactly with the integer value set `[82, 337, 365, 477, 1025,
30000]`, so the swap-list remainder never cancels to zero on the `see()` path).

## Consequences

- SEE's evaluator (`see` / `SEE_VALUE` / `attackers_to`) now has a live
  production caller; the module-wide `#![allow(dead_code)]` on `src/see.rs` is
  removed (ADR-0039 disposition).
- Node counts drop (fewer losing-capture subtrees searched); the deterministic
  `bench`/depth-4 fixtures are re-pinned to the post-prune counts — expected, not
  a regression (`bench/m7.md`).
- Per-node cost rises only on `attacker > victim` captures (the resolver call),
  bounded by the fast-out and the allocation-free swap-list; validated by the
  sustained-go NPS gate on the capture-dense Kiwipete position (plan §6,
  `bench/m7.md`).
- The negamax-side SEE capture-futility / delta pruning is **out of scope** —
  M7.C (ADR-0026 extension), an independently-SPRT-able fast-follow.
