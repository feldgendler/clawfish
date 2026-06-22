# Overnight tuning campaign — 2026-06-02 (A(a) / B / C)

**Started 2026-06-02 ~03:00 CEST (overnight, unattended).** User prompt: "try
A(a), B, C in order, implement and evaluate each, follow normal routine."

Pulls three actionable items from `docs/tuning-backlog.md` (the higher items —
M5.I / M5.H2 — remain gated on depth≥14, not yet met).

## Baseline & methodology (shared)

- **Baseline tag = `M5.F.1`** (current production HEAD; main is byte-identical).
- **Mixed-TC + virtual-clock + full QoS** per ELOH.D / ADR-0037:
  `SPRT_TC_SAMPLE='10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1'`, `SPRT_VIRTUAL_CLOCK=1`,
  `SPRT_LAUNCH_PREFIX=''`.
- 400-game cap; distinct seed per candidate.
- **Decision rule:** ship if SPRT crosses H1, or (at the 400-game cap with
  verdict=continue) if the pentanomial CI lower bound > 0 (rung-1-by-CI,
  ADR-0037 §9). Otherwise revert; production stays `M5.F.1`. Either way the run
  is logged under `bench/sprt/`.
- **One SPRT at a time** (serial; resource-contention discipline, workflow §12).
- **elo1:** 10 for the search-layer items (A, B); 5 for the eval-term retune (C,
  marginal eval terms per the M6 convention).
- **Review compression** (per the prior overnight-tuning precedent): one blind
  final-review per code-changing candidate; no separate plan/test-suite review
  loop for the small search tweaks (A, B). WAC+STS run only on a *shipping*
  candidate (post-SPRT, pre-commit), as diagnostics — not as a gate.

## Item A(a) — recompose M5.F.3 (Path-A store suppression) with M5.F.1

**Backlog ref:** M5.F item 3, re-queue option (b) "bisect the interaction and
re-tune so they compose."

**Background.** M5.F.3 (unconditional Path-A stand-pat-fail-high store
suppression) validated +37.49 standalone vs M6.J but did NOT compose with the
shipped M5.F.1 (Exact at completed-loop): combined collapsed at 40+0.4 (−58 Elo,
seed-confirmed) while spiking at 10+0.1 (+72%, partly seed-amplified). Leading
mechanism (H1): both reduce qsearch nodes / reshape TT; combined the speed
saturates while inaccuracies compound at mid/slow TC.

**Honest prior.** The two are largely *substitutes* (both "qsearch wants better
TT behavior"), so additional composable Elo on top of M5.F.1 is a long shot. We
take one informed, principled shot rather than an exhaustive node-level
bisection (budget).

**Approach — margin-gated Path-A suppression.** Replace the unconditional
M5.F.3 suppression with a value-conditional one at `src/search.rs` Path A
(~2494):

```rust
if sp >= beta {
    // Decisive stand-pat fail-high (sp - beta >= KEEP_MARGIN): a high-value
    // Lower bound worth storing for re-probe cutoffs. Marginal fail-high
    // (sp - beta < KEEP_MARGIN): low-value, high-volume, near-pure TT
    // pressure -> suppress (M5.F.3 rationale). Keeping the decisive entries
    // preserves the cutoff info whose absence (under unconditional M5.F.3)
    // is the leading suspect for the slow-TC collapse against M5.F.1's Exact
    // entries.
    if sp - beta >= QS_PATHA_KEEP_MARGIN {
        return self.qsearch_store_and_return(pos, sp, TtBound::Lower, 0, ply);
    }
    return sp;
}
```

- `const QS_PATHA_KEEP_MARGIN: i32 = 100;` (≈ a decisive >1-pawn fail-high).
- Cheap (≤10 LOC), TC-agnostic, no signature change → small mutation surface.
- **Test:** update `qsearch_path_a_*` store test → two phases: decisive
  fail-high (sp−beta ≥ 100) stores Lower; marginal fail-high (sp−beta < 100)
  produces zero stores / no TT entry; both still RETURN stand_pat.
- **Bench:** node count shifts (fewer suppressed stores than unconditional
  M5.F.3, more than M5.F.1) → re-pin `DEPTH4_BENCH_NODES` on-trial.
- **SPRT** vs `M5.F.1`, elo1=10, seed `0xC1ABF15AE10DE0A0`.
- **Alternative noted (not taken):** the opposite gate (suppress decisive, keep
  marginal) and a qsearch-local-depth gate (thread `qs_ply`, suppress deep
  frames). Margin-gate chosen as the cheapest principled lever.

## Item B — depth-adaptive aspiration first-try half-width

**Backlog ref:** "ML-tuned aspiration window sizing" tier 2 (depth-adaptive
parametric). The cheapest, lowest-risk item; explicitly "post-M5, pre-M12."

**Current.** `aspiration_window(prior, depth)` uses a constant
`ASPIRATION_HALF_WIDTH = 50` for all `depth >= ASPIRATION_MIN_DEPTH = 6`.

**Approach.** Replace the constant first-try half-width with a depth-adaptive
parametric width that narrows as depth grows (deeper iterations have more stable
score continuity → a tighter window saves more nodes; a miss costs one
re-search, already cheap at depth):

```
half_width(depth) = max(MIN_WIDTH, BASE - SLOPE * (depth - ASPIRATION_MIN_DEPTH))
```

integer arithmetic, saturating at `MIN_WIDTH`. Equivalent to the backlog's
`base · max(min_factor, 1 − α·(depth − threshold))` with `threshold =
ASPIRATION_MIN_DEPTH`.

- Defaults (literature-reasonable, no SPSA harness yet): `BASE = 50` (== current
  constant, so depth==6 is unchanged), `SLOPE = 4`, `MIN_WIDTH = 16`. Reaches
  `MIN_WIDTH` at depth ~14.5; mild monotone narrowing across the typical 6–14
  band.
- `aspiration_window` calls the new `fn aspiration_half_width(depth: u32) -> i32`.
- ~30–40 LOC. Light plan only (this doc); tests + impl by coder; final-review.
- **Tests:** `aspiration_half_width` unit tests (depth==6 → 50; monotone
  non-increasing; saturates at MIN_WIDTH; never below MIN_WIDTH); update any
  `aspiration_window` test that pinned the constant 50 at depth>6.
- **Bench:** aspiration fires at depth ≥ 6; the d4 bench is unaffected (depth 4
  < 6) — expect d4 bench unchanged at `112_020`; confirm. d7 will move; record.
- **SPRT** vs `M5.F.1`, elo1=10, seed `0xC1ABF15AE10DE0B0`.
- **If flat (likely small effect):** record, revert. A second config
  (steeper SLOPE) only if time and the first shows a directional signal.

## Item C — sign/monotonicity-constrained deferred-term retune

**Backlog ref:** "M6.I sign/monotonicity-constrained deferred-term retune"
(DEFERRED, low priority). Small expected delta.

**Note on staleness.** The backlog entry warm-starts from the `M6.I` vector and
SPRTs vs `M6.I`. Production eval is now `M6.J`'s meta-tuned mix (shipped on top
of M6.I). The *correct* baseline is current production = `M5.F.1` (whose eval ==
M6.J). So: warm-start the constrained Texel retune from the **current shipped
eval weights**, apply sign/monotonicity constraints, select by held-out
validation loss, SPRT the winner vs `M5.F.1` at elo1=5.

**Approach (infra permitting; lowest priority, may not finish overnight).**
1. Inspect `src/bin/texel-tune.rs` for an existing constraint/regularization
   surface; the corpus lanes are on disk (`bench/corpus/*/lane.bin`).
2. Add per-term sign clamps (penalty terms ≤ 0; passed/connected bonuses ≥ 0 and
   rank-monotone) as projected-gradient clamps or strong one-sided ridge,
   warm-started from the shipped vector.
3. Select λ / constraint strength by held-out validation loss.
4. `texel-tune apply` the winner; final-review the eval-data diff; bench re-pin;
   SPRT vs `M5.F.1` (elo1=5).
- **Abandon if:** constrained val-loss is materially worse than shipped
  (corpus genuinely wants the unconstrained shapes), or the pipeline can't be
  driven cleanly overnight.

### C — scoping discovery (2026-06-02 ~08:45, BLOCKED ON DECISION)

Inspected `src/bin/texel-tune.rs` + `src/texel/{optimizer,loss}.rs`:
- `tune` warm-starts from `EvalParams::shipped()` ✓ (correct baseline frame).
- `Reg` supports **L2 ridge toward shipped** (`l2_lambda`) + **monotonicity**
  (`mono_lambda`) on structurally-monotone table groups — both already wired
  into `loss_and_grad`, both **hardcoded to 0.0 in `cmd_tune`** (no CLI flag).
- **No hard sign constraint** (projected-gradient clamp) exists. The L2 ridge
  "pulls toward production, never toward literature" — so it pulls toward the
  *wrong-signed* shipped values (e.g. `ISO_MG=+5`), i.e. it does **not** address
  the sign violations that are C's defining target.

⇒ C as written (sign **and** monotonicity constraints) needs **new
constrained-optimizer numerics** (sign-projection in `optimizer.rs` + tests +
review) — a real implementation task, not a config run. The monotonicity half is
cheap (existing `mono_lambda` + ~6 LOC CLI plumbing); the sign half is not.

Combined with: C = lowest priority / smallest expected delta; tonight's A(a) +
B both NO-SHIP (engine looks at a local optimum at current strength/TC); and the
backlog's own guidance to fold C into the larger Arm-B PST co-tune. Surfaced to
the user for a go/no-go rather than auto-writing optimizer numerics unattended.

## Sequencing & ETA

Serial: A(a) → B → C. Each: implement → blind final-review → bench → SPRT
(the gate) → ship (CI-lower>0) + WAC/STS + commit, or revert + record.
At ~2–2.5 h per mixed-TC SPRT, realistic overnight reach is A(a) + B evaluated,
C started. Commits land on `main` but are **not pushed** (left for morning
review). Hourly progress + refreshed ETA per CLAUDE.md.

## Results log

| Item | Candidate | Seed | Verdict | Δ Elo [CI] | Decision |
|---|---|---|---|---|---|
| A(a) | margin-gated Path-A store | `…E0A0` | continue@400 (llr=−0.76) | **−7.82 [−33.43, +17.71]** | **NO SHIP** — 40+0.4 collapses again (39.9%); substitutes confirmed. `bench/sprt/2026-06-02-aa-margin-gate-vs-m5f1.md` |
| B | depth-adaptive aspiration width | `…E0B0` | continue@400 (llr=−1.07) | **−9.56 [−32.51, +13.32]** | **NO SHIP** — flat, no depth-amplifying trend. `bench/sprt/2026-06-02-b-depth-adaptive-aspiration-vs-m5f1.md` |
| C | sign/monotonicity eval retune | — | not run | — | **DEFERRED** (user decision) — sign-constraint core needs new optimizer numerics; folded into Arm-B PST co-tune. See "C — scoping discovery" above. |

## Campaign close (2026-06-02 ~08:50)

**0 ships.** Production stays `M5.F.1` (bench d4 `112_020` / d7 `1_354_640`,
byte-identical — A(a) and B both reverted, C not run). The two search-layer
tweaks (A(a) qsearch Path-A margin gate, B depth-adaptive aspiration) each
SPRT'd flat-to-slightly-negative vs `M5.F.1`; together with the recently-closed
M5.F items 2/4–8 and M5.G items 4–8 (also 0 ships), the search layer looks at a
**local optimum** at the current strength/TC. C deferred to Arm-B.

**Commits:** docs/records only (no code change — both candidates reverted),
committed to `main`, **not pushed** (left for review). Patches preserved under
`bench/sprt/patches/`.
