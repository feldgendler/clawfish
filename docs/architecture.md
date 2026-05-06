# Architecture

Current architectural state. Decisions and their rationale live in `docs/decisions/`; per-feature design detail lives in the dedicated sections below the index.

## Foundational commitments

| Area | Choice | Decision record |
|---|---|---|
| Language | Rust | (foundational) |
| Scope | Standard chess only; no variant abstraction | ADR-0001 |
| Board representation | Bitboards, `u64` | implied by 8×8 + variant-out-of-scope |
| Sliding-piece move gen | Magic bitboards | implied by 8×8 |
| Move encoding | 16-bit | implied by standard chess |
| Evaluation v1 | Classical (material, PSTs, structural terms) | (foundational) |
| Evaluation future | NNUE — planned milestone (M9), not optional | ADR-0004 |
| Make/unmake structure | Single function calls; clean interception point for future NNUE accumulator | ADR-0004 |
| Strength dial | Planned milestone (M7); reuses the eval/move-selection function-call discipline | ADR-0005 |
| Parallelism | Not v1, but designed in (lockless TT, Lazy SMP affinity) | (foundational) |
| Protocol | UCI | (foundational) |
| Primary platform | Apple Silicon (ARM64) macOS; mobile is a downstream port | ADR-0002 |
| Source-code reading | No third-party chess engine source code as research input | ADR-0003 |
| Testing | TDD on rules layer (perft); property tests on search; SPRT on strength changes | `workflow.md` |
| Perft oracle | Stockfish (Homebrew) is the sole external source for perft fixtures | ADR-0006 |
| Benchmark baseline | `criterion 0.7` + per-machine `--save-baseline`; committed `bench/<milestone>.md` table | ADR-0010 |
| Tournament harness | `fastchess` 1.8.0-alpha is retained for `scripts/match.sh compliance` only (the `--compliance` UCI shake-out has no in-house substitute). All other flows are harness-side. | ADR-0012 (amended), ADR-0022 |
| In-process harness | `src/bin/elo-iterate.rs` drives SPRT, fixed-games match, smoke flows, and rating-estimate via persistent UCI subprocesses with native adjudication. Pentanomial-GSPRT verdict + post-hoc Δ Elo CI emitted in `summary.txt`; per-game PGN files + run-end `match.pgn`. | ADR-0020, ADR-0021, ADR-0022 |
| Time management | `compute_caps` pure function (soft + hard caps); ID outer loop with abort-between-iterations + mid-iteration hard-cap abort; `MoveOverhead` UCI option (default 50 ms) | ADR-0017 |
| Search time source | Wallclock by default; thread CPU time (`clock_gettime(CLOCK_THREAD_CPUTIME_ID)`) when `VirtualClock=true` per ADR-0021; ownership in the worker thread (per-thread counter semantics), not the orchestrator | ADR-0021 |
| Bench regression baseline | `bench` UCI command iterates a 16-position vendored corpus (`src/bench.rs::BENCH_POSITIONS`) at default depth 7; sums node counts; emits `info string bench: <N> nodes <NPS> nps` signature line. M3.F end signature: `bench: 172312700 nodes 11489045 nps`. | M3.F |
| SPRT runner | `scripts/sprt.sh sprt\|match\|rating-estimate` builds baseline binary in `git worktree`-isolated checkout; runs the in-process harness (`elo-iterate`) for pentanomial-GSPRT, fixed-game match, or rating-estimate. Field-standard historical-commit-baseline methodology. | `docs/workflow.md` §SPRT, ADR-0022 |

## Current design surface

Each row points to the dedicated section in this file (and the canonical ADR / plan) for full detail.

| Area | One-line shape | See |
|---|---|---|
| Position layout | 6+2 bitboards + mailbox + cached king squares + aux + Polyglot Zobrist + `static_eval_white` | "Position layout" below; ADR-0009, ADR-0014 |
| FEN parsing | Strict per Edwards 1994 §16.1 (single-space separator, strict-decimal integers, structural sanity); phantom-EP sanitized to `None` per ADR-0015 | `docs/plans/m1.b.md`; ADR-0015 (phantom-EP only) |
| Move encoding | 16-bit `Move(u16)`: from(6) / to(6) / flag-nibble(4); 14 valid flag codes | `docs/research/m1-engine-architecture.md` §2 |
| Sliding-piece attacks | Fancy magic bitboards, variable shift (~840 KiB) | "Sliding-piece attack lookup" below; ADR-0008 |
| Position hashing | Polyglot 781-key Zobrist; pseudo-legal-only EP rule; incremental in `make_move` | "Position hashing" below; ADR-0009 |
| Make/unmake | Free fns + `Position::*` delegates; incremental Zobrist + `static_eval` with debug round-trip | "Make / unmake" below; ADR-0004, ADR-0014 |
| Move generation | Legal-direct, mask-based, with check-evasion specialization | ADR-0007 |
| Perft validation | `perft` / `perft_bulk` / `divide` / `perft_categorized` against Stockfish 18 fixtures | "Perft validation" below; ADR-0006 |
| UCI move encoding | `Move::to_uci` / `Move::from_uci`; generate-and-match parsing | `docs/plans/m2.a.md` |
| UCI command parser | `parse_uci_line(&str) -> Command`; total function (no panics) | `docs/plans/m2.b.md` |
| UCI engine I/O loop | Reader thread → mpsc → orchestrator + per-`go` worker; `Arc<AtomicBool>` cancellation | ADR-0011 |
| Search trait | `Search` trait + `SearchContext` + `SearchLimits` + `SearchResult` (unchanged since M2.C) | ADR-0011; "Search v1" below |
| UCI options | `Random_Seed` (M2.D), `MoveOverhead` (M3.E), `Hash` (M4.A) | `docs/plans/m2.d.md`, ADR-0017, ADR-0018 |
| Evaluation v1 | PeSTO MG material + PST in a precomputed `PSQT[color][kind][square]` const table | "Evaluation v1" below; ADR-0014 |
| Production search | `AlphaBetaMover`: fail-soft negamax + qsearch (M3.D) + ID + caps (M3.E) + TT (M4.A) + killers (M4.B) + history (M4.C) + aspiration (M4.D) + NMP (M5.A) + RFP (M5.B) + LMR (M5.C) + FFP (M5.D); `bench` regression baseline (M3.F) | "Search v1" below; ADR-0016, ADR-0017, ADR-0018, ADR-0019, ADR-0023, ADR-0024, ADR-0025, ADR-0026 |
| Transposition table (M4.A) | `TranspositionTable` in `src/tt.rs`: `UnsafeCell<Vec<TtEntry>>` + `AtomicUsize` mask + `AtomicU8` generation; depth-preferred + age-bias replacement; full 64-bit Zobrist key; mate-score depth-adjustment; bound-aware probe with cutoffs at non-PV nodes only; `Hash` UCI option (default 16 MiB, range 1–4096) | "Transposition table" below; ADR-0018 |
| Game history + draw helpers | `Engine::game_history: Vec<u64>` + `is_repetition` + `is_fifty_move_draw` | "Game history and draw-detection helpers" below |
| Bench command (M3.F) | `Command::Bench { depth: Option<u32> }` + `Engine::handle_bench`; per-position `reset_for_new_game()` (M4.A) preserves determinism with TT in play | "Bench command" below |
| Per-game state lifecycle (M4.A) | `Engine::reset_for_new_game()` clears TT + position + game_history + Search-internal state. Called by `handle_ucinewgame` and per-position inside `handle_bench` | ADR-0018 §14 |
| History heuristic (M4.C) | `HistoryTable` in `src/history.rs`: `[[[i16; 64]; 64]; 2]` butterfly + side dim (16 KiB); `+= depth*depth` bonus on quiet-cutoff with `±MAX_HISTORY = 16384` clamp (literature standard); matching `-= depth*depth` malus on prior quiets in `quiets_searched`; persists across ID iterations + across `go` within a game; clears via `Search::reset()` (additive on M4.A's + M4.B's clear paths). Score tier discipline: `CAPTURE_OFFSET = 1_000_000`, `KILLER0_SCORE = 100_001`, `KILLER1_SCORE = 100_000`, `MAX_HISTORY = 16384` (the M4.B-merged `200`/`100` killer constants were bumped at M4.C-rebase to keep the captures > killers > history-quiets hierarchy intact while the history dynamic range extends across `[-MAX_HISTORY, MAX_HISTORY]`). | "History heuristic" below; ADR-0019 |
| Elo iteration harness (ELOH.A) | `src/bin/elo-iterate.rs` in-process tournament binary + `src/match_clock.rs` `MatchTimeMode { Wallclock, Nodes(u64) }` lib seam; spawn-once UCI subprocess driver, native adjudication (mate / stalemate / 50-move / FIDE-3-fold / insufficient material), per-side wallclock TC with grace; replaces `scripts/elo-iterate.sh` for online rating-estimation runs | ADR-0020; `docs/plans/eloh.a.md`; `docs/tooling/elo-iteration-harness.md` |
| Elo harness concurrency model (ELOH.B) | `std::thread` + `std::sync::mpsc` for N parallel color-pair workers; each worker owns its own engine subprocess pair; results merge in arrival order into a single-threaded Robbins-Monro K-updater. `WorkerPool` owns `senders: Vec<Sender<WorkerCmd>>` + `reports: Receiver<WorkerReport>` + `join_handles`; Drop clears senders (disconnect-driven worker exit) but does not join. No async runtime dependency. | `docs/plans/eloh.b.md`; `docs/tooling/elo-iteration-harness.md` |
| VirtualClock UCI option (ELOH.C) | `option name VirtualClock type check default false` (`#[cfg(unix)]`). When true, `Search::go` worker constructs a `SearchClock` using `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` instead of `Instant::now()`; harness `--virtual-clock` flag negotiates the option via handshake. Time-source ownership is worker-local. | ADR-0019; `docs/plans/eloh.c.md`; `bench/eloh-c.md` |

## Position layout

`Position` carries:

- `piece_bb: [u64; 6]` — one bitboard per piece kind.
- `color_bb: [u64; 2]` — one bitboard per color (occupancy).
- `mailbox: [Option<Piece>; 64]` — per-square piece type+color (saves popcount on capture lookups).
- Cached king squares for both colors.
- Aux state: side-to-move, castling rights (4-bit `KQkq`), EP target (`Option<Square>`), halfmove clock, fullmove number.
- `zobrist: u64` — Polyglot hash, maintained incrementally by `make_move` / `unmake_move` (ADR-0009).
- `static_eval_white: i32` — combined material + PST score from White's perspective, maintained incrementally (ADR-0014).

**Phantom-EP sanitization (Stockfish-compatible).** Per ADR-0015: the EP target field is set iff a pseudo-legal en passant capture actually exists. Both `from_fen` and `make_move` apply the same `fen::ep_capturer_exists` predicate, so `Position::ep_target` and `zobrist::ep_file_to_hash` agree by construction.

## Sliding-piece attack lookup

**Public API.** `magic::{rook_attacks, bishop_attacks, queen_attacks}` — pure functions `(Square, Bitboard) -> Bitboard`.

**Implementation.** Fancy magic bitboards with variable shift:

- Per-square `Magic { mask, magic, shift, offset }` struct, committed in `src/magic/constants.rs`, generated by `cargo run --release --bin magicgen`.
- The struct parameterises a multiply-and-shift hash from blocker-subset to a slot in a single shared backing array per piece-type.

**Differential oracle.** The `slow_attacks` module is a permanent slow ray-walker exposing the same API shape. It builds the runtime tables (so the source of truth is one walker) and powers the gold-standard test in `tests/magic_consistency.rs` (every `(square, occupancy ⊆ mask)` pair, both pieces).

See `decisions/0008-magic-bitboards-fancy-variable-shift.md`.

## Position hashing

**Public API.** The `zobrist` module exposes:

- `piece_key`, `castling_key`, `ep_file_key`, `turn_key` — per-component key accessors.
- `from_scratch(&Position) -> u64` — full hash composition.
- `ep_file_to_hash(&Position) -> Option<u8>` — the pseudo-legal EP predicate.

**Key set.** The 781 published Polyglot constants are vendored verbatim in `src/zobrist/keys.rs` from `docs/reference/polyglot-book-format.md`. Turn key is asymmetric: XORed *only* when White is to move.

**Storage.** `Position::zobrist()` exposes the cached field. Populated by `refresh_zobrist()` after every parse / construction; maintained incrementally by `mov::make_move` thereafter.

**EP-only-when-pseudo-legal rule.** The EP key is XORed only when an EP capture is geometrically possible — i.e. a pawn of the side-to-move sits adjacent (same rank, file ±1) to the opponent's just-pushed pawn. Pin and own-king-discovered-check are explicitly *not* tested, per the Polyglot spec's "irrelevant if the potential en passant capturing move is legal or not." This keeps our hashes interoperable with every published Polyglot opening book by construction.

See `decisions/0009-polyglot-zobrist.md`.

## Evaluation v1

**Public API.** The `eval` module exposes:

- `evaluate(&Position) -> i32` — side-to-move-relative centipawn score. Insufficient-material override returns 0 for KvK / KvN / KvB; otherwise reads `Position::static_eval_white()` and flips for Black.
- `eval_white_from_scratch(&Position) -> i32` — `pub(crate)` from-scratch recomputation. Used by `Position::refresh_static_eval()` after FEN parse / construction, and by debug-build round-trip asserts inside `make_move` / `unmake_move`.

**PSQT lookup.** `pub(crate) const PSQT: [[[i32; 64]; 6]; 2]` — compile-time table. `PSQT[color][kind][sq]` is the signed contribution of a (color, kind, sq) piece to `static_eval_white`:

- White lookup: `+(MATERIAL[kind] + MG_PST[kind][sq ^ 56])`.
- Black lookup: `-(MATERIAL[kind] + MG_PST[kind][sq])`.

The `s ^ 56` flip converts our internal LERF index (a1 = 0) to the PeSTO array's a8-origin layout (a8 = 0). Built at compile time by `const fn build_psqt()` in `src/eval.rs`.

**Vendored data.** `src/eval/data.rs` carries:

- `MATERIAL: [i32; 6]` = `[82, 337, 365, 477, 1025, 0]` — PeSTO MG values; king material 0 (kings are never captured).
- Six `MG_*_PST: [i32; 64]` arrays — PeSTO MG piece-square tables, vendored verbatim from Ronald Friederich's CPW post.

The split file is `cargo mutants`-excluded at file level via `.cargo/mutants.toml`. Same precedent as `src/magic/constants.rs` — vendored data, not logic.

**Insufficient-material draws.**

- `total_count == 2` → KvK → 0.
- `total_count == 3` AND (`knight_count == 1` OR `bishop_count == 1`) → KvN / KvB → 0.
- KBvKB same-color bishops deliberately not detected at M3 (deferred to M6 alongside the bishop-pair term).

**Incremental update site.** `Position::static_eval_white: i32` is maintained by `make_move` / `unmake_move` via the `update_static_eval_after_make` private helper in `src/mov.rs`. Six flag-arm deltas (one per MoveFlag category); the helper takes `mover: Piece` by value and uses `mover.color`, making it order-agnostic at the call boundary (mirrors `update_zobrist_after_make` discipline). `Undo::prior_static_eval` is captured pre-mutation; `unmake_move` restores via `refresh_static_eval_from`.

**NNUE-readiness (ADR-0004 extension).** When NNUE arrives at M9, the PSQT-based incremental update slots out for the accumulator update; `make_move` / `unmake_move` signatures stay. Exactly the discrete-function shape ADR-0004 designed for.

See `decisions/0014-eval-material-pst.md` and `docs/research/m3-eval-material-pst.md`.

## Search v1 (production: alpha-beta)

**Public surface.** `Search` trait + `SearchContext` + `SearchLimits` + `SearchResult` in `src/search.rs` (unchanged trait signature since M2.C).

**Production impl.** `AlphaBetaMover` in `src/search.rs` — replaces M3.A's `GreedyMover` with full alpha-beta recursion + qsearch leaf-extension + iterative deepening. ADR-0016 codifies the search structure; ADR-0017 codifies the time-management + ID outer loop.

**ID outer loop in `Search::go` (M3.E)**: iterates `for d in 1..=max_depth_from_limits(&ctx.limits)`. Per-iteration reset of `aborted` / `root_score` / `pv.lengths[..]`; `nodes` and `prior_root_move` are NOT reset between iterations. Mid-iteration aborts (hard cap fires via `should_abort` at the 4096-cadence) discard the partial PV/score; `last_complete` snapshot from the prior iteration becomes the reported result. Inter-iteration checks (post-emit): `depth >= max_depth` → break; `ctx.stop` → break; `ctx.soft_deadline` elapsed → break. Iteration 1 unconditionally runs once before the soft check fires. Final-result fallback when `last_complete = None` is extracted into a named `aborted_fallback_result(&PvTable, Option<i32>)` helper for direct unit-testability (M3.D `negate_window` precedent).

**Time management (M3.E)**: `compute_caps(&SearchLimits, Color, u64) -> TimeCaps` pure function in `src/search.rs`, called from `handle_go`. Returns `(soft, hard)` durations with `Duration::MAX` as the "no cap" sentinel; caller in `handle_go` constructs `(deadline, soft_deadline) = ((now + caps.hard).then_if_not_max, (now + caps.soft).then_if_not_max)`. Formula tree per ADR-0017 §1.

**`MoveOverhead` UCI option (M3.E)**: `Engine::move_overhead: u64` field, default 50 ms, valid `[0, 5000]`. Threaded into `compute_caps` at every `handle_go`. Latency hedge subtracted from clock-derived caps.

**`prior_root_move` ordering hint (M3.E, REMOVED in M4.A)**: superseded by the TT — every completed iteration's bestmove is stored at the position's TT entry; the next iteration's TT probe at the root extracts and reorders it via the TT-move-first discipline (see "Transposition table" below). Removal preserved by the M4.A replacement-scheme + abort-skip + end-of-loop-only-store invariants documented in ADR-0018 §1, §11, §13.

**Transposition table (M4.A)**: `Engine::tt: Arc<TranspositionTable>` shared with the search worker via `SearchContext::tt`. `AlphaBetaMover::tt: Option<Arc<TranspositionTable>>` populated at top of `Search::go`; `tt.new_search()` advances the per-search generation counter. `negamax` now threads an `is_pv: bool` parameter (root = `true`; child = `parent_is_pv && i == 0` where `i` is the post-reorder recursion index). Per-node prologue order: `original_alpha = alpha` capture (BEFORE MDP) → rep/50-move → MDP → TT probe (cutoffs at non-PV nodes only, ordering-only at PV) → moves → TT-move-first reorder → recurse → store on completion (skip on abort, end-of-loop only). See "Transposition table" subsection and ADR-0018.

**Time-source dispatch (ELOH.C / ADR-0021)**: `Engine::virtual_clock: bool` field (default `false`). When `true`, the search worker uses `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` instead of `Instant::now()`. The orchestrator passes `caps: TimeCaps` (durations only) and `virtual_clock` through `SearchContext`; the worker constructs a `SearchClock` at the entry of `Search::go`, reading the per-thread CPU clock on the correct thread. Cross-variant `SearchInstant` comparisons are `unreachable!`. The `VirtualClock` UCI option is `#[cfg(unix)]`-gated.

**`Search::go` body sketch** (M3.E, full detail in ADR-0017 §2):

1. Per-go reset: clone `ctx.history` into search-owned `self.history`; zero `nodes`; clear `aborted`, `root_score`, `prior_root_move`, `pv.lengths[..]`.
2. `max_depth = max_depth_from_limits(&ctx.limits)` (depth-cap → clamped, time-bounded → 63, bare → 4).
3. ID outer loop `for d in 1..=max_depth`:
   - Per-iteration reset: `aborted = false`, `root_score = None`, `pv.lengths[..] = 0`. (NOT reset: `nodes`, `prior_root_move`.)
   - `negamax(&mut pos_clone, d, 0, -INF, INF, ctx)`. Balanced make/unmake; debug_assert verifies.
   - On `self.aborted`: break (mid-iteration abort discards partial state; `last_complete` from prior iteration preserved).
   - Iteration completed: snapshot `last_complete = Some((d, bestmove, score))`; set `prior_root_move = bestmove` (consumed by next iteration's negamax at ply 0).
   - Emit `info depth <d> score <cp|mate N> nodes <N> time <ms> pv <line>` per completed iteration (NOT just at the end).
   - Inter-iteration breaks: `if depth >= max_depth { break; }`; `if ctx.stop.load(Relaxed) { break; }` (load-bearing — closes the per-iteration `aborted` reset gap); `if ctx.soft_deadline elapsed { break; }`.
4. Final result: prefer `last_complete` snapshot; fall back via `aborted_fallback_result` for the pathological "iter 1 aborted before any root improvement" case (extracted helper, mutation-testable).
5. If `infinite || movetime || ponder`: post-loop `while !ctx.should_abort(self.nodes) { sleep 1ms; }` — engine waits for stop / hard cap before emitting `bestmove`.

Per ADR-0017 §2, mid-iteration aborts go through `should_abort` (the hard-cap path, polled at `nodes & 4095 == 0` inside negamax/qsearch). The inter-iteration `stop` check between iterations exists because per-iteration `aborted` reset would otherwise mask a `stop` flipped between iterations.

**Negamax body** (`src/search.rs::AlphaBetaMover::negamax`, M3.D plan §5 restructure → M4.A prologue restructure → M4.B killer ordering + cutoff dispatch → M4.C history bonus/malus dispatch):

1. `pv.clear_ply(ply)` first — runs even on the leaf path so a stale `lengths[ply] > 0` from a prior subtree doesn't make the parent's `pv.update` copy stale child PV moves.
2. Horizon: at `depth == 0`, delegate to `qsearch` BEFORE the nodes increment. Qsearch's own per-frame increment + cancellation poll covers the leaf — preserves the "1 leaf = 1 node" budget contract under `go nodes <N>`.
3. Cancellation poll at `nodes & 4095 == 0` cadence + `should_abort` (non-leaf only). On abort: set `self.aborted = true`, return 0.
4. Capture `original_alpha = alpha` BEFORE MDP — load-bearing for M4.A's bound classification (Lower/Exact/Upper) at the store site. ADR-0018 §13.
5. Repetition + 50-move draw checks at `ply > 0` only (root must always pick a move; helpers from M3.B). Runs BEFORE TT probe per ADR-0018 §10.
6. Mate-distance pruning: tighten `(alpha, beta)` against `MATE - ply` / `-(MATE - ply)`; return early if window collapses.
7. TT probe (M4.A): bound-aware cutoff at non-PV nodes only; ordering-only at PV nodes. ADR-0018 §11.
8. Generate legal moves; at `ply == 0` apply `searchmoves` filter.
9. Empty move list: at `ply == 0 with searchmoves filter active`, return 0 (degenerate user input). Otherwise mate (`-(MATE - ply)`) if in_check, stalemate (0) if not.
10. **Sort by `negamax_move_order_score(mv, pos, killer0, killer1, &history_table)` descending (M4.A + M4.B + M4.C)**: non-quiets get `mvv_lva_score(mv, pos) + CAPTURE_OFFSET` (≥ 1_000_287 for QxP); killer0 → 100_001; killer1 → 100_000; non-killer quiets → `history_table.score(side, from, to) as i32` in `[-16384, 16384]`. M4.A's TT-move-first bubble runs after the comparator sort. See "Move ordering" subsection above.
11. Recurse fail-soft: `make_move + history.push + recurse + history.pop + unmake_move + post-call abort check + alpha-update + PV-update + beta-cutoff (return `best`, not `beta`)`. **M4.B killer-update**: on quiet beta-cutoff, `update_killers(killers, ply, mv)`. **M4.C history dispatch**: on quiet beta-cutoff, apply `+depth*depth` to the cutter's history entry and `-depth*depth` to every prior quiet in the per-frame `quiets_searched: MoveList` accumulator. **M4.C push site**: on no-cutoff quiet, push `mv` onto `quiets_searched` (with `debug_assert!` sentinel guard against `Move::default()` injection). See "Move ordering history-update discipline" paragraph above.
12. On post-call `self.aborted`: return 0 without committing score / PV update / history bonus or malus.
13. PV update at `ply == 0 && score > alpha`: `pv.update(ply, mv)` AND `self.root_score = Some(score)` (lockstep — ADR-0016 §5).
14. Store on completion (M4.A): bound classification against `original_alpha`; skip on abort, end-of-loop only. ADR-0018 §13.

**Qsearch body** (`src/search.rs::AlphaBetaMover::qsearch`, M3.D plan §3):

1. Per-frame nodes increment + cancellation poll (sole counter for depth==0 leaves).
2. Mate-distance pruning (may short-circuit before stand-pat is computed; safe).
3. In-check triage via `in_check(pos)` — decides whether stand-pat is permitted and which moves are searched.
4. Stand-pat lower-bound: only when not in check; `>= beta` returns stand_pat (fail-soft cutoff); `> alpha` tightens. `best_init = stand_pat` on not-in-check, `-INF` on in-check (forbidden in check per CPW; stand-pat-in-check is unsound).
5. Move generation: full legal moves in check (evasions); captures + queen-promos otherwise (filter from `generate_moves` output via `qsearch_move_filter`).
6. Terminal: in-check + empty → mate `-(MATE - ply)`; not-in-check + empty → return `best_init = stand_pat` (false-stalemate guard per CPW pitfall §10.7 — empty-after-capture-filter does NOT mean stalemate).
7. MVV-LVA ordering on the resulting list (reuses negamax's `mvv_lva_score`).
8. Fail-soft recursion with post-recursion abort propagation. Push/pop on `self.history` is balanced even on abort (the abort check runs after pop+unmake).

**Qsearch filter.** `qsearch_move_filter(mv) -> bool` accepts `Capture | EnPassant | QueenPromo | QueenPromoCapture`. Excludes under-promotions and under-promo-captures despite the latter being captures — under-promotion's tactical value (avoiding stalemate, knight-fork tactics) is rare enough at M3 strength that the extra qsearch breadth isn't justified. M5 may revisit (single-reply qsearch extension + stalemate-conditional rook/bishop under-promo-in-qsearch).

**Qsearch does NOT consult repetition / 50-move helpers.** By design (plan §3 "Subtleties to honor"). Captures + queen-promos reset halfmove_clock; in-check evasions can include non-capture king moves (which don't reset clock), but the chain of in-check evasions required to produce a repetition is vanishingly rare and detectable only by explicit history-walking that we deliberately skip for tightness. Pinned by `qsearch_skips_fifty_move_at_threshold` and `qsearch_skips_repetition_when_history_contains_current_position` tests.

**Move ordering (M4.A + M4.B + M4.C).** Layered scoring per `negamax_move_order_score(mv, pos, killer0, killer1, &history_table)`:
- **Non-quiets** (captures + EP + promotions) — `mvv_lva_score(mv, pos) + CAPTURE_OFFSET` where `CAPTURE_OFFSET = 1_000_000`. Smallest non-losing capture (QxP) = 287 cp raw → 1_000_287 in the comparator; queen-promo = 1025 cp raw → 1_001_025; promotions valued by promo piece (see ADR-0016 §6 for the worked-out table). The offset places every non-quiet strictly above every killer and every history-rated quiet; relative ordering between captures (and between captures and promos) is preserved by their raw `mvv_lva_score`.
- **Killer slot 0** (most recent quiet beta-cutoff at this ply, M4.B): `KILLER0_SCORE = 100_001`. Above killer1 and all non-killer quiets.
- **Killer slot 1** (prior quiet beta-cutoff at this ply, M4.B): `KILLER1_SCORE = 100_000`. Above all non-killer quiets.
- **Non-killer quiets** (M4.C): `history_table.score(pos.side_to_move(), mv.from_square(), mv.to_square()) as i32`. Range `[-MAX_HISTORY, MAX_HISTORY] = [-16384, 16384]`, strictly below `KILLER1_SCORE`.

The four tunables (`CAPTURE_OFFSET`, `KILLER0_SCORE`, `KILLER1_SCORE`, `MAX_HISTORY`) are pinned by a compile-time const-assert (`_SCORE_TIER_INVARIANTS`) at the top of M4.B+M4.C's helpers section in `src/search.rs`; runtime test S23 re-pins the discipline against MVV-LVA-formula drift, and HS12 re-pins the captures > killers > history-quiets ordering across the full pipeline.

Post-sort, the TT move (M4.A) is unconditionally promoted to index 0 if present and not already first. Killer slots: `[[Move; 2]; MAX_PLY]` on `AlphaBetaMover`, `Move::default()` sentinel; updated on quiet beta cutoffs via shift-on-distinct (`update_killers`); cleared per-go, per-iteration (M4.B inter-iteration policy), and on `Search::reset`.

**Move ordering history-update discipline (M4.C).** On quiet-move beta cutoff in negamax, the cutter's `(side, from, to)` history entry receives `+depth²` (clamped to `±MAX_HISTORY`); every quiet in the per-frame `quiets_searched: MoveList` accumulator (quiets recursed-but-not-cutting earlier in this node's move loop) receives `-depth²` (also clamped). Captures, EP, and promotions are NOT in `quiets_searched` (only `is_quiet(mv)` moves are pushed). The `pos.side_to_move()` read happens at the cutoff site AFTER `unmake_move(mv, undo)` — i.e., at the mover's color. Path-independent; not gated by `is_pv`. Killers (M4.B) are quiets and DO participate in `quiets_searched` like any other quiet — when tried-and-failed at this node and a later quiet cuts, the killer receives malus (research §10).

**Aspiration windows (M4.D).** The ID outer loop wraps each iteration's negamax call inside a two-tier aspiration loop. At depth ≥ `ASPIRATION_MIN_DEPTH = 6`, the first try uses a window centered on the prior iteration's score with half-width `±ASPIRATION_HALF_WIDTH = 50` cp. Threshold raised from initial 4 to 6 after empirical tc=10+0.1 SPRT (threshold=4: −22.62 ± 21.70 Elo regression; threshold=6: +65.92 ± 26.52 Elo LOS=100% vs same baseline). At fast TC the engine reaches ~depth 7, so threshold=4 exposed too many shallow iterations to aspiration's re-search overhead; threshold=6 limits the exposure to iterations 6–7 where prior-iteration scores are stable enough for first-try success rate to dominate the EV calculus. On fail-high (`returned ≥ prev_beta`) the re-search uses `(returned, +INF)`; on fail-low it uses `(-INF, returned)` — the proved fail-soft bound is preserved on the unfailed side. Window-contained returns short-circuit the re-search. The `tries >= 2` cap enforces "two-tier": at most two negamax calls per iteration. Mate-score priors are NOT special-cased (the asymmetric widening handles them; research §7.2). Iteration 1 and depths < 4 use full window `(-INF, INF)`. Per-try resets clear `aborted` / `root_score` / `pv.lengths[..]`; killers are NOT cleared between tries (preserve cross-try ordering hints; research §12.7). Empty-PV-after-fail-high recovery via `extract_bestmove_or_tt_fallback` helper that falls back to the root TT entry's `best_move` (preserved via ADR-0018 §7's best-move-on-overwrite rule). One `info string aspiration_re_search depth=N alpha=A beta=B` line is emitted per re-search for test-instrumentation.

**Null-move pruning (M5.A).** A null-move sub-search slots into the negamax prologue between the TT probe + cutoff (step 7) and movegen (now step 10), as step 9. Seven-condition gate: `ply > 0` (structural-root guard, defense-in-depth against future PVS refactor) ∧ `allow_null` (stacked-null prevention; new `bool` parameter on `negamax`) ∧ `!is_pv` ∧ `depth >= NMP_MIN_DEPTH = 3` ∧ `!in_check(pos)` ∧ `has_non_pawn_material(pos, stm)` (zugzwang guard: any knight/bishop/rook/queen) ∧ `static_eval >= beta` (lazy read inside the gate; sign-flipped from `Position::static_eval_white` for STM). On gate-pass, fire a zero-window null-search at `(-beta, -beta + 1)` with `allow_null = false` and reduced depth `depth - 1 - R` where `R = NMP_BASE_R + depth/NMP_DEPTH_DIVISOR = 2 + depth/6`. On `null_score >= beta`, mate-cap to `beta` if `null_score >= MATE_IN_MAX_PLY` (NMP doesn't prove mate; returning a mate-magnitude score from the null branch would mis-rank the position) — otherwise `cutoff_score = null_score`. Store TT as `Bound::Lower` at the **current depth** (not reduced — the cutoff proves a lower bound at this node, not at the reduced child; ADR-0018 §3 + §1) with `best_move = 0` (NMP didn't pick a move; ADR-0018 §7 preserves any prior best_move) and `score = score_to_tt(cutoff_score, ply)` (the mate-capped value, NOT raw `null_score` — propagating mate-magnitude through TT would later be returned as real mate proof at a different ply). Companion `make_null_move` / `unmake_null_move` in `src/mov.rs` flip STM, clear EP, increment halfmove, conditionally increment fullmove (Black-was-to-move), and XOR turn-key + ep_file_key (when Polyglot-active before the null) into the Zobrist; pieces / mailbox / castling / `static_eval_white` untouched. `NullUndo` carries `prior_ep` + `prior_halfmove` + `prior_zobrist`; `prior_fullmove` is derived from post-make STM in `unmake_null_move` per the existing `unmake_move` pattern at `mov.rs:803`. ADR-0023 codifies the formula, gate set, zugzwang policy, mate-cap, and TT-store discipline; defers verification search, eval-aware R bonus, and threat extraction to M5+. Bench signature drops from M4.D's 15.86M nodes to M5.A's 5.35M nodes (−66.3% at default depth 7).

**Frontier futility pruning (M5.D).** A per-quiet-move skip inside the negamax move loop at shallow depth (`depth ≤ FFP_MAX_DEPTH = 2`). Five-condition node-level gate (`ply > 0`, `!is_pv`, `depth >= 1 && depth <= FFP_MAX_DEPTH`, `alpha.abs() < MATE_IN_MAX_PLY`, `!in_check(pos)`) computed once before the move loop, latched into `ffp_static_eval: Option<i32>` whose `Some/None` IS the eligibility predicate. Per-quiet helper `ffp_pruned_bound(static_eval, depth, alpha) -> Option<i32>` returns `Some(static_eval + margin)` (saturating) when the gate fires, else `None`. Per-depth margin table: `FFP_MARGIN_D1 = 100`, `FFP_MARGIN_D2 = 150`, `FFP_MARGIN_D3 = 250` (the d=3 entry is defined for forward compatibility but inactive at v1). FFP fires before LMR in the per-quiet branch; at v1 constants the two paths cannot co-fire (compile-time invariant `FFP_MAX_DEPTH < LMR_MIN_DEPTH` pins the disjointness). When FFP fires, the move's proved fail-soft upper bound `pruned_bound = static_eval + margin ≤ alpha` is contributed to the node's running `best`, **routed through `best_is_full_depth_after_score(..., move_is_full_depth = false)`** so the flag is downgraded if the FFP bound overwrites a previously full-depth-witnessed `best` (load-bearing for TT-store correctness — ADR-0026 §7; mirrors ADR-0025 §6's reduced-only-LMR suppression rule). FFP-pruned quiets do NOT advance `quiet_index` (semantic: ordinal counts considered-for-reduction quiets, not seen-in-ordering — ADR-0025 §3) and do NOT enter `quiets_searched` (no recursive evidence — ADR-0026 §8). No TT store directly on FFP fire; the end-of-loop store is suppressed via the flag downgrade. Lazy-dup `static_eval` read independent of NMP's (step 9) and RFP's (step 8) — preserves ADR-0023/0024 byte-identical, makes the M5.D SPRT signal attributable to FFP alone. Bench signature drops from M5.C's 1.65M nodes to M5.D's 1.41M nodes (−14.4% at default depth 7).

**Reverse futility pruning (M5.B).** A purely static cutoff slots into the negamax prologue between the TT probe (step 7) and NMP (step 9), as step 8. Six-condition gate: `ply > 0` ∧ `!is_pv` ∧ `!in_check(pos)` ∧ `depth <= RFP_MAX_DEPTH = 6` ∧ `beta.abs() < MATE_IN_MAX_PLY` ∧ `static_eval - reverse_futility_margin(depth) >= beta`. The margin is `RFP_MARGIN_PER_DEPTH * depth = 100 * depth` cp (100 cp at d=1, 600 cp at d=6). On gate-pass, return `static_eval - margin` immediately (fail-soft proved lower bound) without generating a single move. No TT store: the proof is depth-specific to the margin and not reusable as a search-quality bound at a different probe depth (ADR-0024 §5). `static_eval` is read lazily **inside** the gate and is **not** shared with NMP's read — each block reads independently; the lazy-dup design preserves NMP semantics verbatim and attributes M5.B's SPRT signal cleanly to RFP alone (ADR-0024 §6). RFP fires before NMP because it is cheaper; on the `d=3..6` overlap, an RFP cutoff skips NMP's null-search entirely. ADR-0024 codifies the gate set, margin formula, return value, TT-store policy, and ordering decision; defers eval-aware margin, per-depth table, and improving heuristic to M5+. Bench signature drops from M5.A's 5.35M nodes to M5.B's 3.36M nodes (−37.2% at default depth 7).

**Cancellation.** Per ADR-0016 §7: 4096-node poll cadence; sentinel-return-0 on abort; post-call `self.aborted` check skips score/PV update. Worker thread joined by `handle_quit` so `bestmove` write is visible before `run` returns.

**`Random_Seed` UCI option preserved as no-op.** Alpha-beta is deterministic; the `set_seed` default trait method is a no-op for `AlphaBetaMover`. Cleanup deferred to a separate commit (would touch `scripts/match.sh`'s self-play target which still uses seed-based per-engine differentiation).

**Why no ID at M3.D.** Plan keeps M3.D narrow: qsearch as a leaf-evaluation refinement, not a search-depth refinement. M3.E adds iterative deepening + soft/hard time-management (wraps `negamax` in an outer ID loop; qsearch unaffected).

See `docs/plans/m3.c.md`, `docs/plans/m3.d.md`, `docs/decisions/0016-search-structure.md`, `bench/m3.md` (M3.C and M3.D sections).

## Transposition table (M4.A)

**Public surface (within crate).** `src/tt.rs` exposes:

- `pub(crate) enum TtBound { Exact, Lower, Upper }` — `#[repr(u8)]`, packed into 2 bits.
- `pub(crate) struct TtEntry` — 16-byte packed: `key: u64`, `score: i16`, `depth: u8`, `age_and_bound: u8` (6 bits age + 2 bits bound), `best_move: u16`, `_pad: u16`. `_pad` is logically zero.
- `pub(crate) struct TtData` — input bundle to `store`.
- `pub(crate) struct TranspositionTable` — `UnsafeCell<Vec<TtEntry>>` + `AtomicUsize` mask + `AtomicU8` generation. `unsafe impl Sync` with single-mutator discipline anchored to ADR-0011.
- `pub(crate) fn score_to_tt(score: i32, ply: i32) -> i32` and `pub(crate) fn score_from_tt(score: i32, ply: i32) -> i32` — mate-aware adjustments via threshold `MATE_IN_MAX_PLY = 29936`.

**API.** `new(size_mib)` / `resize(size_mib)` / `clear()` / `new_search()` / `probe(key)` / `store(key, data)` / `entry_count()` / `generation()` / `index_for(key)`.

**Replacement scheme (depth-preferred + age-bias).** Replace if old entry is empty OR `old.age != current_gen` OR `old.depth <= new.depth`. Best-move preserved when new entry has `best_move == 0` AND the slot's existing entry has the same key with a non-zero best_move.

**Mate-score adjustment.** On store: positive mate gets `+ply` added; negative mate gets `-ply` subtracted. On probe: inverse. Threshold: `|score| > MATE_IN_MAX_PLY` (29936 with `MATE = 30000`, `MAX_PLY = 64`).

**Lifecycle.** Engine owns the canonical `Arc<TranspositionTable>`; cloned into each `SearchContext` at `handle_go` / `handle_bench` time. `new_search()` is called once per `go` to advance the generation counter — entries from prior `go`s are freely replaceable. `clear()` zeros all entries AND resets generation to 0; called via `Engine::reset_for_new_game()` from `handle_ucinewgame` and per-bench-position from `handle_bench`. `Hash` UCI option mid-session resize: `setoption name Hash value <N>` joins any in-flight worker, then calls `tt.resize(N)` (allocates new Vec, zero-init).

**PV-node vs non-PV-node discipline.** TT cutoffs are scoped to non-PV nodes only. `is_pv: bool` parameter threaded through `negamax` (root = `true`; child = `parent_is_pv && i == 0` where `i` is the post-reorder recursion index). Under fail-soft pure alpha-beta `is_pv` is a synthetic ordering predicate; PVS at M4.D will replace it with `beta - alpha == 1`.

**Per-node prologue ordering.** `original_alpha = alpha` capture (BEFORE MDP, BEFORE TT probe — load-bearing for bound classification) → rep/50-move check → MDP → TT probe (with score-from-TT mate adjustment) → bound comparison vs post-MDP window → cutoff or fall-through with TT move kept as ordering hint.

**Store discipline.** Store fires after the move loop returns; skipped if `self.aborted`. Bound classified as: `best >= beta → Lower`, `best > original_alpha → Exact`, else `Upper`. Best-move: cutoff move on Lower; `pv[ply][0]` on Exact; 0 on Upper (replacement preserves slot's existing best_move on same-key).

**`Hash` UCI option.** `option name Hash type spin default 16 min 1 max 4096`. Default 16 MiB ÷ 16-byte entries = 1,048,576 entries. Allocation rounds DOWN to nearest power of two (mask-based indexing).

**Graph-history-interaction.** Option 1 ("live with it") — repetition check runs BEFORE the TT probe in the prologue, neutralizing the most common GHI manifestation (draw-by-repetition mis-scoring). 50-move-boundary GHI is acknowledged and deferred. ADR-0018 §10.

**Per-game state inventory.** `Engine::reset_for_new_game()` clears: TT entries (zero), TT generation (0), position (startpos), game_history (`vec![startpos.zobrist()]`), Search-internal state (via `Search::reset`, which clears the M3.B game-history Zobrist Vec AND the M4.C `HistoryTable`). Called from `handle_ucinewgame` AND per-position inside `handle_bench` for deterministic bench across positions.

## History heuristic (M4.C)

`src/history.rs` ships:

- `pub(crate) const MAX_HISTORY: i16 = 16384` — saturation cap (literature standard; CPW + MadChess + general practice).
- `pub(crate) struct HistoryTable { entries: [[[i16; 64]; 64]; 2] }` — 16 KiB; `[side][from][to]` butterfly + side dim.
- `impl HistoryTable` with `new`, `clear` (memset via `*self = Self::new()`), `score`, `update` (clamp on add).

**Update semantics.** `update(side, from, to, bonus: i32)` adds `bonus` to the entry then clamps to `[-MAX_HISTORY, MAX_HISTORY]`. Callers pass `+depth*depth` for cutter bonus or `-depth*depth` for malus; the i32 intermediate prevents transient overflow before clamping.

**Lifecycle.** `AlphaBetaMover` owns the `HistoryTable` directly (no shared `Arc`; the search worker is the sole mutator per ADR-0011). `Search::reset()` clears it (additive — preserves the M3.B `self.history.clear()` line). Engine's `reset_for_new_game()` call chain is unchanged from M4.A — this is the M4.A boundary the M4.C clear hooks into.

**Why no `Hash` UCI integration.** The 16 KiB table is below the threshold cited in TalkChess t=67878; the `Hash` UCI option remains TT-only per ADR-0018 §4. ADR-0019 §5 codifies.

**Persistence.** History accumulates across all ID iterations within a `go`, AND across `go` invocations within a game. Resets only on the M4.A boundary (`ucinewgame` + bench-position). Clearing between iterations would destroy the cross-iteration carry-over which is the primary value of history.

**Path-independence.** Indexed only by the move's `(side, from, to)` — not by position. A move's history score depends on its overall historical effectiveness, not on the path taken. ADR-0019 §3 + research §1.

**Killer interaction (post-M4.B-merge).** Killers participate in `quiets_searched` like ordinary quiets — when tried-and-failed they receive malus from a later quiet's cutoff. ADR-0019 §3 + research §10. The merge plan in `docs/plans/m4.c.md` §7 documents the integration explicitly to prevent re-introduction of an unfounded killer-skip filter.

See `decisions/0019-history-heuristic.md` and `docs/research/m4-history-heuristic.md`.

## Game history and draw-detection helpers

**Public surface (within crate).** `src/search.rs` exposes:

- `pub(crate) fn is_repetition(history: &[u64], halfmove_clock: u8) -> bool` — single-occurrence-in-search repetition test.
- `pub(crate) fn is_fifty_move_draw(halfmove_clock: u8) -> bool` — true at `halfmove_clock >= 100`.

**Engine state.** `Engine::game_history: Vec<u64>` holds the full Polyglot Zobrist trajectory of the current game, **including the current position**. The invariant is `engine.game_history.last() == Some(&engine.position.zobrist())` after every command handler returns.

**Lifecycle.**

- `Engine::new()` initializes `game_history = vec![Position::starting_position().zobrist()]`.
- `handle_ucinewgame` resets both `position` and `game_history` to startpos / `vec![startpos.zobrist()]`.
- `handle_position` rebuilds `game_history` per command. FEN parse error returns *before* touching either field (preserves both prior). Move-clause error sets `position = base` and `game_history = vec![base.zobrist()]` (single entry, the *new* base — not `vec![]`, not the prior history, not the partially-built one). On full success commits both atomically: `position = pos; game_history = hist`.

**`SearchContext::history`.** Owned `Vec<u64>`, cloned from `Engine::game_history` at `go`-spawn time in `handle_go`. The clone is intentional — the per-`go` worker thread holds its own copy so subsequent `go` invocations see a pristine engine state (and so M3.C's negamax can push/pop in place during recursion without affecting the engine).

**Algorithm — `is_repetition`.**

- Empty history returns `false` (no current position to compare).
- Walks priors backward in 2-ply steps `i = 2, 4, 6, …` capped at `min(halfmove_clock, history.len() - 1)`.
- First match (current's Zobrist == prior's Zobrist) returns `true`.
- Per CPW "Repetitions": a single match counts inside the search; FIDE 9.2 three-fold-claim is the GUI/adjudicator's responsibility.

**Why 2-ply step.** Only same-side-to-move positions can repeat — the Polyglot Zobrist turn key (`turn_key`) flips on every ply, so opposite-color positions can never share a hash.

**Why two caps on `max_back`.** `halfmove_clock` is the irreversible-move stop (any pawn move or capture severs the chain); `history.len() - 1` is the safety bound (FEN-loaded clocks may exceed observed history depth).

**Algorithm — `is_fifty_move_draw`.** Returns `true` at `halfmove_clock >= 100`. 100 plies = 50 moves by each player without pawn move or capture, the FIDE 9.3 claimable threshold. The 75-move auto-draw at 150 plies (FIDE 9.6.2) is not separately handled — once claimable, treated as drawn for engine reasoning.

**Consumer.** None at M3.B. M3.C's negamax is the first consumer; the helpers' signatures are intentionally `&[u64]` / `u8` so the data's exact storage location (inside `SearchContext`, in a `Cell`, or as a separate `&mut Vec<u64>` parameter) is a deferred M3.C decision.

See `docs/plans/m3.b.md` and `docs/research/m3-search-basics.md` §8–§9.

## Bench command (M3.F)

**Public surface (within crate).** `src/bench.rs` exposes `BENCH_POSITIONS: [&str; 16]` (vendored FEN corpus, 3 opening / 8 middlegame / 5 endgame) and `BENCH_DEFAULT_DEPTH: u32 = 7`. Module-scope `const _: () = assert!(...)` invariant ties the constant to the parser's `1..=63` accept range — failing the build, not just `cargo test`, on out-of-range edits.

**Public surface (UCI).** `Command::Bench { depth: Option<u32> }`, parsed by `parse_bench` in `src/uci.rs`. Bare `bench` → `depth: None` (handler picks default); `bench <N>` for `N ∈ 1..=63` → `Some(N)`. Strict-on-extra-tokens, mirrors `parse_debug` discipline.

**Handler.** `Engine::handle_bench` (synchronous on orchestrator thread) iterates `BENCH_POSITIONS`, drives `Search::go` at fixed depth on each with a no-op `info_sink`, sums node counts, emits per-position `info string bench position N/16: <fen> nodes <X> time <ms>` lines plus a final `info string Nodes searched: <N>` and `info string bench: <N> nodes <NPS> nps` summary. The signature line is OpenBench-grep-compatible (substring regex `bench: \d+ nodes \d+ nps` matches the `info string`-prefixed form).

**`stop` discipline.** `handle_bench`'s first action is `join_in_flight_worker()` (mirrors `handle_ucinewgame`), followed immediately by `self.stop.store(false, Ordering::Relaxed)` — same pattern as `handle_go`. Without the explicit clear, a `[go infinite, bench]` sequence would inherit `stop=true` from the join, contaminating per-position `Search::go` invocations: at default depth 7, the inter-iteration stop check (search.rs:383) breaks the ID loop after iter 1, producing tens of nodes per position instead of millions. Pinned by `handle_bench_after_go_infinite_produces_clean_state_results`.

**`compute_bench_nps` helper.** `pub(crate) fn compute_bench_nps(total_nodes: u64, total_ms: u128) -> u128` extracted from `handle_bench`'s body. Returns `(total_nodes × 1000) / max(total_ms, 1)`. Three cargo-mutants survivors on the inline expression (`/ → %`, `/ → *`, `* → +`) were structurally undetectable at the integration-test layer (the order-of-magnitude NPS sanity band is too wide to catch precise arithmetic mutations), so the helper is unit-tested directly with `(10, 3)` fixtures that distinguish each mutation. Same precedent as M3.D's `negate_window` and M3.E's `aborted_fallback_result`.

**Determinism scope.** The node-count signature is deterministic across runs from the same binary; per-position `time {ms}` and aggregate `<NPS>` are wallclock-dependent and explicitly excluded from the regression signature. Pinned by `bench_node_count_is_reproducible_across_invocations`. Within-run cross-position state isolation is a `Search::go` invariant (per-go reset at line 321 of `src/search.rs`) — already pinned by M3.E's ID tests.

**SPRT runner.** `scripts/sprt.sh` wraps the in-process `elo-iterate` harness (ELOH.E migration; previously fastchess) with three subcommands:

- `sprt <baseline-tag>` — pentanomial-GSPRT match HEAD vs baseline-tag (clawfish-vs-clawfish), bounds `elo0=0, elo1=10, alpha=0.05, beta=0.05`, up to 400 games default. M3 exit-criterion gate. The verdict and post-hoc Δ Elo CI both land in `<out-dir>/summary.txt`.
- `match <baseline-tag>` — fixed-game-count match (200 games default), no SPRT termination. Post-hoc CI emitted at run end.
- `rating-estimate` — fixed-game-count match HEAD vs Stockfish UCI_Elo=1320 (200 games default; ADR-0012 reference point). Frozen-K + disabled-σ.

The script builds the baseline binary in a `git worktree`-isolated checkout (`target/sprt-baselines/<tag-slug>/`), caches it across runs, and probes the result with `printf 'uci\nquit\n' | <bin> | grep '^uciok$'` before the harness starts (catches stale-toolchain rebuilds). `SPRT_REBUILD=1` forces a fresh worktree. Forward-compatible with the historical `chess` → `clawfish` package rename: reads the package name from the baseline's `Cargo.toml` to derive the binary path.

See `docs/plans/m3.f.md` and `bench/m3.md` M3.F section.

## Make / unmake

**Public API.** The `mov` module is the engine's sole supported state-transition surface:

- `make_move(&mut Position, Move) -> Undo`
- `unmake_move(&mut Position, Move, Undo)`
- Ergonomic delegates: `Position::make_move`, `Position::unmake_move`.

Both functions trust their callers to supply pseudo-legal moves; movegen (M1.F) is responsible for emitting only legal moves.

**`make_move` decomposes as:**

1. Capture `Undo` snapshot (prior aux state + zobrist).
2. Stash the prior position's EP-key contribution (computed pre-mutation while side-to-move's pawn bitboards still reflect the prior state).
3. Mutate bitboards / mailbox per the move's flag.
4. Compute new aux state:
   - Castling rights via the 64-entry `CASTLING_MASK` table: `new = old & MASK[from] & MASK[to]`.
   - EP target: set iff `DoublePush` *and* an opponent pawn capturer is geometrically adjacent (`fen::ep_capturer_exists`); else `None`. The phantom-EP sanitization mirrors `from_fen` so `Position::ep_target` and `zobrist::ep_file_to_hash` agree by construction.
   - Halfmove: reset on pawn move or capture, else saturating-incremented.
   - Fullmove: incremented after black's move.
5. Re-read the EP-key contribution under the new aux state.
6. Apply the incremental Zobrist delta — mover keys (promotion uses promoted-piece, not pawn), captured-piece key on its actual square (NOT `to` for EP), castling-rights deltas, EP-file delta (out + in), and turn key.

**`unmake_move`** is the inverse: it consumes the `Undo` to restore aux state and zobrist directly (no incremental inversion) and reverses the mailbox/bitboard mutations.

**Validation.**

- *Debug builds:* cross-field consistency assert + zobrist round-trip against `from_scratch` after every make/unmake.
- *Release builds:* trust the incremental delta exclusively. Always-on `make_move_no_from_scratch_in_release` perf sentinel guards against accidental from-scratch reintroduction.

**NNUE-readiness (ADR-0004).** Satisfied by the discrete-function shape: when an accumulator lands, its update slots into `make_move`'s body without any signature change. `Move` and `Undo` carry every input the delta needs (from, to, mover, captured piece, promotion target, castling rook movement, EP capture square).

## Perft validation

**Public API.** The `perft` module exposes:

- `perft(&Position, depth: u32) -> u64` — plain recursive perft.
- `perft_bulk(&Position, depth: u32) -> u64` — bulk-counting variant (CPW depth-1 leaf-skip; ~3.6× faster than plain at D4 from start).
- `divide(&Position, depth: u32) -> Vec<(Move, u64)>` — per-move sub-counts, **sorted by UCI move notation ascending** for direct text-diff against `sort` of Stockfish's `go perft N` output.
- `perft_categorized(&Position, depth: u32) -> PerftCounts` — internal-only category counts (captures, EP, castles, promotions, checks, discovery checks, double checks, checkmates) per ADR-0006.

**Recursion driver.** Plain and bulk share `perft_inner<const BULK: bool>`, monomorphized so the `BULK && depth == 1` leaf-skip branch is dead-code-eliminated from the plain path. Stack-allocated `MoveList` per recursion frame (~512 B / frame; D8 ≈ 4 KiB total stack — well under macOS 8 MiB default).

**Categorized recursion.** Separate `perft_categorized_inner` path (no bulk-skip) classifies each move's flag category and post-make check / mate state. Mate detection is anchored at the leaf-classification ply (depth==1 post-make), not at depth==0 — see `docs/plans/m1.g.md` §3 "Recursion-base contract" for the why.

**Test fixtures.** `tests/fixtures/perft_canonical6.txt` (canonical 6 D1–D6) and `tests/fixtures/perft_whittington.epd` (174 positions D1–D4) regenerated from Stockfish 18 by `scripts/regen-perft-fixtures.sh`. Per ADR-0006 Stockfish is the sole oracle; we use the Whittington FEN list as a diversity corpus and regenerate counts ourselves. Test partition: D1–D3 + light-D4 default-fast; heavy-D4 + D5 + D6 + Whittington `#[ignore]`-gated.

**Benchmarking.** `benches/perft.rs` (criterion: starting D4 plain + bulk, Kiwipete D3 bulk) and `benches/movegen.rs` (`generate_moves` per canonical-6 position). Baseline numbers in `bench/<milestone>.md` per ADR-0010.

See `docs/plans/m1.g.md` for the full design.

## Hot-path implications of Apple Silicon target

- **No PEXT / BMI2** — but we're not using them anyway (magic bitboards don't need them).
- **ARM NEON** is the relevant SIMD ISA. Matters when NNUE inference arrives; irrelevant for v1 classical eval.
- Apple's `aarch64` is `LITTLE_ENDIAN`, `64-bit`, with strong unaligned load support — no portability guards needed for v1.
- Profiling: `samply` (good flamegraphs), Instruments (Time Profiler), `cargo bench` with `criterion`.

## Out of scope

See ADR-0001 for variant chess; ADR-0002 for the mobile-as-downstream stance. Also explicitly not designed for: multiple board sizes, fairy pieces, drops, distributed search, GPU acceleration. NNUE-readiness specifically is satisfied by the make/unmake discrete-function shape — see ADR-0004; no abstraction pre-built today.
