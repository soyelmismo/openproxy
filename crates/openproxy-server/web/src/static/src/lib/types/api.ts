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

/** `#[serde(rename_all = "snake_case")]`.
 *  Selection algorithm used to order targets at request time, layered
 *  on top of `Strategy::Priority`. `null` / `"strict"` = legacy
 *  `priority_order` walk (migration 000035).
 *  @see crates/openproxy-core/src/combos.rs:57 */
export type PriorityMode = "strict" | "lkgp" | "weighted" | "least_used" | "p2c";

/** `#[serde(rename_all = "snake_case")]`.
 *  Cooldown growth mode. `null` / `"flat"` = current behavior (always
 *  `cooldown_base_secs`); `"exponential"` grows as
 *  `base * factor^(failures-1)`, capped at `max` (migration 000035).
 *  @see crates/openproxy-core/src/combos.rs:129 */
export type CooldownMode = "flat" | "exponential";

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
  /** Override del context window (tokens). `null` = auto-compute
   *  (mínimo entre todos los targets, incluyendo sub-combos). */
  context_window?: number | null;
  /** Selection algorithm for `Strategy::Priority`. `null` / `"strict"`
   *  = legacy `priority_order` walk (migration 000035). Ignored for
   *  `RoundRobin` / `Shuffle`. */
  priority_mode?: PriorityMode | null;
  /** Cooldown growth mode. `null` / `"flat"` = always `cooldown_base_secs`;
   *  `"exponential"` = `base * factor^(failures-1)` capped at `max`. */
  cooldown_mode?: CooldownMode | null;
  /** Per-combo override for the cooldown base (seconds). `null` = use
   *  the global `[cooldown] cooldown_secs`. */
  cooldown_base_secs?: number | null;
  /** Per-combo override for the cooldown cap (seconds). `null` = use
   *  the global `[cooldown] max_secs` (default 3600). */
  cooldown_max_secs?: number | null;
  /** Per-combo override for the exponential growth factor. `null` =
   *  use the global `[cooldown] factor` (default 2). */
  cooldown_factor?: number | null;
  /** LKGP exploration rate (0.0–1.0). `null` = default 0.1. Only
   *  meaningful when `priority_mode = "lkgp"`. */
  lkgp_exploration_rate?: number | null;
  /** Selection window (seconds) for `least_used` / `p2c` modes. `null`
   *  = default 3600 (1 hour). */
  selection_window_secs?: number | null;
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
  /** Per-target weight for the `weighted` priority mode (migration
   *  000035). The DB column is `INTEGER NOT NULL DEFAULT 1`; existing
   *  rows that pre-date the migration read back as `1`. The field is
   *  optional in the TS type so older API responses that omit it
   *  still parse — the dashboard treats `undefined` as `1`. */
  weight?: number;
}

/** `ComboTarget` enriquecido con metadata del model (display name, etc.)
 *  y datos de cooldown. Lo devuelve el endpoint admin para que el
 *  dashboard no haga un roundtrip extra a `/admin/models`.
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
  /** Context window del modelo (tokens). `null` para sub-combo targets
   *  o modelos sin metadata. */
  context_length: number | null;
  /** Max output tokens del modelo. `null` para sub-combo targets. */
  max_output_tokens: number | null;
  /** `true` cuando el provider de este target está activo. `false`
   *  cuando el provider fue desactivado — el target sigue visible y
   *  reordenable en el dashboard, pero no se usa para routing. */
  provider_active: boolean;
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

/** Fila de la agregación `by_provider` — como `ByModelRow` pero
 *  agrupada solo por `provider_id`. El dashboard la usa para la
 *  tabla de totales por provider en el rango seleccionado.
 *  @see crates/openproxy-core/src/usage.rs:305 */
export interface ByProviderRow {
  provider_id: string;
  unique_requests: number;
  total_rows: number;
  winners: number;
  total_prompt_tokens: number;
  total_completion_tokens: number;
  total_cost_usd: number;
}

/** Fila de la agregación `monthly_by_provider` — `provider_id` × mes.
 *  `month` es `strftime('%Y-%m', created_at)` (e.g. `"2026-06"`). El
 *  frontend pivota estas filas en una matriz providers × months.
 *  @see crates/openproxy-core/src/usage.rs:321 */
export interface MonthlyByProviderRow {
  provider_id: string;
  /** `"YYYY-MM"` — el mes calendario en UTC. */
  month: string;
  unique_requests: number;
  total_rows: number;
  total_prompt_tokens: number;
  total_completion_tokens: number;
  total_cost_usd: number;
}

/** Fila de la agregación `by_day` — totales diarios para gráficas.
 *  `date` es `strftime('%Y-%m-%d', created_at)` (e.g. `"2026-06-21"`).
 *  @see crates/openproxy-core/src/usage.rs:334 */
export interface ByDayRow {
  /** `"YYYY-MM-DD"` — el día calendario en UTC. */
  date: string;
  unique_requests: number;
  total_rows: number;
  total_prompt_tokens: number;
  total_completion_tokens: number;
  total_cost_usd: number;
  errors: number;
}

/** Fila de la agregación `by_status` — conteo por código HTTP.
 *  @see crates/openproxy-core/src/usage.rs:592 */
export interface ByStatusRow {
  status_code: number;
  count: number;
}

/** Fila de la agregación `errors` — últimos N errores.
 *  @see crates/openproxy-core/src/usage.rs:637 */
export interface ErrorRow {
  request_id: string;
  trace_id: string;
  provider_id: string;
  upstream_model_id: string;
  status_code: number;
  error_msg_redacted: string;
  created_at: string;
}

/** Valores aceptados por el query param `?preset=` de los endpoints
 *  `/usage/*`. `custom` significa "sin preset" (el server usa los
 *  `from`/`to` explícitos o, si ambos faltan, devuelve todo el
 *  histórico). El resto se resuelve a una ventana `(from, to)` en
 *  `crates/openproxy-server/src/handlers/admin.rs:resolve_preset`.
 *  @see crates/openproxy-server/src/handlers/admin.rs:168 */
export type UsagePreset =
  | "today" | "7d" | "30d"
  | "this_month" | "last_month" | "last_6_months"
  | "ytd" | "custom";

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
  /** Rows where `cost_usd = 0.0 AND prompt_tokens > 0` — consumieron
   *  tokens (el pricing debería haber aplicado) pero cost quedó en 0,
   *  indicando pricing faltante al grabar. El dashboard lo surfacea
   *  como un banner amarillo.
   *  @see crates/openproxy-core/src/usage.rs:283 */
  rows_with_null_pricing: number;
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
  /** True iff this row's response was actually delivered to the HTTP
   *  client (winning attempt). False for intermediate retries that
   *  were tried internally but never reached the client. */
  client_response: boolean;
  /** True if prompt_tokens were estimated (upstream didn't report usage). */
  prompt_tokens_estimated: boolean;
  /** True if completion_tokens were estimated (upstream didn't report usage). */
  completion_tokens_estimated: boolean;
  created_at: string;
}

// ----------------------------------------------------------------------------
// Inputs — cuerpos que el dashboard POSTea al server.
// ----------------------------------------------------------------------------

/** POST `/admin/providers`. `auth_type` y `format` viajan como
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

/** PATCH `/admin/providers/:id`. Partial-update; todos los campos
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

/** POST `/admin/accounts`. `api_key` se encripta antes de guardarse.
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

/** POST `/admin/combos`.
 *  @see crates/openproxy-core/src/admin.rs:451 */
export interface CreateComboInput {
  name: string;
  /** "priority" | "round_robin" | "shuffle" — el server lo parsea. */
  strategy: string;
  /** 1..=8; default 1. */
  race_size: number | null;
  /** "strict" | "lkgp" | "weighted" | "least_used" | "p2c" — el server
   *  lo parsea. Omit / `undefined` = legacy `"strict"` default. */
  priority_mode?: string;
  /** "flat" | "exponential". Omit / `undefined` = legacy `"flat"` default. */
  cooldown_mode?: string;
  /** Per-combo cooldown base (seconds). Omit = use global default. */
  cooldown_base_secs?: number;
  /** Per-combo cooldown cap (seconds). Omit = use global default. */
  cooldown_max_secs?: number;
  /** Per-combo exponential growth factor. Omit = use global default. */
  cooldown_factor?: number;
  /** LKGP exploration rate (0.0–1.0). Omit = default 0.1. */
  lkgp_exploration_rate?: number;
  /** Selection window (seconds) for `least_used` / `p2c`. Omit = 3600. */
  selection_window_secs?: number;
}

/** POST `/admin/combos/:id/targets`. XOR entre `model_row_id` y
 *  `sub_combo_id`: exactamente uno de los dos.
 *  @see crates/openproxy-core/src/admin.rs:470 */
export interface AddTargetInput {
  provider_id: string;
  account_id: AccountId | null;
  model_row_id: ModelRowId | null;
  sub_combo_id: ComboId | null;
  priority_order: number;
}

/** POST `/admin/models/custom` (creación a mano, no-discovery).
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

/** POST `/admin/models/bulk-toggle`.
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

// ----------------------------------------------------------------------------
// Debug logs — in-memory ring buffer of recent `tracing` events.
// ----------------------------------------------------------------------------

/** A single captured `tracing` event, serialized from the server's
 *  in-memory ring buffer. `request_id`, `trace_id`, and `span_path`
 *  are `Option<String>` on the Rust side, so they serialize as `null`
 *  when the event had no span context (e.g. startup logs, background
 *  scheduler events).
 *  @see crates/openproxy-server/src/debug_log.rs:64 */
export interface DebugLogEntry {
  /** Monotonically increasing sequence number. The frontend polls
   *  with `?since=N` to fetch only entries with `seq > N`. */
  seq: number;
  /** ISO-8601 timestamp with millisecond precision
   *  (e.g. `"2026-06-23T12:34:56.789Z"`). */
  timestamp: string;
  /** `WARN`, `ERROR`, `INFO`, `DEBUG`, etc. Free-form string — the
   *  dashboard color-codes by uppercasing and matching known levels. */
  level: string;
  /** The tracing target (usually the module path, e.g.
   *  `openproxy_core::pipeline`). */
  target: string;
  /** The formatted message string (post-`tracing` field formatting).
   *  Sensitive values are already redacted by the pipeline before
   *  logging. */
  message: string;
  /** `request_id` extracted from the span context, when available. */
  request_id: string | null;
  /** `trace_id` extracted from the span context, when available. */
  trace_id: string | null;
  /** The span hierarchy as a slash-separated path (e.g.
   *  `execute_single/dispatch_upstream_streaming`). `null` when the
   *  event fired outside any span. */
  span_path: string | null;
}

/** Response envelope for `GET /admin/debug/logs`.
 *  @see crates/openproxy-server/src/handlers/admin.rs:4528 */
export interface DebugLogsResponse {
  /** The entries matching the query, oldest-first. */
  entries: DebugLogEntry[];
  /** The highest `seq` in the returned set. The frontend passes
   *  this as `since` on the next poll to fetch only new entries. */
  latest_seq: number;
  /** Total entries currently in the ring buffer matching the filter
   *  (before the `limit` truncation). The dashboard renders this as
   *  "Buffer: X / 1000" so the operator can see how full the buffer
   *  is at a glance. */
  total_in_buffer: number;
}
