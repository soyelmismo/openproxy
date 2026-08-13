use super::*;
use axum::{Json, extract::State};

pub async fn admin_health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

pub async fn get_runtime_config(
    State(s): State<AppState>,
    _auth: crate::extractors::AdminAuth,
) -> Result<Json<RuntimeConfigResponse>, ApiError> {
    let cfg = s.config();
    Ok(Json(RuntimeConfigResponse {
        timeouts: s.timeouts(),
        retries: cfg.retries,
        circuit_breaker: cfg.circuit_breaker,
        racing: cfg.racing.clone(),
        recording_ttl_secs: s.recording_ttl_secs(),
        compression: s.compression_mode(),
        idle_chunk_retryable: s.idle_chunk_retryable(),
        quota_protection: s.quota_protection(),
    }))
}

pub async fn put_runtime_timeouts(
    State(s): State<AppState>,
    _auth: crate::extractors::AdminAuth,
    Json(body): Json<TimeoutsConfig>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let inner: Result<Json<serde_json::Value>, ApiError> = async {
        // 1. Persist to DB first. The UPSERT is atomic in SQLite.
        //    We let the application timestamp it (rather than relying
        //    on `strftime('%s','now')`) so the value matches what
        //    `load_timeouts_override_from_db` expects on the next
        //    boot: an `i64` unix seconds.
        {
            let w = s.db_pool().writer();
            let now = chrono::Utc::now().timestamp();
            core_db::app_config::save_timeouts_to_db(&w, &body, now)?;
        }
        // 2. Update the in-memory slot. Readers see the new value
        //    as soon as this returns. Note: requests already in
        //    flight captured a `Copy` of the old value into their
        //    PipelineConfig and are unaffected.
        s.set_timeouts(body);
        Ok(Json(serde_json::json!({
            "connect_ms": body.connect_ms,
            "request_send_ms": body.request_send_ms,
            "ttft_ms": body.ttft_ms,
            "idle_chunk_ms": body.idle_chunk_ms,
            "total_ms": body.total_ms,
            "applies_to": "next_requests",
        })))
    }
    .await;
    inner
}

pub async fn put_runtime_compression(
    State(s): State<AppState>,
    _auth: crate::extractors::AdminAuth,
    Json(body): Json<openproxy_compression::CompressionMode>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let inner: Result<Json<serde_json::Value>, ApiError> = async {
        {
            let w = s.db_pool().writer();
            let now = chrono::Utc::now().timestamp();
            core_db::app_config::save_compression_to_db(&w, &body, now)?;
        }
        s.set_compression_mode(body);
        Ok(Json(serde_json::json!({
            "mode": body,
            "applies_to": "next_requests",
        })))
    }
    .await;
    inner
}

pub async fn put_idle_chunk_retryable(
    State(s): State<AppState>,
    _auth: crate::extractors::AdminAuth,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let inner: Result<Json<serde_json::Value>, ApiError> = async {
        let val = body
            .get("idle_chunk_retryable")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| {
                ApiError(CoreError::Validation(
                    "idle_chunk_retryable must be a boolean".into(),
                ))
            })?;
        {
            let w = s.db_pool().writer();
            let now = chrono::Utc::now().timestamp();
            core_db::app_config::save_idle_chunk_retryable_to_db(&w, val, now)?;
        }
        s.set_idle_chunk_retryable(val);
        tracing::info!(
            idle_chunk_retryable = val,
            "updated idle_chunk_retryable via admin API"
        );
        Ok(Json(serde_json::json!({
            "idle_chunk_retryable": val,
            "applies_to": "next_requests",
        })))
    }
    .await;
    inner
}

pub async fn put_runtime_quota_protection(
    State(s): State<AppState>,
    _auth: crate::extractors::AdminAuth,
    Json(body): Json<openproxy_types::config::QuotaProtectionConfig>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let inner: Result<Json<serde_json::Value>, ApiError> = async {
        {
            let w = s.db_pool().writer();
            let now = chrono::Utc::now().timestamp();
            openproxy_db::app_config::save_quota_protection_to_db(&w, &body, now)?;
        }
        let enabled = body.enabled;
        let threshold_percentage = body.threshold_percentage;
        s.set_quota_protection(body);
        tracing::info!(
            enabled,
            threshold_percentage,
            "updated quota_protection via admin API"
        );
        Ok(Json(serde_json::json!({
            "enabled": enabled,
            "threshold_percentage": threshold_percentage,
            "applies_to": "next_requests",
        })))
    }
    .await;
    inner
}

pub async fn get_maintenance_config(
    State(s): State<AppState>,
    _auth: crate::extractors::AdminAuth,
) -> Result<Json<serde_json::Value>, ApiError> {
    let cfg = s.maintenance_config();
    let status = s.vacuum_status();
    Ok(Json(serde_json::json!({
        "auto_vacuum": cfg.auto_vacuum,
        "vacuum_interval_hours": cfg.interval_secs / 3600,
        "usage_retention_days": cfg.usage_retention_days,
        "vacuum_status": status,
    })))
}

pub async fn put_maintenance_config(
    State(s): State<AppState>,
    _auth: crate::extractors::AdminAuth,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut cfg = s.maintenance_config();
    if let Some(v) = body.get("auto_vacuum").and_then(|v| v.as_bool()) {
        cfg.auto_vacuum = v;
    }
    if let Some(v) = body.get("vacuum_interval_hours").and_then(|v| v.as_u64()) {
        cfg.interval_secs = v.max(1) * 3600;
    }
    if let Some(v) = body.get("usage_retention_days").and_then(|v| v.as_u64()) {
        cfg.usage_retention_days = v as u32;
    }
    let auto_vacuum = cfg.auto_vacuum;
    let vacuum_interval_hours = cfg.interval_secs / 3600;
    let usage_retention_days = cfg.usage_retention_days;
    s.set_maintenance_config(cfg);
    Ok(Json(serde_json::json!({
        "updated": true,
        "config": {
            "auto_vacuum": auto_vacuum,
            "vacuum_interval_hours": vacuum_interval_hours,
            "usage_retention_days": usage_retention_days,
        }
    })))
}

pub async fn get_vacuum_status(
    State(s): State<AppState>,
    _auth: crate::extractors::AdminAuth,
) -> Result<Json<crate::state::VacuumStatus>, ApiError> {
    Ok(Json(s.vacuum_status()))
}

pub async fn get_recording_ttl(
    State(s): State<AppState>,
    _auth: crate::extractors::AdminAuth,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(serde_json::json!({
        "recording_ttl_secs": s.recording_ttl_secs(),
    })))
}

pub async fn put_recording_ttl(
    State(s): State<AppState>,
    _auth: crate::extractors::AdminAuth,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let inner: Result<Json<serde_json::Value>, ApiError> = async {
        let ttl_secs = body
            .get("recording_ttl_secs")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| CoreError::Validation("missing 'recording_ttl_secs' integer".into()))?;
        if ttl_secs < 0 {
            return Err(
                CoreError::Validation("'recording_ttl_secs' must be non-negative".into()).into(),
            );
        }
        {
            let w = s.db_pool().writer();
            let now = chrono::Utc::now().timestamp();
            core_db::app_config::save_recording_ttl_to_db(&w, ttl_secs, now)?;
        }
        s.set_recording_ttl_secs(ttl_secs);
        Ok(Json(serde_json::json!({
            "recording_ttl_secs": ttl_secs,
            "applies_to": "next_prune_tick",
        })))
    }
    .await;
    inner
}
