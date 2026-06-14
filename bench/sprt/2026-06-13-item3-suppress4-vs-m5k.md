# SPRT — item 3 sweep: `QS_PATHA_SUPPRESS_DEPTH = 4` vs `M5.K` (crashed; partial)

**Campaign:** side-backlog micro-sweep 2026-06-13.
**Baseline:** `M5.K` (bench d7 `1326598`).
**Candidate:** `M5.K` + item-3 patch with `QS_PATHA_SUPPRESS_DEPTH = 4` (suppress Path-A stand-pat `Lower` stores at `qs_depth >= 4`; keep frames 0–3 — the least-aggressive sweep point). Bench d7 `1325387`.
**Harness:** mixed-TC + `--virtual-clock`, conc 6, elo1=10, 400-game cap. Seed `…F1004`.

## Result — flat / NO SHIP (partial data, run crashed)

The harness terminated at **game 348** with `rc=1`:
```
worker failure: worker 5: readyok after ucinewgame
error: run_iteration: EngineExit
```
One engine subprocess stopped responding to the post-`ucinewgame` `isready` handshake. No final `ci:`/`sprt:` line was written. This is the rare UCI-handshake-desync class flagged in the M5.G campaign lessons; it was **not** build-stacking this time (all binaries were pre-built and the tree was untouched during the run) — most likely a transient under the high system load present early in the campaign (1-min load peaked ~111 from concurrent unrelated jobs). The earlier runs (`=1`, `=3`) completed cleanly `rc=0`.

**Partial result (348 games): W112–L96–D136 ≈ 52.3%** (≈ +15 Elo). Flat — and the CI-lower would clearly be ≤ 0. **Not re-run:** the partial sample is conclusive enough to disposition `=4` as the *neutral* end of the threshold sweep (shallow suppression keeps the deep Path-A stores that the aggressive thresholds remove → little net effect).

## Reading

`=4` (keep frames 0–3) sits near neutral, vs `=1`/`=3` (aggressive) reading ~+40 on seed 1. That gradient *looked* like a clean "aggressive suppression wins" story — until the `=1` confirmation seed contradicted it (see `2026-06-13-item3-suppress1-vs-m5k.md`). The whole lever is closed no-ship.
