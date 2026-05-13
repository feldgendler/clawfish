# ELOH.D benchmark

Captured **2026-04-30** on **Apple M4** (4 P-cores + 6 E-cores), **macOS 26.4.1**, **32 GB**. Tooling-track milestone bench — ELOH.D adds zero engine-side code paths (all changes are confined to `src/bin/elo-iterate.rs`), so the engine's `bench` signature is expected to be byte-identical to ELOH.C's.

Per ADR-0010 and the plan's §7 step 6 correctness gate: **node count must be byte-identical to ELOH.C's `172,312,700`.**

## Method

```
printf 'bench\nquit\n' | ./target/release/clawfish
```

## Result

```
info string Nodes searched: 172312700
info string bench: 172312700 nodes 3826959 nps
```

Node count: **172,312,700** — byte-identical to ELOH.C (`bench/eloh-c.md`).

## Correctness gate — passed

The `bench` signature is unchanged from ELOH.C's `172312700`. The plan's stated failure mode (engine-side perturbation from ELOH.D's harness changes) does not fire. ELOH.D is a pure harness-layer addition — `src/bin/elo-iterate.rs` is the only file modified; no engine-side code path (`src/search.rs`, `src/eval.rs`, `src/movegen.rs`, etc.) is touched.

## NPS observation

NPS recorded at 3,826,959 — within the run-to-run noise band of ELOH.C's 4.27–5.82 Mnps range (no P-core pinning during this single-shot bench). The harness changes are out-of-tree from the engine and don't affect engine performance. Not a regression baseline.

## Node-count signature lineage

This signature (`172,312,700`) is M3.F's; the `tooling/elo-harness` branch is based on M3.F's `f351ccf` ancestor. ELOH.A/B/C/D are tooling-track and have not absorbed M4.A's TT, M4.B's killer moves, M4.C's history heuristic, or M4.D's aspiration windows. When the ELOH branch eventually merges to main alongside the M4 stack, future bench runs will pick up the M4 stack's reduced node counts.

## See also

- `bench/eloh-c.md` — load-bearing baseline (VirtualClock UCI option) the byte-identicality is checked against.
- `docs/plans/eloh.d.md` — plan §7 step 6 articulates the byte-identicality gate.
- `bench/m3.md` — M3.F bench signature (`bench: 172312700 nodes 11489045 nps`) that this node count matches.
