# Magic Bitboards: Prior-Art Research for M1

Authoritative internal reference for our Rust chess engine on Apple Silicon (M4 primary target). Compiled from prose sources only — no engine source code consulted. When prose is ambiguous the document either reasons from first principles or flags the ambiguity.

Date: 2026-04-27. Status: deep dive complete; ready for implementation discussion.

## 1. The core mechanism

Magic bitboards are a **multiply–shift perfect-hashing scheme** for sliding-piece attack lookup. The classic problem is that bishops, rooks, and queens can be blocked anywhere along a ray, and naive ray-walking is much too slow for engines that need millions of attack queries per second. Magic bitboards reframe attack generation as a table lookup whose index is computed from the *occupancy along the relevant rays*.

The four steps, every time we want the attacks of a bishop or rook on square `sq` given a board occupancy `occ`:

1. **Mask** the relevant blockers: `blockers = occ & MASK[sq]`. `MASK[sq]` keeps only the squares that could ever block this piece's rays from `sq` (see Section 3 for what's included and what's deliberately not).
2. **Multiply** by a precomputed magic constant: `hash = blockers.wrapping_mul(MAGIC[sq])`. The multiplication is a 64-bit unsigned wrap; we don't care about overflow.
3. **Shift** the high bits down to an index of width `n`: `idx = hash >> (64 - n)`. The high bits of a multiplication are the most "mixed" bits, which is why we shift right rather than mask.
4. **Look up** the precomputed attack bitboard: `attacks = ATTACK[sq][idx]`.

The result is the bitboard of squares the piece attacks (i.e., squares it can reach, including its first blocker on each ray — the side-to-move filter for "can I capture this?" is applied separately by AND-ing with `~own_pieces`).

Why does multiplication by a magic number give a usable hash? Because there are far fewer *distinct attack sets* than *possible occupancy patterns*. A rook on d4 has 10 relevant blocker bits, hence 1024 possible blocker configurations, but only 3·4·3·4 = 144 distinct attack sets, since only the *first* blocker on each of the four rays matters. The magic constant is found by trial-and-error so that all 1024 configurations land in a table sized to the number of distinct outputs (or a small power of two above it), with **constructive collisions** allowed: two blocker configurations may share an index *if and only if* they yield the same attack set. See Sections 4 and 8 for the search and pitfalls. ([Chess Programming Wiki: Magic Bitboards](https://www.chessprogramming.org/Magic_Bitboards), [Analog Hors: Magical Bitboards](https://analog-hors.github.io/site/magic-bitboards/))

Queens are not stored: `queen_attacks(sq, occ) = rook_attacks(sq, occ) | bishop_attacks(sq, occ)`. Two lookups, one OR.

## 2. Plain vs. Fancy vs. Black magic

Three established variants of the same multiply-shift idea, differing only in how the lookup table is laid out and how we squeeze its size down.

**Plain magic.** Two flat 3-D arrays: `rook_attacks[64][4096]` and `bishop_attacks[64][512]`. Every square gets a uniform table sized to the *worst-case* relevant-bit count (12 bits for rooks → 2^12 = 4096 entries; 9 bits for bishops → 2^9 = 512 entries), regardless of how many bits that square actually needs. Indexing is a simple 2-D array access with a constant shift. Reported total size: **256 KiB bishops + 2048 KiB rooks = 2304 KiB ≈ 2.3 MiB**. ([CPW: Magic Bitboards](https://www.chessprogramming.org/Magic_Bitboards), [Plain and fancy magic on modern hardware (TalkChess)](https://www.talkchess.com/forum3/viewtopic.php?t=35858))

**Fancy magic.** Each square has its own per-square shift (`64 - relevant_bits[sq]`), magic, mask, and **offset into a single shared backing array**. Squares that need only 5 bits get a 32-entry slot; squares that need 12 bits get a 4096-entry slot. The shared backing array is sized to the sum of per-square slot sizes. Reported total: **~38 KiB bishops + ~800 KiB rooks ≈ 840 KiB**. The cost is one extra struct field (`offset`) per square and a per-square shift instead of a constant. ([CPW: Magic Bitboards](https://www.chessprogramming.org/Magic_Bitboards), [TalkChess thread above](https://www.talkchess.com/forum3/viewtopic.php?t=35858))

A subvariant, **fixed-shift fancy magic**, uses a *single* shift value (e.g. 64−12 for rooks across all squares, 64−9 for bishops) but searches for magics whose top bits happen to be zero on the squares that need fewer bits, so the table effectively shortens itself. This trades a slightly larger total table (>800 KiB on the rook side) for a constant shift, which removes one memory load per attack query. Reported speed: 1.6% faster perft on Core 2 Duo, "nearly indistinguishable from fancy" on Core i5. ([TalkChess: Plain and fancy magic on modern hardware](https://www.talkchess.com/forum3/viewtopic.php?t=35858))

**Black magic.** Volker Annuss, August 2017. Replaces the AND-with-mask with `OR ~mask` — i.e., we *set* all irrelevant occupancy bits to 1 instead of clearing them. Hashing then becomes `idx = ((occ | ~MASK[sq]).wrapping_mul(MAGIC[sq])) >> (64 - n)`. The crucial property: the empty-board input is no longer all zeros (it's `~MASK[sq]`), so the empty-board index is no longer pinned at 0. That removes a constraint on the search space, lets the search find magics whose occupied range starts at some nonzero offset, and as a result allows neighbouring per-square slots in the shared backing array to **overlap** at unused entries. Annuss reported **88,507 total entries** across all 64 (bishop + rook) squares, ≈ **692 KiB at 8 bytes/entry**. ([CPW: Magic Bitboards (Black magic section)](https://www.chessprogramming.org/Magic_Bitboards), [TalkChess: Black magic bitboards](https://talkchess.com/viewtopic.php?t=64790))

**Speed differences (cited).** On a Core 2 Duo, fancy was ~2.6% faster than plain in perft. On a Core i5, ~1.6% faster. On modern CPUs the gap is small enough that the *real* benefit of fancy and black magic is memory footprint, which feeds back into cache pressure during long search.

**Recommendation for M1.** Start with **fancy magic, variable shift**. Reasoning:

- It is the de facto standard described in essentially every prose tutorial; that means the most cross-checkable reference points when debugging.
- Memory footprint (~840 KiB) is small enough to be uninteresting on a Mac and large enough to be unmistakable in a profiler if something is wrong.
- Variable shift means the per-square struct is the natural thing to write in Rust (`struct Magic { mask: u64, magic: u64, shift: u8, offset: u32 }` with a single shared `Vec<u64>` or `Box<[u64]>` backing it). Black magic adds complexity (overlapping ranges, search-space changes) for ~150 KiB of memory savings — not the right tradeoff while we're learning.
- We can switch to fixed-shift fancy or black magic later as a benchmark-driven tweak. The interface (`fn rook_attacks(sq, occ) -> u64`) hides the choice. Keep the door open; don't pay for it now.

Plain magic is a defensible alternative if we want the simplest possible code and don't mind 2.3 MiB of static data. Both Plain and Fancy are well within the M4's L2 cache (see Section 5), so cache pressure is not the deciding factor at this stage.

## 3. Mask construction

The relevant-blocker mask `MASK[sq]` for a square is "the set of squares where a piece could sit and affect the attack set." It is **not** simply the rook's or bishop's full attack pattern from that square. The crucial omission: **squares from which a piece cannot block any further movement are excluded.**

Concretely: a piece sitting on the *last* square of a ray cannot block the slider from reaching anywhere new — there is nothing beyond it. So that square is irrelevant.

For a **rook on a1**: the mask covers `a2..a7` (6 squares up the a-file) and `b1..g1` (6 squares along the rank), totalling **12 bits**. We exclude `a8` (end of the file ray) and `h1` (end of the rank ray). We also exclude `a1` itself.

For a **bishop on a1**: the mask covers `b2..g7` (6 squares along the only ray), totalling **6 bits**. We exclude `h8` (end of the ray) and `a1` itself.

For a **rook on d4**: the mask covers `d2..d7` (6 squares on the file, excluding d1 and d8) and `b4..g4` (6 squares on the rank, excluding a4 and h4). **10 bits**.

For a **bishop on d4**: the mask covers four diagonals; with the four endpoints removed and the bishop's own square excluded, **9 bits** (the maximum for a bishop, achieved by central squares).

**Per-square bit counts.**

- **Rook**: 12 bits at the four corners (a1, a8, h1, h8); 11 bits along the edge files/ranks; 10 bits on most interior squares. Total over 64 squares: 4 × 12 + 24 × 11 + 36 × 10 = 48 + 264 + 360 = **672 entries' worth in the exponent**, but per square the slot size is `1 << bits`, so the sum of slot sizes is 4·4096 + 24·2048 + 36·1024 = 16384 + 49152 + 36864 = **102,400 entries × 8 bytes = 800 KiB**. That matches the cited fancy-magic rook footprint.
- **Bishop**: 5–9 bits depending on square. Corners 6 bits, central squares 9 bits. Sum of slot sizes ≈ 5,248 entries × 8 bytes ≈ **41 KiB** (CPW reports ~38 KiB depending on exact construction; close enough).

([CPW: Best Magics so far](https://www.chessprogramming.org/Best_Magics_so_far), [Analog Hors](https://analog-hors.github.io/site/magic-bitboards/))

**Why excluding edge bits is correct.** If we *included* edge bits, we'd produce a different index for "blocker on h1 vs. no blocker on h1" for a rook on a1 — but the attack set is identical in both cases, because a1's rank ray either reaches h1 (where it stops on the blocker, capturing) or reaches h1 (where there is no blocker, stopping at the edge). Either way, h1 is in the attack set. So including the bit splits identical outputs across two indices, doubling the table for no information gain. (This is also why it would still *work* — just be wasteful.)

**The standard implementation pattern**: write a small helper that, given `(sq, direction)`, walks the ray and ORs in every square *except* the last one. Do this for rook directions (N, S, E, W) and bishop directions (NE, NW, SE, SW). This helper is tiny, runs once at table-build time, and is dead-obvious to read. Prefer it to a magic-formula incantation.

## 4. Finding magic constants

The only known way to find magic constants is **trial and error**, but the search is fast.

**The basic algorithm** (per square, per piece type):

1. Compute `mask` and `n = popcount(mask)`. Enumerate all `2^n` blocker subsets of `mask` (see *Carry-Rippler*, below), and for each, compute the correct attack set with the slow ray-walker. Store these as parallel arrays `blockers[2^n]` and `attacks[2^n]`.
2. Pick a candidate magic `m` (sparse random; see below).
3. Allocate a scratch array `used[2^n]` of attack bitboards, zero it.
4. For each `i` in `0..2^n`: compute `idx = (blockers[i].wrapping_mul(m)) >> (64 - n)`. If `used[idx] == 0`, write `used[idx] = attacks[i]`. Else if `used[idx] == attacks[i]`, this is a constructive collision — fine, continue. Else it's a destructive collision — reject `m` and go back to step 2.
5. If we make it through all `2^n` entries, `m` is a valid magic; copy `used` into the live attack table.

([CPW: Looking for Magics](https://www.chessprogramming.org/Looking_for_Magics))

**Sparseness heuristic.** Random 64-bit numbers have ~32 set bits on average, which gives poor multiplication entropy for our purposes (the high bits are too dense and too "averaged out"). The community's standard trick is to AND three random u64s together, yielding a number with an expected ~8 set bits. These sparse multipliers empirically have a much higher success rate. ([CPW: Looking for Magics](https://www.chessprogramming.org/Looking_for_Magics), [Analog Hors](https://analog-hors.github.io/site/magic-bitboards/))

```rust
fn random_sparse_u64(rng: &mut impl Rng) -> u64 {
    rng.gen::<u64>() & rng.gen::<u64>() & rng.gen::<u64>()
}
```

**Carry-Rippler subset enumeration.** Iterating all `2^n` subsets of a non-contiguous bitmask is done with a 4-character formula:

```rust
let mut sub: u64 = 0;
loop {
    process(sub);
    sub = sub.wrapping_sub(mask) & mask;
    if sub == 0 { break; }
}
```

This visits every subset of `mask` exactly once, in some order, terminating when it wraps back to 0. Saves us writing a popcount-and-permute loop. ([CPW: Traversing Subsets of a Set](https://www.chessprogramming.org/Traversing_Subsets_of_a_Set))

**"Plain" vs. "minimum-table-size" magics.** A "plain" magic just needs to be collision-free at shift `64 - n` where `n = popcount(mask)`. These are abundant — Tord Romstad reported finding all 128 magics in under a second on a Core Duo. **Minimum-table-size** (or "tightest") magics try to use *fewer* than `n` index bits, exploiting the fact that the number of distinct attack sets is much less than `2^n`. Those are dramatically harder to find — the search space is tighter and the success rate per random candidate is much lower. CPW's "Best Magics so far" page tracks the community's best; recent improvements (Niklas Fiekas, 2017) found magics one bit shorter than `n` for many squares using LCM-of-occupancy-period tricks. ([CPW: Best Magics so far](https://www.chessprogramming.org/Best_Magics_so_far), [CPW: Magic Bitboards](https://www.chessprogramming.org/Magic_Bitboards))

**Time to find magics.** Plain magics: well under 1 second total for all 128 squares on any modern laptop. Minimum-size magics: seconds to minutes per square in pathological cases.

**Hardcode constants vs. compute at startup — the tradeoff.**

- *Hardcoded* (constants in the binary): zero startup cost; reproducible across runs; binary size grows by 64·8·2 = 1 KiB of magics plus a few KiB of masks/shifts/offsets — negligible. The attack tables themselves still need to be filled at startup *or* via Rust `const fn` precomputation. Risk: the magics in the file have to be regenerated if the mask construction changes, and the only way to validate them is via the slow oracle (Section 7) — which we're going to do anyway.
- *Computed at startup*: searches all 128 magics on engine launch, ~1 second on the M4 (probably much less). Adds startup latency that matters for UCI (`isready` timeout) and especially for short test runs. Adds nondeterminism unless we seed the PRNG. Lets the engine self-heal if mask construction changes.
- *Precomputed at compile time via `const fn`*: in principle Rust can do this, but const-eval with large fixed-iteration loops blows up compile times. Documented experience from a Rust engine author calls this "blowing up my compile times for dubious benefits" — worth avoiding. ([clayton ramsey: Blowing up my compile times](https://claytonwramsey.com/blog/fiddler-const-magic/))

**Recommendation.** *Hardcode the magics, masks, shifts, and offsets as `static` constants in a generated Rust module; build the attack tables at runtime in `lazy_static`/`OnceLock` on first use, or eagerly on startup (cheap).* Write a separate `magicgen` binary in the same crate (or as a `cargo` example) that runs the search, validates against the slow oracle, and emits a Rust source file we check in. This gives us: zero startup search, deterministic engine builds, full reproducibility, and a well-tested generation tool we can re-run if we ever change the mask construction. Building the attack tables themselves at startup costs a few milliseconds and avoids const-eval pain.

## 5. Total table sizes

Cited footprints, all assuming 8-byte (u64) attack entries:

- **Plain magic**: 256 KiB bishops + 2048 KiB rooks = **2304 KiB ≈ 2.3 MiB**.
- **Fancy magic (variable shift)**: ~38 KiB bishops + ~800 KiB rooks = **~840 KiB**.
- **Black magic (Annuss 2017)**: 88,507 entries total = **~692 KiB**.
- **Theoretical minimum** if every entry could be perfectly packed: ~16 KiB bishops + ~64 KiB rooks = ~80 KiB. Not achievable in practice without a search breakthrough.

([CPW: Magic Bitboards](https://www.chessprogramming.org/Magic_Bitboards), [CPW: Efficient Generation of Sliding Piece Attacks](https://www.chessprogramming.org/Efficient_Generation_of_Sliding_Piece_Attacks))

**On Apple Silicon M4** (our primary target):

- M4 base **performance core**: 192 KiB L1 data cache, with a 16 MiB shared L2 across the P-core cluster.
- M4 Pro performance core: 128 KiB L1 data cache, 32 MiB shared L2.
- M4 efficiency core: 64 KiB L1 data, 4 MiB shared L2.

(Sources differ on exact numbers per M4 SKU; safe to assume **L1d ≥ 64 KiB**, **shared L2 ≥ 4 MiB** on any M4 variant.) ([Apple silicon — Wikipedia](https://en.wikipedia.org/wiki/Apple_silicon), various M4 reviews)

Implications:

- **Bishop tables (~38 KiB) fit comfortably in L1d** on every M4 core type. Bishop lookups should essentially always hit L1.
- **Rook tables (~800 KiB)** are far too big for L1d (192 KiB at best). They sit in L2. Most L2s on M4 are 4 MiB or larger, so the rook table is fully L2-resident.
- During a real search, *we don't touch most of the rook table* — at any given position, a typical query lands on one of the ~200 entries actually touched by the squares with rooks/queens currently on the board. The working set is much smaller than the full table.
- **Plain magic (2.3 MiB)** would still fit in L2 on every M4. So even there, we're not paying L2-eviction costs for sliding attacks during search. The choice between plain and fancy on M4 is essentially aesthetic plus startup-time considerations, not a cache-driven optimization.

Net: on the M4, magic-bitboard table size is *not* a bottleneck. Optimize for code clarity and search-loop friendliness, not for shaving the last KiB off the static data.

## 6. Apple Silicon considerations

Magic bitboards use only **AND, 64-bit unsigned MUL, right shift, and a 64-bit table load**. All are single-cycle (or close to) on every modern ARM64 core. There are no special intrinsics or platform-specific tricks needed. This is the *technique's* great virtue on Apple Silicon: it depends only on operations every architecture has had since the 1990s.

**The PEXT alternative is irrelevant to us.** PEXT (parallel bit extract) is a BMI2 x86-64 instruction that replaces the multiply-shift with a direct gather of the relevant bits, eliminating the magic constant entirely. It is genuinely faster on Intel CPUs (5% in one cited Perft test on Kiwipete — [josh's site: PEXT footnote](https://www.josherv.in/2022/08/28/chess-3/)) but *not on AMD Zen 1/2* (microcoded, slow), and *non-existent on ARM64*. Apple Silicon does not have any equivalent instruction. There is no "ARM PEXT" hidden in NEON or SVE we should be looking for. Magic bitboards are the technique. ([CPW: BMI2](https://www.chessprogramming.org/BMI2), [TalkChess: PEXT mostly for magic bitboards](https://talkchess.com/viewtopic.php?t=83159))

**Cache access pattern.** The table loads in step 4 are essentially random-access from the CPU's perspective: the index is a hash, so consecutive queries during search land in unrelated cache lines. CPW notes "we likely fetch distinct cachelines for each different square or occupancy" ([CPW: Magic Bitboards](https://www.chessprogramming.org/Magic_Bitboards)). On the M4 the L2 access latency is short enough that this hasn't, historically, been a bottleneck for magic-bitboard engines, but it is the most plausible micro-optimization target if profiling later identifies a problem.

**SIMD attempts.** A few experiments in the literature have tried to do multiple sliding-piece attacks in parallel using NEON or AVX2, but none have demonstrated a meaningful speedup over scalar magic on either platform. The bottleneck is the random-access table load, which SIMD does not help with. Don't go there at M1 stage.

**M4-specific notes from the chess community.** Stockfish on M4 Pro has been reported around 28 Mnps (12 cores, [TalkChess M3/M4 benchmarks](https://talkchess.com/viewtopic.php?t=84661)) — meaning Apple Silicon is now competitive with high-end x86 for chess. Move generation is not the bottleneck even at those speeds; eval (NNUE) and search dominate. We have plenty of headroom.

## 7. Validation strategy

The unanimous prose recommendation: **build a slow ray-walker first, build magic second, property-test that they produce identical attack bitboards for every (square, occupancy) input.** This catches:

- mask construction bugs (wrong bits included or excluded);
- magic search bugs (constructive vs. destructive collision misclassification);
- table-build bugs (off-by-one in the offset arithmetic);
- subtle endianness or square-numbering inconsistencies;
- the "I forgot to include the piece itself in the attack set" or "I included it" errors that bite when castling and pin-detection code is layered on top.

The slow ray-walker is trivial and obviously correct: for each direction, step square-by-square until either the edge or a blocker is reached; the blocker square *is* in the attack set (it's a potential capture); the square beyond it is not. Test it against perft on starting position and a few standard test positions to confirm.

The differential test: for each square `sq` in `0..64` and for each subset `occ` of `MASK[sq]` (enumerated with carry-rippler), assert `slow_rook_attacks(sq, occ) == magic_rook_attacks(sq, occ)`. Same for bishop. Total work: sum over all squares of `2^bits[sq]` ≈ 102,400 rook calls + ~5,200 bishop calls ≈ 100k iterations. Runs in well under a second. **This is the gold-standard test** — much sharper than perft for catching attack-generation bugs, because perft can mask compensating errors in mask + lookup.

**Recommendation: keep the ray-walker permanently in the codebase**, behind a feature flag or in a `slow_attacks` module:

- It's a single dirt-simple function, zero maintenance burden.
- It is the oracle of last resort whenever we hit a perft mismatch.
- It lets us re-run the differential test after any future change to magic constants or mask construction (e.g., if we move to black magic).
- It is also useful as a reference implementation in tests of higher-layer code (pin detection, attack maps, etc.) where we want to assert against an obviously-correct lower layer.

Cost is negligible (a few dozen lines of Rust, no runtime overhead unless explicitly invoked).

## 8. Pitfalls

Compiled from the literature plus first-principles reasoning:

- **Mask edge bits.** Section 3. Including the last square of a ray in the mask doubles the table for no information gain; *forgetting* to exclude it is the most common newcomer bug, because the "obvious" full attack-from-empty mask is wrong. Validate against the bit counts in Section 3 (rook 10–12 bits; bishop 5–9 bits).
- **Including the piece's own square.** The attack mask should *not* contain `sq` itself. The blocker mask should *not* contain `sq` itself. The attack *output* should not include `sq` itself (a piece doesn't attack its own square). Easy to mess up consistently in one direction and cancel out — until castling code reads it.
- **The shift parameter.** Always `64 - n` where `n` is the *number of relevant bits* — i.e., `popcount(mask)`. Not `64 - sq`, not `64 - 12`, not anything else. Variable per square unless you go fixed-shift fancy. If the shift is wrong the index is in the wrong range and we either overflow the slot or waste 90% of it.
- **Constructive vs. destructive collisions.** During the magic search we *want* `used[idx] == attacks[i]` to count as a successful slot, not a failure. Getting this wrong means we never find magics (treating constructive collisions as failures) or we silently produce wrong attack sets (treating destructive collisions as successes). Section 4 has the exact `if/else` pattern.
- **Wrap-on-multiply.** Rust will panic in debug mode on `u64 * u64` overflow. Use `u64::wrapping_mul`, or cast through `u128`, or wrap in `Wrapping<u64>`. If we forget, the engine works in release and crashes in `cargo test` on every interesting position — confusing.
- **Magic-search getting stuck.** If the random number generator is seeded with something that yields consistently bad candidates (all-dense, or strongly correlated), the search can run for a long time without finding a magic. The sparseness heuristic mitigates this; using a known-good PRNG (e.g., a simple xorshift or splitmix64 with a literature seed) avoids it entirely. ([CPW: Looking for Magics](https://www.chessprogramming.org/Looking_for_Magics))
- **Square numbering.** Are we 0=A1 or 0=A8? LSB-of-rank-1 or LSB-of-rank-8? Either convention works as long as we are consistent across mask construction, attack lookup, FEN parsing, and UCI output. Pick one and document it in `architecture.md`. The classic bug is generating masks under one convention and using them under another — the magic search itself will *succeed* (bug invisible until test), but the attack sets will be reflected/rotated.
- **Forgetting to include the first blocker in the attack set.** A rook attacks the first blocker on each ray (because that's a potential capture target). The attack set should include those blocker squares; the `~own_pieces` filter is what later removes friendly captures. If we omit blockers from the attack set, our move generator produces no captures. If we include blockers *beyond* the first, we produce illegal moves. The slow ray-walker should produce the *correct* set; the magic table should reproduce it identically.
- **Cache-line crossing on the magic struct.** If we pack `Magic` as `(mask: u64, magic: u64, shift: u8, offset: u32)`, that's 24+ bytes. Two of those = 48 bytes, fits in a cache line; three doesn't. Ensure the per-square struct is well-aligned and consider padding to 32 bytes for predictable cache behavior. Minor optimization, profile-driven.
- **Don't share masks across pieces.** The rook mask and bishop mask for the same square are different and don't overlap (perpendicular vs. diagonal). Two separate arrays.

## Summary of recommendations

1. **Variant: fancy magic, variable shift.** ~840 KiB total. Standard, well-documented, debuggable. Switch to black magic later only if profiling motivates it.
2. **Magic constants: hardcoded** in a checked-in generated Rust file. **Attack tables: built at runtime** from those constants on engine startup or via `OnceLock`.
3. **Generation tool: separate `magicgen` binary** in the project. Uses sparse-random search with carry-rippler subset enumeration and a known PRNG seed for reproducibility. Validates output against the slow ray-walker before emitting Rust source.
4. **Validation: keep the ray-walker permanently** as `slow_attacks` module. Use it in a differential test that covers every (square, occupancy) pair for both pieces. Run that test in CI.
5. **Don't bother with fixed-shift, black magic, or PEXT** at M1. They are all later optimizations; the M4 cache and the search-loop budget have plenty of headroom.

## Sources

- [Chess Programming Wiki: Magic Bitboards](https://www.chessprogramming.org/Magic_Bitboards) — the canonical reference; covers all variants, mask construction, search algorithm, table sizes.
- [Chess Programming Wiki: Looking for Magics](https://www.chessprogramming.org/Looking_for_Magics) — Tord Romstad's algorithm for the trial-and-error search, with sparseness heuristic and collision detection.
- [Chess Programming Wiki: Best Magics so far](https://www.chessprogramming.org/Best_Magics_so_far) — per-square bit counts and the community's best known magics.
- [Chess Programming Wiki: Traversing Subsets of a Set](https://www.chessprogramming.org/Traversing_Subsets_of_a_Set) — the carry-rippler subset enumeration trick.
- [Chess Programming Wiki: Efficient Generation of Sliding Piece Attacks](https://www.chessprogramming.org/Efficient_Generation_of_Sliding_Piece_Attacks) — comparison of magic vs. rotated, kindergarten, hyperbola quintessence.
- [Chess Programming Wiki: BMI2](https://www.chessprogramming.org/BMI2) — PEXT for x86-64 only.
- [Analog Hors — Magical Bitboards and How to Find Them](https://analog-hors.github.io/site/magic-bitboards/) — clean prose tutorial with the search algorithm.
- [Rhys Rustad-Elliott — Fast Chess Move Generation With Magic Bitboards](https://rhysre.net/fast-chess-move-generation-with-magic-bitboards.html) — implementation walkthrough; quoted 30% speedup over classical, ~5 s magic generation on Core i5.
- [TalkChess — Plain and fancy magic on modern hardware](https://www.talkchess.com/forum3/viewtopic.php?t=35858) — benchmark results comparing plain, fancy, and fixed-shift fancy.
- [TalkChess — Black magic bitboards](https://talkchess.com/viewtopic.php?t=64790) — Volker Annuss's introduction.
- [Clayton W. Ramsey — Blowing up my compile times for dubious benefits](https://claytonwramsey.com/blog/fiddler-const-magic/) — Rust `const fn` magic-table experience report.
- [josh's site — Chess Engine Move Generator: PEXT footnote](https://www.josherv.in/2022/08/28/chess-3/) — magic vs. PEXT benchmarking on x86-64.
- [Pradyumna Kannan — Magic Move-Bitboard Generation in Computer Chess (PDF)](http://pradu.us/old/Nov27_2008/Buzz/research/magic/Bitboards.pdf) — early seminal description.
- [Apple silicon — Wikipedia](https://en.wikipedia.org/wiki/Apple_silicon) — M4 cache hierarchy reference.
- [TalkChess — Apple M3, M4 Benchmarks](https://talkchess.com/viewtopic.php?t=84661) — Stockfish nps on Apple Silicon.
