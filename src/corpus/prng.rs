//! SplitMix64 PRNG for M6.G reproducibility (corpus shuffle, per-game-cap
//! reservoir, self-play opening + depth-rung sampling, train/val split).
//!
//! Deliberate ~20-LOC duplicate of `elo_iterate::prng` (Vigna 2014 /
//! Steele-Lea-Flood 2014 SplitMix64). Kept separate so M6.G does not touch
//! the SPRT-critical harness module. Both copies are golden-pinned to the
//! IDENTICAL first-three-words fixture (see `prng_seed_zero_first_three_words_golden`)
//! so a transcription divergence between the two copies — the entire risk
//! this dup-vs-hoist tradeoff turns on — fails the test.

/// SplitMix64 state.
#[derive(Debug, Clone, Copy)]
pub struct Prng(u64);

const GOLDEN_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;
const MIX_C1: u64 = 0xBF58_476D_1CE4_E5B9;
const MIX_C2: u64 = 0x94D0_49BB_1331_11EB;

impl Prng {
    /// Construct from a u64 seed. One mix step so seed 0 isn't a 0-state.
    pub fn new(seed: u64) -> Self {
        let mut p = Self(seed);
        let _ = p.next_u64();
        p
    }

    /// Reconstruct from a checkpointed raw state (R3 deterministic resume).
    /// Pairs with [`Prng::state`]; does NOT run the constructor mix step.
    pub fn from_state(state: u64) -> Self {
        Self(state)
    }

    /// Raw state for checkpointing.
    pub fn state(&self) -> u64 {
        self.0
    }

    /// SplitMix64 next. `state += GAMMA; z = state; z = (z^(z>>30))*C1;
    /// z = (z^(z>>27))*C2; z ^ (z>>31)`.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(GOLDEN_GAMMA);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(MIX_C1);
        z = (z ^ (z >> 27)).wrapping_mul(MIX_C2);
        z ^ (z >> 31)
    }

    /// Uniform-ish `0..n` (n > 0). Modulo bias ≤ 2^-? — acceptable for
    /// corpus shuffling / reservoir sampling (not cryptographic).
    pub fn below(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0, "Prng::below requires n > 0");
        self.next_u64() % n
    }
}

/// Derive an independent sub-stream seed from a base seed + index. Used for
/// per-worker / per-game decorrelated streams so the union is
/// scheduler-independent deterministic (R7 striping).
pub fn substream_seed(base: u64, index: u64) -> u64 {
    // One SplitMix64 mix of (base XOR a gamma-spread index) — decorrelated,
    // deterministic, order-independent.
    let mut z = base ^ index.wrapping_mul(GOLDEN_GAMMA);
    z = (z ^ (z >> 30)).wrapping_mul(MIX_C1);
    z = (z ^ (z >> 27)).wrapping_mul(MIX_C2);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prng_seed_zero_first_three_words_golden() {
        // First three `next_u64()` outputs from `Prng::new(0)` (which
        // performs ONE constructor mix step before these reads). These
        // are post-construction-step outputs, not the bare initial state.
        // IDENTICAL literals to elo_iterate::prng::tests — a transcription
        // divergence between the two SplitMix64 copies fails here.
        let mut rng = Prng::new(0);
        let (w0, w1, w2) = (rng.next_u64(), rng.next_u64(), rng.next_u64());
        assert_eq!(
            (w0, w1, w2),
            (
                7_960_286_522_194_355_700_u64,
                487_617_019_471_545_679_u64,
                17_909_611_376_780_542_444_u64,
            ),
            "SplitMix64 golden fixture must match elo_iterate::prng"
        );
    }

    #[test]
    fn prng_deterministic_across_constructions() {
        let mut a = Prng::new(0xDEAD_BEEF);
        let mut b = Prng::new(0xDEAD_BEEF);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn prng_from_state_resumes_bit_identical() {
        let mut a = Prng::new(42);
        for _ in 0..17 {
            a.next_u64();
        }
        let snap = a.state();
        let mut b = Prng::from_state(snap);
        for _ in 0..50 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn substream_seeds_decorrelated_and_deterministic() {
        let s0 = substream_seed(0xABCD, 0);
        let s1 = substream_seed(0xABCD, 1);
        assert_ne!(s0, s1);
        assert_eq!(s0, substream_seed(0xABCD, 0));
        // Different streams diverge immediately.
        let mut p0 = Prng::new(s0);
        let mut p1 = Prng::new(s1);
        assert_ne!(p0.next_u64(), p1.next_u64());
    }
}
