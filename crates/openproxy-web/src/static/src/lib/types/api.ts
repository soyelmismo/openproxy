// lib/types/api.ts
// ============================================================================
// Tipos TypeScript que reflejan los `pub struct` / `pub enum` de
// `crates/openproxy-core/src/*.rs` (más el sobre de error de
// `crates/openproxy-server/src/error.rs`).
//
// Decisión de arquitectura: tipos MANUALES, sin ts-rs. La razón está
// documentada en el task G2. Cuando cambie un struct del core, hay que
// actualizar el tipo correspondiente acá. El comentario `/** @see ... */`
// apunta a la línea exacta en el `.rs` para facilitar el grep cruzado.
//
// Convenciones de mapeo serde → TS:
//   - `pub enum` con `#[serde(rename_all = "lowercase")]`  → string union
//     de strings lowercase ("healthy", "openai", "bearer", ...).
//   - `pub enum` con `#[serde(rename_all = "snake_case")]` → string union
//     de strings snake ("priority", "round_robin", ...).
//   - Wrapper ID (`ProviderId(String)`, `AccountId(i64)`, ...) con
//     `#[serde(transparent)]` → `string` o `number` (el inner type).
//   - `Option<T>` → `T | null`.
//   - `Vec<T>`   → `T[]`.
//   - `HashMap<K,V>` / `BTreeMap<K,V>` → `Record<K, V>`.
//   - `serde_json::Value` → `unknown` (el consumidor debe type-guard).
//
// `tsconfig` está en modo superestricto (`exactOptionalPropertyTypes`,
// `noUncheckedIndexedAccess`, `noPropertyAccessFromIndexSignature`), así
// que los campos se declaran como `field: T | null` (requerido, nunca
// `undefined`) y los index signatures se acceden con `obj["key"]`.
// ============================================================================

// ----------------------------------------------------------------------------
// ID wrappers — todos `#[serde(transparent)]`, serializan como el inner type.
// ----------------------------------------------------------------------------

/** @see crates/openproxy-core/src/ids.rs:47 */
export type ProviderId = string;

/** @see crates/openproxy-core/src/ids.rs:59 */
export type AccountId = number;

/** @see crates/openproxy-core/src/ids.rs:66 */
export type ComboId = number;

/** @see crates/openproxy-core/src/ids.rs:69 */
export type ComboTargetId = number;

/** @see crates/openproxy-core/src/ids.rs:73 */
export type ModelId = string;

/** @see crates/openproxy-core/src/ids.rs:82 */
export type ModelRowId = number;

/** @see crates/openproxy-core/src/ids.rs:85 */
export type UsageId = number;

/** @see crates/openproxy-core/src/ids.rs:88 */
export type ApiKeyId = number;

// ----------------------------------------------------------------------------
// Enums (string unions).
// ----------------------------------------------------------------------------

/** `#[serde(rename_all = "lowercase")]`.
 *  @see crates/openproxy-core/src/accounts.rs:18 */
export type HealthStatus = "healthy" | "degraded" | "unhealthy";

/** `#[serde(rename_all = "lowercase")]`.
 *  @see crates/openproxy-core/src/providers.rs:31 */
export type ProviderFormat = "openai" | "anthropic" | "mixed" | "gemini";

/** `#[serde(rename_all = "lowercase")]`.
 *  @see crates/openproxy-core/src/providers.rs:65 */
export type AuthType = "bearer" | "x-api-key" | "goog-api-key" | "oauth" | "none";

/** `#[serde(rename_all = "snake_case")]`.
 *  @see crates/openproxy-core/src/combos.rs:13 */
export type Strategy = "priority" | "round_robin" | "shuffle";

/** `#[serde(rename_all = "lowercase")]`.
 *  @see crates/openproxy-core/src/models.rs:22 */
export type TargetFormat = "openai" | "anthropic" | "gemini";

// ----------------------------------------------------------------------------
// Model capabilities — todas las flags son `Option<bool>` por el
// `skip_serializing_if` del lado Rust. Pueden ser `null` o estar ausentes.
// ----------------------------------------------------------------------------

/** @see crates/openproxy-core/src/capabilities.rs:27 */
export interface ModelCapabilities {
  vision: boolean | null;
  tool_calling: boolean | null;
  reasoning: boolean | null;
  thinking: boolean | null;
  attachment: boolean | null;
  structured_output: boolean | null;
  temperature: boolean | null;
}

// ----------------------------------------------------------------------------
// Provider
// ----------------------------------------------------------------------------

/** Fila de la tabla `providers`. `auth_type` y `format` son enums
 *  tipados (no strings libres). `active` lleva `#[serde(default = "default_true")]`
 *  así que clientes viejos que no lo manden lo ven como `true`.
 *  @see crates/openproxy-core/src/providers.rs:107 */
export interface Provider {
  id: ProviderId;
  name: string;
  base_url: string;
  auth_type: AuthType;
  format: ProviderFormat;
  /** JSON de headers extra (string con JSON dentro, no objeto). */
  extra_headers_json: string | null;
  /** Substring que decide qué modelos recién descubiertos se activan. */
  auto_activate_keyword: string | null;
  active: boolean;
  created_at: string;
}

// ----------------------------------------------------------------------------
// Account
// ----------------------------------------------------------------------------

/** Fila de la tabla `accounts`. El `auth_type` acá es `String` libre (no
 *  el enum) — el core lo guarda como `api_key` | `oauth` y no como
 *  `AuthType` tipado. Los `quota_*` los puebla `quota::fetch_*`.
 *  @see crates/openproxy-core/src/accounts.rs:54 */
export interface Account {
  id: AccountId;
  provider_id: ProviderId;
  label: string | null;
  /** Más bajo = más prioritario (convención del priority-router). */
  priority: number;
  extra_config_json: string | null;
  health_status: HealthStatus;
  rate_limited_until: string | null;
  quota_session_used: number | null;
  quota_session_limit: number | null;
  quota_session_reset_at: string | null;
  quota_weekly_used: number | null;
  quota_weekly_limit: number | null;
  quota_weekly_reset_at: string | null;
  quota_plan_name: string | null;
  quota_last_fetched_at: string | null;
  quota_fetch_error: string | null;
  /** Per-model quota details (Antigravity family). Not persisted in DB —
   *  populated from the refresh-quota response. */
  quota_model_details?: ModelQuotaDetail[] | null;
  /** "api_key" u "oauth" — texto libre, no enum. */
  auth_type: string;
  email: string | null;
  oauth_scope: string | null;
  /** JSON de metadata específica del provider OAuth. */
  oauth_provider_specific: string | null;
  /** ISO-8601; null para cuentas no-OAuth. */
  expires_at: string | null;
  created_at: string;
}

// ----------------------------------------------------------------------------
// AccountQuota
// ----------------------------------------------------------------------------

/** Snapshot de cuota de un account. `last_fetched_at` es el único campo
 *  siempre presente; el resto puede ser null si el upstream no los
 *  expone. `fetch_error != null` es la señal de "cuota desconocida".
 *  @see crates/openproxy-core/src/quota.rs:37 */
export interface AccountQuota {
  session_used: number | null;
  session_limit: number | null;
  /** ISO-8601 o epoch seconds — el upstream decide. */
  session_reset_at: string | null;
  weekly_used: number | null;
  weekly_limit: number | null;
  weekly_reset_at: string | null;
  plan_name: string | null;
  /** Siempre presente (epoch secs as string). */
  last_fetched_at: string;
  fetch_error: string | null;
  /** Per-model quota details (Antigravity family). */
  model_details?: ModelQuotaDetail[] | null;
}

/** Per-model quota detail inside `AccountQuota.model_details`.
 *  @see crates/openproxy-core/src/quota.rs:22 */
export interface ModelQuotaDetail {
  model_id: string;
  session_used: number;
  session_limit: number;
  session_reset_at: string | null;
  remaining_fraction: number;
}

// ----------------------------------------------------------------------------
// Model
// ----------------------------------------------------------------------------

/** Fila de la tabla `models`. `active`/`custom` son soft-bits. Los
 *  `*_json` y `*_overrides_json` son strings con JSON adentro (no
 *  objetos tipados) — se persisten como TEXT en SQLite.
 *  @see crates/openproxy-core/src/models.rs:49 */
export interface Model {
  row_id: ModelRowId;
  provider_id: ProviderId;
  /** Upstream model id (e.g. `anthropic/claude-3.5-sonnet`). */
  model_id: ModelId;
  display_name: string | null;
  target_format: TargetFormat;
  discovered_at: string;
  expires_at: string | null;
  timeout_overrides_json: string | null;
  active: boolean;
  /** 0 = request nunca llegó al upstream (DNS/connect/TLS). */
  last_test_status: number | null;
  last_test_at: string | null;
  /** `true` para filas creadas a mano vía `create_custom`. */
  custom: boolean;
  context_length: number | null;
  max_output_tokens: number | null;
  /** JSON serializado de `ModelCapabilities`. */
  capabilities_json: string | null;
  /** Familia lógica (e.g. "Qwen3", "Llama-3.3"). */
  family: string | null;
  /** "chat" | "embedding" | "image" | "audio" | "rerank". */
  model_type: string;
  input_modalities_json: string | null;
  output_modalities_json: string | null;
}

/** Input del adapter (lo que un provider reporta en `/models`).
 *  `row_id`, `discovered_at` y `expires_at` los llena el storage layer.
 *  @see crates/openproxy-core/src/models.rs:117 */
export interface DiscoveredModel {
  model_id: ModelId;
  display_name: string | null;
  target_format: TargetFormat;
  context_length: number | null;
  max_output_tokens: number | null;
  input_modalities: string[] | null;
  output_modalities: string[] | null;
  /** "chat" | "embedding" | "image" | "audio" | "rerank". */
  model_type: string | null;
  family: string | null;
  capabilities: ModelCapabilities | null;
}

// ----------------------------------------------------------------------------
// Combos
// ----------------------------------------------------------------------------

/** @see crates/openproxy-core/src/combos.rs:38 */
export interface Combo {
  id: ComboId;
  name: string;
  strategy: Strategy;
  /** 1..=8 (CHECK constraint). */
  race_size: number;
  created_at: string;
}

/** @see crates/openproxy-core/src/combos.rs:47 */
export interface ComboTarget {
  id: ComboTargetId;
  combo_id: ComboId;
  provider_id: ProviderId;
  /** `null` = rotar entre cuentas healthy de este provider. */
  account_id: AccountId | null;
  /** XOR con `sub_combo_id`: exactamente uno de los dos es `Some`. */
  model_row_id: ModelRowId | null;
  sub_combo_id: ComboId | null;
  priority_order: number;
}

/** `ComboTarget` enriquecido con metadata del model (display name, etc.)
 *  y datos de cooldown. Lo devuelve el endpoint admin para que el
 *  dashboard no haga un roundtrip extra a `/v1/admin/models`.
 *  @see crates/openproxy-core/src/combos.rs:81 */
export interface ComboTargetWithModel extends ComboTarget {
  /** Nombre del sub-combo upstream; `null` para flat targets. */
  sub_combo_name: string | null;
  /** Upstream model id (e.g. `anthropic/claude-3.5-sonnet`). Vacío
   *  para sub-combo targets o si la fila del model fue borrada. */
  model_id: string;
  model_display_name: string | null;
  /** `true` cuando hay un row activo en `target_cooldowns`. */
  in_cooldown: boolean;
  /** ISO-8601; `null` cuando no está en cooldown. */
  cooldown_until: string | null;
  /** Motivo del cooldown; `null` cuando no está en cooldown. */
  cooldown_reason: string | null;
}

/** Proyección liviana para el picker "add sub-combo target".
 *  @see crates/openproxy-core/src/admin.rs:513 */
export interface ComboSummary {
  id: number;
  name: string;
}

// ----------------------------------------------------------------------------
// Usage
// ----------------------------------------------------------------------------

/** Stage transition de un request in-flight. Lo emite el pipeline y lo
 *  re-emite el WS de live-log al dashboard. NO se persiste a DB.
 *  @see crates/openproxy-core/src/usage.rs:108 */
export interface StageEvent {
  request_id: string;
  trace_id: string;
  provider_id: string;
  upstream_model_id: string;
  /** "started" | "connecting" | "waiting_ttft" | "streaming"
   *  | "completed" | "failed" | "cancelled". */
  stage: string;
  elapsed_ms: number;
  connect_ms: number | null;
  ttft_ms: number | null;
  /** 0 mientras está in-flight; código final en `completed`/`failed`. */
  status_code: number;
  error: string | null;
  /** Upstream stop reason (e.g. "end_turn", "max_tokens", "stop_sequence"
   *  for Anthropic; "stop", "length" for OpenAI). Only set on terminal
   *  events (`completed`/`failed`). */
  stop_reason: string | null;
  /** RFC-3339 del momento de emisión. */
  timestamp: string;
  /** Compression savings (0.0–100.0) or null when off. */
  compression_savings_pct: number | null;
  /** Compression techniques applied (CSV) or null when off. */
  compression_techniques: string | null;
}

/** Fila de la agregación `by_model`.
 *  @see crates/openproxy-core/src/usage.rs:290 */
export interface ByModelRow {
  provider_id: ProviderId;
  upstream_model_id: string;
  unique_requests: number;
  total_rows: number;
  winners: number;
  total_prompt_tokens: number;
  total_completion_tokens: number;
  total_cost_usd: number;
}

/** Fila de la agregación `by_account`.
 *  @see crates/openproxy-core/src/usage.rs:303 */
export interface ByAccountRow {
  account_id: AccountId;
  provider_id: ProviderId;
  unique_requests: number;
  total_rows: number;
  errors: number;
  total_cost_usd: number;
}

/** Roll-up agregado sobre un set filtrado de usage rows.
 *  @see crates/openproxy-core/src/usage.rs:264 */
export interface UsageSummary {
  unique_requests: number;
  total_rows: number;
  total_attempts: number;
  winners: number;
  losers: number;
  errors: number;
  total_prompt_tokens: number;
  total_completion_tokens: number;
  total_cost_usd: number;
  /** `null` cuando ningún row del filtro tiene `ttft_ms`. */
  avg_ttft_ms: number | null;
  avg_total_ms: number;
}

/** Fila devuelta por el long-polling feed del dashboard.
 *  @see crates/openproxy-core/src/usage.rs:726 */
export interface RecentUsageRow {
  id: UsageId;
  request_id: string;
  trace_id: string;
  provider_id: ProviderId;
  upstream_model_id: string;
  status_code: number;
  total_ms: number;
  prompt_tokens: number | null;
  completion_tokens: number | null;
  cost_usd: number | null;
  connect_ms: number | null;
  ttft_ms: number | null;
  /** JSON arbitrario del body — el consumidor type-guarde'a. */
  request_body_json: unknown;
  response_body_json: unknown;
  request_headers: Record<string, string> | null;
  response_headers: Record<string, string> | null;
  error_message: string | null;
  race_total: number | null;
  race_attempts: number | null;
  is_streaming: boolean;
  stream_complete: boolean;
  race_lost: boolean;
  /** Upstream stop reason (e.g. "end_turn", "max_tokens", "stop_sequence"
   *  for Anthropic; "stop", "length" for OpenAI). */
  stop_reason: string | null;
  /** Compression savings (0.0–100.0) or null when off. */
  compression_savings_pct: number | null;
  /** Compression techniques applied (CSV) or null when off. */
  compression_techniques: string | null;
  created_at: string;
}

// ----------------------------------------------------------------------------
// Inputs — cuerpos que el dashboard POSTea al server.
// ----------------------------------------------------------------------------

/** POST `/v1/admin/providers`. `auth_type` y `format` viajan como
 *  strings (no enums) y el server los parsea a los enums tipados.
 *  @see crates/openproxy-core/src/admin.rs:45 */
export interface CreateProviderInput {
  id: string;
  name: string;
  base_url: string;
  auth_type: string;
  format: string;
  extra_headers_json: string | null;
}

/** PATCH `/v1/admin/providers/:id`. Partial-update; todos los campos
 *  opcionales. `auto_activate_keyword` usa la convención de tres
 *  estados para distinguir "no tocar" de "set a null":
 *    - `undefined` (omitir el campo): no se toca la columna.
 *    - `null`:           se borra la columna.
 *    - `string`:         se setea al string.
 *  NOTA: con `exactOptionalPropertyTypes: true`, "omitir el campo" se
 *  expresa dejándolo fuera del objeto literal, no pasando `undefined`.
 *  @see crates/openproxy-core/src/admin.rs:133 */
export interface UpdateProviderInput {
  name?: string;
  base_url?: string;
  extra_headers_json?: string;
  /** `string | null` — el server distingue ausente de null. */
  auto_activate_keyword?: string | null;
}

/** POST `/v1/admin/accounts`. `api_key` se encripta antes de guardarse.
 *  `priority` default 100 (lower = higher).
 *  @see crates/openproxy-core/src/admin.rs:234 */
export interface CreateAccountInput {
  provider_id: string;
  /** `null` para cuentas OAuth. */
  api_key: string | null;
  label: string | null;
  priority: number | null;
  extra_config_json: string | null;
}

/** POST `/v1/admin/combos`.
 *  @see crates/openproxy-core/src/admin.rs:451 */
export interface CreateComboInput {
  name: string;
  /** "priority" | "round_robin" | "shuffle" — el server lo parsea. */
  strategy: string;
  /** 1..=8; default 1. */
  race_size: number | null;
}

/** POST `/v1/admin/combos/:id/targets`. XOR entre `model_row_id` y
 *  `sub_combo_id`: exactamente uno de los dos.
 *  @see crates/openproxy-core/src/admin.rs:470 */
export interface AddTargetInput {
  provider_id: string;
  account_id: AccountId | null;
  model_row_id: ModelRowId | null;
  sub_combo_id: ComboId | null;
  priority_order: number;
}

/** POST `/v1/admin/models/custom` (creación a mano, no-discovery).
 *  @see crates/openproxy-core/src/admin.rs:745 */
export interface CreateCustomModelInput {
  provider_id: string;
  model_id: string;
  display_name: string | null;
  /** "openai" | "anthropic" | "gemini" — el server lo parsea. */
  target_format: string;
  /** 0 = nunca expira. */
  ttl_seconds: number;
}

/** POST `/v1/admin/models/bulk-toggle`.
 *  @see crates/openproxy-core/src/admin.rs:828 */
export interface BulkToggleInput {
  provider_id: string;
  active: boolean;
}

// ----------------------------------------------------------------------------
// Error envelope
// ----------------------------------------------------------------------------

/** `{"error": {"code", "message"}}` que devuelve `ApiError` en el server.
 *  `code` es uno de los strings del `CoreError::code()` (e.g.
 *  `"validation"`, `"auth"`, `"provider_not_found"`, ...).
 *  @see crates/openproxy-server/src/error.rs:20 */
export interface ApiErrorBody {
  code: string;
  message: string;
}

export interface ApiErrorEnvelope {
  error: ApiErrorBody;
}
