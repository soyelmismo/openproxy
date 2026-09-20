#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::{
    Router,
    body::Body,
    extract::State as AxumState,
    http::{HeaderMap, Method, Request, StatusCode, header},
    response::{IntoResponse, Json as AxumJson, Response},
    routing::{get, post},
};
use bytes::Bytes;
use http_body_util::BodyExt;
use openproxy_core::{
    AppConfig,
    admin::{self, CreateCustomModelInput},
    api_keys::CreateApiKeyInput,
};
use openproxy_db::secrets::MasterKey;
use openproxy_db::testing::TempDir;
use openproxy_db::{DbPool, migrations};
use openproxy_server::{router::build_router, state::AppState};
use openproxy_types::ids::{ApiKeyId, ProviderId};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tower::ServiceExt;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedRequest {
    pub path: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

#[derive(Debug, Default)]
pub struct MockState {
    pub catalog: Mutex<Vec<String>>,
    pub error_status: Mutex<Option<StatusCode>>,
    pub error_body: Mutex<Option<String>>,
    pub delay: Mutex<Option<Duration>>,
    pub recorded_requests: Mutex<Vec<RecordedRequest>>,
}

impl MockState {
    pub fn new(initial: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            catalog: Mutex::new(initial),
            ..Default::default()
        })
    }
}

#[derive(Clone)]
pub struct MockHandle(pub Arc<MockState>);

impl MockHandle {
    pub fn replace_catalog(&self, ids: Vec<String>) {
        *self.0.catalog.lock() = ids;
    }
    pub fn set_error(&self, status: StatusCode, body: Option<&str>) {
        *self.0.error_status.lock() = Some(status);
        *self.0.error_body.lock() = body.map(ToString::to_string);
    }
    pub fn clear_error(&self) {
        *self.0.error_status.lock() = None;
        *self.0.error_body.lock() = None;
    }
    pub fn set_delay(&self, delay: Duration) {
        *self.0.delay.lock() = Some(delay);
    }
    pub fn clear_delay(&self) {
        *self.0.delay.lock() = None;
    }
    pub fn recorded_requests(&self) -> Vec<RecordedRequest> {
        self.0.recorded_requests.lock().clone()
    }
    pub fn clear_recorded_requests(&self) {
        self.0.recorded_requests.lock().clear();
    }
}

async fn mock_models_handler(AxumState(state): AxumState<Arc<MockState>>) -> impl IntoResponse {
    let ids = state.catalog.lock().clone();
    let data: Vec<Value> = ids.into_iter().map(|id| json!({"id": id, "object": "model", "created": 1_700_000_000, "owned_by": "mock_upstream"})).collect();
    (
        StatusCode::OK,
        AxumJson(json!({"object": "list", "data": data})),
    )
}

async fn mock_chat_handler(
    AxumState(state): AxumState<Arc<MockState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let delay = *state.delay.lock();
    if let Some(d) = delay {
        tokio::time::sleep(d).await;
    }
    if let Some(status) = *state.error_status.lock() {
        let err_msg =
            state.error_body.lock().clone().unwrap_or_else(|| {
                json!({"error": {"message": "injected mock error"}}).to_string()
            });
        return (
            status,
            [(header::CONTENT_TYPE, "application/json")],
            err_msg,
        )
            .into_response();
    }
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let rec_headers = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    state.recorded_requests.lock().push(RecordedRequest {
        path: "/chat/completions".into(),
        method: "POST".into(),
        headers: rec_headers,
        body: parsed.clone(),
    });

    let model = parsed
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("mock-gpt-4");
    if parsed
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let sse = format!(
            "data: {{\"id\":\"chatcmpl-mock\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"{model}\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"Hello\"}},\"finish_reason\":null}}]}}\n\n\
             data: {{\"id\":\"chatcmpl-mock\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"{model}\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\" from mock!\"}},\"finish_reason\":null}}]}}\n\n\
             data: {{\"id\":\"chatcmpl-mock\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"{model}\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":12,\"completion_tokens\":5,\"total_tokens\":17}}}}\n\n\
             data: [DONE]\n\n"
        );
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")],
            sse,
        )
            .into_response()
    } else {
        let resp = json!({"id": "chatcmpl-mock", "object": "chat.completion", "created": 1_700_000_000, "model": model, "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hello from mock upstream!"}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 12, "completion_tokens": 5, "total_tokens": 17}});
        (StatusCode::OK, AxumJson(resp)).into_response()
    }
}

async fn mock_embeddings_handler(
    AxumState(state): AxumState<Arc<MockState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let delay = *state.delay.lock();
    if let Some(d) = delay {
        tokio::time::sleep(d).await;
    }
    if let Some(status) = *state.error_status.lock() {
        let err_msg =
            state.error_body.lock().clone().unwrap_or_else(|| {
                json!({"error": {"message": "injected mock error"}}).to_string()
            });
        return (
            status,
            [(header::CONTENT_TYPE, "application/json")],
            err_msg,
        )
            .into_response();
    }
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let rec_headers = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    state.recorded_requests.lock().push(RecordedRequest {
        path: "/embeddings".into(),
        method: "POST".into(),
        headers: rec_headers,
        body: parsed.clone(),
    });

    let model = parsed
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("mock-embed");
    let count = match parsed.get("input") {
        Some(Value::Array(a)) => a.len(),
        Some(Value::String(_)) => 1,
        _ => 1,
    };
    let data: Vec<Value> = (0..count).map(|idx| json!({"object": "embedding", "embedding": [0.012, -0.034, 0.056, 0.078], "index": idx})).collect();
    let resp = json!({"object": "list", "data": data, "model": model, "usage": {"prompt_tokens": 8 * count, "total_tokens": 8 * count}});
    (StatusCode::OK, AxumJson(resp)).into_response()
}

async fn mock_systemone_handler(
    AxumState(state): AxumState<Arc<MockState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let delay = *state.delay.lock();
    if let Some(d) = delay {
        tokio::time::sleep(d).await;
    }
    if let Some(status) = *state.error_status.lock() {
        let err_msg =
            state.error_body.lock().clone().unwrap_or_else(|| {
                json!({"error": {"message": "injected mock error"}}).to_string()
            });
        return (
            status,
            [(header::CONTENT_TYPE, "application/json")],
            err_msg,
        )
            .into_response();
    }
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let rec_headers = headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    state.recorded_requests.lock().push(RecordedRequest {
        path: "/systemone".into(),
        method: "POST".into(),
        headers: rec_headers,
        body: parsed.clone(),
    });

    let state_text = parsed.get("state").and_then(Value::as_str).unwrap_or("");
    let questions = parsed.get("questions").and_then(Value::as_object);

    let mut answers = serde_json::Map::new();
    if let Some(q_map) = questions {
        for (q_id, q_val) in q_map {
            let opt_strs: Vec<&str> = if let Some(opts) = q_val.get("options").and_then(Value::as_array) {
                opts.iter().filter_map(Value::as_str).collect()
            } else if let Some(crit) = q_val.get("criteria").and_then(Value::as_object) {
                crit.keys().map(String::as_str).collect()
            } else {
                Vec::new()
            };
            let choice = if !opt_strs.is_empty() {
                let chosen = opt_strs
                    .iter()
                    .copied()
                    .find(|opt| {
                        let st = state_text.to_lowercase();
                        let criteria_desc = q_val
                            .get("criteria")
                            .and_then(|c| c.get(*opt))
                            .and_then(Value::as_str)
                            .unwrap_or(*opt)
                            .to_lowercase();
                        if st.contains("complex") || st.contains("rust") || st.contains("deep") {
                            criteria_desc.contains("deep")
                                || criteria_desc.contains("complex")
                                || criteria_desc.contains("rust")
                                || opt.contains('2')
                                || opt.contains("deep")
                        } else {
                            criteria_desc.contains("fast")
                                || criteria_desc.contains("simple")
                                || criteria_desc.contains("operational")
                                || criteria_desc.contains("ok")
                                || opt.contains('1')
                                || opt.contains("fast")
                                || *opt == "ok"
                        }
                    })
                    .or_else(|| opt_strs.first().copied())
                    .unwrap_or("choice_1");
                Some(chosen.to_string())
            } else {
                None
            };

            answers.insert(
                q_id.clone(),
                json!({
                    "choice": choice,
                    "response": if choice.is_none() { Some("mock answer") } else { None },
                    "probabilities": {
                        choice.as_deref().unwrap_or("choice_1"): 0.98
                    }
                }),
            );
        }
    }

    let resp = json!({
        "answers": answers,
        "usage": {
            "input_tokens": 15,
            "output_tokens": 5,
            "total_tokens": 20
        }
    });

    (StatusCode::OK, AxumJson(resp)).into_response()
}

pub async fn spawn_mock_server(initial_models: Vec<String>) -> (SocketAddr, MockHandle) {
    let state = MockState::new(initial_models);
    let handle = MockHandle(Arc::clone(&state));
    let app = Router::new()
        .route("/v1/models", get(mock_models_handler))
        .route("/models", get(mock_models_handler))
        .route("/v1/chat/completions", post(mock_chat_handler))
        .route("/chat/completions", post(mock_chat_handler))
        .route("/v1/embeddings", post(mock_embeddings_handler))
        .route("/embeddings", post(mock_embeddings_handler))
        .route("/v1/systemone", post(mock_systemone_handler))
        .route("/systemone", post(mock_systemone_handler))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock axum server");
    });
    (addr, handle)
}

pub struct TestHarness {
    pub temp_dir: TempDir,
    pub db_pool: Arc<DbPool>,
    pub master_key: Arc<MasterKey>,
    pub app_state: AppState,
    pub router: Router,
    pub mock_addr: SocketAddr,
    pub mock_handle: MockHandle,
    pub admin_key: String,
    pub client_key: String,
    pub admin_key_id: ApiKeyId,
    pub client_key_id: ApiKeyId,
    pub provider_id: ProviderId,
    pub model_id: String,
}

impl TestHarness {
    pub async fn new() -> Self {
        unsafe {
            std::env::set_var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS", "true");
        }
        let temp_dir = TempDir::new("openproxy-e2e").expect("temp dir");
        let db_pool =
            Arc::new(DbPool::open(&temp_dir.path().join("e2e.db")).expect("open db pool"));
        {
            let mut w = db_pool.writer();
            migrations::run(&mut w).expect("run migrations");
        }
        let master_key = Arc::new(MasterKey::generate().expect("generate master key"));
        let (mock_addr, mock_handle) = spawn_mock_server(vec![
            "mock-gpt-4".into(),
            "mock-claude".into(),
            "mock-embed".into(),
        ])
        .await;
        let provider_id = ProviderId::new("mock-prov");
        let model_id = "mock-gpt-4".to_string();

        {
            let w = db_pool.writer();
            admin::create_provider(
                &w,
                admin::CreateProviderInput {
                    id: provider_id.as_str().into(),
                    name: "Mock Upstream Provider".into(),
                    base_url: format!("http://{mock_addr}/v1"),
                    auth_type: "bearer".into(),
                    format: "openai".into(),
                    extra_headers_json: None,
                    rate_limit_scope: None,
                },
            )
            .expect("create mock provider");
            admin::create_account(
                &w,
                &master_key,
                admin::CreateAccountInput {
                    provider_id: provider_id.as_str().into(),
                    api_key: Some("sk-mock-upstream-key".into()),
                    label: Some("mock-primary".into()),
                    priority: Some(10),
                    extra_config_json: None,
                },
            )
            .expect("create mock account");
            admin::create_custom_model(
                &w,
                CreateCustomModelInput {
                    provider_id: provider_id.as_str().into(),
                    model_id: model_id.clone(),
                    display_name: Some("Mock GPT 4".into()),
                    target_format: "openai".into(),
                    ttl_seconds: 3600,
                    model_type: Some("chat".into()),
                },
            )
            .expect("create custom model");
            admin::create_custom_model(
                &w,
                CreateCustomModelInput {
                    provider_id: provider_id.as_str().into(),
                    model_id: "mock-embed".into(),
                    display_name: Some("Mock Embeddings".into()),
                    target_format: "openai".into(),
                    ttl_seconds: 3600,
                    model_type: Some("embedding".into()),
                },
            )
            .expect("create custom embedding model");
        }

        let mut app_config = AppConfig::default();
        app_config.server.allow_anonymous = false;
        let adapters = Arc::new(parking_lot::RwLock::new(Arc::new(vec![])));
        let app_state = AppState::for_test(
            app_config,
            Arc::clone(&db_pool),
            Arc::clone(&master_key),
            adapters,
        );
        app_state
            .rebuild_adapters()
            .await
            .expect("rebuild adapters");

        let (admin_key_record, admin_key_plaintext) = app_state
            .services()
            .api_keys
            .create(
                CreateApiKeyInput {
                    label: Some("Admin E2E Key".into()),
                    scopes: vec!["manage".into(), "chat".into()],
                    allowed_models: None,
                    allowed_combos: None,
                    blacklisted_providers: None,
                    blacklisted_models: None,
                    expires_at: None,
                },
                "e2e_setup",
            )
            .expect("create admin api key");
        app_state.cache_api_key(Arc::new(admin_key_record.clone()));

        let (client_key_record, client_key_plaintext) = app_state
            .services()
            .api_keys
            .create(
                CreateApiKeyInput {
                    label: Some("Client E2E Key".into()),
                    scopes: vec!["chat".into()],
                    allowed_models: None,
                    allowed_combos: None,
                    blacklisted_providers: None,
                    blacklisted_models: None,
                    expires_at: None,
                },
                "e2e_setup",
            )
            .expect("create client api key");
        app_state.cache_api_key(Arc::new(client_key_record.clone()));

        let router = build_router(app_state.clone());
        Self {
            temp_dir,
            db_pool,
            master_key,
            app_state,
            router,
            mock_addr,
            mock_handle,
            admin_key: admin_key_plaintext,
            client_key: client_key_plaintext,
            admin_key_id: admin_key_record.id,
            client_key_id: client_key_record.id,
            provider_id,
            model_id,
        }
    }

    pub async fn oneshot(&self, mut req: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
        if req
            .extensions()
            .get::<axum::extract::connect_info::ConnectInfo<SocketAddr>>()
            .is_none()
        {
            req.extensions_mut()
                .insert(axum::extract::connect_info::ConnectInfo(SocketAddr::from(
                    ([127, 0, 0, 1], 12345),
                )));
        }
        let resp = self
            .router
            .clone()
            .oneshot(req)
            .await
            .expect("oneshot request");
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes();
        (status, headers, body)
    }

    pub async fn oneshot_json(&self, req: Request<Body>) -> (StatusCode, Value) {
        let (status, _, body) = self.oneshot(req).await;
        let val: Value = serde_json::from_slice(&body)
            .unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&body).to_string()}));
        (status, val)
    }

    pub fn admin_request<T: Into<Body>>(
        &self,
        method: Method,
        uri: &str,
        body: T,
    ) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {}", self.admin_key))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.into())
            .expect("build admin request")
    }

    pub fn client_request<T: Into<Body>>(
        &self,
        method: Method,
        uri: &str,
        body: T,
    ) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {}", self.client_key))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.into())
            .expect("build client request")
    }

    pub async fn admin_get(&self, path: &str) -> (StatusCode, Value) {
        self.oneshot_json(self.admin_request(Method::GET, path, Body::empty()))
            .await
    }
    pub async fn admin_post(&self, path: &str, payload: Value) -> (StatusCode, Value) {
        self.oneshot_json(self.admin_request(Method::POST, path, Body::from(payload.to_string())))
            .await
    }
    pub async fn admin_patch(&self, path: &str, payload: Value) -> (StatusCode, Value) {
        self.oneshot_json(self.admin_request(Method::PATCH, path, Body::from(payload.to_string())))
            .await
    }
    pub async fn admin_delete(&self, path: &str) -> (StatusCode, Value) {
        self.oneshot_json(self.admin_request(Method::DELETE, path, Body::empty()))
            .await
    }

    pub async fn client_chat_call(
        &self,
        model: &str,
        prompt: &str,
        stream: bool,
    ) -> (StatusCode, Value) {
        self.oneshot_json(self.client_request(Method::POST, "/v1/chat/completions", Body::from(json!({"model": model, "messages": [{"role": "user", "content": prompt}], "stream": stream}).to_string()))).await
    }

    pub async fn client_chat_raw(&self, payload: Value) -> (StatusCode, HeaderMap, Bytes) {
        self.oneshot(self.client_request(
            Method::POST,
            "/v1/chat/completions",
            Body::from(payload.to_string()),
        ))
        .await
    }

    pub async fn client_embeddings_call(&self, model: &str, input: Value) -> (StatusCode, Value) {
        self.oneshot_json(self.client_request(
            Method::POST,
            "/v1/embeddings",
            Body::from(json!({"model": model, "input": input}).to_string()),
        ))
        .await
    }

    pub async fn client_systemone_call(&self, payload: Value) -> (StatusCode, Value) {
        self.oneshot_json(self.client_request(
            Method::POST,
            "/v1/systemone",
            Body::from(payload.to_string()),
        ))
        .await
    }
}
