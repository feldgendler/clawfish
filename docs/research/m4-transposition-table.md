# Prior-Art Research: Transposition Table Design for M4.A

Sources consulted: Chess Programming Wiki (CPW), Mediocre Chess blog (Jonatan Dahl), TalkChess forums (H.G. Muller, Bob Hyatt, et al.), Breuker et al. 1994 "Replacement Schemes for Transposition Tables" (ICCA Journal), Stockfish UCI docs.

Per ADR-0003, no third-party engine source code was read; CPW articles, papers, blog posts, and forum threads were the source material.

---

## 1. Replacement Scheme

### Schemes Compared

| Scheme | Description | Strengths | Weaknesses |
|---|---|---|---|
| Always-replace | Any new entry overwrites the slot | Fresh entries always available; simple | Deep entries evicted freely; no depth-based pruning gain |
| Depth-preferred | Overwrite only if new depth ≥ stored depth | Preserves expensive deep analyses | Stale entries survive indefinitely; table fills with old positions |
| Two-tier (Thompson-Condon) | Two slots per index: one depth-preferred, one always-replace | Preserves both deep and recent data | 2× memory per logical slot; more complex |
| Bucket (N-tier) | N slots per index; replace the shallowest entry | Balances depth and recency at N-way granularity | Cache-line sizing constrains N; more complex |
| Depth-preferred + age bias | Overwrite if new_depth ≥ old_depth − K, OR if old entry is from a previous search generation | Flushes stale entries without fully sacrificing depth-preference | Age field required; K is a tuning parameter |

### Research Findings

- Breuker et al. 1994 compared seven schemes on middlegame positions; a two-level scheme using subtree-node-count as the criterion performed best and was "provisionally recommended." [(Semantic Scholar)](https://www.semanticscholar.org/paper/Replacement-Schemes-for-Transposition-Tables-Breuker-Uiterwijk/1db6bdd7b588c35d01c7f8a5454a610664789ff4)
- CPW notes "most well-performing replacement strategies use a mix" of depth and recency, with "recent" being always more important than any other single factor. [(CPW Transposition Table)](https://www.chessprogramming.org/Transposition_Table)
- HGM (TalkChess): difference between "a very simple TT and a very complex one will make only very little marginal difference on engine strength." A simple scheme is correct to start with. [(TalkChess t=76508)](https://talkchess.com/viewtopic.php?t=76508)
- The key invariant: "New positions should always go into the table, no matter how precious the entry seems that would have to be overwritten." [(CPW)](https://www.chessprogramming.org/Transposition_Table)
- "Undercut replacement" variant (HGM): allow overwrite if new_depth ≥ old_depth − 1; gradually flushes stale depth-preferred entries. [(TalkChess t=76499)](https://talkchess.com/viewtopic.php?t=76499)

### Recommendation for M4.A

**Default: depth-preferred with age bias** (overwrite if `new_depth >= old_depth`, OR if old entry's age generation != current generation). Rationale:
- Simpler than two-tier; avoids table stagnation of pure depth-preferred.
- The age field doubles as a probe-refresh mechanism (touch on probe; replace old-gen entries freely).
- Directly compositional with M9 Lazy SMP: the generation counter remains meaningful in a shared TT because threads increment together.

**Second choice:** Two-tier (one depth-preferred + one always-replace per bucket). Widely cited, well-understood, but requires 2× slots or half the effective entries at the same MiB budget.

---

## 2. Entry Key Discipline

### Options

| Option | Key stored | Collision detection | Thread-safety for Lazy SMP | Cost |
|---|---|---|---|---|
| Full 64-bit Zobrist | All 64 bits in entry | Strong; ~1 collision per 4B probes | Non-atomic 16-byte write; can produce torn reads under Lazy SMP | 8 bytes of key per entry |
| Partial 32-bit | Upper/lower 32 bits | Moderate; 1 collision per ~20 × 200M-node searches | Still non-atomic | 4 bytes of key per entry |
| XOR trick (lockless) | `stored_key = zobrist XOR data_word` | Detects torn writes; any partial overwrite corrupts XOR check | Lock-free and safe for Lazy SMP | Requires 64-bit data word; no extra key bytes if data fits |

### Key Facts

- XOR trick: `hashtable[idx].key = key ^ data; hashtable[idx].data = data`. On probe: `(stored_key ^ stored_data) == search_key`. If another thread partially overwrites data, the XOR check fails and the entry is discarded. [(Binary Debt blog)](https://binarydebt.wordpress.com/2013/09/29/lockless-transposition-tables/)
- Texel's Lazy SMP implementation uses the XOR trick for its shared TT. [(TalkChess t=64824)](https://talkchess.com/viewtopic.php?t=64824)
- With full 64-bit single-threaded: "torn reads" are impossible; the entry is either fully written or not written at all (no parallel writer). The XOR trick adds no value for correctness in single-threaded context. [(CPW Shared Hash Table)](https://www.chessprogramming.org/Shared_Hash_Table)
- Cost of migrating to lockless at M9: if M4.A stores a full 64-bit key separately from data, moving to XOR-trick requires restructuring the data word. If the entry is laid out with `key_xor_data | data` from day one, M9 is a structural no-op.

### Recommendation for M4.A

**Default: full 64-bit Zobrist key stored in entry.** Single-threaded at M4; lockless XOR trick solves a concurrency problem that doesn't exist yet, and adds complexity for zero benefit now.

**Migration path to M9:** Restructure the entry as a `(key_lo: u32, data: TtData)` at M9.A, with `key_lo = (zobrist >> 32) ^ data_as_u32`. No M4.A code needs to anticipate this — it's a clean M9 internal refactor. The full 64-bit key does not "paint us into a corner."

**Second choice:** 32-bit partial key (upper 32 bits of Zobrist). Saves 4 bytes per entry, allowing ~33% more entries for the same MiB. With a 16 MiB default this matters, but at classical-eval strength the collision rate with 32 bits is acceptable.

---

## 3. Per-Entry Packing

### Field Inventory and Bit Widths

| Field | Recommended width | Notes |
|---|---|---|
| Zobrist key | 64 bits (8 bytes) | Full key for single-threaded; see §2 |
| Score | 16 bits (i16) | Range ±32767; well above ±30000 MATE constant |
| Depth | 8 bits (i8 or u8) | Max depth 127 (i8) or 255 (u8); u8 sufficient for max_ply 64 |
| Bound type | 2 bits | Exact / LowerBound (beta-cutoff) / UpperBound (fail-low); 3 values → 2 bits |
| Best move | 16 bits | Matches existing `Move` encoding (from/to/4-bit flag per ADR) |
| Age/generation | 4–6 bits | A 4-bit counter wraps at 16 searches; 6-bit at 64 — ample for typical game length |

### Example 16-byte Layout

```
Offset  Bits   Field
0       64     key: u64           (Zobrist, full)
8       16     score: i16
10       8     depth: u8
11       8     age_and_bound: u8  (bits 7-2: age 6 bits; bits 1-0: bound type 2 bits)
12      16     best_move: u16
14      16     (padding to align to 16 bytes)
```

Total: 16 bytes. Fits in one cache line together with adjacent entries in a 4-entry bucket. [(TalkChess t=65327)](https://talkchess.com/forum3/viewtopic.php?t=65327) [(CPW)](https://www.chessprogramming.org/Transposition_Table)

**Alternative 12-byte layout** (if 4-byte key reduction is chosen):
- Drop full 64-bit key to 32-bit → save 4 bytes → entries shrink to 12 bytes.
- Pack `score(16) + depth(8) + bound(2) + age(6) + move(16)` = 48 bits = 6 bytes.
- Total: 32+48 = 80 bits = 10 bytes (pad to 12 or 16 for alignment).

The 16-byte layout is the standard industry choice for cache-line cleanliness. 4-entry buckets of 16-byte entries fill a 64-byte cache line exactly.

---

## 4. Hash UCI Option Default + Bounds

### Industry Conventions

| Engine | Default (MiB) | Min (MiB) | Max (MiB) |
|---|---|---|---|
| Stockfish | 16 | 1 | 33,554,432 |
| Defenchess | 16 | (not stated) | (not stated) |
| Euwe | 16 | (not stated) | (not stated) |
| Komodo | 128 | (not stated) | (not stated) |

- 16 MiB is the consensus default for most engines. [(Stockfish UCI docs)](https://official-stockfish.github.io/docs/stockfish-wiki/UCI-&-Commands.html)
- UCI spec says "Hash tables" (plural) — the setting covers all significant dynamically-allocated hash memory. Kill-move tables, history tables, static lookup tables are negligible and need not be counted. [(TalkChess t=67878)](https://talkchess.com/forum3/viewtopic.php?t=67878)
- `ucinewgame` must be followed by `isready`/`readyok` because clearing a large TT takes non-trivial time.

### Recommendation for M4.A

- **Default:** `Hash spin default 16 min 1 max 4096` (MiB).
  - 16 MiB matches Stockfish default; appropriate for single-threaded classical eval.
  - 4096 MiB upper bound is realistic for modern desktops (Apple Silicon M-series supports it).
  - The bench corpus is 16 positions; at ~1 MiB per position's working set, 16 MiB is ample for deterministic bench. Larger hash does not hurt bench determinism if TT is cleared between positions (see §8).
- **Type:** `spin` (integer, MiB).
- **Allocation:** size rounded down to the nearest power of two in entries (for fast modulo via bitmask). With 16-byte entries: 16 MiB / 16 bytes = 1,048,576 entries.

---

## 5. Mate-Score Depth-Adjust on Store/Probe

### Problem Statement

- Mate-in-N scores are path-length-relative: the N counts plies from the **current node** to the mating position.
- If position P is found via path A at ply 5, and stored as "mate in 3" (score = MATE − 3), and later found via path B at ply 8, the stored score would be interpreted as "mate in 3 from ply 8" — off by 3.
- Correction: store "distance from current node to mate," independent of distance from root. [(CPW Score)](https://www.chessprogramming.org/Score)

### Standard Formula

Conventions below use:
- `MATE = 30000` (project constant, per M3.C).
- `MAX_PLY = 64` (project max search depth).
- `ply` = current node's distance from root (0 at root).
- Positive MATE score = the side to move will deliver checkmate; negative = will be mated.

**Detection threshold:** A score is a mate score if `|score| > MATE − MAX_PLY`, i.e. `|score| > 29936`. With `MATE = 30000`, `MAX_PLY = 64`: threshold = `30000 − 64 = 29936`.

**Store side (score_to_tt):**
```
fn score_to_tt(score: i16, ply: i32) -> i16 {
    if score > 29936 {
        score + ply as i16   // "mate in N" → store as "mate in N-ply from here"
    } else if score < -29936 {
        score - ply as i16   // "mated in N" → store as "mated in N-ply from here"
    } else {
        score
    }
}
```

**Probe side (score_from_tt):**
```
fn score_from_tt(score: i16, ply: i32) -> i16 {
    if score > 29936 {
        score - ply as i16   // restore ply-relative mate distance
    } else if score < -29936 {
        score + ply as i16   // restore ply-relative mated distance
    } else {
        score
    }
}
```

**Worked example (negamax convention):**
- At root ply 0, engine finds forced mate. The engine returns score `MATE − p` where p is the depth of the mate node from root.
- At ply `p = 3`, the engine returns score `MATE − 3 = 29997` (meaning "I can force checkmate in 3 more plies").
- `score_to_tt(29997, 3)`: since `29997 > 29936`, stored = `29997 + 3 = 30000`.
- At ply `p = 5` (same position reached by transposition), probe returns `score_from_tt(30000, 5) = 30000 − 5 = 29995`.
- `MATE − 29995 = 5`, i.e., "checkmate in 5 more plies from here." Correct — from ply 5 the original mate sequence still terminates at the same absolute ply in the tree.

**Sign rationale:**
- Store: add ply for positive mate (mates are "closer to root" than they appear from deeper); subtract ply for negative (mated-scores are symmetrically adjusted).
- Probe: subtract ply for positive; add for negative. Round-trip: `score_from_tt(score_to_tt(s, p), p) = s`. ✓ [(CPW Score page; TalkChess t=37016)](https://talkchess.com/forum3/viewtopic.php?t=37016)

**Literature source for formula:** The exact formula above (`+ply on store, −ply on probe` for positive mate) appears as the standard in CPW's Score article and is corroborated by the pseudocode extracted from TalkChess discussions. [(CPW Score)](https://www.chessprogramming.org/Score)

---

## 6. Probe-But-Don't-Store Inside Qsearch

### Literature Survey

- The "probe-but-don't-store" pattern appears in TalkChess discussions; HGM: "For my engines probing in QS always has proved a big win." [(TalkChess t=47373)](https://talkchess.com/viewtopic.php?t=47373)
- Bob Hyatt (Crafty): tested full TT in qsearch; "hash in q-search slows the search down a bit due to significantly increased memory traffic, but the search tree shrinks. They seemed to perfectly offset each other." Chose to omit to reduce TLB thrashing.
- One implementation reported +25 Elo at 5s+100ms with 32 MB hash from full qsearch TT. Highly engine-dependent.
- Jon Dart (Arasan): separate eval cache for qsearch nodes rather than the main TT, to avoid TT pressure.
- CPW lists "Transposition table usage in quiescent search?" as an open discussion topic with no authoritative guidance. [(CPW Quiescence Search)](https://www.chessprogramming.org/Quiescence_Search)
- The +5–15 Elo figure cited in the roadmap is consistent with the forum range; full qsearch-in-TT (probe + store) is the M5 target.
- "Probe-but-don't-store" is a documented intermediate: catches hits when qsearch interior nodes happen to match a previously stored negamax entry, without polluting the TT with shallow qsearch results.

### Recommendation for M4.A

**Defer entirely.** Do not probe the TT inside qsearch in M4.A.

Rationale:
- Benefit is empirically uncertain (zero in Crafty, +25 Elo in one other engine).
- Adds complexity at M4.A where the TT itself is being established.
- The "probe-but-don't-store" intermediate does not compose cleanly without a separate depth field in qsearch — qsearch nodes have no meaningful "depth" in the negamax sense.
- Full qsearch-in-TT is the M5 target and is better designed with M4's experience in hand.

**Second choice:** probe-but-don't-store at `depth == 0` (horizon only), as one TalkChess developer found it worth checking the TT only at the negamax/qsearch boundary. Low implementation cost; adds one probe per horizon node.

---

## 7. Best-Move Preservation

### Problem

An entry stores a best-move from a previous (e.g., fail-high) result. A later search of the same position fails low (no move beat alpha), producing an upper-bound entry with no best move. If the fail-low entry overwrites the old entry, the move-ordering hint is lost.

### Literature

- CPW: "Even if the depth of the related TT entry is not big enough...a best move from a previous search can improve move ordering and save search time. Moves from earlier searches stored in TT tend to be very good." [(CPW TT)](https://www.chessprogramming.org/Transposition_Table)
- Standard practice: **preserve the old best-move when storing a new fail-low (upper-bound) entry**, by copying `old_entry.best_move` into the new entry.
- Alternative: do not overwrite the entry at all if the new result has no best move and the existing entry has one (partial-update policy).

### Recommendation

**On overwrite, if new bound is UpperBound (fail-low) and new entry has no best move, copy `old_entry.best_move` into the new entry** before writing. This requires reading the old entry's move before overwriting. Cost: one extra load (already in cache since we just probed).

---

## 8. `ucinewgame` and Hash Resize Semantics

### `ucinewgame`

- **Stockfish behavior:** `ucinewgame` clears the TT and resets all search state. The command is followed by `isready`/`readyok` because the clear is not instant. [(Stockfish UCI docs)](https://official-stockfish.github.io/docs/stockfish-wiki/UCI-&-Commands.html)
- **Practical consensus** (TalkChess t=77569): most production engines clear TT only on `ucinewgame`, not between `go` commands within the same game. Aging handles stale entries within-game. [(TalkChess t=77569)](https://talkchess.com/viewtopic.php?t=77569)
- **For bench determinism:** the bench command must clear TT (and other per-game state) between positions. This is independent of the `ucinewgame` clearing policy.

### Hash Resize Mid-Session

- Resize always implies rebuild-and-clear. Preserving entries after resize is not practical because index computation changes with table size.
- **Industry convention:** allocate new Vec, zero-initialize, replace old.

### Recommendation

- `ucinewgame`: clear TT + reset `game_history` + reset age counter to 0 + reset killer + reset history.
- `Hash` option change (mid-session resize): rebuild-and-clear (new Vec allocation, zero-init).
- Bench: between positions, call a `reset_game_state()` method that does the same as `ucinewgame`.

---

## 9. Age Semantics

### When to Increment

- **Per `go` command** (i.e., per root search): the overwhelming consensus. Age tracks "which game position search are we in?" not which ID iteration. [(CPW TT)](https://www.chessprogramming.org/Transposition_Table) [(TalkChess t=76499)](https://talkchess.com/viewtopic.php?t=76499)
- Per ID iteration: wrong — would age out entries from earlier iterations of the same root search, preventing cross-iteration reuse (the main benefit of ID + TT).
- HGM: "a search counter that you increment for every new search" (i.e., per `go`). A 2-bit counter suffices for basic "same generation vs. old" distinction. [(TalkChess t=76499)](https://talkchess.com/viewtopic.php?t=76499)

### Interaction with Replacement

- Age + depth-preferred: an entry is replaced if `new_depth >= old_depth`, OR if `old_age != current_age`.
- This means: all entries from the previous root search are freely replaceable, regardless of depth. Only entries from the current search compete on depth.
- 4–6 bits for age field. With 4 bits: wraps at 16 root searches. With 6 bits: wraps at 64. Either is ample for a game.

### Recommendation

- Increment age counter by 1 on each `go` command (before the search starts).
- Store current age in each TT entry on write.
- On probe: stored age does not affect probe correctness (the score is still valid for cutoffs; the age field only affects replacement). Use the stored move for ordering and stored score/bound for cutoffs regardless of age.

---

## 10. Graph History Interaction (GHI)

### The Problem

- The Polyglot Zobrist key (ADR-0009) does not encode: game-path repetition history, or the halfmove clock (50-move rule).
- A TT entry for position P stores a score from a prior search where P had, say, 10 halfmoves. At a later probe, P has 45 halfmoves — the score may be wrong (the 50-move deadline is imminent, changing the game-theoretic value).
- Similarly, a score that did not account for a repetition draw becomes wrong when the same position has appeared twice already.
- Academic work: Kishimoto & Müller 2004 ("A General Solution to the Graph History Interaction Problem") offers a formal solution but is complex. [(AAAI 2004)](https://cdn.aaai.org/AAAI/2004/AAAI04-102.pdf)

### Practical Approaches

| Option | Description | Elo cost | Complexity |
|---|---|---|---|
| Option 1: Live with it | Accept incorrect scores near 50-move boundary or in repetition lines | Small in practice; rare positions | Zero |
| Option 2: Suppress probe/store | Skip TT probe/store when `halfmove_clock > threshold` (e.g., 80+) | None in most games | Low |
| Option 3: Encode path state in key | XOR halfmove clock or repetition count into the Zobrist key | None | High; invalidates all entries on each ply |

- CPW notes most programs "do not" clear hash tables between searches; GHI is "accepted in practice" for repetition. [(CPW Repetitions)](https://www.chessprogramming.org/Repetitions)
- The repetition check is the primary defense: engines check for repetitions **before** the TT probe, so repetition draws are correctly detected before a cached score is returned. [(TalkChess repetition discussion)](https://talkchess.com/viewtopic.php?t=22968)
- The 50-move GHI is a genuine but rare problem; its practical Elo impact at classical-eval strength is small.

### Recommendation for M4.A

**Option 1: live with it**, with one explicit defense: the repetition check runs **before** the TT probe in the node prologue (see §13). This neutralizes the most common GHI manifestation (draw-by-repetition mis-scoring).

The 50-move boundary issue is acknowledged and deferred. If it surfaces during SPRT analysis (unusual early-draw or late-draw miscounting), Option 2 can be added as a 5-line guard.

**Document in M4.A ADR as an explicit choice**, not an omission.

---

## 11. PV-Node vs. Non-PV-Node Probe Discipline

### The Principle

- TT cutoffs at PV nodes shorten the displayed PV ("the problem of short principal variations") because an exact-score hit returns immediately without searching all moves. [(CPW TT)](https://www.chessprogramming.org/Transposition_Table)
- "In more advanced engines transposition table cutoffs are not performed on PV-Nodes." [(CPW TT)](https://www.chessprogramming.org/Transposition_Table)
- PV nodes still benefit from TT for move ordering (try stored best-move first).

### How to Distinguish PV Nodes

**Under PVS (M4.D):** PV nodes have `beta − alpha > 1` (open window); non-PV nodes have `beta − alpha == 1` (null window). This is the standard predicate. [(CPW PVS)](https://www.chessprogramming.org/Principal_Variation_Search) [(CPW Node Types)](https://www.chessprogramming.org/Node_Types)

**Under pure alpha-beta (M4.A):** There is no null-window call. All nodes have the same window type semantically. However, in practice with triangular PV recovery (M3.C), the distinction is still meaningful:

- The root node is always a PV node.
- A node is a PV node if it has been reached by following the PV from the previous iteration. Under M3.E's `prior_root_move` ordering and triangular PV, the PV path is the leftmost path at each depth.
- **Practical rule for M4.A (pre-PVS):** track PV vs. non-PV with a boolean `is_pv` flag passed as a parameter, set `true` for the root and for the first child of any PV node, `false` for all other children.
- At PV nodes: allow TT probe for move ordering only; do not return early on TT score (even exact match).
- At non-PV nodes: full TT cutoff permitted.

**Concrete rule for M4.A:** pass `is_pv: bool` into `negamax`. At non-PV nodes, apply full bound comparison (lower-bound ≥ beta → cutoff, upper-bound ≤ alpha → return, exact within window → return). At PV nodes, use TT entry only for move ordering; skip the score-based return.

---

## 12. TT Best-Move Legality Check Before Use

### Findings

- With full 64-bit key: collision rate is ~1 per 4 billion probes. At 200M nodes/search, ~1 collision per 20 searches. Most collisions produce incorrect but valid moves; ~1% produce moves that expose the king (crash risk). [(TalkChess t=54941)](https://talkchess.com/viewtopic.php?t=54941)
- For single-threaded engines, race conditions are impossible. The main risk is hash collision only.
- Under partial-key or XOR-trick: higher effective collision rate; legality check is more important.
- Recommended minimum check (Bob Hyatt's formulation): verify that (a) the source square has a piece belonging to side-to-move, (b) the destination square is not occupied by a friendly piece, (c) for castling, king is on the correct square. Cost is "immeasurable" in profiling. [(TalkChess t=54941)](https://talkchess.com/viewtopic.php?t=54941)
- Our engine uses legal-direct movegen (ADR-0007). The simplest legality check: generate moves and check if the TT move is in the list. Cost: one movegen call per TT hit with a stored move. More expensive than the minimal check; may not be worth it.

### Recommendation for M4.A

**Full 64-bit key + simple "is in legal-move list" check.** Since we already generate the legal move list in the node body, an `is_member(tt_move, &moves)` check is a single linear scan. Cost is bounded (max ~218 legal moves; typical <40); we can prepend the TT move to the ordered list only if found. This is correct by construction and trivially testable.

**Second choice:** structural pseudo-legality check (source piece, destination piece, flag consistency) without movegen. Faster but more error-prone for promotions/EP/castling edge cases. Defer to a later optimization phase when profiling shows the legal-list scan as a hotspot.

**If migrating to partial-key at M9:** legality check remains mandatory.

---

## 13. Per-Node Prologue Ordering

### Recommended Sequence (with citations)

1. **Repetition / 50-move draw check** — before TT probe. Rationale: TT may have a cached non-draw score for a position that is now a draw by repetition; checking first prevents returning the stale score. "(Normally you check for repetitions before probing the hash table.)" [(TalkChess t=22968)](https://talkchess.com/viewtopic.php?t=22968) [(CPW Repetitions)](https://www.chessprogramming.org/Repetitions)

2. **Mate distance pruning (MDP) — tighten alpha/beta.** Run after draw-check (draw scores don't benefit from MDP) and before TT probe. Tightening the window first allows more TT entries to produce cutoffs.

3. **TT probe**, using the post-MDP window `(alpha, beta)`.

4. **`score_from_tt(stored_score, ply)`** — apply mate-score adjustment to the stored score before comparing to alpha/beta.

5. **Bound comparison vs. post-MDP window → cutoff or fall-through.** At non-PV nodes: lower-bound ≥ beta → return; upper-bound ≤ alpha → return (with the TT move available for ordering); exact → return. At PV nodes: no early return on score; use TT move for ordering only.

### Notes on CPW Ordering Discussions

For pure negamax with explicit repetition detection, the standard advice is: repetition check first, then MDP, then TT probe. Per-engine variants differ on the placement of check-extensions and qsearch entry, but the core ordering (rep → MDP → TT) is consistent.

---

## 14. Per-Game State Inventory for `bench` Reset

### State That Must Be Reset Between Bench Positions

| State | Reset needed | Rationale |
|---|---|---|
| TT | Yes — clear or age-increment | Position order in the 16-position corpus must not affect node counts |
| `game_history` (Vec<u64>) | Yes — clear | Repetition detection would give false draws if prior position keys persist |
| Age counter | Yes — reset to 0 (or to a fresh generation) | Without reset, entries from position N appear "old" to position N+1, changing replacement behavior |
| `prior_root_move` | Yes — clear (Option 3: reset to None) | Must not carry a root ordering hint from a prior bench position |
| Killer moves (M4.B) | Yes — clear all slots | Killers from prior position affect move ordering in next |
| History table (M4.C) | Yes — clear (zero all entries) | History scores from prior position corrupt ordering in next |

**Industry guidance:** "clear the TT before every search...along with the killer and history tables...to make sure your search is completely deterministic." [(TalkChess t=77569)](https://talkchess.com/viewtopic.php?t=77569)

### Recommendation

Implement a single `reset_for_new_game()` method on `Engine` (or an equivalent struct) that clears TT, game_history, age counter, prior_root_move, killers (M4.B), and history table (M4.C). Call this method:
- In `handle_ucinewgame`.
- At the start of each position in `handle_bench`.
- **Not** between ID iterations within a single `go` search.

This is the same "per-game reset" contract, not "per-search reset."

---

## Source List

- [CPW Transposition Table](https://www.chessprogramming.org/Transposition_Table)
- [CPW Score (mate scores)](https://www.chessprogramming.org/Score)
- [CPW Node Types](https://www.chessprogramming.org/Node_Types)
- [CPW Shared Hash Table](https://www.chessprogramming.org/Shared_Hash_Table)
- [CPW Quiescence Search](https://www.chessprogramming.org/Quiescence_Search)
- [CPW Repetitions](https://www.chessprogramming.org/Repetitions)
- [CPW Graph History Interaction](https://www.chessprogramming.org/Graph_History_Interaction)
- [CPW Principal Variation Search](https://www.chessprogramming.org/Principal_Variation_Search)
- [Mediocre Chess TT guide](http://mediocrechess.blogspot.com/2007/01/guide-transposition-tables.html)
- [TalkChess: Best practices for TT (t=76508)](https://talkchess.com/viewtopic.php?t=76508)
- [TalkChess: Replacement scheme (t=76499)](https://talkchess.com/viewtopic.php?t=76499)
- [TalkChess: TT age tutorial (t=59047)](https://talkchess.com/forum3/viewtopic.php?start=30&t=59047)
- [TalkChess: QSearch TT usage (t=47373)](https://talkchess.com/viewtopic.php?t=47373)
- [TalkChess: Mate handling TT (t=15496)](https://talkchess.com/viewtopic.php?t=15496)
- [TalkChess: Puzzle mate scores TT (t=37016)](https://talkchess.com/forum3/viewtopic.php?t=37016)
- [TalkChess: Legality check TT move (t=54941)](https://talkchess.com/viewtopic.php?t=54941&start=20)
- [TalkChess: TT legality checking (t=82494)](https://talkchess.com/viewtopic.php?t=82494)
- [TalkChess: Repetition detection (t=22968)](https://talkchess.com/viewtopic.php?t=22968)
- [TalkChess: UCI Hash rules (t=67878)](https://talkchess.com/forum3/viewtopic.php?t=67878)
- [TalkChess: When to clear TT (t=77569)](https://talkchess.com/viewtopic.php?t=77569)
- [Breuker et al. 1994 - Replacement Schemes (Semantic Scholar)](https://www.semanticscholar.org/paper/Replacement-Schemes-for-Transposition-Tables-Breuker-Uiterwijk/1db6bdd7b588c35d01c7f8a5454a610664789ff4)
- [Binary Debt: Lockless TT](https://binarydebt.wordpress.com/2013/09/29/lockless-transposition-tables/)
- [Stockfish UCI docs](https://official-stockfish.github.io/docs/stockfish-wiki/UCI-&-Commands.html)

---

## Summary of Recommendations (Quick Reference)

| # | Question | Recommendation | Second choice |
|---|---|---|---|
| 1 | Replacement scheme | Depth-preferred + age bias (replace if new_depth ≥ old_depth OR old_gen ≠ current_gen) | Two-tier (depth-preferred + always-replace) |
| 2 | Entry key discipline | Full 64-bit Zobrist; XOR trick deferred to M9 | 32-bit partial key (saves 4 bytes/entry) |
| 3 | Entry packing | 16 bytes: u64 key + i16 score + u8 depth + u8 (age:6 + bound:2) + u16 move + u16 pad | 12 bytes with 32-bit partial key |
| 4 | Hash UCI option | `spin default 16 min 1 max 4096` (MiB) | default 32 for larger dev boxes |
| 5 | Mate-score adjustment | `score_to_tt = mate_score + ply`; `score_from_tt = stored − ply` (positive mate); inverted for negative; threshold `|score| > MATE − MAX_PLY = 29936` | Delayed-loss bonus (Micro-Max style) |
| 6 | Probe-but-don't-store in qsearch | Defer entirely to M5 | Probe-only at depth == 0 horizon |
| 7 | Best-move preservation | Copy old entry's best-move into new fail-low entry | Keep old entry if new has no best move |
| 8 | `ucinewgame` / resize | Clear TT + all game state on `ucinewgame`; resize always rebuild-and-clear | — |
| 9 | Age semantics | Increment per `go` command; 4–6 bit field | — |
| 10 | GHI | Option 1: live with it; repetition check before TT probe as the primary defense | Option 2: suppress probe when halfmove_clock > 80 |
| 11 | PV-node discipline | Pass `is_pv: bool`; at PV nodes, use TT for ordering only (no score cutoffs) | Allow exact-match returns at PV nodes only |
| 12 | TT-move legality | "Is in legal-move list" check (linear scan; we already have the list) | Structural pseudo-legality check |
| 13 | Node prologue order | repetition/50-move → MDP tighten → TT probe → score_from_tt → bound compare → cutoff or fall-through | — |
| 14 | Bench reset | `reset_for_new_game()` clears: TT, game_history, age, prior_root_move, killers (M4.B), history (M4.C) | — |
