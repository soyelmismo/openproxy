// Modal state: the shared row shape (LogDetailLog), the pinned modal identity,
// the openLogDetail generation counter, and hasCompleteLogDetail.
//
// No sibling imports, so the rest of the log-detail family can import from
// here without cycles.

/** Loose shape for the `log` arg in renderLogDetailModal. RecentUsageRow
 *  (long-poll feed) and UsageDetailRow (detail endpoint) overlap without one
 *  being a superset, so the union is an open record that keeps the `||`
 *  lookups in the body typechecking. */
export interface LogDetailLog {
  // RecentUsageRow (long-poll feed)
  id?: number;
  request_id?: string;
  provider_id?: string;
  upstream_model_id?: string;
  status_code?: number;
  total_ms?: number;
  prompt_tokens?: number | null;
  completion_tokens?: number | null;
  prompt_tokens_estimated?: boolean;
  completion_tokens_estimated?: boolean;
  tokens_per_sec?: number | null;
  cost_usd?: number | null;
  is_streaming?: boolean;
  stream_complete?: boolean;
  /** Compression savings in tokens (0.0–100.0) or null when off. */
  compression_savings_pct?: number | null;
  compression_techniques?: string | null;
  pii_redacted?: string | null;
  race_lost?: boolean;
  request_body_json?: unknown;
  response_body_json?: unknown;
  error_message?: string | null;
  created_at?: string;
  // The interface has no index signature, so an unlisted field would fail under
  // `noPropertyAccessFromIndexSignature`.
  trace_id?: string;
  endpoint_kind?: string;
  request_headers?: Record<string, string> | null;
  response_headers?: Record<string, string> | null;
  // UsageDetailRow (detail endpoint) extras
  detail?: Record<string, unknown> | null;
  meta?: Record<string, unknown> | null;
  response?: unknown;
  error_msg?: string | null;
  error_msg_redacted?: string | null;
  error_message_redacted?: string | null;
  errors?: unknown;
  error?: unknown;
  model_id?: string;
  upstream_model?: string;
  account_id?: string | number | null;
  account_label?: string | null;
  combo_id?: string | number | null;
  api_key_id?: string | number | null;
  user_agent?: string | null;
  latency_ms?: number | null;
  elapsed_ms?: number | null;
  timestamp?: string;
  cost?: number | null;
  status?: string | null;
  usage?: { cost?: number | null } | null;
  requests?: unknown[];
  stages?: unknown[];
}

// Active tab ("request" | "response" | "errors" | "raw"). Set by
// logDetailTabClick / initializeLogDetailTabs so clock-tick re-renders keep the
// user's selection.
let currentActiveTab: string = "request";

// Presentation preference survives clock/WS updates and reopening during this session.
let markdownEnabled = false;
export function getLogDetailMarkdown(): boolean { return markdownEnabled; }
export function setLogDetailMarkdown(enabled: boolean): void { markdownEnabled = enabled; }

/** Read the currently-active modal tab. */
export function getActiveLogDetailTab(): string {
  return currentActiveTab;
}

/** Set the currently-active modal tab. Called by logDetailTabClick. */
export function setActiveLogDetailTab(tab: string): void {
  currentActiveTab = tab;
}

// The immutable request_id + trace_id of the row the user opened, set in
// `openLogDetail` and cleared in `removeLogDetailModal` (modal.ts).
//
// `state.logs.selectedRow` is a mutable reference that `updateOpenLogDetail`
// itself, or a race in `openLogDetail` (user clicks row B while row A's detail
// fetch is in flight), can reassign. Filtering on it would let another row's
// updates replace the modal mid-debug. Filtering on the pinned identity, fixed
// for the modal's lifetime, makes that impossible.
let pinnedRequestId: string | null = null;
let pinnedTraceId: string | null = null;

/** Pin the modal identity, called once `openLogDetail` has rendered. */
export function setPinnedIdentity(requestId: string, traceId: string): void {
  pinnedRequestId = requestId;
  pinnedTraceId = traceId;
}

/** Clear the pinned identity on modal removal, so later WS events do not
 *  target a closed modal. */
export function clearPinnedIdentity(): void {
  pinnedRequestId = null;
  pinnedTraceId = null;
}

/** Get the currently pinned traceId. */
export function getPinnedTraceId(): string | null {
  return pinnedTraceId;
}

// Race protection across the async `/usage/detail` fetch: each `openLogDetail`
// captures the generation, and the callback discards its result if the user
// clicked another row in the meantime.
let openLogDetailGeneration: number = 0;

/** Bump and return the generation. Call at the START of `openLogDetail` to
 *  invalidate an in-flight fetch from a previous click. */
export function bumpOpenLogDetailGeneration(): number {
  openLogDetailGeneration += 1;
  return openLogDetailGeneration;
}

/** Whether `gen` is still the latest `openLogDetail`. Check after an await to
 *  decide whether to apply or discard the result. */
export function isCurrentOpenLogDetailGeneration(gen: number): boolean {
  return gen === openLogDetailGeneration;
}

/** Whether `row` is the pinned row. False when no modal is open.
 *
 *  trace_id matching is strict and symmetric: a pinned trace_id demands the
 *  same one on the row, and an empty pinned trace_id demands an empty row
 *  trace_id. Without that, a row with no trace_id would match its own retries
 *  and the modal's model name would shift mid-debug. */
export function matchesPinnedModalIdentity(
  row: { request_id?: string; trace_id?: string } | null | undefined,
): boolean {
  if (pinnedRequestId === null) return false;
  if (!row) return false;
  // request_id is the primary identity.
  if (row.request_id !== pinnedRequestId) return false;
  // STRICT trace_id matching — no skipping. Normalize empty/null/undefined
  // to a single canonical value so "" === null === undefined.
  const pinnedTid = pinnedTraceId || "";
  const rowTid = row.trace_id || "";
  // Two empty trace_ids cannot confirm identity, so the modal stays frozen on
  // its snapshot rather than risk overlaying a sibling request.
  if (pinnedTid === "" && rowTid === "") return false;
  if (pinnedTid !== rowTid) return false;
  return true;
}

/** A row is complete once it carries a request body, a response body, or an
 *  error block. In-flight rows (request_id only) return false so the caller
 *  fetches /usage/detail. `requests[]` / `stages[]` count as fallback signals
 *  for older codepaths. */
export function hasCompleteLogDetail(row: LogDetailLog | null | undefined): boolean {
  if (!row) return false;
  if (row.request_body_json != null) return true;
  if (row.response_body_json != null) return true;
  if (Array.isArray(row.requests) && row.requests.length > 0) return true;
  if (Array.isArray(row.stages) && row.stages.length > 0) return true;
  if (row.response != null) return true;
  if (row.errors != null || row.error != null || row.error_msg != null) return true;
  const detail: Record<string, unknown> | null | undefined = row.detail;
  if (detail && (detail["response"] != null || detail["request_body_json"] != null
    || (Array.isArray(detail["requests"]) && (detail["requests"] as unknown[]).length > 0))) return true;
  return false;
}
