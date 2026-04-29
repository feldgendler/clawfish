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

### M4.B — Killer moves (committed 2026-04-29; rebased onto main 2026-04-30 on `m4.b-killer-moves` branch)

**Diff command** (resilient to working-tree state — uses commit hashes):

```sh
git diff 33a0d0d..1771e57 -- 'src/*.rs' 'tests/*.rs' > "$TMPDIR/m4.b.diff"
```

- `33a0d0d` — M4.A follow-up (cargo-mutants survivors fix); M4.B's branching point after the rebase onto main.
- `1771e57` — M4.B landing commit on branch `m4.b-killer-moves` post-rebase; the head of the unit's diff.
- Pathspec `'src/*.rs' 'tests/*.rs'` — single-star glob (git pathspec doesn't expand `**`); restricts to the source + integration-test surface. The M4.B unit modified only `src/search.rs` plus four doc files; the doc files are out of scope for cargo-mutants.

After landing the diff, run:

```sh
cargo mutants --in-diff "$TMPDIR/m4.b.diff"
```

If the unit has merged to main by the time the campaign starts, the commit hashes above are still valid — they are immutable. Once `baseline/alpha-beta-tt-killer` is created at the M4.B merge commit, a tag-based regenerate is also available: `git diff baseline/alpha-beta-tt..baseline/alpha-beta-tt-killer -- 'src/*.rs' 'tests/*.rs' > "$TMPDIR/m4.b.diff"`. Note that `baseline/alpha-beta-tt` points at `ecacf57` (M4.A landing, before the M4.A mutants follow-up), so a tag-baseline regenerate would also include `33a0d0d`'s S13 tightening + T20c boundary tests in the diff — wider than the M4.B-only surface above, but harmless.

**Unit context** (read these before triaging):

- Plan: [`docs/plans/m4.b.md`](plans/m4.b.md) — see §3 (helper signatures) and §8 (anticipated catchable surface table).
- Research: [`docs/research/m4-killer-moves.md`](research/m4-killer-moves.md).
- Retrospective: [`docs/milestones/m4.b.md`](milestones/m4.b.md) — "Mutation-survivor analysis" section is the target for the post-triage update.
- Code surface: `src/search.rs` lines ~285 (field), ~870–945 (helpers), ~552–610 (negamax steps 10 + 11), ~320 / ~338 / ~426 (lifecycle reset call sites), plus ~22 tests in the `mod tests` block at S14–S29 (search for the `S14 —` comment marker).

**Anticipated catchable surface** (from plan §8; verify or refute per actual run):

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

**Known structural test-surface gap** (per plan §8):

The per-iteration `clear_killers` call site is structurally hard to distinguish from per-go via test surface alone — per-iteration runs at the top of iteration 1 (before any negamax call), so dropping the per-go call alone is masked by per-iteration. If the campaign reports a survivor at the per-iteration call site, follow the fix protocol above (extract a named method + direct unit test).

**No `.cargo/mutants.toml` changes anticipated.** Helper extraction (`order_moves`, `clear_killers`, etc.) follows the M3.D `negate_window` + M3.E `aborted_fallback_result` precedent specifically to avoid needing exclusions; if the actual run produces survivors that don't fit into the categories above, prefer "extract a named helper + add a unit test" before "add an `exclude_re` rule".

## Done

### M4.A — Transposition table (campaign ran 2026-04-29 on main as commit `33a0d0d`)

The M4.A campaign already ran on main. Three survivors triaged: one real coverage gap (S13 node-count assertion tightened from `>= (n_children - 1)` to `< nodes_ref / 2`), one equivalent mutant on disjoint-OR `|` ↔ `^` in `TtEntry::pack_age_bound` (`exclude_re` rule added with rationale), one boundary gap on `score_from_tt`'s `MATE_IN_MAX_PLY` boundary (added T20c — three deterministic boundary tests pinning the no-op at `±MATE_IN_MAX_PLY` and the adjust-by-ply at `±(MATE_IN_MAX_PLY + 1)`).

Resulting test count: 818 lib + 9 integration. Bench unchanged: `bench: 39964046 nodes <NPS> nps`.

As of 2026-04-30 the M4.B branch has been rebased onto `33a0d0d`, so the M4.A follow-up's S13 tightening + T20c boundary tests are part of the branch's history; the M4.A retrospective at `docs/milestones/m4.a.md` is current on the branch (no stale "(populated post-mutants run.)" placeholder).

For an M4.A re-run scenario (e.g., revalidating after a search refactor), the regenerable diff is:

```sh
git diff baseline/alpha-beta-no-tt..ecacf57 -- 'src/*.rs' 'tests/*.rs' > "$TMPDIR/m4.a.diff"
```
