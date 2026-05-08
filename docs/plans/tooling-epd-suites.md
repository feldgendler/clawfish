# Plan — EPD diagnostic suites (WAC + STS regression harness)

Tooling unit. Builds a deterministic per-position best-move scorer against two canonical public EPD suites:

- **WAC** (Win at Chess): 300 tactical positions, single-`bm` solution.
- **STS** (Strategic Test Suite): 1500 themed positions across 15 strategy themes, weighted multi-move `c0` scoring (`Bf8=10, Bxd5=2, Be8=2, Bxc6=2`).

Complementary to SPRT — SPRT measures relative strength stochastically; WAC/STS measure absolute correctness deterministically on a fixed corpus, with per-theme attribution for STS.

Authorized by tooling-backlog item; no engine code changes.

## 1. Scope

**In:**
- `src/bin/epd-suite.rs` standalone binary; `[[bin]] name = "epd-suite"` in `Cargo.toml`.
- EPD parser handling `bm` and `c0` opcodes (both quoted-string and bare-token forms; `;` terminator).
- SAN renderer for legal moves (used to compare engine UCI output against EPD's SAN annotations).
- UCI subprocess driver (simpler than elo-iterate's: send `position fen <FEN>` + `go movetime <T>`, wait for `bestmove`).
- Parallel worker pool over independent positions (each worker owns its own engine subprocess).
- Aggregate output: total score + per-theme breakdown for STS + summary table.
- Vendored EPD data under `bench/data/wac.epd` and `bench/data/sts.epd`.
- Backfill report under `bench/epd-suites.md` recording per-baseline-tag scores + estimated STS-Elo.
- Documentation: README "Diagnostic suites" section + roadmap reference.

**Out:**
- Engine code changes. Harness is purely external.
- CI integration. Multi-hour wallclock at meaningful per-position time; runs on demand.
- Bratko-Kopec (24 positions) — too small for statistical signal; covered functionally by WAC + STS overlap.
- Knight-promotion handling in SAN (the engine's UCI output doesn't include `=N` differently from `=Q/R/B`; we handle all four uniformly).

## 2. Files

| Path | Status | What |
|---|---|---|
| `Cargo.toml` | edit | Add `[[bin]] name = "epd-suite", path = "src/bin/epd-suite.rs"`. |
| `src/bin/epd-suite.rs` | create | The harness binary. ~700-900 LOC including tests. |
| `bench/data/wac.epd` | create | Vendored WAC.300 corpus (300 positions). Public domain. |
| `bench/data/sts.epd` | create | Vendored STS 1.0 corpus (15 themes × 100 positions = 1500). Public domain (CC-BY by some sources; verify). |
| `bench/epd-suites.md` | create | Backfill table across baseline tags. |
| `README.md` | edit | Add "Diagnostic suites" section. |
| `docs/tooling-backlog.md` | edit | Strikethrough the EPD item; move to Done section. |
| `docs/architecture.md` | edit | One-line entry under "Verification standards" referencing the harness. |
| `docs/roadmap.md` | edit | If references to "EPD suites are open" exist, mark closed. |

## 3. EPD data — sourcing and vendoring

EPD format per Edwards, *EPD format* (1995); `bm` and `c0` per the same standard.

- **WAC** ("Win at Chess" by Fred Reinfeld, 300 positions; the EPD form maintained by Walter Browne and others is in widespread distribution). Format: `<FEN-4-fields> bm <san>; id "<tag>";`. We vendor under `bench/data/wac.epd`.
- **STS** ("Strategic Test Suite" 1.0 by Dann Corbit and Swaminathan Natarajan; 15 themes × 100 positions). Format: `<FEN-4-fields> bm <san>; c0 "<san>=<weight>, <san>=<weight>, …"; id "STS(<theme-num>): <theme-name>.<position-num>";`. Each `c0` annotation has the BM as the highest-scored entry (10 by convention; 8 in some themes); other moves carry partial credit (2-9).

Both are public-domain or community-licensed; redistribution under tools is standard. We download from canonical archives (see implementation step 1 below for the exact URLs the orchestrator will fetch). Vendoring means the harness needs no network access at run time and the result is stable across data-source flux.

**Sanity checks at parse time:**
- WAC: 300 positions parsed; every position has exactly one `bm`.
- STS: 1500 positions parsed; every position has both `bm` and `c0`; every `c0` weighted entry's primary value matches the `bm` (i.e. the `bm` is in `c0` at the highest weight). A test asserts these invariants on the vendored corpus directly.

## 4. Module layout (single-file binary)

```
src/bin/epd-suite.rs
├── mod cli            // arg parsing
├── mod epd            // EPD parser (Position from FEN, opcode parser)
├── mod san            // SAN renderer for legal moves
├── mod scorer         // WAC + STS scoring (per-position credit assignment)
├── mod driver         // UCI subprocess (spawn / send / wait-for-bestmove / shutdown)
├── mod runner         // worker pool + per-position dispatch
├── mod summary        // aggregate output (per-theme for STS, total for WAC)
├── fn main
└── #[cfg(test)] mod tests
```

Each module is tested directly via unit tests in the same file. No integration tests under `tests/` for v1 — tooling-style, follows the `mock_engine.rs` pattern.

## 5. Data types and signatures

### `mod epd`

```rust
pub(crate) struct EpdEntry {
    pub fen: String,                       // 4-field or 6-field FEN as written
    pub position: clawfish::Position,      // parsed from fen
    pub bm: Vec<String>,                   // SAN moves; can be multiple in `bm a b`
    pub c0: Option<Vec<(String, u32)>>,    // STS c0 weighted entries (san, weight)
    pub id: Option<String>,                // EPD `id` opcode
}

pub(crate) enum EpdParseError {
    BadFen(clawfish::FenError),
    MissingBm,
    BadC0(String),
    EmptyLine,
}

pub(crate) fn parse_epd_line(line: &str) -> Result<Option<EpdEntry>, EpdParseError>;
pub(crate) fn parse_epd_file(text: &str) -> Vec<Result<EpdEntry, EpdParseError>>;

/// Extract STS theme number ("STS(7): …" → 7) and name ("…: Knight Outposts.42" → "Knight Outposts").
pub(crate) fn parse_sts_id(id: &str) -> Option<(u32, String)>;
```

EPD line shape: `<piece-placement> <stm> <castling> <ep> [opcode <args>; …]`. Halfmove/fullmove are optional in EPD — we backfill `0 1` if absent before handing to `Position::from_fen`.

**EPD tokenizer is quoted-string-aware.** Per Edwards EPD format, operand values can be STRING (double-quote-delimited, with `\"` and `\\` escapes inside), INTEGER, or MOVE/SAN. The operand list for one opcode ends at the FIRST UNQUOTED `;`. A `;` inside `"..."` is part of the string operand, not the terminator. The tokenizer:

- Treats `"..."` as a single token spanning from `"` to the matching unescaped `"`.
- Inside a string token, accepts `\"` and `\\` as escapes; other backslash sequences pass through verbatim.
- After parsing a string operand, strips the surrounding quotes before storing in `EpdEntry::id` or in the `c0` value.

**EPD lines are unwrapped.** Folded `c0` strings across multiple physical lines are unsupported — each EPD record must occupy exactly one line. CV1/CV2 invariants catch any vendored entry that violates this.

`bm` opcode: a whitespace-separated list of SAN MOVE tokens (not strings) until the unquoted `;`. STS positions have a single best move; some test suites (rare) list alternatives.

`c0` opcode: a quoted-string comment. Standard interpretation per the STS conventions: comma-separated `<san>=<int>` pairs with optional spaces. The implementation tolerates extra whitespace and ignores any non-`san=int` entries.

### `mod san`

```rust
/// Render a legal move in canonical SAN form, given the position before the move.
/// No check/mate suffix (we strip from EPD annotations on compare; no need to compute).
pub(crate) fn san_of_legal_move(pos: &clawfish::Position, mv: clawfish::Move) -> String;

/// Canonicalize a SAN string for comparison: trim, strip trailing `+`/`#`/`?`/`!`,
/// substitute `0-0(-0)` → `O-O(-O)`, strip `e.p.` suffix.
/// Used on BOTH sides of any SAN comparison (engine-rendered and EPD-annotated).
pub(crate) fn canonicalize_san(s: &str) -> String;

/// Find the unique legal move whose canonical SAN equals `canonicalize_san(san_target)`.
/// Returns `None` if no legal move matches; the caller treats that as a parse failure.
pub(crate) fn legal_move_from_san(pos: &clawfish::Position, san_target: &str) -> Option<clawfish::Move>;
```

**Disambiguation rule:** the renderer uses `clawfish::generate_moves` (which per ADR-0007 returns LEGAL moves, not pseudo-legal). Pinned same-kind pieces are already absent from the candidate list, so disambiguation is correct by construction — `Nbd2` is not emitted when the b-knight is pinned.

**SAN rendering rules** (rendering side; we never have to *parse* SAN modulo the legal-move-comparison loop above):
- Castling: `O-O`, `O-O-O`.
- Pawn moves: `<dest>` (e.g. `e4`); pawn captures `<from-file>x<dest>` (e.g. `exd5`); en passant identical to capture (no `e.p.` suffix — STS doesn't use it).
- Pawn promotions: append `=Q`/`=R`/`=B`/`=N` (queen-promo capture: `exd8=Q`).
- Piece moves: `<piece-letter><disambig><capture><dest>`.
  - `<piece-letter>` ∈ `K Q R B N`.
  - `<capture>` is `x` if capture or EP; else empty.
  - `<disambig>` is empty if exactly one piece of this kind can legally reach `dest`; else `<from-file>` if file alone disambiguates; else `<from-rank>` if rank alone disambiguates; else `<from-square>` (full).
- Check/mate suffix: not generated. Comparator strips `+`/`#` from EPD annotations.

This is enough for WAC and STS — neither suite uses non-standard SAN.

### `mod scorer`

```rust
pub(crate) struct ScoringResult {
    pub credit: u32,    // For WAC: 1 if BM matched, else 0. For STS: the weighted credit.
    pub max_credit: u32,// For WAC: 1. For STS: max weight in c0 (typically 10).
}

/// WAC: 1 if the engine's UCI move's SAN ∈ entry.bm, else 0. max_credit = 1.
pub(crate) fn score_wac(entry: &EpdEntry, engine_uci: &str) -> ScoringResult;

/// STS: lookup engine's move's SAN in entry.c0; credit = weight if found, else 0. max_credit = max weight.
pub(crate) fn score_sts(entry: &EpdEntry, engine_uci: &str) -> ScoringResult;
```

Both helpers convert the engine's UCI output back to SAN via `legal_move_from_san`'s inverse: parse UCI, render SAN, compare strings. Engine output `0000` (null move; should not happen post-M3.E but defended) scores 0.

### `mod driver`

Simplified UCI driver — single engine, sequential `position`+`go`+`bestmove`:

```rust
pub(crate) struct EngineDriver {
    child: Child, stdin: Option<ChildStdin>, rx: Receiver<String>,
    reader: Option<JoinHandle<()>>,
}

impl EngineDriver {
    pub fn spawn(path: &str, hash_mib: u32) -> io::Result<Self>;
    /// Sends `ucinewgame` + `isready`/`readyok`; resets per-position TT state.
    pub fn new_game(&mut self) -> io::Result<()>;
    /// Sends `position fen <fen>` then `go movetime <ms>`; blocks until `bestmove`.
    /// Returns the UCI move string. Hard ceiling at 10× movetime to defend against
    /// pathological hangs.
    pub fn search(&mut self, fen: &str, movetime_ms: u64) -> io::Result<String>;
    pub fn quit(mut self);
}
```

Reader thread drains stdout into a `mpsc::sync_channel(1024)` of full lines. `search` consumes lines until `^bestmove ` prefix; sets a wall-clock deadline `now + 10*movetime_ms` and returns an error on timeout. Standard handshake (`uci` + wait for `uciok`, then `setoption Hash N`, `isready`/`readyok`) runs in `spawn`. On `quit`, write `quit\n`, drop stdin, join reader.

### `mod runner`

```rust
pub(crate) struct RunConfig {
    pub engine_path: String,
    pub epd_path: String,
    pub movetime_ms: u64,
    pub hash_mib: u32,
    pub concurrency: usize,
    pub suite: Suite,                    // Wac | Sts (informs scoring + summary shape)
    pub limit: Option<usize>,            // For smoke runs — first N positions only
}

pub(crate) enum Suite { Wac, Sts }

pub(crate) struct PositionResult {
    pub index: usize,
    pub id: Option<String>,
    pub theme: Option<(u32, String)>,    // STS only
    pub credit: u32,
    pub max_credit: u32,
    pub engine_uci: String,
    pub engine_san: String,
    pub elapsed_ms: u128,
}

pub(crate) fn run(cfg: &RunConfig) -> io::Result<Vec<PositionResult>>;
```

Worker pool: spawn `cfg.concurrency` workers, each owning its own `EngineDriver`. Job queue is a shared `Arc<Mutex<VecDeque<usize>>>` of position indices; results merge via `mpsc::Sender<PositionResult>` into the main thread. Each worker calls `engine.new_game()` before every position (fresh TT — guarantees per-position determinism without cross-pollution).

Progress: emit one `info: position N/total credit=X/Y san=… elapsed=…ms` line per result on stderr, ascending by `index`. (Workers can finish out of order; we buffer and flush in order via a `BTreeMap<usize, PositionResult>` cursor.)

### `mod summary`

```rust
pub(crate) fn summarize_wac(results: &[PositionResult]) -> WacSummary;
pub(crate) fn summarize_sts(results: &[PositionResult]) -> StsSummary;

pub(crate) struct WacSummary { pub total: usize, pub solved: usize }

pub(crate) struct StsSummary {
    pub total_credit: u32,
    pub max_credit: u32,
    pub per_theme: Vec<ThemeSummary>,        // input-order (theme-num ascending)
    pub elo_estimate: f64,                   // 44.523 * (points/100) - 242.85
}
pub(crate) struct ThemeSummary { pub theme_num: u32, pub name: String, pub credit: u32, pub max: u32, pub positions: usize }
```

`elo_estimate` uses Swaminathan's published regression. The harness emits the figure with a "[CCRL band 2000-2800; extrapolation outside degrades]" caveat in the summary header.

### `mod cli`

CLI flags (Unix-style; same `--key value` shape as `elo-iterate.rs`):

- `--engine <path>` (required)
- `--suite wac|sts` (required)
- `--epd <path>` (required; usually `bench/data/wac.epd` or `bench/data/sts.epd`)
- `--movetime <ms>` (default 1000)
- `--hash <MiB>` (default 16; matches engine default for cross-tag determinism)
- `--concurrency <N>` (default 1; recommend `physical_cores - 1` for parallelism)
- `--limit <N>` (optional smoke limit; runs first N positions)
- `--output <path>` (optional; writes summary to file in addition to stdout)

Validation: positive numeric ranges; `--suite` value parsed; `--epd` exists; `--engine` exists.

### `fn main`

1. Parse args.
2. Read EPD file; collect `EpdEntry`s; report parse errors with line numbers; abort if any.
3. For STS, validate every entry has `c0` with `bm` at top weight (informational warning — some sources have minor errata; don't abort).
4. Spawn worker pool; run; collect results.
5. Compute summary; print to stdout (and `--output` if set).
6. Exit code 0 on success, 1 on any I/O / parse / engine-protocol error.

## 6. Test plan

All tests in `#[cfg(test)] mod tests` inside `epd-suite.rs`. The binary's tests compile under `cargo test --bin epd-suite` (which `cargo test` runs by default).

### `mod epd` tests
- T1: `parse_epd_line` round-trip on a synthetic 4-field FEN with `bm Nf3; id "test1";`.
- T2: 6-field FEN parses identically (halfmove/fullmove fields preserved).
- T3: `bm` with multiple SAN moves (`bm Nf3 Bc4`) yields a 2-element vec.
- T4: STS-style `c0 "Bf8=10, Bxd5=2, Be8=2, Bxc6=2"` parses to 4 weighted pairs in input order.
- T5: `c0` with extra spaces / trailing comma — parses what's well-formed, ignores garbage.
- T6: empty line and pure-comment line return `Ok(None)`.
- T7: missing `bm` returns `EpdParseError::MissingBm`.
- T8: malformed FEN returns `EpdParseError::BadFen`.
- T9: `parse_sts_id` on `"STS(7): Knight Outposts.42"` → `Some((7, "Knight Outposts"))`; on garbage → `None`.

### `mod san` tests
- S1: pawn push `e2e4` from startpos → `"e4"`.
- S2: pawn capture `e4d5` (after `e4 d5`) → `"exd5"`.
- S3: castling kingside / queenside → `"O-O"` / `"O-O-O"`.
- S4: knight move with file disambiguation (two knights to same square) → `"Nbd2"` style.
- S5: rook move with rank disambiguation (two rooks on same file) → `"R1d2"` style.
- S6: queen move with full-square disambiguation (three queens reaching same square) → `"Qa1d4"` style.
- S7: promotion `a7a8=Q`-equivalent → `"a8=Q"`; promotion-capture → `"axb8=Q"`.
- S8: en passant from `5r2/8/8/3pP3/.../...` style position → `"exd6"` (no `e.p.` suffix).
- S9: `legal_move_from_san` round-trip: render → re-parse via SAN-targeted comparator → same `Move`.
- S10: `strip_san_decoration` strips trailing `+`, `#`, multiples of `?!`.

### `mod scorer` tests
- SC1: `score_wac` matched annotation → `credit=1, max=1`.
- SC2: `score_wac` mismatched annotation → `credit=0, max=1`.
- SC3: `score_sts` engine SAN found in `c0` at weight 10 → `credit=10, max=10`.
- SC4: `score_sts` engine SAN found at weight 2 → `credit=2, max=10`.
- SC5: `score_sts` engine SAN not in `c0` → `credit=0, max=10`.
- SC6: engine UCI `0000` (null) scores 0 in both suites.
- SC7: SAN-decoration tolerance: `bm "Nf3+;"` matches engine SAN `"Nf3"`.

### `mod driver` tests
- D1: `mock_engine` reused as a fixture; `EngineDriver::spawn` + handshake completes.
- D2: `search` issues `position fen … go movetime …`, returns `"0000"` from mock.
- D3: hard ceiling triggers `Err` when mock never emits `bestmove` — use a sleep-fixture variant or the `cat` pattern from elo-iterate's tests.

The harness reuses `mock_engine` as the test-fixture engine; this avoids spinning up the real clawfish in unit tests.

### `mod scorer` corpus invariants (vendored data)
- CV1: `bench/data/wac.epd` parses; 300 entries; every entry has `bm.len() >= 1` and `c0.is_none()`.
- CV2: `bench/data/sts.epd` parses; 1500 entries; every entry has `bm.len() == 1` and `c0.is_some()`; `parse_sts_id(id)` succeeds; `canonicalize_san(bm[0])` is **present** in `c0` (presence-only invariant).
- CV2b (informational): of the 1500 STS entries, count how many have `canonicalize_san(bm[0])` at the maximum c0 weight in that entry; assert `count >= 1495` (allowing up to 5 errata across the 15-theme corpus).

These tests are the parser's contract anchored to the actual data files — they catch both parser regressions and corpus corruption.

### `mod summary` tests
- SU1: `summarize_wac` of 3 results (2 solved) → `total=3, solved=2`.
- SU2: `summarize_sts` of synthesised theme distribution computes per-theme totals + Elo regression.
- SU3: `elo_estimate(2000 raw)` ≈ `44.523 * 20 - 242.85 = 647.61`.

### `mod runner` smoke
- R1: `run` against the mock engine on a 2-line synthetic EPD → 2 `PositionResult`s with `engine_uci == "0000"` and credit 0; ordering preserved.

### Cargo-level
- All under `cargo test --bin epd-suite`.
- Mock-engine fixture: re-use `target/debug/mock-engine` (built by the test target via `assert_cmd`-style helper or env-var path; same pattern as `controller::production_worker_tests` in `elo-iterate.rs`).

## 7. Order of operations

1. (Sequential) Vendor EPD data files. Parse-test them inline (CV1 + CV2) to confirm the format before any code.
2. (Sequential) Plan-review loop. Background.
3. (Parallel) Tests: `mod epd` + `mod san` + `mod scorer` + `mod summary` can be written by parallel coding agents — no shared state, distinct module boundaries.
4. (Sequential) Test-suite review loop. Background.
5. (Parallel) Implementation: `mod epd` + `mod san` + `mod scorer` + `mod summary` parallelizable; `mod driver` + `mod runner` after `mod epd` is in (they consume `EpdEntry`); `fn main` last.
6. (Sequential) Pre-review mechanical checks; final-review loop.
7. (Sequential) Commit.
8. (Sequential) Backfill across baseline tags. WAC at `--movetime 1000 --concurrency 4` for all 11 tags, STS at `--movetime 1000 --concurrency 4` for the recent 6 tags + a representative spread on older ones.
9. (Sequential) README + tooling-backlog update; commit + push.

## 8. Parallelization map

- **Plan / test-suite / final review** loops run in background while the orchestrator does other work.
- **Test-writing** (step 3): up to 4 parallel chess-coder agents — `mod epd`, `mod san`, `mod scorer`, `mod summary` are independent file regions.
- **Implementation** (step 5): up to 4 parallel agents on `mod epd`/`mod san`/`mod scorer`/`mod summary`. `mod driver`/`mod runner` are smaller and can be one agent each (or combined).
- **Backfill** runs sequentially across tags (one engine binary per tag, one harness invocation per (tag, suite) pair). Within a single invocation the harness itself parallelizes positions across `--concurrency` workers.

## 9. Backfill methodology

For each baseline tag in `git tag -l 'baseline/*'` except `random-mover` (no search; scores zero):

1. Build the tag's binary in a `git worktree`-isolated checkout under `target/epd-baselines/<tag-slug>/` (mirrors `scripts/sprt.sh`'s pattern).
2. Run `epd-suite --engine <bin> --suite wac --epd bench/data/wac.epd --movetime 1000 --concurrency 4 --hash 16`; record total/solved.
3. Run the same with `--suite sts`; record total credit, max credit, per-theme breakdown, Elo estimate.
4. Append a row to `bench/epd-suites.md`.

**Caveat per the M3.E inflection.** `baseline/material-greedy` (M3.A) and `baseline/alpha-beta-no-tt` (M3.C) predate iterative deepening + time management. Their search shape is depth-fixed; `go movetime` is accepted syntactically but doesn't budget time. **Pre-flight required:** before backfilling those tags at `--movetime 1000`, run a 5-position smoke at the same TC. If the engine emits `bestmove` synchronously at native depth (i.e., faster than the harness's 10× movetime ceiling), the row is meaningful as "what this engine outputs at its native search shape" — annotated as "fixed-depth point" in the table. If the engine hangs or trips the ceiling, exclude the tag from the backfill rather than recording a 0 that's a measurement artifact, not a tactical miss.

**M5.E qsearch-shape annotation.** M3.D through M5.D-v2 use the *uncorrected* qsearch (CLAUDE.md "M3.D" through "M5.D"); M5.E onward uses the corrected qsearch (single-reply extension, true-stalemate detection, stalemate-conditional under-promo, MAX_PLY ceiling guard). The M5.E bench result was unchanged from M5.D-v2 because the corner cases don't fire on the 16-position bench corpus, but WAC's 300 tactical positions and STS's 1500 strategic positions are broader corpora — corner-case fire rates may differ. Annotate the M5.E row in `bench/epd-suites.md` with "qsearch corrections active" so a step change between M5.D-v2 and M5.E is not misread as an SPRT-relevant regression.

**STS Elo regression citation.** The `Elo ≈ 44.523 · (points/100) − 242.85` formula is from Swaminathan Natarajan's STS calibration post (linked from <https://sites.google.com/site/strategictestsuite/about> and from the Chess Programming Wiki's Strategic Test Suite article). Vendor the citation as a comment on the `STS_ELO_SLOPE`/`STS_ELO_INTERCEPT` constants in `src/bin/epd-suite.rs` so a future session can confirm the regression's provenance.

`baseline/random-mover` is excluded — score would be ~0 by construction (random move from legal moves; tactical hits are < 1/N).

For tags that fail to build under the current toolchain (Rust edition mismatch, etc.), record "build failed" in the table. Old enough tags may use `chess` as the package name; the worktree-build step reads from the tag's `Cargo.toml`.

## 10. Risks and mitigations

- **EPD source variability.** Different distributors of WAC/STS have minor edits (typos in `id` strings, occasional `bm` tweaks). Mitigation: pin the source URLs in this plan; the parser is tolerant of common deviations (extra whitespace, decoration suffix); CV1 + CV2 catch any regression via the corpus invariants.
- **SAN ambiguity.** Some EPD files include check markers in `bm`/`c0`; `strip_san_decoration` handles. Some use `0-0` instead of `O-O`; we tolerate both via comparator normalization.
- **Subprocess hangs.** `EngineDriver::search` enforces a `10× movetime` ceiling; the worker discards the result and continues to the next position with a fresh subprocess if a hang is observed (rare; the per-position cost of ceiling-firing is bounded).
- **Parse failures masking large fractions of the corpus.** CV1 + CV2 invariants run on every test pass and block merge if vendored data parses to fewer than 300 / 1500 entries.
- **Hash carryover across positions.** Mitigated by `ucinewgame` per position. The TT is cleared between positions (M4.A's `Engine::reset_for_new_game()`).
- **STS Elo regression extrapolation.** Caveat printed in summary header; not load-bearing for any decision yet.

## 11. Acceptance criteria

- `cargo build --release --bin epd-suite` succeeds.
- `cargo test --bin epd-suite` all green.
- `cargo run --release --bin epd-suite -- --engine target/release/clawfish --suite wac --epd bench/data/wac.epd --movetime 100 --limit 10` produces a coherent summary on the first 10 positions.
- Backfill table in `bench/epd-suites.md` has a row for every viable baseline tag.
- README has a "Diagnostic suites" section with usage and a snapshot of the latest tag's results.
- `docs/tooling-backlog.md` strikethrough on the EPD item.
- Final-review loop converges.

## 12. Estimated size

- `src/bin/epd-suite.rs` ~700-900 LOC including tests.
- `bench/data/wac.epd` ~22 KB (300 lines).
- `bench/data/sts.epd` ~250 KB (1500 lines).
- `bench/epd-suites.md` ~100 lines.
- README delta ~30 lines.
- Total: ~1100-1300 LOC of code+test diff, plus vendored data.
