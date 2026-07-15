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
    pub model_details: Option<Vec<ModelQuotaDetail>>,
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
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}
pub fn parse_reset_time(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Ok(secs) = s.parse::<u64>() {
        return Some(secs);
    }
    if let Ok(secs_f) = s.parse::<f64>() {
        return Some(secs_f.ceil() as u64);
    }
    let mut total_secs = 0.0;
    let mut num_str = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            num_str.push(c);
        } else if matches!(c, 'h' | 'm' | 's') {
            let val = num_str.parse::<f64>().unwrap_or(0.0);
            match c {
                'h' => total_secs += val * 3600.0,
                'm' => total_secs += val * 60.0,
                's' => total_secs += val,
                _ => {}
            }
            num_str.clear();
        }
    }
    let total = total_secs.ceil() as u64;
    if total > 0 { Some(total) } else { None }
}
