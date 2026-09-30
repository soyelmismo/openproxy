//! HTTP router.
//!
//! Spec §2: every public + admin endpoint is wired here, in axum 0.8 syntax.
//! Routes are grouped into nested sub-routers (`public_api_routes`, `admin_routes`,
//! `admin_api_routes`) then merged into the root `Router`; the request-id
//! middleware sits on the outermost layer so every response carries
//! `x-request-id`.
//!
//! ## Top-level URL layout (dashboard SPA merged into the server binary)
//!
//! | Path                          | Handler / source                          |
//! |----------|----------|
//! | `GET  /v1/health`             | `health` (unauthenticated)                |
//! | `GET  /v1/models`             | `handlers::models::list_models`           |
//! | `POST /v1/chat/completions`   | `handlers::chat::chat_completions`        |
//! | `POST /v1/embeddings`         | `handlers::embeddings::create_embeddings` |
//! | `POST /v1/images/generations`  | `handlers::images::generate_images`       |
//! | `POST /v1/images/edits`        | `handlers::images::edit_images`           |
//! | `POST /v1/images/variations`   | `handlers::images::create_image_variation`|
//! | `POST /v1/audio/transcriptions` | `handlers::audio::transcribe` (Whisper) |
//! | `GET  /admin`                 | SPA shell (`admin_ui::index_html`)        |
//! | `GET  /admin/`                | SPA shell (`admin_ui::index_html`)        |
//! | `GET  /admin/callback.html`   | OAuth callback page (`admin_ui::callback_html`) |
//! | `GET  /admin/dist/*`          | embedded bundle (`admin_ui::serve_asset`) |
//! | `GET  /admin/styles/*`        | embedded CSS (`admin_ui::serve_asset`)    |
//! | `GET  /admin/fonts/*`         | embedded fonts (`admin_ui::serve_asset`)  |
//! | `*    /admin/api/*`           | admin REST API (auth-protected)           |
//! | `POST /admin/api/ws-ticket`   | single-use handshake ticket for `/admin/ws` (auth-protected) |
//! | `GET  /admin/ws`              | live-logs WebSocket (own auth: Bearer header or `?ticket=`) |
//! | `GET  /admin/health`          | `handlers::admin::runtime::admin_health` (unauthenticated, kept public for LB probes) |
//! | `GET  /admin/oauth/callback`  | `handlers::admin::oauth::oauth_callback` (unauthenticated, browser callback) |
//!
//! The SPA loads BEFORE auth: `index.html`, `callback.html` and every
//! `/admin/dist/*` / `/admin/styles/*` / `/admin/fonts/*` asset are served
//! without credentials; the SPA then sends the admin API key as a Bearer token
//! on each `/admin/api/*` call. `/admin/ws` does its own auth in the handler
//! (`handlers::admin::usage::usage_stream`): a Bearer header (non-browser
//! clients) or a single-use `?ticket=` from `POST /admin/api/ws-ticket` —
//! browsers cannot set headers on a WS handshake and a raw key in the URL would
//! land in proxy logs.

use axum::{Json, Router, middleware, routing::get};
use serde_json::json;

use crate::{
    admin_ui,
    handlers::{self, admin::admin_auth_middleware},
    state::AppState,
};

pub fn build_router(state: AppState) -> Router {
    let public_api_routes = handlers::public_api_routes(&state);
    let admin_routes = build_admin_router(&state);

    // Security (OP-14): honor the configured `server.request_max_body_bytes`
    // (default 10 MiB) instead of a hardcoded 32 MiB that ignored the setting.
    // Admin backup restore raises its own per-route limit (see backup.rs).
    let request_body_limit = state.config().server.request_max_body_bytes;
    // Cloned for the outermost security-headers layer, which needs the
    // trusted-proxy config for conditional HSTS (OP-21).
    let state_clone = state.clone();

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
        .layer(crate::middleware::compression::transport_compression_layer())
        .layer(middleware::from_fn(
            crate::middleware::request_id::request_id,
        ))
        // Request bodies only — SSE responses are unaffected. Backup restore
        // and other admin routes that legitimately need larger payloads raise
        // this with their own per-route `DefaultBodyLimit`.
        .layer(axum::extract::DefaultBodyLimit::max(request_body_limit))
        .with_state(state)
        // Outermost so every response carries the browser security headers;
        // policy rationale in `middleware::security_headers`.
        .layer(middleware::from_fn_with_state(
            state_clone,
            crate::middleware::security_headers::security_headers,
        ))
}

fn build_admin_router(state: &AppState) -> Router<AppState> {
    // Admin REST API. `admin_auth_middleware` is layered on this sub-router
    // ONLY, so the SPA shell, static assets, the WS handler and the public
    // OAuth/health endpoints stay unauthenticated and the dashboard can load
    // before credentials are entered.
    //
    // Every route here requires a `manage`-scope API key except two
    // intentional exemptions: `/admin/health` (LB probes carry no credentials)
    // and `/admin/oauth/callback` (the provider redirects the browser there by
    // design; the handler just echoes the `code` back for pasting into the
    // dashboard).
    //
    // The middleware reads only the `Authorization` header (the HTTP contract).
    // The WebSocket upgrade additionally accepts a single-use `?ticket=` — never
    // a raw key — and is authenticated inside
    // `handlers::admin::usage::usage_stream`, since the middleware would not see
    // a WS upgrade as a normal request.
    let admin_api_routes = handlers::admin::admin_api_routes();

    // The state clone is required because `from_fn_with_state` takes ownership;
    // the same state is attached to the root router via `with_state(state)`.
    let admin_api_routes = admin_api_routes.layer(middleware::from_fn_with_state(
        state.clone(),
        admin_auth_middleware,
    ));

    // Top-level admin router: SPA shell at `/admin` and `/admin/`, the OAuth
    // callback page, the protected REST API under `/admin/api/*`, the WS upgrade,
    // and the two public endpoints. Anything else under `/admin/*` falls through
    // to `admin_ui::serve_asset`, which serves an embedded asset or (unknown
    // path) the SPA shell, whose hash-router takes over.
    //
    // Auth scope:
    //   - `/admin/api/*`         — auth middleware (above)
    //   - `/admin/ws`            — per-handler auth (`handlers::admin::usage::usage_stream`)
    //   - `/admin/health`        — public (LB probes)
    //   - `/admin/oauth/callback` — public (browser callback)
    //   - everything else        — public (SPA shell + assets)
    Router::new()
        // axum 0.7+ treats trailing-slash and no-trailing-slash as distinct
        // paths. Only "/" is registered here: axum 0.8 rejects empty-string route
        // paths, and the outer `.nest("/admin", admin_routes)` covers the
        // no-trailing-slash form via the SPA fallback.
        .route("/", get(admin_ui::index_html))
        .route(
            "/callback.html",
            get(admin_ui::callback_html).post(admin_ui::callback_html),
        )
        .route("/health", get(handlers::admin::runtime::admin_health))
        .route(
            "/oauth/callback",
            get(handlers::admin::oauth::oauth_callback),
        )
        .route("/ws", get(handlers::admin::usage::usage_stream))
        // i18n string packs, public (no auth): `loadLang('en')` runs at boot
        // before the SPA can attach the admin Bearer token, and the packs hold
        // only generic UI labels. Registered here (not under `/api`) to stay
        // outside the auth middleware.
        //
        // Route pattern: axum 0.8 rejects `/i18n/{lang}.json` ("Only one
        // parameter is allowed per path segment"), so `/i18n/{lang}` is
        // registered instead — it matches `/i18n/en.json` as one segment and
        // captures `lang = "en.json"`. The handler strips the optional `.json`
        // and validates the code; path-traversal guard, cache headers and
        // extension parsing in `admin_ui::serve_i18n`.
        .route("/i18n/{lang}", get(admin_ui::serve_i18n))
        .nest("/api", admin_api_routes)
        .fallback(admin_ui::serve_asset)
}

/// `GET /v1/health` — unauthenticated liveness probe.
///
/// Security (OP-26): returns `{"status": "ok"}` only. The exact binary
/// version used to be exposed here without authentication, handing attackers
/// a precise fingerprint for mapping the install to future advisories; it now
/// lives behind the admin API (`GET /admin/api/version`).
async fn health() -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
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
        let master_key = Arc::new(MasterKey::generate().unwrap());
        let adapters = Arc::new(RwLock::new(Arc::new(
            Vec::<adapters::ProviderAdapterEnum>::new(),
        )));
        let mut config = AppConfig::default();
        config.server.allow_anonymous = true;
        AppState::for_test(config, db_pool, master_key, adapters)
    }

    fn fresh_pool() -> (core_db::DbPool, PathBuf) {
        let pool =
            core_db::DbPool::test_pool_with_prefix("openproxy-router-test").expect("open pool");
        let path = pool.path().to_path_buf();
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

        assert_eq!(
            response
                .headers()
                .get(axum::http::header::X_CONTENT_TYPE_OPTIONS)
                .unwrap(),
            "nosniff"
        );
        assert_eq!(response.headers().get("x-frame-options").unwrap(), "DENY");
        let csp = response
            .headers()
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            csp.starts_with("default-src 'self'; script-src 'self';"),
            "{csp}"
        );
        assert!(csp.contains("style-src-elem 'self';"), "{csp}");
        assert!(
            csp.ends_with("connect-src 'self';"),
            "no Host header → no ws origins: {csp}"
        );

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["status"], "ok");
    }

    #[tokio::test]
    async fn test_models_catalog_requires_key_unless_anonymous_opt_in() {
        // No active keys + `allow_anonymous = false` (the default): the catalog
        // must not leak. Same gate as chat.
        let (pool, _path) = fresh_pool();
        let db_pool = Arc::new(pool);
        let master_key = Arc::new(MasterKey::generate().unwrap());
        let adapters = Arc::new(RwLock::new(Arc::new(
            Vec::<adapters::ProviderAdapterEnum>::new(),
        )));
        let config = AppConfig::default();
        assert!(!config.server.allow_anonymous, "default must be closed");
        let state = AppState::for_test(config, db_pool, master_key, adapters);
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // The explicit opt-in (`make_state()`) keeps the first-boot window open.
        let app = build_router(make_state().await);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_csp_pins_ws_origin_to_host_header() {
        let state = make_state().await;
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/health")
                    .header("host", "listo.click")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let csp = response
            .headers()
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            csp.ends_with("connect-src 'self' ws://listo.click wss://listo.click;"),
            "{csp}"
        );
        assert!(!csp.contains("connect-src 'self' ws: wss:"), "{csp}");
        assert!(csp.contains("frame-ancestors 'none';"), "{csp}");
    }

    #[tokio::test]
    async fn test_admin_api_fallback_404_json() {
        // Unmatched /admin/api/* must return JSON 404, not the HTML SPA shell.
        let state = make_state().await;
        let app = build_router(state.clone()).layer(axum::Extension(
            axum::extract::connect_info::ConnectInfo(
                "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
            ),
        ));

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
                    .header("Authorization", format!("Bearer {api_key}"))
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

        assert_eq!(
            response
                .headers()
                .get(axum::http::header::X_CONTENT_TYPE_OPTIONS)
                .unwrap(),
            "nosniff"
        );
        assert_eq!(response.headers().get("x-frame-options").unwrap(), "DENY");
        let csp = response
            .headers()
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            csp.starts_with("default-src 'self'; script-src 'self';"),
            "{csp}"
        );
        assert!(csp.contains("style-src-elem 'self';"), "{csp}");
        assert!(
            csp.ends_with("connect-src 'self';"),
            "no Host header → no ws origins: {csp}"
        );

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "not_found");
    }

    #[tokio::test]
    async fn test_transport_compression_json_gzip() {
        let state = make_state().await;
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .header("Accept-Encoding", "gzip")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-encoding")
                .and_then(|v| v.to_str().ok()),
            Some("gzip")
        );
    }

    #[tokio::test]
    async fn test_transport_compression_json_gzip_fallback_or_bypass() {
        let state = make_state().await;
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .header("Accept-Encoding", "gzip, zstd")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-encoding")
                .and_then(|v| v.to_str().ok()),
            Some("gzip")
        );
    }

    #[tokio::test]
    async fn test_image_generations_not_found_returns_404() {
        let state = make_state().await;
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/images/generations")
                    .header("Content-Type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"prompt":"a painting of a sunset","model":"nonexistent-image-model"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_image_generations_empty_prompt_validation() {
        let state = make_state().await;
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/images/generations")
                    .header("Content-Type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"prompt":"","model":"dall-e-3"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_embeddings_not_found_returns_404() {
        let state = make_state().await;
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/embeddings")
                    .header("Content-Type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"input":"hello world","model":"nonexistent-embedding-model"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_embeddings_empty_input_validation() {
        let state = make_state().await;
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/embeddings")
                    .header("Content-Type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"input":"","model":"text-embedding-3-small"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_image_edits_missing_image_returns_400() {
        let state = make_state().await;
        let app = build_router(state);

        let boundary = "------------------------boundary123";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nedit test\r\n--{boundary}--\r\n"
        );

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/images/edits")
                    .header(
                        "Content-Type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_image_edits_missing_prompt_returns_400() {
        let state = make_state().await;
        let app = build_router(state);

        let boundary = "------------------------boundary123";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"image.png\"\r\nContent-Type: image/png\r\n\r\nfakeimagebytes\r\n--{boundary}--\r\n"
        );

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/images/edits")
                    .header(
                        "Content-Type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_image_variations_missing_image_returns_400() {
        let state = make_state().await;
        let app = build_router(state);

        let boundary = "------------------------boundary123";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"n\"\r\n\r\n1\r\n--{boundary}--\r\n"
        );

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/images/variations")
                    .header(
                        "Content-Type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_image_edits_not_found_returns_404() {
        let state = make_state().await;
        let app = build_router(state);

        let boundary = "------------------------boundary123";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nnonexistent-image-model\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nedit test\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"image.png\"\r\nContent-Type: image/png\r\n\r\nfakeimagebytes\r\n--{boundary}--\r\n"
        );

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/images/edits")
                    .header(
                        "Content-Type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
