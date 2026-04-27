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

### Search
*Not yet researched.*

### Evaluation
*Not yet researched.*

### Time management
*Not yet researched.*

### NNUE
*Not yet researched.*
