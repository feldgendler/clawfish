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

## Per-component research notes

(Empty. Populated as each feature passes through the research phase of the workflow loop.)

### Move generation (M1)
*Not yet researched. First M1 task: prior-art pass on magic bitboards, make/unmake patterns, perft methodology.*

### Search
*Not yet researched.*

### Evaluation
*Not yet researched.*

### Time management
*Not yet researched.*

### NNUE
*Not yet researched.*
