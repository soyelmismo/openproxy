//! HTTP router.
//!
//! Spec §2: every public + admin endpoint is wired here, in axum 0.8
//! syntax. Routes are grouped into nested sub-routers (`chat_routes`,
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
//! | `GET  /admin`                 | SPA shell (`admin_ui::index_html`)        |
//! | `GET  /admin/`                | SPA shell (`admin_ui::index_html`)        |
//! | `GET  /admin/callback.html`   | OAuth callback page (`admin_ui::callback_html`) |
//! | `GET  /admin/dist/*`          | embedded bundle (`admin_ui::serve_asset`) |
//! | `GET  /admin/styles/*`        | embedded CSS (`admin_ui::serve_asset`)    |
//! | `GET  /admin/fonts/*`         | embedded fonts (`admin_ui::serve_asset`)  |
//! | `*    /admin/api/*`           | admin REST API (auth-protected)           |
//! | `GET  /admin/ws`              | live-logs WebSocket (own auth via `?token=`) |
//! | `GET  /admin/health`          | `admin_health` (unauthenticated, kept public for LB probes) |
//! | `GET  /admin/oauth/callback`  | `oauth_callback` (unauthenticated, browser callback) |
//!
//! The dashboard SPA loads BEFORE auth: `index.html`, `callback.html`,
//! and every `/admin/dist/*` / `/admin/styles/*` / `/admin/fonts/*`
//! asset are served without checking credentials. The SPA itself
//! sends the admin API key as a Bearer token on each `/admin/api/*`
//! call. The WebSocket upgrade at `/admin/ws` does its own auth
//! inside the handler (`usage_stream`) so it can accept `?token=`
//! in the query string (browsers can't set headers on WS handshakes).

use axum::{
    Json, Router, middleware,
    routing::{get, post, put},
};
use serde_json::json;

use crate::{
    admin_ui,
    disconnect::client_disconnect_middleware,
    handlers::{self, admin::admin_auth_middleware},
    middleware::request_id,
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
    let chat_routes = Router::new()
        .route("/v1/models", get(handlers::models::list_models))
        .route(
            "/v1/chat/completions",
            post(handlers::chat::chat_completions),
        )
        .layer(middleware::from_fn(client_disconnect_middleware));

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
    // handler (`usage_stream`) also accepts `?token=` in the query
    // string — that path is handled inside the handler itself
    // (the middleware would not see the WS upgrade as a normal
    // request), so the per-handler auth check there is the source
    // of truth for the WebSocket path.
    let admin_api_routes = Router::new()
        .route(
            "/config",
            get(handlers::admin::get_runtime_config),
        )
        .route(
            "/config/timeouts",
            axum::routing::put(handlers::admin::put_runtime_timeouts),
        )
        .route(
            "/config/recording-ttl",
            get(handlers::admin::get_recording_ttl)
                .put(handlers::admin::put_recording_ttl),
        )
        .route(
            "/config/compression",
            axum::routing::put(handlers::admin::put_runtime_compression),
        )
        .route(
            "/config/idle-chunk-retryable",
            axum::routing::put(handlers::admin::put_idle_chunk_retryable),
        )
        .route(
            "/providers",
            get(handlers::admin::list_providers).post(handlers::admin::create_provider),
        )
        .route(
            "/providers/{id}",
            get(handlers::admin::get_provider)
                .delete(handlers::admin::delete_provider)
                .patch(handlers::admin::update_provider),
        )
        .route(
            "/accounts",
            get(handlers::admin::list_accounts).post(handlers::admin::create_account),
        )
        .route(
            "/accounts/{id}",
            axum::routing::delete(handlers::admin::delete_account),
        )
        .route(
            "/accounts/{id}/health",
            post(handlers::admin::set_account_health),
        )
        .route(
            "/accounts/{id}/api-key",
            put(handlers::admin::update_account_api_key),
        )
        .route(
            "/accounts/{id}/refresh-quota",
            post(handlers::admin::refresh_account_quota),
        )
        .route(
            "/combos",
            get(handlers::admin::list_combos).post(handlers::admin::create_combo),
        )
        .route(
            "/combos/{id}",
            get(handlers::admin::get_combo)
                .delete(handlers::admin::delete_combo)
                .patch(handlers::admin::update_combo),
        )
        .route(
            "/combos/{id}/test-all",
            post(handlers::admin::test_combo_targets),
        )
        .route(
            "/combos/{id}/targets",
            get(handlers::admin::list_combo_targets).post(handlers::admin::add_target),
        )
        // IMPORTANT: this literal-segment route MUST be registered
        // before `/combos/{id}/targets/:target_id`. axum 0.7
        // matches routes in registration order; if `:target_id` is
        // registered first it would happily swallow `valid-sub-combos`
        // and 405 the GET (because the :target_id route only allows
        // PATCH and DELETE).
        .route(
            "/combos/{id}/targets/valid-sub-combos",
            get(handlers::admin::list_valid_sub_combos),
        )
        // IMPORTANT: this literal-segment route MUST be registered
        // before `/combos/{id}/targets/:target_id`. axum 0.7
        // matches routes in registration order; if `:target_id` is
        // registered first it would happily swallow `reorder` and
        // 405 every POST (because the :target_id route only allows
        // PATCH and DELETE).
        .route(
            "/combos/{id}/targets/reorder",
            axum::routing::post(handlers::admin::reorder_combo_targets),
        )
        // IMPORTANT: this literal-segment route MUST be registered
        // before `/combos/{id}/targets/:target_id`. axum 0.7
        // matches routes in registration order; if `:target_id` is
        // registered first it would happily swallow `clear-cooldown`
        // and 405 every POST (because the :target_id route only allows
        // PATCH and DELETE).
        .route(
            "/combos/{id}/targets/{target_id}/clear-cooldown",
            axum::routing::post(handlers::admin::clear_combo_target_cooldown),
        )
        .route(
            "/combos/{id}/targets/{target_id}",
            axum::routing::patch(handlers::admin::update_combo_target)
                .delete(handlers::admin::delete_combo_target),
        )
        .route(
            "/usage/summary",
            get(handlers::admin::usage_summary),
        )
        .route(
            "/usage/by-model",
            get(handlers::admin::usage_by_model),
        )
        .route(
            "/usage/by-provider",
            get(handlers::admin::usage_by_provider),
        )
        .route(
            "/usage/monthly-by-provider",
            get(handlers::admin::usage_monthly_by_provider),
        )
        .route(
            "/usage/by-day",
            get(handlers::admin::usage_by_day),
        )
        .route(
            "/usage/by-account",
            get(handlers::admin::usage_by_account),
        )
        .route(
            "/usage/by-status",
            get(handlers::admin::usage_by_status),
        )
        .route(
            "/usage/errors",
            get(handlers::admin::usage_errors),
        )
        .route(
            "/usage/latency",
            get(handlers::admin::usage_latency),
        )
        .route(
            "/usage/races",
            get(handlers::admin::usage_races),
        )
        .route(
            "/usage/recent",
            get(handlers::admin::usage_recent),
        )
        .route(
            "/usage/detail",
            get(handlers::admin::usage_detail),
        )
        .route(
            "/debug/logs",
            get(handlers::admin::debug_logs),
        )
        .route(
            "/debug/clear",
            post(handlers::admin::debug_logs_clear),
        )
        .route(
            "/recording",
            get(handlers::admin::get_recording).post(handlers::admin::set_recording),
        )
        .route(
            "/models/{id}/refresh",
            post(handlers::admin::refresh_models),
        )
        .route(
            "/models/{id}/toggle",
            post(handlers::admin::toggle_model),
        )
        .route(
            "/models/bulk-toggle",
            post(handlers::admin::bulk_toggle_models),
        )
        .route(
            "/models/{id}",
            axum::routing::delete(handlers::admin::delete_model),
        )
        .route(
            "/models",
            get(handlers::admin::list_models_admin),
        )
        .route(
            "/models/custom",
            post(handlers::admin::create_custom_model),
        )
        .route(
            "/models/{id}/test",
            post(handlers::admin::test_model),
        )
        .route(
            "/providers/{id}/refresh",
            post(handlers::admin::refresh_provider_models),
        )
        .route(
            "/providers/{id}/active",
            post(handlers::admin::set_provider_active),
        )
        .route(
            "/keys",
            get(handlers::admin::list_api_keys).post(handlers::admin::create_api_key),
        )
        .route(
            "/keys/{id}",
            get(handlers::admin::get_api_key)
                .patch(handlers::admin::update_api_key)
                .delete(handlers::admin::delete_api_key),
        )
        .route(
            "/keys/{id}/revoke",
            post(handlers::admin::revoke_api_key),
        )
        .route(
            "/keys/{id}/regenerate",
            post(handlers::admin::regenerate_api_key),
        )
        .route(
            "/keys/{id}/usage",
            get(handlers::admin::api_key_usage),
        )
        // OAuth endpoints
        // models.dev sync
        .route(
            "/models/sync-models-dev",
            post(handlers::admin::sync_models_dev),
        )
        // Re-price historical usage rows after models.dev sync
        .route(
            "/usage/recompute-costs",
            post(handlers::admin::recompute_usage_costs),
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
            get(handlers::admin::list_notifications),
        )
        .route(
            "/notifications/read-all",
            post(handlers::admin::mark_all_notifications_read),
        )
        .route(
            "/notifications/unread-count",
            get(handlers::admin::notifications_unread_count),
        )
        .route(
            "/notifications/{id}/read",
            post(handlers::admin::mark_notification_read),
        )
        .route(
            "/notifications/{id}/archive",
            post(handlers::admin::archive_notification),
        )
        .route(
            "/notifications/{id}",
            axum::routing::delete(handlers::admin::delete_notification),
        )
        .route(
            "/oauth/{provider}/authorize",
            get(handlers::admin::oauth_authorize),
        )
        .route(
            "/oauth/{provider}/exchange",
            post(handlers::admin::oauth_exchange),
        )
        .route(
            "/oauth/{provider}/device-code",
            post(handlers::admin::oauth_device_code),
        )
        .route(
            "/oauth/{provider}/device-poll",
            post(handlers::admin::oauth_device_poll),
        )
        // NOTE: `/oauth/callback` is intentionally NOT
        // registered here — it lives at `/admin/oauth/callback`
        // (top-level public route in `admin_routes` below, the
        // browser-callback URL, no auth required).
        ;

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
    //   - `/admin/ws`          — per-handler auth (`usage_stream`)
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
        .route("/health", get(handlers::admin::admin_health))
        .route("/oauth/callback", get(handlers::admin::oauth_callback))
        .route("/ws", get(handlers::admin::usage_stream))
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
        .route("/v1/health", get(health))
        .merge(chat_routes)
        .nest("/admin", admin_routes)
        .layer(middleware::from_fn(request_id))
        // MEDIUM fix (audit finding #8): axum's default body limit is
        // 2 MiB, which is too small for a single legitimate prompt (some
        // long-context requests attach tens of KiB of system prompt +
        // tool definitions) and has no project-wide ceiling for the
        // admin JSON extractors (POST /admin/api/combos/{id}/targets,
        // bulk_toggle_models, reorder_combo_targets, etc.). Raising to
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
