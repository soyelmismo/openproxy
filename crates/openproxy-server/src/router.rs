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
//! | `GET  /admin/panel`           | embedded tile SPA shell (`admin_ui::index_html`) |
//! | `GET  /admin/panel/`          | embedded tile SPA shell (`admin_ui::index_html`) |
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
    apply_layers(public_routes(&state).merge(admin_routes(&state)), state)
}

/// Public listener: no dashboard, admin API, or admin redirects.
pub fn build_public_router(state: AppState) -> Router {
    apply_layers(public_routes(&state), state)
}

/// Dedicated administrative listener, retaining the same authentication policy.
pub fn build_admin_listener_router(state: AppState) -> Router {
    apply_layers(admin_routes(&state), state)
}

fn public_routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/health", get(health))
        .merge(handlers::public_api_routes(state))
}

fn admin_routes(state: &AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/",
            get(|| async { axum::response::Redirect::temporary("/admin") }),
        )
        .route(
            "/admin/",
            get(|| async { axum::response::Redirect::temporary("/admin") }),
        )
        // Providers whose authorization allowlist only accepts a literal
        // `http://127.0.0.1:<port>/callback` redirect (Devin) land on this
        // root-level twin of `/admin/callback.html`. Same unauthenticated
        // bridge page: it postMessages the `code` back to the dashboard
        // opener and shows it for manual paste.
        .route(
            "/callback",
            get(admin_ui::callback_html).post(admin_ui::callback_html),
        )
        .nest("/admin", build_admin_router(state))
}

fn apply_layers(routes: Router<AppState>, state: AppState) -> Router {
    // Security (OP-14): honor the configured `server.request_max_body_bytes`
    // (default 10 MiB) instead of a hardcoded 32 MiB that ignored the setting.
    // Admin backup restore raises its own per-route limit (see backup.rs).
    let request_body_limit = state.config().server.request_max_body_bytes;
    // Cloned for the outermost security-headers layer, which needs the
    // trusted-proxy config for conditional HSTS (OP-21).
    let state_clone = state.clone();

    routes
        .layer(crate::middleware::compression::transport_compression_layer())
        // Innermost: strip JetBrains JBR25 `Upgrade: h2c` preambles so those
        // clients fall back to plain HTTP/1.1 (see `middleware::h2c`). Only
        // touches the literal h2c token — WebSocket handshakes are untouched.
        .layer(middleware::from_fn(crate::middleware::h2c::h2c_downgrade))
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
    // to `admin_ui::serve_asset`, which serves allowlisted assets or 404.
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
        .route("/panel", get(admin_ui::index_html))
        .route("/panel/", get(admin_ui::index_html))
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
mod tests;
