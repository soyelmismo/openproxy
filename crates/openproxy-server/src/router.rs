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
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            axum::http::header::X_CONTENT_TYPE_OPTIONS,
            axum::http::HeaderValue::from_static("nosniff"),
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("x-frame-options"),
            axum::http::HeaderValue::from_static("DENY"),
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("content-security-policy"),
            axum::http::HeaderValue::from_static("default-src 'self'"),
        ))
}

fn build_admin_router(state: &AppState) -> Router<AppState> {
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
    let admin_api_routes = handlers::admin::admin_api_routes();

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
    Router::new()
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
        .fallback(admin_ui::serve_asset)
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
        let adapters = Arc::new(RwLock::new(Arc::new(
            Vec::<adapters::ProviderAdapterEnum>::new(),
        )));
        let mut config = AppConfig::default();
        config.server.allow_anonymous = true;
        AppState::for_test(config, db_pool, master_key, adapters)
    }

    fn fresh_pool() -> (core_db::DbPool, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("openproxy-router-test-{pid}-{nanos}-{n}"));
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

        assert_eq!(response.headers().get(axum::http::header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
        assert_eq!(response.headers().get("x-frame-options").unwrap(), "DENY");
        assert_eq!(response.headers().get("content-security-policy").unwrap(), "default-src 'self'");

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["status"], "ok");
    }

    #[tokio::test]
    async fn test_admin_api_fallback_404_json() {
        // Unmatched /admin/api/* routes should return JSON 404, not HTML
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

        assert_eq!(response.headers().get(axum::http::header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
        assert_eq!(response.headers().get("x-frame-options").unwrap(), "DENY");
        assert_eq!(response.headers().get("content-security-policy").unwrap(), "default-src 'self'");

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
    async fn test_transport_compression_json_zstd() {
        let state = make_state().await;
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .header("Accept-Encoding", "zstd")
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
            Some("zstd")
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
