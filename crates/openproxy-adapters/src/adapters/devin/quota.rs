//! Devin `GetUserStatus` quota fetch + parse.
//!
//! `POST /exa.seat_management_pb.SeatManagementService/GetUserStatus` speaks
//! bare protobuf (`Content-Type: application/proto`, no Connect envelope, no
//! sentry-trace), carrying the same ClientMetadata field-1 blob as chat.

use std::sync::Arc;

use openproxy_types::{AccountQuota, CoreError, ModelQuotaDetail, Result};

use super::catalog::{self, DevinLiveModelRow};
use super::protoc::{self, ProtoReader};
use super::wire;
use crate::adapters::{CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest};

/// Which quota window an upstream exhaustion message refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevinQuotaKind {
    Daily,
    Weekly,
}

/// Parsed `GetUserStatus` response (response field 1 → `user_status`).
#[derive(Debug, Clone, Default)]
pub struct DevinUserStatus {
    pub user_name: String,
    pub team_id: String,
    pub email: String,
    pub user_id: String,
    pub plan_name: String,
    pub org_id: String,
    pub org_name: String,
    pub plan_start_secs: Option<i64>,
    pub plan_end_secs: Option<i64>,
    /// Remaining daily quota as a percentage (0..=100).
    pub daily_remaining_pct: Option<i64>,
    /// Remaining weekly quota as a percentage (0..=100).
    pub weekly_remaining_pct: Option<i64>,
    pub daily_reset_unix: Option<i64>,
    pub weekly_reset_unix: Option<i64>,
    /// `cascade_model_config_data.client_model_configs` — the live model
    /// catalog the account can serve (field 33). Empty when the upstream
    /// omits the blob.
    pub model_configs: Vec<DevinLiveModelRow>,
}

fn reader_str(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Percent/seconds fields arrive as varint in practice; accept fixed64
/// doubles as well so an upstream wire-type change can't zero out the quota.
fn read_int_like(reader: &mut ProtoReader<'_>, wire: u8) -> Option<i64> {
    match wire {
        protoc::WIRE_VARINT => reader.varint().map(|v| v as i64),
        protoc::WIRE_64BIT => reader
            .fixed64_value()
            .map(|bits| f64::from_bits(bits) as i64),
        _ => reader.skip(wire).map(|()| 0),
    }
}

/// plan_status (field 13): plan identity, period bounds, remaining
/// percentages and reset timestamps.
fn parse_plan_status(payload: &[u8], us: &mut DevinUserStatus) -> Option<()> {
    let mut reader = ProtoReader::new(payload);
    while let Some((field, wire)) = reader.tag() {
        match (field, wire) {
            (1, protoc::WIRE_LEN) => {
                let plan = reader.bytes_value()?;
                parse_plan_identity(plan, us);
            }
            (2, protoc::WIRE_LEN) => {
                if let Some(secs) = read_timestamp_seconds(reader.bytes_value()?) {
                    us.plan_start_secs = Some(secs);
                }
            }
            (3, protoc::WIRE_LEN) => {
                if let Some(secs) = read_timestamp_seconds(reader.bytes_value()?) {
                    us.plan_end_secs = Some(secs);
                }
            }
            (14, _) => us.daily_remaining_pct = read_int_like(&mut reader, wire),
            (15, _) => us.weekly_remaining_pct = read_int_like(&mut reader, wire),
            (17, _) => us.daily_reset_unix = read_int_like(&mut reader, wire),
            (18, _) => us.weekly_reset_unix = read_int_like(&mut reader, wire),
            _ => reader.skip(wire)?,
        }
    }
    Some(())
}

/// plan message `{2: plan_name, 33: org{4: org_id, 8: org_name}}`.
fn parse_plan_identity(plan: &[u8], us: &mut DevinUserStatus) {
    let mut reader = ProtoReader::new(plan);
    while let Some((field, wire)) = reader.tag() {
        match (field, wire) {
            (2, protoc::WIRE_LEN) => {
                if let Some(bytes) = reader.bytes_value() {
                    us.plan_name = reader_str(bytes);
                }
            }
            (33, protoc::WIRE_LEN) => {
                if let Some(org) = reader.bytes_value() {
                    parse_org(org, us);
                }
            }
            _ => {
                if reader.skip(wire).is_none() {
                    return;
                }
            }
        }
    }
}

fn parse_org(org: &[u8], us: &mut DevinUserStatus) {
    let mut reader = ProtoReader::new(org);
    while let Some((field, wire)) = reader.tag() {
        match (field, wire) {
            (4, protoc::WIRE_LEN) => {
                if let Some(bytes) = reader.bytes_value() {
                    us.org_id = reader_str(bytes);
                }
            }
            (8, protoc::WIRE_LEN) => {
                if let Some(bytes) = reader.bytes_value() {
                    us.org_name = reader_str(bytes);
                }
            }
            _ => {
                if reader.skip(wire).is_none() {
                    return;
                }
            }
        }
    }
}

/// A `google.protobuf.Timestamp`-like `{1: seconds}` wrapper.
fn read_timestamp_seconds(payload: &[u8]) -> Option<i64> {
    let mut reader = ProtoReader::new(payload);
    while let Some((field, wire)) = reader.tag() {
        match (field, wire) {
            (1, protoc::WIRE_VARINT) => return reader.varint().map(|v| v as i64),
            _ => reader.skip(wire)?,
        }
    }
    None
}

fn parse_user_status_field(payload: &[u8], us: &mut DevinUserStatus) -> Option<()> {
    let mut reader = ProtoReader::new(payload);
    while let Some((field, wire)) = reader.tag() {
        match (field, wire) {
            (3, protoc::WIRE_LEN) => us.user_name = reader_str(reader.bytes_value()?),
            (5, protoc::WIRE_LEN) => us.team_id = reader_str(reader.bytes_value()?),
            (7, protoc::WIRE_LEN) => us.email = reader_str(reader.bytes_value()?),
            (13, protoc::WIRE_LEN) => {
                let plan = reader.bytes_value()?;
                parse_plan_status(plan, us);
            }
            (33, protoc::WIRE_LEN) => {
                us.model_configs = catalog::parse_client_model_configs(reader.bytes_value()?);
            }
            (36, protoc::WIRE_LEN) => us.user_id = reader_str(reader.bytes_value()?),
            _ => reader.skip(wire)?,
        }
    }
    Some(())
}

/// Parse a raw `GetUserStatus` response body.
pub fn parse_user_status(body: &[u8]) -> Option<DevinUserStatus> {
    let mut us = DevinUserStatus::default();
    let mut reader = ProtoReader::new(body);
    while let Some((field, wire)) = reader.tag() {
        match (field, wire) {
            (1, protoc::WIRE_LEN) => {
                let payload = reader.bytes_value()?;
                parse_user_status_field(payload, &mut us);
            }
            _ => reader.skip(wire)?,
        }
    }
    Some(us)
}

fn unix_to_rfc3339(secs: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(secs, 0).map(|dt| dt.to_rfc3339())
}

fn window(remaining_pct: Option<i64>) -> (Option<i64>, Option<i64>, f64) {
    remaining_pct.map_or((None, None, 0.0), |pct| {
        let pct = pct.clamp(0, 100);
        (Some(100 - pct), Some(100), pct as f64 / 100.0)
    })
}

/// Map a parsed user status onto the account quota shape. Devin reports
/// *remaining* percentages: the session window is the daily one, the weekly
/// window carries `weekly_*`.
pub fn to_account_quota(us: &DevinUserStatus) -> AccountQuota {
    let (session_used, session_limit, daily_frac) = window(us.daily_remaining_pct);
    let (weekly_used, weekly_limit, weekly_frac) = window(us.weekly_remaining_pct);
    let session_reset_at = us.daily_reset_unix.and_then(unix_to_rfc3339);
    let weekly_reset_at = us.weekly_reset_unix.and_then(unix_to_rfc3339);

    let mut model_details = Vec::new();
    if us.daily_remaining_pct.is_some() {
        model_details.push(ModelQuotaDetail {
            model_id: "devin".to_string(),
            session_used: session_used.unwrap_or(0),
            session_limit: 100,
            session_reset_at: session_reset_at.clone(),
            remaining_fraction: daily_frac,
        });
    }
    if us.weekly_remaining_pct.is_some() {
        model_details.push(ModelQuotaDetail {
            model_id: "devin".to_string(),
            session_used: weekly_used.unwrap_or(0),
            session_limit: 100,
            session_reset_at: weekly_reset_at.clone(),
            remaining_fraction: weekly_frac,
        });
    }

    // Free models (free cost tier or active promo) never draw from the paid
    // pool, so they get a full-fraction detail of their own. Without it, an
    // exhausted account window would still gate them out in
    // `check_quota_windows_exhausted` — the account `devin` details only
    // cover models that actually bill. Named `{base}:free` so both the
    // listed model id and the bare base match `find_model_quota_detail`.
    for row in &us.model_configs {
        if !row.is_free() {
            continue;
        }
        let base = catalog::normalize_base_id(&row.base);
        if model_details
            .iter()
            .any(|d| d.model_id == format!("{base}:free") || d.model_id == base)
        {
            continue;
        }
        model_details.push(ModelQuotaDetail {
            model_id: format!("{base}:free"),
            session_used: 0,
            session_limit: 9_999,
            session_reset_at: session_reset_at.clone(),
            remaining_fraction: 1.0,
        });
    }

    let plan_name = if us.plan_name.trim().is_empty() {
        None
    } else {
        Some(us.plan_name.clone())
    };

    AccountQuota {
        session_used,
        session_limit,
        session_reset_at,
        weekly_used,
        weekly_limit,
        weekly_reset_at,
        plan_name,
        last_fetched_at: openproxy_types::now_unix_secs_str(),
        fetch_error: None,
        pools: None,
        model_details: if model_details.is_empty() {
            None
        } else {
            Some(model_details.into_boxed_slice())
        },
    }
}

/// Classify an upstream exhaustion message: must mention "quota" and
/// "exhausted" (case-insensitive); "weekly" wins over "daily" when both show.
pub fn classify_devin_quota_exhaustion(msg: &str) -> Option<DevinQuotaKind> {
    let lower = msg.to_ascii_lowercase();
    if !(lower.contains("quota") && lower.contains("exhausted")) {
        return None;
    }
    if lower.contains("weekly") {
        return Some(DevinQuotaKind::Weekly);
    }
    if lower.contains("daily") {
        return Some(DevinQuotaKind::Daily);
    }
    None
}

/// `POST {base}/exa.seat_management_pb.SeatManagementService/GetUserStatus`,
/// returning the raw parsed status (profile fields included).
pub async fn fetch_devin_user_status(
    upstream: &Arc<UpstreamClient>,
    base_url: &str,
    api_key: &str,
    device_seed: &str,
    proxy_url: Option<&str>,
) -> Result<DevinUserStatus> {
    let token = api_key.trim();
    if token.is_empty() {
        return Err(CoreError::Validation(
            "devin: missing session token for status fetch".into(),
        ));
    }
    let url = format!(
        "{}{}",
        base_url.trim_end_matches('/'),
        super::DEVIN_USER_STATUS_PATH
    );

    let mut req =
        UpstreamRequest::post_json(url, wire::build_user_status_request(token, device_seed));
    let auth = http::HeaderValue::from_str(&wire::basic_auth(token))
        .map_err(|e| CoreError::Validation(format!("devin: invalid auth header: {e}")))?;
    req.headers.insert(http::header::AUTHORIZATION, auth);
    req.headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/proto"),
    );
    req.headers.insert(
        http::HeaderName::from_static("connect-protocol-version"),
        http::HeaderValue::from_static("1"),
    );
    req.headers
        .insert(http::header::ACCEPT, http::HeaderValue::from_static("*/*"));
    req.proxy = proxy_url.map(ToString::to_string);

    let resp = upstream
        .call(req, TimeoutProfile::Quota, CancellationToken::new())
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("devin user status: {e}")))?;

    let status = resp.status;
    let body = resp
        .collect()
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("devin user status body: {e}")))?;

    if !status.is_success() {
        let body_str = String::from_utf8_lossy(&body).to_string();
        return Err(CoreError::upstream_error(
            status.as_u16(),
            "devin",
            "<user-status>",
            body_str,
            false,
        ));
    }

    let us = parse_user_status(&body)
        .ok_or_else(|| CoreError::Parse("devin user status: parse response".into()))?;
    // Refresh the live model catalog snapshot: every successful
    // GetUserStatus carries the full cascade config blob, so quota fetches
    // and the OAuth post-exchange hook both keep it warm.
    catalog::store_live_catalog(us.model_configs.clone());
    Ok(us)
}

/// `POST {base}/exa.seat_management_pb.SeatManagementService/GetUserStatus`.
pub async fn fetch_devin_quota(
    upstream: &Arc<UpstreamClient>,
    base_url: &str,
    api_key: &str,
    device_seed: &str,
    proxy_url: Option<&str>,
) -> Result<AccountQuota> {
    let us = fetch_devin_user_status(upstream, base_url, api_key, device_seed, proxy_url).await?;
    Ok(to_account_quota(&us))
}
