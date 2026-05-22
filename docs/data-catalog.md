# Data catalog — external corpus sources (M6.H)

Vetted source URLs + acquisition notes for `corpus fetch` (M6.H on-demand
ingestion). See `docs/plans/m6.h.md`, ADR-0036, and ADR-0035 (corpus infra).

`corpus fetch` streams a compressed PGN dump over HTTPS, decompresses
(`.pgn.zst` streamed via `zstd`; `.zip` via `zip` and `.7z` via `sevenz_rust2`,
both downloaded to a resumable temp file then parsed locally), applies
`GameFilter::default()` (WhiteElo/BlackElo ≥ 2000, Standard time-control,
`Termination != Time forfeit/Abandoned`) AND the full per-position inline
pipeline (skip8 ∧ `!in_check` ∧ `|static_eval| ≤ HIGH_SCORE_CP` ∧ `is_quiet`),
then per-lane FEN dedup → per-game cap → exact target via the shared
`LaneCommitter`, appending each surviving game as one CRC block to
`<out>/lane.bin` (the M6.H2 flat build-ready lane). The codec is chosen by the
URL extension (`.pgn.zst` / `.zip` / `.7z`).

**`--target-positions` counts USABLE positions** (post quiet-filter → dedup →
cap), NOT raw parsed positions; the committer's exact truncation lands `lane.bin`
on the target exactly. **`source_url` must be pinned in the lane manifest** (the
resolved URL `corpus fetch` writes to `manifest.json`) — a *fresh uninterrupted*
fetch from `source_url` re-derives `lane.bin` byte-for-byte (ADR-0035 §8/§12
re-derivable tier; a resumed fetch is content/label-equivalent but its `game_id`
provenance tags may shift).

## Lichess (human games) — `Source::LichessOpen`

**Auto-constructible.** The standard-rated monthly dumps follow a fixed pattern:

```
https://database.lichess.org/standard/lichess_db_standard_rated_YYYY-MM.pgn.zst
```

- Served by nginx with `Accept-Ranges: bytes` → HTTP Range resume works
  (verified by HEAD, 2026-05).
- `corpus fetch --source lichess` builds the URL from `--lichess-month YYYY-MM`
  (or a pinned default month when omitted — see `catalog::DEFAULT_LICHESS_MONTH`).
- Verified present: `2013-01` (17.7 MB), through recent months (recent months
  are tens of GB compressed — stream + early-terminate; do **not** pre-download).
- **Band-filter yield is low for old/low-rated months.** Most early-Lichess
  games are < 2000 Elo and are dropped. For a large ≥ 2000-Elo yield, prefer a
  recent month (`--lichess-month 2024-12`), which has far more titled/high-rated
  games per GB streamed.

## CCRL (engine games) — `Source::Ccrl`

CCRL (computerchess.org.uk) publishes its databases as **`.7z`** (LZMA2), which
`corpus fetch` supports directly (`sevenz_rust2`). The archive files download
fine over plain HTTPS (the *index* page is JS-driven, but the archive URLs are
static). **`--url` is REQUIRED** — CCRL filenames embed a game count
(`CCRL-4040.[N].pgn.7z`), so there's no stable auto-constructible URL. Vetted
sources (computerchess.org.uk, browser-UA HEAD-verified 2026-05):

- Full 40/40 database (~2.35 M games):
  `https://computerchess.org.uk/4040/CCRL-4040.[2349311].pgn.7z`
  (the `[N]` count changes when CCRL re-publishes — `[2343842]` rolled to
  `[2349311]` by 2026-05-23. The files live under `/4040/` (the `/ccrl/4040/`
  path 302-redirects there); get the current filename from the `/4040/`
  directory listing or `https://computerchess.org.uk/ccrl/4040/games.html`).
- Per-month slices: `https://computerchess.org.uk/ccrl/4040/games-by-month/YYYY-MM.bare.[N].pgn.7z`.
- 40/2 archive: `https://computerchess.org.uk/ccrl/402.archive/games-by-engine/<engine>.bare.[N].pgn.7z`.

```
corpus fetch --source ccrl --out <dir> --target-positions 2000000 \
    --url 'https://computerchess.org.uk/4040/CCRL-4040.[2349311].pgn.7z'
```

The fetcher downloads the `.7z` to a resumable temp file, opens it with
`sevenz_rust2`, and streams the first `*.pgn` entry — early-terminating (via the
target guard) once `--target-positions` is reached, so a multi-GB full-DB
archive is not decompressed in full for a small slice. (`.zip`/`.pgn.zst` CCRL
mirrors also work; `corpus ingest-pgn --source ccrl --path <local.pgn>` remains
the no-network on-disk path.)

## Provenance discipline (ADR-0035 §1)

Only **original game-result labels** (`1-0`/`0-1`/`1/2-1/2`) are admitted —
never engine-evaluation labels. The `--source` tag records provenance and must
match the actual source (do not tag non-CCRL games as `ccrl`).

## Per-URL state (`<out>/fetch-state.json`)

Each fetch merges into `<out>/fetch-state.json` (per-URL `positions_contributed`,
`bytes_received`, `last_termination` ∈ {`early_target`, `eos`, `stopped`}).
`early_target` means the URL likely has more to give; `eos` means it drained.
The M6.I bi-level driver consults this to decide whether to revisit a URL or
move to the next. This file is gitignored (operator-local bookkeeping); the
lane (`lane.bin`) is the source of truth.
