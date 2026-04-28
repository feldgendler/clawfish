# M1 Prior-Art Research: Perft Methodology and Rust Project Layout

*Prepared for the chess engine project. M1 move-generation milestone.*

---

## Perft Methodology

### Recursive definition and bulk-counting optimization

Perft ("performance test, move path enumeration") is the gold standard for validating a move generator. The recursive definition is simple: from a position, generate all legal moves, make each one, recurse to depth−1, unmake each move, and sum the child counts. At depth 0 the function returns 1 (the current node counts as a leaf).

```
perft(pos, depth):
    if depth == 0: return 1
    moves = generate_legal_moves(pos)
    total = 0
    for m in moves:
        make(pos, m)
        total += perft(pos, depth - 1)
        unmake(pos, m)
    return total
```

**Bulk-counting optimization:** Instead of bottoming out at depth 0, return the number of legal moves at depth 1 without performing make/unmake on those leaf positions. This works because every leaf would return 1 anyway — the count at the parent is exactly the number of children. The CPW article notes this "could improve speed significantly." Community experience cites roughly **20–30% speedup** from eliminating make/unmake on all leaf nodes, which are the majority of nodes visited. The tradeoff is that collecting per-leaf categorized statistics (captures, checks, etc.) becomes harder or impossible at that ply — important to know before committing to when categorized counting happens.

**Recommendation:** Implement plain recursive perft first. Add bulk counting as a separate code path once correctness is confirmed. Keep both paths available: the plain path for collecting internal categorized counts, the bulk path for speed regression benchmarks.

Sources: [Perft — Chess Programming Wiki](https://www.chessprogramming.org/Perft); [Mediocre Chess perft guide](http://mediocrechess.blogspot.com/2007/01/guide-perft-scores.html)

---

### Divide perft as a debugging tool

**Divide** is a variant of perft that prints, for each legal move from the root position, that move in UCI notation followed by `perft(depth − 1)` of the resulting position. The final line prints the total. Example output from Stockfish at depth 5, starting position:

```
a2a3: 181046
b2b3: 215255
...
Nodes searched: 4865609
```

**Debugging workflow:** When your engine's total at depth N disagrees with Stockfish's, run divide at depth N on both. Find the move where the subtree counts diverge. Make that move, run divide at depth N−1. Repeat, narrowing the subtree depth by one each iteration, until you reach depth 1 or 2 and can read the list of moves directly to spot the illegal move, duplicate move, or missing move.

**`go perft N` convention:** Stockfish accepts `go perft N` via UCI (technically a Stockfish extension, not in the base UCI spec, but ubiquitous in the community). It responds with the divide output above. Supporting the same command and format means any tool that talks to Stockfish can talk to our engine without modification.

**`position fen … ; go perft N`:** The Stockfish workflow also supports arbitrary FEN input via the `position fen <fen>` command before `go perft N`. This is essential for testing all six canonical positions. Implement this from day one — perft on the start position alone will not catch en-passant, castling, or promotion bugs.

**Tooling:** [perftree](https://github.com/agausmann/perftree) is a semi-automated interactive divide debugger. You provide a script that produces your engine's divide output; perftree queries Stockfish in parallel and highlights differing lines. This is far faster than manual binary-search tree navigation. Worth having set up before M1 testing begins.

Sources: [Perft — CPW](https://www.chessprogramming.org/Perft); [perftree GitHub](https://github.com/agausmann/perftree)

---

### Detailed perft (categorized counts) as internal sanity

The standard categorized perft tracks additional event counts alongside the raw node count. The conventional set, as used in CPW's reference tables, is:

| Category | How detected |
|---|---|
| **Captures** | The destination square was occupied before the move (or en-passant flag set) |
| **En passant** | Move is flagged as en-passant (separate from general captures) |
| **Castles** | King moves two squares sideways |
| **Promotions** | Pawn reaches rank 8 or rank 1; count regardless of promotion piece |
| **Checks** | The side to move at the *next* ply is in check after the move |
| **Discovery checks** | Check delivered by a piece other than the moving piece |
| **Double checks** | Both the moving piece and a revealed piece give check simultaneously |
| **Checkmates** | Position at the leaf has no legal moves and the side to move is in check |

Note: **discovery checks** and **double checks** are subsets of checks. Double checks are a subset of discovery checks. Some implementations only track the parent categories; CPW tracks all five check variants for the initial position and Kiwipete.

Per ADR-0006, our engine will produce these counts as **internal validation only**. They are not cross-validated against external sources because different engines count edge cases (e.g., whether promotion+check counts as one promotion or whether stalemate is counted in checkmates) differently. The counts are useful for catching specific bug classes — a zero en-passant count at depth 5 from the starting position immediately indicates a broken EP generator.

**Bulk-counting interaction:** If bulk counting is used, categorized counts must be collected at the parent ply (depth 1), not at depth 0. A capture is detected when making the move at depth 1, so it is still accessible; the leaf-skip only removes the depth-0 recursive call.

Sources: [Perft Results — CPW](https://www.chessprogramming.org/Perft_Results)

---

### The canonical six test positions

These are the positions the community uses universally. FENs are as published on CPW. The "Position 4 mirror" has identical perft counts and exercises the same rules from the opposite color's perspective — worth keeping in the suite.

**Position 1 — Initial position**
```
rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1
```

**Position 2 — Kiwipete** (rich mix of castling, promotions, en passant)
```
r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K22R w KQkq -
```

**Position 3** (en passant, discovered check, promotion stress)
```
8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1
```

**Position 4**
```
r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1
```

**Position 4 mirror**
```
r2q1rk1/pP1p2pp/Q4n2/bbp1p3/Np6/1B3NBn/pPPP1PPP/R3K2R b KQ - 0 1
```

**Position 5** (promotions with checks)
```
rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8
```

**Position 6** (Steven Edwards' position)
```
r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10
```

**Practical depth guidance** (rough wall-clock thresholds on modern hardware, optimized engine):

| Position | Fast (< 1 s) | Moderate (seconds) | Slow (minutes) | Avoid in CI |
|---|---|---|---|---|
| 1 (Initial) | D1–D4 | D5 | D6 | D7+ |
| 2 (Kiwipete) | D1–D3 | D4 | D5 | D6+ |
| 3 | D1–D4 | D5 | D6 | D7+ |
| 4 | D1–D3 | D4 | D5 | D6+ |
| 5 | D1–D3 | D4 | D5 | — |
| 6 | D1–D3 | D4 | D5 | D6+ |

Early in M1 (unoptimized interpreter), D4 for positions 1 and 3, D3 for the rest is a practical ceiling for fast-test runs. Add the D5/D6 cases as `#[ignore]`-gated tests from the start; run them explicitly when the generator is believed correct.

Sources: [Perft Results — CPW](https://www.chessprogramming.org/Perft_Results); [Perfect Perft — chessprogramming.net](https://www.chessprogramming.net/perfect-perft/)

---

### Bulk EPD regression suites

There **is** a community-circulated EPD perft regression suite. The most referenced is maintained by Chris Whittington: [`Chess-EPDs/perft.epd`](https://github.com/ChrisWhittington/Chess-EPDs/blob/master/perft.epd). It contains approximately 175 positions in the format:

```
<FEN>; D1 <count>; D2 <count>; ... D6 <count>
```

Example line:
```
k7/6p1/8/8/8/8/7P/K7 b - - 0 1; D1 5; D2 25; D3 161; D4 1035; D5 7574; D6 55338
```

The 6 canonical positions appear in the file, as does the starting position. Running all 175 positions to D5 on an optimized engine takes seconds to minutes; to D6 takes longer. The CPW recommends running every position to about depth 6 to surface essentially all move-generation bugs.

Per ADR-0006, **we will not use these published counts as test fixtures**. We will use this file's *position list* as a convenient input corpus, then generate our own expected counts by running Stockfish locally. This sidesteps any subtle rule-interpretation differences between engines. The position list from `perft.epd` is a useful source of varied positions covering en passant, promotion, castling edge cases — its value is in the diversity of positions, not the pre-computed numbers.

Additionally, [sohamkorade/autoperft](https://github.com/sohamkorade/autoperft) automates running a UCI engine against an EPD suite and comparing results to Stockfish. Worth examining for our test harness design.

Sources: [Chess-EPDs/perft.epd](https://github.com/ChrisWhittington/Chess-EPDs/blob/master/perft.epd); [autoperft](https://github.com/sohamkorade/autoperft)

---

### Test harness pattern in Rust

**Integration tests vs inline unit tests:**

- Place perft tests as integration tests in `tests/perft.rs` (or `tests/perft/`). Integration tests import the library's public API (`use clawfish::perft;`), which matches how users of the library would call it, and they compile as a separate crate. This is the right home for cross-position regression suites.
- Use inline `#[cfg(test)] mod tests` inside `src/perft.rs` for unit-level checks of the perft function's internal logic (e.g., that depth-0 returns 1, that depth-1 from a trivial position matches manual count).

**Gating slow tests with `#[ignore]`:**

The standard Rust convention for slow tests is:

```rust
#[test]
#[ignore]
fn perft_kiwipete_depth5() {
    // takes ~10 seconds
}
```

- `cargo test` — runs only non-ignored tests (fast suite, safe for every commit).
- `cargo test -- --ignored` — runs only the ignored tests (slow suite, run before marking a milestone done).
- `cargo test -- --include-ignored` — runs everything.

This is cleaner than feature flags for a binary slow/fast distinction, and it requires no Cargo.toml changes. Feature flags are better when a test requires a heavyweight dependency that you don't want compiled by default — not the case here.

**Suggested file layout for perft tests:**

```
tests/
  perft.rs          # canonical 6 positions, fast depths (not ignored)
  perft_slow.rs     # canonical 6 positions, D5–D6 (#[ignore] on each)
  perft_suite.rs    # all 175 EPD positions at D4 (#[ignore] on the bulk run)
```

Or equivalently, a single `tests/perft.rs` with modules:

```rust
mod fast { /* D3-D4 tests, no #[ignore] */ }
mod slow { /* D5-D6 tests, all #[ignore] */ }
```

The single-file approach is simpler to start; split only when compile time becomes noticeable.

Sources: [Test Organization — The Rust Book](https://doc.rust-lang.org/book/ch11-03-test-organization.html); [Controlling How Tests Are Run — The Rust Book](https://doc.rust-lang.org/book/ch11-02-running-tests.html)

---

## Rust Project Layout

### Library + binary split

The standard Cargo pattern for an engine is a single package containing both `src/lib.rs` (the library crate) and `src/main.rs` (the thin binary that invokes UCI). `src/main.rs` simply wires the library to stdio:

```rust
// src/main.rs
fn main() {
    clawfish::uci::run();
}
```

**Introduce the split now, at M1.** The concrete reason: perft integration tests (in `tests/`) can only call public API. If the engine is a binary-only package, there is no library crate to import and integration tests cannot exist. Starting M1 with `src/lib.rs` present means perft tests can be written immediately as proper integration tests. The cost is negligible (create one file). The alternative — adding lib.rs later — requires moving code around under time pressure.

Pleco takes this further by using *two separate packages* (`pleco` library + `pleco_engine` binary), but for our scale a single package with lib + bin targets is the right starting point. Split into a workspace only if compile times demand it.

Sources: [Package Layout — The Cargo Book](https://doc.rust-lang.org/cargo/guide/project-layout.html)

---

### Module hierarchy

Recommended layout for the library at M1 scope. Flat modules work well until a module's file exceeds ~500 lines; use subdirectories (with `mod.rs` or the `src/movegen.rs` + `src/movegen/` pattern) only when warranted.

```
src/
  lib.rs              # re-exports public API; declares all modules
  types.rs            # Color, PieceKind, File, Rank — tiny primitives
  square.rs           # Square newtype (u8), conversions, display
  bitboard.rs         # Bitboard newtype (u64), set ops, iterators
  piece.rs            # Piece = (Color, PieceKind), compact representation
  mov.rs              # Move type (see naming note below)
  position.rs         # Position struct: boards[], side_to_move, castling, ep, halfmove, fullmove
  zobrist.rs          # Zobrist key tables and incremental update (hook point for NNUE accumulator)
  movegen.rs          # move generation entry point
  movegen/            # OR keep as a flat file until magic tables are needed
    magic.rs          # magic bitboard tables for sliders
    pawns.rs
    knights.rs
    kings.rs
    sliders.rs
  perft.rs            # perft() function, divide(), categorized counts
  fen.rs              # FEN parsing and formatting (can live in position.rs early on)
```

`movegen` is the natural candidate for a subdirectory because magic table initialization, pawn generation, and slider generation are each substantial and largely independent. Start flat (`movegen.rs`), then split into `movegen/` when the file grows unwieldy.

Keep `perft.rs` in the library (not only in tests) so it can be called from both integration tests and the UCI `go perft` handler. It can live behind a feature flag (`perft`) if binary size ever matters, but that is premature for now.

Sources: [Package Layout — The Cargo Book](https://doc.rust-lang.org/cargo/guide/project-layout.html); [Crate Layout Best Practices — DEV Community](https://dev.to/sgchris/crate-layout-best-practices-librs-modrs-and-srcbin-4abd)

---

### Test layout

| Test type | Location | What it tests | Private access |
|---|---|---|---|
| Unit | `src/foo.rs` → `#[cfg(test)] mod tests` | Individual function correctness | Yes |
| Integration | `tests/` | Public API contracts, multi-module flows | No |

**Unit tests** belong inline for: individual bitboard operations, square conversion, FEN parsing, Zobrist key accumulation, move encoding/decoding. These test implementation details that are not part of the public API.

**Integration tests** belong in `tests/` for: perft at every depth against the canonical positions, divide output format, round-trip FEN parsing. These test that the public surface behaves correctly as a whole.

The boundary is public vs. private. Perft correctness is fundamentally about the *observable output* of the move generator — it belongs in `tests/`. Bitboard `pop_lsb` returning the correct square is an implementation detail — it belongs inline.

Sources: [Test Organization — The Rust Book](https://doc.rust-lang.org/book/ch11-03-test-organization.html)

---

### Benchmark layout

Use [Criterion](https://github.com/bheisler/criterion.rs), the community standard for Rust micro-benchmarks. It provides statistical analysis, outlier detection, and baseline comparison — far more reliable than the built-in `#[bench]` (nightly-only and no statistics).

**Cargo.toml additions:**

```toml
[dev-dependencies]
criterion = { version = "0.5", features = ["html_reports"] }

[[bench]]
name = "movegen"
harness = false

[[bench]]
name = "perft"
harness = false
```

**Directory layout:**

```
benches/
  movegen.rs    # benchmark move generation: nodes/sec from canonical positions
  perft.rs      # benchmark perft() itself at fixed depth, with and without bulk counting
```

One concern per bench file keeps compile units separate and avoids one bench's setup code from inflating another's measurements.

**Baseline workflow:**

```bash
# Save a named baseline before a change:
cargo bench --bench movegen -- --save-baseline before

# Make your change, then compare:
cargo bench --bench movegen -- --baseline before
```

Criterion prints whether the change is statistically significant and the direction. This is the "benchmark every change" discipline from the workflow doc made mechanical.

**When to set up:** Now, at M1. Even empty bench files act as a reminder. The first real benchmarks will be `perft(startpos, 5)` and `perft(kiwipete, 4)`.

Sources: [Criterion Getting Started](https://bheisler.github.io/criterion.rs/book/getting_started.html); [Rust Performance Book](https://nnethercote.github.io/perf-book/build-configuration.html)

---

### `Cargo.toml` conventions

**Fields worth setting now:**

```toml
[package]
name = "clawfish"
version = "0.1.0"
edition = "2024"
description = "Standard chess engine"
license = "MIT"   # or your choice

[profile.release]
lto = "thin"
codegen-units = 1
panic = "abort"
opt-level = 3     # default, stated explicitly

[profile.bench]
# Inherits from release; no additional overrides needed initially
```

**Rationale for each `[profile.release]` setting:**

- **`lto = "thin"`**: Thin LTO enables cross-crate inlining at substantially lower link-time cost than fat LTO. Typical measured gains are 5–15% over the default (`lto = false`). Fat LTO (`lto = "fat"`) can squeeze out another few percent but doubles or triples link time. Start with thin; switch to fat only if SPRT results show it matters.
- **`codegen-units = 1`**: Disables parallel codegen, allowing LLVM to see the entire crate at once and apply optimizations that cross the default per-function boundaries. Compile time increases noticeably (minutes vs. seconds) but runtime can improve measurably for tight inner loops. Accept the cost now — the engine is small and will stay small for a while.
- **`panic = "abort"`**: Eliminates unwinding code from the binary. A chess engine never needs to catch a panic in production; abort is strictly cleaner. Marginally smaller binary, marginally faster in panic paths (which should never occur in correct code anyway).

**Settings to add later (not now):**

- `strip = true` — reduces binary size, irrelevant until distribution.
- A `[profile.profiling]` block (`opt-level = 3`, `debug = true`, `lto = false`) — useful when profiling with Instruments on Apple Silicon; add it when you need it.

**`[profile.bench]` note:** By default Cargo's `bench` profile inherits from `release`. Your `[profile.release]` settings (`lto`, `codegen-units`) therefore apply to `cargo bench` automatically. This is the desired behavior — you want benchmarks to reflect release performance.

Sources: [Profiles — The Cargo Book](https://doc.rust-lang.org/cargo/reference/profiles.html); [Rust Performance Book: Build Configuration](https://nnethercote.github.io/perf-book/build-configuration.html)

---

### Naming around the `move` keyword

`move` is a Rust keyword (used in closure capture). The community has no single universally-adopted convention; the forum thread at users.rust-lang.org identifies several options:

| Option | Example | Assessment |
|---|---|---|
| `r#move` | `r#move::Move` | Technically valid; deeply unergonomic — every use site needs the `r#` prefix |
| `move_` | `move_::Move` | Avoids the keyword; looks like a typo |
| `mov` | `mov::Move` | Short, unambiguous, used in several community engines; slightly unfamiliar |
| `chess_move` | `chess_move::ChessMove` | Verbose but self-documenting; works well as a *type* name |
| `moves` | `moves::Move` | Plural module, singular type — acceptable if the module holds multiple move-related items |

**Recommendation: use `mov` for the module name and `Move` for the type.**

```rust
// src/mov.rs
pub struct Move { ... }
pub enum MoveKind { Quiet, Capture, EnPassant, Castle, Promotion(PieceKind) }
```

The `mov` convention is clean, short, and unambiguous. `r#move` is a non-starter for daily use. `chess_move` as the *type name* is a reasonable alternative if you want more disambiguation (e.g., `pub use mov::ChessMove`), but inside a chess crate the `chess_` prefix is redundant.

The rust-analyzer style guide maps `type` → `ty` and `fn` → `func` as established abbreviations; `move` → `mov` follows the same pattern and is the closest thing to a convention in the chess programming Rust community.

Sources: [Rust Forum: naming move without using "move"](https://users.rust-lang.org/t/what-is-the-conventon-to-name-a-move-fn-without-using-move/89751); [Rust API Guidelines: Naming](https://rust-lang.github.io/api-guidelines/naming.html)
