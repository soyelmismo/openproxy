//! HTTP router.
//!
//! Spec §2: every public + admin endpoint is wired here, in axum 0.8
//! syntax. Routes are grouped into nested sub-routers (`public_api_routes`,
//! `admin_routes`, `admin_api_routes`) for readability, then merged
//! into the root `Router`. The request-id middleware sits on the
//! outermost layer so every response — public or admin — carries an
//! `x-request-id` header.
//!
//! ## Top-level URL layout (post-F0 merge of the dashboard SPA into
//! the server binary)
//!
//! | Path                          | Handler / source                          |
//! |-------------------------------|--------------------------------------------|
//! | `GET  /v1/health`             | `health` (unauthenticated)                |
//! | `GET  /v1/models`             | `handlers::models::list_models`           |
//! | `POST /v1/chat/completions`   | `handlers::chat::chat_completions`        |
//! | `POST /v1/audio/transcriptions` | `handlers::audio::transcribe` (Whisper) |
//! | `GET  /admin`                 | SPA shell (`admin_ui::index_html`)        |
//! | `GET  /admin/`                | SPA shell (`admin_ui::index_html`)        |
//! | `GET  /admin/callback.html`   | OAuth callback page (`admin_ui::callback_html`) |
//! | `GET  /admin/dist/*`          | embedded bundle (`admin_ui::serve_asset`) |
//! | `GET  /admin/styles/*`        | embedded CSS (`admin_ui::serve_asset`)    |
//! | `GET  /admin/fonts/*`         | embedded fonts (`admin_ui::serve_asset`)  |
//! | `*    /admin/api/*`           | admin REST API (auth-protected)           |
//! | `GET  /admin/ws`              | live-logs WebSocket (own auth via `?token=`) |
//! | `GET  /admin/health`          | `handlers::admin::runtime::admin_health` (unauthenticated, kept public for LB probes) |
//! | `GET  /admin/oauth/callback`  | `handlers::admin::oauth::oauth_callback` (unauthenticated, browser callback) |
//!
//! The dashboard SPA loads BEFORE auth: `index.html`, `callback.html`,
//! and every `/admin/dist/*` / `/admin/styles/*` / `/admin/fonts/*`
//! asset are served without checking credentials. The SPA itself
//! sends the admin API key as a Bearer token on each `/admin/api/*`
//! call. The WebSocket upgrade at `/admin/ws` does its own auth
//! inside the handler (`handlers::admin::usage::usage_stream`) so it can accept `?token=`
//! in the query string (browsers can't set headers on WS handshakes).

use axum::{
    Json, Router, middleware,
    routing::{delete, get, post, put},
};
use serde_json::json;

use crate::{
    admin_ui,
    handlers::{self, admin::admin_auth_middleware},
    state::AppState,
};

/// Build the root [`Router`] for the server.
///
/// See the module docs for the full URL layout. The state is attached
/// via `with_state` so individual handlers can accept `State<AppState>`
/// in their extractor list. The request-id middleware is applied at
/// the outermost layer.
///
/// The chat routes are wrapped in [`client_disconnect_middleware`]
/// so the chat handler's `client_disconnected` watch is driven by
/// real TCP-level events (request-body read errors + response-body
/// write errors) instead of a time-based watchdog. See
/// `crates/openproxy-server/src/disconnect.rs` for the rationale.
pub fn build_router(state: AppState) -> Router {
    // Public + chat routes. `/v1/health` is a tiny liveness probe;
    // `/v1/models` lists known models in OpenAI shape;
    // `/v1/chat/completions` is the main entry point.
    //
    // The disconnect middleware is layered ONLY on the chat routes:
    // admin CRUD endpoints are short-lived and don't need
    // per-request cancel tracking, and the liveness probe would
    // pay the wrapper cost on every health check.
    //
    // NOTE: client_disconnect_middleware is applied to the chat
    // path. It previously caused false-positive "client disconnected"
    // errors because the request body was also wrapped, meaning
    // hyper could poll the socket after the request body had been
    // fully read, encountering TCP read half-closes/RSTs and firing
    // the cancellation watch.
    //
    // By modifying client_disconnect_middleware to ONLY wrap the
    // response body, we safely avoid these false-positives while
    // still reliably detecting actual client disconnects (write
    // failures) during sync response writing or active SSE stream
    // generation.

    let public_api_routes = Router::new()
        .route("/v1/models", get(handlers::models::list_models))
        .route(
            "/v1/chat/completions",
            post(handlers::chat::chat_completions)
                .route_layer(middleware::from_fn(
                    crate::disconnect::client_disconnect_middleware,
                ))
                .route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    crate::middleware::rate_limit::rate_limit_middleware,
                ))
                .route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    crate::middleware::routing::routing_middleware,
                ))
                .route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    crate::middleware::auth::auth_middleware,
                )),
        )
        .route(
            "/v1/messages",
            post(handlers::messages::anthropic_messages)
                .route_layer(middleware::from_fn(
                    crate::disconnect::client_disconnect_middleware,
                ))
                .route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    crate::middleware::rate_limit::rate_limit_middleware,
                ))
                .route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    crate::middleware::routing::routing_middleware,
                ))
                .route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    crate::middleware::auth::auth_middleware,
                )),
        )
        .route(
            "/v1/audio/transcriptions",
            post(handlers::audio::transcribe),
        );

    // Admin REST API. Every route here is mounted under `/admin/api/*`
    // (see `admin_routes` below). The auth middleware
    // (`admin_auth_middleware`) is layered on this sub-router ONLY —
    // the SPA shell, static assets, the WS handler, and the
    // public OAuth/health endpoints stay unauthenticated so the
    // dashboard can load before the user enters credentials.
    //
    // Authorization model: every admin REST route EXCEPT the
    // liveness probe (`/admin/health`) and the OAuth browser
    // callback (`/admin/oauth/callback`) requires a `manage`-scope
    // API key, verified by [`admin_auth_middleware`]. Those two
    // exempt routes are intentionally public: the liveness probe
    // is for load balancers and uptime monitors that should not
    // need credentials, and the OAuth callback is the URL the
    // upstream provider (Google, etc.) redirects the user's
    // browser to — by design the browser arrives without admin
    // credentials, and the handler just echoes back the `code`
    // for the user to copy into the dashboard.
    //
    // The middleware reads only the `Authorization` header, which
    // is the contract for the HTTP path. The WebSocket upgrade
    // handler (`handlers::admin::usage::usage_stream`) also accepts `?token=` in the query
    // string — that path is handled inside the handler itself
    // (the middleware would not see the WS upgrade as a normal
    // request), so the per-handler auth check there is the source
    // of truth for the WebSocket path.
    let admin_api_routes = Router::new()
        .route("/config", get(handlers::admin::runtime::get_runtime_config))
        .route(
            "/config/timeouts",
            axum::routing::put(handlers::admin::runtime::put_runtime_timeouts),
        )
        .route(
            "/config/recording-ttl",
            get(handlers::admin::runtime::get_recording_ttl)
                .put(handlers::admin::runtime::put_recording_ttl),
        )
        .route(
            "/config/compression",
            axum::routing::put(handlers::admin::runtime::put_runtime_compression),
        )
        .route(
            "/config/idle-chunk-retryable",
            axum::routing::put(handlers::admin::runtime::put_idle_chunk_retryable),
        )
        .route(
            "/config/quota-protection",
            axum::routing::put(handlers::admin::runtime::put_runtime_quota_protection),
        )
        .route(
            "/config/maintenance",
            get(handlers::admin::runtime::get_maintenance_config)
                .put(handlers::admin::runtime::put_maintenance_config),
        )
        .route(
            "/config/vacuum-status",
            get(handlers::admin::runtime::get_vacuum_status),
        )
        .route(
            "/providers",
            get(handlers::admin::providers::list_providers)
                .post(handlers::admin::providers::create_provider),
        )
        .route(
            "/providers/{id}",
            get(handlers::admin::providers::get_provider)
                .delete(handlers::admin::providers::delete_provider)
                .patch(handlers::admin::providers::update_provider),
        )
        .route(
            "/accounts",
            get(handlers::admin::accounts::list_accounts)
                .post(handlers::admin::accounts::create_account),
        )
        .route(
            "/accounts/{id}",
            axum::routing::delete(handlers::admin::accounts::delete_account),
        )
        .route(
            "/accounts/{id}/health",
            post(handlers::admin::accounts::set_account_health),
        )
        .route(
            "/accounts/{id}/api-key",
            put(handlers::admin::accounts::update_account_api_key),
        )
        .route(
            "/accounts/{id}/refresh-quota",
            post(handlers::admin::accounts::refresh_account_quota),
        )
        .route(
            "/combos",
            get(handlers::admin::combos::list_combos).post(handlers::admin::combos::create_combo),
        )
        .route(
            "/combos/{id}",
            get(handlers::admin::combos::get_combo)
                .delete(handlers::admin::combos::delete_combo)
                .patch(handlers::admin::combos::update_combo),
        )
        .route(
            "/combos/{id}/test-all",
            post(handlers::admin::combos::test_combo_targets).route_layer(middleware::from_fn(
                crate::disconnect::client_disconnect_middleware,
            )),
        )
        .route(
            "/combos/{id}/targets",
            get(handlers::admin::combos::list_combo_targets)
                .post(handlers::admin::combos::add_target),
        )
        // IMPORTANT: this literal-segment route MUST be registered
        // before `/combos/{id}/targets/:target_id`. axum 0.7
        // matches routes in registration order; if `:target_id` is
        // registered first it would happily swallow `valid-sub-combos`
        // and 405 the GET (because the :target_id route only allows
        // PATCH and DELETE).
        .route(
            "/combos/{id}/targets/valid-sub-combos",
            get(handlers::admin::combos::list_valid_sub_combos),
        )
        // IMPORTANT: this literal-segment route MUST be registered
        // before `/combos/{id}/targets/:target_id`. axum 0.7
        // matches routes in registration order; if `:target_id` is
        // registered first it would happily swallow `reorder` and
        // 405 every POST (because the :target_id route only allows
        // PATCH and DELETE).
        .route(
            "/combos/{id}/targets/reorder",
            axum::routing::post(handlers::admin::combos::reorder_combo_targets),
        )
        // IMPORTANT: this literal-segment route MUST be registered
        // before `/combos/{id}/targets/:target_id`. axum 0.7
        // matches routes in registration order; if `:target_id` is
        // registered first it would happily swallow `clear-cooldown`
        // and 405 every POST (because the :target_id route only allows
        // PATCH and DELETE).
        .route(
            "/combos/{id}/targets/{target_id}/clear-cooldown",
            axum::routing::post(handlers::admin::combos::clear_combo_target_cooldown),
        )
        .route(
            "/combos/{id}/targets/{target_id}",
            axum::routing::patch(handlers::admin::combos::update_combo_target)
                .delete(handlers::admin::combos::delete_combo_target),
        )
        .route("/usage/summary", get(handlers::admin::usage::usage_summary))
        .route(
            "/usage/by-model",
            get(handlers::admin::usage::usage_by_model),
        )
        .route(
            "/usage/by-provider",
            get(handlers::admin::usage::usage_by_provider),
        )
        .route(
            "/usage/monthly-by-provider",
            get(handlers::admin::usage::usage_monthly_by_provider),
        )
        .route("/usage/by-day", get(handlers::admin::usage::usage_by_day))
        .route(
            "/usage/by-account",
            get(handlers::admin::usage::usage_by_account),
        )
        .route(
            "/usage/by-status",
            get(handlers::admin::usage::usage_by_status),
        )
        .route("/usage/errors", get(handlers::admin::usage::usage_errors))
        .route("/usage/latency", get(handlers::admin::usage::usage_latency))
        .route("/usage/races", get(handlers::admin::usage::usage_races))
        .route("/usage/recent", get(handlers::admin::usage::usage_recent))
        .route("/usage/detail", get(handlers::admin::usage::usage_detail))
        .route("/debug/logs", get(handlers::admin::debug::debug_logs))
        .route(
            "/debug/clear",
            post(handlers::admin::debug::debug_logs_clear),
        )
        .route("/debug/vacuum", post(handlers::admin::debug::debug_vacuum))
        .route(
            "/debug/recover",
            post(handlers::admin::debug::debug_recover),
        )
        .route(
            "/recording",
            get(handlers::admin::debug::get_recording).post(handlers::admin::debug::set_recording),
        )
        .route(
            "/models/{id}/refresh",
            post(handlers::admin::models::refresh_models),
        )
        .route(
            "/models/{id}/toggle",
            post(handlers::admin::models::toggle_model),
        )
        .route(
            "/models/bulk-toggle",
            post(handlers::admin::models::bulk_toggle_models),
        )
        .route(
            "/models/{id}",
            axum::routing::delete(handlers::admin::models::delete_model),
        )
        .route("/models", get(handlers::admin::models::list_models_admin))
        .route(
            "/models/custom",
            post(handlers::admin::models::create_custom_model),
        )
        .route(
            "/models/{id}/test",
            post(handlers::admin::models::test_model).route_layer(middleware::from_fn(
                crate::disconnect::client_disconnect_middleware,
            )),
        )
        .route(
            "/providers/{id}/refresh",
            post(handlers::admin::providers::refresh_provider_models),
        )
        .route(
            "/providers/{id}/active",
            post(handlers::admin::providers::set_provider_active),
        )
        .route(
            "/keys",
            get(handlers::admin::api_keys::list_api_keys)
                .post(handlers::admin::api_keys::create_api_key),
        )
        .route(
            "/keys/{id}",
            get(handlers::admin::api_keys::get_api_key)
                .patch(handlers::admin::api_keys::update_api_key)
                .delete(handlers::admin::api_keys::delete_api_key),
        )
        .route(
            "/keys/{id}/revoke",
            post(handlers::admin::api_keys::revoke_api_key),
        )
        .route(
            "/keys/{id}/regenerate",
            post(handlers::admin::api_keys::regenerate_api_key),
        )
        .route(
            "/keys/{id}/usage",
            get(handlers::admin::api_keys::api_key_usage),
        )
        // Free proxies endpoints
        .route(
            "/proxies",
            get(handlers::admin::proxies::list_proxies)
                .post(handlers::admin::proxies::create_custom_proxy),
        )
        .route(
            "/proxies/sync",
            post(handlers::admin::proxies::sync_proxies),
        )
        .route(
            "/proxies/test-all",
            post(handlers::admin::proxies::test_all_proxies),
        )
        .route(
            "/proxies/{id}",
            delete(handlers::admin::proxies::delete_proxy),
        )
        .route(
            "/proxies/{id}/test",
            post(handlers::admin::proxies::test_proxy),
        )
        // OAuth endpoints
        // models.dev sync
        .route(
            "/models/sync-models-dev",
            post(handlers::admin::models::sync_models_dev),
        )
        // Re-price historical usage rows after models.dev sync
        .route(
            "/usage/recompute-costs",
            post(handlers::admin::usage::recompute_usage_costs),
        )
        // ----------------------------------------------------------------
        // Notifications tray (F1). Surfaces discovery + system events
        // to the dashboard. Real-time push is delivered via the WS
        // handler in `stream_usage_rows` (F2 wires the broadcast
        // subscription); these REST endpoints are for the initial
        // load + user-initiated mutations.
        //
        // Route registration order: literal segments
        // (`/notifications`, `/notifications/read-all`,
        // `/notifications/unread-count`) MUST come before the
        // `{id}`-param routes so axum 0.8's registration-order
        // matcher doesn't let `{id}` swallow `read-all` / `unread-count`.
        .route(
            "/notifications",
            get(handlers::admin::notifications::list_notifications),
        )
        .route(
            "/notifications/read-all",
            post(handlers::admin::notifications::mark_all_notifications_read),
        )
        .route(
            "/notifications/unread-count",
            get(handlers::admin::notifications::notifications_unread_count),
        )
        .route(
            "/notifications/{id}/read",
            post(handlers::admin::notifications::mark_notification_read),
        )
        .route(
            "/notifications/{id}/archive",
            post(handlers::admin::notifications::archive_notification),
        )
        .route(
            "/notifications/{id}",
            axum::routing::delete(handlers::admin::notifications::delete_notification),
        )
        .route(
            "/oauth/{provider}/authorize",
            get(handlers::admin::oauth::oauth_authorize),
        )
        .route(
            "/oauth/{provider}/exchange",
            post(handlers::admin::oauth::oauth_exchange),
        )
        .route(
            "/oauth/{provider}/device-code",
            post(handlers::admin::oauth::oauth_device_code),
        )
        .route(
            "/oauth/{provider}/device-poll",
            post(handlers::admin::oauth::oauth_device_poll),
        )
        // NOTE: `/oauth/callback` is intentionally NOT
        // registered here — it lives at `/admin/oauth/callback`
        // (top-level public route in `admin_routes` below, the
        // browser-callback URL, no auth required).
        //
        // B1 (Bug 1): JSON 404 fallback for unmatched `/admin/api/*`
        // routes. In axum 0.8, when a nested router doesn't match,
        // the request falls through to the PARENT router's fallback.
        // Without this fallback here, an unmatched `/admin/api/*`
        // path (e.g. `/admin/api/health` — there's a public
        // `/admin/health` but no `/admin/api/health`) would fall
        // through to `admin_routes`'s `.fallback(admin_ui::serve_asset)`,
        // which returns the SPA's `index.html` (HTML) — confusing
        // for API clients that expect JSON. This fallback returns a
        // proper JSON 404 with a structured `{"error":{"code":...}}`
        // body, matching the shape used by the rest of the admin
        // API's error responses. Only truly non-API paths under
        // `/admin/*` (e.g. `/admin/combos/42/edit`) now fall through
        // to the SPA.
        .fallback(|| async {
            (
                axum::http::StatusCode::NOT_FOUND,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                r#"{"error":{"code":"not_found","message":"endpoint not found"}}"#,
            )
        });

    // Apply the admin auth middleware to the protected admin REST
    // routes ONLY. The state-clone is required because
    // `from_fn_with_state` takes ownership of the state; we still
    // attach the same state to the root router via `with_state(state)`
    // below.
    let admin_api_routes = admin_api_routes.layer(middleware::from_fn_with_state(
        state.clone(),
        admin_auth_middleware,
    ));

    // Top-level admin router. Mounts the SPA shell at `/admin` and
    // `/admin/`, the OAuth callback page at `/admin/callback.html`,
    // the protected REST API under `/admin/api/*`, the WS upgrade at
    // `/admin/ws`, and the two intentionally-public endpoints
    // (`/admin/health`, `/admin/oauth/callback`). Anything else under
    // `/admin/*` falls through to `admin_ui::serve_asset`, which
    // either serves an embedded static asset (`/admin/dist/app.js`,
    // `/admin/styles/index.css`, etc.) or the SPA shell (for unknown
    // paths — the SPA's hash-router takes over from there).
    //
    // Auth scope:
    //   - `/admin/api/*`       — auth middleware (above)
    //   - `/admin/ws`          — per-handler auth (`handlers::admin::usage::usage_stream`)
    //   - `/admin/health`      — public (LB probes)
    //   - `/admin/oauth/callback` — public (browser callback)
    //   - everything else      — public (SPA shell + assets)
    let admin_routes = Router::new()
        // `/admin` and `/admin/` both serve the SPA shell. axum 0.7+
        // treats trailing-slash and no-trailing-slash as different
        // paths, so we register both. (Note: axum 0.8 rejects empty-string
        // route paths, so we only register "/" here — the outer router's
        // `.nest("/admin", admin_routes)` handles the no-trailing-slash case
        // via the SPA fallback.)
        .route("/", get(admin_ui::index_html))
        .route("/callback.html", get(admin_ui::callback_html))
        .route("/health", get(handlers::admin::runtime::admin_health))
        .route(
            "/oauth/callback",
            get(handlers::admin::oauth::oauth_callback),
        )
        .route("/ws", get(handlers::admin::usage::usage_stream))
        // F3: i18n string packs. Public (no auth) — the dashboard's
        // `loadLang('en')` runs at boot BEFORE the SPA can attach the
        // admin Bearer token, and i18n packs contain no secrets
        // (only generic UI labels). Registered as a literal route here
        // (not under `/api`) so it stays outside the auth middleware.
        //
        // NOTE on the route pattern: axum 0.8 rejects `/i18n/{lang}.json`
        // ("Only one parameter is allowed per path segment") because
        // mixing a path-param with a literal `.json` suffix in a single
        // segment is no longer supported. We register `/i18n/{lang}`
        // instead, which matches `/i18n/en.json` as a single segment
        // (no slash in `en.json`) and captures `lang = "en.json"`.
        // The handler then strips the optional `.json` extension and
        // validates the lang code. See `admin_ui::serve_i18n` for the
        // path-traversal guard + cache headers + extension parsing.
        .route("/i18n/{lang}", get(admin_ui::serve_i18n))
        .nest("/api", admin_api_routes)
        .fallback(admin_ui::serve_asset);

    Router::new()
        .route(
            "/",
            get(|| async { axum::response::Redirect::temporary("/admin") }),
        )
        .route(
            "/admin/",
            get(|| async { axum::response::Redirect::temporary("/admin") }),
        )
        .route("/v1/health", get(health))
        .merge(public_api_routes)
        .nest("/admin", admin_routes)
        .layer(middleware::from_fn(
            crate::middleware::request_id::request_id,
        ))
        // MEDIUM fix (audit finding #8): axum's default body limit is
        // 2 MiB, which is too small for a single legitimate prompt (some
        // long-context requests attach tens of KiB of system prompt +
        // tool definitions) and has no project-wide ceiling for the
        // admin JSON extractors (POST /admin/api/combos/{id}/targets,
        // handlers::admin::models::bulk_toggle_models, handlers::admin::combos::reorder_combo_targets, etc.). Raising to
        // 32 MiB allows long-context chat while keeping a sane DoS
        // ceiling. Streaming requests (SSE) are not affected — the
        // limit applies to the request body, not the response.
        .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024))
        .with_state(state)
}

/// `GET /v1/health` — unauthenticated liveness probe.
///
/// Returns `{"status": "ok", "version": <CARGO_PKG_VERSION>}`. The
/// version string is baked at compile time and reflects the server
/// crate's package version.
async fn health() -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use openproxy_adapters::adapters;
    use openproxy_core::AppConfig;
    use openproxy_db as core_db;
    use openproxy_db::MasterKey;
    use parking_lot::RwLock;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn make_state() -> AppState {
        let (pool, _path) = fresh_pool();
        let db_pool = Arc::new(pool);
        let master_key = Arc::new(MasterKey::generate());
        let adapters = Arc::new(RwLock::new(Vec::<adapters::ProviderAdapterEnum>::new()));
        AppState::for_test(AppConfig::default(), db_pool, master_key, adapters).await
    }

    fn fresh_pool() -> (core_db::DbPool, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("openproxy-router-test-{}-{}-{}", pid, nanos, n));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("state.db");
        let pool = core_db::DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            core_db::migrations::run(&mut w).expect("migrations");
        }
        (pool, path)
    }

    #[tokio::test]
    async fn test_public_health() {
        let state = make_state().await;
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["status"], "ok");
    }

    #[tokio::test]
    async fn test_admin_api_fallback_404_json() {
        // Unmatched /admin/api/* routes should return JSON 404, not HTML
        let state = make_state().await;
        let app = build_router(state.clone());

        let api_key = "test-api-key-123";
        let key_hash = openproxy_core::api_keys::hash_key(api_key);
        {
            let w = state.db_pool().writer();
            w.execute(
                "INSERT INTO api_keys (key_hash, key_prefix, label, scopes_json, \
                    allowed_models_json, allowed_combos_json, expires_at, created_by) \
                 VALUES (?1, ?2, ?3, ?4, NULL, NULL, NULL, 'test')",
                rusqlite::params![
                    key_hash,
                    &api_key[..api_key.len().min(12)],
                    "smoke-test",
                    "[\"manage\"]",
                ],
            )
            .unwrap();
        }

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/admin/api/does-not-exist-12345")
                    .header("Authorization", format!("Bearer {}", api_key))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/json"
        );

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "not_found");
    }
}
