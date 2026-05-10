# ADR-0030 — Staged movegen (architecture)

**Status:** Accepted (lands with M5.H1).

## Context

Today's negamax move loop conflates ordering (`order_moves`'s monolithic single-key sort plus TT promotion) with iteration (`for mv in moves_vec`). The cutoff-skip economy that staged-movegen offers — never generating quiets when an earlier stage causes a beta cutoff — requires per-stage generation, but per-stage generation changes history-score timing (research §15.6 / §16.4) and therefore node counts. The literature splits this into refactor-first (architecture without behaviour change) and lazy-second (cutoff economy, SPRT-gated).

Plan and test surface: `docs/plans/m5.h1.md`. Prior-art research: `docs/research/m5-staged-movegen.md`.

M5.H1 is the architectural refactor. M5.H2 (separate future unit) will switch to lazy generation.

## Decision

### 1. Stage taxonomy

Five stages: **TT → captures (MVV-LVA-desc) → killer 0 → killer 1 → quiets (history-desc)**. Conforms to the CPW Move Ordering / TalkChess t=76835 mainstream pattern. No countermove stage at H1 (forward-compat slot deferred to M6+ per `CLAUDE.md` "Don't design for hypothetical future requirements"). No SEE split. No bad-captures-after-quiets refinement.

### 2. Eager generation at H1

`MoveStager::new` calls `generate_moves` once at construction and partitions into per-stage Vecs. Captures and quiets are sorted up-front. The yielded sequence is byte-equivalent to today's `order_moves` output, by construction. **Bench parity (`bench: 1147614 nodes` exact, identical to M5.G) is the empirical no-regression gate.**

H2 will defer per-stage generation: TT validation typed (no list scan); captures generated on first entry to Stage::Captures; killers validated typed; quiets generated on first entry to Stage::Quiets. This changes history-score timing (the quiet sort happens after captures and killers have searched, with possibly fresher history) and node counts. SPRT-gated. Expected +5–15 Elo per literature.

### 3. Legality discipline

**Membership-scan**, not typed validation:

- TT: `extract_move_by_bits(&mut all_legal, tt_move) -> Option<Move>` — looks up the bits in the freshly-generated legal list; absent → no TT yield. Mirrors today's `order_moves`'s `iter().position(|m| m.bits() == tt_move)` at `src/search.rs:2906`.
- Killers: `extract_move_by_eq(&mut quiets, killer_move) -> Option<Move>` — full Move equality (flag bits included) on the quiets-only Vec. Defensive: a killer slot containing a non-quiet (impossible per ADR-0019) is never found in the quiets Vec, so it is naturally rejected.

H2 may switch to a typed `is_pseudo_legal`-style validator if lazy generation needs the typed path; H1 is membership-only.

### 4. Stage state machine encoding

Idiomatic Rust loop-match per research §14.2: a `Stage` enum advanced inside `next()`'s `loop { match self.stage { ... } }`. Pass-through stages (the `Captures`, `Killer0`, `Killer1` arms when their Vec/Option is exhausted) immediately fall through to the next stage in the same `next()` call — no spurious `None` yield.

`MoveStager` does **not** implement `Iterator`. Reason: `len()` is the pre-iteration count and does NOT decrement (callers must not rely on iterator-like remaining-count semantics). The `&self`-receiver `peek()` is type-enforced idempotent, load-bearing for the M5.G SE block's double-call (gate predicate + verification call argument).

### 5. SE excluded-move handling

Caller-side `if Some(mv) == excluded_move { continue; }` in the move loop — matches M5.G's existing pattern. No stager API change. Plan §7 + research §12.2 Option B.

### 6. Position-counter pattern

The move-loop's `i` becomes `cur_i` from an explicit counter:

```rust
let mut i: usize = 0;
while let Some(mv) = stager.next() {
    let cur_i = i;
    i += 1;
    if Some(mv) == excluded_move { continue; }
    // ... body uses cur_i where it previously used i ...
}
```

Preserves M5.G's `enumerate()` semantics: `i` is the position in the post-`order_moves` ordering (== position in the stager's yield order), NOT the iteration count. The unconditional `i += 1` before the skip-check ensures sequence-position is incremented even on excluded moves, matching the `enumerate()` + `continue` pattern.

### 7. Searchmoves filter is folded into `new()`

`Option<&[Move]>` parameter on `MoveStager::new`. Eliminates temporal coupling (a separate `retain_root_filter` method would have a "must call before next()" invariant the type can't enforce). Filter applied at Step B of `new()` before TT extraction, mirroring today's `moves_vec.retain(filter)` BEFORE `order_moves` ordering.

### 8. Test surface

The H1-* tests in `src/search.rs::tests`:
- 19 stager pure-unit tests (H1-S1..S19, plus H1-S5b for tied MVV-LVA, H1-S13b for `peek_from_captures` mutation kill).
- 10 helper-unit tests (H1-H1..H10).
- 6 negamax integration tests (H1-I1..I6).
- 1 proptest at 2048 cases (H1-P1) — yield-sequence equivalence to `order_moves`. Bench-parity proof.
- E51 (H1-B2) depth-4 pin (`bench_signature_deterministic_across_two_runs_with_qsearch_tt`) at `89_080`.
- H1-B1 bench-CLI parity (`info string bench: 1147614 nodes <NPS> nps`) — verification-gate command per plan §12 step 3.

`order_moves` and `negamax_move_order_score` remain in the file as `#[cfg(test)]` and `#[allow(dead_code)]` respectively, preserving the 14+ existing test call sites (S20–S26, HS9, HS12) and the score-tier discipline (`KILLER0_SCORE > KILLER1_SCORE > MAX_HISTORY` compile-time invariant via `_SCORE_TIER_INVARIANTS`).

### 9. Verification

Bench parity at `1147614` exact (M5.G's count). Extended perft passes (`canonical_six_d4_heavy_slow`, `canonical_six_d5_slow`, `whittington_epd_d4_slow`) — the movegen-adjacent gate. Mutation testing: 60 mutants → 55 caught + 3 timeout (functionally caught via inf-loop) + 1 unviable + 1 missed-then-killed via H1-S13b; final viable catch rate 58/59 = 98.3%.

50-pair defensive SPRT confirmation match vs `baseline/m5g-singular` per plan §12 step 7 (mixed-TC, seed `0xC1ABF15AE10DD00E`). H1 makes no strength claim; the SPRT confirms no-regression.

## Consequences

### Positive

- **H2 architectural prerequisite landed.** The stage machine is in place; H2 only needs to flip eager generation to lazy generation per stage. The TT and killer-membership scans become typed `is_pseudo_legal` validators at H2 (since there will be no full legal-move list to scan against).
- **Move-loop reads cleaner.** The negamax move-loop body no longer mixes `generate_moves` / `order_moves` / `for enumerate()` boilerplate with the per-move logic; the `MoveStager` abstracts the staging away.
- **Test surface for stage-by-stage logic in place.** Future stage additions (countermoves at M6+, SEE split) extend the existing test pattern.

### Negative

- **No Elo gain at H1 by design.** The architectural refactor pays the LOC cost (~600 LOC including tests + ADR + retrospective) without strength benefit. H2 captures the Elo signal.
- **H1 introduces an indirection layer.** `MoveStager::new` + `next()` is ~5% slower than today's monolithic `order_moves` + `for enumerate()` per-call by abstract count, but bench parity at `1147614` confirms the actual runtime overhead is below noise.

### Open questions deferred

- **Typed TT/killer validators** (no full legal-list scan) — required by H2.
- **SEE split** of captures into `SEE ≥ 0` (good) and `SEE < 0` (bad) for stage 2/6 separation. Tuning backlog.
- **Countermove stage** between Killer1 and Quiets. M6+ candidate per CPW Countermove Heuristic.
- **Selection-sort within stages** (find-max instead of `sort_by_cached_key`) — micro-optimization; not measurable at typical batch sizes per research §9.
- **Qsearch stager** — out of scope per research §13.3 (qsearch's stages are a strict subset; type-sharing not worthwhile).

## §10. H1 v2 thin-wrapper retune

**Symptom.** v1 implementation (per-stage state machine: `Stage::{Tt, Captures, Killer0, Killer1, Quiets, Done}` + per-stage Vecs `captures`/`quiets` + per-stage sorts) was bench-equivalent at depth 7 (`1147614 nodes` exact) and produced byte-equivalent yield sequences (verified by H1-P1 proptest at 2048 cases against `order_moves`), AND identical PV/nodes at single-shot `go depth 12` from startpos and from a 16-move mid-game position. But under sustained game-load, HEAD was ~3.5% slower at depth 14 from startpos (single-shot, byte-equivalent tree: 14201938 nodes both, but 6426ms HEAD vs 6206ms baseline) and ~5–9% slower in 20-sequential-`go-depth-10` tests. Defensive 100-game SPRT confirmation match vs `baseline/m5g-singular` (mixed-TC, seed `_00E`): Δ Elo **−110.48 [−207.37, −28.06]** at 26 games (verdict=H0). 300-game continuation Δ Elo **~−44** at 222 games (W=46 L=74 D=102, 43.7% score, sigma 8.65). Real regression.

**Diagnosis.** v1's per-node cost was ~3× M5.G's per-node cost on three counts:
- 3 `Vec<Move>` allocations per node (`all`, `captures`, `quiets`) vs M5.G's 1 (`moves_vec`).
- 2 `sort_by_cached_key` calls (captures + quiets) vs M5.G's 1.
- Each `Vec::with_capacity(all.len())` alloc-then-drop cycle adds allocator bookkeeping.

At ~15ns/node × 200K nodes/sec = 3µs/sec extra. Bench's snapshot measurement on a 16-position corpus with fresh allocator state did not surface this; long-running games (millions of nodes, sustained allocator pressure) did.

**Fix.** Collapse the stage state machine into a single `Vec<Move>` sorted once by `negamax_move_order_score` with TT promotion to index 0 — literally M5.G's `order_moves` algorithm wrapped in the M5.H1 stager's API. The public API (`new` / `next` / `peek` / `len` / `is_empty` / `yield_sequence`) is preserved verbatim. `peek` is `&self`-receiver and reads `self.moves.get(self.idx)` — type-enforced idempotent for the SE block's double-call. `len()` returns the cached `total_len` (= `moves.len()` at construction time); does not decrement; `MoveStager` does NOT impl `Iterator`. `next()` is `let mv = self.moves.get(self.idx).copied()?; self.idx += 1; Some(mv)`.

**v2 verification.**
- Bench parity holds: `1147614 nodes` exact.
- Sustained-go test (20-sequential-`go-depth-10`) post-v2: HEAD ≈ baseline (3 runs: ratio 0.967, 0.997, 0.958; avg 0.974, slightly faster).
- Single-shot depth-14 from startpos post-v2: HEAD/baseline within ~1% (run-to-run noise).
- Test surface: 30+ H1-* tests (H1-S/H/I/B/P) all pass against v2 (yield-order properties unchanged because v2 produces the same yield sequence as v1 — v2 just gets there with one sort instead of two).
- Defensive 200-game SPRT confirmation match vs `baseline/m5g-singular` (seed `_010`) post-v2 — see SPRT log.

**API stability for M5.H2.** v2 preserves the public API verbatim. M5.H2's contract is unchanged: keep the same `new` / `next` / `peek` / `len` / `is_empty` / `yield_sequence` surface, swap the internal `Vec<Move> + sort` for per-stage lazy generation (TT typed-validate, captures generated on first entry to virtual `Captures` stage, etc.). The `Stage` enum was removed at v2 — H2 will reintroduce it as part of the per-stage lazy-generation work, scoped to the impl, not exposed via the API.

**v1 helpers preserved as `#[cfg(test)]`.** The v1 helpers (`extract_move_by_bits`, `extract_move_by_eq`, `partition_captures_quiets`, `mvv_lva_sort_in_place`, `history_sort_in_place`) remain in the file as `#[cfg(test)]`, retaining the H1-H1..H10 mutation-discrimination test surface. M5.H2 may resurrect them as production code paths if per-stage lazy generation needs them (no API change required).

**Lessons.** (1) Bench parity at depth 7 is necessary but NOT sufficient — sustained game-load surfaces per-node cost differences invisible to single-shot bench. Future bench-neutrality claims should also include sustained-load NPS measurement (multi-`go` sequential test) when the change touches the per-node hot path. (2) The H1-P1 proptest correctly verified yield-sequence equivalence (algorithmic correctness) but said nothing about per-node cost (performance correctness). Both gates are needed for "bench-neutral" claims. (3) The defensive SPRT confirmation match in plan §12 step 7 caught a regression that bench + tests didn't. Worth the ~30 min wallclock. (4) The "thin-wrapper as the simplest H1" alternative considered and rejected at plan time turned out to be the correct choice. The "stage-aware H1 prepares the architecture for H2" rationale was true but cost-prohibitive at H1 — the H2 architecture work can land at H2 itself without H1 carrying the cost.

## References

- Plan: [`docs/plans/m5.h1.md`](../plans/m5.h1.md).
- Research: [`docs/research/m5-staged-movegen.md`](../research/m5-staged-movegen.md).
- CPW: [Move Generation](https://www.chessprogramming.org/Move_Generation), [Move Ordering](https://www.chessprogramming.org/Move_Ordering), [Move List](https://www.chessprogramming.org/Move_List).
- TalkChess t=76835 (staged-movegen architectural discussion); t=68923 (killer + TT pseudo-legality); t=76491 (sort-vs-pick).
- ADR-0007 (legal-direct movegen), ADR-0019 (history heuristic — killer quiets-only invariant), ADR-0029 (singular extensions — peek consumer).
