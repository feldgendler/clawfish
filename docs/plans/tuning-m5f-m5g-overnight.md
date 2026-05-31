# Overnight tuning campaign — M5.F qsearch-in-TT + M5.G singular-extensions

**Started 2026-05-30 (overnight, unattended).** Pulls the two top *actionable*
items from `docs/tuning-backlog.md` (the higher items — M5.I / M5.H2 — are gated
on depth≥14, not yet met). Tuning-class changes: each ≤20 LOC, gate = SPRT.

## Baseline & methodology

- **Baseline tag = `M6.J`.** Current production HEAD (`266c314`, the M6.K
  removal) is behavior-identical to `M6.J`: M6.K only deleted an eval term and is
  eval-byte-identical (bench `1357063` both), search untouched. So `M6.J`'s
  binary faithfully represents current production for an SPRT baseline. The
  backlog's "SPRT vs M5.F / M5.G" wording is M5-era; the correct baseline is
  whatever currently ships, i.e. `M6.J`.
- **Mixed-TC + virtual-clock** per ELOH.D / ADR-0037:
  `SPRT_TC_SAMPLE='10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1'`, `SPRT_VIRTUAL_CLOCK=1`.
  Virtual clock makes per-game results robust to CPU contention, so a background
  build of the next candidate doesn't bias a running SPRT (only slows it).
- **Full QoS** (`SPRT_LAUNCH_PREFIX=''`) — machine idle overnight, throughput
  matters.
- 400-game cap, `elo1=10` (search-change convention), distinct seed per campaign.
- **Decision rule:** ship if SPRT crosses H1, or (at the 400-game cap with
  verdict=continue) if the pentanomial CI lower bound > 0 — the rung-1-by-CI
  ship precedent (ADR-0037 §9, how M6.J itself shipped). Otherwise revert;
  production stays `M6.J`. Either way the run is logged under `bench/sprt/`.
- **One SPRT at a time** (serial). Concurrent runs would only halve throughput
  and add OOM risk (resource-contention discipline, workflow §12).
- **Review compression:** one blind final-review per code-changing candidate; no
  plan/test-suite review loop (disproportionate for ≤20-LOC tuning changes).
  The const-only retune (C2) skips review entirely — its diff is a constant plus
  a test-robustness edit; the SPRT is the gate.

## Candidate queue (ordered by hypothesis clarity, my call per the prompt)

### C1 — M5.F-3: suppress Path-A (stand-pat fail-high) qsearch TT store

- **Hypothesis.** Stand-pat cutoffs are the most frequent qsearch store path;
  they store a Lower entry (`best=stand_pat`) that can be recomputed cheaply on
  the next visit. Suppressing them slashes store count → less TT pressure →
  recovers the documented M5.F slow-TC slight-regression without losing the
  fast-TC gains. (backlog M5.F item 3, highest-leverage.)
- **Change.** `src/search.rs` Path A (~line 2465): replace
  `return self.qsearch_store_and_return(pos, sp, TtBound::Lower, 0, ply)` with
  `return sp` (no store).
- **Test.** Rewrite `qsearch_tt_store_stand_pat_fail_high_lower` → assert Path A
  now produces **zero** stores and `tt.probe` returns `None`, while the return
  value still equals stand_pat. (Behavior-change update — the old test pinned the
  store this campaign removes.)
- **Risk.** Loses re-visit cutoffs on stand-pat-fail-high transpositions; net
  effect is exactly what the SPRT measures.

### C2 — M5.G-2: `SE_MARGIN_PER_DEPTH` 1 → 2

- **Hypothesis.** Wider singular margin (`singular_beta = tt_score − depth·M`)
  tightens SE eligibility → fewer extensions, less verification cost.
  Stockfish-historical default 2 vs our Xiphos/Ethereal 1. (backlog M5.G item 2.)
- **Change.** `src/search.rs:525` `SE_MARGIN_PER_DEPTH = 2`.
- **Test.** `negamax_se_extension_at_singular_beta_boundary` bakes in margin=1
  (`tt_score_raw = s_beta_value + depth`). Make it symbolic:
  `tt_score_raw = s_beta_value + depth as i32 * SE_MARGIN_PER_DEPTH` so the
  boundary fixture holds at any margin. (Robustness edit, not test-gaming.)
- **Note.** Backlog item M5.G-3 (`SE_MIN_DEPTH 8→6`) is **already shipped**
  (constant is 6 — the v2 retune); dropped from this campaign.

### Final disposition (combined ship)

Both C1 and C3 ship (each +37.49 Elo vs M6.J, CI-lower>0), with **complementary
per-TC profiles** (C1 strongest fast/mid, C3 strongest slow). They both touch
qsearch + the E51 bench pin, so they are combined into one tree (M6.J + C1 + C3),
the E51 pin re-pinned to the combined d4 bench `111_992` (deterministic ×2;
< either alone — C1 112467, C3 112020 — confirming they compose), and a
**combined-confirmation SPRT (C1+C3 vs M6.J, seed `0xC1ABF15AE10DDF09`)** run
before commit to confirm no destructive interaction. C2 stays reverted. On a
positive combined verdict: commit C1 then C3 to main (no push — surface for
morning review), update tuning-backlog + CLAUDE.md + the workflow tag table.
Combined patch: `bench/sprt/patches/c1c3-combined.patch`.

| Combined-confirmation | Seed | Verdict | Δ Elo [CI] | Decision |
|---|---|---|---|---|
| C1+C3 vs M6.J | `0xC1ABF15AE10DDF09` | continue@400 (llr=0.25) | +9.56 **[−17.18**, +36.41] | **DOES NOT CONFIRM** — destructive interaction at 40+0.4 (W=3 L=35). `bench/sprt/2026-05-30-c1c3-combined-vs-m6j.md` |

**Outcome: the two ships do NOT compose.** C1 and C3 each ship +37.49 alone but
the combination is flat (CI straddles 0). A fresh-seed 40+0.4-only re-run
confirmed the interaction is **real** (combined −57.86 [−90.15, −26.53] there;
the original 31.4% / W=3 L=35 was seed-amplified but the sign/significance are
robust across seeds). H0 (seed artifact) is dead; the two qsearch-TT changes
trade accuracy for speed and the inaccuracies compound at mid/slow TC (H1).

**FINAL DISPOSITION (user decision 2026-05-31):** **ship C3 (M5.F.1) alone** —
depth-amplifying (strongest at 60+0.6, cleanest per-TC), the most ELOH.D-aligned
single ship. **Defer C1 (M5.F.3)** to the tuning-backlog with the interaction
data (validated +37.49 standalone; re-queue as ship-instead or
bisect-and-recompose). **Revert C2 (M5.G-2)**, flat. Committed C3-alone to main
(bench d4 `112020` / d7 `1354640`); not pushed (left for review).

### C3 — M5.F-1: allow `Exact` at non-terminal completed-loop qsearch paths (contingent)

- **Hypothesis.** `qsearch_tt_bound_for_completed_node` currently caps
  completed-loop bounds at Lower/Upper (Stockfish 45e5e65 rule). Allowing Exact
  when `alpha_initial < best < beta` tightens qsearch's TT contribution.
  (backlog M5.F item 1.) ~20 LOC + tests + blind review. Runs only if overnight
  time remains after C1/C2.

## Results log

| Campaign | Seed | Verdict | Δ Elo [CI] | Decision |
|---|---|---|---|---|
| C1 | `0xC1ABF15AE10DDF03` | continue@400 (llr=2.01) | **+37.49 [+12.46, +62.92]** | **SHIP** (CI-lower>0). `bench/sprt/2026-05-30-c1-*.md` |
| C2 | `0xC1ABF15AE10DDF05` | continue@400 (llr=1.04) | +20.87 **[−3.30**, +45.24] | **REVERT** (CI straddles 0; bimodal per-TC). `bench/sprt/2026-05-30-c2-*.md` |
| C3 | `0xC1ABF15AE10DDF07` | continue@400 (llr=2.65) | **+37.49 [+15.71, +59.58]** | **SHIP** (CI-lower>0; strongest at 60+0.6, complementary to C1). `bench/sprt/2026-05-30-c3-*.md` |

**C3 pre-review record:** 3-way completed-node bound classifier (Exact when
`alpha_entry < best < beta`); `alpha_entry` captured post-MDP / pre-stand-pat;
paths C + F pass it. **Soundness established + blind-review-verified:** negamax
delegates to qsearch at depth 0 *before* its TT-cutoff probe, so a depth-0
qsearch entry never triggers a negamax cutoff — the only consumer of
qsearch-Exact is qsearch's own re-probe, where a completed fail-soft loop value
with `alpha_entry < best < beta` (full-window capture search) is
window-independent (standard fail-soft PV-exactness). fmt/clippy clean;
all-target `cargo test --release` green; `cargo mutants --in-diff --lib` 6
caught / 1 unviable / **0 survivors**; blind review **no further substantive
issues** (rigorous soundness trace; 2 nits fixed — bench-comment causal
wording). Bench re-pin d4 `112497→112020` (deterministic; via qsearch's own
re-probe cutoffs, NOT negamax). Helper §6.3 tests converted to 3-way with
boundary kills; completed-loop store test split into Exact (alpha=-INF) + Upper
(alpha=900) phases; path-C now asserts Exact.

**C2 pre-review record:** fmt/clippy clean; full-target `cargo test --release`
green (SE boundary test made margin-agnostic via symbolic `SE_MARGIN_PER_DEPTH`).
**No bench re-pin needed** — d4 bench is unchanged at `112_497` because singular
extensions fire only at `depth ≥ SE_MIN_DEPTH=6` and the bench runs at depth 4;
the change is confirmed live at d7 (`1334671`, fewer extensions). Review/mutants
loops skipped per the const-only-tuning compression (constant + test-robustness
edit; the SPRT is the gate).

**Pre-review record:**
- **C1**: fmt/clippy clean; `cargo test --release` all-target green (1901 lib +
  integration); `cargo mutants --in-diff` (--lib-scoped) **4/4 caught**; blind
  final review **no further substantive issues** (reviewer independently traced
  every reader of the suppressed entry → value-identical, speed-only). New d4
  bench `112_467`, d7 full `1_354_385` (both deterministic ×2). Note: the
  unscoped mutants baseline flaked twice — once on the (now-fixed) E51 bench pin,
  once on the load-sensitive `integration_eof_terminates_engine_cleanly` 2s exit
  timeout; `--lib` scoping (the mutants are all in `qsearch`) gave the clean run.
