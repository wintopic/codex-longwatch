use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const MIN_WAKE_DELAY_SECS: u64 = 60;
/// A failed attempt is retried right away. The only wait keeps two attempts
/// of the same task at least this far apart, so a request that fails the
/// moment it is sent cannot turn into a tight loop.
pub const MIN_RETRY_SPACING: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct RetryPolicy {
    pub max_empty_replies: u8,
    pub wake_delay_secs: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_empty_replies: 5,
            wake_delay_secs: MIN_WAKE_DELAY_SECS,
        }
    }
}

impl RetryPolicy {
    pub fn validate(&self) -> Result<(), RetryPolicyError> {
        if self.wake_delay_secs < MIN_WAKE_DELAY_SECS {
            return Err(RetryPolicyError::WakeDelayTooShort);
        }
        Ok(())
    }

    #[must_use]
    pub fn wake_delay(&self) -> Duration {
        Duration::from_secs(self.wake_delay_secs.max(MIN_WAKE_DELAY_SECS))
    }
}

/// How long to wait before retrying an attempt that began at `started`:
/// nothing, unless that attempt is younger than [`MIN_RETRY_SPACING`].
#[must_use]
pub fn retry_delay(started: Option<Instant>, now: Instant) -> Duration {
    started.map_or(Duration::ZERO, |started| {
        MIN_RETRY_SPACING.saturating_sub(now.saturating_duration_since(started))
    })
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RetryPolicyError {
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

    #[must_use]
    pub fn first(&self) -> Option<DateTime<Utc>> {
        self.0.first().copied()
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

    use super::*;

    #[test]
    fn a_retry_never_waits_once_the_attempt_has_run_for_a_while() {
        let now = Instant::now();
        assert_eq!(retry_delay(None, now), Duration::ZERO);
        assert_eq!(
            retry_delay(Some(now), now + Duration::from_secs(60)),
            Duration::ZERO
        );
        assert_eq!(
            retry_delay(Some(now), now + MIN_RETRY_SPACING),
            Duration::ZERO
        );
    }

    #[test]
    fn an_instant_failure_waits_out_the_rest_of_the_spacing() {
        let now = Instant::now();
        assert_eq!(retry_delay(Some(now), now), MIN_RETRY_SPACING);
        assert_eq!(
            retry_delay(Some(now), now + Duration::from_millis(500)),
            Duration::from_millis(1500)
        );
    }

    #[test]
    fn wake_delay_cannot_drop_below_sixty_seconds() {
        let policy = RetryPolicy {
            wake_delay_secs: 59,
            ..RetryPolicy::default()
        };
        assert_eq!(policy.validate(), Err(RetryPolicyError::WakeDelayTooShort));
        assert!(RetryPolicy::default().validate().is_ok());
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
