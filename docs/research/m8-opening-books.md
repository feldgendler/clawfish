# Opening Book Landscape for M8.A

**Report for milestone M8.A (opening-book integration)**
**Research date: 2026-06-22**

---

## 1. Format Landscape

### 1.1 Polyglot (.bin) — the de-facto standard

**Structure.** A flat binary file of 16-byte entries, sorted in ascending key order. All integers are big-endian.

| Field | Type | Size | Description |
|---|---|---|---|
| `key` | u64 | 8 bytes | Zobrist hash of position |
| `move` | u16 | 2 bytes | Encoded move |
| `weight` | u16 | 2 bytes | Move quality metric |
| `learn` | u32 | 4 bytes | Online learning data (nearly always 0) |

**Position keying.** Uses the Polyglot 781-key Zobrist set — identical to what this engine already computes for `M1.D`. This is the defining advantage: no second hash scheme needed.

Key formula: `key = piece_XOR ^ castle_XOR ^ enpassant_XOR ^ turn_XOR`
- `RandomPiece[0..768]`: 12 piece kinds × 64 squares; piece-kind order is (bP, wP, bN, wN, bB, wB, bR, wR, bQ, wQ, bK, wK)
- `RandomCastle[768..772]`: WK, WQ, BK, BQ rights
- `RandomEnPassant[772..780]`: only XOR'd when an EP capture is actually possible (the M1.D rule)
- `RandomTurn[780]`: XOR'd when white to move

**Move encoding** (u16, bits 0-14):
- Bits 0-2: destination file
- Bits 3-5: destination rank
- Bits 6-8: source file
- Bits 9-11: source rank
- Bits 12-14: promotion (0=none, 1=N, 2=B, 3=R, 4=Q)

**Castling gotcha.** Castling is encoded king-to-rook, not king-to-king-final-square: WK=e1h1, WQ=e1a1, BK=e8h8, BQ=e8a8. The engine's book reader must convert to the UCI convention (e1g1 etc.) before sending the move. This is the most common parser bug.

**Lookup.** Binary search on `key`. All entries for one position are contiguous. Weight-proportional random choice is `weight / sum(weights)`. Entries with weight=0 are conventionally deleted (never present in well-formed books, but a reader must skip them to be defensive).

**Documentation status.** Openly documented. Canonical spec: [Polyglot book format by H.G. Muller](http://hgm.nubati.net/book_format.html); also in the [ddugovic/polyglot repo](https://github.com/ddugovic/polyglot/blob/master/book_format.html).

**Parser complexity.** Low. Straight `read_exact` in a loop; binary search; one move-encoding translation table. Under 150 lines of Rust including tests.

---

### 1.2 ChessBase CTG — the hard case

**Structure.** A four-file bundle:
- `.ctg` — actual data, organized in 4 KiB pages; page 0 = header (game count etc.); subsequent pages are data pages each starting with a header (position count + bytes used), followed by variable-length position entries
- `.ctb` — bitmap of free pages within the CTG file
- `.cto` — lookup table for fast indexed access into the CTG
- `.ini` — text auxiliary info

**Position keying.** Not Polyglot Zobrist. The format stores only white-to-move positions. Black positions require a color flip before lookup. Additional horizontal mirroring applies when the white king is on files a–d and neither side has remaining castling rights. No public documentation of the hash function; it was reverse-engineered.

**Specification status.** Proprietary; a partial spec was leaked via the Rybka Forum on 2007-09-30 by Sesse (Steinar H. Gunderson). This is the only public specification, and it is incomplete. The [Chess Programming Wiki CTG article](https://www.chessprogramming.org/CTG) summarises the page layout but does not document move encoding or the hash algorithm.

**Open-source implementations.** [ctgexporter](https://github.com/sshivaji/ctgexporter) (GPL-3.0, C++) — acknowledged incomplete as of its last commit; based on Daydreamer's CTG parsing plus Sebastien Major's work on parsing without `.cto`/`.ctb`. The [jja](https://git.sr.ht/~alip/jja) tool (Rust) also converts CTG to Polyglot.

**Parser complexity.** High. Variable-length entries, multi-file coordination, reverse-engineered incomplete spec, mandatory color-flip and mirror transforms, no Polyglot Zobrist reuse. Estimated 500–1,000 lines of non-trivial Rust to implement correctly. Open question: the exact hash algorithm is not publicly documented and would have to be inferred from existing implementations — which would require reading engine-adjacent source code and may not be possible under ADR-0003.

---

### 1.3 Arena ABK

**Structure.** A single binary file of 28-byte entries forming a linked tree. Each entry encodes one book move with statistics plus two index pointers (next move in line, sibling move). The starting position is at fixed index 900. Integers are little-endian (Windows/x86 convention).

| Field | Type | Notes |
|---|---|---|
| `from` / `to` | square indices | 0–63, a1=0 |
| `promotion` | piece code | 0=none, ±1=rook, etc. |
| `priority` | u8 | playback priority |
| `ngames` / `nwon` / `nlost` | counters | statistics |
| `plycount` | halfmove stats | |
| `nextMove` | index | > 0 if continuation exists |
| `nextSibling` | index | ≥ 0 for alternative moves |

**Position keying.** Tree structure — no hash index; navigation is by following pointers from root. Not Polyglot Zobrist.

**Documentation.** Reverse-engineered and documented on the [Chess Programming Wiki ABK article](https://www.chessprogramming.org/ABK). Fairly complete.

**Parser complexity.** Medium. Simpler than CTG (no page structure, single file), but the linked-tree navigation is non-trivial and the pointer-chasing structure is less cache-friendly than the Polyglot flat binary search.

---

### 1.4 Cerebellum / BrainFish native format

The distributed `Cerebellum_Light.bin` is **not** standard Polyglot despite the `.bin` extension. BrainFish's custom book handler is integrated directly into the engine binary and uses its own format that encodes backwards-calculated Stockfish moves, evaluations, search depths, and game-result statistics. The full (commercial) library contains multiple moves per position with evaluation metadata; the light (free) version limits to one or two best moves without evaluation data.

Conversion to Polyglot format is possible via the Cerebellum book converter and jja, but the native format is opaque and not publicly documented. **Not a target format for this project.**

---

### 1.5 Shredder BKT

Proprietary format for Shredder Classic. Documented only in Shredder user manuals. Not widely supported outside Shredder. The Perfect book series uses it as one of four distribution formats but it is not a target for independent engines. **Not a viable implementation target.**

---

### 1.6 OOBS (Open Opening Book Standard) — emerging / niche

SQLite-based format (`.obs.db3`) storing positions as FEN strings with raw win/draw/loss counts. MIT-licensed format spec at [github.com/nguyenpham/oobs](https://github.com/nguyenpham/oobs). Advantages: human-readable, editable without special tools. Disadvantages: ~3× larger than Polyglot for the same data; no chess engine has adopted it as of 2026; binary search performance is worse. **Not a current viable target.**

---

### Format comparison table

| Format | Keying | Entry size | Documented? | Parse difficulty | Engine adoption |
|---|---|---|---|---|---|
| Polyglot (.bin) | Polyglot Zobrist | 16 B fixed | Open spec | Low (~150 LOC) | Universal |
| CTG | Proprietary (RE) | Variable | Partial RE | High (500–1000 LOC) | Fritz/ChessBase GUIs only |
| ABK | Linked-tree index | 28 B fixed | RE, fairly complete | Medium (~250 LOC) | Arena only |
| BrainFish/Cerebellum | Custom opaque | Variable | Not documented | Impractical | BrainFish only |
| BKT | Proprietary | Unknown | Shredder manual only | High | Shredder only |
| OOBS | SQLite/FEN | ~70 B avg | Open (MIT) | Low (SQLite) | None |

**Bottom line on format choice:** Polyglot is the only format worth implementing natively. CTG would unlock specific books (Perfect CTG, HIARCS) but the implementation cost is very high and the format is proprietary with an incomplete spec — the resolution would likely require reading engine-adjacent source code, which is out of bounds under ADR-0003. ABK is a distant second option but adds no book that isn't also available in Polyglot.

---

## 2. Candidate Books

### 2.1 Perfect series (Sedat Canbaz)

- **Reputation/strength.** Engine-match–optimised book; depth mainly ≤ 8 moves; based on >1 million SCCT engine-engine games since 2002. Most recently tested against 3700+ Elo NNUE engines; 52% White / 48% Black expected results. Not a "sound theory" book in the human-player sense — tuned for engine vs. engine performance.
- **Generation.** Hand-tuned short opening lines, tested over large engine match sets. Not frequency-from-GM-games; not pure engine analysis. Hybrid.
- **Recency.** Latest public release: Perfect 2023 (released 2023-04-01). Site: [sites.google.com/site/computerschess](https://sites.google.com/site/computerschess/perfect-2023-books).
- **Size.** Not publicly stated; typical engine-match book is 1–5 MB in BIN format.
- **Formats.** BIN (Polyglot), CTG, ABK, BKT (all four formats).
- **License.** Freeware with restrictions: "Nobody is allowed to sell copies of Perfect books. It may be freely distributed for non-commercial purposes as long as no files in this package are modified." **Not suitable for a public git repo** without explicit permission — the "non-commercial" clause and "no modifications" clause are incompatible with open-source repo inclusion where the book might be modified or used commercially.

---

### 2.2 Cerebellum Light (zipproth.de / BrainFish)

- **Reputation/strength.** Contains ~4.5 million positions; backwards-calculated via Stockfish analysis with score consistency enforced by graph algorithm. Considered one of the deepest analytically-sound free books. High practical strength.
- **Generation.** Engine analysis (Stockfish), not game frequency. Score-consistent graph traversal.
- **Recency.** Updates stopped around 2020–2021; `Cerebellum_Light_3Merge_200916` appears to be the last public version.
- **Size.** ~400 MB in native format; smaller in converted Polyglot form.
- **Formats.** Custom BrainFish format (native); convertible to Polyglot via third-party tools.
- **License.** **Commercial** for the full library (sold via ChessBase). The Light version is made available for free download but has no explicit open-source/CC license. The site (zipproth.de) returned HTTP 403 during research — download terms are unclear. **Do not vendor in a public repo** — license is unknown/proprietary; free availability is by tolerance, not by explicit grant.

---

### 2.3 gm2600.bin / gm2001.bin

- **gm2600.bin.** Created by Pascal Georges. Built from high-rated GM game PGN (presumably 2600+ Elo). Default book in Scid vs. PC.
- **gm2001.bin.** Created by Oliver Deville. Games from 2001–2013 with minimum Elo ~2530.
- **Generation.** Frequency-weighted from GM game collections. Standard `polyglot make-book` workflow.
- **Size.** gm2600: ~346 KB; gm2001: ~2 MB.
- **Formats.** Polyglot BIN only.
- **License.** **Restricted.** Both books are copyright their respective authors; Scid's COPYING file notes that users must contact the respective authors before re-using these files for any purpose. Not GPL-licensed alongside Scid. **Do not vendor in a public repo without explicit permission.**

---

### 2.4 Performance.bin / varied.bin (Marc Lacrosse)

- **Reputation/strength.** Widely distributed; Performance.bin is strong for engine matches; varied.bin prioritises variety.
- **Generation.** Frequency-weighted from curated game collections. Source PGN and "cooking recipe" kept private by the author.
- **Size.** ~1.5 MB each.
- **Formats.** Polyglot BIN.
- **License.** Freely distributable as binary files. However, the source materials are private. No explicit open-source license stated. **Vendoring is ambiguous** — binary distribution is by tolerance; no explicit CC/MIT/GPL grant. Treat as "do not vendor without confirmation."

---

### 2.5 Titans.bin (Flavio Martin)

- **Reputation/strength.** Described as an excellent analysis and preparation tool; widely used in Italian computer chess community.
- **Generation.** Method not publicly documented.
- **Size.** Unknown.
- **Formats.** Polyglot BIN.
- **License.** Unknown. No explicit license found in any source. **Do not vendor without confirmation.**

---

### 2.6 ProDeo.bin (Ed Schröder / Jeroen Noomen)

- **Reputation/strength.** Jeroen Noomen book converted from the 2000/2001 REBEL book to Polyglot; enhanced with analysis from Dann Corbit and Les Fernandez; incorporates CCRL 40/40 2900+ rated games. 112 million moves. Old but still strong.
- **Generation.** Hybrid: original hand-tuned Noomen book + CCRL game frequency + engine analysis additions.
- **Size.** Large (DC.BIN variant is ~1.6 GB; ProDeo.bin alone is ~3.5 MB).
- **Formats.** Polyglot BIN; CTG conversion also exists.
- **License.** Available at rebel13.nl for free download. No explicit CC or OSS license documented in the TalkChess thread or the website pages that were accessible. **License unknown — do not vendor.**

---

### 2.7 HIARCS opening book (CTG)

- **Reputation/strength.** Commercial-grade, maintained, regularly updated. Used in top engine-engine competition. Available in a subscription model.
- **Generation.** Hand-crafted and engine-verified.
- **Formats.** CTG only.
- **License.** "Personal use only; not for commercial purposes; may not be hosted on another website or used in official tournaments without prior express written permission." Copyright Applied Computer Concepts Ltd. **Absolutely do not vendor.**

---

### 2.8 KomodoBook (Salvo Spitaleri)

- **Reputation/strength.** Available from komodochess.com. Built for variety; good for engine games.
- **Generation.** Not clearly documented. Likely frequency-weighted from curated games.
- **Formats.** Polyglot BIN.
- **License.** "Komodo is protected by copyright; even freeware versions cannot be redistributed on other websites." The book is bundled with the freeware engine distribution. License does not explicitly cover the book file separately. **Do not vendor without explicit permission.**

---

### 2.9 jja CC0 Lichess books (alpltl)

- **Reputation/strength.** Five books derived from Lichess rated games (2013-01 to 2023-03). Filtered by Elo threshold. Not hand-curated; frequency-weighted from actual games.
- **Generation.** jja `make-book` command applied to Lichess monthly PGN dumps. Weights scaled automatically by jja to avoid u16 overflow.
- **Size.** Ranges from 50 KB (3000+ Elo, Magnus) to 159 MB (2400+ Elo). Available at [chesswob.org/jja/books/](https://www.chesswob.org/jja/books/).
- **Available books.** `lichess-201301-202303-2800+.bin.zst` (3 MB), `lichess-201301-202303-3000+.bin.zst` (50 KB), `lichess-201301-202303-elo2400.bin.zst` (159 MB), `lichess-201301-202303-gm2600.bin.zst` (5.6 MB), `lichess-201301-202303-magnus.bin.zst` (47 KB).
- **Formats.** Polyglot BIN (compressed with Zstandard).
- **License.** **Creative Commons CC0** — explicitly permits research, commercial use, publication, modification, and redistribution. This is the cleanest license of any ready-made candidate. **Suitable for vendoring in a public repo.**
- **Gotcha.** These books are "current as of 2023" and have not been updated since April 2023. Coverage is Lichess online games only — includes computers, bots, and human players, not purely GM OTB games.

---

### 2.10 Human.bin

- **Origin.** Author and generation method not definitively identified in research.
- **License.** Unknown. Widely redistributed informally. **Do not vendor without confirmation.**

---

### 2.11 rodent.bin (Pawel Koziol)

- **Reputation.** Opening book for the Rodent chess engine; distributed alongside the engine.
- **License.** Bundled with Rodent (GPL). Whether the book itself inherits GPL is unclear. **License status ambiguous — do not vendor without confirmation.**

---

## 3. Self-Generation Option

### 3.1 Data sources

| Source | Content | License | Elo filter possible? | Size |
|---|---|---|---|---|
| Lichess monthly PGN dumps | All rated standard games by month | **CC0** | Yes (by PGN WhiteElo/BlackElo header) | 10–30 GB/month compressed |
| Lichess Elite Database (database.nikonoel.fr) | Lichess games 2500+/2300+ | No explicit license stated on site | Pre-filtered | 582 MB (2013-2025 combined .7z) |
| CCRL game archives | Engine-engine games | No explicit license (no explicit grant found at computerchess.org.uk) | By engine rating | Several GB |
| TWIC | Over-the-board GM/professional games | Not freely redistributable as a compiled database; individual issue downloads are available; bulk "all issues" requires a £30 donation | Yes (by event/players) | 4+ million games |
| Self-play on opening positions | Engine self-play | **Fully controlled — cleanest option** | N/A | As much as desired |

**Recommendation for generation:** Use the Lichess CC0 monthly PGN dumps with an Elo filter (e.g. 2300+ average, excluding bullet), or use jja's pre-built CC0 books directly. Both are legally unambiguous.

### 3.2 Standard polyglot make-book workflow

1. Download Lichess PGN dumps (CC0), decompress (`zstd -d`)
2. Filter by Elo using SCID, PGN-extract, or jja filter expressions
3. Run `polyglot make-book -pgn filtered.pgn -bin book.bin -max-ply 30 -min-game 3 -min-score 1`
4. Optionally merge white and black repertoires: `polyglot merge-book`

**Modern alternative:** `jja make-book` handles compressed PGN directly, can filter on Elo in one pass, and auto-scales weights to avoid u16 overflow:
```
jja make-book --input lichess_2024_01.pgn.zst \
    --filter "Elo >= 2400" \
    --output book.bin
```

### 3.3 Expected strength and size

- A book built from 2400+ Elo Lichess games covering common openings to 20 moves typically produces a 5–50 MB file covering the main lines well.
- For engine strength it won't meaningfully differ from hand-tuned books in the first 5–8 moves (most books agree there); the tail differs.
- A self-generated book ensures zero licensing issues and deterministic provenance.

---

## 4. Polyglot Weight Field Semantics

**The weight field is tool-defined, not format-defined.** The format spec (the `book_format.html` document) only specifies that weight is a u16 and that weight=0 entries should be skipped. It does not mandate a computation formula.

**Reference make-book tool formula.** The `polyglot make-book` tool computes:

```
raw_score(move) = 2 × wins(move) + 1 × draws(move) + 0 × losses(move)
```

...where wins/draws/losses are counted from the mover's perspective. All raw scores for a given position are then globally scaled so the maximum fits in u16 (0–65535). The scaling is:

```
weight = (raw_score × 65535) / max_raw_score_in_file
```

...applied at book-build time, across the entire file. This is what "globally scaled to fit into 16 bits" means — it is a linear rescale with the global maximum as the ceiling. Individual positions are therefore not comparable across books with different game counts; only within-position relative weights are meaningful.

**Weight=0 convention.** Entries with weight=0 are permanently excluded from selection. The standard polyglot make-book uses the `-min-score` filter to exclude entries below a threshold before writing — so weight=0 entries should not appear in a well-formed book. However, a defensive reader should skip them anyway; some book-building tools write them without filtering.

**u16 saturation at 65535.** If a move appears in so many games that its raw score would round to 65535 and other moves in the same position would lose distinguishing resolution, the global scaling can push rare alternatives to weight=0. In practice this only matters for extremely common positions (the starting position, 1.e4 response) in very large books.

**The learn field (u32, 4 bytes).** Nearly always zero in distributed books. Polyglot 2.0 and later can use it to record online learning: successful moves accumulate higher learn values and the engine adjusts selection probabilities. Not relevant for a first implementation.

**BanksiaGUI uses a different formula:** `5 × wins + 2 × draws + 0 × losses`. The weight formula is convention-only; engines must treat weights as opaque relative priorities within a position, not as absolute scores.

Sources: [hgm.nubati.net book_format.html](http://hgm.nubati.net/book_format.html), [ddugovic/polyglot book_format.html](https://github.com/ddugovic/polyglot/blob/master/book_format.html), [BanksiaGUI book creation docs](https://banksiagui.com/wiki/create-opening-books-from-games/), [TalkChess understanding polyglot books](https://talkchess.com/viewtopic.php?t=81626).

---

## 5. Synthesis / Recommendation Matrix

### 5.1 Candidate scoring

| Book | Strength | License | Format cost | Vendor? |
|---|---|---|---|---|
| jja CC0 2800+ Lichess | Good (2800+ games, 2013-23) | **CC0** | Polyglot (zero) | **Yes — primary candidate** |
| jja CC0 gm2600 Lichess | Good (2600+ games, large) | **CC0** | Polyglot (zero) | Yes — larger alternative |
| Self-generated (Lichess CC0 PGN) | Tunable | **CC0** (provenance fully controlled) | Polyglot (zero) | Yes — build at any time |
| Perfect 2023 (BIN) | Good (engine-match tuned) | Freeware, non-commercial, no modification | Polyglot (zero) | No — license prohibits commercial use |
| gm2600.bin (Pascal Georges) | Good | Author copyright, contact required | Polyglot (zero) | No |
| Performance.bin (Marc Lacrosse) | Good | No explicit license | Polyglot (zero) | No — ambiguous |
| Titans.bin (Flavio Martin) | Unknown quality claim | Unknown | Polyglot (zero) | No |
| ProDeo.bin (Rebel) | Very large, strong | Unknown explicit terms | Polyglot (zero) | No |
| Cerebellum Light | Excellent (engine analysis) | Proprietary/commercial | Custom format (impractical) | No |
| HIARCS CTG | Commercial-grade | All rights reserved | CTG (very high) | No |
| KomodoBook | Engine-match tuned | Bundled with engine, no redistribution | Polyglot (zero) | No |

### 5.2 Format parser recommendation

**Implement Polyglot only.** No existing book justifies the CTG implementation cost:
- Cerebellum (the main CTG-adjacent contender) is commercially licensed.
- HIARCS is commercially licensed.
- Perfect 2023 is available in BIN; the CTG version adds nothing.
- The CTG spec is incomplete, proprietary, and would likely require reading engine-adjacent source code (ctgexporter C++ or jja Rust) to resolve ambiguities — directly conflicting with ADR-0003.

ABK similarly adds no book that is not also available in Polyglot; skip it.

### 5.3 Book vendoring recommendation

**Vendor `lichess-201301-202303-2800+.bin` from the jja CC0 collection as the default production book.** Rationale:
- CC0 license — unambiguously safe for a public git repo including any future commercial use.
- Good quality: 2800+ Elo Lichess games cover all main theory lines.
- Reasonable size: ~3 MB compressed (decompresses to similar; acceptable to commit).
- Provenance is clear: CC0 from Lichess CC0 data, built with jja whose method is documented.

**Do not vendor** Perfect, gm2600, Performance, Titans, ProDeo, Cerebellum, or KomodoBook — all have unclear or restrictive licenses.

### 5.4 Test fixture recommendation

**Self-generate a tiny fixture book** for the parser correctness gate. The recommended approach:

1. Construct two or three known positions programmatically (starting position, position after 1.e4, position after 1.e4 e5).
2. Manually compute the expected Polyglot key for each (engine already has the hash; verify against the known 781-key values from M1.D).
3. Construct 16-byte entries by hand with known move encodings and weights (e.g., e2e4 encoded as bits per spec, weight=100).
4. Write the entries sorted by key to a tiny `.bin` file committed at `tests/fixtures/opening_test.bin`.

This gives unambiguous provenance (no third-party copyright) and a deterministic oracle for every bit of the decoder. It also catches the castling encoding gotcha (can include one castling position as a test case) and the big-endian field ordering.

**Alternative:** use the jja CC0 `lichess-201301-202303-3000+.bin.zst` (50 KB compressed) as a fixture. It is CC0, small, and covers real positions. The downside is it lacks hand-crafted known byte values to diff against.

**Not recommended:** using gm2600.bin or Performance.bin as fixtures — their license is unclear and they should not be committed.

---

## Key Gotchas and Corner Cases

- **Castling encoding mismatch.** Polyglot encodes castling as king-to-rook square (e1h1), not king-to-king-final (e1g1). A book reader that passes the raw Polyglot move to the engine over UCI without translating it will produce illegal moves or no-ops. Must translate before sending.
- **Big-endian field order.** All u64, u16, and u32 fields in Polyglot entries are big-endian. x86/ARM are little-endian. `u64::from_be_bytes` / `u16::from_be_bytes` required.
- **Weight=0 entries.** Skip them; they represent deleted entries.
- **Promotion encoding.** Bits 12-14 of the move word: 0=not a promotion, 1=N, 2=B, 3=R, 4=Q. Underpromotions are thus representable.
- **Hash collisions.** Multiple distinct positions can share a Polyglot key (64-bit birthday collision probability is negligible in practice but a reader should not panic on unexpected entries; just skip if the position doesn't match after full board verification, though books don't store the full board so verification requires position lookup). The load-bearing defense is to validate every decoded move against the position's legal move list and skip any that doesn't validate.
- **Entries sorted by key, not by position.** Binary search finds the first entry with a matching key; must read forward until the key changes to collect all moves for a position.
- **"Engine books" vs Polyglot books share the .bin extension.** Winboard-era proprietary engine books also used `.bin`. Loading one as the other will produce garbage. A Polyglot book can be identified by its sorted structure and 16-byte entry alignment.
- **Weight scaling is global.** Weights from two different books are not comparable; only within-position relative weights matter.
- **Cerebellum Light .bin is NOT standard Polyglot.** Despite the extension, the native Cerebellum format requires BrainFish's custom book handler.

---

## Locked M8.A scope (decided 2026-06-22)

Decisions taken on the basis of this report; the format scope falls out of the book
evaluation (requirements-first), not the other way around.

- **Format: Polyglot `.bin` only.** Requirements-justified, not convenience-justified —
  every book strong enough to warrant a CTG parser (Cerebellum, HIARCS) is
  proprietary/commercial and therefore unredistributable, so the hard parser unlocks
  nothing. CTG / ABK / OOBS are out of scope.
- **Decoder.** Header-less 16-byte big-endian records; binary search by the M1.D Polyglot
  Zobrist key; collect contiguous same-key entries. Decode the king→rook castling
  encoding and promotion bits. Weights treated as opaque within-position priorities;
  selection policies weighted-random (default) + best-weight; sum in `u64`; skip weight-0.
- **Robustness / fuzzing.** M8.A is the engine's first runtime parser of an untrusted
  binary blob. Mandatory legality validation: every decoded move is checked against the
  position's legal moves and skipped (never played) on mismatch — this neutralizes Zobrist
  collisions and corruption. Any structural anomaly → "book miss → fall through to search,"
  never a panic. Ship a **fuzz target** over the decoder (likely the project's first):
  never panics, terminates, only returns legal moves, no overflow. Add "fuzz harness for
  binary parsers" to `docs/tooling-backlog.md`; this sets the precedent for M11 NNUE binary
  loading.
- **Production book.** Vendor jja CC0 `lichess-201301-202303-2800+.bin` (~3 MB, CC0) as the
  default, **overridable via `BookFile`**.
- **Test fixture.** Self-generate a tiny hand-built `.bin` (unambiguous copyright + a
  byte-exact oracle for the castling / big-endian / promotion gotchas).
- **UCI.** `OwnBook`, `BookFile`, plus a selection-policy knob; root probe before search;
  miss → normal search. Open plan-time detail: `OwnBook` default (proposed **on**, since we
  ship a book).
- **Multi-book.** Design the probe path as a `BookProvider` trait seam from day one (also the
  future multi-format seam), but ship single-book in v1. Priority-chain is a cheap optional
  add; blended-union (per-book normalization + mixing coefficients) deferred.
- **Gate.** Correctness, not Elo (M5.E precedent): hash-compat + weight-decode round-trips +
  the fuzz invariants. SPRT dilutes book Elo to ~0 by design; strength confirmation deferred
  to M13. One ADR allocated at landing. Est. ~500–800 LOC.

---

## Sources

- [Chess Programming Wiki — Opening Book](https://www.chessprogramming.org/Opening_Book)
- [Chess Programming Wiki — PolyGlot](https://www.chessprogramming.org/PolyGlot)
- [Chess Programming Wiki — CTG](https://www.chessprogramming.org/CTG)
- [Chess Programming Wiki — ABK](https://www.chessprogramming.org/ABK)
- [Polyglot book format (H.G. Muller / WinBoard)](http://hgm.nubati.net/book_format.html)
- [ddugovic/polyglot book_format.html](https://github.com/ddugovic/polyglot/blob/master/book_format.html)
- [python-chess polyglot documentation](https://python-chess.readthedocs.io/en/latest/polyglot.html)
- [jja blog post — Introducing jja 0.4.0](https://lichess.org/@/alpltl/blog/introducing-jja-040/qn4NPl2l)
- [jja CC0 books — chesswob.org](https://www.chesswob.org/jja/books/)
- [jja TalkChess thread](https://talkchess.com/viewtopic.php?t=81702)
- [TalkChess — Understanding polyglot books](https://talkchess.com/viewtopic.php?t=81626)
- [TalkChess — Opening books free to community](https://talkchess.com/viewtopic.php?t=17749)
- [TalkChess — Opening book in bin format](https://talkchess.com/viewtopic.php?t=61146)
- [TalkChess — ProDeo book in Polyglot](https://talkchess.com/viewtopic.php?t=59435)
- [TalkChess — Is there a big Cerebellum.bin?](https://talkchess.com/viewtopic.php?t=61078)
- [TalkChess — Open Opening Book Standard OOBS](https://talkchess.com/forum/forum3/viewtopic.php?t=79804)
- [SedatChess Perfect 2023 books](https://sites.google.com/site/computerschess/perfect-2023-books)
- [ChessEngeria — Perfect 2023 books released](https://www.chessengeria.eu/post/perfect-2023-books)
- [BanksiaGUI — Create opening books from games](https://banksiagui.com/wiki/create-opening-books-from-games/)
- [Lichess open database — CC0 license](https://database.lichess.org/)
- [Lichess Elite Database](https://database.nikonoel.fr/)
- [HIARCS opening books — license terms](https://www.hiarcs.com/chess-opening-books.html)
- [ctgexporter — GitHub](https://github.com/sshivaji/ctgexporter)
- [OOBS — GitHub](https://github.com/nguyenpham/oobs)
- [Creating an opening book for Maverick](https://www.chessprogramming.net/creating-opening-book-maverick/)
- [TWIC archive](https://theweekinchess.com/twic)
- [Scid vs. PC COPYING file (re: gm2600/Performance license)](https://sourceforge.net/p/scidvspc/code/1584/tree/COPYING)
- [PGN2ABK — GitHub](https://github.com/Tearth/PGN2ABK)
- [Chess Programming Wiki — Brainfish](https://www.chessprogramming.org/Brainfish)
