# ADR-0034 — Tier-1 HCE features (outposts + rook-on-file + endgame draw-scaling), shipped score-neutral

**Status:** Accepted (M6.F ships three Tier-1 HCE constructs **wired but
inert** — all additive weights zeroed + the endgame draw-scale ≡ identity,
byte-identical to `M6.E`; the entire weight/coefficient set deferred to the
M6.I joint-Texel pass; the M6.C/M6.D/M6.E inert-landing disposition. See
Decision §2 + §4.).

## Context

M6.A (ADR-0031) established the tapered `(static_mg_white, static_eg_white,
raw_phase)` representation. M6.B (ADR-0032) added the pawn hash + pawn-structure
infra; M6.C added passed pawns (score-neutral); M6.D added piece mobility
(score-neutral, no ADR); M6.E (ADR-0033) added king safety (score-neutral).
M6.F is the **Tier-1 HCE feature phase** inserted *before* the M6.I joint
Texel pass per the roadmap-designated binding research
`docs/research/m6-remaining-hce-features.md` (Priority-1 list): the "extend
then tune" law — Texel calibrates present features, it cannot discover absent
ones, so the missing Tier-1 discriminators must exist in tree before the one
joint pass (adding them after M6.I would force a second Texel pass).

The roadmap §M6 ADR list explicitly binds an ADR on M6.F. The three features
(research §2.1/§2.2/§2.4):

1. **Outposts** — a knight or bishop in the enemy half, on a square no enemy
   pawn can ever attack (a "hole"), defended by an own pawn. The
   load-bearing justification is **not theoretical**: STS theme #3 "Knight
   Outposts" measured **−40** on the M6.D rejected-config build
   (`bench/epd-suites.md`) — a confirmed strategic blindspot the M6.I Texel
   pass *alone* cannot fix (Texel finds weights for present features; it
   cannot add a discriminator that does not exist). Outpost is the structural
   discrimination mobility/PST cannot make: pawn-supported-and-unchallengeable
   vs. merely-central placement.
2. **Rook on open / semi-open file** — `file_fill` discriminator; mobility
   counts moves, not file-open status (a structural axis Texel-on-mobility
   cannot recover).
3. **Endgame draw-scaling** — a multiplicative reduction on the blended eval
   for structurally drawish material: opposite-colored bishops with pawns,
   pawnless non-mating residues, and a 50-move-proximity taper. A *correctness*
   feature (draw recognition is a search-misguidance fix, research §2.4), not
   only an Elo knob. **KBvKB-same-color is already in M6.A**
   (`is_insufficient_material`) and is **not** duplicated here.

## Decision

### 1. Three constructs, one inert module

`src/eval/tier1.rs` exposes `outpost_term_white(pos) -> (i32,i32)`,
`rook_file_term_white(pos) -> (i32,i32)` (additive, blend-numerator), and
`endgame_scale(pos) -> i32` (a fixed-point numerator out of the structural
denominator `EG_SCALE_DEN`, applied as `blended * scale / EG_SCALE_DEN`).
**Three** `pub(crate)` testable seams — `outpost_squares(pos, side) ->
Bitboard` (the hole∩support set the term's per-piece loop MUST iterate, so the
weight-free structural test pins the *real* selector — added per the
test-suite review must-fix, the M6.E `king_zone`-seam mandate: for a
score-neutral term the structural seam is the only correctness signal beyond
the bench gate, an inline reconstruction pins nothing), `is_ocb_with_pawns`,
`is_pawnless_drawish` — pin the real predicates from structural tests (the
M6.E `king_zone` `pub(crate)`-seam precedent: a seam turns "tests a
reconstruction" into a real code-path pin). One `TIER1_IN_EVAL = true` const
gates all three constructs (the M6.B–E `*_IN_EVAL` precedent). Plan + test
surface: `docs/plans/m6.f.md`.

### 2. M6.F ships score-neutral; the whole set → M6.I joint Texel

Per the roadmap §M6.F row and the three-phase M6.C/M6.D/M6.E inert-landing
disposition, M6.F ships the infrastructure **wired but inert**: every additive
outpost/rook-file weight `= 0` and every endgame-scale tunable
`= EG_SCALE_DEN` (identity). `const TIER1_IN_EVAL = true`; the full term math
(outpost hole/support/rank predicate, rook file projection, OCB/pawnless
predicates, the 50-move ramp, the `*scale/DEN` step) is live and tested at the
inert config (M6.I-ready). Literature defaults preserved in the `data.rs`
provenance block as the M6.I starting point (the M6.C/M6.D/M6.E
`PASSED_*`/`*_MOBILITY_*`/`KING_*` precedent verbatim).

**Inert-identity construction.** For the additive terms this is the familiar
"weight = 0 ⇒ term ≡ (0,0)". For the multiplicative scale the analogue is
"every scale tunable = `EG_SCALE_DEN` ⇒ the `min`/taper math returns
`EG_SCALE_DEN` for **all** inputs ⇒ `blended * EG_SCALE_DEN / EG_SCALE_DEN ==
blended` **exactly** in i32" (`blended·D` is exactly divisible by `D` for any
i32 `blended`; no overflow — `blended` is the phase-blend of PST + addends in
the centipawn range, the existing `debug_assert!(result_white.abs() <
MATE_IN_MAX_PLY - 1)` confirms the pipeline stays in-band, and
`MATE_IN_MAX_PLY·64` ≈ 1.9M ≪ `i32::MAX` is ~1000× headroom; tied to the
existing invariant, not a magic literal). So `evaluate` is byte-identical to
`M6.E` ⇒ bench `1213649` / depth-4 `90591` byte-for-byte.

**Reconciliation with roadmap §M6 line 296 ("each phase commits its own
SPRT-positive change").** Satisfied **vacuously**: the shipped eval is
byte-identical to `M6.E`, so there is no Elo claim to test — the settled
M6.C/M6.D/M6.E *disposition* (a provably-inert change cannot regress; SPRT
would measure zero by construction). **No diagnostic-screen-skip novelty to
own** (unlike M6.E): the roadmap constructs M6.F as carrying *no SPRT gate of
its own* (§M6 line ~298) — there is no screen to skip, only the
inert-vs-live disposition (§4) to own. The M6.I outpost directional brief
already exists (the measured −40 STS gap); M6.F commits **no** rejected-config
diagnostic (the M6.E commitment was to substitute for a *skipped screen*;
M6.F skips none).

### 3. Predicate definitions (committed — bound plan-time interpretation)

- **Outpost hole.** A square is unchallengeable by enemy pawns iff it is
  outside `*_attack_front_spans(enemy_pawns)` (White piece ⇒ `!
  black_attack_front_spans(bp)`; the M6.B span helper). Pawn-supported iff the
  square is in the side's immediate pawn-attack set
  (`shift_north_east|north_west` for White). Gated to relative rank ∈ {3,4,5}
  (chess ranks 4–6 — the classic outpost band; CPW/MadChess). Per-kind
  (N/B) per-relative-rank MG/EG tables (a fully linear, per-rank-tunable
  shape — the research §2.1 "per-square rank-scaled table" option, chosen over
  a flat bonus because it keeps the M6.I surface linear-in-weights and lets
  Texel discover the rank profile).
- **Rook file.** Via `file_fill(own_pawns)` / `file_fill(enemy_pawns)`
  projection: no pawn either color on the rook's file ⇒ **open**; own pawn
  absent but enemy pawn present ⇒ **semi-open**; own pawn present ⇒ neither
  (open takes precedence; the CPW Rook-on-Open-File condition).
- **OCB-with-pawns.** Each side has **exactly one** bishop, on **opposite**
  color complexes (`LIGHT_SQUARES`/`DARK_SQUARES` popcount — the M6.A
  `bishop_pair_term_white` idiom), **no queens, no rooks** (heavy majors
  break OCB drawishness — CPW Bishops-of-Opposite-Colors), and at least one
  pawn present. **Disjoint by construction** from
  `is_kbkb_same_color`/`is_insufficient_material` (same vs opposite complex;
  the same-color KBvKB draw stays solely in M6.A's insufficient-material
  return-0 path — no double handling, pinned by
  `ocb_disjoint_from_kbkb_same_color`).
- **Pawnless drawish.** Zero pawns, zero queens, zero rooks **both sides**,
  then an **enumerated narrow accept-list** (NOT a "no major ⇒ drawish"
  sweep): **KNNvK** (one side `knights==2 ∧ bishops==0`, other side bare K —
  the canonical pawnless draw, the *new* coverage beyond M6.A
  insufficient-material) **or** a **balanced ≤1-minor-each** residue (each
  side `bishops+knights ≤ 1`). **Explicitly excludes KBNvK, KBBvK, and any
  side with `bishops+knights ≥ 2` other than the exact KNNvK pattern** —
  those are forced wins and must not be scaled down (the M6.I-active form is a
  correctness feature; mis-flagging a win is a correctness *regression*, not a
  fix). KRvKN / KQvKR "hard but not dead" tablebase draws are a **documented
  out-of-scope gap** (excluded by the no-rook/no-queen clause; need EGTB —
  M9). M6.I widens only the *coefficient*; the **pattern set is fixed at
  M6.F**. **Overlap precision:** the predicate returns `true` for KBvK/KNvK
  (balanced ≤1-minor) but those early-return `0` via `is_insufficient_material`
  at `eval.rs:228` *before* `endgame_scale` runs — the overlap is unobservable
  in eval; the *observable* new coverage is KNNvK + KBvKB-opposite + KNvKN +
  KBvKN. The `pub(crate)` predicate is tested in isolation, so the
  KBvK→true region is expected, not a defect. Pinned by
  `pawnless_drawish_predicate` + `pawnless_drawish_excludes_KBNvK`.

### 4. Endgame-scaling lands **inert**, not live — the open roadmap question resolved (owned, not papered)

The roadmap (§M6 ADR list + §M6.F scope detail) records this as an explicit
**open ADR question, not pre-decided**: endgame-scaling is a *correctness*
feature the research says "does not strictly need Texel calibration," so the
M6.F ADR *may* land that one sub-feature **live** behind an M5.E-style
correctness gate rather than inert. A **secondary optimizer-tractability
argument independently reinforces *live*** — a Texel-tuned multiplicative
scale makes the per-position score `scale(θ)·(w·f)` *bilinear*, breaking
M6.I's linear-in-weights feature-caching + closed-form gradient and opening
the `K·scale` gauge ridge; landing it live with a *fixed* literature
coefficient keeps `scale_i` a known per-position constant that folds into the
cached feature mask, preserving full linearity.

**Decision: inert-per-precedent (all three zeroed; the scale ≡ identity).**
Rationale, owned explicitly (the M6.E "own the divergence, don't paper it"
discipline):

1. **The roadmap's stated default is inert-per-precedent.** The live option
   is a *may*, not a directive; the default framing is "all three zeroed,
   M6.I-ready."
2. **The reinforcing optimizer argument is *conditional on an undecided
   premise*.** It bites only the gradient / finite-difference Texel paths; an
   **SPSA** optimizer is gradient-free and indifferent to the multiplier. The
   M6.I optimizer ADR is **still open** (Texel local-search vs SPSA vs
   gradient/Adam — roadmap §M6 M6.I ADR-bullets). Landing endgame-scaling
   live now commits an irreversible-by-precedent design change on a premise
   that is not yet decided.
3. **The optimizer concern is fully honorable at M6.I without landing live
   at M6.F.** If M6.I chooses a gradient/FD optimizer, M6.I's own ADR fixes
   `OCB_WITH_PAWNS_SCALE`/`PAWNLESS_DRAW_SCALE`/`FIFTY_MOVE_FLOOR` at
   literature values and **excludes them from the tunable vector** —
   `EG_SCALE_DEN` and `FIFTY_MOVE_TAPER_FROM` are already declared
   **structural** (out of the tunable vector) precisely so the per-position
   scale is a known constant folding into the cached feature mask. The
   linearity-preserving move is M6.I's to make *when the optimizer is
   actually chosen*, not M6.F's to pre-commit.
4. **Inert preserves the zero-risk byte-identical landing gate**, maximally
   consistent with three phases of precedent (the bench-byte-identity gate
   makes regression structurally impossible — no SPRT, no correctness gate to
   run, no WAC/STS re-baseline). Landing live would break the uniform inert
   landing and require its own M5.E-style correctness gate + a same-campaign
   WAC/STS re-baseline, for a correctness improvement that — given the short
   no-SPRT M6.F→M6.G→M6.I chain — lands at M6.I anyway.
5. **The correctness benefit is not forfeited, only sequenced.** OCB/pawnless
   draw recognition becomes active at M6.I (the first non-inert build). The
   no-rated-play dependency is explicit: M6.F → M6.G → M6.I carries **no
   intervening rated games** — M6.G is corpus *data* construction (no engine
   build, no bench, no SPRT — roadmap §M6.G + the `M6.G` tag row: "no engine
   build"), and M6.F itself ships byte-identical to `M6.E`. So nothing plays
   rated games at the M6.F config; the deferral costs no measurable strength.
   *(If a future roadmap change inserts rated play between M6.F and M6.I this
   point weakens and the decision should be revisited — the dependency is
   stated so that revisit trigger is auditable.)*

**Honest trade-off (not a closer call than it is).** Live-with-a-fixed-
coefficient **strictly dominates inert on two of three axes**: (i)
*optimizer-robustness* — it satisfies the M6.I linearity concern
*unconditionally*, with no dependence on the still-open M6.I optimizer ADR
(inert's point 3 only *defers* that resolution to M6.I); (ii)
*correctness-timing* — it delivers the OCB/pawnless draw-recognition fix at
M6.F instead of M6.I. Inert wins on **exactly one** axis: *landing-gate
uniformity + reversibility/risk* — it keeps the zero-risk byte-identical gate
(no correctness gate, no WAC/STS re-baseline), stays uniform with three phases
of precedent, and is trivially reversible (a one-commit weight flip at M6.I)
whereas a live landing is a shipped behavioral change. The research itself
(§2.4, Priority-1 table) and roadmap §M6.F say endgame-scaling is *the* feature
whose coefficient is a structural constant not needing Texel — i.e. the
live-with-fixed-coefficient case is genuinely strong. The decision selects
inert because (a) the roadmap's explicit default for this open question is
inert-per-precedent, (b) the dominated axis (correctness-timing) costs **zero
measurable strength** here (point 5: no rated play in the gap), and (c)
reversibility asymmetry favors the lower-risk option when the higher-EV option
costs nothing to defer. This is a *defensible discretionary tiebreak under a
roadmap-stated default*, not a claim that inert is the higher-EV engineering
choice in the abstract — the orchestrator surfaces this honestly so the user
(reading along) can override toward live before tests are written.

The live alternative is recorded under *Alternatives considered* with its
full pro-case so the blind plan-review loop pressure-tests this judgment.

### 5. Scale application point — the blended value, before mop-up

`result_white = (blended * scale / EG_SCALE_DEN) + mop_up`. The scale damps
the material/positional eval; **mop-up is added after, unscaled** — mop-up is
the king-driving conversion nudge for *won* endgames (it fires only above an
advantage threshold), conceptually orthogonal to draw-dampening (the CPW
"scale the positional eval, keep the conversion term" convention). At the
inert ship `scale ≡ EG_SCALE_DEN` so the placement is behaviorally moot; it is
committed now to bound the M6.I interpretation (M6.I may revisit whether
mop-up should also be damped in a dead-drawn pawnless residue — flagged, not
decided here). The scale multiplies the **blend numerator only** — never
`static_eval_white()` (the RFP/NMP/FFP pruning input) nor the mop-up estimate
(the ADR-0032 §4 boundary rule). Because the scale is the *first
multiplicative construct* in M6, "the pruning eval is unscaled" is a fresh
invariant given its own boundary regression test
(`endgame_scale_excludes_static_accessor_vs_m6e`) in addition to the
M6.E-mirrored accessor/mop-up pins.

### 6. Computed live in `evaluate_core`, never cached

All three constructs are computed **live on every `evaluate_core` call, never
cached** — the M6.C/M6.D/M6.E live-term class (ADR-0033 §6). The dominant
inputs are not pawn-only (rook squares, bishop-complex material, the halfmove
clock); no `PawnHashEntry`/`PawnEval`/Zobrist change. Not movegen-adjacent ⇒
extended perft not triggered (M6.B–E precedent).

### 7. Independence / double-count (the M6.I brief)

Research §2/§3 ranks the overlaps the M6.I joint Texel must decorrelate: (a)
**outpost × PeSTO N/B MG PST — moderate** (the PST already prices central
N/B; the outpost term adds the pawn-support + unchallengeable condition the
PST cannot express — additive, but co-calibrate to avoid over-valuing central
minors); (b) **outpost × M6.D mobility — low-moderate** (an outposted knight
often has average mobility; the −40 STS gap shows mobility *mis-leads* here —
the discriminator is genuinely absent, not double-counted); (c) **rook-file ×
M6.D rook mobility — low-moderate** (mobility rises on an open file; the
explicit predicate is additive discrimination — M6.D STS showed "+90 Open
Files" on live mobility, so co-calibration must split the credit); (d)
**endgame-scale × phase blend — low** (the EG/MG taper already damps as
material falls; the OCB/pawnless scale is a *multiplicative* correction on top,
structurally orthogonal). The outpost rank table, rook open/semi-open split,
and the three scale coefficients are the load-bearing M6.I inputs; the −40 STS
outpost gap is the directional brief (already measured — no new diagnostic
needed, §2).

### 8. M6.I obligation (extended again — one joint pass)

The single joint-Texel pass now **also** re-derives the Tier-1 set (outpost
per-kind per-rank tables + rook open/semi-open MG/EG + the OCB / pawnless /
50-move scale coefficients) — *jointly* with the M6.B ISO/DBL/BWD
re-introduction + CONN rescale, the M6.C passed-pawn reshape, the M6.D N/B/R/Q
mobility reshape, and the M6.E king-safety full reshape. One joint pass; until
then M6.B–F's inert terms stay inert by design. `EG_SCALE_DEN` and
`FIFTY_MOVE_TAPER_FROM` stay **structural** (out of the tunable vector — §4.3
linearity rationale).

## Consequences

- **Eval cost.** `tier1.rs` adds an uncached per-leaf cost (two
  `*_attack_front_spans` fills + outpost intersections + per-rook file
  projection + OCB/pawnless popcounts + the scale arithmetic + the
  `*scale/DEN` step) on the `evaluate_cached` qsearch hot path. It reuses the
  M6.B `file_fill`/`*_attack_front_spans` helpers + the `shift_*` ops M6.D
  already pays; the marginal cost is light (no slider sweep). NPS drop > 10%
  on bench = should-investigate (roadmap policy); the zeroed-but-live cost is
  the honest M6.I-ready price (the M6.C/M6.D/M6.E precedent).
- **Bench node count.** Unchanged: inert ⇒ `evaluate` byte-identical to
  `M6.E` ⇒ `bench 1213649` byte-for-byte. ADR-0010 determinism holds; M6.F's
  bench anchors M6.I (M6.G is corpus data infra — no engine build / no bench;
  M6.I is where eval/bench changes via the joint Texel tune).
- **Architectural boundary.** No change to `PawnHashEntry`/`PawnEval`/the
  pawn hash, Zobrist, movegen, make/unmake, or the negamax/qsearch loop — a
  pure additive + scaling `evaluate()` extension reading existing M1 attack
  fns + `Position` occupancy + the M6.B `bitboard` helpers. The scale is the
  first multiplicative eval construct: the ADR-0032 §4 boundary (scale never
  in the pruning eval) is preserved and gets its own pin (§5).
- **First multiplicative construct → M6.I linearity.** Recorded for the M6.I
  ADR: `EG_SCALE_DEN`/`FIFTY_MOVE_TAPER_FROM` structural + the scale
  coefficients tunable means M6.I must, *if it picks a gradient/FD
  optimizer*, either fix the scale coefficients (fold `scale_i` into the
  cached feature mask, preserving linearity) or accept the bilinear surface +
  the `K·scale` gauge ridge the M6.I stopping sub-protocol already flags. §4.3.

## Alternatives considered

- **Land endgame-scaling live behind an M5.E-style correctness gate
  (outposts + rook-file still inert).** The roadmap-recorded open option.
  Pro-case (recorded in full so the blind reviewer can pressure-test §4):
  (i) draw recognition is a *correctness* feature — a pure-HCE engine without
  it actively misguides search in OCB/pawnless positions (research §2.4),
  and M5.E set the "land a correctness feature on a correctness gate, not
  SPRT" precedent; (ii) the literature OCB coefficient (~0.5) does not need
  Texel — it is a structural draw-probability constant, not an Elo-tuned
  weight; (iii) the optimizer-tractability argument independently favors
  *live-with-fixed-coefficient* (keeps M6.I linear-in-weights). **Rejected**
  per Decision §4: the reinforcing optimizer argument is conditional on the
  *undecided* M6.I optimizer choice; the linearity-preserving move is M6.I's
  to make when the optimizer is chosen (the scale coefficients can be fixed +
  excluded from the tunable vector at M6.I without M6.F landing live); inert
  preserves the zero-risk uniform landing gate; and the correctness benefit
  is sequenced (active at M6.I, the first non-inert build) not forfeited —
  nothing plays rated games at the M6.F config.
- **Flat per-kind outpost bonus instead of a per-relative-rank table.**
  Rejected: a flat bonus + a separate rank scale would introduce a second
  multiplicative knob (bilinear, anti-M6.I-linearity); the per-rank table is
  fully linear and lets Texel discover the rank profile (research §2.1).
- **Include rook-on-7th / doubled-rooks / Kaufman material adjustments /
  threats.** Rejected — research Priority-2/3: rook-on-7th double-counts the
  PeSTO rook EG PST unless gated (cost > value pre-M6.I); doubled-rooks within
  Texel noise; Kaufman has an unresolved `static_eval_white` vs `evaluate`
  architectural question (defer per research §3); threats +7 Elo at best and
  search already resolves them. Out of M6.F scope.
- **Scale the full `result_white` including mop-up.** Rejected: mop-up is the
  won-endgame conversion nudge, orthogonal to draw-dampening; scaling the
  positional eval only is the CPW convention (Decision §5; M6.I may revisit
  the dead-drawn-pawnless sub-case).
- **Cache the Tier-1 terms in the pawn hash.** Rejected — not pawn-only
  (rook squares, bishop-complex material, halfmove clock); the ADR-0033 §6
  live-term class.

## References

- `docs/research/m6-remaining-hce-features.md` (roadmap-designated binding
  research; Priority-1 list + the −40 STS outpost measurement),
  `docs/plans/m6.f.md` (plan + test surface).
- ADR-0031 (tapered eval), ADR-0032 (pawn hash + §4 eval-boundary rule +
  §7/§8 score-neutral inert-landing disposition), ADR-0033 (king safety + §6
  live-not-cached precedent + §8 inert-landing + the "own the divergence"
  discipline), ADR-0010 (bench determinism), ADR-0004 (eval interception
  point), ADR-0003 (no engine-source reading — the literature defaults are
  CPW/MadChess-dev-blog, ADR-0003-clean).
- `docs/milestones/m6.c.md`, `docs/milestones/m6.d.md`,
  `docs/milestones/m6.e.md` (the score-neutral inert-landing + "co-calibrated
  -elsewhere ≠ transfers here" + "own the divergence" precedents).
- CPW: Outposts, Rook on Open File, Rook on Seventh, Bishops of Opposite
  Colors, Material Tables, Evaluation Overlap, Texel's Tuning Method.
  MadChess dev blog: Knight Outpost (+25), Endgame Eval Scaling (+12).
