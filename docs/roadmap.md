# Roadmap

Milestone plan. Update as we complete or revise.

## Status

**M1 complete (M1.G landed); M2 next — UCI random mover.**

### M1.G — what landed

The `perft` module — the engine's move-generation validation + measurement layer:

- **`src/perft.rs`** with `perft`, `perft_bulk` (CPW depth-1 leaf-skip), `divide` (UCI-sorted), `perft_categorized` (internal-only category counts per ADR-0006), and the `PerftCounts` struct.
- **Stockfish-regenerated fixtures** at `tests/fixtures/perft_canonical6.txt` (canonical 6 D1–D6) + `tests/fixtures/perft_whittington.epd` (174 positions D1–D4). Per ADR-0006 Stockfish 18 is the sole oracle.
- **`scripts/regen-perft-fixtures.sh`** — idempotent fixture-regeneration script; spawns one Stockfish per (fen, depth) pair, parses `Nodes searched:` lines.
- **Test partition** matching the plan's §"Fast-suite wall-clock budget": D1–D3 + light-D4 default-fast (sub-second); heavy-D4 + D5 + D6 + Whittington D4 `#[ignore]`-gated.
- **`criterion 0.7` benchmark harness** at `benches/perft.rs` + `benches/movegen.rs`; first baseline committed at `bench/m1.g.md` per **ADR-0010** (this phase's binding ADR).
- **M1.F smoke benchmark deleted** — replaced by the criterion harness.

### M1.G — verification

| Metric | Result |
|---|---|
| Tests | 446 fast + 9 ignored (4 perft-integration ignored slow + 1 perft-unit Kiwipete D4 + 4 prior). All ignored slow tests verified to pass: D4-heavy 0.23s; D5 + Whittington 2.47s combined; **D6 117.93s**. |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo fmt --check` | clean |
| `cargo audit` + `cargo deny check` | clean (criterion + transitives MIT/Apache-2.0) |
| `cargo llvm-cov --summary-only --lib` | `perft.rs`: 98.32% region / 98.25% line / 93.75% function. Crate total 95.19%. |
| Headline perft throughput (Apple M4, release) | starting D4 plain **33 Mnps**; starting D4 bulk **119 Mnps**; Kiwipete D3 bulk **168 Mnps** — meeting the M1 ≥100 Mnps exit criterion on the bulk path. |
| Headline movegen throughput | 81–277 ns/call across canonical-6, ~200 ns/call typical. |
| D6 end-to-end | ~22B nodes / 117.93s ≈ ~187 Mnps (consistent with bench numbers). |

All four review loops (plan, test-suite, final code+tests, plus the post-final benchmark capture) converged. Plan archived at `docs/plans/m1.g.md`. ADR-0010 codifies the bench format.

### M1.F — what landed

The `movegen` module — the engine's legal-move-enumeration surface:

- **`MoveList`** — stack-allocated `[MaybeUninit<Move>; 256]` + `len: u16`. `push` and `clear` are `pub(crate)` so the soundness contract stays inside the crate; `as_slice` exposes `&[Move]` via a single justified `unsafe` block.
- **`generate_moves(pos, &mut MoveList)`** — legal-direct emission, mask-based, with check-evasion specialization (single check → king + capture-the-checker + block; double check → king-only).
- **`in_check(pos)`** — public side-to-move checker.
- **Per-call `MaskInfo`** — `checkers`, `pinned`, `capture_mask`, `push_mask`, `king_danger` (computed against `occupancy ^ king_bb` per the king-flee gotcha), `pin_rays[64]`. No cache on `Position`.
- **EP horizontal-pin filter** AND symmetric **diagonal discovery filter** at emission time (covers Position-3 trap and the diagonal counterpart).
- **Castling** — only when not in check; transit + destination squares not in `king_danger`. Mailbox `debug_assert!` on king/rook starting squares (release-trusted; the FEN parser validates at the boundary).
- **`validate_post_parse` extended** — rejects FENs whose castling rights don't match the king/rook mailbox; new `FenError::InconsistentCastlingRights`.

### M1.F — implementation highlights

- **Const-fn pre-computed leaf attack tables** (`PAWN_ATTACKS`, `KNIGHT_ATTACKS`, `KING_ATTACKS`) — built at compile time; no `LazyLock` overhead on the hot path.
- **Defensive-checks-debug-only convention** added to `docs/workflow.md` "Final review loop" → Code quality. Codified by the castling §13 invariant: validation at FEN parse, `debug_assert!` at consumers, release trusts.
- **`prop_no_legal_move_leaves_us_in_check`** — proptest with deterministic SplitMix64 random walk over §6 edge fixtures + canonical 6 seeds. Pins the legal-direct invariant against any future regression.
- **EP-double-check `count == 2` assertion** — in-crate test using crate-private `checkers_of` to verify §6.3 taxonomy.

### M1.F — verification

| Metric | Result |
|---|---|
| Tests | **401 passing** (372 lib + 12 movegen integration + 9 zobrist-vector + 3 fen + 2 magic + 3 make_unmake) |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo test --release --test movegen -- --ignored` (smoke throughput) | starting 68 ns/call, Kiwipete 114, Pos-3 37, Pos-4 43, Pos-5 129, Pos-6 99 — all 50× under the 5 µs/call threshold |
| Plan, test-suite, final review loops | converged after 3 / 3 / 2 rounds |

Plan archived at `docs/plans/m1.f.md`. ADR-0007 codifies legal-direct + mask-based + check-evasion specialization.

### M1.E — prior milestone

## Prior status

### M1.E — what landed

The `mov` module — the engine's first mutating layer:

- **`Move`** — 16-bit packed: bits 0–5 from / 6–11 to / 12–15 flag.
- **`MoveFlag`** — 14 valid codes (6 and 7 deliberately absent).
- **`Undo`** (~16 B) — captured piece + prior aux state + prior zobrist.
- **`make_move(&mut Position, Move) -> Undo`** and **`unmake_move(&mut Position, Move, Undo)`** — free functions per ADR-0004; ergonomic `Position::make_move` / `Position::unmake_move` delegates.

All special cases: quiet, double-push, capture, en passant, kingside/queenside castle, four `*Promo` and four `*PromoCapture`.

### M1.E — implementation highlights

- **Castling-rights update** via 64-entry `CASTLING_MASK` table (from/to indexed) — handles the rook-captured-on-corner Kiwipete depth-4 trap.
- **Incremental Zobrist** with debug-build round-trip assert against `from_scratch`.
- **Always-on release-build perf sentinel** at 100 ns/cycle threshold guards against accidental from-scratch reintroduction.
- **`Position` extensions:** `clear_square`, `refresh_zobrist_from`, method delegates, `BitAnd` impl on `CastlingRights`.

### M1.E — verification

| Metric | Result |
|---|---|
| Tests | **303 passing** + 3 ignored benches (286 lib + 3 fen + 2 magic + 3 make/unmake integ + 9 zobrist-vector) |
| `cargo build --release` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo llvm-cov` on `mov.rs` | 95.65% region / 94.71% line (gaps: const-fn at compile time + `unreachable!()` arms + `debug_assert` formatters) |
| `cargo mutants --in-diff` | **0 survivors** on 123 mutants (107 caught, 16 unviable) |
| Throughput (Apple Silicon, release) | quiet 27 ns/cycle, capture 38, EP 36, castle 42, promo 22 — under <50 ns/cycle target |

All three review loops (plan, test-suite, final) converged. Plan archived at `docs/plans/m1.e.md`.

### What's next

**M1.G — perft + benchmarks.** Recursive perft driver with bulk-counting at depth 1, canonical-6 fixtures via Stockfish, EPD-corpus regression (Whittington `perft.epd`, Stockfish-generated counts), `criterion` benchmark harness with baseline saving.

### M1.D ✓ — Polyglot Zobrist hashing

- Vendored the Polyglot 781-key set verbatim from `docs/reference/polyglot-book-format.md`.
- Implemented the EP-only-when-pseudo-legal hashing rule, asymmetric turn key (XORed iff WHITE-to-move), four-key castling encoding.
- `Position::zobrist` field with `refresh_zobrist()` setter; 9 published Polyglot test vectors as gold-standard interop check.
- ADR-0009 landed in the same commit.

| Metric | Result |
|---|---|
| Tests | **238 passing** + 1 ignored bench (224 lib + 3 fen + 2 magic + 9 zobrist-vector) |
| `cargo mutants --in-diff` | 0 survivors on 1067-line diff |
| Throughput | `from_scratch(starting_position)` ≈ 50.8 ns/op; `ep_file_to_hash` ≈ 0.72 ns/op on no-EP early-exit |

## Milestones

### M0 — Scope & architecture ✓
Resolve foundational architectural questions.

**Outcome:**
- Variant chess: out of scope (`decisions/0001`).
- Target platform: Apple Silicon macOS primary, mobile downstream (`decisions/0002`).
- Source-code research restriction: no engine source code (`decisions/0003`).
- NNUE-readiness: `make_move`/`unmake_move` as interceptable function calls (`decisions/0004`).
- Implied: `u64` bitboards, magic bitboards, 16-bit move encoding, single-variant codebase.

### M1 — Move generator + perft (standard chess)
Bitboards, all rules of standard chess, no search, no eval. Validated against perft fixtures generated by Stockfish 18 (sole oracle — see `decisions/0006-stockfish-as-perft-oracle.md`) on the canonical 6 test positions (starting position, Kiwipete, Position 3, Position 4, Position 5, Position 6) to depth 6 or 7. A larger EPD-format regression suite (also Stockfish-generated) is an optional bulk-confidence layer.

**Exit criteria:** perft suite passes; benchmark harness records nodes/sec for move generation. Target ≥100 Mnps single-threaded on Apple Silicon M4 (≥200 Mnps would be excellent).

**TDD applicability:** maximum. Perft gives exact integer answers from known positions.

**Status — prior-art research complete (2026-04-27).** Three parallel research reports in `docs/research/`: `m1-engine-architecture.md`, `m1-magic-bitboards.md`, `m1-perft-and-rust.md`. Synthesis recorded in `docs/prior-art.md`.

**ADRs to write per-phase, as each binds.** None of these gates M1.A. Material for all three is already captured in `docs/prior-art.md` (headline calls) and `docs/research/m1-engine-architecture.md` + `docs/research/m1-magic-bitboards.md` (full reasoning). Each ADR lands just before the phase that depends on it, so the ADR can be refined by anything we learn during plan-mode for that phase:

- **ADR-0008** — Magic bitboards: fancy variant with variable shift; magic constants generated and committed via a separate `magicgen` binary; attack tables built at runtime startup; slow ray-walker kept as permanent differential-test oracle. **Binds on M1.C.**
- **ADR-0009** — Polyglot Zobrist key set + refined EP-file hashing rule (hash EP file only when an EP capture is actually pseudo-legally possible). **Binds on M1.D.**
- **ADR-0007** — Legal-direct move generation (vs. pseudo-legal-and-filter). **Binds on M1.F.**

**Sub-phases.** M1 is decomposed into seven plan-and-execute cycles. Each phase gets its own plan-mode pass with the self-review loop (see `workflow.md`), executes independently, lands its own commit(s), runs its own tests before we move to the next phase.

| Phase | Scope | Approx size |
|---|---|---|
| **M1.A** ✓ — Skeleton + primitives | `lib.rs` split, `Cargo.toml` release profile (`lto = "thin"`, `codegen-units = 1`, `panic = "abort"`), module skeleton, `Square` type, `Bitboard` type and primitive operations, unit tests for primitives | ~500–800 lines (actual: ~720 lines, 50 tests) |
| **M1.B** ✓ — Position + FEN | `Position` struct (6+2 bitboards + mailbox + cached king squares + auxiliary state), `Color` / `PieceKind` / `Piece` / `CastlingRights` types, FEN parse/format per Edwards 1994 §16.1 (strict syntactic + structural sanity checks), position-equality tests | ~600–900 lines (actual: ~2175 lines including ~40 negative-parse tests, 139 unit + 3 integration tests) |
| **M1.C** ✓ — Sliding-piece attacks | Slow ray-walker as permanent `slow_attacks` oracle module, `src/bin/magicgen.rs` (search + validation + codegen), generated magic constants source file, fancy-magic attack lookups, differential tests over all ~108k (square, occupancy) pairs | ~800–1200 lines (actual: ~1934 lines including ~145-line generated constants file and ~68-line ADR; 50 new unit + 2 integration tests) |
| **M1.D** ✓ — Zobrist | Polyglot 781-key table vendored from in-tree spec, EP-only-when-pseudo-legal hashing rule, side-to-move asymmetric turn key, `Position::zobrist` field + `refresh_zobrist()` setter; 9 published test vectors as gold-standard interop check. M1.E will add the incremental hash update + debug round-trip assert in make/unmake. | ~250–350 lines (actual: ~787 lines including 121-line vendored data file and ~120-line ADR; 30 unit + 5 property + 3 position + 9 integration tests) |
| **M1.E** ✓ — Make/unmake | `Move` (16-bit), `MoveFlag` (14 valid), `Undo` (~16 B), free functions `make_move`/`unmake_move` per ADR-0004, all special cases (castling, EP, all 8 promotion variants, double-push), incremental Zobrist with debug round-trip assert + release-build perf sentinel, round-trip property tests, ergonomic `Position::make_move`/`Position::unmake_move` delegates | ~600–900 lines (actual: ~2050 lines including ~700-line plan; 65 new tests) |
| **M1.F** ✓ — Legal move generation | `movegen` module with `MoveList` + `generate_moves` + `in_check`; per-call `MaskInfo` (checkers, pinned, capture/push masks, king_danger, pin_rays); per-piece emit fns; EP horizontal-pin + symmetric diagonal-pin filters; castling with mailbox `debug_assert!`s; `validate_post_parse` extended for castling consistency; defensive-checks-debug-only convention codified | ~1500–2000 lines (actual: ~3700 lines including ~700-line plan, ~110-line ADR; 91 new tests) |
| **M1.G** ✓ — Perft + benchmarks | Recursive perft (plain + bulk-count + divide + categorized) with 174-position Whittington EPD regression suite (Stockfish-regenerated counts) and canonical-6 D1–D6 fixtures, `criterion` benchmark harness with baseline saving (ADR-0010), 119 Mnps bulk on starting D4 (meets M1 exit criterion) | ~500–800 lines (actual: ~1300 lines incl. ~700-line plan, ~80-line ADR; 21 perft unit tests + 14 integration tests, plus the fixture parser) |

Phases A→F are foundational and largely sequential. G is the validation layer.

### M2 — Random-mover engine speaking UCI
Plays legal random moves through a tournament harness (fastchess; see ADR-0012 below). No search depth. Establishes the UCI skeleton, time-management harness, and tournament tooling before any search complexity.

**Exit criteria:** plays a complete game through `fastchess` against itself or another engine without protocol errors or illegal moves.

**TDD applicability:** very high. UCI is a text-in/text-out protocol — parsers and command dispatch are pure functions. Random move selection is deterministic given a seed. The end-to-end game loop is the only piece needing process-spawning integration tests.

**ADRs to write per-phase, as each binds.**

- **ADR-0011** — UCI I/O threading model. **Binds on M2.C.**
  - Constraint: UCI mandates stdin readable during search (`isready` → `readyok` mid-search; `stop` aborts `go infinite`).
  - Choice space: dedicated reader thread + cancellation channel to a search worker, vs. single thread polling stdin between search iterations.
  - Research: `research/m2-uci-threading.md` recommends reader thread + mpsc + per-`go` worker + `Arc<AtomicBool>` cancellation.
- **ADR-0012** — Tournament-harness conventions. **Binds on M2.E.**
  - Runner: `fastchess` (Cute Chess 1.4.0 ships zero macOS assets; fastchess ships pre-built `mac-arm64` binaries and is what Stockfish/Fishtest migrated to in 2024).
  - Engine config: `scripts/match.sh` wrapper. fastchess has no `engines.json` registry.
  - Output layout: raw PGN/log → `target/matches/` (gitignored). Milestone summaries → `bench/m2.md` per ADR-0010.
  - Smoke contract: 4 self-play + 4 vs Stockfish at `tc=10+0.1`, all legally terminated, no protocol errors, fastchess UCI-compliance checker silent.
  - SPRT (per-change strength gating): deferred to M3.
  - Research: `research/m2-tournament-harness.md`.

**Sub-phases.** M2 is decomposed into five plan-and-execute cycles. Each phase gets its own plan-mode pass with the self-review loop (see `workflow.md`), executes independently, lands its own commit(s), runs its own tests before we move to the next phase.

| Phase | Scope | Approx size |
|---|---|---|
| **M2.A** — UCI move encoding | `Move::to_uci(self) -> String` (no Position needed — flag distinguishes promo/castle) and `Move::from_uci(&str, &Position) -> Result<Move, _>` (needs Position to resolve king-move vs. castling, pawn-to vs. EP, single- vs. double-push, infer promo-capture flag). Long algebraic per UCI spec: `e2e4`, `e1g1`/`e1c1` for castling, `e7e8q` for promotion (lowercase), `0000` for null. Round-trip tests against canonical-6 perft moves; reject malformed input | ~400–600 lines |
| **M2.B** — UCI command parser | New `uci` module: `Command` enum + `parse_uci_line(&str) -> Command` (returns `Command::Unknown` for stuff to silently skip per spec §"unknown command or token"). Covers `uci`, `debug on/off`, `isready`, `setoption name … [value …]`, `register`, `ucinewgame`, `position [startpos\|fen …] [moves …]`, `go` (with `searchmoves`/`ponder`/`wtime`/`btime`/`winc`/`binc`/`movestogo`/`depth`/`nodes`/`mate`/`movetime`/`infinite`), `stop`, `ponderhit`, `quit`. Strict whitespace tolerance per spec §arbitrary-whitespace. Property tests for grammar coverage | ~600–900 lines |
| **M2.C** — Engine I/O loop + position state | `Engine` struct (current `Position`, options, RNG seed), main `run()` loop reads lines from stdin and dispatches parsed `Command`s to handlers, writes responses to stdout. Handlers: `uci` (emits `id name`/`id author`/`option …`/`uciok`), `isready` (always answers `readyok`, even mid-search), `setoption`, `ucinewgame` (resets state), `position` (parses moves via M2.A `from_uci` and applies them), `quit`. `info string …` debug logging behind `debug on`. `go` parsed but answered with placeholder until M2.D. Threading model (ADR-0011) lands here so `isready` and `stop` work concurrently with future search | ~800–1200 lines |
| **M2.D** — Random search + `go`/`bestmove` | `Search` trait/struct with `start(pos, params, output) -> Handle` and `Handle::stop()`, preserving the eval/search interception point per ADR-0004 (NNUE + skill-dial future hooks). Random move via SplitMix64; seed from system time or a custom `Random_Seed` UCI option for reproducibility. Honors `go movetime <ms>` (sleeps then emits), `go infinite` (waits for `stop`), `stop` (cancels pending emit), `searchmoves` (filters legal-move pool). `bestmove <uci>` output. End-to-end test: drive a full game from `position startpos` through repeated `go movetime 10` until terminal | ~700–1000 lines |
| **M2.E** — Tournament harness + fastchess | `scripts/match.sh` wrapper around `fastchess`; documentation for downloading the `mac-arm64` release and reading PGN/log output. Integration test that spawns the release binary, drives it through a complete self-game via piped stdin/stdout, asserts the game terminates legally (checkmate / stalemate / 50-move / threefold / insufficient material per FIDE) and that the PGN parses. ADR-0012 codifies the harness layout. `docs/workflow.md` gains a "running a match" runbook | ~400–700 lines |

Phases A → D are foundational and largely sequential; A and B are independent of each other and could be planned in either order, but C depends on both. E is the validation layer (analogous to M1.G).

### M3 — Alpha-beta + material eval
First playing engine. Negamax with iterative deepening, quiescence search, simple material + piece-square table eval. No transposition table yet.

**Exit criteria:** beats the random mover ~100% via SPRT; estimated rating from self-play and a known-strength reference.

### M4 — Search basics
Transposition table (Zobrist), move ordering (PV move, MVV-LVA, killer moves, history heuristic), aspiration windows.

**Exit criteria:** each addition justified by SPRT win.

### M5 — Search advanced
Null-move pruning, late move reductions, futility pruning, singular extensions. Each gated by SPRT.

### M6 — Eval improvements
Tapered eval, pawn structure, king safety, mobility, passed pawn evaluation. Texel-tuned where possible.

### M7 — Skill dial (basic strength reduction)
Configurable strength reduction: UCI's standard `UCI_LimitStrength` and `UCI_Elo` options, plus a granular "skill level" knob. Mechanisms: depth/node-count caps, eval noise injection, top-N randomized move selection. Each mechanism is a pure function (TDD-able); their composition's actual Elo at each setting is calibrated empirically via self-play matches.

**Exit criteria:** at advertised Elo settings, calibrated self-play matches confirm strength within a stated margin (e.g. ±50 Elo). Engine plays "interestingly bad" at low settings rather than randomly bad.

**Schedulable earlier.** This milestone has no hard dependency past M3 — if the user wants to play against the engine before M7, we can pull a basic version forward. Full calibration only makes sense once eval is reasonably stable (post-M6).

See `decisions/0005-strength-dial-as-planned-milestone.md`.

### M8 — Parallelism
Lazy SMP or equivalent. Lockless TT.

### M9 — NNUE
Train and integrate. Requires a data pipeline and training infrastructure separate from the engine. Replaces classical eval as the primary scoring function. Re-uses make/unmake hooks from `decisions/0004`.

### M10 — Android app
Wrap engine as a mobile app. Toy target — performance regressions vs. macOS expected and acceptable. The skill dial (M7) is the primary mechanism for making the mobile engine a plausible opponent for the user.

### M11 — Tournament play
Enter CCRL Blitz, CEGT, or open TalkChess events to obtain external Elo.

### M12 — Human-like play (optional, post-NNUE)
A separate model (Maia-style — trained to predict human moves at a target rating band) plugged in via the same eval/policy hook used by NNUE. Distinct from M7's "skill dial," which is an engine playing weakly; this is an engine playing *like a human* of a target rating. Open question: whether worth the training infrastructure investment, given M7 may be sufficient for the app use case.

## Long-term strength target

GM-level (~2700+) is the design ceiling. Classical eval (M3–M6) targets the high-amateur to weak-master range; NNUE (M9) is what carries us into GM territory. The skill dial (M7) and optional human-like play (M12) provide the inverse — making the engine a calibrated opponent at lower levels.

## Notes

- Each milestone produces benchmarks recorded somewhere persistent (TBD — likely `bench/` directory with timestamped results).
- Each milestone updates this file with completion notes and rating estimate.
- SPRT-driven changes apply from M3 onward.
