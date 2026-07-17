use super::*;
use crate::handlers::admin::accounts::refresh_account_quota;
use crate::handlers::admin::providers::refresh_provider_models;
use crate::handlers::admin::runtime::{get_recording_ttl, put_recording_ttl, put_runtime_timeouts};
use crate::handlers::admin::usage::UsageQuery;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::{get, post, put},
};
use openproxy_adapters::adapters;
use openproxy_core::api_keys as core_api_keys;
use openproxy_db as core_db;
use openproxy_db::secrets::MasterKey;
use openproxy_types::config::TimeoutsConfig;
use std::path::PathBuf;
use tower::ServiceExt;

fn tempdir() -> PathBuf {
    let base = std::env::temp_dir();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = base.join(format!("openproxy-admin-test-{}-{}", pid, nanos));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn insert_manage_key(pool: &core_db::DbPool, plaintext: &str) {
    // Plant a `manage`-scope, non-expired key directly via the
    // helper. The auth path matches by hash.
    let w = pool.writer();
    let key_hash = core_api_keys::hash_key(plaintext);
    w.execute(
        "INSERT INTO api_keys (key_hash, key_prefix, label, scopes_json, \
                allowed_models_json, allowed_combos_json, expires_at, created_by) \
             VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL, 'test')",
        rusqlite::params![
            key_hash,
            &plaintext[..plaintext.len().min(12)],
            "smoke-test",
            "[\"manage\"]",
        ],
    )
    .expect("insert api key");
}

async fn make_state_with_key(dir: &std::path::Path) -> (AppState, String) {
    let pool =
        std::sync::Arc::new(core_db::DbPool::open(&dir.join("smoke.db")).expect("open pool"));
    // Migrations + bootstrap are required for api_keys to exist
    // and for `authenticate_admin_ws` to find the row.
    {
        let mut w = pool.writer();
        core_db::migrations::run(&mut w).expect("migrations");
    }
    let plaintext = format!("sk-smoke-{}", "x".repeat(40));
    insert_manage_key(&pool, &plaintext);

    // MasterKey for tests: any 32 bytes is fine. Use the
    // built-in generator rather than baking a private constructor.
    let mk = MasterKey::generate();
    let adapters = std::sync::Arc::new(parking_lot::RwLock::new(adapters::builtin_adapters()));
    let state = AppState::for_test(
        openproxy_core::AppConfig::default(),
        pool,
        std::sync::Arc::new(mk),
        adapters,
    )
    .await;
    (state, plaintext)
}

fn assert_recording_ttl_db_count(state: &AppState, expected: i64) {
    let count: i64 = state.db_pool().with_conn(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM app_config WHERE key = 'recording_ttl_secs'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert_eq!(
        count, expected,
        "app_config recording_ttl_secs row count mismatch"
    );
}

#[tokio::test]
async fn put_runtime_timeouts_writes_db_and_updates_slot() {
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;

    // Sanity: the slot starts at the TOML defaults (5000/10000/...).
    let initial = state.timeouts();
    assert_eq!(initial.connect_ms, 5_000);

    let app = Router::new()
        .route("/admin/config/timeouts", put(put_runtime_timeouts))
        .with_state(state.clone());

    let body = serde_json::json!({
        "connect_ms": 1_u64,
        "request_send_ms": 2_u64,
        "ttft_ms": 3_u64,
        "idle_chunk_ms": 4_u64,
        "total_ms": 5_u64,
    });
    let req = Request::builder()
        .method("PUT")
        .uri("/admin/config/timeouts")
        .header("authorization", format!("Bearer {}", plaintext))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("build req");

    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK, "PUT should be 200");

    // Body shape: the 5 fields echoed back + `applies_to`.
    let bytes = axum::body::to_bytes(resp.into_body(), 16 * 1024)
        .await
        .expect("body");
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
    assert_eq!(parsed["connect_ms"], 1);
    assert_eq!(parsed["request_send_ms"], 2);
    assert_eq!(parsed["ttft_ms"], 3);
    assert_eq!(parsed["idle_chunk_ms"], 4);
    assert_eq!(parsed["total_ms"], 5);
    assert_eq!(parsed["applies_to"], "next_requests");

    // The slot was updated in-memory.
    let after = state.timeouts();
    assert_eq!(after.connect_ms, 1);
    assert_eq!(after.total_ms, 5);
    assert_eq!(
        after,
        TimeoutsConfig {
            connect_ms: 1,
            request_send_ms: 2,
            ttft_ms: 3,
            idle_chunk_ms: 4,
            total_ms: 5,
        }
    );

    // The row landed in the DB (one row, key='timeouts').
    let count: i64 = state.db_pool().with_conn(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM app_config WHERE key = 'timeouts'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert_eq!(count, 1, "PUT must have written a row");
}

#[tokio::test]
async fn put_runtime_timeouts_without_auth_returns_401() {
    let dir = tempdir();
    let (state, _plaintext) = make_state_with_key(&dir).await;
    let app = Router::new()
        .route("/admin/config/timeouts", put(put_runtime_timeouts))
        .with_state(state);
    let body = serde_json::json!({
        "connect_ms": 1_u64, "request_send_ms": 2_u64, "ttft_ms": 3_u64,
        "idle_chunk_ms": 4_u64, "total_ms": 5_u64,
    });
    let req = Request::builder()
        .method("PUT")
        .uri("/admin/config/timeouts")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("build req");
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// Sanity: a caller passing a malformed body (missing field) gets
// axum's default 400 from the JSON extractor.
#[tokio::test]
async fn put_runtime_timeouts_malformed_body_returns_400() {
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;
    let app = Router::new()
        .route("/admin/config/timeouts", put(put_runtime_timeouts))
        .with_state(state);
    let req = Request::builder()
        .method("PUT")
        .uri("/admin/config/timeouts")
        .header("authorization", format!("Bearer {}", plaintext))
        .header("content-type", "application/json")
        // Missing `total_ms` — serde will reject.
        .body(Body::from(
            r#"{"connect_ms":1,"request_send_ms":2,"ttft_ms":3,"idle_chunk_ms":4}"#,
        ))
        .expect("build req");
    let resp = app.oneshot(req).await.expect("oneshot");
    // axum's Json extractor reports malformed bodies as 422
    // (Unprocessable Entity), not 400. Either is a "client did
    // something wrong"; we just want to confirm the handler
    // doesn't 500 / leak internal state.
    assert!(
        resp.status() == StatusCode::BAD_REQUEST
            || resp.status() == StatusCode::UNPROCESSABLE_ENTITY,
        "expected 400 or 422, got {:?}",
        resp.status()
    );
}

// ---- HIGH fix: OPENPROXY_DASHBOARD_AUTH_BYPASS is an exact-match
// sentinel, not "any non-empty value". The old behaviour silently
// granted full admin access for `=false`, `=yes`, `=0`, etc.
//
// Both auth_bypass tests below mutate the same process-global env var
// (`OPENPROXY_DASHBOARD_AUTH_BYPASS`). `#[tokio::test]` runs tests in
// parallel by default, so without serialization the two tests race:
// one sets the var to `"1"`, the other sets it to `"false"`, and
// whichever reads first wins. This mutex serializes them so the
// set-var → authenticate → restore-var sequence is atomic.
static AUTH_BYPASS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tokio::test]
async fn auth_bypass_sentinel_1_admits_admin_request_without_key() {
    // When OPENPROXY_DASHBOARD_AUTH_BYPASS=*** is set AND no API key
    // exists, the request must succeed. This is the legitimate
    // "dev convenience" path and the operator has explicitly opted
    // in.
    let tmp = tempfile::tempdir().expect("tempdir");
    let (state, _key) = make_state_with_key(tmp.path()).await;
    // Drop the API key the helper just created so the request
    // would otherwise 401.
    {
        let w = state.db_pool().writer();
        w.execute("DELETE FROM api_keys", []).expect("delete keys");
    }
    let headers = HeaderMap::new();
    // SAFETY: the AUTH_BYPASS_TEST_LOCK mutex serializes all tests
    // that touch this env var, so the set-var → read → restore-var
    // sequence is atomic with respect to other tests in this module.
    let _guard = AUTH_BYPASS_TEST_LOCK.lock().unwrap();
    let prev = std::env::var("OPENPROXY_DASHBOARD_AUTH_BYPASS").ok();
    unsafe { std::env::set_var("OPENPROXY_DASHBOARD_AUTH_BYPASS", "1") };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        authenticate_admin_ws(&state, &headers, None).await
    }));
    match prev {
        Some(v) => unsafe { std::env::set_var("OPENPROXY_DASHBOARD_AUTH_BYPASS", v) },
        None => unsafe { std::env::remove_var("OPENPROXY_DASHBOARD_AUTH_BYPASS") },
    }
    let result = result.expect("authenticate_admin_ws should not panic");
    assert!(
        result.is_ok(),
        "authenticate_admin_ws should succeed when bypass=*** is set, got {:?}",
        result.err()
    );
}

#[tokio::test]
async fn auth_bypass_does_not_admit_on_non_sentinel_values() {
    // The old bug: any non-empty value of OPENPROXY_DASHBOARD_AUTH_BYPASS
    // bypassed auth. That meant `=false`, `=yes`, `=0`, `=legacy-token`
    // and other operator typos silently granted full admin access. The
    // fix restricts the bypass to the exact sentinel `1`; everything
    // else must fall through to normal auth, which fails here because
    // no API key is configured.
    let tmp = tempfile::tempdir().expect("tempdir");
    let (state, _key) = make_state_with_key(tmp.path()).await;
    {
        let w = state.db_pool().writer();
        w.execute("DELETE FROM api_keys", []).expect("delete keys");
    }
    for sentinel in ["false", "yes", "0", "true", "TRUE", "legacy-token", " "] {
        let headers = HeaderMap::new();
        // SAFETY: serialized by AUTH_BYPASS_TEST_LOCK.
        let _guard = AUTH_BYPASS_TEST_LOCK.lock().unwrap();
        let prev = std::env::var("OPENPROXY_DASHBOARD_AUTH_BYPASS").ok();
        unsafe { std::env::set_var("OPENPROXY_DASHBOARD_AUTH_BYPASS", sentinel) };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            authenticate_admin_ws(&state, &headers, None).await
        }));
        match prev {
            Some(v) => unsafe { std::env::set_var("OPENPROXY_DASHBOARD_AUTH_BYPASS", v) },
            None => unsafe { std::env::remove_var("OPENPROXY_DASHBOARD_AUTH_BYPASS") },
        }
        let result = result.expect("authenticate_admin_ws should not panic");
        assert!(
            result.is_err(),
            "OPENPROXY_DASHBOARD_AUTH_BYPASS={:?} must NOT bypass auth \
                 (sentinel must be exactly \"1\")",
            sentinel
        );
    }
}

// ---- MEDIUM fix: from/to usage filter validation ----

#[test]
fn usage_filter_rejects_garbage_timestamp_with_400() {
    // Pre-fix, `?from=garbage` returned zero rows with no error.
    // The operator got a misleading "no data" view. Post-fix it
    // surfaces as a validation error.
    let q = UsageQuery {
        from: Some("garbage".to_string()),
        to: None,
        provider_id: None,
        model_id: None,
        account_id: None,
        combo_id: None,
        api_key_id: None,
        preset: None,
    };
    let result = q.into_filter();
    let err = result.expect_err("garbage timestamp must be rejected");
    let msg = format!("{:?}", err);
    assert!(
        msg.contains("from"),
        "error must mention the bad field, got: {}",
        msg
    );
    assert!(
        msg.contains("garbage"),
        "error must include the bad value, got: {}",
        msg
    );
}

#[test]
fn usage_filter_accepts_rfc3339_and_canonicalises() {
    // The dashboard sends RFC-3339; the SQL builder compares against
    // canonical RFC-3339 in `created_at`. We must accept and
    // canonicalise (not reject) RFC-3339 input.
    let q = UsageQuery {
        from: Some("2026-06-18T07:00:00+02:00".to_string()),
        to: None,
        provider_id: None,
        model_id: None,
        account_id: None,
        combo_id: None,
        api_key_id: None,
        preset: None,
    };
    let f = q.into_filter().expect("RFC-3339 with offset is valid");
    let from = f.from.expect("from present");
    // The offset is normalised to UTC and the suffix is `Z`.
    assert!(from.ends_with('Z'), "expected Z-suffix, got: {}", from);
    assert!(
        from.starts_with("2026-06-18T05:00:00"),
        "expected 05:00 UTC, got: {}",
        from
    );
}

#[test]
fn usage_filter_accepts_sqlite_format() {
    // Operators paste `2026-06-18 07:00:00` from log lines; this
    // form must also be accepted.
    let q = UsageQuery {
        from: None,
        to: Some("2026-06-18 07:00:00".to_string()),
        provider_id: None,
        model_id: None,
        account_id: None,
        combo_id: None,
        api_key_id: None,
        preset: None,
    };
    let f = q.into_filter().expect("SQLite-style timestamp is valid");
    let to = f.to.expect("to present");
    assert_eq!(to, "2026-06-18T07:00:00Z");
}

#[test]
fn usage_filter_rejects_from_after_to() {
    // A reversed range is a client error: it would return zero
    // rows silently otherwise.
    let q = UsageQuery {
        from: Some("2026-06-18T08:00:00Z".to_string()),
        to: Some("2026-06-18T07:00:00Z".to_string()),
        provider_id: None,
        model_id: None,
        account_id: None,
        combo_id: None,
        api_key_id: None,
        preset: None,
    };
    let err = q
        .into_filter()
        .expect_err("reversed range must be rejected");
    let msg = format!("{:?}", err);
    assert!(
        msg.contains("must be <="),
        "expected ordering error, got: {}",
        msg
    );
}

#[test]
fn usage_filter_absent_timestamps_still_pass() {
    // Backward compat: when both fields are absent (the common
    // case in the dashboard's "show all" view), validation must
    // be a no-op.
    let q = UsageQuery {
        from: None,
        to: None,
        provider_id: None,
        model_id: None,
        account_id: None,
        combo_id: None,
        api_key_id: None,
        preset: None,
    };
    let f = q.into_filter().expect("absent timestamps are valid");
    assert!(f.from.is_none());
    assert!(f.to.is_none());
}

// ---- preset handling ----

#[test]
fn usage_filter_preset_this_month_resolves_to_month_bounds() {
    // `this_month` must produce [first-of-this-month 00:00 UTC,
    // first-of-next-month 00:00 UTC). We assert the day-of-month
    // rather than the full timestamp so the test is stable across
    // whatever month it runs in.
    let q = UsageQuery {
        from: None,
        to: None,
        provider_id: None,
        model_id: None,
        account_id: None,
        combo_id: None,
        api_key_id: None,
        preset: Some("this_month".to_string()),
    };
    let f = q.into_filter().expect("this_month preset is valid");
    let from = f.from.expect("from is computed from preset");
    let to = f.to.expect("to is computed from preset");
    assert!(
        from.ends_with("T00:00:00Z"),
        "from is midnight UTC: {}",
        from
    );
    assert!(to.ends_with("T00:00:00Z"), "to is midnight UTC: {}", to);
    assert!(
        from.ends_with("-01T00:00:00Z"),
        "from is the 1st of the month: {}",
        from
    );
    assert!(
        to.ends_with("-01T00:00:00Z"),
        "to is the 1st of the month: {}",
        to
    );
    assert!(from < to, "from must be before to");
}

#[test]
fn usage_filter_preset_overrides_explicit_from_to() {
    // When both `preset` and explicit `from`/`to` are set, the
    // preset wins. We pick a `7d` preset and an explicit `from`
    // far in the past; the resolved `from` should be ~7 days ago,
    // not the explicit value.
    let q = UsageQuery {
        from: Some("2000-01-01T00:00:00Z".to_string()),
        to: Some("2000-01-02T00:00:00Z".to_string()),
        provider_id: None,
        model_id: None,
        account_id: None,
        combo_id: None,
        api_key_id: None,
        preset: Some("7d".to_string()),
    };
    let f = q.into_filter().expect("preset + explicit range is valid");
    let from = f.from.expect("from is computed from preset");
    // The explicit `2000-01-01` would have been used if preset
    // did not take precedence — assert it is not 2000.
    assert!(
        !from.starts_with("2000-"),
        "preset must override explicit from: {}",
        from
    );
    assert!(
        from.starts_with("20"),
        "from is a recent-ish year: {}",
        from
    );
}

#[test]
fn usage_filter_preset_custom_falls_through_to_explicit_values() {
    // `custom` is the explicit opt-out sentinel: explicit
    // `from`/`to` must survive untouched.
    let q = UsageQuery {
        from: Some("2026-06-18T07:00:00Z".to_string()),
        to: Some("2026-06-19T07:00:00Z".to_string()),
        provider_id: None,
        model_id: None,
        account_id: None,
        combo_id: None,
        api_key_id: None,
        preset: Some("custom".to_string()),
    };
    let f = q.into_filter().expect("custom preset is valid");
    assert_eq!(f.from.as_deref(), Some("2026-06-18T07:00:00Z"));
    assert_eq!(f.to.as_deref(), Some("2026-06-19T07:00:00Z"));
}

#[test]
fn usage_filter_preset_unknown_string_returns_400() {
    // Unknown preset strings must surface as 400 so a typo in the
    // dashboard doesn't silently miss a window.
    let q = UsageQuery {
        from: None,
        to: None,
        provider_id: None,
        model_id: None,
        account_id: None,
        combo_id: None,
        api_key_id: None,
        preset: Some("last_week".to_string()),
    };
    let err = q
        .into_filter()
        .expect_err("unknown preset must be rejected");
    let msg = format!("{:?}", err);
    assert!(
        msg.contains("preset"),
        "error must mention preset, got: {}",
        msg
    );
    assert!(
        msg.contains("last_week"),
        "error must include the bad value, got: {}",
        msg
    );
}

// ---- MEDIUM fix: DefaultBodyLimit is raised to 32 MiB ----
//
// axum's default is 2 MiB. We raise it so long-context chat
// requests (system prompt + tool definitions + history) are not
// rejected. The smoke test below confirms a 10 MiB body is
// accepted and a 100 MiB body is rejected (the upper bound is
// configurable but currently 32 MiB; 100 MiB exceeds that).

#[tokio::test]
async fn body_limit_accepts_10_mib_chat_body() {
    // Build a minimal chat request with a 10 MiB system prompt.
    // 10 MiB ≪ 32 MiB ceiling → must be accepted (the handler
    // still rejects it for missing auth, but the rejection is
    // NOT 413 Payload Too Large).
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    let tmp = tempfile::tempdir().expect("tempdir");
    let (state, _key) = make_state_with_key(tmp.path()).await;
    let app = crate::router::build_router(state);
    let big = "x".repeat(10 * 1024 * 1024);
    let body_json = format!(
        r#"{{"model":"gpt-4o","messages":[{{"role":"system","content":"{}"}}]}}"#,
        big
    );
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body_json))
        .expect("build req");
    req.extensions_mut()
        .insert(axum::extract::connect_info::ConnectInfo(
            std::net::SocketAddr::from(([127, 0, 0, 1], 12345)),
        ));
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_ne!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "100 MiB body must be rejected by the 32 MiB body limit"
    );
}

#[tokio::test]
async fn body_limit_rejects_100_mib_chat_body() {
    // 100 MiB ≫ 32 MiB ceiling → must be rejected with 413.
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    let tmp = tempfile::tempdir().expect("tempdir");
    let (state, _key) = make_state_with_key(tmp.path()).await;
    let app = crate::router::build_router(state);
    let big = "x".repeat(100 * 1024 * 1024);
    let body_json = format!(
        r#"{{"model":"gpt-4o","messages":[{{"role":"system","content":"{}"}}]}}"#,
        big
    );
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body_json))
        .expect("build req");
    req.extensions_mut()
        .insert(axum::extract::connect_info::ConnectInfo(
            std::net::SocketAddr::from(([127, 0, 0, 1], 12345)),
        ));
    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "100 MiB body must be rejected by the 32 MiB body limit"
    );
}

// ---- LOW fix: clamp `since_id` to USAGE_RECENT_MAX_SINCE_ID so a
// client passing `?since_id=i64::MAX` cannot force the SQL planner
// to consider garbage keys. Negative values are still clamped to 0
// (existing behavior). The behavior we test is "doesn't blow up",
// not "returns the right rows" — the SQL is exercised elsewhere
// (core_usage::recent's tests); here we only validate input handling.

#[tokio::test]
async fn usage_recent_clamps_since_id_at_max() {
    // Build a request with `since_id=i64::MAX`. The handler must
    // clamp instead of forwarding; if it forwarded, the SQL `WHERE
    // id > ?1` on the PK is still index-driven and returns [] in
    // microseconds, but a malicious client shouldn't get the
    // satisfaction of forcing a comparison against MAX.
    use axum::http::Request;
    use tower::ServiceExt;
    let tmp = tempfile::tempdir().expect("tempdir");
    let (state, key) = make_state_with_key(tmp.path()).await;
    let app = crate::router::build_router(state);
    let req = Request::builder()
        .uri("/admin/usage/recent?since_id=9223372036854775807&limit=1")
        .header("authorization", format!("Bearer {key}"))
        .body(axum::body::Body::empty())
        .expect("build req");
    let resp = app.oneshot(req).await.expect("oneshot");
    assert!(
        resp.status().is_success(),
        "since_id=MAX must NOT 5xx; got {}",
        resp.status()
    );
}

#[tokio::test]
async fn usage_recent_rejects_negative_since_id() {
    // Negative since_id is meaningless (PK is positive). Existing
    // behavior clamps to 0, so the request returns the most-recent
    // rows. We assert it doesn't 5xx.
    use axum::http::Request;
    use tower::ServiceExt;
    let tmp = tempfile::tempdir().expect("tempdir");
    let (state, key) = make_state_with_key(tmp.path()).await;
    let app = crate::router::build_router(state);
    let req = Request::builder()
        .uri("/admin/usage/recent?since_id=-42&limit=1")
        .header("authorization", format!("Bearer {key}"))
        .body(axum::body::Body::empty())
        .expect("build req");
    let resp = app.oneshot(req).await.expect("oneshot");
    assert!(
        resp.status().is_success(),
        "since_id=-42 must NOT 5xx; got {}",
        resp.status()
    );
}

// ---- Cancellation note (was MEDIUM fix #10): the
// `test_combo_targets` handler used to spawn a disconnect-watcher
// task that drained `request.into_parts().1` (the request body)
// and flipped a `tokio::sync::watch` flag when the body stream
// ended. The fan-out loop polled that flag between targets and
// short-circuited when it flipped. The previous test
// (`test_combo_targets_signals_cancellation_via_watch`) exercised
// that watch-channel wiring in isolation.
//
// We removed the watcher because, for a POST with no body — which
// is what the dashboard actually sends — `Body::frame()` resolves
// to `None` immediately, so the watcher fired `disconnect_tx`
// before the fan-out loop started its second iteration. The
// fan-out then aborted after the first target, which silently
// broke "Test all". The handler now relies on Axum's natural
// cancellation: when the client drops the response future, the
// handler future is dropped, which in turn drops the in-flight
// `UpstreamClient` future (cancel-safe) and aborts the loop. No watcher
// task is needed.
//
// No regression test is added here because exercising the
// cancellation path end-to-end requires a mock upstream with
// controllable latency and a way to drop the response future
// mid-flight. The happy-path coverage (handler completes the
// fan-out for a bodyless POST) is provided by the dashboard's
// Playwright suite; the 180s timeout wrap is exercised by
// `run_test_for_model`'s own unit tests.

// ---- Recording TTL admin endpoints ----

#[tokio::test]
async fn get_recording_ttl_returns_default_value() {
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;

    // Sanity: the slot starts at the default (300s).
    assert_eq!(state.recording_ttl_secs(), 300);

    let app = Router::new()
        .route("/admin/config/recording-ttl", get(get_recording_ttl))
        .with_state(state.clone());

    let req = Request::builder()
        .method("GET")
        .uri("/admin/config/recording-ttl")
        .header("authorization", format!("Bearer {}", plaintext))
        .body(Body::empty())
        .expect("build req");

    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK, "GET should be 200");

    let bytes = axum::body::to_bytes(resp.into_body(), 1024)
        .await
        .expect("body");
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
    assert_eq!(
        parsed["recording_ttl_secs"], 300,
        "default recording TTL must be 300"
    );
}

#[tokio::test]
async fn put_recording_ttl_persists_new_value() {
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;

    let app = Router::new()
        .route(
            "/admin/config/recording-ttl",
            get(get_recording_ttl).put(put_recording_ttl),
        )
        .with_state(state.clone());

    let body = serde_json::json!({ "recording_ttl_secs": 600_i64 });
    let req = Request::builder()
        .method("PUT")
        .uri("/admin/config/recording-ttl")
        .header("authorization", format!("Bearer {}", plaintext))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("build req");

    let resp = app.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK, "PUT should be 200");

    // Body shape: the value echoed back + applies_to.
    let bytes = axum::body::to_bytes(resp.into_body(), 1024)
        .await
        .expect("body");
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
    assert_eq!(parsed["recording_ttl_secs"], 600);
    assert_eq!(parsed["applies_to"], "next_prune_tick");

    // The in-memory slot was updated.
    assert_eq!(state.recording_ttl_secs(), 600);

    // The row landed in the DB.
    let count: i64 = state.db_pool().with_conn(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM app_config WHERE key = 'recording_ttl_secs'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert_eq!(count, 1, "PUT must have written a row to app_config");

    // The persisted value matches what we sent.
    let value: String = state.db_pool().with_conn(|c| {
        c.query_row(
            "SELECT value FROM app_config WHERE key = 'recording_ttl_secs'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    let persisted: i64 = serde_json::from_str(&value).expect("parse value");
    assert_eq!(persisted, 600);
}

#[tokio::test]
async fn put_recording_ttl_rejects_negative_value() {
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;

    let app = Router::new()
        .route("/admin/config/recording-ttl", put(put_recording_ttl))
        .with_state(state.clone());

    let body = serde_json::json!({ "recording_ttl_secs": -1_i64 });
    let req = Request::builder()
        .method("PUT")
        .uri("/admin/config/recording-ttl")
        .header("authorization", format!("Bearer {}", plaintext))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("build req");

    let resp = app.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
        "expected 400 or 422 for negative TTL, got {:?}",
        status
    );

    // In-memory slot must NOT have changed.
    assert_eq!(
        state.recording_ttl_secs(),
        300,
        "in-memory TTL must not change on rejected PUT"
    );
    assert_recording_ttl_db_count(&state, 0);
}

#[tokio::test]
async fn put_recording_ttl_rejects_missing_field() {
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;

    let app = Router::new()
        .route("/admin/config/recording-ttl", put(put_recording_ttl))
        .with_state(state.clone());

    // Send a valid JSON object but missing the required
    // "recording_ttl_secs" field.
    let req = Request::builder()
        .method("PUT")
        .uri("/admin/config/recording-ttl")
        .header("authorization", format!("Bearer {}", plaintext))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"foo":"bar"}"#))
        .expect("build req");

    let resp = app.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
        "expected 400 or 422 for missing required field, got {:?}",
        status
    );

    // In-memory slot must NOT have changed.
    assert_eq!(
        state.recording_ttl_secs(),
        300,
        "in-memory TTL must not change on rejected PUT"
    );
    assert_recording_ttl_db_count(&state, 0);
}

#[tokio::test]
async fn put_recording_ttl_rejects_invalid_json_syntax() {
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;

    let app = Router::new()
        .route("/admin/config/recording-ttl", put(put_recording_ttl))
        .with_state(state.clone());

    let req = Request::builder()
        .method("PUT")
        .uri("/admin/config/recording-ttl")
        .header("authorization", format!("Bearer {}", plaintext))
        .header("content-type", "application/json")
        .body(Body::from(r#"{invalid"#))
        .expect("build req");

    let resp = app.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    assert!(
        status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
        "expected 400 or 422 for invalid JSON syntax, got {:?}",
        status
    );

    assert_eq!(
        state.recording_ttl_secs(),
        300,
        "in-memory TTL must not change on rejected PUT"
    );
    assert_recording_ttl_db_count(&state, 0);
}

// -----------------------------------------------------------------
// Regression tests for admin refresh endpoints.
//
// These tests exist because the endpoints `POST /admin/accounts/:id/refresh-quota`
// and `POST /admin/providers/:id/refresh` were hanging after the
// refactor to the hyper-based UpstreamClient. The dashboard
// reported "error sending request for url" because the server
// never responded. The tests call the handlers directly with a
// timeout to catch any hang regression.
// -----------------------------------------------------------------

/// Insert a test account for a given provider_id. Returns the
/// account id. The account has a dummy API key (not used for
/// upstream calls in the non-quota-capable path).
fn insert_test_account(state: &AppState, provider_id: &str) -> i64 {
    let w = state.db_pool().writer();
    // Ensure the provider exists (FK constraint).
    let _ = openproxy_core::providers::create(
        &w,
        openproxy_core::providers::NewProvider {
            id: &ProviderId::new(provider_id),
            name: provider_id,
            base_url: "https://example.com",
            auth_type: openproxy_core::providers::AuthType::Bearer,
            format: openproxy_core::providers::ProviderFormat::Openai,
            extra_headers_json: None,
            auto_activate_keyword: None,
            rate_limit_scope: openproxy_core::providers::RateLimitScope::Account,
        },
    );
    // Now insert the account using the core helper.
    let mk = state.master_key();
    let aid = openproxy_core::accounts::create(
        &w,
        &ProviderId::new(provider_id),
        Some("sk-test-dummy-key"),
        mk.as_ref(),
        Some("test"),
        0,
        None,
    )
    .expect("insert account");
    aid.0
}

#[tokio::test]
async fn refresh_account_quota_non_capable_provider_responds_fast() {
    // Regression: the endpoint must NOT hang when called for a
    // provider that doesn't have a quota fetcher (e.g.
    // 'openai'). The handler short-circuits with
    // {"supported": false} — no upstream call, no deadlock.
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;
    let account_id = insert_test_account(&state, "openai");

    let app = Router::new()
        .route(
            "/admin/accounts/{id}/refresh-quota",
            post(refresh_account_quota),
        )
        .with_state(state.clone());

    let req = Request::builder()
        .method("POST")
        .uri(format!("/admin/accounts/{}/refresh-quota", account_id))
        .header("authorization", format!("Bearer {}", plaintext))
        .body(Body::empty())
        .expect("build req");

    // Wrap in a timeout — if the handler hangs, the test fails
    // instead of blocking forever.
    let resp = tokio::time::timeout(std::time::Duration::from_secs(5), app.oneshot(req))
        .await
        .expect("refresh-quota handler hung for >5s (regression)")
        .expect("oneshot");

    assert_eq!(resp.status(), StatusCode::OK, "expected 200");
    let body = axum::body::to_bytes(resp.into_body(), 1024)
        .await
        .expect("body");
    let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(v["supported"], false, "expected supported=false for openai");
}

#[tokio::test]
async fn refresh_provider_models_unknown_provider_responds_fast() {
    // Regression: the endpoint must NOT hang when called for a
    // provider that doesn't exist. The handler returns an error
    // (404 or 400) — no upstream call, no deadlock.
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;

    let app = Router::new()
        .route(
            "/admin/providers/{id}/refresh",
            post(refresh_provider_models),
        )
        .with_state(state.clone());

    let req = Request::builder()
        .method("POST")
        .uri("/admin/providers/nonexistent-provider/refresh")
        .header("authorization", format!("Bearer {}", plaintext))
        .body(Body::empty())
        .expect("build req");

    let resp = tokio::time::timeout(std::time::Duration::from_secs(5), app.oneshot(req))
        .await
        .expect("refresh-provider handler hung for >5s (regression)")
        .expect("oneshot");

    // The handler returns 200 with an error in the JSON body, or
    // a 4xx/5xx. Either is acceptable as long as it doesn't hang.
    assert!(
        resp.status().is_client_error()
            || resp.status().is_server_error()
            || resp.status() == StatusCode::OK,
        "expected error or 200, got {:?}",
        resp.status()
    );
}

#[tokio::test]
async fn refresh_account_quota_nonexistent_account_responds_fast() {
    // Regression: the endpoint must NOT hang when called for an
    // account that doesn't exist. The handler returns 404 — no
    // upstream call, no deadlock.
    let dir = tempdir();
    let (state, plaintext) = make_state_with_key(&dir).await;

    let app = Router::new()
        .route(
            "/admin/accounts/{id}/refresh-quota",
            post(refresh_account_quota),
        )
        .with_state(state.clone());

    let req = Request::builder()
        .method("POST")
        .uri("/admin/accounts/99999/refresh-quota")
        .header("authorization", format!("Bearer {}", plaintext))
        .body(Body::empty())
        .expect("build req");

    let resp = tokio::time::timeout(std::time::Duration::from_secs(5), app.oneshot(req))
        .await
        .expect("refresh-quota handler hung for >5s on nonexistent account (regression)")
        .expect("oneshot");

    // Account not found -> 404 or 500. Either is fine as long as
    // it doesn't hang.
    assert!(
        resp.status().is_client_error() || resp.status().is_server_error(),
        "expected 4xx/5xx for nonexistent account, got {:?}",
        resp.status()
    );
}

// ---- G2.4 quota_low helper tests -----------------------------------

#[tokio::test]
async fn test_run_test_for_model_cancellation() {
    let dir = tempdir();
    let (state, _plaintext) = make_state_with_key(&dir).await;

    // Seed the built-in providers so openrouter exists
    {
        let w = state.db_pool().writer();
        seed::seed_builtin_providers(&w).expect("seed");
    }

    // Create a model in the DB to test against.
    let model_row_id = {
        let w = state.db_pool().writer();
        w.execute(
            "INSERT INTO models (provider_id, model_id, target_format, active) VALUES (?, ?, ?, ?)",
            ("openrouter", "gpt-4o", "openai", 1),
        )
        .expect("insert model");
        w.last_insert_rowid()
    };

    // Create a pre-cancelled watch receiver
    let (tx, rx) = tokio::sync::watch::channel(false);
    tx.send(true).unwrap();

    let r = run_test_for_model(
        &state,
        model_row_id,
        None,
        None,
        TestOptions::default(),
        Some(rx),
    )
    .await;

    assert_eq!(r.status, 0);
    assert_eq!(r.error_msg.as_deref(), Some("Cancel"));
}
