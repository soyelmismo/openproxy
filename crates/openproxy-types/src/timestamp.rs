use chrono::{DateTime, NaiveDateTime, Utc};

pub fn parse_timestamp(s: &str) -> std::result::Result<DateTime<Utc>, chrono::ParseError> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }

    // SQLite default datetime() format
    if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Ok(ndt.and_utc());
    }

    // ISO-8601 without timezone
    if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Ok(ndt.and_utc());
    }

    // Try passing back the original error for rfc3339
    DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_timestamp() {
        assert!(parse_timestamp("2025-06-18T12:00:00Z").is_ok());
        assert!(parse_timestamp("2025-06-18 12:00:00").is_ok());
        assert!(parse_timestamp("2025-06-18T12:00:00").is_ok());
        assert!(parse_timestamp("not-a-date").is_err());
    }

    #[test]
    fn test_parse_timestamp_formats_and_offsets() {
        let parsed_offset = parse_timestamp("2025-06-18T14:00:00+02:00")
            .expect("should parse RFC3339 with timezone offset");
        assert_eq!(parsed_offset.to_rfc3339(), "2025-06-18T12:00:00+00:00");

        let parsed_fractional = parse_timestamp("2025-06-18T12:00:00.123456Z")
            .expect("should parse RFC3339 with fractional seconds");
        assert_eq!(parsed_fractional.timestamp(), 1750248000);

        let parsed_sqlite =
            parse_timestamp("2025-06-18 12:00:00").expect("should parse SQLite datetime string");
        assert_eq!(parsed_sqlite.timestamp(), 1750248000);

        let parsed_iso_no_tz =
            parse_timestamp("2025-06-18T12:00:00").expect("should parse ISO-8601 without timezone");
        assert_eq!(parsed_iso_no_tz.timestamp(), 1750248000);
    }
}
