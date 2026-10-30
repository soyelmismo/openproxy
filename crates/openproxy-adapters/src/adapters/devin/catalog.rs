//! Live Devin model catalog decoded from `GetUserStatus`.
//!
//! `user_status.cascade_model_config_data` (field 33) carries one
//! `ClientModelConfig` row per model variant the account can serve — roughly
//! 250 rows across ~43 base models. Each row declares the canonical wire
//! `model_uid`, context and completion ceilings, image support, the base
//! model alias it belongs to and the reasoning-effort option it represents.
//!
//! The rows are parsed inside `quota::parse_user_status` and published here
//! as an immutable snapshot; catalog lookups (`models.rs`) read the snapshot
//! and fall back to the static table when no live data has arrived yet.
//!
//! The snapshot is account-scoped: it describes the account behind the most
//! recent `GetUserStatus`. That is still correct for routing — the upstream
//! re-validates every `chat_model_uid` against the caller's token — but
//! limits and flags always reflect the freshest account queried, not a union
//! of all accounts.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use super::protoc::{self, ProtoReader};

/// One `ClientModelConfig` row from `cascade_model_config_data`.
#[derive(Debug, Clone, Default)]
pub struct DevinLiveModelRow {
    /// Wire `chat_model_uid` for this variant (`swe-2-high`,
    /// `claude-opus-4-6-thinking-1m`, `gpt-5-6-sol-low-priority`, …).
    pub uid: String,
    /// Base alias from `model_info` (`swe-2`, `claude-opus-4.6`, `glm-5.2`).
    pub base: String,
    /// Base display name shown in Devin's model picker (`SWE-2`).
    pub display_name: String,
    /// Advertised context window for this variant.
    pub context_length: i64,
    /// `model_info.max_output_tokens` — the real completion ceiling, which
    /// varies per model (swe-2: 128k, nemotron-3-ultra: 32k,
    /// deepseek-v4-flash: 384k) unlike upstream's flat 64k catalog cap.
    pub max_output_tokens: i64,
    /// `supports_images` — absent on the wire for text-only models.
    pub supports_images: bool,
    /// Selected `Reasoning Effort`/`Effort` dimension option, lowercased
    /// (`high`, `max`, `none`, …). Empty when the variant carries no effort
    /// dimension (bare-uid models such as `kimi-k2-6`).
    pub effort: String,
}

/// Normalize a catalog base or request model id to the dashed, lowercase
/// spelling openproxy uses (`claude-opus-4.6` → `claude-opus-4-6`).
pub fn normalize_base_id(raw: &str) -> String {
    raw.trim().to_ascii_lowercase().replace('.', "-")
}

/// Immutable snapshot of the live catalog: rows grouped by normalized base
/// plus a uid index for exact-match resolution.
#[derive(Debug, Default)]
pub struct DevinLiveCatalog {
    by_base: HashMap<String, Vec<DevinLiveModelRow>>,
    by_uid: HashMap<String, DevinLiveModelRow>,
    uids: HashSet<String>,
}

impl DevinLiveCatalog {
    fn build(rows: Vec<DevinLiveModelRow>) -> Self {
        let mut by_base: HashMap<String, Vec<DevinLiveModelRow>> = HashMap::new();
        let mut by_uid = HashMap::with_capacity(rows.len());
        let mut uids = HashSet::with_capacity(rows.len());
        for row in rows {
            uids.insert(row.uid.clone());
            by_uid.insert(row.uid.clone(), row.clone());
            by_base
                .entry(normalize_base_id(&row.base))
                .or_default()
                .push(row);
        }
        Self {
            by_base,
            by_uid,
            uids,
        }
    }

    /// Whether `uid` is an exact live wire model id.
    pub fn is_live_uid(&self, uid: &str) -> bool {
        self.uids.contains(uid)
    }

    /// Rows for a base id or alias (normalized before lookup).
    pub fn rows_for_base(&self, base: &str) -> Option<&[DevinLiveModelRow]> {
        self.by_base
            .get(&normalize_base_id(base))
            .map(Vec::as_slice)
    }

    /// Row for an exact uid, when live.
    pub fn row_for_uid(&self, uid: &str) -> Option<&DevinLiveModelRow> {
        self.by_uid.get(uid)
    }

    /// Base ids present in the live catalog (normalized spelling).
    pub fn base_ids(&self) -> impl Iterator<Item = &String> {
        self.by_base.keys()
    }

    /// Whether any row of `base` accepts inline images.
    pub fn base_supports_images(&self, base: &str) -> Option<bool> {
        self.rows_for_base(base)
            .map(|rows| rows.iter().any(|r| r.supports_images))
    }

    /// Display name of the base model (from the first row carrying one).
    pub fn base_display_name(&self, base: &str) -> Option<String> {
        self.rows_for_base(base)?
            .iter()
            .find(|r| !r.display_name.is_empty())
            .map(|r| r.display_name.clone())
    }

    /// Context/completion ceilings of the base's default variant — the row
    /// without variant markers (`-1m`, `-lightning`, `-fast`, `-priority`,
    /// `-thinking`), falling back to the first row.
    pub fn base_limits(&self, base: &str) -> Option<(i64, i64)> {
        let rows = self.rows_for_base(base)?;
        let row = rows
            .iter()
            .find(|r| !is_variant_uid(&normalize_base_id(&r.base), &r.uid))
            .unwrap_or(&rows[0]);
        Some((row.context_length, row.max_output_tokens))
    }

    /// Ordered distinct effort labels a base accepts (`none`, `minimal`,
    /// `low`, … in rank order, unranked extras last).
    pub fn base_effort_levels(&self, base: &str) -> Vec<String> {
        let Some(rows) = self.rows_for_base(base) else {
            return Vec::new();
        };
        let mut seen: Vec<String> = Vec::new();
        for row in rows {
            if !row.effort.is_empty() && !seen.iter().any(|e| e == &row.effort) {
                seen.push(row.effort.clone());
            }
        }
        seen
    }

    /// Pick the canonical uid for `(base, effort)` from live rows.
    ///
    /// Exact label match wins, preferring the plain `{base}-{effort}` spelling
    /// over variant rows (`-priority`, `-fast`, `-lightning`, `-1m`,
    /// `-thinking`). When `effort` matches nothing, the caller's `default`
    /// label is tried; bases with a single row or unlabeled rows return their
    /// bare uid.
    pub fn pick_uid(&self, base: &str, effort: &str, default: &str) -> Option<String> {
        let rows = self.rows_for_base(base)?;
        if rows.len() == 1 {
            return Some(rows[0].uid.clone());
        }
        let base_id = normalize_base_id(base);
        for wanted in [effort, default] {
            if wanted.is_empty() {
                continue;
            }
            if let Some(uid) = pick_labeled_row(rows, &base_id, wanted) {
                return Some(uid);
            }
        }
        // No effort requested and none matched: prefer the bare row, then any
        // non-variant row.
        rows.iter()
            .find(|r| r.effort.is_empty())
            .or_else(|| rows.iter().find(|r| !is_variant_uid(&base_id, &r.uid)))
            .or(rows.first())
            .map(|r| r.uid.clone())
    }
}

/// Variant suffixes that make a live uid non-canonical for its base.
const VARIANT_MARKERS: &[&str] = &["-lightning", "-priority", "-fast", "-1m", "-thinking"];

fn is_variant_uid(_base_id: &str, uid: &str) -> bool {
    VARIANT_MARKERS.iter().any(|m| uid.ends_with(m))
}

/// Among rows whose effort label equals `label`, prefer the plain
/// `{base}-{label}` uid, then non-variant uids, then the first match.
fn pick_labeled_row(rows: &[DevinLiveModelRow], base_id: &str, label: &str) -> Option<String> {
    let mut candidates = rows.iter().filter(|r| r.effort == label).peekable();
    candidates.peek()?;
    let canonical = format!("{base_id}-{label}");
    if let Some(row) = candidates.clone().find(|r| r.uid == canonical) {
        return Some(row.uid.clone());
    }
    candidates
        .clone()
        .find(|r| !is_variant_uid(base_id, &r.uid))
        .or_else(|| candidates.find(|_| true))
        .map(|r| r.uid.clone())
}

// ---------------------------------------------------------------------------
// Wire parsing — ClientModelConfig inside cascade_model_config_data
// ---------------------------------------------------------------------------

fn reader_str(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Effort label selected on a display dimension (`f30.2`): keeps only
/// dimensions whose name mentions "Effort" (`Reasoning Effort`, `Effort`);
/// `Fast Mode`, `Thinking`, `1M Context`, `Speed` and cache-retention
/// selectors are orthogonal to reasoning effort.
fn dim_effort_label(dim: &[u8]) -> Option<String> {
    let mut label = String::new();
    let mut option_label = String::new();
    let mut reader = ProtoReader::new(dim);
    while let Some((field, wire)) = reader.tag() {
        match (field, wire) {
            (1, protoc::WIRE_LEN) => label = reader_str(reader.bytes_value()?),
            (2, protoc::WIRE_LEN) => {
                option_label = read_option_label(reader.bytes_value()?).unwrap_or_default();
            }
            _ => reader.skip(wire)?,
        }
    }
    label
        .contains("Effort")
        .then(|| option_label.to_ascii_lowercase())
        .filter(|s| !s.is_empty())
}

/// Dimension option (`f30.2.2`): `{1: index, 2: label, 3: selected}`.
fn read_option_label(option: &[u8]) -> Option<String> {
    let mut label = String::new();
    let mut reader = ProtoReader::new(option);
    while let Some((field, wire)) = reader.tag() {
        match (field, wire) {
            (2, protoc::WIRE_LEN) => label = reader_str(reader.bytes_value()?),
            _ => reader.skip(wire)?,
        }
    }
    Some(label)
}

/// Display block (`f30`): `{1: base display name, 2: dim×}` — returns the
/// base display name and the selected effort option, if any.
fn parse_display(display: &[u8], row: &mut DevinLiveModelRow) -> Option<()> {
    let mut reader = ProtoReader::new(display);
    while let Some((field, wire)) = reader.tag() {
        match (field, wire) {
            (1, protoc::WIRE_LEN) => row.display_name = reader_str(reader.bytes_value()?),
            (2, protoc::WIRE_LEN) => {
                if let Some(effort) = dim_effort_label(reader.bytes_value()?) {
                    row.effort = effort;
                }
            }
            _ => reader.skip(wire)?,
        }
    }
    Some(())
}

/// `model_info` (`f23`): `{4: ctx, 13: max_output_tokens, 23: base alias}`.
fn parse_model_info(info: &[u8], row: &mut DevinLiveModelRow) -> Option<()> {
    let mut reader = ProtoReader::new(info);
    while let Some((field, wire)) = reader.tag() {
        match (field, wire) {
            (4, protoc::WIRE_VARINT) => {
                if row.context_length == 0 {
                    row.context_length = reader.varint()? as i64;
                } else {
                    reader.varint()?;
                }
            }
            (13, protoc::WIRE_VARINT) => row.max_output_tokens = reader.varint()? as i64,
            (23, protoc::WIRE_LEN) => row.base = reader_str(reader.bytes_value()?),
            _ => reader.skip(wire)?,
        }
    }
    Some(())
}

/// One `ClientModelConfig` row (`client_model_configs` repeated field 1).
fn parse_client_model_config(config: &[u8], row: &mut DevinLiveModelRow) -> Option<()> {
    let mut reader = ProtoReader::new(config);
    while let Some((field, wire)) = reader.tag() {
        match (field, wire) {
            // f5: supports_images — presence marks the variant image-capable.
            (5, protoc::WIRE_VARINT) => row.supports_images = reader.varint()? != 0,
            (18, protoc::WIRE_VARINT) => row.context_length = reader.varint()? as i64,
            (22, protoc::WIRE_LEN) => row.uid = reader_str(reader.bytes_value()?),
            (23, protoc::WIRE_LEN) => parse_model_info(reader.bytes_value()?, row)?,
            (30, protoc::WIRE_LEN) => parse_display(reader.bytes_value()?, row)?,
            _ => reader.skip(wire)?,
        }
    }
    Some(())
}

/// Parse `cascade_model_config_data` (the bytes of user_status field 33)
/// into one row per `ClientModelConfig`. Rows missing a uid or a base alias
/// are dropped — the catalog is advisory, never a hard-fail.
pub fn parse_client_model_configs(data: &[u8]) -> Vec<DevinLiveModelRow> {
    let mut rows = Vec::new();
    let mut reader = ProtoReader::new(data);
    while let Some((field, wire)) = reader.tag() {
        if field != 1 {
            if reader.skip(wire).is_none() {
                break;
            }
            continue;
        }
        let Some(config) = reader.bytes_value() else {
            break;
        };
        let mut row = DevinLiveModelRow::default();
        if parse_client_model_config(config, &mut row).is_none() {
            continue;
        }
        if !row.uid.is_empty() && !row.base.is_empty() {
            rows.push(row);
        }
    }
    rows
}

// ---------------------------------------------------------------------------
// Global snapshot
// ---------------------------------------------------------------------------

static LIVE_CATALOG: RwLock<Option<Arc<DevinLiveCatalog>>> = RwLock::new(None);

/// Publish a freshly parsed catalog. Empty parses (absent/malformed field
/// 33) never clobber a good snapshot — the server only omits the blob on
/// degraded responses.
pub fn store_live_catalog(rows: Vec<DevinLiveModelRow>) {
    if rows.is_empty() {
        return;
    }
    let next = Arc::new(DevinLiveCatalog::build(rows));
    let mut guard = LIVE_CATALOG.write().unwrap_or_else(|e| e.into_inner());
    *guard = Some(next);
}

/// Current live catalog snapshot, when a `GetUserStatus` has already
/// delivered one this process.
pub fn live_catalog() -> Option<Arc<DevinLiveCatalog>> {
    LIVE_CATALOG
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Drop the snapshot (test isolation).
#[cfg(test)]
pub fn clear_live_catalog() {
    let mut guard = LIVE_CATALOG.write().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}
