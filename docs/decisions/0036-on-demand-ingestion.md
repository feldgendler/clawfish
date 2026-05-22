# ADR-0036 — M6.H robust on-demand Lichess/CCRL ingestion (corpus::fetch)

**Status:** Accepted (M6.H ships `corpus::fetch` + the `corpus fetch` CLI behind
the non-default `corpus-fetch` Cargo feature; no `evaluate`/`Position`/search
touch — bench byte-identical to `M6.F`/`M6.G` (`1213649` / d4 `90591`);
**functional gate, not SPRT** — no Elo claim, the ADR-0035/M5.E
data-infra-gate precedent). Extends ADR-0035 (corpus infra §11 forward-pointer).

## Context

M6.G built the corpus data-infra (PGN ingestion, filtering, the CRC-framed
per-game append-block shard, dedup, split, the data-quality gate) but external
sources (`Source::Ccrl`, `Source::LichessOpen`) were operator-staged on disk and
ingested via `corpus ingest-pgn`. M6.I's bi-level tuner needs a *synchronous
"give me N more positions from source X"* primitive so its learning-curve
experiment can pull data on demand instead of requiring pre-staged multi-GB
downloads. M6.H is that data-acquisition layer.

Binding inputs: `docs/roadmap.md` §M6 "M6.H scope detail" + the gates-1–7
sub-protocol; `docs/research/m6-network-fetch.md` (ureq 3.x / zstd / zip /
Range-resume / statvfs); `docs/plans/m6.h.md`; ADR-0035 §6 (the atomic block
log reused verbatim), §11 (the M6.H forward-pointer).

## Decision

### 1. A `corpus fetch` subcommand + `corpus::fetch` module, feature-gated

`corpus::fetch::stream_to_ingest(source, url, target_positions, out_dir, filter,
stop, cfg)` streams a compressed PGN dump over HTTPS → decompress → the
*existing* M6.G ingest path (`pgn::stream_pgn` → `filter::game_admitted` →
`store::append_block` into `pgn-shard.bin`), reused **verbatim** (R1 atomic
block log + dedup for free). The `corpus fetch` CLI wraps it.

The network stack (`ureq`, `zstd`, `zip`) is gated behind the **non-default
`corpus-fetch` Cargo feature** so the engine binary and its
`cargo build`/`audit`/`deny` are unaffected — the project's deliberately-lean
dependency posture (only `libc` otherwise) extended to "no engine *dependency*
touch." Build/test the fetcher with `--features corpus-fetch`; `corpus fetch`
without the feature prints a rebuild hint. (Rejected: unconditional deps —
simpler build matrix, but links rustls/ring/zstd/zip into the engine for zero
benefit.)

### 2. Pure-Rust deps; OS trust store via `platform-verifier`; no 7z

`ureq` 3.3 (sync HTTP, streaming body, granular timeouts), `zstd` 0.13
(streaming Zstandard for Lichess `.pgn.zst`; vendors libzstd — build-time C
compiler only, no runtime system dep), `zip` 8.6 (CCRL `.zip`). **TLS uses
`ureq`'s `platform-verifier` feature** — the OS trust store (macOS Security
framework) for cert-chain + hostname verification. This realizes the roadmap's
"validate against the OS trust store / `rustls-native-certs`" *intent* with the
better mechanism: `platform-verifier` subsumes the static-snapshot
`rustls-native-certs` and keeps clawfish working on networks with a
legitimately-installed corporate root CA. **7z is out of scope** (deliberate —
`.zip`/`.pgn.zst` suffice; not worth a dep + the streaming-7z complexity).

**License gate:** `cargo deny check` + `cargo audit` pass clean with **no
`deny.toml` edit** — the precaution to add `Unicode-3.0`/`Zlib` + a `ring`
clarify proved unnecessary (modern ring/zstd-sys/miniz_oxide declare clean SPDX
licenses all matching the existing allowlist). +~76 transitive crates, all
permissive, no advisories.

### 3. Two-level resume + games-parsed stall watchdog (the robustness core)

`ResumableHttpReader<R, F>` is a `Read` adapter *below* the decompressor,
generic over the inner reader `R` and a reconnect closure `F` (so the whole
state machine is unit-testable with no socket).

- **Within an attempt:** any `inner.read` error or stall reopens `GET … Range:
  bytes={consumed}-` keeping the **same decoder alive** (zero re-decompression;
  byte-level reconnect is transparent to zstd). A `206` continues; a `200` on a
  resume is `RangeIgnored`; a `416` is clean EOS.
- **Across attempts:** the outer infinite-backoff loop (1s→…→5 min cap, honors
  SIGINT, ETAs in absolute local time) starts a **fresh byte-0 attempt** with a
  fresh decoder.
- **Stall watchdog keyed on games parsed, not bytes:** ureq's `timeout_recv_body
  = stall_timeout` is the heartbeat (a hung socket hands control back within the
  window); the reader aborts when `now − last_game_at > stall_timeout`, where
  `last_game_at` is bumped only by a yielded game (or a raw byte read in the
  CCRL temp-download phase). Consecutive no-progress resumes escalate to a
  byte-0 restart (defeats a byte-alive-but-semantically-dead stream).
- **Early termination is decoder-agnostic:** a shared `positions_ingested`
  counter (not the decoder's terminal state) drives classification; a
  disambiguating peek distinguishes drained-at-target (`Eos`) from
  more-available (`EarlyTarget`).

### 4. Byte-0-restart idempotence

`base_game_id` is pinned once per call; `stream_pgn` re-derives the same id per
physical game on a re-parse, and the ingest closure skips ids `<=
max_appended_game_id` (and never bumps the position counter for skipped games)
— so an **in-process byte-0 restart is a true no-op append**. Cross-process
re-ingest (a new process, recomputed base) appends FEN-duplicates removed by the
later `corpus build` FEN-dedup (the documented weaker guarantee, ADR-0035 §8).

### 5. Source resolution: Lichess auto, CCRL `--url`-required

Lichess monthly dumps are auto-constructed (`lichess_url(year, month)`; default
month pinned). **CCRL has no auto-default**: it distributes only as `.7z`
(unsupported) and is bot-hostile, so `--source ccrl` requires an operator
`--url` to a `.zip`/`.pgn.zst` mirror (the `.zip` path downloads to a temp file
— Range-resumable — then parses the first `.pgn` entry locally; research §4).
See `docs/data-catalog.md`.

### 6. Gate — functional, not SPRT

The landing gate is the test suite (reader state-machine unit tests + pure-gate
unit tests + the localhost-HTTP-server integration suite covering
early-termination byte bounds, drop+resume for both `.zst` and `.zip`,
EOS-at-target, 404, HTML-rejection, byte-0-restart idempotence, stop). No SPRT,
no Elo claim — bench unchanged by construction (no engine touch).

## Consequences

- M6.I's bi-level driver can call `stream_to_ingest(...)` as a synchronous
  "give me N more positions" primitive, parallel to `corpus selfplay`.
- CCRL acquisition needs an operator-supplied `.zip`/`.pgn.zst` mirror until/
  unless a public one is pinned or 7z support is added (out of scope here).
- `<out>/fetch-state.json` (gitignored) tracks per-URL contribution for the
  driver's revisit/advance decision.
- The engine binary, its dependency audit, and its bench are untouched (feature
  off by default).

## Alternatives considered

- **Unconditional deps** (no feature gate). Rejected — bloats the engine's
  supply-chain surface for zero benefit (§1).
- **`rustls-native-certs`** (the roadmap's literal wording). Superseded by
  `platform-verifier`, which subsumes it and verifies natively against the OS
  (§2).
- **Streaming-zip with `read_zipfile_from_stream`.** Rejected for CCRL —
  mid-entry Range-resume is a footgun (research §4); the small-archive temp-file
  download is robust and resume-trivial.
- **7z support.** Out of scope (§2).
- **A second hand-rolled JSON parser for `fetch-state.json`.** Rejected — reuse
  `corpus::manifest`'s tested minimal parser (exposed `pub(crate)`,
  feature-gated entry point).

## References

- ADR-0035 — corpus infra (§6 atomic block log, §8 reproducibility tiers, §11
  M6.H forward-pointer).
- `docs/plans/m6.h.md`, `docs/research/m6-network-fetch.md`,
  `docs/data-catalog.md`, `docs/roadmap.md` §M6 (M6.H scope detail + gates 1–7).
