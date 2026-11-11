use serde::{Deserialize, Serialize};

fn deserialize_flex_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct FlexBoolVisitor;

    impl<'de> serde::de::Visitor<'de> for FlexBoolVisitor {
        type Value = Option<bool>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a boolean, integer (0 or 1), or null")
        }

        fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
            Ok(Some(v))
        }

        fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
            Ok(Some(v != 0))
        }

        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Some(v != 0))
        }

        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_any(FlexBoolVisitor)
        }
    }

    deserializer.deserialize_option(FlexBoolVisitor)
}

/// Capability flags surfaced to clients via `GET /v1/models`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelCapabilities {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_flex_bool"
    )]
    pub vision: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_flex_bool"
    )]
    pub tool_calling: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_flex_bool"
    )]
    pub reasoning: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_flex_bool"
    )]
    pub thinking: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_flex_bool"
    )]
    pub attachment: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_flex_bool"
    )]
    pub structured_output: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_flex_bool"
    )]
    pub temperature: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_flex_bool"
    )]
    pub decisions: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_flex_bool"
    )]
    pub streaming: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_flex_bool"
    )]
    pub video: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_flex_bool"
    )]
    pub image: Option<bool>,
}

impl ModelCapabilities {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        [
            self.vision,
            self.tool_calling,
            self.reasoning,
            self.thinking,
            self.attachment,
            self.structured_output,
            self.temperature,
            self.decisions,
            self.streaming,
            self.video,
        ]
        .iter()
        .all(Option::is_none)
    }

    pub fn to_json(&self) -> Option<String> {
        if self.is_empty() {
            None
        } else {
            serde_json::to_string(self).ok()
        }
    }

    pub fn from_json(s: Option<&str>) -> Self {
        s.and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(Self::empty)
    }

    pub fn merge_union(&mut self, other: &Self) {
        if other.vision == Some(true) {
            self.vision = Some(true);
        }
        if other.tool_calling == Some(true) {
            self.tool_calling = Some(true);
        }
        if other.reasoning == Some(true) {
            self.reasoning = Some(true);
        }
        if other.thinking == Some(true) {
            self.thinking = Some(true);
        }
        if other.attachment == Some(true) {
            self.attachment = Some(true);
        }
        if other.structured_output == Some(true) {
            self.structured_output = Some(true);
        }
        if other.temperature == Some(true) {
            self.temperature = Some(true);
        }
        if other.decisions == Some(true) {
            self.decisions = Some(true);
        }
        if other.video == Some(true) {
            self.video = Some(true);
        }
        if other.image == Some(true) {
            self.image = Some(true);
        }
    }
}
