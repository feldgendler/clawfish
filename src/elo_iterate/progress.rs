//! Progress-line and convergence-line formatters for the iteration harness.

use super::StopReason;

/// Snapshot of per-game-batch progress for human-readable output.
pub(crate) struct ProgressLine {
    /// Game (pair) serial index `t`.
    pub t: u32,
    /// Total games completed so far.
    pub games: u32,
    /// Current Elo estimate.
    pub elo: f64,
    /// Current trailing-window σ.
    pub sigma: f64,
    /// Current K factor.
    pub k: f64,
    /// Win / Loss / Draw counts from clawfish's perspective.
    pub wld: (u32, u32, u32),
}

/// Format a mid-run progress line.
///
/// Output: `progress: t=<t> games=<G> elo=<%.2f> sigma=<%.2f> K=<%.3f> wld=<W>-<L>-<D>`
pub(crate) fn format_progress(line: &ProgressLine) -> String {
    let (w, l, d) = line.wld;
    format!(
        "progress: t={t} games={games} elo={elo:.2} sigma={sigma:.2} K={k:.3} wld={w}-{l}-{d}",
        t = line.t,
        games = line.games,
        elo = line.elo,
        sigma = line.sigma,
        k = line.k,
    )
}

/// Format the final convergence line.
///
/// Output: `converged: elo=<%.2f> sigma=<%.2f> games=<G> reason=<sigma|max-games>`
pub(crate) fn format_converged(
    final_elo: f64,
    final_sigma: f64,
    games: u32,
    reason: StopReason,
) -> String {
    let reason_str = match reason {
        StopReason::Sigma => "sigma",
        StopReason::MaxGames => "max-games",
        StopReason::SprtAcceptH0 => "sprt-h0",
        StopReason::SprtAcceptH1 => "sprt-h1",
    };
    format!(
        "converged: elo={final_elo:.2} sigma={final_sigma:.2} games={games} reason={reason_str}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_progress_canonical_string() {
        let line = ProgressLine {
            t: 60,
            games: 60,
            elo: 2103.45,
            sigma: 28.7,
            k: 13.333,
            wld: (45, 8, 7),
        };
        assert_eq!(
            format_progress(&line),
            "progress: t=60 games=60 elo=2103.45 sigma=28.70 K=13.333 wld=45-8-7"
        );
    }

    #[test]
    fn format_converged_sigma_reason() {
        let s = format_converged(2103.45, 28.7, 60, StopReason::Sigma);
        assert_eq!(
            s,
            "converged: elo=2103.45 sigma=28.70 games=60 reason=sigma"
        );
    }

    #[test]
    fn format_converged_max_games_reason() {
        let s = format_converged(2103.45, 28.7, 60, StopReason::MaxGames);
        assert_eq!(
            s,
            "converged: elo=2103.45 sigma=28.70 games=60 reason=max-games"
        );
    }

    #[test]
    fn format_progress_zero_sigma() {
        let line = ProgressLine {
            t: 1,
            games: 2,
            elo: 2000.0,
            sigma: 0.0,
            k: 40.0,
            wld: (1, 0, 1),
        };
        let s = format_progress(&line);
        assert!(s.contains("sigma=0.00"));
    }

    #[test]
    fn format_progress_two_decimal_elo_rounds() {
        let line = ProgressLine {
            t: 1,
            games: 2,
            elo: 1999.999,
            sigma: 5.0,
            k: 40.0,
            wld: (2, 0, 0),
        };
        let s = format_progress(&line);
        assert!(s.contains("elo=2000.00"));
    }
}
