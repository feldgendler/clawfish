# ADR-0028 — Qsearch participation in the transposition table

**Status:** Accepted (lands with M5.F).

## Context

ADR-0018 §6 deferred qsearch participation in the TT to M5: M4.A's negamax probes/stores while qsearch did not. The acknowledged miss: a position visited as a qsearch *interior* node on Branch A and reached as a horizon node on Branch B incurs a redundant qsearch on B. Cited Elo gap: +5–15 Elo per CPW survey; closing it is M5.F.

M5.E qsearch correctness landed first (2026-05-08; ADR-0027) so the TT does not memoize qsearch holes. With single-reply extension, true-stalemate detection, stalemate-conditional rook/bishop under-promo, and MAX_PLY ceiling guard in place, qsearch results are now safe to memoize.

Prior-art survey: `docs/research/m5-qsearch-in-tt.md` (literature ranges from Crafty's 0 Elo "wash" through MartinBryant's +44 Elo at 1280 games). Plan and test surface: `docs/plans/m5.f.md`.

## Decision

### 1. Full probe-and-store (not probe-only)

M5.F implements the full mainstream pattern: TT probe at qsearch entry; TT store at every real-result return point. The "probe-but-don't-store" intermediate noted in ADR-0018 §6 was rejected at planning time — research §11 found that the store side is the primary source of benefit for interior qsearch nodes (the main miss ADR-0018 documented), while probe-only catches only pre-existing negamax entries.

### 2. depth = 0 for all qsearch entries

Every qsearch TT entry stores `depth = 0`. The depth field acts as a tier marker (qsearch vs negamax), not a measure of search effort within qsearch.

Negamax entries always store with `depth ≥ 1`. The negamax probe rule `entry.depth as u32 >= depth` (where `depth ≥ 1`) naturally rejects depth=0 qsearch entries from causing negamax cutoffs — exactly correct, since a qsearch score with restricted moves cannot soundly cut a full-width search. The qsearch probe applies no depth comparison: any TT entry's depth (0 or ≥ 1) is at least as deep as qsearch's notional depth.

Under depth-preferred replacement (`old.depth <= data.depth`):
- qsearch over qsearch (0 ≤ 0): replace ✓
- negamax over qsearch (0 ≤ 5): replace ✓ (negamax always wins same-gen)
- qsearch over negamax (5 ≤ 0): keep old ✓ (qsearch never replaces negamax same-gen)

### 3. Empty-slot discriminator: `key == 0`

Storing depth=0 entries broke the prior multi-field `is_empty()` test (`key == 0 && depth == 0 && age_and_bound == 0 && best_move == 0`) — a stored qsearch entry with all-zero fields except score and best_move would alias with default. The forward-planning comment at `src/tt.rs:222` anticipated this.

M5.F changes `is_empty()` to:
```rust
pub fn is_empty(&self) -> bool { self.key == 0 }
```

`store()`'s precondition assertion changes from `data.depth >= 1` to `key != 0`. The Polyglot Zobrist key of any reachable position is non-zero with probability `1 − 2⁻⁶⁴`; the assertion is defense-in-depth. Release-build behavior on a hypothetical `key == 0` collision is benign: `store(0, ...)` writes `key=0`, subsequent `probe(0)` sees `is_empty() == true` and returns `None`. Net: silent TT skip for that one position; no soundness violation.

### 4. Bound semantics: no Exact in non-terminal qsearch

Per Stockfish commit 45e5e65 (Nov 2021): qsearch's restricted move set (captures + EP + queen-promo, plus in-check evasions) does not produce a true minimax value over all legal moves. Calling `Exact` on a non-terminal qsearch result overstates precision and can short-circuit a future negamax PV-node probe with an unsound score.

M5.F's `qsearch_tt_bound_for_completed_node(best, beta) -> TtBound` returns:
- `Lower` if `best >= beta` (fail-high; cutoff fired)
- `Upper` otherwise (everything else, including the would-be-Exact zone where `original_alpha < best < beta`)

The two **terminal exceptions** are FIDE-definite regardless of move-set restriction and store as `Exact`:
- True stalemate at qsearch (`!in_check && ml.is_empty()`) returns `0` with bound `Exact`.
- Mate at horizon (`in_check && moves_vec.is_empty()`) returns `-(MATE - ply)` with bound `Exact`.

### 5. Probe-time Exact cutoffs are sound

The probe accepts an `Exact` cutoff because the only sources of stored Exact entries are: (1) negamax-tier entries (depth ≥ 1) — full-width search results, sound to use; (2) qsearch terminal Exact entries — FIDE-definite. The asymmetry (no-Exact-in-store, Exact-IS-honored-in-probe) is principled: the store discipline prevents bad Exacts from existing.

### 6. PV suppression NOT applied in qsearch

`is_pv` is not threaded into qsearch. The "short PV" motivation that drives PV-node cutoff suppression in negamax (ADR-0018 §11) does not apply: qsearch does not contribute to the displayed triangular PV. Applying full probe-and-cutoff at all qsearch nodes maximises the efficiency gain.

### 7. TT-move ordering: filter-gated via moves_vec membership

When the qsearch probe extracts a `tt_move` for ordering, no explicit qsearch-filter check is needed at the probe site. The ordering step (after the existing MVV-LVA sort) attempts to promote `tt_move` to index 0 only if it appears in `moves_vec`. The not-in-check arm constructs `moves_vec` from `qsearch_move_filter` (captures / EP / queen-promo); a quiet TT move does not appear there and is silently rejected by the membership scan. The in-check arm constructs `moves_vec` from all legal evasions; any legal TT move passes.

This addresses the "long-chain problem" Andrew Grant flagged on TalkChess t=69629: a quiet TT move in a not-in-check qsearch frame would otherwise enter the qsearch loop and could cascade into a chain of quiet TT moves with no captures to terminate. Filter-gated ordering prevents this by construction without a separate gate.

### 8. MAX_PLY ceiling guard skips TT store

The M5.E #4 MAX_PLY ceiling guard returns `evaluate(pos)` without recursing. M5.F deliberately does NOT store at this path — the return is an artificial truncation, not a search-quality bound. Storing it would pollute the TT with a score that doesn't reflect a genuine stand-pat decision.

### 9. Abort skips TT store

Existing pattern carried forward: when the qsearch frame aborts mid-recursion, the abort-propagation path returns 0 without storing. The `qsearch_store_and_return` helper checks `!self.aborted` before writing.

### 10. GHI: maintain "live with it" stance

ADR-0018 §10 accepts the GHI imprecision in the negamax TT; ADR-0027 §7 preserved the M3.D deliberate skip of repetition / 50-move detection in qsearch. M5.F preserves both. Qsearch entries (depth=0) are excluded from negamax cutoffs by the `entry.depth >= depth` test, bounding GHI exposure to within-qsearch subtrees. No new mitigation in M5.F.

### 11. Mate-score discipline: reuse `score_to_tt` / `score_from_tt` unchanged

The ply-relative mate adjustment from ADR-0018 §5 applies uniformly to qsearch results. No qsearch-specific mate logic. The in-check mate at horizon (`-(MATE - ply)`) and any mate-bounded score from a single-reply extension recursion both round-trip through the existing helpers.

### 12. Move-loop best_move tracking with first-cut-wins

Path F (completed move loop) stores `best_move = cutoff_move` when bound is Lower. The qsearch move loop introduces a `cutoff_move: Option<Move>` accumulator. The M5.E #3 under-promo inner loop sets `cutoff_move = Some(under_mv)` unconditionally on its own break. The outer cutoff guards with `if cutoff_move.is_none() { cutoff_move = Some(mv); }` so the under-promo's record is preserved when both loops cut at the same `alpha >= beta` value. The "first-cut-wins" rule ensures the actually-cutting move is memoized, not the queen-promo whose stalemate child triggered the under-promo synthesis.

## Per-path store table

| Path | Trigger | score | bound | best_move |
|---|---|---|---|---|
| A | `!in_chk && sp >= beta` | `sp` | Lower | 0 |
| B | `!in_chk && moves_vec.is_empty() && ml.is_empty()` | 0 | Exact | 0 |
| C | `!in_chk && moves_vec.is_empty() && ml.len() == 1` | recurse(score) | helper(score, β) | only_mv.bits() |
| D | `in_chk && moves_vec.is_empty()` | `-(MATE-ply)` | Exact | 0 |
| E | `!in_chk && moves_vec.is_empty() && ml.len() ≥ 2` | `stand_pat` | Upper | 0 |
| F | move loop completes | `best` | helper(best, β) | Lower→cutoff_move else 0 |
| X (no store) | MAX_PLY ceiling guard | `evaluate(pos)` | — | — |
| Y (no store) | abort during recursion | 0 sentinel | — | — |

## Out of scope

- **Probe-only intermediate**: rejected; full probe-and-store is the mainstream pattern.
- **PVS / `is_pv` threading into qsearch**: rejected; "short PV" doesn't apply to qsearch.
- **Qsearch repetition / 50-move detection**: ADR-0027 §7 preserved skip; preserved here.
- **Per-entry static-eval cache**: separate engine subsystem; out of scope.
- **Bucketed TT**: direct-mapped depth-preferred unchanged.
- **Lockless XOR trick (M11)**: deferred.

## Consequences

**Positive:**
- Closes the +5–15 Elo gap deferred from M4.A (ADR-0018 §6).
- TT pollution from depth=0 entries is naturally self-limiting (negamax always wins same-gen replacement).
- Bound classification helper isolates the no-Exact rule for mutation testing.
- The discriminator change to `key == 0` is forward-planned (`src/tt.rs:222` comment) and survives all existing TT tests verbatim.

**Negative:**
- Qsearch frames now incur a TT probe + store (~one cache-resident lookup per frame). Probe overhead at fast TC may partially offset savings.
- GHI exposure marginally increases (qsearch entries at low halfmove clocks may be probed at high halfmove clocks). Bounded to within-qsearch subtrees by depth=0 negamax-probe rejection.
- The "no Exact in non-terminal qsearch" rule is non-obvious; future maintainers must understand the asymmetry with the probe-time Exact-IS-honored rule.
