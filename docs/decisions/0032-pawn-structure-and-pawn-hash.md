# ADR-0032 — Pawn structure evaluation + pawn hash + pawn-Zobrist substream

**Status:** Accepted (M6.B ships the **connected-pawn term only**; ISO/DBL/BWD
zeroed pending M6.F joint Texel — see Decision §7. Δ Elo +45.42 vs `M6.A`.
**M6.C** adds the passed-pawn term **wired but all weights zeroed** — score-
neutral vs `M6.B`, entire weight set deferred to the M6.F joint-Texel reshape;
see Decision §8.).

## Context

M6.A (ADR-0031) established the tapered `(static_mg_white, static_eg_white,
raw_phase)` representation. M6.B is the first M6 phase to add eval terms that
require a whole-board pawn scan: isolated / doubled / backward / connected
pawn-structure penalties and bonuses, plus passed-pawn *detection* (the
passed-pawn bonus is deferred to M6.C).

A whole-board pawn scan per `evaluate()` call is expensive and highly
redundant — pawn structure is stable across most search-tree moves. The
standard accelerator is a pawn hash table keyed by a pawn-only Zobrist
substream.

Plan + test surface: `docs/plans/m6.b.md`. Design space:
`docs/research/m6-pawn-structure.md`.

## Decision

### 1. Pawn-Zobrist substream — reuse the Polyglot pawn keys

`Position` gains `pawn_zobrist: u64`, the XOR of `zobrist::piece_key(pawn, sq)`
over every pawn (both colors), maintained incrementally in
`update_zobrist_after_make` and restored structurally in `unmake_move` from
`Undo.prior_pawn_zobrist`. The incremental delta uses the **same unified
structural form as the existing main-Zobrist update** — three conditional XOR
sites (pawn mover out@`from`; pawn-non-promo in@`to`; pawn victim
out@`capture_sq`), *not* a per-`MoveFlag` match. This makes the EP two-pawn
delta and promotion / promo-capture arms correct by construction (they fall
out of the upstream-computed `capture_sq` and `mv.promotion_kind()`, exactly
as the main Zobrist already relies on them). FEN-parsed positions are
initialized via `refresh_pawn_zobrist()` adjacent to every `refresh_zobrist()`
call in `src/fen.rs` (the parse path), not in `Position::from_fen` (which only
delegates).

- Side-to-move is **excluded** — pawn structure is STM-independent.
- The Polyglot pawn keys (`POLYGLOT_KEYS[0..128]`, black/white pawn ×64
  squares) are already vendored and audited under ADR-0009.
- A debug-build from-scratch round-trip assert
  (`pawn_zobrist == zobrist::pawn_zobrist_from_scratch(pos)`) is added to
  `make_move`/`unmake_move`/`make_null_move`, mirroring the existing main-
  Zobrist and eval-triple asserts.

### 2. Pawn hash table — search-owned, fixed 4 MiB, always-replace

`PawnHashTable` is a fixed 4 MiB (2¹⁷ × 32-byte entries) array owned as a
field on `AlphaBetaMover`, allocated in `AlphaBetaMover::new()` and cleared in
`Search::reset()`.

- `Search::reset()` already fires on `ucinewgame` *and* per bench position
  (via `Engine::reset_for_new_game`), so the ADR-0010 bench-determinism
  discipline is satisfied with no engine-side plumbing.
- No `PawnHash` UCI option. 4 MiB is the literature L3-fitting sweet spot
  (Apple-Silicon shared L3 8–16 MB); a tuning knob is unjustified surface.
- Always-replace (universal pawn-hash recommendation; pawn structure changes
  rarely, stale entries are replaced before harmful).
- Entry: `#[repr(C)] { key: u64, mg: i16, eg: i16, passed: [Bitboard; 2] }`,
  pinned to 32 bytes by a `const` size assert (the 4-MiB/2¹⁷-entry arithmetic
  must not silently break on a field reorder). Full-key verification on probe.
- `key == 0` is **never cached**: it is both the zeroed-slot sentinel and a
  reachable real value (the no-pawns position XORs to 0; some symmetric
  structures too). Such positions recompute every call — correctness-neutral
  (decision §5 still holds), performance-only, vanishingly rare.

### 3. Cached entry holds only consumed fields

The cached entry stores the white-perspective pawn-structure `(mg, eg)` and
`passed_pawns[White|Black]` — and nothing else. The roadmap's M6.B row
illustratively lists "isolated/doubled/backward/connected flag bitmaps"; these
are **not** cached because no M6.B/M6.C consumer reads them (the score already
integrates the penalties; M6.C reads only `passed_pawns[]`). M6.E will extend
the entry with pawn-shield masks when king safety needs them — pawn-shield is
a function of pawn positions only and is validly keyed by `pawn_zobrist`.
King-distance-to-passer is *not* pawn-only (king moves invalidate it) and is
computed live in M6.C, never cached here.

### 4. Pawn structure enters at `evaluate()` only, not the incremental accessor

`Position::static_eval_white()` (the RFP/NMP/FFP pruning input) continues to
return the material+PST blend with **no** pawn-structure contribution — the
same precedent as ADR-0031 §3 keeping bishop-pair and mop-up out of the
accessor. Pawn structure is added only inside `evaluate()`. Consequence: the
search-side pruning gates' firing patterns are unchanged by M6.B.

The **mop-up** advantage estimate also keeps its PST-only `(mg, eg)` inputs —
pawn structure is *not* added to it, matching the M6.A precedent that
bishop-pair is likewise excluded from the mop-up estimate (ADR-0031;
src/eval.rs:188–193). A regression assertion pins a mop-up-eligible fixture's
addend unchanged from M6.A, so the SPRT delta is cleanly attributable to the
pawn-structure terms and not a mop-up gating shift.

### 5. `evaluate_cached` is a pure accelerator

`evaluate(pos)` is preserved with its signature and recomputes pawn structure
uncached (used by tests/tools). `evaluate_cached(pos, &mut PawnHashTable)`
probes the hash. Both delegate to a shared `evaluate_core` and to a single
`pawn_eval` source of truth, so `evaluate_cached(p, h) == evaluate(p)` for all
`p` and any hash state. This invariant is pinned by a property test and is the
M6.B determinism guarantee (ADR-0010): the cache changes speed, never result.

### 6. Term definitions

- **Isolated** — no friendly pawn on either adjacent file; per pawn.
- **Doubled** — per *extra* pawn on a file (`popcount − 1`); tripled counts 2.
- **Backward** — CPW-simple bitboard predicate: stop square attacked by an
  enemy pawn AND not covered by own pawn attack-front-spans; **no** rank or
  half-open-file restriction. Rejected: Kmoch/Straggler, stop-square-only,
  SEE-based (research §1.3).
- **Connected** — the CPW bitboard predicate `phalanx | defended`: `phalanx`
  = own pawns with an own pawn on an adjacent file of the same rank;
  `defended` = own pawns on a square attacked by another own pawn. A bare
  defender that is itself neither defended nor in a phalanx is **not**
  connected (lone c3→d4 chain → `{d4}` only). One rank-scaled bonus per
  connected pawn by relative rank.
- **Passed (detection only)** — no enemy pawn on the file or either adjacent
  file strictly ahead; cached as `passed_pawns[color]`. Bonus is M6.C.
- Isolated/doubled/backward **stack** (no if-else suppression). Literature-
  default weights ship un-tuned; M6.F Texel calibrates.

### 7. M6.B ships **CONN-only**; M6.F re-introduces ISO/DBL/BWD via joint Texel

**First attempt — all four §6 literature-default terms, single-TC `10+0.1`
SPRT vs `M6.A`: verdict=H0, Δ Elo −99.88 [−140.71, −61.58]** (218 games /
109 pairs, pentanomial `[27,28,39,9,6]`). Correctness was exonerated up
front (extended perft, 1610 debug tests incl. round-trip asserts,
`evaluate_cached == evaluate` proptest, 0 missed mutants, final-review
clean; with all pawn weights zeroed `bench` == `M6.A`'s `1093365`
byte-for-byte ⇒ the infrastructure is provably behaviorally inert and the §4
`evaluate`/`static_eval_white` asymmetry does not independently corrupt
pruning). The regression is **100% the weights**, not a code defect.

**Per-term screen + confirmation (SPRT vs `M6.A`, single-TC `10+0.1`,
elo0=0/elo1=10, shared seed; an env-mask experiment build, reverted).** The
"one rotten term" hypothesis was **falsified**:

| Subset | Δ Elo vs `M6.A` | verdict |
|---|---|---|
| ISO only | **+82.70** [+50.57, +116.29] | H1 @214g |
| DBL only | **+31.35** [+9.43, +53.53] | CI fully + |
| BWD only | −7.53 [−27.84, +12.73] | ≈ neutral |
| CONN only | **+103.08** [+65.54, +143.13] | H1 @222g |
| ISO+DBL+CONN | +9.85 [−13.92, +33.70] | ≈ neutral |
| ISO+CONN | **−197.94** [−281.29, −131.31] | H0 @66g |

Every term is individually positive-to-neutral (Σ marginals ≈ +209) yet
all-four = −99.88 and **ISO+CONN alone = −197.94** (worse than all-four;
dropping BWD made it *worse*). The −100 is a **catastrophic ISO×CONN
interaction**: ISO and CONN are opposite-signed measurements of the *same
pawn-connectivity axis* with grossly mismatched magnitudes (CONN rank-scaled
to +95; ISO flat −20), coherent only after the joint Texel co-calibration §6
always assigned to M6.F. Applied raw together they double-count connectivity
into an internally-contradictory, over-scaled signal. This is precisely the
double-counting §6 anticipated — now empirically proven, not theorized.

**Provenance — the weights are a non-co-calibrated pastiche.** Per
`docs/research/m6-pawn-structure.md` §1/§6 the four defaults come from
*heterogeneous, methodologically incommensurable* origins, not one tuned
source: ISO/DBL are hand-picked **midpoints of wide cross-engine ranges**
(CPW qualitative + post-NNUE folklore), flat per-pawn; BWD is a deliberately
un-amplified placeholder; CONN is **one component of the Stockfish-lineage
`Connected[rank]` expression — the bare rank table stripped of the
phalanx/supported/opposed modulators it was co-designed and jointly-tuned to
be used with**. Mixing a de-modulated fragment of an integrated, co-tuned
expression with standalone round-number penalties measuring the *same axis*
is a first-order cause of the interaction, not merely "untuned magnitudes."
The literature's standing position (Texel's method; CPW Automated Tuning) is
that eval terms are *jointly fit, never independently sourced*, precisely
because they overlap — §5/§6 flagged the coupling qualitatively; the screen
quantified it as a −100…−198 Elo cliff.

**Probe — `(ISO+CONN)/2` decomposes the catastrophe (single-TC `10+0.1`,
reverted).** Halving *both* ISO and CONN weights moved ISO+CONN from
**−197.94 → +6.37 [−14.15, +26.93]** (≈ flat). This cleanly separates the
two failure modes: **~+204 Elo of the −198 was pure over-magnitude**
(uniform downscaling rescues it — the over-weighting component), but the
co-scaled pair still **plateaus at ≈0, ~100 Elo below CONN-only's +103 and
ISO-only's +83** — that residual is the **scale-invariant structural /
wrong-shape double-count** (flat ISO vs rank-scaled CONN, opposite signs on
one axis) that *no global multiplier can fix*. ⇒ M6.F's task is **reshape,
not rescale**: re-attach CONN's modulation context + decorrelate the axis
via joint Texel; a uniform weight scale provably plateaus at break-even.

**Decision.** M6.B ships the **connected-pawn term only**:
`const PAWN_STRUCTURE_IN_EVAL = true`; `ISO_MG=ISO_EG=DBL_MG=DBL_EG=BWD_MG=
BWD_EG = 0` in `eval::data` (CONN keeps its literature-default table). The
zeroed constants and their term math in `pawn_eval` stay (M6.F-ready,
referenced at zero weight); every predicate stays live and tested;
`pe.passed` is still cached for M6.C; `evaluate_cached == evaluate` (D6)
trivially holds. CONN-only is the **largest** confirmed gain *and*
structurally **interaction-immune** (a single term cannot interact with
itself) — every multi-term subset either collapses or is neutral.

**Landing-gate SPRT — CONN-only vs `M6.A`, canonical M6-phase mixed-TC
4-bucket, elo0=0/elo1=5, seed `0xC1ABF15AE10DD014`: verdict=continue at the
400-game cap, Δ Elo +45.42 [+19.68, +71.67]** (200 pairs, ptnml
`[9,39,70,55,27]`). It did **not** cross the H1 Wald bound (LLR 1.26); it
**lands by the M5.F / M5.G-v2 outcome-ladder precedent** — `continue@cap`
with a positive CI whose lower bound (+19.68) is far above the +5/−10
small-but-not-regression floors and *substantially stronger* than both
landed precedents (M5.F +13.03 [−10.92,+37.12]; M5.G-v2 +23.49
[+0.65,+46.53]). It is unambiguously not a regression and a real, no-tuning
strength gain over the score-neutral baseline (which was 0 by construction).

**Caveat — per-TC depth reversal (M6.F watch-item).** summary-by-tc:
`10+0.1 W72-L17` (strong+), `20+0.2 W32-L33` (flat), `40+0.4 W36-L23` (+),
`60+0.6 W16-L31` (**negative**). Unlike M6.A's depth-robust eval gain,
CONN-only's benefit is fast-TC-concentrated and *reverses* at the slowest
bucket — CONN's raw magnitude over-helps shallow move ordering and washes
out / hurts with deeper search. The aggregate **mixed-TC** game (the ELOH.D
gate object) is firmly positive; the slow-TC component is adverse. M6.F's
joint Texel must rescale CONN (and re-introduce ISO/DBL/BWD) against the
CONN-only baseline — the depth reversal is direct evidence the literature
CONN table is over-scaled for this engine.

**M6.F obligation (revised).** No longer "flip a gate" — the gate is live.
M6.F **re-introduces ISO/DBL/BWD via joint Texel against the shipped
CONN-only baseline** — and per the `(ISO+CONN)/2` probe this must be a
**reshape, not a rescale**: a uniform multiplier on the literature pair
plateaus at break-even (≈0), so M6.F must (a) re-attach CONN's
phalanx/supported/opposed modulation context (the de-modulated-fragment
provenance), (b) decorrelate the ISO/CONN connectivity axis, and (c) fix the
slow-TC CONN over-scaling — jointly, not by scalar tuning. Precedent for the
landing shape: M5.F / M5.G-v2 `continue@cap` positive-CI lands.

### 8. M6.C ships the passed-pawn term **wired, all weights zeroed**

M6.C adds `passed_pawn_term_white` (per-passer rank bonus + EG three-state
path discriminator + EG king-tropism, reading M6.B's cached `passed[2]`,
computed live in `evaluate_core` — never cached, king-distance/path are not
pawn-only, §3). It is no separate ADR (the roadmap M6.C row commits the
path-blocked semantic); this is its addendum.

**Three-config screen ladder vs `M6.B`** (canonical mixed-TC 4-bucket,
elo0=0/elo1=5, RUN ALONE; `bench/sprt/2026-05-17-m6.c-screen-ladder-vs-m6b-mixed-tc.md`):

| config | Δ Elo vs `M6.B` | failure |
|---|---|---|
| all-three (literature default) | **−21.74** [−49.60, +5.84] | 60+0.6 W15-L57 — KDIST slow-TC collapse |
| {RANK+PATH} (KDIST off) | +4.34 [−22.33, +31.07] | 10+0.1 W30-L59 — RANK+PATH fast-TC over-magnitude |
| {RANK+PATH}/2 (co-scale) | −0.87 [−23.68, +21.94] | 20+0.2 W15-L42 — failure migrates; scale-invariant |

**Decision.** The literature-default passed-pawn weights have a
**scale-invariant structural mismatch** with this engine: KDIST is an
independently-sourced, not-co-calibrated, rank-scaled term (the §7
de-modulated-fragment + standalone-coefficient pastiche pattern) that is
slow-TC-toxic; the MadChess RANK+PATH pair was Texel-tuned against a
*different* EG pawn PST and is fast-TC over-magnitude on our PeSTO EG pawn
PST; the `{RANK+PATH}/2` co-scale probe reproduces the §7 `(ISO+CONN)/2`
plateau — a uniform multiplier merely migrates the over-magnitude across TC
buckets, it cannot make a wrong-shaped term non-negative across the profile.
No positive interaction-immune subset exists (contrast M6.B's clean CONN-only
+103 H1). M6.C therefore ships **`PASSED_MG=PASSED_EG=PASSED_FREE_EG_DELTA=
PASSED_KDIST_OWN_PER_STEP=PASSED_KDIST_ENEMY_PER_STEP = 0`** in `eval::data`
(`PASSED_KDIST_CAP=5` kept as the named structural clamp); the term math
stays live at zero weight (M6.F-ready) — the §7 / M6.B
`PAWN_STRUCTURE_IN_EVAL` precedent verbatim.

**Landing gate.** Zeroed weights ⇒ `passed_pawn_term_white` ≡ `(0,0)` ⇒
`evaluate` byte-identical to `M6.B` ⇒ `bench 1213649` byte-for-byte ⇒
provably behaviorally inert ⇒ M6.C lands **without a confirmation SPRT** (the
§7 / M6.B inert-landing disposition). The live-term same-campaign WAC/STS
(+151 STS, #9 "Advancement a/b/c" +58) is a *rejected-config* diagnostic —
the term is directionally correct, mis-scaled not mis-designed; the shipped
build's WAC/STS == `M6.B`'s by construction.

**M6.F obligation (extended).** The M6.F joint-Texel pass now also
**re-derives the passed-pawn rank table against our PeSTO EG pawn PST and
reshapes (not rescales) king-distance** — jointly with the §7 ISO/DBL/BWD
re-introduction + CONN rescale. One joint pass; until then the passed term
is inert by design.

## Consequences

- **Eval cost.** A whole-board pawn scan on pawn-hash miss; expected ≥95% hit
  rate amortizes it. NPS drop > 10% on bench = should-investigate (roadmap
  policy); the hash is the mitigation.
- **Bench node count.** May shift (pawn eval changes qsearch leaf scores →
  root-move ordering). Allowed from M6.A onward; determinism across reruns at
  a fixed HEAD still required (ADR-0010).
- **Architectural boundary.** The qsearch eval call site changes from
  `evaluate(pos)` to `evaluate_cached(pos, &mut self.pawn_hash)` — a threaded
  search-owned scratch through the eval entry point, consistent with ADR-0004's
  "eval as a discrete interception point" (the same hook a future NNUE
  accumulator threads through).
- **New general-purpose bitboard primitives** (`*_fill`, `*_front_spans`) added
  to `src/bitboard.rs` — reusable by M6.C/M6.E.
- **ADR-0009 / ADR-0031 unaffected** — this ADR adds a parallel substream and
  an additive eval term; it supersedes nothing.

## Alternatives considered

- **Separate pawn-only random key set** (literature default). Rejected: +1 KB
  vendored constants and a parallel from-scratch/round-trip discipline to
  audit, for an independence property whose only failure mode is a bounded
  (tens of cp) pawn-score perturbation on a false hit — it cannot corrupt a
  TT-stored bestmove. Reusing the audited Polyglot pawn subrange is less code,
  better cache locality (the key row is already resident for the main Zobrist
  on every pawn move), and the always-on debug round-trip assert is a stronger
  correctness guarantee than auditability-by-separation.
- **`PawnHash` UCI spin option / engine-owned table.** Rejected: search-owned
  + `Search::reset()` clearing reuses the exact `history_table` discipline,
  needs zero engine plumbing, and gets per-bench-position clearing for free.
- **Pawn structure in the incremental `static_eval_white()` accessor.**
  Rejected: not cheaply incrementally maintainable (whole-board scan — the
  reason the pawn hash exists); and ADR-0031 §3 already set the precedent that
  non-PST eval terms live only in `evaluate()`.
- **Caching the per-term flag bitmaps.** Rejected: no consumer; dead cache
  weight. See decision §3.

## References

- `docs/plans/m6.b.md`, `docs/research/m6-pawn-structure.md`.
- ADR-0009 (Polyglot Zobrist), ADR-0031 (tapered eval), ADR-0010 (bench
  determinism), ADR-0004 (eval/NNUE interception point).
- CPW: Pawn Hash Table, Pawn Structure, Backward/Isolated/Doubled/Connected/
  Passed Pawns (Bitboards), Pawn Spans.
