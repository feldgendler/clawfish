# M5.E — Qsearch correctness (research)

Research brief for M5.E — two narrow qsearch corrections: single-reply extension when the legal-move filter would otherwise return stand-pat (closes the M3.D horizon hole on legally-forced quiet moves) and stalemate-conditional rook/bishop under-promotion (after a queen-promo with zero legal replies + not in check, also search RookPromo / BishopPromo). Sized to land before M5.F so qsearch-in-TT does not memoize the holes.

## Headline calls

1. **Single-reply extension in qsearch is novel.** The literature recognises the false-stand-pat problem (TalkChess t=76266) but the consensus answer is "live with it / search deeper in main search," not "extend qsearch." CPW's "One Reply Extensions" technique lives in negamax (typically check-extension), not qsearch. M5.E's qsearch-side single-reply extension is correctness-motivated and bespoke; no published Elo number to anchor against.

2. **The "exactly one" gate is the only termination-safe form.** "Exactly one legal move and the filter would have rejected it" is the safe shape. "Any time quiets are the only replies" reintroduces qsearch's termination problem (HGM: "unlike captures there is no guarantee you will ever run out of them"). Lock the gate at exactly-one.

3. **Stalemate-conditional rook/bishop under-promo IS standard.** CPW Promotions: *"In quiescence search most programs only consider queening. Bishop and rook promotions may only be generated if the queen promotion returns an explicit stalemate score."* That is precisely M5.E's commitment. Knight-promo's fork-tactic motivation is a different (rare) case and stays out of scope as the roadmap row commits.

4. **TT memoization ordering rationale holds.** A qsearch stand-pat returned at a node where the actual forced reply loses material would be stored as `Bound::Upper` by M5.F and propagate the inflated bound. Same for the queen-promo-stalemate case: the parent's score is wrong if we don't try rook/bishop. Fixing semantics first is the right order.

5. **Plan SPRT as a no-regression gate, not a positive-Elo gate.** Both corner cases are rare; expected combined Elo per the roadmap is `<10`, plausibly `<3`. Use `elo0=-5, elo1=5` bounds; accept either H1 (positive) or no-regression. Correctness is the gate; SPRT is documentation.

## 1. Single-reply extension

### Definition for M5.E

When `negamax::qsearch` reaches a node that is **not in check** and the move filter (`Capture | EnPassant | QueenPromo | QueenPromoCapture`) yields zero moves while `generate_moves` produces **exactly one legal move** (necessarily a quiet), recurse on that one move at `qsearch`'s normal depth and `ply + 1`. Otherwise return stand-pat (current M3.D behaviour preserved).

### Pitfalls

| Pitfall | Mitigation |
|---|---|
| Infinite chain through repeated forced quiets | Natural ply ceiling: qsearch already runs at the negamax horizon and inherits MAX_PLY; the extension is not itself a loop. |
| Stand-pat interaction (mixing single-quiet recurse with stand-pat at the same node) | Gate firmly: extension fires **only** when filter is empty, not in check, and `legal_moves.len() == 1`. |
| History accounting | Convention: no history bonus/malus on the forced reply (this is correctness, not a tactical scoring event). |
| Draws on the forced reply | The recursive `qsearch` call inherits the existing repetition / 50-move logic — same as any other qsearch recursion (which today is "qsearch does NOT consult repetition / 50-move", per `architecture.md`). Acceptable for v1. |

### Out of scope

- Two-or-more-legal-quiets-only case. Stays at stand-pat.
- Quiet checking moves not detected (would require `gives_check(pos, mv)` infrastructure that does not exist).
- Recursive single-reply chains. Fires at most one extra ply per qsearch node; if the recursion lands on another single-reply position, that frame fires its own extension independently. No special chain handling.

## 2. Stalemate-conditional rook/bishop under-promotion

### Definition for M5.E

When the qsearch move loop has just searched a `QueenPromo` or `QueenPromoCapture` move `mv` and the recursive qsearch on the post-promo position returned a **stalemate score (0)** AND the post-promo position was not in check, also search the matching `RookPromo` (or `RookPromoCapture`) and `BishopPromo` (or `BishopPromoCapture`) variants of the same `(from, to)` pair. Knight-promo deliberately not searched.

### Why rook/bishop, not knight

- Rook + bishop attack squares are subsets of the queen's. The reason rook/bishop avoids stalemate is the *gap* between their coverage and the queen's: a square the queen attacks (preventing a king move) but the rook/bishop does not now becomes a legal escape, breaking the stalemate.
- Knight is motivated separately — fork tactics, mate patterns. The stalemate-avoidance rationale does not apply to knights, and the fork motivation is rare enough to remain out of scope.

### Important detail: bishop-promo doesn't always work

A bishop's coverage gap from the queen is different from a rook's. Some queen-stalemate positions are also bishop-stalemate. The fix tries each independently — make the rook-promo, recurse, observe the result; same for bishop. No assumption that "if rook works, bishop works."

### Implementation shape (sketch, not a commitment)

Inside qsearch's move loop, when the just-searched `mv` is a queen-promo and the recursion returned `score == 0` *and* the post-make position was not in check (i.e., genuine stalemate, not draw-by-something-else), construct synthetic `RookPromo` / `BishopPromo` (or `*PromoCapture`) variants and search them with the same `(child_alpha, child_beta)`. Each contributes its score to `best`/`alpha`/`beta` like any other recursion. Beta cutoff propagates normally.

### Pitfalls

| Pitfall | Mitigation |
|---|---|
| "Explicit stalemate" misdetected | Define stalemate as `legal_moves.is_empty() && !in_check(child)` after the queen-promo make. Score 0 alone is not enough — repetition / 50-move also return 0 and don't motivate under-promo. Either re-detect stalemate post-make, or short-circuit the trigger by checking the move's terminal condition more precisely. |
| Promotion variant construction | The `Move` 16-bit encoding has a flag nibble; flipping `QueenPromo → RookPromo` (and `*PromoCapture` analogously) is a flag-bit edit, not a re-generation. Test the construction helper directly. |
| Move legality | The synthetic rook/bishop promo at the same (from, to) is legal iff the queen-promo at the same (from, to) is legal. Same legality check applies; no re-validation needed. |
| Over-counting nodes | Each extra promotion variant is one extra recursion; bounded by 2 per stalemate-trigger. Bench impact: tiny (corner case). |

### Out of scope

- Knight-promo (fork-tactic motivation, separate).
- Promotion variants when the queen-promo did not stalemate.
- Loop-over-all-four-promotions in qsearch unconditionally (more general, more node-bloat, deferred).

## 3. M5.E ordering vs M5.F

M5.F (qsearch-in-TT) memoises qsearch results into the TT. Two failure modes if M5.E lands after:

- **Forced-quiet stand-pat case.** A qsearch node where the filter returns empty but the actual forced reply loses a piece returns stand-pat (overestimate). M5.F would store this as `Bound::Upper` (no move beat stand-pat). A future probe at the same Zobrist + depth reads `tt_score = stand_pat`, sees `bound = Upper`, sees `tt_score ≤ alpha` and returns the inflated score immediately — propagating the hole.
- **Queen-promo-stalemate case.** Without M5.E, the parent's TT entry stores a 0 (stalemate) for what could be a winning rook-promo line. Future probes propagate the wrong score.

In both cases, fixing qsearch semantics before adding qsearch-in-TT is correct. The M5.E → M5.F ordering is load-bearing.

## 4. SPRT planning

- Both fixes are corner-case-frequency events: queen-promo-stalemate is endgame-rare; positions with exactly-one-legal-quiet-and-no-captures-and-no-queen-promos are also rare (typically: side has all pieces pinned or blocked, only a king move available).
- Roadmap commits `<10 Elo combined`. Realistic estimate: combined `<5 Elo`, possibly `<3 Elo`. Mixed-TC SPRT may be inconclusive.
- Recommended SPRT shape: `elo0=-5, elo1=5` (no-regression framing); cap at 400 games; accept if H1 fires OR if the post-hoc CI lower bound is `> -10 Elo`. Correctness, not Elo, is the gate.
- Alternative validation: targeted unit tests on hand-crafted positions where each correction fires and changes the score in the expected direction. These are more informative than SPRT for corner-case fixes.

## 5. Sources

- [Quiescence Search — CPW](https://www.chessprogramming.org/Quiescence_Search)
- [Promotions — CPW](https://www.chessprogramming.org/Promotions) — explicit "Bishop and rook promotions … only if queen promotion returns an explicit stalemate score" rule.
- [One Reply Extensions — CPW](https://www.chessprogramming.org/One_Reply_Extensions) — main-search technique; informs the "exactly one" gate decision.
- [Stalemate — CPW](https://www.chessprogramming.org/Stalemate)
- [Non-quiet position after quiescence — TalkChess t=76266](https://talkchess.com/viewtopic.php?t=76266) — community context on the false-stand-pat problem.
- [Slow Chess implementation notes — 3DKingdoms](https://3dkingdoms.com/chess/implementation.htm) — closest precedent (forced-quiet handling via main-search depth extension, not qsearch).
