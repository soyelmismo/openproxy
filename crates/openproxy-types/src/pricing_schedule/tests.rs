//! Boundary tests for the peak / off-peak resolver.

use super::*;
use chrono::TimeZone as _;

/// The schedule from the API contract: two Mon–Fri windows, UTC.
const SAMPLE_JSON: &str = r#"{
    "timezone": "UTC",
    "peak": { "input_per_1m": 6.0, "output_per_1m": 24.0, "cached_per_1m": 3.0 },
    "off_peak": { "input_per_1m": 2.0, "output_per_1m": 8.0, "cached_per_1m": null },
    "peak_windows": [
        { "days": [1, 2, 3, 4, 5], "start": "01:00", "end": "04:00" },
        { "days": [1, 2, 3, 4, 5], "start": "06:00", "end": "10:00" }
    ],
    "off_peak_dates": ["2026-11-18"]
}"#;

fn sample() -> PricingSchedule {
    serde_json::from_str(SAMPLE_JSON).expect("sample schedule parses")
}

fn at(text: &str) -> DateTime<Utc> {
    chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S")
        .expect("test instant")
        .and_utc()
}

fn schedule_with(windows: &str, extra_dates: &str) -> PricingSchedule {
    let json = format!(
        r#"{{"timezone":"UTC",
            "peak":{{"input_per_1m":6.0,"output_per_1m":24.0}},
            "off_peak":{{"input_per_1m":2.0,"output_per_1m":8.0}},
            "peak_windows":{windows},
            "off_peak_dates":{extra_dates}}}"#
    );
    serde_json::from_str(&json).expect("schedule parses")
}

fn window(days: &str, start: &str, end: &str) -> String {
    format!(r#"{{"days":{days},"start":"{start}","end":"{end}"}}"#)
}

#[test]
fn peak_clock_parses_only_strict_hh_mm() {
    assert_eq!(PeakClock::parse("00:00").unwrap().minutes(), 0);
    assert_eq!(PeakClock::parse("01:00").unwrap().minutes(), 60);
    assert_eq!(PeakClock::parse("23:59").unwrap().minutes(), 23 * 60 + 59);
    for bad in [
        "1:00", "01:0", "0100", "24:00", "12:60", "ab:cd", "", " 01:00",
    ] {
        assert!(PeakClock::parse(bad).is_err(), "{bad:?} must be rejected");
    }
    assert_eq!(PeakClock::parse("06:05").unwrap().to_string(), "06:05");
}

#[test]
fn off_peak_date_parses_only_strict_yyyy_mm_dd() {
    assert_eq!(
        OffPeakDate::parse("2026-11-18").unwrap().date(),
        NaiveDate::from_ymd_opt(2026, 11, 18).unwrap()
    );
    for bad in [
        "2026-11-8",
        "2026-1-08",
        "20261118",
        "2026-02-30",
        "18-11-2026",
    ] {
        assert!(OffPeakDate::parse(bad).is_err(), "{bad:?} must be rejected");
    }
    assert_eq!(
        OffPeakDate::parse("2027-01-01").unwrap().to_string(),
        "2027-01-01"
    );
}

#[test]
fn json_round_trip_preserves_the_contract() {
    let schedule = sample();
    let text = serde_json::to_string(&schedule).expect("serialize");
    let parsed: PricingSchedule = serde_json::from_str(&text).expect("deserialize");
    assert_eq!(parsed, schedule);
    assert!(text.contains(r#""start":"01:00""#), "{text}");
    // A present cached rate survives; an absent one is omitted (never `null`) on
    // the way out, matching the rest of the pricing wire shape. `null` is still
    // accepted on input.
    let wire: serde_json::Value = serde_json::from_str(&text).expect("wire json");
    assert_eq!(
        wire["peak"]["cached_per_1m"],
        serde_json::json!(3.0),
        "{text}"
    );
    assert!(
        wire["off_peak"].get("cached_per_1m").is_none(),
        "an absent cached rate must be omitted, not null: {text}"
    );
    assert!(
        text.contains(r#""off_peak_dates":["2026-11-18"]"#),
        "{text}"
    );

    // Both spellings of "no cached rate" deserialize to the same value: the
    // serialized text omits the key, the sample spells it as `null`.
    let omitted: PricingSchedule = serde_json::from_str(&text).expect("omitted");
    let explicit_null: PricingSchedule = serde_json::from_str(SAMPLE_JSON).expect("explicit null");
    assert_eq!(omitted.off_peak.cached_per_1m, None);
    assert_eq!(explicit_null.off_peak.cached_per_1m, None);
    assert_eq!(omitted, explicit_null);
}

#[test]
fn unknown_fields_are_rejected() {
    let json = SAMPLE_JSON.replace(r#""timezone": "UTC""#, r#""timezone": "UTC", "extra": 1"#);
    assert!(serde_json::from_str::<PricingSchedule>(&json).is_err());
    let json = SAMPLE_JSON.replace(r#""end": "04:00""#, r#""end": "04:00", "tz": "UTC""#);
    assert!(serde_json::from_str::<PricingSchedule>(&json).is_err());
}

#[test]
fn window_boundaries_are_half_open() {
    let schedule = sample();
    // Monday 2026-11-16.
    assert!(!schedule.is_peak_at(at("2026-11-16T00:59:59")));
    assert!(schedule.is_peak_at(at("2026-11-16T01:00:00")));
    assert!(schedule.is_peak_at(at("2026-11-16T03:59:59")));
    assert!(!schedule.is_peak_at(at("2026-11-16T04:00:00")));
    assert!(!schedule.is_peak_at(at("2026-11-16T05:59:59")));
    assert!(schedule.is_peak_at(at("2026-11-16T06:00:00")));
    assert!(schedule.is_peak_at(at("2026-11-16T09:59:59")));
    assert!(!schedule.is_peak_at(at("2026-11-16T10:00:00")));
    assert!(!schedule.is_peak_at(at("2026-11-16T12:00:00")));
    assert!(!schedule.is_peak_at(at("2026-11-16T23:59:59")));
}

#[test]
fn weekend_and_explicit_dates_resolve_off_peak() {
    let schedule = sample();
    // Saturday / Sunday are outside the Mon–Fri window list.
    assert!(!schedule.is_peak_at(at("2026-11-21T02:00:00")));
    assert!(!schedule.is_peak_at(at("2026-11-22T07:00:00")));
    // Explicit off-peak date overrides an active window (Wednesday 02:00).
    assert!(schedule.is_peak_at(at("2026-11-17T02:00:00")));
    assert!(!schedule.is_peak_at(at("2026-11-18T02:00:00")));
    // ... and the whole day, not just the window.
    assert!(!schedule.is_peak_at(at("2026-11-18T08:00:00")));
    // The next day bills normally again.
    assert!(schedule.is_peak_at(at("2026-11-19T08:00:00")));
}

#[test]
fn band_selection_returns_the_matching_rate() {
    let schedule = sample();
    let peak = schedule.rate_at(at("2026-11-16T02:00:00"));
    assert_eq!(peak.input_per_1m, 6.0);
    assert_eq!(peak.output_per_1m, 24.0);
    assert_eq!(peak.cached_per_1m, Some(3.0));

    let off = schedule.rate_at(at("2026-11-16T05:00:00"));
    assert_eq!(off.input_per_1m, 2.0);
    assert_eq!(off.output_per_1m, 8.0);
    assert_eq!(off.cached_per_1m, None);
}

#[test]
fn overnight_window_is_anchored_at_its_start_day() {
    let schedule = schedule_with(&format!("[{}]", window("[1]", "22:00", "02:00")), "[]");
    assert!(
        schedule.is_peak_at(at("2026-11-16T22:00:00")),
        "Monday 22:00"
    );
    assert!(schedule.is_peak_at(at("2026-11-16T23:59:59")));
    assert!(
        schedule.is_peak_at(at("2026-11-17T00:00:00")),
        "Tuesday 00:00 belongs to Monday's window"
    );
    assert!(schedule.is_peak_at(at("2026-11-17T01:59:59")));
    assert!(
        !schedule.is_peak_at(at("2026-11-17T02:00:00")),
        "the morning tail ends at 02:00"
    );
    assert!(
        !schedule.is_peak_at(at("2026-11-17T22:00:00")),
        "Tuesday has no window of its own"
    );
    assert!(!schedule.is_peak_at(at("2026-11-16T21:59:59")));
}

#[test]
fn overnight_window_crosses_month_and_year() {
    // Thursday 2026-12-31 22:00 -> Friday 2027-01-01 02:00.
    let schedule = schedule_with(&format!("[{}]", window("[4]", "22:00", "02:00")), "[]");
    assert!(schedule.is_peak_at(at("2026-12-31T23:30:00")));
    assert!(schedule.is_peak_at(at("2027-01-01T01:30:00")));
    assert!(!schedule.is_peak_at(at("2027-01-01T02:00:00")));
    assert!(!schedule.is_peak_at(at("2026-12-30T23:30:00")));
}

#[test]
fn sunday_evening_wrap_meets_monday_morning() {
    // Sunday 22:00-02:00 wraps into Monday 00:00-02:00.
    let schedule = schedule_with(&format!("[{}]", window("[7]", "22:00", "02:00")), "[]");
    assert!(schedule.is_peak_at(at("2026-11-22T23:00:00")));
    assert!(schedule.is_peak_at(at("2026-11-23T01:00:00")));
    assert!(!schedule.is_peak_at(at("2026-11-23T02:00:00")));
}

#[test]
fn window_ending_at_midnight_covers_only_the_evening() {
    let schedule = schedule_with(&format!("[{}]", window("[1]", "22:00", "00:00")), "[]");
    assert!(schedule.is_peak_at(at("2026-11-16T22:00:00")));
    assert!(schedule.is_peak_at(at("2026-11-16T23:59:59")));
    assert!(!schedule.is_peak_at(at("2026-11-17T00:00:00")));
}

#[test]
fn adjacent_windows_are_allowed() {
    let schedule = schedule_with(
        &format!(
            "[{},{}]",
            window("[1]", "01:00", "04:00"),
            window("[1]", "04:00", "06:00")
        ),
        "[]",
    );
    assert!(schedule.is_peak_at(at("2026-11-16T03:59:59")));
    assert!(schedule.is_peak_at(at("2026-11-16T04:00:00")));
    assert!(!schedule.is_peak_at(at("2026-11-16T06:00:00")));
}

#[test]
fn overlapping_windows_are_rejected() {
    let schedule = schedule_with(
        &format!(
            "[{},{}]",
            window("[1,2]", "01:00", "04:00"),
            window("[2]", "03:00", "05:00")
        ),
        "[]",
    );
    let err = schedule.validate().unwrap_err();
    assert!(
        matches!(err, CoreError::Validation(ref m) if m.contains("overlap")),
        "{err:?}"
    );
}

#[test]
fn overlap_is_detected_across_the_overnight_tail() {
    // Monday 22:00-02:00 already owns Tuesday 00:00-02:00.
    let schedule = schedule_with(
        &format!(
            "[{},{}]",
            window("[1]", "22:00", "02:00"),
            window("[2]", "01:00", "03:00")
        ),
        "[]",
    );
    assert!(schedule.validate().is_err());

    // The Sunday-evening tail collides with Monday 00:30.
    let schedule = schedule_with(
        &format!(
            "[{},{}]",
            window("[7]", "22:00", "02:00"),
            window("[1]", "00:30", "01:30")
        ),
        "[]",
    );
    assert!(schedule.validate().is_err());

    // ... but Monday 02:00 onwards is free.
    let schedule = schedule_with(
        &format!(
            "[{},{}]",
            window("[7]", "22:00", "02:00"),
            window("[1]", "02:00", "03:00")
        ),
        "[]",
    );
    schedule.validate().expect("no overlap");
}

#[test]
fn validation_rejects_unbillable_schedules() {
    let mut schedule = sample();
    schedule.timezone = "Europe/Madrid".into();
    assert!(schedule.validate().is_err(), "timezone must be UTC");

    let mut schedule = sample();
    schedule.timezone = "utc".into();
    assert!(schedule.validate().is_err(), "timezone is case sensitive");

    let mut schedule = sample();
    schedule.peak.input_per_1m = -0.01;
    assert!(schedule.validate().is_err());

    let mut schedule = sample();
    schedule.off_peak.output_per_1m = f64::NAN;
    assert!(schedule.validate().is_err());

    let mut schedule = sample();
    schedule.off_peak.cached_per_1m = Some(f64::INFINITY);
    assert!(schedule.validate().is_err());

    let mut schedule = sample();
    schedule.peak_windows.clear();
    assert!(schedule.validate().is_err(), "at least one window");

    let mut schedule = sample();
    schedule.peak_windows[0].days.clear();
    assert!(schedule.validate().is_err());

    let mut schedule = sample();
    schedule.peak_windows[0].days = vec![0, 1];
    assert!(schedule.validate().is_err(), "weekdays start at 1");

    let mut schedule = sample();
    schedule.peak_windows[0].days = vec![1, 8];
    assert!(schedule.validate().is_err(), "weekdays end at 7");

    let mut schedule = sample();
    schedule.peak_windows[0].days = vec![1, 1];
    assert!(schedule.validate().is_err(), "duplicate weekday");

    let mut schedule = sample();
    schedule.peak_windows[0].end = schedule.peak_windows[0].start;
    assert!(schedule.validate().is_err(), "zero-length window");

    // Every valid combination above must still pass.
    sample().validate().expect("sample is valid");
}

#[test]
fn window_and_date_counts_are_bounded() {
    // i % 24 keeps the generated clocks in 00:00..23:59; the point of the test
    // is the COUNT bound, not the clock values.
    let windows: Vec<String> = (0..=MAX_PEAK_WINDOWS)
        .map(|i| window("[1]", &format!("{:02}:00", i % 24), &format!("{:02}:30", i % 24)))
        .collect();
    let too_many = schedule_with(&format!("[{}]", windows.join(",")), "[]");
    assert!(too_many.validate().is_err());

    let mut dates: Vec<String> = Vec::new();
    for day in 0..=MAX_OFF_PEAK_DATES {
        let date =
            NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + chrono::Duration::days(day as i64);
        dates.push(format!("\"{}\"", OffPeakDate(date)));
    }
    let too_many_dates = schedule_with(
        &format!("[{}]", window("[1]", "01:00", "02:00")),
        &format!("[{}]", dates.join(",")),
    );
    assert!(too_many_dates.validate().is_err());
}

#[test]
fn conservative_rate_is_the_elementwise_maximum() {
    let schedule = sample();
    let bound = schedule.conservative_rate();
    assert_eq!(bound.input_per_1m, 6.0);
    assert_eq!(bound.output_per_1m, 24.0);
    // Peak bills cache reads at 3.0, off-peak at its 2.0 input rate: 3.0 wins.
    assert_eq!(bound.cached_per_1m, Some(3.0));

    // With no explicit cached rate anywhere, the bound keeps the cache at the
    // input rate instead of inventing a discount.
    let schedule = schedule_with(&format!("[{}]", window("[1]", "01:00", "02:00")), "[]");
    assert_eq!(schedule.conservative_rate().cached_per_1m, None);

    // A cheaper peak than off-peak (an inverted tariff) still bounds at the max.
    let inverted = schedule_with(&format!("[{}]", window("[1]", "01:00", "02:00")), "[]");
    let mut inverted = inverted;
    inverted.peak = PricingRate {
        input_per_1m: 1.0,
        output_per_1m: 1.0,
        cached_per_1m: Some(0.5),
    };
    inverted.off_peak = PricingRate {
        input_per_1m: 5.0,
        output_per_1m: 9.0,
        cached_per_1m: None,
    };
    let bound = inverted.conservative_rate();
    assert_eq!(bound.input_per_1m, 5.0);
    assert_eq!(bound.output_per_1m, 9.0);
    assert_eq!(bound.cached_per_1m, Some(5.0));
}

#[test]
fn resolver_matches_the_wall_clock_band_for_a_known_instant() {
    let schedule = sample();
    let instant = Utc
        .with_ymd_and_hms(2026, 11, 20, 7, 30, 0)
        .single()
        .expect("instant");
    assert!(
        schedule.is_peak_at(instant),
        "Friday 07:30 is inside 06:00-10:00"
    );
}
