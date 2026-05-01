# ADR-0022 — In-process SPRT mechanics

**Status:** Accepted, 2026-05-01.

## Context

ELOH.E moves the SPRT/match/smoke flows out of fastchess and into the in-process `elo-iterate` harness (see ADR-0012's 2026-05-01 amendment). Several SPRT-mechanics decisions had to be settled in the process. This ADR records them so that a future contributor doesn't re-litigate them silently.

The math reference is `docs/research/eloh.e-pentanomial-sprt.md`. The migration plan is `docs/plans/eloh.e.md`.

## Decisions

### 1. Pentanomial-only

The harness reports SPRT verdicts using the pentanomial GSPRT formulation (per-pair score in {0.0, 0.5, 1.0, 1.5, 2.0}), not trinomial-over-individual-games.

- Pentanomial absorbs the within-pair color-balance correlation; per the reference, ~15% smaller variance and ~8–9% fewer games to a verdict.
- The fishtest infrastructure migrated trinomial → pentanomial circa 2018–2019.
- Rejected: emit both pentanomial and trinomial counts. The only downstream consumer is the SPRT verdict, which uses pentanomial; trinomial output would invite confusion about which is canonical.

### 2. Logistic Elo (no `model=` flag)

`elo0` and `elo1` are interpreted in **logistic Elo**: `expected_score = 1 / (1 + 10^(-Δ/400))`.

- Matches fastchess's `model=logistic` (and its inferred default when `model=` is omitted).
- Rejected: a `--sprt-model {logistic|normalized|bayesian}` flag. Logistic is the field-standard for chess SPRT and is what every reference implementation we surveyed uses. A future contributor who wants normalized Elo writes a separate plan with its own back-test gate.

### 3. Normal-approximation GSPRT (not exact MLE)

The LLR formula uses the normal-approximation GSPRT (`(s1−s0) · (2μ−s0−s1) · N / var / 2`), not the exact multinomial MLE used by `vdbergh/pentanomial`.

- Well-calibrated for pool sizes ≥ 100 pairs — comfortably within ELOH.E's working range.
- Same approximation used by cutechess-cli and fastchess under `model=logistic`.
- The closed-form is small (one expression in `mod sprt::compute_llr`), trivially testable.
- Rejected: a Siegmund overshoot correction. The overshoot at α=β=0.05 is ~0.5% over realised α/β; the field accepts it without correction.

### 4. Pair-cadence LLR check (never per-game)

The harness evaluates LLR exactly once per completed pair, not after every game.

- Per-game evaluation introduces a subtle bias: the pair is not yet complete when game A finishes, so the sample is not i.i.d. at the pair level. This invalidates the α/β guarantees.
- The controller routes per-game scores into a per-`worker_id` buffer (`HashMap<u32, Vec<f64>>`); on `WorkerReport::PairComplete` it drains the 2-element buffer and calls `sprt::update_pair`.
- Per-worker keying (not a single shared buffer) is load-bearing under `concurrency > 1` — two workers' game-A scores would otherwise interleave before either pair completed.

### 5. Singleton handling at `--max-games` boundary

If `--max-games` interrupts mid-pair (game A's `GameComplete` arrived but game B's `GameComplete` did not):

- The orphan game's score is **discarded** from the pentanomial state.
- A `discarded_singletons` audit counter increments. (The counter is recorded in `SprtState` but not surfaced in `summary.txt`; it's available for diagnostics if the count ever matters.)

If both games of a pair completed but the `PairComplete` was not processed (because the loop exited at `--max-games` after the 2nd `GameComplete`), the harness folds the complete pair into the SPRT state at run-end drain. This is a backstop; production SPRT runs that terminate via Wald-bound crossing don't hit this path.

### 6. Startpos-only opening

The harness plays from the standard chess starting position; no opening-book PGN/EPD ingestion.

- Matches the M4.D mixed-TC SPRT campaign (which was startpos-only via fastchess defaults).
- Deferred: `--openings <file>` for PGN or EPD opening-book ingestion. No active consumer (M4.D-class runs and the upcoming M5 search-tuning runs have not asked for one). Will be added when a real consumer asks.

### 7. Separate flags `--sprt-elo0/elo1/alpha/beta` (not parameterized string)

The four SPRT parameters are four separate flags, not one parameterized string like `--sprt elo0=0,elo1=10,alpha=0.05,beta=0.05`.

- Matches the existing harness convention (`--k0`, `--tau`, `--target-sigma`, `--initial-elo` are all separate flags).
- Mirrors fastchess's `-sprt elo0=… elo1=… alpha=… beta=…` form so operators familiar with the fastchess convention transfer cleanly.
- The `--tc-sample` parameterized-string convention from ELOH.D is for a *list* (variable-length); the SPRT config is fixed-arity (always exactly four scalars), so the rationale for parameterization doesn't apply.

### 8. SPRT mode is mutually exclusive with K-update / σ-stopping

The `cli::parse_args` post-loop validation rejects any combination of `--sprt-*` with non-default values for `--k0`, `--tau`, `--target-sigma`, `--stop-window`, `--stop-window-confirm`.

- SPRT compares two binaries at fixed (unknown) Elo via LLR-bound stopping; combining it with a moving K-update estimate is methodologically incoherent.
- The σ-stopping check is also skipped at run time when SPRT is active (defense in depth). σ-stopping firing in SPRT mode would preempt the LLR-bound termination and invalidate the α/β guarantees.
- Loud rejection (rather than silent override) follows ELOH.B's `--k0 0 requires --target-sigma 0` mutex precedent.

### 9. Pentanomial CI emitted always (≥2 pairs)

The post-hoc 95% CI on Δ Elo (from accumulated pair counts via the inverse-logistic transformation) is emitted as a `ci: elo=±N.NN [±N.NN, ±N.NN] pairs=N` line whenever ≥2 pairs completed, regardless of mode.

- In SPRT mode the CI is reported alongside the verdict (separate `sprt:` and `ci:` lines).
- In fixed-games match mode (no SPRT, no K-update, no σ-stopping) the CI is the run's primary numerical output.
- In rating-estimate frozen-anchor mode the CI is informational alongside the K-update estimate.
- When `<2` pairs completed or variance collapses, the line reads `ci: undefined (n=N)` so downstream parsers always have a parsable `ci:` line.

### 10. Combined `match.pgn` at run-end

After all dispatched pairs complete, the harness concatenates `<out-dir>/games/<N>.pgn` files in ascending `game_index` order into `<out-dir>/match.pgn`, separated by a single blank line.

- Per-game PGN files at `<out-dir>/games/<N>.pgn` continue to exist verbatim — the combined file is additive.
- Runs that terminate before any game completes produce an empty `match.pgn` (zero bytes), not a missing file.
- Concatenation is a small (~20 LOC) post-processing step in `controller::write_match_pgn`.

## Consequences

- The SPRT/match/smoke surface is now in-tree; bumping the harness's behaviour is a Rust change reviewable in the same diff as the consumer that motivates it.
- One Rust binary (`elo-iterate`) replaces three behaviorally distinct fastchess invocation modes; the structured `summary.txt` is uniform across modes.
- fastchess remains required only for `scripts/match.sh compliance` — the `--compliance` UCI shake-out has no in-house substitute. `scripts/install-fastchess.sh` stays on disk for that one consumer.
- Future SPRT-mechanics changes (a new `--sprt-model` flag, opening-book ingestion, a Siegmund correction) are separate ADRs with their own back-test gates.

## How to apply

- New SPRT runs invoke `scripts/sprt.sh sprt <baseline-tag>` (unchanged entry point; the script body now drives `elo-iterate` instead of `fastchess`).
- Fixed-games match runs invoke `scripts/sprt.sh match <baseline-tag>` or `scripts/match.sh self-play|vs-stockfish` (same entry points, harness-driven).
- Compliance check: `scripts/match.sh compliance` (still fastchess).
- New back-test gates land atomic with the unit (synthetic Bernoulli + draw-heavy streams in `mod sprt::tests`); the M4.D-replay statistical-equivalence gate is deferred to a manual post-merge run per ELOH.B/C/D precedent.

## Variants considered and rejected

| Variant | Reason rejected |
|---|---|
| Trinomial-over-games SPRT with pentanomial CI alongside | The two outputs would invite confusion about which is canonical; pentanomial is strictly more sample-efficient and is the field standard. |
| `--sprt-model {logistic\|normalized\|bayesian}` flag | No active consumer; logistic is field-standard for chess SPRT and matches fastchess's default behavior. |
| Exact multinomial MLE LLR (vdbergh/pentanomial reference impl) | Closed-form normal approximation is much simpler and well-calibrated for our pool sizes. |
| Per-game LLR check | Invalidates α/β; sample is not i.i.d. at the pair level mid-pair. |
| Single shared `pair_buffer: Vec<f64>` across workers | Aliasing under concurrency >1 would silently corrupt SPRT state. |
| Silently override K-update flags when `--sprt-*` is set | Caller's command line stops being self-documenting; values silently disappear. |
| Combined `match.pgn` only (drop per-game files) | Per-game files are useful for triaging a single bad game; downstream consumers expect them. |
| Mid-LLR diagnostic in the per-pair progress line | Inviting humans to read mid-run LLR encourages premature stopping that invalidates α/β. |
