//! Criterion benchmarks for `generate_moves` throughput.
//!
//! Per `docs/plans/m1.g.md` §6 "Benchmark layout". One bench per
//! canonical-6 position. **Replaces** the M1.F-era smoke benchmark
//! (`tests/movegen.rs::bench_generate_moves_throughput`), which is
//! deleted in M1.G — see plan §"Smoke benchmark resolution".
//!
//! Run via:
//!     cargo bench --bench movegen -- --save-baseline m1.g
//!     cargo bench --bench movegen -- --baseline m1.g

use std::hint::black_box;

use chess::{MoveList, Position, generate_moves};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

const CANONICAL_6: &[(&str, &str)] = &[
    (
        "starting",
        "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
    ),
    (
        "kiwipete",
        "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
    ),
    ("position_3", "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1"),
    (
        "position_4",
        "r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1",
    ),
    (
        "position_5",
        "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
    ),
    (
        "position_6",
        "r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10",
    ),
];

fn bench_generate_moves(c: &mut Criterion) {
    let mut group = c.benchmark_group("generate_moves");
    for &(name, fen) in CANONICAL_6 {
        let pos = Position::from_fen(fen).unwrap();
        group.bench_with_input(BenchmarkId::from_parameter(name), &pos, |b, p| {
            // One MoveList allocated once outside the timed loop; the
            // measured operation is `generate_moves` itself.
            let mut ml = MoveList::new();
            b.iter(|| {
                generate_moves(black_box(p), &mut ml);
                black_box(ml.len());
            });
        });
    }
    group.finish();
}

criterion_group!(movegen_benches, bench_generate_moves);
criterion_main!(movegen_benches);
