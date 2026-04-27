//! Integration tests for the perft module (M1.G).
//!
//! Per `docs/plans/m1.g.md` §"Test layout — Integration tests". Tests
//! consume the public crate-root API (`chess::*`) only.
//!
//! ## Partition
//!
//! Per the plan's §"Fast-suite wall-clock budget":
//!
//! - **Fast (default `cargo test`):** D1–D3 across all canonical 6
//!   (`perft_bulk` only — parity in unit tests), plus D4 on Pos-3 and
//!   Pos-4. Plus a divide sanity check at D4 from the starting position.
//! - **Ignored (`cargo test --release -- --ignored`):** D4 on the four
//!   heavy positions (starting, Kiwipete, Pos-5, Pos-6); D5 on all six;
//!   D6 on all six; Whittington EPD D4.
//!
//! All counts come from `tests/fixtures/perft_canonical6.txt` and
//! `tests/fixtures/perft_whittington.epd`, generated from Stockfish 18
//! per ADR-0006. The fixture format is EPD-style (`<FEN>; D1 N; D2 N;
//! ...`); a tiny parser at the bottom of this file handles both.

use std::collections::BTreeMap;

use chess::{Position, divide, perft, perft_bulk};

// -----------------------------------------------------------------------
// Fixture loading.
// -----------------------------------------------------------------------

/// One row from a perft fixture file: a position plus a `BTreeMap` of
/// depth → expected leaf count. Using `BTreeMap` (not `[u64; N]`) so we
/// can mix depths cleanly across the canonical-6 (D1–D6) and Whittington
/// (D1–D4) corpora without parser branches.
struct PerftFixture {
    fen: String,
    counts: BTreeMap<u32, u64>,
}

/// Parse an EPD-style perft fixture. Lines starting with `#` and blank
/// lines are skipped. Each non-comment line has form `<FEN>; D1 N; D2 N;
/// ...`. Returns one `PerftFixture` per non-comment line.
///
/// Per plan §13: the helper accepts only `Dn N` tokens and **silently
/// ignores** other tokens (other tokens in EPD aren't perft-relevant —
/// the upstream `perft.epd` may carry EPD-extension fields that this
/// project doesn't consume). Hard parse failure on FEN issues
/// (empty FEN, garbled `D` token where `n` isn't a number) — those
/// indicate fixture corruption, not benign EPD extension.
fn parse_fixture(text: &str, source_path: &str) -> Vec<PerftFixture> {
    let mut out = Vec::new();
    for (lineno_minus_one, line) in text.lines().enumerate() {
        let lineno = lineno_minus_one + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut parts = trimmed.split(';').map(str::trim);
        let fen = parts.next().unwrap_or("").to_string();
        if fen.is_empty() {
            panic!("{source_path}:{lineno}: empty FEN — malformed line: {line:?}");
        }
        let mut counts = BTreeMap::new();
        for token in parts {
            if token.is_empty() {
                continue;
            }
            // Token is like "D3 8902". Anything not starting with `D`
            // followed by a digit is a non-perft EPD extension we skip.
            let mut it = token.split_whitespace();
            let depth_tok = it.next().unwrap_or("");
            let count_tok = it.next().unwrap_or("");
            // Skip non-Dn tokens (per plan §13). A `D`-prefixed token
            // with a non-numeric suffix IS an error — that's corrupted
            // perft data, not benign extension.
            if !depth_tok.starts_with('D') {
                continue;
            }
            let depth: u32 = depth_tok[1..]
                .parse()
                .unwrap_or_else(|_| panic!("{source_path}:{lineno}: bad depth in {depth_tok:?}"));
            let count: u64 = count_tok
                .parse()
                .unwrap_or_else(|_| panic!("{source_path}:{lineno}: bad count in {count_tok:?}"));
            if it.next().is_some() {
                panic!("{source_path}:{lineno}: extra tokens in {token:?}");
            }
            if counts.insert(depth, count).is_some() {
                panic!("{source_path}:{lineno}: duplicate D{depth} token in {line:?}");
            }
        }
        if counts.is_empty() {
            panic!("{source_path}:{lineno}: no Dn N tokens parsed from {line:?}");
        }
        out.push(PerftFixture { fen, counts });
    }
    out
}

const CANONICAL_FIXTURE_PATH: &str = "tests/fixtures/perft_canonical6.txt";
const CANONICAL_FIXTURE_TEXT: &str = include_str!("../tests/fixtures/perft_canonical6.txt");

const WHITTINGTON_FIXTURE_PATH: &str = "tests/fixtures/perft_whittington.epd";
const WHITTINGTON_FIXTURE_TEXT: &str = include_str!("../tests/fixtures/perft_whittington.epd");

fn canonical_six() -> Vec<PerftFixture> {
    let f = parse_fixture(CANONICAL_FIXTURE_TEXT, CANONICAL_FIXTURE_PATH);
    assert_eq!(
        f.len(),
        6,
        "canonical-6 fixture must have exactly six positions; got {}",
        f.len()
    );
    f
}

/// The committed Whittington fixture has exactly 174 positions. Pin the
/// count exactly so a parser regression (silent skips) or a fixture
/// regeneration that drops rows fails loudly.
const WHITTINGTON_EXPECTED_LEN: usize = 174;

fn whittington() -> Vec<PerftFixture> {
    let f = parse_fixture(WHITTINGTON_FIXTURE_TEXT, WHITTINGTON_FIXTURE_PATH);
    assert_eq!(
        f.len(),
        WHITTINGTON_EXPECTED_LEN,
        "Whittington fixture position count regression",
    );
    f
}

fn parse(fen: &str) -> Position {
    Position::from_fen(fen).unwrap_or_else(|e| panic!("FEN must parse: {fen} ({e:?})"))
}

// -----------------------------------------------------------------------
// Canonical-6 — fast suite (D1–D3 all + D4 light).
// -----------------------------------------------------------------------

/// D1–D3 across all six canonical positions, `perft_bulk` only — bulk
/// is the headline path and plain-vs-bulk parity is exhaustively
/// asserted in the unit tests at D2 (3 positions) and D3 (Kiwipete).
/// Skipping plain here keeps the fast suite under the 10 s budget.
/// Run by default via `cargo test`.
#[test]
fn canonical_six_fast_d1_d3() {
    for fix in canonical_six() {
        let pos = parse(&fix.fen);
        for d in 1..=3 {
            let expected = *fix
                .counts
                .get(&d)
                .unwrap_or_else(|| panic!("fixture missing depth {d} for {}", fix.fen));
            assert_eq!(
                perft_bulk(&pos, d),
                expected,
                "perft_bulk({}, {d})",
                fix.fen
            );
        }
    }
}

/// D4 on the two lightest canonical positions (Pos-3, Pos-4).
/// `perft_bulk` only — bulk-vs-plain parity is asserted in the unit
/// tests at smaller depths and on the heavy-D4 ignored test. Pos-5 D4
/// (2.1M leaves) was demoted to the ignored heavy-D4 set during
/// final-review iteration 1: at debug-build throughput it pushed the
/// fast suite over the agreed 10 s budget.
#[test]
fn canonical_six_fast_d4_light_only() {
    // The two lightest D4 cases by leaf count: Pos-3 (43k), Pos-4 (422k).
    const LIGHT_FENS: &[&str] = &[
        "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
        "r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1",
    ];
    let fixtures = canonical_six();
    for fen in LIGHT_FENS {
        let fix = fixtures
            .iter()
            .find(|f| f.fen == *fen)
            .unwrap_or_else(|| panic!("light position not in fixture: {fen}"));
        let expected = *fix.counts.get(&4).unwrap();
        let pos = parse(&fix.fen);
        assert_eq!(perft_bulk(&pos, 4), expected, "perft_bulk({fen}, 4)");
    }
}

// -----------------------------------------------------------------------
// Divide sanity at depth 4 from the starting position.
// -----------------------------------------------------------------------

#[test]
fn divide_starting_position_depth_4_matches_canonical() {
    // 20 entries (one per legal move at depth 1 from start), summing to
    // perft(start, 4) = 197_281. UCI-sortedness asserted per plan
    // §"Public API".
    let pos = parse("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1");
    let entries = divide(&pos, 4);

    assert_eq!(entries.len(), 20, "20 legal moves from starting position");

    let sum: u64 = entries.iter().map(|(_, n)| n).sum();
    assert_eq!(sum, 197_281, "divide sum must equal perft(starting, 4)");

    let uci: Vec<String> = entries.iter().map(|(m, _)| m.to_string()).collect();
    let mut expected_sorted = uci.clone();
    expected_sorted.sort();
    assert_eq!(
        uci, expected_sorted,
        "divide entries must be UCI-sorted ascending"
    );
}

// -----------------------------------------------------------------------
// Canonical-6 — ignored slow suite.
// -----------------------------------------------------------------------

/// D4 on the four heavy canonical positions (starting, Kiwipete, Pos-5,
/// Pos-6). Plain + bulk parity asserted — the strongest single
/// regression check at higher depth (Kiwipete D4 covers captures, EP,
/// castling, promotions). Pos-5 demoted from the fast suite during
/// final-review iteration 1 (see `canonical_six_fast_d4_light_only`).
///
/// Expected runtime at the §10 throughput estimate (~1 Mnps plain,
/// ~1.4 Mnps bulk): ~15 s combined for the four positions across both
/// paths.
#[test]
#[ignore]
fn canonical_six_d4_heavy_slow() {
    const HEAVY_FENS: &[&str] = &[
        "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
        "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
        "r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10",
    ];
    let fixtures = canonical_six();
    for fen in HEAVY_FENS {
        let fix = fixtures
            .iter()
            .find(|f| f.fen == *fen)
            .unwrap_or_else(|| panic!("heavy position not in fixture: {fen}"));
        let expected = *fix.counts.get(&4).unwrap();
        let pos = parse(&fix.fen);
        assert_eq!(perft(&pos, 4), expected, "perft({fen}, 4) plain");
        assert_eq!(perft_bulk(&pos, 4), expected, "perft_bulk({fen}, 4)");
    }
}

/// All six canonical positions at D5. `perft_bulk` only.
///
/// Expected runtime: O(minutes) at the §10 estimate. Run via:
///     cargo test --release -- --ignored canonical_six_d5_slow
#[test]
#[ignore]
fn canonical_six_d5_slow() {
    for fix in canonical_six() {
        let expected = *fix
            .counts
            .get(&5)
            .unwrap_or_else(|| panic!("fixture missing depth 5 for {}", fix.fen));
        let pos = parse(&fix.fen);
        assert_eq!(perft_bulk(&pos, 5), expected, "perft_bulk({}, 5)", fix.fen);
    }
}

/// All six canonical positions at D6. `perft_bulk` only. Most-aggressive
/// validation gate.
///
/// Expected runtime: very long. Kiwipete D6 = 8.0B nodes — at 1.4 Mnps
/// bulk that's ~95 minutes; the test scales with throughput improvement.
/// Run only when validating the milestone or chasing a regression at
/// depth.
///     cargo test --release -- --ignored canonical_six_d6_slow
#[test]
#[ignore]
fn canonical_six_d6_slow() {
    for fix in canonical_six() {
        let expected = *fix
            .counts
            .get(&6)
            .unwrap_or_else(|| panic!("fixture missing depth 6 for {}", fix.fen));
        let pos = parse(&fix.fen);
        assert_eq!(perft_bulk(&pos, 6), expected, "perft_bulk({}, 6)", fix.fen);
    }
}

// -----------------------------------------------------------------------
// Whittington EPD regression — ignored slow suite.
// -----------------------------------------------------------------------

/// Iterate the Whittington fixture and assert `perft_bulk(pos, 4) ==
/// fixture[D4]` for each. 174 positions; expected ~2 minutes at the
/// §10 estimate of 1.4 Mnps. Investigate if >5 minutes (risk-table entry).
///
///     cargo test --release -- --ignored whittington_epd_d4_slow
///
/// FENs that our parser rejects are skipped with a logged warning (per
/// plan §13: "we accept dropping any line that fails our FEN parser");
/// the test body counts skipped FENs and asserts the post-skip count is
/// still close to the full corpus (≥ 90% of the fixture).
#[test]
#[ignore]
fn whittington_epd_d4_slow() {
    let fixtures = whittington();
    let total = fixtures.len();
    let mut checked = 0usize;
    let mut skipped = Vec::new();
    for fix in fixtures {
        let expected = match fix.counts.get(&4) {
            Some(n) => *n,
            None => panic!(
                "Whittington fixture missing D4 for {}: regen the fixture",
                fix.fen
            ),
        };
        let pos = match Position::from_fen(&fix.fen) {
            Ok(p) => p,
            Err(e) => {
                skipped.push((fix.fen.clone(), format!("{e:?}")));
                continue;
            }
        };
        let got = perft_bulk(&pos, 4);
        assert_eq!(
            got, expected,
            "Whittington perft_bulk({}, 4) mismatch",
            fix.fen
        );
        checked += 1;
    }
    eprintln!(
        "Whittington D4: {checked} checked, {} skipped (out of {total}).",
        skipped.len()
    );
    for (fen, err) in &skipped {
        eprintln!("  skipped: {fen:?}  ({err})");
    }
    // The committed fixture's FENs all came from Stockfish-accepted EPD
    // input. If our FEN parser rejects more than 10% of them, that's a
    // parser-strictness regression worth investigating, not just a
    // benign skip. Threshold: at least 90% must validate.
    let min_required = (total * 9) / 10;
    assert!(
        checked >= min_required,
        "Whittington D4: only {checked}/{total} validated (< 90%). Investigate parser strictness.",
    );
}

// -----------------------------------------------------------------------
// Fixture-parser sanity tests (ensure the parser itself works as
// expected — guards against silent regressions in the fixture loader).
// -----------------------------------------------------------------------

#[test]
fn fixture_parser_skips_comments_and_blanks() {
    let text = "\
# header comment
# another
\n\
rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1; D1 20; D2 400
";
    let f = parse_fixture(text, "test://inline");
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].counts.get(&1), Some(&20));
    assert_eq!(f[0].counts.get(&2), Some(&400));
    assert_eq!(f[0].counts.get(&3), None);
}

#[test]
fn fixture_parser_skips_non_d_tokens() {
    // Per plan §13 the parser silently ignores tokens that aren't `Dn N`
    // pairs (EPD-extension fields aren't perft-relevant). The position
    // here has both an extension token (`bm Bxa1`) and a valid D1.
    let text = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1; bm Bxa1; D1 20\n";
    let f = parse_fixture(text, "test://inline");
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].counts.get(&1), Some(&20));
}

#[test]
#[should_panic(expected = "bad depth in")]
fn fixture_parser_rejects_corrupted_d_token() {
    // A `D`-prefixed token with a non-numeric suffix is corruption, not
    // benign EPD extension — the parser must hard-fail.
    let text = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1; Dx 20\n";
    let _ = parse_fixture(text, "test://inline");
}

#[test]
#[should_panic(expected = "no Dn N tokens parsed")]
fn fixture_parser_rejects_no_count_tokens() {
    let text = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1\n";
    let _ = parse_fixture(text, "test://inline");
}

#[test]
#[should_panic(expected = "duplicate D")]
fn fixture_parser_rejects_duplicate_depth_token() {
    // A second `D1 N` overwriting the first would silently lose data;
    // the parser must hard-fail to surface a regen-script regression.
    let text = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1; D1 20; D1 30\n";
    let _ = parse_fixture(text, "test://inline");
}

#[test]
#[should_panic(expected = "no Dn N tokens parsed")]
fn fixture_parser_rejects_only_extension_tokens() {
    // If a line has tokens but none are `Dn N`, that's not perft data.
    let text = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1; bm Bxa1; ce 100\n";
    let _ = parse_fixture(text, "test://inline");
}

#[test]
fn fixture_parser_canonical_6_loads_all_depths() {
    // Sanity check on the actual committed fixture: every canonical-6
    // entry has D1..=D6.
    for fix in canonical_six() {
        for d in 1..=6 {
            assert!(
                fix.counts.contains_key(&d),
                "canonical-6 fixture {} missing D{d}",
                fix.fen
            );
        }
    }
}
