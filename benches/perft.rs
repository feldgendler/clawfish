//! Criterion benchmarks for the perft module.
//!
//! Per `docs/plans/m1.g.md` §6 "Benchmark layout":
//!
//! - `perft_starting_d4_plain` — plain perft, starting position, depth 4.
//! - `perft_starting_d4_bulk`  — bulk-counting perft, starting, depth 4.
//! - `perft_kiwipete_d3`       — bulk-counting perft, Kiwipete, depth 3.
//!
//! Depths chosen for ~70–200 ms/iter at the §10 throughput estimate
//! (~1.4 Mnps bulk), keeping criterion's 100-sample regime stable. If
//! actual landed throughput far exceeds the estimate (e.g. ≥10 Mnps),
//! depths can be bumped at the next baseline capture without changing
//! the bench surface.
//!
//! Run via:
//!     cargo bench --bench perft -- --save-baseline m1.g
//!     cargo bench --bench perft -- --baseline m1.g
//!
//! Per ADR-0010, raw artifacts under `target/criterion/` are
//! per-machine and gitignored; the cross-commit baseline lives in
//! `bench/m1.g.md`.

use std::hint::black_box;

use clawfish::{Position, perft, perft_bulk};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};

const STARTING_FEN: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";
const KIWIPETE_FEN: &str = "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1";

fn bench_perft_starting_d4_plain(c: &mut Criterion) {
    let pos = Position::from_fen(STARTING_FEN).unwrap();
    let mut group = c.benchmark_group("perft_starting_d4_plain");
    // Throughput in nodes/sec. Plain perft visits 197_281 leaves.
    group.throughput(Throughput::Elements(197_281));
    group.bench_function("plain", |b| {
        b.iter(|| perft(black_box(&pos), black_box(4)));
    });
    group.finish();
}

fn bench_perft_starting_d4_bulk(c: &mut Criterion) {
    let pos = Position::from_fen(STARTING_FEN).unwrap();
    let mut group = c.benchmark_group("perft_starting_d4_bulk");
    group.throughput(Throughput::Elements(197_281));
    group.bench_function("bulk", |b| {
        b.iter(|| perft_bulk(black_box(&pos), black_box(4)));
    });
    group.finish();
}

fn bench_perft_kiwipete_d3(c: &mut Criterion) {
    let pos = Position::from_fen(KIWIPETE_FEN).unwrap();
    let mut group = c.benchmark_group("perft_kiwipete_d3");
    group.throughput(Throughput::Elements(97_862));
    group.bench_function("bulk", |b| {
        b.iter(|| perft_bulk(black_box(&pos), black_box(3)));
    });
    group.finish();
}

criterion_group!(
    perft_benches,
    bench_perft_starting_d4_plain,
    bench_perft_starting_d4_bulk,
    bench_perft_kiwipete_d3,
);
criterion_main!(perft_benches);
