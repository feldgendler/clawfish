# Prior-Art Research: Null-Move Pruning (M5.A)

Sources consulted: Chess Programming Wiki (CPW), Donninger 1993 ("Null Move and Deep Search"), Heinz 1999 ("Adaptive Null-Move Pruning"), David & Netanyahu (verified NMP), TalkChess forums (jdart/Arasan, Ras/CT800, Bob Hyatt, H.G. Muller, ciorap0, JimmyRustles, et al.), Mediocre Chess blog (Jonatan Dahl), CPW-Engine reference implementation, OpenChess forum.

Per ADR-0003, no third-party engine source code was read. All findings come from CPW articles, academic papers, blog posts, and forum threads.

---

## TL;DR — Five-Bullet Bottom Line

- NMP fires a zero-window null-move sub-search (`-beta, -beta+1`) at non-PV interior nodes before the move loop; if `null_score >= beta` it returns a fail-high, saving the entire move loop.
- Gates that **must** all be true: not in check; `beta - alpha > 1` (non-PV); no stacked null; `static_eval >= beta`; side-to-move has at least one non-pawn piece; `depth >= 3`.
- **Recommended R formula for M5.A v1:** `R = 2 + depth / 6` (integer division). CPW-Engine uses `depth > 6 ? 3 : 2`; either is an SPRT-positive starting point; the `depth/6` variant scales more smoothly and is a common modern default.
- `make_null_move` is trivial: flip STM, clear EP, increment halfmove clock, update Zobrist (turn key XOR + EP file key XOR out if was active). No piece moves. Undo via a lightweight `NullUndo { ep, halfmove, zobrist }`.
- Slots into the negamax prologue **after** the TT probe+cutoff check, **before** move generation — per the roadmap pinned order. The `static_eval` read it requires is new to the prologue (M5.A is the first prologue consumer); M5.B/RFP will reuse the same read.

---

## 1. Why NMP Works

### The Null-Move Observation

"In almost all chess positions, making a null move (passing a turn) is worse than the best legal move." — Donninger 1993.

If the side-to-move can afford to **give the opponent an extra free move** and the opponent still cannot reach beta from the opponent's perspective, then the side-to-move can produce a score ≥ beta with any real move — so return a beta cutoff without searching the full move tree.

### Why Fail-Soft Makes the Gate Correct

Under fail-soft negamax (ADR-0016 §1), the null search returns the actual best score found, not clamped. The null branch uses a zero window `(-beta, -beta+1)`, so the child gets `(beta-1, beta)` negated. If the child returns `null_score >= beta`, that is a proved lower bound — the position is "too good" and the full move loop cannot do worse. Returning `null_score` (or `beta` in fail-hard; we are fail-soft so we may return the actual score) is correct.

Formally: `null_score = -negamax(pos_after_null, depth-1-R, ply+1, -beta, -beta+1, ...)` and `null_score >= beta` → return `null_score`.

**Important**: do NOT return a mate score from the null branch as a confident result. A `null_score >= MATE_IN_MAX_PLY` means the opponent mates us (we gave them a free move and they found mate) — that indicates a dangerous position but does not mean we have a real winning move. Cap the returned score to `beta` when `null_score` is a mate-scale value (see §12).

---

## 2. The Pre-NMP Gate Set

Every condition below must hold before attempting NMP. All are load-bearing.

| Gate | Condition | Why | Citation |
|------|-----------|-----|---------|
| Not in check | `!in_check(pos)` | Making a null move while in check leaves the king attacked — the resulting "position" is illegal; any search on it is meaningless. | CPW NMP; Mediocre Chess guide |
| Non-PV node | `beta - alpha > 1` | At PV nodes (α+1 < β) we need the full PV. CPW: "used only at non-PV cut nodes." Under PVS (which clawfish uses via aspiration) the zero-window `β-α==1` is the non-PV signal. | CPW Node Types; ADR-0018 §11 |
| No stacked null | `allow_null` flag is `true` | Two consecutive null moves in one path would mean the same side passed twice. NMP's null branch is already playing "pass + opponent pass"; a second null is semantically nonsense and makes the search unsound. | CPW NMP; Mediocre Chess; TalkChess t=74279 |
| Static-eval gate | `static_eval >= beta` | NMP is only profitable when our position is already at or above beta; if we are below beta, the null search is unlikely to produce a cutoff and costs nodes. Gate is marginal (+4 Elo empirically per TalkChess t=69722), but cheap and standard. | TalkChess t=69722; CPW-Engine reference; OpenChess t=2994 |
| Zugzwang guard | Side-to-move has at least one non-pawn, non-king piece | Zugzwang (pass = beneficial) occurs almost exclusively in K+P endings. With any non-pawn piece the risk is low. "K-only" is the minimal guard; "non-pawn piece count ≥ 1" is the consensus choice (Fruit/Toga default; jdart/Arasan). | TalkChess t=73713; TalkChess t=82201; CPW-Engine |
| Depth threshold | `depth >= 3` | R=2 on a null-move at depth=2 gives depth-1=depth-1-R=−1 which bottoms to qsearch(depth=0). That still runs but is degenerate; depth=3 gives a useful child search. The CPW-Engine uses `depth > 2` (i.e., `depth >= 3`). | CPW-Engine; OpenChess t=2994 |

### Zugzwang Guard Variants — Comparison

| Guard | Positions Skipped | Residual Risk | Recommendation |
|-------|-----------------|--------------|----------------|
| K-only (side has no pieces, no pawns) | Very few | Pawn endgames still get NMP | Too permissive |
| K+P only (side has no pieces, any number of pawns OK) | All pawn endgames | K+R+P rook endings still have rare zugzwang | Standard consensus; cheap |
| Non-pawn piece count ≥ 1 | Same as K+P only | Identical | Preferred wording; matches Fruit/Toga/jdart |
| Non-pawn piece count ≥ 2 | K + one minor + pawns skipped | Over-conservative; ~10% of middlegame positions excluded | Unnecessary; no Elo benefit over ≥1 |
| Phase-based (disable when phase ≥ 85) | Smooth ramp-off | More complex; harder to test | M5.A+ tuning, not v1 |

**Recommendation for M5.A v1**: require `pos.pieces_colored(stm, non-pawn) != Bitboard::EMPTY`. This is one popcount call or a union of bitboards. Equivalent to "non-pawn piece count ≥ 1". Reviewable via a separate named predicate `has_non_pawn_material(pos, stm)`.

---

## 3. Reduction Formula R

The formula determines how far below the current depth the null search runs. Larger R = cheaper null search = riskier (more tactical blindness risk).

| Formula | Effect at depth=6 | Characteristic | Source |
|---------|-------------------|----------------|--------|
| `R = 2` | depth−3 | Static; safe; misses deep cutoffs | Pre-1999 engines, Mediocre Chess |
| `R = 3` | depth−4 | Static; aggressive; may miss tactics | Some older implementations |
| `depth > 6 ? R=3 : R=2` | depth−4 or depth−3 | Two-step; CPW-Engine workhorse | CPW-Engine reference code; OpenChess t=2994 |
| `R = 2 + depth/6` | depth−3 (d=6), depth−4 (d=12) | Smooth linear growth; popular modern default | Forum consensus; roadmap M5.A row |
| `R = 3 + depth/6` | depth−4 (d=6), depth−5 (d=12) | Deeper null; aggressive; needs good eval | Deeper-reduction variant |
| Eval-aware: `R = base + min((static_eval - beta) / SCALE, CAP)` | Variable | Larger bonus when position is clearly winning | Heinz 1999; modern engines |

**Recommendation for M5.A v1**: `R = 2 + depth / 6` (integer division). Rationale:
- At depth 3: R=2 → child at depth 0 (qsearch). Barely safe.
- At depth 6: R=3 → child at depth 2. Good.
- At depth 12: R=4 → child at depth 7. Reasonable.
- Smoothly scales; widely documented as a solid baseline.
- The eval-aware bonus `min((static_eval - beta) / 200, 3)` adds +R if we are way above beta. This is documented as beneficial but adds a tuning parameter. **Defer to post-M5.A SPRT-tune.** Include as a placeholder constant `NMP_EVAL_SCALE = 200` that can be activated via a one-liner later.

**Open question (to SPRT-tune)**: compare `R = 2 + depth/6` vs `depth > 6 ? 3 : 2`. Both are SPRT-positive starting points; pick one for v1, tune in a follow-up.

---

## 4. Verification Search

### What It Is

After the null-move sub-search fails high (`null_score >= beta`), instead of immediately returning the cutoff, run a second reduced-depth search **without allowing nulls** (`allow_null = false`, depth `depth - R - 1` or similar). Only return the cutoff if the verification also fails high.

Purpose: catches zugzwang false positives in positions where the zugzwang guard's material threshold was too permissive (e.g., K+R+P endings with rook zugzwang).

### Literature Assessment

| Source | Verdict |
|--------|---------|
| Tabibi & Netanyahu 2002 ("Verified Null-Move Pruning") | Catches most zugzwang false positives; worth ~5–15 Elo in positions with non-K+P pieces |
| Robert Hyatt (Crafty testing) | "Does not help at all" in practice — his testing found verification added overhead without Elo gain |
| Forum consensus (OpenChess t=2994) | Mixed; depends on how strict the zugzwang guard already is |

### Recommendation for M5.A v1: **Defer verification search**.

Rationale:
- The `has_non_pawn_material` zugzwang guard already eliminates the highest-risk positions (K+P endings).
- Residual zugzwang risk in K+R+P or K+B+P endings is real but rare in standard time-control games.
- Verification search doubles the work on the "uncertain" branch; net effect is usually zero or marginal given a good material guard.
- If SPRT analysis post-M5.A shows unusual Elo loss in endgame positions, add verification in a follow-up targeting depth ≥ 8 (the minimum where the verification sub-search has enough depth to be useful).
- Fallback safety: the `has_non_pawn_material` guard is the primary protection; SPRT is the gate.

---

## 5. `make_null_move` / `unmake_null_move` Mechanics

### State Changes Required

| Field | Change | Why |
|-------|--------|-----|
| `side_to_move` | Flip (White→Black or Black→White) | The null move passes the turn to the opponent |
| `ep_target` | Clear to `None` | A null move forfeits any pending en-passant capture (same semantics as any non-double-pawn move: the EP right expires). **Critical**: failure to clear EP means the post-null position has a stale EP target that pollutes the Zobrist key. |
| `halfmove_clock` | Increment by 1 | A null move is not a capture or pawn push; the clock advances. |
| `fullmove_number` | **Only** increment if side was Black (same rule as regular moves: fullmove increments after Black moves) | FIDE convention. |
| `castling` | Untouched | No rook or king moved. |
| Piece bitboards | Untouched | No piece moves. |
| Mailbox | Untouched | No piece moves. |
| King squares | Untouched | No piece moves. |
| `static_eval_white` | **Untouched** | No piece moved, so the incremental PST field is still valid. It will be sign-flipped for the opponent at the next call site. |
| `zobrist` | Incremental update (see below) | Must reflect the changed position. |

### Zobrist Incremental Update

ADR-0009 (Polyglot): the key includes a turn key (XOR once per side-flip) and an EP file key (XOR when an EP capture is pseudo-legally possible). Per ADR-0009's "EP only when pseudo-legally possible" rule, the EP key is only active if there is a pawn that can capture. On a null move:

```
new_zobrist = prior_zobrist
    XOR turn_key()                   // flip side to move
    XOR ep_file_key(prior_ep_file)   // if prior EP was active (Polyglot-style), remove it
                                     // no new EP: null move doesn't create EP
```

Where `prior_ep_file = zobrist::ep_file_to_hash(pos)` — returns `Some(file)` only if there is a pseudo-legal EP capturer for the *current* side-to-move, per the ADR-0009 rule. This is identical to the existing `update_zobrist_after_make` code's EP-out step.

No piece key changes — no pieces moved.

### `NullUndo` Type

The existing `Undo` struct (M1.E) records `prior_ep`, `prior_halfmove`, `prior_zobrist`, piece captures, castling changes, etc. For a null move, piece data and castling data are irrelevant but would be zeroed/defaulted.

**Recommended approach**: a separate `NullUndo` type containing only what changed:

```rust
pub(crate) struct NullUndo {
    prior_ep: Option<Square>,
    prior_halfmove: u8,
    prior_zobrist: u64,
    // prior_fullmove is derivable (decrement if current stm==White after null),
    // but storing it is simpler than deriving — 2 extra bytes, negligible.
    prior_fullmove: u16,
}
```

Tradeoffs vs. reusing `Undo`:
- **Separate `NullUndo`**: smaller (16 bytes vs ~48 bytes); semantically clear; no risk of confusing null-undo with regular-undo in call sites. Recommended.
- **Reusing `Undo`**: reuses existing infrastructure but the struct carries 30 bytes of irrelevant zeroed fields; the type name is misleading at null-move call sites.

### Signature Recommendation

```rust
pub(crate) fn make_null_move(pos: &mut Position) -> NullUndo;
pub(crate) fn unmake_null_move(pos: &mut Position, undo: NullUndo);
```

Located in `src/mov.rs` alongside `make_move`/`unmake_move`. Alternatively as `Position::make_null_move` / `Position::unmake_null_move` methods per the M1.E delegation pattern (`pos.make_move(mv)` calls `crate::mov::make_move(pos, mv)`). Either is fine; the delegation pattern is already established.

### Test Coverage Required

- Round-trip: `make_null_move` then `unmake_null_move` restores all fields including zobrist.
- EP cleared: position with active EP target → after null, `pos.ep_target() == None`.
- Zobrist: manually compute expected hash after null (XOR turn_key, XOR out EP file if active) and assert match.
- Halfmove clock: increments by 1.
- Side to move: flips.
- No piece change: all piece bitboards and mailbox unchanged.
- Fullmove increments on Black→White; not on White→Black.
- `static_eval_white` unchanged.

---

## 6. Negamax Integration

### Exact Prologue Ordering

Per the roadmap pinned constraint ("Slots into the `negamax` prologue **after** the TT probe + cutoff check, before the move loop") and ADR-0018 §13:

```
1. clear PV slot at this ply
2. depth == 0 → qsearch
3. nodes increment + cancellation poll
4. capture original_alpha
5. ply > 0: repetition + 50-move draw check
6. Mate-distance pruning (MDP)
7. TT probe → (tt_move, optional early return at non-PV)
8. *** NEW: Static-eval read (first prologue consumer) ***
9. *** NEW: NMP block (uses static_eval + beta) ***
10. Generate moves + terminal check
11. Order moves
12. Move loop + recursive negamax
13. TT store
```

Why this order:
- **MDP before NMP**: MDP may tighten beta downward (toward `MATE - ply`). The NMP gate `static_eval >= beta` should see the MDP-tightened beta so it correctly skips NMP if beta is a mate score (very tight window).
- **TT probe before NMP**: The TT probe may return an early cutoff, saving the NMP entirely. TT-probe cost is cheap (one cache line fetch); NMP costs an entire sub-search. Cheaper check first.
- **Static-eval read at step 8**: This is the first negamax prologue consumer of `static_eval_white`. The incremental eval field is available from `pos.static_eval_white()`. Sign-flip for STM is needed.
- **NMP before move generation**: If NMP cuts off, we avoid the `generate_moves` call entirely. Generate-moves is ~40ns at interior nodes; saving it matters.

### Static-Eval Sign Convention

`pos.static_eval_white()` returns a White-perspective score. For NMP's `static_eval >= beta` gate, we need the side-to-move perspective:

```rust
let static_eval = if pos.side_to_move() == Color::White {
    pos.static_eval_white()
} else {
    -pos.static_eval_white()
};
```

This is a one-branch integer negation. No new method needed on `Position` for M5.A v1; the inline conversion is readable. M5.B/RFP reuses the same `static_eval` variable (it's already in scope at step 9 and visible at step 10 where RFP fires).

### Null-Search Recursive Call

```rust
let null_undo = pos.make_null_move();
self.history.push(pos.zobrist());  // maintain history invariant
let null_score = -self.negamax(
    pos,
    depth - 1 - R,
    ply + 1,
    -beta,        // child: alpha = -beta
    -beta + 1,    // child: beta  = -beta + 1   (zero window)
    false,        // is_pv = false (null branch is never PV)
    false,        // allow_null = false (stacked-null prevention)
    ctx,
    clock,
);
self.history.pop();
pos.unmake_null_move(null_undo);
```

Note: `negamax` needs a new `allow_null: bool` parameter (see §6.1 below).

### 6.1 Stacked-Null Guard: Flag vs Ply Counter

Two design options:

| Approach | Mechanism | Tradeoff |
|----------|-----------|---------|
| `allow_null: bool` (parameter) | Pass `false` in the null-search recursive call; all other calls pass `true` | Simple; zero overhead; proven (Mediocre, CPW-Engine, jdart). No ambiguity: only one "skip" level. |
| `ply_of_last_null: Option<u32>` (state field) | Track the ply of the most recent null in the current path; skip NMP if `ply - ply_of_last_null <= 1` | Allows "skip only adjacent"; could allow nulls 2+ plies apart. More complex; no documented Elo advantage over the bool. |

**Recommendation**: `allow_null: bool` parameter. Thread it through `negamax` like `is_pv`. In the null-search recursive call, pass `allow_null = false`. All normal recursive calls pass `allow_null = true`.

This adds one `bool` to the negamax signature (same as `is_pv`). At M5, clawfish is not yet on PVS; the `is_pv` parameter will eventually be replaced by the window-width check. `allow_null` is a new explicit parameter that documents intent and is directly testable.

---

## 7. TT Interaction

### Store Policy

After a null-move fail-high, **store in the TT as `Bound::Lower` at the current depth, with `best_move = 0`** (no best move — the cutoff came from the null search, not from trying any move at this node).

Rationale:
- ADR-0018 §7's best-move preservation rule: "When the new store has `best_move == 0` AND the slot's current entry has the same key with a non-zero `best_move`, the old `best_move` is preserved." So storing `best_move = 0` after a NMP cutoff is safe; the existing best-move hint is not destroyed.
- The `Bound::Lower` entry is valid: any probe at this node that sees score ≥ beta will correctly cut off.
- **Mate-score adjustment** (ADR-0018 §5): `score_to_tt(null_score, ply)` still applies when storing. A null-score that looks like a mate score gets the standard ply adjustment on store and the inverse on probe.
- **Practical impact**: storing helps subsequent probes of the same position. The cost is one cache write (already in cache from the probe). Don't skip it.

### Probe Policy

No change needed. The TT probe already happens at step 7 (before NMP). The result of the TT probe — specifically `tt_move` for ordering and the possible early-return — is fully consumed before NMP runs.

### Open Question (flag as design detail for plan)

Should NMP itself call `tt.probe()` to get a refutation move from the null-search's TT entry, to use as a threat detection hint? This is documented in CPW as beneficial for move ordering ("extract a move that refuted null move from the TT, record the target square, give a move ordering boost for escaping from that square"). This is a non-trivial addition and should be **deferred to a post-M5.A refinement**. Not in M5.A v1 scope.

---

## 8. PV Table Interaction

No changes needed to the triangular PV table (ADR-0016 §5).

When NMP returns a cutoff, we return without entering the move loop. `pv.clear_ply(ply)` was called at step 1 of the prologue. So `pv.lengths[ply] = 0` at this point, which is correct: this node didn't establish a PV move. The parent's `pv.update` will not copy from this ply because the parent only copies `pv.lengths[ply+1]` entries — and `pv.lengths[ply] = 0` means the parent sees an empty child PV, which is correct for a beta-cutoff node.

The static-eval read at step 8 has no PV effect.

---

## 9. Static-Eval API

`Position::static_eval_white()` already exists (M3.A / ADR-0014). It is incremental: updated by `make_move` / `unmake_move` via the PST delta stored in `Undo`. After `make_null_move`, the value is unchanged (no pieces moved), so it is valid immediately for use in the NMP gate.

**No new method needed for M5.A v1.** The sign-flip inline:

```rust
let static_eval = if pos.side_to_move() == Color::White {
    pos.static_eval_white()
} else {
    -pos.static_eval_white()
};
```

This is one branch + one arithmetic op. It's used at the NMP gate and will be reused by M5.B's RFP. The pattern is clean and testable. A helper method `pos.static_eval_stm()` could be added later if the call sites proliferate.

---

## 10. Aspiration Windows Interaction

**No special handling required.** Aspiration windows (M4.D) introduce zero-window nodes at `beta - alpha == 1` (non-PV). These are exactly the nodes where NMP is most valuable — they are the highest-volume cut nodes in the tree. The `beta - alpha > 1` (non-PV) gate that enables NMP fires correctly for all aspiration re-search nodes.

The aspiration outer loop is not affected by NMP. NMP fires inside `negamax`, not in the ID loop.

One interaction to note: during an aspiration re-search (second tier, asymmetric full-window), NMP runs normally on all non-PV, non-check nodes. TT entries from the first aspiration try (stored as `Bound::Lower` or `Bound::Upper`) remain valid and will be probed before NMP. No special NMP suppression is needed in re-searches.

---

## 11. Iterative Deepening Interaction

No changes to the ID outer loop. NMP's effect is purely inside `negamax` — it reduces the number of nodes searched at each depth. The ID loop sees fewer nodes per depth and either completes the same depth faster (leaving more time budget for deeper iterations) or reaches +1–2 plies within the same time budget.

Typical bench-node delta: **30–50% node reduction** at fixed depth (empirical; engine-dependent; clawfish with strong M4 ordering should be at the upper end). The bench signature will change significantly.

NMP does not interact with the `last_complete` snap or `prior_root_move` logic. Killers persist across iterations normally.

---

## 12. Failure Modes and Pitfalls

| Failure Mode | Cause | Mitigation | Residual Risk |
|---|---|---|---|
| Stacked nulls | `allow_null` not threaded correctly; child null-search calls parent gate without flag | Always pass `allow_null = false` in the null sub-search; test with a position where NMP would stack | None if flag is correct |
| Zugzwang false positive | K+P ending where pass is beneficial; material guard too permissive | `has_non_pawn_material` guard; verify with an endgame test position | Low (K+R+P rare zugzwang) |
| Returning a mate score from null | `null_score` is `MATE_IN_MAX_PLY` (the opponent can force mate after a null move); returning this as the node score misrepresents the real position | When `null_score >= beta`: if `null_score >= MATE_IN_MAX_PLY`, return `beta` instead of `null_score`. The position may be dangerous but the engine didn't actually try moves. | Correctness issue if omitted |
| Static-eval sign error | Forgetting to flip `static_eval_white` for Black STM | Use the explicit sign-flip; pin with tests for both White and Black positions | Test-catchable |
| EP not cleared on null | Null move inherits prior EP target; Zobrist is wrong; TT collisions | Always clear `ep_target` in `make_null_move`; verify with round-trip hash test | Test-catchable |
| Halfmove clock not incremented | Null move treated as a zeroing move (it is not) | Increment in `make_null_move`; round-trip test pins this | Test-catchable |
| NMP at PV nodes | `is_pv` check omitted; TT cutoff suppression at PV already prevents TT cutoffs but NMP is separate | Explicitly gate on `beta - alpha == 1` or equivalently `!is_pv` — both signals are available | If omitted, PV display shortened |
| NMP in check | `in_check` not called before null; resulting position leaves king attacked | Gate `!in_check(pos)` first; test with a position in check where NMP would fire | Test-catchable |
| Depth underflow | `depth - 1 - R < 0` when `depth = 2`, `R = 2` gives `depth = -1` → saturates to 0 and calls qsearch | `depth >= 3` threshold prevents `depth - 1 - R < 0` for any `R >= 2` | Covered by depth gate |
| NMP at ply=0 | Root must return a move; NMP cutoff at root returns no move | Add `ply > 0` guard (or rely on the fact that the root is always a PV node, and the `!is_pv` gate already prevents this) | Covered by non-PV gate |
| Bench determinism | `make_null_move` mutates `halfmove_clock` or other field not restored → bench nodes differ across runs | Full `NullUndo` round-trip restores all fields; bench-position reset calls `Engine::reset_for_new_game` which is unchanged | Test: re-run bench, same count |

---

## 13. M5.A vs M5.B — Recommendation: Keep Separate

The roadmap asks for a recommendation on whether to bundle NMP (M5.A) with RFP (M5.B).

### Depth Range Analysis

| Technique | Typical depth range | Condition |
|-----------|---------------------|-----------|
| NMP | `depth >= 3` — deepens with `R = 2 + depth/6` | `static_eval >= beta`, non-PV, not in check, has material, no stacked null |
| RFP | `depth <= 3` (typically d=1/2/3) | `static_eval - margin*depth >= beta` |

At exactly `depth = 3` both techniques can apply. NMP fires first in the prologue order (step 9 above); if NMP cuts off, RFP never runs. If NMP does not cut off (no null-move refutation), RFP at d=3 gets a chance.

### SPRT Signal Separability

| Technique | Typical Elo gain | TC dependence | Independence |
|-----------|-----------------|---------------|-------------|
| NMP | +30–70 Elo (literature; +54 Elo at 10s/game, CT800; ~100 Elo when added first) | Strong TC amplification (deeper = more benefit) | Large, independently measurable signal |
| RFP | +20–50 Elo (literature estimates) | Moderate | Smaller, independently measurable signal |

### Code-Region Overlap

Both techniques read `static_eval` from the same prologue location (step 8). They are adjacent blocks:

```
// Step 9a: NMP (depth >= 3)
if allow_null && !in_check && !is_pv && depth >= 3 && has_material && static_eval >= beta {
    // ... null search ...
    if null_score >= beta { return null_score; }
}

// Step 9b: RFP (depth <= 3) — candidate M5.B location
if !in_check && !is_pv && depth <= 3 && static_eval - RFP_MARGIN * depth as i32 >= beta {
    return static_eval;
}
```

They are two independent `if` blocks sharing only the `static_eval` variable. Not entangled.

### Recommendation: Keep Separate

- The SPRT signals are independently large and distinct enough to measure cleanly.
- The depth-3 overlap is not a problem in code (sequential blocks) or in attribution (if both are present from day one, we can't isolate either's contribution).
- Separate SPRTs give cleaner Elo attribution and allow tuning each technique's parameters independently (NMP's `R` formula and zugzwang guard vs. RFP's per-depth margin table).
- Merging would produce a single SPRT that conflates the two techniques' parameter spaces — a risk noted in the roadmap's bundle-decision guidance.

---

## 14. Expected Elo and Bench-Node Delta

| Metric | Estimate | Conditioning |
|--------|----------|-------------|
| Elo gain (CPW prior) | +30–70 Elo | Engine-dependent; best with strong move ordering |
| Elo gain (CT800 at 10s) | +54 Elo | Measured; 6k games |
| Elo gain (as first search optimization) | ~100 Elo | Before other pruning; clawfish has M4 ordering so lower |
| Bench-node delta | −30 to −50% nodes at fixed depth | Depends on branching factor + eval tightness |
| NPS impact | Negligible to small increase | NMP reduces nodes; fewer `generate_moves` calls; instruction cache benefit |
| TC amplification | Strong: deeper searches benefit more | Mixed-TC SPRT is correct methodology |

Clawfish has TT + killers + history + aspiration (M4 complete). This is strong first-move ordering, which is the primary condition for NMP to deliver the upper end of the +30–70 Elo range. Expected gain: **+40–70 Elo** at slow TC, **+20–40 Elo** at fast TC.

---

## 15. SPRT Methodology for M5.A

| Parameter | Value |
|-----------|-------|
| Baseline tag | `M4.D` (M4.D end) |
| SPRT bounds | `elo0=0, elo1=10` (NMP is large enough; lower to 5 only if surprisingly weak) |
| TC sampling | `--tc-sample 10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1` (mixed-TC; NMP amplifies with depth) |
| Acceptance criterion | Mixed-game SPRT H1 + Δ Elo ≥ 0 at every TC |
| Follow-up tune | After SPRT accepts: compare `R = 2 + depth/6` vs `depth > 6 ? 3 : 2`; compare static-eval gate vs no-gate (~+4 Elo marginal per t=69722) |

---

## 16. Sources Cited

- [CPW — Null Move Pruning](https://www.chessprogramming.org/Null_Move_Pruning)
- [CPW — Null Move Reductions](https://www.chessprogramming.org/Null_Move_Reductions)
- [CPW — Null Move](https://www.chessprogramming.org/Null_Move)
- [CPW — Node Types](https://www.chessprogramming.org/Node_Types)
- [CPW — Zugzwang](https://www.chessprogramming.org/Zugzwang)
- [CPW — Reverse Futility Pruning](https://www.chessprogramming.org/Reverse_Futility_Pruning)
- [CPW — CPW-Engine search (reference implementation)](https://www.chessprogramming.org/CPW-Engine_search)
- [CPW — Fail-Soft](https://www.chessprogramming.org/Fail-Soft)
- [Mediocre Chess — Guide: Null-moves](http://mediocrechess.blogspot.com/2007/01/guide-null-moves.html)
- [TalkChess t=84260 — Criteria for Null Move Pruning](https://talkchess.com/viewtopic.php?t=84260)
- [TalkChess t=82201 — Null Move Pruning gives worse results (p.2)](https://talkchess.com/viewtopic.php?t=82201&start=10)
- [TalkChess t=81949 — Importance of Null Move Pruning](https://talkchess.com/viewtopic.php?t=81949)
- [TalkChess t=73713 — Allowing null move pruning in the endgame](https://talkchess.com/viewtopic.php?t=73713)
- [TalkChess t=69722 — Null move pruning, only when score >= beta?](https://www.talkchess.com/forum3/viewtopic.php?t=69722)
- [TalkChess t=74279 — Null move heuristic implementation](https://talkchess.com/viewtopic.php?t=74279)
- [TalkChess t=29797 — Null move pruning](https://talkchess.com/viewtopic.php?t=29797)
- [TalkChess t=38640 — Null Move Pruning](https://talkchess.com/viewtopic.php?t=38640)
- [OpenChess t=2994 — Null Move recommendations](https://open-chess.org/viewtopic.php?t=2994)
- [Donninger, C. (1993) "Null Move and Deep Search: Selective Search Heuristics for Obtuse Chess Programs." ICCA Journal 16(3):137–143](https://www.semanticscholar.org/paper/Null-Move-and-Deep-Search%3A-Selective-Search/4de06bff5eee7b9ca9c12c60a8e03b1c5ef6dcf4)
- [Heinz, E.A. (1999) "Adaptive Null-Move Pruning." ICCA Journal 22(3):123–132](https://link.springer.com/chapter/10.1007/978-3-322-90178-1_3)
- [Tabibi & Netanyahu (2002) "Verified Null-Move Pruning." ICGA Journal](https://www.researchgate.net/publication/297377298_Verified_Null-Move_Pruning)
- [Wikipedia — Null-move heuristic](https://en.wikipedia.org/wiki/Null-move_heuristic)
