# ELOH — Elo-iteration harness milestone

Tooling milestone (not strength). Replaces the `fastchess` + `bash` stack at `scripts/elo-iterate.sh` for online rating-estimation runs, then adds hardware-invariant time control. Three sub-phases — ELOH.A, ELOH.B, ELOH.C — each gated by its own plan + tests + back-validation before the next is planned.

ELOH.A and ELOH.B land on `tooling/elo-harness` branch (worktree at `/Users/alex/clawfish-elo-harness`); ELOH.C lands on a separate branch covering both engine and harness changes for the `VirtualClock` / `--go-nodes` capability — see "Branches and worktrees" below.

**Lineage.** Original case in [`docs/tooling-backlog.md`](../tooling-backlog.md) — "Custom in-process Elo-iteration harness" + "Hardware-invariant TC: `go nodes` mode + `VirtualClock` UCI extension." This doc consolidates both backlog entries into a phased plan; entries are moved to "Done" at the relevant phase landing.

## Why now

- **Per-sub-milestone rating-estimate cadence.** With each M4.* (and M5.*) sub-milestone consuming one rating estimate as its forward-validation, the backlog entry's "10-run payback" threshold is reached inside M4 alone.
- **Hardware-invariant TC (ELOH.C)** addresses the wallclock-noise concern surfaced during M3.F: results currently couple to thermal state, background load, and scheduler decisions. Defers the engine-side change until the harness exists, so ELOH.C layers cleanly into ELOH.A/B's `--go-nodes` + engine-side `VirtualClock`.

## ELOH-vs-M4 dividing line

- **ELOH** is the *measurement infrastructure* — how rating estimates are produced.
- **M4** is the *strength change* — what those estimates measure.
- Each M4.* sub-milestone consumes one ELOH-driven rating estimate as forward-validation. ELOH.A and ELOH.B's *back*-validation gate is M3.F's existing rating-estimate measurement (a known-answer replay analogous to perft for the rules layer).

## Exit criteria (milestone-level)

- All three sub-phases pass their respective back-validation gates (per-phase detail below).
- `scripts/elo-iterate.sh` removed or replaced with a thin wrapper; rating-estimation runs go through `cargo run --release --bin elo-iterate -- ...`.
- `docs/tooling-backlog.md` entries for both items moved to "Done" at the relevant phase landing — ELOH.B closes the harness entry; ELOH.C closes the hardware-invariant-TC entry.
- M4.A's first rating estimate uses the harness end-to-end as forward-validation.

## Status

No external research pending — UCI protocol is well-documented (`docs/reference/uci-protocol-2017-04-26.txt`); clawfish's own UCI handling (`src/engine.rs`, `parse_uci_line`) provides the parsing reference; `CLOCK_THREAD_CPUTIME_ID` semantics are well-documented in POSIX/Darwin man pages.

**Pre-ELOH.A preflight probe (required, ~1 hour manual work).** Stockfish 18's behavior under mid-session `setoption name UCI_Elo value <new>` (without an intervening process restart) is unprobed — the bash version always spawns fresh Stockfish processes per fastchess invocation, so the harness model is empirically untested at this exact path. The probe runs *before ELOH.A is planned* (not before ELOH.B) so that ELOH.A's spawn-once-vs-spawn-per-pair contract is decided up front, not retroactively after ELOH.A has landed. Procedure: spawn Stockfish, set `UCI_Elo=1320`, play one game, set `UCI_Elo=1500`, play another, verify the second game's strength is consistent with the new setting (e.g. via per-move score tracking). Result archived to `docs/research/tooling-stockfish-mid-session-setoption.md`. If the probe fails (Stockfish ignores the change or requires `ucinewgame` between settings), ELOH.A's spawn-once contract becomes spawn-per-game-pair, and ELOH.A's match-loop budget grows by ~30 LOC for the spawn-per-pair lifecycle.

## ADRs likely to bind per-phase

- **Harness driving model — wallclock TC + match-loop time-source seam.** Likely binds on **ELOH.A**. Open: time source for ELOH.A (wallclock); subprocess lifecycle (spawn-once-per-run, killed at run-end). The seam abstraction (`MatchClock` / `format_go_command` indirection so ELOH.C can swap to nodes-mode without ELOH.A refactor) is load-bearing for ELOH.C's budget; ADR commits to the seam shape even though only the wallclock variant is implemented in ELOH.A. Default in plan: wallclock + spawn-once + a `MatchTimeMode { Wallclock, Nodes(u64) }` enum threaded through the match loop's `go`-formatting and clock-tracking paths.
- **Concurrency primitive.** Binds on **ELOH.B** (where concurrency is actually implemented and exercised). Open: `std::thread` vs. `tokio` vs. `rayon`. Default in plan: `std::thread` + `std::sync::mpsc` (no async dependency for a workload that's pure blocking subprocess I/O).
- **`VirtualClock` UCI option semantics + `--go-nodes` harness mode.** Binds on **ELOH.C** (single ADR covering both since the engine and harness sides are tightly coupled). Open: time-source primitive (`clock_gettime(CLOCK_THREAD_CPUTIME_ID)` POSIX vs. `mach_thread_info(THREAD_BASIC_INFO)` Darwin) — both surfaces are platform-specific and need an `#[cfg(target_os)]` shim; mate-distance interaction (presumably none at the algorithmic level); `MoveOverhead` semantics under CPU-time (still milliseconds, but of CPU time, not wallclock); whether the substitution is a runtime check or a generic over a `TimeSource` trait monomorphized at search entry; degenerate `MoveOverhead` semantics under `VirtualClock=true` (the wallclock-jitter hedge is partially meaningless under CPU-time TC).
- **Adjudication thresholds (resign/draw/maxmoves).** Possibly binds on **ELOH.B** (where they're scoped — see scope detail). Open: match `scripts/match.sh` defaults exactly vs. expose as CLI flags. Default in plan: both — match defaults *and* expose as flags.

ADR numbers allocated at landing time. ELOH.B may land without an ADR if plans expose only parameter-level decisions on adjudication thresholds; the time-source ADRs at ELOH.A and ELOH.C are likely binding.

## Sub-phases

| Phase | Scope | Approx size |
|---|---|---|
| **ELOH.A** ✓ — Harness foundation | Persistent subprocesses + UCI driver + native adjudication (mate/stalemate/50-move/FIDE-3-fold/insufficient-material) + per-side clock management + match-loop time-source seam + color-paired fixed-batch match loop. Plays N games against a fixed-config opponent; emits per-game PGN + summary. No K-update, no σ-stopping, no concurrency, no threshold adjudication. **Landed 2026-04-29.** ADR-0020; plan: `docs/plans/eloh.a.md`. Actual landing size: ~2300 LOC binary + 80 LOC lib seam (`src/match_clock.rs`) + 51 in-tree tests passing. Back-test gate result archived in `docs/research/tooling-elo-harness-validation.md` (lands as follow-up commit once the manual back-test completes). | ~500 (actual ~2300; integration glue across 7 submodules — driver/cli/adjudicate/pgn/summary/match_loop/main — overshot the plan's bin estimate) |
| **ELOH.B** ✓ — Online iteration + concurrency + progress + threshold adjudication | Robbins-Monro K-update + σ-stopping + concurrency (N parallel game-pairs) + convergence-progress output + resign/draw/max-moves threshold adjudication. Replaces `scripts/elo-iterate.sh` (thin wrapper) and `scripts/sprt.sh rating-estimate` arm end-to-end. **Landed 2026-04-30.** Plan: `docs/plans/eloh.b.md`. Actual landing size: ~5917 LOC total binary (ELOH.A+B combined) + 139 in-tree tests passing. Back-test gate (Part 1: online run vs M3.F ~2114 Elo; Part 2: synthetic Bernoulli σ-stopping) deferred to post-merge manual run. | ~150 binary + ~80 tests (actual ~650 new LOC across `estimator`, `sigma`, `progress`, `adjudicate` extension, `match_loop` extension, `controller`, `main` rewrite) |
| **ELOH.C** — Hardware-invariant TC | Harness `--go-nodes N` flag (fills the seam ELOH.A leaves) + clawfish `VirtualClock` UCI option (`option name VirtualClock type check default false`) substituting `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` for `Instant::now()` in `compute_caps`. Single ADR for the time-source decision covering both sides. | ~70 harness + ~80 engine + ~80 tests |

ELOH.A → B → C is sequential. Each phase's back-validation gate must pass before the next is planned.

**Decision rationale on the 3-phase split.** A reviewer-suggested alternative was to collapse A and B into a single "harness" phase, matching the original two backlog entries 1:1. Rejected here because A's back-validation gate — correct W/L/D against M3.F's saturating 1320-anchor match — is cleanly separable from B's statistical-layer validation, and a per-phase plan-mode pass is shorter than one combined pass on ~650 LOC. The 3-phase shape costs slightly more workflow ceremony (one extra plan + tests + review cycle) and earns a clean correctness-vs-statistics-vs-environment decomposition that maps to three independently testable layers.

## ELOH.A scope detail

ELOH.A is the harness's **correctness layer**: it produces a working in-process match runner against a fixed-config opponent, validating that the UCI driver and native adjudication produce results matching `fastchess`'s behavior on a known-answer replay. None of the online-iteration statistical machinery lives here — ELOH.A's purpose is to prove the harness can play a game correctly before ELOH.B trusts it for K-updates.

**In scope.**
1. **Persistent UCI subprocess management.** Spawn clawfish + Stockfish once via `std::process::Command` with stdin/stdout pipes; keep alive across all batches; `quit` + 1s SIGKILL fallback on shutdown. **`--engine-launch-prefix CMD`** flag to inject `taskpolicy -c utility` (or equivalent) ahead of the engine binary, replicating `scripts/elo-iterate.sh`'s P-core-pinning wrapper-script trick (lines 101–108) without external scripts.
2. **UCI driver (FROM-engine direction).** Hand-built `uci`/`isready`/`setoption`/`ucinewgame`/`position fen ... moves ...`/`go wtime/btime/winc/binc`. `bestmove` parsed via `parse_uci_line`. Reuses crate `Position`, movegen, `is_repetition`, `is_fifty_move_draw` for harness-side board state.
3. **Adjudication — native only (no thresholds).** Mate, stalemate, 50-move (uses `is_fifty_move_draw`), 3-fold (uses `is_repetition`), insufficient material (KK / KBK / KNK / KBKB-same-color, with negative case at K+N-vs-K+N which is *not* insufficient by FIDE).
4. **Per-side clock management.** `wtime/btime/winc/binc` per UCI; `Instant::now()` measurement; time-forfeit on overflow with PGN result tag; harness-side grace window absorbs pipe latency (default 50ms, configurable via `--harness-overhead-ms`).
5. **Match-loop time-source seam.** `MatchTimeMode { Wallclock, Nodes(u64) }` enum threaded through the match loop's `go`-formatting and clock-tracking paths. ELOH.A implements only the `Wallclock` variant; the `Nodes(u64)` variant is unconstructible from CLI in ELOH.A but the type exists. ELOH.C fills in the variant without a structural refactor. **Invariant:** the wallclock pipe-watchdog (a generous ~60s timeout that kills hung subprocesses) stays active in *all* modes; only the per-game UCI time-forfeit logic becomes unreachable when `MatchTimeMode = Nodes(N)`. This keeps ELOH.A's responsibility split stable across ELOH.C's variant addition. Roughly 30 LOC of plumbing in ELOH.A; pays back in ELOH.C's budget.
6. **Color-paired match loop.** Mirror fastchess `-repeat` semantics — within a "pair," game N+1 plays inverse colors of game N (same opening position if applicable). Pair counts as 2 games for `--max-games N`. (M3.F's 200-game match used `-repeat`; replicating the back-test requires color-pair support in ELOH.A.)
7. **Fixed-batch match loop.** Plays `--max-games N` (in games, not pairs) against a fixed-config opponent; emits per-game PGN at `target/elo-iterate/<run-id>/games/<game-index>.pgn` (Seven Tag Roster + per-move `{depth N score cp X time T}` comments where the engine reports them); appends per-game line to `summary.txt`.
8. **PGN per-move comment policy.** Take the line whose `depth` matches the engine's final iteration before `bestmove` — i.e., the last `info depth N` line emitted before `bestmove`. Documented in plan.
9. **CLI surface (subset).** `--engine PATH`, `--engine-launch-prefix CMD`, `--opponent PATH`, `--opponent-launch-prefix CMD`, `--tc TC`, `--opponent-tc-override TC`, `--max-games N`, `--out-dir PATH`, `--harness-overhead-ms N`, `--seed N`. Sensible defaults from `scripts/match.sh`.

**Deferred to ELOH.B.** K-update, σ-stopping, concurrency, mid-run opponent reconfiguration, convergence-progress output, **threshold adjudication (resign/draw/max-moves)**.

**Deferred to ELOH.C.** `--go-nodes` mode (the `Nodes(u64)` variant); `VirtualClock` UCI option negotiation.

**Open questions (resolved at ELOH.A plan landing).**
- Verification surface for the back-test: smoke integration test (`#[ignore]`-gated) + manual full back-test recorded in `docs/research/tooling-elo-harness-validation.md`.
- Process-pair core pinning at concurrency=1: handled via `--engine-launch-prefix taskpolicy -c utility` (in scope item 1).
- Opening positions: same set as `scripts/elo-iterate.sh` (confirm in plan from current bash invocation; likely startpos-only at concurrency=1).
- Whether the `MatchTimeMode` enum lives in `src/bin/elo-iterate.rs` or in a small `src/match_clock.rs` module under the crate library: decided in plan; lib-module is preferable for reusability if M5+ tooling needs the same abstraction.

**Back-validation gate.** Run a fixed-batch match (no online iteration; fixed UCI_Elo=1320 on Stockfish; `--engine-launch-prefix 'taskpolicy -c utility'`; `--max-games 200`; `-repeat` color-pair semantics) replicating M3.F's *single 1320-anchor* fixed match. Pass: harness reproduces W/L/D within Wilson-95% interval of M3.F's 196-4-0 (CI roughly 192–199 wins out of 200, 1–8 losses out of 200). Result + log archived to `docs/research/tooling-elo-harness-validation.md` (created at ELOH.A landing). The CI is wide enough that wallclock noise (thermal state at run time, no hardware-invariant TC yet) doesn't spuriously fail the gate; tight enough that a buggy adjudication or UCI driver fails it.

**Doc-delta (atomic with ELOH.A landing).**
- `docs/tooling/elo-iteration-harness.md` — ELOH.A row marked done; ELOH.A scope detail moved to "Done" prose noting the actual landing size.
- `docs/research/tooling-elo-harness-validation.md` — created with ELOH.A back-test result.
- `Cargo.toml` — `[[bin]]` entry for `elo-iterate`; `[[bin]]` may also pull in a small `src/match_clock.rs` lib module if the seam decision lands lib-side.

## ELOH.B scope detail

ELOH.B is the harness's **statistical layer**: it adds the K-update, σ-stopping, concurrency, and threshold adjudication that make the harness suitable for online rating-estimation runs. The K-update math and σ-stopping logic are unit-testable in isolation; the integrated behavior is back-validated against M3.F's online-iteration result.

**In scope.**
1. **Online Robbins-Monro K-update at single-game cadence.** `K_t = K_0 / (1 + t/τ)` with `K_0` and `τ` as CLI flags. Estimate update `Elo_{t+1} = Elo_t + K_t · (S − E)` where `S ∈ {0, ½, 1}` is the game result and `E` is the expected score from the current Elo gap.
2. **Mid-run opponent reconfiguration.** `setoption name UCI_Elo value <new>` between games; no process restart. Reconfigures only the opponent (anchor moves to track the current estimate); clawfish-side options stay fixed. Contingent on the pre-ELOH.B preflight probe (see "Status") confirming Stockfish honors mid-session `setoption UCI_Elo` without `ucinewgame`. If the probe fails, fall back to spawn-per-game-pair with documented overhead.
3. **σ-based stopping criterion.** Trailing-window σ over `--stop-window` recent estimates; terminate when σ < `--target-sigma` for `--stop-window-confirm` consecutive games (anti-flap). Falls back to `--max-games` cap. **Default values:** `--target-sigma 30 --stop-window 30 --stop-window-confirm 5`, calibrated against M3.F's empirical convergence curve (σ ≈ 35 at 120 games).
4. **Threshold adjudication.** Resign (consecutive `movecount` moves with engine-reported score below `-resign-score`), draw (consecutive `movecount` moves past `movenumber` with both engines' |score| below threshold), max-moves cap. Defaults from `scripts/match.sh`: `resign movecount=3 score=600`, `draw movenumber=34 movecount=8 score=20`, `maxmoves=200`. Exposed as CLI flags (`--resign-movecount`, `--resign-score`, `--draw-movenumber`, `--draw-movecount`, `--draw-score`, `--max-moves`).
5. **Concurrency.** N parallel games via `std::thread` (per ADR-binding default); each game owns its own subprocess pair; results merge into central K-updater via `std::sync::mpsc`. K-updater is single-threaded and processes results in arrival order; the Robbins-Monro schedule is robust to arrival-order variance.
6. **Convergence-progress output.** Once per completed parallel-game batch (where a "batch" is the round-robin grouping of `--concurrency` games that complete and feed into the K-updater together), emit a single stdout line: `progress: t=<step> games=<G> elo=<current> sigma=<trailing-σ> K=<K_t> wlD=<W>-<L>-<D>`. Format parseable by `awk` for downstream automation. Final line on convergence: `converged: elo=<final> sigma=<final-σ> games=<G> reason=<sigma|max-games>`. Same line appended to `summary.txt`.
7. **CLI surface additions.** `--initial-elo N`, `--k0 F`, `--tau F`, `--target-sigma F`, `--stop-window N`, `--stop-window-confirm N`, `--concurrency N`, plus the threshold-adjudication flags from item 4. (`--max-games` from ELOH.A becomes the fallback cap; semantics stay "in games, not pairs" since color-pair support landed in ELOH.A.)
8. **`scripts/sprt.sh rating-estimate` reroutes** through the new binary.

**Deferred to ELOH.C.** `--go-nodes` mode + `VirtualClock` honoring.

**Out of scope (deferred indefinitely).** Adaptive K from running variance; aggregate per-anchor depth instrumentation; multi-anchor regression; resume-from-checkpoint.

**Open questions (resolved at ELOH.B plan landing).**
- K-update state persistence: emit on stdout + `summary.txt`; no separate `state.jsonl`. Resume is YAGNI for M4-cadence runs.
- Asymmetric TC default: same as bash version (no asymmetric default).
- Pre-existing fastchess output compatibility: harness format documented here; downstream `awk` updated atomically with the landing commit; no compatibility shim.
- Determinism: harness is *not* deterministic across runs (parallel scheduling, opponent UCI_Elo nondeterminism). Reproducibility is at the result-distribution level. `--seed` controls only opening-position selection. Documented in `--help`.

**Back-validation gate.** Two parts — must both pass.

- **Part 1 — K-update + concurrency reproduction (deterministic-ish).** Replicate M3.F's online-iteration with `--max-games 120 --target-sigma 0` (stops on max-games, not σ). The value `0` for `--target-sigma` is treated as a sentinel meaning σ-stopping is disabled — the σ computation short-circuits and the run terminates only on `--max-games`. Documented in `--help`. Pass: convergence to within ±2σ of M3.F's ~2114 Elo (i.e. ~2044–2184). Wider than ±1σ to acknowledge irreducible wallclock noise: M3.F's environment isn't perfectly reproducible, so a stricter gate would fail spuriously on a correct harness running on a hot CPU. Result archived as a "Part 1" section in `docs/research/tooling-elo-harness-validation.md`.
- **Part 2 — σ-stopping logic (synthetic, fully reproducible).** Unit-style test: feed the K-updater a synthetic game-result stream drawn from a known noise distribution (e.g. a Bernoulli sequence with fixed `p` matching a 200-Elo gap), verify that σ-stopping triggers at the expected sample size to within a small tolerance. Validates that the stopping *decision* is correct independent of the network/Stockfish path. ~30 LOC test.

Together these gates exercise the K-update + concurrency path (Part 1, against a known answer) and the σ-stopping path (Part 2, deterministically). Either one alone is insufficient; both are required.

**Doc-delta (atomic with ELOH.B landing).**
- `docs/workflow.md` — "Online Elo iteration" subsection updated to point at the binary.
- `docs/tooling-backlog.md` — "Custom in-process Elo-iteration harness" entry moved to "Done since the 2026-04-27 review."
- `scripts/elo-iterate.sh` — replaced with thin wrapper invoking `cargo run --release --bin elo-iterate -- ...`, OR removed entirely with `scripts/sprt.sh rating-estimate` rerouted directly. Decided in plan.
- `docs/tooling/elo-iteration-harness.md` — ELOH.B row marked done.

## ELOH.C scope detail

ELOH.C is a **single coordinated change** spanning clawfish source (engine-side `VirtualClock` UCI option) and the harness binary (`--go-nodes` mode + `VirtualClock` negotiation). Lands on a single branch covering both — likely `tooling/eloh-c-hardware-invariant-tc` — since the engine and harness changes are tightly coupled (the engine option is unusable without harness negotiation; the harness `--go-nodes` mode is a separate axis but lands together for cohesion). Earlier draft suggested `engine/virtual-clock` as a pure-engine branch; revised because ELOH.C's harness-side scope makes the boundary moot.

**In scope (clawfish-side, ~80 LOC).**
1. New UCI option `option name VirtualClock type check default false`.
2. When `true`, `compute_caps` substitutes `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` (POSIX) / `mach_thread_info(THREAD_BASIC_INFO)` (Darwin) for `Instant::now()`. Platform shim via `#[cfg(target_os)]`.
3. UCI option plumbing in `Engine::handle_setoption` (mirrors `MoveOverhead`).
4. `MoveOverhead` semantics under `VirtualClock=true`: still in milliseconds, but milliseconds of CPU time. Documented in the option's UCI description string. Acknowledged degenerate case: under CPU-time TC there's no wallclock-scheduling jitter for `MoveOverhead` to hedge against, so the option becomes a small fixed-cost conservatism (engine plays slightly under budget). Harmless at the default 50ms; left as-is unless empirically problematic.
5. Test fixture pinning the option's effect: deterministic CPU-time fixture (a tight loop consuming a known number of cycles via `std::hint::black_box`) confirms `compute_caps` returns CPU-time-based deadlines when `VirtualClock=true` and wallclock-based deadlines when `VirtualClock=false`.

**In scope (harness-side, ~70 LOC).**
6. **`--go-nodes N` CLI flag (~30 LOC).** Constructs the `MatchTimeMode::Nodes(N)` variant from the seam ELOH.A landed. Harness substitutes `go nodes <N>` for `go wtime/btime/winc/binc`; per-side clock tracking is bypassed (the seam already makes this conditional); the wallclock pipe-watchdog stays active per ELOH.A's invariant. Asymmetric: per-engine via `--engine-go-nodes` / `--opponent-go-nodes` if needed (decided in plan).
7. **`VirtualClock` negotiation (~40 LOC).** Parse opponent's `option name VirtualClock ...` line from the `uci` response handshake; conditionally send `setoption name VirtualClock value true`; fall back silently when the opponent doesn't advertise the option (Stockfish's case). Integrates with the existing UCI handshake state machine.

**Out of scope.**
- PMU instruction counting (the "even sharper" follow-up in the original backlog entry). Defer until `CLOCK_THREAD_CPUTIME_ID` proves empirically inadequate.
- Cross-engine `VirtualClock` enforcement (Stockfish doesn't support it; harness simply doesn't send the option to engines that don't advertise it).

**Open questions (resolved at ELOH.C plan landing).**
- Time-source generic vs. runtime check: zero-cost generic over a `TimeSource` trait monomorphized at search entry, vs. an `is_virtual_clock` field in `compute_caps` and a runtime branch. Default plan: runtime branch (simpler; predictable; not in the inner loop).
- `MoveOverhead` reinterpretation under `VirtualClock=true`: defaults to CPU time (since the whole point is to make TC CPU-time-relative); documented explicitly.
- Mate-distance pruning interaction: M3.E's MDP is algorithmically agnostic to the time source; confirm in tests.

**Back-validation gate.** Two parts — both M4-independent, addresses earlier concern that M4.A dependency was elided.

- **Part 1 — Engine-side under load.** Clawfish-vs-clawfish SPRT under simulated CPU load (a `stress -c 4 &` background process) with `setoption VirtualClock true` on both sides. Pass: variance is meaningfully tighter than the wallclock-baseline equivalent under the same load. Quantitative target: σ of the SPRT's Elo estimator at least 30% lower than the wallclock baseline at equivalent compute budget. Result archived to `docs/research/tooling-virtual-clock-validation.md`.
- **Part 2 — Harness-side `--go-nodes` invariance (synthetic, no M4 dependency).** Run two clawfish-vs-clawfish matches on the *same* binary: one with `--tc 10+0.1`, one with `--go-nodes N` where `N` is chosen so the *median wallclock duration per game* matches the `--tc 10+0.1` baseline on the test machine. Calibration recipe: run a 10-game `--tc 10+0.1` calibration match first; record median per-game wallclock; pick `N` such that a follow-up 10-game `--go-nodes N` calibration match has the same median wallclock to within 5%. (Picking `N` from `bench` nps is *not* sufficient — node-count-per-game varies with position complexity, so the calibration must be empirical at the match level.) Then run the two 200-game matches at the calibrated `N`. Pass: result distributions overlap (W/L/D within Wilson-95% CI of each other). Validates that `--go-nodes` mode produces statistically equivalent results to wallclock mode on identical engines at comparable compute. Independent of M4.A's existence.

The earlier "replay M4.A's TT-vs-no-TT estimate with `--go-nodes`" forward-validation is *opportunistic*, not a gate — when M4.A lands and uses `--go-nodes` for forward-validation, that's a useful signal but not a precondition for ELOH.C closure.

**Doc-delta (atomic with ELOH.C landing).**
- New ADR — `VirtualClock` UCI option + `--go-nodes` harness mode (number allocated at landing).
- `docs/tooling-backlog.md` — "Hardware-invariant TC" entry moved to "Done."
- `docs/architecture.md` — small update if `compute_caps`'s time-source abstraction warrants (likely a ~3-line note in the Search-v1 subsection).
- New: `docs/research/tooling-virtual-clock-validation.md` — engine-side back-test result.
- `docs/tooling/elo-iteration-harness.md` — ELOH.C row marked done; ELOH milestone closed.

## Branches and worktrees

- **ELOH.A and ELOH.B.** Branch `tooling/elo-harness`, worktree `/Users/alex/clawfish-elo-harness`.
- **ELOH.C.** Single branch covering both engine and harness changes, default name `tooling/eloh-c-hardware-invariant-tc`. Branched off `main` after ELOH.B has merged so the harness-side seam is in place. Worktree path TBD at ELOH.C plan time.

## Sequencing relative to M4

ELOH.A → B → C sequential. Parallel with M4.A (ELOH.A and ELOH.B share no source surface with M4.A's TT branch on `src/search.rs` / `src/engine.rs` — the harness lives at `src/bin/elo-iterate.rs` plus a possible `src/match_clock.rs` lib module). M4.A consumes ELOH.B's output once ELOH.B is merged to main; if ELOH.B misses the M4.A measurement window, M4.A's first rating estimate falls back to the bash version (no schedule risk to M4 itself). ELOH.C has no M4 dependency — it lands when wallclock variance empirically warrants, independent of M4 phase progression. M4.A is *not* a precondition for ELOH.C closure.

## Sizing

- Total ELOH milestone: ~720 LOC harness + ~80 LOC engine + ~310 LOC tests, decomposed across three phases as listed. ELOH.A is the largest at ~500 + 150 LOC — at the upper end of the workflow's typical-unit target (300–800), but cleanly below the ceiling.
- **ELOH.A binary breakdown (~500 LOC).**
  - Adjudication (~150 LOC): insufficient-material variants alone are non-trivial, including the K+B-vs-K+B-same-color case and the K+N-vs-K+N negative case, plus mate / stalemate / 50-move / 3-fold dispatch.
  - UCI driver state machine (~120 LOC): handshake, options parse, command dispatch, `bestmove` / `info` line parsing on the FROM-engine direction.
  - Color-paired fixed-batch match loop (~80 LOC): per-game state, position threading, color swap within pair, summary aggregation.
  - PGN emission (~50 LOC): Seven Tag Roster + per-move comment formatting per the comment-policy decision.
  - Subprocess lifecycle (~50 LOC): spawn, pipe wiring, `quit` + SIGKILL fallback, watchdog hookup, panic-time zombie cleanup.
  - Per-side clock management (~30 LOC): `wtime/btime/winc/binc` accounting, time-forfeit detection, harness-overhead grace.
  - Match-loop time-source seam (~30 LOC): `MatchTimeMode` enum + plumbing through the `go`-formatting and clock-tracking paths.
  - CLI parsing + run-id / out-dir setup (~30 LOC).
- **ELOH.A test breakdown (~150 LOC).** Adjudication unit tests dominate (~80 LOC across all material configurations); UCI driver mock-pipe tests (~30 LOC); time-forfeit + grace tests (~20 LOC); PGN-formatter golden-file test (~20 LOC).
- **ELOH.B breakdown (~150 + 80 LOC).** K-update + Robbins-Monro schedule (~30); σ-stopping (~30); concurrency + mpsc plumbing (~40); convergence-progress formatter (~20); threshold adjudication (~30). Tests: K-update math (~20); σ-stopping incl. synthetic Bernoulli back-test gate (~30); threshold adjudication (~20); progress-format golden file (~10).
- **ELOH.C breakdown (~70 harness + ~80 engine + ~80 tests).** Per the ELOH.C scope detail above.
- **Per-phase contingency: +30% à la M3.D / M3.E.** ELOH.A's largest tail risks are subprocess lifecycle (shutdown races, broken-pipe recovery on engine crash) and adjudication edge cases (insufficient-material variants in test discovery). ELOH.C's tail risk is the platform-shim work between POSIX and Darwin time-source primitives. The spec accommodates ELOH.A landing closer to ~650 LOC if subprocess-driver edge cases eat budget.

## ELOH.B's role in M4.A measurement (precision caveat)

**Acknowledged structural concern:** ELOH.B's wallclock-based harness has back-validation tolerance ±2σ ≈ ±70 Elo (per Part 1 gate above) — the same order of magnitude as M4.A's expected ~50 Elo TT delta from the literature. ELOH.B is therefore **borderline-precise** for M4.A's forward-validation when used in wallclock mode. Three possible mitigations, picked at M4.A measurement time:

1. Accept the noise — record M4.A's wallclock-noisy estimate; re-measure under `--go-nodes` once ELOH.C lands. M4.A's SPRT (the primary gate) is wallclock-robust because SPRT compares two binaries on the same hardware in the same session; the rating estimate is documentation-grade, not gate-grade.
2. Run the M4.A rating-estimate at a longer TC (e.g. `tc=30+0.3`) where wallclock variance is proportionally smaller relative to the engine's compute budget.
3. Defer M4.A's rating-estimate forward-validation until ELOH.C lands. Adds milestone-coupling — typically undesirable, but acceptable here if ELOH.C is already near.

**Plan of record (user decision, 2026-04-29):** mitigation 1 immediately — M4.A's first rating estimate runs under wallclock ELOH.B and is documented as wallclock-noisy. **Likely mitigation 3 once ELOH.C is on the near horizon** — at that point defer rather than re-measure, since the wallclock number is throwaway anyway. The crossover point (when ELOH.C is "near enough") is decided in the moment, not pre-committed.

The milestone-level exit criterion at the top of this doc says "M4.A's first rating estimate uses the harness end-to-end as forward-validation" — that commitment stands under mitigation 1, with the resulting Elo number documentation-grade and carrying a footnote pointing at this precision caveat.
