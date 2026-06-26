# M8.A.1 — Depth-Conditioned PVS: SPRT vs `M7.B.2`

**Date:** 2026-06-26. **Candidate:** M8.A.1 (branch `m8a1-depth-conditioned-pvs`, off `m8a-pvs`).
**Baseline:** `M7.B.2` tag (`f8d6746`, the production search HEAD; eval `M6.J`). Only
behavioural delta = the PVS three-step ladder (M8.A, ADR-0043) **gated by the M8.A.1
root-depth scout-start ramp** (ADR-0044): a non-first move at move-ordering rank `cur_i`
is scouted iff `cur_i >= pvs_scout_start(root_depth)` — off (≡ M7.B.2) at root_depth ≤ 12,
full PVS at ≥ 16, smooth between (`D0=12, BASE=16, SLOPE=4`). Same-binary
clawfish-vs-clawfish, mixed-TC `10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1` + virtual-clock,
elo0=0/elo1=5, alpha=beta=0.05, up to 400 games/seed. Runner: `scripts/sprt.sh sprt
M7.B.2`. Seeds + TC shape identical to M7.B / M7.B.2 / M7.C / M8.A ⇒ per-TC directly
comparable to the M8.A campaign.

## Result — SHIP (rung-1, both seeds CI-lower > 0). New production search HEAD = `M6.J` + `M8.A.1`

| Seed | Δ Elo | CI | verdict | llr | ptnml |
|---|---|---|---|---|---|
| `…D00B` | **+30.48** | [+9.63, +51.55] | continue@400 | +1.24 | [4,40,81,67,8] |
| `…D01B` | **+26.98** | [+3.32, +50.89] | continue@400 | +0.84 | [11,35,81,58,15] |

**2-seed mean ≈ +28.7 Elo; BOTH CI-lowers > 0** (+9.63, +3.32) ⇒ clears the ADR-0037
**rung-1 ship-by-CI** bar (CI-lower > 0), 2-seed-consistent. Both LLRs positive (drifting
toward H1) but did not reach the H1 boundary before the 400-game cap — the
M6.I / M6.J / M7.B continue-at-cap ship-by-CI precedent. User pre-authorized the
overnight campaign and the ship-on-positive-verdict decision.

### Per-TC (W-L-D), 2-seed — the M8.A fast-TC regression is GONE; the slow-TC gain is KEPT

| TC | seed00B | seed01B | combined | ≈Elo | M8.A combined | read |
|---|---|---|---|---|---|---|
| 10+0.1 | 23-17-42 | 36-39-53 | 59-56-95 (210) | ≈ +5 | −45 | **recovered** (neutral+) |
| 20+0.2 | 27-19-60 | 21-20-57 | 48-39-117 (204) | ≈ +15 | −25 | **recovered** (positive) |
| 40+0.4 | 22-20-60 | 24-22-54 | 46-42-114 (202) | ≈ +7 | −20 | **recovered** (neutral+) |
| **60+0.6** | **35-16-59** | **41-10-23** | **76-26-82 (184)** | **≈ +97** | +73 | **kept — clearly positive (63.6%)** |

## Reading

- **The depth-conditioning thesis is vindicated.** M8.A (unconditioned PVS) was net
  ≈ −20 Elo because PVS regressed every fast TC (10+0.1 −45, 20+0.2 −25, 40+0.4 −20)
  while only 60+0.6 gained (+73). M8.A.1 conditions the scout on the ID *root* depth so
  the prune-suppressing re-search engages only at the deep iterations a slow TC reaches:
  **every fast TC recovers to neutral-or-positive** and **60+0.6 stays clearly positive**
  (combined 63.6%, ≈ +97 Elo — even stronger than M8.A's +73 in this campaign). Net
  swing vs shelved M8.A ≈ **+49 Elo** (−20 → +28.7), entirely from recovering the
  fast-TC losses without sacrificing the slow-TC win.
- **Off-regime byte-identity is the safety mechanism, and it held.** At root_depth ≤ 12
  (all a fast TC completes) `scout_start = MAX` ⇒ every non-first move takes the reference
  full-window path ⇒ byte-identical to `M7.B.2` (proven at bench: d4 `45788` / d7
  `662085`, unchanged from M7.B.2; M8.A's bench was `47763`/`654940`). So the fast-TC
  buckets are protected by construction, not by hope — and indeed they land at ≈ 0.
- **This resolves M8 (Search refinements I) the way the M5.K lesson demanded.** The
  M5.K depth-*band* on aspiration width backfired ("the adaptive benefit is not cleanly
  depth-localizable", −34.9 Elo). M8.A.1 avoided that failure mode with a **monotonic,
  smooth** ramp (no band, no single-ply cliff) whose off-regime is the proven-safe
  baseline. Same playbook as M7.B → M7.B.2, applied in the mirror direction.

## Verification (pre-SPRT gates, all PASS)

- Full lib suite 2098-pass/0-fail; integration suite green (bench-signature pin re-pinned
  47763 → 45788 — the intended M7.B.2 revert, off-regime proof). clippy + fmt clean.
- Bench d4 `45788` / d7 `662085` — **byte-identical to M7.B.2** (ramp inert at bench depth
  ≤ 7 ≤ D0). Determinism anchor; a mismatch is stop-the-line per ADR-0044.
- llvm-cov `search.rs` 97.94% regions / 98.22% lines (≥ the M8.A 97% gate).
- `cargo mutants --in-diff` **0 missed** (15 caught, 4 timeout-caught, 1 unviable — the
  M7.B.2 outcome class).
- Blind review loops all converged: plan APPROVE, test-suite APPROVE, final code+tests
  APPROVE (byte-identity verified structurally via the `lmr_node_eligible` `!is_pv` gate,
  not just by bench).

## Provenance

- Seed 1 (`0xC1ABF15AE10DD00B`): `target/matches/sprt/20260626T064220-M7.B.2-sprt/`
- Seed 2 (`0xC1ABF15AE10DD01B`): `target/matches/sprt/20260626T084936-M7.B.2-sprt/`
- Baseline cache: `target/sprt-baselines/M7.B.2/` (M7.B.2 tag `f8d6746`).

## Disposition

- **M8.A.1 (depth-conditioned PVS): SHIPPED.** New production search HEAD =
  `M6.J` (eval) + `M8.A.1` (search). Supersedes M7.B.2 on the search layer; supersedes
  the shelved M8.A (ADR-0043) for shipping.
- **M8.A (unconditioned PVS) stays shelved** (`m8a-pvs`, not merged) — its record
  [`2026-06-25-m8a-pvs-vs-m7b2.md`](2026-06-25-m8a-pvs-vs-m7b2.md) documents why the
  conditioning was needed.
- Constants `D0=12, BASE=16, SLOPE=4` are a conservative first operating point; a
  sweep/SPSA toward a steeper onset (more of the 60+0.6 +97) is a tuning-backlog
  candidate but **not** required — the shipped point is already net +28.7.

See ADR-0044, [`docs/plans/m8.a.1.md`](../../docs/plans/m8.a.1.md),
[`docs/research/m8.a.1-depth-conditioned-pvs.md`](../../docs/research/m8.a.1-depth-conditioned-pvs.md),
[`docs/milestones/m8.a.1.md`](../../docs/milestones/m8.a.1.md).
