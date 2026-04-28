//! Perft — recursive move-generation validation. Per `docs/plans/m1.g.md`.
//!
//! ## Public API
//!
//! ```ignore
//! use clawfish::{Position, perft, perft_bulk, divide, perft_categorized};
//!
//! let pos = Position::starting_position();
//! assert_eq!(perft(&pos, 4), 197_281);
//! assert_eq!(perft_bulk(&pos, 4), 197_281);
//!
//! // `divide` is sorted by UCI move notation ascending — see fn doc.
//! let entries = divide(&pos, 2);
//! assert_eq!(entries.iter().map(|(_, n)| n).sum::<u64>(), 400);
//!
//! // Categorized counts — internal sanity, not cross-validated against
//! // Stockfish (per ADR-0006).
//! let counts = perft_categorized(&pos, 3);
//! assert_eq!(counts.nodes, 8_902);
//! ```
//!
//! See `docs/plans/m1.g.md` for the full design.

use crate::generate_moves;
use crate::mov::{Move, MoveFlag};
use crate::movegen::MoveList;
use crate::movegen::masks::checkers_of;
use crate::position::Position;

/// Inner recursion driver, monomorphized over bulk-vs-plain via the
/// `BULK` const generic. Compiled twice (once for each bool); the
/// dead-code-elimination at monomorphization removes the depth-1
/// branch entirely from the plain path. Takes `&mut Position` so
/// make/unmake can thread through; callers (`perft`, `perft_bulk`)
/// hand a clone of the public `&Position`.
fn perft_inner<const BULK: bool>(pos: &mut Position, depth: u32) -> u64 {
    if depth == 0 {
        return 1;
    }
    let mut ml = MoveList::new();
    generate_moves(pos, &mut ml);
    if BULK && depth == 1 {
        return ml.len() as u64;
    }
    let mut total: u64 = 0;
    for mv in ml.iter() {
        let undo = pos.make_move(mv);
        total += perft_inner::<BULK>(pos, depth - 1);
        pos.unmake_move(mv, undo);
    }
    total
}

/// Plain recursive perft. Counts leaf nodes at `depth` plies from `pos`.
pub fn perft(pos: &Position, depth: u32) -> u64 {
    let mut working = *pos;
    perft_inner::<false>(&mut working, depth)
}

/// Bulk-counting perft (CPW "bulk counting"): at depth 1, returns the
/// legal-move count without making + unmaking each leaf.
pub fn perft_bulk(pos: &Position, depth: u32) -> u64 {
    let mut working = *pos;
    perft_inner::<true>(&mut working, depth)
}

/// Divide: for each legal move at depth `depth`, report the move and
/// `perft(make(pos, mv), depth - 1)`. Output is **sorted by UCI move
/// notation ascending** — a deterministic, stable ordering that does not
/// depend on `generate_moves`'s internal emit order.
///
/// Stockfish 18's `go perft N` emits divide output in its engine-internal
/// generation order, NOT in lex-sorted order (verified at plan-write
/// time: `position startpos / go perft 2` produces `a2a3, b2b3, ...,
/// h2h3, a2a4, b2b4, ..., h2h4, b1a3, b1c3, g1f3, g1h3` — pawn moves
/// first ordered by source file, then knights, etc.). For the documented
/// debugging workflow (`docs/research/m1-perft-and-rust.md` §"Divide as
/// a debugging tool"), pipe both outputs through `sort` before `diff`:
///
/// ```sh
/// # `our_divide` is the debug harness that prints `divide(pos, N)` —
/// # one "from-to[promo]: count" line per move, e.g. an `eprintln!` loop
/// # over the returned Vec. `sort_stockfish` is the Stockfish equivalent:
/// #   printf 'position fen $FEN\ngo perft $N\nquit\n' | stockfish \
/// #     | grep -E '^[a-h][1-8][a-h][1-8][nbrq]?:'
/// our_divide       | sort > ours.txt
/// sort_stockfish   | sort > theirs.txt
/// diff ours.txt theirs.txt
/// ```
///
/// Panics with a message starting with `"divide"` if `depth == 0`.
pub fn divide(pos: &Position, depth: u32) -> Vec<(Move, u64)> {
    assert!(
        depth >= 1,
        "divide called with depth == 0; use perft for depth-0 leaf counts"
    );
    let mut working = *pos;
    let mut ml = MoveList::new();
    generate_moves(&working, &mut ml);
    let mut entries: Vec<(Move, u64)> = ml
        .iter()
        .map(|mv| {
            let undo = working.make_move(mv);
            let count = perft_inner::<false>(&mut working, depth - 1);
            working.unmake_move(mv, undo);
            (mv, count)
        })
        .collect();
    // `sort_by_cached_key` formats each move's UCI string once (n
    // allocations) instead of `sort_by_key`'s once-per-comparison
    // (~n log n allocations). For ≤218 moves both are fine, but this
    // is the correct shape.
    entries.sort_by_cached_key(|(mv, _)| mv.to_string());
    entries
}

/// Categorized perft counts. Per ADR-0006 these are **internal sanity
/// only** — not cross-validated against Stockfish, but useful for
/// catching specific bug classes.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct PerftCounts {
    /// Total leaf nodes (equals `perft(pos, depth)` for the same depth).
    pub nodes: u64,
    /// Moves that captured an enemy piece (includes en passant).
    pub captures: u64,
    /// Moves that captured via en passant.
    pub en_passant: u64,
    /// Castling moves (kingside or queenside).
    pub castles: u64,
    /// Moves that promoted a pawn (all four promotion types counted separately).
    pub promotions: u64,
    /// Moves that left the opponent's king in check.
    pub checks: u64,
    /// Moves that gave check by uncovering an attack from another piece.
    pub discovery_checks: u64,
    /// Moves that gave check from two pieces simultaneously.
    pub double_checks: u64,
    /// Moves that delivered checkmate (a subset of `checks`).
    pub checkmates: u64,
}

/// Categorized recursive perft. Slower than `perft_bulk` (no leaf-skip;
/// must classify each leaf event), so intended for shallow-depth runs.
/// `nodes` field equals what `perft(pos, depth)` would return.
pub fn perft_categorized(pos: &Position, depth: u32) -> PerftCounts {
    let mut counts = PerftCounts::default();
    if depth == 0 {
        // Top-level call with depth 0: a single "leaf" that no move
        // produced. nodes = 1, every other counter stays 0 (no
        // classification — no move was made). See plan §3 "Recursion-
        // base contract".
        counts.nodes = 1;
        return counts;
    }
    let mut working = *pos;
    perft_categorized_inner(&mut working, depth, &mut counts);
    counts
}

fn perft_categorized_inner(pos: &mut Position, depth: u32, counts: &mut PerftCounts) {
    debug_assert!(
        depth >= 1,
        "categorized inner must not be called at depth 0"
    );
    let us = pos.side_to_move();
    let mut ml = MoveList::new();
    generate_moves(pos, &mut ml);

    for mv in ml.iter() {
        let flag = mv.flag();

        // Make the move and classify post-move check / mate state.
        let undo = pos.make_move(mv);

        // After make_move, side-to-move flipped. The mover's color is
        // `us` (captured pre-make above). The new opponent's king is
        // where the categorizer-of-checks looks: did our move put the
        // OPPONENT's king in check?
        let opp = us.flip();
        let opp_king_sq = pos.king_square(opp);
        let chk = checkers_of(pos, opp, opp_king_sq, pos.occupied_all());
        let n_checkers = chk.count();
        let is_check = n_checkers >= 1;

        // Discovery: at least one checker is NOT on `mv.to_square()`
        // (i.e., a piece other than the mover gave check). Castling is
        // counted as discovery here because the moving piece in a
        // castling move is the king (whose `to` square is g1/c1), while
        // the checker — if any — is the rook on f1/d1, which sits OFF
        // the king's `to`. CPW makes the same call.
        let to_bb = crate::Bitboard::from_square(mv.to_square());
        let is_discovery = is_check && (chk & !to_bb).any();
        let is_double = n_checkers == 2;

        // Mate at the leaf-classification ply: only check at depth == 1
        // (we'd need to generate the post-move legal-move set; doing
        // that one ply earlier — when `mv` is still in scope — is
        // strictly cheaper than recursing to depth 0 just to classify).
        let is_mate = if is_check && depth == 1 {
            let mut leaf_ml = MoveList::new();
            generate_moves(pos, &mut leaf_ml);
            leaf_ml.is_empty()
        } else {
            false
        };

        // Recurse — this contributes `nodes` increments at the leaf.
        if depth == 1 {
            counts.nodes += 1;
        } else {
            perft_categorized_inner(pos, depth - 1, counts);
        }

        pos.unmake_move(mv, undo);

        // Classify the move itself (move-shape categories).
        if mv.is_capture() {
            counts.captures += 1;
        }
        if flag == MoveFlag::EnPassant {
            counts.en_passant += 1;
        }
        if flag == MoveFlag::KingCastle || flag == MoveFlag::QueenCastle {
            counts.castles += 1;
        }
        if mv.is_promotion() {
            counts.promotions += 1;
        }
        // Check classification (post-move).
        if is_check {
            counts.checks += 1;
        }
        if is_discovery {
            counts.discovery_checks += 1;
        }
        if is_double {
            counts.double_checks += 1;
        }
        if is_mate {
            counts.checkmates += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the perft module. Per `docs/plans/m1.g.md` §"Test
    //! layout — Unit tests".
    //!
    //! These tests will be RED until the M1.G implementation pass lands;
    //! that's the project's TDD posture.

    use super::*;

    const STARTING_FEN: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
    const KIWIPETE_FEN: &str =
        "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1";
    const POSITION_3_FEN: &str = "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1";

    fn parse(fen: &str) -> Position {
        Position::from_fen(fen).unwrap_or_else(|e| panic!("FEN must parse: {fen} ({e:?})"))
    }

    // ---------------------------------------------------------------
    // Plain / bulk / divide invariants.
    // ---------------------------------------------------------------

    #[test]
    fn perft_depth_zero_returns_one() {
        // All four entry points return 1 (or PerftCounts { nodes: 1, .. })
        // at depth 0. Anchors the §"Recursion-base contract".
        let pos = parse(STARTING_FEN);
        assert_eq!(perft(&pos, 0), 1);
        assert_eq!(perft_bulk(&pos, 0), 1);
        let c = perft_categorized(&pos, 0);
        assert_eq!(c.nodes, 1);
        assert_eq!(
            c,
            PerftCounts {
                nodes: 1,
                ..PerftCounts::default()
            },
            "perft_categorized(_, 0) must zero every counter except nodes",
        );
    }

    #[test]
    fn perft_depth_one_starting_position() {
        // 20 legal moves from the starting position. Trivial anchor.
        let pos = parse(STARTING_FEN);
        assert_eq!(perft(&pos, 1), 20);
        assert_eq!(perft_bulk(&pos, 1), 20);
    }

    #[test]
    fn perft_bulk_agrees_with_perft_at_depth_2() {
        // For three diverse positions, plain and bulk-counting must agree
        // at depth 2. Pins the bulk-counting logic against the reference.
        for fen in [STARTING_FEN, KIWIPETE_FEN, POSITION_3_FEN] {
            let pos = parse(fen);
            let plain = perft(&pos, 2);
            let bulk = perft_bulk(&pos, 2);
            assert_eq!(plain, bulk, "bulk != plain at depth 2 for {fen}");
        }
    }

    #[test]
    fn perft_bulk_agrees_with_perft_at_depth_3_kiwipete() {
        // Kiwipete D3 = 97_862. Strongest bulk-vs-plain anchor — Kiwipete
        // exercises captures, EP, castling, and promotions in one
        // position; if the bulk path mishandles any leaf class, this
        // catches it. Per plan §"Risks and mitigations".
        let pos = parse(KIWIPETE_FEN);
        assert_eq!(perft(&pos, 3), 97_862);
        assert_eq!(perft_bulk(&pos, 3), 97_862);
    }

    #[test]
    #[ignore]
    fn perft_kiwipete_depth_4_total() {
        // Anchor the Stockfish-known D4 value (4_085_603) directly in unit
        // tests so a PR that breaks Kiwipete fails. `#[ignore]`-gated to
        // match the plan's §"Fast-suite wall-clock budget" — Kiwipete D4
        // = 4.09M nodes, too heavy for the default `cargo test` budget.
        // Run via `cargo test --release -- --ignored`.
        let pos = parse(KIWIPETE_FEN);
        assert_eq!(perft_bulk(&pos, 4), 4_085_603);
    }

    // ---------------------------------------------------------------
    // Divide.
    // ---------------------------------------------------------------

    #[test]
    fn divide_at_depth_one_equals_legal_move_set() {
        // At depth 1 every divide entry must have count == 1 (since
        // perft(child, 0) == 1), and the move set must equal the
        // legal-move set generated by `generate_moves`.
        use crate::generate_moves;
        use crate::movegen::MoveList;

        let pos = parse(STARTING_FEN);
        let entries = divide(&pos, 1);
        assert_eq!(entries.len(), 20, "starting depth-1 has 20 legal moves");
        for (_, count) in &entries {
            assert_eq!(*count, 1, "every divide-at-depth-1 entry must have count 1");
        }

        // The set of moves emitted by divide must match `generate_moves`.
        let mut ml = MoveList::new();
        generate_moves(&pos, &mut ml);
        let mut expected: Vec<u16> = ml.iter().map(|m| m.bits()).collect();
        let mut got: Vec<u16> = entries.iter().map(|(m, _)| m.bits()).collect();
        expected.sort_unstable();
        got.sort_unstable();
        assert_eq!(
            got, expected,
            "divide-at-depth-1 move set must equal legal-move set"
        );
    }

    #[test]
    fn divide_total_equals_perft_starting_d3() {
        // Divide entries' counts must sum to perft(pos, depth).
        let pos = parse(STARTING_FEN);
        let entries = divide(&pos, 3);
        let sum: u64 = entries.iter().map(|(_, n)| n).sum();
        assert_eq!(sum, perft(&pos, 3));
        assert_eq!(sum, 8_902, "starting D3 total");
    }

    #[test]
    fn divide_total_equals_perft_kiwipete_d2() {
        let pos = parse(KIWIPETE_FEN);
        let entries = divide(&pos, 2);
        let sum: u64 = entries.iter().map(|(_, n)| n).sum();
        assert_eq!(sum, perft(&pos, 2));
        assert_eq!(sum, 2_039, "Kiwipete D2 total");
    }

    #[test]
    fn divide_returns_uci_sorted_entries() {
        // §"Public API" pins the output as UCI-sorted ascending.
        let pos = parse(STARTING_FEN);
        let entries = divide(&pos, 2);
        let uci: Vec<String> = entries.iter().map(|(m, _)| m.to_string()).collect();
        let mut expected = uci.clone();
        expected.sort();
        assert_eq!(
            uci, expected,
            "divide must return entries UCI-sorted ascending"
        );
    }

    #[test]
    #[should_panic(expected = "divide")]
    fn divide_panics_on_depth_zero() {
        // The implementation's panic message MUST start with "divide" so
        // the panic is unambiguously the documented "panics if depth ==
        // 0" contract, not some unrelated debug-assert in the recursion.
        let pos = parse(STARTING_FEN);
        let _ = divide(&pos, 0);
    }

    // ---------------------------------------------------------------
    // Categorized counts.
    // ---------------------------------------------------------------

    #[test]
    fn categorized_starting_position_depth_3_no_promotions_no_castles_no_ep() {
        // From starting, depth 3: no promotions/castles/EP/checkmates
        // are reachable in 3 plies. The §3 invariants must hold.
        let pos = parse(STARTING_FEN);
        let c = perft_categorized(&pos, 3);
        assert_eq!(c.nodes, 8_902, "nodes must equal perft(pos, 3)");
        assert_eq!(
            c.nodes,
            perft(&pos, 3),
            "categorized.nodes must equal plain perft"
        );
        assert_eq!(c.castles, 0, "no castles reachable in 3 plies from start");
        assert_eq!(
            c.promotions, 0,
            "no promotions reachable in 3 plies from start"
        );
        assert_eq!(c.en_passant, 0, "no EP reachable in 3 plies from start");
        assert_eq!(c.checkmates, 0, "no mate reachable in 3 plies from start");
        // Positive sanity — a zero-stub categorizer would silently pass
        // every assertion above; these discriminate.
        assert!(
            c.captures > 0,
            "starting D3 has captures (e.g. e2-e4 d7-d5 e4xd5)"
        );
        assert!(c.checks > 0, "starting D3 has check-delivering moves");
        // Invariants that the categorization function must always uphold:
        assert!(c.discovery_checks <= c.checks, "discovery_checks <= checks");
        assert!(
            c.double_checks <= c.discovery_checks,
            "double_checks <= discovery_checks"
        );
        assert!(c.en_passant <= c.captures, "en_passant <= captures");
    }

    #[test]
    fn categorized_starting_position_depth_4_invariants() {
        // From CPW Perft Results, starting D4: nodes=197_281, captures=1_576,
        // en_passant=0, castles=0, promotions=0, checks=469, checkmates=8.
        // We assert ONLY the structural invariants (per ADR-0006:
        // categorized counts are internal-only sanity, not cross-validated).
        let pos = parse(STARTING_FEN);
        let c = perft_categorized(&pos, 4);
        assert_eq!(c.nodes, 197_281, "nodes must equal perft(pos, 4)");
        // (We don't re-call `perft(&pos, 4)` here — the literal 197_281
        // pin above is strictly stronger and the redundant call doubles
        // the test's wall-clock cost.)
        // From start at D4: still no promotions, castles, or EP reachable
        // in 4 plies (rank 7 white pawns can't reach rank 8 in 4 plies; no
        // black pawn double-push has occurred yet for white to EP-capture
        // because EP requires black-just-moved followed by white-EP — the
        // white-pawn-on-rank-5 prerequisite needs at least 2 white plies).
        // Actually: white pushes a pawn to rank 4 (ply 1), black plays X
        // (ply 2), white pushes the same pawn to rank 5 (ply 3), black
        // double-pushes a pawn to rank 5 next to it (ply 4 — the leaf).
        // So at the *leaf* an EP target is set, but EP-the-move requires
        // depth >= 5 to be made.
        assert_eq!(c.castles, 0, "no castles in 4 plies from start");
        assert_eq!(c.promotions, 0, "no promotions in 4 plies from start");
        assert_eq!(c.en_passant, 0, "no EP move made in 4 plies from start");
        // Positive sanity — discriminates a zero-stub categorizer.
        assert!(c.captures > 0, "starting D4 has many captures");
        assert!(c.checks > 0, "starting D4 has check-delivering moves");
        assert!(
            c.checkmates > 0,
            "starting D4 has at least one fool's-mate-class mate"
        );
        assert!(c.discovery_checks <= c.checks, "discovery_checks <= checks");
        assert!(
            c.double_checks <= c.discovery_checks,
            "double_checks <= discovery_checks"
        );
        assert!(c.en_passant <= c.captures, "en_passant <= captures");
        assert!(c.checkmates <= c.checks, "checkmates is a subset of checks");
    }

    #[test]
    fn categorized_kiwipete_depth_3_classifies_at_depth() {
        // Kiwipete D3 = 97_862 nodes. At depth 3, EP captures, castles,
        // and various check classes become reachable — anchored here so a
        // recursion-frame double-counting bug (e.g. counting an EP move
        // twice when its child explores further) fires beyond D1's
        // single-ply scope. Internal-only sanity per ADR-0006: we don't
        // pin Stockfish category numbers (different engines disagree on
        // edge counts), only structural invariants.
        let pos = parse(KIWIPETE_FEN);
        let c = perft_categorized(&pos, 3);
        assert_eq!(
            c.nodes, 97_862,
            "Kiwipete D3 nodes must match the Stockfish total"
        );
        // (Literal pin above is stronger than `c.nodes == perft(...)`;
        // dropped the redundant call to halve runtime.)
        assert!(c.captures > 0, "Kiwipete D3 has many captures (CPW: 17102)");
        assert!(
            c.en_passant > 0,
            "Kiwipete D3 has EP captures reachable (CPW: 45)"
        );
        assert!(c.checks > 0, "Kiwipete D3 has check-delivering moves");
        // Structural invariants:
        assert!(c.en_passant <= c.captures, "EP ⊆ captures");
        assert!(c.discovery_checks <= c.checks, "discovery_checks ⊆ checks");
        assert!(
            c.double_checks <= c.discovery_checks,
            "double_checks ⊆ discovery_checks"
        );
        assert!(c.checkmates <= c.checks, "checkmates ⊆ checks");
    }

    #[test]
    fn categorized_kiwipete_depth_1_classifies_captures() {
        // Kiwipete D1 = 48 legal moves. The position has many capture
        // candidates; the categorized count must classify them. Stockfish's
        // perft (Kiwipete D1 captures = 8 per CPW) is NOT what we assert
        // (per ADR-0006, internal-only); we assert structural soundness:
        // - nodes == 48
        // - captures > 0 (the position obviously admits captures)
        // - en_passant == 0 (no EP target on D1 of Kiwipete)
        let pos = parse(KIWIPETE_FEN);
        let c = perft_categorized(&pos, 1);
        assert_eq!(c.nodes, 48);
        assert!(c.captures > 0, "Kiwipete D1 has many capture candidates");
        assert_eq!(c.en_passant, 0, "Kiwipete D1: no EP target -> no EP moves");
    }

    #[test]
    fn categorized_classifies_castles() {
        // FEN: white king e1, white rook h1, black king e8. White to move.
        // Legal moves at D1:
        //   - 5 king-quiet moves: d1, d2, e2, f1, f2 (none attacked).
        //   - 9 rook moves: h-file h2..h8 (7) + rank-1 g1, f1 (2; e1 has the king).
        //   - 1 castle: O-O (e1-g1, h1-f1).
        // Total: 15.
        let fen = "4k3/8/8/8/8/8/8/4K2R w K - 0 1";
        let pos = parse(fen);
        let c = perft_categorized(&pos, 1);
        assert_eq!(
            c.nodes, 15,
            "exactly 15 legal moves at D1 (5 K + 9 R + 1 O-O)"
        );
        assert_eq!(
            c.nodes,
            perft(&pos, 1),
            "nodes must equal plain perft for this fixture"
        );
        assert_eq!(c.castles, 1, "exactly one kingside castle move at D1");
        assert_eq!(c.en_passant, 0, "no EP available in this position");
        assert_eq!(c.promotions, 0, "no promotions available");
    }

    #[test]
    fn categorized_classifies_promotions() {
        // Position with a white pawn on b7, black king on e8, white king
        // on a1. Legal pawn promotions: four flag codes on b7-b8. Plus
        // the white king has a few legal moves. Assert nodes-pin to
        // discriminate stub.
        let fen = "4k3/1P6/8/8/8/8/8/K7 w - - 0 1";
        let pos = parse(fen);
        let c = perft_categorized(&pos, 1);
        assert_eq!(
            c.nodes,
            perft(&pos, 1),
            "nodes must equal plain perft for this fixture"
        );
        assert_eq!(c.promotions, 4, "four promotion-flag codes on b7-b8");
        assert_eq!(c.captures, 0, "no captures here");
        assert_eq!(c.castles, 0, "no castling rights");
        assert_eq!(c.en_passant, 0, "no EP available");
    }

    #[test]
    fn categorized_classifies_checkmate_at_depth_1() {
        // Set up a position where some legal move at depth 1 is checkmate.
        // Use the back-rank-mate: white K on g1, white R on a8, black K on
        // h8, black blocked by own pawns. White to move, plays Ra8 (already
        // there) — actually we need a move that delivers mate at the leaf.
        //
        // Simpler: white plays Rd8# from a position where this is mate.
        // Construct: black K h8, black pawns g7 h7, white R d1, white K a1.
        // White plays Rd8 (or anything on the 8th rank that nets a check
        // with no escape). Let's use:
        //   "5rk1/6pp/8/8/8/8/8/R6K w - - 0 1"
        // White plays Ra8 — black king on g8, no escape (g7/h7 own pawns
        // block, f8 own rook blocks; no capture/block possible). Mate.
        //
        // Actually, with the rook on f8 the king has f7 as escape. Try a
        // simpler classical: black K g8, black P f7 g7 h7, white R a8.
        let fen = "6k1/5ppp/8/8/8/8/8/R6K w - - 0 1";
        let pos = parse(fen);
        let c = perft_categorized(&pos, 1);
        // We don't assert exactly which move is mate; we assert >= 1 mate
        // exists at D1 (Ra8#: rook to a8 attacks the back rank; black king
        // has no escape because of own pawns blocking f7, g7, h7).
        // Wait — Ra8 doesn't deliver check on g8, it puts the rook on the
        // 8th rank but king on g8 isn't in check. Need a move that puts
        // the rook on the king's rank with check + no escape.
        //
        // Re-engineer: black K g8, black pawns f7 g7 h7 are PINNED-irrelevant;
        // white rook on a-file. White plays Ra8 — the rook ends on a8,
        // attacking g8? No, a8 to g8 is the 8th rank and the rook controls
        // it, so Ra8+ checks the king. King escape attempts:
        //   - Kf8: blocked? No — f8 is empty. So Kf8 escapes the rook (Kf8
        //     is on the same rank as the rook; rook still attacks).
        //     Actually after Ra8, rook is on a8 — it attacks all of rank 8.
        //     So Kf8 stays in check. Kh8 stays in check.
        //   - Capture rook: g8 to a8 isn't a king-legal step. Block: no
        //     piece can interpose on the 8th rank (own pawns are on 7th
        //     rank; rook is the only white piece besides king).
        // So Ra8 is mate. Good.
        assert_eq!(
            c.nodes,
            perft(&pos, 1),
            "nodes must equal plain perft for this fixture"
        );
        // Ra8# is the unique mate at D1 from this position. Be exact.
        assert_eq!(c.checkmates, 1, "exactly one checkmate (Ra8#) at D1");
        assert!(c.checks >= 1, "checkmate is a subset of checks");
        assert!(c.checks >= c.checkmates, "every mate counts as a check");
        // Ra8# is a *direct* check (the moving rook delivers it; no piece
        // is uncovered). The discovery counter must NOT include it. Pins
        // the discovery predicate `!(chk & !to_bb).is_empty()` against
        // mutants that would invert the inner `!`, replace `&` with `|`
        // or `^`, or short-circuit on `is_check` alone.
        assert_eq!(
            c.discovery_checks, 0,
            "Ra8# is a direct check, not a discovery"
        );
        assert_eq!(c.double_checks, 0, "Ra8# is single check, not double");
    }

    #[test]
    fn categorized_does_not_count_stalemate_as_mate() {
        // Parent: white K g1, white Q f2, black K h8. White-to-move.
        // Qf2-f7 reaches a stalemate-for-black position (Qf7 controls g7,
        // g8, h7; black king h8 has no escape, not in check). No mate-in-1
        // exists from this parent (Qf8+ allows Kxh7 — Qf8 doesn't cover
        // h7 and no other white piece reaches it). Pins the predicate
        // `is_check && depth == 1` against a `||` mutant that would
        // mis-fire on every depth-1 move whose leaf has no legal moves
        // (i.e., classify stalemate as mate).
        let fen = "7k/8/8/8/8/8/5Q2/6K1 w - - 0 1";
        let pos = parse(fen);
        let c = perft_categorized(&pos, 1);
        assert_eq!(c.nodes, perft(&pos, 1));
        assert_eq!(
            c.checkmates, 0,
            "no mate-in-1 here; stalemate moves must not be classified as mate"
        );
    }

    #[test]
    fn categorized_classifies_en_passant() {
        // Position-3 mini fixture: black to move, black pawn c4, white
        // pawn just double-pushed to d4 (EP target d3). Black has the
        // legal EP move c4xd3. The position also has bishop and king
        // moves; the EP gives a discovered single check on the white
        // king (the bishop on c5 sees through the now-empty d4).
        //
        // Whittington corpus pins D1 = 15 for this exact FEN — anchor
        // both the absolute count AND `c.nodes == perft(...)` so a
        // coupled `perft + perft_categorized` stub returning matching-
        // but-wrong values is caught.
        let fen = "8/8/1k6/2b5/2pP4/8/5K2/8 b - d3 0 1";
        let pos = parse(fen);
        let c = perft_categorized(&pos, 1);
        assert_eq!(c.nodes, 15, "Whittington D1 = 15 for this fixture");
        assert_eq!(
            c.nodes,
            perft(&pos, 1),
            "nodes must equal plain perft for this fixture"
        );
        assert_eq!(c.en_passant, 1, "exactly one EP move (c4xd3) at D1");
        assert!(c.captures >= c.en_passant, "EP is a subset of captures");
    }

    // ---------------------------------------------------------------
    // Discovery / double-check classification — anchored on a known
    // fixture from M1.F's edge taxonomy.
    // ---------------------------------------------------------------

    #[test]
    fn categorized_classifies_double_check_via_ep() {
        // EP-double-check parent fixture: the only way to deliver double
        // check via the EP move (§6.3 taxonomy in m1-engine-architecture.md).
        // Apply at D1 from this position; the moving black pawn lands on
        // d3 attacking white king's e2 (pawn check), and the now-empty e4
        // unblocks black rook on e8 → e2 (discovery check).
        let fen = "4r2k/8/8/8/3Pp3/8/4K3/8 b - d3 0 1";
        let pos = parse(fen);
        let c = perft_categorized(&pos, 1);
        // The EP move e4xd3 is the only legal black move that delivers
        // double check (it's the unique single-move-class double-check
        // taxonomy point). Other black moves may also give check but not
        // double check.
        assert_eq!(
            c.nodes,
            perft(&pos, 1),
            "nodes must equal plain perft for this fixture"
        );
        // Pin the full subset chain: checks ⊇ discovery_checks ⊇ double_checks.
        // The EP move e4xd3 is the only legal move that delivers double
        // check from this parent — anchor exact value.
        assert_eq!(
            c.double_checks, 1,
            "exactly one double-check move (EP e4xd3) at D1"
        );
        assert!(
            c.discovery_checks >= c.double_checks,
            "discovery_checks ⊇ double_checks"
        );
        assert!(
            c.checks >= c.discovery_checks,
            "checks ⊇ discovery_checks (the full subset chain)"
        );
    }
}
