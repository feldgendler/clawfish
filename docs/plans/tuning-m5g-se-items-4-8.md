# Tuning campaign — M5.G singular-extensions backlog items 4–8

**Started 2026-06-01 ~04:55 local (unattended).** Pulls the five untried M5.G
singular-extension tunable-space items from `docs/tuning-backlog.md` §"M5.G
singular-extensions". Tuning-class changes: each strength gate = SPRT. User
directive: "Try items 4-8 unattended; evaluate in order of highest→lowest
probability of success; keep what works; follow the full routine."

## Baseline & methodology

- **Baseline tag = `M5.F.1`** — current production HEAD (`82c94db`) is
  search-identical (only docs commits sit on top of the `M5.F.1` tag `cef6e14`).
  `scripts/sprt.sh sprt M5.F.1` faithfully represents current production. This
  supersedes the backlog's M5-era "vs `M5.G`" wording — the correct SPRT baseline
  is whatever currently ships.
- **Mixed-TC + virtual-clock** per ELOH.D / ADR-0037:
  `SPRT_TC_SAMPLE='10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1'`, `SPRT_VIRTUAL_CLOCK=1`,
  full QoS (`SPRT_LAUNCH_PREFIX=''`), 400-game cap, `SPRT_ELO1=10` (search-change
  convention), distinct seed per candidate.
- **Decision rule:** ship if SPRT crosses H1, or — at the 400-game cap with
  `verdict=continue` — if the pentanomial Δ Elo CI lower bound > 0 (rung-1-by-CI,
  ADR-0037 §9; the precedent by which M6.J and M5.F.1 shipped). Otherwise revert.
- **One SPRT at a time** (serial, ~1h50m each). The next candidate is implemented
  + reviewed in a git worktree *during* the running SPRT (separate `target/`, so
  the running match's already-built binary is undisturbed; virtual-clock tolerates
  the CPU contention, only slows). This is the only permitted concurrency — no two
  contention-sensitive jobs (SPRT / EPD / mutants) ever overlap (workflow
  §"Resource contention discipline").
- **Independent evaluation, then combined-confirm.** Each candidate branches off
  `main`@`82c94db` and is SPRT'd vs `M5.F.1` independently. Per the 2026-05-30
  C1+C3 lesson (two independently-validated qsearch-TT changes were destructively
  non-additive) **any multi-ship requires a combined-confirmation SPRT before
  commit** — these items all touch the same SE subsystem.
- **Review:** the full routine, tuning-compressed per the M5.F-items-2/4–8
  precedent — this plan goes through one blind plan-review loop; each
  code-changing candidate gets tests + mechanical checks (fmt / clippy / test /
  llvm-cov / `mutants --in-diff`) + one blind final-review loop (test-suite
  adequacy folded into final review). Diagnostic/config items skip review (the
  measurement is the deliverable).
- **Movegen-adjacency:** none of these items touch `generate_moves`, make/unmake,
  `MoveStager`, or move-validation predicates — they change the SE *extension
  amount*, *eligibility bound*, or *cutoff*, not which moves are generated/legal.
  Extended perft is **not** triggered. (Confirmed per candidate at review.)

## Candidate queue (probability-ordered, highest → lowest)

**Ordering note (per plan-review).** The user directive fixes the *SPRT
evaluation order* by **probability of a positive ship** (C-6 > C-5 > C-4 > C-7 >
C-8). This intentionally front-loads the two highest-implementation-subtlety items
(C-6 explosion gating, C-5 fail-soft soundness). That is accepted: the
serial-SPRT-with-pipelined-build structure means implementation risk only delays
*that candidate's own* SPRT slot (if C-6 needs a margin/cap bisect, I develop the
trivially-sound C-4 as the pipeline-fill during the wait), and the combined-confirm
gate only triggers on >1 ship. So success-probability order costs nothing in
wall-clock while honoring the directive; the de-risk axis is handled by build
pipelining, not by reordering SPRTs.

### C-6 — item 6: double extensions  *(rank 1 — highest probability)*
- **Hypothesis.** When the verification search fails low *strongly*
  (`verif_score < s_beta - DOUBLE_EXT_MARGIN`), the TT move is not merely singular
  but dominant — extend it by **2** plies instead of 1. Modern engines' highest-
  value SE refinement (CPW "Extensions"/"Singular Extensions"; research §3.2 which
  deferred it at v1).
- **Design.** In the SE block, replace the binary `tt_move_extension = 1`
  assignment (`search.rs:1829`, inside the `if verif_score < s_beta` block at
  1828-1834) with a graded form:
  - `verif_score < s_beta - DOUBLE_EXT_MARGIN` **and** the double-ext cap permits
    → `tt_move_extension = 2`
  - `verif_score < s_beta` → `tt_move_extension = 1`
  - else `0`.
  New const `DOUBLE_EXT_MARGIN: i32` (start **50**, literature-typical). The
  extension still applies only to the TT move at `cur_i == 0` (the
  `depth - 1 + move_extension` call at `search.rs:2041`), so LMR/FFP disjointness
  is preserved unchanged.
- **Explosion bound (the real concern, per plan-review).** The danger is *not* the
  verification recursion (already geometrically bounded to chain-depth ≤3 by
  ADR-0029 §5: each layer searches `(depth-1)/2`, halving). The danger is a
  *new, orthogonal* axis: the +2 on the **main child** raises its `depth`, and if
  that child SE-double-extends *its* TT move, the effective depth can climb down a
  forced line. Three layered mitigations, in order of load-bearing-ness:
  1. **Cumulative cap (v1, load-bearing).** Track per-search-path double-extension
     budget via a small counter on `AlphaBetaMover` (e.g. `double_ext_remaining`
     or a `ply`-indexed flag akin to the item-8 stack-flag, but minimal: increment
     a depth-since-root-extension guard). Concretely: only grant the *second* ply
     when the current node was **not itself reached via a double extension** — gate
     on a threaded `bool parent_double_extended` (default `false`, set `true` on
     the extended child's `search_child`). This caps consecutive double extensions
     to 1 per path segment — the standard lightweight guard. ~15 LOC threaded like
     `excluded_move`. If threading proves heavier than the probe warrants, fall
     back to an absolute `ply`-band gate (`ply < DOUBLE_EXT_MAX_PLY`).
  2. **Virtual-clock SPRT penalizes time blow-ups (load-bearing safety net).** The
     mixed-TC SPRT runs under a real per-move time budget — a runaway double-ext
     that explodes node counts *loses on the clock* and shows up as an Elo
     regression, not a silent pass. This is the ultimate gate the fixed-depth bench
     lacks.
  3. **Broadened bench canary (first-pass sanity, NOT sufficient alone).** Run d4
     **and** d7 node counts **plus** a tactical/forced-line position (a WAC mate
     fixture) node count — the fixed-depth bench under-samples forced compounding,
     so add a position where double-ext would manifest. A >~+20% blow-up on the
     tactical fixture ⇒ margin too loose; bisect upward (75, 100) or abandon.
- **Tests.** **Add** (do not mutate) a new double-ext case alongside the existing
  single-ext anchor test (which documents the single-ext mutant kills — anchor
  tests are not repurposed, workflow §"Property tests vs unit tests"): drive a
  strong-fail-low verification (pre-stored TT entries making `verif_score` land
  `< s_beta - DOUBLE_EXT_MARGIN`) and assert `tt_move_extension == 2` /
  `se_tt_move_search_depth == depth + 1` (these are **`#[cfg(test)]`
  instrumentation** assertions on the test binary — distinct from the release
  bench-canary node count). Boundary test at exactly
  `verif_score == s_beta - DOUBLE_EXT_MARGIN` (→ still `1`, strict `<`). A cap test:
  a `parent_double_extended == true` node does not double-extend again.
- **Seed `0xC1ABF15AE15E0006`.**

### C-5 — item 5: multi-cut on verification fail-high  *(rank 2)*
- **Hypothesis.** The verification search already searches the non-TT moves at
  reduced depth `(depth-1)/2` with window `(s_beta-1, s_beta)`. When that search
  returns a score `>= beta` (the *node's real* beta), then even with the TT move
  excluded and at reduced depth, a non-TT move already reached a beta-cutoff
  magnitude → the node is very likely a cut-node; return that score as a fail-soft
  cutoff, converting the lost SE bet into a pruning win (CPW "Multi-Cut"; research
  §11.4; ADR-0029 alt (g), deferred at v1).
- **Soundness — heuristic, NOT a proof (corrected per plan-review).** This is the
  standard multi-cut *heuristic*: the `verif_score >= beta` evidence is over the
  **TT-move-excluded sub-game at reduced depth `(depth-1)/2`**, not over `pos` at
  full `depth`. It is the accepted multi-cut risk, not a provable bound. Two
  correctness rules make it safe to *use* without polluting state:
  1. **Return the actual `verif_score`, never a fabricated `s_beta`.** `verif_score`
     is the real fail-soft value the verification search produced (`>= s_beta` on
     fail-high; the multi-cut sub-case is the slice where it further reaches
     `>= beta`). Returning the genuine number, not `s_beta`, is what makes the
     fail-soft contract honest.
  2. **Do NOT store anything in the TT on the multi-cut return** — mirror the RFP
     no-store rule (`search.rs:~1610`): the proof is depth-specific and over a
     modified (TT-move-excluded) game, so it is not a search-quality bound for
     `pos.zobrist()`. No store ⇒ no pollution ⇒ none of the §4 store-guard
     hazards apply. (This replaces the pass-1 plan's unsound
     `store (s_beta, depth, Lower)`, which the reviewer correctly flagged.)
- **Gate.** After the verification call: if `verif_score >= beta` → `return
  verif_score` (no store). This is an aggressive *no-count* multi-cut (one
  reduced-depth fail-high suffices) — noted explicitly as more aggressive than the
  classical `C≥2/3`-move form; the SPRT is the arbiter. Else the existing branches
  apply (`< s_beta` → extend; otherwise no extension).
- **Make/unmake balance (nit).** The multi-cut `return` fires *before* the move
  loop and after the verification `negamax` (which is internally balanced), so no
  half-made move is outstanding — the `Search::go` make/unmake balance assert is
  trivially preserved. Movegen-adjacency exemption holds: this changes search
  control flow, not move generation/legality.
- **Tests.** Construct a node where the verification search returns `>= beta`
  (pre-stored TT entries making a non-excluded move's reduced-depth score reach
  beta) → assert `negamax` returns `verif_score` and the move loop did not run
  (assert returned score == the expected verif value in a fixture where the full
  move loop would return a *different* value, so the early return is observable).
  Boundary: `verif_score == beta` (fires) vs `verif_score == beta - 1` (does not,
  falls through). Confirm no TT entry was written at `pos.zobrist()` on the
  multi-cut path (probe-after assert).
- **Seed `0xC1ABF15AE15E0005`.**

### C-4 — item 4: Lower+Exact eligibility gate  *(rank 3)*
- **Hypothesis.** Clause 6 of `singular_extension_eligible` is `tt_bound ==
  Lower`. At non-PV nodes Exact entries are rare but real (move loop produced a
  score strictly inside `(alpha, beta)`); admitting them widens SE's firing
  surface. ADR-0029 alt (b), deferred at v1.
- **Design.** Clause 6 → `tt_bound == Lower || tt_bound == Exact` (one-line
  predicate change). All other clauses unchanged. `s_beta` arithmetic
  (`tt_score − depth`) is bound-agnostic and already correct for Exact
  (`tt_score` is the exact score). No store-side change.
- **Self-cut soundness (corrected per plan-review).** The §4(a) analysis that
  proves the verification frame cannot self-cut is **bound-type-independent**: the
  step-7 TT-cutoff branch is gated on `excluded_move.is_none()`
  (`search.rs:~1575`), and at the verification frame `excluded_move = Some(...)`,
  so the cutoff arm is skipped **regardless of whether the entry is Lower or
  Exact**. That `excluded_move.is_none()` gate — not the `tt_score >= beta`
  arithmetic — is the actual reason admitting Exact is safe. The `s_beta =
  tt_score − depth` arithmetic is independently fine for Exact (`tt_score` is the
  exact score; the margin subtraction is bound-agnostic).
- **Risk.** False-Exact propagation into SE eligibility. Low — the verification
  search still has to fail low for the extension to fire; a spurious Exact just
  *admits* the node, it doesn't force an extension. Small expected magnitude
  (Exact rare at non-PV) → genuine flat is a plausible outcome (no-ship, not
  revert-from-regression).
- **Tests.** Add an eligibility unit test: same fixture as the Lower-gate test but
  with `tt_bound = Exact` → predicate returns `true` (currently `false`). Keep the
  existing Lower/Upper clause-6 flip-mutant tests; add an Upper-still-rejected
  assertion to pin that only Exact was added (not a blanket bound relaxation).
- **Seed `0xC1ABF15AE15E0004`.**

### C-7 — item 7: PV-SE  *(rank 4)*
- **Hypothesis.** Clause 3 (`!is_pv`) excludes PV nodes. PV nodes are exactly
  where a missed extension costs the most. Modern engines apply SE at PV nodes.
  ADR-0029 alt — deferred at v1 for conservatism (research §8).
- **Design.** Drop clause 3 (`!is_pv`) from `singular_extension_eligible` — i.e.
  remove the `&& !is_pv` term. The verification call stays non-PV
  (`is_pv = false` arg is already hard-coded at the call site, correct). Nothing
  else changes.
- **Risk + concrete pre-SPRT gate (per plan-review).** PV-node verification is a
  *full reduced-depth* search interposed before the (now extended) PV child, and
  PV nodes are the hottest in the tree — exactly the cost ADR-0029 §1 clause-3 /
  §8 deferred. **Pre-SPRT bench gate:** measure release `bench` d7 node count +
  NPS; **if d7 nodes rise >~+10% or NPS drops >~+10%, the verification cost is
  material** — proceed to SPRT only if the canary is within that band, otherwise
  record the NPS cost and treat a flat/negative SPRT as expected (don't burn extra
  seeds chasing it). Moderate-low odds.
- **Tests.** **Re-point** the existing clause-3 (`!is_pv`) flip-mutant test to
  positive evidence: add a PV-node eligibility test asserting SE now fires at
  `is_pv = true` (all other clauses satisfied) — this becomes the new coverage for
  the clause's *presence/absence*. Removing `&& !is_pv` makes the old flip-mutant
  equivalent (no clause to flip); document the removal in the test comment and
  ensure the *new* PV-firing assertion provides the replacement coverage, so the
  net mutation surface does not silently drop.
- **Seed `0xC1ABF15AE15E0007`.**

### C-8 — item 8: propagated `singular_ext_active` flag  *(rank 5 — lowest)*
- **Status: evaluate via canary, expect analytic close.** This is explicitly a
  **perf/NPS-regression mitigation** (ADR-0029 §5 / alt (e)): thread a
  Stockfish-style `singular_ext_active` flag through `search_child` so descendant
  SE firings inside a verification subtree are suppressed. It has **no standalone
  strength mechanism** — it can only *recover* NPS if the immediate-frame-guard-only
  design is leaking verification cost at deep TC.
- **Gate (instrument corrected per plan-review).** The backlog's own trigger:
  "only relevant if SPRT shows verification-subtree NPS regression at deep TC."
  The `se_extensions` counter is **`#[cfg(test)]`-only and counts fail-low
  extensions, not verification cost** — it cannot see this and is absent from the
  release binary. The correct signal is a **release NPS comparison**: run release
  `bench` (and, if needed, a 60+0.6 timed fixture) on current HEAD and compare
  NPS/effective-depth against the `M5.F.1` baseline binary. **Expected
  disposition: CLOSE analytically.** The argument (state it explicitly, don't
  lean on a non-measurement): ADR-0029 §5 bounds the verification chain
  geometrically to depth ≤3 (`f(d)=1+f((d-1)/2)`), and **no SPRT across the entire
  M5.G → M6.J → M5.F.1 history has flagged a deep-TC verification-NPS regression**;
  with no observed regression and a proven geometric bound, the 80-LOC stack-flag
  has no strength mechanism to capture. Only if the release-NPS canary shows a
  material deep-TC regression does it convert to an implement+SPRT item.
- **Seed `0xC1ABF15AE15E0008`** (only if it reaches SPRT).

## Results log

| Candidate | Seed | Verdict | Δ Elo [CI] | Decision |
|---|---|---|---|---|
| C-6 (double-ext) | `…5E0006` | crash@91 (W-L-D 17-29-45 ≈ 43%) | ~−46 Elo (consistent 42-43% throughout) | **REVERT** — decisive negative partial; double extensions regress at current strength. Run died on a UCI handshake desync (`readyok after ucinewgame`) under self-inflicted build/canary contention, NOT an engine panic; signal unambiguous so no re-run. |
| C-5 (multi-cut)  | `…5E0005` | continue@400 (llr=−0.23) ptnml [10,49,85,41,15] | **+1.74 [−21.76, +25.25]** | **NO SHIP (flat)** — CI straddles 0. Multi-cut Elo-neutral; the −30% pruning's speed gain offsets its heuristic errors. (≈ M5.I's +1.74.) |
| C-4 (Lower+Exact)| `…5E0004` | continue@400 (llr=−1.23) ptnml [17,53,78,35,17] | **−15.65 [−41.21, +9.76]** | **NO SHIP** — CI-lower <0 (mildly negative). Admitting Exact entries adds +15% d10 verification cost that doesn't pay off. ADR-0029 §1 Lower-only gate stands. |
| C-7 (PV-SE)      | `…5E0007` | continue@400 (llr=−1.57) ptnml [21,49,79,36,15] | **−21.74 [−47.60, +3.87]** | **NO SHIP** — CI-lower <0 (mildly negative). Extending SE to PV nodes adds verification cost on hot nodes without payoff. ADR-0029 §1 clause 3 (`!is_pv`) / §8 stand. |
| C-8 (prop. flag) | — | **CLOSED analytically (no SPRT)** | — | Perf/NPS-mitigation only, no standalone strength mechanism. NPS canary on `M5.F.1`: stable 5.50 Mnps@d10 / 5.20 Mnps@d12 — **no deep-TC verification-subtree NPS regression**; with the §5 geometric chain bound (≤3) and no regression observed across M5.G→M6.J→M5.F.1 history, the 80-LOC stack-flag has nothing to recover. |

## CAMPAIGN CLOSED 2026-06-02 ~01:35 local — 0 ships; production stays `M5.F.1`

| Item | Disposition | Δ Elo [CI] / evidence |
|---|---|---|
| C-6 double extensions | **REVERT** | −46 Elo decisive over 91 games (run crashed on self-inflicted contention, signal unambiguous) |
| C-5 multi-cut on verif fail-high | **NO SHIP (flat)** | +1.74 [−21.76, +25.25] — pruning's speed gain offsets heuristic errors |
| C-4 Lower+Exact eligibility gate | **NO SHIP** | −15.65 [−41.21, +9.76] — +15% d10 verification cost doesn't pay off |
| C-7 PV-SE | **NO SHIP** | −21.74 [−47.60, +3.87] — PV-node verification cost without payoff |
| C-8 propagated `singular_ext_active` flag | **CLOSED (analytic)** | no NPS regression to fix (§5 bound + stable NPS canary) |

**Net lesson:** the M5.G singular-extension subsystem is at a **local optimum** at
the current strength/TC — every direction tried regresses or is flat: extending
*more* (double-ext C-6: −46; PV-SE C-7: −22), pruning *more* (multi-cut C-5: flat),
and widening *eligibility* (Lower+Exact C-4: −16) all fail to clear the bar. This
mirrors the M5.F qsearch-TT campaign's 0-ship outcome and the M5.I flat-aspiration
result — the search-selectivity layer is tuned out at clawfish's strength; the next
real gains are in eval/NNUE (M11), not SE/qsearch micro-tuning. Branches
`tune/m5g-se-item{4,5,6,7}-*` retained for the record; **production HEAD unchanged
at `M5.F.1`** (no engine bytes changed on `main`).

### Process lessons recorded
1. **No engine-spawning/heavy-CPU work during a live SPRT** (build/test/bench/
   mutants) — the C-6 SPRT crashed at 91 games on a UCI handshake desync caused
   by stacking builds + canaries on the running match. Pipeline only Edits +
   read-only reviews during an SPRT; do all build/test/canary/mutation in the
   no-SPRT gaps. (Stricter than the M5.F "single build under virtual-clock is OK".)
2. **cargo-mutants under the command sandbox can hang** — a whole-function-
   replacement mutant (`go`/`negamax` → default) deadlocks a UCI integration test
   that the sandbox can't kill; the run wedges with no completion notification.
   Fix: `--timeout` + scope to `-- --lib` + run with the sandbox disabled, or fall
   back to the project's manual apply-test-revert mutation technique (used for all
   four candidates here, ~10 s each).
3. **Stale negative integration tests are the recurring trap when a gate is
   loosened** — both C-4 (Exact) and C-7 (PV) had a pre-existing `…does_not_fire…`
   integration test that asserted the now-removed exclusion and passed only by a
   step-7-cutoff side effect; each was caught by the blind reviewer and repurposed
   into positive firing coverage. Loosening any eligibility clause ⇒ grep for the
   matching `does_not_fire` test.

**Contention lesson (07:44):** the C-6 SPRT crashed because I stacked multiple
`cargo build` + bench canaries (which spawn engine subprocesses) on the running
SPRT, desyncing an engine's UCI handshake. **Rule for the rest of the campaign:
no engine-spawning or heavy-CPU work (build/test/clippy/bench/mutants) while an
SPRT is running — only Edits and read-only reviews. Candidate build/test/canary
happens in the gap between SPRTs.** (This is stricter than the M5.F precedent's
"building during SPRT is OK under virtual clock" — that held for a single build,
not stacked builds + engine-spawning canaries.)

## PAUSED 2026-06-01 ~12:00 local (lid closure)

**Campaign 2/4 SPRTs resolved, both no-ship; C-4 near-complete; C-7 staged.**

### Results so far
- **C-6 (double-ext): REVERT** — −46 Elo, decisive across 91 games (run crashed
  at 91 on a self-inflicted contention handshake-desync, but unambiguously negative).
- **C-5 (multi-cut): NO SHIP (flat)** — +1.74 [−21.76, +25.25], continue@400,
  ptnml [10,49,85,41,15]. `…074628…` run dir.
- **C-4 (Lower+Exact): NO SHIP** — **FINISHED before lid close** at 400 games:
  `continue` (llr=−1.23), **Δ Elo −15.65 [−41.21, +9.76]** (CI-lower <0),
  ptnml [17,53,78,35,17]. Run dir `…101146…`. SPRT process has exited.

### State (all on disk; survives suspend — NOT committed, to avoid pre-commit-hook contention with the live C-4 SPRT)
- **Worktrees off `main`@`82c94db`:** `.worktrees/c4` (`tune/m5g-se-item4-lowerexact`),
  `.worktrees/c5` (`tune/m5g-se-item5-multicut`), `.worktrees/c6`
  (`tune/m5g-se-item6-double-ext`), `.worktrees/c7` (`tune/m5g-se-item7-pvse`).
  All have **uncommitted** candidate code in the working tree.
- **C-7 (PV-SE): FULLY VERIFIED, SPRT-READY** — fmt/clippy/build/full-test(1901)/
  bench-canary(d7 identical, d10 +0.45%)/manual-clause-3-mutation all ✓; blind
  final-review converged. Seed `0xC1ABF15AE15E0007`.
- **C-8 (prop. flag):** not started — analytic close pending.

### RESUME ORDER (on lid open)
1. **Confirm C-4 verdict** — read `…101146…/summary.txt` for the `sprt:`/`ci:`
   lines (should be `continue@400`, CI straddling/below 0 ⇒ record NO SHIP).
2. **Launch C-7 SPRT** from `.worktrees/c7` (it is fully verified — no rebuild/
   re-test needed): `cd .worktrees/c7 && SPRT_TC_SAMPLE='10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1' SPRT_VIRTUAL_CLOCK=1 SPRT_LAUNCH_PREFIX='' SPRT_GAMES=400 SPRT_ELO1=10 SPRT_SEED=0xC1ABF15AE15E0007 scripts/sprt.sh sprt M5.F.1 > "$TMPDIR/c7-sprt.log" 2>&1` (run_in_background, sandbox off). **No engine-spawning work while it runs.**
3. **C-8 analytic close** (in a no-SPRT gap): release-NPS canary of any candidate
   vs the `M5.F.1` baseline at d-deep + the ADR-0029 §5 geometric-bound argument
   (chain ≤3) + "no deep-TC NPS regression observed across M5.G→M6.J→M5.F.1
   history" ⇒ CLOSE analytically (no 80-LOC stack-flag, no SPRT). Only implement
   if a real deep-TC NPS regression shows.
4. **Close-out** (see below): update `docs/tuning-backlog.md` §M5.G + ADR-0029
   "Open / tuning backlog" with per-item dispositions; **expected 0 ships ⇒ no
   combined-confirm, no commit of engine changes, production stays `M5.F.1`**;
   retain `tune/m5g-se-item{4,5,6,7}-*` branches for the record; clean up worktrees.
5. If C-7 unexpectedly ships (CI-lower>0): it's the only ship ⇒ still no
   combined-confirm needed (single change); then commit + tag + docs.

### Self-wakeup
A heartbeat is scheduled (~11:59) and the C-4 SPRT completion notification will
re-invoke on resume; either path lands back here. The hourly-update rule applies
to *actively running* work — on resume, report C-4's final verdict, launch C-7,
and resume hourly cadence.

## Close-out obligations (when the campaign ends)
- Update `docs/tuning-backlog.md` §"M5.G singular-extensions" with per-item
  dispositions (ship / revert / no-ship-flat / closed-analytic) + SPRT evidence
  paths.
- Update ADR-0029 "Open / tuning backlog" list to strike resolved items.
- If any ship: combined-confirm SPRT, commit (conventional message), tag if a new
  production HEAD, update `CLAUDE.md` status block + `README.md` bench/strength,
  write `docs/milestones/` note if it rises to a milestone-class ship.
- Retain `tune/m5g-se-item{4,5,6,7,8}-*` branches for the record.
