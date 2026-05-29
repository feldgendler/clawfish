# ADR-0038 — King-safety attacker S-curve activation (M6.K): literature defaults, no runtime gain knob

**Status:** Accepted — **candidate landed, stage-1 SPRT vs `M6.J` PENDING (operator-gated).** The attacker-count S-curve (the 100-entry `KING_SAFETY_TABLE` + the 4 per-kind `KING_ATTACK_WEIGHT_{N,B,R,Q}` multipliers), zeroed/inert since M6.E and excluded from the M6.I/M6.J Texel tune (ADR-0037 §3), is activated to the literature CPW values (`2/2/3/4` + the Glaurung-1.2-lineage table). Pure `src/eval/data.rs` data edit; `evaluate` now scores king attacks. Bench moved `1357063`/d4 `112497` → **`1431810`/d4 `117941`**. The ship/revert decision is the operator-run mixed-TC SPRT (§ verdict ladder); this ADR records the candidate and the methodology.

## Context

M6.J STS diagnostics pinned **King Activity 555 / AKPC 628** as the largest residual classical-eval gap. The fix — the attacker-count S-curve — is the one M6 term the Texel-on-quiet-positions pipeline structurally **could not** tune (ADR-0037 §3):

- **Non-linear in the tunable surface.** The term is `score = KING_SAFETY_TABLE[Σ multiplier_kind · attackers]`. With the multipliers shipped at zero, `units ≡ 0` ⇒ only `TABLE[0]` is reachable ⇒ the table is dead by construction. Making the index live needs non-zero multipliers, which breaks the linear inner solve.
- **Quiet-corpus blind spot.** The corpus's `|static − qsearch| < 30` quiet filter preferentially strips the sharp king-attack positions whose game outcome would train the high-danger end of the curve.

King danger is realized in *games*, so it is tuned on games (SPRT, and SPSA if needed), not on a quiet-position static regression. This was `tuning-backlog.md` item 9; M6.K promotes it to a milestone. Gate opened when M6.J shipped (2026-05-29). Pre-M11 opportunity only — NNUE (M11) re-trains the eval from scratch and obsoletes all classical king-safety weights.

## Decision

### 1. Stage 1 — activate the literature CPW S-curve (this candidate)

Set `KING_ATTACK_WEIGHT_{N,B,R,Q} = 2/2/3/4` and `KING_SAFETY_TABLE` to the 100-entry Glaurung-1.2-lineage S-curve (already recorded verbatim in the `data.rs` block comment; ADR-0003-clean — CPW-Engine eval page, not engine source). Global gain `g = 1`. This is the roadmap's "literature on/off probe": it tests whether the M6.E **HIGH transfer-risk** worry — the PeSTO MG king PST already prices ~30–50 cp of castled-king safety, so the literature term may double-count — actually bites at the *joint-tuned* M6.J landscape.

**Pure data edit.** The S-curve computation (`king_safety_term_white`, `king_zone`, the `< 2 attackers ∨ no queen` gate, the `units.clamp(0, 99)` index) already exists and is unit-tested; activation only changes the frozen data it reads. The term stays **excluded from the Texel core vector** — it is frozen at literature instead of frozen at zero.

### 2. Texel faithfulness preserved by construction

The Texel reference scorer recomputes the S-curve from `EvalParams`' frozen fields (`features.rs::king_safety_scurve_mg`), which read `eval::data` via `EvalParams::shipped()`. So the §7 Tier-1 cross-check `reference_score_white(pos, shipped()) == static_eval_white(pos)` stays green automatically — both sides read the same now-non-zero constants. The fast **linear** model (`extract`/`model_score_white`) excludes the S-curve by design (it is the excluded nonlinear term); its Tier-2 equivalence test (`model_matches_reference_at_random_and_literature_weights`) now zeroes the frozen S-curve in its comparison params to isolate the linear-core fidelity it actually validates. **Consequence:** a future re-tune with the S-curve *active* must fold the frozen S-curve into the model base or freeze it consistently — deferred with the non-quiet-corpus campaign.

### 3. Stage 2 (if stage 1 fails) — SPSA-deflate, realized as data, not a runtime knob

If the stage-1 SPRT returns H0/regress, the double-count is real and stage 2 deflates. The roadmap frames a global gain `g ∈ (0, 1]` on the table output. **Decision: realize `g` as a pre-scaled table** (`KING_SAFETY_TABLE[i] = round(literature[i] · g)`), a further `data.rs` edit — **not** a runtime gain knob. Rationale: (a) the project ethos is hardcoded weights, not runtime tuning knobs (ADR-0037 §9); (b) the SPSA harness is not built and the roadmap explicitly sanctions a **manual coordinate grid `g ∈ {0.25, 0.5, 0.75, 1.0}` × coarse multiplier sweep** as the substitute; (c) it keeps both stages on the same minimal data surface. **Do NOT tune the 100 individual entries** — no signal at corpus scale, guaranteed overfit; keep the literature *shape*, deflation is the lever.

### 4. Verdict ladder (mixed-TC + virtual-clock SPRT vs `M6.J`; mirrors ADR-0037 §9)

1. **rung-1** — CI lower ≥ 5: ship, tag `M6.K` at the activated build.
2. **rung-2** — mean ≥ 5 ∧ CI lower > −10: ship + retrospective note.
3. **rung-3** — mean ≥ 0 ∧ CI lower > −10: ship + caveat.
4. **rung-4** — mean < 0 ∨ CI lower ≤ −10: **revert** the activation (back to the zeroed M6.J state — there is no other production value to revert to), M6.K closes negative, recommend the SPSA follow-up.

**STS sub-gates.** Primary **King Activity** (M6.J 555 → expected lift; M6.E live-default precedent predicted +62); secondary **AKPC** (M6.J 628 → expected flat-to-modest; M6.E was −10 within per-theme noise). A strong-negative on King Activity with a flat-or-positive SPRT is a `should-fix` per the M6 §297 secondary-gate rule (M5.E correctness-over-Elo precedent).

## Consequences

- **Positive:** closes the largest measured classical-eval gap with a minimal, reviewable data edit; faithfulness preserved by construction; both campaign stages share one data surface; the verdict ladder makes ship/revert principled.
- **Negative / accepted:**
  - The land decision is **operator-gated** by the ~10 h mixed-TC SPRT (ADR-0037 §10) — the candidate ships SPRT-pending (M6.I/M6.J precedent).
  - A double-count (M6.E HIGH transfer-risk) may make stage 1 H0/regress — a *documented outcome* routing to stage 2, not a unit failure.
  - The Texel fast model excludes the active S-curve; a future re-tune must handle it (§2).
  - `bench/m6-params.json`'s frozen S-curve fields still read zero (generated pre-activation); harmless for stage 1 (`texel-tune apply` never writes the S-curve), but a **stage-2 prerequisite**: regenerate or hand-reconcile the JSON before any stage-2 re-apply so zeroed params are not applied over the activated table.

## References

- Roadmap §M6.K scope-detail + baseline-tag row.
- ADR-0037 §3 (the exclusion + its structural/semantic reasons), §9 (verdict ladder), §10 (operator-gated SPRT).
- ADR-0033 (king-safety infra; §2/§5 MG-only S-curve).
- `docs/research/m6-king-safety.md` (M6.E HIGH transfer-risk / double-count verdict).
- `docs/milestones/m6.k.md` (this candidate's retrospective + stage-1 command).
