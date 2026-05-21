# Vendored opening book — `bench/data/openings.epd`

A 40,457-position opening EPD vendored as the M6.G/M6.I opening-seed
source. Consumed by an `OpeningMode::Book` self-play campaign (every
record tagged `Source::SelfPlayOnBook`); the complementary
`OpeningMode::Random` campaign starts every game from `startpos +
opening_random_plies` random plies and tags records
`Source::SelfPlayOffBook`. The operator runs one campaign per regime
and grows the two corpora independently. The book / off-book
proportion is a **training-time per-source reweighting** axis at M6.I
(ADR-0035 §10), no longer a corpus-generation knob.

## Provenance

| Field | Value |
|---|---|
| Source repo | `official-stockfish/books` (the canonical Stockfish testing-book mirror) |
| Source file | `2moves_v1.epd.zip` |
| License | **CC0 1.0 Universal** (public domain dedication — `LICENSE` in upstream repo) |
| Acquired | 2026-05-20 from `https://raw.githubusercontent.com/official-stockfish/books/master/2moves_v1.epd.zip` |
| Upstream README claim | 40457 positions, min/max depth = 4 plies (2 plies per side after startpos) |
| Vendored unzipped size | 2,530,049 bytes (40457 lines) |

## SHA-256

- `bench/data/openings.epd` (vendored, unzipped) — `dc91f225bc93e7ec091095bf8264595da33d36b9d3ac97ddd2dd54bc3a094fa4`
- Source zip (downloaded copy, for upstream-drift detection) — `d816b8b97f2050ca728502f1dd84f556ee09c2e5b36527783de5930eaff39dd6`

The OpenBench `AndyGrant/openbench-books` mirror publishes a different
zip-level SHA (`7bec98239836f219dc41944a768c0506abed950aaec48da69a0782643e90f237`)
— OpenBench's metadata may be stale relative to the official-stockfish
mirror, or the two mirrors diverged. Our pinned reference is the SHA
of the *unzipped* `bench/data/openings.epd` above; the manifest's
`opening_book_sha256` is computed from that.

## Format

One position per line. Each line is a 6-field FEN (board / side / castling
/ en-passant / halfmove / fullmove). No comments, no `c0`/`c9` operands.
The vendored set is white-to-move at depth 4 in every entry (each side has
played exactly one move from startpos).

```
rn1qkbnr/ppp1pppp/8/3p1b2/2P5/1P6/P2PPPPP/RNBQKBNR w KQkq - 0 3
rnbqkbnr/1p1ppppp/p1p5/8/8/4PQ2/PPPP1PPP/RNB1KBNR w KQkq - 0 3
…
```

## Why vendored

CLAUDE.md §"Domain code restrictions" explicitly endorses *public chess
data* (perft suites, opening books, Syzygy tablebases, PGN databases,
eval test suites). CC0 makes the licensing trivial. Vendoring beats
re-downloading because:

- **Reproducibility.** `bench/corpus/re-run.sh` verifies the
  `opening_book_sha256` field matches the committed value of
  `bench/data/openings.epd` — a re-run on any machine reproduces the
  same opening distribution. Network-only retrieval would be subject
  to upstream drift.
- **R5 (no hard network during crunch).** Self-play campaigns are
  offline — they read the vendored EPD locally.
- **Size.** 2.5 MB uncompressed; ~500 KB-1 MB in the git pack format
  (FEN strings compress very well due to repeated substrings).

## Re-derivation

```sh
curl -sL https://raw.githubusercontent.com/official-stockfish/books/master/2moves_v1.epd.zip -o /tmp/2moves.zip
unzip -p /tmp/2moves.zip > bench/data/openings.epd
shasum -a 256 bench/data/openings.epd   # expect dc91f225…3a094fa4
```

If the upstream content changes (upstream replaces the file under the
same name), the SHA above will not match the re-derived file; that's
expected and means the committed copy is pinned to the 2026-05-20
content.
