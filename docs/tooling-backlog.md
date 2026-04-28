# Tooling and QA backlog

Industry-best-practice items surfaced during the 2026-04-27 workflow review but not yet adopted. **Listed in recommended implementation order** — pick from the top when the next slot opens for tooling work.

*Active queue is currently empty — all items prioritized in the 2026-04-27 review have landed. New items append here.*

---

## Deferred — gated on a specific later trigger, not on prioritization

- **SPRT infrastructure** (`cutechess-cli` / `fastchess`) — premature until M3; nothing has strength to test yet.
- **`unsafe` audit policy** — defer until the engine first uses `unsafe` (likely a hot-path `get_unchecked` in magic-bitboard lookups, possibly M1.C or later). At that point write an ADR for when `unsafe` is allowed and how it's reviewed.
- **LICENSE file** — needed when the repo goes public or accepts external contributions. `Cargo.toml` is currently `publish = false`; cargo-deny ignores via `[licenses] private = { ignore = true }`.
- **CHANGELOG.md** — auto-generatable from conventional commits; only valuable once releases or external consumers exist.
- **codecov.io / Codecov trend tracking** — depends on CI (#1); revisit then.
- **Doc-coverage lint (`#![deny(missing_docs)]`)** — low ROI for an engine the user isn't reading; reconsider only if the codebase ever becomes a library others consume.

---

## Done since the 2026-04-27 review

- **CI (GitHub Actions)** — completed 2026-04-28. `.github/workflows/ci.yml` runs on push to `main`, on PRs, and on manual dispatch. Jobs: `test` matrix on `macos-14` (primary, Apple Silicon) + `ubuntu-latest` (portability) running `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` + `cargo test --all-targets` + `cargo test --doc`; `fuzz-check` on the separate fuzz workspace; `cargo deny` via the EmbarkStudios action (covers RustSec advisories + license + source-allowlist policy from `deny.toml` — `cargo audit` skipped as redundant); `coverage` via `cargo-llvm-cov --summary-only` (informational; no enforced threshold). All jobs cached via `Swatinem/rust-cache@v2`. Closes backlog item #1.
- **Fuzzing (`cargo-fuzz`)** — completed 2026-04-28 per ADR-0013 (`docs/decisions/0013-fuzzing-strategy.md`). `fuzz/` workspace (independent — `libfuzzer-sys` stays out of root `cargo audit` / `cargo deny` scope). Two `Arbitrary<String>` harnesses: `fuzz_fen` and `fuzz_uci`. Hand-curated seed corpora (35 fen, 30 uci) under `fuzz/corpus/<target>/`. `--sanitizer none` (no fuzzed parser path reaches `unsafe`). Cadence + commands documented under `docs/workflow.md` "Fuzzing". The first 2-hour campaigns per target are deferred to a later session — operator runs `cd fuzz && cargo fuzz run --sanitizer none <target> -- -max_len=200 -max_total_time=7200` once nightly is installed; campaign-result tables go to `docs/research/tooling-fuzzing-results.md`.

## Done in the 2026-04-27 review

- **Benchmark baseline format** — ratified at M1.G's plan-mode pass; ADR-0010 (`docs/decisions/0010-benchmark-baseline-format.md`) committed alongside the M1.G `bench/m1.g.md` first baseline. `criterion 0.7` + `--save-baseline` on per-machine `target/criterion/` (gitignored) + committed human-readable table.
- `cargo fmt --check` enforcement (commits `5ca6c86` style + `eaf9d37` workflow).
- Pre-commit hook at `.claude/hooks/pre-commit-check.sh`, wired via `.claude/settings.json`.
- `cargo audit` + `cargo deny` with policy in `deny.toml`; `Cargo.toml` marked `publish = false`.
- Documentation in `docs/workflow.md` under "Static analysis and dependency hygiene".
- **Mutation testing (`cargo-mutants`)** — backfilled across M1.A + M1.B + M1.C. Configuration in `.cargo/mutants.toml` (with `exclude_re` rules documenting the equivalent mutants). Integrated into the final-review loop per `docs/workflow.md`. Baseline run against committed code: 333 caught + 7 timeout (caught) + 47 unviable + 0 unaddressed survivors, after adding seven targeted tests (idempotency for `Bitboard::with` / `CastlingRights::with`, `Square` Debug-format, `Position::debug_assert_consistent` panic-on-broken-state, `slow_attacks::ray_attack` per-axis step pinning) and excluding eleven equivalent-mutant patterns.
- **Property-based testing (`proptest`)** — `proptest = "1.11"` wired as a dev-dependency. Eight property tests backfill M1.A/M1.B primitives: `Square` index/file/rank round-trip + out-of-range rejection (`src/square.rs`), `Bitboard` set algebra (commutativity, associativity, identity, idempotence, De Morgan, absorption) + membership/`pop_lsb`-ordering invariants (`src/bitboard.rs`), `CastlingRights` bit packing + multi-flag `has` semantics (`src/position.rs`), and `Position` ↔ FEN round-trip across randomly generated valid positions (`src/fen.rs`). Existing anchor unit tests are kept (proptest samples rather than enumerates; anchors document specific mutants killed). Test-suite review loop converged after one revision (stronger idempotence form on `with`/`without`, LSB-ascending order pin on `pop_lsb`, positive-only scope made explicit on the FEN generator).
