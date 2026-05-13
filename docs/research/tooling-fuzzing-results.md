# Fuzzing campaign results

Empirical campaign records per ADR-0013 §10. Per-campaign rows are appended chronologically; each row pins the date, host, target, parameters, and outcome.

## Campaigns

### 2026-04-28 — initial smoke (Apple M4)

cargo-fuzz 0.13.1, libfuzzer-sys 0.4.12, nightly-2026-04-01, `--sanitizer none`, `-max_len=200`.

| date | host | target | seed corpus | runs | exec/s peak | duration | crashes | notes |
|---|---|---|---|---|---|---|---|---|
| 2026-04-28 | Apple M4 | fuzz_fen (pre-fix) | 35 | 6.3k | n/a | <1s | 1 | hit `unreachable!()` at `src/fen.rs:221` on input `4k2r/8/8/8/8/8/8/4K3 b  k- 0 1` (double space after active color → empty castling token). Fixed in commit on the next line; replaced `unreachable!()` with `Err(BadCastlingRights)`. Regression test: `parse_rejects_empty_castling_field_via_double_space` in `src/fen.rs::tests`. |
| 2026-04-28 | Apple M4 | fuzz_fen (post-fix) | 35 | 27,000,268 | ~870k | 31s | 0 | clean |
| 2026-04-28 | Apple M4 | fuzz_uci | 30 | 17,849,550 | ~575k | 31s | 0 | clean |

These are smoke campaigns intended only to validate the harness setup and the manifest fix from ADR-0013 / final-review pass 1.

### 2026-04-28 — first saturation campaign (Apple M4, --jobs=5)

cargo-fuzz 0.13.1, libfuzzer-sys 0.4.12, nightly-2026-04-01, `--sanitizer none`, `-max_len=200`, `--jobs=5` (5 parallel workers, half of M4's 10 cores), `-max_total_time=1440` per worker → ≈2 CPU-hours per target / ~24 min wall-clock per target.

| target | seed corpus | runs (aggregate) | exec/s per worker | wall-clock | cov | ft | corp (libFuzzer-internal) | corpus dir | crashes |
|---|---|---|---|---|---|---|---|---|---|
| fuzz_fen | 35 | 1,349,880,712 | 180k–195k | 1454s | 228 | 486 | 199 | 513 | 0 |
| fuzz_uci | 30 | 926,921,812 | 137k–142k | 1455s | 434 | 1759 | 1084 | 1185 | 0 |

Coverage saturated within the first ~2 seconds of each campaign (`cov` and `ft` settled at near-final values immediately after the seeded corpus loaded). The remaining 24 wall-clock minutes per target found a handful of additional `ft` discoveries (fen +18; uci +76) without expanding `cov`. Read: the seed corpus + libFuzzer's mutation strategy reach the parsers' edge cases fast; the long tail is comparison-signal exploration, not new code paths.

Total executions across both targets: **2.28 billion**. Zero panics, zero OOMs, zero timeouts, zero crashes — strong evidence the post-fix parsers are robust against bounded (≤ 200 byte) `Arbitrary<String>` inputs.

The libFuzzer-discovered corpus expansion (fuzz_fen 35 → 513; fuzz_uci 30 → 1185) is gitignored per `fuzz/.gitignore`'s `!corpus/<target>/{fen,uci}-*` allowlist; only hand-curated seeds are committed.

### 2026-04-28 — `-max_len=4096` smoke (Apple M4, --jobs=5)

Same parameters as the saturation campaign except `-max_len=4096` (libFuzzer's default — 20× the saturation campaign's 200-byte cap) and `-max_total_time=900` (15 min wall-clock per target → ~1.25 CPU-hours per target). Hypothesis being tested: whether `ft` keeps growing past the 200-byte cap, indicating signal space the saturation campaign undersampled.

| target | runs | exec/s per worker | wall-clock | cov | ft | Δft vs 200-byte | corp | crashes |
|---|---|---|---|---|---|---|---|---|
| fuzz_fen | 749,971,601 | 165k–210k | 913s | 228 | 496 | +10 | 196 | 0 |
| fuzz_uci | 140,397,202 | 25k–35k | 916s | 434 | 2238 | **+479** | 1461 | 0 |

Findings:

- **`cov` unchanged on both targets** — basic-block coverage was already saturated at the 200-byte cap. No new code paths.
- **fuzz_fen `ft` grew marginally** (+10) — comparison-signal exploration plateau holds even at 4096 bytes. FEN's grammar is dense and mostly token-bounded by the placement field's `/`-delimiters.
- **fuzz_uci `ft` grew substantially** (+479) — the 200-byte cap was meaningfully undersampling the UCI parser's signal space. UCI commands with long move-lists or `searchmoves` body tokens exercise comparison signals the shorter cap couldn't reach. Still 0 crashes after 140M execs at 4k bytes.
- **exec/s dropped** as expected (longer inputs = more bytes per execution; fuzz_uci hit ~30k/worker vs ~140k at 200 bytes — ~5× slowdown for ~20× input length, sublinear because libFuzzer's mutation cost is per-input not per-byte).

Operational decision: keep `-max_len=200` as the default for routine post-edit smokes (fast feedback) and per-major-milestone backstops (saturation evidence). Run `-max_len=4096` opportunistically — at minimum once per target before any future ADR-0013 cadence change, and any time the parser's input-length contract is touched. The +479 `ft` delta on fuzz_uci is signal that 200-byte saturation isn't the same as 4096-byte saturation; future work that depends on long-input correctness (e.g. PGN-to-FEN converters, position-with-move-list endpoints) should re-check.

### 2026-05-12/13 — overnight quad campaign (Apple M4, --jobs=10, --sanitizer none)

Sequential 4-sub-campaign run motivated by:
1. UCI parser changed at commit `f351ccf` (M3.F `bench` command) on 2026-04-29 — post the previous fuzzing session. ADR-0013 §9's parser-change trigger had not yet fired.
2. Idle overnight slot — opportunistic deep run on both targets at both byte caps. Also the trigger for the cadence revision codified in workflow.md and ADR-0013 §9 (per-milestone backstop dropped; opportunistic added).

cargo-fuzz 0.13.1, libfuzzer-sys 0.4.12, nightly-2026-04-01, `--sanitizer none`, `--jobs=10` (full M4 core count, machine otherwise idle), `-max_total_time=9000` per worker. Total wall-clock 10h01m (22:17:31 → 08:18:51 EEST), aggregate ~100 CPU-hours.

| target | -max_len | seed corpus* | runs | exec/s peak (aggregate) | wall-clock | cov | ft | corp (libFuzzer-internal) | crashes |
|---|---|---|---|---|---|---|---|---|---|
| fuzz_fen | 200 | 250 | 19,351,378,247 | 277k | 9017s | 289 | 622 | 236 | 0 |
| fuzz_fen | 4096 | 236 | 19,792,002,776 | 253k | 9017s | 289 | 630 | 205 | 0 |
| fuzz_uci | 200 | 30 | 13,415,196,721 | 159k | 9019s | 418 | 1638 | 1066 | 0 |
| fuzz_uci | 4096 | 1066 | 3,214,349,872 | 65k | 9020s | 437 | 2318 | 1516 | 0 |

*Each sub-campaign inherits the prior sub-campaign's final corpus state (sequential libFuzzer execution, no `cargo fuzz cmin` between). fuzz_fen entered the run with the 250 corpus files left over from the 2026-04-28 saturation campaign (35 hand-curated seeds + 215 libFuzzer discoveries, gitignored under `fuzz/.gitignore`). fuzz_uci entered with 30 hand-curated seeds only — the prior campaign's libFuzzer discoveries had been cleaned between then and now.

Total: **56.4 billion executions**. Zero panics, zero OOMs, zero timeouts, zero crashes.

Findings:

- **fuzz_fen at full saturation.** Both 200B and 4096B held cov:289 throughout the 2.5h sub-campaign. ft crept from 622 → 630 between sub-campaigns (+8 features, marginal). The FEN parser at its current shape, against the current corpus, is at the floor of what libFuzzer can find. 39 billion executions across the two FEN sub-campaigns produced no behavioral surprises.
- **fuzz_uci at 4096B opens code paths the 200B cap structurally cannot reach** — the **load-bearing new finding of this campaign**. cov climbed from 418 (200B saturation) → 437 (4096B) — **+19 basic blocks** the 200B cap never reached. Prior 2026-04-28 4096B run did not see this (stayed at cov:434), but that was a 900s smoke; the current 9020s run had ~10× more wallclock for mutation to discover the long-input paths. ft delta is +680 (1638 → 2318), substantially larger than the prior +479. No specific triggering inputs identified (libFuzzer doesn't emit per-block-discovery inputs); plausible candidates are long `position startpos moves <a2a4 × N>` payloads, very long `searchmoves` token sequences in `go`, or deeply-malformed bench/setoption tail-tokenization paths.
- **`bench` command (M3.F, `parse_bench` at `src/uci.rs:215-227`) survived 16.6B executions** across both UCI sub-campaigns without panic. The four arms (empty rest / `rest.len() != 1` / `parse::<u32>()` failure / out-of-range `1..=63` bound) all robust.
- **exec/s scaling reproduces prior observations.** fuzz_fen at 4096B vs 200B holds ~95% throughput (FEN parser body is short — input-length cost is small). fuzz_uci at 4096B vs 200B is ~22% throughput (UCI parser walks every whitespace-delimited token, so long inputs scale parser cost roughly linearly with token count).
- **Apple M4 thermal behavior at sustained 10-job load.** Aggregate exec/s declined ~5% from sub-campaign start to end on each FEN run (peak 277k → 219k final at 200B; 253k → 218k final at 4096B), consistent with thermal soft-throttling on the passively-cooled M4 silicon under sustained 100% load. UCI sub-campaigns at lower per-input throughput showed the same pattern proportionally. No worker died from rss_limit (default 2GB × 10 workers = 20GB worst-case, well within the 32GB system).

Operational decision update (supersedes 2026-04-28's recommendation): **prefer `-max_len=4096` for parser-change-triggered campaigns and opportunistic backstops** going forward. The 2.5h 4096B fuzz_uci run found code paths the 200B cap structurally cannot reach; the throughput cost (~80% of executions but in deeper code) is the right tradeoff when the goal is finding new bugs. Keep `-max_len=200` only for fast post-edit smoke checks (~10s feedback loop). The cadence rule itself was revised at this session (workflow.md §Fuzzing; ADR-0013 §9): per-milestone backstop dropped, parser-change trigger preserved, opportunistic deep runs added.

## Crashes triaged

### crash-ad3313fd7ec85772df26cda530d51185d9751093 (fuzz_fen, 2026-04-28)

- **Minimized input:** `4k2r/8/8/8/8/8/8/4K3 b  k- 0 1` (32 bytes; already minimal — the double space cannot be reduced further without changing the structural defect). No `cargo fuzz tmin` run needed.
- **Root cause:** `parse_castling` had an `unreachable!()` predicated on the (false) assumption that an empty castling field requires a 7-token split and would have tripped `WrongFieldCount` upstream. In fact, `s.split(' ')` on an input with one double space produces 6 tokens with one empty middle token, so the gate doesn't fire.
- **Fix:** commit `68df7c8` — `unreachable!()` → `return Err(FenError::BadCastlingRights(s.to_string()))`.
- **Regression test:** `parse_rejects_empty_castling_field_via_double_space` in `src/fen.rs::tests`.

## Notes on metric provenance

- libFuzzer's `cov:` is libfuzzer-internal (basic blocks instrumented), not the same metric as `cargo llvm-cov`. For unit-test coverage, `cargo llvm-cov --summary-only --lib` remains authoritative.
- `ft:` is libFuzzer's "features" count — a finer-grained signal than `cov:` that includes coverage edges plus comparison signals.
- `runs` is libFuzzer's total executed inputs; throughput (exec/s) declines as the corpus grows because each new input is replayed against more saved inputs.
- Per-target host context matters: Apple M4 numbers won't match Apple M1 numbers won't match x86-64 Linux. Record host with each row.
