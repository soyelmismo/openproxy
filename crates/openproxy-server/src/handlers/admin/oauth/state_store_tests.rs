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

/// Audit H-10 (OP-19): the zai exchange fallback used to accept codes whose
/// state this server never issued (empty verifier + hardcoded redirect),
/// letting an attacker-supplied authorization code be exchanged — binding is
/// now provider-independent. Pinned at the store level: an unknown state for
/// ANY provider (zai included) yields None, which the exchange handler turns
/// into a 400 before any upstream call.
#[test]
fn unknown_state_yields_none_for_every_provider_including_zai() {
    let store = OAuthStateStore::new();
    for provider in ["antigravity", "zai", "kiro", "minimax"] {
        assert!(
            store.consume(provider, "state-never-issued").is_none(),
            "provider {provider} must not resolve an unissued state"
        );
    }
    // And the pair the server DID issue still resolves exactly once.
    store.insert(
        "zai",
        "issued-state",
        "verifier-z",
        "http://localhost:18432/auth/callback-zai",
    );
    let pair = store
        .consume("zai", "issued-state")
        .expect("issued state resolves");
    assert_eq!(
        pair,
        (
            "verifier-z".to_string(),
            "http://localhost:18432/auth/callback-zai".to_string()
        )
    );
    assert!(store.consume("zai", "issued-state").is_none(), "single use");
}

/// Audit H-5: saturation used to `clear()` the whole store, so a flood of
/// authorize calls invalidated the pending states of every other operator.
/// Eviction must be oldest-first: the newest states survive a flood, and a
/// pre-flood state that is still fresh only falls out once the bound is
/// genuinely exhausted by newer entries.
#[test]
fn saturation_evicts_oldest_first_and_never_everything() {
    let store = OAuthStateStore::new();

    // Fill to the bound with sequential states. Each insert at capacity
    // evicts exactly the oldest record, so entry i displaces entry i-K+1.
    const FLOOD: usize = OAUTH_STATE_MAX_ENTRIES + 32;
    for i in 0..FLOOD {
        store.insert(
            "antigravity",
            &format!("st-{i}"),
            "v",
            "http://localhost:8787/cb",
        );
    }

    assert!(
        store.len() <= OAUTH_STATE_MAX_ENTRIES,
        "the bound must hold: {} entries",
        store.len()
    );
    // The oldest states were displaced one by one (NOT wiped in bulk): the
    // newest third must ALL still be present.
    for i in (FLOOD - OAUTH_STATE_MAX_ENTRIES / 3)..FLOOD {
        assert!(
            store.consume("antigravity", &format!("st-{i}")).is_some(),
            "fresh state st-{i} must survive the flood"
        );
    }
    // And the store did not empty: consuming a third still leaves most of
    // the bound populated (a clear() would have left only the post-flood 32).
    assert!(
        store.len() > OAUTH_STATE_MAX_ENTRIES / 2,
        "bulk clear() must not happen; {} entries remain",
        store.len()
    );
}
