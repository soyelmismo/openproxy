//! Database access layer and repository for the `models` table.

pub mod activation;
pub mod crud;
pub mod repository;
pub mod upsert;

#[cfg(test)]
mod tests;

pub use activation::*;
pub use crud::*;
pub use repository::*;
pub use upsert::*;

use openproxy_types::{Model, ModelId, ModelRowId, ProviderId, TargetFormat};
use rusqlite::Row;

pub(crate) fn map_row(row: &Row<'_>) -> rusqlite::Result<Model> {
    crate::map_row_struct!(row, Model {
        row_id: @id(0, ModelRowId),
        provider_id: @id_str(1, ProviderId),
        model_id: @id_str(2, ModelId),
        display_name: @opt_box_str(3),
        target_format: @enum_parse(4, TargetFormat),
        discovered_at: @box_str(5),
        expires_at: @opt_box_str(6),
        timeout_overrides_json: @opt_box_str(7),
        active: @bool(8),
        last_test_status: 9,
        last_test_at: @opt_box_str(10),
        custom: @bool(11),
        context_length: 12,
        max_output_tokens: 13,
        capabilities_json: @opt_box_str(14),
        family: @opt_box_str(15),
        model_type: @box_str_default(16, "chat"),
        input_modalities_json: @opt_box_str(17),
        output_modalities_json: @opt_box_str(18),
        manually_disabled_at: @opt_box_str(19),
    })
}

crate::def_table_select!(
    model_select,
    "models",
    "id, provider_id, model_id, display_name, target_format, \
     discovered_at, expires_at, timeout_overrides_json, active, \
     last_test_status, last_test_at, custom, \
     context_length, max_output_tokens, capabilities_json, \
     family, model_type, input_modalities_json, \
     output_modalities_json, \
     manually_disabled_at"
);
pub(crate) use model_select;

crate::def_table_select!(model_auto_active_select, "models", "model_id, display_name");
pub(crate) use model_auto_active_select;

crate::def_table_select!(
    model_existing_select,
    "models",
    "model_id, id, display_name"
);
pub(crate) use model_existing_select;

crate::def_table_select!(model_inserted_select, "models", "id, model_id");
pub(crate) use model_inserted_select;

impl crate::crud::FromRow for Model {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        map_row(row)
    }
}
