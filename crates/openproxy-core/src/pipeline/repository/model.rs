use std::collections::HashMap;
use rusqlite::Connection;
use crate::error::Result;
use crate::models::Model;

pub fn get_models_by_row_ids(
    conn: &Connection,
    model_row_ids: &[crate::ids::ModelRowId],
) -> Result<HashMap<i64, Model>> {
    let mut models_map = HashMap::new();
    if model_row_ids.is_empty() {
        return Ok(models_map);
    }
    
    if let Ok(models) = crate::models::crud::get_by_row_ids(conn, model_row_ids) {
        for m in models {
            models_map.insert(m.row_id.0, m);
        }
    }
    
    Ok(models_map)
}
