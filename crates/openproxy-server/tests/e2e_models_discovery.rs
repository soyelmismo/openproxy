#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::{
    Router,
    extract::State as AxumState,
    http::StatusCode,
    response::{IntoResponse, Json as AxumJson},
    routing::get,
};
use openproxy_adapters::adapters::{
    AdapterAuthType, AdapterFormat, ProviderAdapter, ProviderAdapterConfig,
};
use openproxy_core::{
    AppConfig, accounts, admin,
    models::{self, DiscoveredModel, TargetFormat},
};
use openproxy_db::secrets::MasterKey;
use openproxy_db::testing::TempDir;
use openproxy_db::{self as core_db, combos, migrations};
use openproxy_server::state::AppState;
use openproxy_types::combos::Strategy;
use openproxy_types::ids::{AccountId, ComboId, ComboTargetId, ModelId, ModelRowId, ProviderId};
use parking_lot::Mutex;
use rusqlite::Connection;
use serde_json::json;
use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;

#[derive(Debug, Default)]
struct MockState {
    catalog: Mutex<Vec<String>>,
}
#[derive(Clone)]
struct MockStateHandle(Arc<MockState>);
impl MockStateHandle {
    fn replace(&self, ids: Vec<String>) {
        *self.0.catalog.lock() = ids;
    }
}

async fn mock_models_handler(AxumState(state): AxumState<Arc<MockState>>) -> impl IntoResponse {
    let data: Vec<_> = state
        .catalog
        .lock()
        .iter()
        .map(|id| json!({"id": id, "name": id}))
        .collect();
    (StatusCode::OK, AxumJson(json!({"data": data})))
}

async fn spawn_mock(initial: Vec<String>) -> (SocketAddr, MockStateHandle) {
    let state = Arc::new(MockState {
        catalog: Mutex::new(initial),
    });
    let handle = MockStateHandle(Arc::clone(&state));
    let app = Router::new()
        .route("/v1/models", get(mock_models_handler))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock axum");
    });
    (addr, handle)
}

struct TestMockAdapter {
    config: ProviderAdapterConfig,
}
impl TestMockAdapter {
    fn new(id: &str, base_url: &str) -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new(id),
                name: format!("Mock {id}"),
                base_url: base_url.into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: vec![],
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
            },
        }
    }
}

impl ProviderAdapter for TestMockAdapter {
    fn id(&self) -> &ProviderId {
        &self.config.id
    }
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }
    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        format!("{}/chat/completions", self.config.base_url)
    }
    fn build_auth_header(&self, api_key: &str) -> Option<(String, String)> {
        Some(("Authorization".into(), format!("Bearer {api_key}")))
    }
    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let mut h = vec![("Content-Type".into(), "application/json".into())];
        if let Some((k, v)) = self.build_auth_header(api_key) {
            h.push((k, v));
        }
        h
    }
    fn models_url(&self) -> Option<String> {
        Some(format!("{}/v1/models", self.config.base_url))
    }
    async fn fetch_models(
        &self,
        upstream_client: &Arc<openproxy_adapters::upstream::UpstreamClient>,
        _api_key: &str,
    ) -> Result<Vec<DiscoveredModel>, openproxy_types::CoreError> {
        let url = self.models_url().expect("url");
        let resp = upstream_client
            .call(
                openproxy_adapters::upstream::UpstreamRequest::get(&url),
                openproxy_adapters::upstream::TimeoutProfile::ModelDiscovery,
                openproxy_adapters::upstream::CancellationToken::new(),
            )
            .await
            .map_err(|e| openproxy_types::CoreError::UpstreamConnection(e.to_string()))?;
        let body = resp
            .collect()
            .await
            .map_err(|e| openproxy_types::CoreError::UpstreamConnection(e.to_string()))?;
        let val: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| openproxy_types::CoreError::Parse(e.to_string()))?;
        let arr = val
            .get("data")
            .and_then(|v| v.as_array())
            .ok_or_else(|| openproxy_types::CoreError::Parse("missing data".into()))?;
        Ok(arr
            .iter()
            .filter_map(|e| e.get("id").and_then(|v| v.as_str()))
            .map(|id| DiscoveredModel {
                model_id: ModelId::new(id),
                display_name: None,
                target_format: TargetFormat::Openai,
                context_length: None,
                max_output_tokens: None,
                input_modalities: None,
                output_modalities: None,
                model_type: Some("chat".into()),
                family: None,
                capabilities: None,
            })
            .collect())
    }
}

async fn make_test_state(dir: &std::path::Path, adapter: &TestMockAdapter) -> AppState {
    let pool = Arc::new(core_db::DbPool::open(&dir.join("e2e.db")).expect("pool"));
    {
        let mut w = pool.writer();
        migrations::run(&mut w).expect("migrations");
    }
    let mk = Arc::new(MasterKey::generate().unwrap());
    {
        let w = pool.writer();
        admin::create_provider(
            &w,
            admin::CreateProviderInput {
                id: adapter.id().as_str().into(),
                name: "E2E Mock Provider".into(),
                base_url: adapter.config().base_url.clone(),
                auth_type: "bearer".into(),
                format: "openai".into(),
                extra_headers_json: None,
                rate_limit_scope: None,
            },
        )
        .expect("prov");
        admin::create_account(
            &w,
            &mk,
            admin::CreateAccountInput {
                provider_id: adapter.id().as_str().into(),
                api_key: Some("sk-e2e-fake".into()),
                label: Some("e2e-mock".into()),
                priority: Some(10),
                extra_config_json: None,
            },
        )
        .expect("acc");
    }
    let adapters = Arc::new(parking_lot::RwLock::new(Arc::new(vec![])));
    AppState::for_test(AppConfig::default(), pool, mk, adapters)
}

struct ModelRowLite {
    model_id: String,
    active: bool,
    custom: bool,
}
fn select_models(conn: &Connection, provider_id: &ProviderId) -> Vec<ModelRowLite> {
    let mut stmt = conn
        .prepare(
            "SELECT model_id, active, custom FROM models WHERE provider_id = ?1 ORDER BY model_id",
        )
        .expect("prep");
    stmt.query_map([provider_id.as_str()], |row| {
        Ok(ModelRowLite {
            model_id: row.get(0)?,
            active: row.get::<_, i64>(1)? != 0,
            custom: row.get::<_, i64>(2)? != 0,
        })
    })
    .expect("query")
    .map(|r| r.expect("row"))
    .collect()
}

async fn call_refresh(
    state: &AppState,
    provider: &ProviderId,
    api_key: &str,
) -> Option<models::UpsertResult> {
    let mut provider_row = {
        let r = state.db_pool().reader();
        openproxy_core::providers::get(&r, provider)
            .unwrap()
            .unwrap()
    };
    if !provider_row.base_url.ends_with("/v1") {
        provider_row.base_url = format!("{}/v1", provider_row.base_url).into();
    }
    let custom_adapter =
        openproxy_adapters::adapters::CustomAdapter::from_provider_row(&provider_row);
    let adapter_enum =
        openproxy_adapters::adapters::ProviderAdapterEnum::Custom(Box::new(custom_adapter));
    admin::refresh_models(
        state.db_pool(),
        provider,
        api_key,
        &adapter_enum,
        state.upstream_client(),
        3_600,
        "",
    )
    .await
    .ok()
}

#[tokio::test]
async fn e2e_discovery_and_delete_on_disappear() {
    unsafe {
        std::env::set_var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS", "true");
    }
    let (addr, mock) = spawn_mock(vec![]).await;
    let adapter = TestMockAdapter::new("e2e-mock", &format!("http://{addr}"));
    let tmp = TempDir::new("openproxy-e2e-discovery").expect("tmp");
    let state = make_test_state(tmp.path(), &adapter).await;
    let provider = ProviderId::new("e2e-mock");

    let account_id: AccountId = {
        let w = state.db_pool().writer();
        let list = accounts::list(&w, Some(&provider), state.master_key().as_ref()).expect("list");
        assert_eq!(list.len(), 1);
        list[0].id
    };

    // Round 1: catalog [a, b, c]
    mock.replace(vec!["a".into(), "b".into(), "c".into()]);
    let r1 = call_refresh(&state, &provider, "sk-e2e-fake")
        .await
        .expect("r1");
    assert_eq!(r1.touched, 3);
    assert_eq!(
        r1.new_model_ids
            .iter()
            .map(|m| m.as_str().to_string())
            .collect::<BTreeSet<_>>(),
        ["a", "b", "c"]
            .into_iter()
            .map(String::from)
            .collect::<BTreeSet<_>>()
    );

    {
        let w = state.db_pool().writer();
        let rows = select_models(&w, &provider);
        assert_eq!(
            rows.iter()
                .map(|r| r.model_id.as_str())
                .collect::<BTreeSet<_>>(),
            ["a", "b", "c"].into_iter().collect::<BTreeSet<_>>()
        );
        for row in &rows {
            assert!(row.active && !row.custom);
        }
        let live = models::list_active_all(&w).expect("list_active");
        assert_eq!(
            live.iter()
                .map(|m| m.model_id.as_str().to_string())
                .collect::<BTreeSet<_>>(),
            ["a", "b", "c"]
                .into_iter()
                .map(String::from)
                .collect::<BTreeSet<_>>()
        );
    }

    // Round 2: drop c -> [a, b]
    mock.replace(vec!["a".into(), "b".into()]);
    let r2 = call_refresh(&state, &provider, "sk-e2e-fake")
        .await
        .expect("r2");
    assert!(r2.new_model_ids.is_empty());
    {
        let w = state.db_pool().writer();
        let rows = select_models(&w, &provider);
        assert_eq!(
            rows.iter()
                .map(|r| r.model_id.as_str())
                .collect::<BTreeSet<_>>(),
            ["a", "b"].into_iter().collect::<BTreeSet<_>>()
        );
    }

    // Custom row z survives refresh
    {
        let w = state.db_pool().writer();
        let z_id: ModelRowId = models::create_custom(
            &w,
            &provider,
            &ModelId::new("z"),
            Some("z (custom)"),
            TargetFormat::Openai,
            0,
            None,
        )
        .expect("create z");
        assert!(z_id.0 > 0);
    }
    let r3 = call_refresh(&state, &provider, "sk-e2e-fake")
        .await
        .expect("r3");
    assert!(r3.new_model_ids.is_empty());
    {
        let w = state.db_pool().writer();
        let rows = select_models(&w, &provider);
        assert_eq!(
            rows.iter()
                .map(|r| r.model_id.as_str())
                .collect::<BTreeSet<_>>(),
            ["a", "b", "z"].into_iter().collect::<BTreeSet<_>>()
        );
        let z_row = rows.iter().find(|r| r.model_id == "z").expect("z row");
        assert!(z_row.custom && z_row.active);
    }

    // Re-establish [a, b, c] and wire combo target to c
    mock.replace(vec!["a".into(), "b".into(), "c".into()]);
    call_refresh(&state, &provider, "sk-e2e-fake")
        .await
        .expect("refresh re-establish");
    let c_row_id: ModelRowId = {
        let w = state.db_pool().writer();
        let id: i64 = w
            .query_row(
                "SELECT id FROM models WHERE provider_id = ?1 AND model_id = 'c' AND custom = 0",
                [provider.as_str()],
                |r| r.get(0),
            )
            .expect("c id");
        w.execute(
            "DELETE FROM models WHERE provider_id = ?1 AND model_id = 'z'",
            [provider.as_str()],
        )
        .unwrap();
        ModelRowId(id)
    };

    let (combo_id, c_target_id) = {
        let w = state.db_pool().writer();
        let combo_id: ComboId =
            combos::create_combo(&w, "e2e-combo", Strategy::Priority, 1).expect("create_combo");
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

    {
        let w = state.db_pool().writer();
        let before = combos::list_targets_with_model(&w, combo_id).expect("list before");
        assert_eq!(before.len(), 1);
        assert_eq!(&*before[0].model_id, "c");
    }

    // Refresh dropping c with active target -> cascade delete
    mock.replace(vec!["a".into(), "b".into()]);
    call_refresh(&state, &provider, "sk-e2e-fake")
        .await
        .expect("drop c");
    {
        let w = state.db_pool().writer();
        let rows = select_models(&w, &provider);
        assert!(!rows.iter().map(|r| r.model_id.as_str()).any(|id| id == "c"));
        assert_eq!(
            combos::list_targets_with_model(&w, combo_id)
                .expect("list after")
                .len(),
            0
        );
        assert_eq!(
            combos::list_targets(&w, combo_id)
                .expect("list plain")
                .len(),
            0
        );
        let raw_orphan: i64 = w
            .query_row(
                "SELECT COUNT(*) FROM combo_targets WHERE id = ?1",
                rusqlite::params![c_target_id.0],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(raw_orphan, 0);
    }

    // Re-introducing c and re-adding target
    mock.replace(vec!["a".into(), "b".into(), "c".into()]);
    let r4 = call_refresh(&state, &provider, "sk-e2e-fake")
        .await
        .expect("re-add c");
    assert!(r4.new_model_ids.iter().any(|m| m.as_str() == "c"));
    {
        let w = state.db_pool().writer();
        let new_c_id: i64 = w
            .query_row(
                "SELECT id FROM models WHERE provider_id = ?1 AND model_id = 'c' AND custom = 0",
                [provider.as_str()],
                |r| r.get(0),
            )
            .expect("new c id");
        assert_ne!(new_c_id, c_row_id.0);
        let new_target_id: ComboTargetId = combos::add_target(
            &w,
            combos::AddTargetInput {
                combo_id,
                provider_id: provider.clone(),
                account_id: Some(account_id),
                model_row_id: Some(ModelRowId(new_c_id)),
                sub_combo_id: None,
                priority_order: 1,
            },
        )
        .expect("re-add target");
        let routable = combos::list_targets(&w, combo_id).expect("list");
        assert_eq!(routable.len(), 1);
        assert_eq!(routable[0].id, new_target_id);
        let detailed = combos::list_targets_with_model(&w, combo_id).expect("detailed");
        assert_eq!(detailed.len(), 1);
        assert_eq!(&*detailed[0].model_id, "c");
        assert_eq!(detailed[0].model_row_id, Some(ModelRowId(new_c_id)));
    }
}
