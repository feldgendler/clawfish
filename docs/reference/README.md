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

## Refresh policy

These are vendored snapshots, not live links. If FIDE issues a new Laws of Chess revision, or Shredder posts a revised UCI spec, re-fetch and update the in-tree copy (and the FIDE slices in `rules/`). Snapshot dates above are the source of truth for what version we actually built against.
