# Data catalog (vetted CCRL + Lichess sources)

**Status:** stub. Populated at M6.H landing time (see Roadmap §M6 M6.H scope detail).

This file will catalog vetted CCRL snapshot URLs + Lichess monthly URLs with their SHA-256 hashes (where known) and acquisition dates, for use by `corpus fetch --source={ccrl,lichess}`.

## Lichess

Lichess monthly dumps follow a predictable URL pattern, so the M6.H driver can construct URLs on demand:

```
https://database.lichess.org/standard/lichess_db_standard_rated_<YYYY-MM>.pgn.zst
```

The catalog here records which months we've actually used + their SHA-256 + the date of acquisition (manifest-pinned).

## CCRL

CCRL snapshot URLs are hand-curated (mirror selection, ratings-list-specific paths). Stick to the `.pgn.zip` variants — M6.H's fetcher does not support 7z.

The catalog here records snapshot URLs + acquired SHA-256 + acquisition date.

## Update policy

When the M6.H driver fetches from a new URL, the recorded `bench/corpus/fetch-state.json` per-URL state file is the operational source of truth; this catalog is the curated-and-reviewed cross-reference that `corpus::fetch` can consult to validate "is this URL one we've vetted?" — a lightweight check above and beyond the per-fetch robustness gates.
