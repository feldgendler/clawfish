# Authoritative reference documents

External specifications this engine implements. Only the documents the engine actually consults are kept in tree; bulky originals (full PDFs, monolithic Markdown) are pruned to keep LLM context reads cheap. Every snapshot records its upstream URL and a one-line `curl` command so the original can be re-fetched on demand.

## FIDE Laws of Chess (in force from 1 January 2023)

The rules of standard chess our engine implements. Approved by the FIDE General Assembly on 7 August 2022 (93rd FIDE Congress, Chennai). The English text is the authentic version.

The full document was downloaded as a PDF, converted to Markdown, then sliced into engine-relevant topic files. The PDF and the monolithic Markdown were intentionally deleted; the slices are the canonical in-tree copy.

- **Engine-facing copy:** [`rules/`](rules/) — topic-sliced Markdown, one file per concern (board, piece movement, pawn, castling, check, win/loss, draws, algebraic notation, glossary). See [`rules/README.md`](rules/README.md) for the file index and the explicit list of FIDE sections deliberately omitted as engine-irrelevant (clocks, conduct, arbitration, time controls, variant Chess960).
- **Upstream PDF:** <https://rcc.fide.com/wp-content/uploads/2022/12/20230101Laws-of-Chess.pdf>
- **Pointer page:** <http://rcc.fide.com/2023-laws-of-chess/>
- **Snapshot date:** 2026-04-27 (35-page PDF, 658 KB; slices are verbatim from that snapshot).
- **Re-fetch the original PDF:**
  ```sh
  curl -fsSL -o fide-laws-of-chess-2023.pdf \
    "https://rcc.fide.com/wp-content/uploads/2022/12/20230101Laws-of-Chess.pdf"
  ```
  Use this when the in-tree slices are insufficient — for instance, if you need to consult a movement diagram (placeholders are kept in the slices but the figures themselves are not vendored).

## Universal Chess Interface (UCI) protocol

The protocol our engine speaks to GUIs and tournament tooling (M2 onwards). Compact enough to keep verbatim in tree.

- **File:** [`uci-protocol-2006.txt`](uci-protocol-2006.txt) — extracted from `engine-interface.txt` inside the official ZIP. ASCII text, 544 lines, CRLF line endings preserved as-published.
- **Source page:** <https://www.shredderchess.com/chess-features/uci-universal-chess-interface.html>
- **Direct ZIP:** <https://www.shredderchess.com/download/div/uci.zip>
- **Snapshot date:** 2026-04-27 (the spec text itself dates from April 2006 — the last formal revision; subsequent extensions are de-facto-standard but not part of the official document).
- **Re-fetch:**
  ```sh
  curl -fsSL -o uci.zip "https://www.shredderchess.com/download/div/uci.zip" \
    && unzip -p uci.zip engine-interface.txt > uci-protocol-2006.txt && rm uci.zip
  ```

## PGN and FEN — Portable Game Notation Specification and Implementation Guide (Edwards, 1994)

The canonical specification for **PGN** (game notation, used by tournament databases, opening books, training data) and **FEN** (position notation, used in test fixtures and UCI `position fen ...` commands). Authored by Steven J. Edwards on behalf of contributors to `rec.games.chess`, revised 1994-03-12.

One document covers both notations:

- **PGN** (the game format) is the bulk of the document.
- **FEN** is defined in **section 16.1** with all six fields (piece placement, active color, castling availability, en passant target square, halfmove clock, fullmove number).
- **EPD** (Extended Position Description, FEN's superset for test suites) is in **section 16.2**.

The 1994 text remains the authoritative spec; there has been no formal revision. Subsequent practice has accreted minor de-facto extensions (e.g. annotation glyphs, additional STR tags) which are documented elsewhere as needed.

- **File:** [`pgn-spec-1994.txt`](pgn-spec-1994.txt) — plain ASCII, 2921 lines. The text-of-record from the Internet Archive's preservation copy.
- **Upstream archive:** <https://archive.org/details/pgn-standard-1994-03-12>
- **Direct download:** <https://archive.org/download/pgn-standard-1994-03-12/PGN_standard_1994-03-12.txt>
- **Snapshot date:** 2026-04-27 (document itself dates from 1994-03-12).
- **Re-fetch:**
  ```sh
  curl -fsSL -o pgn-spec-1994.txt \
    "https://archive.org/download/pgn-standard-1994-03-12/PGN_standard_1994-03-12.txt"
  ```

### Section map

Use these line ranges with the Read tool's `offset` and `limit` parameters to consult specific sections without loading the whole file. (`offset` is 0-based line index, so subtract 1 from the line number shown.)

| Section | Lines | Topic |
|---------|-------|-------|
| 0 | 10–20 | Preface |
| 1 | 21–38 | Introduction |
| 2 | 39–129 | Chess data representation |
| 3 | 131–214 | Formats: import and export |
| 4 | 216–287 | Lexicographical issues |
| 5 | 289–303 | Commentary |
| 6 | 305–315 | Escape mechanism |
| 7 | 317–373 | Tokens — string, integer, symbol, NAG, bracket token types |
| 8 | 375–891 | Parsing games — tag pairs and movetext |
| 8.1 | 389–435 | Tag pair section |
| 8.1.1 | 436–473 | Seven Tag Roster (STR) — the seven mandatory tags |
| 8.2 | 624–891 | Movetext section |
| 8.2.3 | 691–858 | Movetext SAN (Standard Algebraic Notation) |
| 8.2.3.1 | 707–719 | Square identification |
| 8.2.3.2 | 720–738 | Piece identification |
| 8.2.3.3 | 740–763 | Basic SAN move construction — captures, castling, promotion |
| 8.2.3.4 | 764–794 | Disambiguation |
| 8.2.3.5 | 795–818 | Check and checkmate indication characters |
| 8.2.3.6 | 819–829 | SAN move length |
| 8.2.3.7 | 830–846 | Import and export SAN |
| 8.2.3.8 | 847–858 | SAN move suffix annotations (!, ?, !!, etc.) |
| 8.2.4 | 860–867 | Movetext NAG (Numeric Annotation Glyph) |
| 8.2.5 | 868–878 | Movetext RAV (Recursive Annotation Variation) |
| 8.2.6 | 880–891 | Game Termination Markers |
| 9 | 893–1190 | Supplemental tag names — player, event, opening, time, FEN/SetUp, termination |
| 10 | 1192–1351 | Numeric Annotation Glyphs — full NAG value table (0–139) |
| 11 | 1353–1441 | File names and directories |
| 12 | 1443–1472 | PGN collating sequence |
| 13 | 1474–1799 | PGN software — catalog of 1994-era tools |
| 14 | 1801–1808 | PGN data archives |
| 15 | 1810–1971 | International Olympic Committee country codes |
| 16 | 1973–2625 | Additional chess data standards (FEN and EPD) |
| 16.1 | 1980–2117 | FEN — Forsyth-Edwards Notation |
| 16.1.1 | 1993–2001 | FEN history |
| 16.1.2 | 2002–2015 | Uses for a position notation |
| 16.1.3 | 2016–2092 | FEN data fields — all six fields |
| 16.1.3.1 | 2031–2042 | Piece placement data — rank/file encoding, empty-square digits |
| 16.1.3.2 | 2043–2048 | Active color — "w" or "b" |
| 16.1.3.3 | 2049–2063 | Castling availability — KQkq flags |
| 16.1.3.4 | 2064–2078 | En passant target square |
| 16.1.3.5 | 2079–2085 | Halfmove clock — fifty-move rule counter |
| 16.1.3.6 | 2086–2092 | Fullmove number |
| 16.1.4 | 2093–2117 | FEN examples — starting position and sample positions |
| 16.2 | 2118–2625 | EPD — Extended Position Description |
| 16.2.1 | 2134–2142 | EPD history |
| 16.2.2 | 2143–2152 | Uses for an extended position notation |
| 16.2.3 | 2153–2218 | EPD data fields — first four fields (same as FEN fields 1–4) |
| 16.2.4 | 2219–2280 | EPD operations — opcode/operand format |
| 16.2.5 | 2298–2625 | EPD opcode list — all standard opcodes (acn, acs, am, bm, ce, dm, bm, pv, sm, etc.) |
| 17 | 2627–2658 | Alternative chesspiece identifier letters — non-English piece codes |
| 18 | 2660–2693 | Formal syntax — BNF grammar for PGN |
| 19 | 2695–2699 | Canonical chess position hash coding (stub — under development) |
| 20 | 2700–2914 | Binary representation (PGC) — binary encoding of PGN |
| 21 | 2916–2921 | E-mail correspondence usage (stub — under development) |

## Polyglot opening-book format

The binary format for opening books used across the Polyglot/XBoard ecosystem, plus the 781-entry Zobrist random-number table that defines the position hash key on which all such books are indexed. Authored by Hartmut Kapsch (HGM) by inspecting the freely-available Polyglot source; the table-of-randoms and algorithm description are public-domain.

Vendored because the engine's Zobrist hashing implements this spec verbatim (per [ADR-0009](decisions/0009-polyglot-zobrist.md), pending) — the in-tree spec is the truth-of-record for what each hash key bit means, the canonical 781-constant table, and the test vectors used to validate the implementation. The future opening-book reader will follow the binary entry layout (`key`/`move`/`weight`/`learn`, big-endian, sorted by key) defined in the same document.

- **File:** [`polyglot-book-format.md`](polyglot-book-format.md) — HTML converted to Markdown, prose preserved verbatim, all 781 `U64(...)` constants intact and in original order.
- **Source URL:** <http://hgm.nubati.net/book_format.html>
- **Snapshot date:** 2026-04-27.
- **Upstream Last-Modified:** Tue, 17 Sep 2013 10:00:43 GMT — the format has been frozen since.
- **Re-fetch:**
  ```sh
  curl -fsSL -o /tmp/book_format.html http://hgm.nubati.net/book_format.html
  ```
  Note the URL is HTTP, not HTTPS — the upstream certificate is self-signed; this specific URL is pre-approved.
- **Local secondary copy:** the Homebrew `polyglot` package ships a functionally-similar (slightly extended for variant chess) HTML copy at `/opt/homebrew/Cellar/polyglot/<version>/share/doc/polyglot/book_format.html`.

## Refresh policy

These are vendored snapshots, not live links. If FIDE issues a new Laws of Chess revision, or Shredder posts a revised UCI spec, re-fetch and update the in-tree copy (and the FIDE slices in `rules/`). The PGN spec has been frozen since 1994; refresh is essentially never. Snapshot dates above are the source of truth for what version we actually built against.
