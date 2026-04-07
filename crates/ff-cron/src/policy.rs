use std::time::Duration;

use chrono::{DateTime, Datelike, FixedOffset, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};

/// Job priority used for scheduling decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum JobPriority {
    Low = 0,
    #[default]
    Normal = 1,
    High = 2,
    Critical = 3,
}

impl JobPriority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }

    pub fn parse_str(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "normal" => Some(Self::Normal),
            "high" => Some(Self::High),
            "critical" => Some(Self::Critical),
            _ => None,
        }
    }
}

/// Exponential backoff settings used for retries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackoffPolicy {
    pub initial_delay_secs: u64,
    pub max_delay_secs: u64,
    pub multiplier: f64,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            initial_delay_secs: 30,
            max_delay_secs: 60 * 30,
            multiplier: 2.0,
        }
    }
}

impl BackoffPolicy {
    /// Compute retry delay for attempt number (1-based).
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        if attempt == 0 {
            return Duration::from_secs(0);
        }

        let exponent = (attempt - 1) as i32;
        let base = self.initial_delay_secs as f64;
        let raw = base * self.multiplier.powi(exponent);
        let clamped = raw
            .max(self.initial_delay_secs as f64)
            .min(self.max_delay_secs as f64);

        Duration::from_secs(clamped.round() as u64)
    }
}

/// Quiet-hour policy to suppress non-urgent jobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuietHoursPolicy {
    /// Hour in local policy timezone where quiet mode starts (0..=23).
    pub start_hour: u8,
    /// Hour in local policy timezone where quiet mode ends (0..=23).
    pub end_hour: u8,
    /// Fixed timezone offset from UTC in minutes.
    /// Example: -300 for America/New_York (EST), -240 for EDT.
    pub timezone_offset_minutes: i32,
}

impl QuietHoursPolicy {
    pub fn is_quiet(&self, now_utc: DateTime<Utc>) -> bool {
        let Some(offset) = FixedOffset::east_opt(self.timezone_offset_minutes * 60) else {
            return false;
        };

        let local = now_utc.with_timezone(&offset);
        let hour = local.hour() as u8;

        match self.start_hour.cmp(&self.end_hour) {
            std::cmp::Ordering::Less => hour >= self.start_hour && hour < self.end_hour,
            std::cmp::Ordering::Greater => hour >= self.start_hour || hour < self.end_hour,
            std::cmp::Ordering::Equal => true, // 24h quiet period
        }
    }

    /// If currently inside quiet window, returns the UTC timestamp when quiet period ends.
    pub fn quiet_ends_at(&self, now_utc: DateTime<Utc>) -> Option<DateTime<Utc>> {
        if !self.is_quiet(now_utc) {
            return None;
        }

        let offset = FixedOffset::east_opt(self.timezone_offset_minutes * 60)?;
        let local = now_utc.with_timezone(&offset);

        let target_date = if self.start_hour < self.end_hour {
            // Non-wrapping quiet window (e.g. 13 -> 17).
            local.date_naive()
        } else if local.hour() as u8 >= self.start_hour {
            // Wrapping quiet window, in evening side (e.g. 22 -> 7).
            local.date_naive().succ_opt()?
        } else {
            // Wrapping quiet window, morning side.
            local.date_naive()
        };

        let local_end = offset
            .with_ymd_and_hms(
                target_date.year(),
                target_date.month(),
                target_date.day(),
                self.end_hour as u32,
                0,
                0,
            )
            .single()?;

        Some(local_end.with_timezone(&Utc))
    }
}

/// Runtime scheduling policy combining quiet-hours + backoff rules.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulingPolicy {
    pub quiet_hours: Option<QuietHoursPolicy>,
    pub min_priority_during_quiet: JobPriority,
    pub backoff: BackoffPolicy,
}

impl Default for SchedulingPolicy {
    fn default() -> Self {
        Self {
            quiet_hours: None,
            min_priority_during_quiet: JobPriority::Critical,
            backoff: BackoffPolicy::default(),
        }
    }
}

impl SchedulingPolicy {
    pub fn should_run(&self, priority: JobPriority, now: DateTime<Utc>) -> bool {
        if let Some(quiet) = &self.quiet_hours
            && quiet.is_quiet(now)
            && priority < self.min_priority_during_quiet
        {
            return false;
        }
        true
    }

    pub fn defer_until(&self, priority: JobPriority, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let quiet = self.quiet_hours.as_ref()?;
        if quiet.is_quiet(now) && priority < self.min_priority_during_quiet {
            quiet.quiet_ends_at(now)
        } else {
            None
        }
    }

    pub fn retry_delay(&self, attempt: u32) -> Duration {
        self.backoff.delay_for_attempt(attempt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).single().unwrap()
    }

    #[test]
    fn backoff_grows_and_caps() {
        let backoff = BackoffPolicy {
            initial_delay_secs: 10,
            max_delay_secs: 100,
            multiplier: 2.0,
        };

        assert_eq!(backoff.delay_for_attempt(1).as_secs(), 10);
        assert_eq!(backoff.delay_for_attempt(2).as_secs(), 20);
        assert_eq!(backoff.delay_for_attempt(3).as_secs(), 40);
        assert_eq!(backoff.delay_for_attempt(5).as_secs(), 100);
    }

    #[test]
    fn quiet_hours_wrap_correctly() {
        let quiet = QuietHoursPolicy {
            start_hour: 22,
            end_hour: 7,
            timezone_offset_minutes: 0,
        };

        assert!(quiet.is_quiet(utc(2026, 4, 4, 23, 0)));
        assert!(quiet.is_quiet(utc(2026, 4, 5, 6, 30)));
        assert!(!quiet.is_quiet(utc(2026, 4, 5, 12, 0)));
    }

    #[test]
    fn scheduling_policy_defers_low_priority_in_quiet_hours() {
        let policy = SchedulingPolicy {
            quiet_hours: Some(QuietHoursPolicy {
                start_hour: 22,
                end_hour: 7,
                timezone_offset_minutes: 0,
            }),
            min_priority_during_quiet: JobPriority::High,
            backoff: BackoffPolicy::default(),
        };

        let now = utc(2026, 4, 4, 23, 15);
        assert!(!policy.should_run(JobPriority::Normal, now));
        assert!(policy.should_run(JobPriority::Critical, now));
        assert_eq!(
            policy.defer_until(JobPriority::Normal, now).unwrap(),
            utc(2026, 4, 5, 7, 0)
        );
    }
}
