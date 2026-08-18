use super::{
    ApiError, AppState, CircuitBreakerConfig, CoreError, RacingConfig, RetriesConfig, Serialize,
    TimeoutsConfig, core_db,
};
use axum::{Json, extract::State};

/// Read-only view of the relevant `AppConfig` sections.
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeConfigResponse {
    pub timeouts: TimeoutsConfig,
    pub retries: RetriesConfig,
    pub circuit_breaker: CircuitBreakerConfig,
    pub racing: RacingConfig,
    /// Lifetime in seconds for recorded request/response bodies and
    /// headers. `0` means bodies are pruned immediately on the next
    /// prune tick.
    pub recording_ttl_secs: i64,
    pub compression: openproxy_compression::CompressionMode,
    /// When true, idle_chunk timeouts are treated as retryable
    /// (pipeline falls through to the next target).
    pub idle_chunk_retryable: bool,
    pub quota_protection: openproxy_types::config::QuotaProtectionConfig,
}

pub async fn admin_health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

pub async fn get_runtime_config(
    State(s): State<AppState>,
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

macro_rules! runtime_config_put {
    (
        $fn_name:ident($body:ident: $body_ty:ty) {
            save: $save_fn:path,
            state: $set_fn:ident,
            $(log: $log_expr:expr,)?
            response: $resp:expr $(,)?
        }
    ) => {
        pub async fn $fn_name(
            State(s): State<AppState>,
            Json($body): Json<$body_ty>,
        ) -> Result<Json<serde_json::Value>, ApiError> {
            {
                let w = s.db_pool().writer();
                let now = chrono::Utc::now().timestamp();
                $save_fn(&w, &$body, now)?;
            }
            $($log_expr;)?
            let resp = $resp;
            s.$set_fn($body);
            Ok(Json(resp))
        }
    };
    (
        $fn_name:ident(Json($body:ident)) -> $val:ident {
            extract: $extract:expr,
            save: $save_fn:path,
            state: $set_fn:ident,
            $(log: $log_expr:expr,)?
            response: $resp:expr $(,)?
        }
    ) => {
        pub async fn $fn_name(
            State(s): State<AppState>,
            Json($body): Json<serde_json::Value>,
        ) -> Result<Json<serde_json::Value>, ApiError> {
            let $val = $extract;
            {
                let w = s.db_pool().writer();
                let now = chrono::Utc::now().timestamp();
                $save_fn(&w, $val, now)?;
            }
            $($log_expr;)?
            let resp = $resp;
            s.$set_fn($val);
            Ok(Json(resp))
        }
    };
}

runtime_config_put!(
    put_runtime_timeouts(body: TimeoutsConfig) {
        save: core_db::app_config::save_timeouts_to_db,
        state: set_timeouts,
        response: serde_json::json!({
            "connect_ms": body.connect_ms,
            "request_send_ms": body.request_send_ms,
            "ttft_ms": body.ttft_ms,
            "idle_chunk_ms": body.idle_chunk_ms,
            "total_ms": body.total_ms,
            "applies_to": "next_requests",
        }),
    }
);

runtime_config_put!(
    put_runtime_compression(body: openproxy_compression::CompressionMode) {
        save: core_db::app_config::save_compression_to_db,
        state: set_compression_mode,
        response: serde_json::json!({
            "mode": body,
            "applies_to": "next_requests",
        }),
    }
);

runtime_config_put!(
    put_idle_chunk_retryable(Json(body)) -> val {
        extract: body
            .get("idle_chunk_retryable")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| {
                ApiError(CoreError::Validation(
                    "idle_chunk_retryable must be a boolean".into(),
                ))
            })?,
        save: core_db::app_config::save_idle_chunk_retryable_to_db,
        state: set_idle_chunk_retryable,
        log: tracing::info!(
            idle_chunk_retryable = val,
            "updated idle_chunk_retryable via admin API"
        ),
        response: serde_json::json!({
            "idle_chunk_retryable": val,
            "applies_to": "next_requests",
        }),
    }
);

runtime_config_put!(
    put_runtime_quota_protection(body: openproxy_types::config::QuotaProtectionConfig) {
        save: core_db::app_config::save_quota_protection_to_db,
        state: set_quota_protection,
        log: tracing::info!(
            enabled = body.enabled,
            threshold_percentage = body.threshold_percentage,
            "updated quota_protection via admin API"
        ),
        response: serde_json::json!({
            "enabled": body.enabled,
            "threshold_percentage": body.threshold_percentage,
            "applies_to": "next_requests",
        }),
    }
);

pub async fn get_maintenance_config(
    State(s): State<AppState>,
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
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut cfg = s.maintenance_config();
    if let Some(v) = body.get("auto_vacuum").and_then(serde_json::Value::as_bool) {
        cfg.auto_vacuum = v;
    }
    if let Some(v) = body
        .get("vacuum_interval_hours")
        .and_then(serde_json::Value::as_u64)
    {
        cfg.interval_secs = v.max(1) * 3600;
    }
    if let Some(v) = body
        .get("usage_retention_days")
        .and_then(serde_json::Value::as_u64)
    {
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
) -> Result<Json<crate::state::VacuumStatus>, ApiError> {
    Ok(Json(s.vacuum_status()))
}

pub async fn get_recording_ttl(
    State(s): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(serde_json::json!({
        "recording_ttl_secs": s.recording_ttl_secs(),
    })))
}

runtime_config_put!(
    put_recording_ttl(Json(body)) -> ttl_secs {
        extract: {
            let ttl = body
                .get("recording_ttl_secs")
                .and_then(serde_json::Value::as_i64)
                .ok_or_else(|| CoreError::Validation("missing 'recording_ttl_secs' integer".into()))?;
            if ttl < 0 {
                return Err(
                    CoreError::Validation("'recording_ttl_secs' must be non-negative".into()).into(),
                );
            }
            ttl
        },
        save: core_db::app_config::save_recording_ttl_to_db,
        state: set_recording_ttl_secs,
        response: serde_json::json!({
            "recording_ttl_secs": ttl_secs,
            "applies_to": "next_prune_tick",
        }),
    }
);
