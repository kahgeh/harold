//! A five-field cron expression: minute, hour, day, month and weekday.
//! All times are local minutes. This module does not read the clock.
// Task 4 uses these items. Remove this allow when it lands.
#![allow(dead_code)]

use std::fmt;

const MAX_LENGTH: usize = 200;
const SEARCH_YEARS: i32 = 8;
const REFERENCE_YEAR: i32 = 2024;
const MONTH_NAMES: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CronError {
    TooLong,
    NotAscii,
    FieldCount(usize),
    OutOfRange {
        field: &'static str,
        value: String,
        low: u32,
        high: u32,
    },
    NotAName {
        field: &'static str,
        text: String,
    },
    NotANumber {
        field: &'static str,
        text: String,
    },
    Backwards {
        field: &'static str,
        low: u32,
        high: u32,
    },
    StepTooSmall {
        field: &'static str,
        step: String,
    },
    StepWithoutRange {
        field: &'static str,
        text: String,
    },
    EmptyPart {
        field: &'static str,
    },
    NeverRuns,
}

impl CronError {
    /// The name of the field with the fault, or `cron` for the whole expression.
    pub(crate) fn field(&self) -> &'static str {
        match self {
            CronError::TooLong
            | CronError::NotAscii
            | CronError::FieldCount(_)
            | CronError::NeverRuns => "cron",
            CronError::OutOfRange { field, .. }
            | CronError::NotAName { field, .. }
            | CronError::NotANumber { field, .. }
            | CronError::Backwards { field, .. }
            | CronError::StepTooSmall { field, .. }
            | CronError::StepWithoutRange { field, .. }
            | CronError::EmptyPart { field } => field,
        }
    }
}

impl fmt::Display for CronError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CronError::TooLong => write!(
                out,
                "a cron expression is longer than {MAX_LENGTH} characters"
            ),
            CronError::NotAscii => write!(out, "a cron expression uses only ASCII characters"),
            CronError::FieldCount(count) => write!(
                out,
                "a cron expression has 5 fields (minute hour day month weekday); this one has {count}"
            ),
            CronError::OutOfRange {
                field,
                value,
                low,
                high,
            } => write!(out, "{field} {value} is out of range: use {low} to {high}"),
            CronError::NotAName { field, text } => {
                let names = if *field == "month" {
                    "use jan, feb, mar, apr, may, jun, jul, aug, sep, oct, nov or dec"
                } else {
                    "use sun, mon, tue, wed, thu, fri or sat"
                };
                write!(out, "{field} \"{text}\" is not a name: {names}")
            }
            CronError::NotANumber { field, text } => {
                write!(out, "{field} \"{text}\" is not a number")
            }
            CronError::Backwards { field, low, high } => write!(
                out,
                "{field} range {low}-{high} runs backwards: write {high}-{low}"
            ),
            CronError::StepTooSmall { field, step } => {
                write!(out, "{field} step {step} is too small: use 1 or more")
            }
            CronError::StepWithoutRange { field, text } => write!(
                out,
                "{field} step in \"{text}\" needs * or a range before the slash"
            ),
            CronError::EmptyPart { field } => {
                write!(out, "{field} has an empty part: remove the extra comma")
            }
            CronError::NeverRuns => write!(out, "this schedule never runs"),
        }
    }
}

impl std::error::Error for CronError {}

/// A parsed cron expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cron {
    source: String,
    /// Bits 0 to 59.
    minutes: u64,
    /// Bits 0 to 23.
    hours: u32,
    /// Bits 1 to 31.
    days: u32,
    /// Bits 1 to 12.
    months: u16,
    /// Bits 0 to 6, where 0 is Sunday.
    weekdays: u8,
    any_day: bool,
    any_weekday: bool,
}

struct FieldSpec {
    name: &'static str,
    low: u32,
    high: u32,
    names: &'static [&'static str],
    /// The value of the first name.
    names_from: u32,
}

const MINUTE: FieldSpec = FieldSpec {
    name: "minute",
    low: 0,
    high: 59,
    names: &[],
    names_from: 0,
};
const HOUR: FieldSpec = FieldSpec {
    name: "hour",
    low: 0,
    high: 23,
    names: &[],
    names_from: 0,
};
const DAY: FieldSpec = FieldSpec {
    name: "day",
    low: 1,
    high: 31,
    names: &[],
    names_from: 0,
};
const MONTH: FieldSpec = FieldSpec {
    name: "month",
    low: 1,
    high: 12,
    names: &MONTH_NAMES,
    names_from: 1,
};
const WEEKDAY: FieldSpec = FieldSpec {
    name: "weekday",
    low: 0,
    high: 7,
    names: &WEEKDAY_NAMES,
    names_from: 0,
};

fn parse_value(spec: &FieldSpec, text: &str) -> Result<u32, CronError> {
    if text.is_empty() {
        return Err(CronError::EmptyPart { field: spec.name });
    }
    if text.bytes().all(|byte| byte.is_ascii_digit()) {
        return match text.parse::<u32>() {
            Ok(value) if (spec.low..=spec.high).contains(&value) => Ok(value),
            _ => Err(CronError::OutOfRange {
                field: spec.name,
                value: text.to_string(),
                low: spec.low,
                high: spec.high,
            }),
        };
    }
    if !spec.names.is_empty() {
        let lower = text.to_ascii_lowercase();
        if let Some(index) = spec.names.iter().position(|name| *name == lower) {
            return Ok(spec.names_from + index as u32);
        }
        return Err(CronError::NotAName {
            field: spec.name,
            text: text.to_string(),
        });
    }
    Err(CronError::NotANumber {
        field: spec.name,
        text: text.to_string(),
    })
}

/// Parse one field into a bit set. The bit for value `n` is `1 << n`.
fn parse_field(spec: &FieldSpec, text: &str) -> Result<u64, CronError> {
    let mut set = 0u64;
    for part in text.split(',') {
        if part.is_empty() {
            return Err(CronError::EmptyPart { field: spec.name });
        }
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (range, Some(step)),
            None => (part, None),
        };
        let step = match step {
            None => 1,
            Some(step) => match step.parse::<u32>() {
                Ok(0) => {
                    return Err(CronError::StepTooSmall {
                        field: spec.name,
                        step: step.to_string(),
                    });
                }
                Ok(value) => value,
                Err(_) if !step.is_empty() && step.bytes().all(|byte| byte.is_ascii_digit()) => {
                    u32::MAX
                }
                Err(_) => {
                    return Err(CronError::NotANumber {
                        field: spec.name,
                        text: step.to_string(),
                    });
                }
            },
        };
        let (from, to) = if range == "*" {
            (spec.low, spec.high)
        } else if let Some((low, high)) = range.split_once('-') {
            let low = parse_value(spec, low)?;
            let high = parse_value(spec, high)?;
            if low > high {
                return Err(CronError::Backwards {
                    field: spec.name,
                    low,
                    high,
                });
            }
            (low, high)
        } else {
            if part.contains('/') {
                return Err(CronError::StepWithoutRange {
                    field: spec.name,
                    text: part.to_string(),
                });
            }
            let value = parse_value(spec, range)?;
            (value, value)
        };
        let mut value = from;
        while value <= to {
            set |= 1u64 << value;
            value = match value.checked_add(step) {
                Some(next) => next,
                None => break,
            };
        }
    }
    Ok(set)
}

fn mask_of(spec: &FieldSpec, text: &str) -> Result<u64, CronError> {
    parse_field(spec, text)
}

impl Cron {
    pub(crate) fn parse(text: &str) -> Result<Cron, CronError> {
        if text.chars().count() > MAX_LENGTH {
            return Err(CronError::TooLong);
        }
        if !text.is_ascii() {
            return Err(CronError::NotAscii);
        }
        let fields: Vec<&str> = text.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(CronError::FieldCount(fields.len()));
        }
        let minutes = mask_of(&MINUTE, fields[0])?;
        let hours = mask_of(&HOUR, fields[1])?;
        let days = mask_of(&DAY, fields[2])?;
        let months = mask_of(&MONTH, fields[3])?;
        let mut weekdays = mask_of(&WEEKDAY, fields[4])?;
        // 7 means Sunday, the same as 0.
        if weekdays & (1 << 7) != 0 {
            weekdays = (weekdays & !(1 << 7)) | 1;
        }
        let cron = Cron {
            source: fields.join(" "),
            minutes,
            hours: hours as u32,
            days: days as u32,
            months: months as u16,
            weekdays: weekdays as u8,
            any_day: fields[2].starts_with('*'),
            any_weekday: fields[4].starts_with('*'),
        };
        if !cron.runs_on_some_day() {
            return Err(CronError::NeverRuns);
        }
        Ok(cron)
    }

    pub(crate) fn source(&self) -> &str {
        &self.source
    }

    fn day_matches(&self, year: i32, month: u32, day: u32) -> bool {
        if self.months & (1 << month) == 0 {
            return false;
        }
        let day_ok = self.days & (1 << day) != 0;
        let weekday = weekday_of(year, month, day);
        let weekday_ok = self.weekdays & (1 << weekday) != 0;
        if !self.any_day && !self.any_weekday {
            day_ok || weekday_ok
        } else {
            day_ok && weekday_ok
        }
    }

    /// Whether some day in the next 8 years from a fixed date matches.
    fn runs_on_some_day(&self) -> bool {
        let start = days_from_civil(REFERENCE_YEAR, 1, 1);
        let end = days_from_civil(REFERENCE_YEAR + SEARCH_YEARS, 1, 1);
        (start..end).any(|number| {
            let (year, month, day) = civil_from_days(number);
            self.day_matches(year, month, day)
        })
    }

    fn time_matches(&self, hour: u32, minute: u32) -> bool {
        self.hours & (1 << hour) != 0 && self.minutes & (1u64 << minute) != 0
    }

    pub(crate) fn matches(&self, minute: LocalMinute) -> bool {
        self.day_matches(minute.year, minute.month, minute.day)
            && self.time_matches(minute.hour, minute.minute)
    }

    /// The first match after `from`, searched over 8 years.
    pub(crate) fn next_after(&self, from: LocalMinute) -> Option<LocalMinute> {
        let start = from.next();
        let first_day = days_from_civil(start.year, start.month, start.day);
        let last_day = days_from_civil(start.year + SEARCH_YEARS, start.month, start.day);
        for number in first_day..=last_day {
            let (year, month, day) = civil_from_days(number);
            if !self.day_matches(year, month, day) {
                continue;
            }
            let from_minute = if number == first_day {
                start.hour * 60 + start.minute
            } else {
                0
            };
            for time in from_minute..1440 {
                let (hour, minute) = (time / 60, time % 60);
                if self.time_matches(hour, minute) {
                    return Some(LocalMinute {
                        year,
                        month,
                        day,
                        hour,
                        minute,
                    });
                }
            }
        }
        None
    }

    /// The smallest gap in minutes between two runs, or `None` if the schedule
    /// has fewer than two runs in 8 years.
    pub(crate) fn min_gap_minutes(&self) -> Option<u32> {
        let times: Vec<u64> = (0..1440u64)
            .filter(|time| self.time_matches((*time / 60) as u32, (*time % 60) as u32))
            .collect();
        let start = days_from_civil(REFERENCE_YEAR, 1, 1);
        let end = days_from_civil(REFERENCE_YEAR + SEARCH_YEARS, 1, 1);
        let mut previous: Option<u64> = None;
        let mut smallest: Option<u64> = None;
        for number in start..end {
            let (year, month, day) = civil_from_days(number);
            if !self.day_matches(year, month, day) {
                continue;
            }
            let base = (number - start) as u64 * 1440;
            for time in &times {
                let now = base + time;
                if let Some(before) = previous {
                    let gap = now - before;
                    smallest = Some(smallest.map_or(gap, |value| value.min(gap)));
                    if gap == 1 {
                        return Some(1);
                    }
                }
                previous = Some(now);
            }
        }
        smallest.map(|gap| gap as u32)
    }
}

/// A local minute: a date and a time without a zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct LocalMinute {
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
}

impl LocalMinute {
    /// Parse `YYYY-MM-DD HH:MM`.
    pub(crate) fn parse(text: &str) -> Option<LocalMinute> {
        let bytes = text.as_bytes();
        if bytes.len() != 16
            || bytes[4] != b'-'
            || bytes[7] != b'-'
            || bytes[10] != b' '
            || bytes[13] != b':'
        {
            return None;
        }
        let number = |from: usize, to: usize| -> Option<u32> {
            let part = &bytes[from..to];
            if part.iter().all(u8::is_ascii_digit) {
                std::str::from_utf8(part).ok()?.parse().ok()
            } else {
                None
            }
        };
        let year = number(0, 4)? as i32;
        let (month, day) = (number(5, 7)?, number(8, 10)?);
        let (hour, minute) = (number(11, 13)?, number(14, 16)?);
        if !(1..=12).contains(&month)
            || day < 1
            || day > days_in_month(year, month)
            || hour > 23
            || minute > 59
        {
            return None;
        }
        Some(LocalMinute {
            year,
            month,
            day,
            hour,
            minute,
        })
    }

    pub(crate) fn next(self) -> LocalMinute {
        if self.minute < 59 {
            return LocalMinute {
                minute: self.minute + 1,
                ..self
            };
        }
        if self.hour < 23 {
            return LocalMinute {
                hour: self.hour + 1,
                minute: 0,
                ..self
            };
        }
        let (year, month, day) =
            civil_from_days(days_from_civil(self.year, self.month, self.day) + 1);
        LocalMinute {
            year,
            month,
            day,
            hour: 0,
            minute: 0,
        }
    }

    /// 0 is Sunday.
    pub(crate) fn weekday(self) -> u32 {
        weekday_of(self.year, self.month, self.day)
    }

    fn total_minutes(self) -> i64 {
        days_from_civil(self.year, self.month, self.day) * 1440
            + i64::from(self.hour * 60 + self.minute)
    }
}

impl fmt::Display for LocalMinute {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            out,
            "{:04}-{:02}-{:02} {:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DueRun {
    /// The newest match after the checked minute, when it is inside the window.
    pub(crate) run: Option<LocalMinute>,
    /// The count of older matches skipped.
    pub(crate) missed: u32,
}

/// Find the run due now. Look at the matches after `checked` up to `now`.
pub(crate) fn due_run(
    cron: &Cron,
    checked: Option<LocalMinute>,
    now: LocalMinute,
    give_up_minutes: u32,
) -> DueRun {
    let none = DueRun {
        run: None,
        missed: 0,
    };
    let Some(checked) = checked else {
        return none;
    };
    if checked >= now {
        return none;
    }
    let mut count = 0u32;
    let mut newest = None;
    let mut cursor = checked;
    while let Some(found) = cron.next_after(cursor) {
        if found > now {
            break;
        }
        count = count.saturating_add(1);
        newest = Some(found);
        cursor = found;
    }
    let Some(newest) = newest else {
        return none;
    };
    if now.total_minutes() - newest.total_minutes() <= i64::from(give_up_minutes) {
        DueRun {
            run: Some(newest),
            missed: count - 1,
        }
    } else {
        DueRun {
            run: None,
            missed: count,
        }
    }
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(year) => 29,
        _ => 28,
    }
}

/// Days since 1970-01-01 for a civil date.
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = i64::from(year) - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted = (i64::from(month) + 9) % 12;
    let day_of_year = (153 * shifted + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_shifted = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_shifted + 2) / 5 + 1) as u32;
    let month = if month_shifted < 10 {
        month_shifted + 3
    } else {
        month_shifted - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year as i32, month, day)
}

fn weekday_of(year: i32, month: u32, day: u32) -> u32 {
    // 1970-01-01 was a Thursday.
    (days_from_civil(year, month, day) + 4).rem_euclid(7) as u32
}

#[cfg(test)]
#[path = "cron_tests.rs"]
mod tests;
