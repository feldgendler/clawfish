# ADR-0035 — M6.G corpus construction (game-result-labeled quiet-position infra; ADR-0003 audit; Zurichess c9 REJECTED)

**Status:** Accepted (M6.G ships the full corpus pipeline + a vendored
**self-play-dominant** frozen artifact at `bench/corpus/`; `evaluate` /
search behavior unchanged — bench byte-identical to `M6.F`
(`1213649` / d4 `90591`); data-quality gate PASS; no SPRT, no Elo claim —
the M5.E "correctness gate, not SPRT" precedent applied to data).

## Context

M6.A–F landed the cumulative HCE eval surface (tapered eval + bishop pair
+ pawn structure infra + passed pawns + piece mobility + king safety +
Tier-1 features). M6.I is the single joint-Texel pass over that surface
against a baseline tag of `M6.F`. M6.I consumes a **reusable
game-result-labeled quiet-position corpus**, which is also a precondition
of three other downstream consumers: the tuning-backlog "PST co-tuning"
Arm B, future SPSA campaigns, and M10 NNUE data-prep. M6.G is the corpus
phase that decouples the corpus from M6.I's tuning loop — its own
sub-milestone because the corpus has distinct *data* failure modes
(label-provenance leakage, decisive/draw imbalance, opening
over-representation, held-out contamination, non-reproducible
acquisition) and a *data-quality gate*, not an SPRT.

Binding inputs:

- `docs/plans/m6.g.md` (the implementation plan; the §3.1 pinned
  constants are the M6.G↔M6.I interface contract).
- `docs/research/m6-corpus-construction.md` (prior art — Texel recipe,
  quiet-position predicates, source surveys, the **load-bearing Zurichess
  `c9` label-provenance finding** the ADR-0003 audit codifies).
- `docs/roadmap.md` §M6 "M6.G scope detail" + the operational-robustness
  R1–R7 + R-TC + Verification block.
- `docs/decisions/0003-no-third-party-source-code-reading.md` (the
  *spirit* constraint that bans label-provenance bleed from third-party
  engines into clawfish's weights).
- `docs/decisions/0021-virtual-clock-uci-option.md` §4 (the
  cross-version-SPRT-comparability scoping the R4 deviation argues
  against).
- The committed Slice-0 interface (`src/corpus/mod.rs` types + pinned
  constants; the additive `pub fn search::quiescence_eval_white` /
  `QSearcher` seam — confirmed bench-byte-identical to `M6.F`).

## Decision

### 1. Game-result labels ONLY (the ADR-0003 spirit constraint, codified)

The corpus admits **original game-outcome labels only** (1-0 / 0-1 /
1/2-1/2). Engine-evaluation labels — including `c9` annotations that
record an engine's score, and engine-played-from-position continuation
results — are out of scope: they leak the labeling engine's eval-design
bias into clawfish's tuned weights, which is the ADR-0003 spirit
constraint applied to *data*.

The `Source` enum in `src/corpus/mod.rs` enumerates exactly the four
permitted provenances: `SelfPlayOnBook`, `SelfPlayOffBook`, `Ccrl`,
`LichessOpen`. The on-disk frame's source byte (`Source::as_u8`)
physically cannot encode anything else — `Source::from_u8(b)` returns
`None` for `b ≥ 4` and `store::decode_block` rejects such frames
wholesale. (Self-play is split into two source variants by opening
regime — book-seeded vs. startpos + random plies — so the book / off-
book mix is a training-time per-source reweighting axis at M6.I rather
than a corpus-generation knob. See §10.)

### 2. ADR-0003 label-provenance audit verdict: Zurichess `c9` REJECTED

The roadmap explicitly required an audit of the candidate sources. The
research note §3 found, from the Zurichess Bitbucket README quoted in
the announcing TalkChess thread, that `quiet-labeled.epd`'s `c9` labels
are **NOT** the original 75K Zurichess self-play game outcomes — they
are the results of separate games played by **Stockfish 080916** from
each quiet position. They are engine-played continuation results, the
exact label-source bias §1 prohibits. The audit's verdict on Zurichess
`quiet-labeled.epd` is therefore **REJECTED**.

Programmatic defense (in `quality_gate::check_adr0003_audit`): three
layers — (a) the `Source` enum intentionally omits Zurichess; (b)
`Source::from_u8` rejects unrecognized bytes; (c) the audit pins
`accept_list.len() == 4` and enumerates `[SelfPlayOnBook,
SelfPlayOffBook, Ccrl, LichessOpen]` explicitly, so any future addition
forces both the audit logic and the audit's own test to update. The
`adr0003_audit_rejects_zurichess_c9_fixture` test asserts all three.
The audit is one of the three must-PASS data-quality gate checks.

### 3. Source mixture + R5 staging

Permitted sources: **CCRL PGN snapshots** + **band-filtered Lichess
open-database PGNs** (≥ 2000 Elo, ≥ 5-minute TC, `Termination="Normal"`)
+ **clawfish self-play** (deterministic, in-process). The Texel-original
8.5M-position corpus is *out of scope* per ADR-0003 (variant-specific
label-provenance unverified).

R5 (no hard network during crunch): external sources are staged into
git-ignored `target/corpus-raw/` by `bench/corpus/re-run.sh` and
`scripts/corpus.sh` **before** the long phase. Self-play has zero
network. The committed `bench/corpus/` artifact carries **no raw
external DBs** (multi-GB files are not git-vendor-appropriate); the
manifest pins their URLs + SHA-256 + acquisition date for the
re-derivability recipe.

### 4. M6.G↔M6.I interface contract (the pinned constants)

The §3.1 module-level constants in `src/corpus/mod.rs` ARE the contract
M6.I reads from `bench/corpus/filter_spec.txt`:

| Constant | Value | Role |
|---|---|---|
| `QUIET_MARGIN_CP` | 30 | `|static_eval_white − qsearch_white| <` ⇒ quiet |
| `OPENING_SKIP_PLIES` | 8 | Records dropped for `ply < 8` (book over-representation) |
| `HIGH_SCORE_CP` | 600 | Records dropped for `|eval| > 600` (resignation regime) |
| `PER_GAME_CAP` | 10 | At most this many retained positions per source game |

The pinned **score function** for M6.I Texel: a quiet-certified position
is scored by `corpus::quiet::static_eval_white` (= `evaluate` with the
White-POV flip), NOT qsearch — the quiet certificate makes
`static_eval ≈ qsearch` within `QUIET_MARGIN_CP` by construction.
Resolves research §2.5's open question against running qsearch at tune
time.

The **frozen** outpost-stratum predicate (`objective::frozen_outpost_squares`)
is a code-level snapshot of `eval::tier1::outpost_squares` taken at
M6.F-snapshot time. M6.I must NOT redirect to the live impl — re-tuning
M6.F's outpost weights would otherwise drift the stratum definition
beneath the held-out objective (circularity). The
`frozen_outpost_squares_byte_equals_live_at_m6f_snapshot` test pins the
backward direction (was the snapshot correct).

### 5. Fixed-depth deterministic self-play (R-TC empirically-anchored; R4 reasoned exception)

Self-play runs in-process via `Search::go` with
`SearchLimits{ depth: Some(d), nodes: None, movetime: None, infinite: false }`
and `TimeCaps{ soft: MAX, hard: MAX }`. The only `should_abort` reach is
`ctx.stop`. A `stop`-aborted in-flight game contributes **zero** records
(R2 correctness gate). Each completed game's move sequence is therefore
a pure deterministic function of `(start_pos, depth, seed)` —
load-, suspend-, renice-independent ⇒ R3 resumed run bit-identical
modulo the one dropped in-flight game.

**R-TC empirical anchoring (closes the descope-by-fiat path):** the
depth ladder is **NOT** plan literals. `corpus calibrate-ladder
--buckets 100,200,400,600` runs `Search::go` at each canonical
deployment movetime bucket over `bench::BENCH_POSITIONS` on the dev
machine, records the median completed iterative-deepening depth per
bucket, and writes the `(depth, weight)` rungs to `filter_spec.txt` +
`manifest.json`. The held-out objective is stratified by the
*measured-effective-depth* rung ⇒ the R-TC precondition ("the held-out
objective must itself be TC-stratified to the deployment profile") is
genuinely met. Residual caveat owned here: depth is a *proxy* for TC;
the rung↔bucket correspondence is the measured median (run-to-run
variance ±1 ply at fast buckets). Re-runs on a slower machine produce
a different (machine-local) ladder — the vendored manifest's ladder is
the dev-machine value, frozen.

**R4 / VirtualClock — reasoned exception (not "moot"):** R4 literally
mandates `VirtualClock` for M6.G self-play. VirtualClock exists to make
*wall-clock* play load-independent. Fixed-depth play is load-
independent without a clock at all, so VirtualClock is *unnecessary
here*, not merely moot. ADR-0021 §4 rejects node/implementation-coupled
TC because it "shifts what N nodes means across runtime settings /
versions" — the objection is scoped to **cross-version SPRT
comparability**; M6.G is **one frozen build**, and the roadmap R-TC
clause *explicitly* permits "fixed-shallow-depth stratified across
their effective depths (ADR-0021 §4's node-TC rejection is scoped to
cross-version SPRT comparability and does NOT forbid fixed-depth /
fixed-nodes for corpus generation — one fixed build, not a version
comparison)." VirtualClock remains binding on the M6.I SPRT (M6.I's
concern, not M6.G's).

**Fresh searcher per game (R3 bit-identical-resume):** each worker
allocates a new `AlphaBetaMover` at the start of every game. Without
this, the TT/history/killers accumulated across games would taint
post-crash games' continuations (a cold-start resume would diverge from
the warm-from-prior-games uninterrupted baseline). The
`fresh_vs_warm_searcher_same_seed_same_game_identical` test pins the
invariant. This also improves self-play decorrelation (research §2.4).

### 6. Crash-safe per-game atomic block log (R1/R2/R3)

The shard format is **NOT** a line-oriented log. It is an append-only
sequence of **CRC-framed game blocks**:

```
MAGIC:u32 | game_id:u64 | rec_count:u32 | payload_len:u32 | payload | crc32(header‖payload):u32
```

`payload` is the rec_count records, each
`fen_len:u16 | fen | label:u8 | source:u8 | ply:u32 | depth_rung:u8 | strata:u8`
in little-endian. Hand-rolled CRC-32 (IEEE poly), pinned by a known-vector
test.

- **`append_block` is the atomic unit.** A whole game's records are
  buffered in RAM; on a *natural terminal result* the writer issues one
  `write` of the whole block + `fsync`. Crash mid-game ⇒ buffer lost,
  never flushed ⇒ **zero partial-game labels**.
- **Torn final block is discarded WHOLESALE.** `scan_valid_blocks` walks
  frames; the first frame failing magic / length-bounds / CRC stops the
  scan, and the shard is truncated to the last fully-valid byte on
  resume. Never line-by-line.
- **Ordering:** game-block `fsync` THEN checkpoint
  `.tmp`→`fsync`→rename→dir-`fsync`. A crash between them re-emits one
  already-durable game; resume skips already-present `game_id`s
  (idempotent — never partial, never double).

The `tests/corpus_crash_safety.rs::crash_kill_after_first_game_resumes_to_uninterrupted_corpus`
integration test sends a real `libc::kill(pid, SIGKILL)`, resumes, and
asserts shard-records *multiset byte equality* to an uninterrupted
reference (with `--workers 1`, also list-equality on-disk). A heavier
randomized + simulated-suspend variant is `#[ignore]`-gated.

### 7. Two-pass design + deterministic on-disk ordering

`selfplay` emits every post-opening-skip position with the game label +
`depth_rung` transactionally per game — it does **not** apply the quiet
predicate. The separate `build` pass loads the raw shards, applies (in
order): `static_eval_white` + `QSearcher::eval_white` → quiet predicate
→ `|eval|` cutoff → `objective::strata_for` tagging → `dedup_fen`
(deterministic survivor = min `(source, game_id, ply)`, independent of
input order) → `per_game_cap` (seeded reservoir) → game-level
`split_by_game` → emit `train.bin` + `val.bin` with records sorted by
`(game_id, ply, fen)`. The sort is the load-bearing reproducibility
primitive: corpus bytes are a deterministic function of the input
multiset, independent of self-play worker scheduling. `corpus_sha256`
is therefore worker-count-independent.

### 8. Reproducibility — manifest-driven recipe contract (post-tag amendment)

The committed `bench/corpus/manifest.json` records **every** knob
needed to byte-reproduce the artifact: `self_play_seed`, `games`,
`max_plies`, `opening_random_plies`, `workers`, `depth_ladder`,
`split_seed`, `val_fraction`, `corpus_sha256`, plus the source-entry
provenance for any ingested external slice. `bench/corpus/re-run.sh`
reads every knob from the manifest — no `${X:-default}` shell
substitution. `scripts/corpus.sh` is the **operator fresh-build** tool;
`bench/corpus/re-run.sh` is the **byte-identical reproduction** tool —
the header of each documents the distinction.

**Post-tag amendment (the corpus bytes are gitignored).** The original
ADR §8 + the plan §1 committed to "consumers freeze on bytes" — but
that contract only fit a tens-of-MB artifact. At M6.I production scale
(1.5–3 M records ≈ 100–300 MB, plus raw external sources at multi-GB)
git is the wrong tool: GitHub's 100 MB hard file cap, pack-inflation
across nightly regenerations, and useless diff/blame on binary push
us out of git's design envelope. Resolution: `bench/corpus/shard.bin`,
`bench/corpus/val.bin`, `bench/corpus/checkpoint.bin`, and
`bench/corpus/pgn-shard.bin` are now **gitignored**. The committed
artifacts in `bench/corpus/` are the **manifest + filter_spec +
corpus_stats + re-run.sh + the vendored opening book** (the recipe);
**consumers freeze on the manifest, not the bytes.** The bytes are
**operator-materialized** by running `sh bench/corpus/re-run.sh`,
which consumes the manifest's pinned knobs + the vendored opening
book + (operator-staged) external sources and emits byte-identical
shard.bin / val.bin in place.

Reproducibility properties under the amendment:

| Slice | Reproducibility | What's needed |
|---|---|---|
| Self-play | **Strong (offline)** | Manifest seed + games + max_plies + opening_random_plies + workers + depth_ladder + opening_book_sha256 + the engine binary at this commit + the vendored book file. No network. |
| CCRL/Lichess external | **Weaker (re-derivable)** | Above + the raw PGN files at their pinned SHA-256s available at their source URL (or operator-staged from another source). |
| `corpus_sha256` | Verifiable post-build | The manifest's recorded digest is checked by `corpus quality-gate`'s `reproducibility_rerun_match`; mismatch means the recipe drifted. |

**Future hosting (deferred, not in scope for this commit).** Re-running
re-run.sh costs CPU-days at M6.I scale — burning that on every
consumer wanting to reproduce the Texel tune is wasteful. A future
follow-up (separate ADR or hosting-decision note) will pick a canonical
host for the frozen artifact bytes: GitHub Release asset on the `M6.G`
/ `M6.I` tag, S3-style bucket with the manifest's `corpus_sha256` as
the key, or sister-repo `clawfish-corpus`. Consumers download the
bytes + sha256-verify against the manifest; the manifest stays the
truth source. Until that lands, the operator's local materialized
`bench/corpus/{shard,val}.bin` is the only copy.

The integration test `tests/corpus_reproducibility.rs::rerun_byte_identical`
drives the full pipeline twice into sibling temp dirs and asserts
SHA-256 equality of `shard.bin` + `val.bin` — the reproducibility gate
in test form (unchanged across the gitignore-the-bytes amendment, since
the test produces both directories fresh and never depends on
git-committed bytes).

### 9. Disposition — data-quality gate (NOT SPRT); committed artifact is self-play-dominant

The M6.G "tag" is a **frozen-corpus-artifact reference, not an SPRT
engine baseline.** M6.I's SPRT baseline remains the `M6.F` features
tag. `evaluate` / search are unchanged across M6.G — the only engine
touch is the additive `pub fn search::quiescence_eval_white` /
`QSearcher` seam, which leaves negamax / qsearch / the deterministic
bench signature byte-identical (`1213649`).

The **landing gate is the six-check data-quality report**, three of
which are must-PASS: ADR-0003 label-provenance audit, reproducibility
re-run match, held-out-split integrity. The other three (coverage,
decisive/draw balance, dedup ratios) are recorded.

The committed `bench/corpus/` artifact is **self-play-dominant** (12
games × `seed=12648430` × `depth_ladder` calibrated for the dev
machine; `corpus_sha256 = c03f5aa99dcf20bcedb69801c1633b8c34e8e85c1880528be4e62a0ef0f50e60`).
Plan §1 explicitly admits this disposition: "If an external-source
acquisition is infeasible within the landing budget, the artifact is
self-play-dominant with the gap recorded as a coverage stat in the
data-quality gate — an honest recorded shortfall, not a hidden
descope — the parser + filter + `re-run.sh` infra is complete and the
manifest pins how to extend it." An operator with download budget runs
`scripts/corpus.sh` (or `re-run.sh` after staging) to extend the
artifact with CCRL/Lichess slices.

### 10. Four-source taxonomy + opening regime as a per-campaign choice (post-tag amendment)

The original ADR §1/§3 enumerated three `Source` variants (`SelfPlay` /
`Ccrl` / `LichessOpen`) and treated the book / off-book opening mix as
an in-campaign coin flip parameterized by `book_fraction`. The
amendment removes `book_fraction` and splits `SelfPlay` into two
variants by **opening regime**:

| Source variant | Opening seeded from | Operator campaign |
|---|---|---|
| `SelfPlayOnBook` | Sampled FEN from the vendored CC0 opening book | `corpus selfplay --opening-mode=book` |
| `SelfPlayOffBook` | `startpos + opening_random_plies` random plies | `corpus selfplay --opening-mode=random` |
| `Ccrl` | External CCRL PGN | `corpus ingest-pgn --source=ccrl` |
| `LichessOpen` | External band-filtered Lichess PGN | `corpus ingest-pgn --source=lichess` |

Each self-play campaign runs one regime end-to-end and every committed
record carries that regime's `Source` variant. The operator runs one
campaign per regime and extends each independently.

The on-book / off-book proportion is **a training-time per-source
reweighting axis** in M6.I's bi-level optimizer, identical mechanism
to the CCRL / Lichess proportion: the outer simplex moves the
`StratObjective::per_source` weights and the inner Texel refits at
each meta-tunable evaluation. No mix happens at corpus-build time.
The four-way per-source loss vector is the M6.I meta-tunable axis;
the only generation-time decision is "how many records of each
provenance do I have on disk?", which the operator addresses by
running the relevant campaign for longer.

This collapses what was a corpus-generation knob (`book_fraction`,
irreversible without regeneration) into a per-source reweighting (free
at training time on a frozen corpus). Symmetry: all four sources are
treated identically by the outer optimizer.

Wire-format change: the source-byte assignment is
`SelfPlayOnBook = 0`, `SelfPlayOffBook = 1`, `Ccrl = 2`,
`LichessOpen = 3`. Older shard.bin files written before this
amendment (which used `SelfPlay = 0`, `Ccrl = 1`, `LichessOpen = 2`)
are incompatible and are not migrated — the M6.I corpus will be
generated fresh under the four-source taxonomy.

The frozen `STRATUM_BOOK_OPENING` stratum bit on `CorpusRecord::strata`
is removed by the same amendment (bit 2 is reserved). Book vs.
off-book provenance is now strictly the source byte; the stratum bit
became redundant.

The `manifest.json` `book_fraction` field is replaced by
`opening_mode: Option<String>` (`"book"` / `"random"` / absent for
calibrate-ladder-only stubs). `bench/corpus/re-run.sh` reads
`opening_mode` from the manifest and passes the matching
`--opening-mode` flag to `corpus selfplay`.

### 11. Forward-pointer to M6.H — robust on-demand Lichess/CCRL ingestion (post-tag amendment, 2026-05-21)

Beyond the four-source taxonomy of §10, an additional M6 sub-phase **M6.H** was inserted between the corpus infra (M6.G) and the Texel pass (M6.I) to add **on-demand network-layer fetching** of the external sources (`Source::Ccrl`, `Source::LichessOpen`). Pre-amendment, external PGNs were operator-staged into `target/corpus-raw/` and ingested via `corpus ingest-pgn` against already-on-disk files; M6.H adds a `corpus fetch --source={ccrl,lichess}` subcommand + `corpus::fetch` library module that streams from public mirrors with early-termination, in-RAM resume, and infinite-backoff retry — so M6.I's bi-level learning-curve driver can call M6.H as a synchronous "give me N more positions" primitive instead of requiring pre-staged multi-GB downloads.

Design summary (full scope detail in `docs/roadmap.md` §M6 M6.H scope detail):

- **Pure-Rust deps** (`ureq`, `zstd`, `zip`) — no `7z` support, no system-tool subprocess for the fetcher. Self-play continues to use the existing `corpus selfplay` campaign.
- **Streaming + early termination.** The pipeline `HTTP → decompress → stream_pgn → filter → ingest into pgn-shard.bin` is driven by M6.I's `positions_ingested >= target` check. Dropping the reader closes the connection; for a 250K-position request against a Lichess monthly dump, only ~5–10% of the file is downloaded.
- **In-RAM resume via `ResumableHttpReader<R: Read>`.** Custom `Read` impl tracks `bytes_received` and transparently retries on transient TCP failure with `Range: bytes=N-`. The `zstd::Decoder` instance stays alive across retries — zero re-decompression cost on a network blip.
- **No `.partial` file on disk** — the corpus shard is the persistent state; full-process restart re-streams from byte 0 and the shard's dedup absorbs already-ingested games as no-ops.
- **R1/R2 reuse**: `corpus fetch` writes into the same `pgn-shard.bin` log via the same CRC-framed per-game append-block + atomic-rename discipline as `corpus ingest-pgn` and `corpus selfplay`. Interrupted games leak zero records, by construction (M6.G §6 atomic-block contract).
- **No engine touch** — same M6.G discipline. Bench unchanged by construction.

M6.H lands with its own ADR (forthcoming at landing time) and its own milestone retrospective. This entry is the forward-pointer in ADR-0035 so a reader looking at the corpus infra ADR sees that the data-acquisition layer exists.

## Consequences

- M6.I is unblocked: a `bench/corpus/` artifact (with a pinned interface
  contract + a frozen-bytes reproducibility guarantee + a passing
  data-quality gate) is consumable by the M6.I tuner.
- The tuning-backlog "PST co-tuning" Arm B, future SPSA campaigns, and
  M10 NNUE data-prep have a stable contract surface
  (`manifest.json` + `filter_spec.txt` + the binary shard format).
- The committed artifact is small (self-play-dominant). M6.I's
  effective corpus size in the joint-Texel pass depends on the operator
  extending the artifact via `scripts/corpus.sh` before the tune.
- The R-TC depth ladder is dev-machine-local. A re-run on different
  hardware produces a different (recalibrated) ladder — the
  reproducibility gate verifies the *committed* ladder, not a
  recalibration.
- The `cmd_build` no-cleanup of `pgn-shard.bin` after build is a
  documented latent issue for the multi-PGN-ingest workflow (not
  exercised by the committed artifact) — operator-handled until the
  workflow is exercised in M6.I corpus extension.

## Alternatives considered

- **Vendor only the self-play slice; CCRL/Lichess as a documented
  recipe-only path.** Rejected at M6.G landing (plan-review pass-1
  must-fix #4): the consumers freeze on bytes, not recipes; a
  network-only recipe leaves the offline-repro guarantee weaker than
  the plan's "vendored data file in `bench/`" wording. At landing the
  *full filtered* artifact was vendored (compact post-filter); raw DBs
  weren't. **Subsequently amended (§8 post-tag amendment): the bytes
  are gitignored, only the manifest + recipe stay in git** — at M6.I
  production scale (100–300 MB filtered, multi-GB raw) git is the
  wrong tool, and the manifest captures all reproducibility-load-
  bearing state regardless of where the bytes live. Future hosting
  (release asset / S3 / sister-repo) is the long-term home for a
  canonical frozen-bytes copy so consumers don't pay CPU-days
  re-running re-run.sh.
- **Wall-clock self-play with `VirtualClock`** (R4 literal). Rejected
  for the one-frozen-build / R-TC-explicit-carve-out reasoning above —
  fixed-depth gives R3/R4 *by construction* with no clock surface.
- **Engine-evaluation labels (Zurichess `quiet-labeled.epd` `c9`).**
  Rejected by the audit (research §3 finding — Stockfish-080916
  continuation results, not original outcomes).
- **A single shared `Prng` module (hoist `elo_iterate::prng` to a
  crate-level `src/prng.rs`).** Rejected for scope hygiene (churns the
  SPRT-critical harness). The two SplitMix64 copies are pinned to
  identical golden literals so a transcription divergence fails
  test-compile.
- **Per-position `QSearcher` allocation in `build`.** Rejected (R6/R7);
  `QSearcher` is per-worker-reused with cached `Arc<AtomicBool>` /
  history so the build pass over millions of positions doesn't
  thrash the allocator.

## References

- ADR-0003 — `docs/decisions/0003-no-third-party-source-code-reading.md`
  (the spirit constraint the §1/§2 audit codifies).
- ADR-0021 — `docs/decisions/0021-virtual-clock-uci-option.md` §4
  (the cross-version-SPRT scoping the §5 R4 deviation argues against).
- ADR-0031 / ADR-0032 / ADR-0033 / ADR-0034 — the M6.A–F cumulative HCE
  surface the M6.I consumer tunes against.
- `docs/plans/m6.g.md` — the implementation plan (§3.1 pinned interface
  constants; §3.5 R-TC + R4 + R3; §3.6 store frame; §6 quality gate).
- `docs/research/m6-corpus-construction.md` — binding prior art
  (§1 Texel recipe; §2 quiet predicates; §3 the load-bearing
  Zurichess `c9` provenance finding; §6 held-out discipline; §7
  reproducibility).
- `docs/roadmap.md` §M6 — "M6.G scope detail" + R1–R7 + R-TC +
  Verification.
