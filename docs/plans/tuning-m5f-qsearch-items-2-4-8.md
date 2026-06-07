# Tuning campaign — M5.F qsearch backlog items 2 / 4–8

**Started 2026-05-31 (unattended).** Pulls the remaining untried M5.F qsearch-TT
backlog items (item 1 = M5.F.1 shipped; item 3 = M5.F.3 deferred). Tuning-class
changes: each gate = SPRT. User directive: "M5.F qsearch items 2/4–8".

## Baseline & methodology

- **Baseline tag = `M5.F.1`.** Current production HEAD (`1bef931`) is
  search-identical to the `M5.F.1` tag (`95a7c65`); HEAD only adds a docs commit.
  So `scripts/sprt.sh sprt M5.F.1` faithfully represents current production.
  **This supersedes the backlog's M5-era "vs M5.F" wording** — the correct
  baseline is whatever currently ships.
- **Mixed-TC + virtual-clock** per ELOH.D / ADR-0037:
  `SPRT_TC_SAMPLE='10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1'`, `SPRT_VIRTUAL_CLOCK=1`,
  full QoS (`SPRT_LAUNCH_PREFIX=''`), 400-game cap, `elo1=10` (search-change
  convention), distinct seed per campaign.
- **Decision rule:** ship if SPRT crosses H1, or (at the 400-game cap with
  `verdict=continue`) if the pentanomial CI lower bound > 0 (rung-1-by-CI,
  ADR-0037 §9 — the precedent by which M6.J and M5.F.1 shipped). Otherwise revert.
- **One SPRT at a time** (serial; ~1h50m each at this hardware). Implementation
  of the next candidate happens in a git worktree during the running SPRT — the
  running SPRT uses its already-built `target/release/clawfish`, so building the
  next candidate in a separate worktree (separate `target/`) does not disturb it
  (virtual-clock tolerates the CPU contention; only slows).
- **Independent evaluation, then combined-confirm.** Each candidate is a branch
  off `main`@`1bef931` and SPRT'd against `M5.F.1` independently. **Per the
  2026-05-30 C1+C3 lesson** (two independently-validated qsearch-TT changes were
  destructively non-additive), **any multi-ship requires a combined-confirmation
  SPRT before commit.** These items are all the same qsearch-TT subsystem.
- **Review compression:** one blind final-review per code-changing candidate; no
  plan/test-suite review loop (tuning-class). Diagnostic/config items (7, 8) skip
  review — the measurement is the deliverable.

## Candidate queue

### C-6 — item 6: TT-move ordering filter relaxation
- **Hypothesis.** A legal but filtered-out (quiet/under-promo) TT move at a
  `!in_check` qsearch node is currently dropped. Searching it (prepended for
  ordering) recovers ordering wins. **Long-chain de-risk:** added only at the
  step-7 ordering point, i.e. *past* the step-6 terminal returns, so `moves_vec`
  already contains captures — a pure-quiet chain (Andrew Grant's concern,
  ADR-0028 §7) cannot sustain (a node with no captures terminal-returns before
  reaching step 7). Legality = membership in the already-generated full legal
  list `ml` (no extra make/unmake). Reconsiders ADR-0028 §7.
- **Canary.** Bench node counts (d4/d7) — watch for explosion.

### C-2 — item 2: PV-node store suppression
- **Hypothesis.** Thread `is_pv` into qsearch (from negamax's depth-0 delegation
  + PV-first recursion); skip the TT *store* on PV nodes to cut TT pressure where
  bound looseness hurts most. Reconsiders ADR-0028 §6 (which declined `is_pv` in
  qsearch — but for *cutoffs*; this is *store* suppression). ~50 LOC.

### C-4 — item 4: qsearch TT depth / replacement convention
- **Note: the backlog premise is partly stale.** `TtEntry.depth` is `u8` (no
  `-1`), and negamax *never* stores depth 0 (it delegates to qsearch at depth 0),
  so qsearch-at-0 is already the within-generation replacement floor. The real
  remaining lever is **cross-generation**: today a stale-generation negamax entry
  can be displaced by a fresh qsearch entry. Candidate: never let a qsearch
  (depth-0) entry displace a negamax (depth≥1) entry regardless of generation.
  `tt.rs` replacement-logic change.

### C-5 — item 5: qsearch probe gating by ply
- **Low-confidence** (the "negamax just wrote it" premise is weak in this engine).
  Single defensible candidate: skip the TT probe on deep qsearch frames
  (`ply` beyond a delta from the qsearch root) where stored data is staler than
  the recompute. Sweep only if a signal appears.

### Item 7 — per-path enable/disable (DIAGNOSTIC, no SPRT)
- Not a strength play ("code-size argument once SPRT-confirmed inert"). Instrument
  per-path (A–F) qsearch store frequencies; report which paths are rare/inert.

### Item 8 — Hash-size interaction (CONFIG, 0 LOC)
- Qsearch entries ~doubled TT pressure. Measure whether a larger `Hash` shifts
  the equilibrium (harness `setoption Hash` sweep / engine default reconsideration).

## Items 7 & 8 — CLOSED analytically (no actionable strength lever)

**Item 7 (per-path enable/disable) — CLOSED.** The premise ("code-size argument
once a path is SPRT-confirmed inert") is void: paths B/C/D/E are
**correctness-mandated** terminal/edge handlers — B true-stalemate `Exact`, D
mate-at-horizon `Exact`, C single-reply extension (M5.E #1), E false-stalemate
guard (M3.D) — and **cannot be removed regardless of frequency** (they handle
legal positions that must return the right value). A (stand-pat fail-high) and F
(completed loop) are the high-frequency, load-bearing paths. So no path is
removable and there is nothing to enable/disable for code-size. No instrumentation
run was warranted (a frequency table would only confirm A/F dominate; the
conclusion — nothing removable — is independent of the numbers).

**Item 8 (Hash-size interaction) — CLOSED.** Three facts kill the lever for this
campaign: (1) the SPRT harness sets `Hash=64` for **both** engines
(`src/elo_iterate/cli.rs:789`), so Hash size is **not a candidate-vs-baseline
differential** — both scale together, the verdict is unaffected. (2) `TtEntry` is
16 B → 64 MiB = 4M entries; at the engine's per-move node counts the table is not
the bottleneck, and TT pressure that *would* bite only does so over long games —
which the SPRT already exercises at 64. (3) The remaining angle is the engine's
**default** `Hash` (`DEFAULT_HASH_MIB = 16`, `src/engine.rs:41`), which only
applies when a GUI does not override it; bumping it is a standalone config choice
with a **mobile-footprint tradeoff** (CLAUDE.md: mobile is a memory-constrained
downstream target — the 16 MiB default is plausibly deliberate), so it is **not**
a unilateral change and is **not** part of this qsearch-TT campaign. Deferred to a
future config decision/ADR if desired; an optional `Hash=128`-vs-`64` self-SPRT
(methodology probe, not a strength claim) is available on request.

## Results log

| Campaign | Seed | Verdict | Δ Elo [CI] | Decision |
|---|---|---|---|---|
| C-6 | `0xC1ABF15AE10DE016` | continue@400 (llr=−1.65) | **−24.36 [−50.84, +1.84]** | **REVERT** (CI-lower<0). `bench/sprt/2026-05-31-item6-ttmove-filter-relax-vs-m5f1.md` |
| C-2 | `0xC1ABF15AE10DE002` | continue@400 (llr=−2.38) | **−31.35 [−55.91, −7.10]** (CI fully <0) | **REVERT**. Survived suspend, ran to completion. `bench/sprt/2026-06-01-item2-pv-store-suppress-vs-m5f1.md` |
| C-4 | `0xC1ABF15AE10DE004` | continue@400 (llr=0.08) | **+6.08 [−16.44, +28.65]** (straddles 0) | **NO SHIP (flat)**. `bench/sprt/2026-06-01-item4-ttrepl-crossgen-vs-m5f1.md` |

## CAMPAIGN CLOSED 2026-06-01 ~04:35 local — 0 ships; production stays `M5.F.1`

| Item | Disposition | Evidence |
|---|---|---|
| 2 (PV loose-store suppress) | **REVERT** −31.35 [−55.91, −7.10] | CI fully <0 |
| 4 (cross-gen negamax-evict block) | **NO SHIP (flat)** +6.08 [−16.44, +28.65] | CI straddles 0; re-visit as free-rider in a future TT-replacement change |
| 5 (probe-gating by qs_depth) | **CLOSED (canary)** | strictly adds nodes (d4 +12%@4 / +4.4%@8); ~0 ceiling |
| 6 (TT-move filter relax) | **REVERT** −24.36 [−50.84, +1.84] | CI-lower <0 |
| 7 (per-path enable/disable) | **CLOSED (analytic)** | B/C/D/E correctness-mandated, non-removable |
| 8 (Hash sizing) | **CLOSED (analytic)** | not a candidate-vs-baseline differential; mobile-footprint caveat on the 16-MiB default |

**Net: nothing shipped; the M5.F qsearch-TT subsystem is at a local optimum
under the current strength/TC profile.** Lessons: (a) injecting non-forcing
moves into qsearch (item 6) or trimming its TT participation (items 2, 5) both
regress — qsearch wants *more* accurate TT data and tight tactical focus, not
less; M5.F.1 (add `Exact`) was the right direction and apparently captured the
available gain. (b) The suspend-tolerance of the elo-iterate harness was
confirmed empirically (item-2 SPRT ran through a laptop suspend to a clean
verdict). The per-item `tune/m5f-item{2,4,5,6}-*` branches were deleted 2026-06-07
(this plan + the per-item `bench/sprt/` result docs are the record, not branches);
production HEAD unchanged at `M5.F.1`.
| C-4 | _pending_ | queued after C-2 | — | review-APPROVED, 12/12 mutants, bench-inert |
| C-5 | — | **CLOSED on bench canary (no SPRT)** | ceiling ~0 | probe-gating strictly adds nodes (d4 +12%@thr4, +4.4%@thr8); no favorable mechanism |

## PAUSED 2026-05-31 ~13:10 local (laptop suspend)

State at pause (all work committed; nothing lost):

- **Branches off `main`@`1bef931`:**
  - `tune/m5f-item6-ttmove` (`f08c80d`) — item 6 complete (build+test+bench+9/9
    mutants + blind review approve-with-nits). **Its SPRT was killed mid-run at
    ~89 games trending negative (~37%)** — re-run from scratch on resume; the
    partial match dir `target/matches/sprt/20260531T124515-M5.F.1-sprt/` can be
    discarded.
  - `tune/m5f-item2-pvstore` (`54068fa`) — item 2 complete; blind review returned
    **revisions-required** (pin the *completed-loop* Exact, not just terminal),
    which the amend **addressed** (added Path-F completed-loop Exact-kept +
    Upper-suppressed PV cases + comment-drift nit). ⚠️ **The review-revision
    additions were NOT re-built/re-tested before pause** (resource discipline).
    On resume: build the item2 worktree, run the qsearch tests, then
    `SendMessage` the reviewer (`a30d52f3c86ef4abf`) to confirm convergence.
    A full-lib `cargo mutants --in-diff` was running and was killed — re-run.
  - `tune/m5f-item4-ttrepl` (`cc9fc19`) — item 4 **[WIP, UNBUILT]**: tt.rs
    cross-generation guard + 2 tests, written but never compiled/tested. On
    resume: build, test, clippy, re-pin d4 bench, blind review, then SPRT.

- **Worktrees:** `.worktrees/item2`, `.worktrees/item4` (+ baseline worktree
  `target/sprt-baselines/M5.F.1`, built & cached).

- **Not started:** item 5 (probe gating — design: add `qs_depth` frames-since-
  entry param, skip probe when `qs_depth >= N`), item 7 (per-path store-freq
  instrumentation, diagnostic), item 8 (Hash-size config sweep). Combined-confirm
  step pending if >1 ships.

**Resume order:** re-launch item-6 SPRT (or, given the negative lean, consider
reverting item 6 without a full re-run) → item-2 (validate revision, SPRT) →
item-4 (build+validate, SPRT) → item-5 → diagnostics 7/8 → combined-confirm.

## RESUMED 2026-05-31 ~20:40 local

- **Item 6** SPRT re-launched (seed `0xC1ABF15AE10DE016`, dir `…203727…`). Verdict pending.
- **Item 2** — review-revision validated: build green, fmt/clippy clean, full
  test green, d4 bench 112306, new completed-loop Exact/Upper PV cases pass.
  **Blind review: APPROVED ("no further substantive issues").** Full-lib
  `cargo mutants --in-diff`: **19 caught / 7 timeouts (caught-via-hang) / 1
  unviable / 3 missed**. The 3 missed are `ply+1` arithmetic mutants at the
  single-reply (`2568`) + under-promo (`2662`) recursion sites — **pre-existing
  coverage gaps, NOT item-2 logic** (item 2 only appended `, is_pv` to those
  lines; `ply+1` semantics untouched), surfaced in-diff only because the lines
  are touched. The item-2 store-suppression guard (`qsearch_store_and_return`)
  is fully covered. → deferred to the standing full-suite mutation backstop
  (tooling-backlog). **Item 2 ready for SPRT.**
- **Item 5** code staged (`tune/m5f-item5-probegate`, `9bd8bd9`, WIP/unbuilt,
  no test yet): `qs_depth` frames-since-entry param; skip probe when
  `qs_depth >= QS_PROBE_MAX_DEPTH=4`.

## PAUSED (2) 2026-05-31 ~23:00 local (laptop lid close)

All four trees clean (0 uncommitted). State:

- **Items 5, 7, 8 CLOSED** since the first resume (5 on bench canary; 7 & 8
  analytically — see results table + Items-7&8 section). **Item 6 REVERTED**
  (−24.36 Elo). Rule added to `CLAUDE.md`: long-running work → ETA up front +
  hourly updates.
- **Item 2 SPRT KILLED @213 games** for the lid close: **43.2% (W-L-D
  47-76-90) ≈ −48 Elo — clearly negative** past the noise floor. On resume,
  either (a) re-run the SPRT cleanly for the logged verdict (expected REVERT),
  or (b) given the decisive 213-game partial, accept REVERT and skip the re-run.
  Branch `tune/m5f-item2-pvstore` (`54068fa`), review-APPROVED, mutants-clean —
  the *code* is sound; it just doesn't help (suppressing loose PV stores removes
  net-useful TT entries even with Exact kept).
- **Item 4** (`tune/m5f-item4-ttrepl`, `0df7f3a`) — review-APPROVED, 12/12
  mutants, bench-inert. **SPRT not yet run.** On resume: launch item-4 SPRT from
  `.worktrees/item4/scripts/sprt.sh` (baseline pre-built), seed e.g.
  `0xC1ABF15AE10DE004`.
- **Remaining on resume:** (1) finalize item-2 verdict (revert), (2) run item-4
  SPRT, (3) ship/revert per CI rule + combined-confirm only if >1 ships (likely
  0–1 ships → no combined-confirm needed), (4) consolidate docs (tuning-backlog
  update: items 2/5/6 rejected, 4 pending; ADR-0028 note if item 4 ships),
  (5) clean up campaign worktrees/branches.
- **Pending self-wakeup** (~23:58, fires on resume): per the prior standing
  preference ("I'll tell you when to continue"), on fire it should **report
  status only and await go-ahead**, NOT auto-relaunch SPRTs — the hourly-update
  rule applies to *actively running* work, and the campaign is paused here.
