# Prior-Art Research: Staged Move Generation (M5.H)

Sources consulted: Chess Programming Wiki (CPW) — Move Generation, Move Ordering, Move List, Hash Move, Killer Move, Killer Heuristic, Pseudo-Legal Move, Beta-Cutoff, Node Types, Static Exchange Evaluation, History Heuristic, Countermove Heuristic, Quiescence Search; TalkChess threads (t=68923 staged-movegen-and-killers, t=76835 staged-movegen-question, t=76491 sorting-moves, t=73930 sort-every-moves-or-pickNext, t=79279 lazy-sorting-algorithm, t=69704 lazy-movegen-and-ordering, t=82494 checking-TT-move-for-legality); MadChess dev blog (Build 093 staged movegen, 2018); Rustic chess engine tutorial (rustic-chess.org).

Per ADR-0003, no third-party engine source code was read. All findings come from prose: wikis, papers, blog posts, and forum threads. Cited Elo numbers come from published blog posts or forum reports by the engine developers themselves.

---

## TL;DR — Eight-Bullet Bottom Line

- Staged movegen (also: move picker, lazy movegen, incremental movegen) delays generating later-stage moves until earlier stages fail to produce a cutoff. Economy: when the TT move or a capture causes a beta cutoff, killers and quiets are never generated.
- Standard stage order: **TT move → good captures (MVV-LVA sorted batch) → killer 0 → killer 1 → quiet moves (history sorted batch)**. With SEE: good captures → killers → quiets → bad captures is a common refinement.
- Beta cutoffs at CUT-nodes occur on the **first move searched in 90–95% of cases** (CPW Beta-Cutoff; CPW Node Types citing Robert Hyatt). This is the statistical argument for staged generation — the first-stage TT-move cutoff alone eliminates capture + quiet generation at those nodes.
- TT move and killer moves require **explicit legality validation before yielding**. Captures and quiets generated lazily by `generate_moves` are already legal (clawfish is legal-direct per ADR-0007) — no extra check.
- Quiet deduplication: the quiet batch must skip the TT move and any killer already yielded. Failure to deduplicate causes double-searching a move, changing node counts and introducing a subtle search bug (not a crash, but incorrect ordering).
- Capture sort: **batch-sort up-front by MVV-LVA**. Quiet sort: **batch-sort up-front by history**. Selection sort (find-max) is literature-documented as mildly superior at cut-nodes but not conclusively faster overall; for typical batch sizes (~5–30 captures, ~20–40 quiets) the difference is small.
- SE verification: stager should accept `excluded_move: Option<Move>` at construction and skip on yield. This matches the already-landed M5.G design (`excluded_move` on negamax) and is the only approach documented at scale.
- The roadmap's H1/H2 split (behavior-equivalent refactor first, lazy generation SPRT-gated second) **is well supported by the literature**: multiple TalkChess developers report node counts changing between equivalent implementations due to history-score timing, and the net Elo from lazy generation is reported as nil to +39 depending on engine (median estimate ~15 Elo).

---

## 1. Vocabulary and Folklore

### 1.1 Synonyms Across the Literature

| Term | Where Used | Notes |
|------|-----------|-------|
| Staged move generation | CPW; TalkChess (most common forum usage) | Emphasizes the staging architecture |
| Move picker | TalkChess; Rustic-chess.org; MadChess blog | Emphasizes the picker abstraction returned to the caller |
| Lazy move generation | CPW; TalkChess t=69704 | Emphasizes deferral of work |
| Incremental move generation | CPW Move Generation page | Less common; same concept |
| Move loop | CPW Move List | The combined abstraction hiding generation + ordering behind an iterator |

### 1.2 Historical Timeline

- **Pre-1990s:** Most engines generate all pseudo-legal moves into a list, sort, and iterate. CPW describes this as "chunk move generation."
- **Late 1980s–1990s:** TT-move-first ordering becomes standard after the transposition table itself becomes standard (e.g., Crafty, Rebel). The TT move is tried before generating anything else — the first element of staged movegen even if the engine does not yet call it that.
- **1990s–2000s:** Killer heuristic (first described by David Slate, Scott Atkin ~1977; CPW) becomes standard. Engines begin testing killers before generating quiets.
- **2000s onward:** Capture vs quiet split becomes common. Bitboard engines (Stockfish lineage) formalize the multi-stage picker with: TT → (good captures via SEE) → killers → quiets → (bad captures). The MadChess blog (2018) confirms that even a simple captures vs quiets split alone yielded +39 Elo.

---

## 2. Stage Taxonomy

### 2.1 Standard Stages

The CPW Move Ordering page lists a practical seven-element ordering (paraphrased); the staged movegen literature maps these to five functional stages:

| Stage | Content | Key Ordering Heuristic | Notes |
|-------|---------|----------------------|-------|
| 1. TT move | Single move from TT probe; pre-yield legality check | None (singleton) | If it causes cutoff, stages 2–5 never run |
| 2. Good captures | All captures (+ promotions) with SEE ≥ 0; or all captures with MVV-LVA if no SEE | MVV-LVA or SEE score | Batch-generated, batch-sorted |
| 3. Killer 0 | First killer slot; pre-yield legality + quiet check | None (singleton) | Skip if equal to TT move |
| 4. Killer 1 | Second killer slot; same validation | None (singleton) | Skip if equal to TT move or killer 0 |
| 5. Quiet moves | All non-captures minus TT and killer moves | Butterfly history score | Batch-generated, batch-sorted; deduplicate against TT + killers |
| 6. Bad captures (optional) | Captures with SEE < 0, deferred from stage 2 | MVV-LVA of losing captures | Only if SEE split is implemented; skipped by most without SEE |

Sources: CPW Move Ordering; CPW Move Generation; TalkChess t=76835 (Erik Madsen's description); TalkChess t=68923; MadChess blog Build 093.

### 2.2 Minority Orderings

- **Bad captures before quiets:** Some engines (CPW Move Ordering: "many programmers favor losing captures before other non-captures") place bad captures directly behind killers, before quiet moves. Rationale: losing captures are already generated when all captures are generated; searching them early avoids missing tactical sequences.
- **Checks as a separate stage:** Some engines generate checking moves as a stage between killers and quiets, or as the first item in qsearch (after captures). CPW Quiescence Search documents this as "some programs search non-capture moves that deliver check." The consensus (TalkChess t=59529) is that first-ply-only checks in qsearch are mildly beneficial when combined with aggressive NMP/LMR. Checks as a main-search stage are rare and not recommended without prior feature measurement.
- **Countermoves:** The countermove heuristic (CPW; Uiterwijk 1992) is sometimes added as a stage between killers and quiets — functionally acting as a second-tier killer indexed by the opponent's previous move rather than the ply. CPW notes it is "complementary to killers." TalkChess discussion attributes 10+ Elo to it in some engines. This is a future M6+ consideration for clawfish; the stager design should leave room for a countermove slot without hardcoding the stage count.

### 2.3 Stage Order Rationale

- TT move first: a transposition-table hit means the TT move is likely the best move (it proved a lower bound at depth ≥ current). If it causes a cutoff immediately, zero generation is needed.
- Good captures before quiets: captures tend to be forcing; MVV-LVA provides decent ordering at low cost.
- Killers before quiets: killers caused cutoffs at sibling nodes at the same ply; positionally unrelated but structurally relevant.
- Quiets last: the expensive stage to generate and sort (largest batch, most sorting).
- Bad captures last (SEE variant): losing captures are poor moves statistically but needed for correctness; delaying them avoids paying SEE cost when a quiet causes a cutoff anyway.

---

## 3. Pseudo-Legality vs Legality — What Changes for clawfish

### 3.1 Standard Pseudo-Legal Engines

Most documented engines use pseudo-legal generation. Their stager generates pseudo-legal moves in each stage and tests legality at make-move time (i.e., `make_move` checks if the king is in check after the move; if so, the move is abandoned without recursing). The legality test is O(1) per move via an `in_check` call after make.

The implication: TT and killer moves pulled without generating can be pseudo-legal-validated cheaply (see §4 and §5), and any remaining illegality is caught by make-move.

### 3.2 clawfish Is Legal-Direct (ADR-0007)

`generate_moves` returns only fully legal moves. Consequences for the stager:

| Move Source | Legal Check Needed? | Why |
|-------------|--------------------|----|
| TT move (from a prior search at same position) | Yes — explicit pre-yield check | TT key collisions; race conditions (TalkChess t=82494 syzygy). Even with 64-bit Zobrist, rare collisions occur over long time controls (Gobbato, t=82494). |
| Killer slots (from sibling node, same ply) | Yes — explicit pre-yield check | Killers are stored from a different position; pieces may have moved, captures occurred, sliders may be blocked. CPW Killer Heuristic; TalkChess t=68923. |
| Captures batch (generated lazily) | No | `generate_moves` guarantees legal output. |
| Quiets batch (generated lazily) | No | Same guarantee. |

The net effect: clawfish's stager can **omit the per-move legality check inside make-move** that pseudo-legal engines need, because its generated batches are already legal. But it still needs explicit typed checks for TT and killer moves. The overhead balance is slightly better for clawfish: killer/TT legality validation is done once per slot, not per every generated move.

### 3.3 Reference

- CPW Pseudo-Legal Move: "playing hash- or even killer moves immediately without explicit move generation, but a pseudo legality test" — confirming this is the standard pattern even for pseudo-legal engines.
- TalkChess t=68923 (Sven Schüle): "Full pseudo-legality check for both TT and killer moves, noting the computational cost is negligible compared to total engine time."

---

## 4. TT-Move Legality Check Approach

### 4.1 Two Options

**Option A — "Generate then look up":** Generate the full legal move list first; promote TT move if present in the list. This is clawfish's current approach. It works and is correct but defeats the entire point of staged movegen (we just generated everything).

**Option B — "Typed validate without generating":** Validate the TT move directly against the position, without generating any moves. Components:

1. The `from` square contains a piece of the side-to-move.
2. The `to` square does not contain a friendly piece.
3. The piece type at `from` is capable of reaching `to` (e.g., a bishop moves diagonally; a knight jumps per knight pattern).
4. For sliding pieces (rook, bishop, queen): the path from `from` to `to` is unobstructed.
5. For king moves: `to` is not attacked by the opponent (anti-check after king move).
6. For non-king moves: moving piece is not absolutely pinned to the king (i.e., the king would be in check after the move).
7. Special cases: en passant (horizontal pin check), castling (path clear + not passing through check).

TalkChess t=82494 (syzygy) documents the full typed-validate approach. Community consensus: validate; the bug rate without validation is low per move but compounds over long time controls and multithreaded search.

### 4.2 Correctness Traps

- **Type collision:** A Zobrist key collision can produce a `from/to` pair that makes geometric sense for the wrong piece type. Without step 3, a rook stored from position A can be replayed in position B where a different piece sits on `from`, with the validator passing step 1 (friendly piece present) and step 2 (to square empty) but failing to notice the piece cannot reach `to` as a rook.
- **Promotion flag:** Killers and TT moves encode flags (including promotion piece). If the flag encodes a promotion but the pawn is not on rank 7 (for White), the validator must detect this. CPW Encoding Moves; TalkChess t=68923 (under-promotion killer applied to non-promotable piece).
- **King-capture bit:** A collision can produce a move that "captures" the opponent's king. TalkChess t=68923 notes this causes engine crashes in some implementations (if `make_move` does not defensively check). The validator should reject any move whose `to` square contains the opponent's king.

### 4.3 Practical Recommendation for clawfish

Implement typed validation for TT move (Option B). clawfish's `Move` type encodes (from, to, flag) in 16 bits. A validate function needs:

```rust
fn is_valid_tt_move(mv: Move, pos: &Position) -> bool {
    let from = mv.from();
    let to = mv.to();
    let stm = pos.side_to_move();

    // Step 1: own piece on from
    if !pos.piece_bb(stm).has(from) { return false; }
    // Step 2: no friendly piece on to
    if pos.piece_bb(stm).has(to) { return false; }
    // Step 3: piece type can reach to
    let piece = pos.piece_at(from); // must be stm's piece
    if !piece_can_reach(piece, from, to, pos.all_pieces_bb()) { return false; }
    // Step 4: no self-check (king moves: not attacked; non-king: not pinned)
    // This is more expensive; skip for first cut if crash-safety is all that's needed
    true
}
```

Steps 1–3 prevent crashes and most corruption. Step 4 (self-check) catches the rare pin-related invalid moves; it can be deferred to make-move for pseudo-legal engines but clawfish must handle it pre-yield (since make-move does not re-check for legal-direct engines). In practice, pins are rare and the step 4 cost is low (one `in_check` call after hypothetically applying the move on a scratch copy, or a direct pin-ray computation).

An alternative for clawfish that avoids implementing `piece_can_reach`: **generate the captures + quiets batch lazily, then scan for TT move membership**. This is more expensive than typed validation but is simpler and leverages the already-correct legal movegen. For M5.H, this is an acceptable implementation option since the batch scan is O(N) not O(1), but N is typically 30–60 and the scan only fires when a TT hit exists.

---

## 5. Killer-Move Legality Check

### 5.1 Validation Requirements

Killers must be validated before yielding (TalkChess t=68923; CPW Pseudo-Legal Move). Minimum checks:

| Check | Purpose | Cost |
|-------|---------|------|
| `pos.piece_at(from) == stm_piece` | Piece still present (not captured) | O(1) mailbox lookup |
| `!pos.piece_bb(stm).has(to)` | No friendly piece at destination | O(1) bitboard |
| `!pos.piece_bb(opponent).king_sq() == to` | Not a king capture | O(1) bitboard |
| `mv.is_quiet()` | Killer slots should only contain quiets; defensive check | O(1) flag check |
| For sliding pieces: `path_clear(from, to, pos.all_bb())` | Slider not blocked | O(popcount of ray) |
| Self-check: king move or pinned piece | No illegal exposure of king | O(in_check) |

TalkChess t=68923 (Sven Schüle): full pseudo-legality check is standard; the cost is "negligible compared to total engine time."

### 5.2 The is_quiet Invariant

CPW Killer Heuristic: "a quiet move that caused a cutoff is stored." The update path filters for `is_quiet` before storing — killers should never contain captures.

clawfish's M4.B ADR-0019 documents this: killer update path calls `is_quiet`. The validator should still defensively assert `mv.is_quiet()` before yielding a killer, because:
- A corrupted TT-overlap scenario is impossible by construction (TT uses a different store path), but a logic bug in the update side would cause silent misclassification.
- The defensive check is a single flag inspection; cost is negligible.

### 5.3 Deduplication: Killer vs TT Move

If the TT move was yielded in stage 1, and killer 0 or killer 1 happens to be the same move, the killer slot should be skipped. Double-searching the same move is not a crash, but it wastes a search call and can change node counts.

Deduplication check: `if killer == tt_move { continue; }`. This requires the stager to remember `tt_move` through the killer stage.

Between killers: killer 1 should be skipped if it equals killer 0. CPW Killer Heuristic: "the replacement scheme ought to ensure that all the available slots contain different moves." But at read time in the stager, a defensive check is cheap: `if killer1 == killer0 { skip; }`.

---

## 6. Lazy Capture Generation

### 6.1 Batch vs One-at-a-Time

The literature consensus: captures are generated **as a batch** (one call to the capture-only generator), then **sorted once by MVV-LVA** (or SEE score), then iterated in order.

Reasons for batch:
- Move generation for captures is not easily decomposed to "next capture" without re-walking all attacker sets (the bitboard capture generator enumerates all attackers for all squares in one pass).
- MVV-LVA sorting requires the full list to sort.
- The batch size is small (~5–30 captures in a typical middlegame position); sorting cost is minimal.

One-at-a-time via find-max (selection sort over the batch): achievable, but the CPW Move Ordering page notes this is the standard approach for the iterate-with-selection-sort pattern ("chess programs usually don't sort the whole move list, but perform a selection sort each time a move is fetched"). With a batch already in memory, selection sort gives identical results to an upfront sort at O(N²) total vs O(N log N); for N ≤ 30, both are fast and the difference is noise.

TalkChess t=79279 (lazy sorting benchmark): lazy-sort (find-max) measured at 1,155,482 NPS vs std::sort at 1,016,291 NPS vs selection sort at 985,227 NPS. The differences are real but small; other developers (algorhythm) found -7.8 Elo in the other direction from lazy sorting, suggesting the winner is engine- and position-dependent.

**Recommendation for clawfish:** Sort the capture batch upfront by MVV-LVA score. Simple, predictable, and correct. Revisit with selection sort only if profiling shows capture-sort as a hotspot.

### 6.2 SEE Split (Good vs Bad Captures)

The SEE split adds a second sort pass that separates captures into `SEE ≥ 0` (good) and `SEE < 0` (bad). Good captures are yielded in stage 2; bad captures in stage 6 (after quiets).

CPW Static Exchange Evaluation: "SEE is used in move ordering to separate good/bad captures." The CPW Move Ordering page documents SEE ≥ 0 before quiets, SEE < 0 after quiets, as a common refinement.

Cost: SEE itself is not cheap (TalkChess t=76835, Henk: "SEE is not cheap"). For M5.H, MVV-LVA without SEE split is the recommended starting point. SEE split is a follow-up tuning candidate.

---

## 7. Lazy Quiet Generation

### 7.1 The Core Economy Claim

The key benefit of staged movegen is here: if stage 1 (TT), stage 2 (captures), or stage 3–4 (killers) causes a beta cutoff, **the quiet batch is never generated and never sorted**. For cut-nodes where the TT move causes a cutoff on the first move searched (~90% of TT-hit cut-nodes), this saves one call to `generate_moves` and one `sort_by_key` call per node.

CPW Beta-Cutoff: "Nodes where a beta-cutoff occurs are cut-nodes where move ordering was crucial to try the refutation move as early as possible — typically as first move in 90 to 95 per cent of all cases." Combined with CPW Node Types (Robert Hyatt: "> 90% of CUT nodes are discovered on the first move searched"), the fraction of nodes where quiet generation is skipped is substantial.

### 7.2 Batch Generation and Sort

Quiets are generated as a batch (one call to a quiet-only generator or a full-minus-captures generator), then sorted by history score, then iterated.

The deduplication filter (remove TT move and killers from the quiet batch) runs over the generated batch before sorting. This is an O(N) scan with equality checks.

TalkChess t=76835 (stage-2 thread): history scores are computed at sort time, not at generation time. One subtlety: "quiet moves get another history score in staged move generation than in a 'one run' implementation because the time when you check the scores is different." This means history scores are re-read at sort time (not cached at generation time), so any history updates from searching earlier moves at the same node are reflected in the sort order of later moves. This is the **intended behavior** — it is a feature of the stager, not a bug.

---

## 8. Cutoff-Before-Generation Elo Signal

### 8.1 Published Numbers

| Engine / Source | Implementation | Reported Elo | Notes |
|-----------------|---------------|-------------|-------|
| MadChess 3.0 Beta Build 093 (2018) | Captures vs quiets split | **+39 Elo** | Engine at roughly 2000–2200 Elo; relatively weak ordering before split |
| TalkChess t=76835 (Desperado) | General estimate | **~+15 Elo** | "More typical for staged movegen when properly implemented" |
| TalkChess t=69704 (tsoj, Joost Buijs) | Full lazy staged | **~0 Elo** | "Doesn't change anything performance-wise" with fast bitboard generator |
| TalkChess t=79279 (lazy sort test) | Lazy sort vs std::sort | **−7.8 Elo** | Algorhythm's test; opposite direction from expected |

### 8.2 TT-Move Cutoff Rate

When the TT move yields a cutoff, the entire captures + killers + quiets stages are skipped. This rate is high at TT-hit nodes: TT moves at PV-nodes or recently-searched positions have high cutoff probability. No published paper pins an exact rate for "TT-move-only cutoff fraction," but the 90–95% first-move cutoff rate at cut-nodes is the closest proxy. At positions where the TT move IS the first move (which it is for all nodes with a TT hit), the savings are proportional to that 90–95%.

### 8.3 Why the Spread Is Large

- Engines with fast bitboard movegen (e.g., magic bitboards + PEXT) generate all moves very cheaply; the savings from skipping generation are smaller in absolute time.
- Engines with already-strong ordering (TT + killers + history) already avoid wasted search at cut-nodes; the marginal Elo from also skipping generation is smaller.
- Engines without NMP, LMR, aspiration windows produce fewer cut-nodes overall; staged movegen has less opportunity to fire.
- clawfish at M5.G has NMP + LMR + aspiration + killers + history + SE — a well-pruned tree. Marginal Elo from staged movegen is likely **5–15 Elo** (lower end of the range), not 39.

### 8.4 The Roadmap's "+5–15 Elo" Estimate

This is consistent with the literature for a well-developed engine. The MadChess +39 was at an engine with weaker prior ordering; TalkChess community estimates cluster around 10–20 for typical engines.

---

## 9. Sort-Once vs On-Demand (Selection Sort)

### 9.1 The Question

Once the capture or quiet batch is generated, do we: (A) sort the entire batch upfront using `sort_by_cached_key`, then iterate sequentially; or (B) find-max per iteration (selection sort), stopping when a cutoff occurs?

### 9.2 Literature Findings

CPW Move Ordering: "chess programs usually don't sort the whole move list, but perform a selection sort each time a move is fetched." This refers to the classical one-list-with-scoring approach; in a staged stager the same principle applies inside each stage.

TalkChess t=76491 (Abulmo2, Minic): "I tried both inside Minic and I get a little more performance using a picker instead of sorting them all."

TalkChess t=73930 (Bob Crafty / multiple): "procrastination" principle — never sort what you might not need. But also: O(N²) selection sort is slower than O(N log N) quicksort for large N (all-nodes that search all quiets).

TalkChess t=79279: lazy-sort measures faster than std::sort in NPS benchmark, but one developer's Elo test contradicted this.

### 9.3 Practical Recommendation for clawfish

- **Captures (N ≈ 5–15):** Sort upfront with `sort_by_cached_key`. N is small enough that sort is fast; selection sort provides no measurable benefit.
- **Quiets (N ≈ 20–60):** Sort upfront with `sort_by_cached_key`. For an engine with already-strong ordering (killers cut frequently), the quiet batch is often not iterated fully anyway. The practical difference between upfront-sort and selection-sort at N ≈ 30 is small (a few microseconds per node). Choose the simpler implementation.

If profiling reveals quiets-sort as a hotspot, switch to selection sort on the quiet slice. This is an optimization, not an architectural decision.

---

## 10. Interaction with LMR

### 10.1 The Assumption

clawfish's LMR (M5.C) uses `quiet_index` to decide reduction eligibility: `quiet_index >= 2` (after the first quiet and killers). This assumes quiet moves are seen in history-best-first order — the first quiet is history-best, the later ones are late moves eligible for reduction.

### 10.2 Stager Compatibility

With a stager, `quiet_index` is simply the number of quiet moves yielded so far. The stager's history-sorted quiet batch ensures the ordering assumption holds.

Key correctness point (TalkChess t=76835): history scores are evaluated **at sort time** (when the quiet batch is sorted), not pre-computed at generation time. If searching a capture or killer earlier at the same node updated the history table, the quiet sort reflects those updates. This is fine and correct: the stager reads fresh history scores when sorting the quiet batch, giving better ordering than a pre-sorted list would.

### 10.3 TT Move and Killers in quiet_index

Under the current M5.C implementation, killers are accounted for via `is_killer` and excluded from LMR. The TT move is at index 0 and implicitly excluded. With the stager:

- The TT move is yielded by the stager before any batch is generated. It is not part of the quiet batch and does not increment `quiet_index`.
- Killers are yielded as singleton stages. They do not increment `quiet_index` (they are handled in their own stage).
- The quiet batch iterator begins counting from `quiet_index = 0` (or 1, depending on convention). The LMR threshold `quiet_index >= 2` is unaffected.

This is correct by construction: the stager's stage separation naturally implements what the current code does via `is_killer` / priority-score checks.

---

## 11. Interaction with FFP

FFP (M5.D) applies a per-quiet-skip inside the move loop: if `static_eval + margin <= alpha`, the quiet is pruned. This is a per-move gate applied after the quiet is yielded by the stager.

The stager delivers quiets one at a time (or yields them in order from the pre-sorted batch). FFP's gate applies to each yielded quiet. No change to FFP logic is needed.

**The economy benefit of the stager is upstream of FFP:** if a killer cut before quiets were generated, FFP never fires at all. This is strictly additive: staged movegen reduces the number of nodes that reach the quiet loop, and FFP reduces the number of quiets searched within that loop.

---

## 12. Interaction with M5.G Singular Extensions

### 12.1 The Excluded-Move Problem

SE's verification call passes `excluded_move = Some(moves_vec[0])` and recurses into negamax. In the verification frame, the stager must skip the excluded move wherever it appears (TT stage, capture stage, quiet stage). The excluded move is always the TT move at the parent (it's `moves_vec[0]` = the TT-ordered first move), so it is most likely to appear in the stager's TT-move slot.

### 12.2 Three Design Options

| Option | Mechanism | Tradeoffs |
|--------|-----------|-----------|
| A. Stager takes `excluded_move: Option<Move>` at construction | Stager skips yielding `excluded_move` in any stage | Simple API; stager is stateless re: exclusion after construction |
| B. Caller filters at loop site | Stager yields all moves; caller's `if mv == excluded_move { continue; }` skips | One extra equality check per yielded move; caller logic unchanged |
| C. Verification builds a separate stager without TT stage | Stager subtype for verification only | More complex; still needs to skip TT move appearing in quiet stage |

clawfish's M5.G already implements Option B: `if Some(mv) == excluded_move { continue; }` in the move loop. This pattern transfers directly to the stager: the loop over the stager's `next()` calls includes the same skip. Option B is the simplest to preserve the existing design.

Option A (construction parameter) is marginally cleaner but requires threading the parameter through negamax to the stager construction site — which negamax already receives as `excluded_move: Option<Move>`.

**Recommendation:** Option B. Keep the caller-side skip, which is a single comparison per yielded move and matches the already-landed M5.G implementation exactly. No stager API change needed.

---

## 13. Interaction with Qsearch

### 13.1 clawfish Qsearch Context

M5.E–M5.F added qsearch correctness fixes and TT probe/store to qsearch. Qsearch generates: (a) captures + promotions when not in check, (b) all legal moves when in check, (c) no quiet moves except the single-reply extension (M5.E). The ordering is MVV-LVA on captures.

### 13.2 Staged Movegen in Qsearch

TalkChess t=76835 (Rasmus Althoff): "most nodes are spent in quiescence, so staged move generation in main search won't have any large impact anyway." Qsearch itself is the dominant consumer.

A qsearch stager would yield: TT move (if present, validated) → captures sorted by MVV-LVA. There are no killers or quiet moves (except the single-reply case). The simplification is: qsearch's "stages" are a strict subset of negamax's stages, and the two share no structural overlap beyond the TT-move validation logic.

### 13.3 Type Sharing Recommendation

Do not share a single `MoveStager` type between negamax and qsearch. Reasons:

- Qsearch has fewer stages; encoding the full stage enum in qsearch adds dead branches.
- The polymorphism cost (match on stage enum) is on the hot path (every qsearch call).
- Qsearch already has a simpler structure; a purpose-specific qsearch iterator is cleaner.

**M5.H scope:** qsearch is out of scope. The stager design should not preclude a future qsearch stager, but should not add hooks for it in M5.H.

---

## 14. State Machine Encoding

### 14.1 Standard Pattern

CPW Move List: "move access in conjunction with move generation is usually hidden behind an iterator interface with two methods, one for initializing the move generator, and a method to get the next move, where there could be any finite-state machine and data structures hidden by that interface."

The standard implementation is a `next()` / `pick_next()` method returning `Option<Move>`, with an internal state variable advancing through stages.

### 14.2 Rust-Specific Encoding

An `enum Stage` advancing on each call to `next()` is the idiomatic Rust pattern:

```rust
enum Stage {
    TtMove,
    GenerateCaptures,   // entry point: generate & sort capture batch
    YieldCapture,       // yield next from sorted capture slice
    Killer0,
    Killer1,
    GenerateQuiets,     // entry point: generate & sort quiet batch (dedup TT + killers)
    YieldQuiet,         // yield next from sorted quiet slice
    Done,
}

struct MoveStager<'a> {
    stage: Stage,
    pos: &'a Position,
    tt_move: Option<Move>,
    killer0: Option<Move>,
    killer1: Option<Move>,
    captures: Vec<Move>,    // pre-sorted by MVV-LVA
    capture_idx: usize,
    quiets: Vec<Move>,      // pre-sorted by history
    quiet_idx: usize,
}

impl<'a> MoveStager<'a> {
    pub fn next(&mut self, history: &HistoryTable) -> Option<Move> {
        loop {
            match self.stage {
                Stage::TtMove => {
                    self.stage = Stage::GenerateCaptures;
                    if let Some(mv) = self.tt_move {
                        if is_valid_tt_move(mv, self.pos) {
                            return Some(mv);
                        }
                    }
                }
                Stage::GenerateCaptures => {
                    self.captures = generate_captures(self.pos);
                    sort_by_mvv_lva(&mut self.captures);
                    self.capture_idx = 0;
                    self.stage = Stage::YieldCapture;
                }
                Stage::YieldCapture => {
                    if self.capture_idx < self.captures.len() {
                        let mv = self.captures[self.capture_idx];
                        self.capture_idx += 1;
                        return Some(mv);
                    }
                    self.stage = Stage::Killer0;
                }
                Stage::Killer0 => {
                    self.stage = Stage::Killer1;
                    if let Some(mv) = self.killer0 {
                        if Some(mv) != self.tt_move && is_valid_killer(mv, self.pos) {
                            return Some(mv);
                        }
                    }
                }
                Stage::Killer1 => {
                    self.stage = Stage::GenerateQuiets;
                    if let Some(mv) = self.killer1 {
                        if Some(mv) != self.tt_move
                            && Some(mv) != self.killer0
                            && is_valid_killer(mv, self.pos)
                        {
                            return Some(mv);
                        }
                    }
                }
                Stage::GenerateQuiets => {
                    self.quiets = generate_quiets(self.pos);
                    // Dedup: remove TT move and killers from quiet batch
                    self.quiets.retain(|mv| {
                        Some(*mv) != self.tt_move
                            && Some(*mv) != self.killer0
                            && Some(*mv) != self.killer1
                    });
                    sort_by_history(&mut self.quiets, history);
                    self.quiet_idx = 0;
                    self.stage = Stage::YieldQuiet;
                }
                Stage::YieldQuiet => {
                    if self.quiet_idx < self.quiets.len() {
                        let mv = self.quiets[self.quiet_idx];
                        self.quiet_idx += 1;
                        return Some(mv);
                    }
                    self.stage = Stage::Done;
                }
                Stage::Done => return None,
            }
        }
    }
}
```

Notes on this pseudo-Rust:
- `generate_captures` and `generate_quiets` are separate entry points into `generate_moves`; in clawfish's current code there is one `generate_moves` function. The split needs to be implemented or achieved by filtering.
- `is_valid_killer` applies the checks from §5.1.
- The `GenerateCaptures` and `GenerateQuiets` stages are pass-through driver stages (no move yielded; they set up the slice then fall through to the yield stage via `loop { match ... }`). This is the standard Rust state-machine idiom: the `loop` retries the `match` immediately after transitioning state.

### 14.3 Coroutine Alternative

Rust stable does not have `async`-based generators in a form usable here (as of 2025). The state-machine enum is the idiomatic pattern; it is what engines like Shen Yu (Rust, TalkChess) implement. No coroutine-style alternative is needed or recommended.

---

## 15. Common Bugs

### 15.1 TT Move Bits Collision

A hash collision produces a `Move` whose bits are not consistent with the current position. Without typed validation, this move may be searched, producing garbage results or (in pseudo-legal engines) a crash at make-move. In clawfish's legal-direct framework, the move would be absent from the generated legal list; without validation, the stager yields it and the search frame receives an illegal move. The make-move call would update the position incorrectly (since ADR-0007's `generate_moves` is the legality gate, not `make_move`).

**Fix:** typed TT-move validation (§4.2) before yielding.

### 15.2 Killer Yielded Twice (Equal to TT Move)

If `killer0 == tt_move`, and both are yielded without deduplication, the same move is searched twice. Node counts change; the second search is wasted work. Not a crash.

**Fix:** `if killer == tt_move { skip; }` at the killer stages.

### 15.3 Quiet Batch Contains TT Move or Killer

When the quiet batch is generated, it contains all legal non-captures. The TT move (if quiet) and the killers are in this batch. Without deduplication, they are searched again in the quiet stage.

**Fix:** `quiets.retain(|mv| mv not in {tt_move, killer0, killer1})` before sorting. This is O(N) with equality checks per move; negligible cost.

### 15.4 Killer That Is a Capture

If the killer update path ever stores a capture (i.e., `is_quiet` guard fails or is missing), the killer slot contains a capture. The capture appears in both the capture stage and the killer stage. Double search.

**Fix:** assert `is_quiet(mv)` in the killer-update path (M4.B ADR-0019 already enforces this); add defensive `is_quiet` check at killer-yield time.

### 15.5 Under-Promotion Killer on Non-Promotable Piece

A killer stored when a pawn on rank 7 (White) made a promotion is replayed at a position where the same square has a non-pawn piece. The flag bits encode a promotion piece; `make_move` may interpret this as a promotion for a non-pawn piece, causing position corruption.

**Fix:** typed validation checks the piece type at `from`; if the move flag is a promotion but the piece is not a pawn, or the pawn is not on the promotion rank, reject.

TalkChess t=68923 explicitly documents this bug: "crashes can occur from under-promotion killers applied to non-promotable pieces."

### 15.6 History Score Timing Subtlety

In staged movegen, the quiet batch is generated and sorted **after** captures and killers are searched. This means history scores at sort time reflect updates from searching captures and killers at the same node. This differs from a "sort once at start of node" approach.

This is not a bug — it is the intended behavior described in TalkChess t=76835. But it means that switching from the current monolithic sort to staged sort will **change node counts** (different ordering of quiets → different PV paths). This is the key reason to split M5.H into H1 (behavior-equivalent refactor) and H2 (enable lazy generation).

The behavior-equivalent refactor (H1) preserves the same node counts by sorting the entire move list before any searching, exactly as current code does. H2 then enables lazy generation, intentionally changing the history-score timing and therefore the search tree. The H2 node-count change should produce a net Elo gain (better ordering from fresher history), but requires SPRT validation.

---

## 16. Performance Characteristics — Can Staged Movegen Be Neutral or Negative?

### 16.1 Cost Side

The stager adds:
- One `match` per yielded move (stage state check): negligible.
- One equality check per killer slot (dedup): negligible.
- One equality check per quiet move (dedup against TT and killers): O(N) per batch; N ≈ 30, cost ~30 comparisons per node where quiets are generated.
- The killer validation calls: two `is_valid_killer` calls per node maximum; cost is a few operations each.

### 16.2 Savings Side

- At every node where TT → cutoff: saves one `generate_captures` call, one `sort` call, two killer validations, one `generate_quiets` call, one `sort` call.
- At every node where captures → cutoff: saves two killer validations, one `generate_quiets` call, one `sort` call.
- At every node where a killer → cutoff: saves one `generate_quiets` call, one `sort` call.

### 16.3 When Staged Movegen Is Neutral or Negative

TalkChess t=69704 (Joost Buijs, tsoj): "with a bitboard move generator, generating all moves doesn't take much longer than generating captures alone; in case captures don't produce a cutoff, the whole process takes longer."

Conditions where gains shrink to zero or turn negative:
- Move generation is extremely fast (magic bitboard PEXT; generation cost is negligible anyway).
- The engine already has very high cut-node first-move rates (strong existing ordering reduces the marginal gain of skipping generation).
- The per-move overhead of the stager (state machine, dedup checks) outweighs the savings on small trees (very shallow searches or endgames with few legal moves).

For clawfish specifically: magic bitboards are in place (M1.C); the generator is fast. The net Elo gain is expected at the lower end of the literature range (5–15 Elo rather than 39 Elo).

### 16.4 Does the Literature Support the H1/H2 Split?

Yes. Multiple TalkChess discussions (t=76835 page 2: "when reengineering the move picker properly (producing the same node counts for example) you will realize issues") document that switching from sort-all-upfront to staged generation changes node counts due to history-score timing. The H1 refactor (behavior-equivalent restructure, bench-neutral) validates the stager architecture before enabling the lazy-generation savings. H2 then enables lazy generation with a SPRT gate to confirm the expected Elo signal.

This is the standard approach in the community for introducing staged movegen into a mature engine: validate structure separately from enabling lazy generation.

---

## 17. Counter-Moves and Future Ordering Refinements

The countermove heuristic (CPW; Uiterwijk 1992) stores, per (previous_move.from, previous_move.to), the quiet move that best refuted that move. It acts as a third single-move stage between killers and quiets.

TalkChess discussions report 10+ Elo for countermoves ("for about 10 lines of code, they're considered a must-have"). The CPW describes it as complementary to killers.

**M5.H forward-compat:** The stager enum should include a `CounterMove` stage slot between `Killer1` and `GenerateQuiets`, even if it is initially implemented as a no-op or pass-through. This avoids a structural refactor when countermoves are added in M6.

Concrete: add `CounterMove` stage; if `counter_move: Option<Move>` is `None` (not yet implemented), the stage immediately transitions to `GenerateQuiets`. Adding real countermove logic later only requires filling in `counter_move` at construction and adding validation logic in the `CounterMove` arm.

Continuation history (generalized countermoves indexed by N-ply-ago move) is a further refinement; leave room but do not add hooks beyond countermove for M5.H.

---

## 18. References and Links

All links are marked by type:

| Source | Type | URL |
|--------|------|-----|
| CPW — Move Generation | wiki | https://www.chessprogramming.org/Move_Generation |
| CPW — Move Ordering | wiki | https://www.chessprogramming.org/Move_Ordering |
| CPW — Move List | wiki | https://www.chessprogramming.org/Move_List |
| CPW — Hash Move | wiki | https://www.chessprogramming.org/Hash_Move |
| CPW — Killer Move | wiki | https://www.chessprogramming.org/Killer_Move |
| CPW — Killer Heuristic | wiki | https://www.chessprogramming.org/Killer_Heuristic |
| CPW — Pseudo-Legal Move | wiki | https://www.chessprogramming.org/Pseudo-Legal_Move |
| CPW — Beta-Cutoff | wiki | https://www.chessprogramming.org/Beta-Cutoff |
| CPW — Node Types | wiki | https://www.chessprogramming.org/Node_Types |
| CPW — Static Exchange Evaluation | wiki | https://www.chessprogramming.org/Static_Exchange_Evaluation |
| CPW — History Heuristic | wiki | https://www.chessprogramming.org/History_Heuristic |
| CPW — Countermove Heuristic | wiki | https://www.chessprogramming.org/Countermove_Heuristic |
| CPW — Quiescence Search | wiki | https://www.chessprogramming.org/Quiescence_Search |
| CPW — Singular Extensions | wiki | https://www.chessprogramming.org/Singular_Extensions |
| TalkChess t=68923 — Staged movegen and killers | forum | https://talkchess.com/forum3/viewtopic.php?t=68923 |
| TalkChess t=76835 — Staged movegen question | forum | https://talkchess.com/viewtopic.php?t=76835 |
| TalkChess t=76835 p.2 | forum | https://talkchess.com/viewtopic.php?t=76835&start=10 |
| TalkChess t=76491 — Sorting moves | forum | https://talkchess.com/viewtopic.php?t=76491 |
| TalkChess t=73930 — Sort every move or pickNext | forum | https://talkchess.com/viewtopic.php?t=73930 |
| TalkChess t=79279 — Lazy sorting algorithm | forum | https://talkchess.com/viewtopic.php?t=79279 |
| TalkChess t=69704 — Lazy movegen and ordering | forum | https://www.talkchess.com/forum3/viewtopic.php?t=69704 |
| TalkChess t=82494 — Checking TT move for legality | forum | https://talkchess.com/viewtopic.php?t=82494 |
| TalkChess t=59529 — Checks in qsearch | forum | https://www.talkchess.com/forum3/viewtopic.php?t=59529 |
| MadChess 3.0 Beta Build 093 — Staged Move Generation | blog | https://www.madchess.net/2018/12/15/madchess-3-0-beta-build-093-staged-move-generation/ |
| Rustic Chess Engine — TT-Move Ordering | tutorial | https://rustic-chess.org/search/ordering/tt_move.html |
| Rustic Chess Engine — Killer Moves | tutorial | https://rustic-chess.org/search/ordering/killers.html |
| Hyatt & Cozzie 2005 — Hash Signature Collisions | paper | (cited in CPW Hash Move; not directly fetched) |
| Uiterwijk 1992 — Countermove Heuristic | paper | (cited in CPW Countermove Heuristic; not directly fetched) |

---

## 19. Synthesis for M5.H Plan

### 19.1 Stage Order

Use: TT move → captures (MVV-LVA sorted batch) → killer 0 → killer 1 → [counter-move slot, pass-through for M5.H] → quiet moves (history sorted batch).

No SEE split in M5.H. Bad captures are not a separate stage unless SEE is implemented as a follow-up. This is the minimum viable staged-movegen that captures the main Elo signal.

### 19.2 Legality

- TT move: typed validate before yielding (steps 1–4 from §4.2). At minimum: correct side-to-move piece on `from`; no friendly piece on `to`; piece type can reach `to`; no obvious king-capture.
- Killers: same typed validate plus `is_quiet()` defensive check. Skip if equal to already-yielded TT move.
- Captures batch: no additional check (legal-direct movegen guarantees legality).
- Quiets batch: no additional check; dedup against TT move and killers via `retain`.

An alternative to typed TT validation: **scan the legal capture+quiet batches for TT-move membership**. More expensive (O(N) scan vs O(1) typed validate) but simpler to implement correctly. Acceptable for M5.H; replace with typed validation later if bench shows scan is a hotspot.

### 19.3 Capture Sort

Sort the capture batch upfront by MVV-LVA. Selection sort is not worth the added complexity at N ≤ 30.

### 19.4 Quiet Sort

Sort the quiet batch upfront by history score. History scores are read at sort time (after captures and killers have been searched), giving fresher scores than a pre-node sort would. This is the intended behavior.

### 19.5 SE Verification Excluded Move

Keep the caller-side skip: `if Some(mv) == excluded_move { continue; }` in the move loop over the stager. This matches the already-landed M5.G design with no API change to the stager.

### 19.6 Qsearch

Out of scope for M5.H. Qsearch gets a purpose-built stager (if ever) in a later milestone. The `MoveStager` type for negamax should not be designed for polymorphic reuse with qsearch.

### 19.7 H1/H2 Split Recommendation

**Recommend the split.**

- H1 (behavior-equivalent refactor): Introduce `MoveStager` as the iterator, but generate all moves up-front and sort them at node entry exactly as today. The stager wraps the sorted `Vec<Move>` and yields from it. Node counts and bench signature are unchanged. This validates the stager API and the stage-skip logic.
- H2 (lazy generation): Change `MoveStager` internals to generate captures lazily (on entering `GenerateCaptures` stage) and quiets lazily (on entering `GenerateQuiets` stage). This changes history-score timing and therefore node counts and bench signature. SPRT-gate H2 against `M5.G`.

The split is supported by the literature (TalkChess t=76835 page 2 on history-score timing) and by the project's own SPRT-gating discipline. H1 gives the architectural benefit without the risk of a silent regression from history-score timing changes.

### 19.8 Expected Elo

5–15 Elo from H2, based on the literature range for a well-ordered engine (clawfish at M5.G has NMP + LMR + killers + history + SE). H1 is expected to be bench-neutral and Elo-neutral by design. H2 SPRT should gate on `elo0 = -10, elo1 = 5` or similar (small-but-not-regression criterion matching M5.F/M5.G precedent).
