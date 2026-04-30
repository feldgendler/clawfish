# ELOH — Elo-iteration harness milestone

Tooling milestone (not strength). Replaces the `fastchess` + `bash` stack at `scripts/elo-iterate.sh` for online rating-estimation runs, then adds hardware-invariant time control, then adds per-game TC sampling for mixed-TC SPRT. Four sub-phases — ELOH.A, ELOH.B, ELOH.C, ELOH.D — each gated by its own plan + tests + back-validation before the next is planned.

ELOH.A and ELOH.B land on `tooling/elo-harness` branch (worktree at `/Users/alex/clawfish-elo-harness`); ELOH.C and ELOH.D each land on their own branch — see "Branches and worktrees" below.

**Lineage.** Original case in [`docs/tooling-backlog.md`](../tooling-backlog.md) — "Custom in-process Elo-iteration harness" + "Hardware-invariant TC: `go nodes` mode + `VirtualClock` UCI extension." This doc consolidates both backlog entries into a phased plan; entries are moved to "Done" at the relevant phase landing.

**Lineage extension (2026-04-30).** ELOH.D added as a fourth sub-phase covering per-game TC sampling. Surfaced during the M4.D walkthrough — the M4.C empirical result (fast-TC `tc=10+0.1` SPRT inconclusive at +3.47 ± 5.88; slow-TC `tc=60+0.6` load-bearing at +35.74 ± 27.51) showed that fast-TC-only SPRT can be blind to M4-class changes whose Elo amplifies with depth. The methodological response is to redefine "the game" as "draw TC from a distribution D, then play standard chess at that TC" — i.i.d. holds again under the mixed game, SPRT applies, and the same data also supports a per-(TC, outcome) regression for the Δ(TC) curve. ELOH.D is the harness-side enabler. The companion entry "Per-game TC sampling for mixed-TC SPRT" in `docs/tooling-backlog.md` (a sub-bullet on the harness item, item 8) is what ELOH.D operationalises.

## Why now

- **Per-sub-milestone rating-estimate cadence.** With each M4.* (and M5.*) sub-milestone consuming one rating estimate as its forward-validation, the backlog entry's "10-run payback" threshold is reached inside M4 alone.
- **Hardware-invariant TC (ELOH.C)** addresses the wallclock-noise concern surfaced during M3.F: results currently couple to thermal state, background load, and scheduler decisions. Defers the engine-side change until the harness exists, so ELOH.C layers cleanly into ELOH.A/B's `--go-nodes` + engine-side `VirtualClock`.
- **Mixed-TC SPRT (ELOH.D)** addresses the fast-TC-blindness concern surfaced during M4.C SPRT and formalised in the M4.D walkthrough. M4.D's mixed-TC width-tune campaign is the first consumer; subsequent M4+/M5+ phases that rely on TC-amplifying mechanisms (history, aspiration, depth-conditional pruning) all benefit. The discrete-TC approximation that M4.D's plan currently runs against `fastchess` (4 fixed-TC matches with manually unioned games) is replaced by true per-game TC sampling once ELOH.D lands; M4.D's downstream analysis tooling stays unchanged.

## ELOH-vs-M4 dividing line

- **ELOH** is the *measurement infrastructure* — how rating estimates and SPRT verdicts are produced.
- **M4** is the *strength change* — what those estimates / verdicts measure.
- Each M4.* sub-milestone consumes one ELOH-driven rating estimate as forward-validation. ELOH.A and ELOH.B's *back*-validation gate is M3.F's existing rating-estimate measurement (a known-answer replay analogous to perft for the rules layer).
- ELOH.D's first real consumer is M4.D's mixed-TC width-tune campaign (per-game TC sampling replaces M4.D's discrete 4-TC sweep approximation). ELOH.D is *not* a precondition for M4.D — M4.D can run the discrete approximation against `fastchess` if ELOH.D hasn't merged when M4.D plan-mode starts.

## Exit criteria (milestone-level)

- All four sub-phases pass their respective back-validation gates (per-phase detail below).
- `scripts/elo-iterate.sh` removed or replaced with a thin wrapper; rating-estimation runs go through `cargo run --release --bin elo-iterate -- ...`.
- `docs/tooling-backlog.md` entries moved to "Done" at the relevant phase landing — ELOH.B closes the harness entry's main body, ELOH.C closes the hardware-invariant-TC entry, ELOH.D closes the per-game-TC-sampling sub-bullet (item 8 of the harness entry).
- M4.A's first rating estimate uses the harness end-to-end as forward-validation.
- ELOH.D's first real consumer is M4.D's mixed-TC width-tune campaign — no hard milestone-level commitment beyond ELOH.D's own back-validation gate, but the consumer relationship is documented in M4.D's plan.

## Status

No external research pending — UCI protocol is well-documented (`docs/reference/uci-protocol-2017-04-26.txt`); clawfish's own UCI handling (`src/engine.rs`, `parse_uci_line`) provides the parsing reference; `CLOCK_THREAD_CPUTIME_ID` semantics are well-documented in POSIX/Darwin man pages.

**Pre-ELOH.A preflight probe (required, ~1 hour manual work).** Stockfish 18's behavior under mid-session `setoption name UCI_Elo value <new>` (without an intervening process restart) is unprobed — the bash version always spawns fresh Stockfish processes per fastchess invocation, so the harness model is empirically untested at this exact path. The probe runs *before ELOH.A is planned* (not before ELOH.B) so that ELOH.A's spawn-once-vs-spawn-per-pair contract is decided up front, not retroactively after ELOH.A has landed. Procedure: spawn Stockfish, set `UCI_Elo=1320`, play one game, set `UCI_Elo=1500`, play another, verify the second game's strength is consistent with the new setting (e.g. via per-move score tracking). Result archived to `docs/research/tooling-stockfish-mid-session-setoption.md`. If the probe fails (Stockfish ignores the change or requires `ucinewgame` between settings), ELOH.A's spawn-once contract becomes spawn-per-game-pair, and ELOH.A's match-loop budget grows by ~30 LOC for the spawn-per-pair lifecycle.

## ADRs likely to bind per-phase

- **Harness driving model — wallclock TC + match-loop time-source seam.** Likely binds on **ELOH.A**. Open: time source for ELOH.A (wallclock); subprocess lifecycle (spawn-once-per-run, killed at run-end). The seam abstraction (`MatchClock` / `format_go_command` indirection so ELOH.C can swap to nodes-mode without ELOH.A refactor) is load-bearing for ELOH.C's budget; ADR commits to the seam shape even though only the wallclock variant is implemented in ELOH.A. Default in plan: wallclock + spawn-once + a `MatchTimeMode { Wallclock, Nodes(u64) }` enum threaded through the match loop's `go`-formatting and clock-tracking paths.
- **Concurrency primitive.** Binds on **ELOH.B** (where concurrency is actually implemented and exercised). Open: `std::thread` vs. `tokio` vs. `rayon`. Default in plan: `std::thread` + `std::sync::mpsc` (no async dependency for a workload that's pure blocking subprocess I/O).
- **`VirtualClock` UCI option semantics + `--go-nodes` harness mode.** Binds on **ELOH.C** (single ADR covering both since the engine and harness sides are tightly coupled). Open: time-source primitive (`clock_gettime(CLOCK_THREAD_CPUTIME_ID)` POSIX vs. `mach_thread_info(THREAD_BASIC_INFO)` Darwin) — both surfaces are platform-specific and need an `#[cfg(target_os)]` shim; mate-distance interaction (presumably none at the algorithmic level); `MoveOverhead` semantics under CPU-time (still milliseconds, but of CPU time, not wallclock); whether the substitution is a runtime check or a generic over a `TimeSource` trait monomorphized at search entry; degenerate `MoveOverhead` semantics under `VirtualClock=true` (the wallclock-jitter hedge is partially meaningless under CPU-time TC).
- **Adjudication thresholds (resign/draw/maxmoves).** Possibly binds on **ELOH.B** (where they're scoped — see scope detail). Open: match `scripts/match.sh` defaults exactly vs. expose as CLI flags. Default in plan: both — match defaults *and* expose as flags.
- **TC-distribution spec grammar + sampling cadence.** Possibly binds on **ELOH.D**, but most likely not — the spec grammar (`<TC>:<weight>(,<TC>:<weight>)*`) and per-pair sampling cadence are parameter-level decisions defended in the ELOH.D scope detail. Default plan: no ADR; revisit if reviewers flag the grammar choice as architectural.

ADR numbers allocated at landing time. ELOH.B and ELOH.D may land without an ADR if plans expose only parameter-level decisions; the time-source ADRs at ELOH.A and ELOH.C are likely binding.

## Sub-phases

| Phase | Scope | Approx size |
|---|---|---|
| **ELOH.A** ✓ — Harness foundation | Persistent subprocesses + UCI driver + native adjudication (mate/stalemate/50-move/FIDE-3-fold/insufficient-material) + per-side clock management + match-loop time-source seam + color-paired fixed-batch match loop. Plays N games against a fixed-config opponent; emits per-game PGN + summary. No K-update, no σ-stopping, no concurrency, no threshold adjudication. **Landed 2026-04-29.** ADR-0020; plan: `docs/plans/eloh.a.md`. Actual landing size: ~2300 LOC binary + 80 LOC lib seam (`src/match_clock.rs`) + 51 in-tree tests passing. Back-test gate result archived in `docs/research/tooling-elo-harness-validation.md` (lands as follow-up commit once the manual back-test completes). | ~500 (actual ~2300; integration glue across 7 submodules — driver/cli/adjudicate/pgn/summary/match_loop/main — overshot the plan's bin estimate) |
| **ELOH.B** ✓ — Online iteration + concurrency + progress + threshold adjudication | Robbins-Monro K-update + σ-stopping + concurrency (N parallel game-pairs) + convergence-progress output + resign/draw/max-moves threshold adjudication. Replaces `scripts/elo-iterate.sh` (thin wrapper) and `scripts/sprt.sh rating-estimate` arm end-to-end. **Landed 2026-04-30.** Plan: `docs/plans/eloh.b.md`. Actual landing size: ~5917 LOC total binary (ELOH.A+B combined) + 139 in-tree tests passing. Back-test gate (Part 1: online run vs M3.F ~2114 Elo; Part 2: synthetic Bernoulli σ-stopping) deferred to post-merge manual run. | ~150 binary + ~80 tests (actual ~650 new LOC across `estimator`, `sigma`, `progress`, `adjudicate` extension, `match_loop` extension, `controller`, `main` rewrite) |
| **ELOH.C** ✓ — Hardware-invariant TC | VirtualClock UCI option (`option name VirtualClock type check default false`) in clawfish: swaps `Instant::now()` for per-thread `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` in the worker-owned `SearchClock`; harness `--virtual-clock` flag + handshake-driven `setoption` negotiation (sent only to engines that advertise the option). `--go-nodes N` dropped per user decision 2026-04-30. ADR-0019. Back-test gate Part 1 deferred to post-merge manual run. **Landed 2026-04-30.** Plan: `docs/plans/eloh.c.md`. Actual landing size: ~190 prod LOC + ~140 test LOC = ~330 LOC. | ~190 prod + ~140 tests (actual) |
| **ELOH.D** — Mixed-TC sampling | Per-game TC sampling for mixed-TC SPRT and Δ(TC) regression. New `--tc-sample <SPEC>` flag accepting a discrete weighted list of TCs (e.g. `10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1`); harness draws TC per game before clock initialisation; PGN `TimeControl` tag and per-game summary line record the sampled TC. Mutually exclusive with `--tc`. Compatible with both SPRT and rating-estimate workflows. Decision-rule (mixed-game SPRT verdict, Δ(TC) regression fit) computed by downstream tooling — harness emits data only. | ~120 harness + ~50 tests |

ELOH.A → B → C is sequential. ELOH.D depends on ELOH.B (must have); is parallel-compatible with ELOH.C at the type-system level (TC sampling and time-source primitive are orthogonal axes — TC sampling sets the *base + increment* before clock initialisation; ELOH.C swaps the *time-source primitive* under those clocks); but is sequenced *after* ELOH.C to avoid match-loop merge conflicts and to inherit hardware-invariant TC for the Δ(TC) curve fit (less wallclock noise → tighter per-TC confidence bands). Each phase's back-validation gate must pass before the next is planned.

**Decision rationale on the 4-phase split.** A reviewer-suggested alternative was to collapse A and B into a single "harness" phase, matching the original two backlog entries 1:1. Rejected here because A's back-validation gate — correct W/L/D against M3.F's saturating 1320-anchor match — is cleanly separable from B's statistical-layer validation, and a per-phase plan-mode pass is shorter than one combined pass on ~650 LOC. The 4-phase shape (with ELOH.D added 2026-04-30) costs slightly more workflow ceremony and earns a clean decomposition that maps to four independently testable layers: correctness (A) → statistics (B) → environment-invariance (C) → TC-mixed-game support (D).

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

## ELOH.C scope detail — Done (2026-04-30)

ELOH.C landed on branch `tooling/elo-harness` (worktree `/Users/alex/clawfish-elo-harness`) on top of ELOH.B. ADR-0021 (`docs/decisions/0021-virtual-clock-uci-option.md`) captures the load-bearing design decisions. Research: `docs/research/tooling-cpu-cycle-counters.md` (initial survey + Apple Silicon follow-up + Linux VM follow-up + M4 empirical probe).

**What landed (clawfish-side, ~130 prod LOC including test-site migrations).**
1. New `SearchInstant` enum (`Wall(Instant)` / `Cpu(u64)`) + `SearchClock` worker-local struct. Time-source ownership lives in the worker thread — `SearchClock::start_for(virtual_clock, caps)` reads the calling thread's CPU clock once at entry of `Search::go` and derives all deadlines from that single read.
2. `SearchContext` loses the orchestrator-thread `start`/`deadline`/`soft_deadline` fields; gains `caps: TimeCaps` and `virtual_clock: bool`. This was the plan-review v2 must-fix: per-thread `CLOCK_THREAD_CPUTIME_ID` semantics made orchestrator-pre-computed deadlines categorically wrong for the worker thread.
3. `read_thread_cpu_ns()` private libc shim (`#[cfg(unix)]`).
4. `Engine::virtual_clock: bool` field + `handle_uci` emits `option name VirtualClock type check default false` (`#[cfg(unix)]`); `handle_setoption` parses and validates it.

**What landed (harness-side, ~55 prod LOC).**
5. `EngineCapabilities { supports_virtual_clock: bool }` + `parse_option_advertisement` in `mod driver`; `wait_for_uciok` extended to return `EngineCapabilities`.
6. `--virtual-clock` CLI flag (boolean, no value, default false); sends `setoption name VirtualClock value true` after `uciok` to any engine advertising the option when the flag is set.

**`--go-nodes N` — explicitly dropped.** The `MatchTimeMode::Nodes(u64)` seam from ELOH.A remains in the codebase as unconstructible-from-CLI dead code. Per user decision 2026-04-30: `go nodes N` is implementation-coupled — the nodes-per-work-unit figure shifts even within a single binary across runtime settings (Hash size, eval weights, etc.), making it unsuitable for cross-version SPRT. The seam is retained (removing it costs more churn than retention; future work may revive it for non-gate diagnostic use). The pre-ELOH.C spec item 6 of "In scope (harness-side)" is struck by this decision; ADR-0019 carries the full rationale.

**Size.** Actual landing: ~190 prod LOC + ~140 test LOC = ~330 LOC total. The plan's earlier `~70 harness + ~80 engine + ~80 tests` estimate predated the plan-review v2 must-fix that moved `SearchClock` to the worker thread — the `SearchContext` field migration across ~16 test-site constructions accounts for the growth.

**Back-test gate Part 1** (clawfish-vs-clawfish SPRT under simulated CPU load with `--virtual-clock`; target σ_VC / σ_wall ≤ 0.7) — deferred to post-merge manual run. Result will be archived to `docs/research/tooling-virtual-clock-validation.md` once completed.

See also: ADR-0021 (`docs/decisions/0021-virtual-clock-uci-option.md`); research: `docs/research/tooling-cpu-cycle-counters.md`; bench: `bench/eloh-c.md`.

## ELOH.D scope detail

ELOH.D is the harness's **mixed-TC layer**: it adds per-game TC sampling so a single harness run can play games drawn from a TC distribution, enabling true mixed-TC SPRT under the redefined game ("draw TC from D, then play standard chess at that TC") and supporting downstream per-(TC, outcome) regression for Δ(TC) curve diagnostics.

The fundamental insight: SPRT's i.i.d. assumption holds *under the redefined game*, even though it doesn't hold across fixed-TC games at different TCs. So mixed-TC SPRT is a clean (`elo0`, `elo1`) accept/reject on the aggregate Δ Elo of the mixed game; the per-game (TC, W/D/L) tuples additionally support a separate regression analysis for the curve. Both analyses run from the same data — ELOH.D's job is to produce that data.

**In scope.**
1. **CLI flag — `--tc-sample <SPEC>`.** Discrete weighted distribution. Format: `<TC>:<weight>(,<TC>:<weight>)*` where each `<TC>` is a `parse_tc`-compatible string and each `<weight>` is a positive integer. Example: `10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1` (uniform over 4 TCs); `10+0.1:3,60+0.6:1` (3:1 fast-to-slow). Weights are normalised at parse time. Mutually exclusive with `--tc` — harness errors at parse time if both are specified.
2. **Per-game TC sampling.** Before each game's clock initialisation, the harness draws a TC from the configured distribution using a seeded PRNG. **The same `--seed` controls both opening-position selection and TC sampling**, with the TC-sampling stream forked from the master seed via a documented sub-stream derivation (`SplitMix64::next` style) so opening selection isn't perturbed by adding `--tc-sample` to a previously-deterministic run. Default plan: re-document `--seed` in `--help` to note the dual role.
3. **Color-pair invariance.** Within a color-pair (mirror of fastchess `-repeat`), both games of the pair use the *same sampled TC* — the pair plays one position at one TC with both colors. Sampling fires once per pair, not once per game. Preserves ELOH.A's color-pair invariant: a pair is a single fair experiment at one TC.
4. **Stockfish-side TC compatibility.** When `--opponent-tc-override <TC>` is set with `--tc-sample`, Stockfish uses the override regardless of sampled TC; only clawfish's TC varies per pair. When the override is absent, both engines use the same sampled TC. Documented in `--help`.
5. **PGN per-game annotation.** Use the Seven Tag Roster `TimeControl` tag — already a standard PGN tag, format `<base>+<inc>` matching `parse_tc`'s input grammar. Each PGN's `TimeControl` reflects the sampled TC. Existing fastchess output is compatible at the PGN level.
6. **Summary file extension.** Per-game summary line gains a `tc=<base>+<inc>` field. Aggregate summary at run end emits a per-TC W/L/D table in addition to the existing aggregate count. Format example:
   ```
   summary: total W=420 L=380 D=200 (1000 games)
   summary-by-tc: 10+0.1: W=110 L=95 D=45 (250)  20+0.2: W=105 L=90 D=55 (250)  40+0.4: W=103 L=98 D=49 (250)  60+0.6: W=102 L=97 D=51 (250)
   ```
7. **Mixed-mode rating-estimate compatibility.** The K-update path (ELOH.B) accepts `--tc-sample` without modification; the resulting Elo number is documented as "Elo of the mixed game." For users who want per-TC Elo, the recommendation is to run separate fixed-TC rating-estimate sessions; ELOH.D's mixed-mode rating-estimate is for the aggregate. The K-update math is unchanged — game outcomes are still i.i.d. under the mixed game.
8. **Decision-rule support is *out of scope*.** Mixed-game SPRT verdict computation, Δ(TC) regression fitting, confidence-band visualisation are downstream tooling. The harness emits per-game (TC, W/L/D) data; analysis tooling consumes it. Keeping the harness as a data-emitter means it doesn't need to know about LLR thresholds, regression model choice, or confidence-band methodology — those are decisions that should be made per consuming use case (M4.D, M4.E adaptive width, M5+ mechanisms).

**Out of scope (deferred indefinitely).**
- **Continuous TC distributions** (`uniform:10+0.1..60+0.6`, `loguniform:10..80`, etc.). Discrete weighted lists cover the M4.D / M5 use cases; continuous distributions add CLI grammar surface without obvious downstream consumers. Defer until a real consumer asks for it.
- **Per-game TC asymmetry** (different sampled TC for white and black). Symmetric sampling is the natural fair-experiment choice; asymmetry is methodologically suspect for SPRT (breaks the "same game" assumption further than ELOH.D already does). Defer indefinitely.
- **Live curve-fitting / scatterplot output.** ELOH.D emits data; the curve fit lives in downstream tooling. The custom in-process Elo-iteration harness backlog item already mentions live regression as an extension; keep it as a follow-up if visualisation becomes a recurring need.
- **TC-adaptive aspiration negotiation.** A clawfish-side feature (engine adapts its window width as a function of received TC), tracked separately in `docs/tooling-backlog.md` under the ML/parametric aspiration item. ELOH.D is the measurement layer; engine-side adaptation is a separate scope.

**Open questions (resolved at ELOH.D plan landing).**
- Whether `--seed` controls both opening selection and TC sampling jointly (default plan: yes, with documented sub-stream derivation) or `--tc-sample-seed` is separate. Joint is simpler; separate is more flexible. Default joint unless plan-mode finds a reason to split.
- Whether the per-TC summary table also goes into `summary.txt` (default plan: yes, appended after the aggregate summary line) or only stdout (terser). Default both for now.
- Whether color-pair invariance is configurable (`--tc-sample-per-game` to fire sampling per game instead of per pair). Default plan: not configurable in ELOH.D; revisit if a consumer asks.

**Back-validation gate.** Two parts — both must pass.

- **Part 1 — Sampler correctness (synthetic, fully reproducible).** Drive the sampler with a known seed and a 4-bucket discrete distribution; verify the empirical bucket-frequency distribution matches the spec to within a chi-squared 99% CI at sample size N=1000. Tests bucket sampling correctness independent of the rest of the harness. ~40 LOC test.
- **Part 2 — Mixed-mode rating-estimate consistency (back-validation).** Run an ELOH.B rating-estimate against M3.F's saturating-anchor configuration, but with `--tc-sample 10+0.1:1` (a degenerate single-TC mix). Pass: result reproduces M3.F's ~2114 Elo within ±2σ ≈ ±70 Elo, identical to ELOH.B's Part 1 gate. Validates that the mixed-TC code path is a strict superset of the fixed-TC path — when the distribution has support of size 1, the harness's behaviour is observationally identical to ELOH.B's `--tc 10+0.1` path. ~30 minutes wallclock.

The two gates exercise the sampler (Part 1, deterministic) and the integration-with-K-update path (Part 2, against a known answer). Either alone is insufficient.

**Doc-delta (atomic with ELOH.D landing).**
- `docs/tooling-backlog.md` — "Per-game TC sampling for mixed-TC SPRT" sub-bullet (item 8 of the harness entry) moved to "Done."
- `docs/tooling/elo-iteration-harness.md` — ELOH.D row marked done; ELOH milestone closed.
- `docs/workflow.md` — short "Mixed-TC SPRT" subsection under "Online Elo iteration," documenting `--tc-sample` and the mixed-game SPRT framing.
- `docs/research/tooling-elo-harness-validation.md` — Part 1 + Part 2 results appended.
- `docs/architecture.md` — none expected (no engine-side change).

## Branches and worktrees

- **ELOH.A, ELOH.B, and ELOH.C.** Branch `tooling/elo-harness`, worktree `/Users/alex/clawfish-elo-harness`. (ELOH.C landed on the same branch as A and B rather than its own branch, per user directive to work in the existing worktree; ELOH.B had not merged to main when ELOH.C planning started.)
- **ELOH.D.** Harness-only branch, default name `tooling/eloh-d-mixed-tc`. Branched off `main` after ELOH.C has merged so the match-loop's TC-sampling addition layers on top of the time-source seam without merge-conflict risk. Worktree path TBD at ELOH.D plan time.

## Sequencing relative to M4

ELOH.A → B → C → D sequential. Parallel with M4.A (ELOH.A and ELOH.B share no source surface with M4.A's TT branch on `src/search.rs` / `src/engine.rs` — the harness lives at `src/bin/elo-iterate.rs` plus a possible `src/match_clock.rs` lib module). M4.A consumes ELOH.B's output once ELOH.B is merged to main; if ELOH.B misses the M4.A measurement window, M4.A's first rating estimate falls back to the bash version (no schedule risk to M4 itself). ELOH.C has no M4 dependency — it lands when wallclock variance empirically warrants, independent of M4 phase progression. M4.A is *not* a precondition for ELOH.C closure. ELOH.D's first real consumer is M4.D; if ELOH.D misses M4.D's window, M4.D runs the discrete 4-TC sweep approximation against `fastchess` (per the M4.D scope detail in `docs/roadmap.md`), and ELOH.D's first consumer becomes a follow-up M4.D analysis pass or the next mixed-TC-sensitive M4+/M5+ phase. No schedule risk to M4.D itself.

## Sizing

- Total ELOH milestone: ~840 LOC harness + ~80 LOC engine + ~360 LOC tests, decomposed across four phases as listed. ELOH.A is the largest at ~500 + 150 LOC — at the upper end of the workflow's typical-unit target (300–800), but cleanly below the ceiling. ELOH.D is the smallest at ~120 + 50 LOC.
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
- **ELOH.D breakdown (~120 harness + ~50 tests).** TC-spec parser (~30); per-pair sampler with seeded sub-stream (~25); CLI integration + mutual-exclusion check with `--tc` (~20); PGN `TimeControl` tag emission (~10); per-game summary line extension + per-TC aggregate emission (~25); plumbing + flag wiring (~10). Tests: sampler chi-squared back-test (~25); mutual-exclusion + spec-grammar parse tests (~15); PGN-tag golden file (~10).
- **Per-phase contingency: +30% à la M3.D / M3.E.** ELOH.A's largest tail risks are subprocess lifecycle (shutdown races, broken-pipe recovery on engine crash) and adjudication edge cases (insufficient-material variants in test discovery). ELOH.C's tail risk is the platform-shim work between POSIX and Darwin time-source primitives. ELOH.D's tail risk is small — the only nontrivial axis is the seeded sub-stream derivation (must not perturb existing single-`--seed` reproducibility for runs that don't use `--tc-sample`); covered by a regression test that pins opening-position selection across `--tc-sample` opt-in / opt-out at the same `--seed`. The spec accommodates ELOH.A landing closer to ~650 LOC if subprocess-driver edge cases eat budget.

## ELOH.B's role in M4.A measurement (precision caveat)

**Acknowledged structural concern:** ELOH.B's wallclock-based harness has back-validation tolerance ±2σ ≈ ±70 Elo (per Part 1 gate above) — the same order of magnitude as M4.A's expected ~50 Elo TT delta from the literature. ELOH.B is therefore **borderline-precise** for M4.A's forward-validation when used in wallclock mode. Three possible mitigations, picked at M4.A measurement time:

1. Accept the noise — record M4.A's wallclock-noisy estimate; re-measure under `--go-nodes` once ELOH.C lands. M4.A's SPRT (the primary gate) is wallclock-robust because SPRT compares two binaries on the same hardware in the same session; the rating estimate is documentation-grade, not gate-grade.
2. Run the M4.A rating-estimate at a longer TC (e.g. `tc=30+0.3`) where wallclock variance is proportionally smaller relative to the engine's compute budget.
3. Defer M4.A's rating-estimate forward-validation until ELOH.C lands. Adds milestone-coupling — typically undesirable, but acceptable here if ELOH.C is already near.

**Plan of record (user decision, 2026-04-29):** mitigation 1 immediately — M4.A's first rating estimate runs under wallclock ELOH.B and is documented as wallclock-noisy. **Likely mitigation 3 once ELOH.C is on the near horizon** — at that point defer rather than re-measure, since the wallclock number is throwaway anyway. The crossover point (when ELOH.C is "near enough") is decided in the moment, not pre-committed.

The milestone-level exit criterion at the top of this doc says "M4.A's first rating estimate uses the harness end-to-end as forward-validation" — that commitment stands under mitigation 1, with the resulting Elo number documentation-grade and carrying a footnote pointing at this precision caveat.
