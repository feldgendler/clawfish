# ADR-0042 — SEE capture futility pruning: gate set, two-layer bound, fail-soft policy

**Status:** Accepted as a design, but the feature was **EXPLORED & SHELVED 2026-06-18** —
both SPRT variants vs `M7.B.2` failed (v1 conservative flat ≈−1.7 Elo; v2 broadened
— §4's exemption dropped — a clear regression −49 with a 60+0.6 collapse). **Not
shipped; production HEAD stays `M6.J` + `M7.B.2`.** The code is retained on branch
`m7c-see-capture-futility` (unmerged) and the design below stands as the record. The
engineering was sound (2101-pass/0-fail, mutants 0-missed, llvm-cov 97.06%, three
review loops converged); the failure was *strength* — the mechanism has no productive
operating point (too thin converts nothing; too broad over-prunes the saving tactics
slow TC would find). SEE's Elo for this engine is concentrated in the qsearch prune
(ADR-0040/0041). Record: [`../../bench/sprt/2026-06-18-m7c-cfp-vs-m7b2.md`](../../bench/sprt/2026-06-18-m7c-cfp-vs-m7b2.md),
[`../milestones/m7.c.md`](../milestones/m7.c.md). The §4 `victim ≥ attacker` exemption
was v1; §"v2" of the SPRT record removed it (the regression).

## Context

M7.C adds **SEE capture futility / delta pruning** to the negamax move loop — the
**second production consumer** of the SEE evaluator (`src/see.rs`, ADR-0039), after
M7.B's qsearch SEE-pruning (ADR-0040/0041). It is the explicit deferral recorded in
**ADR-0026 §10** ("Per-move SEE-based capture futility / delta pruning") and handed
off in **ADR-0040 §Consequences** ("the negamax-side SEE capture-futility / delta
pruning is out of scope — M7.C").

Frontier futility pruning (FFP, M5.D / ADR-0026) prunes only **quiet** moves at a
frontier node: when `static_eval + margin ≤ alpha`, a quiet's true score cannot
reach alpha. FFP deliberately leaves captures untouched because a capture swings
material that the quiet ceiling doesn't model. M7.C closes that gap for captures,
using SEE to bound the material swing.

This phase layers on:
- ADR-0039 (the SEE evaluator: `see(pos, mv) -> i32`, `SEE_VALUE`; `see ≤ victim`
  for non-promo captures is the soundness load-bearer — see §3).
- ADR-0026 (FFP: the frontier-node gate, fail-soft floor, and provenance-downgrade
  TT discipline this reuses verbatim).
- ADR-0040 (qsearch SEE-pruning: the EP-aware victim read, the `victim ≥ attacker`
  fast-out class, the `!mv.is_promotion()` exclusion).

Plan and test surface: `docs/plans/m7.c.md`. Prior-art: `docs/research/m7-see-capture-futility.md`.

## Decision

### 1. The rule (two layers)

At a frontier node already cleared by the FFP node-level gate (§2), for each
**non-promotion capture** `mv` (flag ∈ {`Capture`, `EnPassant`}), `cfp_pruned_bound`
returns `Some(bound)` iff the move should be skipped, where `bound ≤ alpha` is a
fail-soft upper bound on the move's true score. Two layers (the Stockfish MVV-first /
SEE-second pattern; research §1):

- **(a) MVV delta-prune** — `static_eval + victim + margin ≤ alpha`. `victim` is the
  maximum material a non-promo capture can net, so this is a **sound optimistic
  ceiling**. Returns `Some(mvv_bound)` **before any `see()` call**.
- **(b) SEE refinement** — only for materially-losing captures (`victim < attacker`):
  `static_eval + see(mv) + margin ≤ alpha`. Catches captures whose victim is big
  enough to pass (a) but whose recapture-aware outcome still can't reach alpha (an
  even RxR-defended trade in a lost position: `see ≈ 0`, `victim` large).
  **Winning/equal captures (`victim ≥ attacker`) are never SEE-pruned in v1** (§4).

On a fire: floor `best` and `continue` (§5) — no make/unmake, no recursion.

### 2. Node gate — reuse FFP's `ffp_static_eval`

CFP fires under the **same frontier-node gate** as FFP, already computed once before
the move loop as `ffp_static_eval: Option<i32>`: `ply > 0 && !is_pv && depth ∈
[1, FFP_MAX_DEPTH] && alpha.abs() < MATE_IN_MAX_PLY && !in_check(pos)`, payload =
STM-relative static eval. The capture block matches on `Some(static_eval)` exactly
like the quiet block — **no new node-level computation, no second `in_check` read,
no new `Option`**. Every clause transfers verbatim (`!in_check` load-bearing — the
move loop holds evasions; `!is_pv` — PV must search all moves; mate-alpha — cp
margins meaningless near mate). This intentionally couples CFP's frontier-node
definition to FFP's; a future `FFP_MAX_DEPTH` change moves both. A separate
`CFP_MAX_DEPTH` is a deferred tunable.

### 3. Soundness & fail-soft

Both bounds are valid fail-soft **upper** bounds: `see ≤ victim` holds
**unconditionally** for non-promo captures — `see()` sets `gain0 = victim` (no promo
bonus) and the swap-list backup only ever *subtracts* a non-negative recapture
(`gain[i-1] -= max(0, gain[i])`), even when an opponent recapture promotes. Hence
`see_bound ≤ mvv_bound`, and both `≤ alpha` on a fire. Like all futility pruning the
prune is heuristic (it can skip a move that is actually good); soundness here is
about **not corrupting the alpha-beta / TT invariants**, validated by SPRT, not about
the prune being provably correct. Node count is a diagnostic, not a strength proxy
(the ADR-0039 lesson).

### 4. `victim ≥ attacker` exemption on branch (b) — v1 conservatism

Restricting (b) to losing captures (`victim < attacker`):
- **Perf (strong):** bounds `see()` to the same `attacker > victim` class as the
  validated `qsearch_see_pruneable` fast-out (ADR-0040 §3) — winning/equal captures
  never pay for the resolver. (Branch (a) still prunes *any* capture, including
  winning ones, in a sufficiently lost position — its optimistic ceiling is sound.)
- **Conservatism (moderate):** winning/equal captures are the likeliest saving
  tactics. This is "fewer false prunes," **not** "no missed prunes" — it does forgo
  SEE-pruning *equal* trades (`victim == attacker`, `see ≈ 0`) in lost positions.

The literature applies the SEE gate to all captures (research §1b); broadening (b)
is the **top deferred lever** if v1 SPRTs marginal-positive (before the margin sweep).

### 5. Fail-soft floor + provenance downgrade (load-bearing — ADR-0026 §7 verbatim)

On a fire, before `continue`:

```rust
best_is_full_depth = best_is_full_depth_after_score(best, best_is_full_depth, bound, /*move_is_full_depth=*/ false);
best = best.max(bound);
```

The bound is `≤ alpha`, so it never improves alpha or causes a cutoff — it only
floors the fail-soft return and **downgrades the TT-store provenance flag** so an
all-captures-pruned node does not advertise a phantom full-depth bound. This is
byte-for-byte the FFP §7 accounting; the helper generalizes verbatim. CFP-pruned
captures are not quiets — they never enter `quiets_searched`, never advance
`quiet_index`, and the `continue` sits before the LMR machinery (captures already
bypass LMR). No mate-misdetection: terminal mate/stalemate is decided by
`stager.is_empty()` *before* the move loop, so an all-captures-pruned node returns
the floored Upper bound, not a phantom mate.

### 6. Margins

```rust
pub(crate) const CFP_MARGIN_D1: i32 = 150;   // active (FFP_MAX_DEPTH = 1)
pub(crate) const CFP_MARGIN_D2: i32 = 300;   // inactive (forward-compat)
pub(crate) const CFP_MARGIN_D3: i32 = 500;   // inactive (forward-compat)
```

The bound is `static_eval + see/victim + margin`, so the margin is **positional
slack on top of the material term** — a *larger* margin is the *safer* direction
(harder to satisfy → fewer false prunes). `150` is research-anchored (MadChess d1 =
150; Heinz ≈ 125; TalkChess "200–300 safe"); captures carry more positional
volatility than quiets, so it sits above FFP's quiet `FFP_MARGIN_D1 = 100`. D2/D3
mirror the FFP-table forward-compat precedent (inert at `FFP_MAX_DEPTH = 1`). Margin
sweep is the top tuning lever.

### 7. No root-depth ramp in v1

Unlike M7.B's alpha-*independent* `see < 0` threshold (which regressed at 60+0.6 and
needed the M7.B.2 ramp), alpha-relative capture futility is **self-regulating in TC**:
as deeper search raises alpha from below, the prune condition fires *less* at depth
(research §4). So v1 ships flat. **The per-TC SPRT read is a gate, not a footnote:**
if 60+0.6 regresses, a root-depth margin ramp ships as M7.C.1 *before* M7.C (the
M7.B→M7.B.2 sequence).

## Consequences

- Node counts drop modestly (frontier captures are far rarer than M7.B's qsearch
  leaves): bench d4 `45788 → 43128` (−5.8%), d7 `662085 → 588673` (−11.1%). Re-pinned
  in `bench/m7.md`. Per-node cost rises only on branch (b)'s losing captures (the
  fast-out class); the node saving outweighs it on time-to-depth.
- SPRT vs `M7.B.2` (mixed-TC + virtual-clock, 2-seed, per-TC read) is the gate;
  marginal-but-positive lands per the M5.F precedent, a clear regression reverts.
- Deferred levers (tuning-backlog): margin sweep; broaden (b) to all captures;
  gives-check exemption; root-depth ramp (M7.C.1); the absolute SEE-by-depth lever
  (research §1c — a *separate* future mechanism); TT-move/killer-capture exemption.
