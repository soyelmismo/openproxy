use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroRequest {
    #[serde(rename = "conversationState")]
    pub conversation_state: KiroConversationState,
    #[serde(rename = "profileArn", skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,
    #[serde(rename = "inferenceConfig", skip_serializing_if = "Option::is_none")]
    pub inference_config: Option<KiroInferenceConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroConversationState {
    #[serde(rename = "currentMessage")]
    pub current_message: KiroCurrentMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<KiroHistoryTurn>>,
    #[serde(rename = "chatTriggerType")]
    pub chat_trigger_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroCurrentMessage {
    #[serde(rename = "userInputMessage")]
    pub user_input_message: KiroUserInputMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KiroUserInputMessage {
    pub content: String,
    #[serde(rename = "modelId")]
    pub model_id: String,
    pub origin: String,
    #[serde(
        rename = "userInputMessageContext",
        skip_serializing_if = "Option::is_none"
    )]
    pub user_input_message_context: Option<KiroUserInputMessageContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KiroUserInputMessageContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    #[serde(rename = "toolResults", skip_serializing_if = "Option::is_none")]
    pub tool_results: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KiroHistoryTurn {
    User {
        #[serde(rename = "userInputMessage")]
        user_input_message: KiroUserInputMessage,
    },
    Assistant {
        #[serde(rename = "assistantResponseMessage")]
        assistant_response_message: KiroAssistantResponseMessage,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KiroAssistantResponseMessage {
    #[serde(default)]
    pub content: String,
    #[serde(rename = "toolUses", skip_serializing_if = "Option::is_none")]
    pub tool_uses: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroInferenceConfig {
    #[serde(rename = "maxTokens", skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(rename = "topP", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
}
