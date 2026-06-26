# ADR-0044 — Depth-Conditioned PVS: smooth root-depth scout-start ramp

**Status:** Accepted — **SHIPPED 2026-06-26** (M8.A.1; new production search HEAD =
`M6.J` eval + `M8.A.1` search). 2-seed SPRT vs `M7.B.2` rung-1 (both CI-lower > 0):
`…D00B` +30.48 [+9.63, +51.55], `…D01B` +26.98 [+3.32, +50.89]; mean ≈ +28.7 Elo. The
fast-TC regression of the shelved M8.A recovered to neutral/positive and the 60+0.6 gain
held (combined 63.6%). Record:
[`bench/sprt/2026-06-26-m8a1-depth-conditioned-pvs-vs-m7b2.md`](../../bench/sprt/2026-06-26-m8a1-depth-conditioned-pvs-vs-m7b2.md).

## Context

M8.A (ADR-0043) added Principal Variation Search. Its 2-seed SPRT vs `M7.B.2` was
**net-negative but depth-amplifying** — regression at fast TC (10+0.1 ≈ −45, 20+0.2 ≈ −25,
40+0.4 ≈ −20), gain at slow TC (60+0.6 ≈ +73, 2-seed-robust); aggregate ≈ −20 ⇒ NO SHIP
(record: [`bench/sprt/2026-06-25-m8a-pvs-vs-m7b2.md`](../../bench/sprt/2026-06-25-m8a-pvs-vs-m7b2.md)).
This is the **mirror image** of M7.B's depth-inverse profile that M7.B.2 (ADR-0041) fixed
with a root-depth ramp.

PVS's per-node ledger is *scout savings − (re-search cost + prune-suppression cost)*; the
re-search flips the child to `is_pv = true`, suppressing `is_pv`-gated prunes (ADR-0043 §1).
Fast TC has no depth headroom to convert the savings, so the overhead dominates; slow TC
converts them. Because the engine experiences TC only through the **ID root depth it reaches**
(ADR-0041), the harm concentrates at **shallow** root iterations and the gain at **deep**
ones. See [`docs/research/m8.a.1-depth-conditioned-pvs.md`](../research/m8.a.1-depth-conditioned-pvs.md).

Two precedents bound the design:
- **M7.B.2 (success):** a *smooth, monotonic* root-depth ramp with the knee placed above the
  protected TC's median depth.
- **M5.K (failure):** a depth *band* `[8,12]` on aspiration width lost −34.9 Elo in the very
  bucket it meant to protect — a **non-monotonic band + hard cliff**. The user explicitly
  flagged this caution for M8.A.1.

## Decision

### 1. Condition PVS scouting on the ID root depth via a smooth scout-start ramp

A non-first move at move-ordering rank `cur_i` is scouted (PVS ladder) **iff**
`cur_i >= pvs_scout_start(root_depth)`; otherwise it takes the reference full-window LMR path
(the pre-PVS loop, `is_pv` not flipped). `root_depth` is the ID iteration depth (ADR-0041,
published once per iteration, constant across the negamax tree — *not* the local node depth).

```rust
const PVS_RAMP_D0: u32 = 12;
const PVS_RAMP_BASE: u32 = 16;
const PVS_RAMP_SLOPE: u32 = 4;

fn pvs_scout_start(root_depth: u32) -> u32 {
    if root_depth <= PVS_RAMP_D0 { return u32::MAX; }
    PVS_RAMP_BASE.saturating_sub(PVS_RAMP_SLOPE * (root_depth - PVS_RAMP_D0)).max(1)
}
```

`d≤12 → MAX (no scout); d13 → 12; d14 → 8; d15 → 4; d≥16 → 1 (full PVS).`

### 2. Why this shape (vs the alternatives)

- **Monotonic, not a band.** Scouting only ever *increases* with depth — avoids the M5.K
  non-monotonic-band failure mode the user flagged.
- **Smooth, not a cliff.** One ply of depth unlocks `SLOPE = 4` ranks; the off→on transition
  spans d13–d16, not a single-ply flip. The smoothing knob is the **scout-start rank**
  (PVS has no continuous per-move dial), thresholded on **move-ordering rank** because rank
  predicts fail-high probability — late-ranked moves rarely re-search, so scouting them is
  near-pure savings (research §4).
- **Off-regime is byte-identical to `M7.B.2`.** `root_depth ≤ D0 ⇒ scout_start = MAX ⇒` every
  non-first move takes the reference path ⇒ exactly M7.B.2. Fast TC (only shallow iterations
  completed) is *guaranteed* unchanged, not merely hoped-unchanged. This is the strongest
  defense against re-introducing M8.A's fast-TC regression. **It holds only because LMR can
  never fire at a PV node** (LMR is `!is_pv`-gated, ADR-0025 §2): at a PV node `r = 0`, so the
  reference path searches once at `pv_scout_child = false` (= M7.B.2's non-first-move
  `is_pv && cur_i==0 ⇒ false`); were LMR able to fire at a PV node the reference re-search would
  use `pv_full_child = is_pv = true` and diverge. The off-regime bench equality (d4 `45788` /
  d7 `662085`) is the proof; a mismatch is stop-the-line, not a number to re-record.
- **On-regime is exactly M8.A.** `root_depth ≥ D1 = 16 ⇒ scout_start = 1 ⇒` full PVS. M8.A.1
  is a strict generalization: M8.A = `(D0=0)`, M7.B.2 = `(D0=∞)`.

A simple monotonic depth *gate* (PVS fully off below D0, fully on above) was rejected: it
reintroduces a single-ply cliff, the one structural feature M5.K warns against, for no
implementation saving over the ramp.

### 3. Constants are tunable; conservative first candidate

`D0 = 12` matches M7.B.2's own knee and sits above the 10+0.1/20+0.2 deciding depths
(~10.5/11.5). The candidate deliberately under-engages 60+0.6 (only its deep tail/iterations
scout) to prioritize *not* re-introducing the fast-TC regression. If the first SPRT is
marginal, the levers (research §5.1, in order): lower `D0`→10 / steepen `SLOPE`→6 (engage
earlier); raise `D0` (if fast TC still regressed); lower `BASE`. All single-constant edits;
mechanism unchanged. Mirrors the M7.B → M7.B.1 → M7.B.2 iterative path.

### 4. Test-harness `root_depth` control

`negamax_for_test` resets `root_depth = 0`, so the M8.A PVS-ladder tests (which assume
scouting is always on) would exercise the *off* regime under the default ramp. The harness
gains `negamax_at_root_depth_for_test(…, root_depth)` — a sibling test entry that runs the
per-entry resets and **then** publishes `self.root_depth = root_depth` (a direct mirror of the
existing `qsearch_at_root_depth_for_test`, `search.rs:2525`; zero existing-caller churn). The
shared reset block is extracted to a private `#[cfg(test)] reset_negamax_test_state` so the two
entries cannot drift. PVS tests call it with `root_depth ≥ 16`. The reference full-window path —
now production-active for non-scouted moves — gains the **complete** `#[cfg(test)]` LMR
instrumentation the ladder has, gated on `lmr_trace_root_ply == Some(ply)`: the scalar counters
`lmr_reduced_searches` / `lmr_full_researches` **and** the move vectors `lmr_reduced_moves` /
`lmr_researched_moves` (three LMR-firing tests assert on the vectors, not the scalars). This
keeps the ~19 wide-window `is_pv=false` LMR-firing tests valid with `root_depth` unset (LMR
fires identically on both paths; counting it on both keeps those tests ramp-agnostic). The
reference path does **not** get `pvs_*` counters (those events exist only on the ladder).

## Consequences

**Positive:**
- Targeted recovery of M8.A's fast-TC regression with the slow-TC gain retained, *if* the
  ~1-ply crossover is separable. Off-regime byte-identity makes the fast-TC safety provable.
- No signature changes to production code; search-layer only; three named, inert-at-bench
  constants. Bench (depth ≤ 7 ≤ D0) byte-identical to M7.B.2/M8.A ⇒ determinism anchor intact.

**Negative / risk:**
- **The crossover is ~1 ply wide** (deciding depths overlap across TCs); a root-depth ramp has
  little room to help 60+0.6 without touching 40+0.4. If even a swept ramp cannot net positive,
  the conclusion is that PVS's gain is not depth-separable on this engine — a clean shelve
  (like M7.A's ordering split / M7.C), HEAD stays `M7.B.2`.
- Test churn: the reference path becomes production-active and the PVS tests must set
  `root_depth` (§4). Mitigated by reference-path counter instrumentation (low per-test churn).

**SPRT-gated.** Ship per the ADR-0037 ladder (rung-1 CI-lower > 0; rung-2/3 mean ≥ 0,
CI-lower > −10), 2-seed. Verdict read from `wld` / `summary-by-tc` / `converged:` / `sprt:` /
`ci:`, not the lagging progress-line `elo`.

## References

ADR-0043 (PVS), ADR-0041 (M7.B.2 root-depth ramp — the precedent), ADR-0037 (ship ladder),
M5.K depth-gate cautionary record (`docs/milestones/m5.k.md`),
[`docs/plans/m8.a.1.md`](../plans/m8.a.1.md),
[`docs/research/m8.a.1-depth-conditioned-pvs.md`](../research/m8.a.1-depth-conditioned-pvs.md).
