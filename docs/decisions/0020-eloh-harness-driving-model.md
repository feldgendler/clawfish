# ADR-0020 — ELOH harness driving model: wallclock TC + match-loop time-source seam + native adjudication

**Status:** Accepted (lands with ELOH.A, 2026-04-29).

## Context

ELOH.A is the harness foundation phase of the ELOH (Elo-iteration harness) milestone — an in-process Rust binary at `src/bin/elo-iterate.rs` that drives clawfish + a fixed-config opponent via stdin/stdout pipes, plays games with native adjudication, and emits per-game PGN. Replaces `scripts/elo-iterate.sh` for online rating-estimation runs. ELOH.B adds the K-update + concurrency + σ-stopping + threshold adjudication; ELOH.C adds hardware-invariant TC via `--go-nodes` + `VirtualClock`.

Spec source: `docs/tooling/elo-iteration-harness.md`. Plan: `docs/plans/eloh.a.md`. Pre-ELOH.A preflight probe (Stockfish 18 mid-session `setoption UCI_Elo` honoured): `docs/research/tooling-stockfish-mid-session-setoption.md`.

This ADR records the load-bearing design commitments. Parameter-level decisions (watchdog default formula, harness-overhead grace, test fixture choice) live in the plan; only the architecturally binding items live here.

## Decision

### 1. Wallclock TC for ELOH.A

Per-side time control tracked harness-side via `std::time::Instant::now()`. Per-side `PerSideClock { remaining_ms: i64, increment_ms: u32 }` accumulators, one per *engine* (each carrying its own engine's TC's increment, so `--opponent-tc-override` produces asymmetric TCs cleanly). Per-game colour assignment selects which engine's `PerSideClock` fills white/black slots in the UCI wire string.

The `Wallclock` arm of `MatchTimeMode` issues `go wtime <W> btime <B> winc <Wi> binc <Bi>` per UCI's colour-absolute clock convention. UCI clocks are colour-absolute, not recipient-relative — both engines in a game receive the same wire string for a given ply; each engine extracts only the field corresponding to its own colour.

Increment is credited *after* the move, not before: post-move clock = `prior_remaining - elapsed + increment`. Forfeit fires when post-deduction `remaining_ms < -i64::from(harness_overhead_ms)`. Equivalent stricter form: `elapsed_ms > prior_remaining + harness_overhead_ms`.

**Forfeit signal is harness-measured wallclock, not engine-reported `info ... time T`.** A misbehaving engine could claim `time 0` while wallclocking arbitrarily; the harness MUST measure elapsed via `Instant::now()` deltas around the `go`-send / `bestmove`-receipt boundary. The engine-reported `time` field flows into PGN comments only. Pinned structurally: `pure_apply_move_clock_update`'s signature takes only `(PerSideClock, Instant, Instant, u32)`, no engine-time parameter — a future regression that added one would fail to compile against the test's function-pointer signature pin.

### 2. Subprocess lifecycle: spawn-once-per-run

Persistent subprocesses (clawfish + opponent) spawned once at run start, killed at run end. Per-engine reader thread reads stdout via `BufReader::lines()`, parses each line via `parse_engine_line`, sends `EngineLine` events to a `std::sync::mpsc::sync_channel(1024)` (bounded; pathological engines can't OOM the harness). Main thread consumes the channel via `recv_until_bestmove(handle, watchdog)` which aggregates `Info` lines into the engine's `LastInfo` and returns on `Bestmove`.

Watchdog: default `max(60_000ms, 2 * tc.initial_ms + 30_000ms)`, configurable via `--watchdog-ms`. On watchdog timeout, harness calls `child.kill()` and returns `Err(HarnessError::Watchdog)`. The watchdog kills hung subprocesses regardless of `MatchTimeMode`; only the per-move forfeit-detection logic becomes inert when `mode = Nodes(N)` (gated by `should_apply_clock_update(mode)`).

Shutdown sequence: `quit\n` → close stdin → `try_wait` poll for up to 1 s → `kill()` if still alive → `wait()` to reap → join reader thread. `Drop` impl on `EngineHandle` is best-effort kill on panic-time drop (no blocking; OS reaps).

**Recv-pump discipline (invariant).** Main thread MUST be in a recv-loop during all engine-output windows. Channel-full would block the reader thread; if the main thread is not draining, stale `info` lines could fill the channel and cause the next `bestmove` to be missed. Documented in `match_loop`'s module prose.

Stdin write discipline: `write_all + flush` after every line; line-buffered pipes on macOS otherwise hold lines in kernel buffer until the next write.

Spawn-once contract validated by the preflight probe — Stockfish 18 honours mid-session `setoption UCI_Elo` without `ucinewgame` or process restart.

### 3. `MatchTimeMode { Wallclock, Nodes(u64) }` seam

Lib-side at `src/match_clock.rs`. Method-on-enum: `MatchTimeMode::format_go_command(self, white: PerSideClock, black: PerSideClock) -> String`. The `Nodes(N)` variant is unconstructible from CLI in ELOH.A; ELOH.C's `--go-nodes N` flag fills it in.

This seam covers the `go`-formatting half of ELOH.C's harness-side scope. The other half — `VirtualClock` UCI option negotiation through the handshake state machine — is a separate code path that this seam does not aid; the seam claim is half-the-budget, not full-budget.

Lib-side rather than binary-internal because the seam is a reusable abstraction (M5+ tooling may need the same `go`-format primitive) and is unit-testable in isolation.

### 4. Visibility bump rather than module move

`is_repetition` and `is_fifty_move_draw` bumped from `pub(crate)` to `pub` in `src/search.rs`. The harness needs them for adjudication.

Trade-off considered: moving to a neutral location (`src/draw.rs` or `Position` impl) would be architecturally cleaner but introduces M4-period churn. The lighter visibility-bump touch is chosen for ELOH.A; future search refactors that need to mutate these signatures freely can move them at that point. **Deliberate, not a refactor commitment.**

### 5. FIDE-correct threefold-repetition for adjudication

Harness-side helper `is_threefold_repetition_for_adjudication(history: &[u64]) -> bool` requiring **3 occurrences total** (current + 2 prior) of the last entry's zobrist in the full game history. This is FIDE 9.2's tournament rule.

Distinct from `crate::search::is_repetition`, which fires on a **single prior occurrence** because the search treats a to-be-second occurrence as an effective draw under the worst-case-extends-to-third assumption. Reusing the search helper for adjudication would *under-count* (return `ThreefoldRepetition` after only 2 occurrences in the game record).

Whole-history walk is FIDE-correct; halfmove-bounded walk would be wrong. Equal zobrists in the trail are guaranteed to share the same irreversible-state ancestry — any move that severs the chain (pawn push / capture / castling-rights change / EP-availability change) also flips a zobrist key (piece keys, castling keys, EP-file key per ADR-0009; side-to-move turn key for parity). The doc-comment in the function explicitly warns future readers off "fixing" this to walk-since-last-irreversible.

### 6. PGN move format: UCI long-algebraic, archival use only

Per-game PGN tokens in UCI long-algebraic (`e2e4`, `e7e8q`, `e1g1`). **Non-standard; will not import into lichess / ChessBase / scid** which strictly require SAN.

Rationale: a correct SAN formatter requires per-move movegen for disambiguation (~80 LOC + non-trivial test surface). At ELOH.A scope, the PGN's purpose is archival inspection by harness-internal tooling, not interop. SAN deferred to a later phase if a downstream consumer needs it; new SAN files would land alongside existing UCI-only files with no breakage.

No invented `[Notation "UCI"]` PGN tag — there's no PGN convention or de-facto consumer that reads such a tag. The non-standard format is documented in the plan and validation note, not in the PGN itself.

### 7. Engine launch prefix: split-on-whitespace, prepend to argv

`--engine-launch-prefix "taskpolicy -c utility"` splits on ASCII whitespace and prepends to argv: `Command::new("taskpolicy").args(["-c", "utility"]).arg(engine_path)`. Replaces `scripts/elo-iterate.sh`'s wrapper-script trick (lines 101–108 of the bash version) without external scripts.

Limitation: paths or args with embedded whitespace need quoting and are not currently supported (the only motivating use is `taskpolicy -c utility`). Documented in `--help`.

### 8. `--max-games >= 2 && (n % 2) == 0`

Color-pair invariant: each pair of games swaps colours, so total games must be even. `--max-games 0` is degenerate; `--max-games 1` is both odd and below the minimum. CLI parse rejects with `CliError::InvalidMaxGames(n)`.

### 9. ADR scope is the wire-level + adjudication contract

Items 1–5 above are architecturally load-bearing — they constrain ELOH.B, ELOH.C, and the M4.A measurement window's harness expectations. Items 6–8 are parameter-level decisions but bundled into this single ADR for cohesion. A future split (e.g. into ADR-0020 = items 1–3, 5; `architecture.md` settled-commitments rows for items 4, 6, 7, 8) is acceptable; the present bundling avoids ADR proliferation for a single phase.

## Consequences

- **ELOH.B can layer K-update + σ-stopping + concurrency + threshold adjudication** on top of this foundation without modifying the wire-level contract.
- **ELOH.C fills the `Nodes(u64)` seam variant** for `--go-nodes N`; the `VirtualClock` UCI option negotiation is a separate handshake-state-machine code path.
- **PGN consumers requiring SAN are not served by ELOH.A.** A SAN formatter is a clean follow-up unit if needed.
- **`is_repetition` / `is_fifty_move_draw` are now public crate API.** Future refactor that wants to relocate them can do so without breaking external consumers (the harness binary is the only consumer).
- **Wallclock-noise variance affects the M4.A first rating-estimate's precision.** The plan acknowledges this as documentation-grade, not gate-grade; `docs/tooling/elo-iteration-harness.md` "ELOH.B's role in M4.A measurement" section spells out the mitigation menu (re-measure under `--go-nodes` once ELOH.C lands; or run M4.A's rating estimate at longer TC).

## See also

- Plan: `docs/plans/eloh.a.md`.
- Spec: `docs/tooling/elo-iteration-harness.md`.
- Preflight probe: `docs/research/tooling-stockfish-mid-session-setoption.md`.
- Back-test result: `docs/research/tooling-elo-harness-validation.md` (created post-back-test).
- Related ADRs: ADR-0009 (Polyglot Zobrist key set — load-bearing for the FIDE-correct threefold's zobrist-equivalence argument); ADR-0011 (UCI threading); ADR-0017 (time-management compute_caps formula).
