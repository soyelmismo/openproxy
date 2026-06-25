// lib/types/notifications.ts
// ============================================================================
// TypeScript mirror of `crates/openproxy-core/src/notifications.rs` (F1).
//
// Contract: every field name here matches the Rust struct field name
// (snake_case) because F1 serializes via `serde_json::json!` macros with
// literal keys. When the Rust side changes, update both sides — there is
// no codegen.
//
// Three groups:
//   1. `NotificationEvent` — pushed live over the WebSocket (F2).
//   2. `NotificationRow` — returned by `GET /admin/api/notifications`.
//   3. Per-kind payload structs — narrow `NotificationEvent.payload` /
//      `NotificationRow.payload` based on `kind`.
//
// See `lib/types/api.ts` for the conventions (snake_case fields,
// `Option<T>` → `T | null`, `serde_json::Value` → `unknown`-ish).
// ============================================================================

/** Kind discriminator for notifications. Mirrors the CHECK constraint
 *  in migration 000036 + the `KIND_*` constants in `notifications.rs`.
 *  @see crates/openproxy-core/src/notifications.rs:34 */
export type NotificationKind =
  | "model_new"
  | "model_gone"
  | "model_auto_activated"
  | "system";

/** Real-time event pushed over the WebSocket. The server wraps it as
 *  `{ "type": "notification", "data": <NotificationEvent> }` (F2).
 *  @see crates/openproxy-core/src/notifications.rs:64 */
export interface NotificationEvent {
  id: number;
  kind: NotificationKind;
  /** Free-form JSON payload. Always an object in practice (per-kind
   *  payload struct); narrow via `is${Kind}Payload` if you need typed
   *  access. We use `Record<string, unknown>` (slightly tighter than
   *  `unknown`) because every payload emitted by F1 is a JSON object. */
  payload: Record<string, unknown>;
  /** RFC 3339 timestamp set by SQLite `datetime('now')` on insert. */
  created_at: string;
}

/** `model_new` payload — emitted by `models::upsert_many` when a model
 *  appears in discovery that wasn't in the existing snapshot.
 *  @see crates/openproxy-core/src/notifications.rs:75 */
export interface ModelNewPayload {
  provider_id: string;
  model_id: string;
  display_name: string | null;
  target_format: string;
  context_length: number | null;
}

/** `model_gone` payload — emitted by `models::upsert_many` when a model
 *  that was in the existing snapshot is no longer in discovery. The
 *  display_name is snapshotted BEFORE the DELETE so we can show what
 *  was lost; it may be `null` if we couldn't read it.
 *  @see crates/openproxy-core/src/notifications.rs:84 */
export interface ModelGonePayload {
  provider_id: string;
  model_id: string;
  display_name: string | null;
}

/** `model_auto_activated` payload — emitted by
 *  `models::apply_auto_activation` when a newly discovered model
 *  matches the provider's `auto_activate_keyword` and is flipped to
 *  `active=1`. `matched_keyword` is `null` when the provider had no
 *  keyword configured (all new models auto-activate).
 *  @see crates/openproxy-core/src/notifications.rs:93 */
export interface ModelAutoActivatedPayload {
  provider_id: string;
  model_id: string;
  display_name: string | null;
  matched_keyword: string | null;
}

/** `system` payload — emitted by `discovery_scheduler` on error paths
 *  (`discovery_failed`, `account_key_decrypt_failed`) and any future
 *  system-level event. `code` is the stable machine-readable
 *  identifier (also the dedup key); `details` is free-form.
 *  @see crates/openproxy-core/src/notifications.rs:103 */
export interface SystemPayload {
  code: string;
  message: string;
  provider_id: string | null;
  details: unknown;
}

/** Notification row as returned by `GET /admin/api/notifications`. The
 *  `read_at` / `archived_at` fields are `null` until the corresponding
 *  action is taken. `dedup_key` and `provider_id` are informational —
 *  the dashboard doesn't typically need them, but they're useful for
 *  grouping/debugging.
 *  @see crates/openproxy-core/src/notifications.rs:120 */
export interface NotificationRow {
  id: number;
  kind: NotificationKind;
  payload: Record<string, unknown>;
  read_at: string | null;
  archived_at: string | null;
  created_at: string;
  dedup_key: string | null;
  provider_id: string | null;
}

// ----------------------------------------------------------------------------
// Payload type-guards. Consumers use these to narrow `payload` once they
// know `kind`. They're defensive: a malformed payload (missing fields,
// wrong types) returns `false`, not a runtime error.
// ----------------------------------------------------------------------------

export function isModelNewPayload(p: unknown): p is ModelNewPayload {
  if (typeof p !== "object" || p === null) return false;
  const o = p as Record<string, unknown>;
  return (
    typeof o["provider_id"] === "string" &&
    typeof o["model_id"] === "string" &&
    (o["display_name"] === null || typeof o["display_name"] === "string") &&
    typeof o["target_format"] === "string" &&
    (o["context_length"] === null || typeof o["context_length"] === "number")
  );
}

export function isModelGonePayload(p: unknown): p is ModelGonePayload {
  if (typeof p !== "object" || p === null) return false;
  const o = p as Record<string, unknown>;
  return (
    typeof o["provider_id"] === "string" &&
    typeof o["model_id"] === "string" &&
    (o["display_name"] === null || typeof o["display_name"] === "string")
  );
}

export function isModelAutoActivatedPayload(p: unknown): p is ModelAutoActivatedPayload {
  if (typeof p !== "object" || p === null) return false;
  const o = p as Record<string, unknown>;
  return (
    typeof o["provider_id"] === "string" &&
    typeof o["model_id"] === "string" &&
    (o["display_name"] === null || typeof o["display_name"] === "string") &&
    (o["matched_keyword"] === null || typeof o["matched_keyword"] === "string")
  );
}

export function isSystemPayload(p: unknown): p is SystemPayload {
  if (typeof p !== "object" || p === null) return false;
  const o = p as Record<string, unknown>;
  return (
    typeof o["code"] === "string" &&
    typeof o["message"] === "string" &&
    (o["provider_id"] === null || typeof o["provider_id"] === "string")
    // `details` is `unknown` so any value is acceptable.
  );
}

/** Query string for `GET /admin/api/notifications`.
 *  @see crates/openproxy-server/src/handlers/admin.rs:4810
 *  `NotificationsQuery`. */
export interface NotificationsListQuery {
  unread_only?: boolean;
  limit?: number;
  before_id?: number;
}

/** Response body of `GET /admin/api/notifications/unread-count`.
 *  `{ "unread_count": <n> }`. */
export interface NotificationsUnreadCountResponse {
  unread_count: number;
}
