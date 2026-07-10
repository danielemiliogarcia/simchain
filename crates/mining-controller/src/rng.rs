//! Deterministic RNG for the stochastic mining modes (poisson intervals and
//! weighted miner selection).

use std::time::{SystemTime, UNIX_EPOCH};

// SplitMix64 is small, seedable, and has a stable stream across builds. Using it
// directly keeps MINING_RNG_SEED reproducible without adding an RNG dependency
// whose standard generator may change between crate versions.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    // Uniform in [0, 1), using the top 53 bits (the f64 mantissa width).
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1_u64 << 53) as f64
    }

    pub fn next_exp(&mut self, mean: f64) -> f64 {
        -mean * (1.0 - self.next_f64()).ln()
    }

    // Rejection sampling avoids the small bias introduced by `random % upper`.
    pub fn next_below(&mut self, upper: u64) -> u64 {
        debug_assert!(upper > 0);
        let threshold = upper.wrapping_neg() % upper;
        loop {
            let value = self.next_u64();
            if value >= threshold {
                return value % upper;
            }
        }
    }
}

pub fn entropy_seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after the Unix epoch")
        .as_nanos() as u64;
    nanos ^ (u64::from(std::process::id()).rotate_left(32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix64_stream_is_stable() {
        let mut rng = Rng::new(0);
        assert_eq!(rng.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(rng.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(rng.next_u64(), 0x06C4_5D18_8009_454F);
    }

    #[test]
    fn exponential_samples_have_expected_shape() {
        let mean = 5.0;
        let count = 100_000;
        let mut rng = Rng::new(42);
        let samples: Vec<f64> = (0..count).map(|_| rng.next_exp(mean)).collect();
        let actual_mean = samples.iter().sum::<f64>() / count as f64;
        let variance = samples
            .iter()
            .map(|sample| (sample - actual_mean).powi(2))
            .sum::<f64>()
            / count as f64;
        let coefficient_of_variation = variance.sqrt() / actual_mean;

        assert!((actual_mean - mean).abs() < 0.05);
        assert!((coefficient_of_variation - 1.0).abs() < 0.03);
        assert!(samples.iter().any(|sample| *sample < 1.0));
        assert!(samples.iter().any(|sample| *sample > 15.0));
    }

    #[test]
    fn weighted_draw_is_reproducible_and_proportional() {
        let mut first = Rng::new(42);
        let mut replay = Rng::new(42);
        let first_draws: Vec<u64> = (0..1_000).map(|_| first.next_below(100)).collect();
        let replay_draws: Vec<u64> = (0..1_000).map(|_| replay.next_below(100)).collect();
        assert_eq!(first_draws, replay_draws);

        let node2_wins = first_draws.iter().filter(|draw| **draw < 70).count();
        assert!((650..=750).contains(&node2_wins));
    }
}
