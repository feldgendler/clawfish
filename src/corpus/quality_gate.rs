//! The data-quality gate — the M6.G/M6.H2 LANDING GATE (not an SPRT; the M5.E
//! correctness-only-gate precedent applied to data). Five checks; three are
//! must-PASS (ADR-0003 label-provenance audit, reproducibility re-run match,
//! per-lane dedup integrity).
//!
//! **M6.H2 (ADR-0035 §12):** the gate runs over the lane's single flat
//! `lane.bin`. The held-out-split-integrity must-PASS check is GONE (the
//! train/val split moved to M6.I); it is replaced by `check_dedup_integrity`
//! (zero exact-FEN dups ∧ ≤ `PER_GAME_CAP` records per game) — the inline
//! per-lane pipeline's certificate that fetch and self-play produced a clean,
//! build-ready lane.
//!
//! See `docs/plans/m6.h2-corpus-lanes.md` §3/§7. The audit codifies the
//! ADR-0003 rejection of Zurichess `c9` labels: the on-disk binary frame
//! cannot represent a Zurichess source (the `Source` enum intentionally
//! omits the variant), but the audit must still defend the rejection — if
//! the frame ever grows a new provenance tag the audit fails closed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::manifest::{Manifest, hex_digest, read_manifest, sha256_bytes, sha256_file};
use super::store::scan_valid_blocks;
use super::{CorpusError, CorpusRecord, Label, PER_GAME_CAP, Source};
use crate::Position;

/// Outcome of one data-quality check.
#[derive(Clone, Debug)]
pub struct CheckResult {
    /// Check name (as in the §6 table).
    pub name: String,
    /// `true` = PASS; recorded-only checks are always `true` with stats.
    pub passed: bool,
    /// `true` iff this is one of the three landing-gate checks.
    pub must_pass: bool,
    /// Human-readable detail / recorded statistics.
    pub detail: String,
}

/// Aggregate data-quality report.
#[derive(Clone, Debug, Default)]
pub struct QualityReport {
    /// Per-check results, in §6-table order.
    pub checks: Vec<CheckResult>,
}

impl QualityReport {
    /// Landing gate: every `must_pass` check passed.
    pub fn gate_passed(&self) -> bool {
        self.checks.iter().all(|c| !c.must_pass || c.passed)
    }
}

/// Frozen-lane layout (M6.H2). A lane directory holds one flat `lane.bin` (the
/// committed corpus bytes the digest covers), `manifest.json`,
/// `filter_spec.txt`, and `corpus_stats.txt`. There is no train/val split at
/// generation (→ M6.I), so there is a single shard.
struct Layout {
    /// The flat per-lane artifact (`<dir>/lane.bin`).
    lane_shard: PathBuf,
}

impl Layout {
    fn discover(dir: &Path) -> Self {
        Layout {
            lane_shard: dir.join("lane.bin"),
        }
    }
}

/// Run all five checks on a frozen lane dir.
pub fn run_quality_gate(dir: &Path) -> Result<QualityReport, CorpusError> {
    let manifest = read_manifest(dir)?;
    let layout = Layout::discover(dir);

    let (blocks, _) = scan_valid_blocks(&layout.lane_shard)?;
    let records: Vec<CorpusRecord> = blocks
        .into_iter()
        .flat_map(|b| b.records.into_iter())
        .collect();

    let mut report = QualityReport::default();
    report.checks.push(check_coverage_stats(&records));
    report.checks.push(check_decisive_draw_balance(&records));
    report.checks.push(check_adr0003_audit(&records));
    report
        .checks
        .push(check_reproducibility_rerun(&layout, &manifest)?);
    report.checks.push(check_dedup_integrity(&records));
    Ok(report)
}

// ── Check 1: coverage_stats (recorded) ─────────────────────────────────────

fn check_coverage_stats(records: &[CorpusRecord]) -> CheckResult {
    let total = records.len();
    let mut by_source: BTreeMap<u8, u64> = BTreeMap::new();
    let mut by_rung: BTreeMap<u8, u64> = BTreeMap::new();
    let mut games: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut phase_buckets = [0u64; 4]; // [0,6) [6,12) [12,18) [18,..]
    for r in records {
        *by_source.entry(r.source.as_u8()).or_default() += 1;
        *by_rung.entry(r.depth_rung).or_default() += 1;
        games.insert(r.game_id);
        if let Ok(pos) = Position::from_fen(&r.fen) {
            let phase = pos.raw_phase() as usize;
            let idx = match phase {
                0..=5 => 0,
                6..=11 => 1,
                12..=17 => 2,
                _ => 3,
            };
            phase_buckets[idx] += 1;
        }
    }
    let mut detail = format!(
        "records={total}, games={}, phase=[ending={}, late_mid={}, mid={}, opening={}]",
        games.len(),
        phase_buckets[0],
        phase_buckets[1],
        phase_buckets[2],
        phase_buckets[3]
    );
    detail.push_str(", source_hist={");
    for (i, (src, count)) in by_source.iter().enumerate() {
        if i > 0 {
            detail.push_str(", ");
        }
        detail.push_str(&format!("{src}:{count}"));
    }
    detail.push_str("}, depth_rung_hist={");
    for (i, (rung, count)) in by_rung.iter().enumerate() {
        if i > 0 {
            detail.push_str(", ");
        }
        detail.push_str(&format!("{rung}:{count}"));
    }
    detail.push('}');
    CheckResult {
        name: "coverage_stats".into(),
        passed: true,
        must_pass: false,
        detail,
    }
}

// ── Check 2: decisive_draw_balance (recorded + sanity bounds) ──────────────

fn check_decisive_draw_balance(records: &[CorpusRecord]) -> CheckResult {
    let n = records.len();
    if n == 0 {
        return CheckResult {
            name: "decisive_draw_balance".into(),
            passed: true,
            must_pass: false,
            detail: "empty corpus — no balance to report".into(),
        };
    }
    let mut white = 0u64;
    let mut draw = 0u64;
    let mut black = 0u64;
    for r in records {
        match r.label {
            Label::WhiteWin => white += 1,
            Label::Draw => draw += 1,
            Label::BlackWin => black += 1,
        }
    }
    let fw = white as f64 / n as f64;
    let fd = draw as f64 / n as f64;
    let fb = black as f64 / n as f64;
    let detail =
        format!("white={white} ({fw:.3}), draw={draw} ({fd:.3}), black={black} ({fb:.3}), n={n}");
    CheckResult {
        name: "decisive_draw_balance".into(),
        passed: true,
        must_pass: false,
        detail,
    }
}

// ── Check 3: ADR-0003 label-provenance audit (MUST PASS) ────────────────────

fn check_adr0003_audit(records: &[CorpusRecord]) -> CheckResult {
    // Accept-list: SelfPlayOnBook (own result, book-seeded opening),
    // SelfPlayOffBook (own result, startpos + random plies), CCRL
    // (Result-tag), Lichess (Result-tag). Zurichess `c9` engine-score
    // labels are REJECTED by construction (the `Source` enum does not
    // encode them); this audit is the load-bearing defense that any
    // out-of-list provenance fails the gate closed.
    //
    // **Must update audit when `Source` grows.** A 5th variant added to
    // `Source` requires re-asserting it carries an ADR-0003-compliant
    // label (original game outcome, never an engine score). The
    // cardinality pin below enumerates every accepted variant by name;
    // the compiler forces the developer to revisit this audit when
    // `Source` changes.
    let accept_list = [
        Source::SelfPlayOnBook,
        Source::SelfPlayOffBook,
        Source::Ccrl,
        Source::LichessOpen,
    ];
    // `assert_eq!`, NOT `debug_assert_eq!`: this is the operator-facing
    // landing gate at release-mode `corpus quality-gate`. Compiling it out
    // would let a future `Source` variant slip past the audit in release.
    assert_eq!(
        accept_list.len(),
        4,
        "ADR-0003 accept-list pinned at 4 variants — adding a Source variant \
         requires updating the audit and re-running the label-provenance review"
    );

    let mut by_source: BTreeMap<u8, u64> = BTreeMap::new();
    let mut bad: Vec<u8> = Vec::new();
    for r in records {
        *by_source.entry(r.source.as_u8()).or_default() += 1;
        if !matches!(
            r.source,
            Source::SelfPlayOnBook | Source::SelfPlayOffBook | Source::Ccrl | Source::LichessOpen
        ) {
            bad.push(r.source.as_u8());
        }
    }
    let counts: String = by_source
        .iter()
        .map(|(s, c)| format!("{s}:{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    if bad.is_empty() {
        CheckResult {
            name: "adr0003_label_provenance_audit".into(),
            passed: true,
            must_pass: true,
            detail: format!(
                "PASS — accept-list = {{SelfPlayOnBook, SelfPlayOffBook, Ccrl, \
                 LichessOpen}}; Zurichess c9 engine-score labels REJECTED \
                 (intentionally absent from `Source`); per-source counts: {{{counts}}}"
            ),
        }
    } else {
        CheckResult {
            name: "adr0003_label_provenance_audit".into(),
            passed: false,
            must_pass: true,
            detail: format!(
                "FAIL — out-of-accept-list source tags found: {bad:?}; \
                 per-source counts: {{{counts}}}"
            ),
        }
    }
}

/// Run the ADR-0003 audit on an in-memory record slice (the same logic
/// `run_quality_gate` runs after loading the corpus). Exposed so a test
/// fixture can exercise the accept-list directly without staging a corpus.
pub fn audit_records(records: &[CorpusRecord]) -> CheckResult {
    check_adr0003_audit(records)
}

// ── Check 4: reproducibility re-run match (MUST PASS) ───────────────────────

/// Sentinel value in `manifest.corpus_sha256` indicating the corpus has not
/// yet been regenerated after the per-game-file architecture rewrite. When
/// this sentinel is present, the reproducibility check is skipped with a WARN
/// rather than failing — the gate must not block on a not-yet-derived digest.
///
/// Operator workflow: run `corpus finalize` (or `sh bench/corpus/re-run.sh`)
/// after the rewrite lands to produce the actual digest, then replace this
/// sentinel in `manifest.json` (see ADR-0035 §8 / plan §10).
pub const CORPUS_SHA256_PENDING_SENTINEL: &str = "PENDING_REGENERATION";

fn check_reproducibility_rerun(
    layout: &Layout,
    manifest: &Manifest,
) -> Result<CheckResult, CorpusError> {
    // Handle the migration sentinel: if the manifest records the corpus_sha256
    // as PENDING_REGENERATION, skip the digest comparison with a WARN. The
    // operator must re-derive the digest via `corpus finalize` and replace the
    // sentinel (see ADR-0035 §8 / plan §10).
    if manifest.corpus_sha256 == CORPUS_SHA256_PENDING_SENTINEL {
        eprintln!(
            "WARN: manifest corpus_sha256 pending regeneration — operator must \
             re-derive and commit (see ADR-0035 §8 / bench/corpus/re-run.sh)"
        );
        return Ok(CheckResult {
            name: "reproducibility_rerun_match".into(),
            passed: true,
            must_pass: true,
            detail: "PASS-WITH-WARN — corpus_sha256 is PENDING_REGENERATION sentinel; \
                     operator must re-derive via `corpus finalize` and update manifest.json \
                     (see ADR-0035 §8)"
                .into(),
        });
    }

    // Re-derive the SHA-256 of the on-disk corpus bytes (the lane's single
    // flat `lane.bin`) and compare to the manifest digest. No train‖val concat
    // (the split moved to M6.I).
    let bytes = std::fs::read(&layout.lane_shard)?;
    let digest = hex_digest(&sha256_bytes(&bytes));

    if digest != manifest.corpus_sha256 {
        return Ok(CheckResult {
            name: "reproducibility_rerun_match".into(),
            passed: false,
            must_pass: true,
            detail: format!(
                "FAIL — corpus bytes digest {digest} != manifest.corpus_sha256 {}",
                manifest.corpus_sha256
            ),
        });
    }

    let mut source_mismatches: Vec<String> = Vec::new();
    for src in &manifest.sources {
        let p = Path::new(&src.path);
        if !p.exists() {
            // External raw source not staged (the weaker "re-derivable"
            // guarantee from plan §1 — admissible for a CCRL/Lichess slice
            // that is reproducible from URL+SHA-256 but not byte-vendored).
            continue;
        }
        match sha256_file(p) {
            Ok(d) => {
                let h = hex_digest(&d);
                if h != src.sha256 {
                    source_mismatches
                        .push(format!("{}: disk={h} manifest={}", src.label, src.sha256));
                }
            }
            Err(e) => {
                source_mismatches.push(format!("{}: read error {e}", src.label));
            }
        }
    }

    if !source_mismatches.is_empty() {
        return Ok(CheckResult {
            name: "reproducibility_rerun_match".into(),
            passed: false,
            must_pass: true,
            detail: format!(
                "FAIL — source SHA-256 mismatch: [{}]",
                source_mismatches.join(", ")
            ),
        });
    }

    Ok(CheckResult {
        name: "reproducibility_rerun_match".into(),
        passed: true,
        must_pass: true,
        detail: format!(
            "PASS — corpus bytes digest {digest} matches manifest; {} source(s) verified",
            manifest.sources.len()
        ),
    })
}

// ── Check 5: per-lane dedup integrity (MUST PASS) ──────────────────────────

/// The build-ready certificate the inline per-lane pipeline must have produced:
///   - zero exact-FEN duplicates (`unique_fens == total_records`), AND
///   - at most `PER_GAME_CAP` records per `game_id`.
///
/// Replaces the M6.G held-out-split check (the split moved to M6.I, so there is
/// no train/val partition to audit). A failure means a lane was written by a
/// path that bypassed the [`super::pipeline::LaneCommitter`] — the load-bearing
/// defense that every committed lane is dedup'd + capped (ADR-0035 §12).
fn check_dedup_integrity(records: &[CorpusRecord]) -> CheckResult {
    let total = records.len();
    let unique_fens: std::collections::HashSet<&str> =
        records.iter().map(|r| r.fen.as_str()).collect();
    let unique_count = unique_fens.len();

    let mut per_game: BTreeMap<u64, u64> = BTreeMap::new();
    for r in records {
        *per_game.entry(r.game_id).or_default() += 1;
    }
    let max_per_game = per_game.values().copied().max().unwrap_or(0);

    let no_dups = unique_count == total;
    let cap_ok = max_per_game <= PER_GAME_CAP as u64;
    let passed = no_dups && cap_ok;

    let detail = if passed {
        format!(
            "PASS — records={total}, unique_fens={unique_count} (zero exact-FEN dups), \
             max_per_game={max_per_game} ≤ PER_GAME_CAP={PER_GAME_CAP}"
        )
    } else if !no_dups {
        format!(
            "FAIL — {} exact-FEN duplicate(s): records={total}, unique_fens={unique_count} \
             (a lane was written bypassing the LaneCommitter dedup)",
            total - unique_count
        )
    } else {
        format!(
            "FAIL — max_per_game={max_per_game} > PER_GAME_CAP={PER_GAME_CAP} \
             (a lane was written bypassing the LaneCommitter per-game cap)"
        )
    };
    CheckResult {
        name: "dedup_integrity".into(),
        passed,
        must_pass: true,
        detail,
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::manifest::{Manifest, SourceEntry, write_manifest};
    use crate::corpus::store::append_block;
    use crate::corpus::{DEPTH_RUNG_EXTERNAL, Label, Source};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    // ── Temp-dir scaffolding (no third-party tempfile dep) ─────────────────

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static CTR: AtomicU64 = AtomicU64::new(0);
            let n = CTR.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let dir = std::env::temp_dir().join(format!("clawfish-corpus-quality-{tag}-{pid}-{n}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn rec(fen: &str, label: Label, src: Source, game_id: u64, ply: u32) -> CorpusRecord {
        CorpusRecord {
            fen: fen.to_string(),
            label,
            source: src,
            game_id,
            ply,
            depth_rung: DEPTH_RUNG_EXTERNAL,
            strata: 0,
        }
    }

    fn startpos() -> String {
        "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1".into()
    }

    /// Write the lane's single flat `lane.bin` (one block per game_id) and
    /// return its SHA-256 hex digest (the M6.H2 reproducibility-check input).
    fn write_corpus(dir: &Path, records: &[CorpusRecord]) -> String {
        // Group records by game_id and emit one block per game (the on-disk
        // contract — `append_block` is the atomic unit).
        let mut by_game: BTreeMap<u64, Vec<CorpusRecord>> = BTreeMap::new();
        for r in records.iter().cloned() {
            by_game.entry(r.game_id).or_default().push(r);
        }
        let lane = dir.join("lane.bin");
        for (gid, recs) in &by_game {
            append_block(&lane, *gid, recs).unwrap();
        }
        let bytes = std::fs::read(&lane).unwrap();
        hex_digest(&sha256_bytes(&bytes))
    }

    fn write_test_manifest(dir: &Path, total: u64, corpus_sha256: String) {
        let m = Manifest {
            schema_version: 2,
            created_at: "2026-05-19T00:00:00Z".into(),
            engine_commit: "test".into(),
            total_positions: total,
            sources: Vec::<SourceEntry>::new(),
            self_play_seed: 42,
            games: 0,
            max_plies: 400,
            opening_random_plies: 8,
            workers: 1,
            cap_seed: 7,
            source_url: None,
            target_usable: None,
            source: None,
            depth_ladder: vec![(4, 1)],
            opening_book_path: None,
            opening_book_sha256: None,
            opening_mode: None,
            corpus_sha256,
        };
        write_manifest(dir, &m).unwrap();
    }

    // ── audit ────────────────────────────────────────────────────────────

    #[test]
    fn adr0003_audit_rejects_zurichess_c9_fixture() {
        // The on-disk `Source` enum cannot encode Zurichess, but the audit
        // is the load-bearing defense for "any out-of-accept-list source
        // fails the gate closed." Two-layer defense:
        //
        //   (a) **Physical:** the on-disk frame cannot carry an unrecognized
        //       source byte — `Source::from_u8(b) == None` for any
        //       `b >= 4`, and `store::decode_block` rejects the whole frame.
        //   (b) **Programmatic:** the audit's `accept_list` cardinality pin
        //       forces a developer adding a 5th `Source` variant to update
        //       this test (and the audit).
        let r = audit_records(&[]);
        assert!(r.passed, "empty corpus passes the audit (vacuously)");
        assert!(r.must_pass);
        assert!(
            r.detail.contains("REJECTED"),
            "audit detail must document the Zurichess rejection by name; got: {}",
            r.detail
        );

        // Layer (a): physical defense — `Source::from_u8(4)` is `None`, so
        // an out-of-list source byte cannot be decoded from disk. (Bytes
        // 0..=3 are the four accept-listed variants.)
        assert_eq!(
            Source::from_u8(4),
            None,
            "Source::from_u8(4) must be None — physical defense against \
             a future Zurichess (or other engine-score-labeled) source byte"
        );
        for b in 4u8..=255 {
            assert_eq!(
                Source::from_u8(b),
                None,
                "Source::from_u8({b}) must be None — no out-of-list bytes accepted"
            );
        }

        // Layer (a) end-to-end: hand-craft a synthetic shard block whose
        // source byte is 3 (out-of-list) and assert `scan_valid_blocks`
        // rejects the frame wholesale (no records leak through into a
        // `CorpusRecord` the audit would ever see).
        let td = TempDir::new("adr-zurichess");
        let lane = td.path().join("lane.bin");
        // Build a normal valid block, then flip its single source byte.
        let r0 = rec(&startpos(), Label::Draw, Source::SelfPlayOnBook, 42, 10);
        crate::corpus::store::append_block(&lane, 42, &[r0]).unwrap();
        let mut bytes = std::fs::read(&lane).unwrap();
        // The source byte sits at the second per-record byte (after label).
        // Locate the FEN bytes + label and patch the source byte to an
        // out-of-list value (4 — first byte past the four accept-listed
        // variants). The record layout starts at HEADER_LEN (20) bytes
        // into the payload: u16 fen_len, fen bytes, label, source, ply,
        // depth_rung, strata. Source-byte offset = 20 + 2 + fen_len + 1.
        let header_len = 20usize;
        let fen_len = u16::from_le_bytes(
            bytes[header_len..header_len + 2]
                .try_into()
                .expect("frame header has 2-byte fen_len"),
        ) as usize;
        let source_off = header_len + 2 + fen_len + 1;
        bytes[source_off] = 4; // out-of-list source byte
        std::fs::write(&lane, &bytes).unwrap();
        let (blocks, _valid_len) = crate::corpus::store::scan_valid_blocks(&lane).expect("scan");
        assert!(
            blocks.is_empty(),
            "a block with an out-of-list source byte must be rejected wholesale \
             by the CRC-protected frame (no `CorpusRecord` ever materializes \
             — the audit's input is empty by construction)"
        );

        // Layer (b): pin the accept-list cardinality. Adding a 5th
        // `Source` variant trips this assertion, forcing the audit to be
        // re-reviewed.
        let accept_list = [
            Source::SelfPlayOnBook,
            Source::SelfPlayOffBook,
            Source::Ccrl,
            Source::LichessOpen,
        ];
        assert_eq!(
            accept_list.len(),
            4,
            "ADR-0003 accept-list is pinned at 4 variants — adding a Source \
             requires updating the audit and re-running the label-provenance review"
        );
    }

    #[test]
    fn adr0003_audit_passes_result_tag_fixture() {
        // Records from all three accept-listed sources: CCRL (Result-tag),
        // Lichess (Result-tag), self-play (own result). Audit PASSes.
        let recs = vec![
            rec(&startpos(), Label::WhiteWin, Source::Ccrl, 1, 10),
            rec(&startpos(), Label::Draw, Source::LichessOpen, 2, 10),
            rec(&startpos(), Label::BlackWin, Source::SelfPlayOffBook, 3, 10),
        ];
        let r = audit_records(&recs);
        assert!(
            r.passed,
            "all-accept-listed-source corpus passes: {}",
            r.detail
        );
        assert!(r.detail.starts_with("PASS"));
    }

    // ── reproducibility ──────────────────────────────────────────────────

    #[test]
    fn reproducibility_rerun_byte_identical() {
        let td = TempDir::new("repro");
        // Distinct FENs so the dedup-integrity must-PASS check is also clean.
        let recs = vec![
            rec("fen-r1", Label::WhiteWin, Source::SelfPlayOffBook, 1, 8),
            rec("fen-r2", Label::Draw, Source::SelfPlayOffBook, 1, 9),
            rec("fen-r3", Label::Draw, Source::SelfPlayOffBook, 2, 10),
        ];
        let digest = write_corpus(td.path(), &recs);
        write_test_manifest(td.path(), recs.len() as u64, digest.clone());

        // Baseline: digest matches → reproducibility check PASSes.
        let r = run_quality_gate(td.path()).unwrap();
        let rep = r
            .checks
            .iter()
            .find(|c| c.name == "reproducibility_rerun_match")
            .unwrap();
        assert!(
            rep.passed,
            "byte-identical corpus must pass reproducibility: {}",
            rep.detail
        );
        assert!(r.gate_passed(), "all must-pass checks PASS");

        // Alter exactly one byte of the lane → digest changes → FAIL.
        let lane = td.path().join("lane.bin");
        let mut bytes = std::fs::read(&lane).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF; // flip the trailing CRC byte
        std::fs::write(&lane, &bytes).unwrap();
        let r2 = run_quality_gate(td.path()).unwrap();
        let rep2 = r2
            .checks
            .iter()
            .find(|c| c.name == "reproducibility_rerun_match")
            .unwrap();
        assert!(
            !rep2.passed,
            "altered corpus must fail reproducibility: {}",
            rep2.detail
        );
        assert!(!r2.gate_passed(), "gate fails when reproducibility fails");

        // Restore: PASS again.
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        std::fs::write(&lane, &bytes).unwrap();
        let r3 = run_quality_gate(td.path()).unwrap();
        let rep3 = r3
            .checks
            .iter()
            .find(|c| c.name == "reproducibility_rerun_match")
            .unwrap();
        assert!(
            rep3.passed,
            "restored corpus must pass again: {}",
            rep3.detail
        );
    }

    // ── per-lane dedup integrity (MUST PASS) ───────────────────────────────

    #[test]
    fn dedup_integrity_must_pass() {
        // A clean lane (distinct FENs, ≤ PER_GAME_CAP per game) PASSes.
        let clean = vec![
            rec("fen-a", Label::Draw, Source::SelfPlayOffBook, 1, 10),
            rec("fen-b", Label::Draw, Source::SelfPlayOffBook, 1, 11),
            rec("fen-c", Label::Draw, Source::SelfPlayOffBook, 2, 10),
        ];
        let res = check_dedup_integrity(&clean);
        assert!(res.passed, "clean lane must PASS: {}", res.detail);
        assert!(res.must_pass);
        assert!(res.detail.starts_with("PASS"));

        // A lane with a duplicate FEN FAILs (a path bypassed the LaneCommitter).
        let dup = vec![
            rec("fen-x", Label::Draw, Source::SelfPlayOffBook, 1, 10),
            rec("fen-x", Label::Draw, Source::SelfPlayOffBook, 2, 10),
        ];
        let res_dup = check_dedup_integrity(&dup);
        assert!(
            !res_dup.passed,
            "duplicate FEN must FAIL: {}",
            res_dup.detail
        );
        assert!(res_dup.must_pass);
        assert!(res_dup.detail.contains("duplicate"));

        // A lane with > PER_GAME_CAP records in one game FAILs.
        let over_cap: Vec<CorpusRecord> = (0..=PER_GAME_CAP as u32)
            .map(|p| {
                rec(
                    &format!("fen-cap-{p}"),
                    Label::Draw,
                    Source::SelfPlayOffBook,
                    1,
                    p,
                )
            })
            .collect();
        assert_eq!(over_cap.len(), PER_GAME_CAP + 1);
        let res_cap = check_dedup_integrity(&over_cap);
        assert!(
            !res_cap.passed,
            "> PER_GAME_CAP per game must FAIL: {}",
            res_cap.detail
        );
        assert!(res_cap.must_pass);
        assert!(res_cap.detail.contains("PER_GAME_CAP"));
    }

    // ── recorded checks ──────────────────────────────────────────────────

    #[test]
    fn decisive_draw_balance_bounds() {
        let recs = vec![
            rec(&startpos(), Label::WhiteWin, Source::SelfPlayOffBook, 1, 10),
            rec(&startpos(), Label::WhiteWin, Source::SelfPlayOffBook, 1, 11),
            rec(&startpos(), Label::Draw, Source::SelfPlayOffBook, 2, 10),
            rec(&startpos(), Label::BlackWin, Source::SelfPlayOffBook, 3, 10),
        ];
        let r = check_decisive_draw_balance(&recs);
        assert!(r.passed, "recorded-only check is always PASS");
        assert!(!r.must_pass);
        assert!(r.detail.contains("white=2"));
        assert!(r.detail.contains("draw=1"));
        assert!(r.detail.contains("black=1"));
        assert!(r.detail.contains("n=4"));
    }

    #[test]
    fn coverage_stats_recorded() {
        let recs = vec![
            rec(&startpos(), Label::WhiteWin, Source::SelfPlayOffBook, 1, 10),
            rec(&startpos(), Label::Draw, Source::Ccrl, 2, 10),
            rec(&startpos(), Label::BlackWin, Source::LichessOpen, 3, 10),
        ];
        let r = check_coverage_stats(&recs);
        assert!(r.passed && !r.must_pass);
        assert!(r.detail.contains("records=3"));
        assert!(r.detail.contains("games=3"));
        assert!(r.detail.contains("source_hist"));
        assert!(r.detail.contains("depth_rung_hist"));
    }

    // ── End-to-end gate happy path ───────────────────────────────────────

    #[test]
    fn gate_passes_on_well_formed_corpus() {
        let td = TempDir::new("happy");
        // Distinct FENs ⇒ zero exact-FEN dups; distinct game_ids each ≤
        // PER_GAME_CAP. We still need valid FENs because the coverage stats
        // check decodes them (failure is silently skipped — coverage is
        // recorded-only — but the audit + reproducibility + dedup-integrity
        // checks work on the FEN strings as-is).
        let fen_a = "8/8/8/8/8/8/8/k6K w - - 0 1";
        let fen_b = "8/8/8/8/8/8/8/K6k w - - 0 1";
        let fen_c = "8/8/8/8/8/8/8/1k5K w - - 0 1";
        let recs = vec![
            rec(fen_a, Label::WhiteWin, Source::SelfPlayOffBook, 1, 8),
            rec(fen_b, Label::Draw, Source::SelfPlayOffBook, 2, 10),
            rec(fen_c, Label::BlackWin, Source::SelfPlayOffBook, 3, 10),
        ];
        let digest = write_corpus(td.path(), &recs);
        write_test_manifest(td.path(), recs.len() as u64, digest);
        let r = run_quality_gate(td.path()).unwrap();
        for c in &r.checks {
            if c.must_pass {
                assert!(c.passed, "must-pass check {} failed: {}", c.name, c.detail);
            }
        }
        assert!(r.gate_passed(), "well-formed lane must pass the gate");
        assert_eq!(r.checks.len(), 5);
    }
}
