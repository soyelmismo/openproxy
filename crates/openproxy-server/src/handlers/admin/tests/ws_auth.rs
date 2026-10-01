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
