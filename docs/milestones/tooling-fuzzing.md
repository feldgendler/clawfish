# Tooling/fuzzing — coverage-guided fuzz harnesses for FEN + UCI parsers

Status: complete. `fuzz/` independent workspace + two `Arbitrary<String>` harnesses (`fuzz_fen`, `fuzz_uci`) + 35 fen + 30 uci hand-curated seed files + **ADR-0013**. Closes `docs/tooling-backlog.md` item #1; CI (#2) is now top of the active queue. `--sanitizer none` (no fuzzed parser path reaches `unsafe`). Pinned nightly via `fuzz/rust-toolchain.toml`. Policy-isolated: root `cargo audit` / `cargo deny` does not see `libfuzzer-sys`.

Saturation-quality campaigns clean (Apple M4, `--jobs=5`, `-max_total_time=1440` per worker): fuzz_fen 1.35B execs, fuzz_uci 927M execs, 0 crashes. Subsequent `-max_len=4096` smoke (15 min/target): fuzz_fen 750M, fuzz_uci 140M, 0 crashes. One real parser bug found by smoke (the `unreachable!()` in `parse_castling`) and fixed in the same branch.

## What landed

The first coverage-guided fuzzing setup, scoped to the two parsers shipped to date:

- **`fuzz/` independent workspace.** `[workspace] members = ["."]` inside `fuzz/Cargo.toml` (the standard `cargo fuzz init --fuzzing-workspace=true` output) keeps `libfuzzer-sys` out of root `Cargo.lock`. Verified empirically: root `cargo audit` and `cargo deny check` do not see fuzz-side deps even after `cargo fuzz build` materializes `fuzz/Cargo.lock`.
- **Pinned nightly.** `fuzz/rust-toolchain.toml` with `channel = "nightly-2026-04-01"`. Scoped to the `fuzz/` directory; main stable build untouched. Bumping is a one-line edit.
- **Two `Arbitrary<String>` harnesses.** Five lines each. `fuzz_fen` wraps `Position::from_fen`; `fuzz_uci` wraps `parse_uci_line`. Both bind result to `_` (contract = no panic, not a return-value check).
- **`--sanitizer none` decision.** Fuzzed entry points don't reach the engine's `unsafe` site (`src/movegen/move_list.rs:78`, a `MaybeUninit` slice cast in `MoveList::as_slice` reachable only via `generate_moves`). ASan would only check `libfuzzer-sys` transitives at 2× cost. Re-evaluation trigger codified in ADR-0013 §5.
- **Hand-curated seed corpus.** 35 fen + 30 uci files under `fuzz/corpus/<target>/`. FEN seeds: 10 from `UCI_SEED_FENS` (canonical 6 + EP-horizontal-pin + EP-double-check + mate + stalemate) + 25 from `src/fen.rs::tests` covering castling-rights combinations, ep targets on both ranks, varied halfmove/fullmove. UCI seeds: at least one representative for each of the 11 non-Unknown `Command` variants, 9 forms of `go`, 5 forms of `position`, 3 forms each of `setoption` and `register`.
- **Workflow integration.** `docs/workflow.md` "Fuzzing" subsection under "Static analysis and dependency hygiene": cadence (parser change → ≥30-min run; major milestone → 2-hour backstop; not in pre-commit), commands, triage workflow (`tmin` → `#[test]` regression), seed maintenance (manual; no extraction tooling).
- **Bug found by fuzzing.** `parse_castling` had an `unsound unreachable!()` predicated on the (false) assumption that an empty castling field requires 7 split tokens. Inputs with double spaces (`b  k-`) split into 6 tokens with one empty middle, so the gate didn't fire and the empty token reached the panic. Fix: `unreachable!()` → `Err(BadCastlingRights)`. Regression test: `parse_rejects_empty_castling_field_via_double_space`. Caught by fuzz_fen smoke at ~6k runs.

## Implementation highlights

- **Plan-review loop converged in 2 passes.** Pass 1 caught 2 must-fix items: the false "no `unsafe` in the engine" claim (recursive grep finds `src/movegen/move_list.rs:78`; my pass-0 grep was top-level only and missed the subdirectory) and the `cargo mutants --in-diff` gap on feature-gated code. Pass 1 also caught 6 should-fix items including the seed-extractor over-engineering (the strongest cut; rolled the entire feature flag + module + binary back to plain text files in one move) and the `bench/` placement (moved to `docs/research/tooling-fuzzing-results.md`). Pass 2 returned "no further substantive issues" with two low-risk nits.
- **Final-review pass-1 caught a load-bearing manifest bug.** Reviewer empirically read cargo-fuzz upstream's `is_fuzz_manifest` predicate and found that `[package.metadata.cargo-fuzz]` (empty subtable) **fails** the check; cargo-fuzz requires `cargo-fuzz = true` (boolean key under `[package.metadata]`). My pass-1-nit fix to the plan had gone the wrong direction. Fixed before any cargo-fuzz invocation; would otherwise have blocked every fuzz command.
- **`--fuzzing-workspace=true` policy isolation conclusively verified.** Acceptance criterion #3 added during plan review: after `cargo fuzz build` materializes `fuzz/Cargo.lock`, root `cargo audit` still scans 109 deps with no `libfuzzer-sys` reference; `fuzz/Cargo.lock` exists separately with 2 references. Empirical confirmation that the workspace-isolation flag survives the path-dep on the parent crate.
- **Test-suite-review loop skipped** per `docs/workflow.md` §"Test-suite review loop". Justification: zero LOC of new in-tree Rust source under `src/` (the slice deliverable is harnesses + corpus + docs); the fuzz harnesses themselves (5 LOC × 2) are exercised continuously by libFuzzer, not by `cargo test`. Regression tests for any future fuzzing-found bugs enter the standard `src/<file>.rs::tests` suite via the existing convention; no separate review pass needed.
- **Seed corpus is curated, not derived.** Initial draft used a `[features] fuzz-seeds = []` cargo feature + `src/fuzz_seeds.rs` module + `fuzz/seed-corpus/main.rs` extraction binary. Plan-review pass-1 flagged this as over-engineered for hand-picked literal strings — static text files are simpler, eliminate the orphan-on-delete problem, and avoid the `cargo mutants --features` follow-up question on feature-gated code. Switched to plain-file `fuzz/corpus/<target>/<name>` entries.
- **Discarded alternatives** (recorded in ADR-0013 + plan §15):
  - **`bench/tooling-fuzzing.md` for results.** ADR-0010 reserves `bench/` for performance baselines. Fuzzing campaign metrics (corpus size, exec/s, libFuzzer's `cov:`/`ft:`) aren't perf. Moved to `docs/research/tooling-fuzzing-results.md`.
  - **`arbitrary-derive` feature on `libfuzzer-sys`.** Final-review pass-1 caught: `Arbitrary<String>` impl is in the `arbitrary` crate proper (always pulled in by `libfuzzer-sys`), not behind a feature flag. The feature only enables the `#[derive(Arbitrary)]` proc-macro for custom types we don't have. Dropped; documented re-enable trigger in `fuzz/Cargo.toml`.

## Verification (Apple M4, dev machine)

| Metric | Result |
|---|---|
| `cargo +nightly fuzz build` | clean (both targets) |
| `cargo +nightly fuzz list` | `fuzz_fen` + `fuzz_uci` |
| Root `cargo audit` post-`fuzz build` | 109 deps scanned, 0 advisories, no `libfuzzer-sys` reference (policy isolation conclusively verified) |
| Root `cargo deny check` | advisories ok, bans ok, licenses ok, sources ok |
| Main-crate tests post-fix | 636 passed (+1 regression test), 12 ignored, 0 failed |
| Saturation campaign — `fuzz_fen` | 1,349,880,712 runs, 24 min wall-clock, exec/s 180–195k per worker, cov: 228, ft: 486, **0 crashes** |
| Saturation campaign — `fuzz_uci` | 926,921,812 runs, 24 min wall-clock, exec/s 137–142k per worker, cov: 434, ft: 1759, **0 crashes** |
| `-max_len=4096` smoke — `fuzz_fen` | 749,971,601 runs, 15 min wall-clock, ft 496 (+10 vs 200-byte cap), 0 crashes |
| `-max_len=4096` smoke — `fuzz_uci` | 140,397,202 runs, 15 min wall-clock, ft 2238 (+479 vs 200-byte cap), 0 crashes |

Aggregate: **3.17 billion executions** across the campaigns + smokes, 0 unaddressed crashes after the parse_castling fix. Coverage saturated within ~2s of each campaign; the long tail traversed comparison-signal space without expanding `cov`.

## What's next for fuzzing

- Per-major-milestone backstop per ADR-0013 §9: another 2-hour-per-target campaign at M3 close.
- Future `fuzz_fen_roundtrip` target (`from_fen → to_fen → from_fen` equality) — different bug class than no-panic; deferred until justified.
- Domain-aware `Arbitrary` for FEN/UCI grammar — bigger effort; probably waits for OSS-Fuzz / CI integration (backlog #2).
- `arbitrary-derive` feature re-enabled when a future fuzz target adds a custom-derive type.
