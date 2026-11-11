use super::*;

#[test]
fn oauth_state_is_single_use_and_ttl_bound() {
    let store = OAuthStateStore::new();
    store.insert(
        "antigravity",
        "st1",
        "verifier-1",
        "http://localhost:8787/admin/callback.html",
    );
    let (verifier, redirect) = store
        .consume("antigravity", "st1")
        .expect("state resolves once");
    assert_eq!(verifier, "verifier-1");
    assert_eq!(redirect, "http://localhost:8787/admin/callback.html");
    assert!(store.consume("antigravity", "st1").is_none(), "single use");
    assert!(store.consume("antigravity", "other").is_none());
    assert!(store.consume("zai", "st1").is_none(), "provider-bound");
}
