# ADR-0015 — Phantom-EP sanitization at FEN parse and `make_move`

**Status:** Accepted, 2026-04-28.
**Phase:** Binds at the time of writing (post-M3.A).

## Context

Edwards 1994 §16.1.3.4 directs FEN encoders to set the EP target after **any** double pawn push, regardless of whether an opposing pawn could actually capture. ADR-0009 §2 chose the Polyglot pseudo-legal-only **hashing** rule for the corresponding Zobrist key — i.e. `zobrist::ep_file_to_hash` returns `Some(file)` iff a side-to-move pawn sits adjacent to the just-pushed pawn. Up to this ADR, the position's `ep_target` field was set independently of capturer presence (FEN-spec literal), so two physically equivalent positions could share a Zobrist hash but disagree on `Position::ep_target` — and `Position == Position::from_fen(p.to_fen())` did not hold for positions whose EP was a phantom.

Stockfish (the de-facto reference) sanitizes phantom EPs to `None` at both entry points: FEN parse and post-double-push. The sanitizer's predicate is the same Polyglot pseudo-legal adjacency check the hashing rule already runs.

## Decision

1. **The EP target field carries pseudo-legal-only EP semantics.** `Position::ep_target` is `Some(ep_sq)` iff a side-to-move pawn is geometrically positioned to capture en passant. Phantom EPs (no capturer adjacent) are sanitized to `None` at both entry points where the field is set:

   - **FEN parse** (`fen::parse`): after `parse_ep` validates the rank, a private `sanitize_ep` helper drops the EP target unless side-to-move matches the rank's expected capturer (Black for rank 3, White for rank 6), the opponent's pawn that supposedly just double-pushed is present on `(ep_sq.file(), pawn_rank)`, and a side-to-move pawn is adjacent.
   - **`make_move`** (after a `DoublePush`): the EP target is set iff the same adjacency predicate holds against the opponent.

   The shared adjacency helper is `pub(crate) fn fen::ep_capturer_exists(pos, capturer_color, ep_file, capturer_rank)`. It is parametric over color/file/rank because `make_move` runs it *before* `set_aux_state` writes the new aux state — the existing `zobrist::ep_file_to_hash` reads `pos.ep_target()` and `pos.side_to_move()`, which aren't yet updated at that point.

2. **`set_aux_state` does not sanitize.** It is the low-level mutator (used by tests + internal call sites that already know what they're doing). Direct callers that want a phantom EP for white-box testing of downstream consumers (e.g. `zobrist::ep_file_to_hash_none_when_no_capturer` in `src/zobrist.rs`) construct it deliberately. Documented contract.

3. **Round-trip identity holds.** `Position::from_fen(p.to_fen()) == p` for any `Position` that itself came from `from_fen` or from `make_move` chains — both entry points produce post-sanitization EPs, so the field round-trips through FEN identically. (Round-trip is *not* identity for FEN strings carrying a phantom EP from another encoder; the parser silently strips them, matching Stockfish.)

## Alternatives considered

- **FEN parse only (asymmetric).** Sanitize at FEN parse but leave `make_move`'s "set on every double-push" behavior intact. **Rejected:** introduces `from_fen(p.to_fen()) != p` for any position whose EP was set by an internal `make_move` without an adjacent capturer. Hash equality still holds (the Polyglot filter masks the divergence at hash time) but structural `PartialEq` would silently disagree — exactly the kind of latent inconsistency that surfaces months later as a TT, repetition, or proptest bug.

- **Defer.** Keep the prior FEN-spec literal commitment; sanitize nowhere. **Rejected:** the user explicitly asked for Stockfish-compatible behavior, and the prior architecture commitment was already expressed only in `architecture.md` (no ADR pinned it). Promoting the layered defense to a state-level invariant trades one place that filters for two places that produce-already-filtered, eliminating the divergence class entirely.

- **Sanitize in `set_aux_state` too.** Make `set_aux_state` enforce the invariant unconditionally. **Rejected:** the invariant is on the *external* shape of `Position` (what `from_fen`/`make_move` produce); `set_aux_state` is a low-level test backdoor. Sanitizing there would block the intentional `zobrist::ep_file_to_hash_none_when_no_capturer` test's construction. Document the contract instead.

- **Stricter legal-only EP filter** (also test pin / discovered-check on the EP capture). **Rejected, same as ADR-0009 §"Stricter legal-only EP hashing".** More code, harder to test, no real benefit at engine strength below master level. Consistent with ADR-0009's pseudo-legal-only stance.

## Consequences

- **+** `Position::ep_target` and `zobrist::ep_file_to_hash` agree by construction. Two physically equivalent positions can no longer disagree on the field while sharing a hash.
- **+** `Position::from_fen(p.to_fen()) == p` holds for engine-produced positions — the structural equality test is faithful to the position's identity.
- **+** Matches Stockfish behavior on FEN ingestion. Tournament-runner FENs (fastchess `--compliance`, opening books, study databases) that carry phantom EPs are normalized identically to how Stockfish normalizes them — no asymmetry to reason about across engines.
- **−** Round-trip is not identity for FEN strings with phantom EPs. The Edwards 1994 §16.1.4 example FENs (after 1.e4 with `e3` set, after 1.e4 c5 with `c6` set) re-format with `-` in the EP field. Documented in fen.rs module docstring; tests pin the new round-trip output explicitly.
- **−** Two entry points share the sanitization predicate. Mitigation: the `fen::ep_capturer_exists` helper has a single definition; both `sanitize_ep` (FEN parse) and `make_move`'s `new_ep` computation call it. Mutation testing pins zero misses on either site.
- **−** Direct `set_aux_state` callers in tests must do their own sanitization (or deliberately construct phantom EPs for white-box testing). Three call sites need this awareness today: `zobrist::ep_file_to_hash_none_when_no_capturer`, `movegen::pawn::tests::pawn_ep_target_set_but_no_capturer`, and `position::tests::debug_grid_midgame_snapshot`. All three are documented at the call site.

## Relation to ADR-0009

ADR-0009 §2 codifies the Polyglot **hashing** rule: `ep_file_to_hash` returns `Some(file)` iff a side-to-move pawn is adjacent. That decision stands. ADR-0015 promotes the same predicate from a hash-time filter to a state-level invariant on `Position::ep_target`. ADR-0009's Context paragraph 2 (which described FEN as "unconditionally sets the EP target after any double pawn push") is now a description of the *prior* state, not the current one — this ADR replaces it for the position-state perspective; ADR-0009's hashing decision is unchanged.

## References

- `decisions/0009-polyglot-zobrist.md` §2 — the underlying pseudo-legal-only adjacency rule.
- `src/fen.rs` — `sanitize_ep`, `ep_capturer_exists`, module docstring.
- `src/mov.rs` — `make_move`'s `new_ep` computation.
- `docs/architecture.md` — Position layout + FEN parsing + make_move walk rows.
