# ADR-0012 — Tournament harness conventions

**Status:** Accepted, 2026-04-28.

## Context

The engine needs a tournament runner from M2 onward:

- **M2:** smoke validation — does the engine speak UCI to a real runner without illegal moves or stalled connections?
- **M3+:** SPRT — does a search change actually gain Elo?

`docs/research/m2-tournament-harness.md` (pre-M2) settled the runner choice. This ADR ratifies the layout commitments that flow from that research.

## Decision

### 1. Runner: fastchess

- Pre-built `mac-arm64` binary; no build-from-source step.
- Pinned via three constants in `scripts/install-fastchess.sh`:
  - `EXPECTED_RELEASE_TAG` — GitHub release tag (URL form).
  - `EXPECTED_VERSION_LINE` — substring grep against `--version` output.
  - `EXPECTED_SHA256` — tarball SHA256; closes the supply-chain gap between download and execution.
- Bumping the pinned release is a three-line edit.

### 2. Install path: `vendor/fastchess/fastchess`

- Repo-local; gitignored via `/vendor` (line 2 of `.gitignore`).
- Replaces `docs/research/m2-tournament-harness.md`'s `~/.local/bin/` suggestion.
- Rationale: co-locating with the project simplifies reproducibility; `vendor/` is the conventional place for third-party binaries.

### 3. Engine registry: `scripts/match.sh`

- Three subcommands: `self-play`, `vs-stockfish`, `compliance`.
- No `engines.json` — fastchess does not consume one; the shell wrapper is the same affordance with engine config under git.
- Locator order:
  1. `$REPO_ROOT/vendor/fastchess/fastchess`
  2. `command -v fastchess` (PATH fallback)
  3. Hard-fail with pointer to `scripts/install-fastchess.sh`
- After locating, gates on `--version | grep -q "$EXPECTED_VERSION_LINE"` — catches stale installs on PATH.

### 4. Output paths

| Artifact type | Path |
|---|---|
| Raw PGN | `target/matches/smoke/<name>-<timestamp>.pgn` |
| Raw log | `target/matches/smoke/<name>-<timestamp>.log` |
| Milestone summary | `bench/m2.md` (per ADR-0010) |
| SPRT raw | `target/matches/sprt/<feature>/<name>-<timestamp>.{pgn,log}` |
| SPRT summary | `bench/sprt/<dated>.md` |

`target/matches/` is gitignored alongside `target/criterion/`.

### 5. Adjudication

| Knob | Value | Role |
|---|---|---|
| `-maxmoves 300` | hard cap | Primary; mirrors E36's `MAX_PLY=300`. |
| `-resign movecount=3 score=600` | best-effort | No-op for random-vs-random (no `score cp`); may fire vs Stockfish. |

`-draw` is deliberately omitted.

- The random mover emits `score cp 0` in its info line (required for `--compliance`).
- Any score-threshold draw heuristic would fire as soon as its `movenumber` threshold is reached, regardless of board position.
- `-maxmoves 300` is the honest cap; `-draw` would be dead-code-cheating to a shorter PGN.

### 6. Smoke contract for M2

Two runs, each `-rounds 1 -repeat` (two games per run):

| Subcommand | Games | Load-bearing gates |
|---|---|---|
| `compliance` | — | all steps pass — 40/40 on fastchess 1.8.0-alpha (pins §11 info-line requirement) |
| `self-play` | 2 | C3, C4 (no `illegal move`, no `stalled connection`) |
| `vs-stockfish` | 2 | C3, C4 |

Full criterion list (§6 of `docs/plans/m2.e.md`): C1–C7.

### 7. In-tree integration test contribution

- **E37** (`integration_unknown_command_silently_ignored`) — new test in `tests/uci_integration.rs`.
- **E33 amendment** — one assertion line added: `assert!(lines.iter().any(|l| l.starts_with("info depth ")))` (tightened from `"info "` per test-suite reviewer to specifically pin the §11 emission while staying compatible with future M3+ alpha-beta `info depth N` lines).
- Both reuse existing `spawn_engine` / `drain_stdout` / `wait_for_exit` helpers.
- No new test file.

### 8. Engine-side requirement: `RandomMover::go` emits an `info` line

Required format (empirically verified against `--compliance` Step 12):

```
info depth 0 score cp 0 nodes 1 time <ms> pv <move-or-0000>
```

- `score cp 0` is required — `--compliance` Step 12 rejects info lines without a `score` field.
- Emitted post-wait so `time` reports the full wall-clock used.
- M3+ alpha-beta replaces this with iterative-deepening info lines naturally.

## Consequences

- `target/matches/` joins `target/criterion/` as the gitignored "raw output" sibling.
- ADR-0010's `bench/<milestone>.md` shape extends naturally to match summaries.
- `scripts/match.sh` is the canonical runbook entry point; `docs/workflow.md` gains a "Running a match" section.

## Variants considered and rejected

| Variant | Reason rejected |
|---|---|
| cutechess-cli | No macOS asset; build-from-source pulls ~1 GB Qt (research §1). |
| `~/.local/bin/fastchess` install path | Worse reproducibility; not repo-local. |
| `engines.json` | fastchess does not consume one; shell wrapper is equivalent and is under git. |
| Cute Chess GUI | No working macOS binary; PGN replay covers ad-hoc review needs. |
| SPRT now | No second engine version yet; deferred to M3. |
| `-draw` adjudication | Fires trivially when `score cp` is absent; dishonest cap. |

## How to apply

- First match captured at M2.E commit; `bench/m2.md` written as part of the same commit.
- Future SPRT campaigns extend the layout per Decision 4 above.
- To bump the pinned fastchess release: edit `EXPECTED_RELEASE_TAG`, `EXPECTED_VERSION_LINE`, and `EXPECTED_SHA256` in `scripts/install-fastchess.sh` (and the matching `EXPECTED_VERSION_LINE` in `scripts/match.sh`).

## Fresh-clone bootstrap sequence

1. `cargo build --release`
2. `scripts/install-fastchess.sh` — one-time per machine; idempotent on re-runs.
3. `cargo test` — in-tree gates (no fastchess dependency).
4. `scripts/match.sh compliance`
5. `scripts/match.sh self-play`
6. `scripts/match.sh vs-stockfish`

`bench/m2.md` is regenerated by the operator per ADR-0010 when results are committed.
