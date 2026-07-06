
use crate::error::CoreError;
use crate::models::{Model, TargetFormat};
use crate::pipeline::PipelineRequest;
use crate::translation::OpenAIMessage;
use crate::adapters::ProviderAdapter;

pub trait TargetFormatter: Send + Sync {
    fn format_request(
        &self,
        req: &PipelineRequest,
        model: &Model,
        messages_ref: &[OpenAIMessage],
        stream: bool,
        adapter: &dyn ProviderAdapter,
    ) -> Result<bytes::Bytes, CoreError>;
}

pub struct OpenaiFormatter;
impl TargetFormatter for OpenaiFormatter {
    fn format_request(
        &self,
        req: &PipelineRequest,
        model: &Model,
        messages_ref: &[OpenAIMessage],
        stream: bool,
        adapter: &dyn ProviderAdapter,
    ) -> Result<bytes::Bytes, CoreError> {
        let mut view = crate::translation::OpenAIRequestView::new(
            &req.openai_request,
            model.model_id.as_str(),
            messages_ref,
            stream,
        );
        adapter.normalize_openai_request(&mut view);
        match serde_json::to_vec(&view) {
            Ok(v) => Ok(bytes::Bytes::from(v)),
            Err(e) => Err(CoreError::Parse(format!("serialize openai request: {}", e))),
        }
    }
}

pub struct AnthropicFormatter;
impl TargetFormatter for AnthropicFormatter {
    fn format_request(
        &self,
        req: &PipelineRequest,
        model: &Model,
        messages_ref: &[OpenAIMessage],
        stream: bool,
        _adapter: &dyn ProviderAdapter,
    ) -> Result<bytes::Bytes, CoreError> {
        let anthro = crate::translation::openai_to_anthropic(
            &req.openai_request,
            model.model_id.as_str(),
            messages_ref,
            stream,
        );
        match serde_json::to_vec(&anthro) {
            Ok(v) => Ok(bytes::Bytes::from(v)),
            Err(e) => Err(CoreError::Parse(format!("serialize anthropic request: {}", e))),
        }
    }
}

pub struct GeminiFormatter;
impl TargetFormatter for GeminiFormatter {
    fn format_request(
        &self,
        req: &PipelineRequest,
        _model: &Model,
        messages_ref: &[OpenAIMessage],
        _stream: bool,
        _adapter: &dyn ProviderAdapter,
    ) -> Result<bytes::Bytes, CoreError> {
        let gemini = crate::translation::openai_to_gemini(&req.openai_request, messages_ref);
        match serde_json::to_vec(&gemini) {
            Ok(v) => Ok(bytes::Bytes::from(v)),
            Err(e) => Err(CoreError::Parse(format!("serialize gemini request: {}", e))),
        }
    }
}

pub fn get_formatter(target_format: TargetFormat) -> Box<dyn TargetFormatter> {
    match target_format {
        TargetFormat::Openai => Box::new(OpenaiFormatter),
        TargetFormat::Anthropic => Box::new(AnthropicFormatter),
        TargetFormat::Gemini => Box::new(GeminiFormatter),
    }
}
