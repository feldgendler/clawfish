//! `magicgen` — search, validate, and emit fancy-magic-bitboard constants for
//! sliding pieces (rook, bishop). Per ADR-0008.
//!
//! Run from the workspace root:
//!
//!     cargo run --release --bin magicgen [output_path]
//!
//! Default `output_path = src/magic/constants.rs`. The path is resolved
//! relative to the invocation CWD (cargo does not chdir to the manifest
//! directory before running the binary), so run from the workspace root or
//! pass an absolute path.
//!
//! **Iteration order is load-bearing for reproducibility:** rook before
//! bishop, and within each piece-type the squares iterated in LERF order
//! (a1, b1, ..., h8). A single shared `SplitMix64` rng instance threads
//! through both pieces and all 64 squares. Swapping the order changes every
//! emitted magic — the verification step in `docs/plans/m1.c.md` would catch
//! it but only after the fact.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use clawfish::bitboard::Bitboard;
use clawfish::slow_attacks;
use clawfish::square::Square;

// ---------- constants ----------

const PRNG_SEED: u64 = 0xC0FFEE_C0FFEE;
const SEARCH_BUDGET_PER_SQUARE: u32 = 1_000_000;

// ---------- PRNG ----------

/// SplitMix64 — a tiny, well-known 64-bit PRNG. Five lines of state-update,
/// no external dependency. We use sparse candidates (`a & b & c`) per
/// `docs/research/m1-magic-bitboards.md` §4.
struct SplitMix64(u64);

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Sparse candidate: bitwise-AND of three independent draws. Per the
    /// research's §4: low-popcount candidates dramatically improve the
    /// success rate of the magic search.
    fn next_sparse(&mut self) -> u64 {
        self.next() & self.next() & self.next()
    }
}

// ---------- piece taxonomy (binary-local) ----------

/// Binary-local piece taxonomy. Distinct from `clawfish::piece::PieceKind` —
/// magicgen only deals with sliders, and importing the engine's full piece
/// taxonomy here would muddy the boundary.
#[derive(Copy, Clone, Debug)]
enum Piece {
    Rook,
    Bishop,
}

impl Piece {
    fn name(self) -> &'static str {
        match self {
            Piece::Rook => "Rook",
            Piece::Bishop => "Bishop",
        }
    }

    fn mask(self, sq: Square) -> u64 {
        match self {
            Piece::Rook => slow_attacks::rook_mask(sq).0,
            Piece::Bishop => slow_attacks::bishop_mask(sq).0,
        }
    }

    fn attacks(self, sq: Square, occ: Bitboard) -> u64 {
        match self {
            Piece::Rook => slow_attacks::rook_attacks(sq, occ).0,
            Piece::Bishop => slow_attacks::bishop_attacks(sq, occ).0,
        }
    }
}

// ---------- in-binary magic record ----------

#[derive(Copy, Clone)]
struct PieceMagic {
    mask: u64,
    magic: u64,
    shift: u8,
    offset: u32,
}

// ---------- carry-rippler subset enumeration ----------

/// Enumerate every subset of `mask` exactly once via the carry-rippler trick.
/// Visits the empty subset first; terminates after wrapping back to 0.
/// `docs/research/m1-magic-bitboards.md` §4.
fn for_each_subset(mask: u64, mut f: impl FnMut(u64)) {
    let mut sub: u64 = 0;
    loop {
        f(sub);
        sub = sub.wrapping_sub(mask) & mask;
        if sub == 0 {
            break;
        }
    }
}

// ---------- per-square search ----------

/// Search a magic for `(piece, sq)` using up to `SEARCH_BUDGET_PER_SQUARE`
/// candidates. On budget exhaustion: `eprintln!` + `exit(1)` per Decision §10
/// step 4 of the plan.
fn search_one(piece: Piece, sq: Square, rng: &mut SplitMix64) -> (u64, u8, u32) {
    let mask = piece.mask(sq);
    let n_bits = mask.count_ones();
    assert!(
        n_bits >= 5,
        "sliding piece on {sq:?} has mask with {n_bits} bits — slider geometry guarantees ≥ 5"
    );
    assert!(
        n_bits <= 12,
        "sliding piece on {sq:?} has mask with {n_bits} bits — exceeds the 12-bit rook ceiling"
    );

    let table_size: usize = 1usize << n_bits;
    let shift: u32 = 64 - n_bits;

    // Pre-compute (blockers, attacks) for every subset. Single pass; the
    // search loop reuses these.
    let mut blockers: Vec<u64> = Vec::with_capacity(table_size);
    let mut attacks: Vec<u64> = Vec::with_capacity(table_size);
    for_each_subset(mask, |subset| {
        blockers.push(subset);
        attacks.push(piece.attacks(sq, Bitboard(subset)));
    });
    debug_assert_eq!(blockers.len(), table_size);

    let mut used = vec![0u64; table_size];

    'candidate: for _ in 0..SEARCH_BUDGET_PER_SQUARE {
        let candidate = rng.next_sparse();

        // No prefilter: the basic Tord Romstad / Pradyumna Kannan algorithm
        // uses sparse candidates plus the constructive-collision check, which
        // is sufficient on its own. The "popcount of high 8 bits of mask*magic
        // >= 6" heuristic is mentioned in `m1-magic-bitboards.md` §4 as an
        // optional pre-filter but it interacts badly with sparse candidates:
        // empirically, at PRNG_SEED 0xC0FFEE_C0FFEE with the triple-AND sparse
        // draw, the prefilter exhausted the 1M per-square budget on f8 (rook).
        // Without the prefilter, the constructive-collision loop converges
        // well within budget for all 128 squares.

        // Reset the scratch table.
        used.fill(0);

        for i in 0..table_size {
            let idx = (blockers[i].wrapping_mul(candidate) >> shift) as usize;
            let attack = attacks[i];
            let slot = used[idx];
            if slot == 0 {
                used[idx] = attack;
            } else if slot == attack {
                // Constructive collision — fine.
            } else {
                // Destructive collision — try the next candidate.
                continue 'candidate;
            }
        }

        // Got one.
        return (candidate, shift as u8, table_size as u32);
    }

    eprintln!(
        "magicgen: exhausted {SEARCH_BUDGET_PER_SQUARE} candidates for {} on {sq}",
        piece.name()
    );
    std::process::exit(1);
}

/// Search all 64 squares for `piece` in LERF order, threading the shared rng.
/// Returns the per-square magics and the total table length (sum of slot
/// sizes).
fn search_all(piece: Piece, rng: &mut SplitMix64) -> (Vec<PieceMagic>, usize) {
    let mut magics: Vec<PieceMagic> = Vec::with_capacity(64);
    let mut offset: u32 = 0;
    for sq in Square::all() {
        let mask = piece.mask(sq);
        let (magic, shift, slot_size) = search_one(piece, sq, rng);
        magics.push(PieceMagic {
            mask,
            magic,
            shift,
            offset,
        });
        offset = offset
            .checked_add(slot_size)
            .expect("magic table offset overflowed u32 — table size implausibly large");
    }
    (magics, offset as usize)
}

// ---------- validation: rebuild from scratch + differential ----------

/// Build the in-memory attack table for `piece` using the search results,
/// then run a (sq, subset) differential against `slow_attacks`. Mirrors the
/// runtime `LazyLock` build path so codegen exercises that allocation+fill
/// shape end-to-end (Decision §10 step 2).
///
/// Any mismatch: `eprintln!` + `exit(1)`.
fn validate(piece: Piece, magics: &[PieceMagic], table_len: usize) {
    assert_eq!(magics.len(), 64, "must have one magic per square");

    let mut table = vec![0u64; table_len];

    // Fill the table — same algorithm the runtime LazyLock uses.
    for (i, sq) in Square::all().enumerate() {
        let m = magics[i];
        let shift = m.shift as u32;
        for_each_subset(m.mask, |subset| {
            let idx = (subset.wrapping_mul(m.magic) >> shift) as usize;
            let slot_index = m.offset as usize + idx;
            let attack = piece.attacks(sq, Bitboard(subset));
            let existing = table[slot_index];
            if existing == 0 {
                table[slot_index] = attack;
            } else if existing != attack {
                eprintln!(
                    "magicgen: destructive collision while filling table for {} on {sq} \
                     (existing = 0x{existing:016x}, new = 0x{attack:016x})",
                    piece.name()
                );
                std::process::exit(1);
            }
        });
    }

    // Differential pass: every (sq, subset) lookup must match slow_attacks.
    for (i, sq) in Square::all().enumerate() {
        let m = magics[i];
        let shift = m.shift as u32;
        for_each_subset(m.mask, |subset| {
            let idx = (subset.wrapping_mul(m.magic) >> shift) as usize;
            let got = table[m.offset as usize + idx];
            let expected = piece.attacks(sq, Bitboard(subset));
            if got != expected {
                eprintln!(
                    "magicgen: differential mismatch for {} on {sq}, occ = 0x{subset:016x}: \
                     got 0x{got:016x}, expected 0x{expected:016x}",
                    piece.name()
                );
                std::process::exit(1);
            }
        });
    }
}

// ---------- source-file emission ----------

/// Format the generated `src/magic/constants.rs` content per Decision §9.
fn format_constants(
    rook_magics: &[PieceMagic],
    rook_total: usize,
    bishop_magics: &[PieceMagic],
    bishop_total: usize,
) -> String {
    let mut s = String::new();
    // Header + imports. `use` ordering matches rustfmt's group order
    // (super::* before crate::*).
    s.push_str(
        "// AUTO-GENERATED by `cargo run --release --bin magicgen`. Do not edit by hand.\n\
         //\n\
         // Re-run magicgen to regenerate. The generator is deterministic given its\n\
         // PRNG seed (currently 0xC0FFEE_C0FFEE); committed constants must match a\n\
         // fresh run with the same seed.\n\
         \n\
         use super::Magic;\n\
         use crate::bitboard::Bitboard;\n\
         \n",
    );

    writeln!(s, "pub(super) const ROOK_TABLE_LEN: usize = {rook_total};").unwrap();
    writeln!(
        s,
        "pub(super) const BISHOP_TABLE_LEN: usize = {bishop_total};"
    )
    .unwrap();
    s.push('\n');

    s.push_str("pub(super) const ROOK_MAGICS: [Magic; 64] = [\n");
    for m in rook_magics {
        write_magic_line(&mut s, m);
    }
    s.push_str("];\n\n");

    s.push_str("pub(super) const BISHOP_MAGICS: [Magic; 64] = [\n");
    for m in bishop_magics {
        write_magic_line(&mut s, m);
    }
    s.push_str("];\n");

    s
}

fn write_magic_line(s: &mut String, m: &PieceMagic) {
    // Multi-line struct literal — matches rustfmt's canonical layout for
    // long struct initializers, so committed constants pass `rustfmt --check`
    // and a fresh magicgen re-run produces a byte-identical file.
    writeln!(s, "    Magic {{").unwrap();
    writeln!(s, "        mask: Bitboard(0x{:016x}),", m.mask).unwrap();
    writeln!(s, "        magic: 0x{:016x},", m.magic).unwrap();
    writeln!(s, "        shift: {},", m.shift).unwrap();
    writeln!(s, "        offset: {},", m.offset).unwrap();
    writeln!(s, "    }},").unwrap();
}

/// Atomic write: open `<output>.tmp`, write, close, rename to `<output>`.
fn atomic_write(path: &Path, content: &str) {
    // Build `<output>.tmp` by appending to the OsString, not via
    // `set_extension` (which would *replace* the existing extension —
    // turning `constants.rs` into `constants.tmp` instead of
    // `constants.rs.tmp`).
    let mut tmp_os = path.as_os_str().to_owned();
    tmp_os.push(".tmp");
    let tmp_path = PathBuf::from(tmp_os);

    {
        let mut f = fs::File::create(&tmp_path).unwrap_or_else(|e| {
            eprintln!(
                "magicgen: failed to open {} for writing: {e}",
                tmp_path.display()
            );
            std::process::exit(1);
        });
        f.write_all(content.as_bytes()).unwrap_or_else(|e| {
            eprintln!("magicgen: failed to write to {}: {e}", tmp_path.display());
            std::process::exit(1);
        });
        // The subsequent `fs::rename` is the durability point on POSIX; sync_all
        // before rename buys nothing here for a small generated file. Skipping.
    }

    fs::rename(&tmp_path, path).unwrap_or_else(|e| {
        eprintln!(
            "magicgen: failed to rename {} to {}: {e}",
            tmp_path.display(),
            path.display()
        );
        std::process::exit(1);
    });
}

// ---------- main ----------

fn main() {
    let out_path: PathBuf = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| "src/magic/constants.rs".into());

    let mut rng = SplitMix64::new(PRNG_SEED);

    // 1. Search rook + bishop magics. Order is load-bearing (rook first,
    //    LERF squares within each piece) — see file-level doc-comment.
    let (rook_magics, rook_total) = search_all(Piece::Rook, &mut rng);
    let (bishop_magics, bishop_total) = search_all(Piece::Bishop, &mut rng);

    // 2. Validate in-memory: build attack tables (mirrors the runtime
    //    LazyLock build path) + differential against slow_attacks.
    validate(Piece::Rook, &rook_magics, rook_total);
    validate(Piece::Bishop, &bishop_magics, bishop_total);

    // 3. Emit the source file via atomic write.
    let content = format_constants(&rook_magics, rook_total, &bishop_magics, bishop_total);
    atomic_write(&out_path, &content);

    // 4. Sanity print.
    println!(
        "rook: {} entries ({:.1} KiB); bishop: {} entries ({:.1} KiB)",
        rook_total,
        (rook_total * 8) as f64 / 1024.0,
        bishop_total,
        (bishop_total * 8) as f64 / 1024.0,
    );
}
