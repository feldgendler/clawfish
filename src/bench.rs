//! `bench` UCI command (M3.F): deterministic node-count regression baseline.
//!
//! Vendors a fixed-position FEN corpus + default-depth constant. The
//! `Engine::handle_bench` consumer in `src/engine.rs` iterates the corpus,
//! drives `Search::go` at fixed depth on each, sums node counts, and emits
//! `info string` summary + signature lines.
//!
//! See `docs/plans/m3.f.md` §3.2/§3.3 for the corpus rationale and depth
//! calibration.

/// Vendored bench corpus: 16 FENs covering opening / middlegame / endgame
/// phases. Hand-curated for diversity. Phase distribution: 3 opening, 8
/// middlegame, 5 endgame. The `bench_positions_all_parse_via_from_fen` test
/// validates every entry against `Position::from_fen`.
pub(crate) const BENCH_POSITIONS: [&str; 16] = [
    // 1. Opening — startpos.
    "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
    // 2. Middlegame — Kiwipete (CPW canonical-6 #2).
    "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
    // 3. Endgame — CPW canonical-6 #3.
    "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
    // 4. Middlegame promo-rich — CPW canonical-6 #4.
    "r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2pP/R2Q1RK1 w kq - 0 1",
    // 5. Middlegame — CPW canonical-6 #5.
    "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
    // 6. Middlegame symmetric — CPW canonical-6 #6.
    "r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10",
    // 7. Opening — English (Symmetric, after Nf3 Nc6 Nc3 Nf6).
    "r1bqkb1r/pp1ppppp/2n2n2/2p5/2P5/2N2N2/PP1PPPPP/R1BQKB1R w KQkq - 4 4",
    // 8. Opening — King's Pawn (after 1.e4 e5 2.Nf3 Nc6).
    "r1bqkbnr/pppp1ppp/2n5/4p3/4P3/5N2/PPPP1PPP/RNBQKB1R w KQkq - 2 3",
    // 9. Middlegame closed.
    "r1bqr1k1/pp1nbppp/2p2n2/3p4/3P4/2NBPN2/PP3PPP/R1BQ1RK1 w - - 0 9",
    // 10. Middlegame strategic.
    "r2q1rk1/pp1bbppp/2n1pn2/3p4/3P4/2NBPN2/PP1B1PPP/R2Q1RK1 w - - 6 11",
    // 11. Middlegame → endgame transition.
    "2r3k1/p4ppp/4p3/3pP3/3P4/2P2N2/P4PPP/3R2K1 b - - 0 20",
    // 12. Endgame — pure 4-rook endgame, both sides retain castling.
    "r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 30",
    // 13. Endgame — KPK reference.
    "4k3/8/8/8/8/8/4P3/4K3 w - - 0 1",
    // 14. Endgame — King + 3 pawns vs King.
    "8/8/8/4k3/8/8/3PPP2/4K3 w - - 0 1",
    // 15. Middlegame castled.
    "r1bq1rk1/pp2bppp/2n2n2/2pp4/3P4/2NBPN2/PP3PPP/R1BQK2R w KQ - 0 8",
    // 16. Endgame — Lasker–Reichhelm K+P study.
    "8/k7/3p4/p2P1p2/P2P1P2/8/8/K7 w - - 0 1",
];

/// Default depth for `bench` when no argument given. Empirically calibrated
/// to ~9 s wall on Apple M4 (M3.E binary, 2026-04-29) at the engine's
/// ~7–11 Mnps; sub-30 s on 3× slower CI runners. Raise as TT (M4) and
/// parallelism (M9) cut per-depth wallclock; revisit at each milestone where
/// nps changes materially.
pub(crate) const BENCH_DEFAULT_DEPTH: u32 = 7;

/// Compile-time invariant: the parser at `parse_bench` accepts depths in
/// `1..=63`; `BENCH_DEFAULT_DEPTH` must satisfy the same bound or the bare
/// `bench` command's default would route to `Command::Unknown`. Module-scope
/// const-assert (vs runtime test) so a future edit to the constant fails the
/// build, not just `cargo test`.
const _: () = assert!(BENCH_DEFAULT_DEPTH >= 1 && BENCH_DEFAULT_DEPTH <= 63);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Position;

    #[test]
    fn bench_positions_count_is_16() {
        assert_eq!(BENCH_POSITIONS.len(), 16);
    }

    #[test]
    fn bench_positions_all_parse_via_from_fen() {
        for (i, fen) in BENCH_POSITIONS.iter().enumerate() {
            let pos = Position::from_fen(fen)
                .unwrap_or_else(|e| panic!("BENCH_POSITIONS[{i}] failed to parse: {fen:?} ({e})"));
            // Round-trip — the canonical re-emission must itself parse.
            // Catches FENs that are valid input but produce non-canonical
            // output (which would surprise downstream consumers).
            let canon = pos.to_fen();
            Position::from_fen(&canon).unwrap_or_else(|e| {
                panic!("BENCH_POSITIONS[{i}] round-trip failed: {fen:?} → {canon:?} ({e})")
            });
        }
    }

    #[test]
    fn bench_positions_are_all_distinct() {
        // Anchor against accidental duplicate FENs (a typo'd copy/paste would
        // otherwise pass `bench_positions_count_is_16` and `…_all_parse…`
        // silently while halving the corpus's effective surface area).
        let mut seen: Vec<&str> = Vec::new();
        for fen in BENCH_POSITIONS.iter() {
            assert!(
                !seen.contains(fen),
                "duplicate FEN in BENCH_POSITIONS: {fen:?}"
            );
            seen.push(fen);
        }
    }

    #[test]
    fn bench_positions_cover_diverse_piece_counts() {
        // The plan's phase distribution (3 opening + 8 middlegame + 5 endgame
        // per §3.2) is hand-curated. Piece count alone can't distinguish
        // opening from middlegame — Kiwipete (a middlegame fixture) has all 32
        // pieces. So this test asserts the weaker but observable property:
        // the corpus spans the *piece-count range*. Both full-board (≥30)
        // and clearly-sparse (≤14) fixtures are present, and at least 5
        // distinct piece counts appear across the 16 entries (so the corpus
        // doesn't accidentally cluster on one or two material configurations).
        let mut full_board = 0;
        let mut sparse = 0;
        let mut counts = std::collections::BTreeSet::new();
        for fen in BENCH_POSITIONS.iter() {
            let pos = Position::from_fen(fen).unwrap();
            let total = pos.occupied_all().count();
            counts.insert(total);
            if total >= 30 {
                full_board += 1;
            }
            if total <= 14 {
                sparse += 1;
            }
        }
        assert!(
            full_board >= 1,
            "expected ≥1 full-board position (≥30 pieces); got {full_board}"
        );
        assert!(
            sparse >= 4,
            "expected ≥4 sparse positions (≤14 pieces); got {sparse}"
        );
        assert!(
            counts.len() >= 5,
            "expected ≥5 distinct piece counts across the corpus; got {} ({counts:?})",
            counts.len()
        );
    }
}
