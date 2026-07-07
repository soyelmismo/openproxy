//! CRUD operations for the `models` table.
//!
//! These are the building-block SQL functions consumed by
//! [`super::repository::SqliteModelRepository`] and (for now) by direct
//! `models::` call sites throughout the crate. The functions are
//! `pub(crate)` so internal callers keep compiling while the codebase
//! migrates to the `ModelRepository` trait.

use super::{DiscoveredModel, Model, TargetFormat, UpsertResult};
use crate::error::{CoreError, Result};
use crate::ids::{ModelId, ModelRowId, ProviderId};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::time::Duration;

fn map_row(row: &Row<'_>) -> rusqlite::Result<Model> {
    let target_format_str: String = row.get("target_format")?;
    let target_format = match target_format_str.as_str() {
        "openai" => TargetFormat::Openai,
        "anthropic" => TargetFormat::Anthropic,
        "gemini" => TargetFormat::Gemini,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid target_format in db: {}", other),
                )),
            ));
        }
    };

    // The schema's CHECK constraint guarantees `active` is 0 or 1; map
    // it to a bool at the boundary so the rest of the code stays type
    // safe.
    let active_bit: i64 = row.get("active")?;
    let active = match active_bit {
        0 => false,
        1 => true,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Integer,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid active bit in db: {}", other),
                )),
            ));
        }
    };

    let custom_bit: i64 = row.get("custom")?;
    let custom = match custom_bit {
        0 => false,
        1 => true,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Integer,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid custom bit in db: {}", other),
                )),
            ));
        }
    };

    Ok(Model {
        row_id: ModelRowId(row.get::<_, i64>("id")?),
        provider_id: ProviderId::new(row.get::<_, String>("provider_id")?),
        model_id: ModelId::new(row.get::<_, String>("model_id")?),
        display_name: row.get::<_, Option<String>>("display_name")?,
        target_format,
        discovered_at: row.get::<_, String>("discovered_at")?,
        expires_at: row.get::<_, Option<String>>("expires_at")?,
        timeout_overrides_json: row.get::<_, Option<String>>("timeout_overrides_json")?,
        active,
        last_test_status: row.get::<_, Option<i32>>("last_test_status")?,
        last_test_at: row.get::<_, Option<String>>("last_test_at")?,
        custom,
        context_length: row.get::<_, Option<i64>>("context_length")?,
        max_output_tokens: row.get::<_, Option<i64>>("max_output_tokens")?,
        capabilities_json: row.get::<_, Option<String>>("capabilities_json")?,
        family: row.get::<_, Option<String>>("family")?,
        // model_type is NOT NULL with a DEFAULT of 'chat' in the schema,
        // so the column is guaranteed to come back as a string.
        model_type: row
            .get::<_, Option<String>>("model_type")?
            .unwrap_or_else(|| "chat".to_string()),
        input_modalities_json: row.get::<_, Option<String>>("input_modalities_json")?,
        output_modalities_json: row.get::<_, Option<String>>("output_modalities_json")?,
    })
}

/// Insert or update models reported by a provider's `/models` endpoint,
/// and remove the ones the upstream stopped listing.
///
/// For each entry in `discovered`:
/// - if `(provider_id, model_id)` does not exist, insert a new row with
///   `discovered_at = now` and `expires_at = now + ttl`;
/// - otherwise refresh only the mutable metadata (`display_name`,
///   `target_format`, and the optional OpenRouter-derived columns).
///   `discovered_at` and `expires_at` are **preserved** so that the
///   60-second recency window used by [`apply_auto_activation`] only
///   flags truly *new* rows. If we refreshed `discovered_at` on
///   re-upsert, a hand-disabled model that the provider keeps listing
///   would be considered "new" on every refresh and have its `active`
///   bit clobbered.
///
/// After the upsert phase, the same transaction runs a hard delete:
/// every non-custom row of `provider` whose `model_id` is not in the
/// just-upserted set is removed. The `custom = 0` gate preserves
/// operator-curated rows from accidental purge. An empty `discovered`
/// slice is interpreted as "the upstream lists nothing for this
/// provider" and removes all non-custom rows for the provider.
///
/// Why hard-delete instead of an `expires_at` filter?
///   - the registry reflects upstream truth with no `datetime('now')`
///     math at query time;
///   - the `combo_targets` rows that referenced a vanished model
///     become orphans, but routing already filters on
///     `model_row_id IN (live models)` at request time.
///
/// See the module-level docs for the visibility semantic.
///
/// Insert or update a batch of discovered models for a provider.
///
/// See parent module docs for the full upsert semantics. Returns an
/// [`UpsertResult`] with the touched count and the list of model_ids
/// that were newly inserted (i.e. not present in the table before
/// the call).
pub fn upsert_many(
    conn: &Connection,
    provider: &ProviderId,
    discovered: &[DiscoveredModel],
    ttl: Duration,
) -> Result<UpsertResult> {
    let diff = super::sync::compute_diff(conn, provider, discovered)?;
    let (upsert_result, events) =
        super::sync::execute_sync_transaction(conn, provider, discovered, &diff, ttl)?;
    super::sync::broadcast_notifications(conn, &events);
    Ok(upsert_result)
}

/// List all active (live) models for a given provider.
///
/// A row is considered live when:
/// - `active = 1` (the soft-disable bit set by [`set_active`]).
///
/// Presence in the most recent successful refresh of the provider is
/// enforced at *write* time, not at *read* time: [`upsert_many`] deletes
/// the rows the upstream stopped listing, so by the time this query
/// runs the table is already a faithful mirror of the provider's
/// `/models` response. There is no `expires_at > now()` filter on the
/// hot path — a row with `active = 1` is live, period. (`expires_at`
/// stays in the schema for diagnostic / debug purposes.)
///
/// An admin can flip a model out of routing without deleting the row by
/// calling [`set_active`] with `false`; the row stays in the table for
/// audit / re-enable. Hand-curated `custom = 1` rows are also returned
/// here as long as they're `active = 1`.
pub fn list_active(conn: &Connection, provider: &ProviderId) -> Result<Vec<Model>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, provider_id, model_id, display_name, target_format, \
                    discovered_at, expires_at, timeout_overrides_json, active, \
                    last_test_status, last_test_at, custom, \
                    context_length, max_output_tokens, capabilities_json, \
                    family, model_type, input_modalities_json, \
                    output_modalities_json \
             FROM models \
             WHERE provider_id = ? \
               AND active = 1",
        )
        .map_err(|e| CoreError::Database {
            message: format!("prepare list_active: {}", e),
            source: Some(Box::new(e)),
        })?;

    let rows = stmt
        .query_map([provider.as_str()], map_row)
        .map_err(|e| CoreError::Database {
            message: format!("query list_active: {}", e),
            source: Some(Box::new(e)),
        })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| CoreError::Database {
            message: format!("row list_active: {}", e),
            source: Some(Box::new(e)),
        })?);
    }
    Ok(out)
}

/// List all active (live) models across every provider.
///
/// A row is considered live when `active = 1`. Visibility is enforced
/// at *write* time by [`upsert_many`] (the row is removed when the
/// upstream drops it from `/models`), so the hot path here is a plain
/// `active = 1` filter — no `expires_at > now()` math. See
/// [`list_active`] for the rationale and the module-level doc for the
/// full semantic.
///
/// This is the cross-provider variant of [`list_active`]. The public
/// `GET /v1/models` endpoint consumes it so SDKs/CLIs pointing at
/// openproxy never see rows the admin has disabled (which is the
/// whole point of having a soft-disable bit at all). The single-
/// provider [`list_active`] is still used by the routing pipeline,
/// which only cares about the models it can send to a specific
/// upstream.
pub fn list_active_all(conn: &Connection) -> Result<Vec<Model>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, provider_id, model_id, display_name, target_format, \
                    discovered_at, expires_at, timeout_overrides_json, active, \
                    last_test_status, last_test_at, custom, \
                    context_length, max_output_tokens, capabilities_json, \
                    family, model_type, input_modalities_json, \
                    output_modalities_json \
             FROM models \
             WHERE active = 1",
        )
        .map_err(|e| CoreError::Database {
            message: format!("prepare list_active_all: {}", e),
            source: Some(Box::new(e)),
        })?;

    let rows = stmt
        .query_map([], map_row)
        .map_err(|e| CoreError::Database {
            message: format!("query list_active_all: {}", e),
            source: Some(Box::new(e)),
        })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| CoreError::Database {
            message: format!("row list_active_all: {}", e),
            source: Some(Box::new(e)),
        })?);
    }
    Ok(out)
}

/// List every row in `models`. Used by the `/admin/models` admin
/// endpoint which surfaces both live and recently-expired entries (the
/// `active` field tells the UI which ones are currently routable).
pub fn list_all(conn: &Connection) -> Result<Vec<Model>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, provider_id, model_id, display_name, target_format, \
                    discovered_at, expires_at, timeout_overrides_json, active, \
                    last_test_status, last_test_at, custom, \
                    context_length, max_output_tokens, capabilities_json, \
                    family, model_type, input_modalities_json, \
                    output_modalities_json \
             FROM models",
        )
        .map_err(|e| CoreError::Database {
            message: format!("prepare list_all: {}", e),
            source: Some(Box::new(e)),
        })?;

    let rows = stmt
        .query_map([], map_row)
        .map_err(|e| CoreError::Database {
            message: format!("query list_all: {}", e),
            source: Some(Box::new(e)),
        })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| CoreError::Database {
            message: format!("row list_all: {}", e),
            source: Some(Box::new(e)),
        })?);
    }
    Ok(out)
}

/// Delete orphaned model rows that haven't been refreshed in more than
/// 7 days. Returns the number of rows removed.
///
/// This is a **manual cleanup utility** for orphan rows, NOT part of
/// the normal hot path. Normal model lifecycle is handled by
/// [`upsert_many`], which removes rows the upstream stopped listing
/// on the next refresh. `mark_expired` exists to clean up edge cases
/// such as:
///   - the provider was deleted while model rows were still in place
///     (the `combo_targets` FK to `models` would otherwise dangle);
///   - a process crashed mid-upsert and left rows in an inconsistent
///     state;
///   - a hand-curated `custom = 1` row whose upstream still lists the
///     model but the upstream was removed from the registry.
///
/// The 7-day threshold is intentionally long: it must be longer than
/// any plausible refresh interval so we never delete a row the
/// background scheduler (Gate A) is about to refresh anyway. Rows
/// that are still in the table this long are orphans by definition.
///
/// Rows with `expires_at IS NULL` are never deleted — `expires_at`
/// is no longer the visibility gate, so a NULL there is a legitimate
/// "no expiry set" state (e.g. `create_custom` with `ttl_seconds = 0`)
/// and is not, by itself, evidence of an orphan.
pub fn mark_expired(conn: &Connection) -> Result<usize> {
    let n = conn
        .execute(
            "DELETE FROM models \
             WHERE expires_at IS NOT NULL \
               AND expires_at < datetime('now', '-7 days')",
            [],
        )
        .map_err(|e| CoreError::Database {
            message: format!("execute mark_expired: {}", e),
            source: Some(Box::new(e)),
        })?;
    Ok(n)
}

/// Set the soft-disable flag on a model. Returns `Ok(())` whether or not a
/// row was affected — toggling a non-existent id is a no-op, mirroring the
/// idempotent style of the other admin mutations.
///
/// `active = true` makes the row visible to [`list_active`]; `false` hides
/// it from routing. The row is preserved either way, so a future
/// re-enable doesn't lose any data.
pub fn set_active(conn: &Connection, id: ModelRowId, active: bool) -> Result<()> {
    let bit = if active { 1i64 } else { 0i64 };
    conn.execute(
        "UPDATE models SET active = ?1 WHERE id = ?2",
        params![bit, id.0],
    )
    .map_err(|e| CoreError::Database {
        message: format!("update active for model {}: {}", id.0, e),
        source: Some(Box::new(e)),
    })?;
    Ok(())
}

/// Bulk set active for all non-custom models of a provider.
/// Returns the number of rows updated.
///
/// Honors `custom = 1` rows (does NOT touch them), matching the
/// behavior of `apply_auto_activation`.
pub fn set_active_bulk(conn: &Connection, provider: &ProviderId, active: bool) -> Result<u64> {
    let bit = if active { 1i64 } else { 0i64 };
    let n = conn
        .execute(
            "UPDATE models SET active = ?1 WHERE provider_id = ?2 AND custom = 0",
            params![bit, provider.as_str()],
        )
        .map_err(|e| CoreError::Database {
            message: format!("set_active_bulk for {}: {}", provider, e),
            source: Some(Box::new(e)),
        })?;
    Ok(n as u64)
}

/// Fetch a single model by its primary key. Returns `None` if not found.
pub fn get_by_row_id(conn: &Connection, row_id: ModelRowId) -> Result<Option<Model>> {
    let res = conn
        .query_row(
            "SELECT id, provider_id, model_id, display_name, target_format, \
                    discovered_at, expires_at, timeout_overrides_json, active, \
                    last_test_status, last_test_at, custom, \
                    context_length, max_output_tokens, capabilities_json, \
                    family, model_type, input_modalities_json, \
                    output_modalities_json \
             FROM models WHERE id = ?",
            [row_id.0],
            map_row,
        )
        .optional()
        .map_err(|e| CoreError::Database {
            message: format!("query get_by_row_id: {}", e),
            source: Some(Box::new(e)),
        })?;
    Ok(res)
}

/// Fetch multiple models by their primary keys.
pub fn get_by_row_ids(conn: &Connection, row_ids: &[ModelRowId]) -> Result<Vec<Model>> {
    if row_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = std::iter::repeat_n("?", row_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let query = format!(
        "SELECT id, provider_id, model_id, display_name, target_format, \
                discovered_at, expires_at, timeout_overrides_json, active, \
                last_test_status, last_test_at, custom, \
                context_length, max_output_tokens, capabilities_json, \
                family, model_type, input_modalities_json, \
                output_modalities_json \
         FROM models WHERE id IN ({})",
        placeholders
    );
    let mut stmt = conn
        .prepare_cached(&query)
        .map_err(|e| CoreError::Database {
            message: format!("prepare get_by_row_ids: {}", e),
            source: Some(Box::new(e)),
        })?;
    let ids: Vec<&dyn rusqlite::ToSql> = row_ids
        .iter()
        .map(|id| &id.0 as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt
        .query_map(&*ids, map_row)
        .map_err(|e| CoreError::Database {
            message: format!("query get_by_row_ids: {}", e),
            source: Some(Box::new(e)),
        })?;
    let mut models = Vec::with_capacity(row_ids.len());
    for row in rows {
        models.push(row.map_err(|e| CoreError::Database {
            message: format!("map row get_by_row_ids: {}", e),
            source: Some(Box::new(e)),
        })?);
    }
    Ok(models)
}

/// Find an active model by its (exact, case-sensitive) `model_id`.
///
/// "Active" means `active = 1`. The match is exact because upstream
/// model ids are looked up verbatim by adapters and downstream tools;
/// a fuzzy/prefix match here would silently alias models that share a
/// namespace and lead to surprising routing decisions. Rows the
/// upstream dropped are removed by [`upsert_many`] at refresh time,
/// so the table itself is the source of truth — there is no
/// `expires_at` filter on this query.
///
/// The function returns at most one row. If two providers happened to
/// both register the same `model_id` (the schema's `UNIQUE` constraint
/// is per-`(provider_id, model_id)`, so this is allowed), the
/// tie-breaker is `id ASC` — a deterministic but otherwise arbitrary
/// choice. The caller is expected to treat the result as
/// "one candidate" and decide via the routing layer which provider to
/// use.
///
/// Used by the model-first routing layer in
/// [`crate::routing::resolve`]: a chat request whose `model` field
/// matches a row in the `models` table is dispatched to that provider.
pub fn find_active_by_name(conn: &Connection, model_id: &str) -> Result<Option<Model>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, provider_id, model_id, display_name, target_format, \
                    discovered_at, expires_at, timeout_overrides_json, active, \
                    last_test_status, last_test_at, custom, \
                    context_length, max_output_tokens, capabilities_json, \
                    family, model_type, input_modalities_json, \
                    output_modalities_json \
             FROM models \
             WHERE model_id = ?1 \
               AND active = 1 \
             ORDER BY id ASC \
             LIMIT 1",
        )
        .map_err(|e| CoreError::Database {
            message: format!("prepare find_active_by_name: {}", e),
            source: Some(Box::new(e)),
        })?;
    let mut rows = stmt
        .query_map([model_id], map_row)
        .map_err(|e| CoreError::Database {
            message: format!("query find_active_by_name: {}", e),
            source: Some(Box::new(e)),
        })?;
    match rows.next() {
        Some(row) => Ok(Some(row.map_err(|e| CoreError::Database {
            message: format!("read find_active_by_name row: {}", e),
            source: Some(Box::new(e)),
        })?)),
        None => Ok(None),
    }
}

/// Find a single active model by provider id and model name.
///
/// Like [`find_active_by_name`] but scoped to a specific provider.
/// Used by the routing layer when the client sends a
/// `<provider>/<model_id>` prefix — we look for the model under that
/// exact provider rather than picking whichever was seeded first.
pub fn find_active_by_provider_and_name(
    conn: &Connection,
    provider_id: &ProviderId,
    model_id: &str,
) -> Result<Option<Model>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, provider_id, model_id, display_name, target_format, \
                    discovered_at, expires_at, timeout_overrides_json, active, \
                    last_test_status, last_test_at, custom, \
                    context_length, max_output_tokens, capabilities_json, \
                    family, model_type, input_modalities_json, \
                    output_modalities_json \
             FROM models \
             WHERE provider_id = ?1 \
               AND model_id = ?2 \
               AND active = 1 \
             ORDER BY id ASC \
             LIMIT 1",
        )
        .map_err(|e| CoreError::Database {
            message: format!("prepare find_active_by_provider_and_name: {}", e),
            source: Some(Box::new(e)),
        })?;
    let mut rows = stmt
        .query_map(rusqlite::params![provider_id.as_str(), model_id], map_row)
        .map_err(|e| CoreError::Database {
            message: format!("query find_active_by_provider_and_name: {}", e),
            source: Some(Box::new(e)),
        })?;
    match rows.next() {
        Some(row) => Ok(Some(row.map_err(|e| CoreError::Database {
            message: format!("read find_active_by_provider_and_name row: {}", e),
            source: Some(Box::new(e)),
        })?)),
        None => Ok(None),
    }
}

/// Stamp the test-status columns on a model row. `status` is the raw
/// HTTP status code from the most recent `POST /admin/models/:id/test`
/// call; pass `0` to mean "request never reached the upstream"
/// (DNS / connect / TLS errors are folded into this single value).
///
/// The companion `last_test_at` column is set to `datetime('now')` in
/// the same statement so the two columns always move together.
pub fn set_test_status(conn: &Connection, id: ModelRowId, status: i32) -> Result<()> {
    conn.execute(
        "UPDATE models \
         SET last_test_status = ?1, last_test_at = datetime('now') \
         WHERE id = ?2",
        params![status, id.0],
    )
    .map_err(|e| CoreError::Database {
        message: format!("update test status for model {}: {}", id.0, e),
        source: Some(Box::new(e)),
    })?;
    Ok(())
}

/// Hard-delete a model row. The `combo_targets.model_row_id` FK is
/// declared with `ON DELETE CASCADE` (see migration 000030), so the
/// `DELETE FROM models` below automatically cascade-deletes any
/// `combo_targets` row that referenced this model.
///
/// A missing id is a silent no-op (0 rows affected), matching the
/// idempotent style of the other admin deletions.
///
/// Returns the number of rows removed from `models`.
pub fn delete(conn: &Connection, id: ModelRowId) -> Result<u64> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| CoreError::Database {
            message: format!("begin delete model tx: {}", e),
            source: Some(Box::new(e)),
        })?;

    // combo_targets.model_row_id has ON DELETE CASCADE (migration 000030);
    // the target row is cascade-deleted alongside the model.
    // No pre-emptive cleanup needed.
    let removed = tx
        .execute("DELETE FROM models WHERE id = ?1", params![id.0])
        .map_err(|e| CoreError::Database {
            message: format!("delete model {}: {}", id.0, e),
            source: Some(Box::new(e)),
        })?;

    tx.commit().map_err(|e| CoreError::Database {
        message: format!("commit delete model tx: {}", e),
        source: Some(Box::new(e)),
    })?;

    Ok(removed as u64)
}

/// Insert a hand-crafted model row (or refresh an existing one).
///
/// Distinct from the adapter-driven [`upsert_many`] path: this sets
/// `custom = 1` and `active = 1` unconditionally so the operator's
/// intent ("this is a real model I want to call") is preserved across
/// refreshes that may not list the same id from the upstream.
///
/// `ttl_seconds` is the cache lifetime of the row. `0` means "never
/// expire" (sets `expires_at = NULL`); any other value is interpreted
/// as a delta from `datetime('now')`.
///
/// On conflict on `(provider_id, model_id)` the existing row is
/// preserved (its `row_id` is returned) and only the mutable fields
/// are refreshed; `custom` is forced back to `1` to defend against a
/// previously-discovered row being later "promoted" to custom without
/// the operator's consent — they asked for a custom row, so it stays
/// custom.
pub fn create_custom(
    conn: &Connection,
    provider_id: &ProviderId,
    model_id: &ModelId,
    display_name: Option<&str>,
    target_format: TargetFormat,
    ttl_seconds: i64,
) -> Result<ModelRowId> {
    // The `expires_at` value depends on `ttl_seconds`. We compose it
    // as a tiny SQL fragment because the same expression needs to
    // appear in both the INSERT VALUES list (a raw expression) and
    // the ON CONFLICT DO UPDATE clause (an `= <expression>` pair).
    let expires_expr = if ttl_seconds <= 0 {
        "NULL".to_string()
    } else {
        format!("datetime('now', '+' || {} || ' seconds')", ttl_seconds)
    };

    // The `RETURNING id` clause gives us the rowid regardless of
    // whether the row was inserted or updated, so the caller can
    // chain a `set_test_status` or future operation off of it.
    let normalized = crate::model_normalize::normalize_model_id(model_id.as_str());
    let sql = format!(
        "INSERT INTO models \
            (provider_id, model_id, display_name, target_format, \
             discovered_at, expires_at, active, custom, model_id_normalized) \
         VALUES (?1, ?2, ?3, ?4, datetime('now'), {expires_expr}, 1, 1, ?5) \
         ON CONFLICT(provider_id, model_id) DO UPDATE SET \
            display_name = excluded.display_name, \
            target_format = excluded.target_format, \
            discovered_at = datetime('now'), \
            expires_at = {expires_expr}, \
            active = 1, \
            custom = 1, \
            model_id_normalized = COALESCE(excluded.model_id_normalized, model_id_normalized) \
         RETURNING id",
    );

    let row_id: i64 = conn
        .query_row(
            &sql,
            params![
                provider_id.as_str(),
                model_id.as_str(),
                display_name,
                target_format.as_str(),
                &normalized,
            ],
            |r| r.get(0),
        )
        .map_err(|e| {
            // FK violation → unknown provider. Same idiom as
            // `accounts::create` keeps error messages consistent.
            let msg = e.to_string();
            if msg.contains("FOREIGN KEY") {
                CoreError::Validation(format!("provider_id does not exist: {}", provider_id))
            } else {
                CoreError::Database {
                    message: format!("create_custom model for {}: {}", provider_id, e),
                    source: Some(Box::new(e)),
                }
            }
        })?;

    Ok(ModelRowId(row_id))
}

/// Recompute `active` for **newly discovered** non-custom models of a
/// provider, based on whether the model's `model_id` contains `keyword`.
///
/// "Newly discovered" means `discovered_at >= datetime('now', '-60 seconds')`
/// — i.e. rows that were **inserted** (not just refreshed) in the
/// current discovery cycle. Pre-existing rows whose `active` bit the
/// operator toggled by hand are preserved across refreshes because
/// [`upsert_many`] does not touch `discovered_at` on re-upsert.
///
/// - `Some(k)` — `active = 1` if `model_id LIKE '%k%'`, `0` otherwise.
/// - `None`    — `active = 1` for every newly discovered non-custom row.
///
/// Custom rows (`custom = 1`) are skipped entirely so the operator's
/// hand-picked entries survive a refresh. Returns the number of rows
/// whose `active` bit was changed.
pub fn apply_auto_activation(
    conn: &Connection,
    provider: &ProviderId,
    keyword: Option<&str>,
) -> Result<u64> {
    // Wrap SELECT + UPDATE + (best-effort) notification insert in a tx
    // so a failure in any step rolls back the auto-activation. Without
    // the tx, a notification insert error mid-way would leave the
    // `active` bit half-flipped for the rows we'd already UPDATEd.
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| CoreError::Database {
            message: format!("begin apply_auto_activation tx: {}", e),
            source: Some(Box::new(e)),
        })?;

    // ----------------------------------------------------------------
    // Step 1: identify rows that will be flipped from `active = 0` to
    // `active = 1`. We need to SELECT them BEFORE the UPDATE because
    // the UPDATE itself doesn't return which rows matched.
    //
    // The 60-second window is the key bit. The refresh flow is:
    //   1. `upsert_many` inserts new rows with `discovered_at = now`
    //      and preserves `discovered_at` on already-present rows.
    //   2. `apply_auto_activation` runs immediately after, in the same
    //      request handler.
    // So any row whose `discovered_at` is within the last 60s was a
    // *new insert* by this refresh. Rows older than that were already
    // present before the refresh and must keep their `active` bit.
    //
    // We additionally require `active = 0` so we only capture rows
    // that are actually about to be flipped to 1 — rows already at 1
    // are no-ops and don't warrant a notification.
    let newly_active: Vec<(String, Option<String>)> = match keyword {
        Some(k) => {
            let mut stmt = tx
                .prepare(
                    "SELECT model_id, display_name FROM models \
                     WHERE provider_id = ?1 AND custom = 0 \
                       AND discovered_at >= datetime('now', '-60 seconds') \
                       AND active = 0 \
                       AND model_id LIKE '%' || ?2 || '%'",
                )
                .map_err(|e| CoreError::Database {
                    message: format!("apply_auto_activation prepare select-keyword: {}", e),
                    source: Some(Box::new(e)),
                })?;
            let rows = stmt
                .query_map(params![provider.as_str(), k], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                })
                .map_err(|e| CoreError::Database {
                    message: format!("apply_auto_activation query select-keyword: {}", e),
                    source: Some(Box::new(e)),
                })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| CoreError::Database {
                    message: format!("apply_auto_activation row select-keyword: {}", e),
                    source: Some(Box::new(e)),
                })?);
            }
            out
        }
        None => {
            let mut stmt = tx
                .prepare(
                    "SELECT model_id, display_name FROM models \
                     WHERE provider_id = ?1 AND custom = 0 \
                       AND discovered_at >= datetime('now', '-60 seconds') \
                       AND active = 0",
                )
                .map_err(|e| CoreError::Database {
                    message: format!("apply_auto_activation prepare select-all: {}", e),
                    source: Some(Box::new(e)),
                })?;
            let rows = stmt
                .query_map(params![provider.as_str()], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                })
                .map_err(|e| CoreError::Database {
                    message: format!("apply_auto_activation query select-all: {}", e),
                    source: Some(Box::new(e)),
                })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| CoreError::Database {
                    message: format!("apply_auto_activation row select-all: {}", e),
                    source: Some(Box::new(e)),
                })?);
            }
            out
        }
    };

    // ----------------------------------------------------------------
    // Step 2: run the original UPDATE. The CASE/WHEN for the keyword
    // path will also flip active=0 for non-matching newly-discovered
    // rows, which is the intended semantic (a keyword match "claims"
    // the model; everything else the upstream listed is de-activated
    // so it doesn't appear in /v1/models until the operator
    // explicitly enables it).
    let updated = match keyword {
        Some(k) => tx.execute(
            "UPDATE models \
             SET active = CASE WHEN model_id LIKE '%' || ?1 || '%' THEN 1 ELSE 0 END \
             WHERE provider_id = ?2 \
               AND custom = 0 \
               AND discovered_at >= datetime('now', '-60 seconds')",
            params![k, provider.as_str()],
        ),
        None => tx.execute(
            "UPDATE models SET active = 1 \
             WHERE provider_id = ?1 \
               AND custom = 0 \
               AND discovered_at >= datetime('now', '-60 seconds')",
            params![provider.as_str()],
        ),
    }
    .map_err(|e| CoreError::Database {
        message: format!("apply_auto_activation for {}: {}", provider, e),
        source: Some(Box::new(e)),
    })?;

    // ----------------------------------------------------------------
    // Step 3: insert `model_auto_activated` notifications for the rows
    // identified in step 1. The dedup key includes `:auto` so it
    // doesn't collide with the `model_new` dedup space (same model
    // can produce both a `model_new` and a `model_auto_activated` in
    // the same discovery cycle, and we want both rows in the tray).
    //
    // Failures here are swallowed with `.ok().flatten()` — a
    // notification insert error must NOT roll back the
    // auto-activation, which would leave the operator's keyword rule
    // silently un-applied.
    let mut to_broadcast: Vec<(i64, &'static str, serde_json::Value)> = Vec::new();
    for (model_id, display_name) in &newly_active {
        let payload = serde_json::json!({
            "provider_id": provider.as_str(),
            "model_id": model_id,
            "display_name": display_name,
            "matched_keyword": keyword,
        });
        let dedup = format!("{}:{}:auto", provider.as_str(), model_id);
        if let Some(id) = crate::notifications::insert(
            &tx,
            crate::notifications::KIND_MODEL_AUTO_ACTIVATED,
            &payload,
            Some(&dedup),
            Some(provider.as_str()),
        )
        .ok()
        .flatten()
        {
            to_broadcast.push((id, crate::notifications::KIND_MODEL_AUTO_ACTIVATED, payload));
        }
    }

    tx.commit().map_err(|e| CoreError::Database {
        message: format!("commit apply_auto_activation: {}", e),
        source: Some(Box::new(e)),
    })?;

    // After commit: broadcast each newly-inserted notification to any
    // connected WS clients. Failures here are swallowed — broadcast
    // has no subscribers during cold-start / unit tests, which is the
    // normal case.
    for (id, kind, payload) in &to_broadcast {
        let _ = crate::notifications::broadcast_one(conn, *id, kind, payload);
    }

    Ok(updated as u64)
}
