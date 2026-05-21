# Vendored chess data — provenance

This directory holds public chess test data the engine consults at
benchmark / diagnostic time. Each file's upstream source, snapshot date,
and redistribution status is recorded below. SHA-256 pins are the
truth-of-record for what version we built against.

`bench/data/openings.epd` is documented separately in
[`../openings.md`](../openings.md) (CC0, Stockfish testing-book mirror).

## `wac.epd` — Win at Chess

A 300-position tactical test suite, the chess-engine community's de-facto
standard tactical benchmark. Each position is annotated with a `bm` (best
move) operand; engines are scored on per-position move-correctness at a
fixed think-time.

| Field | Value |
|---|---|
| Original source | Fred Reinfeld, *Win At Chess* (1958, reissued by Dover Publications 1990). 300 instructive tactical puzzles. |
| EPD digitization | Community-maintained transcription; the canonical 300-position EPD has circulated since the mid-1990s. No single authoritative "first publisher" — the file passed through multiple chess sites before becoming a fixture in engine benchmarks. |
| Redistribution status | The puzzles themselves date from a 1958 print collection. The EPD digitization is universally vendored across open-source chess engines (Stockfish, Ethereal, Laser, Leela, every Rust engine on crates.io) without exception or controversy. Treated as community-standard test data. |
| Format | 6-field FEN + `bm` operand + `id` operand, one position per line. |

## `sts.epd` — Strategic Test Suite v1.0

A 1500-position positional / strategic test suite covering 15 themes
(100 positions per theme): Undermine, Open Files and Diagonals, Knight
Outposts, Square Vacation, Bishop vs Knight, Re-Capturing, Offer of
Simplification, Advancement of f/g/h Pawns, Advancement of a/b/c Pawns,
Simplification, Activity of the King, Center Control, Pawn Play in the
Center, Queens and Rooks to the 7th rank, Offer of Simplification (v2).

Unlike WAC, STS uses **weighted multi-move scoring**: each position lists
up to 5 candidate moves with assigned points (best move = 10, alternatives
0-9). The engine's chosen move scores the corresponding weight, enabling
finer-grained measurement than binary correct/incorrect.

| Field | Value |
|---|---|
| Authors | Dann Corbit and Swaminathan ("Sam") Natarajan. |
| First release | STS v1.0 published 2008-2009 via CCC (Computer Chess Club) and chess-engine community channels. |
| Redistribution status | Publicly distributed by the authors with the explicit purpose of being used to test and tune chess engines. Universally vendored across the open-source chess engine ecosystem. Treated as community-standard test data. |
| Format | 6-field FEN + `bm` (best move) + `id` (theme + index) + `c0` (full move/weight list, e.g. `"f5=10, Be5+=2, Bf2=3, Bg4=2"`) + `c7`/`c8`/`c9` (alternate-encoding move lists and weights), one position per line. |

## Refresh policy

These are vendored snapshots. The underlying corpora (1958 puzzles, 2008
STS v1.0) are frozen — there is no upstream drift to track. If a future
revision of STS appears (e.g. v2.0 with rebalanced themes), we may add
it alongside as a separate file rather than overwriting.
