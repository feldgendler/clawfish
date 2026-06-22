//! Per-run summary.txt aggregation.

use std::path::Path;

/// One line in `summary.txt`.
#[derive(Debug)]
pub(crate) struct SummaryLine {
    pub game_index: u32,
    pub white: String,
    pub black: String,
    /// "1-0", "0-1", or "1/2-1/2".
    pub result: String,
    pub plies: u32,
    /// Human-readable termination reason.
    pub termination: String,
    /// NEW (ELOH.D). Format `<base>+<inc>` matching `format_tc`. `None` in
    /// legacy ELOH.A/B/C fixtures; always `Some` when ELOH.D TC sampling is active.
    pub tc: Option<String>,
}

/// Per-TC W/L/D bucket; ordered by input spec. Built incrementally in
/// the controller's drain loop alongside the global wins/losses/draws counters.
pub(crate) struct TcBucket {
    pub tc: super::cli::TimeControl,
    pub wins: u32,
    pub losses: u32,
    pub draws: u32,
}

/// Format per-TC summary line for `summary-by-tc:` emission.
///
/// Output: `"summary-by-tc: 10+0.1: W=110 L=95 D=45 (250)  20+0.2: W=..."` —
/// two spaces between bucket entries. Emitted even for a single-bucket distribution
/// (degenerate single-TC mix) to preserve the invariant that `summary-by-tc:` is
/// present iff `--tc-sample` was active.
pub(crate) fn format_summary_by_tc(buckets: &[TcBucket]) -> String {
    let parts: Vec<String> = buckets
        .iter()
        .map(|b| {
            let tc_str = super::format_tc(b.tc);
            let total = b.wins + b.losses + b.draws;
            format!(
                "{tc_str}: W={} L={} D={} ({total})",
                b.wins, b.losses, b.draws
            )
        })
        .collect();
    format!("summary-by-tc: {}", parts.join("  "))
}

/// Format the SPRT verdict line for the run-end summary. Always emitted in
/// SPRT mode regardless of whether termination was via Wald-bound crossing
/// or `--max-games`.
///
/// Output (canonical):
///   `sprt: verdict=H1 llr=2.95 elo0=0.0 elo1=10.0 alpha=0.050 beta=0.050 \
///    pairs=187 ptnml=[3,28,71,52,33]`
///   `sprt: verdict=continue llr=1.23 elo0=0.0 elo1=10.0 alpha=0.050 \
///    beta=0.050 pairs=199 ptnml=[5,40,78,56,21]`
///
/// Elo bounds use 1-decimal precision, alpha/beta 3-decimal, LLR 2-decimal.
pub(crate) fn format_sprt_verdict(
    state: &super::sprt::SprtState,
    cfg: &super::sprt::SprtConfig,
    verdict: super::sprt::SprtVerdict,
) -> String {
    let verdict_token = match verdict {
        super::sprt::SprtVerdict::AcceptH0 => "H0",
        super::sprt::SprtVerdict::AcceptH1 => "H1",
        super::sprt::SprtVerdict::Continue => "continue",
    };
    let pairs: u32 = state.pair_counts.iter().sum();
    let n = state.pair_counts;
    format!(
        "sprt: verdict={verdict_token} llr={llr:.2} elo0={e0:.1} elo1={e1:.1} \
         alpha={a:.3} beta={b:.3} pairs={pairs} \
         ptnml=[{},{},{},{},{}]",
        n[0],
        n[1],
        n[2],
        n[3],
        n[4],
        llr = state.llr,
        e0 = cfg.elo0,
        e1 = cfg.elo1,
        a = cfg.alpha,
        b = cfg.beta,
    )
}

/// Format the post-hoc pentanomial 95% CI line. Emitted whenever ≥2 pairs
/// completed, regardless of mode (rating-estimate, fixed-games match, SPRT).
///
/// Output (canonical):
///   `ci: elo=+41.85 [+18.33, +65.85] pairs=200`
///   `ci: undefined (n=1)` when fewer than 2 pairs are present.
pub(crate) fn format_pentanomial_ci(state: &super::sprt::SprtState) -> String {
    let pairs: u32 = state.pair_counts.iter().sum();
    let (lo, est, hi) = super::sprt::pentanomial_ci(state);
    if est.is_nan() {
        return format!("ci: undefined (n={pairs})");
    }
    format!("ci: elo={est:+.2} [{lo:+.2}, {hi:+.2}] pairs={pairs}")
}

/// Append a summary line to `path` (tab-separated, one line per game).
pub(crate) fn append_summary_line(path: &Path, line: &SummaryLine) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(
        f,
        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
        line.game_index,
        line.white,
        line.black,
        line.result,
        line.plies,
        line.termination,
        line.tc.as_deref().unwrap_or("-"),
    )?;
    f.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // §6.4 ELOH.D summary tests
    // -----------------------------------------------------------------------

    #[test]
    fn summary_line_with_tc_appends_tab_separated() {
        // Append a SummaryLine { tc: Some("10+0.1"), .. }; resulting line ends with \t10+0.1\n.
        let dir = std::env::temp_dir().join("eloh_d_summary_tc_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("summary_tc.txt");
        let _ = std::fs::remove_file(&path);
        let line = SummaryLine {
            game_index: 1,
            white: "engine".into(),
            black: "opponent".into(),
            result: "1-0".into(),
            plies: 42,
            termination: "normal".into(),
            tc: Some("10+0.1".into()),
        };
        append_summary_line(&path, &line).expect("append_summary_line ok");
        let content = std::fs::read_to_string(&path).expect("read ok");
        assert!(
            content.ends_with("\t10+0.1\n"),
            "line must end with '\\t10+0.1\\n'; got: {content:?}"
        );
    }

    #[test]
    fn summary_line_without_tc_appends_dash() {
        // tc: None → trailing \t-\n (sentinel for ELOH.A/B fixtures).
        let dir = std::env::temp_dir().join("eloh_d_summary_notc_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("summary_notc.txt");
        let _ = std::fs::remove_file(&path);
        let line = SummaryLine {
            game_index: 1,
            white: "engine".into(),
            black: "opponent".into(),
            result: "1/2-1/2".into(),
            plies: 10,
            termination: "adjudication: max moves".into(),
            tc: None,
        };
        append_summary_line(&path, &line).expect("append_summary_line ok");
        let content = std::fs::read_to_string(&path).expect("read ok");
        assert!(
            content.ends_with("\t-\n"),
            "tc=None must emit '\\t-\\n'; got: {content:?}"
        );
    }

    #[test]
    fn format_summary_by_tc_two_buckets() {
        // Buckets [(10+0.1, W=110, L=95, D=45), (20+0.2, W=105, L=90, D=55)] →
        // exact string "summary-by-tc: 10+0.1: W=110 L=95 D=45 (250)  20+0.2: W=105 L=90 D=55 (250)".
        let buckets = vec![
            TcBucket {
                tc: super::super::cli::TimeControl {
                    initial_ms: 10_000,
                    increment_ms: 100,
                },
                wins: 110,
                losses: 95,
                draws: 45,
            },
            TcBucket {
                tc: super::super::cli::TimeControl {
                    initial_ms: 20_000,
                    increment_ms: 200,
                },
                wins: 105,
                losses: 90,
                draws: 55,
            },
        ];
        let s = format_summary_by_tc(&buckets);
        assert_eq!(
            s, "summary-by-tc: 10+0.1: W=110 L=95 D=45 (250)  20+0.2: W=105 L=90 D=55 (250)",
            "two-bucket format mismatch; got: {s:?}"
        );
    }

    #[test]
    fn format_summary_by_tc_single_bucket_emitted() {
        // One-bucket input → still emits the full "summary-by-tc: 10+0.1: W=N L=N D=N (N)" line.
        // Degenerate single-TC mix must still emit the summary-by-tc line — preserves the
        // invariant that summary-by-tc is present iff --tc-sample was active.
        let buckets = vec![TcBucket {
            tc: super::super::cli::TimeControl {
                initial_ms: 10_000,
                increment_ms: 100,
            },
            wins: 7,
            losses: 3,
            draws: 2,
        }];
        let s = format_summary_by_tc(&buckets);
        assert_eq!(
            s, "summary-by-tc: 10+0.1: W=7 L=3 D=2 (12)",
            "single-bucket format mismatch; got: {s:?}"
        );
    }

    #[test]
    fn format_summary_by_tc_zero_games_in_bucket() {
        // A bucket with W=L=D=0 emits "... 30+0.3: W=0 L=0 D=0 (0)".
        let buckets = vec![TcBucket {
            tc: super::super::cli::TimeControl {
                initial_ms: 30_000,
                increment_ms: 300,
            },
            wins: 0,
            losses: 0,
            draws: 0,
        }];
        let s = format_summary_by_tc(&buckets);
        assert!(
            s.contains("W=0 L=0 D=0 (0)"),
            "zero-game bucket must emit 'W=0 L=0 D=0 (0)'; got: {s:?}"
        );
    }

    // -----------------------------------------------------------------------
    // §6.4 ELOH.E SPRT-summary tests
    // -----------------------------------------------------------------------

    #[test]
    fn format_sprt_verdict_h1_canonical_string() {
        let cfg = super::super::sprt::SprtConfig {
            elo0: 0.0,
            elo1: 10.0,
            alpha: 0.05,
            beta: 0.05,
        };
        let s = super::super::sprt::SprtState {
            pair_counts: [3, 28, 71, 52, 33],
            llr: 2.95,
            ..Default::default()
        };
        let out = format_sprt_verdict(&s, &cfg, super::super::sprt::SprtVerdict::AcceptH1);
        assert_eq!(
            out,
            "sprt: verdict=H1 llr=2.95 elo0=0.0 elo1=10.0 alpha=0.050 beta=0.050 \
             pairs=187 ptnml=[3,28,71,52,33]"
        );
    }

    #[test]
    fn format_sprt_verdict_h0_canonical_string() {
        let cfg = super::super::sprt::SprtConfig {
            elo0: 0.0,
            elo1: 10.0,
            alpha: 0.05,
            beta: 0.05,
        };
        let s = super::super::sprt::SprtState {
            pair_counts: [33, 52, 71, 28, 3],
            llr: -2.95,
            ..Default::default()
        };
        let out = format_sprt_verdict(&s, &cfg, super::super::sprt::SprtVerdict::AcceptH0);
        assert_eq!(
            out,
            "sprt: verdict=H0 llr=-2.95 elo0=0.0 elo1=10.0 alpha=0.050 beta=0.050 \
             pairs=187 ptnml=[33,52,71,28,3]"
        );
    }

    #[test]
    fn format_sprt_verdict_continue_at_max_games() {
        let cfg = super::super::sprt::SprtConfig {
            elo0: 0.0,
            elo1: 10.0,
            alpha: 0.05,
            beta: 0.05,
        };
        let s = super::super::sprt::SprtState {
            pair_counts: [5, 40, 78, 56, 21],
            llr: 1.23,
            ..Default::default()
        };
        let out = format_sprt_verdict(&s, &cfg, super::super::sprt::SprtVerdict::Continue);
        assert!(out.starts_with("sprt: verdict=continue llr=1.23 "));
        assert!(out.contains("pairs=200 ptnml=[5,40,78,56,21]"));
    }

    #[test]
    fn format_pentanomial_ci_two_decimal_elo() {
        let s = super::super::sprt::SprtState {
            pair_counts: [5, 40, 78, 56, 21],
            ..Default::default()
        };
        let out = format_pentanomial_ci(&s);
        assert!(out.starts_with("ci: elo=+41."));
        assert!(out.contains("pairs=200"));
        // Bounds present, comma-separated, with explicit signs.
        assert!(
            out.contains(", +"),
            "upper bound must carry explicit + sign; got {out}"
        );
    }

    #[test]
    fn format_pentanomial_ci_undefined_at_n_lt_2() {
        let s = super::super::sprt::SprtState {
            pair_counts: [0, 0, 1, 0, 0],
            ..Default::default()
        };
        assert_eq!(format_pentanomial_ci(&s), "ci: undefined (n=1)");
    }
}
