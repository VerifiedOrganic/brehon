//! Chaos testing configuration.
//!
//! Randomized delays, dropped messages, duplicate delivery simulation.

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::time::Duration;

/// Configuration for chaos testing.
#[derive(Debug, Clone)]
pub struct ChaosConfig {
    pub seed: u64,
    pub delay_range: Option<(Duration, Duration)>,
    pub drop_probability: f32,
    pub duplicate_probability: f32,
    pub lease_expiry_probability: f32,
    pub enabled: bool,
}

impl Default for ChaosConfig {
    fn default() -> Self {
        Self {
            seed: 42,
            delay_range: None,
            drop_probability: 0.0,
            duplicate_probability: 0.0,
            lease_expiry_probability: 0.0,
            enabled: false,
        }
    }
}

impl ChaosConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn seeded(seed: u64) -> Self {
        Self {
            seed,
            ..Self::default()
        }
    }

    pub fn with_delays(min: Duration, max: Duration) -> Self {
        Self {
            delay_range: Some((min, max)),
            enabled: true,
            ..Self::default()
        }
    }

    pub fn with_drops(probability: f32) -> Self {
        Self {
            drop_probability: probability.clamp(0.0, 1.0),
            enabled: probability > 0.0,
            ..Self::default()
        }
    }

    pub fn with_duplicates(probability: f32) -> Self {
        Self {
            duplicate_probability: probability.clamp(0.0, 1.0),
            enabled: probability > 0.0,
            ..Self::default()
        }
    }

    pub fn with_lease_expiry(probability: f32) -> Self {
        Self {
            lease_expiry_probability: probability.clamp(0.0, 1.0),
            enabled: probability > 0.0,
            ..Self::default()
        }
    }

    pub fn full_chaos(seed: u64) -> Self {
        Self {
            seed,
            delay_range: Some((Duration::from_millis(0), Duration::from_millis(5000))),
            drop_probability: 0.03,
            duplicate_probability: 0.03,
            lease_expiry_probability: 0.01,
            enabled: true,
        }
    }

    pub fn rng(&self) -> StdRng {
        StdRng::seed_from_u64(self.seed)
    }
}

/// Chaos injector for operations.
#[derive(Debug)]
pub struct ChaosInjector {
    config: ChaosConfig,
    rng: StdRng,
}

impl ChaosInjector {
    pub fn new(config: ChaosConfig) -> Self {
        let rng = config.rng();
        Self { config, rng }
    }

    pub fn should_drop(&mut self) -> bool {
        if !self.config.enabled || self.config.drop_probability == 0.0 {
            return false;
        }
        self.rng.gen::<f32>() < self.config.drop_probability
    }

    pub fn should_duplicate(&mut self) -> bool {
        if !self.config.enabled || self.config.duplicate_probability == 0.0 {
            return false;
        }
        self.rng.gen::<f32>() < self.config.duplicate_probability
    }

    pub fn should_expire_lease(&mut self) -> bool {
        if !self.config.enabled || self.config.lease_expiry_probability == 0.0 {
            return false;
        }
        self.rng.gen::<f32>() < self.config.lease_expiry_probability
    }

    pub async fn delay(&mut self) {
        if let Some((min, max)) = self.config.delay_range {
            if min == max {
                tokio::time::sleep(min).await;
            } else {
                let min_ms = min.as_millis() as u64;
                let max_ms = max.as_millis() as u64;
                let delay_ms = self.rng.gen_range(min_ms..=max_ms);
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
        }
    }

    pub fn random_delay(&mut self) -> Option<Duration> {
        if let Some((min, max)) = self.config.delay_range {
            let min_ms = min.as_millis() as u64;
            let max_ms = max.as_millis() as u64;
            let delay_ms = self.rng.gen_range(min_ms..=max_ms);
            Some(Duration::from_millis(delay_ms))
        } else {
            None
        }
    }
}

impl Clone for ChaosInjector {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            rng: self.config.rng(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chaos_config_defaults() {
        let config = ChaosConfig::new();
        assert!(!config.enabled);
        assert_eq!(config.drop_probability, 0.0);
    }

    #[test]
    fn chaos_config_with_delays() {
        let config =
            ChaosConfig::with_delays(Duration::from_millis(100), Duration::from_millis(500));
        assert!(config.enabled);
        assert!(config.delay_range.is_some());
    }

    #[test]
    fn chaos_config_deterministic() {
        let config1 = ChaosConfig::seeded(42);
        let config2 = ChaosConfig::seeded(42);

        let mut inj1 = ChaosInjector::new(config1);
        let mut inj2 = ChaosInjector::new(config2);

        for _ in 0..10 {
            assert_eq!(inj1.should_drop(), inj2.should_drop());
            assert_eq!(inj1.should_duplicate(), inj2.should_duplicate());
        }
    }

    #[tokio::test]
    async fn delay_applies() {
        let config = ChaosConfig {
            seed: 42,
            delay_range: Some((Duration::from_millis(10), Duration::from_millis(20))),
            ..Default::default()
        };

        let mut inj = ChaosInjector::new(config);
        let start = std::time::Instant::now();
        inj.delay().await;
        let elapsed = start.elapsed();

        assert!(elapsed >= Duration::from_millis(10));
        assert!(elapsed <= Duration::from_millis(30));
    }
}
