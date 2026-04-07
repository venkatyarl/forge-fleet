use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ScheduleError {
    #[error("invalid cron expression '{expr}': expected 5 fields (minute hour day month weekday)")]
    InvalidFieldCount { expr: String },

    #[error("invalid field '{field}': {reason}")]
    InvalidField { field: String, reason: String },

    #[error("invalid value '{value}' in field '{field}'")]
    InvalidValue { field: String, value: String },

    #[error("empty field in cron expression")]
    EmptyField,
}

#[derive(Debug, Clone)]
struct CronField {
    values: BTreeSet<u32>,
    min: u32,
}

impl CronField {
    fn new(values: BTreeSet<u32>, min: u32, _max: u32) -> Result<Self, ScheduleError> {
        if values.is_empty() {
            return Err(ScheduleError::EmptyField);
        }
        Ok(Self { values, min })
    }

    fn contains(&self, value: u32) -> bool {
        self.values.contains(&value)
    }

    fn first(&self) -> u32 {
        *self.values.first().unwrap_or(&self.min)
    }

    fn next_or_first(&self, current: u32) -> (u32, bool) {
        if let Some(next) = self.values.range(current..).next() {
            (*next, false)
        } else {
            (self.first(), true)
        }
    }
}

/// Parsed cron schedule (5-field format: minute hour day month weekday).
#[derive(Debug, Clone)]
pub struct CronSchedule {
    expression: String,
    minute: CronField,
    hour: CronField,
    day_of_month: CronField,
    month: CronField,
    day_of_week: CronField,
    dom_any: bool,
    dow_any: bool,
}

impl CronSchedule {
    /// Parse a cron expression in 5-field format:
    /// `minute hour day_of_month month day_of_week`
    pub fn parse(expr: &str) -> Result<Self, ScheduleError> {
        let trimmed = expr.trim();
        let parts: Vec<&str> = trimmed.split_whitespace().collect();

        if parts.len() != 5 {
            return Err(ScheduleError::InvalidFieldCount {
                expr: expr.to_string(),
            });
        }

        let month_aliases = month_aliases();
        let weekday_aliases = weekday_aliases();

        let (minute, _) = parse_field(parts[0], "minute", 0, 59, &HashMap::new(), false)?;
        let (hour, _) = parse_field(parts[1], "hour", 0, 23, &HashMap::new(), false)?;
        let (day_of_month, dom_any) =
            parse_field(parts[2], "day_of_month", 1, 31, &HashMap::new(), false)?;
        let (month, _) = parse_field(parts[3], "month", 1, 12, &month_aliases, false)?;
        let (day_of_week, dow_any) =
            parse_field(parts[4], "day_of_week", 0, 6, &weekday_aliases, true)?;

        Ok(Self {
            expression: trimmed.to_string(),
            minute,
            hour,
            day_of_month,
            month,
            day_of_week,
            dom_any,
            dow_any,
        })
    }

    /// Original cron expression.
    pub fn expression(&self) -> &str {
        &self.expression
    }

    /// Return the next run strictly after `from`.
    pub fn next_after(&self, from: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let mut candidate = round_up_to_next_minute(from);
        let max_year = from.year() + 10;

        while candidate.year() <= max_year {
            // Month fast-forward.
            let month = candidate.month();
            if !self.month.contains(month) {
                let (next_month, rolled) = self.month.next_or_first(month);
                let year = if rolled {
                    candidate.year() + 1
                } else {
                    candidate.year()
                };

                candidate = make_utc(year, next_month, 1, 0, 0)?;
                continue;
            }

            if !self.day_matches(candidate) {
                candidate = make_utc(candidate.year(), candidate.month(), candidate.day(), 0, 0)?
                    + Duration::days(1);
                continue;
            }

            let hour = candidate.hour();
            if !self.hour.contains(hour) {
                let (next_hour, rolled) = self.hour.next_or_first(hour);
                if rolled {
                    candidate =
                        make_utc(candidate.year(), candidate.month(), candidate.day(), 0, 0)?
                            + Duration::days(1);
                    candidate = set_time(candidate, next_hour, 0)?;
                } else {
                    candidate = set_time(candidate, next_hour, 0)?;
                }
                continue;
            }

            let minute = candidate.minute();
            if !self.minute.contains(minute) {
                let (next_minute, rolled) = self.minute.next_or_first(minute);
                if rolled {
                    candidate = set_time(candidate, candidate.hour(), 0)? + Duration::hours(1);
                    candidate = set_time(candidate, candidate.hour(), next_minute)?;
                } else {
                    candidate = set_time(candidate, candidate.hour(), next_minute)?;
                }
                continue;
            }

            // Re-check day match after any rollover caused by hour/minute adjustment.
            if !self.day_matches(candidate) {
                candidate = make_utc(candidate.year(), candidate.month(), candidate.day(), 0, 0)?
                    + Duration::days(1);
                continue;
            }

            return Some(candidate);
        }

        None
    }

    fn day_matches(&self, dt: DateTime<Utc>) -> bool {
        let dom_match = self.day_of_month.contains(dt.day());
        let dow = dt.weekday().num_days_from_sunday();
        let dow_match = self.day_of_week.contains(dow);

        match (self.dom_any, self.dow_any) {
            (true, true) => true,
            (true, false) => dow_match,
            (false, true) => dom_match,
            (false, false) => dom_match || dow_match,
        }
    }
}

fn parse_field(
    raw: &str,
    field_name: &str,
    min: u32,
    max: u32,
    aliases: &HashMap<&'static str, u32>,
    normalize_weekday: bool,
) -> Result<(CronField, bool), ScheduleError> {
    if raw.trim().is_empty() {
        return Err(ScheduleError::InvalidField {
            field: field_name.to_string(),
            reason: "field cannot be empty".to_string(),
        });
    }

    let any = raw.trim() == "*";
    let mut values = BTreeSet::new();

    for token in raw.split(',') {
        let token = token.trim();
        if token.is_empty() {
            return Err(ScheduleError::InvalidField {
                field: field_name.to_string(),
                reason: "empty token in list".to_string(),
            });
        }

        let (base, step) = if let Some((lhs, rhs)) = token.split_once('/') {
            let step = rhs
                .trim()
                .parse::<u32>()
                .map_err(|_| ScheduleError::InvalidValue {
                    field: field_name.to_string(),
                    value: rhs.trim().to_string(),
                })?;
            if step == 0 {
                return Err(ScheduleError::InvalidField {
                    field: field_name.to_string(),
                    reason: "step must be >= 1".to_string(),
                });
            }
            (lhs.trim(), step)
        } else {
            (token, 1)
        };

        let (start, end) = if base == "*" {
            (min, max)
        } else if let Some((lhs, rhs)) = base.split_once('-') {
            let start = parse_value(lhs.trim(), field_name, min, max, aliases, normalize_weekday)?;
            let end = parse_value(rhs.trim(), field_name, min, max, aliases, normalize_weekday)?;
            if start > end {
                return Err(ScheduleError::InvalidField {
                    field: field_name.to_string(),
                    reason: format!("range start {} > end {}", start, end),
                });
            }
            (start, end)
        } else {
            let start = parse_value(base, field_name, min, max, aliases, normalize_weekday)?;
            if token.contains('/') {
                (start, max)
            } else {
                (start, start)
            }
        };

        let mut value = start;
        while value <= end {
            values.insert(value);
            match value.checked_add(step) {
                Some(next) if next > value => value = next,
                _ => break,
            }
        }
    }

    Ok((CronField::new(values, min, max)?, any))
}

fn parse_value(
    token: &str,
    field_name: &str,
    min: u32,
    max: u32,
    aliases: &HashMap<&'static str, u32>,
    normalize_weekday: bool,
) -> Result<u32, ScheduleError> {
    let lowered = token.to_ascii_lowercase();

    let mut value = if let Some(alias) = aliases.get(lowered.as_str()) {
        *alias
    } else {
        lowered
            .parse::<u32>()
            .map_err(|_| ScheduleError::InvalidValue {
                field: field_name.to_string(),
                value: token.to_string(),
            })?
    };

    if normalize_weekday && value == 7 {
        value = 0;
    }

    let valid_upper = if normalize_weekday { 7 } else { max };
    if !(min..=valid_upper).contains(&value) {
        return Err(ScheduleError::InvalidField {
            field: field_name.to_string(),
            reason: format!("value {} outside {}..={}", value, min, valid_upper),
        });
    }

    Ok(value)
}

fn month_aliases() -> HashMap<&'static str, u32> {
    HashMap::from([
        ("jan", 1),
        ("feb", 2),
        ("mar", 3),
        ("apr", 4),
        ("may", 5),
        ("jun", 6),
        ("jul", 7),
        ("aug", 8),
        ("sep", 9),
        ("oct", 10),
        ("nov", 11),
        ("dec", 12),
    ])
}

fn weekday_aliases() -> HashMap<&'static str, u32> {
    HashMap::from([
        ("sun", 0),
        ("mon", 1),
        ("tue", 2),
        ("wed", 3),
        ("thu", 4),
        ("fri", 5),
        ("sat", 6),
    ])
}

fn round_up_to_next_minute(dt: DateTime<Utc>) -> DateTime<Utc> {
    let base = dt
        .with_second(0)
        .and_then(|d| d.with_nanosecond(0))
        .unwrap_or(dt);
    base + Duration::minutes(1)
}

fn make_utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> Option<DateTime<Utc>> {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
}

fn set_time(dt: DateTime<Utc>, hour: u32, minute: u32) -> Option<DateTime<Utc>> {
    make_utc(dt.year(), dt.month(), dt.day(), hour, minute)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).single().unwrap()
    }

    #[test]
    fn parse_basic_expression() {
        let schedule = CronSchedule::parse("*/15 9-17 * * 1-5").unwrap();
        assert_eq!(schedule.expression(), "*/15 9-17 * * 1-5");
    }

    #[test]
    fn parse_aliases() {
        let schedule = CronSchedule::parse("0 8 * jan,mar mon-fri").unwrap();
        let next = schedule.next_after(utc(2026, 1, 1, 0, 0)).unwrap();
        assert_eq!(next.month(), 1);
        assert!((1..=5).contains(&next.weekday().num_days_from_monday()));
    }

    #[test]
    fn next_every_minute() {
        let schedule = CronSchedule::parse("* * * * *").unwrap();
        let now = utc(2026, 4, 4, 10, 30);
        let next = schedule.next_after(now).unwrap();
        assert_eq!(next, utc(2026, 4, 4, 10, 31));
    }

    #[test]
    fn next_weekday_work_hours() {
        let schedule = CronSchedule::parse("0 9 * * 1-5").unwrap();
        // Friday 10:00 -> next Monday 09:00
        let next = schedule.next_after(utc(2026, 4, 10, 10, 0)).unwrap();
        assert_eq!(next, utc(2026, 4, 13, 9, 0));
    }

    #[test]
    fn day_of_month_or_weekday_semantics() {
        // day_of_month=1 OR weekday=Sunday
        let schedule = CronSchedule::parse("0 0 1 * 0").unwrap();
        let next = schedule.next_after(utc(2026, 4, 2, 0, 0)).unwrap();
        // First upcoming Sunday is Apr 5, 2026.
        assert_eq!(next, utc(2026, 4, 5, 0, 0));
    }

    #[test]
    fn invalid_field_count() {
        let err = CronSchedule::parse("* * * *").unwrap_err();
        assert!(matches!(err, ScheduleError::InvalidFieldCount { .. }));
    }
}
