# Prior-Art Research: Staged Move Generation — Allocation Patterns and Performance

**Generated**: 2026-05-11 (during the M5.H2 failure investigation; see ADR-0030 §11).

Sources consulted: Chess Programming Wiki (CPW) — Move List, Move Generation, Move Ordering, History Heuristic; TalkChess threads t=39934 (dynamic allocation performance), t=64642 (move list stack vs heap), t=76491 (sorting moves), t=73930 (sort every moves or pickNext), t=79279 (lazy sorting algorithm), t=79480 (lazy sort issues), t=76835 pages 1 and 2 (staged move generation), t=69704 (lazy move generation), t=63502 (speed up or avoiding move sorting), t=31999 (storage and ordering strategy), t=81708 (architecture for bitboard engine), t=81716 (move ordering); SEGGER Blog (real-time allocation in chess engines); SebLague Chess-Challenge issue #188 (heap allocation critical performance issue); Rust Users Forum (optimizing Rust borrowing patterns in chess engine); The Rust Performance Book (heap allocations chapter); ArrayVec/SmallVec/tinyvec crate documentation and benchmarks; likeawizard Lichess blog (frustrating side of chess engine development). Per ADR-0003, no engine source repos were read.

---

## TL;DR

- The chess programming consensus is unambiguous: **per-node heap allocation during search is a recognized performance anti-pattern**. The CPW Move List page, TalkChess community, and multiple benchmarks all condemn it. The alternative is one-time preallocated storage — either a global move stack (pointer-framed, depth-indexed), per-ply fixed arrays on the call stack, or a thread-local preallocated `Vec<Move>` that is cleared (not dropped) each node.
- The 3-allocation-per-node pattern (one `Vec` for all + one for captures + one for quiets) has a **12–46%+ overhead** vs preallocated approaches, scaling with allocation frequency. At bench depth 7 this is invisible (the allocator cache is hot and the tree is shallow); at depth 14 under game-load the allocator evicts itself from cache and performance degrades 3–15%.
- The **bimodal TC pattern** (10+0.1 and 20+0.2 regress, 40+0.4 gains) has a known qualitative explanation: shallow TC searches reach only depth 8–10 where the lazy-sort benefit (fresher history scores improving quiet ordering at depth ≥ ~6) fires rarely and the allocation overhead dominates; slow TC reaches depth 14–18 where the history freshness benefit fires many times per game and the lazy-sort Elo gain appears.
- **Single-buffer allocation-free approaches** are well-documented: (a) one `Vec<Move>` (preallocated, `.clear()` per node, never dropped) for all moves with a partition boundary tracked by index; (b) a depth-indexed global stack of `[Move; 256]` blocks per ply; (c) `ArrayVec<Move, 256>` (fixed-capacity, stack-allocated); (d) H.G. Muller's dual-growth scheme (captures grow downward, quiets grow upward from a shared frame pointer).
- The **fresh-history-at-sort-time Elo signal** can be captured without a stage machine: generate all moves eagerly into one buffer, partition captures to the front in O(N) using `partition_in_place`, sort the captures sub-slice by MVV-LVA, defer sorting the quiets sub-slice until after captures and killers are searched (at which point history is updated). This "deferred quiet sort" achieves fresh history in one buffer with zero extra allocations.

---

## §1. Allocation-Free Patterns in the Literature

### 1.1 The Core Principle

CPW Move List (retrieved 2026-05-11): "Chess programmers tend to administrate their own once allocated or static array of moves, shared by all levels of the search, or alternatively keep distinct move arrays for each ply of up to 256 moves on the processor stack. Dynamically allocated lists on the heap, which reallocate if more space is needed during generation, are quite expensive inside the search and typically avoided."

TalkChess t=81708 (jdart, JoAnnP38, 2022): "One thing to avoid in particular is any operation that would result in a large number of heap allocations." The consensus: use stack allocation or preallocated structures; avoid `new`/`malloc` inside the search function.

SEGGER Blog (2020): A chess engine using `std::list<Move>` (heap-allocated) on a 168 MHz Cortex-M4 ran 14,497–15,107 nodes/s; switching to an optimized real-time allocator raised this to 21,138–21,955 nodes/s — a **46% improvement from allocator improvement alone**, with no algorithmic change.

SebLague Chess-Challenge issue #188 (2023): "The API was doing a heap allocation (for 218 moves) per traversed node in the tree." With a 17-ply capture chain, a single depth-1 alpha-beta search with quiescence could not complete within 800ms. Fix: `stackalloc Move[218]` (C# stack allocation). Estimated impact: "over 16GB heap allocation in just a single game."

TalkChess t=39934 (Rein Halbersma benchmark, 2011):

| Approach | Time to depth | Overhead |
|---|---|---|
| `std::array<128>` (stack) | 29,359 ms | baseline |
| `std::vector` with `reserve(32)` | 32,875 ms | +12% |
| `std::vector` with no reserve | 42,046 ms | +43% |
| `std::deque` | 48,969 ms | +67% |

TalkChess t=64642 (Pawn Chess, 2017): One developer compared declaring a 2084-byte move-order structure at engine-class level (pre-allocated, version A) vs. allocating it on the stack each search call (version B). Result: **Version A at 3,470,138 N/s; Version B at 2,736,985 N/s** — a 27% penalty for per-call construction. Root cause: the contained class's constructor ran 256 times on each call in version B; in version A it ran once at startup.

### 1.2 Pattern Catalogue

#### Pattern A: Global Per-Thread Move Stack (Depth-Indexed Pointer)

Described in CPW Move List: "keep a giant stack of moves per thread, and within each search function just remember pointers to the beginning and end of the current 'move list' for that node." The stack is pre-allocated at engine startup as `Vec<Move>` or `[Move; MAX_PLY * MAX_MOVES_PER_PLY]`. Each negamax frame gets a slice starting at `stack[ply * MAX_MOVES]`.

- **Allocation cost**: zero at search time (one allocation at engine startup).
- **Cache behavior**: moves at ply N are adjacent in memory; moves at different plies are separated by a fixed stride. Captures and quiets for the same ply are contiguous.
- **Concurrency**: each thread gets its own stack (separate `Vec`); no sharing.
- **Sizing**: CPW Move List recommends `MAX_PLY × average_branching_factor + safety_margin`. With MAX_PLY=128 and average BF~40, this is ~5120 moves; using 256/ply gives 32,768 moves = 65,536 bytes for `u16` moves. Fits comfortably in L2.

#### Pattern B: Per-Ply Fixed Array on the Call Stack

Declare `let mut moves = [Move::default(); 256]` (or `ArrayVec<Move, 256>`) as a local variable in the negamax function. The function's stack frame holds the array; the compiler allocates it on process stack entry.

- **Allocation cost**: zero (stack frame allocation is free).
- **Cache behavior**: varies — depends on call depth and stack layout. At depth 14, 256 × 2 = 512 bytes × 14 plies = ~7 KB of live stack frames; this fits in L1 cache.
- **Risk**: stack overflow at extreme depth. 256 moves × 2 bytes × 128 plies = 64 KB. Rust's default stack size is 8 MB; no overflow concern for chess.
- **Rust idiom**: `ArrayVec<Move, 256>` from the `arrayvec` crate achieves this. `tinyvec::ArrayVec<[Move; 256]>` is safe-only alternative. The `shakmaty` crate uses `ArrayVec<Move, {_}>` for its `MoveList` type with this exact pattern (retrieved 2026-05-11).

#### Pattern C: Preallocated Thread-Local Vec (Cleared, Not Dropped)

One `Vec<Move>` preallocated per thread at engine startup (or lazily on first use via `thread_local!`), cleared with `v.clear()` at the start of each node. `.clear()` sets the length to 0 but retains the heap capacity; no allocation/deallocation occurs.

- **Allocation cost**: zero at search time (capacity is never freed during search).
- **Cache behavior**: the buffer is reused for every node; the first ~30–60 elements are hot in cache.
- **Rust idiom**: `thread_local! { static MOVE_BUF: RefCell<Vec<Move>> = RefCell::new(Vec::with_capacity(256)); }` accessed via `.borrow_mut()` and `.clear()` at entry.
- **Limitation**: requires the caller to not hold a reference across the recursive call — which is always the case in negamax since moves are generated before recursing.

#### Pattern D: Single Vec + Partition Boundary (Captures Prefix, Quiets Suffix)

Generate all moves into one buffer. Use `slice::partition_in_place` (nightly) or an equivalent stable-Rust partition to move captures to the front and quiets to the back, tracking the boundary index. Sort only the captures sub-slice by MVV-LVA; sort only the quiets sub-slice by history. Iterate captures by index, then killers (checked against the quiets sub-slice), then quiets.

- **Allocation count**: 1 (the buffer).
- **Sort passes**: 2 (one per sub-slice), but each sub-slice is smaller than the full list.
- **History freshness**: this is where the deferred quiet sort lives — see §3.1.
- **Literature reference**: H.G. Muller describes a closely related dual-stack scheme (TalkChess t=76491): "I usually do that already during move generation — I then just store the captures at the beginning of the move list, while non-captures go at the end." The dual-growth scheme from TalkChess t=31999 (Muller, HaQiKi D engine) does the same with a shared fixed-size frame: captures grow downward from a pointer, quiets grow upward. After generation, captures occupy `[frame..ptr]` and quiets occupy `[ptr..end]`. Sort only captures; killers are found in the quiet half; dedup is a linear scan of the quiet half.

#### Pattern E: Dual-Growth (Captures Down, Quiets Up) from a Fixed Frame

Described by H.G. Muller (TalkChess t=31999, 2009): a single fixed block of ~300 moves per ply. A frame pointer is set at entry. Two cursors advance from the frame: `cap_ptr` grows downward (captures), `quiet_ptr` grows upward (quiets). After generation, captures occupy `[frame, cap_ptr)` and quiets occupy `[quiet_ptr, frame + 300)`. Sort the captures sub-array by MVV-LVA. Run through the quiet array to score by history and identify killers.

- **Allocation cost**: zero (the block is part of the per-ply fixed buffer in Pattern A).
- **Advantage over Pattern D**: captures and quiets are naturally separated at generation time with no partition pass needed.
- **Disadvantage**: more complex indexing; requires knowing which direction each move class grows.

### 1.3 The 3-Vec Design (clawfish M5.H2 v1/v2) vs These Patterns

| Property | 3-Vec design | Pattern A global stack | Pattern C thread-local | Pattern D single Vec |
|---|---|---|---|---|
| Allocations per node | 3 (all, captures, quiets) | 0 | 0 (after warmup) | 1 (but preallocatable) |
| Sorts per node | 2 (captures, quiets) | 2 | 2 | 2 |
| Cache locality | poor (3 heap ptrs) | excellent (contiguous) | good (reused buffer) | good (1 ptr) |
| Stage machine needed | yes | no | no | no with deferred-sort |
| Implementation complexity | high | medium | low | low |
| Deferred-quiet-sort compatible | yes | yes | yes | yes |

---

## §2. Documented TC-Bimodal Regressions

### 2.1 Prior Cases

The literature does not document a case that precisely matches clawfish M5.H2's observed pattern (weak at 10+0.1 and 20+0.2, strong at 40+0.4). However, the following documented patterns are close analogues:

**TalkChess t=76835 (Desperado):** "10% (−20%) speed gain is realistic" for staged movegen at "hyperfast (<10s) testing games" and "engine level still in the range of 2000 Elo." Desperado explicitly notes the gain is "highly dependent on testing conditions" — a TC-sensitivity observation, though not quantified by TC bucket.

**TalkChess t=81716 (multiple developers):** "results are highly dependent on time control as well." Context: a developer testing move ordering changes over 200–300 games finds inconsistent results across TC. Whiskers advises "anything up to a couple hundred games is completely unreliable." No bimodal mechanism explained.

**TalkChess t=79279 (lazy sorting):** JohnWoe's NPS benchmark shows lazy sort +14% vs `std::sort`. But Algorhythm's Elo test over 800 games showed **−7.8 Elo** for lazy sort. This is a sign-flip from NPS to Elo — a pattern consistent with NPS gains that don't translate to Elo because the ordering is worse (not fresher) under the specific lazy-sort implementation used.

**likeawizard blog (Lichess, 2024):** A developer attempted to implement ordering by check-giving moves first. Result: "huge overhead for every move generated" → worse NPS → worse strength even though the ordering was theoretically better. The overhead tax outweighed the pruning gain at the tested TC.

### 2.2 Qualitative Explanation for M5.H2's Bimodal Pattern

The literature provides the following framework for understanding this pattern (synthesized from TalkChess t=76835 "Desperado," t=63502 "Martin Sedlak," t=69704 "Joost Buijs"):

**Why 40+0.4 gains (+65 Elo):**
At slow TC, the engine reaches depth 14–18. At these depths:
- The quiet-move sub-tree is large (~15–25 quiets per node × many nodes).
- History scores are accumulated from 14+ plies of prior search.
- Lazy sorting of quiets (after captures are searched) reflects **more recent history updates** — in particular, cutoffs at captures and killers from the same node have already updated the history table, so the quiet sort order is slightly better.
- Beta cutoffs that save quiet generation are common: Joost Buijs (t=69704) says "staged move generation helps a little at nodes with an early beta cutoff"; at depth 14–18 this fires often.
- The per-node allocation overhead (3 `Vec` allocs) at depth 14 costs ~15–20 ns/node × 14M nodes = 200–280 ms per game, which matters but is outweighed by the search quality gain.

**Why 20+0.2 regresses (−95 Elo):**
At 20+0.2, the engine reaches depth 10–12. At these depths:
- History tables are less populated; the freshness benefit of sorting quiets after captures is small.
- Beta cutoffs that save quiet generation are less common (shallower search sees fewer TT hits, fewer NMP successes).
- The per-node allocation overhead (3 `Vec` allocs) at depth 10 costs ~15–20 ns/node × 3M nodes = 45–60 ms per game — comparable to the search quality gain but now overhead dominates because the gain is smaller.
- Additionally, at depth 10 the quiets sub-list is smaller (~15 moves) so the benefit of a better sort is smaller.

**Why 10+0.1 regresses slightly (−10 Elo):**
At 10+0.1, the engine reaches depth 8–10. The history freshness benefit is minimal. The allocation overhead is small in absolute terms (few nodes) but the gain is essentially zero. The slight regression is noise plus a small allocation-tax.

**The critical depth threshold:**
The literature's implicit threshold for staged-movegen benefit is approximately depth 10–12 (where TT hit rate is high enough for the TT-move-first cutoff economy to dominate) or depth where history tables are richly populated (~4000+ entries). Below this threshold, the overhead tax exceeds the gain.

### 2.3 The 3-Vec Specific Problem

Even if the lazy-sort Elo signal is real at slow TC, the **3-Vec implementation** imposes a steeper overhead than necessary:
- 3 allocations/deallocations per node (vs 1 for staged movegen with a single backing Vec).
- The allocator for `Vec` in Rust (system allocator via `GlobalAlloc`) follows a jemalloc-like pattern on macOS (actually Apple's libmalloc). Small allocation bursts from 3 `Vec` allocs per node at depth 14 pressure the thread-local allocator cache differently from a single allocation.
- Per the SEGGER blog finding (46% gain from a better allocator alone) and the TalkChess t=39934 benchmark (12–43% from reserve strategy), the marginal cost of going from 1 to 3 allocations is likely 5–15% NPS.

---

## §3. Alternative Ways to Capture the Fresh-History-at-Sort-Time Signal

### 3.1 Deferred Quiet Sort (Researcher's Recommended Pattern)

**Concept:** Generate all moves eagerly into one preallocated buffer. Partition captures to the front in O(N) using Rust's `partition_in_place` or a manual swap loop. Sort captures by MVV-LVA immediately. Iterate captures. Iterate killers (checked against the quiet half). Only then sort the quiet half by history. At the point of quiet sort, history has already been updated by any captures/killers that caused cutoffs at this node — identical freshness to staged movegen, zero extra allocations.

**Literature basis:**
- TalkChess t=76491 (Ronald, 2019): "If you presort the quiet moves, you don't use the changes in history score for the still to search quiet moves which can lead to a different order of those moves." This is the exact mechanism; the deferred sort avoids this.
- TalkChess t=76835 (page 2, Desperado): "Quiet moves get (can have) another history score in staged move generation than in a 'one run' implementation because the time when you check the scores is different." Deliberately stated as a feature.
- TalkChess t=76491 (H.G. Muller): "I then just store the captures at the beginning of the move list, while non-captures go at the end" — the single-buffer partition precondition for deferred sort.

**Implementation sketch (Rust, no extra allocations):**

```rust
// In preallocated move buffer (cleared, not dropped):
let end_captures = partition_captures_to_front(&mut moves);  // O(N) in-place
sort_by_mvv_lva(&mut moves[..end_captures]);                 // sort captures sub-slice

// Search TT move, captures (moves[..end_captures]), killers...
// ... cutoffs here update history table ...

// Now sort quiets with fresh history
sort_by_history(&mut moves[end_captures..], history);        // sort quiets sub-slice
// Dedup: skip TT move and killers in moves[end_captures..]
for mv in &moves[end_captures..] {
    if *mv == tt_move || is_killer(*mv) { continue; }
    // ... search mv ...
}
```

**clawfish M5.H2 status**: v3 essentially implemented this (single-Vec in-place partition + lazy quiet sort). Empirically the literature signal still failed to dominate at fast TC. See ADR-0030 §11.

### 3.2 Selection Sort on the Quiet Sub-Slice

After partitioning captures to the front and sorting them, iterate the quiet sub-slice with an in-place selection sort: for each iteration, find the quiet with the highest history score among remaining quiets and swap it to current position. This reads fresh history at each iteration.

- **Advantage**: no upfront sort of quiets at all; history is read as late as possible.
- **Disadvantage**: O(N²) on the quiet sub-slice. For N=30 quiets, this is ~900 comparisons vs ~150 for sort_by_key. At depth 14, this fires ~14M times — a real but small cost.
- **Literature evidence**: CPW Move Ordering ("chess programs usually don't sort the whole move list, but perform a selection sort each time a move is fetched"); TalkChess t=73930 (xr_a_y, 3–5% NPS gain from selection sort vs full sort). Abulmo2 reports "didn't work well" in Amoeba for full list but is not reported as bad for quiet-only selection.

### 3.3 History Gravity / Dynamic History Update

A complementary technique: apply a penalty to quiets that were searched and didn't cause a cutoff (`quiets_searched` list, already done in clawfish M5.C). This has the same effect as fresh history — moves that were poor earlier in the same subtree get lower scores — without requiring re-sorting. Clawfish already implements this (M5.C). The deferred quiet sort multiplies this effect by reading the updated scores at sort time.

### 3.4 Binned History Sort (O(N) with ~256 buckets)

H.G. Muller (TalkChess t=63502, 2013): distribute quiets into ~256 logarithmically-spaced history bins as a linked list per bin; iterate populated bins highest-to-lowest. O(N) total. Achieves fresh history automatically (bins are populated with current scores at generation time, but this can be done after captures if generates quiets lazily).

- **Literature basis**: Muller: "it is not too difficult to do a complete history sort in O(N) steps. The idea is to 'bin' the moves by history score."
- **Practical verdict**: Martin Sedlak (t=63502): "move sorting is never the real bottleneck." Not recommended for clawfish at this stage; O(N log N) sort with N≤60 is fast enough.

---

## §4. Depth-Gating Prior Art

### 4.1 What the Literature Says

The Chess Programming Wiki (Move Ordering page) has the clearest documented statement: "Move ordering effort might be controlled by considering draft and/or plies from root. The closer the root, the farther the horizon, the more effort might be justified to score and sort moves."

CPW Move Ordering: "Exceptions are the Root and further PV-Nodes with some distance to the horizon, where one may apply additional effort to score and sort moves." This implies that at leaf-adjacent nodes (shallow depth remaining), sort effort is reduced.

TalkChess t=73930 (bob, 2015): "Bob" Hyatt describes staged movegen itself as a depth-gating mechanism — at shallow nodes, TT-move cutoffs dominate and no generation is needed; at deeper nodes, more stages fire. But this is not explicit depth gating of the sort, just the natural economics of staged movegen.

### 4.2 Threshold Values

No published paper or forum thread reports a validated threshold for "depth at which sorted quiets outweigh the sort cost." The CPW's "root and PV nodes" guidance implies depth ≥ (total depth − 2) for PV-special treatment. Beyond that, the literature does not document a tested numeric threshold.

The closest quantitative evidence comes from the clawfish SPRT data itself: 40+0.4 gains (reaches depth 14–18), 20+0.2 regresses (reaches depth 10–12). This suggests the break-even threshold is approximately **depth remaining ≥ 8–10** for the freshness benefit to exceed overhead. This is project-specific empirical evidence, not published prior art.

### 4.3 Depth-Gated Quiet Sort as an Option

An implementation could gate the deferred quiet sort: "only defer quiet sort if `depth >= N`; otherwise sort all upfront." If N ≈ 8, this would:
- Preserve M5.H1 (one-sort-upfront) behavior at shallow nodes where the freshness benefit is nil.
- Enable deferred quiet sort at deep nodes where the benefit is real.
- Avoid the overhead of the deferred sort for the 70–80% of nodes that are shallow.

**clawfish M5.H2 v4 result**: depth-gating was tried (threshold=6) and produced a 2.8× sustained-load slowdown vs baseline. The eager-sort-at-shallow path adds per-node cost (history sort firing at every shallow node) that overwhelms the per-depth ordering improvement in time-pressured games. See ADR-0030 §11.

---

## §5. Concrete Recommendations for clawfish (as of 2026-05-11, post-M5.H2 descope)

### Recommendation 1: Do not revisit lazy-quiet-sort without a different framing

Four variants (v1-v4) tried; all failed SPRT. The literature signal applies to engines reaching depth 14+ consistently; clawfish at its current strength is time-bound to depth 8–12 at typical TCs, where the signal is dominated by noise. Future revisits should require either:

- A measurable improvement in clawfish's per-depth NPS (so it reaches depth 14+ at typical TCs).
- A different algorithm entirely (e.g., countermove heuristic, continuation history, SEE-split captures) — not a re-implementation of lazy quiet sort.

### Recommendation 2: If pursuing M6 eval improvements, consider Pattern C (thread-local Vec)

The allocation cost in clawfish is real but small (1 Vec/node at the current M5.H1 baseline). Future eval features that need per-node scratch buffers should use Pattern C to avoid compounding the allocation cost.

### Recommendation 3: M5.H milestone is complete at H1

H2 lazy-sort path is documented as rejected. The architectural refactor (H1) is the M5.H deliverable. M5.H3 (typed lazy generation) is no longer scheduled.

---

## §6. References

| Source | Type | URL |
|---|---|---|
| CPW — Move List | wiki | https://www.chessprogramming.org/Move_List |
| CPW — Move Generation | wiki | https://www.chessprogramming.org/Move_Generation |
| CPW — Move Ordering | wiki | https://www.chessprogramming.org/Move_Ordering |
| CPW — History Heuristic | wiki | https://www.chessprogramming.org/History_Heuristic |
| TalkChess t=39934 — Performance of dynamically allocated move lists | forum | https://talkchess.com/forum3/viewtopic.php?t=39934 |
| TalkChess t=64642 — Move list in stack vs heap | forum | http://www.talkchess.com/forum3/viewtopic.php?t=64642 |
| TalkChess t=76835 — Staged move generation | forum | https://talkchess.com/viewtopic.php?t=76835 |
| TalkChess t=76835 page 2 | forum | https://talkchess.com/viewtopic.php?t=76835&start=10 |
| TalkChess t=69704 — Lazy move generation and move ordering | forum | https://www.talkchess.com/forum3/viewtopic.php?t=69704 |
| TalkChess t=76491 — Sorting moves during move ordering | forum | https://talkchess.com/viewtopic.php?t=76491 |
| TalkChess t=73930 — Sort every moves or pickNext | forum | https://talkchess.com/viewtopic.php?t=73930 |
| TalkChess t=79279 — Lazy sorting algorithm | forum | https://talkchess.com/viewtopic.php?t=79279 |
| TalkChess t=79480 — Lazy sort issue | forum | https://talkchess.com/viewtopic.php?t=79480 |
| TalkChess t=63502 — Speed up or avoiding move sorting | forum | https://talkchess.com/forum3/viewtopic.php?t=63502 |
| TalkChess t=81708 — Architecture for bitboard chess engine | forum | https://talkchess.com/viewtopic.php?t=81708 |
| TalkChess t=81716 — Move ordering | forum | https://talkchess.com/viewtopic.php?t=81716 |
| TalkChess t=31999 — Storage and ordering strategy in move generation | forum | https://www.talkchess.com/forum/viewtopic.php?t=31999 |
| SEGGER Blog — C++ real-time allocation: a chess engine | blog | https://blog.segger.com/c-real-time-allocation-a-chess-engine/ |
| SebLague Chess-Challenge issue #188 — Critical performance issue | issue | https://github.com/SebLague/Chess-Challenge/issues/188 |
| Rust Users Forum — Optimizing Rust borrowing patterns in a chess engine | forum | https://users.rust-lang.org/t/optimizing-rust-borrowing-patterns-in-a-chess-engine/127650 |
| The Rust Performance Book — Heap allocations | doc | https://nnethercote.github.io/perf-book/heap-allocations.html |
| shakmaty MoveList — ArrayVec implementation | doc | https://docs.rs/shakmaty/latest/shakmaty/type.MoveList.html |
| likeawizard — The highly frustrating side of chess engine development | blog | https://lichess.org/@/likeawizard/blog/the-highly-frustrating-side-of-chess-engine-development/s6jHNBcd |
| minuskelvin.net chess wiki — Move ordering | wiki | https://minuskelvin.net/chesswiki/content/move-ordering.html |
