# Post-mortem — corpus-mixture meta-tune campaign (aborted 2026-05-26)

**Status: ABORTED. No ship, no Elo claim.** An attempted post-M6.I tuning
campaign — *not* a milestone. The data-infrastructure and harness work it spun
off **did** land and is CI-green (see "What landed"); the meta-tune itself was
killed without completing and is not being relaunched. This document captures
what was attempted, why it failed to produce a result in any reasonable time,
the fixes that close the gaps, and the preconditions for a future overnight
re-run (also recorded in [`../tuning-backlog.md`](../tuning-backlog.md)).

## Goal

Beat the shipped `M6.I` eval via a **corpus-mixture meta-tune**: rebuild the
four-lane corpus under new *dedup-against-committed (II)* semantics (richer
corpus + bit-identical, non-wasting extend), then run `texel-tune mixture` (a
bi-level Nelder–Mead simplex over the four lane proportions, inner Adam Texel
solve per candidate), then confirm the winner with a mixed-TC SPRT vs `M6.I`.
Ship only if it beat `M6.I`'s +93.86 Elo.

## What landed (kept — all CI-green, bench-neutral)

The campaign's infrastructure work is sound and shipped on `main`:

- **`25b943e`** — corpus *dedup-against-committed (II)* + bit-identical,
  non-wasting lane extend (`corpus fetch`/`corpus selfplay` extend drivers;
  stable game ids, prefix skip-by-id, target-as-total, boundary-game
  drop+re-derive). ADR-0035 v2 / ADR-0036. Proven bit-identical for a fetch lane
  **and** a self-play lane (`tests/corpus_extend.rs`, `tests/corpus_fetch.rs`).
- **`c8f2cb7`** — suspend-tolerant elo-iterate watchdog (no lid-close false
  positive). Compares the requested channel wait against the monotonic,
  suspend-excluding `Instant` clock; a short-fall by > `SUSPEND_SLACK` is a
  suspend (re-loop), not a hang.
- **`5caf723`** — `texel-tune mixture` observability: per-epoch + per-inner-tune
  progress logging + an atomic `<out-params>.progress` status file.

The full 16M-position corpus (4 lanes × 4M) was rebuilt under (II) and passed
the quality gate (Phase B complete ~08:36 local).

## Timeline (2026-05-26, local EEST)

- **~08:40** — Phase C launched: `texel-tune mixture --lanes {onbook,offbook,ccrl,lichess}
  --out-params bench/m6i-meta-params.json --seed 20260524 --max-iter 1000
  --eval-every 10 --patience 5`, at background QoS, on the **pre-observability
  binary** (the running binary could not be rebuilt without SIGBUS-ing it, so it
  predated `5caf723`).
- **08:40 → ~17:30** — ran single-threaded for **~8.85 h** (≈514 CPU-min),
  healthy, with **zero output** (old binary, no per-epoch log) and **no
  checkpoint**. No reliable progress read was possible: an lldb attach confirmed
  it was live and in the reflection loop, but the opt3+LTO+`debug=false` binary
  exposed no debug-visible iteration counter.
- **~17:30** — aborted per user directive ("post-mortem; do not relaunch"). The
  meta-tune never wrote `m6i-meta-params.json`.

## Root causes

1. **The inner optimizer re-streamed the entire on-disk cache every epoch.**
   `optimizer::tune` rebuilt the train/val record vectors from the 2.6 GB cache
   file (`stream_chunks` + a `HashSet` membership filter) on *every* epoch — once
   for the gradient and once per held-out eval. A single `M6.I`-style tune
   tolerated this (one tune, modest epochs); the meta-tune amplified it to
   ~17 inner tunes × up to 1000 epochs ≈ **~18,000 full passes over a 2.6 GB
   cache**. The redundant deserialize/alloc dominated; the actual gradient math
   was a small fraction. **This was the headline bug.**

2. **Full-batch loss is too smooth for patience-5 to fire.** The inner solve is
   full-batch Adam: the held-out loss decreases monotonically and minutely for a
   very long time, so the `val + 1e-12 < best_val` improvement test keeps
   succeeding → the patience counter rarely reaches 5 → each inner tune ran
   *toward* `max_iter=1000`. (The integer-quantization-floor stop is likewise
   slow to trigger on a large corpus where weights drift across int boundaries
   gradually.) The two early-stop mechanisms that make a *single* tune terminate
   quickly are weak on a large full-batch corpus.

3. **Launched blind.** The observability commit (`5caf723`) existed but was
   landed *after* the run started, on a binary that could not be hot-swapped.
   The run produced no log line and no status file for its entire 9 h.

4. **No checkpointing in the mixture path.** `simplex_search` passed
   `checkpoint_path: None`; the simplex had no resume. A kill at hour 9 forfeited
   everything — exactly the worst-case for a long unattended run.

## Fixes landed in this post-mortem

- **(1) → fixed.** `optimizer::tune` now deserializes the train + held-out
  records into memory **once** (`load_split_records`, a single streaming pass)
  and iterates them in memory across all epochs. Behaviour-preserving **by
  construction** — the gradient/loss see the identical records in the same cache
  order (train/val disjoint by the group-by-game split). The unchanged
  `optimizer::resume_equals_uninterrupted` (bit-identity within the new
  implementation) and `adam_recovers_known_optimum` tests continue to pass.
  Expected order-of-magnitude speedup on the
  inner solve; bench-neutral (tuning harness only).
- **(4) → fixed.** `mixture::simplex_search` now writes an atomic
  `<out-params>.mixckpt` checkpoint after **every** inner tune and resumes from
  it, skipping completed inner tunes and recomputing reflections from the
  persisted simplex. Encoded in a length-framed **binary** layout (each `f64` by
  its bit pattern + CRC) — *not* JSON, because serde_json's shortest-decimal
  float serialization is not bit-exact for every `f64` and a ≤1-ULP drift would
  let a resumed search pick a different worst/best vertex (caught by the new
  `simplex_resume_equals_uninterrupted` test). Inner-tune granularity is
  sufficient now that fix (1) makes an inner tune cheap (a kill loses at most the
  one in-flight tune, which restarts from scratch).
- **(3) → already landed** (`5caf723`): per-epoch + per-inner-tune log + the
  `<out-params>.progress` status file. The lesson is procedural: **build the
  observability into the binary before launching a multi-hour run.**

## Still open before an overnight re-run (recommendations)

- **(2) early-stop on full-batch.** Either (a) a *relative*-improvement patience
  threshold instead of the absolute `1e-12`, (b) a much lower `--max-iter` for
  the meta-tune's inner tunes (e.g. 100–200; the warm-start from `M6.I` is
  already near-optimal, so inner tunes need few epochs), and/or (c) minibatch /
  subsample the cache so the loss carries enough noise for patience to mean
  something. **Recommendation: cap `--max-iter` low for the meta-tune** — it is
  the cheapest lever and the warm start justifies it. Validate that the chosen
  mix is unchanged vs a longer run on a small corpus before trusting it.
- **Parallelize the seed vertices.** The five seed-vertex inner tunes are
  independent (the reflections are inherently sequential). Running them across
  cores would cut wall-clock further. Not done (kept the search single-threaded
  for determinism simplicity); a deterministic parallel seed phase is a clean
  future addition.
- **Re-run config** (after the above): launch on a binary built *with* the
  observability + checkpoint code, at background QoS + `nice`, with a low
  `--max-iter`, and watch `<out-params>.progress` / the `.mixckpt`. A kill or
  suspend is now recoverable.

## Lessons

1. **An outer loop multiplies every inner inefficiency.** The per-epoch
   re-streaming was invisible at one tune and catastrophic at ~17. Profile the
   inner unit before wrapping it in a search.
2. **Observability and checkpointing are preconditions for an unattended
   multi-hour run, not afterthoughts.** Both must be in the binary you launch.
3. **Full-batch convergence heuristics don't transfer to large corpora.**
   patience/quant-floor stops calibrated on a small tune ran to `max_iter` here.
4. **Bit-exactness needs bit-pattern encoding.** JSON float round-trip is ≤1 ULP
   lossy; the optimizer checkpoint already knew this — the meta-tune checkpoint
   had to learn it too (a failing test, then the binary codec).

## Cross-references

- [`m6.i.md`](m6.i.md) — the shipped baseline this campaign tried to beat.
- ADR-0035 (§ (II) v2 + extend) / ADR-0036 (on-demand ingestion) / ADR-0037
  (Texel harness; `tune` checkpoint discipline).
- [`../tuning-backlog.md`](../tuning-backlog.md) — "corpus-mixture meta-tune
  re-run preconditions" entry.
- `src/texel/optimizer.rs` (`load_split_records`, the perf fix) /
  `src/texel/mixture.rs` (`MixCheckpoint`, `simplex_search` resume).
