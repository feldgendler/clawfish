//! The data-quality gate — the M6.G LANDING GATE (not an SPRT; the M5.E
//! correctness-only-gate precedent applied to data). Six checks; three are
//! must-PASS (ADR-0003 label-provenance audit, reproducibility re-run
//! match, held-out-split integrity).
//!
//! Implemented in the M6.G integration slice per `docs/plans/m6.g.md` §6.

use std::path::Path;

use super::CorpusError;

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

/// Run all six checks on a frozen corpus dir. The ADR-0003 audit REJECTs
/// any engine-score-labeled source (Zurichess `c9`) and PASSes only
/// original-game-result sources.
pub fn run_quality_gate(_dir: &Path) -> Result<QualityReport, CorpusError> {
    todo!("M6.G integration slice")
}
