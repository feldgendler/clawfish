# ADR-0027 — Qsearch correctness: single-reply extension, true-stalemate, stalemate-conditional under-promo, MAX_PLY guard

**Status:** Accepted (lands with M5.E).

## Context

M5.E adds two narrow corrections to `AlphaBetaMover::qsearch` that close the M3.D horizon hole on legally-forced quiet replies and the M3.D queen-only-promo filter gap. M5.F (qsearch-in-TT) follows immediately; M5.E must land first so the TT does not memoize qsearch holes (a qsearch-stand-pat overestimate stored as `Bound::Upper` would propagate the inflated bound).

This phase layers on top of:

- M3.D qsearch (filter accepts `Capture | EnPassant | QueenPromo | QueenPromoCapture`; in-check arm searches all evasions; not-in-check empty-filter returns stand-pat per the false-stalemate guard).
- ADR-0014 evaluation (`evaluate(pos)` STM-relative).
- ADR-0023 NMP, ADR-0024 RFP, ADR-0025 LMR, ADR-0026 FFP — all unchanged by M5.E.

Prior-art survey: `docs/research/m5-qsearch-correctness.md`.
Plan and test surface: `docs/plans/m5.e.md`.

## Decision

### 1. Single-reply extension (M5.E #1)

When qsearch's terminal-handler reaches the `!in_chk && moves_vec.is_empty()` arm AND `ml.len() == 1` (the unique legal move), recurse on the unique move at qsearch with `ply + 1`. Return the recursed-and-negated child score. Closes the M3.D horizon hole on legally-forced quiet replies — pre-M5.E, qsearch returned stand-pat unconditionally on this branch, which over-states the position when the forced move is materially decisive (e.g. a forced king walk that loses a piece on the next ply).

The unique move is necessarily a non-promo quiet (`Quiet | DoublePush | KingCastle | QueenCastle`) by movegen invariant: legal-direct movegen (`src/movegen/pawn.rs:50-54, 88-92`) emits all four promotion variants whenever a promotion is geometrically legal, so an under-promo can never appear alone. Pinned by `qsearch_single_reply_under_promo_uniqueness_is_structurally_unreachable`. No `is_quiet` defense-in-depth gate is needed at the call site — the structural argument suffices.

**Termination.** The recursion is naturally bounded by §4's MAX_PLY ceiling guard (`ply >= MAX_PLY - 1 && !in_check` returns stand-pat immediately). Recursive single-reply chains fire each frame's M5.E #1 independently with no special chain handling.

**Per-research-§4 prior-art note**: the "One Reply Extensions" CPW technique is a main-search technique, typically scoped to check-evasion fractional extensions in negamax. Applying single-reply extension *inside qsearch* for non-check forced quiet moves is novel relative to documented chess-engine practice. The "exactly one" gate is the only termination-safe form: "any-time-quiets-are-the-only-replies" reintroduces qsearch's termination problem (HGM: "no guarantee you ever run out of quiets").

### 2. True-stalemate detection (M5.E #2)

When qsearch's terminal-handler reaches the `!in_chk && moves_vec.is_empty()` arm AND `ml.is_empty()` (zero legal moves), return `0` (FIDE 9.2 stalemate draw). Pre-M5.E this conflated with the false-stalemate guard and returned stand-pat, an approximation acknowledged in M3.D's plan as "the empty-after-capture-filter does NOT mean stalemate" but accepted because true stalemate at qsearch is rare. M5.E corrects the score; the change is structurally tied to `ml.is_empty()` and is mechanically free once we test legal-move emptiness for #1.

### 3. Stalemate-conditional rook/bishop under-promotion (M5.E #3)

When qsearch's move loop is about to recurse into a `QueenPromo | QueenPromoCapture` move whose post-make child position is **stalemate** (`legal_moves.is_empty() && !in_check(child)`), additionally search the `RookPromo | RookPromoCapture` and `BishopPromo | BishopPromoCapture` variants of the same `(from, to)`. Knight-promo deliberately not searched (fork-tactic motivation, separate; out of scope).

This is a textbook formulation per CPW Promotions: *"Bishop and rook promotions may only be generated if the queen promotion returns an explicit stalemate score."* The narrow condition (queen-promo causes stalemate, side did not just want to draw) lets the engine find the rare position where rook-promo or bishop-promo avoids the stalemate by leaving the opponent's king an escape square not covered by the under-promotion's reduced piece geometry.

#### Predicate placement: BEFORE the recursion

The `queen_promo_stalemates` predicate is captured **before** the qsearch recursion (after `make_move`, before the recursive `qsearch(...)` call), not after. The detection scans the post-make position (child's POV) — at that point `pos` is in the post-make state, so movegen + `in_check(pos)` correctly enumerate the opponent's options. Capturing the predicate before recursion (rather than between recursion and unmake) keeps `history.pop()` and `unmake_move` adjacent — M3.D's abort-discipline pairing ("the push/pop above is balanced even on abort because the abort check runs AFTER both `history.pop()` and `unmake_move`") stays load-bearing.

#### Synthesized move legality

A rook-promo at the same (from, to) as a legal queen-promo is itself legal-by-construction: the only differentiation between the four promotion variants is the choice of promoted piece, and the legality of the move (geometric path, check resolution, pin) is identical. Movegen would have emitted all four promo variants from the same (from, to). No re-validation; `make_move` accepts the synthesized variant.

#### Helper `stalemate_avoiding_under_promos`

```rust
pub(crate) fn stalemate_avoiding_under_promos(mv: Move) -> [Option<Move>; 2] {
    use crate::mov::MoveFlag::*;
    let (rook_flag, bishop_flag) = match mv.flag() {
        QueenPromo => (RookPromo, BishopPromo),
        QueenPromoCapture => (RookPromoCapture, BishopPromoCapture),
        _ => return [None, None],
    };
    let from = mv.from_square();
    let to = mv.to_square();
    [
        Some(Move::new(from, to, rook_flag)),
        Some(Move::new(from, to, bishop_flag)),
    ]
}
```

`[Option<Move>; 2]` (rather than `(Move, Move)`) carries the gate-fail semantics into the type: a misuse on a non-queen-promo input returns `[None, None]` and the caller's `for ... if let Some` iterator naturally skips both. Mirrors the M5.D `ffp_pruned_bound -> Option<i32>` precedent: helper-level domain guard returns the empty value of the contribution type rather than panicking.

### 4. MAX_PLY ceiling guard (!in_check arm only)

At qsearch entry, AFTER step 1 (nodes increment + cancellation poll) and BEFORE step 2 (mate-distance pruning):

```rust
if ply >= MAX_PLY as u32 - 1 && !in_check(pos) {
    return evaluate(pos);
}
```

Defense-in-depth against pathological forced-quiet chains under M5.E #1. Pre-M5.E qsearch terminated naturally as captures ran out; #1 introduces all-quiet recursion. The guard returns `evaluate(pos)` (the side-to-move-relative stand-pat) without recursing.

**Only the `!in_check` arm is guarded.** At in-check + ply ceiling the existing evasion arm runs as before. Returning a fabricated mate-in-ply score (`-(MATE - ply)`) would propagate a false mate through `score_to_uci`, `score_to_tt`, and mate-distance pruning. The in-check ceiling case is structurally near-impossible in practice (a chain of forced check evasions tall enough to hit MAX_PLY does not occur in real positions), so the asymmetric guard is principled.

**Stack-budget check.** A qsearch frame at the deepest reach holds a `MoveList` (~1.5 KiB) and `moves_vec: Vec<Move>` (~256 B). The `child_ml` allocation in §3's predicate-block lives only inside the `&& { ... }` expression, ending before recursion — its lifetime is the predicate window, not the recursion frame. 63 qsearch frames stacked atop ≤ 63 negamax frames sum to ~200 KiB peak — well within the 8 MiB macOS default thread stack.

### 5. Per-quiet move-loop reshape: under-promo BEFORE cutoff

Plan §4.3's reshape moves the per-move beta cutoff (`if alpha >= beta { break; }`) **out** of the existing `if score > best { ... }` block into a sibling block (10c), so the under-promo loop (10b) runs between alpha-tightening (10a) and the cutoff (10c). Behavior-preserving on non-queen-promo moves: alpha can only be raised inside the `score > best` arm, so the outer cutoff sees the same alpha as the inner one would have. Existing M3.D capture-cutoff fixtures continue to fire identically.

The reshape is load-bearing for M5.E #3: a queen-promo's stalemate score (= 0) raising alpha to 0 = beta would otherwise trigger `alpha >= beta` and suppress the under-promo recursions before they run. With under-promo BEFORE cutoff, the rook/bishop variants get to contribute their scores; the cutoff fires once after all moves contribute.

A mid-under-promo cutoff (`if alpha >= beta { break; }` inside the under-promo loop) skips remaining variants if a rook/bishop promo's score already cut the node; the outer cutoff is then redundant but harmless.

### 6. TT-store policy

Unchanged. Qsearch does not write to the TT in M3.D, and M5.E preserves that. M5.F is the next phase that adds qsearch-in-TT; landing M5.E first ensures that phase memoizes correct qsearch values, not horizon holes.

### 7. Repetition / 50-move detection in qsearch

Unchanged. M3.D's deliberate skip ("Qsearch does NOT consult repetition / 50-move helpers") is preserved. The M5.E #1 forced-quiet recursion inherits the same skip — acceptable for v1.

### 8. SPRT framing — no-regression, not positive-Elo

Both M5.E corrections are corner-case-frequency events:
- Queen-promo-stalemate is endgame-rare.
- Positions where the qsearch filter empties at not-in-check with exactly one legal quiet are also rare (typically: side has all pieces blocked or pinned, only a king move legal).

The roadmap predicts `<10 Elo combined`. Realistic estimate: combined `<5 Elo`, possibly `<3 Elo`. Mixed-TC SPRT may be inconclusive on positive signal. M5.E SPRT framing is **no-regression**: H0 elo0=-5, H1 elo1=5; accept if H1 fires OR if the post-hoc CI lower bound is `> -10 Elo` at a 400-game cap. Correctness, not Elo, is the gate. These are known holes that must land before M5.F regardless.

## Out of scope (deferred)

- Knight-promo stalemate trigger (fork-tactic motivation, separate).
- Loop-over-all-four-promotions in qsearch unconditionally (broader; bench noise).
- Multi-reply extension (any-time-quiets-are-the-only-replies — termination unsafe).
- Quiet checking moves in qsearch (requires `gives_check(pos, mv)` infrastructure).
- Recursive single-reply chain ceiling (relies on MAX_PLY natural bound).
- Repetition / 50-move detection in qsearch (M3.D deliberate skip preserved).
- Qsearch-in-TT (M5.F next phase).

## Open SPRT-tunable parameters

Post-M5.E campaigns:

1. **Knight-promo activation** — extend `stalemate_avoiding_under_promos` to include `KnightPromo`, gated by post-promo position not being stalemate (different trigger; fork-tactics-motivated). Separate phase.
2. **Single-reply extension recursion ceiling** — currently MAX_PLY-bounded; could be tightened to e.g. ply + 4 to bound chain length explicitly.
3. **Quiet-checking-move qsearch inclusion** — requires `gives_check(pos, mv)` infrastructure.
4. **Multi-reply extension cap** — searching all legal moves when `ml.len() <= K` for small K; needs careful termination.

The v1 commitments above are conservative and chosen for **clean correctness + clean SPRT attribution**, not for proven optimality.

## Consequences

**Positive:**

- Closes the M3.D horizon holes on legally-forced quiet replies and queen-promo stalemates.
- Sets up M5.F (qsearch-in-TT) to memoize correct qsearch values.
- Composes naturally with M5.A/M5.B/M5.C/M5.D — M5.E touches qsearch only; the negamax body and M5.A-D cuts are unchanged.
- Default-depth bench unchanged from M5.D-v2 (`bench: 1466436 nodes <NPS> nps`) — M5.E corner cases don't fire on the 16-position middlegame bench corpus, which is plausible given the rarity of the targeted positions.

**Negative:**

- Search-tree shape can change at corner-case positions; SPRT signal is small (`<5 Elo`) and may be inconclusive on positive signal. Run as no-regression SPRT.
- New per-move movegen call inside qsearch's move loop on queen-promo moves (cheap; queen-promos are themselves rare).

## Test surface (lands with M5.E)

- **Helper tests** (5 tests): `stalemate_avoiding_under_promos` per-flag behavior (rook+bishop returned for queen-promo / queen-promo-capture; empty for non-queen-promo; from/to preserved; never includes Knight or Queen).
- **Single-reply / true-stalemate / MAX_PLY** (10 tests): single-reply fires (with anti-vacuous sister), recursion-propagates-score, true-stalemate returns 0, mate-in-ply preserved, false-stalemate guard preserved, MAX_PLY guard at ceiling (!in_check returns stand-pat; in-check arm runs evasions). Includes the structural-unreachability test for under-promo single-reply.
- **Under-promo behavior** (7 tests): fires when queen-promo stalemates (with anti-vacuous: doesn't fire when checkmates / leaves replies / non-queen-promo); finds winning rook-promo (with tightened `score > stand_pat + 100` mutation-killing assertion); respects existing alpha-beta cutoff (firings == 2; score < alpha; under-promos contribute fail-soft); synthetic moves preserve from/to.
- **Compile-time invariant**: none new. M5.E does not add new constants.
- **Mutation campaign**: per-unit `cargo mutants --in-diff` run as part of pre-review mechanical checks; survivor triage under "Triaging a single survivor" precedent.
