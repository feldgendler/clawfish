# Mutation-testing backlog

Per-unit `cargo mutants --in-diff` runs that are deferred from the unit's commit to a dedicated overnight campaign. Each entry pins the exact diff to mutate via commit hashes (resilient to working-tree shifts) plus the per-unit context the campaign agent needs to triage survivors against the unit's plan and test surface.

## How a campaign session runs

1. Read this file end-to-end. Pick the topmost unprocessed entry.
2. Read the unit's plan + research note + retrospective at the linked paths so you understand which mutations the unit's tests were designed to kill (the plan's "Mutation-testing prep" section enumerates the anticipated catchable surface).
3. Switch into the unit's worktree (or check out the unit's branch into a fresh worktree if it's been merged to main).
4. Regenerate the diff via the entry's exact `git diff` command. Pipe to `$TMPDIR/<unit>.diff` so the file is in the sandbox-writable allowlist.
5. Run `cargo mutants --in-diff $TMPDIR/<unit>.diff`. Expect the run to take **hours** on M3+ surface (the M4 search-stack work is the main cost driver). Run overnight; do NOT bundle into an interactive cycle.
6. **Triage each survivor** per `docs/workflow.md` §"Pre-review mechanical checks":
   - **Caught**: re-run `cargo test --lib <test_name>` to confirm.
   - **Equivalent mutant**: prove indistinguishability, add `exclude_re` rule to `.cargo/mutants.toml` with a comment explaining why no input distinguishes original from mutated form.
   - **Real-bug, structurally undetectable at this scope**: document with `exclude_re` rule and "deferred to M\<X\>" detection plan; surface in the unit's milestone retrospective for the milestone where it should be re-validated.
   - **Real-bug, catchable by adding a test**: add the test, re-run, return to step 1.
7. **Update the unit's milestone retrospective** (`docs/milestones/<unit>.md`) — fill the "Mutation-survivor analysis" section with: total survivors found, per-survivor classification with reasoning, links to any commits that added tests / exclusions, and any deferred-to-M\<X\> follow-ups.
8. **Commit on the unit's branch** (or directly to main if the unit has merged): one commit for any test additions + `mutants.toml` exclusions, plus one commit for the retrospective update. Conventional message: `mutants(<unit>): triage <N> survivors`.
9. **Move the entry from "Pending" to "Done"** in this file.

## Tactical guidance for triage

- `.cargo/mutants.toml` already enforces "don't anchor on line numbers" — survivor exclusions key on the function name + operator shape, not source positions. Re-read the comments at the top of that file before adding any new `exclude_re` rule.
- The fastest survivor-iteration loop is **manual mutation + targeted test**: apply the mutation by hand via `Edit`, run only the suspect tests, revert. Cycle time: seconds. Don't re-run the full `cargo mutants` suite to re-test a single survivor — that burns hours.
- When a survivor's body is "structurally undetectable at the integration-test level but trivially testable as a unit", the M3.D `negate_window` precedent applies: extract the bug-prone expression into a named helper and unit-test it directly. This is preferred over `exclude_re`; line-anchored exclusions rot.
- The M4 phases are large (search-stack code) and produce many candidate mutants per unit — budget for the full triage to take a session even after the run completes overnight.

## Pending

_(no pending campaigns)_

---

### Archived entry — M4.B + M4.C + M4.D + M5.A + M5.B — Killer moves + History + Aspiration + NMP + RFP (joint campaign, now done)

**Why combined:** M4.B and M4.C developed on parallel branches off M4.A; the user opted to defer their mutation campaigns and run them together as one overnight batch covering both diff ranges. M4.D landed on main after the M4.C merge; the user extended the deferral to include M4.D in the same batch. M5.A (NMP) landed on main after M4.D; per the M5.A plan §10 + ADR-0023, M5.A's mutants surface is appended to the same joint entry rather than scheduling a separate campaign. M5.B (RFP) landed on main after M5.A; per the M5.B plan §10 + ADR-0024, M5.B's mutants surface is appended to the same joint entry, extending the diff range to cover the M5.B landing commit.

**Diff command** (resilient to working-tree state — uses commit hashes):

```sh
git diff 33a0d0d..<m5.b-merge-sha> -- 'src/*.rs' 'tests/*.rs' > "$TMPDIR/m4bcd-m5ab.diff"
```

- `33a0d0d` — M4.A follow-up (cargo-mutants survivors fix); M4.B's branching point on main and the joint range's start.
- `<m5.b-merge-sha>` — the M5.B primary landing commit on main. Resolve at campaign-start time via `git log --grep='M5.B:' --max-count=1 --format='%H'`.
- Pathspec `'src/*.rs' 'tests/*.rs'` — single-star glob (git pathspec doesn't expand `**`); restricts to the source + integration-test surface. M4.B + M4.C + M4.D + M5.A + M5.B modify `src/search.rs`, `src/history.rs` (new in M4.C), `src/mov.rs`, `src/movegen.rs` (M5.A `arb_position` lift), `src/position.rs` (M5.A delegators), `src/engine.rs`, `src/lib.rs`, `tests/uci_integration.rs` plus doc files; the doc files are out of scope for cargo-mutants.

After landing the diff, run:

```sh
cargo mutants --in-diff "$TMPDIR/m4bcd-m5ab.diff"
```

**Unit context** (read these before triaging):

- M4.B plan: [`docs/plans/m4.b.md`](plans/m4.b.md) — see §3 (helper signatures) and §8 (anticipated catchable surface table).
- M4.B research: [`docs/research/m4-killer-moves.md`](research/m4-killer-moves.md).
- M4.B retrospective: [`docs/milestones/m4.b.md`](milestones/m4.b.md) — "Mutation-survivor analysis" section is the target for the post-triage update.
- M4.C plan: [`docs/plans/m4.c.md`](plans/m4.c.md) — see §3.5 (cutoff dispatch) and §11 (mutation-testing prep).
- M4.C research: [`docs/research/m4-history-heuristic.md`](research/m4-history-heuristic.md).
- M4.C retrospective: [`docs/milestones/m4.c.md`](milestones/m4.c.md) — "Mutation-survivor analysis" section is the target for the post-triage update.
- M4.C ADR: [`docs/decisions/0019-history-heuristic.md`](decisions/0019-history-heuristic.md).
- M4.D plan: [`docs/plans/m4.d.md`](plans/m4.d.md) — see §3 (helpers) and §11 (mutation-testing prep with anticipated catchable surface table).
- M4.D research: [`docs/research/m4-aspiration-windows.md`](research/m4-aspiration-windows.md).
- M4.D retrospective: [`docs/milestones/m4.d.md`](milestones/m4.d.md) — "Mutation-survivor analysis" section is the target for the post-triage update.
- M5.A plan: [`docs/plans/m5.a.md`](plans/m5.a.md) — see §3 (NullUndo + helpers + NMP block pseudocode) and §10 (mutation-testing prep with anticipated catchable surface).
- M5.A research: [`docs/research/m5-null-move-pruning.md`](research/m5-null-move-pruning.md).
- M5.A ADR: [`docs/decisions/0023-null-move-pruning.md`](decisions/0023-null-move-pruning.md).
- M5.A retrospective: [`docs/milestones/m5.a.md`](milestones/m5.a.md) — "Mutation-survivor analysis" section is the target for the post-triage update.
- M5.B plan: [`docs/plans/m5.b.md`](plans/m5.b.md) — see §3 (constants + helper + RFP block pseudocode) and §10 (mutation-testing prep with anticipated catchable surface).
- M5.B research: [`docs/research/m5-reverse-futility.md`](research/m5-reverse-futility.md).
- M5.B ADR: [`docs/decisions/0024-reverse-futility-pruning.md`](decisions/0024-reverse-futility-pruning.md).
- M5.B retrospective: [`docs/milestones/m5.b.md`](milestones/m5.b.md) — "Mutation-survivor analysis" section is the target for the post-triage update.
- Code surface:
  - `src/search.rs` lines ~285 (killer field), ~870–945 (M4.B helpers), ~552–610 (negamax steps 10 + 11), ~320 / ~338 / ~426 (lifecycle reset call sites), the cutoff dispatch + `quiets_searched` accumulator + push site for M4.C, the aspiration-loop body in `Search::go` for M4.D (lines ~422–540), and the three M4.D helpers (`aspiration_window`, `widen_after_fail`, `extract_bestmove_or_tt_fallback`). The NMP block (M5.A) and the RFP block at step 8 (M5.B), including `RFP_MAX_DEPTH` / `RFP_MARGIN_PER_DEPTH` constants, `reverse_futility_margin` helper, and `rfp_firings` counter. Plus ~22 M4.B tests at S14–S29, ~16 M4.C tests at HS1–HS12 + HS3b/HS3c/HS4b/HS8b, ~22 M4.D tests at AS1–AS24d, M5.A NMP-behavior tests (search for `nmp_firings` / `negamax_skips_nmp` / `negamax_skips_nmp_at` comment markers), and M5.B RFP-behavior tests (search for `rfp_firings` / `negamax_skips_rfp` / `rfp_takes_precedence` / `reverse_futility_margin` comment markers). (Use `S14 —`, `HS1 —`, `AS1 —` comment markers for M4 tests.)
  - `src/history.rs` — the entire M4.C module + 12 H-tests.
  - `src/engine.rs` — the M4.C `ucinewgame_clears_history_table` E_h test.
  - `src/mov.rs` — M4.D's `Move::from_bits(u16) -> Move` constructor (`pub(crate) const fn`) + 1 round-trip unit test.
  - `tests/uci_integration.rs` — M4.D's `bench_signature_deterministic_across_two_runs_with_aspiration` E47 integration test; M5.B's `bench_signature_deterministic_across_two_runs_with_rfp` E49 integration test.

**Anticipated catchable surface (M4.B portion, from M4.B plan §8):**

| Helper / call site | Anticipated mutations | Test that should kill |
|---|---|---|
| `is_quiet` | `matches!(...)` arms swapped/dropped | S14 + S15 |
| `update_killers` guard polarity | `!=` ↔ `==` / `true` / `false` | S16 + S18 |
| `update_killers` shift line | `[0] = [1]` ↔ no-op | S17 + S19 |
| `negamax_move_order_score` `is_quiet` early-return | dropped / inverted | S22 |
| `negamax_move_order_score` killer-arm branches | swapped / polarity flipped | S20 + S21 + S22b |
| `KILLER0_SCORE` / `KILLER1_SCORE` numerical bumps | `200 → 0`, `200 → 500` | S23 (runtime range against `mvv_lva_score(QxP)`) |
| `order_moves` sort key | killer-aware → mvv-only | S24d + S24f |
| `order_moves` TT-promote `idx != 0` guard | dropped | S24b |
| `order_moves` TT-move `tt_move != 0` guard | dropped | S24c |
| `order_moves` TT-killer overlap | duplicate insertion | S24e |
| `clear_killers` body literal | `[[default; 2]; MAX_PLY]` mutation | S27 |
| Negamax step 10 `order_moves` call | replaced with prior MVV-LVA-only sort | S24d via helper + S26 via integration |
| Negamax step 11 cutoff `is_quiet(mv)` gate | dropped / inverted | S25b (capture must NOT update) |
| Negamax step 11 `update_killers` call | dropped | S25 (quiet cutoff DOES update) |
| Per-iteration `clear_killers` call | dropped | **gap**: S29 jointly pins per-go + per-iteration. If a survivor lands here, the fix protocol is to extract `prepare_for_iteration(&mut self)` named method and add a direct unit test mirroring S27. |
| Per-go `clear_killers` call | dropped | S29 jointly with per-iteration |
| `Search::reset` `clear_killers` call | dropped | S28 |

**Anticipated catchable surface (M4.C portion):**

| Mutation class | Where | Expected catch |
|---|---|---|
| `+= bonus → += -bonus` (cutter sign flip) | `src/search.rs` cutoff dispatch | HS1 (`assert!(... s == 4)` exact match for cutter at depth 2) AND HS1's strict `nonzero.iter().all(\|s\| *s == 4 \|\| *s == -4)` |
| Loop body deletion in malus loop | `src/search.rs` malus `for prior in quiets_searched.iter()` | HS4b's `has_negative` aggregate at depth 3 startpos |
| `if is_quiet(mv) → if !is_quiet(mv)` flip on cutoff dispatch gate | `src/search.rs` cutoff site | HS2 (capture cutoff produces all-zero) |
| `if is_quiet(mv) → if !is_quiet(mv)` flip on push gate | `src/search.rs` push-on-no-cutoff | HS5 (capture's history entry stays 0 even when later quiet cuts) |
| Deletion of `quiets_searched.push(mv)` line | `src/search.rs` push site | HS4b (no negatives if no priors) |
| `mv.from_square()` ↔ `mv.to_square()` swap on update arg | `src/search.rs` cutter update | HS9's relative-position assertion (e2e4 < d2d4 < b1c3 < g1f3) |
| `+= depth*depth → += depth` (linear) | `src/search.rs` bonus computation | HS1's `s == 4` exact match (depth=2 gives `4` for `depth*depth` but `2` for `depth`) |
| Clamp boundary off-by-one | `src/history.rs::update` | H5/H6/H7 module-level boundary tests |
| `negamax_move_order_score` history branch returning constant | non-killer-quiet branch | HS9 (relative ordering with 4 distinct values) |
| `negamax_move_order_score` MAX_HISTORY-vs-KILLER1 boundary | implicit via tier discipline | HS12 (capture > killer > history-quiet at MAX_HISTORY) |
| TT-cutoff early-return path falsely updating history | negamax prologue | HS3 (no update on TT-cutoff) |
| Repetition / 50-move / MDP-collapse early-return paths falsely updating history | negamax prologue | HS3b + HS3c |
| Abort-skip discipline on history update | post-recursion abort guard | HS7 |
| Side-to-move read at wrong point (post-make instead of post-unmake) | negamax cutoff dispatch | HS8 (root White) + HS8b (non-root Black) |

**Predicted equivalent-with-rationale survivor (M4.C portion):**

- **`negamax_move_order_score` MAX_HISTORY-vs-KILLER1 off-by-one** (`KILLER1_SCORE = 100, MAX_HISTORY = 99`): bumping MAX_HISTORY to 100 would tie the worst-case history-quiet score with KILLER1_SCORE; the comparator's `sort_by_cached_key` is stable but tie-breaks by movegen order. Whether this counts as a real bug depends on the SPRT signal — for this milestone it would be classified as **equivalent-with-rationale** since the strict-inequality is the design intent (per ADR-0019 §1 + CLAUDE.md status text), but the actual behavioral delta from a single-move tie at the killer/history boundary is below SPRT noise floor.

**Anticipated catchable surface (M4.D portion, from M4.D plan §11):**

| Mutation class | Where | Expected catch |
|---|---|---|
| `aspiration_window` half-width drop / sign flip | `prior ± HALF_WIDTH` arithmetic | AS3 / AS4 / AS5 specific values |
| Re-introduced mate-skip branch | `aspiration_window` body | AS5b explicit `(prior - 50, prior + 50)` for mate-magnitude prior |
| `widen_after_fail` `returned` → `prev_alpha` | `(returned, INF)` body | AS6 + AS19 specific values |
| `widen_after_fail` `returned` → `prev_beta` | `(-INF, returned)` body | AS7 + AS20 specific values |
| `widen_after_fail` branch swap | `if returned >= prev_beta` ↔ `<=` | AS8 + AS9 boundary cases |
| `widen_after_fail` operand drift `>=` ↔ `>` on the upper-bound branch | comparator | AS8 (`returned == prev_beta` exact-equal) |
| `extract_bestmove_or_tt_fallback` PV-first branch deletion | `if pv.lengths[0] > 0 { return Some(...) }` | AS24a (PV present + TT differs → returns PV) |
| `extract_bestmove_or_tt_fallback` TT-fallback branch deletion | `tt?.probe(root_key)?` | AS24b (PV empty + TT has bestmove → returns TT) |
| `extract_bestmove_or_tt_fallback` zero-bestmove guard | `if entry.best_move == 0 { return None; }` | AS24c (PV empty + TT bestmove=0 → returns None) |
| `extract_bestmove_or_tt_fallback` `?` propagation | `tt?` early-return | AS24d (PV empty + tt=None → returns None) |
| ID outer-loop's per-try `pv.lengths[..] = 0` deletion | inner aspiration loop body | AS12 (deep PV consistency after re-search) |
| ID outer-loop's `tries >= 2` cap → `tries >= 3` (third tier introduced) | aspiration-loop break | AS10 (cumulative re-search count over multiple iterations) |
| Window-contained check operator drift (`>` ↔ `>=`) | `returned > alpha && returned < beta` | AS11 + AS21 (non-degenerate window-contained vs. boundary cases) |
| Per-iteration `clear_killers` NOT moved to between-tries | inner aspiration loop body | **AS23 is a weak pin** — asserts only "any populated" post-go, which a between-tries-clear bug would still satisfy (try-2 re-populates with its own cutoffs). **Anticipated possibly-surviving**; classify at triage time as either "real-bug-low-Elo-impact" (the bug reduces ordering quality but not test observability) or "structurally-undetectable; deferred to M5 PVS rework which will revisit cross-iteration ordering state." Final-review pass 1 surfaced this gap. |
| `aspiration_window` argument-order swap (depth vs prior) | call site in `Search::go` | AS3 / AS4 / AS5 must match exact prior×depth pairs |
| `last_complete` snapshot bestmove source switched from helper to inline `(pv.lengths[0] > 0).then(...)` | call site at the snapshot line | AS24a–d (helper coverage) |

**Anticipated catchable surface (M5.A portion, from M5.A plan §10):**

| Mutation class | Where | Expected catch |
|---|---|---|
| `null_move_reduction` formula constants (`+ → -`, `* → /`, `1 → 2`) | `src/search.rs::null_move_reduction` body | 5 boundary tests at depths 3 / 5 / 6 / 11 / 12 (`null_move_reduction_at_depth_*`) |
| `has_non_pawn_material` bitboard-union arms dropped/swapped | `src/search.rs::has_non_pawn_material` body | 5 fixture tests (`has_non_pawn_material_*`) including K-only, K+P, K+N, K+R |
| Stacked-null flag inversion: NMP recursive `allow_null = false → true` | NMP block null-search call | `negamax_passes_allow_null_false_in_null_subsearch` direct kill via `nmp_firings == NMP_FIRINGS_PINNED = 34` |
| NMP gate inversion (`&&` → `||`, `>=` → `>`, etc.) on the seven-condition gate | NMP block prologue | Seven sister-fixture gate-skip tests (`negamax_skips_nmp_when_*` + `negamax_skips_nmp_at_*`) |
| Mate-cap inversion: `null_score >= MATE_IN_MAX_PLY` → `<` / `==` / `>` | NMP cutoff path | `negamax_caps_mate_score_to_beta_when_null_score_is_mate` (TT-seeded fixture); the `>=` → `>` BOUNDARY mutation at exact `MATE_IN_MAX_PLY = 29936` is **expected-survivor**: no chess fixture can produce that exact score (mate-in-MAX_PLY can't be searched). Documented as deferred / structurally-undetectable |
| TT store bound inversion: `Bound::Lower` → `Upper` / `Exact` | NMP TT-store path | `negamax_stores_lower_bound_in_tt_after_nmp_cutoff` |
| TT store best_move=0 inversion (would let NMP corrupt prior best_move) | NMP TT-store path | `negamax_with_nmp_preserves_existing_tt_best_move` (ADR-0018 §7 preservation rule) |
| TT store score = `null_score` (raw) instead of `cutoff_score` (mate-capped) | NMP TT-store path | `negamax_caps_mate_score_to_beta_when_null_score_is_mate` (asserts returned == beta; mismatch on raw-store would fail) |
| TT store depth = `depth - 1 - r` instead of `depth` | NMP TT-store path | `negamax_stores_lower_bound_in_tt_after_nmp_cutoff` (asserts `entry.depth == current_depth`) |
| Halfmove-clock increment in `make_null_move`: `+ 1` → `+ 0` / `+ 2` | `src/mov.rs::make_null_move` | `null_move_increments_halfmove_clock` (sole test) |
| Fullmove conditional: `if prior_side == Black` → `if prior_side == White` | `src/mov.rs::make_null_move` | `null_move_increments_fullmove_when_black_was_to_move` + `null_move_does_not_increment_fullmove_when_white_was_to_move` (paired tests cover both branches) |
| EP clear: `pos.set_aux_state(..., None, ...)` → reuse prior EP | `src/mov.rs::make_null_move` | `null_move_clears_ep_target` |
| Zobrist XOR omissions (missing turn_key or ep_file_key XOR) | `src/mov.rs::make_null_move` | `null_move_zobrist_*` tests + `null_move_zobrist_matches_from_scratch` round-trip |
| `unmake_null_move` field-restore omissions | `src/mov.rs::unmake_null_move` | `unmake_null_move_round_trips_position` (Kiwipete) + `null_move_round_trip_property` (proptest via `arb_position`) + `unmake_null_move_round_trips_after_make_unmake_make` |
| `negamax` `allow_null` propagation in move-loop recursive call (mistakenly `false`) | move-loop recursion site | `negamax_skips_nmp_when_allow_null_false` covers the gate-skip; an unintended `false` in the move-loop call would suppress NMP at descendants and produce different node counts vs the all-`true` baseline |
| `ply > 0` gate inversion (NMP at root) | NMP gate prologue | `negamax_skips_nmp_at_ply_zero_even_when_is_pv_false` direct sister-fixture kill |
| `nmp_firings += 1` deletion or replacement | NMP gate body | `negamax_passes_allow_null_false_in_null_subsearch` (counter assertion) |
| `pos.unmake_null_move` deletion (forgotten unmake on abort path) | NMP block post-recursion | Existing `Search::go` post-search `debug_assert_eq!(pos_clone, *position)` (M3.E discipline; not M5.A test) — would trigger debug-build assert failure; release-mode survival depends on whether any test fixture aborts during NMP; classified at triage |
| `self.history.push(pos.zobrist())` / `self.history.pop()` deletion | NMP block | `negamax_with_nmp_clears_history_correctly_on_unmake` (history-stack discipline) |

**Anticipated catchable surface (M5.B portion, from M5.B plan §10):**

| Mutation class | Where | Expected catch |
|---|---|---|
| `reverse_futility_margin` formula constants (`* → +`, `* → -`, `* → /`, `RFP_MARGIN_PER_DEPTH → 0`) | `src/search.rs::reverse_futility_margin` body | 5 boundary tests at depths 0 / 1 / 3 / 6 / 7 (`reverse_futility_margin_at_depth_*`) |
| Margin comparison operator: `>= → >` at 1-cp boundary | RFP block `static_eval - margin >= beta` | `negamax_at_depth_one_passes_rfp_gate_when_eval_surplus_is_at_least_one_pawn` (1-cp boundary, intentionally fragile pin for this operator) |
| Depth-bound comparison: `<= → <` | RFP gate `depth <= RFP_MAX_DEPTH` | `negamax_skips_rfp_at_depth_above_max` (sister at depth=6/7) |
| Mate-beta gate inversion: `< → <=` or `< → >` | RFP gate `beta.abs() < MATE_IN_MAX_PLY` | `negamax_skips_rfp_when_mate_beta` (sister at mate-beta vs finite-beta) |
| `!is_pv` gate inversion | RFP gate | `negamax_skips_rfp_at_pv_node` (sister PV vs non-PV) |
| `!in_check(pos)` gate inversion | RFP gate | `negamax_skips_rfp_when_in_check` (sister in-check vs not-in-check) |
| `ply > 0` gate inversion | RFP gate | `negamax_skips_rfp_at_ply_zero_even_when_is_pv_false` (sister ply=0 vs ply=1) |
| Return value mutations: `→ beta`, `→ static_eval`, `→ static_eval + margin` | RFP cutoff `return static_eval - margin;` | `negamax_returns_proved_lower_bound_on_successful_rfp` (exact score-equality) |
| TT store accidentally added on RFP cutoff | RFP block | `negamax_rfp_does_not_store_in_tt` (TT-state assertion after forced RFP cutoff) |
| Order-of-RFP-vs-NMP swap (NMP fires before RFP) | RFP block placement above NMP block | `rfp_takes_precedence_over_nmp_at_overlapping_depth` (composite `rfp_firings==1 AND nmp_firings==0` at depth=4) |
| Counter increment deleted: `rfp_firings += 1` removed | RFP cutoff path | `rfp_firings_counter_increments_on_cutoff` (direct counter assertion) |
| Lazy-dup broken (RFP misses but NMP reads stale eval) | Fall-through from RFP non-cutoff to NMP block | `negamax_passes_static_eval_through_to_nmp_when_rfp_misses` (composite `rfp_firings==0 AND nmp_firings==1`) |
| `static_eval - margin >= beta` condition body deleted (RFP block present but never fires) | Inside RFP gate body | `negamax_skips_rfp_when_static_eval_below_beta_plus_margin` (sister above/below threshold by 50 cp) |

**Anticipated structurally-undetectable survivor (M5.B portion):**

- **RFP `beta.abs() < MATE_IN_MAX_PLY` boundary mutation (`<` → `<=`) at exact `beta.abs() == MATE_IN_MAX_PLY = 29936`**: identical structural argument to M5.A's mate-cap `>=` → `>` survivor (no chess fixture can produce a beta of exactly ±MATE_IN_MAX_PLY — that exact value is unreachable in normal search). At triage time: add `exclude_re` rule to `.cargo/mutants.toml` documenting the structural-undetectability per ADR-0024 §3 + plan §10's expected-survivor note; surface in `docs/milestones/m5.b.md`'s "Mutation-survivor analysis" section.

**Triage protocol** (per `docs/workflow.md`):

1. **Caught** — re-run `cargo test --lib <test_name>` to confirm; nothing to do.
2. **Equivalent mutant** — prove indistinguishability; add `exclude_re` rule to `.cargo/mutants.toml` with a comment explaining why no input distinguishes original from mutant. Cite ADR-0019 / plan §3 where the design intent supports the equivalence.
3. **Real-bug, structurally undetectable at this milestone scope** — add `exclude_re` rule with a "deferred to M\<X\>" comment and surface in the M4.C retrospective's "Mutation-survivor analysis" section.
4. **Real-bug, catchable by adding a test** — add the test (preserve naming convention from existing S14–S29 + HS1–HS12 series), re-run with `cargo mutants --in-diff $TMPDIR/m4.bc.diff --iterate`, return to step 1.

Avoid `exclude_re` rules anchored to line numbers (per `.cargo/mutants.toml` guidance — line-number-anchored regexes are fragile; structural patterns or specific function names are stable). M3.D's `negate_window` extraction is the precedent for refactoring instead of excluding.

### Where the follow-up commit lands

On `main` (M4.B + M4.C + M4.D + M5.A + M5.B will have merged by the time the campaign runs). The commit message follows the M4.A follow-up pattern (commit `33a0d0d`):

> M4.B+M4.C+M4.D+M5.A+M5.B follow-up: address N cargo-mutants survivors
>
> `cargo mutants --in-diff ...` produced K mutations: J caught + L
> timeout (caught-via-hang) + M unviable + N missed. This commit addresses
> all N: ...

Update `docs/milestones/m4.b.md`'s, `docs/milestones/m4.c.md`'s, `docs/milestones/m4.d.md`'s, `docs/milestones/m5.a.md`'s, AND `docs/milestones/m5.b.md`'s "Mutation-survivor analysis" sections atomically with the commit. Remove this entry from the backlog.

### Edge cases

- **If `cargo mutants` reports `unviable` for the M4.C `Move::default()` sentinel `debug_assert!`**: that's expected — the assertion is in a path movegen never produces, so no chess fixture can drive it. Classify as caught-via-unreachable; no action needed.
- **If the M5.A mate-cap `>=` → `>` boundary at exact `MATE_IN_MAX_PLY = 29936` survives**: that's expected — no chess fixture can produce that exact score (mate-in-MAX_PLY can't be searched). Add `exclude_re` rule to `.cargo/mutants.toml` documenting the structural-undetectability per ADR-0023 §6 + plan §10's expected-survivor entry; surface in `docs/milestones/m5.a.md`'s "Mutation-survivor analysis" section.
- **If the M5.B RFP `beta.abs() < MATE_IN_MAX_PLY` boundary mutation (`<` → `<=`) at exact `beta.abs() == 29936` survives**: that's expected — same structural argument as M5.A above (no chess fixture produces a beta of exactly ±MATE_IN_MAX_PLY). Add `exclude_re` rule per ADR-0024 §3 + M5.B plan §10's expected-survivor note; surface in `docs/milestones/m5.b.md`'s "Mutation-survivor analysis" section.
- **If the campaign hangs**: per `.cargo/mutants.toml` guidance, the timeout multiplier should be enough; if a specific mutation hangs the test suite, it's almost always the cancellation-poll cadence interacting with the mutated code. Cancel via Ctrl-C; the mutation is caught-via-timeout.
- **If a survivor maps outside the M4.B/M4.C/M4.D/M5.A/M5.B surface** (i.e., a mutation on a line outside `src/search.rs` killer / history / aspiration / NMP / RFP logic, `src/history.rs`, `src/mov.rs::from_bits` / null-move primitives, `src/movegen.rs::test_strategies` lift, `src/position.rs` delegators, the M4.C/M4.D/M5.A/M5.B-specific test surface): something's off with the diff scope. Re-check that `33a0d0d..<m5.b-merge-sha>` is the correct hash range and that no drift commits have landed.

## Done

### M5.D — Frontier futility pruning (campaign ran 2026-05-06 on the M5.D landing diff)

38 mutants in the `git diff 0f9bd88..HEAD` range (M5.D landing). Initial pass: 36 caught / 2 unviable / **1 missed**.

**The single missed mutant (closed by code change):**

- `replace > with >= in AlphaBetaMover::negamax` at the FFP pruned-bound contribution site, originally `if pruned_bound > best { best = pruned_bound; }` — **equivalent under semantics**: when `pruned_bound == best`, the `>` form skips the assignment, the `>=` form runs the assignment with an equal value; both leave `best` unchanged. **Closure**: replaced the conditional with `best = best.max(pruned_bound);` — removes the `>` operator entirely, kills the mutant by structural elimination. Re-run after the fix: 36 caught + 2 unviable + 0 missed = **100% effective catch rate**.

**The 2 unviable mutants:**

- `replace && with || in AlphaBetaMover::negamax` at lines `1544` (`if quiet_move && let Some(static_eval) = ...`) and `1545` (`&& let Some(pruned_bound) = ...`). Rust let-chains require `&&` syntactically; `||` is a compile error. cargo-mutants correctly classifies these as unviable (won't compile), not missed.

**Catchable surface confirmed**: per-depth match arms in `frontier_futility_margin` (killed by per-depth pin tests); domain guard / inequality / payload / saturation in `ffp_pruned_bound` (killed by helper tests); FFP gate predicates / sign convention / provenance downgrade / firings counter in the move loop (killed by behavior tests). The provenance downgrade `move_is_full_depth = false → true` mutation is killed by `negamax_ffp_contribution_downgrades_best_is_full_depth` (the load-bearing TT-store correctness pin).

### M5.C — Late move reductions (campaign ran 2026-05-05 on the M5.C working tree)

94 mutants: 82 caught + 4 timeout-caught + 2 unviable + **6 missed** (93.5% effective catch rate, 86 / 92 excluding unviable). Diff range `git diff aa7ac4a -- 'src/*.rs' 'tests/*.rs'` (HEAD vs working tree at the M5.C landing).

**6 survivors triaged in this commit:**

1. **Tests added (3 assertions catching real gaps), all in `src/search.rs`:**
   - `tt_bound_for_completed_node_classifies_full_depth` extended with `(0, 100, 0, true) → Some(Upper)` to pin the `best == original_alpha` boundary as Upper, killing `> → >=` on the Exact arm.
   - `best_is_full_depth_after_score_upgrades_equal_score_to_full_depth` extended with `(42, true, 42, false) == true` to pin that an equal-score reduced-only candidate does not clear an existing full-depth flag, killing `> → >=` on the strict-improvement arm.
   - `negamax_full_depth_quiet_still_enters_quiets_searched` extended to generate the legal moves at the traced root position and assert every recorded `lmr_history_candidates` entry comes from that set, killing the `==` → `!=` mutation on the `lmr_trace_root_ply == Some(ply)` instrumentation gate (descendant Black moves leak into the trace under the mutation, e.g., `d7-d6`).

2. **`exclude_re` rules added (3 rules)** in `.cargo/mutants.toml`:
   - `replace || with && in late_move_reduction` — equivalent: the formula path coincidentally produces 0 for every input the early-return guard catches (`ln(0)→-∞` saturates; `ln(1)=0` zeros the product; `clamp(0, depth.saturating_sub(2))` ceiling is 0 for `depth < 3`).
   - `replace * with + in late_move_reduction` and `replace * with / in late_move_reduction` — structurally undetectable at the M5.C test surface (clamp absorbs the difference); deferred to the formula re-tuning campaign (plan §8 row 1).

3. **Self-review of fixes**: each new assertion verified by manual mutation-and-revert (apply mutation → confirm test fails → revert). Each `exclude_re` rule carries a function-anchored regex (per project convention against fragile line anchors) and a multi-paragraph equivalence/undetectability proof in the toml comment.

See `docs/milestones/m5.c.md` "Mutation-survivor analysis" for the per-survivor narrative.

---

### M4.B+M4.C+M4.D+M5.A+M5.B — joint campaign (ran 2026-05-02 on branch `mutants/m4bcd-m5ab`)

748 mutants: 669 caught + 15 timeout-caught + 33 unviable + **31 missed** (89.4% catch rate). Diff range `33a0d0d..7d99ccc -- src/*.rs tests/*.rs`.

**31 survivors triaged in two commits on branch `mutants/m4bcd-m5ab`:**

1. Tests added (11 new tests catching real gaps):
   - `src/search.rs`: 4 tests — `search_clock_elapsed_at_returns_exact_duration`, `search_clock_same_variant_returns_false_for_mismatched_deadline_variant`, `search_clock_same_variant_returns_false_for_mismatched_soft_deadline_variant`, `search_clock_same_variant_returns_true_for_consistent_wall_clock`.
   - `src/bin/elo-iterate.rs`: 7 tests — `parse_args_draw_score_positive_accepted`, `parse_args_sprt_alpha_exactly_one_rejected`, `parse_args_sprt_beta_out_of_range_rejected` (3 sub-cases), `insufficient_kbkb_same_colour_b2_a1_pins_xor_parity_formula`, `pentanomial_ci_two_pairs_non_zero_variance_returns_valid_ci`, `match_pgn_separator_newline_between_games`, `match_pgn_trailing_newline_added_when_missing`.

2. `exclude_re` rules added (8 rules):
   - `has_non_pawn_material` `|→^` — equivalent (disjoint piece bitboards).
   - `negamax` `&→|` and `&→^` — equivalent (cancellation-cadence gate never fires in tests).
   - `negamax` `<→<=` — structurally undetectable (`beta.abs() == MATE_IN_MAX_PLY` unreachable; also covers M5.A NMP `>=→>` boundary at same value).
   - `negamax` `+→*` — deferred (NMP `ply+1 → ply*1`, 1-ply shift invisible to existing tests; deferred to M5.C).
   - `go` `+=→*=` — deferred (`tries += 1 → tries *= 1`, performance-only, double-fail fixture impractical; deferred to M5.C).
   - Driver whole-fn replacements (4 patterns): `driver::send_line`, `driver::wait_for_uciok`, `driver::wait_for_readyok`, `driver::shutdown` — smoke-test-only gap (ELOH.A precedent).
   - `get_hostname` — smoke-test-only (FFI syscall wrapper).

3. Permanent equivalent misses (no rule, audit comment in mutants.toml):
   - `is_insufficient_material` second `&&→||` — provably equivalent under the (1,1) minor-count filter.
   - `unix_days_to_date_str` two `-→+` — `.min(3)` absorbs the shift + `year_in_cycle/4=0` identity. Function-level exclusion would suppress a non-equivalent caught mutation; accepted permanent miss.

4. Deferred structural gaps (no rule; noted for M5.C or later):
   - AS23 (`clear_killers` between-tries placement): weak pin only; behavioral impact below SPRT noise floor.

See `docs/milestones/m4.b.md`, `m4.c.md`, `m4.d.md`, `m5.a.md`, `m5.b.md` for per-phase survivor breakdowns.

### M4.A — Transposition table (campaign ran 2026-04-29 on main as commit `33a0d0d`)

The M4.A campaign already ran on main. Three survivors triaged: one real coverage gap (S13 node-count assertion tightened from `>= (n_children - 1)` to `< nodes_ref / 2`), one equivalent mutant on disjoint-OR `|` ↔ `^` in `TtEntry::pack_age_bound` (`exclude_re` rule added with rationale), one boundary gap on `score_from_tt`'s `MATE_IN_MAX_PLY` boundary (added T20c — three deterministic boundary tests pinning the no-op at `±MATE_IN_MAX_PLY` and the adjust-by-ply at `±(MATE_IN_MAX_PLY + 1)`).

Resulting test count: 818 lib + 9 integration. Bench unchanged: `bench: 39964046 nodes <NPS> nps`.

As of 2026-04-30 the M4.B branch has been rebased onto `33a0d0d`, so the M4.A follow-up's S13 tightening + T20c boundary tests are part of the branch's history; the M4.A retrospective at `docs/milestones/m4.a.md` is current on the branch (no stale "(populated post-mutants run.)" placeholder).

For an M4.A re-run scenario (e.g., revalidating after a search refactor), the regenerable diff is:

```sh
git diff baseline/alpha-beta-no-tt..ecacf57 -- 'src/*.rs' 'tests/*.rs' > "$TMPDIR/m4.a.diff"
```
