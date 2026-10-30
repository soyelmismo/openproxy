use openproxy_db::DbPool;
use openproxy_types::{
    EndpointKind, UsageInput,
    ids::{
        AccountId, ApiKeyId, ComboId, ComboTargetId, ModelRowId, ProviderId, RequestId, TraceId,
    },
};

use super::UnaryTarget;

#[derive(Debug, Clone)]
pub struct UnaryUsageArgs<'a> {
    pub request_id: RequestId,
    pub api_key_id: Option<ApiKeyId>,
    pub provider_id: &'a ProviderId,
    pub account_id: Option<AccountId>,
    pub combo_id: Option<ComboId>,
    pub combo_target_id: Option<ComboTargetId>,
    pub model_row_id: Option<ModelRowId>,
    pub upstream_model_id: &'a str,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub status_code: u16,
    pub error_msg: Option<String>,
    pub total_ms: u64,
    pub endpoint_kind: EndpointKind,
    pub request_body_json: Option<bytes::Bytes>,
    pub response_body_json: Option<serde_json::Value>,
    /// Duration of the submitted audio in milliseconds. Only set on the
    /// transcription path, where billing is per hour of audio rather than per
    /// token.
    pub audio_ms: Option<u32>,
}

impl<'a> UnaryUsageArgs<'a> {
    pub fn builder(
        provider_id: &'a ProviderId,
        upstream_model_id: &'a str,
        endpoint_kind: EndpointKind,
    ) -> UnaryUsageArgsBuilder<'a> {
        UnaryUsageArgsBuilder {
            request_id: RequestId::new(),
            api_key_id: None,
            provider_id,
            account_id: None,
            combo_id: None,
            combo_target_id: None,
            model_row_id: None,
            upstream_model_id,
            prompt_tokens: None,
            completion_tokens: None,
            status_code: 200,
            error_msg: None,
            total_ms: 0,
            endpoint_kind,
            request_body_json: None,
            response_body_json: None,
            audio_ms: None,
        }
    }

    pub fn from_target(
        target: &'a UnaryTarget,
        endpoint_kind: EndpointKind,
        status_code: u16,
        total_ms: u64,
    ) -> Self {
        let mut args = Self::builder(&target.provider, &target.upstream_model, endpoint_kind)
            .status_code(status_code)
            .total_ms(total_ms)
            .build();
        args.account_id = target.account_id;
        args.combo_id = target.combo_id;
        args.combo_target_id = target.combo_target_id;
        args.model_row_id = target.model_row_id;
        args
    }

    pub fn with_request_id(mut self, request_id: RequestId) -> Self {
        self.request_id = request_id;
        self
    }

    pub fn with_api_key_id(mut self, api_key_id: Option<ApiKeyId>) -> Self {
        self.api_key_id = api_key_id;
        self
    }

    pub fn with_tokens(
        mut self,
        prompt_tokens: Option<u32>,
        completion_tokens: Option<u32>,
    ) -> Self {
        self.prompt_tokens = prompt_tokens;
        self.completion_tokens = completion_tokens;
        self
    }

    /// Record the submitted audio duration (milliseconds) on a transcription
    /// usage row. STT is billed per hour of audio, so this — not a token count —
    /// is the quantity the price multiplies.
    pub fn with_audio_ms(mut self, audio_ms: Option<u32>) -> Self {
        self.audio_ms = audio_ms;
        self
    }

    pub fn with_json(
        mut self,
        request_body_json: Option<bytes::Bytes>,
        response_body_json: Option<serde_json::Value>,
    ) -> Self {
        self.request_body_json = request_body_json;
        self.response_body_json = response_body_json;
        self
    }
}

pub struct UnaryUsageArgsBuilder<'a> {
    request_id: RequestId,
    api_key_id: Option<ApiKeyId>,
    provider_id: &'a ProviderId,
    account_id: Option<AccountId>,
    combo_id: Option<ComboId>,
    combo_target_id: Option<ComboTargetId>,
    model_row_id: Option<ModelRowId>,
    upstream_model_id: &'a str,
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    status_code: u16,
    error_msg: Option<String>,
    total_ms: u64,
    endpoint_kind: EndpointKind,
    request_body_json: Option<bytes::Bytes>,
    response_body_json: Option<serde_json::Value>,
    audio_ms: Option<u32>,
}

impl<'a> UnaryUsageArgsBuilder<'a> {
    pub fn request_id(mut self, request_id: RequestId) -> Self {
        self.request_id = request_id;
        self
    }

    pub fn api_key_id(mut self, api_key_id: Option<ApiKeyId>) -> Self {
        self.api_key_id = api_key_id;
        self
    }

    pub fn status_code(mut self, status_code: u16) -> Self {
        self.status_code = status_code;
        self
    }

    pub fn error_msg(mut self, error_msg: Option<String>) -> Self {
        self.error_msg = error_msg;
        self
    }

    pub fn total_ms(mut self, total_ms: u64) -> Self {
        self.total_ms = total_ms;
        self
    }

    pub fn prompt_tokens(mut self, prompt_tokens: Option<u32>) -> Self {
        self.prompt_tokens = prompt_tokens;
        self
    }

    pub fn completion_tokens(mut self, completion_tokens: Option<u32>) -> Self {
        self.completion_tokens = completion_tokens;
        self
    }

    pub fn request_body_json(mut self, request_body_json: Option<bytes::Bytes>) -> Self {
        self.request_body_json = request_body_json;
        self
    }

    pub fn audio_ms(mut self, audio_ms: Option<u32>) -> Self {
        self.audio_ms = audio_ms;
        self
    }

    pub fn response_body_json(mut self, response_body_json: Option<serde_json::Value>) -> Self {
        self.response_body_json = response_body_json;
        self
    }

    pub fn build(self) -> UnaryUsageArgs<'a> {
        UnaryUsageArgs {
            request_id: self.request_id,
            api_key_id: self.api_key_id,
            provider_id: self.provider_id,
            account_id: self.account_id,
            combo_id: self.combo_id,
            combo_target_id: self.combo_target_id,
            model_row_id: self.model_row_id,
            upstream_model_id: self.upstream_model_id,
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            status_code: self.status_code,
            error_msg: self.error_msg,
            total_ms: self.total_ms,
            endpoint_kind: self.endpoint_kind,
            request_body_json: self.request_body_json,
            response_body_json: self.response_body_json,
            audio_ms: self.audio_ms,
        }
    }
}

fn unary_usage_input(args: &UnaryUsageArgs<'_>) -> UsageInput {
    UsageInput {
        proxy_url: None,
        proxy_status: None,
        request_id: args.request_id,
        trace_id: TraceId::new().to_string(),
        attempt: 1,
        provider_id: args.provider_id.clone(),
        account_id: args.account_id,
        combo_id: args.combo_id,
        combo_target_id: args.combo_target_id,
        model_row_id: args.model_row_id,
        upstream_model_id: args.upstream_model_id.to_string(),
        prompt_tokens: args.prompt_tokens,
        completion_tokens: args.completion_tokens,
        cached_tokens: None,
        connect_ms: None,
        ttft_ms: None,
        total_ms: args.total_ms,
        status_code: args.status_code,
        error_msg: args.error_msg.clone(),
        race_total: 1,
        api_key_id: args.api_key_id,
        request_body_json: args.request_body_json.clone(),
        response_body_json: args.response_body_json.clone(),
        request_headers: None,
        response_headers: None,
        error_message: args.error_msg.clone(),
        race_attempts: 1,
        stop_reason: None,
        compression_savings_pct: None,
        compression_techniques: None,
        pii_redacted: None,
        flags: openproxy_types::usage::USAGE_FLAG_CLIENT_RESPONSE,
        endpoint_kind: args.endpoint_kind,
        audio_ms: args.audio_ms,
    }
}

/// Blocking counterpart for synchronous routing code and non-Tokio consumers.
pub fn record_unary_usage(db_pool: &DbPool, args: &UnaryUsageArgs<'_>) {
    let job =
        openproxy_pipeline::worker::BackgroundJob::RecordUsage(Box::new(unary_usage_input(args)));
    let result = {
        let conn = db_pool.writer_arc();
        openproxy_pipeline::worker::process_job(&conn, job)
    };
    if let Err(error) = result {
        tracing::error!(%error, "unary usage persistence failed; admitted jobs remain journaled");
    }
}

/// Async callers wait for persistence without acquiring SQLite locks on Tokio.
pub async fn record_unary_usage_async(db_pool: &DbPool, args: &UnaryUsageArgs<'_>) {
    let job =
        openproxy_pipeline::worker::BackgroundJob::RecordUsage(Box::new(unary_usage_input(args)));
    let conn = db_pool.writer_arc();
    match tokio::task::spawn_blocking(move || openproxy_pipeline::worker::process_job(&conn, job))
        .await
    {
        Ok(Ok(())) => {}
        result => tracing::error!(
            ?result,
            "unary usage persistence failed; admitted jobs remain journaled"
        ),
    }
}

/// Helper to record successful unary execution usage asynchronously.
pub async fn record_unary_target_success_async(
    db_pool: &DbPool,
    target: &UnaryTarget,
    endpoint_kind: EndpointKind,
    api_key_id: Option<ApiKeyId>,
    status_code: u16,
    total_ms: u64,
    tokens: (Option<u32>, Option<u32>),
) {
    let mut args = UnaryUsageArgs::from_target(target, endpoint_kind, status_code, total_ms)
        .with_api_key_id(api_key_id);
    if tokens.0.is_some() || tokens.1.is_some() {
        args = args.with_tokens(tokens.0, tokens.1);
    }
    record_unary_usage_async(db_pool, &args).await;
}
