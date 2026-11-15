//! Time-of-day (peak / off-peak) list prices for a combo.
//!
//! A combo's base list price (`pricing_input_per_1m` / `pricing_output_per_1m` /
//! `pricing_cached_per_1m`) is flat: every request bills the same rate. Several
//! upstreams sell the same model at two tariffs — a peak band during their busy
//! hours and a cheaper off-peak band outside them — so a reseller that mirrors
//! that structure needs a schedule instead of a single number.
//!
//! This module is the pure, database-free half of that feature:
//!
//! * [`PricingSchedule`] — the JSON contract persisted in `combos.pricing_schedule`.
//! * [`PricingRate`] — one band's USD-per-1M rates.
//! * [`PeakWindow`] — an ISO-weekday window `[start, end)` in UTC; overnight allowed.
//! * [`PeakClock`] / [`OffPeakDate`] — strictly parsed `HH:MM` and `YYYY-MM-DD`.
//!
//! Resolution ([`PricingSchedule::rate_at`]) is a pure function of an instant:
//! nothing here reads the wall clock, so billing can resolve a *stored* usage
//! timestamp and reproduce exactly the band a historical row was billed with.
//!
//! Semantics, in the order they apply at one instant:
//!
//! 1. A date listed in `off_peak_dates` is off-peak all day, whatever the
//!    windows say (holidays, upstream promotions).
//! 2. Otherwise the instant is peak when it falls inside any `peak_windows`
//!    entry, and off-peak when it does not.
//! 3. An overnight window (`end <= start`) is anchored at its **start** day: a
//!    `22:00`–`02:00` window listed for Monday covers Monday 22:00–24:00 and
//!    Tuesday 00:00–02:00.

use crate::error::{CoreError, Result};
use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc, Weekday};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Maximum number of `peak_windows` entries a schedule may declare. A week has
/// 168 hours; 24 windows is already far past any real tariff table and keeps the
/// overlap check trivially bounded.
pub const MAX_PEAK_WINDOWS: usize = 24;

/// Maximum number of explicit `off_peak_dates`. A full year plus slack: the list
/// exists for holidays, not for enumerating a calendar.
pub const MAX_OFF_PEAK_DATES: usize = 366;

/// The only timezone the schedule supports. Windows are authored and billed in
/// UTC; a local-zone schedule would bill differently across DST shifts, so any
/// other value is rejected rather than silently reinterpreted.
pub const SCHEDULE_TIMEZONE: &str = "UTC";

const MINUTES_PER_DAY: u32 = 24 * 60;
const MINUTES_PER_WEEK: u32 = 7 * MINUTES_PER_DAY;

/// One band's rates, USD per 1M tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PricingRate {
    pub input_per_1m: f64,
    pub output_per_1m: f64,
    /// Cached-input rate in the same unit. `None` bills cache reads at
    /// `input_per_1m`, exactly like the combo's flat price does. Serialized by
    /// omission rather than `null` (both are accepted on input).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_per_1m: Option<f64>,
}

impl PricingRate {
    /// Reject rates that cannot be billed: `NaN`, infinite or negative.
    fn validate(&self, band: &str) -> Result<()> {
        for (name, rate) in [
            ("input_per_1m", Some(self.input_per_1m)),
            ("output_per_1m", Some(self.output_per_1m)),
            ("cached_per_1m", self.cached_per_1m),
        ] {
            let Some(rate) = rate else { continue };
            if !rate.is_finite() || rate < 0.0 {
                return Err(CoreError::Validation(format!(
                    "pricing_schedule.{band}.{name} must be a finite, non-negative USD rate per 1M tokens, got {rate}"
                )));
            }
        }
        Ok(())
    }

    /// Element-wise maximum of two bands: the conservative bound used to
    /// reserve credits before the request's band is known.
    #[must_use]
    fn upper_bound(&self, other: &Self) -> Self {
        let cached = match (self.cached_per_1m, other.cached_per_1m) {
            (None, None) => None,
            (a, b) => Some(
                a.unwrap_or(self.input_per_1m)
                    .max(b.unwrap_or(other.input_per_1m)),
            ),
        };
        Self {
            input_per_1m: self.input_per_1m.max(other.input_per_1m),
            output_per_1m: self.output_per_1m.max(other.output_per_1m),
            cached_per_1m: cached,
        }
    }
}

/// `HH:MM` on a 24-hour clock, zero-padded. Stored as minutes since midnight so
/// resolution never re-parses a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeakClock(u16);

impl PeakClock {
    /// Minutes since midnight.
    #[must_use]
    pub const fn minutes(self) -> u16 {
        self.0
    }

    /// Parse the strict wire form `HH:MM` (`00:00`..`23:59`).
    pub fn parse(text: &str) -> std::result::Result<Self, String> {
        let bytes = text.as_bytes();
        if bytes.len() != 5 || bytes[2] != b':' {
            return Err(format!("expected HH:MM, got {text:?}"));
        }
        let digits = [bytes[0], bytes[1], bytes[3], bytes[4]];
        if !digits.iter().all(u8::is_ascii_digit) {
            return Err(format!("expected HH:MM, got {text:?}"));
        }
        // Digits were checked above, so both halves are ASCII numbers.
        let hour = u16::from(bytes[0] - b'0') * 10 + u16::from(bytes[1] - b'0');
        let minute = u16::from(bytes[3] - b'0') * 10 + u16::from(bytes[4] - b'0');
        if hour > 23 || minute > 59 {
            return Err(format!("time out of range (00:00..23:59), got {text:?}"));
        }
        Ok(Self(hour * 60 + minute))
    }
}

impl fmt::Display for PeakClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.0 / 60, self.0 % 60)
    }
}

impl Serialize for PeakClock {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for PeakClock {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(D::Error::custom)
    }
}

/// A calendar date in the strict wire form `YYYY-MM-DD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OffPeakDate(NaiveDate);

impl OffPeakDate {
    #[must_use]
    pub const fn date(self) -> NaiveDate {
        self.0
    }

    /// Parse the strict wire form `YYYY-MM-DD`: ten bytes, zero-padded, and a
    /// date that exists on the calendar.
    pub fn parse(text: &str) -> std::result::Result<Self, String> {
        let bytes = text.as_bytes();
        let shape_ok = bytes.len() == 10
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes
                .iter()
                .enumerate()
                .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit());
        if !shape_ok {
            return Err(format!("expected YYYY-MM-DD, got {text:?}"));
        }
        NaiveDate::parse_from_str(text, "%Y-%m-%d")
            .map(Self)
            .map_err(|_| format!("not a calendar date: {text:?}"))
    }
}

impl fmt::Display for OffPeakDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.format("%Y-%m-%d"))
    }
}

impl Serialize for OffPeakDate {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for OffPeakDate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(D::Error::custom)
    }
}

/// A peak window: `[start, end)` on every ISO weekday listed in `days`.
///
/// `end <= start` means the window wraps past midnight and is anchored at the
/// **start** day (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeakWindow {
    /// ISO weekdays: 1 = Monday … 7 = Sunday.
    pub days: Vec<u8>,
    pub start: PeakClock,
    pub end: PeakClock,
}

impl PeakWindow {
    /// True when `minute` of `day` falls inside this window, honouring the
    /// overnight anchor rule.
    #[must_use]
    fn covers(&self, day: u8, previous_day: u8, minute: u16) -> bool {
        let start = self.start.minutes();
        let end = self.end.minutes();
        if start == end {
            // Zero-length windows are rejected by `validate`; treat a stale row
            // as "never peak" instead of "peak all day".
            return false;
        }
        if start < end {
            return self.days.contains(&day) && (start..end).contains(&minute);
        }
        (self.days.contains(&day) && minute >= start)
            || (self.days.contains(&previous_day) && minute < end)
    }

    /// Week-relative `[start, end)` intervals, one per covered day, each already
    /// wrapped into `0..MINUTES_PER_WEEK`. An overnight window yields two: the
    /// evening half and the next morning's half (the latter is skipped when the
    /// window ends exactly at midnight).
    fn weekly_spans(&self) -> Vec<(u32, u32)> {
        let start = u32::from(self.start.minutes());
        let end = u32::from(self.end.minutes());
        let mut spans = Vec::with_capacity(self.days.len() * 2);
        for &day in &self.days {
            let base = u32::from(day.saturating_sub(1)) * MINUTES_PER_DAY;
            if start < end {
                spans.push((base + start, base + end));
            } else if start > end {
                spans.push((base + start, base + MINUTES_PER_DAY));
                if end > 0 {
                    let next = (base + MINUTES_PER_DAY) % MINUTES_PER_WEEK;
                    spans.push((next, next + end));
                }
            }
        }
        spans
    }

    fn validate(&self, index: usize) -> Result<()> {
        if self.days.is_empty() {
            return Err(CoreError::Validation(format!(
                "pricing_schedule.peak_windows[{index}].days must list at least one ISO weekday (1=Mon..7=Sun)"
            )));
        }
        if self.days.len() > 7 {
            return Err(CoreError::Validation(format!(
                "pricing_schedule.peak_windows[{index}].days must not repeat a weekday (1=Mon..7=Sun)"
            )));
        }
        let mut seen = [false; 8];
        for &day in &self.days {
            if !(1..=7).contains(&day) {
                return Err(CoreError::Validation(format!(
                    "pricing_schedule.peak_windows[{index}].days contains {day}: ISO weekdays are 1=Mon..7=Sun"
                )));
            }
            let slot = usize::from(day);
            if std::mem::replace(&mut seen[slot], true) {
                return Err(CoreError::Validation(format!(
                    "pricing_schedule.peak_windows[{index}].days repeats weekday {day}"
                )));
            }
        }
        if self.start.minutes() == self.end.minutes() {
            return Err(CoreError::Validation(format!(
                "pricing_schedule.peak_windows[{index}] is empty: start and end are both {}",
                self.start
            )));
        }
        Ok(())
    }
}

/// A combo's peak / off-peak tariff.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PricingSchedule {
    /// Always [`SCHEDULE_TIMEZONE`]; windows are UTC.
    pub timezone: String,
    pub peak: PricingRate,
    pub off_peak: PricingRate,
    /// Windows that bill at `peak`. At least one is required: a schedule with no
    /// window is an off-peak-only price, which belongs in the flat base price.
    pub peak_windows: Vec<PeakWindow>,
    /// Dates (`YYYY-MM-DD`, UTC) billed at `off_peak` all day, overriding the
    /// windows. Holidays and upstream promotions.
    #[serde(default)]
    pub off_peak_dates: Vec<OffPeakDate>,
}

impl PricingSchedule {
    /// Reject a schedule that cannot be billed unambiguously.
    ///
    /// Every rate must be finite and non-negative, the timezone must be UTC,
    /// dates must be strict `YYYY-MM-DD`, there must be 1..=`MAX_PEAK_WINDOWS`
    /// non-overlapping windows, and no two windows may share a minute on the
    /// same weekday (adjacent windows are fine).
    pub fn validate(&self) -> Result<()> {
        if self.timezone != SCHEDULE_TIMEZONE {
            return Err(CoreError::Validation(format!(
                "pricing_schedule.timezone must be \"{SCHEDULE_TIMEZONE}\", got {:?}",
                self.timezone
            )));
        }
        self.peak.validate("peak")?;
        self.off_peak.validate("off_peak")?;

        if self.peak_windows.is_empty() {
            return Err(CoreError::Validation(
                "pricing_schedule.peak_windows must contain at least one window".into(),
            ));
        }
        if self.peak_windows.len() > MAX_PEAK_WINDOWS {
            return Err(CoreError::Validation(format!(
                "pricing_schedule.peak_windows holds {} windows; the maximum is {MAX_PEAK_WINDOWS}",
                self.peak_windows.len()
            )));
        }
        for (index, window) in self.peak_windows.iter().enumerate() {
            window.validate(index)?;
        }
        if self.off_peak_dates.len() > MAX_OFF_PEAK_DATES {
            return Err(CoreError::Validation(format!(
                "pricing_schedule.off_peak_dates holds {} dates; the maximum is {MAX_OFF_PEAK_DATES}",
                self.off_peak_dates.len()
            )));
        }

        self.validate_no_overlap()
    }

    /// Reject two windows that share a minute of the same week. Adjacent windows
    /// (`04:00` end, `04:00` start) are allowed; the ranges are half-open.
    ///
    /// The week is walked minute by minute into an ownership table: 10 080
    /// entries against at most [`MAX_PEAK_WINDOWS`] windows, and no
    /// wrap-around arithmetic to get wrong at the Sunday/Monday seam.
    fn validate_no_overlap(&self) -> Result<()> {
        let mut owner: Vec<Option<usize>> = vec![None; MINUTES_PER_WEEK as usize];
        for (index, window) in self.peak_windows.iter().enumerate() {
            for (start, end) in window.weekly_spans() {
                for minute in start..end {
                    let slot = &mut owner[minute as usize];
                    if let Some(previous) = *slot {
                        return Err(CoreError::Validation(format!(
                            "pricing_schedule.peak_windows[{previous}] and pricing_schedule.peak_windows[{index}] overlap"
                        )));
                    }
                    *slot = Some(index);
                }
            }
        }
        Ok(())
    }

    /// True when `at` bills at `peak`.
    #[must_use]
    pub fn is_peak_at(&self, at: DateTime<Utc>) -> bool {
        let date = at.date_naive();
        if self.off_peak_dates.iter().any(|d| d.date() == date) {
            return false;
        }
        let day = iso_weekday(date.weekday());
        let previous_day = if day == 1 { 7 } else { day - 1 };
        let minute = (at.hour() * 60 + at.minute()) as u16;
        self.peak_windows
            .iter()
            .any(|window| window.covers(day, previous_day, minute))
    }

    /// The band that bills `at`.
    #[must_use]
    pub fn rate_at(&self, at: DateTime<Utc>) -> &PricingRate {
        if self.is_peak_at(at) {
            &self.peak
        } else {
            &self.off_peak
        }
    }

    /// Element-wise upper bound of both bands: the rate to reserve credits at
    /// when the request's band is not yet known (or may change before it ends).
    #[must_use]
    pub fn conservative_rate(&self) -> PricingRate {
        self.peak.upper_bound(&self.off_peak)
    }
}

const fn iso_weekday(weekday: Weekday) -> u8 {
    weekday.number_from_monday() as u8
}

#[cfg(test)]
mod tests;
