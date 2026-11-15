//! Tests for the zai loopback listener (audit H-1).
//!
//! These drive the listener over a REAL TCP connection to the ephemeral
//! port, verifying the state-binding contract without any upstream network
//! access: an unknown/foreign state must not be consumed, a matching state
//! must be (single use), and the listener must never hold a fixed port.

use super::*;

/// The listener must reject a callback whose state this server never issued:
/// the stored state stays pending (the operator can still complete their
/// flow) and nothing is exchanged.
#[tokio::test]
async fn unknown_state_is_rejected_and_not_consumed() {
    let (state, _temp_dir) = crate::handlers::admin::proxies::tests::create_test_state().await;
    // A pending, server-issued state for the legitimate operator.
    state
        .oauth_states()
        .insert("zai", "legit-state", "verifier-1", "http://localhost:1/cb");

    let (listener, port) = bind_ephemeral().await.expect("bind ephemeral");
    assert_ne!(port, 18432, "must not use the historical fixed port");
    spawn_single_shot_handler(state.clone(), listener, port, "zai".to_string());

    // A hostile/foreign callback with a state we never issued.
    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    use tokio::io::AsyncWriteExt;
    sock.write_all(
        format!("GET /auth/callback-zai?code=ATTACKER-CODE&state=attacker-state HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = Vec::new();
    use tokio::io::AsyncReadExt;
    let _ = sock.read_to_end(&mut buf).await;

    // The listener still answered politely…
    assert!(buf.starts_with(b"HTTP/1.1 200 OK"), "browser gets the HTML");
    // …but the legitimate state was NOT consumed by the attacker's hit.
    assert!(
        state.oauth_states().consume("zai", "legit-state").is_some(),
        "an unknown-state callback must not disturb pending states"
    );
}

/// A callback carrying the state this server issued consumes it exactly
/// once (the exchange itself fails offline; that path is upstream-covered).
#[tokio::test]
async fn matching_state_is_consumed_exactly_once() {
    let (state, _temp_dir) = crate::handlers::admin::proxies::tests::create_test_state().await;
    {
        let pool = state.db_pool();
        let w = pool.writer();
        w.execute(
            "INSERT INTO providers (id, name, base_url, auth_type, format) \
             VALUES ('zai', 'Z.ai', 'https://api.z.ai', 'oauth', 'openai')",
            [],
        )
        .expect("seed zai provider");
    }
    state
        .oauth_states()
        .insert("zai", "issued-state", "verifier-2", "http://localhost:2/cb");

    let (listener, port) = bind_ephemeral().await.expect("bind ephemeral");
    spawn_single_shot_handler(state.clone(), listener, port, "zai".to_string());

    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    use tokio::io::AsyncWriteExt;
    sock.write_all(
        format!("GET /auth/callback-zai?code=REAL-CODE&state=issued-state HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = Vec::new();
    use tokio::io::AsyncReadExt;
    let _ = sock.read_to_end(&mut buf).await;

    assert!(buf.starts_with(b"HTTP/1.1 200 OK"));
    // Consumed by the handler (the exchange error is upstream noise here).
    assert!(
        state
            .oauth_states()
            .consume("zai", "issued-state")
            .is_none(),
        "the matching state must be spent exactly once"
    );
}

/// Two consecutive listeners must not fight over a port: the OS hands each
/// bind its own ephemeral port.
#[tokio::test]
async fn concurrent_listeners_get_distinct_ports() {
    let (l1, p1) = bind_ephemeral().await.expect("bind 1");
    let (l2, p2) = bind_ephemeral().await.expect("bind 2");
    assert_ne!(p1, p2, "ephemeral binds are independent");
    drop(l1);
    drop(l2);
}
