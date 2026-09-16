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
  created_at: string;
}
