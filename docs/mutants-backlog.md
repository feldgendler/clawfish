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

### M4.B + M4.C — Killer moves + History heuristic (joint campaign)

**Why combined:** M4.B and M4.C developed on parallel branches off M4.A; the user opted to defer their mutation campaigns and run them together as one overnight batch covering both diff ranges. M4.C's rebase onto main folds its history table into M4.B's already-merged `negamax_move_order_score` and extends `Search::reset` / killer scoring boundaries (`MAX_HISTORY = 99` to fit strictly below `KILLER1_SCORE = 100`). One single `cargo mutants --in-diff` invocation covers both phases via the commit-range diff below.

**Diff command** (resilient to working-tree state — uses commit hashes):

```sh
git diff 33a0d0d..<M4.C-TIP> -- 'src/*.rs' 'tests/*.rs' > "$TMPDIR/m4.bc.diff"
```

- `33a0d0d` — M4.A follow-up (cargo-mutants survivors fix); M4.B's branching point on main and the joint range's start.
- `<M4.C-TIP>` — the M4.C tip on `m4.c-history-heuristic` after rebase onto main; resolved to a concrete SHA (printed by `git rev-parse m4.c-history-heuristic` at campaign-start time, or by `git rev-list --max-count=1 baseline/alpha-beta-tt-killer-history` once that tag is created at the M4.C merge commit).
- Pathspec `'src/*.rs' 'tests/*.rs'` — single-star glob (git pathspec doesn't expand `**`); restricts to the source + integration-test surface. M4.B + M4.C modify `src/search.rs`, `src/history.rs` (new), `src/mov.rs`, `src/engine.rs`, `src/lib.rs`, `tests/uci_integration.rs` plus doc files; the doc files are out of scope for cargo-mutants.

After landing the diff, run:

```sh
cargo mutants --in-diff "$TMPDIR/m4.bc.diff"
```

Once `baseline/alpha-beta-tt-killer-history` is created at the M4.C merge commit, a tag-based regenerate is also available: `git diff baseline/alpha-beta-tt..baseline/alpha-beta-tt-killer-history -- 'src/*.rs' 'tests/*.rs' > "$TMPDIR/m4.bc.diff"`. Note that `baseline/alpha-beta-tt` points at `ecacf57` (M4.A landing, before the M4.A mutants follow-up), so a tag-baseline regenerate would also include `33a0d0d`'s S13 tightening + T20c boundary tests in the diff — wider than the M4.B+M4.C-only surface above, but harmless.

**Unit context** (read these before triaging):

- M4.B plan: [`docs/plans/m4.b.md`](plans/m4.b.md) — see §3 (helper signatures) and §8 (anticipated catchable surface table).
- M4.B research: [`docs/research/m4-killer-moves.md`](research/m4-killer-moves.md).
- M4.B retrospective: [`docs/milestones/m4.b.md`](milestones/m4.b.md) — "Mutation-survivor analysis" section is the target for the post-triage update.
- M4.C plan: [`docs/plans/m4.c.md`](plans/m4.c.md) — see §3.5 (cutoff dispatch) and §11 (mutation-testing prep).
- M4.C research: [`docs/research/m4-history-heuristic.md`](research/m4-history-heuristic.md).
- M4.C retrospective: [`docs/milestones/m4.c.md`](milestones/m4.c.md) — "Mutation-survivor analysis" section is the target for the post-triage update.
- M4.C ADR: [`docs/decisions/0019-history-heuristic.md`](decisions/0019-history-heuristic.md).
- Code surface:
  - `src/search.rs` lines ~285 (killer field), ~870–945 (M4.B helpers), ~552–610 (negamax steps 10 + 11), ~320 / ~338 / ~426 (lifecycle reset call sites), the cutoff dispatch + `quiets_searched` accumulator + push site for M4.C, plus ~22 M4.B tests at S14–S29 and ~16 M4.C tests at HS1–HS12 + HS3b/HS3c/HS4b/HS8b (search for the `S14 —` and `HS1 —` comment markers).
  - `src/history.rs` — the entire M4.C module + 12 H-tests.
  - `src/engine.rs` — the M4.C `ucinewgame_clears_history_table` E_h test.

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

**Triage protocol** (per `docs/workflow.md`):

1. **Caught** — re-run `cargo test --lib <test_name>` to confirm; nothing to do.
2. **Equivalent mutant** — prove indistinguishability; add `exclude_re` rule to `.cargo/mutants.toml` with a comment explaining why no input distinguishes original from mutant. Cite ADR-0019 / plan §3 where the design intent supports the equivalence.
3. **Real-bug, structurally undetectable at this milestone scope** — add `exclude_re` rule with a "deferred to M\<X\>" comment and surface in the M4.C retrospective's "Mutation-survivor analysis" section.
4. **Real-bug, catchable by adding a test** — add the test (preserve naming convention from existing S14–S29 + HS1–HS12 series), re-run with `cargo mutants --in-diff $TMPDIR/m4.bc.diff --iterate`, return to step 1.

Avoid `exclude_re` rules anchored to line numbers (per `.cargo/mutants.toml` guidance — line-number-anchored regexes are fragile; structural patterns or specific function names are stable). M3.D's `negate_window` extraction is the precedent for refactoring instead of excluding.

### Where the follow-up commit lands

On `main` (M4.C will have merged by the time the campaign runs). The commit message follows the M4.A follow-up pattern (commit `33a0d0d`):

> M4.B+M4.C follow-up: address N cargo-mutants survivors
>
> `cargo mutants --in-diff ...` produced K mutations: J caught + L
> timeout (caught-via-hang) + M unviable + N missed. This commit addresses
> all N: ...

Update both `docs/milestones/m4.b.md`'s and `docs/milestones/m4.c.md`'s "Mutation-survivor analysis" sections atomically with the commit. Remove this entry from the backlog.

### Edge cases

- **If `cargo mutants` reports `unviable` for the M4.C `Move::default()` sentinel `debug_assert!`**: that's expected — the assertion is in a path movegen never produces, so no chess fixture can drive it. Classify as caught-via-unreachable; no action needed.
- **If the campaign hangs**: per `.cargo/mutants.toml` guidance, the timeout multiplier should be enough; if a specific mutation hangs the test suite, it's almost always the cancellation-poll cadence interacting with the mutated code. Cancel via Ctrl-C; the mutation is caught-via-timeout.
- **If a survivor maps outside the M4.B/M4.C surface** (i.e., a mutation on a line outside `src/search.rs` killer / history logic, `src/history.rs`, or the M4.C-specific test surface): something's off with the diff scope. Re-check that `33a0d0d..<M4.C-TIP>` is the correct hash range and that no drift commits have landed.

## Done

### M4.A — Transposition table (campaign ran 2026-04-29 on main as commit `33a0d0d`)

The M4.A campaign already ran on main. Three survivors triaged: one real coverage gap (S13 node-count assertion tightened from `>= (n_children - 1)` to `< nodes_ref / 2`), one equivalent mutant on disjoint-OR `|` ↔ `^` in `TtEntry::pack_age_bound` (`exclude_re` rule added with rationale), one boundary gap on `score_from_tt`'s `MATE_IN_MAX_PLY` boundary (added T20c — three deterministic boundary tests pinning the no-op at `±MATE_IN_MAX_PLY` and the adjust-by-ply at `±(MATE_IN_MAX_PLY + 1)`).

Resulting test count: 818 lib + 9 integration. Bench unchanged: `bench: 39964046 nodes <NPS> nps`.

As of 2026-04-30 the M4.B branch has been rebased onto `33a0d0d`, so the M4.A follow-up's S13 tightening + T20c boundary tests are part of the branch's history; the M4.A retrospective at `docs/milestones/m4.a.md` is current on the branch (no stale "(populated post-mutants run.)" placeholder).

For an M4.A re-run scenario (e.g., revalidating after a search refactor), the regenerable diff is:

```sh
git diff baseline/alpha-beta-no-tt..ecacf57 -- 'src/*.rs' 'tests/*.rs' > "$TMPDIR/m4.a.diff"
```
