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
| Evaluation future | NNUE — planned milestone (M10), not optional | ADR-0004 |
| Make/unmake structure | Single function calls; clean interception point for future NNUE accumulator | ADR-0004 |
| Strength dial | Planned milestone (M8); reuses the eval/move-selection function-call discipline | ADR-0005 |
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
| EPD diagnostic suites | `src/bin/epd-suite.rs` + `scripts/epd-suite.sh` drive WAC (300 tactical) + STS (1500 strategic, 15 themes) per-position correctness scoring. Vendored corpora at `bench/data/{wac,sts}.epd`. Backfill table at `bench/epd-suites.md`. Complementary to SPRT: absolute correctness vs relative game-strength. Per-theme STS breakdown becomes load-bearing for M6 eval-term validation. | `bench/epd-suites.md`, `docs/plans/tooling-epd-suites.md` |

## Current design surface

Each row points to the dedicated section in this file (and the canonical ADR / plan) for full detail.

| Area | One-line shape | See |
|---|---|---|
| Position layout | 6+2 bitboards + mailbox + cached king squares + aux + Polyglot Zobrist + tapered eval triple `(static_mg_white, static_eg_white, raw_phase)` | "Position layout" below; ADR-0009, ADR-0031 |
| FEN parsing | Strict per Edwards 1994 §16.1 (single-space separator, strict-decimal integers, structural sanity); phantom-EP sanitized to `None` per ADR-0015 | `docs/plans/m1.b.md`; ADR-0015 (phantom-EP only) |
| Move encoding | 16-bit `Move(u16)`: from(6) / to(6) / flag-nibble(4); 14 valid flag codes | `docs/research/m1-engine-architecture.md` §2 |
| Sliding-piece attacks | Fancy magic bitboards, variable shift (~840 KiB) | "Sliding-piece attack lookup" below; ADR-0008 |
| Position hashing | Polyglot 781-key Zobrist; pseudo-legal-only EP rule; incremental in `make_move` | "Position hashing" below; ADR-0009 |
| Make/unmake | Free fns + `Position::*` delegates; incremental Zobrist + tapered `(mg, eg, raw_phase)` with debug round-trip | "Make / unmake" below; ADR-0004, ADR-0031 |
| Move generation | Legal-direct, mask-based, with check-evasion specialization | ADR-0007 |
| Perft validation | `perft` / `perft_bulk` / `divide` / `perft_categorized` against Stockfish 18 fixtures | "Perft validation" below; ADR-0006 |
| UCI move encoding | `Move::to_uci` / `Move::from_uci`; generate-and-match parsing | `docs/plans/m2.a.md` |
| UCI command parser | `parse_uci_line(&str) -> Command`; total function (no panics) | `docs/plans/m2.b.md` |
| UCI engine I/O loop | Reader thread → mpsc → orchestrator + per-`go` worker; `Arc<AtomicBool>` cancellation | ADR-0011 |
| Search trait | `Search` trait + `SearchContext` + `SearchLimits` + `SearchResult` (unchanged since M2.C) | ADR-0011; "Search v1" below |
| UCI options | `Random_Seed` (M2.D), `MoveOverhead` (M3.E), `Hash` (M4.A) | `docs/plans/m2.d.md`, ADR-0017, ADR-0018 |
| Evaluation v2 | Tapered PeSTO MG+EG material+PST (`PSQT_MG`/`PSQT_EG` const tables) blended by phase + bishop pair + KBvKB-same-color + mop-up | "Evaluation v2 — tapered" below; ADR-0031 (supersedes ADR-0014 §1/§5) |
| Pawn-structure infra (M6.B) | Pawn-only Zobrist substream `Position::pawn_zobrist` (structural 3-XOR, Polyglot pawn keys); 4 MiB search-owned always-replace `PawnHashTable` on `AlphaBetaMover` (cleared in `Search::reset`); isolated/doubled/backward/connected predicates + passed detection; `evaluate_core`/`evaluate`/`evaluate_cached` (pure accelerator, D6). **Shipped config: `PAWN_STRUCTURE_IN_EVAL = true`, CONN-only — `ISO/DBL/BWD` weights zeroed in `eval::data` (every multi-term subset collapses via an ISO×CONN connectivity double-count; CONN-only Δ Elo +45.42 vs `M6.A`). M6.I re-introduces ISO/DBL/BWD via joint Texel + rescales CONN.** | "Pawn-structure infra" below; ADR-0032 (§7) |
| Passed-pawn term (M6.C) | `pawns::passed_pawn_term_white` (rank bonus + EG three-state path + EG king-tropism), reads M6.B's cached `passed[2]`, **computed live in `evaluate_core` — never cached** (king-distance/path are not pawn-only, ADR-0032 §3); blend-numerator only, not `static_eval_white`/mop-up (§4). **Shipped score-neutral: every passed weight zeroed in `eval::data` ⇒ term ≡ (0,0) ⇒ `evaluate` byte-identical to `M6.B`** (a three-config screen ladder proved the literature defaults a scale-invariant structural mismatch; whole weight set → M6.I). Term math live at zero weight, M6.I-ready. | "Passed-pawn term" below; ADR-0032 (§8) |
| Piece-mobility term (M6.D) | `eval::mobility::mobility_term_white(pos) -> (i32, i32)` — N/B/R/Q `popcount(piece_attacks(sq, occ_all) ∩ area)`, `area = !(own_occupied ∪ enemy_pawn_attacked)`, per-kind MG/EG tables; **computed live in `evaluate_core` — never cached** (not pawn-only, the ADR-0032 §3 class); blend-numerator only, not `static_eval_white`/mop-up (the ADR-0032 §4 boundary class). Sliders use full `occ_all` (enemy-non-pawn first-blocker counted); pins scored **pseudolegally**; **no x-ray** — deliberate roadmap-committed deferrals. King/pawn excluded. **Shipped score-neutral: all 8 `*_MOBILITY_*` weights zeroed in `eval::data` ⇒ term ≡ (0,0) ⇒ `evaluate` byte-identical to `M6.C`** (the landing-gate + full §11 per-kind screen proved the Stockfish-HCE literature defaults a scale-invariant structural mismatch with our PeSTO PSTs — co-scale ×0.5 *worsened* to −220; whole weight set → M6.I per-kind reshape). `MOBILITY_IN_EVAL=true`, term math live at zero weight, M6.I-ready. **No ADR** — roadmap M6.D row + `docs/milestones/m6.d.md` commit the semantic (ADR-0032 is pawn-structure-scoped). | "Piece-mobility term" below; roadmap §M6 / m6.d.md |
| King-safety term (M6.E) | `eval::king_safety::king_safety_term_white(pos) -> (i32, i32)` — king-zone attacker S-curve (`KING_SAFETY_TABLE[units.clamp(0,99)]`, units = Σ per-kind `KING_ATTACK_WEIGHT·popcount(attacks & king_zone)`, gated `<2 attackers ∨ no queen`, MG-only) + castled pawn-shield (SHIELD_1/2, relative-rank mirror) + MG-only open/semi-open-file penalty (both-adjacent amplifier). Zone via the single-source `pub(crate) king_zone(side,ksq)` = `ring \| (fwd & !from_square(ksq))` (king square excluded; 11 central / 8 back-rank / 5 corner). **Computed live in `evaluate_core` — never cached** (not pawn-only — king square + attacker squares; ADR-0033 §6 **supersedes ADR-0032 §3**'s pawn-shield-cache reservation as a correctness hazard). Blend-numerator only, not `static_eval_white`/mop-up (the ADR-0032 §4 boundary class). Pawn-storm omitted (ADR-0033 §3 — documented gap). **Shipped score-neutral: all 13 king-safety weights zeroed in `eval::data` ⇒ term ≡ (0,0) ⇒ `evaluate` byte-identical to `M6.D`**. **No SPRT screen ladder** (the M6.C/M6.D divergence, owned — ADR-0033 §8): research transfer-risk HIGH (PeSTO king-PST double-count) + the three-phase law + SPRT-noisiness + coupled-by-design components ⇒ negative-EV screen; line-296 satisfied vacuously (inert ⇒ no Elo claim). `KING_SAFETY_IN_EVAL=true`, term math live at zero weight, M6.I-ready. ADR-0033 (binds on M6.E per roadmap §M6). | "King-safety term" below; ADR-0033 / m6.e.md |
| Tier-1 HCE features (M6.F) | `eval::tier1`: (1) **`outpost_term_white`** iterating the `pub(crate) outpost_squares(pos,side) -> Bitboard` seam (`!*_attack_front_spans(enemy_pawns)` ∩ own-pawn-defended), then the `(3..=5)` relative-rank gate + per-kind per-rank `OUTPOST_{KNIGHT,BISHOP}_{MG,EG}` tables — **the gate is correctness-load-bearing** (the span-complement is a valid hole test only in the enemy half); (2) **`rook_file_term_white`** — `file_fill` projection (`(file_fill(own_pawns) & from_square(rook)).any()`), open > semi-open precedence; (3) **`endgame_scale`** — numerator over the **structural** `EG_SCALE_DEN`, applied `blended * scale / EG_SCALE_DEN` **before** `+ mop_up` (`is_ocb_with_pawns` exactly-1-bishop-each opposite complexes + no Q/R + ≥1 pawn; `is_pawnless_drawish` narrow KNNvK/balanced-≤1-minor accept-list, KBNvK/KBBvK excluded as wins; 50-move ramp; `is_kbkb_same_color` already in M6.A — **not** duplicated, disjoint). **Computed live in `evaluate_core` — never cached** (the ADR-0033 §6 live-term class); additive terms blend-numerator only; the scale multiplies the blend numerator only — never `static_eval_white`/mop-up (the ADR-0032 §4 boundary, the **first multiplicative construct** so it gets its own `endgame_scale_excludes_static_accessor_vs_m6e` pin). **Shipped score-neutral inert per the M6.C/M6.D/M6.E precedent: all additive weights zeroed + every scale tunable == `EG_SCALE_DEN` (scale ≡ identity) ⇒ outpost/rook-file ≡ (0,0), `endgame_scale ≡ EG_SCALE_DEN`, `blended·D/D == blended` byte-exact ⇒ `evaluate` byte-identical to `M6.E` ⇒ bench `1213649`/d4 `90591` byte-for-byte (deterministic ×2) ⇒ provably inert ⇒ no confirmation SPRT, no SPRT screen ladder.** `TIER1_IN_EVAL=true`, term math live at the inert config, M6.I-ready. **Endgame-scaling inert-vs-live open ADR question resolved inert-per-precedent** (ADR-0034 §4 — roadmap-stated default; the reinforcing optimizer argument is conditional on the open M6.I optimizer ADR and honorable at M6.I without M6.F landing live; dominated correctness-timing axis costs zero strength — no rated play in the M6.F→M6.G→M6.I gap; owned, not papered). ADR-0034 (binds on M6.F per roadmap §M6). | "Tier-1 HCE features" below; ADR-0034 / m6.f.md |
| Tuning corpus infra (M6.G) | Reusable game-result-labeled position corpus (data infra, **no `evaluate`/`Position`/search touch — no engine build, no bench**): CCRL + band-filtered Lichess + diversified-opening-book clawfish self-play + label-verified Zurichess quiet set; clock-loss / time-forfeit exclusion + TC-class filter; quiet-position extraction (quiet *definition* pinned by M6.I's tuner qsearch — an M6.G↔M6.I interface contract); opening-ply skip; FEN dedup / per-FEN caps; ADR-0003 label-provenance audit (game-result labels ONLY); held-out deployment-distributed self-play validation set; frozen snapshot + manifest + RNG seeds + re-run script vendored in `bench/`. **Gate: data-quality checks, NOT SPRT** (the M5.E correctness-only-gate precedent applied to data; no Elo claim). Consumers: M6.I, the tuning-backlog "PST co-tuning" Arm B, future SPSA campaigns, M10 NNUE data-prep. | "Tuning corpus infra" below; roadmap §M6 (M6.G scope detail) / ADR (allocated at landing) |
| Production search | `AlphaBetaMover`: fail-soft negamax + qsearch (M3.D + M5.E refinements + M5.F TT participation) + ID + caps (M3.E) + TT (M4.A + M5.F qsearch tier) + killers (M4.B) + history (M4.C) + aspiration (M4.D) + NMP (M5.A) + RFP (M5.B) + LMR (M5.C) + FFP (M5.D) + SE (M5.G) + staged movegen (M5.H1 architecture, eager generation; M5.H2 will enable lazy generation); `bench` regression baseline (M3.F) | "Search v1" below; ADR-0016, ADR-0017, ADR-0018, ADR-0019, ADR-0023, ADR-0024, ADR-0025, ADR-0026, ADR-0027, ADR-0028, ADR-0029, ADR-0030 |
| Transposition table (M4.A + M5.F) | `TranspositionTable` in `src/tt.rs`: `UnsafeCell<Vec<TtEntry>>` + `AtomicUsize` mask + `AtomicU8` generation; depth-preferred + age-bias replacement; full 64-bit Zobrist key; mate-score depth-adjustment; bound-aware probe with cutoffs at non-PV nodes (negamax) and unconditionally (qsearch); `Hash` UCI option (default 16 MiB, range 1–4096). M5.F: qsearch participates with `depth = 0` entries; `is_empty()` discriminator changes to `key == 0`; non-terminal qsearch stores only Lower/Upper (no Exact, per Stockfish 45e5e65). | "Transposition table" below; ADR-0018, ADR-0028 |
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
- Tapered eval triple `(static_mg_white: i32, static_eg_white: i32, raw_phase: u8)` — white-perspective MG/EG material+PST plus the material phase tag, maintained incrementally; blended at `evaluate()` time (ADR-0031, supersedes ADR-0014).

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

## Evaluation v2 — tapered (M6.A; ADR-0031, supersedes ADR-0014 §1/§5)

**Public API.** The `eval` module exposes:

- `evaluate(&Position) -> i32` — side-to-move-relative centipawn score. Extended insufficient-material override returns 0 for KvK / KvN / KvB / **KBvKB-same-color**; otherwise blends MG/EG by phase, adds bishop-pair + mop-up addends, flips for Black. Debug-only mate-band assert `result.abs() < MATE_IN_MAX_PLY - 1`.
- `eval_state_from_scratch(&Position) -> (i32, i32, u8)` — `pub(crate)` single-pass from-scratch recomputation returning `(mg_white, eg_white, raw_phase)`. Used by `Position::refresh_static_eval()` and the debug round-trip asserts in `make_move` / `unmake_move`.
- `pub(crate)` helpers `bishop_pair_term_white`, `center_manhattan_distance`, `chebyshev_distance`, `mop_up_term_white` (the last three test-visible for mutation coverage).

**Tapered representation.** `Position` carries the triple `(static_mg_white: i32, static_eg_white: i32, raw_phase: u8)` (replaces M3.A's single `static_eval_white: i32`). `raw_phase` accumulates `PHASE_DELTA[kind]` (`[P=0, N=1, B=1, R=2, Q=4, K=0]`, sums to 24 at the start); promoted-queen positions may exceed 24 — **clamp `min(24)` only at blend time**, never during accumulation. The blend: `((mg + bp_mg) * p + (eg + bp_eg) * (24 - p)) / 24 + mop_up` with `p = raw_phase.min(24)`, Rust truncation. A blended `Position::static_eval_white()` accessor (same signature as M3.A's) returns the phase-blended white-perspective score sans bishop-pair/mop-up — consumed transparently by RFP/NMP/FFP in `negamax`.

**PSQT lookups.** Two compile-time tables `pub(crate) const PSQT_MG / PSQT_EG: [[[i32; 64]; 6]; 2]`, both built by `const fn build_psqt_table(tables, material)`. White lookup `+(material[kind] + pst[kind][sq ^ 56])`; Black `-(...)`. The `s ^ 56` flip converts LERF (a1=0) to PeSTO a8-origin.

**Vendored data (`src/eval/data.rs`, `cargo mutants`-excluded — vendored, not logic):**

- `MATERIAL` = `[82, 337, 365, 477, 1025, 0]` (MG), `MATERIAL_EG` = `[94, 281, 297, 512, 936, 0]` — PeSTO; king material 0.
- Six `MG_*_PST` + six `EG_*_PST` `[i32; 64]` arrays — PeSTO MG+EG tables, vendored verbatim from Ronald Friederich's CPW post.
- `PHASE_DELTA: [u8; 6]`.

**Piggyback terms (at the `evaluate()` boundary, not incrementally tracked):**

- **Bishop pair** — `+30 MG / +50 EG` when a side covers *both* colour complexes (a light-square AND a dark-square bishop). Literature defaults; Texel-calibrated in M6.I.
- **KBvKB-same-color** — `is_kbkb_same_color`: total_count==4, each side exactly one bishop, both on the *same* complex → 0. The *logical opposite* of the bishop-pair predicate (distinct named functions to prevent the inversion bug).
- **Mop-up** — `4·CMD(losing_king) + 2·(14 − chebyshev(kings))`, signed for the winning side, gated `raw_phase < MOP_UP_PHASE_MAX = 5` ∧ `|blended advantage| > 100 cp`. `MOP_UP_PHASE_MAX = 5` (not 4) is load-bearing — KQK has `raw_phase = 4` (lone queen); a `< 4` gate would exclude the canonical KQK conversion case.

**Insufficient-material draws.** `total==2`→KvK; `total==3 ∧ (#N==1 ∨ #B==1)`→KvN/KvB; `total==4 ∧ is_kbkb_same_color`→KBvKB-same-color. All before the blend (a `phase`-gated path must not pre-empt these).

**Incremental update site.** The triple is maintained by `update_static_eval_after_make` in `src/mov.rs` — each M3.A PSQT line splits into an MG line + an EG line; per-arm `raw_phase` delta (EP arm contributes literal `0`, not a `-= PHASE_DELTA[Pawn]` no-op). `Undo` carries `prior_static_mg / prior_static_eg / prior_raw_phase`; `unmake_move` restores via `refresh_static_eval_from_triple`. Round-trip debug asserts compare the triple against `eval_state_from_scratch`. The always-on `make_move_no_from_scratch_in_release` perf sentinel guards against accidental from-scratch reintroduction of either zobrist or eval on the hot path.

**NNUE-readiness (ADR-0004 extension).** When NNUE arrives at M10, the tapered incremental update slots out for the accumulator update; `make_move` / `unmake_move` signatures stay. Exactly the discrete-function shape ADR-0004 designed for.

See `decisions/0014-eval-material-pst.md` and `docs/research/m3-eval-material-pst.md`.

## Pawn-structure infra (M6.B; ADR-0032)

**Pawn-only Zobrist substream.** `Position::pawn_zobrist: u64` = XOR of
`zobrist::piece_key(pawn, sq)` over all pawns, side-to-move excluded.
Maintained by the **structural three-XOR form** in `update_zobrist_after_make`
(pawn-mover out@`from`; pawn-non-promo in@`to`; pawn-victim out@`capture_sq`) —
no per-`MoveFlag` match; EP's two-pawn delta + all promo arms correct by
construction (a promo-capture victim is never a pawn — back-rank geometry).
`Undo.prior_pawn_zobrist`; from-scratch round-trip `debug_assert` in
make/unmake/make_null_move; FEN-parse path refreshes it adjacent to every
`refresh_zobrist`.

**Pawn hash.** `PawnHashTable` — fixed 4 MiB (`#[repr(C)]` 32-byte entries,
2¹⁷, `const`-size-pinned), always-replace, owned by `AlphaBetaMover`, cleared
in `Search::reset()` (ucinewgame + per bench position). `key == 0` never
cached. `evaluate_cached(pos, &mut PawnHashTable)` (qsearch hot path) is a
**pure accelerator**: `evaluate_cached == evaluate` for all positions / any
hash state (D6 proptest).

**Shipped config — CONN-only.** `const PAWN_STRUCTURE_IN_EVAL: bool` in
`eval::evaluate_core` gates the `(pe.mg, pe.eg)` fold into the blended score;
**M6.B ships it `true`** with `ISO_MG=ISO_EG=DBL_MG=DBL_EG=BWD_MG=BWD_EG = 0`
in `eval::data` (CONN keeps its literature-default table). The all-four
literature-default SPRT vs `M6.A` was H0 / −99.88; a per-term screen +
confirmation showed every term individually positive-to-neutral but every
multi-term subset collapses via a **catastrophic ISO×CONN double-count of the
connectivity axis** (ISO+CONN alone = −197.94). CONN-only is the largest
confirmed gain and structurally interaction-immune; landing-gate mixed-TC
SPRT vs `M6.A` (elo0=0/elo1=5, seed `…014`) = `continue@400-cap`, **Δ Elo
+45.42 [+19.68, +71.67]**, lands by the M5.F/M5.G-v2 outcome-ladder
precedent (ADR-0032 §7). bench `1213649` / depth-4 `90591`. Caveat: per-TC
depth reversal (fast-TC strong+, 60+0.6 negative) — M6.I watch-item. The
zeroed ISO/DBL/BWD constants + term math stay (M6.I-ready); `pe.passed` is
cached for M6.C. **M6.I re-introduces ISO/DBL/BWD via joint Texel against the
CONN-only baseline and rescales CONN** (the gate is already live).

**Passed-pawn term (M6.C; ADR-0032 §8) — shipped score-neutral.**
`pawns::passed_pawn_term_white(pos, &pe.passed) -> (i32, i32)` adds a
per-passer rank bonus + EG three-state path discriminator (front-span empty
of all pieces → +Δ, enemy piece on it → −Δ, friendly-only → 0) + EG
king-tropism (rank-scaled Chebyshev to the promotion square, clamped at
`PASSED_KDIST_CAP=5`). **Invariant: computed live in `evaluate_core`, never
cached** — king-distance and path depend on non-pawn state, so the term is
*not* pawn-only and must not enter `PawnEval`/the pawn hash (ADR-0032 §3). It
enters the blend numerator only (gated by `const PASSED_PAWNS_IN_EVAL: bool`,
shipped `true`) — never `static_eval_white()` (the pruning input) nor the
mop-up estimate (ADR-0032 §4 / D7). **Shipped config: every passed weight
zeroed** (`PASSED_MG=PASSED_EG=PASSED_FREE_EG_DELTA=PASSED_KDIST_OWN_PER_STEP=
PASSED_KDIST_ENEMY_PER_STEP = 0`; `PASSED_KDIST_CAP=5` kept as the named
structural clamp) ⇒ the term ≡ `(0,0)` ⇒ `evaluate` byte-identical to `M6.B`
(the score-neutral landing gate — provably inert, no confirmation SPRT, the
M6.B precedent). A three-config screen ladder proved the literature defaults
have a scale-invariant structural mismatch with this engine (KDIST
slow-TC-toxic; RANK+PATH fast-TC over-magnitude vs the PeSTO EG pawn PST;
`{RANK+PATH}/2` co-scale migrates the failure — the M6.B `(ISO+CONN)/2`
plateau). Term math live at zero weight (M6.I-ready, the
`PAWN_STRUCTURE_IN_EVAL` precedent). **M6.I re-derives the rank table against
our PeSTO EG pawn PST and reshapes (not rescales) king-distance, jointly with
the ISO/DBL/BWD + CONN obligation.**

See `decisions/0032-pawn-structure-and-pawn-hash.md` (§7 CONN-only, §8
passed-pawn score-neutral) and `docs/research/m6-pawn-structure.md` +
`docs/research/m6-passed-pawns.md`.

**Piece-mobility term (M6.D; no ADR — roadmap-committed) — shipped
score-neutral.** `eval::mobility::mobility_term_white(pos) -> (i32, i32)`
adds a white-perspective N/B/R/Q mobility term: per piece,
`popcount(piece_attacks(sq, occ_all) ∩ area)` indexes a per-kind MG/EG
table; `area = !(own_occupied ∪ enemy_pawn_attacked)` (friendly-occupied and
enemy-pawn-attacked squares excluded; friendly-pawn-attacked-empty and
enemy-non-pawn-occupied squares kept — captures count as mobility). Sliders
use full `occ_all` (the first enemy non-pawn blocker is counted). King and
pawn mobility excluded. Pins scored **pseudolegally** and **no x-ray**
through friendly sliders — deliberate roadmap-committed deferrals (the eval
leaf has no movegen context; M6.I absorbs the average over-credit). The
index-bound invariant (empty-board geometric maxima 8/13/14/27 = table top
indices; `occ_all`/`& area` only reduce) is a `debug_assert!`, not a clamp
(the M6.A trust-the-invariant-in-release discipline). The term is **not
pawn-only** ⇒ never cached / never in `PawnEval`/the pawn hash (the
ADR-0032 §3 class). It enters the blend numerator only (gated by
`const MOBILITY_IN_EVAL: bool`, shipped `true`) — never `static_eval_white()`
(the RFP/NMP/FFP pruning input) nor the mop-up estimate (the ADR-0032 §4
boundary class; pinned by `static_accessor_excludes_mobility` +
`mop_up_addend_excludes_mobility_vs_m6c`). **Shipped config: all 8
`KNIGHT/BISHOP/ROOK/QUEEN_MOBILITY_{MG,EG}` arrays zeroed in `eval::data` ⇒
the term ≡ `(0,0)` ⇒ `evaluate` byte-identical to `M6.C`** (the landing-gate
+ the full pre-committed §11 per-kind screen vs `M6.C` proved the
Stockfish-HCE literature defaults a scale-invariant structural mismatch with
our PeSTO PSTs: all-four −131.62 H0; per-kind all non-positive; ×0.5
co-scale −220.18 *worsened*; no positive interaction-immune subset). Term
math live at zero weight (M6.I-ready, the M6.B/M6.C `*_IN_EVAL` precedent).
**M6.I re-derives the entire N/B/R/Q mobility weight set against our PeSTO
PSTs (per-kind reshape, not a global rescale — the co-scale-worsens
verdict), jointly with the M6.B ISO/DBL/BWD + CONN and the M6.C passed-pawn
obligations (one joint pass).** The dominant offender was the slider EG
*magnitude* (`ROOK_EG`→169/`QUEEN_EG`→199 vs Stockfish's flatter PSTs), not
the research-predicted N/B PST double-count.

See the roadmap M6.D row + `docs/milestones/m6.d.md` (semantic + screen
ledger + the M6.I reshape brief) and `docs/research/m6-mobility.md`. **No
separate ADR** — ADR-0032 is pawn-structure-scoped; mobility is a distinct
roadmap-committed concern.

**King-safety term (M6.E; ADR-0033 — supersedes ADR-0032 §3) — shipped
score-neutral.** `eval::king_safety::king_safety_term_white(pos) -> (i32,
i32)` adds a white-perspective term, ±sign per defended king, with three
components: (a) an **attacker S-curve** — per enemy N/B/R/Q,
`KING_ATTACK_WEIGHT[k]·popcount(piece_attacks(sq, occ_all) & king_zone)`
summed to `units`, gated `attackers < 2 ∨ no enemy queen`, indexed
`KING_SAFETY_TABLE[units.clamp(0,99)]`, MG-only; (b) a **castled pawn-shield**
(king file `≥ f ∨ ≤ c`; per king-file±1: rank-2 → SHIELD_1, else rank-3 →
SHIELD_2; relative-rank mirror White r2/r3 ↔ Black r7/r6); (c) an **MG-only
open/semi-open king-file + adjacent-file** penalty with the
both-adjacent-semi-open amplifier. The king-zone is the single-source-of-truth
`pub(crate) king_zone(side, ksq)` = `ring | (fwd & !Bitboard::from_square(
ksq))` (`ring = king_attacks(ksq)`; `fwd = ring.shift_north()` White /
`shift_south()` Black) — 11 squares central / 8 back-rank / 5 corner, **king
square excluded** (the `& !from_square(ksq)` mask removes the square the
toward-enemy shift re-introduces for a non-back-rank king). Pawn-storm
omitted (ADR-0033 §3 — a documented strategic gap). The term is **not
pawn-only** (king square + every attacker's square) ⇒ **computed live in
`evaluate_core`, never cached / never in `PawnEval`/the pawn hash** — this is
the ADR-0032 §3 class, and **ADR-0033 §6 supersedes ADR-0032 §3's
forward-looking "M6.E will extend the [pawn-hash] entry with pawn-shield
masks" reservation** (caching a king-file-dependent value under a pawn-only
Zobrist key would be a correctness hazard; ADR-0032's pawn-hash entry /
substream / §4 boundary rule are otherwise unaffected). It enters the blend
numerator only (gated by `const KING_SAFETY_IN_EVAL: bool`, shipped `true`) —
never `static_eval_white()` nor the mop-up estimate (the ADR-0032 §4 boundary
class; pinned by `static_accessor_excludes_king_safety` +
`mop_up_addend_excludes_king_safety_vs_m6d`). `units.clamp(0,99)` is a real
clamp (the saturating S-curve tail is by design — research §6), unlike M6.D's
`debug_assert!` index invariant. **Shipped config: all 13 king-safety weight
constants zeroed in `eval::data` ⇒ the term ≡ `(0,0)` ⇒ `evaluate`
byte-identical to `M6.D`.** Unlike M6.B–D this phase ran **no SPRT screen
ladder** (ADR-0033 §8; the M6.C/M6.D divergence, owned, not papered as
precedent): research transfer-risk verdict HIGH (PeSTO MG king PST already
prices ~30–50 cp of castled-king safety — the dominant double-count axis,
structurally the M6.D PST-double-count finding but stronger) + the M6.B→C→D
three-phase law + king-safety's universal SPRT-noisiness (mixed-TC screens
mandatory, ~5× M6.B's single-TC cost) + components coupled-by-design (no
interaction-immune subset to find) ⇒ the screen's only outcome is "defer," at
the highest screen cost of any M6 term ⇒ negative expected value. Roadmap §M6
line-296 is satisfied **vacuously** for an inert landing (eval byte-identical
to `M6.D` ⇒ no Elo claim ⇒ SPRT measures zero by construction — the settled
M6.C/M6.D disposition). Term math live at zero weight (M6.I-ready, the M6.B–D
`*_IN_EVAL` precedent). **M6.I re-derives the entire king-safety weight set
(S-curve table + per-kind attacker weights + shield + open-file + MG/EG
split) against our PeSTO PSTs, jointly with the M6.B ISO/DBL/BWD + CONN, the
M6.C passed-pawn, the M6.D mobility, and the M6.F outpost/rook-file/endgame-scaling obligations (one joint pass).**

See ADR-0033 + the roadmap M6.E row + `docs/milestones/m6.e.md` (the
no-screen divergence rationale + the M6.I reshape brief) and
`docs/research/m6-king-safety.md`.

**Tier-1 HCE features (M6.F; ADR-0034; binding research
`docs/research/m6-remaining-hce-features.md`) — shipped score-neutral inert
per the M6.C/M6.D/M6.E precedent.** `eval::tier1`, three constructs added
*before* M6.I's joint Texel pass so they co-calibrate in the one tune (the
"extend then tune" law — Texel cannot discover absent features):

- **`outpost_term_white(pos) -> (i32,i32)`** iterating the `pub(crate)
  outpost_squares(pos, side) -> Bitboard` **seam** (the M6.E
  `king_zone`-seam mandate — the structural test pins the *real* selector,
  not an inline reconstruction; without it the per-side span / `& vs |` /
  sign mutants are killed by no test at the inert config). `outpost_squares`
  = `!*_attack_front_spans(enemy_pawns)` (the "hole") ∩ the side's immediate
  pawn-attack set ("supported"). The per-piece loop applies the **`(3..=5)`
  relative-rank gate** then indexes per-kind per-relative-rank
  `OUTPOST_{KNIGHT,BISHOP}_{MG,EG}` tables. **The gate is
  correctness-load-bearing, not a removable filter**: the
  front-span-complement is a valid "no enemy pawn can ever challenge" test
  *only* for enemy-half squares.
- **`rook_file_term_white(pos) -> (i32,i32)`** — `file_fill` projection;
  the rook's own square is in `file_fill(own_pawns)` iff a pawn is on its
  file (`(own_files & Bitboard::from_square(sq)).any()`) — **no per-file
  mask helper exists or is needed**. Open (`!on_own ∧ !on_enemy`) >
  semi-open (`!on_own`) precedence.
- **`endgame_scale(pos) -> i32`** — a numerator over the **structural**
  denominator `EG_SCALE_DEN` (not a tunable); `evaluate_core` applies
  `blended * scale / EG_SCALE_DEN` **before** `+ mop_up` (mop-up is the
  unscaled won-endgame conversion nudge). `is_ocb_with_pawns` (exactly one
  bishop each, opposite complexes, no Q/R, ≥1 pawn) + `is_pawnless_drawish`
  (the narrow KNNvK / balanced-≤1-minor accept-list — **KBNvK/KBBvK
  explicitly EXCLUDED as forced wins**; the KBvK/KNvK overlap with
  `is_insufficient_material` is unobservable in eval, the `eval.rs:228`
  early-return precedes the scale) + the 50-move ramp. **KBvKB-same-color
  is already in M6.A (`is_insufficient_material`) — not duplicated, the
  predicates are disjoint by construction.**

All computed live in `evaluate_core`, never cached (the ADR-0033 §6
live-term class). Additive terms blend-numerator only; the scale multiplies
the blend numerator only — never `static_eval_white`/mop-up (the ADR-0032
§4 boundary; the **first multiplicative construct** in M6, so it gets its
own `endgame_scale_excludes_static_accessor_vs_m6e` boundary pin in addition
to the M6.E-mirrored accessor/mop-up tests). **Shipped score-neutral inert:
all additive weights zeroed + every scale tunable == `EG_SCALE_DEN` (scale ≡
identity) ⇒ outpost/rook-file ≡ (0,0), `endgame_scale ≡ EG_SCALE_DEN`,
`blended·D/D == blended` byte-exact ⇒ `evaluate` byte-identical to `M6.E` ⇒
bench `1213649`/d4 `90591` byte-for-byte (deterministic ×2,
orchestrator-re-verified) ⇒ provably inert ⇒ no confirmation SPRT, no SPRT
screen ladder; roadmap line-296 satisfied vacuously.** `TIER1_IN_EVAL=true`,
term math live at the inert config, M6.I-ready (the M6.B–E `*_IN_EVAL`
precedent). The outpost / rook-file weights enter the M6.I joint Texel pass;
`EG_SCALE_DEN` / `FIFTY_MOVE_TAPER_FROM` stay **structural** (out of the
tunable vector — the ADR-0034 §4 optimizer-linearity rationale). **The
endgame-scaling inert-vs-live open ADR question was resolved
inert-per-precedent** (ADR-0034 §4): the roadmap-stated default is inert;
the reinforcing optimizer-tractability argument is conditional on the
still-open M6.I optimizer ADR and is fully honorable at M6.I without M6.F
landing live (the scale coefficients can be fixed + excluded from the
tunable vector then); the dominated correctness-timing axis costs zero
measurable strength (no rated play in the M6.F→M6.G→M6.I gap); owned, not
papered (§4 honestly concedes live-with-fixed-coefficient dominates on 2 of
3 axes). Unlike M6.E there is **no diagnostic-screen-skip novelty** (M6.F
carries no SPRT gate by roadmap construction).

See ADR-0034, the roadmap M6.F row, `docs/milestones/m6.f.md`, and
`docs/research/m6-remaining-hce-features.md` (Priority-1 list + the "extend
then tune" law).

## Corpus construction (M6.G; ADR-0035)

Reusable game-result-labeled quiet-position corpus + the data-quality
landing gate. Data-infra phase: **no `evaluate` / `Position` / search
behavior change** — bench byte-identical to `M6.F`. The only engine touch
is an additive read-only `pub fn search::quiescence_eval_white` +
`pub struct QSearcher` seam (an opaque per-worker reusable wrapper around
the crate-private `AlphaBetaMover` so the seam does not leak engine
internals).

**Module layout.** `src/corpus/` is 12 modules:

| Module | Role |
|---|---|
| `mod.rs` | Shared types (`Label`, `Source`, `CorpusRecord`, `CorpusError`) + the **pinned M6.G↔M6.I interface constants** (`QUIET_MARGIN_CP=30`, `OPENING_SKIP_PLIES=8`, `HIGH_SCORE_CP=600`, `PER_GAME_CAP=10`). |
| `prng.rs` | SplitMix64; golden-pinned to identical literals as `elo_iterate::prng` (deliberate ~20-LOC dup keeps M6.G off the SPRT-critical harness). |
| `pgn.rs` | Streaming SAN PGN reader; disambiguates against the legal move set (ADR-0007); whole-game-drop on any parse failure. |
| `filter.rs` | Game/position admission (Termination, TC-class, Elo band, opening-skip, `|eval|` cutoff, in-check). |
| `quiet.rs` | The pinned quiet predicate `!in_check ∧ |static_eval − qsearch| < QUIET_MARGIN_CP`. |
| `selfplay.rs` | In-process deterministic fixed-depth self-play. **Fresh `AlphaBetaMover` per game** (R3). |
| `store.rs` | Per-game CRC-framed append-block log + checkpoint ordering (R1/R2). Frame: `MAGIC \| game_id \| rec_count \| payload_len \| payload \| crc32`. |
| `dedup.rs` | Bounded-memory external sort-merge FEN dedup; deterministic min-`(source, game_id, ply)` survivor. |
| `split.rs` | Game-level train/val split; held-out integrity (game-disjoint hard; FEN-leakage ratio ≤ τ). |
| `objective.rs` | Texel K-fit + logistic loss + stratified objective; **frozen-snapshot outpost stratum** (code-level copy of `eval::tier1::outpost_squares` at M6.F-snapshot time). |
| `manifest.rs` | Hand-rolled SHA-256 + JSON (no `serde`/`sha2` dep); manifest + filter_spec writers. |
| `quality_gate.rs` | The six §6 data-quality checks (3 must-PASS — landing gate). |

**Interface contract (the load-bearing invariant; M6.I reads these).**

- **Pinned constants** in `src/corpus/mod.rs`: `QUIET_MARGIN_CP=30`, `OPENING_SKIP_PLIES=8`, `HIGH_SCORE_CP=600`, `PER_GAME_CAP=10`, `FEN_LEAKAGE_TAU=0.05` (echoed into `bench/corpus/filter_spec.txt`).
- **Score function:** a quiet-certified position is scored by `corpus::quiet::static_eval_white` (= White-POV `evaluate`). M6.I does NOT re-run qsearch at tune time — Predicate B was chosen precisely so `static_eval ≈ qsearch` within margin, resolving research §2.5's open question.
- **Frozen outpost stratum:** `objective::frozen_outpost_squares` is a code-level snapshot of `eval::tier1::outpost_squares` at M6.F-snapshot time, NEVER a live call (closes the M6.I re-tune circularity). The `frozen_outpost_squares_byte_equals_live_at_m6f_snapshot` test pins backward correctness over `bench::BENCH_POSITIONS` × both colors.
- **`Source` accept-list:** exactly `{SelfPlayOnBook, SelfPlayOffBook, Ccrl, LichessOpen}` — Zurichess intentionally absent. Self-play is split into two source variants by opening regime so the book / off-book proportion is a training-time per-source reweighting at M6.I rather than a corpus-generation knob (ADR-0035 §10). Three-layer programmatic defense in the ADR-0003 audit: (a) enum omission; (b) `Source::from_u8` returns `None` for `b ≥ 4` and `store::decode_block` rejects unrecognized frames; (c) `check_adr0003_audit` carries an unconditional (release-mode) `assert_eq!(accept_list.len(), 4)`.

**Crash-safety invariants (R1/R2/R3).** The shard is an append-only log of CRC-framed game blocks. A whole game is appended in one `write` + `fsync`; that is the atomic unit. `scan_valid_blocks` walks frames and truncates a torn final block **wholesale** (never line-by-line) at the last fully-valid byte. Ordering: game-block `fsync` THEN checkpoint `.tmp`→`fsync`→rename; resume skips already-present `game_id`s (idempotent). The `tests/corpus_crash_safety.rs::crash_kill_after_first_game_resumes_to_uninterrupted_corpus` integration test sends a real `libc::kill(pid, SIGKILL)`, resumes, and asserts shard-records *multiset byte equality* to an uninterrupted reference (with `--workers 1`, list-equality on disk).

**Determinism precondition (R3/R4).** `Search::go(SearchLimits{ depth: Some(d), nodes: None, movetime: None, infinite: false })` with `TimeCaps{ soft: MAX, hard: MAX }` makes `should_abort` only reachable via `ctx.stop`; a stop-aborted in-flight game is dropped (R2). Each completed game's move sequence is therefore a pure deterministic function of `(start_pos, depth, seed)` — load/suspend/renice-independent. **R4 / VirtualClock deviation owned in ADR-0035 §5**: fixed-depth is load-independent without a clock; ADR-0021 §4's cross-version-SPRT objection does not bite a one-frozen-build corpus generator; R-TC explicitly carves out fixed-shallow-depth corpus generation. Per-game fresh `AlphaBetaMover` (the R3 invariant — pinned by `fresh_vs_warm_searcher_same_seed_same_game_identical`).

**Reproducibility — every campaign knob pinned.** `bench/corpus/manifest.json` carries `self_play_seed`, `games`, `max_plies`, `opening_random_plies`, `workers`, `depth_ladder`, `split_seed`, `val_fraction`, `corpus_sha256`, and per-source provenance entries. `cmd_build` emits `train.bin`/`val.bin` with records sorted by `(game_id, ply, fen)` per game-block ⇒ corpus bytes are a deterministic function of the input multiset, independent of self-play worker scheduling. `bench/corpus/re-run.sh` reads every knob from the manifest (no `${X:-default}` shell substitution). `scripts/corpus.sh` is the operator fresh-build tool; `bench/corpus/re-run.sh` is the byte-identical-reproduction tool — the header of each documents the distinction.

**R-TC empirical anchoring.** The fixed-depth ladder is NOT plan literals. `corpus calibrate-ladder` runs `Search::go` at each canonical deployment movetime bucket {100, 200, 400, 600 ms} over `bench::BENCH_POSITIONS`, records the median completed iterative-deepening depth per bucket, and writes the `(depth, weight)` rungs to `filter_spec.txt`/`manifest.json`. Residual proxy caveat owned: depth is a *proxy* for TC; the rung↔bucket correspondence is the measured median (±1 ply at fast buckets). Re-runs on different hardware produce a different (machine-local) ladder — the vendored manifest's ladder is the dev-machine value, frozen.

**Two-pass discipline.** `selfplay` emits EVERY post-opening-skip position with the game label + `depth_rung`, transactionally per game — it does NOT apply the quiet predicate. The separate `corpus build` pass applies (in order) `static_eval_white` + `QSearcher::eval_white` → quiet predicate → `|eval|` cutoff → `strata_for` tagging → `dedup_fen` (deterministic survivor) → `per_game_cap` (seeded reservoir) → game-level `split_by_game` → emit train.bin + val.bin. This decouples `selfplay`/`store` from `quiet` (the §7 fan-out plan-edge is genuinely absent) and lets `build` apply per-worker-reused `QSearcher` (the R6/R7 invariant: never per-position allocation).

**Frozen artifact disposition.** Per plan §1, the committed `bench/corpus/` artifact is the bytes consumers freeze on. The self-play slice is additionally reproducible from `{seed + clawfish binary}` with no network. CCRL/Lichess slices are reproducible from raw sources *given source availability* — the weaker re-derivable guarantee — raw blobs are NOT git-vendored. The committed artifact at landing is **self-play-dominant** (12 games — sandbox-budget-limited; plan §1 gap-as-coverage-stat acknowledges this); for production-scale M6.I tuning the operator extends via `scripts/corpus.sh` with larger `GAMES` or stages CCRL/Lichess + runs `re-run.sh`.

**Consumers.** M6.I Texel tuning (next phase); the tuning-backlog "PST co-tuning Arm B" entry; future SPSA campaigns; M10 NNUE data-prep.

See ADR-0035, the roadmap M6.G row, `docs/milestones/m6.g.md`, `docs/research/m6-corpus-construction.md`.

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
10. **Construct `MoveStager` (M5.H1 v2 thin-wrapper)** — iterator over a single `Vec<Move>` sorted once by `negamax_move_order_score` with TT promotion to index 0. Yields the byte-equivalent move sequence the legacy `order_moves` produces (the v1 stage-state-machine implementation also yielded equivalent sequence but had ~3× per-node allocation pressure invisible to bench yet ~50 Elo regression in SPRT — ADR-0030 §10). v2 wraps M5.G's `order_moves` algorithm in the M5.H1 stager API (`new` / `next` / `peek` / `len` / `is_empty`); per-node cost matches M5.G exactly. The `searchmoves_filter: Option<&[Move]>` parameter folds the root-only UCI filter into construction (eliminates temporal coupling). `MoveStager` does NOT impl `Iterator` (preserves `len()` temporal contract: pre-iteration count, no decrement). `peek` is `&self`-receiver, type-enforced idempotent (load-bearing for the M5.G SE block's double-call). M5.H2 will swap the internal `Vec<Move> + sort` for per-stage lazy generation behind the same API. `negamax_move_order_score` is the comparator (production-callable) and the score-tier discipline pin (compile-time `_SCORE_TIER_INVARIANTS`). `order_moves` (`#[cfg(test)]`-only post-M5.H1) remains in the file for the 14+ existing test call sites. See "Move ordering" subsection above.
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

**Qsearch filter.** `qsearch_move_filter(mv) -> bool` accepts `Capture | EnPassant | QueenPromo | QueenPromoCapture`. Excludes under-promotions and under-promo-captures despite the latter being captures — under-promotion's tactical value at M3 strength was rare enough that the extra qsearch breadth wasn't justified. **M5.E refinements** (ADR-0027) close two horizon holes the filter introduces: (1) single-reply extension recurses on the unique legal quiet when `!in_chk && moves_vec.is_empty() && ml.len() == 1` (the unique move is provably non-promo by movegen invariant); (2) stalemate-conditional rook/bishop under-promo synthesizes `RookPromo`+`BishopPromo` variants of a queen-promo's `(from, to)` when the post-make child is stalemate (knight-promo stays out of scope; fork-tactic motivation is independent). Plus M5.E #2 corrects qsearch's `ml.is_empty() && !in_chk` branch to return `0` (stalemate draw) instead of stand-pat — the M3.D approximation. Plus M5.E #4: MAX_PLY ceiling guard at qsearch entry, `!in_check` arm only, defends against pathological forced-quiet chains under M5.E #1 (helper `qsearch_short_circuit_at_ply_ceiling` extracted for mutation-coverage discrimination).

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
