# Tooling and QA backlog

Industry-best-practice items surfaced during the 2026-04-27 workflow review but not yet adopted. **Listed in recommended implementation order** — pick from the top when the next slot opens for tooling work.

## 1. Benchmark baseline format

**Why first now.** Both `workflow.md` and `roadmap.md` mark `bench/` as "TBD format". M1.G (perft + benchmarks) is two phases away; deciding the format before then avoids retrofit and ensures the first measurement is comparable to the second. Pure decision work — no code until M1.G actually lands.

**Effort.** A short ADR (`docs/decisions/`) deciding: criterion + `--save-baseline` for the raw artifact (gitignored under `target/criterion/`), plus a human-readable `bench/<milestone>.md` table committed to git. The committed table is the regression-tracking artifact across commits; `criterion` files give per-machine detail.

**Integration.** Ratify as part of M1.G's plan-mode pass.

## 2. Fuzzing (`cargo-fuzz`)

**Why next.** Highest-ROI target is the FEN parser (already shipped — strict spec, lots of edge cases). UCI parser at M2 is the next obvious one. Defer until UCI lands so the same setup amortizes across two targets. Nightly-Rust requirement is friction; alternative is structure-aware property testing with `arbitrary` + `proptest` on stable, which the property-testing infrastructure already covers partially.

**Effort.** Nightly toolchain installation, `cargo fuzz init`, write a fuzz target wrapping `Fen::parse`, run for a few CPU-hours. Investigate any panic. Repeat for UCI at M2.

**Integration.** Run periodically on parsers (any module that ingests external strings). Standalone `cargo fuzz` invocation, not in the pre-commit hook.

## 3. CI (GitHub Actions)

**Why last in the active queue.** Blocked: the project is not on GitHub yet. When it moves, this consolidates everything above (fmt, clippy, test, coverage, audit, deny, plus any of the items implemented by then). Especially valuable since the user doesn't read code and depends on external green/red signals.

**Effort.** One workflow YAML, ~30 minutes once the repo exists on GitHub.

**Integration.** On push/PR: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo llvm-cov --summary-only`, `cargo audit`, `cargo deny check`. Optional matrix across Linux/Windows (Apple Silicon is primary, but a cheap signal that we haven't accidentally broken portability).

---

## Deferred — gated on a specific later trigger, not on prioritization

- **SPRT infrastructure** (`cutechess-cli` / `fastchess`) — premature until M3; nothing has strength to test yet.
- **`unsafe` audit policy** — defer until the engine first uses `unsafe` (likely a hot-path `get_unchecked` in magic-bitboard lookups, possibly M1.C or later). At that point write an ADR for when `unsafe` is allowed and how it's reviewed.
- **LICENSE file** — needed when the repo goes public or accepts external contributions. `Cargo.toml` is currently `publish = false`; cargo-deny ignores via `[licenses] private = { ignore = true }`.
- **CHANGELOG.md** — auto-generatable from conventional commits; only valuable once releases or external consumers exist.
- **codecov.io / Codecov trend tracking** — depends on CI (#5); revisit then.
- **Doc-coverage lint (`#![deny(missing_docs)]`)** — low ROI for an engine the user isn't reading; reconsider only if the codebase ever becomes a library others consume.

---

## Done in the 2026-04-27 review

- `cargo fmt --check` enforcement (commits `5ca6c86` style + `eaf9d37` workflow).
- Pre-commit hook at `.claude/hooks/pre-commit-check.sh`, wired via `.claude/settings.json`.
- `cargo audit` + `cargo deny` with policy in `deny.toml`; `Cargo.toml` marked `publish = false`.
- Documentation in `docs/workflow.md` under "Static analysis and dependency hygiene".
- **Mutation testing (`cargo-mutants`)** — backfilled across M1.A + M1.B + M1.C. Configuration in `.cargo/mutants.toml` (with `exclude_re` rules documenting the equivalent mutants). Integrated into the final-review loop per `docs/workflow.md`. Baseline run against committed code: 333 caught + 7 timeout (caught) + 47 unviable + 0 unaddressed survivors, after adding seven targeted tests (idempotency for `Bitboard::with` / `CastlingRights::with`, `Square` Debug-format, `Position::debug_assert_consistent` panic-on-broken-state, `slow_attacks::ray_attack` per-axis step pinning) and excluding eleven equivalent-mutant patterns.
- **Property-based testing (`proptest`)** — `proptest = "1.11"` wired as a dev-dependency. Eight property tests backfill M1.A/M1.B primitives: `Square` index/file/rank round-trip + out-of-range rejection (`src/square.rs`), `Bitboard` set algebra (commutativity, associativity, identity, idempotence, De Morgan, absorption) + membership/`pop_lsb`-ordering invariants (`src/bitboard.rs`), `CastlingRights` bit packing + multi-flag `has` semantics (`src/position.rs`), and `Position` ↔ FEN round-trip across randomly generated valid positions (`src/fen.rs`). Existing anchor unit tests are kept (proptest samples rather than enumerates; anchors document specific mutants killed). Test-suite review loop converged after one revision (stronger idempotence form on `with`/`without`, LSB-ascending order pin on `pop_lsb`, positive-only scope made explicit on the FEN generator).
