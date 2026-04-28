# ADR-0009 — Polyglot Zobrist key set with EP-only-when-pseudo-legal hashing

**Status:** Accepted, 2026-04-27.
**Phase:** Binds at M1.D.

## Context

Zobrist hashing is a near-universal device for chess engines: the position hash drives the transposition table, repetition detection, and (post-M3) opening-book lookups. The technique is settled — `position_hash = XOR of (piece-on-square key, side-to-move key, castling-rights keys, en-passant-file key)` — but two ingredients are project decisions:

1. **Where the keys come from.** A Zobrist hash needs ~781 random `u64` constants. Each engine that wants to read a published Polyglot opening book must use the same key set the book was built with; choosing the **Polyglot** key set up front is the cheapest way to keep that future option open.
2. **The en-passant hashing rule.** FEN unconditionally sets the EP target after any double pawn push, even when no opponent pawn could actually capture. Hashing that target leads to spurious TT misses: the same physical position reached by `(white double-push, black plays X, white plays Y)` differs in hash from one reached without the spurious EP target ever being set. Two repair rules exist — the **Polyglot rule** (hash EP file iff capture is *pseudo-legally* possible — opponent pawn adjacent to the just-pushed pawn on the right rank, no further checks) and a stricter **legal-only refinement** (also require the capturing pawn to not be pinned, and the EP move not to expose own king to check). (Update: ADR-0015 later promoted the pseudo-legal predicate from a hash-time filter to a state-level invariant on `Position::ep_target` — both FEN parse and `make_move` now sanitize phantom EPs to `None`. The hashing rule below is unchanged; the description of FEN's "unconditionally sets the EP target" behavior describes the *prior* state of `Position`, not the current one.)

`docs/research/m1-engine-architecture.md` §5 ("Zobrist hashing") establishes the ground for both, citing the [Chess Programming Wiki](https://www.chessprogramming.org/Zobrist_Hashing), Wikipedia, and the [Polyglot book format spec](http://hgm.nubati.net/book_format.html). The CastlingRights bit layout in `src/position.rs` was already aligned with Polyglot's castling-key order (bit 0 = WK, 1 = WQ, 2 = BK, 3 = BQ) by M1.B in anticipation of this ADR. The PieceKind enum (`Pawn=0..King=5`) likewise matches Polyglot's piece sub-axis.

## Decision

1. **Key set: vendor the published Polyglot 781-key table verbatim**, organized by Polyglot's offset convention:
   - `RandomPiece[0..768]` — `64 * kind_color + 8 * row + file`, where `kind_color` is the Polyglot enumeration (`BP=0, WP=1, BN=2, WN=3, BB=4, WB=5, BR=6, WR=7, BQ=8, WQ=9, BK=10, WK=11`), `row` is the rank index (rank 1 = 0, rank 8 = 7), and `file` is the file index (a = 0, h = 7).
   - `RandomCastle[768..772]` — order: WK (768), WQ (769), BK (770), BQ (771).
   - `RandomEnPassant[772..780]` — by file (a=0..h=7).
   - `RandomTurn[780]` — XORed in **only when White is to move**. (This is asymmetric — Black-to-move contributes 0.)

2. **EP hashing rule: Polyglot-style pseudo-legal.** Hash the EP file **iff** the side to move has a pawn adjacent (same rank, ±1 file) to the opponent's just-pushed pawn — i.e. iff a pawn capture *toward* the EP target square is geometrically available. **No legality check** beyond pawn presence: pinning, discovered check, and own-king-check are *not* tested. This matches the Polyglot spec verbatim ("...irrelevant if the potential en passant capturing move is legal or not...") and makes our hashes interchangeable with Polyglot books.

3. **Castling key encoding: 4 individual keys, one per right, XORed for each set right.** Not the 16-precomputed-combinations variant. Polyglot uses 4; matching it preserves book interoperability and lines up directly with `CastlingRights`'s 4-bit layout.

4. **Side-to-move asymmetry pinned in the implementation.** `RandomTurn[780]` is XORed iff White-to-move. Tests must specifically exercise this — symmetric "XOR it on every flip" code passes most plausible round-trip tests but breaks Polyglot interoperability.

5. **Storage shape.** `pos.zobrist: u64` field, set by `Position::starting_position()` and `Position::from_fen()` via from-scratch computation. The same `from_scratch` function powers M1.E's debug-build round-trip assert (`debug_assert_eq!(pos.zobrist, zobrist::from_scratch(pos))` after every make/unmake).

6. **Vendoring methodology.** The 781 key constants live in `src/zobrist/keys.rs` as a single `pub(super) const POLYGLOT_KEYS: [u64; 781]` array, extracted from the spec HTML shipped with the Homebrew `polyglot` package (`/opt/homebrew/Cellar/polyglot/<version>/share/doc/polyglot/book_format.html`) — the same authoritative `book_format.html` document that hgm.nubati.net hosts. The file's leading comment names the source, the date of extraction, and the awk-style one-liner used (so a future re-vendor is a 30-second operation). First key (`0x9D39247E33776D41`) and last key (`0xF8D626AAAF278509`) are pinned by unit test — sentinel against accidental row-shift during edits.

## Alternatives considered

- **Generate keys from a deterministic PRNG** (e.g. SplitMix64 with a fixed seed). Saves ~6 KB of vendored data and avoids any "where did these constants come from" question. **Rejected:** opening-book reuse — explicitly listed as in-scope test data in `docs/prior-art.md` — would require a key-translation layer or a re-hash pass at book-load time. The 6 KB cost is trivial; the future-friction cost of breaking Polyglot interop is meaningful.

- **Stricter legal-only EP hashing** (also test pin and discovered-check). Marginally improves TT hit rate by suppressing the extremely rare "pinned EP-capturer" cases. **Rejected for now:** more code, harder to test, and incompatible with Polyglot books. Revisit at M4 if profiling shows spurious EP hashing measurably hurts TT hit rate.

- **Hash EP unconditionally** (matching FEN's "set after every double push"). **Rejected:** the spurious-cache-miss problem from `docs/research/m1-engine-architecture.md` §5. Materially hurts TT hit rate post-M4.

- **16 precomputed castling-combination keys.** A tiny (4-XOR vs. 1-load) speed difference per make/unmake. **Rejected:** breaks Polyglot interop and obscures the per-right structure that incremental updates need.

- **Compute the starting position's hash as a `const`** so `Position::starting_position()` stays `const fn`. **Rejected:** `from_scratch` loops over squares and is not const-compatible without invasive rewrites; `starting_position` is called once per game and is not on a hot path. We drop `const fn` for it. The starting-position hash *is* still pinned by the published test vector (`0x463b96181691fc9c`) — just at runtime, not at compile time.

## Consequences

- **+** Hash matches every Polyglot opening book in existence; future book-lookup milestone (post-roadmap) is a parser, not also a re-hash.
- **+** EP rule sidesteps the spurious-TT-miss problem at the standard cost (~5 lines of pawn-adjacency check at hash-update time).
- **+** All four sub-axes (piece order, castling order, EP file, side-to-move asymmetry) line up with what `Position` already stores — no remap layer.
- **−** Vendored constants. We commit to the spec being right; we don't regenerate them. Mitigation: the vendored file's first/last entries are pinned by test, and the 9 spec-published `(FEN, key)` test vectors stress every sub-axis; any vendoring error surfaces immediately.
- **−** Slightly more state on `Position` (one `u64`). Trivial.
- **−** Chess960 castling — Polyglot has a 16-entry RandomCastle extension for it. **Out of scope** by `decisions/0001` (variant chess explicitly excluded).

## A note on "PolyGlot is being retired"

The CPW page on the **PolyGlot adapter tool** (a C utility from Hartmut Kapsch / HGM that translated XBoard ↔ UCI for legacy engines and also bundled `make-book` / `merge-book` / `info-book` commands) does describe it as essentially abandoned upstream — modern engines speak UCI natively, so the adapter has lost its original use. Homebrew is also deprecating the formula on 2027-01-05 because the upstream URL is HTTP-only.

This **does not** apply to the **PolyGlot binary opening-book format**, which is a frozen spec — a sorted array of `(zobrist_key, move, weight, learn)` records — implemented by every modern chess tool (cutechess, fastchess, scid, python-chess, ChessBase, etc.). The format and its key table outlive the adapter tool, and our hash interoperability targets the format. The deprecation of the adapter tool does not affect this ADR's commitment.

## References

- Spec: `book_format.html` (Hartmut Kapsch / Harm Geert Muller). Authoritative copy at `http://hgm.nubati.net/book_format.html`; functionally identical copy ships with Homebrew `polyglot` at `/opt/homebrew/Cellar/polyglot/<version>/share/doc/polyglot/book_format.html`.
- [Chess Programming Wiki — Zobrist Hashing](https://www.chessprogramming.org/Zobrist_Hashing)
- [Wikipedia — Zobrist hashing](https://en.wikipedia.org/wiki/Zobrist_hashing)
- `docs/research/m1-engine-architecture.md` §5 — prior-art synthesis.
- `decisions/0001-variant-chess-out-of-scope.md` — Chess960 exclusion grounds.
