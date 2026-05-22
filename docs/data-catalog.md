# Data catalog — external corpus sources (M6.H)

Vetted source URLs + acquisition notes for `corpus fetch` (M6.H on-demand
ingestion). See `docs/plans/m6.h.md`, ADR-0036, and ADR-0035 (corpus infra).

`corpus fetch` streams a compressed PGN dump over HTTPS, decompresses on the
fly (`.pgn.zst` via `zstd`, `.zip` via `zip`), filters with
`GameFilter::default()` (WhiteElo/BlackElo ≥ 2000, Standard time-control,
`Termination != Time forfeit/Abandoned`), and appends to
`<out>/pgn-shard.bin`. **7z is not supported** (deliberate — see ADR-0036).

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

**No auto-default — `--url` is REQUIRED.** CCRL (computerchess.org.uk) publishes
its game databases **only as `.7z`**, which M6.H deliberately does not support,
and its site is bot-hostile (302/403 to non-browser clients, 2026-05). There is
no public CCRL `.zip`/`.pgn.zst` mirror to pin here, so the operator must supply
a `.zip`-of-PGN or `.pgn.zst` URL via `--url`:

```
corpus fetch --source ccrl --out <dir> --url <https URL to a .zip/.pgn.zst of CCRL PGN>
```

To obtain one:

- Download the official `.7z` from CCRL, extract locally, recompress the `.pgn`
  as `.zip` (or `.pgn.zst`), host it on any HTTPS server that supports byte
  ranges, and pass that URL; **or**
- use `corpus ingest-pgn --source ccrl --path <local.pgn>` to ingest an
  already-extracted PGN on disk (no network — the pre-M6.H path); **or**
- supply any other engine-vs-engine PGN collection available as `.zip`/`.pgn.zst`
  and tag it `--source ccrl`.

The `corpus fetch` zip path downloads the (small, tens-of-MB) archive to a temp
file with Range-resumable progress, then parses the first `*.pgn` entry locally.

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
shard is the source of truth.
