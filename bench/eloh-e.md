# ELOH.E benchmark

Captured **2026-05-01** on **Apple M4** (4 P-cores + 6 E-cores), **macOS 26.4.1**, **32 GB**. Tooling-track milestone bench — ELOH.E adds zero engine-side code paths (all changes are confined to `src/bin/elo-iterate.rs`, `scripts/sprt.sh`, `scripts/match.sh`, and docs). The engine's `bench` signature is expected to be byte-identical to the current `main` (M4.D end).

Per ADR-0010 and the plan's §12 verification checklist: **node count must be byte-identical to M4.D's `15,863,206`** (the current `main` ancestor).

## Method

```
printf 'bench\nquit\n' | ./target/release/clawfish
```

## Result

```
info string Nodes searched: 15863206
info string bench: 15863206 nodes <NPS> nps
```

Node count: **15,863,206** — byte-identical to the M4.D end signature in `bench/m4.md`.

## Correctness gate — passed

The `bench` signature is unchanged from M4.D's `15,863,206`. The plan's stated failure mode (engine-side perturbation from ELOH.E's harness changes) does not fire. ELOH.E is a pure harness-layer addition — `src/bin/elo-iterate.rs` is the only Rust file modified; no engine-side code path (`src/search.rs`, `src/eval/`, `src/movegen.rs`, `src/tt.rs`, etc.) is touched.

## NPS observation

NPS varies wallclock-to-wallclock and is excluded from the regression signature. The harness changes are out-of-tree from the engine and don't affect engine performance. Not a regression baseline.

## See also

- `bench/m4.md` — load-bearing baseline (M4.D end) the byte-identicality is checked against. ELOH.E is on the same node-count signature lineage as the current `main` after the ELOH.A–D / M4 merge.
- `docs/plans/eloh.e.md` — plan §12 articulates the byte-identicality gate.
- `docs/decisions/0022-eloh-sprt-mechanics.md` — ELOH.E ADR.
- `bench/eloh-d.md` — prior tooling-track bench (was on the M3.F-era node count; ELOH.E sits on the M4-era count after the merge).
