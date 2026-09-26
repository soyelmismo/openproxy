use openproxy_types::OpenAIUsage;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub(crate) struct StreamToolCall {
    pub(crate) tool_index: usize,
    pub(crate) output_index: usize,
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
    pub(crate) done_emitted: bool,
}

#[derive(Deserialize)]
pub(crate) struct ErrorProbe<'a> {
    pub(crate) error: Option<serde_json::Value>,
    #[serde(borrow)]
    pub(crate) r#type: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    pub(crate) message: Option<std::borrow::Cow<'a, str>>,
    pub(crate) code: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesSseProbe<'a> {
    #[serde(borrow)]
    pub id: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    pub choices: Option<Vec<ResponsesChoiceProbe<'a>>>,
    pub usage: Option<ResponsesUsageProbe>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesChoiceProbe<'a> {
    #[serde(borrow)]
    pub delta: Option<ResponsesDeltaProbe<'a>>,
    #[serde(borrow)]
    pub finish_reason: Option<std::borrow::Cow<'a, str>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesDeltaProbe<'a> {
    #[serde(borrow)]
    pub content: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    pub reasoning_content: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    pub tool_calls: Option<Vec<ResponsesToolCallProbe<'a>>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesToolCallProbe<'a> {
    pub index: Option<usize>,
    #[serde(borrow)]
    pub id: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    pub function: Option<ResponsesFunctionCallProbe<'a>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesFunctionCallProbe<'a> {
    #[serde(borrow)]
    pub name: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    pub arguments: Option<std::borrow::Cow<'a, str>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesTokensDetailsProbe {
    #[serde(default)]
    pub cached_tokens: Option<u32>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u32>,
}

impl ResponsesTokensDetailsProbe {
    pub fn extract_cached(&self) -> Option<u32> {
        match (
            self.cached_tokens,
            self.cache_read_input_tokens,
            self.prompt_cache_hit_tokens,
        ) {
            (Some(v), _, _) if v > 0 => Some(v),
            (_, Some(v), _) if v > 0 => Some(v),
            (_, _, Some(v)) if v > 0 => Some(v),
            (Some(0), _, _) | (_, Some(0), _) | (_, _, Some(0)) => Some(0),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesUsageProbe {
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub prompt_tokens_details: Option<ResponsesTokensDetailsProbe>,
    pub input_tokens_details: Option<ResponsesTokensDetailsProbe>,
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u32>,
    #[serde(default)]
    pub cached_tokens: Option<u32>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u32>,
}

impl ResponsesUsageProbe {
    pub fn to_openai_usage(&self) -> OpenAIUsage {
        let pt = self.prompt_tokens.or(self.input_tokens).unwrap_or(0);
        let ct = self.completion_tokens.or(self.output_tokens).unwrap_or(0);
        let tt = self.total_tokens.unwrap_or_else(|| pt.saturating_add(ct));
        let details_cached = self
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.extract_cached())
            .or_else(|| {
                self.input_tokens_details
                    .as_ref()
                    .and_then(|d| d.extract_cached())
            });

        let root_cached = match (
            self.cached_tokens,
            self.cache_read_input_tokens,
            self.prompt_cache_hit_tokens,
        ) {
            (Some(v), _, _) if v > 0 => Some(v),
            (_, Some(v), _) if v > 0 => Some(v),
            (_, _, Some(v)) if v > 0 => Some(v),
            (Some(0), _, _) | (_, Some(0), _) | (_, _, Some(0)) => Some(0),
            _ => None,
        };

        let cached = match (details_cached, root_cached) {
            (Some(d), _) if d > 0 => Some(d),
            (_, Some(r)) if r > 0 => Some(r),
            (Some(0), _) | (_, Some(0)) => Some(0),
            _ => None,
        };

        let details = cached.map(|c| openproxy_types::PromptTokensDetails {
            cached_tokens: Some(c),
        });

        OpenAIUsage {
            prompt_tokens: pt,
            completion_tokens: ct,
            total_tokens: tt,
            prompt_tokens_details: details,
        }
    }
}
