# ADR-0036 — M6.H robust on-demand Lichess/CCRL ingestion (corpus::fetch)

**Status:** Accepted (M6.H ships `corpus::fetch` + the `corpus fetch` CLI behind
the non-default `corpus-fetch` Cargo feature; no `evaluate`/`Position`/search
touch — bench byte-identical to `M6.F`/`M6.G` (`1213649` / d4 `90591`);
**functional gate, not SPRT** — no Elo claim, the ADR-0035/M5.E
data-infra-gate precedent). Extends ADR-0035 (corpus infra §11 forward-pointer).

**Amendment (2026-05-22, same-day): `.7z` IS supported.** §2 originally put 7z
out of scope on the (mistaken) premise that CCRL had `.zip`/`.pgn.zst` variants
and that 7z lacked a good native-Rust decoder. Both were wrong: **CCRL
distributes ONLY as `.7z`** (full DB + per-month + per-engine; static URLs that
download fine over plain HTTPS), and **`sevenz-rust2` is a pure-Rust, Apache-2.0,
maintained LZMA/LZMA2 decompressor** (no system binary). So `corpus fetch` now
routes by URL extension — `.pgn.zst` (streamed) / `.zip` / `.7z` (the latter two
download-to-temp + parse the first `.pgn` entry locally, with a `TargetGuard`
that stops the parse at `target_positions` so a multi-GB full-DB archive is not
decompressed whole). This makes the CCRL "pipeline" actually work on-demand. The
rest of this ADR stands; §2's "7z out of scope" is superseded by this amendment.

**Amendment (2026-05-22, game-level + variant filter spec — IMPLEMENTED
2026-05-22).** The ingest path's game-admission filter (`filter::game_admitted`,
defined in M6.G / ADR-0035 and reused verbatim by §1) is extended with three new
game-level gates and three explicit *non*-gates. Whole-game drops are the right
granularity for all three additions because the game's `Result` is the shared
training label, so a corrupt result poisons every position extracted from that
game.

**Implementation note + a fourth, discovered gate (CCRL TimeControl).** Shipped:
`game_admitted` gained a `ply_count` arg + the Rules-infraction / min-length
(`MIN_GAME_PLIES = 20`) / non-standard-start gates (the last comparing the full
parsed `Position` to `starting_position()`, so a standard-placement-but-
black-to-move `[FEN]` without `[SetUp]` is also rejected); `PgnTags` reads
`SetUp`/`FEN`. **The amendment's non-gate "min_elo is a no-op for CCRL → CCRL
passes" was incomplete**: empirically CCRL's bare PGN carries `WhiteElo`/
`BlackElo` (≥ 2000) but **no `[TimeControl]` tag** (its TC is in
`[Event "CCRL 40/15"]`), so the *TimeControl* gate — not Elo — rejected every
CCRL game (0 ingested). Fix: `GameFilter.require_tc` + `ccrl_filter()`
(require_tc=false) skips the TC gate for CCRL; `cmd_fetch`/`cmd_ingest_pgn`
select the filter per `Source`. Validated end-to-end: the full CCRL 2M ingest
landed 2,000,175 positions / 14,713 games; Lichess (TC gate retained) landed
2,000,064 / 26,628.

*Added gates:*
1. **`Termination "Rules infraction"` → drop**, joining the existing
   `Time forfeit` / `Abandoned`. An admin-decided (cheat-flagged) result does not
   reflect played-out positional truth. Kept as a **blocklist**, not a
   `"Normal"`-only allowlist — CCRL PGNs often carry no `Termination` tag and must
   still pass.
2. **Minimum game length — literature default `≥ 20 plies` (10 full moves)**, the
   `pgn-extract --minmoves 10` convention. Removes aborts / disconnects /
   mouse-slips that escaped the `Abandoned` tag (these cluster under ~5 moves) and
   ensures the game reached a real middlegame so its result is connected to play.
   Measured on the mainline ply count; independent of the position-level
   `OPENING_SKIP_PLIES = 8` extraction cut.
3. **Universal non-standard-start gate** — reject any game carrying
   `[SetUp "1"]` or a non-startpos `[FEN]` tag. This is the source-agnostic
   implementation of "variant filtering": it covers CCRL Chess960/FRC,
   odds/handicap games, and any from-position game across *all* sources, and
   doubles as a **parser-correctness safeguard** — the parser replays from the
   standard initial position, so a from-position game would otherwise mis-replay
   into garbage. Requires teaching the PGN parser to read the `SetUp` + `FEN`
   tags (`PgnTags` does not capture them today).

*Explicit non-gates (decided against, recorded so they are not re-litigated):*
- **No Elo-gap cap.** The both-sides `≥ 2000` floor suffices. A large rating gap
  corrupts labels as *variance*, not directional bias (the stronger side is
  White/Black equally often → equal positions pull toward decisive in both
  directions → absorbed by the per-candidate K refit), whereas a gap cap would
  shrink the corpus and over-represent evenly-matched (drawish) games — a worse,
  self-inflicted skew. Revisit only if the Lichess stratum's held-out MSE looks
  anomalous during M6.I.
- **No CCRL strength floor.** Every CCRL engine plays well above weak-human level,
  so every result is a sensible label. The existing `min_elo = 2000` is already a
  *no-op* for CCRL (engine-scale ratings ~3000+) — effectively a Lichess-only
  gate; left as-is.
- **Resignations + draws by agreement are KEPT.** They do not reach a formally
  terminal position, but under the floor + long-TC + min-length + not-timed-out
  gates the result is a good evaluation proxy — strong humans resign known-lost
  endgames and agree drawn ones cleanly rather than play out KQvK (often a
  *cleaner* label than an engine's 50-move fumble). Draw-by-agreement is in any
  case not detectable from Lichess tags: `Termination "Normal"` covers agreement,
  3-fold, 50-move, checkmate, and resignation alike.

These supersede the M6.H plan §1 non-scope line "any change to `filter` (reused
verbatim)": the filter is now *extended* (not rewritten) as pre-M6.I work.

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

### 2. Pure-Rust deps; OS trust store via `platform-verifier`; 7z (amended in)

`ureq` 3.3 (sync HTTP, streaming body, granular timeouts), `zstd` 0.13
(streaming Zstandard for Lichess `.pgn.zst`; vendors libzstd — build-time C
compiler only, no runtime system dep), `zip` 8.6 (CCRL `.zip`). **TLS uses
`ureq`'s `platform-verifier` feature** — the OS trust store (macOS Security
framework) for cert-chain + hostname verification. This realizes the roadmap's
"validate against the OS trust store / `rustls-native-certs`" *intent* with the
better mechanism: `platform-verifier` subsumes the static-snapshot
`rustls-native-certs` and keeps clawfish working on networks with a
legitimately-installed corporate root CA. **7z** — *originally out of scope;
SUPERSEDED by the 2026-05-22 amendment above: `.7z` IS supported via
`sevenz-rust2` (CCRL is `.7z`-only).*

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
(supported per the amendment), and its filenames embed a game count
(`CCRL-4040.[N].pgn.7z`), so there is no stable auto-constructible URL —
`--source ccrl` requires an operator `--url` to a current CCRL archive
(`.7z`/`.zip`/`.pgn.zst`; archive download + parse the first `.pgn` entry
locally with a `TargetGuard` early-stop). The archive URLs download fine over
plain HTTPS (only the JS *index* page is awkward). See `docs/data-catalog.md`.

### 6. Gate — functional, not SPRT

The landing gate is the test suite (reader state-machine unit tests + pure-gate
unit tests + the localhost-HTTP-server integration suite covering
early-termination byte bounds, drop+resume for both `.zst` and `.zip`,
EOS-at-target, 404, HTML-rejection, byte-0-restart idempotence, stop). No SPRT,
no Elo claim — bench unchanged by construction (no engine touch).

## Consequences

- M6.I's bi-level driver can call `stream_to_ingest(...)` as a synchronous
  "give me N more positions" primitive, parallel to `corpus selfplay`.
- CCRL acquisition uses an operator-supplied `--url` to a current CCRL `.7z`
  (supported per the amendment) — the count-bearing filename precludes a stable
  auto-default.
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
- **7z support.** Originally rejected; **adopted in the 2026-05-22 amendment**
  (CCRL is `.7z`-only; `sevenz-rust2` is a pure-Rust LZMA2 decoder).
- **A second hand-rolled JSON parser for `fetch-state.json`.** Rejected — reuse
  `corpus::manifest`'s tested minimal parser (exposed `pub(crate)`,
  feature-gated entry point).

## References

- ADR-0035 — corpus infra (§6 atomic block log, §8 reproducibility tiers, §11
  M6.H forward-pointer).
- `docs/plans/m6.h.md`, `docs/research/m6-network-fetch.md`,
  `docs/data-catalog.md`, `docs/roadmap.md` §M6 (M6.H scope detail + gates 1–7).
