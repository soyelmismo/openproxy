use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelQuotaDetail {
    pub model_id: String,
    pub session_used: i64,
    pub session_limit: i64,
    pub session_reset_at: Option<String>,
    pub remaining_fraction: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountQuota {
    pub session_used: Option<i64>,
    pub session_limit: Option<i64>,
    pub session_reset_at: Option<String>,
    pub weekly_used: Option<i64>,
    pub weekly_limit: Option<i64>,
    pub weekly_reset_at: Option<String>,
    pub plan_name: Option<String>,
    pub last_fetched_at: String,
    pub fetch_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_details: Option<Box<[ModelQuotaDetail]>>,
}

impl AccountQuota {
    pub fn is_empty(&self) -> bool {
        self.session_used.is_none() && self.weekly_used.is_none() && self.fetch_error.is_none()
    }
}

pub fn now_unix_secs_str() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    secs.to_string()
}
fn parse_duration_unit(unit: u8, val: f64) -> f64 {
    match unit {
        b'h' => val * 3600.0,
        b'm' => val * 60.0,
        b's' => val,
        _ => 0.0,
    }
}

fn parse_compound_duration(s: &str) -> Option<u64> {
    let mut total_secs = 0.0;
    let mut num_range: Option<(usize, usize)> = None;
    for (i, b) in s.bytes().enumerate() {
        if b.is_ascii_digit() || b == b'.' {
            num_range = Some((num_range.map_or(i, |(start, _)| start), i + 1));
        } else if matches!(b, b'h' | b'm' | b's')
            && let Some((start, end)) = num_range.take()
        {
            let val = s[start..end].parse::<f64>().unwrap_or(0.0);
            total_secs += parse_duration_unit(b, val);
        }
    }
    let total = total_secs.ceil() as u64;
    if total > 0 { Some(total) } else { None }
}

pub fn parse_reset_time(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Ok(secs) = s.parse::<u64>() {
        return Some(secs);
    }
    if let Ok(secs_f) = s.parse::<f64>() {
        return Some(secs_f.ceil() as u64);
    }
    parse_compound_duration(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_account_quota_is_empty() {
        let mut quota = AccountQuota {
            session_used: None,
            session_limit: None,
            session_reset_at: None,
            weekly_used: None,
            weekly_limit: None,
            weekly_reset_at: None,
            plan_name: None,
            last_fetched_at: String::new(),
            fetch_error: None,
            model_details: None,
        };

        assert!(quota.is_empty());

        quota.session_used = Some(10);
        assert!(!quota.is_empty());
        quota.session_used = None;

        quota.weekly_used = Some(10);
        assert!(!quota.is_empty());
        quota.weekly_used = None;

        quota.fetch_error = Some("error".to_string());
        assert!(!quota.is_empty());
    }

    #[test]
    fn test_parse_reset_time() {
        assert_eq!(parse_reset_time("60"), Some(60));
        assert_eq!(parse_reset_time("60.5"), Some(61));
        assert_eq!(parse_reset_time("1h"), Some(3600));
        assert_eq!(parse_reset_time("1.5h"), Some(5400));
        assert_eq!(parse_reset_time("2m"), Some(120));
        assert_eq!(parse_reset_time("2.5m"), Some(150));
        assert_eq!(parse_reset_time("30s"), Some(30));
        assert_eq!(parse_reset_time("30.2s"), Some(31));
        assert_eq!(parse_reset_time("1h30m"), Some(5400));
        assert_eq!(parse_reset_time("1h 30m"), Some(5400));
        assert_eq!(parse_reset_time("invalid"), None);
        assert_eq!(parse_reset_time(""), None);
    }
}
