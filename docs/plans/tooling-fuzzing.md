# Plan: Fuzzing harnesses for FEN + UCI parsers

Tooling slice. Implements `docs/tooling-backlog.md` item #1. Research: `docs/research/tooling-fuzzing.md`.

## 1. Goal

Stand up coverage-guided fuzzing for the two string→AST parsers — FEN (`Position::from_fen`) and
UCI (`parse_uci_line`) — and run a real campaign against each. Convert any crashes into regression
tests in the standard suite. Codify the choice in an ADR; document the cadence in `workflow.md`.
Mark backlog #1 done.

## 2. Decisions locked in

| Decision | Choice | Rationale |
|---|---|---|
| Tool | `cargo-fuzz` (libFuzzer) | Only coverage-guided option on Apple Silicon (research §2). |
| Workspace | `cargo fuzz init --fuzzing-workspace=true` (independent) | Isolates `libfuzzer-sys` from root `cargo audit` / `cargo deny` policy (research §3.5). Verified empirically in §11. |
| Nightly pin | `fuzz/rust-toolchain.toml` with `channel = "nightly-2026-04-01"` | Reproducibility; bump in a follow-up (research §3.4 + §11.7). |
| Harness input | `Arbitrary<String>` (via `libfuzzer-sys` feature `arbitrary-derive`) | Both parsers take `&str`; mutation coherence preserved (research §4.3). |
| Sanitizer | `--sanitizer none` | See §3 "Sanitizer rationale." |
| Per-target `max_len` | `200` bytes | FEN ~30–80, UCI ~10–200 (research §11.3). |
| Targets | `fuzz_fen` and `fuzz_uci` only | `Move::from_uci` requires a `Position` arg; defer until structure-aware Position generation is justified. |
| Seed corpus | **Static, hand-curated, committed text files** under `fuzz/corpus/<target>/` | Seeds are literal strings copied from existing test fixtures, not auto-derived; no feature flag, no extraction binary, no orphan-on-delete problem. Simpler than the abandoned binary approach (see §13). |
| Run budget for slice | 2 CPU-hours per target | Saturation typically ≤ 1h for these grammars (research §6.2). |
| Cadence | Re-run on parser change; full backstop per major milestone; not in pre-commit | Confirmed in research §9.2. |
| Crash → regression | `cargo fuzz tmin` → embed minimized input as `#[test]` in `src/fen.rs::tests` or `src/uci.rs::tests` | Brings the bug into `cargo test` permanently (research §7.4). |

## 3. Sanitizer rationale

`--sanitizer none` is correct for **this slice's two fuzz targets**, but **not** because the engine has no `unsafe` (it does — `src/movegen/move_list.rs:78` uses `unsafe { ... &*(... as *const [Move]) }` for a `MaybeUninit` slice cast). The argument is narrower:

- The fuzz targets call `Position::from_fen` and `parse_uci_line`. Neither of those entry points reaches movegen — `from_fen` calls `crate::fen::parse` → `validate_post_parse` (mailbox + bitboard consistency only); `parse_uci_line` is a pure-string parser by contract.
- ASan would therefore only check `libfuzzer-sys`'s own `unsafe` (irrelevant) and the parsers themselves (no `unsafe`).
- Cost is ~2× throughput; benefit is zero.

**Re-evaluation trigger** (recorded in ADR-0013): any change that lets a fuzzed parser path reach an `unsafe` block — for example, if `validate_post_parse` is extended to call `generate_moves` for legality checks, or if a future fuzz target wraps a movegen-consuming function. At that point, run at least one campaign with default sanitizer and decide.

## 4. Files created / modified

### New files

- `fuzz/Cargo.toml` — fuzz workspace manifest with `[[bin]]` for `fuzz_fen` and `fuzz_uci`.
- `fuzz/rust-toolchain.toml` — `channel = "nightly-2026-04-01"`.
- `fuzz/.gitignore` — `target/`, `artifacts/`.
- `fuzz/fuzz_targets/fuzz_fen.rs` — 5-line harness.
- `fuzz/fuzz_targets/fuzz_uci.rs` — 5-line harness.
- `fuzz/corpus/fuzz_fen/<NN-name>` — static seed files (committed). ~30–60 entries hand-curated from `UCI_SEED_FENS` (`src/mov.rs::tests:2567`) plus the canonical 6 perft positions.
- `fuzz/corpus/fuzz_uci/<NN-name>` — static seed files (committed). ~30–60 entries hand-curated from string literals in `src/uci.rs::tests` (one-line literals only).
- `docs/decisions/0013-fuzzing-strategy.md` — ADR.
- `docs/research/tooling-fuzzing-results.md` — campaign result summary (per-target: corpus size at start/end, libFuzzer's `cov:`/`ft:` final values, max throughput exec/s, total runs, crashes, RSS peak). **Not** in `bench/` per ADR-0010 (which is for performance baselines).

### Modified files

- `docs/workflow.md` — add a "Fuzzing" subsection under "Static analysis and dependency hygiene" with cadence + commands.
- `docs/tooling-backlog.md` — move item #1 (Fuzzing) from active queue to "Done"; section #2 (CI) is now the top of the active queue.
- `.gitignore` (root) — `fuzz/target/`, `fuzz/artifacts/`.

### Files explicitly NOT created

- No `src/fuzz_seeds.rs` module.
- No `[features] fuzz-seeds` in root `Cargo.toml`.
- No `fuzz/seed-corpus/main.rs` extraction binary.
- Seeds are hand-curated text files. Adding a seed = `vim fuzz/corpus/fuzz_uci/uci-NNN`. Removing = `git rm`. No tooling.

## 5. Type and signature surfaces

### `fuzz/Cargo.toml`

```toml
[package]
name = "chess-fuzz"
version = "0.0.0"
edition = "2021"
publish = false

[package.metadata]
cargo-fuzz = true

[dependencies]
libfuzzer-sys = "0.4"
chess = { path = ".." }

[[bin]]
name = "fuzz_fen"
path = "fuzz_targets/fuzz_fen.rs"
test = false
doc = false
bench = false

[[bin]]
name = "fuzz_uci"
path = "fuzz_targets/fuzz_uci.rs"
test = false
doc = false
bench = false
```

### Fuzz harnesses (verbatim)

```rust
// fuzz/fuzz_targets/fuzz_fen.rs
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|s: String| {
    let _ = chess::Position::from_fen(&s);
});
```

```rust
// fuzz/fuzz_targets/fuzz_uci.rs
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|s: String| {
    let _ = chess::parse_uci_line(&s);
});
```

Both bind the result to `_` (intentional discard). Contract = no panic / no debug-assert / no OOM / no timeout. The `init` callback parameter to `fuzz_target!` is intentionally not used (no logger or global state needed).

## 6. Order of operations

### Phase 1 — workspace + harnesses

1. `cargo install cargo-fuzz` (operator-side). Documented in §9 `workflow.md` edits.
2. `cargo +nightly fuzz init --fuzzing-workspace=true` from repo root.
3. Edit `fuzz/Cargo.toml` **additively** — preserve `cargo fuzz init`'s emitted `[package.metadata]` block with `cargo-fuzz = true` (the boolean key form, NOT the empty `[package.metadata.cargo-fuzz]` subtable form — cargo-fuzz's `is_fuzz_manifest` predicate matches `cargo-fuzz = true` as a `Value::Boolean(true)`, which fails on a table value). Add `[[bin]]` entries per §5, rename the auto-generated target to `fuzz_fen`. Do **not** enable the `arbitrary-derive` feature on `libfuzzer-sys` — `Arbitrary<String>` is in the `arbitrary` crate proper, no feature required. Do not overwrite the manifest wholesale; §5's example is reference, not literal.
4. Write `fuzz/rust-toolchain.toml`.
5. Write `fuzz/.gitignore`.
6. `cargo +nightly fuzz add fuzz_uci`.
7. Replace harness bodies with the §5 versions.
8. Verify build: `cd fuzz && cargo +nightly fuzz build`. Expected: clean build, two artifact binaries.
9. Verify list: `cd fuzz && cargo +nightly fuzz list`. Expected output: two lines, `fuzz_fen` and `fuzz_uci`.
10. **Verify policy isolation:** run `cargo audit` and `cargo deny check` from repo root. Expected: no `libfuzzer-sys` and no fuzz-workspace transitives appear in the output. Also verify `fuzz/Cargo.lock` exists as a separate file from root `Cargo.lock`.

### Phase 2 — seed corpus

1. Hand-curate `fuzz/corpus/fuzz_fen/` files. Source: `UCI_SEED_FENS` (`src/mov.rs::tests:2567`), canonical 6 perft positions, plus a handful of edge-case FENs from `src/fen.rs::tests`. Filename convention: `fen-<NNN>` where NNN is a 3-digit zero-padded index.
2. Hand-curate `fuzz/corpus/fuzz_uci/` files. Source: positive-case string literals from `src/uci.rs::tests`. Filename convention: `uci-<NNN>`.
3. Spot-check a few files: `wc -l fuzz/corpus/fuzz_fen/fen-000` should be 0 or 1 (single line, optional trailing newline); `cat fuzz/corpus/fuzz_uci/uci-005` should be a recognizable UCI line.
4. Target totals: ≥ 30 fen seeds, ≥ 20 uci seeds. (Lower bounds — more is fine.)

### Phase 3 — campaign

1. **Smoke (~10 seconds each).** `cd fuzz && cargo +nightly fuzz run --sanitizer none fuzz_fen -- -runs=1000 -max_len=200` and same for `fuzz_uci`. Expect: completes without crash; non-zero coverage growth from baseline.
2. **FEN campaign (2 hours).** `cargo +nightly fuzz run --sanitizer none fuzz_fen -- -max_len=200 -max_total_time=7200 -print_final_stats=1`. Capture libFuzzer final-stats line.
3. **UCI campaign (2 hours).** Same as #2 with `fuzz_uci`. Run sequentially after FEN (single-CPU; no parallel speedup).
4. **Triage.** Inspect `fuzz/artifacts/<target>/` after each campaign. For each `crash-*` / `oom-*` / `timeout-*`:
   - `cargo +nightly fuzz tmin <target> fuzz/artifacts/<target>/<artifact>`.
   - Read minimized input; classify:
     - **Real bug in parser** → fix the parser; add a `#[test]` regression to `src/fen.rs::tests` or `src/uci.rs::tests` containing the minimized input as a string literal. Re-run target until clean.
     - **OOM with absurd input length** → confirm `-max_len=200` was set; if so, parser should be O(n) — investigate.
     - **Timeout > 1200 s** → unlikely for a parser; investigate.
   - Slice does not ship until each artifact is closed.
5. **Document.** Write `docs/research/tooling-fuzzing-results.md` per §7.

### Phase 4 — docs + ADR

1. Write `docs/decisions/0013-fuzzing-strategy.md`. Sections: Context, Decision (cargo-fuzz, --fuzzing-workspace, --sanitizer none, Arbitrary<String>, pinned nightly), Re-evaluation triggers, Consequences. References ADR-0002 (Apple Silicon) as the constraint that eliminates AFL++.
2. Update `docs/workflow.md`: add "Fuzzing" subsection under "Static analysis and dependency hygiene". Content per §9 below.
3. Update `docs/tooling-backlog.md`: move #1 to "Done".
4. Update `.gitignore` (root): `fuzz/target/`, `fuzz/artifacts/`.

## 7. Campaign-results doc format

`docs/research/tooling-fuzzing-results.md` table:

| Target | seed corpus size | runs | exec/s peak | cov:final | ft:final | RSS peak | crashes |
|---|---|---|---|---|---|---|---|
| fuzz_fen | … | … | … | … | … | … | … |
| fuzz_uci | … | … | … | … | … | … | … |

Plus per-crash subsection (if any): minimized input, root cause, fix commit, regression test name. Note that libFuzzer's `cov:` is libfuzzer-internal (basic blocks instrumented), not the same metric as `cargo llvm-cov`; the latter remains authoritative for unit-test coverage.

## 8. Test coverage strategy

The slice has **no in-tree code-with-assertable-behavior**. The deliverables are: harnesses + corpus + docs. Per `docs/workflow.md` §"Test-suite review loop":

- **Test-suite-review loop is skipped.** Justification: zero-LOC of new in-tree Rust source. The fuzz harnesses themselves (5 lines × 2) are exercised continuously by libFuzzer; correctness is a build-passes / runs-without-immediate-crash check, not a test-assertion check.
- **Regression tests added in Phase 3 step 4** (if any crashes are found and fixed) live in existing `src/fen.rs::tests` / `src/uci.rs::tests` modules. They follow the existing test conventions in those modules; no separate review pass needed (each is a one-line `let _ = parse(...)` style assertion against a minimized literal).
- **Plan-review loop runs (this loop).** **Final-review loop runs** at end of Phase 3, before commit, on the entire slice (harness sources, seed corpus diff, ADR, docs).

## 9. `docs/workflow.md` "Fuzzing" subsection — bullets

Single bullet per claim (drafted only as scaffolding here; Phase 4 writes the actual prose):

- Targets: `fuzz_fen`, `fuzz_uci`. Both `Arbitrary<String>` harnesses.
- Cadence: parser change → ≥30-min run on the affected target. Major milestone → 2h backstop on each target. Not in pre-commit.
- Install: `cargo install cargo-fuzz`. Nightly auto-selected inside `fuzz/` via `rust-toolchain.toml`.
- Run: `cd fuzz && cargo +nightly fuzz run --sanitizer none <target> -- -max_len=200 -max_total_time=<seconds>`.
- Triage: `cargo +nightly fuzz tmin <target> <artifact>` → embed minimized literal as a `#[test]` in `src/fen.rs::tests` or `src/uci.rs::tests` → fix parser → re-run.
- Seed maintenance: edit files under `fuzz/corpus/<target>/` directly; `git rm` to remove. No extraction tooling.

## 10. Parallelization map

| Subtask | Parallelizable? |
|---|---|
| Phase 1 (workspace + harnesses) | No — foundation. |
| Phase 2 (seed curation) | No — manual curation; one operator. |
| Phase 3 (campaigns) | **No** — 2h × 2 sequential. Running both in parallel doubles CPU contention without saving operator wall-clock attention. ADR drafting (Phase 4 step 1) can interleave with the wall-clock of campaigns since libFuzzer doesn't need orchestrator attention; this is interleaved-by-orchestrator, not parallel-coder. |
| Phase 4 (docs/ADR/backlog/.gitignore) | Trivially parallel across coders, but each subtask ≤ 30 lines; coordination overhead exceeds parallel speedup. **Single coder.** |

**Recommendation: single-coder execution end-to-end.**

## 11. Acceptance criteria

- [ ] `cd fuzz && cargo +nightly fuzz build` clean.
- [ ] `cd fuzz && cargo +nightly fuzz list` outputs both targets.
- [ ] `cargo audit` and `cargo deny check` from repo root: no `libfuzzer-sys` or fuzz-workspace deps appear (policy-isolation guard).
- [ ] `fuzz/Cargo.lock` exists as a separate file from root `Cargo.lock`.
- [ ] `fuzz/corpus/fuzz_fen/` has ≥ 30 files; `fuzz/corpus/fuzz_uci/` has ≥ 20 files; all files are valid UTF-8.
- [ ] 2-hour `fuzz_fen` campaign: 0 unresolved crashes (any crash → regression test added + parser fix + clean re-run).
- [ ] 2-hour `fuzz_uci` campaign: 0 unresolved crashes, same standard.
- [ ] `cargo build --release`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, `cargo test` all clean on the main crate.
- [ ] `docs/research/tooling-fuzzing-results.md` written per §7.
- [ ] `docs/decisions/0013-fuzzing-strategy.md` written.
- [ ] `docs/workflow.md` Fuzzing subsection added.
- [ ] `docs/tooling-backlog.md` item #1 moved to "Done".
- [ ] `.gitignore` excludes `fuzz/target/` and `fuzz/artifacts/`.
- [ ] CLAUDE.md status section updated (analogous to M2.E's writeup style).
- [ ] Final-review-loop convergence on code + docs + corpus jointly.

## 12. Dependencies on other units

- **Depends on:** M2.B shipped (UCI parser stable) and M1.D shipped (FEN parser stable). Both done.
- **Independent of M3:** tooling slice. Can ship before, after, or in parallel with M3.A.

## 13. Risks and unknowns

| Risk | Mitigation |
|---|---|
| Pinned nightly date breaks at install time | `nightly-2026-04-01` is 4 weeks old at slice-start; operator can bump in a follow-up if it fails to install. |
| Crashes found, unbounded scope creep | Hard cap: > 5 distinct bugs → pause and re-scope. Realistically the parsers are well-tested (M1.D: 446 tests; M2.B: 542 tests at 99.9% line coverage); ≤ 1 crash expected. |
| `cargo fuzz tmin` produces still-non-minimal output | Re-run `tmin` on its own output; or hand-shrink. Document in regression test. |
| Seed file maintenance friction (e.g., adding 5 new fixtures to `src/uci.rs::tests` without updating `fuzz/corpus/fuzz_uci/`) | Documented expectation in `workflow.md`: not auto-synced. Coverage-guided fuzzing discovers the same edge cases regardless; missing a manually-mirrored seed reduces ramp-up speed, not eventual coverage. |
| `--fuzzing-workspace=true` regression on a future cargo-fuzz upgrade | Acceptance criterion #3 (root `cargo audit` does not see fuzz deps) catches this. |

## 14. Out of scope

- `Move::from_uci` fuzzing — needs a generated `Position`; defer.
- OSS-Fuzz integration — needs public repo.
- CI-driven fuzzing — depends on backlog #2 (CI).
- MSan / TSan / UBSan — research §8.3; none apply here.
- bolero — research §10.4; cargo-fuzz already covers this.
- Differential fuzzing — ADR-0003 source-code-reading restriction blocks the obvious comparator.

## 15. Discarded alternatives (record-only)

- **Seed-corpus extraction binary + feature-gated `src/fuzz_seeds.rs`.** Initial draft. Reviewer flagged as over-engineered for *curated* (not derived) seeds, where adding a Rust binary + feature flag + module gate just to write 60 strings to disk is more machinery than the problem requires. Static text files are simpler, eliminate the orphan-on-delete issue, eliminate the `cargo mutants --features` follow-up, and remove the test-suite-review skip ambiguity.
- **`bench/tooling-fuzzing.md`.** Initial draft. Reviewer flagged as ADR-0010 violation — `bench/` is for *performance* baselines, not fuzzing campaigns. Moved to `docs/research/tooling-fuzzing-results.md`.
