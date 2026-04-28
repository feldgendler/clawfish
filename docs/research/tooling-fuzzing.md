# Fuzzing Research: Rust Parser Targets

*Prepared for the chess engine project. Covers FEN parser (`src/fen.rs`) and UCI command parser
(`src/uci.rs`) as the two initial fuzz targets. Addresses tooling-backlog item #1.*

---

## 1. Targets under consideration

| Target | Function | Contract |
|---|---|---|
| FEN parser | `Position::from_fen(&str) -> Result<Position, FenError>` | Strict 6-field parse; well-defined error variants; already unit + property tested |
| UCI parser | `parse_uci_line(&str) -> Command` | Total function; never panics; all inputs produce a `Command` |

Both are pure string→type functions with no `unsafe`, no I/O, no allocator surgery.

---

## 2. Tooling choice

### 2.1 Three options

| Tool | Fuzzer backend | Nightly? | Apple Silicon (aarch64-apple-darwin) | Stable? |
|---|---|---|---|---|
| `cargo-fuzz` | libFuzzer (LLVM) | Required | Supported (confirmed) | No |
| `afl.rs` | AFL++ | Not required | **Dumb-fuzzer only** (ARM AFL++ lacks instrumentation; QEMU mode unsupported on macOS) | Yes |
| `honggfuzz-rs` | Honggfuzz | Not required | Listed as supported; macOS build issues reported on some versions | Yes |
| `proptest` alone | Pure random generation | No | Full | Yes |
| `bolero` (front-end) | libFuzzer / AFL / Honggfuzz | Depends on backend | Depends on backend | Partial |

Sources:
- cargo-fuzz Apple Silicon support: [Rust Fuzz Book — Setup](https://rust-fuzz.github.io/book/cargo-fuzz/setup.html)
- AFL++ on Apple Silicon = dumb fuzzer: [Medium — AFL on M1](https://vineethbharadwaj.medium.com/how-to-compile-and-run-afl-fuzzer-on-m1-mac-with-apple-silicon-for-x86-instrumentation-support-4f1700eaafb6)
- honggfuzz-rs stable/beta/nightly support: [honggfuzz-rs README](https://github.com/rust-fuzz/honggfuzz-rs/blob/master/README.md)
- cargo-fuzz current version 0.13.1 (2025-06-27): [crates.io](https://crates.io/crates/cargo-fuzz)

### 2.2 AFL++ on Apple Silicon — the decisive disqualifier

- AFL++'s ARM macOS mode supports only non-instrumented ("dumb") fuzzing.
- Non-instrumented AFL is random-mutation without coverage feedback — equivalent to throwing bytes at the target with no guidance.
- Coverage-guided mutation is the reason fuzzing finds bugs property testing cannot.
- Conclusion: `afl.rs` on Apple Silicon M4 reduces to a worse `proptest`. Eliminate it.

### 2.3 proptest alone — what it cannot do

- proptest uses pure random generation from a programmer-defined strategy.
- It has no coverage feedback; it does not discover which byte sequences unlock new code paths.
- It runs in milliseconds per invocation (normal test cadence).
- It is excellent for properties expressible compactly (round-trips, type invariants).
- It will **not** discover that a specific 87-byte FEN string triggers an integer overflow in a position field.
- Conclusion: proptest is already in the project and already exercising the parsers. It does not replace coverage-guided fuzzing.

Sources: [Fuzzing vs Property Testing (Tedinski)](https://www.tedinski.com/2018/12/11/fuzzing-and-property-testing.html)

### 2.4 honggfuzz-rs — viable but secondary

- Stable Rust: yes.
- macOS support: listed; but build failures reported on some macOS versions.
- Feedback mode: hardware-based coverage feedback (requires OS perf counters).
- No separate fuzz workspace needed; fuzzing targets are normal test functions.
- Documentation and community resources are thinner than cargo-fuzz.
- Not the community default for Rust; less prior art to draw from.

### 2.5 bolero — a unified front-end, not a fuzzer

- `bolero` wraps multiple backends (libFuzzer, AFL++, Honggfuzz) behind one API.
- Coverage-guided campaigns still require the backend (and its nightly requirement if libFuzzer).
- Adds a dependency layer without solving the nightly problem.
- Not recommended for a project where the toolchain constraint is already accepted.

### 2.6 Recommendation: `cargo-fuzz` (libFuzzer)

Rationale:

- Only option with confirmed, first-class, coverage-guided support on Apple Silicon.
- Community default for Rust fuzzing; most tutorials, examples, and prior art target it.
- Both fuzz targets are pure Rust, no FFI — nightly is the only friction point.
- Nightly friction is real but bounded: one-time install; pinned to `fuzz/` subdirectory only (no pollution of the main stable build).
- libFuzzer `--sanitizer none` recovers ~2x throughput for pure-safe-Rust targets.

Sources:
- [cargo-fuzz README](https://github.com/rust-fuzz/cargo-fuzz)
- [Rust Fuzz Book](https://rust-fuzz.github.io/book/cargo-fuzz.html)
- [Testing Handbook — cargo-fuzz](https://appsec.guide/docs/fuzzing/rust/cargo-fuzz/)

---

## 3. Setup mechanics

### 3.1 Installation

```
cargo install cargo-fuzz
rustup toolchain install nightly
```

### 3.2 `cargo fuzz init` output

```
fuzz/
├── Cargo.toml         # fuzz crate manifest
└── fuzz_targets/
    └── fuzz_target_1.rs   # starter harness
```

Run `cargo fuzz add <name>` to add each additional target; updates `fuzz/Cargo.toml` and creates
`fuzz/fuzz_targets/<name>.rs`.

### 3.3 fuzz/Cargo.toml structure

```toml
[package]
name = "chess-fuzz"
version = "0.0.0"
publish = false
edition = "2021"

[package.metadata]
cargo-fuzz = true

[[bin]]
name = "fuzz_fen"
path = "fuzz_targets/fuzz_fen.rs"

[[bin]]
name = "fuzz_uci"
path = "fuzz_targets/fuzz_uci.rs"

[dependencies]
libfuzzer-sys = "0.4"

[dependencies.chess]
path = ".."
```

Notes:
- Each fuzz target is a separate Cargo binary.
- Targets are not included in `cargo test --all` or `cargo doc --all` by default.
- As of cargo-fuzz 0.13.x (2024-01-25), `cargo fuzz init` does **not** create a separate workspace by default; it joins the parent workspace.
  - Use `cargo fuzz init --fuzzing-workspace=true` to create an independent workspace (avoids `workspace.members` change in the root `Cargo.toml`).

Sources:
- [cargo-fuzz CHANGELOG](https://github.com/rust-fuzz/cargo-fuzz/blob/main/CHANGELOG.md) — 2024-01-25 default workspace change
- [Testing Handbook — cargo-fuzz](https://appsec.guide/docs/fuzzing/rust/cargo-fuzz/)

### 3.4 Nightly pinning

Two options:

| Option | How | Effect |
|---|---|---|
| Per-invocation `+nightly` | `cargo +nightly fuzz run <target>` | No file changes; operator types `+nightly` each time |
| `fuzz/rust-toolchain.toml` | Create file with `[toolchain] channel = "nightly"` | Nightly auto-selected when cwd is `fuzz/`; stable used everywhere else |

Recommended: `fuzz/rust-toolchain.toml` with `channel = "nightly"`. Keeps the nightly requirement
scoped to `fuzz/`, visible in git, and automatic for anyone entering the directory.

Source: [Rustup overrides book](https://rust-lang.github.io/rustup/overrides.html)

### 3.5 Effect on `cargo audit` and `cargo deny`

- If the fuzz crate is a workspace member (default since 0.13.x), `cargo audit` run at the repo root **will** scan `fuzz/Cargo.lock` and surface any advisories on `libfuzzer-sys` and its transitive deps.
- If an advisory fires on `libfuzzer-sys` in the future, the operator can either update or use `cargo audit` ignore / `deny.toml` suppression.
- Using `--fuzzing-workspace=true` isolates the fuzz `Cargo.lock` completely; `cargo audit` at the root will not see fuzz deps.
- Recommendation for this project: use `--fuzzing-workspace=true` to keep the existing `cargo audit` / `cargo deny` policy clean. The trade-off is that `fuzz/` becomes a separate workspace not covered by the root lockfile.

---

## 4. Writing fuzz targets for string parsers

### 4.1 Naive raw-bytes approach

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = chess::Position::from_fen(s);
    }
});
```

- libFuzzer feeds arbitrary bytes; the `from_utf8` guard drops non-UTF8 inputs silently.
- Coverage signal is still collected on the `from_utf8`-passing branch.
- The fuzzer will quickly learn to produce valid UTF-8 (from corpus or by noticing the guard).

### 4.2 Structure-aware with `Arbitrary<String>`

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;
use libfuzzer_sys::arbitrary::Arbitrary;

fuzz_target!(|s: String| {
    let _ = chess::Position::from_fen(&s);
});
```

- `String` implements `Arbitrary`; the crate generates valid UTF-8 strings of varying length.
- Skips the `from_utf8` overhead; all inputs are valid UTF-8.
- Mutator preserves UTF-8 validity across mutations (small byte changes produce small string changes).
- Coverage signal reaches parser logic faster.

### 4.3 Tradeoff: raw bytes vs. Arbitrary String

| Dimension | `&[u8]` + `from_utf8` | `Arbitrary<String>` |
|---|---|---|
| Boilerplate | 1 extra guard line | None |
| UTF-8 waste | High at cold start; quickly learned | None |
| Mutation coherence | Byte flips may break UTF-8 | Maintained across mutations |
| Corpus seeding | Seed files are raw bytes (just write text files) | Seed files must be valid Arbitrary-encoded byte sequences |
| Bug coverage | Can find UTF-8 boundary bugs in the guard itself | Skips non-UTF8 entirely |

For FEN and UCI:
- Both parsers accept `&str`; no UTF-8-boundary bugs are expected.
- `Arbitrary<String>` is the correct choice: less waste, more efficient exploration, no raw-byte overhead.
- Exception: if the harness ever calls `from_utf8` as part of the tested code path (it does not here), raw bytes would be warranted.

Source: [Rust Fuzz Book — Structure-Aware Fuzzing](https://rust-fuzz.github.io/book/cargo-fuzz/structure-aware-fuzzing.html)

### 4.4 FEN fuzz target (recommended form)

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|s: String| {
    // Should never panic — only return Ok or Err
    let _ = chess::Position::from_fen(&s);
});
```

### 4.5 UCI fuzz target (recommended form)

```rust
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|s: String| {
    // Total function — every input must produce a Command, never panic
    let _cmd = chess::parse_uci_line(&s);
});
```

- The UCI parser is a total function by contract.
- Any panic is a bug.
- Any non-termination (infinite loop) will be caught by libFuzzer's `-timeout` (default 1200 s).

### 4.6 libfuzzer-sys dependency for Arbitrary String

Requires the `arbitrary-derive` feature:

```toml
[dependencies]
libfuzzer-sys = { version = "0.4", features = ["arbitrary-derive"] }
```

Source: [libfuzzer-sys crates.io](https://crates.io/crates/libfuzzer-sys)

---

## 5. Corpus seeding

### 5.1 Directory convention

```
fuzz/corpus/
├── fuzz_fen/      # one file per seed FEN string
└── fuzz_uci/     # one file per seed UCI command string
```

Files are plain UTF-8 text, one input per file, no newline required.

### 5.2 Seeding from existing test fixtures

- libFuzzer reads every file in the corpus directory before generating mutations.
- Passing a seeded corpus can increase initial code coverage by an order of magnitude.
- Procedure: extract each positive-case string literal from `src/fen.rs::tests` and
  `src/uci.rs::tests`; write each to a separate file under `fuzz/corpus/<target>/`.
- This can be done with a one-time shell script at setup time; corpus files are small and committed.

### 5.3 Seeding tradeoffs

| Approach | Pros | Cons |
|---|---|---|
| Seed from existing tests | Fast coverage ramp-up; parser logic reached immediately | Manual extraction work; seeds must be kept in sync when tests change |
| No seeds (cold start) | No maintenance | Fuzzer may spend many minutes generating valid-UTF-8 baseline |
| OSS-Fuzz corpus dump | Large diverse corpus | Not available for a private project |

Recommendation: seed from existing positive-case fixtures. The existing test suites have hundreds of
FEN strings and dozens of UCI command examples — this is a high-quality starting point.

Source: [Efficient Fuzzing Guide (Chromium)](https://chromium.googlesource.com/chromium/src/+/main/testing/libfuzzer/efficient_fuzzing.md)

### 5.4 Corpus minimization (`cargo fuzz cmin`)

After a long campaign the corpus grows with redundant entries. Before committing or re-running:

```
cargo +nightly fuzz cmin fuzz_fen
cargo +nightly fuzz cmin fuzz_uci
```

Runs `libFuzzer -merge=1` internally: keeps only inputs that contribute unique coverage. Reduces disk
usage and re-run time.

Source: [libFuzzer docs — corpus minimization](https://llvm.org/docs/LibFuzzer.html)

---

## 6. Run length and stopping criteria

### 6.1 Coverage saturation signal

libFuzzer emits periodic stats lines:

```
#262144  pulse  cov: 47 ft: 89 corp: 23/1234b exec/s: 85000 rss: 128Mb
#524288  pulse  cov: 47 ft: 89 corp: 23/1234b exec/s: 84000 rss: 128Mb
```

When `cov:` and `ft:` (features) stop increasing across consecutive `pulse` lines, coverage is
saturated. The fuzzer is still running but not discovering new paths.

Note: coverage saturation does not mean zero bugs remain. Research shows >50% of bugs are discovered
after the initial coverage plateau. However, for a small, well-specified grammar, saturation strongly
suggests the easy-to-reach paths have been explored.

Source: [Google fuzzing tutorial](https://github.com/google/fuzzing/blob/master/tutorial/libFuzzerTutorial.md)

### 6.2 Recommended budget for these targets

| Target | Rationale | Recommended duration |
|---|---|---|
| `fuzz_fen` | Strict 6-field grammar; ~10–20 interesting code paths; already property-tested | 2–4 CPU-hours post-seeding |
| `fuzz_uci` | Total function; 14 command variants; lenient parsing; already at 99.9% line coverage | 2–4 CPU-hours post-seeding |

- Research shows most coverage gains in the first 15 minutes for small grammars.
- 2–4 hours gives ~10× the coverage of the initial burst and captures mutation-discovered edge cases.
- Overnight (8+ hours) is not overkill but offers diminishing returns for these small grammars.
- Use `-max_total_time=<seconds>` to time-bound a run: `cargo +nightly fuzz run fuzz_fen -- -max_total_time=7200`

Source: [libFuzzer docs](https://llvm.org/docs/LibFuzzer.html)

---

## 7. Crash triage

### 7.1 Crash artifact naming

Crashes are written to `fuzz/artifacts/<target>/` with names like:

```
crash-04704b1542f61a21a4649e39023ec57ff502f627
timeout-3e9d4e32f8a5bcde7f67e489cb74d02c98a3f16
oom-7c5e8a2bfd31084c60a18e497d3b5a2a1b9f4e73
```

The prefix indicates the failure type; the suffix is a SHA1 of the input bytes.

### 7.2 Reproducing a crash

```
cargo +nightly fuzz run fuzz_fen fuzz/artifacts/fuzz_fen/crash-<sha>
```

Reproduces the exact crash with the original input.

### 7.3 Minimizing a crash (`cargo fuzz tmin`)

The raw crash input may be hundreds of bytes; minimization finds the smallest input that still
triggers the bug:

```
cargo +nightly fuzz tmin fuzz_fen fuzz/artifacts/fuzz_fen/crash-<sha>
```

Writes a minimized input to `fuzz/artifacts/fuzz_fen/minimized-crash-<sha>`.

### 7.4 Converting a crash to a regression unit test

Once minimized, embed the input as a unit test so the bug cannot regress:

```rust
#[test]
fn fuzz_regression_fen_01() {
    // Found by cargo fuzz; minimized to this input
    let input = include_str!("../fuzz/artifacts/fuzz_fen/minimized-crash-<sha>");
    // Should not panic — should return Err(_)
    let _ = Position::from_fen(input.trim());
}
```

Or for a one-liner if the minimized input is short:

```rust
#[test]
fn fuzz_regression_fen_01() {
    let _ = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w - - 0");
}
```

Source: [Testing Handbook crash workflow](https://appsec.guide/docs/fuzzing/rust/cargo-fuzz/)

---

## 8. Sanitizers

### 8.1 What cargo-fuzz enables by default

- AddressSanitizer (ASan) — detects heap overflows, use-after-free, stack overflows.
- Enabled via `-Zsanitizer=address` (nightly flag).
- Adds ~2× runtime overhead.

### 8.2 ASan value for pure-safe-Rust parsers

| Scenario | ASan value |
|---|---|
| `unsafe` code, FFI | High — finds memory safety bugs the borrow checker can't see |
| Pure safe Rust, no `unsafe` blocks | Low — Rust's ownership guarantees already prevent the bugs ASan looks for |
| Pure safe Rust with `unsafe` transitive deps | Moderate — deps could have bugs; ASan may catch them |

Both FEN and UCI parsers are pure safe Rust.

Recommendation: disable ASan for these targets to get ~2× throughput:

```
cargo +nightly fuzz run --sanitizer none fuzz_fen
cargo +nightly fuzz run --sanitizer none fuzz_uci
```

Caveat: if `libfuzzer-sys` or any transitive dep uses `unsafe`, ASan may still find issues in those
paths. Re-enable ASan for at least one campaign to verify.

Source: [Testing Handbook — ASan overhead](https://appsec.guide/docs/fuzzing/rust/cargo-fuzz/)

### 8.3 Other sanitizers

| Sanitizer | Rust flag | Value for these targets |
|---|---|---|
| UBSan | `-Zsanitizer=undefined` | Low — no integer arithmetic or bit manipulation that could overflow |
| MemorySanitizer (MSan) | `-Zsanitizer=memory` | Not supported on macOS; Linux-only |
| ThreadSanitizer (TSan) | `-Zsanitizer=thread` | Not applicable — single-threaded parsers |

Conclusion: `--sanitizer none` is correct for these targets on Apple Silicon macOS.

Source: [Rust Unstable Book — sanitizer](https://doc.rust-lang.org/beta/unstable-book/compiler-flags/sanitizer.html)

---

## 9. Workflow integration

### 9.1 Confirmed: not in pre-commit hook

- Fuzzing is seconds-to-hours, not milliseconds.
- Pre-commit hooks must be fast; fuzzing in a hook would block every commit.
- This matches the tooling-backlog note: "Standalone `cargo fuzz` invocation, not in the
  pre-commit hook."

### 9.2 Recommended cadence

| Trigger | Action |
|---|---|
| Initial setup (this milestone) | Seed corpus + first run: 4-hour campaign per target |
| Parser change (FEN or UCI logic touched) | Re-run fuzz for the affected target: 1–2 hours |
| Major dep bump (Rust edition, parser rewrite) | Full 4-hour campaign for all targets |
| Per major milestone | Full campaign for all targets before milestone commit |
| On demand | Any time a parser regression is suspected |

Source: [GitLab — fuzz Rust continuously](https://about.gitlab.com/blog/how-to-fuzz-rust-code/)

### 9.3 Corpus management in git

- Commit initial seed corpus (`fuzz/corpus/<target>/`) — small files, text, valuable.
- Commit minimized crash artifacts that become regression tests.
- Do **not** commit the growing fuzzer-discovered corpus (`fuzz/corpus/<target>/` after long campaigns
  without cmin): it can grow to many megabytes. Run `cargo fuzz cmin` first if committing.
- The project's `.gitignore` should not exclude `fuzz/corpus/` since seeds are intentional, but
  should exclude `fuzz/artifacts/` except when converting crashes to regressions.

---

## 10. Stable-Rust alternative (`proptest` + `arbitrary`)

### 10.1 What proptest alone provides

- proptest is already in the project (`proptest = "1.11"` dev-dep).
- It is purely random-sampling — no coverage feedback.
- It tests properties compactly: "for all valid FEN strings, from_fen(to_fen(pos)) == pos."
- It does **not** discover that a specific malformed string triggers an unexpected panic.

### 10.2 What `arbitrary` + proptest would look like on stable

Adding `arbitrary` as a dev-dep (stable, no nightly) enables deriving `Arbitrary` on custom types.
But this alone does not add coverage feedback — it still generates inputs randomly.

This is structurally equivalent to writing a proptest strategy for `String`. It provides:
- Broader input space than hand-written examples.
- No coverage guidance.

Result: better than nothing, weaker than cargo-fuzz. proptest is already doing this.

### 10.3 Does proptest support coverage-guided mutation?

No. proptest is a generation-based framework (QuickCheck lineage). It does not instrument the code
under test and has no coverage feedback loop. It finds bugs proportional to how well the strategy
distribution covers the bug-triggering input space.

Source: [Fuzzing vs Property Testing (Tedinski)](https://www.tedinski.com/2018/12/11/fuzzing-and-property-testing.html)

### 10.4 bolero as a stable-with-future-coverage-upgrade path

`bolero` can run property tests (stable, random) today and switch to libFuzzer (nightly, coverage-
guided) by changing a flag. This is attractive if you want to write one harness that runs both ways.
However:
- Coverage-guided mode still requires nightly via the libFuzzer backend.
- It adds a new dependency front-end.
- cargo-fuzz already provides everything bolero provides on top of libFuzzer.

Source: [bolero GitHub](https://github.com/camshaft/bolero)

### 10.5 Conclusion on the stable alternative

- proptest already handles the "random valid input" dimension.
- The nightly requirement for cargo-fuzz is the only friction; it is limited to `fuzz/`.
- The stable alternative does not add coverage-guided fuzzing — it is not a substitute.

---

## 11. Anti-patterns and gotchas

### 11.1 Harness bugs vs. parser bugs

- A crash in the harness (e.g., incorrect `unwrap()` in setup code outside `fuzz_target!`) is not
  a bug in the parser.
- Keep harness logic minimal: the `fuzz_target!` body should be one or two lines calling the
  function under test with the input.
- Any setup (e.g., initializing a logger) should use the `init` parameter of `fuzz_target!` for
  one-time initialization; avoid stateful mutation inside the body.

Source: [Testing Handbook — writing harnesses](https://appsec.guide/docs/fuzzing/rust/techniques/writing-harnesses/)

### 11.2 OOM is not a parser bug

- libFuzzer's default `-rss_limit_mb=2048` will terminate an input that consumes >2 GB.
- For a string parser this should never fire; both FEN and UCI parsers operate in O(len(input)) memory.
- If OOM fires, investigate whether the harness allocates unboundedly (e.g., collecting all parses).
- OOM crashes create `oom-*` artifacts, not `crash-*`.

Source: [libFuzzer docs — rss_limit_mb](https://llvm.org/docs/LibFuzzer.html)

### 11.3 `max_len` and input size

- libFuzzer default `max_len` is 4096 bytes.
- A valid FEN string is ~30–80 bytes; a UCI command is ~10–200 bytes.
- Setting `-max_len=200` tightens the search space and speeds up execution.
- Add to run invocation: `cargo +nightly fuzz run fuzz_fen -- -max_len=200`

Source: [libFuzzer docs — max_len](https://llvm.org/docs/LibFuzzer.html)

### 11.4 Deduplication of similar crashes

- libFuzzer does not deduplicate crashes; every distinct crashing input produces a separate artifact.
- After a campaign, `fuzz/artifacts/fuzz_fen/` may contain dozens of `crash-*` files from the same
  underlying bug.
- Use `cargo fuzz tmin` on each to find the minimized form; minimized inputs from the same bug often
  converge to identical (or near-identical) strings, making deduplication easy by hand.
- Tooling note: libFuzzer's `-runs=0` with a seeded corpus can be used to replay all seeds without
  fuzzing — useful to batch-check whether a fixed bug still triggers on any saved crash.

### 11.5 `panic!` in the parser is a crash

- cargo-fuzz compiles with `-Cpanic=abort`.
- Any `panic!`, `unwrap()`, `expect()`, `assert!` failure, or index out-of-bounds in the tested
  code will be caught as a crash.
- For the UCI parser (`parse_uci_line`), the contract is "never panics." Any crash artifact is a
  contract violation regardless of sanitizer.
- For the FEN parser, a crash on a valid FEN is a bug; a crash on invalid FEN means an internal
  `panic!` was used where `Err(...)` should have been returned.

### 11.6 Fuzz targets are not run by `cargo test`

- Fuzz targets live in `fuzz/fuzz_targets/` and are binaries, not `#[test]` functions.
- Running `cargo test` in the repo root will not run them.
- Regression tests must be copied back to `src/fen.rs::tests` or `src/uci.rs::tests` to enter the
  standard test suite.

### 11.7 Nightly drift

- Nightly Rust can break compilation of `fuzz/` between updates.
- Pin a specific nightly date in `fuzz/rust-toolchain.toml`:
  ```toml
  [toolchain]
  channel = "nightly-2025-06-01"
  ```
- Update the pin when a new long-term-stable nightly is confirmed working.
- Alternatively, use `channel = "nightly"` (latest) and accept occasional breakage.

---

## 12. Command reference

| Command | Purpose |
|---|---|
| `cargo fuzz init --fuzzing-workspace=true` | Initialize fuzz crate as independent workspace |
| `cargo fuzz add fuzz_fen` | Add a new target; updates `fuzz/Cargo.toml` |
| `cargo fuzz list` | List all defined targets |
| `cargo +nightly fuzz run fuzz_fen` | Start fuzzing (runs until interrupted or crash) |
| `cargo +nightly fuzz run fuzz_fen -- -max_total_time=7200 -max_len=200` | Run for 2 hours with 200-byte limit |
| `cargo +nightly fuzz run --sanitizer none fuzz_fen` | Run without ASan (~2× faster for pure-safe Rust) |
| `cargo +nightly fuzz run fuzz_fen fuzz/corpus/fuzz_fen` | Run with explicit corpus directory |
| `cargo +nightly fuzz tmin fuzz_fen <artifact>` | Minimize a crash input |
| `cargo +nightly fuzz cmin fuzz_fen` | Minimize the corpus (remove redundant entries) |
| `cargo +nightly fuzz coverage fuzz_fen` | Generate LLVM coverage report for fuzzing run |

Sources:
- [cargo-fuzz README](https://github.com/rust-fuzz/cargo-fuzz)
- [libFuzzer docs](https://llvm.org/docs/LibFuzzer.html)

---

## 13. Summary of recommendations

| Decision | Recommendation | Rationale |
|---|---|---|
| Tooling | `cargo-fuzz` (libFuzzer) | Only coverage-guided option on Apple Silicon macOS; community default |
| Workspace | `--fuzzing-workspace=true` | Isolates fuzz `Cargo.lock` from `cargo audit` / `cargo deny` policy |
| Nightly pin | `fuzz/rust-toolchain.toml` | Scoped to `fuzz/`; keeps main build on stable |
| Harness input type | `Arbitrary<String>` | Both parsers take `&str`; avoids UTF-8 waste; better mutation coherence |
| Sanitizer | `--sanitizer none` | Both targets are pure safe Rust; 2× throughput gain |
| `max_len` | `-max_len=200` for UCI; `-max_len=200` for FEN | Both grammars are short; tightens search space |
| Corpus seeds | Extracted from existing test positive cases | High-quality starting point; 10× faster coverage ramp |
| Run budget | 2–4 CPU-hours per target, per trigger | Adequate for small grammars; saturation typically in first hour |
| Crash → regression | `tmin` → `include_str!` unit test | Permanent record; enters standard `cargo test` suite |
| Cadence | Parser change triggers re-run; milestone backstop | Not in pre-commit; bounded by grammar complexity |
