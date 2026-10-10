use super::ZaiTokens;
use super::dual::{QuotaEndpoints, fetch_dual_at};
use crate::upstream::UpstreamClient;
use openproxy_types::quota::{QuotaPoolStatus, QuotaSource};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn endpoint(
    status: u16,
    body: Value,
    credential: String,
    barrier: Option<Arc<tokio::sync::Barrier>>,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut chunk = [0; 4096];
            let length = socket.read(&mut chunk).await.unwrap();
            assert!(length > 0);
            request.extend_from_slice(&chunk[..length]);
            if request.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            assert!(request.len() <= 16384);
        }
        let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
        let expected = format!("authorization: bearer {}", credential.to_ascii_lowercase());
        assert!(
            request.lines().any(|line| line == expected),
            "source credential did not match"
        );
        if let Some(barrier) = barrier {
            barrier.wait().await;
        }
        let body = body.to_string();
        socket.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    });
    (format!("http://{address}/quota"), handle)
}

fn credentials() -> ZaiTokens {
    ZaiTokens {
        zcode_jwt_token: Some(["test", "starter", "credential"].join(".")),
        business_access_token: Some(["test", "business", "credential"].join("-")),
        ..Default::default()
    }
}

fn starter_balance() -> Value {
    json!({"code":0,"success":true,"data":{"plans":[{"user_plan_id":"user-plan","plan_id":"starter","name":"Free Starter","status":"active","starts_at":1000,"ends_at":4102444800_i64}],"balances":[{"bucket_id":"free-bucket","user_plan_id":"user-plan","plan_id":"starter","entitlement_id":"ent","unit_type":"token","capabilities":["model:glm-5.3-flash"],"total_units":1000,"used_units":10,"remaining_units":990,"available_units":990,"expires_at":4102444800_i64}]}})
}

#[tokio::test]
async fn independent_fetches_run_concurrently_with_separate_credentials() {
    let tokens = credentials();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (starter, first) = endpoint(
        200,
        starter_balance(),
        tokens.zcode_jwt_token.clone().unwrap(),
        Some(Arc::clone(&barrier)),
    )
    .await;
    let (subscriptions, second) = endpoint(
        200,
        json!({"code":200,"success":true,"data":[]}),
        tokens.business_access_token.clone().unwrap(),
        Some(barrier),
    )
    .await;
    let endpoints = QuotaEndpoints {
        starter: &starter,
        subscriptions: &subscriptions,
        limits: "http://unused.invalid",
    };
    let quota = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fetch_dual_at(&UpstreamClient::new(), &tokens, None, &endpoints),
    )
    .await
    .unwrap()
    .unwrap();
    first.await.unwrap();
    second.await.unwrap();
    let pools = quota.pools.unwrap();
    assert_eq!(pools.len(), 2);
    assert_eq!(pools[0].source, QuotaSource::ZcodeStarter);
    assert_eq!(pools[0].status, QuotaPoolStatus::Active);
    assert_eq!(pools[0].remaining, Some(990));
    assert_eq!(pools[1].status, QuotaPoolStatus::Absent);
    assert!(quota.session_used.is_none());
}

#[tokio::test]
async fn failed_starter_http_and_envelopes_do_not_hide_paid_verdict_or_authorize_it() {
    for (status, body) in [
        (405, json!({"code":3012,"msg":"unusual activity"})),
        (401, json!({"echo":"private value"})),
        (429, json!({"error":{"code":"rate_limit_exceeded"}})),
        (
            200,
            json!({"success":false,"data":{"plans":[],"balances":[]}}),
        ),
    ] {
        let tokens = credentials();
        let (starter, first) =
            endpoint(status, body, tokens.zcode_jwt_token.clone().unwrap(), None).await;
        let (subscriptions, second) = endpoint(
            200,
            json!({"code":200,"data":[]}),
            tokens.business_access_token.clone().unwrap(),
            None,
        )
        .await;
        let endpoints = QuotaEndpoints {
            starter: &starter,
            subscriptions: &subscriptions,
            limits: "http://unused.invalid",
        };
        let quota = fetch_dual_at(&UpstreamClient::new(), &tokens, None, &endpoints)
            .await
            .unwrap();
        first.await.unwrap();
        second.await.unwrap();
        let pools = quota.pools.unwrap();
        assert_eq!(pools[0].status, QuotaPoolStatus::Unavailable);
        assert!(pools[0].remaining.is_none());
        assert!(
            !pools[0]
                .fetch_error
                .as_deref()
                .unwrap()
                .contains("private value")
        );
        assert_eq!(pools[1].status, QuotaPoolStatus::Absent);
        assert!(super::super::select_zai_inference_source(&pools, "glm-5.3-flash", 2000).is_err());
    }
}

#[tokio::test]
async fn independent_paid_failure_preserves_active_starter() {
    let tokens = credentials();
    let (starter, first) = endpoint(
        200,
        starter_balance(),
        tokens.zcode_jwt_token.clone().unwrap(),
        None,
    )
    .await;
    let (subscriptions, second) = endpoint(
        403,
        json!({"message":"private value"}),
        tokens.business_access_token.clone().unwrap(),
        None,
    )
    .await;
    // Limits are deliberately unreachable: a failed subscription envelope must
    // not be interpreted as permission to use an independent paid credential.
    let endpoints = QuotaEndpoints {
        starter: &starter,
        subscriptions: &subscriptions,
        limits: "http://unused.invalid",
    };
    let quota = fetch_dual_at(&UpstreamClient::new(), &tokens, None, &endpoints)
        .await
        .unwrap();
    first.await.unwrap();
    second.await.unwrap();
    let pools = quota.pools.unwrap();
    assert_eq!(pools[0].status, QuotaPoolStatus::Active);
    assert_eq!(pools[1].status, QuotaPoolStatus::Unavailable);
    assert_eq!(
        super::super::select_zai_inference_source(&pools, "glm-5.3-flash", 2000).unwrap(),
        QuotaSource::ZcodeStarter
    );
}

#[tokio::test]
async fn missing_starter_credential_is_unknown_not_confirmed_absent() {
    let quota = fetch_dual_at(
        &UpstreamClient::new(),
        &Default::default(),
        None,
        &QuotaEndpoints {
            starter: "http://unused.invalid",
            subscriptions: "http://unused.invalid",
            limits: "http://unused.invalid",
        },
    )
    .await
    .unwrap();
    let pools = quota.pools.unwrap();
    assert!(
        pools
            .iter()
            .all(|pool| pool.status == QuotaPoolStatus::Unavailable)
    );
    assert!(pools.iter().all(|pool| pool.remaining.is_none()));
}
