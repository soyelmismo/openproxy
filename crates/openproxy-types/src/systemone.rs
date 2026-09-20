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
    #[serde(rename = "type")]
    pub question_type: SystemOneQuestionType,
    pub instructions: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criteria: Option<serde_json::Value>,
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
    #[serde(rename = "type")]
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
}

/// Usage tokens accounting for a System One evaluation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SystemOneUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

/// A response from the System One protocol (`POST /v1/systemone`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SystemOneResponse {
    pub model: String,
    pub answers: BTreeMap<String, SystemOneAnswer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<SystemOneUsage>,
}
