use crate::adapters::{AdapterAuthType, AdapterFormat, ProviderAdapter, ProviderAdapterConfig};
use crate::combos;
use crate::config::{RacingConfig, RetriesConfig, TimeoutsConfig};
use crate::db::conn::DbPool;
use crate::db::migrations;
use crate::ids::{AccountId, ModelRowId, ProviderId, RequestId, TraceId, ComboId};
use crate::models::TargetFormat;
use crate::pipeline::{PipelineConfig, PipelineRequest};
use crate::providers::{self, AuthType, ProviderFormat};
use crate::secrets::MasterKey;
use crate::translation::{OpenAIMessage, OpenAIRequest};
use crate::timeouts::Timeouts;
use crate::upstream::UpstreamClient;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::sync::watch;
use rusqlite::Connection;
use crate::error::Result;

pub fn fresh_pool() -> (DbPool, Arc<parking_lot::Mutex<Connection>>, PathBuf) {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir =
        std::env::temp_dir().join(format!("openproxy-pipeline-test-{}-{}-{}", pid, nanos, n));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = dir.join("pipeline.db");
    let pool = DbPool::open(&path).expect("open pool");
    {
        let mut w = pool.writer();
        migrations::run(&mut w).expect("migrations");
    }
    // A second connection on the same file, owned by the Pipeline.
    let extra = Connection::open(&path).expect("open extra");
    let conn = Arc::new(parking_lot::Mutex::new(extra));
    (pool, conn, path)
}

/// A reasonable default `PipelineConfig` for tests: no real adapters
/// (the tests only exercise the routing/usage path, not the HTTP path).
pub fn test_config(master_key: Arc<MasterKey>) -> PipelineConfig {
    let defaults = Timeouts::from_config(&TimeoutsConfig::default());
    PipelineConfig {
        defaults,
        racing: RacingConfig::default(),
        retries: RetriesConfig::default(),
        max_attempts: 1,
        master_key,
        adapters: Arc::new(Vec::new()),
        // A vanilla HTTP client is fine for tests: nothing in the
        // routing path actually fires a request.
        http_client: reqwest::Client::new(),
        // 60s default cooldown for tests; individual tests that
        // exercise the cooldown path can pass a shorter value
        // through a local `PipelineConfig` override.
        cooldown_secs: 60,
        cooldown_max_secs: 3600,
        cooldown_factor: 2,
        // Hyper-based upstream client. The default production
        // connector (rustls HTTPS) is fine for tests that don't
        // exercise the HTTP path; tests that DO need a real
        // upstream should rebuild the config with a test
        // connector.
        upstream_client: UpstreamClient::new(),
        oauth_provider_registry: None,
        // Tests use the default Off mode so the production
        // compression behavior is opt-in; individual tests
        // that exercise compression override these.
        compression_mode: crate::compression::CompressionMode::Off,
        // Default matches the production default in
        // state.rs; tests don't need to flip this.
        idle_chunk_retryable: true,
        quota_protection: crate::config::QuotaProtectionConfig::default(),
            background_tx: tokio::sync::mpsc::channel(1).0,
    }
}

/// Seed a provider so combo_targets FKs can be satisfied.
pub fn seed_provider(conn: &Connection, provider_id: &str, auth_type: AuthType) {
    providers::create(
        conn,
        providers::NewProvider {
            id: &ProviderId::new(provider_id),
            name: provider_id,
            base_url: "https://example.com",
            auth_type,
            format: ProviderFormat::Openai,
            extra_headers_json: None,
            auto_activate_keyword: None,
        },
    )
    .expect("seed provider");
}

/// Seed a provider and a single model row, returning the model's row id.
pub fn seed_provider_and_model(
    conn: &Connection,
    provider_id: &str,
    model_id: &str,
    fmt: TargetFormat,
) -> ModelRowId {
    providers::create(
        conn,
        providers::NewProvider {
            id: &ProviderId::new(provider_id),
            name: provider_id,
            base_url: "https://example.com",
            auth_type: AuthType::Bearer,
            format: match fmt {
                TargetFormat::Openai => ProviderFormat::Openai,
                TargetFormat::Anthropic => ProviderFormat::Anthropic,
                TargetFormat::Gemini => ProviderFormat::Openai,
            },
            extra_headers_json: None,
            auto_activate_keyword: None,
        },
    )
    .expect("seed provider");
    conn.execute(
        "INSERT INTO models(provider_id, model_id, target_format) VALUES (?1, ?2, ?3)",
        rusqlite::params![provider_id, model_id, fmt.as_str()],
    )
    .expect("seed model");
    let id: i64 = conn
        .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
        .expect("last_insert_rowid");
    ModelRowId(id)
}

/// Build a `PipelineRequest` with sensible defaults.
pub fn make_request(combo_id: ComboId) -> (PipelineRequest, watch::Sender<bool>) {
    let (_dis_tx, dis_rx) = watch::channel(false);
    let req = PipelineRequest {
        request_id: RequestId::new(),
        trace_id: TraceId::new(),
        combo_id,
        openai_request: std::sync::Arc::new(OpenAIRequest {
            model: "any".into(),
            messages: vec![OpenAIMessage {
                role: "user".into(),
                content: Some(serde_json::Value::String("hi".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            }],
            stream: false,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            top_k: None,
            user: None,
            extra: serde_json::Map::new(),
        }),
        client_disconnected: dis_rx,
        // Use Discard sink for non-streaming test requests. The
        // pipeline forces stream=true to the upstream, but SSE
        // chunks are discarded — the pipeline accumulates the
        // response internally via ResponseAccumulator.
        stream_sink: Some(crate::race_sink::StreamSink::Discard),
        api_key_id: None,
        combo_override: None,
        targets_override: None,
        request_headers: std::collections::BTreeMap::new(),
        request_body_json: None,
        race_cancelled: false,
        race_cancel: None,
        endpoint_kind: crate::endpoint::EndpointKind::Chat,
    };
    (req, _dis_tx)
}

pub fn make_request_with_model(model: &str) -> OpenAIRequest {
    OpenAIRequest {
        model: model.into(),
        messages: vec![OpenAIMessage {
            role: "user".into(),
            content: Some(serde_json::Value::String("hi".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        }],
        stream: false,
        temperature: None,
        max_tokens: None,
        top_p: None,
        stop: None,
        tools: None,
        tool_choice: None,
        top_k: None,
        user: None,
        extra: serde_json::Map::new(),
    }
}

/// Minimal `ProviderAdapter` impl for tests that just need URL/header
/// plumbing without any per-format normalization.
pub struct MockAdapter {
    pub config: ProviderAdapterConfig,
}

impl MockAdapter {
    pub fn new(id: &str, base_url: String, format: AdapterFormat) -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new(id),
                base_url,
                auth_type: AdapterAuthType::Bearer,
                format,
                extra_headers: Vec::new(),
            },
        }
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for MockAdapter {
    fn id(&self) -> &ProviderId {
        &self.config.id
    }
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }
    fn build_chat_url(
        &self,
        _target_format: TargetFormat,
        _model: &crate::ids::ModelId,
    ) -> String {
        self.config.base_url.clone()
    }
    fn build_auth_header(&self, api_key: &str) -> (String, String) {
        ("Authorization".into(), format!("Bearer {api_key}"))
    }
    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &crate::ids::ModelId,
    ) -> Vec<(String, String)> {
        vec![
            self.build_auth_header(api_key),
            ("Content-Type".into(), "application/json".into()),
        ]
    }
    fn models_url(&self) -> Option<String> {
        None
    }
    async fn fetch_models(
        &self,
        _upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
        _api_key: &str,
    ) -> Result<Vec<crate::models::DiscoveredModel>> {
        Ok(Vec::new())
    }
}

pub fn test_config_with_adapters(master_key: Arc<MasterKey>) -> PipelineConfig {
    let mut cfg = test_config(master_key);
    cfg.adapters = Arc::new(crate::adapters::builtin_adapters());
    cfg
}

pub fn seed_solo_combo_at_url(
    conn: &Connection,
    provider_id: &str,
    upstream_url: &str,
    master_key: &MasterKey,
) -> (ComboId, AccountId) {
    providers::create(
        conn,
        providers::NewProvider {
            id: &ProviderId::new(provider_id),
            name: provider_id,
            base_url: upstream_url,
            auth_type: AuthType::Bearer,
            format: ProviderFormat::Openai,
            extra_headers_json: None,
            auto_activate_keyword: None,
        },
    )
    .expect("seed provider");
    conn.execute(
        "INSERT INTO models(provider_id, model_id, target_format) \
         VALUES (?1, 'm', 'openai')",
        [provider_id],
    )
    .expect("seed model");
    let model_rowid: i64 = conn
        .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
        .expect("last_insert_rowid");
    let combo_id =
        combos::create_combo(conn, "c", combos::Strategy::Priority, 1).expect("create combo");
    let account_id = crate::accounts::create(
        conn,
        &ProviderId::new(provider_id),
        Some("sk-test"),
        master_key,
        Some("a1"),
        10,
        None,
    )
    .expect("seed account");
    combos::add_target(
        conn,
        combos::AddTargetInput {
            combo_id,
            provider_id: ProviderId::new(provider_id),
            model_row_id: Some(ModelRowId(model_rowid)),
            account_id: Some(account_id),
            priority_order: 1,
            sub_combo_id: None,
        },
    )
    .expect("add target");
    (combo_id, account_id)
}

pub fn test_config_with_mock(master_key: Arc<MasterKey>, base_url: String) -> PipelineConfig {
    let defaults = Timeouts::from_config(&TimeoutsConfig::default());
    let mock = MockAdapter {
        config: ProviderAdapterConfig {
            id: ProviderId::new("test-mock-sse"),
            base_url,
            auth_type: AdapterAuthType::Bearer,
            format: AdapterFormat::Openai,
            extra_headers: Vec::new(),
        },
    };
    PipelineConfig {
        defaults,
        racing: RacingConfig::default(),
        retries: RetriesConfig::default(),
        max_attempts: 1,
        master_key,
        adapters: Arc::new(vec![Arc::new(mock) as Arc<dyn ProviderAdapter>]),
        http_client: reqwest::Client::new(),
        cooldown_secs: 60,
        cooldown_max_secs: 3600,
        cooldown_factor: 2,
        upstream_client: UpstreamClient::new(),
        oauth_provider_registry: None,
        compression_mode: crate::compression::CompressionMode::Off,
        idle_chunk_retryable: true,
        quota_protection: crate::config::QuotaProtectionConfig::default(),
            background_tx: tokio::sync::mpsc::channel(1).0,
    }
}

pub fn seed_target_with_account(
    conn: &Connection,
    combo_id: ComboId,
    provider_id: &str,
    model_id: &str,
    api_key: Option<&str>,
    master_key: &MasterKey,
    priority: u32,
) -> (ComboId, crate::ids::ComboTargetId, AccountId, ModelRowId) {
    let model_rowid = seed_provider_and_model(conn, provider_id, model_id, TargetFormat::Openai);
    let account_id = crate::accounts::create(
        conn,
        &ProviderId::new(provider_id),
        api_key,
        master_key,
        Some("label"),
        10,
        None,
    )
    .expect("create account");
    let target_id = combos::add_target(
        conn,
        combos::AddTargetInput {
            combo_id,
            provider_id: ProviderId::new(provider_id),
            model_row_id: Some(model_rowid),
            account_id: Some(account_id),
            priority_order: priority as i32,
            sub_combo_id: None,
        },
    )
    .expect("add target");
    (combo_id, target_id, account_id, model_rowid)
}
