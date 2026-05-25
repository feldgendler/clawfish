# 0002 — Primary target platform is Apple Silicon

**Status:** Accepted, 2026-04-27

## Context

The user develops on an M4 MacBook. An Android app is a planned downstream milestone, but explicitly described as a "toy" target where weaker/slower performance is acceptable.

## Decision

**Primary optimization target: Apple Silicon (ARM64) macOS.** All performance work, profiling, and benchmarking is done on this platform. The Android port is a downstream concern; it will use the same codebase but is not allowed to constrain hot-path choices on the primary target.

## Consequences

- **x86-only intrinsics (PEXT, BMI2) are irrelevant.** Don't pursue them. Magic bitboards are the sliding-piece scheme — they don't need PEXT.
- **ARM NEON** is the relevant SIMD ISA when SIMD becomes useful (primarily NNUE inference). Other ARM features (CRC32, AES) are unlikely to matter for chess.
- **Profiling tools:** `samply`, Instruments (Time Profiler), `criterion` for microbenchmarks.
- **Endianness / alignment:** ARM64 macOS is little-endian, supports unaligned loads efficiently. No portability guards needed.
- **Mobile** is treated as a port problem at M12. Performance regressions on mobile are acceptable; functional correctness is required.

## Rationale

Single-platform optimization is dramatically simpler than multi-platform. The user's stated goal is performance on his MacBook. The mobile app is for casual play against the user himself (~1000 Elo) — even a 10× slowdown still produces a hopelessly strong opponent at that level.
