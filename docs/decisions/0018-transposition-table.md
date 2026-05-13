# ADR-0018 — Transposition table: layout, replacement, mate-score discipline, GHI stance

**Status:** Accepted (lands with M4.A).

## Context

M4.A is the first phase that *caches search results* — adds a Zobrist-keyed transposition table layered over M3.C's negamax + M3.D's qsearch + M3.E's iterative deepening. The decisions bind once the TT module ships, the negamax body restructures around the new prologue ordering, and `bench` adopts a per-position TT clear.

Prior-art and full justification: `docs/research/m4-transposition-table.md` (~14 open-question survey). This ADR records the *commitments*; the research is the *evidence*. ADR-0011 (UCI threading) supplies the single-mutator invariant the table relies on for `unsafe impl Sync`; ADR-0016 (search structure) supplies the negamax body that the prologue restructure modifies; ADR-0017 (time management) supplies the ID outer loop that fills the TT across iterations.

Plan and test surface: `docs/plans/m4.a.md`.

## Decision

### 1. Replacement scheme: depth-preferred + age bias

For each store at index `idx = key & mask`:

```text
replace = old.is_empty()
       || old.age() != current_generation     // old generation: free to replace
       || old.depth <= new.depth              // depth-preferred at same gen
```

Rationale: simple; flushes stale entries naturally between root searches; same-depth `<=` (not `<`) bias toward freshness on ties. CPW + Breuker et al. 1994 + Mediocre Chess all converge on a depth-recency mix. Two-tier (Thompson-Condon) is the second choice; rejected because it halves the effective entry count at the same MiB budget without producing a larger Elo delta at our strength range.

### 2. Entry key discipline: full 64-bit Zobrist (single-threaded)

Stored: full 64-bit Zobrist key in each entry. Single-threaded for M4–M7 by ADR-0011 invariant.

Rationale: collision rate ~1 per 4 billion at 64 bits; the lockless XOR-trick (Texel / Stockfish style) solves a concurrency problem we don't have until M8 Lazy SMP. Migration to lockless at M8 replaces `(key: u64, data...)` with `(key_xor_data: u32, data: u64)` — a clean internal refactor; no M4.A code anticipates it.

### 3. Per-entry packing: 16 bytes

```rust
#[repr(C)]
struct TtEntry {
    key: u64,             // 8 bytes
    score: i16,           // 2 bytes (mate-adjusted on store)
    depth: u8,            // 1 byte
    age_and_bound: u8,    // 1 byte: bits [7..2] = age (6 bits); bits [1..0] = bound
    best_move: u16,       // 2 bytes (packed Move; 0 = no move)
    _pad: u16,            // 2 bytes pad → align to 16
}
```

16 bytes ÷ 64-byte cache line = 4 entries per cache line. `_pad` is logically zero; implementations must not encode meaning into it.

**Empty-slot discriminator (M4.A → M5.F).** M4.A used a multi-field test `key == 0 && depth == 0 && age_and_bound == 0 && best_move == 0` and forbade `depth = 0` stores via `debug_assert!(data.depth >= 1)`. M5.F (ADR-0028) needed `depth = 0` for qsearch entries; the discriminator was changed to **`self.key == 0`** (sole field) and the store-side assertion to `debug_assert!(key != 0)`. The Polyglot Zobrist key of any reachable real position is non-zero with probability `1 − 2⁻⁶⁴`; release-build behavior on a hypothetical `key == 0` collision is benign-and-self-recovering (silent skip).

### 4. Hash UCI option: `spin default 16 min 1 max 4096`

UCI line emitted at `uciok` time:
```
option name Hash type spin default 16 min 1 max 4096
```

Default 16 MiB matches Stockfish/Defenchess/Euwe industry consensus; 4096 MiB upper bound is realistic for Apple Silicon dev boxes. Allocation rounds **down** to the nearest power of two in entries (mask-based indexing); 16 MiB / 16-byte entries = 1,048,576 entries.

### 5. Mate-score depth-adjust on store/probe

`MATE = 30000`, `MAX_PLY = 64`, `MATE_IN_MAX_PLY = MATE - MAX_PLY = 29936`. A score is a mate score if `|score| > MATE_IN_MAX_PLY`.

```rust
fn score_to_tt(score: i32, ply: i32) -> i32 {
    if score > MATE_IN_MAX_PLY { score + ply }
    else if score < -MATE_IN_MAX_PLY { score - ply }
    else { score }
}

fn score_from_tt(score: i32, ply: i32) -> i32 {
    if score > MATE_IN_MAX_PLY { score - ply }
    else if score < -MATE_IN_MAX_PLY { score + ply }
    else { score }
}
```

i32 in, i32 out. Store call site narrows to i16 with `debug_assert!(adjusted == adjusted as i16 as i32, ...)`. The bound `|adjusted| ≤ MATE + MAX_PLY = 30064 < i16::MAX = 32767` holds by construction.

Worked example: mate-in-2 found at ply 0 → returned `MATE - 4`. `score_to_tt(MATE - 4, 0) = MATE - 4` (ply=0 no-op). Same position reached at ply 2; probe stored `MATE - 4`; `score_from_tt(MATE - 4, 2) = MATE - 6`, i.e., "mate in 6 plies from ply 2" — same absolute mate node.

Round-trip: `score_from_tt(score_to_tt(s, p), p) == s` for all `s ∈ [-INF, INF]` and `p ∈ [0, MAX_PLY]`. Pinned by proptest T19.

### 6. Qsearch participation in TT: deferred to M5 — **closed at M5.F (ADR-0028)**

M4.A's negamax probes/stores; qsearch does NOT. Rationale: empirical Elo benefit is uncertain (zero in Crafty, +25 Elo in one engine); structural fit is awkward (qsearch nodes have no negamax-equivalent depth field); composing intermediate "probe-but-don't-store at horizon" with full qsearch-in-TT later requires a redesign of the depth field semantic. Cleaner to defer entirely until full M5 redesign.

Acknowledged miss: a position visited as a qsearch *interior* node on Branch A and reached as a horizon node on Branch B incurs a redundant qsearch on B. +5–15 Elo gap per CPW survey.

**Closed at M5.F (2026-05-09; ADR-0028):** full probe-and-store at qsearch entry / exit. Qsearch entries store with `depth = 0`; the `is_empty()` discriminator changes to `self.key == 0` (the forward-planned alternative — see §3 below); the store-side `data.depth >= 1` debug-assert is replaced with `key != 0`. Per Stockfish 45e5e65, non-terminal qsearch results store as Lower or Upper only (never Exact); terminal Exact only at true stalemate (score=0) and mate at horizon (score=−(MATE−ply)). TT-move ordering inside qsearch is filter-gated implicitly via `moves_vec` membership (Andrew Grant's "long-chain protection"). Negamax's existing probe rule `entry.depth as u32 >= depth` (depth ≥ 1 at negamax sites) naturally rejects depth=0 qsearch entries from causing negamax cutoffs — exactly correct, since a qsearch score with restricted moves cannot soundly cut a full-width search. Mixed-TC SPRT vs `M5.E`: Δ Elo +13.03 [−10.92, +37.12], landed as "small-but-not-regression" per plan §11 spirit.

### 7. Best-move preservation on overwrite

When the new store has `best_move == 0` (typically a fail-low Upper-bound) AND the slot's current entry has the same key with a non-zero `best_move`, the old `best_move` is preserved into the new entry. Rationale: the move-ordering hint is more valuable than the bound is fresh; preserving the move costs one extra load (already in cache from the probe) and changes nothing for keys that don't match.

### 8. `ucinewgame` and Hash resize semantics

`ucinewgame`: clears TT entries, resets generation to 0, clears game_history, resets position to startpos, calls `Search::reset` for any search-side per-game state.

`setoption name Hash value <N>`: joins any in-flight worker (existing helper); rebuilds the TT (allocates new `Vec<TtEntry>`, zero-init, replaces old, recomputes mask, resets generation to 0). Industry convention; preserving entries across a size change is impossible because index computation depends on size.

### 9. Age semantics: per-`go` increment, 6-bit field, mod-64 wrap

Generation counter increments once per `go` command via `TranspositionTable::new_search()`: `gen = (gen + 1) & 0x3F`. Range `[0, 63]`; wraps at 64. New entries store the current generation in the entry's `age_and_bound` byte (6 high bits).

Replacement uses age as the primary cross-search override (entries from prior `go`s are freely replaceable, regardless of stored depth). Within the same `go`, depth-preferred (§1) governs.

Per-ID-iteration increment is **wrong** (would age out cross-iteration entries within the same root search); per-`go` is the consensus.

### 10. Graph-history-interaction: live with it (Option 1)

The Polyglot Zobrist key (ADR-0009) does not encode the halfmove clock or game-path repetition history. A TT entry stores a score from a search where the position had specific halfmove/repetition state; a probe under different state may return a path-wrong score.

**Decision:** accept the imprecision (Option 1). The repetition check at the negamax prologue (before TT probe) defangs the most common manifestation (draw-by-repetition mis-scoring): if the current position appears earlier in `history`, return draw=0 *before* the TT probe is consulted.

The 50-move-boundary GHI is acknowledged. Option 2 (suppress probe/store when `halfmove_clock > 80`) and Option 3 (encode path state in the key) are weighed; Option 2 is the cheap fallback if SPRT analysis surfaces unusual late-game draw miscounting at M4.D or beyond. Not in M4.A scope.

### 11. PV-node vs non-PV-node probe discipline

`is_pv: bool` parameter threaded through `negamax`. Set `true` at the root call from `Search::go`; in recursion, child `is_pv = parent_is_pv && i == 0` where `i` is the move-loop **recursion-order index** (after the step-10 TT-move-first reorder).

At PV nodes: TT probe extracts the stored move for ordering only; never returns early on the stored score (even Exact). At non-PV nodes: full bound comparison (`Lower` ≥ beta → return; `Upper` ≤ alpha → return; `Exact` → return).

Under fail-soft pure alpha-beta there is no window-based PV/non-PV distinction (every node has the same window type semantically). `is_pv` is a synthetic ordering predicate governing TT-cutoff suppression. M4.D's aspiration windows still use the synthetic `is_pv` (first-try window has `beta - alpha = 100`, far from PVS's zero-window). PVS (M5) replaces `is_pv` with the window-based `beta - alpha == 1` predicate.

Why suppress cutoffs at PV: an Exact-bound TT hit at a PV node would return early without searching all moves, shortening the displayed `info pv` line and hiding the fully-explored variation. CPW: "in more advanced engines transposition table cutoffs are not performed on PV-Nodes."

### 12. TT-move legality check: "is in legal-move list"

Before promoting the TT move to index 0 of `moves_vec`, scan the legal list for membership. Linear scan over typically <40 moves. Cost is bounded; correctness by construction (we already generated the list). Fast structural pseudo-legality check is a later optimization (deferred to a profiling-driven decision).

`Move::default() == 0u16` (a1-a1-Quiet) is the no-move sentinel; never appears in any legal list, so the membership scan rejects the sentinel without a separate guard.

If migrating to partial-key + XOR-trick at M8: legality check remains mandatory.

### 13. Per-node prologue ordering

```text
1. clear PV slot at this ply
2. depth == 0 → delegate to qsearch (UNCHANGED)
3. nodes increment + 4096-cadence cancellation poll (non-leaf only)
4. CAPTURE original_alpha = alpha   (BEFORE MDP, BEFORE TT probe)
5. ply > 0: repetition + 50-move draw → return 0
6. mate-distance pruning: tighten alpha/beta against ±(MATE - ply)
7. TT probe with key = pos.zobrist():
   - hit + !is_pv + entry.depth >= depth + bound match → return ply-adjusted score
   - hit at PV node: extract tt_move for ordering; do not return
   - miss / shallow / non-cutoff: keep tt_move as ordering hint
8. generate moves; root searchmoves filter
9. terminal: empty list → MDP-tightened mate / stalemate / 0
10. order: MVV-LVA, then move tt_move to index 0 if present in list
11. recurse fail-soft (child is_pv = is_pv && i == 0)
12. STORE on completion (END of move loop, never mid-loop):
    - skip if self.aborted
    - bound from (best, beta, original_alpha)
    - score: score_to_tt(best, ply) narrowed to i16
```

Rationale for the order:
- Repetition first: prevents stale TT score returning when current position is a draw by repetition.
- MDP before TT probe: tightens the window the TT comparison sees, allowing more entries to produce cutoffs.
- `original_alpha` captured **before** MDP (load-bearing): MDP can mutate `alpha` upward; bound classification at step 12 must compare against the caller's pre-MDP window, otherwise a fail-low against the MDP-tightened alpha mis-classifies as Exact.
- Store at end-of-loop (never mid-loop) + skip-on-abort: aborted iterations do not overwrite prior iterations' entries.

### 14. Per-game state inventory for `bench` reset

`Engine::reset_for_new_game()` is the single-source reset method. Called by `handle_ucinewgame` and per-position inside `handle_bench`. Clears:

| State | Owner | Action |
|---|---|---|
| TT entries | `Engine.tt` | `tt.clear()` (zeroes entries, resets generation to 0) |
| `game_history` | `Engine` | `vec![startpos.zobrist()]` |
| Position | `Engine` | `Position::starting_position()` |
| Search-internal | `Search` | `self.search.lock().reset()` |

Future M4.B–D state additions (killer slots, history table) extend the search-internal reset path. The engine-level method does not need to know about them.

`prior_root_move` (M3.E) is removed in M4.A; its state-reset is no longer in the inventory.

## Consequences

- TT module shipping enables M4.B (killer) / M4.C (history) / M4.D (aspiration) without further structural change to the negamax prologue.
- Single-threaded `UnsafeCell` is the structural commitment; M8 Lazy SMP swaps to `Vec<AtomicU64>`-pair entries.
- Mate-score discipline lives in `tt.rs` as `score_to_tt` / `score_from_tt`; M5 qsearch-in-TT reuses them as-is.
- Plan-review (pass 2) verdict: `no further substantive issues`.
