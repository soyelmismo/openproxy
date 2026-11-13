//! `/admin/ws` credential contract: Bearer header or single-use ticket —
//! never a raw API key in the query string.

use super::common::*;
use crate::handlers::admin::auth::AdminIdentity;
use http_body_util::BodyExt;

/// Mirror of what `usage_stream` does before upgrading: parse the query
/// exactly like axum's `Query<UsageStreamQuery>` extractor, then run the
/// shared auth routine. (A full `oneshot` handshake is not possible here:
/// `WebSocketUpgrade` needs hyper's `OnUpgrade` extension, which only a
/// real HTTP/1.1 connection provides, so the extractor fails with 426
/// before the handler — and therefore before auth — ever runs.)
async fn ws_auth(
    state: &AppState,
    uri: &str,
    bearer: Option<&str>,
) -> Result<AdminIdentity, crate::error::ApiError> {
    let uri: axum::http::Uri = uri.parse().unwrap();
    let axum::extract::Query(q) =
        axum::extract::Query::<crate::handlers::admin::usage::UsageStreamQuery>::try_from_uri(&uri)
            .expect("query parses");
    let mut headers = HeaderMap::new();
    headers.insert("host", "127.0.0.1:8787".parse().unwrap());
    headers.insert("origin", "http://127.0.0.1:8787".parse().unwrap());
    headers.insert("upgrade", "websocket".parse().unwrap());
    if let Some(b) = bearer {
        headers.insert("authorization", format!("Bearer {b}").parse().unwrap());
    }
    let addr = "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap();
    authenticate_admin_ws(state, &headers, q.ticket.as_deref(), Some(&addr)).await
}

fn app_for(state: &AppState) -> Router {
    crate::router::build_router(state.clone()).layer(axum::Extension(
        axum::extract::connect_info::ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ),
    ))
}

async fn mint_ticket(app: &Router, key: &str) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/api/ws-ticket")
                .header("Authorization", format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["expires_in_secs"], 30);
    let ticket = v["ticket"].as_str().expect("ticket string").to_string();
    assert_eq!(ticket.len(), 32);
    assert!(ticket.bytes().all(|b| b.is_ascii_alphanumeric()));
    ticket
}

#[tokio::test]
async fn ws_ticket_endpoint_requires_manage_key() {
    let tmp = tempdir();
    let (state, _key) = make_state_with_key(tmp.path()).await;
    let app = app_for(&state);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/api/ws-ticket")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/api/ws-ticket")
                .header("Authorization", "Bearer not-a-real-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(state.ws_tickets().is_empty());
}

#[tokio::test]
async fn ws_upgrade_accepts_ticket_once_and_rejects_replay() {
    let tmp = tempdir();
    let (state, key) = make_state_with_key(tmp.path()).await;
    let app = app_for(&state);
    let ticket = mint_ticket(&app, &key).await;
    assert_eq!(state.ws_tickets().len(), 1);

    let identity = ws_auth(&state, &format!("/admin/ws?ticket={ticket}"), None)
        .await
        .expect("fresh ticket must authenticate");
    assert!(
        identity.key_id().is_some(),
        "identity bound to the minting key"
    );
    assert!(state.ws_tickets().is_empty(), "ticket consumed on use");

    let replay = ws_auth(&state, &format!("/admin/ws?ticket={ticket}"), None).await;
    assert!(replay.is_err(), "replayed ticket must be rejected");
}

#[tokio::test]
async fn ws_upgrade_rejects_raw_key_in_query_string() {
    let tmp = tempdir();
    let (state, key) = make_state_with_key(tmp.path()).await;

    // The legacy `?token=<key>` leaked the long-lived secret into proxy
    // access logs; it must no longer authenticate anything.
    assert!(
        ws_auth(&state, &format!("/admin/ws?token={key}"), None)
            .await
            .is_err()
    );

    // …and a valid key is not a valid ticket either.
    assert!(
        ws_auth(&state, &format!("/admin/ws?ticket={key}"), None)
            .await
            .is_err()
    );

    // No credentials at all.
    assert!(ws_auth(&state, "/admin/ws", None).await.is_err());

    // Bearer header (non-browser clients) keeps working.
    let identity = ws_auth(&state, "/admin/ws", Some(&key))
        .await
        .expect("bearer header");
    assert!(identity.key_id().is_some());
}

#[tokio::test]
async fn ws_ticket_is_rejected_when_bound_key_is_revoked() {
    let tmp = tempdir();
    let (state, key) = make_state_with_key(tmp.path()).await;
    let app = app_for(&state);
    let ticket = mint_ticket(&app, &key).await;

    {
        let w = state.db_pool().writer();
        w.execute("UPDATE api_keys SET is_active = 0", [])
            .expect("revoke keys");
    }
    state.invalidate_api_key_cache(None);

    assert!(
        ws_auth(&state, &format!("/admin/ws?ticket={ticket}"), None)
            .await
            .is_err()
    );
    assert!(
        state.ws_tickets().is_empty(),
        "a failed redemption still burns the ticket"
    );
}

#[tokio::test]
async fn admin_middleware_exposes_identity_to_handlers() {
    let tmp = tempdir();
    let (state, key) = make_state_with_key(tmp.path()).await;
    let addr = "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {key}").parse().unwrap());

    let identity: AdminIdentity =
        crate::handlers::admin::auth::authenticate_admin(&state, &headers, Some(&addr))
            .await
            .expect("valid key");
    assert!(identity.key_id().is_some());
    assert_eq!(identity.remote_addr, Some(addr));
    assert!(
        identity
            .key
            .as_ref()
            .unwrap()
            .scopes
            .iter()
            .any(|s| s == "manage")
    );
}

#[tokio::test]
async fn admin_middleware_resolves_real_ip_behind_proxy() {
    let tmp = tempdir();
    let mut config = openproxy_core::AppConfig::default();
    config.server.trusted_proxies = vec!["127.0.0.1".to_string()];
    let (state, key) = make_state_with_key_and_config(tmp.path(), config).await;
    let addr = "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {key}").parse().unwrap());
    headers.insert("x-real-ip", "198.51.100.42".parse().unwrap());

    let identity: AdminIdentity =
        crate::handlers::admin::auth::authenticate_admin(&state, &headers, Some(&addr))
            .await
            .expect("valid key");
    assert_eq!(
        identity.client_ip(),
        Some("198.51.100.42".parse::<std::net::IpAddr>().unwrap())
    );
    assert_eq!(
        identity.remote_addr,
        Some(
            "198.51.100.42:12345"
                .parse::<std::net::SocketAddr>()
                .unwrap()
        )
    );
}

#[tokio::test]
async fn admin_middleware_rejects_spoofed_real_ip_from_untrusted_peer() {
    let tmp = tempdir();
    let (state, key) = make_state_with_key(tmp.path()).await;
    let untrusted_addr = "203.0.113.10:12345"
        .parse::<std::net::SocketAddr>()
        .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {key}").parse().unwrap());
    headers.insert("x-real-ip", "1.1.1.1".parse().unwrap());

    let identity: AdminIdentity =
        crate::handlers::admin::auth::authenticate_admin(&state, &headers, Some(&untrusted_addr))
            .await
            .expect("valid key");
    // Must NOT be 1.1.1.1 — must be the actual peer IP 203.0.113.10
    assert_eq!(
        identity.client_ip(),
        Some("203.0.113.10".parse::<std::net::IpAddr>().unwrap())
    );
    assert_eq!(identity.remote_addr, Some(untrusted_addr));
}

#[tokio::test]
async fn analytics_scope_gets_client_allowlist_only() {
    // Client keys (analytics scope, no "manage") get ONLY the explicit client
    // allowlist: their own tenanted analytics, auth/me, the client-facing
    // model catalog and their own plan status. Everything operator — keys,
    // config, plan/order management, ws-ticket — stays 403, so a customer key
    // can never reach an admin route regardless of the login screen it used.
    let tmp = tempdir();
    // Credits enabled so the self-service balance/quota endpoints answer 200
    // instead of 503 (credits_disabled).
    let mut config = openproxy_core::AppConfig::default();
    config.credits = Some(openproxy_types::config::CreditsConfig {
        rpc_url: "http://127.0.0.1:1".into(),
        usdc_address: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913".into(),
        collector_address: "0xc46e0bb29db2c74b2ca63cd7d2b61374dadd8ce6".into(),
        payment_window_minutes: 15,
        scan_interval_secs: 5,
        min_topup_usd: 1.0,
        ttl_days: vec![7, 15, 30],
        chain_id: 8453,
    });
    let (state, _admin_key) = make_state_with_key_and_config(tmp.path(), config).await;

    let analytics_raw_key = "op_live_analytics_test_secret_key_123456";
    let key_hash = openproxy_core::api_keys::hash_key(analytics_raw_key);
    let analytics_key_id: i64 = {
        let w = state.db_pool().writer();
        w.execute(
            "INSERT INTO api_keys (key_hash, key_prefix, label, scopes_json, allowed_models_json, is_active, created_at)
             VALUES (?1, 'op_live_analyt', 'Analytics Guest', '[\"analytics\"]', '[]', 1, '2026-11-05 12:00:00')",
            rusqlite::params![key_hash],
        )
        .expect("insert analytics key");
        w.last_insert_rowid()
    };

    let app = app_for(&state);

    // Allowlist: identity, tenanted usage, client catalog, own credits.
    for uri in [
        "/admin/api/auth/me",
        "/admin/api/usage/summary",
        "/admin/api/formats",
        "/admin/api/models/catalog",
        "/admin/api/credits/quota",
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .header("Authorization", format!("Bearer {analytics_raw_key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "analytics key must reach the client allowlist on {uri}"
        );
    }

    // The retired prepaid surfaces are gone and unreachable for a client key:
    // there is no balance to read and no manual top-up to create.
    for (method, uri) in [
        ("GET", "/admin/api/credits/balance"),
        ("GET", "/admin/api/credits/transactions"),
        ("POST", "/admin/api/credits/topup"),
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("Authorization", format!("Bearer {analytics_raw_key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            resp.status() == StatusCode::FORBIDDEN || resp.status() == StatusCode::NOT_FOUND,
            "{method} {uri} must not exist for a client key, got {}",
            resp.status()
        );
    }

    // Deny by default: operator routes stay 403 for client keys.
    for (method, uri) in [
        ("GET", "/admin/api/keys"),
        ("GET", "/admin/api/config"),
        ("GET", "/admin/api/credits/orders"),
        ("POST", "/admin/api/ws-ticket"),
    ] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("Authorization", format!("Bearer {analytics_raw_key}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "analytics key must be rejected on {method} {uri}"
        );
    }

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/api/ws-ticket")
                .header("Authorization", format!("Bearer {analytics_raw_key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(state.ws_tickets().is_empty());
}
