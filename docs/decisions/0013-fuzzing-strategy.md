# ADR-0013 — Fuzzing strategy: cargo-fuzz with isolated workspace

**Status:** Accepted, 2026-04-28.

## Context

Two string→AST parsers ship to date — `Position::from_fen` (M1.D) and `parse_uci_line` (M2.B) — that ingest external input and must not panic. Existing tests (unit + property via `proptest`) cover spec'd cases but sample randomly from generator distributions; they do not discover input sequences that unlock new code paths via instrumentation feedback.

`docs/tooling-backlog.md` item #1 ("Fuzzing") is the prioritized next tooling slot. `docs/research/tooling-fuzzing.md` settles the prior-art choices. This ADR ratifies the commitments that flow from that research.

## Decision

### 1. Tool: `cargo-fuzz` (libFuzzer backend)

- **AFL++ disqualified.** ADR-0002 commits the project to Apple Silicon as the primary platform. AFL++'s ARM macOS mode supports only non-instrumented ("dumb") fuzzing. Without coverage feedback, AFL++ reduces to a worse `proptest`.
- **honggfuzz-rs not chosen.** Listed as macOS-supported but with reported build fragility on some macOS versions; community resources are thinner; not the Rust default.
- **`proptest` alone insufficient.** Already in the project as a dev-dep; pure random sampling from a programmer-defined strategy. No coverage feedback, so it cannot discover that a specific 87-byte FEN string triggers an unexpected panic.
- **`cargo-fuzz` chosen.** Sole option with first-class, coverage-guided support on aarch64-apple-darwin. Community default; most Rust fuzzing tutorials and examples target it.

### 2. Workspace: `--fuzzing-workspace=true` (independent)

- The fuzz crate lives under `fuzz/` with its own `[workspace]` declaration.
- `libfuzzer-sys` and its transitives stay out of the root `Cargo.lock`.
- Therefore stay out of the root `cargo audit` / `cargo deny check` policy.
- `fuzz/` has its own `Cargo.lock`. Operator runs `cd fuzz && cargo audit` separately if concerned about fuzz-side advisories.
- Verified at slice-time: root `cargo audit` and `cargo deny check` report no `libfuzzer-sys` after `fuzz/` is created.

### 3. Toolchain: nightly, scoped via `fuzz/rust-toolchain.toml`

- `cargo-fuzz` requires nightly Rust because it uses `-Zsanitizer` and other nightly-only flags.
- Pinned via `fuzz/rust-toolchain.toml` with `channel = "nightly-2026-04-01"` (specific date, not bare `nightly`) for reproducibility.
- The pin is scoped to the `fuzz/` directory; the main stable build (`cargo build`, `cargo test`, `cargo clippy`) is unaffected.
- Bumping the nightly date is a one-line edit.

### 4. Harness input type: `Arbitrary<String>`

- Both parsers accept `&str`. A raw `&[u8]` harness with a `from_utf8` guard wastes mutation budget on inputs the guard rejects.
- `String` implements `Arbitrary` via a built-in foreign-type impl in the `arbitrary` crate (always pulled in by `libfuzzer-sys`; no feature flag required). The mutator preserves UTF-8 validity across mutations, so byte-level coherence is maintained.
- The `arbitrary-derive` feature on `libfuzzer-sys` is **not** enabled — that feature only adds the `#[derive(Arbitrary)]` proc-macro for custom types, which the current harnesses don't use. Re-enable when a future fuzz target adds a custom-derive type.
- Harness body is two lines: bind `s: String`, call the parser, discard the result. Any panic = bug.

### 5. Sanitizer: `--sanitizer none`

- The two fuzzed entry points (`Position::from_fen`, `parse_uci_line`) do not transitively reach any `unsafe` block in the engine. `from_fen → fen::parse → validate_post_parse` checks mailbox/bitboard consistency only; `parse_uci_line` is a pure-string parser by contract. The one `unsafe` site that exists today (`src/movegen/move_list.rs:78`, a `MaybeUninit` slice cast in `MoveList::as_slice`) is reachable only via `generate_moves` — not from the parsers.
- AddressSanitizer would therefore only check `libfuzzer-sys`'s own transitives. Cost is ~2× throughput; benefit is zero for these targets.
- **Re-evaluation trigger:** any change that lets a fuzzed parser path reach an `unsafe` block. Examples: extending `validate_post_parse` to call `generate_moves`; adding a new fuzz target that wraps a movegen-consuming function. Run at least one campaign with default sanitizer at that point and decide.

### 6. Per-target run-time configuration

- `-max_len=200` for both targets. FEN strings are ~30–80 bytes; UCI commands are ~10–200 bytes. Tightening the search space speeds up exploration.
- `-max_total_time=<seconds>` for time-bounded campaigns. 7200 (2 hours) per target is the slice's default budget.

### 7. Targets shipping in the slice

| Target | Function | Contract |
|---|---|---|
| `fuzz_fen` | `Position::from_fen(&str) -> Result<Position, FenError>` | No panic on any input; either `Ok` or a well-formed `Err`. |
| `fuzz_uci` | `parse_uci_line(&str) -> Command` | Total function; every input produces a `Command` (use `Command::Unknown` for unrecognized input). No panic. |

`Move::from_uci(&str, &Position)` is **not** fuzzed yet — the `Position` argument requires structure-aware position generation that's a separable research task. Defer until justified.

### 8. Seed corpus: hand-curated static text files

- `fuzz/corpus/fuzz_fen/` and `fuzz/corpus/fuzz_uci/` contain one file per seed input (plain UTF-8, one line, optional trailing newline).
- Seeds are extracted from existing positive-case test fixtures (`UCI_SEED_FENS` in `src/mov.rs::tests`; positive-case literals in `src/uci.rs::tests` and `src/fen.rs::tests`).
- Curated, not auto-derived. Adding a seed is `vim fuzz/corpus/fuzz_uci/uci-NNN`; removing is `git rm`. No extraction tooling.
- Seeded corpora typically reach baseline coverage 10× faster than cold-start.

### 9. Cadence

- **Not in the pre-commit hook.** Coverage-guided campaigns run in seconds-to-hours; not pre-commit-shaped.
- **Per parser change.** Touching `src/fen.rs`, `src/uci.rs`, or anything they call → ≥30-min run on the affected target.
- **Per major milestone.** 2-hour run on each target as a backstop, recorded analogously to `bench/<milestone>.md` summaries (in `docs/research/tooling-fuzzing-results.md` — see §10).
- **On demand.** Any time a parser regression is suspected.

### 10. Campaign result artifact

`docs/research/tooling-fuzzing-results.md` accumulates per-campaign summaries. Format: per-target table with corpus size at start/end, libFuzzer's `cov:` and `ft:` final values, exec/s peak, total runs, RSS peak, crashes triaged. **Not** in `bench/`: ADR-0010 reserves `bench/` for performance baselines (Mnps for perft, ns/call for movegen). Fuzzing campaign metrics are a different kind of artifact.

### 11. Crash → regression workflow

- `cargo +nightly fuzz tmin <target> <artifact>` minimizes a crash to the smallest input that still triggers it.
- Embed the minimized literal as a `#[test]` in `src/fen.rs::tests` or `src/uci.rs::tests`.
- Fix the parser; re-run the target until clean.
- The regression test enters the standard `cargo test` suite and catches re-introduction permanently.

## Consequences

- Nightly Rust is a dev-side requirement, scoped to `fuzz/`. Operators outside Apple Silicon (e.g. CI, when it lands per backlog #2) inherit the same scoped requirement.
- Fuzz crate's transitive dependencies are not in the project's audit/deny scope. Operator runs `cd fuzz && cargo audit` separately if a CVE-tracked vulnerability in `libfuzzer-sys` is suspected.
- Seed corpus drift: adding a new fixture to `src/uci.rs::tests` does not auto-flow into `fuzz/corpus/fuzz_uci/`. Coverage-guided fuzzing discovers the same edge cases regardless; the missing seed reduces ramp-up speed, not eventual coverage. Workflow.md documents the manual step.
- A new ADR is required if `unsafe` lands in a fuzzed code path (would likely flip ASan back on). Trigger documented in §5.
- Future targets (e.g. PGN parser, eval table loader) follow the same scaffolding: add a `[[bin]]` entry to `fuzz/Cargo.toml`, write a 5-line harness, seed corpus, run.

## Alternatives considered

- **`afl.rs` / AFL++.** Disqualified by Apple Silicon dumb-only mode (research §2.2).
- **`honggfuzz-rs`.** Stable Rust, but thinner community and reported macOS build issues (research §2.4).
- **`bolero` (front-end over libFuzzer/AFL/Honggfuzz).** Coverage-guided mode still requires nightly; adds a dependency layer without solving the friction (research §2.5).
- **`arbitrary` + `proptest` on stable.** No coverage feedback. Already covered by the existing `proptest` infrastructure for the random-valid-input dimension; not a substitute for coverage-guided fuzzing (research §10).
- **Default-workspace cargo-fuzz** (joining root Cargo.lock). Pulls `libfuzzer-sys` into root `cargo audit` / `cargo deny` scope, conflicting with the project's existing dependency-policy framing (research §3.5).
- **Seed-corpus extraction binary.** Initial draft used a feature-gated `src/fuzz_seeds.rs` module + a Rust binary in the fuzz workspace. Plan-review pass-1 flagged this as over-engineered for *curated* (not derived) seeds — static text files are simpler and eliminate the orphan-on-delete and cargo-mutants-on-feature-gated-code problems.

## Cross-references

- ADR-0002 — Apple Silicon as primary platform (the AFL++ disqualifier).
- ADR-0003 — No third-party source-code reading (rules out comparators-via-engine-source for differential fuzzing).
- ADR-0010 — Benchmark baseline format (clarifies why fuzzing artifacts go to `docs/research/`, not `bench/`).
- `docs/research/tooling-fuzzing.md` — full prior-art research backing this ADR.
- `docs/plans/tooling-fuzzing.md` — the unit's plan.
