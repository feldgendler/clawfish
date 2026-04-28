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

These are smoke campaigns intended only to validate the harness setup and the manifest fix from ADR-0013 / final-review pass 1. Saturation-quality runs (≥ 2h per target per the ADR) are pending.

## Crashes triaged

### crash-ad3313fd7ec85772df26cda530d51185d9751093 (fuzz_fen, 2026-04-28)

- **Minimized input:** `4k2r/8/8/8/8/8/8/4K3 b  k- 0 1` (32 bytes; already minimal — the double space cannot be reduced further without changing the structural defect). No `cargo fuzz tmin` run needed.
- **Root cause:** `parse_castling` at `src/fen.rs:218–222` had an `unreachable!()` predicated on the (false) assumption that an empty castling field requires a 7-token split and would have tripped `WrongFieldCount` upstream. In fact, `s.split(' ')` on an input with one double space produces 6 tokens with one empty middle token, so the gate doesn't fire.
- **Fix commit:** `<this commit>` — `unreachable!()` → `return Err(FenError::BadCastlingRights(s.to_string()))`. Comment updated to reflect the actual behavior.
- **Regression test:** `parse_rejects_empty_castling_field_via_double_space` (`src/fen.rs::tests` line ~688).

## Notes on metric provenance

- libFuzzer's `cov:` is libfuzzer-internal (basic blocks instrumented), not the same metric as `cargo llvm-cov`. For unit-test coverage, `cargo llvm-cov --summary-only --lib` remains authoritative.
- `ft:` is libFuzzer's "features" count — a finer-grained signal than `cov:` that includes coverage edges plus comparison signals.
- `runs` is libFuzzer's total executed inputs; throughput (exec/s) declines as the corpus grows because each new input is replayed against more saved inputs.
- Per-target host context matters: Apple M4 numbers won't match Apple M1 numbers won't match x86-64 Linux. Record host with each row.
