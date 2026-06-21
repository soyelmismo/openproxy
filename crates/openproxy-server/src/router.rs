//! HTTP router.
//!
//! Spec §2: every public + admin endpoint is wired here, in axum 0.7
//! syntax. Routes are grouped into two sub-routers (`chat_routes` and
//! `admin_routes`) for readability, then merged into the root `Router`.
//! The request-id middleware sits on the outermost layer so every
//! response — public or admin — carries an `x-request-id` header.

use axum::{
    middleware,
    routing::{get, post, put},
    Json, Router,
};
use serde_json::json;

use crate::{
    disconnect::client_disconnect_middleware,
    handlers::{self, admin::admin_auth_middleware},
    middleware::request_id,
    state::AppState,
};

/// Build the root [`Router`] for the server.
///
/// The state is attached via `with_state` so individual handlers can
/// accept `State<AppState>` in their extractor list. The request-id
/// middleware is applied at the outermost layer.
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

    // Admin surface (spec §2.3). CRUD for providers, accounts, combos,
    // plus read-only usage analytics.
    //
    // Authorization model: every admin route EXCEPT the liveness
    // probe (`/v1/admin/health`) and the OAuth browser callback
    // (`/v1/admin/oauth/callback`) requires a `manage`-scope API
    // key, verified by [`admin_auth_middleware`]. The two exempt
    // routes are intentionally public: the liveness probe is for
    // load balancers and uptime monitors that should not need
    // credentials, and the OAuth callback is the URL the upstream
    // provider (Google, etc.) redirects the user's browser to —
    // by design the browser arrives without admin credentials, and
    // the handler just echoes back the `code` for the user to copy
    // into the dashboard.
    //
    // The middleware reads only the `Authorization` header, which
    // is the contract for the HTTP path. The WebSocket upgrade
    // handler (`usage_stream`) also accepts `?token=` in the query
    // string — that path is handled inside the handler itself
    // (the middleware would not see the WS upgrade as a normal
    // request), so the per-handler auth check there is the source
    // of truth for the WebSocket path.
    let admin_public_routes = Router::new()
        .route(
            "/v1/admin/health",
            get(handlers::admin::admin_health),
        )
        .route(
            "/v1/admin/oauth/callback",
            get(handlers::admin::oauth_callback),
        );

    let admin_routes = Router::new()
        .route(
            "/v1/admin/config",
            get(handlers::admin::get_runtime_config),
        )
        .route(
            "/v1/admin/config/timeouts",
            axum::routing::put(handlers::admin::put_runtime_timeouts),
        )
        .route(
            "/v1/admin/config/recording-ttl",
            get(handlers::admin::get_recording_ttl)
                .put(handlers::admin::put_recording_ttl),
        )
        .route(
            "/v1/admin/config/compression",
            axum::routing::put(handlers::admin::put_runtime_compression),
        )
        .route(
            "/v1/admin/config/idle-chunk-retryable",
            axum::routing::put(handlers::admin::put_idle_chunk_retryable),
        )
        .route(
            "/v1/admin/providers",
            get(handlers::admin::list_providers).post(handlers::admin::create_provider),
        )
        .route(
            "/v1/admin/providers/:id",
            get(handlers::admin::get_provider)
                .delete(handlers::admin::delete_provider)
                .patch(handlers::admin::update_provider),
        )
        .route(
            "/v1/admin/accounts",
            get(handlers::admin::list_accounts).post(handlers::admin::create_account),
        )
        .route(
            "/v1/admin/accounts/:id",
            axum::routing::delete(handlers::admin::delete_account),
        )
        .route(
            "/v1/admin/accounts/:id/health",
            post(handlers::admin::set_account_health),
        )
        .route(
            "/v1/admin/accounts/:id/api-key",
            put(handlers::admin::update_account_api_key),
        )
        .route(
            "/v1/admin/accounts/:id/refresh-quota",
            post(handlers::admin::refresh_account_quota),
        )
        .route(
            "/v1/admin/combos",
            get(handlers::admin::list_combos).post(handlers::admin::create_combo),
        )
        .route(
            "/v1/admin/combos/:id",
            get(handlers::admin::get_combo)
                .delete(handlers::admin::delete_combo)
                .patch(handlers::admin::update_combo),
        )
        .route(
            "/v1/admin/combos/:id/test-all",
            post(handlers::admin::test_combo_targets),
        )
        .route(
            "/v1/admin/combos/:id/targets",
            get(handlers::admin::list_combo_targets).post(handlers::admin::add_target),
        )
        // IMPORTANT: this literal-segment route MUST be registered
        // before `/v1/admin/combos/:id/targets/:target_id`. axum 0.7
        // matches routes in registration order; if `:target_id` is
        // registered first it would happily swallow `valid-sub-combos`
        // and 405 the GET (because the :target_id route only allows
        // PATCH and DELETE).
        .route(
            "/v1/admin/combos/:id/targets/valid-sub-combos",
            get(handlers::admin::list_valid_sub_combos),
        )
        // IMPORTANT: this literal-segment route MUST be registered
        // before `/v1/admin/combos/:id/targets/:target_id`. axum 0.7
        // matches routes in registration order; if `:target_id` is
        // registered first it would happily swallow `reorder` and
        // 405 every POST (because the :target_id route only allows
        // PATCH and DELETE).
        .route(
            "/v1/admin/combos/:id/targets/reorder",
            axum::routing::post(handlers::admin::reorder_combo_targets),
        )
        // IMPORTANT: this literal-segment route MUST be registered
        // before `/v1/admin/combos/:id/targets/:target_id`. axum 0.7
        // matches routes in registration order; if `:target_id` is
        // registered first it would happily swallow `clear-cooldown`
        // and 405 every POST (because the :target_id route only allows
        // PATCH and DELETE).
        .route(
            "/v1/admin/combos/:id/targets/:target_id/clear-cooldown",
            axum::routing::post(handlers::admin::clear_combo_target_cooldown),
        )
        .route(
            "/v1/admin/combos/:id/targets/:target_id",
            axum::routing::patch(handlers::admin::update_combo_target)
                .delete(handlers::admin::delete_combo_target),
        )
        .route(
            "/v1/admin/usage/summary",
            get(handlers::admin::usage_summary),
        )
        .route(
            "/v1/admin/usage/by-model",
            get(handlers::admin::usage_by_model),
        )
        .route(
            "/v1/admin/usage/by-provider",
            get(handlers::admin::usage_by_provider),
        )
        .route(
            "/v1/admin/usage/monthly-by-provider",
            get(handlers::admin::usage_monthly_by_provider),
        )
        .route(
            "/v1/admin/usage/by-account",
            get(handlers::admin::usage_by_account),
        )
        .route(
            "/v1/admin/usage/by-status",
            get(handlers::admin::usage_by_status),
        )
        .route(
            "/v1/admin/usage/errors",
            get(handlers::admin::usage_errors),
        )
        .route(
            "/v1/admin/usage/latency",
            get(handlers::admin::usage_latency),
        )
        .route(
            "/v1/admin/usage/races",
            get(handlers::admin::usage_races),
        )
        .route(
            "/v1/admin/usage/recent",
            get(handlers::admin::usage_recent),
        )
        .route(
            "/v1/admin/usage/stream",
            get(handlers::admin::usage_stream),
        )
        .route(
            "/v1/admin/usage/detail",
            get(handlers::admin::usage_detail),
        )
        .route(
            "/v1/admin/recording",
            get(handlers::admin::get_recording).post(handlers::admin::set_recording),
        )
        .route(
            "/v1/admin/models/:id/refresh",
            post(handlers::admin::refresh_models),
        )
        .route(
            "/v1/admin/models/:id/toggle",
            post(handlers::admin::toggle_model),
        )
        .route(
            "/v1/admin/models/bulk-toggle",
            post(handlers::admin::bulk_toggle_models),
        )
        .route(
            "/v1/admin/models/:id",
            axum::routing::delete(handlers::admin::delete_model),
        )
        .route(
            "/v1/admin/models",
            get(handlers::admin::list_models_admin),
        )
        .route(
            "/v1/admin/models/custom",
            post(handlers::admin::create_custom_model),
        )
        .route(
            "/v1/admin/models/:id/test",
            post(handlers::admin::test_model),
        )
        .route(
            "/v1/admin/providers/:id/refresh",
            post(handlers::admin::refresh_provider_models),
        )
        .route(
            "/v1/admin/providers/:id/active",
            post(handlers::admin::set_provider_active),
        )
        .route(
            "/v1/admin/keys",
            get(handlers::admin::list_api_keys).post(handlers::admin::create_api_key),
        )
        .route(
            "/v1/admin/keys/:id",
            get(handlers::admin::get_api_key)
                .patch(handlers::admin::update_api_key)
                .delete(handlers::admin::delete_api_key),
        )
        .route(
            "/v1/admin/keys/:id/revoke",
            post(handlers::admin::revoke_api_key),
        )
        .route(
            "/v1/admin/keys/:id/regenerate",
            post(handlers::admin::regenerate_api_key),
        )
        .route(
            "/v1/admin/keys/:id/usage",
            get(handlers::admin::api_key_usage),
        )
        // OAuth endpoints
        // models.dev sync
        .route(
            "/v1/admin/models/sync-models-dev",
            post(handlers::admin::sync_models_dev),
        )
        // Re-price historical usage rows after models.dev sync
        .route(
            "/v1/admin/usage/recompute-costs",
            post(handlers::admin::recompute_usage_costs),
        )
        .route(
            "/v1/admin/oauth/:provider/authorize",
            get(handlers::admin::oauth_authorize),
        )
        .route(
            "/v1/admin/oauth/:provider/exchange",
            post(handlers::admin::oauth_exchange),
        )
        .route(
            "/v1/admin/oauth/:provider/device-code",
            post(handlers::admin::oauth_device_code),
        )
        .route(
            "/v1/admin/oauth/:provider/device-poll",
            post(handlers::admin::oauth_device_poll),
        )
        // NOTE: `/v1/admin/oauth/callback` is intentionally NOT
        // registered here — it lives in `admin_public_routes` (the
        // browser-callback URL, no auth required).
        ;

    // Apply the admin auth middleware to the protected admin routes
    // ONLY. The state-clone is required because `from_fn_with_state`
    // takes ownership of the state; we still attach the same state
    // to the root router via `with_state(state)` below.
    let admin_routes = admin_routes.layer(middleware::from_fn_with_state(
        state.clone(),
        admin_auth_middleware,
    ));

    Router::new()
        .route(
            "/v1/health",
            get(health),
        )
        .merge(chat_routes)
        .merge(admin_public_routes)
        .merge(admin_routes)
        .layer(middleware::from_fn(request_id))
        // MEDIUM fix (audit finding #8): axum's default body limit is
        // 2 MiB, which is too small for a single legitimate prompt (some
        // long-context requests attach tens of KiB of system prompt +
        // tool definitions) and has no project-wide ceiling for the
        // admin JSON extractors (POST /v1/admin/combos/:id/targets,
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
