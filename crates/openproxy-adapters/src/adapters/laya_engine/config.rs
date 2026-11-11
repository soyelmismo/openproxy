//! Engine configuration: deserialized `rl_agent_config.json` plus validation.

use openproxy_types::error::CoreError;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct LayaConfig {
    #[serde(default = "default_max_len")]
    pub max_len: usize,
    #[serde(default = "default_head_max_len")]
    pub head_max_len: usize,
    #[serde(default = "default_temperature")]
    pub temperature: Vec<f32>,
    #[serde(default)]
    pub temperature_by_options: std::collections::HashMap<String, f32>,
}

fn default_max_len() -> usize {
    512
}
fn default_head_max_len() -> usize {
    192
}
fn default_temperature() -> Vec<f32> {
    vec![1.0, 1.0, 1.0]
}

impl Default for LayaConfig {
    fn default() -> Self {
        Self {
            max_len: default_max_len(),
            head_max_len: default_head_max_len(),
            temperature: default_temperature(),
            temperature_by_options: std::collections::HashMap::new(),
        }
    }
}

pub(crate) fn validate_config(config: &LayaConfig) -> Result<(), CoreError> {
    if !(32..=8192).contains(&config.max_len)
        || !(16..=config.max_len - 4).contains(&config.head_max_len)
        || config
            .temperature
            .iter()
            .chain(config.temperature_by_options.values())
            .any(|t| !t.is_finite() || *t <= 0.0)
    {
        return Err(CoreError::Validation(
            "Invalid Laya sequence lengths or temperatures".into(),
        ));
    }
    Ok(())
}
