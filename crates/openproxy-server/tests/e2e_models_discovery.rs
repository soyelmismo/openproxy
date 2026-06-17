//! Gate C — end-to-end test for the discovery + delete-on-disappear
//! chain.
//!
//! Spec: `docs/specs/gate-C-e2e-test.md`. The acceptance criterion is
//! that `cargo test -p openproxy-server --test e2e_models_discovery`
//! passes against a hand-rolled `axum` mock that mimics an
//! OpenAI-compatible `/v1/models` endpoint. The test drives the
//! production code path — `admin::refresh_models` against the real
//! `DbPool`, the real `UpstreamClient`, and the real
//! `models::upsert_many` — synchronously, twice, and asserts the
//! catalog rows match the upstream list each time.
//!
//! This is the bridge between Gate A (background scheduler; the per-
//! provider refresh loop) and Gate B (`upsert_many` deletes rows the
//! upstream no longer lists). Neither A nor B alone exercises the
//! full chain from the scheduler's tick all the way through to a
//! `combo_targets` row that references a vanished model. This test
//! does.
//!
//! All state is local to the test; nothing here mutates a global.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::State as AxumState,
    http::StatusCode,
    response::{IntoResponse, Json as AxumJson},
    routing::get,
    Router,
};
use openproxy_core::{
    accounts,
    admin,
    adapters::{
        AdapterAuthType, AdapterFormat, ProviderAdapter, ProviderAdapterConfig,
    },
    combos,
    db::{self as core_db, migrations},
    ids::{AccountId, ComboId, ComboTargetId, ModelId, ModelRowId, ProviderId},
    models::{self, DiscoveredModel, TargetFormat},
    secrets::MasterKey,
    AppConfig,
};
use openproxy_server::state::AppState;
use parking_lot::Mutex;
use rusqlite::Connection;
use serde_json::json;
use tempfile::TempDir;
use tokio::net::TcpListener;

// =====================================================================
// Mock server
// =====================================================================

/// Shared state for the mock server: the current list of upstream
/// `model_id`s. Tests mutate it from outside via
/// [`MockStateHandle::set`] between refreshes.
#[derive(Debug, Default)]
struct MockState {
    /// Current catalog. The mock's `/v1/models` handler serializes
    /// this into the OpenAI-compatible `{"data":[{"id":...}]}` shape.
    catalog: Mutex<Vec<String>>,
}

impl MockState {
    fn new(initial: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            catalog: Mutex::new(initial),
        })
    }

    fn set(&self, ids: Vec<String>) {
        *self.catalog.lock() = ids;
    }
}

/// Opaque handle the test thread holds to mutate the mock's catalog
/// between refreshes. Cloning is cheap (just an `Arc` bump).
#[derive(Clone)]
struct MockStateHandle(Arc<MockState>);

impl MockStateHandle {
    fn replace(&self, ids: Vec<String>) {
        self.0.set(ids);
    }
}

async fn mock_models_handler(
    AxumState(state): AxumState<Arc<MockState>>,
) -> impl IntoResponse {
    let ids = state.catalog.lock().clone();
    let data: Vec<serde_json::Value> = ids
        .into_iter()
        .map(|id| {
            json!({
                "id": id,
                "name": id,
            })
        })
        .collect();
    (StatusCode::OK, AxumJson(json!({ "data": data })))
}

/// Bind an `axum` server to `127.0.0.1:0`, return the actual bound
/// `SocketAddr` plus a handle the test thread can use to mutate the
/// catalog. The server task runs in the background and is dropped
/// when the test's runtime exits.
async fn spawn_mock(initial: Vec<String>) -> (SocketAddr, MockStateHandle) {
    let state = MockState::new(initial);
    let handle = MockStateHandle(state.clone());

    let app = Router::new()
        .route("/v1/models", get(mock_models_handler))
        .with_state(state);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        // The `axum::serve` future only returns on error. We don't
        // expect one in the test, so `.unwrap()` is the right
        // shape: if the listener dies, the test should fail loudly.
        axum::serve(listener, app).await.expect("mock axum server");
    });

    (addr, handle)
}

// =====================================================================
// Test adapter
// =====================================================================

/// `ProviderAdapter` impl that points at the mock server bound
/// above. Its `fetch_models` GETs `${base_url}/v1/models` via the
/// production hyper-based `UpstreamClient` and parses the standard
/// `{"data":[{"id":...}]}` shape.
struct TestMockAdapter {
    config: ProviderAdapterConfig,
}

impl TestMockAdapter {
    fn new(id: &str, base_url: String) -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new(id),
                base_url,
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: vec![],
            },
        }
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for TestMockAdapter {
    fn id(&self) -> &ProviderId {
        &self.config.id
    }

    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn build_chat_url(
        &self,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> String {
        // The test never makes a chat call against the mock
        // (the `chat/completions` route is only here to round out
        // the surface), so this can be a stub. Build something
        // vaguely valid so the path is grep-able.
        format!("{}/chat/completions", self.config.base_url)
    }

    fn build_auth_header(&self, api_key: &str) -> (String, String) {
        ("Authorization".into(), format!("Bearer {api_key}"))
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let (name, value) = self.build_auth_header(api_key);
        vec![
            (name, value),
            ("Content-Type".to_string(), "application/json".to_string()),
        ]
    }

    fn models_url(&self) -> Option<String> {
        Some(format!("{}/v1/models", self.config.base_url))
    }

    async fn fetch_models(
        &self,
        upstream_client: &std::sync::Arc<openproxy_core::upstream::UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>, openproxy_core::error::CoreError> {
        let url = self.models_url().expect("set above");
        let resp = upstream_client
            .call(
                openproxy_core::upstream::UpstreamRequest::get(&url),
                openproxy_core::upstream::TimeoutProfile::ModelDiscovery,
                openproxy_core::upstream::CancellationToken::new(),
            )
            .await
            .map_err(|e| {
                openproxy_core::error::CoreError::UpstreamConnection(e.to_string())
            })?;
        if !resp.status.is_success() {
            return Err(openproxy_core::error::CoreError::UpstreamConnection(
                format!("mock returned status {}", resp.status),
            ));
        }
        let body = resp
            .collect()
            .await
            .map_err(|e| {
                openproxy_core::error::CoreError::UpstreamConnection(format!(
                    "collect body: {e}"
                ))
            })?;
        let value: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
            openproxy_core::error::CoreError::Parse(format!(
                "mock returned non-JSON: {e}"
            ))
        })?;
        let arr = value.get("data").and_then(|v| v.as_array()).ok_or_else(|| {
            openproxy_core::error::CoreError::Parse(
                "mock response missing 'data' array".into(),
            )
        })?;
        let mut out = Vec::with_capacity(arr.len());
        for entry in arr {
            let id = entry
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    openproxy_core::error::CoreError::Parse(
                        "mock entry missing 'id' string".into(),
                    )
                })?
                .to_string();
            out.push(DiscoveredModel {
                model_id: ModelId::new(id),
                display_name: None,
                target_format: TargetFormat::Openai,
                context_length: None,
                max_output_tokens: None,
                input_modalities: None,
                output_modalities: None,
                model_type: Some("chat".to_string()),
                family: None,
                capabilities: None,
            });
        }
        // Touch the api_key so the compiler doesn't warn about the
        // parameter going unused in a no-auth mock. Real adapters
        // would attach `Authorization: Bearer ${api_key}`; we don't
        // need to here because the mock's `models` handler doesn't
        // look at headers.
        let _ = api_key;
        Ok(out)
    }
}

// =====================================================================
// Test fixture helpers
// =====================================================================

/// Build a fresh in-memory `AppState` mirroring the inline-test
/// helper in `handlers/admin.rs::make_state_with_key`. The only
/// difference: we plug in our custom `TestMockAdapter` instead of
/// the built-in adapter set, and we keep the `DbPool` + `AppState`
/// in the test's hands so the test can mutate the DB directly
/// between refreshes.
async fn make_test_state(
    dir: &std::path::Path,
    adapter: Arc<dyn ProviderAdapter>,
) -> AppState {
    let pool = Arc::new(core_db::DbPool::open(&dir.join("e2e.db")).expect("open pool"));
    {
        let mut w = pool.writer();
        migrations::run(&mut w).expect("migrations");
    }
    let mk = Arc::new(MasterKey::generate());

    // Register the test provider in the providers table so
    // `admin::refresh_models` doesn't bail on the first line
    // ("provider not found"). We use the existing `admin` helpers
    // so the schema and any future CHECK constraints stay
    // enforced.
    {
        let w = pool.writer();
        admin::create_provider(
            &w,
            admin::CreateProviderInput {
                id: adapter.id().as_str().to_string(),
                name: "E2E Mock Provider".into(),
                base_url: adapter.config().base_url.clone(),
                auth_type: "bearer".into(),
                format: "openai".into(),
                extra_headers_json: None,
            },
        )
        .expect("create_provider (test provider)");
        admin::create_account(
            &w,
            &mk,
            admin::CreateAccountInput {
                provider_id: adapter.id().as_str().to_string(),
                api_key: Some("sk-e2e-fake".into()),
                label: Some("e2e-mock".into()),
                priority: Some(10),
                extra_config_json: None,
            },
        )
        .expect("create_account (test account)");
    }

    // Build a Vec<Arc<dyn ProviderAdapter>> containing only the
    // test adapter. We deliberately do NOT include the built-in
    // adapters — the spec says "do NOT modify the seed list", and
    // the test is gated on its own provider id, so a built-in
    // refresh racing the test's call can't affect this DB.
    let adapters: Arc<Vec<Arc<dyn ProviderAdapter>>> = Arc::new(vec![adapter]);

    AppState::for_test(AppConfig::default(), pool, mk, adapters).await
}

/// Read every `(model_id, active, custom)` for a provider straight
/// from the DB. Returns a sorted, distinct `Vec<String>` of the
/// model_ids the spec's "what the upstream lists, the catalog
/// shows" assertion cares about. The full row is fetched so the
/// caller can also assert on `active` and `custom` if it wants.
#[derive(Debug)]
struct ModelRowLite {
    model_id: String,
    active: bool,
    custom: bool,
}

fn select_models(
    conn: &Connection,
    provider_id: &ProviderId,
) -> Vec<ModelRowLite> {
    let mut stmt = conn
        .prepare(
            "SELECT model_id, active, custom FROM models \
             WHERE provider_id = ?1 ORDER BY model_id",
        )
        .expect("prepare select models");
    stmt.query_map([provider_id.as_str()], |row| {
        let active: i64 = row.get(1)?;
        let custom: i64 = row.get(2)?;
        Ok(ModelRowLite {
            model_id: row.get(0)?,
            active: active != 0,
            custom: custom != 0,
        })
    })
    .expect("query select models")
    .map(|r| r.expect("row select models"))
    .collect()
}

/// Drive `admin::refresh_models` against the live pool, mimicking
/// exactly what the production scheduler does (and what the
/// `POST /v1/admin/models/:id/refresh` handler does): open a fresh
/// owned `Connection`, hand it to `refresh_models`, await the
/// future, drop the connection. We do NOT use `pool.writer()` here
/// because `refresh_models` takes the `Connection` by value to keep
/// the future `Send` end to end.
///
/// Returns `None` when the refresh fails — step 9 uses this to
/// detect the FK-blocked path. All earlier steps treat `None` as a
/// hard failure (they `expect`, not `ok`).
async fn call_refresh(
    state: &AppState,
    provider: &ProviderId,
    api_key: &str,
    adapter: &Arc<dyn ProviderAdapter>,
) -> Option<models::UpsertResult> {
    let conn = state
        .db_pool()
        .open_connection()
        .expect("open_connection");
    admin::refresh_models(
        conn,
        provider,
        api_key,
        adapter.as_ref(),
        state.upstream_client(),
        3_600,
    )
    .await
    .ok()
}

// =====================================================================
// The test
// =====================================================================

#[tokio::test]
async fn e2e_discovery_and_delete_on_disappear() {
    // --- Step 1: spin up the mock server on 127.0.0.1:0 ---------
    let (addr, mock) = spawn_mock(vec![]).await;
    let base_url = format!("http://{addr}");

    // --- Step 2: build the test adapter + AppState --------------
    let adapter: Arc<dyn ProviderAdapter> = Arc::new(TestMockAdapter::new(
        "e2e-mock",
        base_url.clone(),
    ));
    let tmp = TempDir::new().expect("tempdir");
    let state = make_test_state(tmp.path(), adapter.clone()).await;
    let provider = ProviderId::new("e2e-mock");

    // The `discover_scheduler` that `AppState::for_test` wires up
    // has a 1h cadence, so it'll never fire during this test
    // (well within the < 5s wall-clock budget the spec requires).
    // We drive `admin::refresh_models` ourselves to keep the test
    // synchronous and deterministic.

    // ============================================================
    // Step 3: ensure a row in `accounts` for the test provider.
    //         (The make_test_state helper already did this; we
    //         pull its id here so we can build a combo target
    //         that points at a real account later.)
    // ============================================================
    let account_id: AccountId = {
        let w = state.db_pool().writer();
        let accounts_list =
            accounts::list(&w, Some(&provider)).expect("list accounts");
        assert_eq!(accounts_list.len(), 1, "fixture must have one account");
        // `accounts::create` defaults `auth_type` to `"api_key"`
        // (the static-key flow). The test doesn't care which it
        // is — it only needs an account row that decrypts back
        // to the plaintext we used at create-time — but pinning
        // the value keeps the fixture from silently changing
        // shape if `accounts::create`'s default ever flips.
        assert_eq!(accounts_list[0].auth_type, "api_key");
        accounts_list[0].id
    };

    // ============================================================
    // Step 4: stub `admin::refresh_models` is already wired
    //         through `AppState::for_test`'s `adapters` vec; we
    //         just call it. (Per the spec, the test does NOT go
    //         through `start_discovery_scheduler` directly.)
    // ============================================================

    // --- Step 5: assertion round 1 ------------------------------
    // Initial catalog from the upstream: [a, b, c].
    mock.replace(vec!["a".into(), "b".into(), "c".into()]);
    let r1 = call_refresh(&state, &provider, "sk-e2e-fake", &adapter)
        .await
        .expect("refresh_models round 1");
    assert_eq!(
        r1.touched, 3,
        "first refresh must have inserted 3 rows"
    );
    let r1_ids: BTreeSet<String> = r1
        .new_model_ids
        .iter()
        .map(|m| m.as_str().to_string())
        .collect();
    assert_eq!(
        r1_ids,
        ["a", "b", "c"]
            .into_iter()
            .map(String::from)
            .collect::<BTreeSet<_>>(),
        "first refresh must report all three as new"
    );

    {
        let w = state.db_pool().writer();
        let rows = select_models(&w, &provider);
        let ids: BTreeSet<String> =
            rows.iter().map(|r| r.model_id.clone()).collect();
        assert_eq!(
            ids,
            ["a", "b", "c"]
                .into_iter()
                .map(String::from)
                .collect::<BTreeSet<_>>(),
            "DB must contain exactly a, b, c"
        );
        // Every row is `active = 1` and `custom = 0` right after
        // first discovery. The `active` bit matters for
        // `models::list_active`; `custom` matters for the
        // delete-on-disappear filter.
        for row in &rows {
            assert!(row.active, "{} must be active", row.model_id);
            assert!(!row.custom, "{} must NOT be custom", row.model_id);
        }

        // `models::list_active_all` is the cross-provider
        // variant the public `/v1/models` endpoint serves. With
        // no other providers active, it should return the same
        // three rows.
        let live = models::list_active_all(&w).expect("list_active_all");
        let live_ids: BTreeSet<String> = live
            .iter()
            .map(|m| m.model_id.as_str().to_string())
            .collect();
        assert_eq!(
            live_ids,
            ["a", "b", "c"]
                .into_iter()
                .map(String::from)
                .collect::<BTreeSet<_>>(),
            "list_active_all must mirror the catalog"
        );
    }

    // --- Step 6: mutate the upstream. Drop `c`. ----------------
    mock.replace(vec!["a".into(), "b".into()]);

    // --- Step 7: assertion round 2 ------------------------------
    let r2 = call_refresh(&state, &provider, "sk-e2e-fake", &adapter)
        .await
        .expect("refresh_models round 2");
    // `r2.touched` is inserts + updates: both `a` and `b` were
    // re-upserted (no row changes but the INSERT...ON CONFLICT
    // statement still returns 2 affected rows from SQLite's
    // point of view). What matters is that no NEW model_ids were
    // reported.
    assert!(
        r2.new_model_ids.is_empty(),
        "second refresh must not report any new model_ids; got {:?}",
        r2.new_model_ids
    );

    {
        let w = state.db_pool().writer();
        let rows = select_models(&w, &provider);
        let ids: BTreeSet<String> =
            rows.iter().map(|r| r.model_id.clone()).collect();
        assert_eq!(
            ids,
            ["a", "b"]
                .into_iter()
                    .map(String::from)
                .collect::<BTreeSet<_>>(),
            "DB must contain exactly a, b; c must be gone"
        );
        assert!(
            !ids.contains("c"),
            "the row for c must have been hard-deleted by upsert_many"
        );
    }

    // --- Step 8: sanity — a custom row survives the refresh. ----
    // Insert a hand-picked row for this provider with `custom = 1`.
    // The model_id is `z`; the upstream never lists `z`, so a
    // non-custom row for it would have been deleted by step 7.
    // The `custom = 1` flag is exactly what the delete branch in
    // `upsert_many` filters on (`WHERE custom = 0`).
    {
        let w = state.db_pool().writer();
        let z_id: ModelRowId = models::create_custom(
            &w,
            &provider,
            &ModelId::new("z"),
            Some("z (custom)"),
            TargetFormat::Openai,
            0,
        )
        .expect("create_custom z");
        assert_eq!(z_id.0 > 0, true, "custom row must have a positive id");
    }

    // Third refresh: catalog is still [a, b]. The custom row `z`
    // must NOT be touched.
    let r3 = call_refresh(&state, &provider, "sk-e2e-fake", &adapter)
        .await
        .expect("refresh_models round 3");
    assert!(
        r3.new_model_ids.is_empty(),
        "third refresh must not report any new model_ids; got {:?}",
        r3.new_model_ids
    );

    {
        let w = state.db_pool().writer();
        let rows = select_models(&w, &provider);
        let ids: BTreeSet<String> =
            rows.iter().map(|r| r.model_id.clone()).collect();
        assert_eq!(
            ids,
            ["a", "b", "z"]
                .into_iter()
                .map(String::from)
                .collect::<BTreeSet<_>>(),
            "the custom row z must survive the refresh that \
             doesn't list it"
        );
        // And it must still be flagged custom in the row.
        let z_row = rows.iter().find(|r| r.model_id == "z").expect("z row");
        assert!(z_row.custom, "z must be flagged custom");
        assert!(z_row.active, "z must still be active");
    }

    // --- Step 9: sanity — `combo_targets` no longer returns `c`.
    // The spec asks us to verify that, after `c` is removed from
    // the upstream catalog, any combo target that referenced
    // `c` no longer surfaces `c` via
    // `combos::list_targets[_with_model]`.
    //
    // This contract has two sides:
    //   - the *visibility* side: the row in `combo_targets` may
    //     stay around (for bookkeeping / re-activation), but
    //     `list_targets_with_model` should fall back to
    //     `model_id = ""` for the orphan target — the
    //     `LEFT JOIN models` + `COALESCE` in that query
    //     guarantees this.
    //   - the *persistence* side: the catalog row for `c` must
    //     actually be gone, otherwise the LEFT JOIN would still
    //     match and `model_id = "c"` would survive.
    //
    // The two sides are coupled by Gate D's
    // `combo_targets.model_row_id ... ON DELETE SET NULL`
    // (migration 000025): a successful `admin::refresh_models`
    // against the updated catalog wipes the `c` row in one go,
    // the SET NULL cascade lands on the orphan target's
    // `model_row_id`, and `list_targets_with_model` starts
    // reporting `model_id = ""` automatically. That's the
    // happy path the spec describes, and it's what this step
    // exercises end-to-end.
    //
    // (Before Gate D, the FK defaulted to `NO ACTION` /
    // `RESTRICT`, the refresh transaction aborted, and the
    // catalog row for `c` survived — the spec for Gate C
    // documented this in step 9.e/9.f as an interaction
    // failure. With Gate D in place, the bypass is no longer
    // needed.)
    //
    // 9.a. Re-establish the catalog so `c` is alive and we can
    //       build a target against its row id. We re-do the
    //       discovery flow rather than poking the row in
    //       directly, so the test exercises the same code path
    //       a live scheduler would.
    mock.replace(vec!["a".into(), "b".into(), "c".into()]);
    let _r3b = call_refresh(&state, &provider, "sk-e2e-fake", &adapter)
        .await
        .expect("refresh_models 9.a (re-establish [a, b, c])");

    // 9.b. Capture the row id of `c` while it's still alive, and
    //       clean up the `z` custom row from step 8 (so the
    //       assertions below read cleanly).
    let c_row_id: ModelRowId = {
        let w = state.db_pool().writer();
        let id: i64 = w
            .query_row(
                "SELECT id FROM models \
                 WHERE provider_id = ?1 AND model_id = 'c' AND custom = 0",
                [provider.as_str()],
                |r| r.get(0),
            )
            .expect("query c row id while it's alive");
        w.execute(
            "DELETE FROM models WHERE provider_id = ?1 AND model_id = 'z'",
            [provider.as_str()],
        )
        .expect("delete z for the step-9 setup");
        ModelRowId(id)
    };

    // 9.c. Build the combo + target via the normal admin API
    //       while `c` is still in the catalog (so the FK and
    //       the model/provider cross-check both pass).
    let (combo_id, c_target_id) = {
        let w = state.db_pool().writer();
        let combo_id: ComboId = combos::create_combo(
            &w,
            "e2e-combo",
            combos::Strategy::Priority,
            1,
        )
        .expect("create_combo");
        let target_id: ComboTargetId = combos::add_target(
            &w,
            combos::AddTargetInput {
                combo_id,
                provider_id: provider.clone(),
                account_id: Some(account_id),
                model_row_id: Some(c_row_id),
                sub_combo_id: None,
                priority_order: 1,
            },
        )
        .expect("add_target c");
        (combo_id, target_id)
    };

    // 9.d. Sanity-check the target is wired up and surfaces
    //       `c` *before* the wipe.
    {
        let w = state.db_pool().writer();
        let before = combos::list_targets_with_model(&w, combo_id)
            .expect("list_targets_with_model before");
        assert_eq!(before.len(), 1);
        assert_eq!(
            before[0].model_id, "c",
            "target must surface c before the upstream drops it"
        );
    }

    // 9.e. Run the real `admin::refresh_models` against a
    //       catalog that drops `c` while a target still
    //       references it. With Gate D's
    //       `ON DELETE SET NULL` on
    //       `combo_targets.model_row_id` (migration 000025),
    //       the hard-delete in `upsert_many` succeeds: the
    //       `models` row for `c` is removed and the orphan
    //       target's `model_row_id` is set to NULL in the
    //       same transaction. We assert the refresh succeeds
    //       (i.e. Gate B and Gate D cooperate cleanly).
    mock.replace(vec!["a".into(), "b".into()]);
    call_refresh(&state, &provider, "sk-e2e-fake", &adapter)
        .await
        .expect(
            "refresh against a catalog that drops c must succeed \
             now that combo_targets.model_row_id has \
             ON DELETE SET NULL (Gate D / migration 000025); \
             a failure here means Gate B ↔ Gate D regressed.",
        );

    // 9.f. After the refresh:
    //   - the catalog row for `c` is gone,
    //   - the combo target's `model_row_id` was set to NULL
    //     by the ON DELETE SET NULL cascade,
    //   - `combos::list_targets_with_model` returns the
    //     bookkeeping row but with `model_id = ""` (the
    //     COALESCE).
    {
        let w = state.db_pool().writer();

        // The catalog no longer has `c`.
        let rows = select_models(&w, &provider);
        let ids: BTreeSet<String> =
            rows.iter().map(|r| r.model_id.clone()).collect();
        assert!(
            !ids.contains("c"),
            "the catalog row for c must be gone after the refresh: \
             got {ids:?}"
        );

        // The target row is still there (the SET NULL preserves
        // it for bookkeeping / re-activation), but the model_id
        // it surfaces is empty. This is the "no longer returns c"
        // contract the spec asks for.
        let after = combos::list_targets_with_model(&w, combo_id)
            .expect("list_targets_with_model after");
        assert_eq!(
            after.len(),
            1,
            "the orphan target must still be visible to the admin API"
        );
        assert_eq!(
            after[0].id, c_target_id,
            "the surviving target is the same id we created"
        );
        assert_eq!(
            after[0].model_id, "",
            "after c is wiped from the catalog, the target must \
             no longer surface `c`; the LEFT JOIN + COALESCE in \
             list_targets_with_model collapses the dangling \
             model_row_id to ''"
        );

        // The plain `list_targets` returns the bookkeeping row
        // as well, but now with `model_row_id = None` (the
        // ON DELETE SET NULL cascade nulled it in the same
        // transaction that wiped the `c` row). This is the
        // *clean* end-state Gate D is meant to produce — the
        // bookkeeping id matches reality (the model is gone)
        // without any FK-bypass hack.
        let plain = combos::list_targets(&w, combo_id)
            .expect("list_targets after");
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].id, c_target_id);
        assert_eq!(
            plain[0].model_row_id, None,
            "model_row_id must be NULL on the surviving target \
             after the catalog row for c was deleted, courtesy of \
             ON DELETE SET NULL (Gate D / migration 000025); got \
             {:?}",
            plain[0].model_row_id
        );
    }

    // ============================================================
    // Final cleanup: drop `state` so its background prune loop
    // unwinds before `tmp` is removed by `Drop` of `TempDir`.
    // Dropping the AppState doesn't currently expose a shutdown
    // hook, but the prune loop only holds an `Arc<DbPool>`, so
    // letting the test function return is enough — `tmp`'s
    // `Drop` removes the on-disk DB and the prune task's
    // `writer()` call returns `Err` from then on, which the
    // prune loop silently swallows (`let _ = …`).
    // ============================================================
    drop(state);
    // Hint to the reader: a `tokio::time::sleep` here would
    // make the test slower than necessary, and the spec caps
    // wall-clock at 5 s.
    let _ = (PathBuf::from(tmp.path()), Duration::from_millis(0));
}
