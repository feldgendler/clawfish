# Plan: Split `src/bin/elo-iterate.rs` into a library + thin binary

**Unit name.** `tooling-elo-iterate-split`
**Source backlog item.** `docs/tooling-backlog.md` §"Split `src/bin/elo-iterate.rs` into a library + thin binary"
**Estimated size.** Pure mechanical refactor. Production-code line count unchanged (~12 000 lines), redistributed across 16 files. Net diff is one new `src/lib.rs` line (`pub mod elo_iterate;`), one new entry-point file, 15 extracted module files, and the gutting of `src/bin/elo-iterate.rs` to a ~10-line shim. No new tests; no production-logic changes; bench unchanged byte-for-byte (no compiled-code change beyond crate-boundary metadata).

## 1. Problem statement

After ELOH.E landed, `src/bin/elo-iterate.rs` is **11 955 lines** in a single file: 13 production sub-modules (`cli`, `prng`, `tc_sample`, `driver`, `adjudicate`, `estimator`, `sigma`, `sprt`, `pgn`, `summary`, `progress`, `match_loop`, `controller`) plus crate-root helpers (`fn main`, `outcome_to_termination_reason`, `outcome_to_pgn_result`, `format_tc`, `current_hostname`, `current_date_str`, `unix_days_to_date_str`, `gethostname` extern, `get_hostname`) plus the file-root `pub(crate) enum StopReason` (defined at line 6108, referenced cross-module by `mod progress` and `mod controller`) plus two test sub-modules (`#[cfg(test)] mod root_tests`, `#[cfg(test)] mod e2e_smoke`).

The 9000-line threshold called out in ELOH.E's plan §11 has been exceeded. Editor responsiveness, IDE indexing latency, blame-output legibility, and the cognitive cost of locating any one item are all degrading. Future units (the ML-tuned aspiration backlog item, any ELOH.F-tier follow-up) will keep adding lines to this file unless the topology breaks first.

## 2. Goal

Move every existing sub-module out of `src/bin/elo-iterate.rs` into its own file under `src/elo_iterate/<modname>.rs`, behind a new library module `clawfish::elo_iterate` declared at `src/lib.rs`. The binary at `src/bin/elo-iterate.rs` becomes a ~10-line entry point that calls `clawfish::elo_iterate::main()`.

**Zero behavioral change.** This is a **pure topological refactor**: no compiled-engine-code change, no item gains or loses functionality, no test changes its assertion. The only intended observable differences are file layout and Cargo's compilation topology: compiled artefacts now live partly under the `clawfish` rlib and the `elo-iterate` binary becomes a thin proxy.

### In scope

- Extract each of the 13 production `mod <name> { ... }` blocks into `src/elo_iterate/<name>.rs`.
- Move the crate-root helpers (`outcome_to_*`, `format_tc`, `unix_days_*`, `current_*`, `gethostname`/`get_hostname`) and the `pub(crate) enum StopReason` into `src/elo_iterate.rs` (the new top-level library file).
- Move `fn main()` into `src/elo_iterate.rs` as `pub fn main() -> ExitCode`.
- Move both test sub-modules (`#[cfg(test)] mod root_tests` and `#[cfg(test)] mod e2e_smoke`) into `src/elo_iterate.rs` alongside their helpers.
- Add `pub mod elo_iterate;` to `src/lib.rs`.
- Replace the body of `src/bin/elo-iterate.rs` with a one-line shim: `fn main() -> std::process::ExitCode { clawfish::elo_iterate::main() }`.
- Verify cargo build / test / clippy / fmt all pass; bench unchanged.

### Out of scope (deliberate)

- **No doc-comment edits.** Module-level `//!` headers travel unchanged. The crate-level `//!` docstring at the top of `src/bin/elo-iterate.rs` (the "Sub-module layout" listing) moves into `src/elo_iterate.rs` verbatim. The implementer is most likely tempted by this — resist; doc polish is a separate cleanup.
- **No visibility tightening.** `pub(crate)` and `pub(super)` items keep their current annotation. A handful of `pub(super)` items (e.g. `recv_until_bestmove_inner`, `clamp_uci_elo`) are intra-module-only callees and could legitimately become private — but they are pre-existing, harmless, and their semantics are preserved by the move (`pub(super)` continues to mean "visible to parent module"). Tightening them is a separate concern; mixing it with the move blurs the diff.
- **No test additions.** The existing 200+ tests are the regression net for this refactor — they all run after the move and pass without modification beyond import-path mechanics (see §6).
- **No mutation-coverage retune.** `.cargo/mutants.toml` rule patterns are anchored to module paths (e.g. `"in controller::run_iteration"`); after the move these become `"in elo_iterate::controller::run_iteration"`. **§7.3 details the migration.**
- **No new public surface on `clawfish`.** The only public addition to the library is `pub mod elo_iterate` at the crate root. The contents stay `pub(crate)`/`pub(super)`. The library does NOT re-export anything from `elo_iterate` at `clawfish::*`.
- **No splitting of any individual module.** A module that's currently 4000 lines stays 4000 lines in its new file. The threshold for value-from-splitting is "single-file friction"; splitting *within* a module is a separate decision per-module if and when its own size becomes painful.

## 3. Target layout

```
src/
├── bin/
│   └── elo-iterate.rs          ← ~10 lines (shim) — was ~12 000
├── elo_iterate/
│   ├── adjudicate.rs           ← extracted from lines  3079–4174 (≈1100 LOC)
│   ├── cli.rs                  ← extracted from lines    22–1542 (≈1500 LOC)
│   ├── controller.rs           ← extracted from lines  6712–11058 (≈4350 LOC)
│   ├── driver.rs               ← extracted from lines  2009–3075 (≈1070 LOC)
│   ├── estimator.rs            ← extracted from lines  4176–4302 (≈ 130 LOC)
│   ├── match_loop.rs           ← extracted from lines  6254–6708 (≈ 460 LOC)
│   ├── pgn.rs                  ← extracted from lines  5269–5752 (≈ 480 LOC)
│   ├── prng.rs                 ← extracted from lines  1546–1662 (≈ 120 LOC)
│   ├── progress.rs             ← extracted from lines  6123–6250 (≈ 130 LOC)
│   ├── sigma.rs                ← extracted from lines  4306–4616 (≈ 310 LOC)
│   ├── sprt.rs                 ← extracted from lines  4620–5265 (≈ 645 LOC)
│   ├── summary.rs              ← extracted from lines  5756–6101 (≈ 350 LOC)
│   └── tc_sample.rs            ← extracted from lines  1666–2005 (≈ 340 LOC)
├── elo_iterate.rs              ← new ~600 LOC: file-root helpers (incl. `pub(crate) enum StopReason`)
│                                  + fn main + mod declarations
│                                  + #[cfg(test)] mod root_tests + #[cfg(test)] mod e2e_smoke
└── lib.rs                      ← +2 lines: doc-comment + `pub mod elo_iterate;`
                                  (the doc comment is mandatory — see note below)
```

13 sub-module files + 1 facade file (`src/elo_iterate.rs`) + 1 thin binary + 1 lib.rs touch = **16 files modified or created**.

The crate carries `#![deny(missing_docs)]` at `src/lib.rs:1`. A bare `pub mod elo_iterate;` declaration fails the lint — the new module needs a `///` doc comment line above its declaration. So lib.rs grows by 2 lines (doc + decl), not 1. Suggested doc: `/// Tournament-harness binary glue (`elo-iterate`). See `docs/decisions/0020-eloh-harness.md`.`

(Line ranges are 1-based offsets in the current `src/bin/elo-iterate.rs`, captured at plan-write time. They shift after the first extraction; treat them as approximations to scope each block, not as anchors. The implementer locates each block by its `mod <name> {` opening line, which is grep-stable. The slight discrepancy between block sizes and file sizes comes from the leading `mod <name> {` and trailing `}` brace lines, plus the `// ---` divider banners between module blocks. Those banners get dropped on extraction — they were single-file scaffolding, redundant once each module owns a file.)

### `src/elo_iterate.rs` (new top-level library file)

Layout:

```rust
//! `elo-iterate` — in-process tournament harness.
//!
//! Drives two UCI engines via persistent subprocess pipes, plays a
//! colour-paired fixed batch of games with native adjudication, and emits
//! per-game PGN plus a summary line. Replaces `scripts/elo-iterate.sh` for
//! the correctness layer of online Elo iteration.
//!
//! Sub-module layout:
//!   - [`cli`]        — argument parsing.
//!   - [`driver`]     — subprocess + UCI line parsing.
//!   - [`adjudicate`] — native game-over detection.
//!   - [`match_loop`] — colour-paired game loop + per-side clock.
//!   - [`pgn`]        — PGN tag-roster + body emission.
//!   - [`summary`]    — summary.txt aggregation.
//!
//! See `docs/decisions/0020-eloh-harness.md`, ADR-0021, ADR-0022 for
//! architectural commitments.

pub(crate) mod adjudicate;
pub(crate) mod cli;
pub(crate) mod controller;
pub(crate) mod driver;
pub(crate) mod estimator;
pub(crate) mod match_loop;
pub(crate) mod pgn;
pub(crate) mod prng;
pub(crate) mod progress;
pub(crate) mod sigma;
pub(crate) mod sprt;
pub(crate) mod summary;
pub(crate) mod tc_sample;

use std::process::ExitCode;

/// Binary entry point. Called from `src/bin/elo-iterate.rs`.
pub fn main() -> ExitCode {
    // ... body of current fn main, verbatim ...
}

// crate-root helpers, verbatim:
#[derive(Debug, Clone, Copy, PartialEq, Eq)] #[allow(dead_code)]
pub(crate) enum StopReason { Sigma, MaxGames, SprtAcceptH0, SprtAcceptH1 }
pub(crate) fn outcome_to_termination_reason(outcome: &match_loop::GameOutcome) -> String { ... }
pub(crate) fn outcome_to_pgn_result(outcome: &match_loop::GameOutcome) -> (String, String) { ... }
pub(crate) fn format_tc(tc: cli::TimeControl) -> String { ... }
unsafe extern "C" { fn gethostname(...) -> i32; }
fn get_hostname(buf: &mut [u8]) { ... }
#[allow(dead_code)] pub(crate) fn current_hostname() -> String { ... }
#[allow(dead_code)] pub(crate) fn current_date_str() -> String { ... }
fn unix_days_to_date_str(days: u64) -> String { ... }

#[cfg(test)] mod root_tests { use super::*; ... }
#[cfg(test)] mod e2e_smoke { ... }
```

Module declarations are `pub(crate) mod <name>;` rather than `pub mod <name>;`: the modules' contents are crate-private (their items are `pub(crate)`/`pub(super)`), so the modules themselves don't need to be public. Only `elo_iterate::main` is public.

### `src/bin/elo-iterate.rs` (rewritten shim)

```rust
//! Thin binary entry point for the `elo-iterate` tournament harness.
//!
//! All logic lives in `clawfish::elo_iterate`. This file exists only so
//! Cargo has a `[[bin]]` target to compile.

fn main() -> std::process::ExitCode {
    clawfish::elo_iterate::main()
}
```

~7 lines including the `//!` docstring (the backlog item described it as a "~10-line entry point" — the actual minimum is closer to 5–7 once `clippy::missing_docs_in_private_items` and the existing `#![deny(missing_docs)]` are honored). The `[[bin]] name = "elo-iterate"` declaration in `Cargo.toml` keeps its `path = "src/bin/elo-iterate.rs"` — no Cargo change required.

## 4. Cross-module reference invariant

This refactor preserves a key depth invariant: **every cross-module reference path that works in the current single-file layout continues to point to the same target after the move.** No `use` rewrites are needed in the body of any module beyond the implicit re-anchoring of `super::` against the new file boundaries.

### 4.1 The depth-invariant argument

Define **module-tree depth** as the number of `super::` hops from a given context to a target.

**Before (single file):**
- File root holds `mod cli`, `mod driver`, `mod adjudicate`, ..., `fn main`. The file root *is* the binary crate root.
- Inside `mod adjudicate { fn body { ... } }`: `super::driver` = sibling of `adjudicate` at file root = `crate::driver`. **Hops: 1.**
- Inside `mod adjudicate { mod tests { fn t { ... } } }`: `super::super::driver` = `crate::driver`. **Hops: 2.**

**After (split):**
- `src/elo_iterate.rs` declares `mod adjudicate; mod driver; mod cli; ...`. `crate::elo_iterate` is the parent module of all 13.
- Inside `src/elo_iterate/adjudicate.rs` (which represents `crate::elo_iterate::adjudicate`): `super::driver` = sibling of `adjudicate` under `crate::elo_iterate` = `crate::elo_iterate::driver`. **Hops: 1.**
- Inside that file's `mod tests`: `super::super::driver` = `crate::elo_iterate::driver`. **Hops: 2.**

The hop count is identical. `super::driver` and `super::super::driver` resolve to *different absolute paths* (`crate::driver` vs `crate::elo_iterate::driver`), but the resolution is correct in both cases because all 13 modules co-move and remain siblings. **The relative paths don't have to change.**

### 4.2 Verified survey of cross-module imports

Grep for `use super::` and `use crate::` across the file; survey cross-module hits:

| Line | Form | Where | Notes |
|---|---|---|---|
| 1774 | `use crate::cli::TimeControl;` | inside `tc_sample::tests` (`super::super::super::cli::TimeControl` would be the `super::`-form alternative) | **Currently rooted at the binary crate.** After the move, `crate::cli` no longer exists; this becomes `crate::elo_iterate::cli` → must rewrite. |
| 5402 | `use crate::driver::{LastInfo, Score};` | inside `pgn::tests` | Same: rewrite to `crate::elo_iterate::driver`. |
| 3089 | `use super::driver;` | top of `mod adjudicate` | `super::driver` works in both layouts (§4.1). **No rewrite.** |
| 3791 | `use super::super::driver::Score::{Cp, Mate};` | inside `mod adjudicate { mod tests { mod sub { ... } } }` | `super::super::` reaches the file root (before) / `crate::elo_iterate` (after). **Both correct.** No rewrite. |
| 4355 | `use super::super::estimator;` | inside `sigma::tests` | Same. No rewrite. |
| 5276 | `use super::driver::LastInfo;` | top of `mod pgn` | Same. No rewrite. |
| 5319 | `use super::driver::Score;` | inside `pgn::format_pgn` | Same. No rewrite. |
| 6126 | `use super::StopReason;` | top of `mod progress` | **Verified during planning:** `StopReason` is defined at file root (line 6108), not inside any sub-module. After the move it lives at the file root of `src/elo_iterate.rs`. `super::StopReason` from inside `progress.rs` resolves to `crate::elo_iterate::StopReason` — same target. **No rewrite.** |
| 6264 | `use super::adjudicate::GameOver;` | top of `mod match_loop` | Same. No rewrite. |
| 6361 | `use super::adjudicate::{ ... };` | inside `match_loop::play_one_game` | No rewrite. |
| 6364 | `use super::driver::{ ... };` | same | No rewrite. |
| 6365 | `use super::pgn::PgnMove;` | same | No rewrite. |
| 6887 | `use super::adjudicate::GameOver;` | **inside `controller::compute_clawfish_score`** (a regular `pub(super) fn` at line 6883, NOT inside `mod tests`) | `super::` from inside that function reaches `mod controller`'s parent = file root. `super::adjudicate` = file-root sibling = `crate::elo_iterate::adjudicate` after the move. **No rewrite.** |
| 8548 et al. (8559, 8572, 8584, 8597, 8623) | `super::super::adjudicate::GameOver` / `super::super::match_loop::GameOutcome` | inside `controller::tests` (depth 2 nested) | `super::super::` reaches file root, then `adjudicate`. Same target after the move (depth invariant §4.1). No rewrite. |
| 10342, 10350, 10449, 10456 | `super::super::super::driver::EngineSpec` / `super::super::super::cli::Thresholds` / `super::super::super::cli::TimeControl` | inside `controller::tests::production_worker_tests` (depth 3 nested, the deepest module nesting in the file) | `super::super::super::` reaches file root from depth 3. Under the new layout, depth 3 hops up from inside `production_worker_tests` (which is depth 4: `crate::elo_iterate::controller::tests::production_worker_tests`) lands at `crate::elo_iterate`, where `driver` / `cli` are siblings. **Same target.** No rewrite. |

### 4.3 The two `use crate::` rewrites

Two import lines need explicit rewriting because they reach for `crate::` (binary crate root) by absolute path:

- `src/bin/elo-iterate.rs:1774` — `use crate::cli::TimeControl;` → after move becomes `use crate::elo_iterate::cli::TimeControl;`.
- `src/bin/elo-iterate.rs:5402` — `use crate::driver::{LastInfo, Score};` → after move becomes `use crate::elo_iterate::driver::{LastInfo, Score};`.

Alternatively, both can be rewritten with `super::` forms (e.g. `use super::super::super::cli::TimeControl;`) to keep the file-relative style that the rest of the codebase uses. The implementer chooses; both work.

### 4.4 The `clawfish::*` imports

Several modules use `use clawfish::{Color, Move, Position, ...};` — references to the parent library crate's public surface. **These continue to work unchanged after the move.** The rust compiler resolves `clawfish::*` from any context within the `clawfish` crate (now including `crate::elo_iterate::*`) the same way it does from a separate binary crate.

Actual sites: line 6262 (`use clawfish::{Color, MatchTimeMode, PerSideClock};` inside `mod match_loop`), 6366 (`use clawfish::{Color, Move, Position, generate_moves};`), and a handful of others. No rewrites needed.

## 5. Items verified during planning + items to verify during implementation

The first three items below were resolved during the plan-writing pass via direct source reads. They are documented here so the implementing agent doesn't repeat the work.

1. **Verified: lines 6887–6888 are in a regular function, not in `mod tests`.** Lines 6883–6890 sit inside `pub(super) fn compute_clawfish_score(...)` at module-body scope inside `mod controller`. So `super::adjudicate::GameOver` resolves through one `super::` hop = file root → `adjudicate`. No re-export aliasing is involved; the import chain is straightforwardly correct under the depth invariant. After the move, the import resolves to `crate::elo_iterate::adjudicate::GameOver` without rewrite.
2. **Verified: `StopReason` is at file root (line 6108), not inside `mod controller`.** `pub(crate) enum StopReason` is defined at file scope between `mod summary` and `mod progress` with the comment `// StopReason — crate-internal; shared by mod progress and (eventually) mod controller`. `super::StopReason` from inside `progress.rs` resolves to `crate::elo_iterate::StopReason` after the move (the new file root). `super::super::StopReason` from inside `controller::tests` (e.g. lines 8136, 8213, 9694, 9720, 9745, 9800, 10291) resolves the same way. **No rewrite needed.** *(This item was originally listed as "the single most likely import to need a rewrite"; that was a misread, corrected after the plan-review pass.)*
3. **Verified by §4.2: only two imports use absolute `crate::*` paths** (`crate::cli::TimeControl` at line 1774, `crate::driver::{LastInfo, Score}` at line 5402). Both must be rewritten per §4.3.

Items still to verify during implementation:

4. **Verify all `pub(super)` items are referenced only from within their own module's file.** If any `pub(super)` item is referenced from a different sub-module, the move would break the visibility because `pub(super)` after the move means "visible only to `crate::elo_iterate`", which is what most cases want, but it would be tighter than the previous "visible to file root", though semantically equivalent for our layout. **Grep table to confirm:**

   - `pub(super) fn recv_until_bestmove_inner` (line 2323) — used at lines 2311, 2496, 2520, 2529, 2538 — all inside `mod driver` (its own module + its `mod tests`). ✓
   - `pub(super) fn wait_for_uciok_inner` (line 2374) — verify only inside `mod driver`.
   - `pub(super) fn pure_apply_move_clock_update` (line 6331) — verify only inside `mod match_loop`.
   - `pub(super) type WorkerFn` (line 6828) — verify only inside `mod controller`.
   - `pub(super) fn clamp_uci_elo` (line 6839) — verify only inside `mod controller`.
   - `pub(super) enum ScoreClass` (line 6864) — verify only inside `mod controller`.
   - `pub(super) fn classify_score` (line 6871) — verify only inside `mod controller`.
   - `pub(super) fn compute_clawfish_score` (line 6883) — verify only inside `mod controller`.
   - `pub(super) fn handshake_caps_missing` (line 6927) — verify only inside `mod controller`.
   - `pub(super) fn post_setoption_readyok_succeeded` (line 6939) — verify only inside `mod controller`.
   - `pub(super) fn either_per_game_readyok_failed` (line 6948) — verify only inside `mod controller`.
   - `pub(super) fn spawn_workers_with_fn` (line 7189) — verify only inside `mod controller`.

   **If all are intra-module:** `pub(super)` continues to work after the move (`super` of an item inside `controller.rs` is `crate::elo_iterate`, which is broader than the strict need, but the item is still visible to the only caller). No rewrites.

   **If any one is cross-module:** that item must be promoted to `pub(crate)` (or kept as `pub(super)` if the crossing happens through `crate::elo_iterate` only — same effective scope). Document the promotion in the implementation report.

5. **Verify the `gethostname` extern block at line 11223** moves with the file-root helpers. `extern "C"` blocks at module scope are perfectly valid in `src/elo_iterate.rs`; no special handling needed.

6. **Verify `option_env!("CARGO_BIN_EXE_clawfish")` and `option_env!("CARGO_BIN_EXE_elo-iterate")` continue to work** in `e2e_smoke` after the move. These env vars are set by Cargo for **integration tests only** — i.e., when tests run in a `tests/*.rs` integration-test crate context. Inside the library's unit tests (which `mod e2e_smoke` will become), they are **not set** — `option_env!` returns `None`. The existing `resolve_bin` helper has a fallback that walks up from `current_exe()` looking for `target/<profile>/<name>`, so the tests still resolve the binary, but only when binaries have been pre-built. The fallback is already exercised today (the existing tests are `#[ignore]`-gated and assume `cargo build --release` was run first). The §8.10 gate runs `cargo clean && cargo test --release -- --ignored` empirically post-move to confirm.

7. **Verify that the `outcome_to_termination_reason`, `outcome_to_pgn_result`, `format_tc`, `current_hostname`, `current_date_str` helpers** are referenced only via `super::<helper>` from inside sub-modules (not via absolute `crate::<helper>` paths). Quick grep confirms only `super::` forms (lines 5797, 7361, 7362, 7365, 7366, 7385). **All survive the move with no rewrite.**

## 6. Test file handling

All 13 production sub-modules contain a `#[cfg(test)] mod tests { ... }` inside the same `{ ... }` block. **Tests travel with their module.** When `mod cli { ... }` is extracted into `src/elo_iterate/cli.rs`, the `#[cfg(test)] mod tests` inside it goes along.

Two test sub-modules at file root (`mod root_tests`, `mod e2e_smoke`) move into `src/elo_iterate.rs` alongside the file-root helpers they test.

**No new test files. No splitting of test modules. No reorganization of test code.**

The `use super::*;` form at the top of every `mod tests` block (lines 683, 1592, 1773, ...) continues to resolve to the parent module after the move. `use super::super::driver::...` forms (line 3791) likewise — see §4.1.

## 7. Implementation sequence

The work is mechanical enough that a single chess-coder agent can do it in one pass. A multi-agent split (one agent per module) would actually be slower because each agent would need to read the full file to understand cross-references; sequential single-agent execution is the right shape.

### 7.1 Sub-task ordering

1. **Add `pub mod elo_iterate;` to `src/lib.rs`** (1 line).
2. **Create `src/elo_iterate.rs`** with the file-level docstring, `mod <name>;` declarations for all 13 sub-modules, the file-root helpers, `fn main`, and the two test sub-modules. This file is created empty initially; sub-module content moves into it incrementally.
3. **For each of the 13 production modules**, in any order (no inter-dependencies for the move itself):
   a. Create `src/elo_iterate/<name>.rs`.
   b. Copy the body inside `mod <name> { ... }` (i.e., everything between the opening `{` and closing `}` braces, *not* including the `mod <name> {` header line) into the new file.
   c. Drop the leading 4-space indentation that was inside the embedded `mod <name>` block (each line had 4 leading spaces because of the wrapping module).
   d. Delete the `mod <name> { ... }` block (and its preceding `// ---` divider banner) from `src/bin/elo-iterate.rs`.
4. **After all 13 modules are extracted:**
   a. Move the file-root helpers (`outcome_to_*`, `format_tc`, `unix_days_*`, `current_*`, `gethostname`/`get_hostname`) and `pub(crate) enum StopReason` from `src/bin/elo-iterate.rs` into `src/elo_iterate.rs`.
   b. Move `fn main` into `src/elo_iterate.rs` **and promote its visibility to `pub fn main`** at the same time (the binary shim calls `clawfish::elo_iterate::main()` across the crate boundary; without `pub`, the call doesn't compile). Don't defer this to a later step.
   c. Move `mod root_tests` and `mod e2e_smoke` into `src/elo_iterate.rs`.
5. **Apply the §4.3 import rewrites:**
   - `src/elo_iterate/tc_sample.rs`: `use crate::cli::TimeControl;` → `use crate::elo_iterate::cli::TimeControl;`.
   - `src/elo_iterate/pgn.rs`: `use crate::driver::{LastInfo, Score};` → `use crate::elo_iterate::driver::{LastInfo, Score};`.
6. **Rewrite `src/bin/elo-iterate.rs`** to the §3 shim. Note: `src/bin/mock_engine.rs` and `src/bin/magicgen.rs` stay where they are — they're binaries unrelated to this refactor.

### 7.2 Compile-driven debugging

Each module move should leave the project in a **build-passing** state if done correctly, assuming the §5 verifications and §4.3 rewrites are honored before the first `cargo build`. The implementer should run `cargo build` after every 2–3 module extractions to catch any unforeseen import issue early. Likely failure modes (residual, after the §5 verifications resolved the StopReason worry):

- **`pub(super)` item invisible to a cross-module caller** (§5 item 4) — promote to `pub(crate)` if any case turns up. Per the planning-time grep, none of the 12 `pub(super)` items has a cross-module caller, so this should not fire.
- **`super::cli::TimeControl` (or `super::driver::{LastInfo, Score}`) resolves to `crate::cli::TimeControl` and fails** — this is the §4.3 case; rewrite the import to `crate::elo_iterate::cli::TimeControl` (or the `super::super::super::cli::TimeControl` form).
- **`pub mod elo_iterate;` in `src/lib.rs` fails the `#![deny(missing_docs)]` lint** — add the `///` doc comment per §3.

After all modules are extracted, the final `cargo build` and `cargo test --all-targets` must both pass.

### 7.3 Mutation-config rule paths

`.cargo/mutants.toml` exclude rules currently reference `controller::production_worker_fn`, `controller::run_iteration`, `pgn::format_pgn`, etc. These are module paths within the binary crate `elo-iterate`. After the move, the same logic lives at `clawfish::elo_iterate::controller::production_worker_fn` etc., and `cargo mutants` runs against the library crate.

**Action:** after the move, regenerate the mutant survey. Two checks needed because they exercise different config behavior:

1. **`cargo mutants --in-diff`** confirms no new un-caught mutants on the diff. **But:** the diff is almost entirely identity-of-line module moves — cargo-mutants is expected to generate few-to-zero mutants on the diff (line-pair-equivalence detection). So `--in-diff` does NOT exercise the rule patterns. *(Acknowledging the gap the plan-reviewer flagged: §8.8 expects "no mutants generated against newly-changed lines", which means the `.cargo/mutants.toml` rule-path correctness is structurally untestable via `--in-diff` alone.)*
2. **`cargo mutants -f src/elo_iterate/controller.rs`** (or whichever moved file carries the rules' targets) is the explicit rule-path-correctness check. Run this *once* after the move; verify that the items the original rules excluded (function-anchored exclusions like `"in controller::production_worker_fn"`) are still excluded under the new module path. Two possible outcomes:
   - **Rules still match** — if cargo-mutants matches against the leaf module path (`controller::production_worker_fn`) regardless of crate prefix, no rule changes needed. (Empirical question; cargo-mutants 27.0.0 documentation suggests it uses fully-qualified `<crate>::<mod_path>::<fn>` strings, but the test confirms.)
   - **Rules need re-anchoring** — rewrite each rule from `"in controller::..."` to `"in elo_iterate::controller::..."` or similar.

Either way, the existing **mutation-coverage** stays the same (the same source lines are excluded for the same reasons; no production code changed); only the path syntax in the config might shift. The §7.3 single-file mutation run is the gate; §8.8 is informational.

## 8. Validation gates

Before proposing the final review:

1. `cargo build --all-targets` clean (no warnings).
2. `cargo test --all-targets` clean. All ~200+ tests pass. No test count regression.
3. `cargo clippy --all-targets -- -D warnings` clean.
4. `cargo fmt --check` clean.
5. `cargo doc --no-deps` clean — catches `#![deny(missing_docs)]` regressions on the new `pub mod elo_iterate` declaration and any moved item whose doc-link path doesn't survive the move. (Cheap to run; valuable signal.)
6. **Engine NPS bench signature byte-identical** to `bench: 1466436 nodes <NPS> nps`. Run via `cargo run --release --bin clawfish -- bench`. (Not `cargo bench` — that's the Criterion benches under `benches/`, which don't carry the bench-signature contract.) The number is the M5.E baseline; **no compiled-engine-code change** for this refactor means the bench node count must be byte-identical. `<NPS>` may vary run-to-run (depends on machine load); only the node count is the contract.
7. `cargo llvm-cov --summary-only` reports unchanged or higher coverage (a refactor that *moves* lines never reduces coverage; a regression here would indicate a test that quietly stopped running).
8. `cargo mutants --in-diff` reports zero new un-caught mutants. **Expected outcome** is "no mutants generated against newly-changed lines" — the diff is all moves, so cargo-mutants should detect the line-pair-equivalence and not generate anything. (See §7.3 — this gate does NOT exercise rule-path correctness; the §7.3 single-file mutation run is the gate for that.)
9. **§7.3 single-file mutation check** — `cargo mutants -f src/elo_iterate/controller.rs` (and any other file carrying `.cargo/mutants.toml` rule targets). Confirms the rule patterns still match after the path shift. May produce wallclock cost (~5–10 min); run once.
10. **Empirical post-move e2e_smoke check.** Run `cargo clean && cargo test --release -- --ignored elo-iterate` and verify all `e2e_smoke` tests pass. The smoke tests' binary-resolution behavior changes from binary-test-context to library-test-context; Cargo *should* still auto-build all `[[bin]]` targets when running `cargo test`, but this is empirical. Includes the four `end_to_end_*` tests; the Stockfish-vs-clawfish one is skip-on-missing if Stockfish isn't on PATH. **If this gate fails:** §9.3 escape hatch — promote `mod e2e_smoke` to a `tests/elo_iterate_e2e.rs` integration-test crate.

## 9. Risks and mitigations

### 9.1 The `crate::cli::` and `crate::driver::` rewrites

Risk: **forgetting** the §4.3 rewrite in `tc_sample.rs` and `pgn.rs`, leading to a compile failure.
Mitigation: explicit grep for `crate::cli::` and `crate::driver::` after the move; rewrite both. The compile error is loud and pinpointed.

### 9.2 `StopReason` import in `progress.rs` *(resolved during planning)*

Originally flagged as "the single most likely import to need a rewrite". **§5 item 2 verified by direct source read** that `StopReason` is defined at file root (line 6108) — it moves with the file-root helpers into `src/elo_iterate.rs`, and `super::StopReason` from inside `progress.rs` continues to resolve correctly. No rewrite needed. Listed here only because the worry was material at plan-write time.

### 9.3 `e2e_smoke` placement

Risk: `option_env!("CARGO_BIN_EXE_*")` returns `None` inside library unit tests, and `resolve_bin` falls back to walking from `current_exe()`. **For the library-side unit tests, `current_exe()` is the test runner inside `target/release/deps/`, so the walk-up to `target/release/<name>` works the same way it works today inside the binary's tests.** Mitigation: re-run `cargo test --release -- --ignored` after the move and verify all four `e2e_smoke` tests pass.

If a test breaks, fall back to: move `mod e2e_smoke` into a `tests/elo_iterate_e2e.rs` integration-test crate. That gets `CARGO_BIN_EXE_*` set automatically. **Out of scope for the initial pass; only triggered if §8.3 fails.**

### 9.4 Public surface bloat

Risk: the new `pub mod elo_iterate;` makes 13 modules + a `main()` reachable as `clawfish::elo_iterate::*` from any consumer of the library. Not a problem now (no external consumers; `Cargo.toml` is `publish = false`), but worth documenting.
Mitigation: keep `mod` declarations as `pub(crate) mod`, not `pub mod`; the modules' contents are crate-private already. Only `pub fn main` is visible outside `clawfish`.

### 9.5 `bench` signature drift

Risk: a missed import or a #[cfg(test)] item accidentally moved into a production-code path causes the binary to compile with different code, shifting the bench number.
Mitigation: §8.6's byte-equal bench check. If it shifts, bisect by reverting modules one at a time.

### 9.6 IDE indexing / blame churn

`git blame` on the moved modules will pin to the refactor commit, not the original feature commit. Mitigation: the commit message points at the original ELOH.A–E commits as the source of truth for line-level intent. This is unavoidable for any file move in git; `git blame --follow` traces across the rename.

### 9.7 Hidden cyclic-import risk

Two modules in the survey reference each other (e.g. `match_loop` uses `adjudicate`; `adjudicate` doesn't use `match_loop` directly). Per the §4 survey, all cross-module references go *down* the dependency chain (`controller` → `match_loop` → `adjudicate` → `driver`; `pgn` ↔ `driver`; `summary` → `sprt`). No cycle survives the move because no cycle exists today; the move is identity-preserving. **Mitigation:** none needed. If a cycle appears, the compile error will pinpoint it; back out the suspect module's imports.

## 10. Workflow loop

This is a **mechanical refactor** — research / approach-choice / blind-test-suite-review do not apply.

- **Plan:** this document; one-pass blind plan-review (workflow §"plan-review loop").
- **Tests:** existing tests are the regression net; **no test-suite-review loop** because no new tests are written.
- **Implement:** single chess-coder agent per §7.
- **Final review:** one-pass blind final-reviewer over the resulting tree (workflow §"final-review loop"). Reviewer's main concerns will be: import correctness, visibility correctness, tests still co-located with modules, no accidental dead-code creation, no doc-comment regressions.
- **Benchmark:** §8.6 byte-equal bench check. If equal, the refactor is logic-preserving.
- **Commit:** §11.

## 11. Commit

Single commit, one logical change. Suggested message:

```
tooling(elo-iterate): split into library + thin binary

src/bin/elo-iterate.rs (~12 000 lines, 13 sub-modules + crate-root helpers)
extracted into src/elo_iterate/<modname>.rs files behind a new
clawfish::elo_iterate library module. Binary becomes a 5-line shim.

Pure topological refactor: no production logic changes, no test
changes, bench unchanged byte-for-byte (bench: 1466436 nodes <NPS> nps).

Files modified:
  src/lib.rs                                   +2 lines  (doc + pub mod elo_iterate)
  src/bin/elo-iterate.rs                       overwritten (~12000 → ~7)
  src/elo_iterate.rs                           new (~600 LOC: file-root helpers,
                                                  StopReason, fn main, mod decls,
                                                  root_tests + e2e_smoke)
  src/elo_iterate/{cli,driver,adjudicate,
    match_loop,pgn,summary,progress,
    estimator,sigma,sprt,prng,tc_sample,
    controller}.rs                             new (13 files)
  .cargo/mutants.toml                          rule-path rewrites if §7.3 confirms

Validation: cargo build/test/clippy/fmt/doc clean; bench identity-pin held;
mutation rule-path correctness re-confirmed via single-file mutation run.

Closes the "Split src/bin/elo-iterate.rs into a library + thin binary"
backlog item (docs/tooling-backlog.md).
```

LOC: ~12 000 lines moved across 16 files, net +1 line in `src/lib.rs` and ~−5 in the shim. Pure refactor; no behavioral change.

## 12. Out of scope (deferred)

- **Tightening over-broad `pub(super)` to private** for the dozen items at §5.2 that are intra-module-only callees. Separate cleanup; not gating any milestone.
- **Splitting any individual module** (e.g. `controller.rs` will be ~4350 LOC after the move). Per §2's out-of-scope clause, single-module splits are decided per-module if and when their own size becomes painful.
- **Promoting `mod root_tests` / `mod e2e_smoke` to integration tests under `tests/`.** The §9.3 fallback makes this a contingency, not a default. Defer unless §8.3 fails.
- **Renaming `mod elo_iterate` to something more chess-themed** (e.g. `harness`). The current name is anchored in 13 ADRs and 5 milestone retrospectives; renaming churns docs without value. Keep.
- **Re-exporting `clawfish::elo_iterate::cli::Args` etc. at the `clawfish::*` level** for ergonomic access. No external consumer exists; not needed.
