# Plan: dedup-against-committed (II) + bit-identical, non-wasting lane extension

## Status

Proposed (revised). Corpus-construction change + new extend capability. Touches
the shared committer (self-play AND fetch paths) and fetch glue. **Bench-neutral**
(no eval/search/`static_eval`/qsearch change ⇒ `bench`/`bench 4` unchanged). No
SPRT. Requires a **corpus rebuild** (changes what a fresh build produces).
User-approved: rebuild all four lanes 0→4M under (II) at high priority, after
extend is proven bit-identical and the change is committed + CI-green.

## Two coupled deliverables

### Deliverable 1 — dedup-against-committed (II)  [the rebuild depends on this]

Today `LaneCommitter::commit_game` inserts EVERY dedup-survivor's FEN into
`fen_set` (step 1), then the per-game reservoir cap keeps only `PER_GAME_CAP=10`.
So positions the cap *discards* are still "seen" and block later games that reach
the same position — even though they were never written. Side effects:
- A unique position discarded by one game's cap is lost forever (no later game
  can contribute it). Under-counts recurring positions.
- `fen_set` ⊋ the on-disk set, so a resume scan (which reads only committed
  records) cannot reconstruct it → extend cannot be made exact without re-running
  the prefix quiet-search.

**Change:** dedup against only the **committed** set. In `commit_game`:
1. Drop `r` if `r.fen ∈ fen_set` (committed-so-far) OR it repeats within this
   game (a local first-seen set). Do **not** insert into `fen_set` here.
2. Reservoir cap among the survivors (unchanged).
3. Exact-target truncate (unchanged).
4. Insert ONLY the committed (post-truncate) FENs into `fen_set`; append block.

Properties: still no duplicates in the corpus; cap still samples among
not-yet-committed uniques (the "don't waste cap slots" benefit is intact); a
discarded position gets a fair chance in a later game (richer); and crucially
`fen_set == on-disk set`, so a resume scan reconstructs it **exactly**. Applies
uniformly to self-play (`consumer.rs` → `from_parts` → `commit_game`) and fetch
(`ingest_game` → `commit_game`). Changes corpus output ⇒ rebuild.

### Deliverable 2 — bit-identical, non-wasting extend

Goal: extending a lane built to N up to T yields a `lane.bin` **byte-identical**
to a fresh build to T, while re-processing only ONE game.

- **C1 — stable game ids.** Pin `base_game_id = 1` (constant) instead of
  `max_existing_game_id+1`. `stream_pgn` assigns `base + parse_index`, so a
  game's id becomes a run-independent parse index. Backward-compatible (fresh
  lanes already used base 1). `next_game_id` stays informational (sole consumer:
  a `bin/corpus.rs` log line). Within-call byte-0-restart idempotence preserved
  (base was already pinned per call). NOTE: `cmd_ingest_pgn` keeps its own
  `max_existing+1` base; fetch-extend is sound only on fetch/self-play-built
  lanes — `ingest-pgn` extension is out of scope.

- **Resume is exact (given Deliverable 1).** The scan rebuilds `fen_set` =
  committed FENs = exactly the original run's `fen_set`, plus `committed` count
  and `committed_ids`.

- **C2 — boundary game: drop + re-derive (the user's idea).** A lane finalized
  at an exact target has its highest-id committed game *truncated* (partial). On
  extend:
  1. Truncate `lane.bin` to the **recorded boundary offset** (the lane length
     just before the boundary block). This is a *set-length to a persisted
     offset*, hence **idempotent** and crash-safe (re-running truncates to the
     same offset; `truncate_to_valid` machinery already exists).
  2. Rebuild committer state from the truncated lane (`fen_set`, `committed`).
  3. Re-process games with `id < boundary`? **No** — their blocks stay on disk
     and `fen_set` already has them (skip; this is the non-wasting win — no
     quiet-search over the prefix).
  4. Re-derive the boundary game `id == boundary` FRESH → one full block (room
     ample at T). Because `fen_set` now == "committed games `< boundary`" ==
     exactly fresh's state at that game, its dedup→cap→commit is identical to a
     fresh build's boundary block.
  5. Process new games `id > boundary` → identical to fresh.
  ⇒ `lane.bin` byte-identical to fresh-to-T; only ONE game re-processed.

  Persist `boundary_offset` (Some when truncated-at-target; None when the build
  drained to EOF before target — then no partial boundary, nothing to drop) in
  the manifest at `finalize`.

- **C3 — target is a TOTAL.** Reorder `stream_to_ingest`: resume first, read
  `existing = committer.committed()`, then `CallState::new(target.saturating_sub
  (existing), …)` so the per-call "new to append" counter and the committer's
  TOTAL truncation agree; early-stop fires exactly when the lane reaches T.
  Guard: `existing ≥ T` ⇒ immediate no-op (`EarlyTarget`, 0 appended). Fresh
  lane: `existing=0` ⇒ unchanged.

- **Crash-safety of new appends.** Standard append-block log: a crash mid-extend
  leaves valid CRC blocks; a re-run truncates to `boundary_offset` (idempotent)
  and deterministically re-derives the boundary + re-appends new games. Bounded
  re-work (the extend's own new games), never the whole prefix, never data loss.

## Determinism premise (document; guard where cheap)

Bit-identity assumes the extend run uses the SAME eval build, the SAME
`cap_seed`, and the SAME source bytes as the original. Add a guard: `cmd_fetch`
(and self-play) refuse an extend when the manifest's recorded `cap_seed` ≠ the
requested one (else silent divergence). Build-consistency (same eval) is already
a corpus contract (ADR-0035/0036).

## Test plan (TDD) — bit-identity is the load-bearing gate

Unit (committer/resume, synthetic records, temp lane):
1. `(II)` cap-discarded position is committed by a later game (was dropped under
   the old semantics) — pins the semantic change.
2. `(II)` resume reconstructs `fen_set` exactly == on-disk (no discarded ghosts).
3. within-game dedup still drops intra-game repeats; cap/target unchanged.
4. extend target-as-total: counts + clean `EarlyTarget`, `existing≥T` no-op.
5. boundary truncate-to-offset is idempotent (run twice ⇒ same lane).
6. stable game_id across runs (C1).

Integration — **prove bit-identity in BOTH production paths** before the rebuild:
7. SELF-PLAY: build fresh to 2X (seed S); separately build to X then EXTEND to
   2X; assert `lane.bin` SHA-256 identical. Assert non-wasteful (only the
   boundary game re-searched — via a skip counter / game-processed count).
8. FETCH: same, using a small Lichess stream (streams fast to a few-thousand
   target; CCRL `.7z` downloads whole, so use Lichess for the small test):
   fresh-to-2X vs build-X-then-extend-to-2X ⇒ identical SHA; non-wasteful.
9. Existing committer/fetch tests updated to (II) and green.

## Risks / out of scope

- Risk: extend with mismatched eval/`cap_seed`/source ⇒ divergence. Mitigated by
  the `cap_seed` guard + documented build-consistency.
- Out of scope: `ingest-pgn` extension; seekable compression (the prefix
  *decompression* for a streaming source is the inherent, cheap residual — only
  the quiet-search is skipped, not the byte read).

## Files
- `src/corpus/pipeline.rs` — `commit_game` (II) dedup; `resume` already returns
  `committed_ids`; boundary-aware extend support; tests.
- `src/corpus/fetch/mod.rs` — C1 base=1; C3 resume-before-CallState + total
  guard; C2 prefix-skip (`id < boundary`) + boundary drop/re-derive; doc.
- `src/corpus/fetch/reader.rs` — `CallState` target semantics.
- `src/corpus/consumer.rs` — confirm self-play routes (II) via `commit_game`
  (it does); self-play extend = same truncate-to-offset + re-derive.
- `src/corpus/manifest.rs` — persist `boundary_offset`; `cap_seed` extend guard.
- `src/bin/corpus.rs` — wire extend (truncate-to-offset) for fetch + self-play.
- ADR-0035 (dedup semantics change → v2), ADR-0036 (extend now supported),
  `docs/architecture.md` corpus section.

## Round-2 review resolutions

Determinism precondition **empirically confirmed** (2026-05-25): two independent
fresh builds are byte-identical for BOTH self-play (workers=6) and fetch — so
bit-identical extend is feasible. Resolutions to the three must-fixes:

**M1 — boundary has three terminal states (not "always partial").** Define
`truncated_boundary_offset: Option<u64>` precisely:
- **partial-mid-game** (exact-target truncation fired: `capped.len() > room`):
  `Some(off)` where `off` = lane length *before* the boundary block.
- **whole-on-boundary** (committed reached target on a whole game): `None`.
- **drained-to-EOF** (target not reached): `None`.
On extend: if `Some(off)` → `truncate_to_valid(lane, off)` (drops the partial
boundary block); then in ALL cases the rule is uniform — rebuild committer state
from the (possibly-truncated) lane and **skip games with `id ≤ max(committed_ids
of the current lane)`**, re-process the rest. In the `Some` case the dropped
boundary game's id now exceeds the truncated lane's max, so it is re-derived
WHOLE (room ample at the higher target) → byte-identical to a fresh build's
now-interior whole block. In the `None` cases nothing is dropped and only
genuinely-new games (`id > max`) are appended. No prefix pipeline runs either
way (non-wasting) because the skip is by id, before the per-position pipeline.

**M2 — `truncated_boundary_offset` is produced by `commit_game`, persisted by
the driver, NOT reconstructed at `finalize`.** `append_block` returns the
pre-append offset; `commit_game` records it as the boundary offset *iff* this
game's exact-target truncation fired. The committer surfaces it
(`LaneCommitter::truncated_boundary_offset()`); it flows out via `FetchOutcome`
(and the self-play campaign summary) to the driver (`cmd_fetch`/`cmd_selfplay`),
which writes it into the manifest alongside `source_url`/`cap_seed` after the
build, fsynced. `finalize` PRESERVES the field (it cannot derive it — a partial
3-record block is byte-indistinguishable from a genuine 3-record game on a
scan). A partial boundary block is always the last block (games commit in id
order), so `off` is unambiguous.

**M3 — interrupted-extend crash matrix (analyzed; locked by a crash test).**
Invariant: the manifest's `truncated_boundary_offset` is NOT updated until the
extend COMPLETES and re-finalizes (with the new target's offset). During an
extend the manifest still names the OLD offset. Matrix (re-run reads the OLD
offset from the manifest, truncates to it = idempotent set-length, re-derives):
- crash after truncate / before re-append: lane already at `off`; truncate is a
  no-op; re-derive proceeds. ✓
- crash mid new-append: lane = `off` + partial new blocks; re-run truncates to
  `off` (drops the partial new blocks) and re-derives. Bounded re-work (the
  extend's own new games, never the prefix), no data loss, deterministic. ✓
- crash after complete / before re-finalize: manifest still old offset; re-run
  truncates to `off` and re-does the extend (bounded, deterministic). ✓
- after re-finalize: manifest names the new offset; a further extend uses it. ✓
`truncate_to_valid(lane, off)` truncates to a known block boundary, so there is
never a torn tail to mis-handle. (A future optimization — an "extending" marker
so mid-extend resume continues from the checkpoint instead of re-doing — is
noted but NOT required for correctness; the re-do path is correct and bounded.)
**Test (crash-injection):** start an extend, kill mid-new-append, re-run, assert
the final `lane.bin` SHA equals the uninterrupted fresh-to-T build (mirrors
`tests/corpus_crash_safety.rs`).

**Should-fix resolutions.**
- *Determinism guard widened*: refuse an extend when the manifest's `cap_seed`
  OR `engine_commit` ≠ the current build's (the eval/quiet-filter identity is
  the likeliest real-world divergence). Documented premise also lists the PGN
  parser (fetch) and the self-play knobs.
- *Id invariant restated*: a fetch game's id is its index among
  successfully-EMITTED games (parse + SAN-replay + result-present), a
  run-independent function of (source bytes, parser, base) — so the determinism
  premise includes "PGN parser identical." Self-play ids come from the
  deterministic dispatcher/consumer given (`self_play_seed`, `workers`,
  `depth_ladder`, `opening_random_plies`, `max_plies`) — guard these on a
  self-play extend (refuse on mismatch). Self-play extend reuses the existing
  `cmd_truncate` machinery (truncate + delete `checkpoint.bin` so
  `next_consume_id` re-derives from the scan).
- *Tests added*: partial-boundary case (target provably lands mid-game →
  pre-extend last block partial, post-extend whole, == fresh), exact-on-boundary
  case, drained case, shrink request (`existing > T` ⇒ defined no-op/err), plus
  the M3 crash test, plus the two end-to-end bit-identity SHA tests
  (self-play + fetch) reusing `tests/corpus_fetch.rs`'s in-process server.
- *Notes*: ADR-0035-v2 records that (II) changes corpus IDENTITY (which
  positions appear), not just size — the M6.I tuning-baseline corpus is a
  different draw, not a superset. `ingest-pgn` on a fetch/self-play lane is
  refused (manifest `source` regime check) so the id-base invariant is enforced,
  not just documented.
