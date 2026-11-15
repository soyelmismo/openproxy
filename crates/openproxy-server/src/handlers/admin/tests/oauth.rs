use super::common::*;
use crate::handlers::admin::oauth::resolve_or_create_oauth_account;

/// VULN (audit, fix/security-audit): `resolve_or_create_oauth_account`
/// accepted a caller-supplied `account_id` verbatim, and the subsequent
/// `store_oauth_tokens` UPDATE is unconditional — so the zai exchange (whose
/// state fallback makes the flow reachable with an arbitrary body) could
/// write zai credentials onto ANY existing account, including one belonging
/// to a different provider. The id must exist and already belong to the
/// provider being connected.
#[tokio::test]
async fn oauth_account_id_must_exist_and_belong_to_the_provider() {
    let dir = tempdir();
    let (state, _token) = make_state_with_key(&dir).await;
    let openai_account = insert_test_account(&state, "openai");
    let zai_account = insert_test_account(&state, "zai");

    // A non-existent account id is refused.
    let err = resolve_or_create_oauth_account(&state, "zai", Some(999_999)).await;
    assert!(err.is_err(), "unknown account id must be refused");

    // An id belonging to another provider is refused: zai tokens must never
    // land on an openai account.
    let err = resolve_or_create_oauth_account(&state, "zai", Some(openai_account)).await;
    assert!(
        err.is_err(),
        "cross-provider account id must be refused (zai exchange onto openai account)"
    );

    // The matching provider passes through unchanged.
    let ok = resolve_or_create_oauth_account(&state, "zai", Some(zai_account))
        .await
        .expect("same-provider account id must resolve");
    assert_eq!(ok.0, zai_account);
}
