//! SplitMix64 PRNG for `--seed`-driven TC-sampling reproducibility.
//!
//! ELOH.D uses a single u64 seed → single SplitMix64 stream consumed by
//! `tc_sample::TcDistribution::sample`. Hand-rolled (~20 LOC); no `rand`
//! crate dep. Mixer constants are pinned by a golden-fixture test
//! (`prng_seed_zero_first_three_words_golden`) so a transcription typo
//! fails at compile-time-of-test.

#[derive(Debug, Clone, Copy)]
pub(crate) struct Prng(u64);

// Vigna 2014 / Steele-Lea-Flood 2014 SplitMix64 constants.
const GOLDEN_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;
const MIX_C1: u64 = 0xBF58_476D_1CE4_E5B9;
const MIX_C2: u64 = 0x94D0_49BB_1331_11EB;

impl Prng {
    /// Construct from a u64 seed. Runs one SplitMix64 mix step so a seed
    /// of 0 doesn't yield a 0-state pathology.
    pub(crate) fn new(seed: u64) -> Self {
        let mut p = Self(seed);
        let _ = p.next_u64();
        p
    }

    /// SplitMix64 next. Standard algorithm (Vigna 2014 / Steele-Lea-Flood 2014):
    /// state += GOLDEN_GAMMA; z = state; z = (z ^ (z >> 30)) * MIX_C1;
    /// z = (z ^ (z >> 27)) * MIX_C2; z ^ (z >> 31).
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(GOLDEN_GAMMA);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(MIX_C1);
        z = (z ^ (z >> 27)).wrapping_mul(MIX_C2);
        z ^ (z >> 31)
    }
}

/// Default seed when `--seed` is absent. Intentionally non-zero. Documented
/// in `--help` so users know no-`--seed` runs are still bit-deterministic.
pub(crate) const DEFAULT_SEED: u64 = 0xC1AB_F15A_E10D_D000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prng_zero_seed_yields_nonzero_first_word() {
        // The constructor's mix step ensures a 0 seed isn't a 0-state.
        let mut rng = Prng::new(0);
        assert_ne!(
            rng.next_u64(),
            0,
            "Prng::new(0) first output must be non-zero"
        );
    }

    #[test]
    fn prng_deterministic_across_constructions() {
        // Two Prng::new(42) instances must produce identical streams.
        let mut a = Prng::new(42);
        let mut b = Prng::new(42);
        for _ in 0..100 {
            assert_eq!(
                a.next_u64(),
                b.next_u64(),
                "two Prng::new(42) instances must produce identical u64 streams"
            );
        }
    }

    #[test]
    fn prng_distinct_seeds_yield_distinct_streams() {
        // Prng::new(42) and Prng::new(43) must produce different first 100 u64s.
        let stream_a: Vec<u64> = {
            let mut rng = Prng::new(42);
            (0..100).map(|_| rng.next_u64()).collect()
        };
        let stream_b: Vec<u64> = {
            let mut rng = Prng::new(43);
            (0..100).map(|_| rng.next_u64()).collect()
        };
        assert_ne!(
            stream_a, stream_b,
            "distinct seeds must yield distinct u64 streams"
        );
    }

    #[test]
    fn prng_seed_zero_first_three_words_golden() {
        // Golden fixture: pins the first three outputs from Prng::new(0) against
        // values produced by the Vigna 2014 / Steele-Lea-Flood 2014 SplitMix64
        // with GOLDEN_GAMMA=0x9E3779B97F4A7C15, MIX_C1=0xBF58476D1CE4E5B9,
        // MIX_C2=0x94D049BB133111EB. Seed=0 → after one constructor mix step, the
        // state is GOLDEN_GAMMA, then three further calls advance it to these values.
        //
        // Catches any mixer-constant transcription typo at compile-time-of-test.
        let mut rng = Prng::new(0);
        let w0 = rng.next_u64();
        let w1 = rng.next_u64();
        let w2 = rng.next_u64();
        assert_eq!(
            (w0, w1, w2),
            (
                7_960_286_522_194_355_700_u64,
                487_617_019_471_545_679_u64,
                17_909_611_376_780_542_444_u64,
            ),
            "Prng::new(0) first three words must match SplitMix64 golden fixture"
        );
    }
}
