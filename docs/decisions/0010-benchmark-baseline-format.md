# ADR-0010 — Benchmark baseline format

**Status:** Accepted, 2026-04-27.
**Phase:** Ratifies at M1.G's plan-mode pass; binds the format used from M1.G onward.

## Context

Both `docs/workflow.md` ("Benchmarking conventions") and prior `docs/roadmap.md` notes recorded `bench/` as "TBD format". `docs/tooling-backlog.md` #1 surfaced this as a decision-only blocker for M1.G — pure documentation work that costs nothing to settle now and that ensures the first M1.G measurement is comparable to subsequent ones.

The two practical questions:

1. **Where do the raw measurement artifacts live, and how stable are they?** `criterion` produces a `target/criterion/` subtree with HTML reports, statistical-distribution plots, and serialized timing data. These are useful for *within-machine, between-commit* comparison via `criterion`'s `--baseline` workflow, but they are not portable across machines (they depend on local clock resolution, CPU frequency, kernel scheduler behavior). Committing them to git would conflate per-machine noise with substantive change.

2. **Where do the headline numbers live so future commits can compare?** A human-readable summary that is committed to git, that any reviewer can `git log --follow` to see drift, and that a future Claude session reads when deciding whether a PR regressed performance.

Microbenchmarking landscape:

- `criterion` is the de facto standard for stable-Rust microbenchmarks — statistical analysis, outlier detection, baseline saving, HTML reports. Selected as the project's microbenchmark harness in `docs/research/m1-perft-and-rust.md` §"Benchmark layout".
- The built-in `#[bench]` is nightly-only and lacks statistics; not viable.
- `divan` is a newer alternative; `criterion` is the safer pick for ecosystem maturity and CI familiarity.

## Decision

1. **Benchmarking framework: `criterion` 0.7** (current stable). One `[[bench]]` entry per microbenchmark file in `Cargo.toml`, all with `harness = false` (mandatory for criterion).

2. **Raw artifacts: gitignored, kept under `target/criterion/`.** Per-machine, per-run; not portable. Used for `criterion --save-baseline <name>` / `--baseline <name>` comparison workflows on the developer's local machine.

3. **Per-milestone summary: `bench/<milestone>.md`, committed to git.** A human-readable Markdown file, one row per microbenchmark, columns for criterion's median time per iteration plus the derived headline metric (Mnps for perft, ns/call for movegen). Records the hardware + Stockfish version + Rust toolchain + date the numbers were captured. Cross-commit regression-tracking happens by reading these files.

4. **Saved baselines for within-machine comparison.** Capture at commit time:

    ```sh
    cargo bench -- --save-baseline <milestone>
    ```

    Future runs on the same machine compare against the saved baseline:

    ```sh
    cargo bench -- --baseline <milestone>
    ```

    The saved baselines are local to `target/criterion/` and not committed.

5. **Format of `bench/<milestone>.md`** — fixed schema per file, three sections:

    a. **Header.** Hardware (CPU model + macOS version), Rust toolchain, criterion version, Stockfish version (when relevant), capture date.
    b. **Results table(s).** One per microbenchmark file. Columns: benchmark name, median ns/iter, derived headline (e.g. Mnps, moves/iter), iteration count, sample size.
    c. **Notes.** Anything qualitative — runs that needed re-doing, system load context, etc.

    The schema is a convention, not a parser-binding contract. Future milestones may extend it (e.g. add a "vs. previous milestone" column) without breaking past entries.

6. **Cumulative vs. per-milestone files.** Each milestone gets its own file (`bench/m1.g.md`, `bench/m2.md`, …). A future "all-milestones" rollup table can be added under `bench/README.md` if useful — deferred until at least three milestones exist.

## Consequences

- **`target/criterion/` joins `.gitignore`.** Verified at M1.G implementation; if `target/` is already wholesale-ignored, this is a no-op.
- **Across-machine comparison happens via the committed table, not via raw criterion artifacts.** Claude (or any human reviewer) reads `bench/m1.g.md` to see what the M1.G numbers were on the dev machine; criterion's HTML reports are local debugging tools.
- **Regression detection inside a single dev session** uses `criterion --baseline`. Statistical significance is criterion's call.
- **Regression detection across dev sessions / commits** uses the committed `bench/<milestone>.md` numbers as reference. A 2× regression vs. the recorded baseline is investigated; small percentages are noise tolerated.
- **No CI bench gate.** When CI lands (`docs/tooling-backlog.md` #3), the bench job is informational, not pass/fail. Cloud runners introduce too much noise for hard thresholds.
- **First baseline lands at M1.G commit.** Format is binding from that point on; future milestones extend the table.

## How to apply

- M1.G commit writes `bench/m1.g.md` with numbers from `cargo bench -- --save-baseline m1.g` on the dev machine.
- Later milestones write a fresh `bench/<milestone>.md`. If a benchmark is removed (e.g. `generate_moves` rolled into a higher-level perft bench), the new file's row count drops; the old file remains in git history as the historical record.
- A bench result that surprises (significantly better OR significantly worse than the prior milestone's recorded number on the same machine) is worth a chat-flag during the per-milestone wrap-up — improvements that aren't explained may be measurement bug; regressions that aren't explained are real bugs.
- This ADR is referenced from `docs/architecture.md` (under "Benchmarking conventions") and `docs/workflow.md` (replacing the prior "TBD format" note).

## Variants considered and rejected

- **`divan` over `criterion`.** Younger ecosystem, fewer downstream users; criterion's stability and the project's preference for the most-vetted choice push us to criterion.
- **Committing `target/criterion/` HTML reports to git.** Per-machine artifacts in git is the wrong direction; the markdown summary covers the cross-commit need without polluting history with binaries and noise.
- **A single, append-only `bench/all.md` instead of one file per milestone.** Single file becomes unwieldy quickly and forces in-place edits; per-milestone files are easier to diff and easier to read.
- **Embedding numbers in roadmap.md.** Mixes process documentation with measurement; keeping `bench/` separate respects the directories' single-purpose layout (`docs/` for process; `bench/` for measurement).
