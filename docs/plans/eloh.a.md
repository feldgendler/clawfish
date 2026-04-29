# ELOH.A — Harness foundation

In-process tournament harness binary `src/bin/elo-iterate.rs`. Drives clawfish + a fixed-config opponent via stdin/stdout pipes; plays games with native adjudication; emits per-game PGN + summary. Replaces fastchess for the *correctness layer* of online Elo iteration. Online K-update / σ-stopping / concurrency / threshold adjudication land in ELOH.B.

Spec source: `docs/tooling/elo-iteration-harness.md` ELOH.A section (in-scope items 1–9, deferred items, open questions, back-validation gate). Pre-ELOH.A preflight probe complete: `docs/research/tooling-stockfish-mid-session-setoption.md` confirms Stockfish 18 honours mid-session `setoption UCI_Elo` — spawn-once contract is viable.

ADR: a small ADR lands with this phase covering the **wallclock-TC harness driving model** + **`MatchTimeMode` seam** decisions. Slot allocated at landing. Title: `ELOH harness driving model — wallclock TC + match-loop time-source seam`.

## 0. Goals

- Persistent UCI subprocesses for both engines (clawfish + opponent), spawned once at run start, killed at run end.
- UCI driver implementing the FROM-engine direction (parses `bestmove`, `info depth … score …`).
- Native adjudication: mate, stalemate, 50-move, 3-fold repetition, insufficient material (KK, KBK, KNK, KBKB-same-colour). Negative case: KNvKN is NOT insufficient. **No threshold adjudication** (resign/draw-by-score/maxmoves) — deferred to ELOH.B.
- Per-side wallclock management: `wtime/btime/winc/binc` with `Instant::now()`-based accounting; time forfeit on overflow; harness-overhead grace (default 50 ms).
- **`MatchTimeMode { Wallclock, Nodes(u64) }`** seam threaded through `go`-formatting and clock-tracking. ELOH.A implements only `Wallclock`; `Nodes(u64)` is unconstructible from CLI; ELOH.C fills it in.
- Color-paired fixed-batch loop: game N+1 plays the inverse colours of game N (same opening). `--max-games N` counts games, not pairs; must be even.
- Per-game PGN at `target/elo-iterate/<run-id>/games/<game-index>.pgn` (Seven Tag Roster + per-move `{depth=N score=cp X time=T}` comments). Move tokens in **UCI long algebraic** form (`e2e4`, `e7e8q`, `e1g1` for kingside castling) — see decision §3.5. Per-game line appended to `summary.txt`.
- `--engine-launch-prefix CMD` on each engine to inject `taskpolicy -c utility` etc., replicating `scripts/elo-iterate.sh`'s P-core-pinning trick without external wrapper scripts.
- Color-paired loop: `--max-games N` requires `N >= 2 && N % 2 == 0`.
- Back-test gate: harness reproduces M3.F's 196-4-0 against `UCI_Elo=1320` Stockfish within Wilson-95 % (~190–198 W / 2–10 L of 200).

## 1. Out of scope

- K-update / Robbins-Monro / σ-stopping / mid-run opponent reconfiguration → **ELOH.B**.
- Concurrency (parallel game-pairs) → **ELOH.B**.
- Threshold adjudication (resign / draw-by-score / max-moves) → **ELOH.B**.
- `--go-nodes N` (the `Nodes(u64)` variant) and `VirtualClock` UCI option → **ELOH.C**.
- Running the full 200-game back-test inside the autonomous loop. The plan covers an `#[ignore]`-gated 4-game smoke; full back-test is launched after the unit lands and recorded in `docs/research/tooling-elo-harness-validation.md` as a manual step.
- SAN move formatter for PGN. Defer to ELOH.B / later if needed; UCI long-algebraic suffices for archival.
- An `engines.json` registry. Each run takes engine + opponent paths on the CLI.
- Tournament book / opening-positions file. Startpos-only for ELOH.A; matches `scripts/elo-iterate.sh`.

## 2. Files modified

| File | Change | LOC est |
|---|---|---|
| `src/bin/elo-iterate.rs` | New binary. Sub-modules `cli`, `driver`, `match_loop`, `pgn`, `adjudicate`. | new ~500 |
| `src/match_clock.rs` | New lib module. `MatchTimeMode { Wallclock, Nodes(u64) }` enum + `MatchTimeMode::format_go_command(self, white: PerSideClock, black: PerSideClock) -> String` (method-on-enum; UCI clocks colour-absolute, no `side_to_move` parameter) + `PerSideClock { remaining_ms, increment_ms }`. Lib-side because the seam is the abstraction other tooling will likely re-use; unit-testable in isolation. | new ~80 |
| `src/lib.rs` | `pub mod match_clock;` + `pub use match_clock::{MatchTimeMode, PerSideClock};`. | +2 |
| `src/search.rs` | Bump `is_repetition` and `is_fifty_move_draw` from `pub(crate)` to `pub`. Two-line visibility bump; signatures unchanged. | +0 / -0 (visibility) |
| `Cargo.toml` | `[[bin]] name = "elo-iterate" path = "src/bin/elo-iterate.rs"`. | +3 |
| `tests/elo_harness.rs` | New integration-style test file inside the binary's test surface (or `src/bin/elo-iterate.rs`'s `#[cfg(test)] mod tests`). Tests for adjudication (~120), `MatchTimeMode` seam (~40), PGN formatter (~30), UCI driver including real-subprocess shutdown (~80), time-forfeit (~50), CLI parse (~20). Plus one `#[ignore]`-gated 4-game smoke spawning the actual built `clawfish` binary against itself (~30). | new ~370 |
| `docs/decisions/00XX-eloh-harness-driving-model.md` | New ADR. | new ~80 |
| `docs/tooling/elo-iteration-harness.md` | Mark ELOH.A row done; convert ELOH.A scope detail into "Done" prose noting actual landing size. | +0 / -0 (re-state) |
| `docs/research/tooling-elo-harness-validation.md` | New file with back-test result + transcript. Created after manual back-test lands. | new ~60 |
| `docs/architecture.md` | Add row "Elo iteration harness" → `src/bin/elo-iterate.rs` + `src/match_clock.rs`. | +5 |
| `docs/roadmap.md` | Mention ELOH milestone alongside M4 (parallel track). | +5 |
| `.cargo/mutants.toml` | Anticipated exclusion: PGN comment string formatting where mutations on the trailing format-string are equivalent. Survivor-driven, added at pre-review if needed. | +0..10 |

## 3. Type definitions and key signatures

### 3.1 `MatchTimeMode` (lib)

```rust
// src/match_clock.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchTimeMode {
    /// Per-side wallclock tracked harness-side. ELOH.A.
    Wallclock,
    /// Send `go nodes N` to both engines; per-side clock unused. ELOH.C.
    Nodes(u64),
}

#[derive(Debug, Clone, Copy)]
pub struct PerSideClock {
    pub remaining_ms: i64,   // signed: negative = time-forfeit imminent
    pub increment_ms: u32,
}

impl MatchTimeMode {
    /// Format the `go` command per UCI's colour-absolute clock convention.
    ///
    /// `Wallclock`: `go wtime <W> btime <B> winc <Wi> binc <Bi>`.
    /// `Nodes(N)`:  `go nodes <N>` — clock params ignored.
    ///
    /// **Asymmetric TC.** UCI's `wtime`/`btime`/`winc`/`binc` are
    /// **colour-absolute**, not relative to the receiving engine. When the
    /// match has different TCs per engine (`--opponent-tc-override`), the
    /// match loop maintains a `PerSideClock` *per engine* (engine A clock
    /// uses `tc_A.inc`; engine B clock uses `tc_B.inc`). Per-game colour
    /// assignment selects which engine's `PerSideClock` fills `white_*`
    /// vs `black_*`; the wire string is then the *same* for both
    /// recipients in that game (because UCI clocks are colour-absolute,
    /// not recipient-relative). Game N+1 swaps the colour mapping; the
    /// strings differ across games but not across recipients within one
    /// game. No `side_to_move` parameter — irrelevant by spec.
    pub fn format_go_command(
        self,
        white: PerSideClock,
        black: PerSideClock,
    ) -> String;
}
```

The match loop owns per-engine `PerSideClock` and resolves white/black assignment at game start. The seam is method-on-enum so the `match` over variants is internal and idiomatic; the unused-params shape from the v1 plan is gone (`Nodes(N)` arm just ignores `white`/`black` and emits `go nodes N`).

**Receiving-engine clock-extraction note.** UCI clocks are colour-absolute. The wire string `go wtime <A's_remaining> btime <B's_remaining> winc <A's_inc> binc <B's_inc>` (where engine A plays White, engine B plays Black) is identical for both recipients in that game; each engine extracts only the field corresponding to its colour at the current FEN's side-to-move. The other colour's fields are reference-only — engines use them for time-management decisions (e.g., "opponent has 5 s left, I should play faster"). This makes the harness's `format_go_command` recipient-agnostic: the same string is sent to both engines.

### 3.2 Adjudication enum (binary-internal)

```rust
// src/bin/elo-iterate.rs::adjudicate
pub(crate) enum GameOver {
    Checkmate(Color),    // colour that delivered mate (= side-to-move's opponent)
    Stalemate,
    FiftyMove,
    ThreefoldRepetition,
    InsufficientMaterial,
    TimeForfeit(Color),  // colour that lost on time
}

/// Native-adjudication check after a move has been made on `pos` and `history`
/// has been extended with the new position's zobrist. Does NOT cover time
/// forfeit (computed by the match loop from per-side-clock state).
///
/// `history` is the full-game zobrist trail (every position from game start),
/// not a since-last-irreversible-move trail. `pos.halfmove_clock()` is the
/// stopping bound for the repetition walk.
pub(crate) fn detect_native_game_over(
    pos: &Position,
    history: &[u64],
) -> Option<GameOver>;

pub(crate) fn is_insufficient_material(pos: &Position) -> bool;

/// Adjudication-level threefold (FIDE 9.2): position has appeared **3 times
/// total** (current + 2 prior occurrences). Distinct from `crate::search::is_repetition`,
/// which fires on a **single prior occurrence** because the search treats a
/// to-be-second occurrence as an effective draw under a worst-case-extends-
/// to-third assumption. Reusing the search helper here would *under-count*
/// (return `ThreefoldRepetition` after only 2 occurrences).
///
/// Implementation: count entries in `history` equal to the last entry; return
/// true iff count ≥ 3. The walk is **whole-history**, not since-last-
/// irreversible — FIDE 9.2 says "the same position has occurred at least three
/// times" *in the game record*, not "in an unbroken chain since the last
/// irreversible move." A future reader tempted to "fix" this by walking only
/// since the last halfmove-clock reset would break FIDE-correctness — DO NOT.
///
/// Why whole-history is safe: equal zobrists in the trail are guaranteed to
/// have the same irreversible-state ancestry. Any move that severs the chain
/// (pawn push / capture / castling-rights change / EP-availability change)
/// also flips a zobrist key (piece keys, castling keys, EP-file key per
/// ADR-0009), so equal zobrists ⇒ same {pieces, castling, EP-availability,
/// side-to-move} state. The 2-ply parity holds implicitly because zobrist
/// includes the side-to-move turn key.
///
/// **Big-O.** O(history.len()). Adjudication runs once per move (not once per
/// node), so a 200-move game is 200 hash comparisons amortised — negligible.
/// The search-side helper's halfmove-bounded walk is a hot-path optimisation,
/// not a correctness requirement.
pub(crate) fn is_threefold_repetition_for_adjudication(history: &[u64]) -> bool;
```

Order of checks inside `detect_native_game_over` (each pair pinned by a precedence test in §5.1):
1. Generate legal moves. If empty: in-check → `Checkmate(non-side-to-move)`; else → `Stalemate`.
2. `is_fifty_move_draw(pos)` → `FiftyMove`.
3. `is_threefold_repetition_for_adjudication(history)` → `ThreefoldRepetition`.
4. `is_insufficient_material(pos)` → `InsufficientMaterial`.
5. None.

### 3.3 UCI driver (binary-internal)

```rust
// src/bin/elo-iterate.rs::driver
struct EngineHandle {
    name: String,
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    rx: std::sync::mpsc::Receiver<EngineLine>,  // bounded — see "channel discipline"
    reader: std::thread::JoinHandle<()>,
    last_info: LastInfo,  // depth, score-cp-or-mate, time-ms — see §3.4
}

enum EngineLine {
    Bestmove { uci: String, ponder: Option<String> },
    Info(InfoLine),
    Other(String),  // unknown / passed-through
    Eof,
}

struct InfoLine {
    depth: Option<u32>,
    score: Option<Score>,    // score cp X | score mate N
    nodes: Option<u64>,
    time_ms: Option<u64>,    // engine-reported; for PGN comments only, NOT forfeit detection
    pv: Option<String>,
}

enum Score { Cp(i32), Mate(i32) }

fn spawn_engine(spec: &EngineSpec) -> Result<EngineHandle, HarnessError>;
fn send_line(h: &mut EngineHandle, line: &str) -> Result<(), HarnessError>;  // write_all + flush
fn recv_until_bestmove(h: &mut EngineHandle, watchdog: Duration) -> Result<BestMoveOutcome, HarnessError>;
fn shutdown(h: EngineHandle) -> Result<(), HarnessError>;  // sends `quit`, joins reader, kills if needed
```

**Watchdog policy.** Default `--watchdog-ms` is `max(60_000, 2 * tc.initial_ms + 30_000)`. For `--tc 10+0.1` → 60 s; for `--tc 60+0` → 150 s; for nodes-mode (ELOH.C) → 60 s. Killing a child that exceeds the watchdog is a misbehaviour-detection mechanism, not a TC-enforcement mechanism (see "Time forfeit" below). **Invariant** (per spec): the watchdog is active in all modes; only the per-move UCI time-forfeit logic becomes inert when `mode = Nodes(N)`.

**Time forfeit (wallclock-measured, not engine-reported).** Per spec item 4 and FIDE-style time-forfeit semantics: the harness measures `elapsed = Instant::now()` between sending `go ...` and receiving `bestmove ...`. After the move, deduct `elapsed.as_millis()` from the active engine's `PerSideClock.remaining_ms`; credit `increment_ms`.

**Sign convention.** `remaining_ms: i64` accumulates `(prior_remaining - elapsed + increment)` per move; can go negative within the grace window. **Forfeit fires when** `remaining_ms < -i64::from(harness_overhead_ms)` after the deduction. Equivalent stricter form for tests: `elapsed_ms > prior_remaining + harness_overhead_ms`. Tests use the *post-deduction* `remaining_ms < -grace` form for consistency with the implementation; the equivalent-elapsed-form is mentioned in test comments only.

The engine-reported `info ... time T` field is **not** used for forfeit — a misbehaving engine could claim `time 0` while wallclocking arbitrarily, and the harness must not be fooled. The `time_ms` field flows into PGN comments (§3.4) and nothing else.

**Channel discipline.** Reader-side sender uses `std::sync::mpsc::sync_channel(1024)` rather than the unbounded `channel()`. A pathological engine emitting `info` faster than the main thread consumes them is held back by the bounded send (mild flow control). 1024 is well above any legitimate per-move info rate (M3.E emits one info per ID iteration; a 12-deep search emits 12 lines).

**Recv-pump discipline.** When the channel is full, the reader thread blocks on `send`. **Invariant:** the main thread MUST be in a recv-loop (`recv_until_bestmove` or equivalent) during all engine-output windows. Concretely: any time the harness has issued a command that may produce engine output (`go`, `setoption`, `isready`, `position`), it must immediately drain the channel until the expected response arrives. A future implementer adding e.g. a `setoption` round-trip without a recv-pump would risk: reader blocks on `send` because the channel filled with stale `info string` from the prior `go`; main thread proceeds without seeing the `setoption` ack; `bestmove` from the next `go` may arrive after the stale `info` and be missed. The plan's `match_loop` interleaves command-issue with recv-pump tightly to avoid this; documented in `match_loop`'s module-level prose.

**Stdin write discipline.** `send_line` issues `stdin.write_all(...)` then `stdin.flush()`. Without flush, line-buffered pipe semantics on macOS can hold the line in the kernel buffer until the next write — a hang the symptoms of which would be hard to debug.

**Subprocess shutdown.** `shutdown` sends `quit\n`, polls `child.try_wait()` for up to 1 s, then `child.kill()` if still alive, then `child.wait()` to reap. Reader thread joins after the child exits (its `BufReader::lines()` returns `Ok(None)` on EOF, which it forwards as `EngineLine::Eof` then exits its loop). Drop-on-panic: `EngineHandle`'s `Drop` impl calls `child.kill()` (best-effort, non-blocking) so a panicking match loop doesn't leak processes.

`recv_until_bestmove` aggregates `Info` lines into `h.last_info` (the most-recent depth/score). On `Eof` returns `Err`; on watchdog timeout calls `child.kill()` and returns `Err`. The kill+reap path is exercised in §5.3 by an integration test that spawns a real `cat`-style child (no clean way to spawn a "hung engine"; `cat` reads stdin forever and never emits `bestmove`).

### 3.4 PGN per-move comment policy

The plan's policy: take the line whose `depth` matches the engine's *final* iteration before `bestmove` — i.e. the last `info depth N …` line emitted before the next `bestmove`. The driver tracks `LastInfo { depth, score, time_ms }` per engine, resetting it when a new `go` is sent and clobbering on each `info depth …`. At `bestmove` arrival, `LastInfo` is the snapshot to write into the PGN comment.

If `LastInfo.depth` is `None` (engine never emitted an info line — e.g. on a position where M3.E ID returns from iter-1 without emitting the info line, which shouldn't happen but is theoretically possible if the engine crashes mid-thought), the PGN comment is omitted for that move. Documented in plan.

**M4 forward-compat note.** M3.F's deterministic single-info-line-per-depth output makes "last line per depth" unambiguous. M4's aspiration windows will emit multiple `info depth N …` lines per depth (re-searches with `lowerbound`/`upperbound` markers). The policy holds — last `info depth N` before `bestmove` still wins — but a `lowerbound`/`upperbound` last-line would surface an aspiration-fail score in the PGN, slightly misleading. M4 may want to filter `info` lines without those markers as the per-depth representative; refinement deferred.

### 3.5 PGN move-token format — UCI long-algebraic

```
1. e2e4 {depth=12 score=cp 35 time=237} e7e5 {depth=11 score=cp -32 time=205}
2. g1f3 {depth=12 score=cp 41 time=251} ...
```

Rationale:
- A correct SAN formatter requires running movegen on every move to compute disambiguation (file/rank/full). Adds ~80 LOC and the test surface is non-trivial. Out of ELOH.A scope per workflow's typical-unit budget.
- UCI long-algebraic is what the engine emits anyway; zero-cost transformation.
- The PGN's purpose at ELOH.A is **archival inspection by harness-internal tooling**, not interop with general chess software. PGN with UCI move tokens is **non-standard** and **will not import correctly into lichess, ChessBase, scid, or other strict-SAN consumers** — those tools require Standard Algebraic Notation. If a downstream consumer of harness PGN is needed (e.g. SPRT-style fastchess result-comparison tooling), a SAN formatter can land in ELOH.B without breaking already-archived ELOH.A PGN files (UCI-only files are still inspectable; new files would be SAN).
- The back-test gate (W/L/D) doesn't depend on move format — the harness reads its own PGN to verify W/L/D matches M3.F's 196-4-0.

No invented `[Notation "UCI"]` PGN tag — the format is documented in the plan / `docs/research/tooling-elo-harness-validation.md`, not in the PGN itself, to avoid implying spec compliance. Operators who need SAN can run the engine through fastchess for now, or wait for the SAN formatter.

### 3.6 PGN tag set

Seven Tag Roster (PGN spec):
- `[Event "ELOH.A back-test"]` — from `--event-tag`, default "ELOH.A run".
- `[Site "<hostname>"]` — `gethostname()`.
- `[Date "YYYY.MM.DD"]` — local date at run start.
- `[Round "N"]` — game index 1-based.
- `[White "<engine name>"]` — from engine spec.
- `[Black "<engine name>"]`.
- `[Result "1-0" | "0-1" | "1/2-1/2"]`.

Plus extensions:
- `[TimeControl "10+0.1"]` — from `--tc`, propagated.
- `[FEN "<startpos-fen>"] [SetUp "1"]` — only when starting position is non-startpos. ELOH.A is startpos-only, but emit unconditionally for robustness; harmless at startpos. **Decision: emit only when non-startpos** to keep the common case PGN clean.
- `[Termination "<reason>"]` — `"normal"` / `"adjudication: <native-reason>"` / `"time forfeit"`.

**Dropped from v1 plan:** `[Variant "Standard"]` (PGN spec uses `Variant` only for *non*-standard chess, e.g. Suicide/Atomic/Fischerandom; including it on standard games is a private extension that confuses strict consumers). `[Notation "UCI"]` (no PGN convention or de-facto consumer reads such a tag — it would mislead).

### 3.7 CLI surface

```
elo-iterate \
    --engine PATH                      # required
    --opponent PATH                    # required
    [--engine-launch-prefix CMD]       # e.g. "taskpolicy -c utility"
    [--opponent-launch-prefix CMD]
    --tc TC                            # e.g. "10+0.1"; required
    [--opponent-tc-override TC]        # default = --tc
    --max-games N                      # required; even and >= 2
    [--out-dir PATH]                   # default = target/elo-iterate/<RUN_ID>/
    [--harness-overhead-ms N]          # default 50
    [--watchdog-ms N]                  # default = max(60_000, 2*tc.initial_ms + 30_000); see §3.3
    [--event-tag STRING]               # default "ELOH.A run"
    [--engine-option NAME=VALUE]+      # repeatable; sent as `setoption name NAME value VALUE` to clawfish
    [--opponent-option NAME=VALUE]+    # repeatable; sent to opponent
```

`--engine-option` covers `MoveOverhead=50` etc. for clawfish; `--opponent-option UCI_LimitStrength=true --opponent-option UCI_Elo=1320` is how the M3.F back-test is configured.

`--max-games` must satisfy `n >= 2 && n % 2 == 0` (color-pair invariant + non-degenerate). CLI parse error otherwise. Tested.

`--seed N` from the spec is **deferred to ELOH.B** alongside opening-position selection — startpos-only ELOH.A has no opening randomness to seed.

`--engine-launch-prefix CMD` splits on whitespace and prepends to argv. Limitation: paths/args with embedded whitespace need quoting and are not currently supported (only `taskpolicy -c utility`-style prefixes work). Documented in `--help`.

Defaults from `scripts/match.sh` and `scripts/elo-iterate.sh` where they exist; new flags fall back to documented defaults.

### 3.8 TC parser

```rust
struct TimeControl { initial_ms: u32, increment_ms: u32 }
fn parse_tc(s: &str) -> Result<TimeControl, ParseError>;
```

Supports `"10+0.1"` (10 s base + 100 ms increment), `"60"` (60 s, no inc), `"10/0+0.05"` (deferred — sudden-death only for ELOH.A; classical-style time controls n/a). Format: `<seconds>+<seconds>` with float seconds for the increment.

## 4. Module boundaries

```
src/match_clock.rs                     <- lib module, pub
src/bin/elo-iterate.rs                 <- binary entrypoint
    mod cli                            <- argument parsing
    mod driver                         <- subprocess + UCI line parsing
    mod adjudicate                     <- native game-over detection
    mod match_loop                     <- color-paired loop, per-game state
    mod pgn                            <- PGN tag-roster + body emission
    mod summary                        <- summary.txt aggregation
    fn main()                          <- glue: parse cli, spawn, run, shutdown
```

`src/match_clock.rs` is the only piece that lives lib-side. The seam is the load-bearing abstraction for ELOH.C; the binary modules are private.

## 5. Test coverage strategy

In-process unit tests live alongside their binary submodule via `#[cfg(test)] mod tests`. Test names use the `<area>_<scenario>` convention.

### 5.1 Adjudication (`mod adjudicate::tests`, ~120 LOC)

| Test | Asserts |
|---|---|
| `mate_in_two_fool_known_position` | After `f2f3`, `e7e5`, `g2g4`, `d8h4` → `detect_native_game_over` returns `Checkmate(Black)`. |
| `stalemate_kf8_pe6_ke6_known_position` | A constructed stalemate position → returns `Stalemate`. |
| `fifty_move_at_halfclock_100` | Position with halfmove_clock=100, not in mate or stalemate → `FiftyMove`. |
| `fifty_move_at_halfclock_99_returns_none` | halfmove_clock=99 → no game-over. |
| `threefold_via_history_three_occurrences` | Manually-fed history where the last entry's zobrist appears 3 times total (current + 2 prior) → `ThreefoldRepetition`. |
| `threefold_only_two_occurrences_returns_none` | Same hash 2× total → no game-over. Pins the FIDE-3-vs-search-1 distinction. |
| `is_threefold_repetition_for_adjudication_single_entry_returns_false` | `[h]` → false. |
| `is_threefold_repetition_for_adjudication_two_entries_returns_false` | `[h, h]` → false (FIDE requires three, not two). |
| `is_threefold_repetition_for_adjudication_three_entries_returns_true` | `[h, h, h]` → true. |
| `is_threefold_repetition_for_adjudication_counts_across_intervening` | `[a, h, b, h, h]` → true (FIDE 9.2 counts position occurrences across the full game record; intervening positions don't reset the count). Comment in test asserts this is FIDE-correct, not implementation-convenient — DO NOT "fix" the function to walk only since the last irreversible move. |
| `insufficient_kk` | Position with only the two kings → `InsufficientMaterial`. |
| `insufficient_kbk` | Position with K + B vs K → `InsufficientMaterial`. |
| `insufficient_knk` | Position with K + N vs K → `InsufficientMaterial`. |
| `insufficient_kbkb_same_colour` | K + B (light sq) vs K + B (light sq) → `InsufficientMaterial`. |
| `not_insufficient_kbkb_opposite_colour` | K + B (light sq) vs K + B (dark sq) → no game-over. |
| `not_insufficient_knkn` | K + N vs K + N → no game-over (FIDE: theoretically winnable). |
| `not_insufficient_two_knights_vs_king` | K + N + N vs K → no game-over (helpmate exists). |
| `not_insufficient_kpvk` | K + P vs K → no game-over. |
| `not_insufficient_krkr` | K + R vs K + R → no game-over. |
| `precedence_mate_over_fifty` | halfmove_clock=100 *and* side to move mated → `Checkmate`, not `FiftyMove`. |
| `precedence_mate_over_threefold` | Threefold-claimable history *and* side to move mated → `Checkmate`, not `ThreefoldRepetition`. |
| `precedence_fifty_over_threefold` | halfmove_clock=100 *and* threefold-claimable → `FiftyMove`, not `ThreefoldRepetition`. |
| `precedence_threefold_over_insufficient` | Threefold-claimable *and* insufficient-material (KK with a history where the current zobrist appears 3×) → `ThreefoldRepetition`, not `InsufficientMaterial`. **Note:** under FIDE both call the game a draw; the precedence pin is purely about the harness's `Termination` tag value, not the game outcome. Test still load-bearing because PGN-tag stability matters for downstream auditing. |

**`precedence_mate_over_insufficient` deliberately omitted.** Mate is provably impossible on an 8×8 board with any FIDE-insufficient material configuration (KK, K+B-vs-K, K+N-vs-K, KBvKB-same-colour) — the bishop can only attack one colour and the lone king always has a different-colour escape square; a knight cannot pin a king to an edge alone. So this precedence pair is vacuous in legal chess. The order-of-checks in §3.2 still puts mate before insufficient (test `mate_in_two_fool_known_position` exercises mate detection independently); no integration test can hit the precedence head-on because no such position exists.

### 5.2 `MatchTimeMode` seam (`src/match_clock.rs::tests`, ~40 LOC)

| Test | Asserts |
|---|---|
| `wallclock_formats_colour_absolute` | `Wallclock.format_go_command(w_clock, b_clock)` → `"go wtime <W> btime <B> winc <Wi> binc <Bi>"`. |
| `wallclock_asymmetric_increments` | `Wallclock.format_go_command({rem:5000, inc:100}, {rem:6000, inc:200})` → `"go wtime 5000 btime 6000 winc 100 binc 200"`. Pins per-engine asymmetric `winc`/`binc` propagation under `--opponent-tc-override`. |
| `nodes_mode_emits_go_nodes_n` | `Nodes(50_000).format_go_command(any, any)` → `"go nodes 50000"`. Clock params ignored; both arms construct equivalent. |
| `wallclock_zero_increment_emits_inc_fields_with_zero` | With `winc=0`, output contains `winc 0 binc 0` literally (UCI-spec compliant; engines accept `0`). |
| `wallclock_negative_remaining_emits_negative_field` | `remaining_ms = -50` (post-deduction, pre-forfeit-detection edge) → `wtime -50` literal. UCI engines should reject or refuse to think; harness logic uses this state for forfeit detection, not for actual `go` issuance, but the formatter is total. |

### 5.3 UCI driver (`mod driver::tests`, ~80 LOC)

Mock-pipe tests use a synthetic `Receiver<EngineLine>` populated by tests directly (no subprocess); driver code is parameterised over the recv side. Equivalent to dependency injection. Plus one *real-subprocess* shutdown test using `cat` as a stand-in hung engine (reads stdin forever, never emits `bestmove`).

| Test | Asserts |
|---|---|
| `recv_until_bestmove_aggregates_last_info` | Stream `info depth 8 …`, `info depth 12 score cp 100 time 250 …`, `bestmove e2e4` → `last_info` reflects depth 12, cp 100, time 250. |
| `recv_until_bestmove_handles_score_mate` | Stream `info depth 5 score mate 3 …`, `bestmove a1a8` → `last_info.score = Some(Mate(3))`. |
| `recv_until_bestmove_eof_is_err` | Stream `Eof` before `bestmove` → returns `Err(HarnessError::EngineExit)`. |
| `recv_until_bestmove_watchdog_fires` | No lines, watchdog 100 ms → returns `Err(HarnessError::Watchdog)`. |
| `parse_info_line_handles_partial_fields` | `info depth 4 nodes 100` (no score, no time) → `InfoLine { depth: Some(4), nodes: Some(100), score: None, time_ms: None, pv: None }`. |
| `parse_bestmove_with_ponder` | `bestmove e2e4 ponder e7e5` → `Bestmove { uci: "e2e4", ponder: Some("e7e5") }`. |
| `parse_bestmove_no_ponder` | `bestmove e2e4` → `Bestmove { uci: "e2e4", ponder: None }`. |
| `parse_bestmove_null_move` | `bestmove 0000` → `Bestmove { uci: "0000", ponder: None }`. (Convention from M3.E aborted-fallback.) |
| `shutdown_kills_hung_real_subprocess` | Spawn `/bin/cat` (or platform equivalent) with piped stdin/stdout, send `go nodes 1000` (which `cat` will echo as a literal line), call `recv_until_bestmove(handle, 100ms)` (watchdog fires; harness calls `child.kill()`). After the call returns `Err(Watchdog)`: assert `child.try_wait()` returns `Ok(Some(_))` (kill was effective + child reaped). Then call `shutdown(handle)` and assert it returns `Ok(())` *idempotently* (no double-kill panic, no hang). Pins the watchdog→kill→reap path against real OS process state and proves the two functions don't compensate for each other's bugs. |
| `shutdown_clean_quit_path` | Spawn `/bin/cat`, call `shutdown(handle)` directly (no preceding watchdog). The driver sends `quit\n`, polls 1 s, then escalates to `kill()` because `cat` doesn't exit on `quit`. Returns `Ok(())` after reap. Pins the `quit` → `kill` escalation path. |

### 5.4 Time-forfeit + grace (`mod match_loop::tests`, ~50 LOC)

The forfeit signal is *harness-measured wallclock* (`Instant::now()` delta from `go` send to `bestmove` arrival), **not** the engine-reported `info ... time T` field. Tests use a `pure_apply_move_clock_update(...)` helper that takes synthetic `(t_go, t_bestmove)` `Instant` pairs and returns the post-move clock state — fully deterministic, no real time.

| Test | Asserts |
|---|---|
| `time_forfeit_when_wallclock_exceeds_remaining_plus_grace` | `remaining_ms=100`, harness-wallclock elapsed=200 ms, grace=50 ms → forfeit. |
| `no_forfeit_when_wallclock_within_grace_window` | `remaining_ms=100`, elapsed=130 ms, grace=50 → no forfeit (130 ≤ 100+50). |
| `no_forfeit_when_engine_lies_about_time` | `remaining_ms=100`, engine reports `info ... time 0`, harness wallclock elapsed=200 → forfeit detected (forfeit signal is harness wallclock, not engine claim). Pins the must-fix from plan-review v1. |
| `increment_credited_after_each_move` | After move with elapsed=200 ms, inc=100 → new `remaining = old - 200 + 100`. |
| `clock_arithmetic_can_go_negative_within_grace` | `remaining_ms=100`, elapsed=130 ms, grace=50 → post-move clock is `100 - 130 + inc` (negative if inc small) but no forfeit because `final_clock ≥ -grace`. |
| `time_forfeit_does_not_apply_in_nodes_mode` | When `MatchTimeMode = Nodes(N)`, `match_loop`'s forfeit-detection branch is skipped regardless of harness-measured elapsed wallclock. (The watchdog still fires on hangs — separate code path — but the per-move forfeit-detection invokes against per-side clocks that aren't tracked in nodes mode.) |

### 5.5 PGN formatter golden file (`mod pgn::tests`, ~30 LOC)

| Test | Asserts |
|---|---|
| `pgn_white_wins_startpos_formats_to_seven_tag_roster_plus_comments` | Synthetic 4-move game (`e2e4 e7e5 d1h5 e8e7 → 1-0` because Black resigned, irrelevant for the test) → emit PGN, compare to a string literal in the test (golden value inline; no separate fixture file). |
| `pgn_black_wins_with_termination_tag` | Game ending with `Termination "adjudication: insufficient material"` → tag present and value matches. |
| `pgn_setup_tag_omitted_for_startpos` | Startpos game → no `[FEN …]` / `[SetUp …]` tags. |
| `pgn_move_comment_omitted_when_lastinfo_none` | Move with `LastInfo { depth: None, … }` → no `{…}` comment after the move. |

### 5.6 `#[ignore]`-gated end-to-end smoke (~30 LOC)

```rust
#[test]
#[ignore = "spawns clawfish; opt-in via cargo test --release -- --ignored"]
fn end_to_end_clawfish_self_play_4_games() { … }
```

- Spawns the built `clawfish` binary (via `env!("CARGO_BIN_EXE_clawfish")`) twice — both sides clawfish.
- Plays 4 games (2 colour-pairs) at `--tc 1+0.05`.
- Asserts: 4 PGN files written, `summary.txt` has 4 entries, exit code 0, all games terminated by a recognized `GameOver` variant.
- This is the smoke gate. The full 200-game back-test runs after impl lands and is recorded to `docs/research/tooling-elo-harness-validation.md` (a manual step described in §11).

## 6. Order of operations

1. **Lib scaffolding (no behaviour change to engine).**
   - Create `src/match_clock.rs` with `MatchTimeMode`, `PerSideClock`, `format_go_command`, and §5.2 tests.
   - `src/lib.rs`: add `pub mod match_clock;` + re-exports.
   - Bump `is_repetition` and `is_fifty_move_draw` from `pub(crate)` → `pub` in `src/search.rs` (visibility only; signatures unchanged; existing tests still pass).
   - `cargo test --lib` after this step → green; full crate compiles unchanged behaviorally.
2. **Binary skeleton + `cli` module.**
   - Create `src/bin/elo-iterate.rs` with `mod cli` + `parse_args`.
   - `Cargo.toml` `[[bin]]` entry.
   - `cargo run --release --bin elo-iterate -- --help` works; no engines spawned yet.
3. **`adjudicate` module.**
   - Implement `detect_native_game_over` + `is_insufficient_material`.
   - All §5.1 tests written and passing.
4. **`driver` module.**
   - Subprocess spawn + reader thread + mpsc channel.
   - `EngineLine` parsing (regex-free, byte-prefix matching).
   - Watchdog `recv_timeout`.
   - All §5.3 tests written and passing.
5. **`pgn` + `summary` modules.**
   - PGN emission + per-move comment + Seven Tag Roster.
   - Golden-file tests (§5.5) passing.
6. **`match_loop` module.**
   - Color-paired game loop, per-side clock, time forfeit, integration with `driver` + `adjudicate` + `pgn`.
   - §5.4 tests passing.
7. **`main` glue.**
   - Compose `cli` → spawn → run → shutdown.
   - `#[ignore]`-gated §5.6 end-to-end smoke writes 4 PGN files and exits cleanly.
8. **Pre-review mechanical checks.**
9. **Final review loop.**
10. **Commit + push (no back-test yet).** Doc-delta atomic with the commit covers `docs/architecture.md`, ADR file, `docs/tooling/elo-iteration-harness.md`'s ELOH.A row, `docs/roadmap.md`.
11. **Manual back-test gate (post-commit).**
    - Run: `cargo run --release --bin elo-iterate -- --engine target/release/clawfish --opponent $(which stockfish) --engine-launch-prefix 'taskpolicy -c utility' --opponent-option UCI_LimitStrength=true --opponent-option UCI_Elo=1320 --tc 10+0.1 --max-games 200`.
    - Compare W/L/D to M3.F's 196-4-0; pass if within Wilson-95 % around p̂=0.98, n=200 → approximately 190–198 wins / 2–10 losses (asymmetric upper bound near 1.0).
    - Archive transcript + verdict to `docs/research/tooling-elo-harness-validation.md`.
    - On pass → ELOH.A closed; ELOH.B can begin planning.
    - On fail → debug; if a structural bug, file as a should-fix-revisit in the same commit's follow-up; if wallclock noise, document and pass.

## 7. Dependencies on other units

- **M3.F** for the deterministic `bench` command and the `taskpolicy -c utility` P-core-pinning convention. Already landed.
- **No M4 dependency.** ELOH.A and ELOH.B share no source surface with M4.A's TT branch on `src/search.rs` / `src/engine.rs` — the harness lives at `src/bin/elo-iterate.rs` plus a small lib module. Confirmed in `docs/tooling/elo-iteration-harness.md` "Sequencing relative to M4."
- **Crate API surface used:** `Position` (FEN, make_move, unmake_move, `halfmove_clock`, `side_to_move`, `zobrist`), `Move::from_uci`, `Move::to_uci`, `MoveList`, `generate_moves`, `in_check`, `Color`, `is_repetition` and `is_fifty_move_draw` (newly `pub`).

## 8. Parallelization map

The plan's eight-stage order has internal sequencing constraints, but several stages are independent and can run on parallel coding agents.

**Sequential bottleneck:** stages 1 → 2. After stage 2, stages 3, 4, 5 are mutually independent (different modules, no shared types beyond what stage 2 lands).

**Parallelizable slices** (after stage 2 lands):
- **Slice A** — `adjudicate` module + §5.1 tests. Self-contained; only depends on crate `Position` + movegen. ~150 LOC including tests. **Sonnet** suffices (prescriptive); no plan-flagged judgment calls.
- **Slice B** — `driver` module + §5.3 tests. Depends on `std::process::Command`, `mpsc`. ~180 LOC including tests. **Sonnet**: workflow precedent from M2.C reader-loop (similar pattern). Edge cases (EOF, watchdog) are spec'd.
- **Slice C** — `pgn` + `summary` modules + §5.5 tests + `MatchTimeMode` tests at §5.2. ~120 LOC. **Sonnet**.

After A+B+C land in any order:

- **Slice D** — `match_loop` integrating A+B+C, plus §5.4 tests. ~120 LOC. **Sonnet**: prescriptive plan, no novel invariants.
- **Slice E** — `main` glue + `#[ignore]`-gated smoke. ~50 LOC. **Sonnet**.

D and E are sequential (E depends on D's match-loop entrypoint). Stages 8–11 are orchestrator-side.

**Decision: spawn slices A, B, C in parallel via three coder agents after stage 2; sequence D and E afterwards.** Honest dependency: `match_loop` needs `driver`'s `EngineHandle` type and `adjudicate`'s `GameOver` enum, so parallelism is real for A+B+C but not for D.

## 9. Risk register

- **Subprocess shutdown races.** If `quit` doesn't terminate the child within 1 s, the harness sends `kill()`. Test: §5.3's `recv_until_bestmove_watchdog_fires` exercises the kill path on a slow/dead child.
- **Reader-thread panic on malformed UTF-8.** UCI guarantees ASCII; defensive read uses `BufReader::lines()` which surfaces UTF-8 errors as `io::Error`. Reader thread translates to `Err`-variant on its mpsc channel; main thread treats as `Eof`.
- **Insufficient-material edge cases.** The KBKB-same-colour case is the trickiest: bishop colour from square parity (light = `(file+rank) & 1 == 0`). Test fixture pins this.
- **Color-pair semantics around `--max-games`.** Odd input is a CLI error; even is unambiguous. fastchess's `-rounds N -repeat` is `2N` games; we use `--max-games` directly to avoid the `rounds vs games` ambiguity.
- **PGN move-format consumer compatibility.** UCI long-algebraic in PGN is non-standard and will not import into lichess/ChessBase/scid. Mitigation: documented as archival-only in §3.5 and in the back-test validation note (`docs/research/tooling-elo-harness-validation.md`). SAN can be added later without breaking already-archived ELOH.A PGN files (extending the formatter is forward-compatible; existing UCI-only files remain inspectable, new files are SAN).
- **Wallclock noise affecting the back-test gate.** The Wilson-95 % interval around p̂ = 196/200 = 0.98 is approximately [0.951, 0.992] → ~190–198 wins, ~2–10 losses out of 200 (asymmetric because the upper bound is near 1.0). The interval is wide enough to absorb thermal-state variance. If the harness reports e.g. 188 W on a hot run, re-run before flagging a structural bug.
- **`is_repetition` / `is_fifty_move_draw` visibility bump.** Bumping `pub(crate)` → `pub` exposes search-internal helpers as crate-stable API. Trade-off: (a) bump-only is the lighter touch, ~0 LOC churn; (b) moving the helpers to a neutral location (e.g. `src/draw.rs` or attaching them to `Position`) is architecturally cleaner but introduces M4-period churn. **Decision: bump only.** Future search refactors that want to mutate these signatures freely can move them at that point. The ADR notes this as deliberate (not a refactor commitment).
- **Asymmetric TC handling.** When `--opponent-tc-override` is set, the harness maintains a separate `PerSideClock` per *engine* (each carrying its own engine's TC's increment), and at `go`-time looks up which engine plays which colour to populate `Wallclock.format_go_command(white, black)`. Per-recipient wire string is identical within a game (UCI clocks are colour-absolute), but engines see different clocks across games as colour assignment swaps. §5.2 `wallclock_asymmetric_increments` pins the formatter; the integration in `match_loop` is exercised by §5.6 e2e smoke (clawfish-vs-clawfish at symmetric TC; asymmetric-TC integration deferred to a smoke probe in ELOH.B).

## 10. ADR sketch — driving model

`docs/decisions/00XX-eloh-harness-driving-model.md` (slot at landing). The 9-item sketch below enumerates load-bearing decisions; at landing it may split into one ADR + a `docs/architecture.md` "settled commitments" row, OR (less likely) two ADRs (one for the wire-protocol/clock model, one for the adjudication model). Decided at landing time. The sketch captures:

1. **Wallclock TC for ELOH.A** — `Instant::now()`-measured per-move elapsed, deducted from per-side accumulators; harness-overhead grace; **forfeit signal is harness wallclock, not engine-reported `time` field**.
2. **Subprocess lifecycle** — spawn-once-per-run; reader thread per engine + main-thread `sync_channel(1024)` consumer; `quit` then 1 s wait then `kill` shutdown sequence; `Drop` on `EngineHandle` best-effort kills on panic.
3. **`MatchTimeMode { Wallclock, Nodes(u64) }` seam** — load-bearing for ELOH.C's `--go-nodes` work (one of two harness-side ELOH.C scope items; `VirtualClock` UCI option negotiation is a separate code path through the handshake state machine that the seam does *not* aid). Lib-side at `src/match_clock.rs` for reusability and unit-testability.
4. **Visibility bump rather than module move** for `is_repetition` / `is_fifty_move_draw` — chosen for minimal churn at ELOH.A; future refactor can relocate.
5. **FIDE-correct threefold-repetition for adjudication** — separate harness-side helper `is_threefold_repetition_for_adjudication(history)` requiring 3 occurrences (current + 2 prior); the search helper's 1-prior-occurrence shortcut is a search heuristic, not a tournament rule.
6. **PGN move format: UCI long-algebraic, archival use only** — explicitly non-portable to lichess/ChessBase/scid; SAN deferred to a later phase. No invented `[Notation "UCI"]` tag.
7. **Engine launch prefix** — split-on-whitespace prefix prepended to argv; replaces `scripts/elo-iterate.sh`'s wrapper-script trick.
8. **Watchdog scales with TC** — `max(60_000, 2*tc.initial_ms + 30_000)` ms; configurable via `--watchdog-ms` for non-default cases.
9. **Spawn-once contract validated by preflight probe.** Stockfish 18 honours mid-session `setoption UCI_Elo` (probe doc cited).

## 11. Doc-delta — atomic with ELOH.A landing

- New ADR file (slot at landing) — see §10.
- `docs/tooling/elo-iteration-harness.md` — ELOH.A row → done (with actual landing size); ELOH.A scope detail moved to "Done" prose (or kept as historical reference; decided in commit).
- `docs/architecture.md` — add row for the harness binary + match-clock lib module.
- `docs/roadmap.md` — note ELOH milestone in parallel with M4.
- `Cargo.toml` — `[[bin]]` entry for `elo-iterate`.

After manual back-test:
- `docs/research/tooling-elo-harness-validation.md` — created with back-test transcript + W/L/D + verdict.

## 12. Verification checklist (per workflow.md chess-coder discipline)

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --release` — green; `--ignored` smoke runs separately.
- `cargo llvm-cov --summary-only --lib --release` — surface to reviewer.
- `cargo mutants --in-diff` against the unit's diff — surface survivors + classifications.

The final-review loop reads code + tests + this plan + the orchestrator's mechanical-check analysis.
