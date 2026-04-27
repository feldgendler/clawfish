# M1 prior-art research — engine architecture

Research pass for M1 (move generator + perft). Scope: bitboard layout, move encoding, move generation strategy, make/unmake, Zobrist, edge cases, performance baselines. Magic bitboards are a sibling agent's territory and treated here as a black box that returns attack-set bitboards.

Sources: Chess Programming Wiki (CPW), TalkChess, blog posts, Wikipedia, the Polyglot format spec. No engine source code was read.

---

## 1. Bitboard layout & square indexing

### Square indexing — LERF

CPW confirms **Little-Endian Rank-File (LERF)** with `a1=0, h1=7, a8=56, h8=63` as the dominant convention, used in nearly all CPW samples ([Square Mapping Considerations](https://www.chessprogramming.org/Square_Mapping_Considerations), [Squares](https://www.chessprogramming.org/Squares)). Relation: `square = 8*rank + file`.

Why LERF wins: consecutive squares of a rank are neighboring bits (shift by 1 = east; by 8 = north from white's side); indices line up with the 0..63 enumeration FEN/PGN/UCI tooling expects. The only alternative — LEFR (file-rank) — buys marginal pawn-attack-wraparound convenience and loses the natural a-to-h ordering. Usability note: a naive LSB-first bitboard print reads top-down ranks 1..8, which is unintuitive — provide a debug pretty-printer day one.

**Recommendation: LERF.** Settled, not research outcome.

### Position struct: 6+2 bitboards, not 12

CPW's [Bitboard Board-Definition](https://www.chessprogramming.org/Bitboard_Board-Definition) lays out two schemes:

- **12 bitboards** (one per color×type), 96 B. "White pawns" is one load.
- **Denser 6+2** (6 piece-type + 2 color-occupancy bitboards), 64 B. "White pawns" = `pawns & white`.

The denser scheme fits piece data in a single cache line, replaces a color branch with a uniform AND (better predictability), and lets generic / pawn movegen code be color-symmetric (`pawns & us`) instead of branchy. The 12-bitboard scheme's only edge — single-load access — is a non-issue on a modern OOO core.

**Recommendation: 6+2.** Both defensible; if wrong, 1-day refactor — public API unchanged.

CPW and the broader community ([Chess Position](https://www.chessprogramming.org/Chess_Position), Rustic Chess docs, Billy Levin) converge on these additional fields:

- **Piece-on-square mailbox** (`[Option<Piece>; 64]`). One extra write per make_move; saves a popcount + AND on every capture lookup. Standard.
- **Cached king squares** for both colors. Used in nearly every check test.
- Side-to-move; castling rights (4 bits, `KQkq`); EP target (`Option<Square>`); halfmove clock (`u8`); fullmove number (`u16`); Zobrist hash (`u64`).

Sketch (under 200 B total):

```
Position {
    piece_bb: [u64; 6], color_bb: [u64; 2],
    mailbox: [Option<(Color, Piece)>; 64],
    king_sq: [Square; 2],
    side: Color, castling: u8, ep: Option<Square>,
    halfmove: u8, fullmove: u16, zobrist: u64,
}
```

---

## 2. Move encoding

### 16-bit format

CPW's [Encoding Moves](https://www.chessprogramming.org/Encoding_Moves) gives the canonical layout: bits 0–5 = from, 6–11 = to, 12–15 = a 4-bit flag nibble. The nibble splits into four 1-bit subfields — `promotion`, `capture`, `special1`, `special0` — yielding 16 codes:

| Code | Promo | Capt | S1 | S0 | Meaning |
|---|---|---|---|---|---|
| 0 | – | – | – | – | quiet |
| 1 | – | – | – | ✓ | double pawn push |
| 2 | – | – | ✓ | – | king-side castle |
| 3 | – | – | ✓ | ✓ | queen-side castle |
| 4 | – | ✓ | – | – | capture |
| 5 | – | ✓ | – | ✓ | en passant |
| 8–11 | ✓ | – | – | – | knight/bishop/rook/queen promotion |
| 12–15 | ✓ | ✓ | – | – | knight/bishop/rook/queen promotion-capture |

Codes 6 and 7 are unused. The bottom two bits in promotions select N/B/R/Q. Moving and captured piece types are *not* encoded — they're recovered from the mailbox at make-time, essentially free.

### 32-bit extension?

CPW notes some engines pack the moving piece (3 bits), captured piece (3 bits), and an ordering score (~16 bits) into 32 bits. Pros: a `Move` knows enough to apply itself without the board. Cons: doubles move-list memory; redundant with our mailbox.

**Recommendation: 16-bit `Move` for storage, with a separate `ScoredMove` (Move + i16, 32 bits) used only in ordered move lists during search.** NNUE-readiness does *not* require a wider move — see §4.

### Move list

[CPW Move List](https://www.chessprogramming.org/Move_List) confirms: stack-allocated fixed-size array. The proven maximum legal moves in any reachable position is **218** (Petrović 1964 composition; integer-programming proof in 2025 — [Lichess](https://lichess.org/forum/redirect/post/0WBRk1ex), [HN](https://news.ycombinator.com/item?id=45382755)). Engines round to **256** for both legal and pseudo-legal lists; pseudo-legal counts can transiently exceed legal but never near 256.

**Recommendation: inline `[Move; 256]` + `u8` length.** No allocation.

---

## 3. Move generation strategy

### Pseudo-legal-and-filter vs. legal-direct

The single biggest architectural choice in M1, and prose is genuinely split.

**Pseudo-legal-and-filter** (CPW's [Move Generation](https://www.chessprogramming.org/Move_Generation), endorsed by most TalkChess threads — [78084](https://talkchess.com/viewtopic.php?t=78084), [76473](https://talkchess.com/viewtopic.php?t=76473)): generate moves ignoring own-king-in-check, then for each played move test legality post-hoc. Simpler code, no upfront pin tracking; cheap when most moves are legal — and a beta-cutoff before the legality test costs nothing.

**Legal-direct** (Peter Ellis Jones, Pradu Kannan, [Analog Hors](https://analog-hors.github.io/site/magic-bitboards/), cozy-chess school): compute checkers, pinned-pieces, capture-mask and push-mask up front, then emit only legal moves. No downstream legality test; stalemate/mate detection becomes `legal_moves.len() == 0`. Peter Ellis Jones reports 106 Mnps for his pure legal generator vs. ~190 Mnps for Qperft (tuned pseudo-legal), so it's not free — but it's not a 10× hit either ([generating legal moves efficiently](https://peterellisjones.com/posts/generating-legal-chess-moves-efficiently/)).

**Recommendation: legal-direct.** Reasoning specific to this project:

1. The user can't catch subtle illegal-move-slips-through bugs by reading code. "The move list is the truth" is a TDD-friendlier invariant.
2. Pin/check/mask infrastructure is wanted *anyway* for evasion specialization (next subsection) and later for SEE and check extensions in search.
3. Perft fixtures from Stockfish are legal-move counts, lining up directly with our generator output. Removes the "is the bug in gen or in the legality filter?" question.

Switching cost if we're wrong: moderate, not free. Internal structure is invasive (mask-based filtering vs. validate-then-emit), but the public API (`fn generate_moves(&Position) -> MoveList`) is unchanged. ~1 week to flip.

### Check evasion specialization

Universally agreed (CPW [Check](https://www.chessprogramming.org/Check), Peter Ellis Jones, Analog Hors):

- **Not in check**: regular generation with pin filtering.
- **Single check**: only (a) king moves to safe squares, (b) capture the checker by a non-pinned piece, (c) interpose on the ray (sliding checkers only, non-pinned pieces only). Implementation: `capture_mask = checkers; push_mask = ray_between(king, checker)`; AND every non-king destination.
- **Double check**: king moves only. Cannot block both, cannot capture both. (En passant is the unique single-move way to *cause* a double check, but the response is still king-only.)

The classic subtlety: when computing which squares the king can flee *to*, compute opponent attacks against `occupancy ^ king_bb` — otherwise the slider that's checking through the king fails to attack the square the king is fleeing to along the same ray. Easy to forget; one of the canonical movegen bugs.

This decision is **cheap to revise independently** of the main gen choice — evasion is a separate code path.

---

## 4. Make/unmake mechanics

### Minimum Undo struct

[CPW Unmake Move](https://www.chessprogramming.org/Unmake_Move) and [CPW Make Move](https://www.chessprogramming.org/Make_Move) call out the irreversible state:

```
Undo {
    captured: Option<Piece>,    // 1B
    prior_castling: u8,         // 1B
    prior_ep: Option<Square>,   // 1–2B
    prior_halfmove: u8,         // 1B
    prior_zobrist: u64,         // 8B
}                                // ~16B with padding
```

That's it. Notable absences:

- **Captured square** — only differs from `move.to` for EP, and is then recoverable from the move + side-to-move.
- **Prior pinned/checker bitboards** — only needed if we cache them in `Position`. Recommendation: don't cache; recompute at top of generation. Cheaper than Undo bloat and avoids invalidation bugs.
- **Prior fullmove number** — deterministically recoverable, but it's one byte; either is fine.

Stack of Undos is `~16 B × ~128 ply = ~2 KB`. Negligible.

### Special-case unmake

- **Castling**: move both king and rook back; the move flag tells you which rook.
- **En passant**: restore the captured pawn one rank behind the EP target.
- **Promotion**: replace the promoted piece on `to` with a pawn on `from`.
- **Promotion capture**: combine with restoring the captured piece on `to`.
- **Double pawn push**: nothing structural; restore EP from Undo.

The mailbox must be kept consistent through all of these — easy to forget for EP, where one square (the captured pawn's) is touched but isn't `from` or `to`.

### NNUE readiness

Per `decisions/0004`, the obligation is just that `make_move` and `unmake_move` are discrete function calls with enough info to compute an accumulator delta. They are. The delta needs: the moving piece's old/new square (mailbox at `from` + `move.to`); the removed piece + square (Undo + EP math); the added piece (move flags for promotion).

The clean **interception point** is the body of `make_move` / `unmake_move`. A future change adds:

```
make_move(pos, mv, &mut undo) {
    /* … existing body … */
    nnue_accumulator.update(<deltas from mv, undo, prior mailbox>);
}
```

No signature change, no plumbing through search. **The 16-bit `Move` is sufficient for NNUE.** Don't widen for hypothetical needs.

---

## 5. Zobrist hashing

### The 781-key table

[CPW Zobrist Hashing](https://www.chessprogramming.org/Zobrist_Hashing), [Wikipedia](https://en.wikipedia.org/wiki/Zobrist_hashing):

- **768** keys for piece-on-square (12 × 64).
- **1** key XORed in on black-to-move.
- **4** keys for individual castling rights, **or** 16 precomputed combinations. Speed/memory tradeoff; both work.
- **8** keys for the EP **file** (rank is determined by side-to-move).

Total 781 (or 793 with the 16-key castling form). Both go by "Zobrist."

### Why XOR works

`a XOR a = 0` (self-inverse — same XOR removes what you added) and XOR is commutative + associative (order-independent). So make_move can XOR-out everything that changed and XOR-in everything new in any order; the result equals from-scratch. **Add a debug-build assert that `pos.zobrist == compute_from_scratch(pos)` after every make/unmake** — cheapest, most powerful test for off-by-one update bugs.

### EP subtlety — the spurious-cache-miss problem

Naive rule: hash the EP file whenever FEN has an EP target. **Problem**: a position reached via `(white double-push, black plays X, white plays Y)` differs in hash from the same position reached without the spurious EP target ever being set. TT misses; work redone.

**Polyglot convention** ([book_format.html](http://hgm.nubati.net/book_format.html)): hash the EP file only when an enemy pawn is positioned to make the EP capture *pseudo-legally* (i.e., an opponent pawn on an adjacent file at the right rank). This is what python-chess's Polyglot keys default to.

A stricter refinement (raised in [python-chess #144](https://github.com/niklasf/python-chess/issues/144)): hash only when the EP capture is fully **legal** (capturer not pinned, no horizontal-pin issue). More correct for TT, more expensive at update time.

**Recommendation: implement the Polyglot rule.** Standard, what published books rely on, gets ~99% of the TT-hit benefit cheaply. The legal-only refinement is a search-time micro-optimization to revisit during M4 if profiling shows spurious EP hashing matters. Mechanically: in make_move, after a double push only XOR the EP file key if there's an opposite-color pawn on a square adjacent to the destination. One AND on the opponent pawn bitboard.

**Document this commitment** so we don't quietly drift to the naive rule under deadline pressure.

### Random source

CPW emphasizes a deterministic PRNG (not OS-derived) so keys are bit-identical across runs.

**Recommendation: use the published Polyglot key set as static data** (~6 KB). Cheap, enables Polyglot opening-book reuse later (per roadmap), and avoids the historical bad-seed pitfall (Schaeffer's anecdote in CPW). When M4 lands, TT hash and book hash agree by construction.

---

## 6. Edge case taxonomy

The 6 canonical perft positions ([CPW Perft Results](https://www.chessprogramming.org/Perft_Results)) plus the [perfect-perft](https://www.chessprogramming.net/perfect-perft/) mini-positions form the test battery. Below: each well-known bug class paired with positions that expose it.

**1. EP on a horizontally pinned pawn.** White king and black rook on the same rank; white pawn between them on rank 5; black double-pushes adjacent. Both pawns vacate the rank simultaneously, exposing the king. Each pawn looks individually un-pinned — that's the trap. Tests: Position 3 (`8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - -`) and the dedicated mini-positions `3k4/3p4/8/K1P4r/8/8/8/8 b - - 0 1` and `8/8/4k3/8/2p5/8/B2P2K1/8 w - - 0 1`.

**2. EP target invalidation timing.** EP is legal *only* the ply immediately following a double push. Setting it at the wrong move type or failing to clear it produces phantom EP. Compounds with §5's hash subtlety.

**3. EP causing discovered / double check.** EP is the unique move that delivers double check from a single move (captured pawn's removal opens a discovery). Test: `8/8/1k6/2b5/2pP4/8/5K2/8 b - d3 0 1` (perfect-perft "EP Checks Opponent").

**4. Castling through / out of / into check.** King cannot castle while in check, through an attacked square, or into one. Squares the *rook* passes over may be attacked. Watch out: an enemy pawn on f3 attacks e2 and g2 even though no piece sits there — blocks white kingside castling. Tests: `r3k2r/8/3Q4/8/8/5q2/8/R3K2R b KQkq - 0 1`, Kiwipete variations.

**5. Rook captured on starting square removes that side's castling rights.** Engines that only update castling on king/rook *moves* miss this. Tests: Kiwipete exposes it at depth 4 via `bxa1`/`bxa1=Q` lines.

**6. Castling delivering check.** The rook moves to a check-giving square; engines that only check "did the moving piece (king) attack the enemy king" miss the rook's contribution. Tests: `5k2/8/8/8/8/8/8/4K2R w K - 0 1` (short), `3k4/8/8/8/8/8/8/R3K3 w Q - 0 1` (long).

**7. Promotion captures.** 4 promo choices × ability to capture = bug-prone combinatorics. Many engines forget under-promotions or generate phantom queens. Test: Position 4 (`r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1`).

**8. Promotion delivering check, including under-promotions.** Promote to knight to fork the king is a real engine-tested case. Tests: `4k3/1P6/8/8/8/8/K7/8 w - - 0 1`, `8/P1k5/K7/8/8/8/8/8 w - - 0 1` (under-promote+check), `2K2r2/4P3/8/8/8/8/8/3k4 w - - 0 1` (promote *from* check).

**9. Discovered check from a non-checking piece moving.** Position 5 (`rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8`) is the standard test.

**10. Stalemate vs. checkmate.** `no legal moves && in_check` = mate; `no legal moves && !in_check` = stalemate. Tests: `K1k5/8/P7/8/8/8/8/8 w - - 0 1`, `8/k1P5/8/1K6/8/8/8/8 w - - 0 1`. With legal-direct generation, "no legal moves" is `legal.len() == 0` — no special infrastructure needed.

**11. King fleeing along a slider's check ray.** See §3. Position 3 and Position 5 stress this.

### Coverage map for the canonical 6

- **Starting position**: baseline; all piece types.
- **Kiwipete** (`r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq -`): captures, both-side castling, EP available, **rook-capture-loses-castling at depth 4.**
- **Position 3** (`8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - -`): endgame; **EP-on-horizontally-pinned-pawn** is the headline; promotions and discovered checks too.
- **Position 4** (above): promotion captures, promo+check, asymmetric castling rights.
- **Position 5** (above): discovered checks, pinned pieces, promo availability.
- **Position 6** (`r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10`): middlegame, both developed, no castling, frequent pins. Good final regression.

Pass all six to depth 5+ with exact node counts matching Stockfish and the residual probability of a rule bug is very low. The perfect-perft mini-positions are useful as **isolated unit tests** — failures point at exactly which rule is broken. Recommend including both.

---

## 7. Performance baselines on Apple Silicon

### Order of magnitude

Concrete points of reference from prose:

- **Cozy-chess** (Rust movegen library): ~318 Mnps for perft 7 — but with PEXT bitboards (BMI2 on x86; absent on Apple Silicon).
- **Peter Ellis Jones legal-direct generator**: 106 Mnps single-threaded on then-current Intel; **Qperft** (tuned pseudo-legal): ~190 Mnps same hardware.
- **Stockfish on M4 Pro**: 28.5 Mnps in `bench` — but that includes search and NNUE, not raw movegen ([TalkChess M3/M4 thread](https://talkchess.com/viewtopic.php?t=84661)).
- **Maverick** (older engine) at ~25 Mnps perft on 2.2 GHz Sandy Bridge ([Is Perft Speed Important?](https://www.chessprogramming.net/is-perft-speed-important/)).

A competent magic-bitboard movegen on a modern desktop CPU sits in the **100–300 Mnps range single-threaded**. Apple's M-series P-cores are competitive with the best x86 cores at single-threaded integer work, so the same range should apply — with the caveat that we lack PEXT (a 1.5–2× factor on the sliding-piece path on x86 BMI2 hardware), so the upper end may be unreachable.

**Recommendation: target ≥100 Mnps single-threaded perft on the canonical 6 positions on M4 once magic bitboards are wired up.** Below 50 Mnps means something is structurally wrong (allocations per node, accidental quadratic loops). Above 200 Mnps would be excellent for a first cut.

### Common bottlenecks (prose-only sketch)

- **Attack-table cache misses.** Magic tables ~2 MB rooks + ~256 KB bishops. Larger than L1d (192 KB on M-series P-cores); stresses L2 (16 MB on M4). Sliding-piece gen is the inner loop; if it dominates profile, Kindergarten/"fancy magic" sharing shrinks tables. Sibling agent owns this; flag if profiling screams cache.
- **Mailbox writes** per make/unmake. Mandatory; eyeball in profiles.
- **Branch misprediction in evasion code.** Different path for ~5–15% of positions. `cold` annotations or function separation help. Premature for M1.
- **Move-list copies.** Returning 256 entries by value is ~1 KB stack churn. Not a real bottleneck.
- **NEON irrelevance for movegen.** Bitwise + popcount + tzcnt on single `u64`s. NEON helps NNUE (M9), not M1.
- **PGO matters.** `cargo-pgo` reportedly buys 10–20%. Defer to post-M3.

Apple-specific: strong unaligned load support — no `#[repr(align)]` needed on `Position`. Use `samply` for sampling (nicer flamegraphs per prose); Instruments → Time Profiler as backup; `criterion` for microbench.

---

## Summary of recommendations

| Area | Recommendation |
|---|---|
| Square indexing | LERF (`a1=0..h8=63`). Pretty-printer day one. |
| Piece bitboards | 6 type + 2 color occupancy + mailbox + cached king squares. |
| Move encoding | 16-bit (from/to/4-bit flags), capt/promo/special1/special0 nibble. Wider only as `ScoredMove` for ordering. |
| Move list | Inline `[Move; 256]` + `u8` length. |
| Generation strategy | Legal-direct, mask-based (checkers + pinned + capture-mask + push-mask). |
| Check evasion | Specialized path; double-check → king moves only; opponent attacks computed on `occupancy ^ king_bb`. |
| Undo | ~16 B: captured piece, prior castling, prior EP, prior halfmove, prior Zobrist. |
| NNUE hook | Discrete make/unmake function calls — done. Current `Move`/`Undo` carry enough. |
| Zobrist | 781-key Polyglot table. EP file hashed only when capture is *pseudo-legally* possible. |
| Perft battery | Canonical 6 to depth 5+ for headline; perfect-perft minis as targeted unit tests. Stockfish oracle. |
| Performance target | ≥100 Mnps single-threaded perft on M4; ≥200 would be excellent. |

### Open uncertainties

- **Polyglot vs. legal-only EP hashing**: prose recommends Polyglot; legal-only refinement may matter at search time. Decide post-M4 with profile data.
- **4 vs. 16 Zobrist castling keys**: doesn't matter for correctness; pick whichever reads more naturally in Rust.
- **Whether to cache pinned/checker bitboards in `Position`**: skip for M1; revisit if profiling shows repeated computation. The risk is invalidation bugs.
- **Exact Apple Silicon perft ceiling**: prose puts us in 100–300 Mnps; we won't know our number until magic bitboards are wired. Treat 100 Mnps as a floor that proves we're not doing something dumb, not as a target.
