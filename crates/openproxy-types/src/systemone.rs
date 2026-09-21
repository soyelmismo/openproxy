use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

impl_string_enum! {
    /// Primitive types for System One (Jev / Laya) questions.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash, Default)]
    #[serde(rename_all = "lowercase")]
    pub enum SystemOneQuestionType {
        #[default]
        Choice => "choice",
        Score => "score",
        Noul => "noul",
    }
    core_error: "systemone_question_type"
}

/// A single question in a System One request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SystemOneQuestion {
    #[serde(default, rename = "type")]
    pub question_type: SystemOneQuestionType,
    #[serde(default)]
    pub instructions: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criteria: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
}

/// A request to the System One protocol (`POST /v1/systemone`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SystemOneRequest {
    /// State can be unstructured text (e.g. ticket, code, prompt) or structured JSON.
    pub state: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub questions: BTreeMap<String, SystemOneQuestion>,
}

/// A calibrated answer to a question produced by a System One model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SystemOneAnswer {
    #[serde(default, rename = "type")]
    pub question_type: SystemOneQuestionType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noul: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probabilities: Option<BTreeMap<String, f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legend: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
}

/// Usage tokens accounting for a System One evaluation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SystemOneUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
}

impl SystemOneUsage {
    pub fn total(&self) -> u64 {
        self.total_tokens
            .unwrap_or(self.input_tokens + self.output_tokens)
    }
}

/// A response from the System One protocol (`POST /v1/systemone`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SystemOneResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub answers: BTreeMap<String, SystemOneAnswer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<SystemOneUsage>,
}
