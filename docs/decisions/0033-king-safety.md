# ADR-0033 — King-safety evaluation (zone attackers + pawn shield + open-file), shipped score-neutral

**Status:** Accepted (M6.E ships the king-safety infrastructure **wired but with
every weight zeroed** — score-neutral vs `M6.D`, the entire weight set deferred
to the M6.F joint-Texel reshape; the M6.C/M6.D inert-landing disposition. See
Decision §8.).

## Context

M6.A (ADR-0031) established the tapered `(static_mg_white, static_eg_white,
raw_phase)` representation. M6.B (ADR-0032) added the pawn hash + pawn-structure
infra; M6.C added the passed-pawn term (score-neutral); M6.D added the
piece-mobility term (score-neutral). M6.E is the **king-safety** phase: a
white-perspective `(mg, eg)` term combining (i) a nonlinear king-zone
attacker-weight S-curve, (ii) a pawn-shield bonus for the castled king, and
(iii) an open/semi-open-file-toward-the-king penalty.

The roadmap §M6 ADR list explicitly binds an ADR on M6.E (unlike M6.C/M6.D,
whose semantics were committed in roadmap rows). Design space + transfer-risk
analysis: `docs/research/m6-king-safety.md`. Plan + test surface:
`docs/plans/m6.e.md`.

The dominant design driver is the **three-phase "co-calibrated-elsewhere ≠
transfers here" law** (M6.B ISO×CONN −197.94; M6.C all-three −21.74 + co-scale
fails; M6.D all-four −131.62 + co-scale *worsens* to −220.18 — a scale-invariant
structural mismatch with our PeSTO PSTs, confirmed three times and strengthening
each time). The research note (§7/§8) finds king safety the **highest** transfer
risk of any M6 term: PeSTO's MG king PST already prices ~30–50 cp of
castled-king safety (the dominant double-count axis — structurally the M6.D
PST-double-count finding, stronger), king safety is universally the noisiest
eval term to SPRT (opening-pool-dominated variance), and its components are
*coupled by design* (no clean interaction-immune subset à la M6.B CONN-only).

## Decision

### 1. King-zone — 3×3 ring + forward wedge

`king_zone(side) = ring | (shift_toward_enemy(ring) & !Bitboard::from_square(
ksq))` where `ring = movegen::masks::king_attacks(king_square(side))`, `ksq =
king_square(side)`, and `shift_toward_enemy` is `Bitboard::shift_north` for
White / `shift_south` for Black. This yields the 8 ring squares plus exactly
the 3 squares one rank toward the enemy beyond the ring's forward edge (the
Stockfish/Glaurung/CPW canonical "squares the enemy king can move to + three
forward" shape; research §1/§6 — 11 squares for a centralized king, fewer on
edge/corner/back-rank). The `& !Bitboard::from_square(ksq)` term is
**load-bearing**: for a king not on its back rank, a ring square sits directly
behind the king, so `shift_toward_enemy(ring)` would re-introduce the king's
own square — masking it out keeps the king square **excluded** (matching the
cited CPW definition; for a back-rank/corner king no ring square is behind it
so the mask is a no-op there). Worked: White Kd4 → ring(8) + {c6,d6,e6} = 11,
d4 excluded; White Kg1 → 8 (no rank-0 ring squares); White Ka1 → 5
(research §1 edge example). Edge/corner clamping is automatic (vertical shift
loses off-board squares; `king_attacks` is already edge-masked). No bespoke
table.

### 2. Attacker S-curve — per-kind units → `SAFETY_TABLE` lookup

For the side *under attack*, sum over each enemy N/B/R/Q:
`attack_units += KING_ATTACK_WEIGHT[kind] * popcount(piece_attacks(sq, occ_all)
& king_zone)`. Per-kind weights (CPW-Engine/Fruit lineage, research §2/§6):
knight 2, bishop 2, rook 3, queen 4. Index `SAFETY_TABLE[min(attack_units, 99)]`
(the cited 100-entry CPW-Engine/Glaurung/Fruit S-curve, research §6). **Gating
(CPW-Engine):** `attack_units = 0` unless ≥ 2 distinct enemy pieces attack the
zone **and** the attacking side has a queen (research §9 q3 — conservative at
M6.E, M6.F may relax). King-attack is **MG-only** (EG contribution 0; research
§5 — the king walks in the endgame, a large EG king-safety penalty fights the
PeSTO EG king PST's centralization incentive).

### 3. Pawn-shield — castled-king, two rank tiers, three files

Evaluated **only when the king is on a castled-side file**: `king_file ≥ f`
(kingside) or `king_file ≤ c` (queenside) — the CPW-Engine `col > E` / `col < D`
wider variant (research §3). For each of the three files (king file ± 1, clamped
to board): a friendly pawn on the king's 2nd rank scores `SHIELD_1`, else one on
the 3rd rank scores `SHIELD_2`, else 0. 2nd-rank > 3rd-rank (unmoved shield
strictly better; universal). White ranks 2/3, Black ranks 7/6 (relative-rank
mirror, the `CONN_*` precedent). Small EG component permitted (M6.F discovers
it). **Pawn storm is omitted at M6.E** (research §3/§9 q4 — CPW backfire
warning; not pawn-only-cacheable; M6.F's pawn-structure Texel covers it; a
documented strategic gap).

### 4. Open / semi-open file toward the king — MG-only

For the king file and each adjacent file: penalty if **semi-open** (no friendly
pawn) and a larger penalty if **open** (no pawn of either color); an extra
amplification when *both* adjacent files are semi-open (the no-cover threshold
effect). All EG = 0 (universally MG-only; open files are normal in the endgame —
research §4). Computed from `bitboard::file_fill` over the pawn bitboards.

### 5. Tapering — full-tapered MG/EG pair, EG near zero, no hard phase gate

King safety is a white-perspective `(mg, eg)` pair blended by the existing
`(mg·phase + eg·(24−phase))/24` framework — no separate phase-threshold gate, no
TSCP-style material scaling (research §5; material scaling fights the project's
phase blend). MG-heavy: the S-curve and open-file components are MG-only (EG 0);
shield carries a small EG term. Setting EG≈0 *is* the "hard-off in EG" extreme
expressed smoothly; M6.F Texel discovers any nonzero EG.

### 6. Computed live in `evaluate_core`, never cached — supersedes ADR-0032 §3's pawn-shield-cache note

`king_safety_term_white(pos) -> (i32, i32)` is computed **live on every
`evaluate_core` call, never cached**, exactly the M6.C-passed-pawn / M6.D-
mobility class (ADR-0032 §3 "not pawn-only ⇒ computed live, never cached").
**This supersedes the forward-looking note in ADR-0032 §3** ("M6.E will extend
the [pawn-hash] entry with pawn-shield masks"). That note was a reserved
*possibility*, not a binding decision; M6.E decides against it:

- The dominant inputs (king square + every attacking piece's square) are **not
  pawn-only** — only the shield/open-file sub-components are pawn-derived.
- The shield value depends on the **king file** (which 3 files to consult).
  The king file is **not in the pawn-Zobrist key**, so caching a scalar shield
  value keyed by `pawn_zobrist` would be a *correctness hazard* (same pawn
  structure, king on g1 vs c1 → different shield). Only a king-independent
  *mask* could be cached, and applying it still needs the live king file —
  the cache would save a `file_fill` + a few `contains`, not the term.
- Extending `PawnHashEntry` breaks the `#[repr(C)]` 32-byte / 2¹⁷-entry /
  4-MiB size-pin (the `const _: () = assert!(size_of == 32)` + the
  `PAWN_HASH_ENTRIES == 131072` arithmetic) for negligible gain.
- A **zeroed** term (§8) has zero cache benefit by construction; M6.F reads the
  live-recompute structure.

The term enters the blend **numerator only** — never `static_eval_white()` (the
RFP/NMP/FFP pruning input) nor the mop-up advantage estimate (the ADR-0032 §4
boundary class). Pinned by boundary regression tests mirroring
`static_accessor_excludes_mobility` + `mop_up_addend_excludes_mobility_vs_m6c`
verbatim in shape.

### 7. Independence / double-count (the M6.F brief)

Research §7 ranks the overlaps the M6.F joint Texel must decorrelate: (a)
**pawn-shield × PeSTO MG king PST — moderate**, the dominant axis (the PST
already prices the castled-king square); (b) **attack-S-curve × PeSTO king PST —
low-moderate** (S-curve is dynamic, PST is static); (c) **shield × M6.B CONN —
low** (CONN's rank-2 bonus is small, shield is castled-gated); (d) **S-curve ×
M6.D mobility — negligible** (different output registers, same bitboard, no
shared score). The S-curve, shield, and open-file components are **coupled by
design** (shield weakness feeds the danger the S-curve and open-file both
price) — there is no M6.B-style interaction-immune subset.

### 8. M6.E ships score-neutral; whole weight set → M6.F joint Texel

Per the research §8 transfer-risk verdict (HIGH; the strongest of any M6 term)
and the three-phase law, M6.E ships the infrastructure **wired but with every
king-safety weight zeroed** in `eval::data`: `KING_ATTACK_WEIGHT_{N,B,R,Q} = 0`,
`KING_SAFETY_TABLE = [0; 100]`, `SHIELD_1_{MG,EG} = SHIELD_2_{MG,EG} = 0`,
`KS_FILE_{SEMI_OPEN,OPEN,BOTH_ADJ}_MG = 0` (the literature defaults preserved in
the `data.rs` provenance block as the M6.F starting point — the M6.C/M6.D
`PASSED_*`/`*_MOBILITY_*` precedent verbatim). `const KING_SAFETY_IN_EVAL =
true`; the full term math (zone, attacker count, S-curve lookup, shield,
open-file) is live and tested at zero weight (M6.F-ready).

**Reconciliation with the roadmap §M6 per-phase-SPRT exit criterion (line
296).** "Each phase commits its own SPRT-positive change vs. the prior phase's
baseline tag" is satisfied **vacuously** for an inert landing: the shipped eval
is byte-identical to `M6.D`, so there is **no Elo claim to test** — the
M6.C/M6.D *disposition* (a provably-inert change cannot regress; SPRT would
measure zero by construction). This part is settled precedent.

**The novel element vs M6.C/M6.D — owned explicitly.** M6.C *ran* its 3-config
mixed-TC screen and M6.D *ran* its 6-SPRT per-kind screen, then each deferred
*because the screen empirically returned "defer."* M6.E goes further: it skips
the diagnostic screen entirely on a *prediction* (research §8). This is a
stronger claim than either precedent established empirically, and the plan owns
it rather than papering it as "precedent." It is justified by research §8's
specific cost/EV argument: king safety needs *mixed-TC* screens (TC-sensitive,
~5× M6.B's single-TC cost), the components are **coupled by design** (§7 — no
interaction-immune subset exists to find, unlike M6.B's CONN-only or M6.D's
independent N/B/R/Q axes), and the PeSTO-king-PST double-count is structurally
certain to need *joint* reshaping not subset selection — so the screen's only
possible outcome is "defer," at the highest screen cost of any M6 term.
Attempting even the open-file-only screen (research §8 Stage 1, the "most
likely to transfer" component) was considered and rejected on the same EV
grounds (it is itself S-curve-coupled, MG-small, and the noisiest term's screen
is the costliest). **To preserve the M6.F directional brief that M6.C/M6.D got
from their screens, M6.E commits to the one cheap substitute:** a single
live-default same-campaign WAC/STS diagnostic (RUN ALONE, ~20 min — *not* a
~40 h screen), run time-permitting after the inert landing is committed,
recorded as a rejected-config diagnostic (next paragraph). It does not gate the
landing; it gives M6.F's king-safety reshape the per-theme directional signal
the mobility/passed-pawn briefs received.

**Landing gate (the M6.C/M6.D inert-landing disposition verbatim).** Zeroed
weights ⇒ `king_safety_term_white ≡ (0, 0)` ⇒ `evaluate` byte-identical to
`M6.D` ⇒ `bench 1213649` / depth-4 `90591` byte-for-byte ⇒ infrastructure
provably behaviorally inert ⇒ regression structurally impossible ⇒ **M6.E lands
without a confirmation SPRT**. The per-theme STS secondary gate is **moot for
the shipped build** (eval == `M6.D` by construction; the M6.C/M6.D precedent).
The committed live-default same-campaign WAC/STS run (preceding paragraph) is
recorded as a **rejected-config diagnostic** (the M6.C/M6.D "directionally
correct, mis-shaped not mis-designed — reshape, don't delete" corroboration for
M6.F, king-safety claiming STS theme "King Activity" + "AKPC"); it is **not** a
landing gate and the milestone does not block on it (run time-permitting,
RUN-ALONE, via a throwaway non-zero-weights build fully reverted before the
clean apply — the M6.D screen-build hygiene).

**M6.F obligation (extended).** The single joint-Texel pass now also re-derives
the entire king-safety weight set (S-curve table + per-kind attacker weights +
shield + open-file + MG/EG split) against our PeSTO PSTs — *jointly* with the
M6.B ISO/DBL/BWD re-introduction + CONN rescale, the M6.C passed-pawn rank-
table/king-distance reshape, and the M6.D N/B/R/Q mobility reshape. One joint
pass; until then M6.B–E's zeroed terms stay inert by design. The §7
double-count axes (esp. shield × PeSTO king PST) are the load-bearing inputs to
that pass.

## Consequences

- **Eval cost.** `king_safety_term_white` adds an uncached whole-board
  per-leaf cost (two king-zone builds + N/B/R/Q attack scans for both sides +
  shield/open-file lookups), on the `evaluate_cached` qsearch hot path. It
  reuses the same `magic::*_attacks`/`masks::*` reads M6.D already pays; the
  marginal cost is the king-zone `&` + popcount + the pawn-derived lookups.
  NPS drop > 10% on bench = should-investigate (roadmap policy); the
  zeroed-but-live term still pays full compute (the M6.C/M6.D precedent — the
  honest cost of an M6.F-ready inert landing).
- **Bench node count.** Unchanged: `king_safety_term_white ≡ (0,0)` ⇒
  `evaluate` byte-identical to `M6.D` ⇒ `bench 1213649` byte-for-byte. The
  ADR-0010 determinism contract holds; M6.E's bench anchors M6.F.
- **Architectural boundary.** No change to `PawnHashEntry`/`PawnEval`/the pawn
  hash, Zobrist, movegen, make/unmake, or the negamax/qsearch loop — a pure
  additive `evaluate()` term reading existing M1 attack fns + `Position`
  occupancy + the M6.B `bitboard::file_fill` helper. Not movegen-adjacent ⇒
  extended perft not triggered (M6.B/C/D precedent).
- **ADR-0032 §3 superseded in part.** Its forward-looking pawn-shield-cache
  note is overridden by Decision §6 (compute live, never cached). ADR-0032's
  pawn-hash entry, ADR-0009/ADR-0031, and the M6.B–D terms are otherwise
  unaffected — this ADR adds a parallel additive term.

## Alternatives considered

- **Ship live literature defaults gated by a landing-gate SPRT + contingency
  screen ladder (the M6.D pattern).** Rejected: research §8 — king safety is
  the highest-transfer-risk M6 term (PeSTO king-PST double-count), the noisiest
  to SPRT (mixed-TC screens mandatory, ~5× M6.B's cost), and 2 of 3 recent
  precedents ran the full ladder only to defer. Negative expected value.
- **Open-file-only screen (research §8 Stage 1).** Rejected on the same EV
  grounds: the "most likely to transfer" component is still S-curve-coupled
  (§7), MG-small, and screening it requires the noisiest term's costliest
  (mixed-TC) campaign for a likely-defer outcome.
- **Extend the pawn-hash entry with pawn-shield/open-file masks (ADR-0032 §3's
  reserved option).** Rejected — Decision §6: correctness hazard
  (king-file-dependent value under a pawn-only key), `#[repr(C)]` size-pin
  breakage, negligible gain, zero benefit for a zeroed term.
- **Linear attacker sum instead of the S-curve.** Rejected: the nonlinear
  2–4-attacker escalation ("critical mass") is the defining qualitative
  property of king safety (research §2); a linear scheme cannot express it and
  the S-curve is the dominant literature consensus.
- **Hard phase-gate (`if phase < k { return (0,0) }`).** Rejected: adds a
  hyperparameter + a discontinuity; the tapered MG/EG pair with EG≈0 expresses
  the same decay smoothly (research §5).
- **Include an explicit pawn-storm term.** Rejected at M6.E: CPW backfire
  warning, not pawn-only-cacheable, low marginal value over the existing
  pawn-structure terms; M6.F's pawn-structure Texel is the right vehicle
  (research §3/§9 q4). Documented strategic gap.

## Supersedes

- **ADR-0032 §3** — the forward-looking pawn-shield-cache reservation ("M6.E
  will extend the [pawn-hash] entry with pawn-shield masks when king safety
  needs them"). That note was a reserved *possibility*, not a binding
  decision. M6.E computes the king-safety term **live in `evaluate_core`,
  never cached** — the dominant inputs (king square + every attacker's square)
  are not pawn-only, the shield value depends on the king *file* which is not
  in the `pawn_zobrist` key (caching a scalar shield keyed by `pawn_zobrist`
  would be a correctness hazard), and extending `PawnHashEntry` breaks the
  `#[repr(C)]` 32-byte / 2¹⁷-entry size-pin for negligible gain on a zeroed
  term. Full rationale in Decision §6. ADR-0032's pawn-hash entry,
  pawn-Zobrist substream, and §4 eval-boundary rule are otherwise unaffected
  and remain in force.

## References

- `docs/research/m6-king-safety.md` (design space + transfer-risk verdict),
  `docs/plans/m6.e.md` (plan + test surface).
- ADR-0031 (tapered eval), ADR-0032 (pawn hash + §3 live-not-cached precedent +
  §4 eval-boundary rule + §7/§8 score-neutral inert-landing disposition),
  ADR-0010 (bench determinism), ADR-0004 (eval interception point).
- `docs/milestones/m6.c.md`, `docs/milestones/m6.d.md` (the score-neutral
  inert-landing + "co-calibrated-elsewhere ≠ transfers here" precedents).
- CPW: King Safety, CPW-Engine eval, CPW King, Pawn Hash Table, Tapered Eval,
  Evaluation Overlap. MadChess 3.0 King Safety. TalkChess t=82407. mACE Chess
  king-safety tuning. PeSTO (rofchade.nl).
