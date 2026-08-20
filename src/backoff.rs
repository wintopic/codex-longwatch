use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const ABSOLUTE_MIN_INTERVAL_SECS: u64 = 15;
pub const MIN_WAKE_DELAY_SECS: u64 = 60;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct RetryPolicy {
    pub base_delay_secs: u64,
    pub multiplier: f64,
    pub max_delay_secs: u64,
    pub jitter_ratio: f64,
    pub max_empty_replies: u8,
    pub wake_delay_secs: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            base_delay_secs: 30,
            multiplier: 1.7,
            max_delay_secs: 10 * 60,
            jitter_ratio: 0.15,
            max_empty_replies: 5,
            wake_delay_secs: MIN_WAKE_DELAY_SECS,
        }
    }
}

impl RetryPolicy {
    pub fn validate(&self) -> Result<(), RetryPolicyError> {
        if self.base_delay_secs < ABSOLUTE_MIN_INTERVAL_SECS {
            return Err(RetryPolicyError::IntervalTooShort(self.base_delay_secs));
        }
        if self.max_delay_secs < self.base_delay_secs {
            return Err(RetryPolicyError::MaxBelowBase);
        }
        if !self.multiplier.is_finite() || self.multiplier < 1.0 {
            return Err(RetryPolicyError::InvalidMultiplier);
        }
        if !self.jitter_ratio.is_finite() || !(0.0..=0.5).contains(&self.jitter_ratio) {
            return Err(RetryPolicyError::InvalidJitter);
        }
        if self.wake_delay_secs < MIN_WAKE_DELAY_SECS {
            return Err(RetryPolicyError::WakeDelayTooShort);
        }
        Ok(())
    }

    #[must_use]
    pub fn delay_for<R: Rng + ?Sized>(&self, retry_number: u32, rng: &mut R) -> Duration {
        let exponent = retry_number.saturating_sub(1) as i32;
        let raw = (self.base_delay_secs as f64 * self.multiplier.powi(exponent))
            .min(self.max_delay_secs as f64);
        let jitter = rng.random_range(-self.jitter_ratio..=self.jitter_ratio);
        let jittered = (raw * (1.0 + jitter)).round();
        let floor = ABSOLUTE_MIN_INTERVAL_SECS as f64;
        Duration::from_secs(jittered.max(floor) as u64)
    }

    #[must_use]
    pub fn wake_delay(&self) -> Duration {
        Duration::from_secs(self.wake_delay_secs.max(MIN_WAKE_DELAY_SECS))
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum RetryPolicyError {
    #[error("重试间隔 {0} 秒低于 15 秒安全下限")]
    IntervalTooShort(u64),
    #[error("最大重试间隔不能小于初始间隔")]
    MaxBelowBase,
    #[error("重试倍率必须是大于或等于 1.0 的有限数值")]
    InvalidMultiplier,
    #[error("抖动比例必须是 0 到 0.5 之间的有限数值")]
    InvalidJitter,
    #[error("系统唤醒后的等待时间不能少于 60 秒")]
    WakeDelayTooShort,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct AttemptLedger(pub Vec<DateTime<Utc>>);

impl AttemptLedger {
    pub fn record(&mut self, now: DateTime<Utc>) {
        self.prune(now);
        self.0.push(now);
    }

    fn prune(&mut self, now: DateTime<Utc>) {
        let one_day_ago = now - chrono::Duration::hours(24);
        self.0.retain(|stamp| *stamp > one_day_ago && *stamp <= now);
    }
}

#[derive(Debug)]
pub struct WakeDetector {
    wall: DateTime<Utc>,
    monotonic: Instant,
    tolerance: Duration,
    max_normal_observation_gap: Duration,
}

impl WakeDetector {
    #[must_use]
    pub fn new(wall: DateTime<Utc>, monotonic: Instant) -> Self {
        Self {
            wall,
            monotonic,
            tolerance: Duration::from_secs(20),
            max_normal_observation_gap: Duration::from_secs(45),
        }
    }

    pub fn observe(&mut self, wall: DateTime<Utc>, monotonic: Instant) -> bool {
        let wall_elapsed = (wall - self.wall).to_std().unwrap_or_default();
        let monotonic_elapsed = monotonic.saturating_duration_since(self.monotonic);
        self.wall = wall;
        self.monotonic = monotonic;
        // Some platforms include suspend time in their monotonic clock while
        // others do not.  Detect both a wall/monotonic discrepancy and a poll
        // that arrived far later than the runtime heartbeat expected.
        wall_elapsed > monotonic_elapsed.saturating_add(self.tolerance)
            || wall_elapsed > self.max_normal_observation_gap
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    use super::*;

    #[test]
    fn default_backoff_is_bounded_and_jittered() {
        let policy = RetryPolicy::default();
        let mut rng = ChaCha8Rng::seed_from_u64(7);
        let first = policy.delay_for(1, &mut rng).as_secs();
        assert!((25..=35).contains(&first));

        let far_future = policy.delay_for(99, &mut rng).as_secs();
        assert!((510..=690).contains(&far_future));
    }

    #[test]
    fn custom_interval_cannot_drop_below_fifteen_seconds() {
        let policy = RetryPolicy {
            base_delay_secs: 14,
            ..RetryPolicy::default()
        };
        assert_eq!(
            policy.validate(),
            Err(RetryPolicyError::IntervalTooShort(14))
        );
    }

    #[test]
    fn wake_detector_handles_monotonic_clocks_that_include_suspend_time() {
        let wall = Utc.with_ymd_and_hms(2026, 7, 28, 0, 0, 0).unwrap();
        let monotonic = Instant::now();
        let mut detector = WakeDetector::new(wall, monotonic);

        assert!(detector.observe(
            wall + chrono::Duration::minutes(5),
            monotonic + Duration::from_secs(5 * 60)
        ));
    }
}
