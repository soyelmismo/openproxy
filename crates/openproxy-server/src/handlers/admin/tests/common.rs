pub(crate) use crate::handlers::admin::auth::authenticate_admin_ws;
pub use crate::handlers::admin::models::{TestOptions, run_test_for_model};
pub use crate::state::AppState;

pub use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode},
    routing::{get, post, put},
};
pub use openproxy_adapters::adapters;
pub use openproxy_core::api_keys as core_api_keys;
pub use openproxy_core::seed;
pub use openproxy_db as core_db;
pub use openproxy_db::secrets::MasterKey;
pub use openproxy_types::ProviderId;
pub use openproxy_types::config::TimeoutsConfig;
pub use tower::ServiceExt;

pub static SCAN_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub struct HomeGuard {
    pub prev: Option<std::ffi::OsString>,
}

impl HomeGuard {
    pub fn set(path: &std::path::Path) -> Self {
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", path) };
        Self { prev }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}

pub static AUTH_BYPASS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub struct EnvVarGuard {
    pub key: &'static str,
    pub prev: Option<String>,
}

impl EnvVarGuard {
    pub fn set(_lock: &std::sync::MutexGuard<'_, ()>, key: &'static str, value: &str) -> Self {
        let prev = std::env::var(key).ok();
        unsafe { std::env::set_var(key, value) };
        Self { key, prev }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

pub fn tempdir() -> openproxy_db::testing::TempDir {
    openproxy_db::testing::TempDir::new("openproxy-admin-test").expect("mkdir")
}

pub fn insert_manage_key(pool: &core_db::DbPool, plaintext: &str) {
    let w = pool.writer();
    let key_hash = core_api_keys::hash_key(plaintext);
    w.execute(
        "INSERT OR REPLACE INTO api_keys (key_hash, key_prefix, label, scopes_json, \
                allowed_models_json, allowed_combos_json, expires_at, created_by) \
             VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL, 'test')",
        rusqlite::params![
            key_hash,
            &plaintext[..plaintext.len().min(12)],
            "smoke-test",
            "[\"manage\", \"chat\"]",
        ],
    )
    .expect("insert api key");
}

pub async fn make_state_with_key(dir: &std::path::Path) -> (AppState, String) {
    let pool =
        std::sync::Arc::new(core_db::DbPool::open(&dir.join("smoke.db")).expect("open pool"));
    {
        let mut w = pool.writer();
        core_db::migrations::run(&mut w).expect("migrations");
    }
    let plaintext = format!("sk-smoke-{}", "x".repeat(40));
    insert_manage_key(&pool, &plaintext);

    let mk = MasterKey::generate().unwrap();
    let adapters = std::sync::Arc::new(parking_lot::RwLock::new(std::sync::Arc::new(
        adapters::builtin_adapters(),
    )));
    let state = AppState::for_test(
        openproxy_core::AppConfig::default(),
        pool,
        std::sync::Arc::new(mk),
        adapters,
    );
    (state, plaintext)
}

pub fn assert_recording_ttl_db_count(state: &AppState, expected: i64) {
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

pub fn insert_test_account(state: &AppState, provider_id: &str) -> i64 {
    let w = state.db_pool().writer();
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

pub async fn test_req(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or_default();
    (status, json)
}
