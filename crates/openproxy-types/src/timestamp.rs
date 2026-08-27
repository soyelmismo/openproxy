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
}
