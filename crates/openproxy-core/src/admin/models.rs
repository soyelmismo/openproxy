//! Model administration service layer.

use crate::error::Result;
use crate::ids::{ModelId, ModelRowId, ProviderId};
use crate::models;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// Known models for a provider, optionally filtered. `provider = None` returns
/// every row in the `models` table.
pub fn list_models(conn: &Connection, provider: Option<&ProviderId>) -> Result<Vec<models::Model>> {
    match provider {
        Some(p) => Ok(models::list_all(conn)?
            .into_iter()
            .filter(|m| &m.provider_id == p)
            .collect()),
        None => models::list_all(conn),
    }
}

/// Inputs for [`create_custom_model`], distinct from the adapter-driven
/// [`refresh_models`] path: the operator hand-picks `(provider_id, model_id)`,
/// `display_name`, the upstream's `target_format` wire format, and a
/// `ttl_seconds` cache lifetime (`0` never expires).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateCustomModelInput {
    pub provider_id: String,
    pub model_id: String,
    pub display_name: Option<String>,
    /// `"openai"` or `"anthropic"`. Anything else surfaces as
    /// [`CoreError::Validation`].
    pub target_format: String,
    pub ttl_seconds: i64,
    #[serde(default)]
    pub model_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateModelInput {
    pub display_name: Option<String>,
    pub model_type: Option<String>,
    pub target_format: Option<String>,
    #[serde(default)]
    pub streaming: Option<Option<bool>>,
    #[serde(default)]
    pub capabilities_json: Option<Option<String>>,
}

/// Create a hand-picked model row, returning the id of the new (or upserted) row.
/// See [`models::create_custom`] for the SQL semantics.
pub fn create_custom_model(conn: &Connection, input: CreateCustomModelInput) -> Result<ModelRowId> {
    let provider = ProviderId::new(input.provider_id);
    let model = ModelId::new(input.model_id);
    let target_format = models::TargetFormat::parse(&input.target_format)?;
    models::create_custom(
        conn,
        &provider,
        &model,
        input.display_name.as_deref(),
        target_format,
        input.ttl_seconds,
        input.model_type.as_deref(),
    )
}

/// Update display_name, model_type, target_format and capabilities on an existing model row.
pub fn update_model(conn: &Connection, id: ModelRowId, input: UpdateModelInput) -> Result<()> {
    let target_format = if let Some(tf) = input.target_format.as_deref() {
        Some(models::TargetFormat::parse(tf)?)
    } else {
        None
    };
    models::update_model_details(
        conn,
        id,
        input.display_name.as_deref(),
        input.model_type.as_deref(),
        target_format,
    )?;

    if let Some(opt_caps) = input.capabilities_json {
        models::update_model_capabilities_json(conn, id, opt_caps.as_deref())?;
    } else if let Some(opt_streaming) = input.streaming {
        let existing = models::get_by_row_id(conn, id)?;
        let mut caps = existing
            .as_ref()
            .and_then(|m| m.capabilities())
            .unwrap_or_default();
        caps.streaming = opt_streaming;
        let new_caps_json = if caps.is_empty() {
            None
        } else {
            serde_json::to_string(&caps).ok()
        };
        models::update_model_capabilities_json(conn, id, new_caps_json.as_deref())?;
    }

    Ok(())
}

pub use openproxy_discovery::models::refresh_models;

/// Inputs for [`set_active_bulk`], sent by the "Enable all" / "Disable all" buttons
/// to toggle every non-custom row of the provider in one UPDATE.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulkToggleInput {
    pub provider_id: String,
    pub active: bool,
}

/// Bulk set `active` for all non-custom models of a provider. One
/// `UPDATE ... WHERE provider_id = ? AND custom = 0` flips every row at once, so a
/// concurrent `apply_auto_activation` cannot interleave and leave the table
/// half-toggled.
///
/// Returns the updated row count. A missing provider matches nothing.
pub fn set_active_bulk(conn: &Connection, input: BulkToggleInput) -> Result<u64> {
    let provider = ProviderId::new(input.provider_id);
    models::set_active_bulk(conn, &provider, input.active)
}
