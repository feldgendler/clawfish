//! `--tc-sample <SPEC>` parsing + cumulative-bucket sampling.
//!
//! Grammar: `<TC>:<weight>(,<TC>:<weight>)*`
//! Each `<TC>` parsed via `cli::parse_tc`; `<weight>` is a u32 in `1..=u32::MAX`.
//! At least one entry required. Empty input, zero weight, weight overflow on
//! summing, or repeated TC keys all yield Err.

/// Parsed `--tc-sample` distribution.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct TcDistribution {
    /// Parsed (TC, weight) entries in input order. Weights are positive.
    pub entries: Vec<(super::cli::TimeControl, u32)>,
    /// Prefix sums of weights; len == entries.len(); strictly increasing;
    /// last element == total.
    cumulative: Vec<u32>,
    /// Sum of all weights.
    total: u32,
}

impl TcDistribution {
    /// Sample one TC. Draw `r = rng.next_u64() % total`, find first cumulative
    /// bucket strictly greater than `r`, return its TC. Linear scan — entries.len()
    /// expected ≤ ~10 in practice.
    ///
    /// Modulo bias: total ≤ u32::MAX, so bias per bucket ≤ u32::MAX / 2^64 < 2^-32.
    pub(crate) fn sample(&self, rng: &mut super::prng::Prng) -> super::cli::TimeControl {
        let r = (rng.next_u64() % self.total as u64) as u32;
        let idx = self
            .cumulative
            .iter()
            .position(|&c| c > r)
            .expect("cumulative invariant: r < total so some bucket strictly exceeds r");
        self.entries[idx].0
    }

    /// Iterate (TC, weight) pairs in input-spec order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &(super::cli::TimeControl, u32)> {
        self.entries.iter()
    }
}

/// Parse `<TC>:<weight>(,<TC>:<weight>)*`.
///
/// Rejects empty input, zero weight, weight-sum overflow, and duplicate TC keys.
/// Duplicate TC keys likely indicate user confusion (e.g. `10+0.1:1,10+0.1:2`)
/// and fail loudly rather than silently merging.
pub(crate) fn parse_tc_sample(s: &str) -> Result<TcDistribution, super::cli::CliError> {
    if s.is_empty() {
        return Err(super::cli::CliError::InvalidValue(
            "--tc-sample: empty spec".into(),
        ));
    }

    let mut entries: Vec<(super::cli::TimeControl, u32)> = Vec::new();
    let mut cumulative: Vec<u32> = Vec::new();
    let mut total: u32 = 0;

    for entry in s.split(',') {
        let (tc_str, weight_str) = entry.split_once(':').ok_or_else(|| {
            super::cli::CliError::InvalidValue(format!(
                "--tc-sample: each entry must be <TC>:<weight>, got: {entry}"
            ))
        })?;

        let tc = super::cli::parse_tc(tc_str)
            .map_err(|e| super::cli::CliError::InvalidValue(format!("--tc-sample: {e}")))?;

        let weight: u32 = weight_str.parse().map_err(|_| {
            super::cli::CliError::InvalidValue(format!(
                "--tc-sample: weight must be a positive integer, got: {weight_str}"
            ))
        })?;

        if weight == 0 {
            return Err(super::cli::CliError::InvalidValue(
                "--tc-sample: weight must be >= 1 (zero weight rejected)".into(),
            ));
        }

        // Reject duplicate TC keys — likely a user typo.
        if entries.iter().any(|(existing, _)| *existing == tc) {
            return Err(super::cli::CliError::InvalidValue(format!(
                "--tc-sample: duplicate TC key {tc_str}"
            )));
        }

        total = total.checked_add(weight).ok_or_else(|| {
            super::cli::CliError::InvalidValue(
                "--tc-sample: total weight overflow (exceeds u32::MAX)".into(),
            )
        })?;

        entries.push((tc, weight));
        cumulative.push(total);
    }

    Ok(TcDistribution {
        entries,
        cumulative,
        total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elo_iterate::cli::TimeControl;

    fn tc(base_s: f64, inc_s: f64) -> TimeControl {
        TimeControl {
            initial_ms: (base_s * 1000.0).round() as u32,
            increment_ms: (inc_s * 1000.0).round() as u32,
        }
    }

    // -----------------------------------------------------------------------
    // §6.2: parse_tc_sample tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_single_entry() {
        // "10+0.1:1" → entries [(10s+0.1s, 1)], total 1.
        let dist = parse_tc_sample("10+0.1:1").expect("should parse");
        assert_eq!(dist.entries.len(), 1);
        assert_eq!(dist.entries[0], (tc(10.0, 0.1), 1));
        assert_eq!(dist.total, 1);
        assert_eq!(dist.cumulative, vec![1]);
    }

    #[test]
    fn parse_four_entries_uniform() {
        // "10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1" → four entries, total 4,
        // cumulative [1,2,3,4].
        let dist = parse_tc_sample("10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1").expect("should parse");
        assert_eq!(dist.entries.len(), 4);
        assert_eq!(dist.total, 4);
        assert_eq!(dist.cumulative, vec![1, 2, 3, 4]);
    }

    #[test]
    fn parse_three_to_one_skewed() {
        // "10+0.1:3,60+0.6:1" → entries [(10s+0.1s, 3), (60s+0.6s, 1)],
        // cumulative [3, 4], total 4.
        let dist = parse_tc_sample("10+0.1:3,60+0.6:1").expect("should parse");
        assert_eq!(dist.entries.len(), 2);
        assert_eq!(dist.entries[0], (tc(10.0, 0.1), 3));
        assert_eq!(dist.entries[1], (tc(60.0, 0.6), 1));
        assert_eq!(dist.cumulative, vec![3, 4]);
        assert_eq!(dist.total, 4);
    }

    #[test]
    fn parse_rejects_empty() {
        // TDD-NOTE: passes trivially against the skeleton's blanket Err
        // ("not yet implemented"); real impl must fail on this specific
        // malformed input with a meaningful error, not just any Err.
        assert!(
            parse_tc_sample("").is_err(),
            "empty string must be rejected"
        );
    }

    #[test]
    fn parse_rejects_zero_weight() {
        // TDD-NOTE: passes trivially against the skeleton's blanket Err
        // ("not yet implemented"); real impl must fail on this specific
        // malformed input with a meaningful error, not just any Err.
        assert!(
            parse_tc_sample("10+0.1:0").is_err(),
            "zero weight must be rejected"
        );
    }

    #[test]
    fn parse_rejects_repeated_tc() {
        let err = parse_tc_sample("10+0.1:1,10+0.1:2").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("duplicate") || msg.contains("repeated") || msg.contains("Duplicate"),
            "error message for repeated TC must mention duplication; got: {msg}"
        );
    }

    #[test]
    fn parse_rejects_malformed_weight() {
        // TDD-NOTE: passes trivially against the skeleton's blanket Err
        // ("not yet implemented"); real impl must fail on this specific
        // malformed input with a meaningful error, not just any Err.
        assert!(
            parse_tc_sample("10+0.1:abc").is_err(),
            "non-numeric weight must be rejected"
        );
    }

    #[test]
    fn parse_rejects_missing_colon() {
        // "10+0.1" with no colon → no weight → Err.
        //
        // TDD-NOTE: passes trivially against the skeleton's blanket Err
        // ("not yet implemented"); real impl must fail on this specific
        // malformed input with a meaningful error, not just any Err.
        assert!(
            parse_tc_sample("10+0.1").is_err(),
            "missing colon (no weight) must be rejected"
        );
    }

    #[test]
    fn parse_rejects_weight_overflow() {
        // Two entries each with u32::MAX/2 + 1 would overflow the total.
        //
        // TDD-NOTE: passes trivially against the skeleton's blanket Err
        // ("not yet implemented"); real impl must surface a distinct
        // overflow error path so this test stays meaningful — i.e. the
        // implementation slice must NOT accept this input even after
        // wiring the parser, and ideally surfaces a distinct CliError
        // variant or message substring (e.g. "weight overflow").
        let half_plus = u32::MAX / 2 + 1;
        let spec = format!("10+0.1:{half_plus},20+0.2:{half_plus}");
        assert!(
            parse_tc_sample(&spec).is_err(),
            "total weight overflow must be rejected"
        );
    }

    #[test]
    fn sample_single_entry_always_returns_it() {
        // 1-entry distribution + 1000 draws → all draws return the single entry.
        let dist = parse_tc_sample("10+0.1:1").expect("should parse");
        let mut rng = super::super::prng::Prng::new(42);
        for _ in 0..1000 {
            let sampled = dist.sample(&mut rng);
            assert_eq!(
                sampled,
                tc(10.0, 0.1),
                "single-entry dist must always return that entry"
            );
        }
    }

    #[test]
    fn sample_skewed_3to1_at_seed_xfeed_yields_known_counts() {
        // Back-validation gate Part 1.
        // Distribution [(A=10+0.1, 3), (B=60+0.6, 1)]; seed 0xC1AB_FEED; 1000 draws.
        // Exact counts produced by SplitMix64 with Vigna 2014 / Steele-Lea-Flood 2014 constants.
        // chi2=0.533 (1 dof; 99% critical value 6.635) — well within expected range.
        // If mixer constants or seed ever change, repin by observing the eprintln! output.
        let dist = parse_tc_sample("10+0.1:3,60+0.6:1").expect("should parse");
        let mut rng = super::super::prng::Prng::new(0xC1AB_FEED);
        let mut count_a = 0u32;
        let mut count_b = 0u32;
        for _ in 0..1000 {
            let s = dist.sample(&mut rng);
            if s == tc(10.0, 0.1) {
                count_a += 1;
            } else if s == tc(60.0, 0.6) {
                count_b += 1;
            } else {
                panic!("unexpected TC sampled: {s:?}");
            }
        }
        // Chi-squared as side observable (1 dof; critical value 6.635 at 99%).
        let expected_a = 750.0f64;
        let expected_b = 250.0f64;
        let chi2 = (count_a as f64 - expected_a).powi(2) / expected_a
            + (count_b as f64 - expected_b).powi(2) / expected_b;
        eprintln!("sample_skewed_3to1: count_a={count_a} count_b={count_b} chi2={chi2:.3}");
        assert!(
            chi2 < 6.635,
            "chi2={chi2:.3} exceeds 99% critical value 6.635 for 1 dof; distribution is biased"
        );
        assert_eq!(
            (count_a, count_b),
            (740, 260),
            "exact seed-driven counts for Prng::new(0xC1AB_FEED) + 3:1 distribution"
        );
    }

    #[test]
    fn sample_uniform_4_bucket_at_seed_xfeed_yields_known_counts() {
        // Back-validation gate Part 1 (4-bucket uniform shape).
        // Distribution 10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1; seed 0xC1AB_FEED; 1000 draws.
        // Exact counts produced by SplitMix64 with Vigna 2014 / Steele-Lea-Flood 2014 constants.
        // chi2=0.888 (3 dof; 99% critical value 11.345) — well within expected range.
        // If mixer constants or seed ever change, repin by observing the eprintln! output.
        let dist = parse_tc_sample("10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1").expect("should parse");
        let mut rng = super::super::prng::Prng::new(0xC1AB_FEED);
        let tcs = [tc(10.0, 0.1), tc(20.0, 0.2), tc(40.0, 0.4), tc(60.0, 0.6)];
        let mut counts = [0u32; 4];
        for _ in 0..1000 {
            let s = dist.sample(&mut rng);
            let idx = tcs
                .iter()
                .position(|&t| t == s)
                .unwrap_or_else(|| panic!("unexpected TC sampled: {s:?}"));
            counts[idx] += 1;
        }
        // Chi-squared as side observable (3 dof; critical value 11.345 at 99%).
        let expected = 250.0f64;
        let chi2: f64 = counts
            .iter()
            .map(|&c| (c as f64 - expected).powi(2) / expected)
            .sum();
        eprintln!("sample_uniform_4: counts={counts:?} chi2={chi2:.3}");
        assert!(
            chi2 < 11.345,
            "chi2={chi2:.3} exceeds 99% critical value 11.345 for 3 dof; distribution is biased"
        );
        assert_eq!(
            counts,
            [250u32, 251, 239, 260],
            "exact seed-driven counts for Prng::new(0xC1AB_FEED) + 4-bucket uniform distribution"
        );
    }

    #[test]
    fn sample_uniform_4_bucket_input_order_preserved_in_iter() {
        // After parsing A:1,B:1,C:1,D:1, dist.iter() yields (A,1),(B,1),(C,1),(D,1).
        let dist = parse_tc_sample("10+0.1:1,20+0.2:1,40+0.4:1,60+0.6:1").expect("should parse");
        let collected: Vec<_> = dist.iter().cloned().collect();
        assert_eq!(
            collected,
            vec![
                (tc(10.0, 0.1), 1u32),
                (tc(20.0, 0.2), 1),
                (tc(40.0, 0.4), 1),
                (tc(60.0, 0.6), 1),
            ],
            "iter() must yield entries in input-spec order"
        );
    }
}
