# Prior Art & References

Reference landscape for the project. Per-feature research notes accumulate here as we move through milestones.

## Research methodology constraint

**No engine source code as a research input.** See `docs/decisions/0003-no-third-party-source-code-reading.md` and `docs/workflow.md`. All prior art comes from prose: wikis, papers, blog posts, forum discussions, articles with illustrative code fragments. The source repos of Stockfish, Fairy-Stockfish, Leela, and any open-source Rust engine are off-limits.

## Vendored authoritative specs

Committed in `docs/reference/` (see its [README](reference/README.md) for upstream URLs, snapshot dates, and re-fetch commands):

- **`reference/rules/`** — engine-relevant slices of the FIDE Laws of Chess (in force from 1 January 2023), one Markdown file per topic. The original PDF and monolithic Markdown were intentionally not vendored to save context; the upstream URL and re-download command live in the reference README.
- **`reference/uci-protocol-2006.txt`** — the official UCI protocol specification (Shredder, April 2006). What our UCI layer must conform to.

## Foundational references

### Chess Programming Wiki
URL: <https://www.chessprogramming.org/>
The field's encyclopedia. First stop for any technique. Coverage is uneven (some topics deep, some shallow) and some advice is dated, but it is the canonical starting point. Articles often link to source code — we read the article, not the source.

### TalkChess forum
URL: <https://talkchess.com/>
Where engine devs argue. Often the only source for *why* a technique works or fails in practice — failure modes that don't make it into wikis. Prose discussion is in bounds; pasted source-code excerpts from specific engines are not, but conceptual snippets shared in arguments generally are.

### Academic papers
Search via Google Scholar / arXiv for specific techniques as needed. Papers often contain pseudocode and small illustrative code fragments — these are in bounds because they're for exposition, not implementation reference.

## Tournaments / rating lists

- **CCRL** (Computer Chess Rating Lists) — <https://www.computerchess.org.uk/ccrl/> — Blitz and 40/15 lists for engines we could realistically join.
- **CEGT** — another mainstream rating list.
- **TCEC** — top-tier; aspirational only.
- Open events organized via TalkChess threads — entry-friendly.

## Test data sources (in bounds — these are *data*, not code)

- **Perft fixtures** — generated on demand from Stockfish 18 (`go perft N`); Stockfish is our sole oracle. See `decisions/0006-stockfish-as-perft-oracle.md`. We do not consult CPW or other perft tables; everything comes from running the installed Stockfish.
- **STS** (Strategic Test Suite) — positional understanding benchmark.
- **Bratko-Kopec** — older positional suite.
- **WAC** (Win at Chess) — tactics.
- **Eigenmann Rapid Engine Test (ERET)** — rapid-time positional/tactical.
- **Syzygy tablebases** — endgame oracle, 7-piece available.
- **Polyglot opening books** — public binary format; we'd write our own parser.

## Tooling

- **Stockfish** (Homebrew) — runtime oracle for perft data and (later) sparring partner for SPRT calibration. We never read its source, only consume its UCI output. See `decisions/0006-stockfish-as-perft-oracle.md`.
- **cutechess-cli** / **fastchess** — for running engine matches and SPRT.
- **Cute Chess** GUI — for interactive testing and watching games.
- **Arena**, **Banksia** — alternative GUIs.
- **samply**, Instruments — profiling on Apple Silicon.
- **criterion** — Rust microbenchmarking.
- **cargo-llvm-cov** — line/branch coverage via LLVM source-based instrumentation (works natively on Apple Silicon). Used by the final review loop to surface implementation paths that contract-driven TDD didn't naturally exercise. See `workflow.md` "Final review loop" → Coverage dimension.

## Per-component research notes

Detailed research reports live in `docs/research/`. Summaries here; consult the full reports for citations and depth.

### Move generation (M1) — researched 2026-04-27

Three parallel research passes covered M1's design space:

- **[`research/m1-engine-architecture.md`](research/m1-engine-architecture.md)** (3.1k words) — bitboard layout, square indexing, move encoding, generation strategy, make/unmake, Zobrist hashing, edge case taxonomy, performance baselines on Apple Silicon. Headline calls: LERF indexing, 6+2 bitboard scheme + mailbox, 16-bit moves with `[Move; 256]` lists, **legal-direct generation with check-evasion specialization**, ~16-byte Undo struct, **Polyglot Zobrist key set** with the **EP-only-when-pseudo-legal hashing rule**, target ≥100 Mnps perft on M4 (≥200 excellent).
- **[`research/m1-magic-bitboards.md`](research/m1-magic-bitboards.md)** (4.4k words) — deep dive on the magic-bitboard technique. Headline calls: **fancy magic with variable shift** (~840 KiB), **magic constants hardcoded in a generated source file** with attack tables built at runtime startup, **separate `magicgen` binary** for the search/validation/codegen step, slow ray-walker kept as **permanent `slow_attacks` differential-test oracle**, skip PEXT entirely (ARM has no equivalent).
- **[`research/m1-perft-and-rust.md`](research/m1-perft-and-rust.md)** (2.5k words) — perft methodology and Rust project layout. Headline calls: bulk-counting at depth 1 for 20–30% perft speedup, `go perft N` UCI extension following Stockfish convention, `perftree` for divide automation, Chris Whittington's `perft.epd` (175 positions) as bulk regression position corpus, `tests/perft.rs` integration with `#[ignore]` for slow depths, **`src/lib.rs` introduced now in M1**, flat module hierarchy with `mov` for the `move`-keyword clash, `lto = "thin"` + `codegen-units = 1` + `panic = "abort"` from the start.

### UCI I/O & threading (M2) — researched 2026-04-27

- **[`research/m2-uci-threading.md`](research/m2-uci-threading.md)** (5.3k words) — how a UCI engine handles stdin concurrent with a search.
  - Recommended architecture: reader thread → mpsc → main-as-orchestrator + per-`go` search worker.
  - Cancellation: `Arc<AtomicBool>` polled every 4096 nodes (`Ordering::Relaxed`).
  - Deadline: lives in `SearchContext`, polled on the same cadence (no timer thread).
  - Stdout via shared `Mutex<Stdout>`; `bestmove` printed by the worker, never the orchestrator.
  - EOF on stdin = synthetic `Quit`; `std::process::exit(0)` on quit (reader thread is uncancellable per `std::io::Stdin`).
  - Latency budget: `isready` <1 ms, `stop` → `bestmove` <10 ms, `quit` → exit <1 s.
  - `Search::go(&Position, &SearchContext, &dyn Fn(&str))` trait sketched so M3+ alpha-beta plugs in without signature change.
  - Binds **ADR-0011** on M2.C.

### Tournament harness (M2) — researched 2026-04-27

- **[`research/m2-tournament-harness.md`](research/m2-tournament-harness.md)** (3.6k words) — match-runner choice and integration patterns on Apple Silicon macOS.
  - Recommended runner: **fastchess** 1.8.0-alpha (pre-built `mac-arm64` binary on each release).
  - Why not Cute Chess: 1.4.0 ships zero macOS assets, requires Qt6 source build (~1 GB).
  - Confidence signal: Stockfish/Fishtest themselves migrated cutechess-cli → fastchess in 2024.
  - No `engines.json` (fastchess has no registry); use `scripts/match.sh` wrapper.
  - Output layout: raw PGN/log → `target/matches/` (gitignored); milestone summaries → `bench/m2.md` per ADR-0010.
  - M2 smoke contract: 4 self-play + 4 vs Stockfish at `tc=10+0.1`, all legally terminated, no protocol errors, fastchess UCI-compliance checker silent.
  - Integration test pattern (~30 lines `tests/uci_smoke.rs`): `env!("CARGO_BIN_EXE_clawfish")` + reader thread + `recv_timeout` to convert hangs into failures.
  - Cute Chess GUI on macOS: skip; suggest `chessx` cask or Lichess paste-import for PGN replay.
  - Binds **ADR-0012** on M2.E.

### Search (M3) — researched 2026-04-28

- **[`research/m3-search-basics.md`](research/m3-search-basics.md)** (Sonnet pass) and **[`research/m3-search-basics.opus.md`](research/m3-search-basics.opus.md)** (Opus calibration parallel pass).
  - Headline calls (both reports converge): **fail-soft negamax**; **triangular PV table** (~4 KB at MAX_PLY=64); **MVV-LVA captures-first** + quiet moves in movegen order (no killer/history yet); **mate-distance pruning** safe and cheap; **PVS / aspiration windows defer to M4** (need TT-move ordering to pay off); **qsearch scope** = stand-pat + captures + queen promos + in-check all-evasions (no checks, no underpromotions, no delta pruning at M3); **stand-pat forbidden when in check**; **ID aborts between iterations only — never mid-iteration** (mid-iteration partials discarded); **cancellation cadence** 2048–4096 nodes via existing `SearchContext::should_abort`; **repetition + 50-move via game-history `Vec<u64>` plumbed through `SearchContext`** (push/pop around make/unmake; first-occurrence-in-search counts as draw); **insufficient-material draw detection lives in eval**.
  - Calibration outcome: substantive convergence; minor wording-only differences (i32 vs i16 score type; 30000 vs 32000 MATE constant; mate-distance pruning M3 vs M4). chess-researcher tier confirmed for Sonnet.

### Evaluation (M3) — researched 2026-04-28

- **[`research/m3-eval-material-pst.md`](research/m3-eval-material-pst.md)**.
  - Headline calls: **Vendor PeSTO middlegame values verbatim** (Texel-tuned; P=82, N=337, B=365, R=477, Q=1025, K=0). **Single-phase MG-only is fine for M3** — tapering = M6; known weakness is bare-king endgame king behavior, acceptable vs SPRT-vs-RandomMover target. **Eval perspective: side-to-move-relative**. **PST symmetry via rank-flip at lookup** (`square ^ 56` for white if a1=0). **Insufficient-material in eval**: KvK / KvN / KvB → 0 (skip same-color-bishops at M3). **Incremental PST delta in `Undo`** from M3.A — aligns with NNUE hook (ADR-0004).
  - All PeSTO PST data tables vendored verbatim in the report so M3.A can copy them directly.
  - Open question for M3.A plan: confirm engine's a1=0 vs a8=0 square indexing convention before writing the lookup formula.

### Time management (M3) — researched 2026-04-28

- **[`research/m3-time-management.md`](research/m3-time-management.md)** (extends `m2-uci-threading.md`'s deadline-polling primitive with the algorithm layer).
  - Headline calls: **Soft cap = `remaining/20 + increment/2`** (CPW baseline). **Hard cap = `min(3 × soft_cap, remaining - latency_margin)`**. **Latency margin = 50 ms** default, configurable via new `MoveOverhead` UCI option (`type spin default 50 min 0 max 5000`). **Sudden death (no `movestogo`):** divisor = 20 (conservative). **`movetime` overrides everything** (`soft = hard = movetime - latency`). **Non-time limits** (`depth`/`nodes`/`mate`/`infinite`) bypass time mgmt. **PV-stability / search-instability extensions defer to M4**. **Pondering: M5+**. Mocked-clock unit tests for `compute_caps` per `docs/workflow.md` TDD scope.

### NNUE
*Not yet researched.*
