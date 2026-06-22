# Overnight tuning campaign — 2026-06-03 (full backlog sweep, in order)

**Started 2026-06-03 ~02:05 CEST (overnight, unattended).** User prompt: "All
these, in order. Unattended overnight campaign." — referring to the full active
queue of `docs/tuning-backlog.md`, walked top-to-bottom.

This campaign goes **deeper** than the 2026-06-02 run, which already spent the
*easy* shots at the actionable items (margin-gated Path-A = item 3's first lever,
depth-adaptive aspiration = item 5's tier-2, and scoped-out C). Tonight attacks
the **previously-deferred / harder levers** of each item, and for the
precondition-gated and closed items, resolves their disposition **empirically**
(measure the gate) rather than citing stale figures.

## Baseline & methodology (shared)

- **Baseline tag = `M5.F.1`** (current production HEAD; `main` byte-identical;
  bench d4 `112_020` / d7 `1_354_640`).
- **Mixed-TC + virtual-clock + full QoS** per ELOH.D / ADR-0037:
  `SPRT_TC_SAMPLE='10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1'`, `SPRT_VIRTUAL_CLOCK=1`,
  `SPRT_LAUNCH_PREFIX=''`. Launched via `scripts/sprt.sh sprt M5.F.1`.
- 400-game cap; distinct seed per candidate.
- **Decision rule:** ship if SPRT crosses H1, or (at the 400-game cap with
  verdict=continue) if the pentanomial CI lower bound > 0 (rung-1-by-CI,
  ADR-0037 §9). Otherwise revert; production stays `M5.F.1`. Either way logged
  under `bench/sprt/`.
- **One SPRT at a time** (serial; resource discipline, workflow §12). **No
  builds / heavy CPU (texel runs, cargo test) during a live SPRT** — the
  2026-06-02 C-6 crash lesson.
- **elo1:** 10 for search-layer items (3, 5); 5 for the eval-term retune (7).
- **Review compression:** one blind final-review per code-changing candidate;
  the small search tweaks (3, 5) get no separate plan/test review loop. Item 7
  (new optimizer numerics) gets a fuller review (it is new numeric code, not a
  tweak). WAC+STS run only on a *shipping* candidate (post-SPRT, pre-commit), as
  diagnostics, never as a gate.
- **Realistic overnight reach: 3 serial SPRT slots** (~2–2.5 h each from ~02:40)
  → items 3 and 5 evaluated, item 7 implemented + launched (maybe not finished)
  by the user's ~08:00–09:00 review. Mirrors the 2026-06-02 reach.

## Per-item disposition (walked in backlog order)

### Item 1 — M5.I aspiration third tier → **DEFERRED (gate measured, not met)**
Precondition = median ID depth ≥ 14 at 20+0.2. **Measured 2026-06-03 02:15** via
`scripts/depth-probe.sh` (production binary, literal `go wtime 20000 winc 200`,
fresh clock per position, the 8 midgame FENs from `src/bench.rs`):
depths `10 11 11 11 12 13 15 16`, **median ≈ 11.5**. The two depth-15/16
outliers are simplifying positions; genuine midgame positions are 10–13. In a
real draining-clock game per-move depth is *lower* still. **Gate not met** (fresh
empirical confirmation of the stale "~depth 8-12" prior). No code change.

### Item 2 — M5.H2 lazy quiet sort → **DEFERRED (gate measured, not met)**
Three-part gate; the depth≥14 sub-gate (shared with item 1) fails ⇒ the item is
gated regardless, so the history≥4000/game sub-measurement is moot tonight. No
code change. (Revisit per the backlog when depth-reach clears 14 — most likely a
post-M12/NNUE event, or after a parallelism/NPS milestone.)

### Item 3 — M5.F qsearch-TT → **SPRT #1: qsearch-local-depth gate**
The margin-gate (2026-06-02 A(a)) failed; the one remaining untried M5.F.3-
recompose lever is the **qsearch-local-depth gate** (backlog: "a qsearch-local-
depth gate remains untried but is lower-prior now"). Thread a qsearch ply/local-
depth counter and suppress Path-A stand-pat `Lower` stores only on *deep* qsearch
frames (where the entry is most stale / lowest-value), keeping shallow-frame
stores (where M5.F.1's Exact entries co-exist). Hypothesis: the 40+0.4 collapse
of unconditional M5.F.3 came from suppressing *all* Path-A stores including the
shallow high-value ones; gating by local depth preserves those. Honest prior:
low (margin-gate already reinforced "substitutes, not complements"), but it is
the last principled lever and the user asked for all items. Implement + blind
final-review + bench re-pin + SPRT vs `M5.F.1` (elo1=10).

### Item 4 — M5.G singular extensions → **CLOSED (no untried lever)**
Items 4–8 all closed 0-ship in the 2026-06-01/02 campaign; items 2 (`SE_MARGIN`)
reverted-flat, 3 stale-already-shipped. The SE subsystem is at a local optimum
(extending more, pruning more, widening eligibility all regress/flat). No new
lever. No run.

### Item 5 — ML-tuned aspiration → **SPRT #2: tier-3 cheap delta-baseline**
Tier 2 (depth-adaptive narrowing) failed 2026-06-02 (−9.56, no depth-amplifying
trend). Tier 3 is the **cheap delta-baseline**:
`half_width = clamp(K·|score(d-1) − score(d-2)|, MIN, MAX)` — a *different*
mechanism (volatility-responsive **widening**, opposite direction from tier-2's
monotone narrowing), so not strictly dominated by tier-2's null. The backlog
gated tier 3 on tier 2 showing ≥+5 Elo headroom (it did not), but the mechanism
distinction justifies one informed shot per the "all items, in order" directive.
Hand-picked `K, MIN, MAX` (no SPSA harness). Implement + final-review + bench +
SPRT vs `M5.F.1` (elo1=10). Honest prior: low.

### Item 6 — M5.H2 SEE-split captures → **BLOCKED (prerequisite missing)**
Requires SEE infrastructure, which does not exist. Building SEE from scratch is a
milestone-scale feature (M12+ candidate per the backlog), not a tuning run — out
of scope for an overnight tuning campaign and inappropriate to land unattended.
No build. Documented as blocked-on-prerequisite.

### Item 7 — sign/monotonicity-constrained eval retune → **SPRT #3 (impl + run)**
The 2026-06-02 C scoping found: `Reg{l2_lambda, mono_lambda}` exist + wired but
hardcoded 0.0 / no CLI; **no sign-projection** exists; doing C *faithfully* needs
new constrained-optimizer numerics. Tonight: implement projected-gradient sign
clamps (penalty terms ≤ 0; passed/connected bonuses ≥ 0; rank-monotone via the
existing `mono_lambda`) in `src/texel/optimizer.rs` + expose `--l2-lambda` /
`--mono-lambda` / `--sign-project` flags in `cmd_tune`. Warm-start from shipped,
select constraint strength by held-out val-loss. **ABANDON before spending the
SPRT slot if constrained val-loss is materially worse than shipped** (the
backlog's documented abandon criterion — the corpus genuinely wants the
unconstrained shapes). Else `apply` + fuller blind review + bench re-pin + SPRT
vs `M5.F.1` (elo1=5). Honest prior: low (sign-constraining fights the data that
produced `ISO_MG=+5`); the *durable* deliverable is the sign-projection numerics,
which Arm-B (item 8) also needs.

#### Item 7 — implementation design (locked 2026-06-03 ~05:10, from the recon map)
- **optimizer.rs:323** — insert sign-projection clamp after the Adam param update, gated on a new `cfg.sign_project` bool: `if s>0 { w[i]=w[i].max(0.0) } else if s<0 { w[i]=w[i].min(0.0) }` where `s = expected_sign(i)`.
- **`expected_sign(idx)` (new, layout.rs)** — explicit, conservative table:
  - **−1 (penalty, ≤0):** `0..6` (ISO/DBL/BWD) + `173..177` (KS open/semi-open-file).
  - **+1 (bonus, ≥0):** `6..18` (Conn) + `18..29` (PASSED_MG+EG ranks) + `37..169` (all mobility) + `169..173` (shield) + `177..189` (outpost) + `189..193` (rook-file).
  - **0 (UNCONSTRAINED):** `29..35` (PASSED_FREE_EG_DELTA) + `35..37` (KDIST_OWN/ENEMY) — structurally ambiguous sign; do not impose a prior.
  - The documented violations (ISO_MG=+5, negative early-rank PASSED_MG) are in the −1/+1 sets ⇒ this item's defining target IS constrained.
- **Flags (texel-tune.rs cmd_tune):** `--l2-lambda` (default 0; NOT used for item 7 — ridge pulls toward the wrong-signed shipped value), `--mono-lambda` (default 0; use a modest value, e.g. 5e-3, on the 12 existing monotone seqs), `--sign-project` (bool; ON for item 7).
- **Cache:** `bench/tune-cache.bin` absent → build once via `texel-tune cache --lanes bench/corpus/{ccrl,lichess,selfplay-onbook,selfplay-offbook} --out bench/tune-cache.bin` (~2–10 min, heavy — between SPRTs only).
- **Abandon gate:** baseline shipped val-loss via `tune --max-iter 0`; run constrained `tune --sign-project --mono-lambda 5e-3`; if constrained val-loss > ~1.02× shipped ⇒ abandon (corpus wants the unconstrained shapes), do NOT spend the SPRT slot. Else `apply` → review → bench re-pin → SPRT #3 vs `M5.F.1` (elo1=5, seed `…F0007`).

### Item 8 — Arm-B PST co-tune → **GATE ASSESSMENT (no ship tonight)**
`text-tune sensitivity` exists → run it to assess go/no-go gate condition 2
(material residual double-count / M6.B–F terms pinned near zero). But the
optimizer's parameter vector **excludes the 768 PST entries** (`N_CORE = 193`,
deferred-terms only) — a real co-tune needs new PST feature-extraction in
`layout.rs`/`features.rs`, a multi-session build, plus the higher SPRT bar
(must beat A clearly *and* justify losing the vendored-PeSTO diff oracle). Not an
overnight deliverable. Tonight: run the sensitivity diagnostic, record the gate
verdict, and (only if SPRT slots finish early) scaffold the PST-extraction. The
sign-projection from item 7 is shared infra toward this.

### Item 9 — king-safety attacker S-curve → **CLOSED NEGATIVE (no re-run)**
Removed at M6.K (both probes regressed; double-count vs PeSTO king PST confirmed
at half magnitude, optimum g≈0). No new hypothesis ⇒ no re-run. Deferred to
NNUE (M12), which re-learns king safety natively.

## Sequencing & ETA

Serial: depth-probe (done) → item 3 SPRT → item 5 SPRT → item 7 (impl + retune +
SPRT). Candidate N+1 code is *edited* (not built) during SPRT N to pipeline;
build+test in the between-SPRT gap. Items 4/6/9 are documentation; item 8 is the
sensitivity diagnostic run in a between-SPRT gap. Commits land on `main`,
**not pushed** (left for morning review). Patches under `bench/sprt/patches/`.
Hourly progress + refreshed ETA per CLAUDE.md.

## Results log

| Item | Candidate | Seed | Verdict | Δ Elo [CI] | Decision |
|---|---|---|---|---|---|
| 1 | — (depth gate measured) | — | — | — | **DEFERRED** — median depth ≈11.5 < 14 |
| 2 | — (gate inherits item 1) | — | — | — | **DEFERRED** — depth sub-gate fails |
| 3 | qsearch-local-depth gate | `…F0003` | continue@400 (llr=0.38) | **+11.30 [−13.86, +36.57]** | **NO SHIP** — flat net; fixes 40+0.4 (W48-L18) but regresses extremes. `bench/sprt/2026-06-03-item3-*.md` |
| 4 | — | — | — | — | **CLOSED** — no untried lever |
| 5 | delta-baseline aspiration | `…F0005`+`…F0015` | 2-seed combined | seed1 **+20.00 [−1.71,+41.87]**, seed2 **+6.08 [−19.58,+31.80]**, **combined +13.03 [−3.78,+29.91]** (800g) | **NO SHIP** (strict CI-lower<0); rung-2 by ADR-0037 §9. **20+0.2 robustly +** both seeds (+23/+20) → TC/depth-gated + SPSA follow-up. Night's best lead. `bench/sprt/2026-06-03-item5-*.md` |
| 6 | — | — | — | — | **BLOCKED** — needs SEE infra |
| 7 | sign/mono eval retune | — | numerics landed; retune deferred | — | **NUMERICS LANDED** (sign-projection + `--l2-lambda`/`--mono-lambda`/`--sign-project`, tuner-only, tests green). Constrained retune **val-loss-neutral** (0.14426 vs 0.14371, +0.38%, passes 2% gate) — but first table was over-broad (mangled centered mobility); table corrected (mobility/conn unconstrained). Retune+SPRT **deferred** to the future Arm-B/sign-mono campaign (user decision). |
| 8 | sensitivity diagnostic (Arm-B gate) | — | — | — | **NO-GO / deprioritize Arm-B** — terms NOT pinned (mobility/passed/rook-file/shield are the MOST sensitive); only sparse dead cells (data-coverage, not double-count). Gate cond. 2 unmet ⇒ frozen-PST bias small. `bench/tune/2026-06-03-sensitivity-shipped.json` |
| 9 | — | — | — | — | **CLOSED NEGATIVE** (M6.K) |

## Campaign close (2026-06-03 ~09:40 CEST)

**0 ships. Production unchanged: `M5.F.1`** (bench d4 `112020` / d7 `1354640`,
byte-identical — items 3 and 5 reverted; item 7 is tuner-only; items 1/2/4/6/8/9
no engine touch). Three campaigns running (2026-06-01/02/03) now all 0-ship: the
search layer is firmly at a **local optimum** at the current strength/TC.

**SPRT budget spent:** 3 mixed-TC runs (item 3, item 5 seed 1, item 5 seed 2)
+ a depth-reach measurement + a texel sensitivity diagnostic + a constrained
texel retune. Item-7 numerics landed (durable tuner infra), retune deferred.

**The one real lead — item 5 (delta-baseline aspiration):** combined +13.03
[−3.78, +29.91] over 800 games, robustly positive at 20+0.2 across both seeds.
No-ship by the strict rule (rung-2 by ADR-0037 §9). **Top follow-up:** a
TC/depth-gated delta-baseline + SPSA(K/MIN/MAX) micro-campaign — promoted to
`docs/tuning-backlog.md`.

**Durable artifacts:** sign-projection optimizer numerics + 3 new CLI flags
(`src/texel/`, `src/bin/texel-tune.rs`); `scripts/depth-probe.sh` (reusable
depth-precondition check); `bench/tune/2026-06-03-sensitivity-shipped.json`
(Arm-B gate evidence); patches under `bench/sprt/patches/`. Commits land on
`main`, **not pushed** (left for review). The 2.6 GB `bench/tune-cache.bin` was
removed (rebuildable via `texel-tune cache` in ~minutes).
