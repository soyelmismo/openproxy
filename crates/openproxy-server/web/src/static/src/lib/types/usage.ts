import type { AccountId, ProviderId, UsageId } from "./common";

export interface StageEvent {
  request_id: string;
  trace_id: string;
  provider_id: string;
  upstream_model_id: string;
  stage: string;
  elapsed_ms: number;
  connect_ms: number | null;
  ttft_ms: number | null;
  status_code: number;
  error: string | null;
  stop_reason: string | null;
  timestamp: string;
  compression_savings_pct: number | null;
  compression_techniques: string | null;
  pii_redacted?: string | null;
  endpoint_kind?: string;
  /** Session fingerprint as a decimal string; absent when the request carried no session identity. */
  session_hash?: string | null;
  api_key_id?: number | null;
}

export interface ByModelRow {
  provider_id: ProviderId;
  upstream_model_id: string;
  unique_requests: number;
  total_rows: number;
  winners: number;
  total_prompt_tokens: number;
  total_completion_tokens: number;
  total_cost_usd: number;
  avg_compression_savings_pct: number | null;
}

export interface ByAccountRow {
  account_id: AccountId;
  provider_id: ProviderId;
  unique_requests: number;
  total_rows: number;
  errors: number;
  total_cost_usd: number;
}

export interface ByProviderRow {
  provider_id: string;
  unique_requests: number;
  total_rows: number;
  winners: number;
  total_prompt_tokens: number;
  total_completion_tokens: number;
  total_cost_usd: number;
  avg_compression_savings_pct: number | null;
}

export interface MonthlyByProviderRow {
  provider_id: string;
  month: string;
  unique_requests: number;
  total_rows: number;
  total_prompt_tokens: number;
  total_completion_tokens: number;
  total_cost_usd: number;
}

export interface ByDayRow {
  date: string;
  unique_requests: number;
  total_rows: number;
  total_prompt_tokens: number;
  total_completion_tokens: number;
  total_cost_usd: number;
  errors: number;
}

export interface ByStatusRow {
  status_code: number;
  count: number;
}

export interface ByComboRow {
  combo_id: number;
  combo_name: string;
  requests: number;
  ok_requests: number;
  error_requests: number;
  total_attempts: number;
  success_rate_pct: number;
  error_rate_pct: number;
  avg_attempts: number;
  /** Mean ms of a successful request, across all the attempts it consumed. */
  avg_ok_ms: number | null;
  /** Mean ms of a request the combo never covered, until the client error. */
  avg_error_ms: number | null;
}

export interface ErrorRow {
  request_id: string;
  trace_id: string;
  provider_id: string;
  upstream_model_id: string;
  status_code: number;
  error_msg_redacted: string;
  created_at: string;
}

export type UsagePreset =
  | "today"
  | "7d"
  | "30d"
  | "this_month"
  | "last_month"
  | "last_6_months"
  | "ytd"
  | "custom";

export interface UsageSummary {
  unique_requests: number;
  all_requests: number;
  failed_requests: number;
  total_rows: number;
  total_attempts: number;
  winners: number;
  losers: number;
  errors: number;
  total_prompt_tokens: number;
  total_completion_tokens: number;
  total_cached_tokens: number;
  total_cost_usd: number;
  avg_ttft_ms: number | null;
  avg_total_ms: number;
  avg_success_connect_ms: number | null;
  avg_success_ttft_ms: number | null;
  avg_success_total_ms: number | null;
  rows_with_null_pricing: number;
  avg_compression_savings_pct: number | null;
}

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
  cached_tokens: number | null;
  cost_usd: number | null;
  connect_ms: number | null;
  ttft_ms: number | null;
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
  stop_reason: string | null;
  compression_savings_pct: number | null;
  compression_techniques: string | null;
  pii_redacted?: string | null;
  client_response: boolean;
  prompt_tokens_estimated: boolean;
  completion_tokens_estimated: boolean;
  proxy_url: string | null;
  proxy_status: string | null;
  is_proxy_rotated: boolean;
  endpoint_kind?: string;
  /** Session fingerprint as a decimal string; absent when the request carried no session identity. */
  session_hash?: string | null;
  api_key_id?: number | null;
  /** Audio duration in milliseconds; set only on transcription rows. */
  audio_ms?: number | null;
  created_at: string;
}

// ── Per-key dollar quotas (5h / weekly / monthly windows) ────────────

export interface QuotaWindowStatus {
  /** `null` = this window has no limit (disabled). */
  cap_usd: number | null;
  /** Spend at official model prices inside the window. */
  spent_usd: number;
  /** Fixed windows: ISO instant when the window resets. Rolling 5h: null. */
  resets_at: string | null;
  /** Rolling 5h only: oldest spend still inside the window. */
  oldest_spend_at: string | null;
}

export interface KeyQuotaStatus {
  /** `false` = the key has no quota row (unlimited). */
  enabled: boolean;
  month: QuotaWindowStatus;
  week: QuotaWindowStatus;
  five_hours: QuotaWindowStatus;
}

export interface KeyQuotaCaps {
  monthly_usd_cap: number | null;
  weekly_usd_cap: number | null;
  five_hour_usd_cap: number | null;
}

/** GET /keys/:id/quota response. */
export interface KeyQuotaResponse {
  api_key_id: number;
  enabled: boolean;
  caps: KeyQuotaCaps | null;
  spend: KeyQuotaStatus;
}
/** Row of `GET /keys/quotas`: every key that carries a quota row. */
export interface KeyQuotaListItem {
  api_key_id: number;
  label: string | null;
  monthly_usd_cap: number | null;
  weekly_usd_cap: number | null;
  five_hour_usd_cap: number | null;
}
