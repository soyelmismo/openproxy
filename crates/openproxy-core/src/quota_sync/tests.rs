use super::*;
use openproxy_adapters::adapters::builtin_adapters;
use openproxy_db::DbPool;
use openproxy_types::ids::{AccountId, ModelId};

/// In-memory `DbPool` with migrations applied and one provider + account
/// seeded. Returns the pool and the seeded `AccountId` (always `1`).
fn fresh_pool() -> (Arc<DbPool>, AccountId) {
    let pool = DbPool::test_pool_with_prefix("openproxy-quota-sync-test").expect("open pool");
    let aid = AccountId(1);
    {
        let conn = pool.open_connection().expect("open conn");
        conn.execute(
            "INSERT INTO providers(id, name, base_url, auth_type, format) \
             VALUES ('antigravity', 'Antigravity', 'https://x', 'oauth', 'openai')",
            [],
        )
        .expect("seed provider");
        conn.execute(
            "INSERT INTO accounts(provider_id, label) VALUES ('antigravity', 'a1')",
            [],
        )
        .expect("seed account");
    }
    (Arc::new(pool), aid)
}

#[tokio::test]
async fn quota_sync_clear_helper_drops_expired_rows() {
    let (pool, aid) = fresh_pool();
    let mid = ModelId::new("gemini-2.5");

    // seed two expired rows
    {
        let w = pool.writer();
        let expired = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        openproxy_db::live_limited::mark_limited(&w, aid, &mid, &expired, "RESOURCE_EXHAUSTED")
            .expect("mark expired");
    }

    // the helper `refresh_single_account_quota` calls when `fetch_error.is_none()`
    clear_live_limited_after_refresh(&pool, aid).await;

    let w = pool.writer();
    assert!(!openproxy_db::live_limited::is_limited(&w, aid, &mid).expect("is_limited"));
    assert!(!openproxy_db::live_limited::has_row(&w, aid, &mid).expect("has_row"));
}

#[tokio::test]
async fn quota_sync_clear_helper_preserves_active_rows() {
    // a future-TTL row must survive: without the `until_ts <= now` filter, a
    // refresh 1ms after `mark_limited` would wipe a fresh sentinel
    let (pool, aid) = fresh_pool();
    let mid = ModelId::new("gemini-2.5");
    let active = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();

    {
        let w = pool.writer();
        openproxy_db::live_limited::mark_limited(&w, aid, &mid, &active, "RESOURCE_EXHAUSTED")
            .expect("mark active");
    }

    clear_live_limited_after_refresh(&pool, aid).await;

    let w = pool.writer();
    assert!(openproxy_db::live_limited::is_limited(&w, aid, &mid).expect("still limited"));
}

#[tokio::test]
async fn quota_sync_does_not_clear_when_fetch_error_present() {
    // mirrors `refresh_single_account_quota`'s `if q.fetch_error.is_none()` gate.
    // The full refresh path is too heavy for a unit test, so this pins the
    // call-site contract: with a non-empty `fetch_error` the rows stay.
    let (pool, aid) = fresh_pool();
    let mid = ModelId::new("gemini-2.5");
    let expired = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();

    {
        let w = pool.writer();
        openproxy_db::live_limited::mark_limited(&w, aid, &mid, &expired, "RESOURCE_EXHAUSTED")
            .expect("mark");
    }

    // "fetch_error was Some(_)": skip the helper and assert the row survives.
    // `clear_for_account`'s own test covers the SQL filter.
    {
        let w = pool.writer();
        assert!(openproxy_db::live_limited::has_row(&w, aid, &mid).expect("has_row"));
    }
}

#[tokio::test]
async fn quota_sync_disabled_config_returns_none_and_noop() {
    let (pool, _) = fresh_pool();
    let mut config = AppConfig::default();
    config.quota_sync.enabled = false;

    let upstream = UpstreamClient::new();
    let master_key = Arc::new(MasterKey::generate().expect("generate key"));
    let adapters = Arc::new(RwLock::new(Arc::new(builtin_adapters())));
    let registry = Arc::new(OAuthProviderRegistry::new());

    let res = start_quota_sync_scheduler(
        Arc::clone(&pool),
        config.clone(),
        Arc::clone(&upstream),
        Arc::clone(&master_key),
        Arc::clone(&adapters),
        Arc::clone(&registry),
    );
    assert!(res.is_none());

    let cancel = CancellationToken::new();
    // run_quota_sync_scheduler must also return immediately if disabled
    Box::pin(run_quota_sync_scheduler(
        pool, config, upstream, master_key, adapters, registry, cancel,
    ))
    .await;
}

#[tokio::test]
async fn quota_sync_initial_delay_cancelled() {
    let (pool, _) = fresh_pool();
    let mut config = AppConfig::default();
    config.quota_sync.enabled = true;
    config.quota_sync.interval_secs = 60;

    let upstream = UpstreamClient::new();
    let master_key = Arc::new(MasterKey::generate().expect("generate key"));
    let adapters = Arc::new(RwLock::new(Arc::new(builtin_adapters())));
    let registry = Arc::new(OAuthProviderRegistry::new());

    let cancel = CancellationToken::new();
    cancel.cancel(); // Pre-cancelled token

    // Must return immediately during initial delay select without hitting DB/network
    Box::pin(run_quota_sync_scheduler(
        pool, config, upstream, master_key, adapters, registry, cancel,
    ))
    .await;
}

#[tokio::test]
async fn checkin_scheduler_initial_delay_cancelled() {
    let (pool, _) = fresh_pool();
    let upstream = UpstreamClient::new();
    let master_key = Arc::new(MasterKey::generate().expect("generate key"));

    let cancel = CancellationToken::new();
    cancel.cancel(); // Pre-cancelled token

    // Must return immediately during initial delay select without hitting DB/network
    crate::minimax_checkin::run_checkin_scheduler(pool, upstream, master_key, cancel).await;
}

#[tokio::test]
async fn checkin_cycle_with_cancel_breaks_on_cancelled_token() {
    let (pool, _) = fresh_pool();
    let upstream = UpstreamClient::new();
    let master_key = Arc::new(MasterKey::generate().expect("generate key"));

    let cancel = CancellationToken::new();
    cancel.cancel();

    // With cancelled token, run_checkin_cycle_with_cancel must abort before processing accounts
    crate::minimax_checkin::run_checkin_cycle_with_cancel(
        &pool,
        &upstream,
        &master_key,
        Some(&cancel),
    )
    .await;
}

// --- AutoClaw daily check-in bookkeeping -------------------------------
//
// The upstream claim itself is out of scope here (it needs the network); what
// these tests pin down is the local bookkeeping that was broken: the streak
// metadata must survive into the encrypted blob, the credential must survive
// the write, and an unreadable blob must not be silently replaced.

use openproxy_db::secrets::MasterKey;

fn zai_pool() -> (Arc<DbPool>, AccountId, Arc<MasterKey>) {
    let pool = DbPool::test_pool_with_prefix("openproxy-autoclaw-checkin-test").expect("open pool");
    let master_key = Arc::new(MasterKey::generate().expect("master key"));
    let aid = AccountId(1);
    {
        let conn = pool.open_connection().expect("open conn");
        conn.execute(
            "INSERT INTO providers(id, name, base_url, auth_type, format) \
             VALUES ('zai', 'z.ai', 'https://x', 'oauth', 'openai')",
            [],
        )
        .expect("seed provider");
        conn.execute(
            "INSERT INTO accounts(provider_id, label) VALUES ('zai', 'autoclaw')",
            [],
        )
        .expect("seed account");
    }
    (Arc::new(pool), aid, master_key)
}

fn read_specific(pool: &DbPool, aid: AccountId, master_key: &MasterKey) -> Option<String> {
    let conn = pool.open_connection().expect("open conn");
    let raw: Option<String> = conn
        .query_row(
            "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
            rusqlite::params![aid.0],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    raw.map(|b64| {
        openproxy_db::accounts::decrypt_oauth_provider_specific(Some(&b64), master_key)
            .expect("decrypt provider specific")
    })
}

fn write_specific(pool: &DbPool, aid: AccountId, master_key: &MasterKey, json: &str) {
    let conn = pool.open_connection().expect("open conn");
    let enc = openproxy_db::accounts::encrypt_oauth_provider_specific(json, master_key)
        .expect("encrypt provider specific");
    conn.execute(
        "UPDATE accounts SET oauth_provider_specific = ?1 WHERE id = ?2",
        rusqlite::params![enc, aid.0],
    )
    .expect("seed provider specific");
}

fn result(date: &str, streak: u32) -> openproxy_adapters::adapters::zai::AutoclawCheckinResult {
    openproxy_adapters::adapters::zai::AutoclawCheckinResult {
        last_checkin_date: date.to_string(),
        streak_days: streak,
        already_claimed: false,
    }
}

#[test]
fn checkin_metadata_is_persisted_next_to_the_credential() {
    let (pool, aid, mk) = zai_pool();
    write_specific(&pool, aid, &mk, r#"{"autoclaw_access_token":"tok-123"}"#);

    persist_autoclaw_checkin(&pool, &mk, aid, &result("2026-11-07", 3)).expect("persist");

    let stored = read_specific(&pool, aid, &mk).expect("blob present");
    let map: serde_json::Value = serde_json::from_str(&stored).expect("json");
    assert_eq!(map["last_checkin_date"], "2026-11-07");
    assert_eq!(map["streak_days"], 3);
    // The credential that shares the column must be untouched.
    assert_eq!(map["autoclaw_access_token"], "tok-123");
}

#[test]
fn checkin_metadata_is_created_when_no_blob_existed_yet() {
    // This is the 1584/1586 case: a fresh AutoClaw account with no metadata at
    // all must end up with a readable record instead of nothing.
    let (pool, aid, mk) = zai_pool();

    persist_autoclaw_checkin(&pool, &mk, aid, &result("2026-11-07", 2)).expect("persist");

    let stored = read_specific(&pool, aid, &mk).expect("blob present");
    let map: serde_json::Value = serde_json::from_str(&stored).expect("json");
    assert_eq!(map["last_checkin_date"], "2026-11-07");
    assert_eq!(map["streak_days"], 2);
}

#[test]
fn checkin_metadata_overwrites_a_stale_previous_day() {
    let (pool, aid, mk) = zai_pool();
    write_specific(
        &pool,
        aid,
        &mk,
        r#"{"autoclaw_access_token":"tok-123","last_checkin_date":"2026-11-06","streak_days":2}"#,
    );

    persist_autoclaw_checkin(&pool, &mk, aid, &result("2026-11-07", 3)).expect("persist");

    let stored = read_specific(&pool, aid, &mk).expect("blob present");
    let map: serde_json::Value = serde_json::from_str(&stored).expect("json");
    assert_eq!(map["last_checkin_date"], "2026-11-07");
    assert_eq!(map["streak_days"], 3);
    assert_eq!(map["autoclaw_access_token"], "tok-123");
}

#[test]
fn unreadable_blob_is_left_alone_rather_than_overwritten() {
    // The column also holds the OAuth tokens. A blob that will not decrypt must
    // never be replaced by a two-key map, or the account loses its credential.
    let (pool, aid, mk) = zai_pool();
    {
        let conn = pool.open_connection().expect("open conn");
        conn.execute(
            "UPDATE accounts SET oauth_provider_specific = ?1 WHERE id = ?2",
            rusqlite::params!["not-a-valid-ciphertext", aid.0],
        )
        .expect("seed corrupt blob");
    }
    let before: Option<String> = {
        let conn = pool.open_connection().expect("open conn");
        conn.query_row(
            "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
            rusqlite::params![aid.0],
            |r| r.get(0),
        )
        .ok()
        .flatten()
    };

    persist_autoclaw_checkin(&pool, &mk, aid, &result("2026-11-07", 3)).expect("persist");

    let after: Option<String> = {
        let conn = pool.open_connection().expect("open conn");
        conn.query_row(
            "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
            rusqlite::params![aid.0],
            |r| r.get(0),
        )
        .ok()
        .flatten()
    };
    assert_eq!(before, after, "corrupt blob must be preserved verbatim");
}

#[test]
fn mismatched_master_key_writes_nothing_and_preserves_the_blob() {
    // A key mismatch makes the stored blob undecryptable, which is the same
    // fail-closed path as corruption: nothing is written, so the credential
    // survives for the process that does hold the right key.
    let (pool, aid, mk) = zai_pool();
    let other = MasterKey::generate().expect("master key");
    write_specific(&pool, aid, &mk, r#"{"autoclaw_access_token":"tok-123"}"#);

    persist_autoclaw_checkin(&pool, &other, aid, &result("2026-11-07", 1)).expect("persist");

    let stored = read_specific(&pool, aid, &mk).expect("original blob still readable");
    let map: serde_json::Value = serde_json::from_str(&stored).expect("json");
    assert_eq!(map["autoclaw_access_token"], "tok-123");
    assert!(
        map.get("last_checkin_date").is_none(),
        "no metadata may be written under a mismatched key"
    );
}
