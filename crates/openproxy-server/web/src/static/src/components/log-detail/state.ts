// components/log-detail/state.ts — modal state: the shared row shape
// (LogDetailLog), the pinned modal identity, the openLogDetail
// generation counter, and the hasCompleteLogDetail predicate.
//
// This module is intentionally dependency-free within the log-detail
// family (no sibling imports) so every other module can import from
// it without cycles.
//
// Split out of the former components/log-detail.ts monolith (Q19).

/** Loose shape for the `log` arg in renderLogDetailModal. The
 *  modal accepts the long-poll row shape (RecentUsageRow) and the
 *  detail-endpoint row shape (UsageDetailRow) — they overlap but
 *  neither is a strict superset. We model the union as an open
 *  record so the various `||` lookups in the body still typecheck
 *  without losing field-level narrowing. */
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
  race_lost?: boolean;
  request_body_json?: unknown;
  response_body_json?: unknown;
  error_message?: string | null;
  created_at?: string;
  // RecentUsageRow fields accessed by buildDebugBundle but not
  // listed above. Added here so the typechecker accepts the
  // dot-notation access (the interface has no index signature,
  // so missing fields are a compile error under `noPropertyAccessFromIndexSignature`).
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

// ----------
// Active tab — which `[data-log-tab]` section is visible in the modal
// ("request" | "response" | "errors" | "raw"). Mutated by
// logDetailTabClick / initializeLogDetailTabs (modal.ts), read by
// renderLogDetailModal (modal.ts) so clock-tick re-renders keep the
// user's selected tab.
// ----------
let currentActiveTab: string = "request";

/** Read the currently-active modal tab. */
export function getActiveLogDetailTab(): string {
  return currentActiveTab;
}

/** Set the currently-active modal tab. Called by logDetailTabClick. */
export function setActiveLogDetailTab(tab: string): void {
  currentActiveTab = tab;
}

// ----------
// Pinned modal identity — the IMMUTABLE request_id + trace_id of the row the
// user opened. Set in `openLogDetail` (modal.ts), cleared in
// `removeLogDetailModal` (modal.ts).
//
// WHY: `state.logs.selectedRow` is a mutable reference that can be reassigned
// by `updateOpenLogDetail` itself (circular dependency) or by a race condition
// in `openLogDetail` (user clicks row B while row A's detail fetch is in
// flight). If `selectedRow` is somehow reassigned to a different row, the
// filter in `updateOpenLogDetail` (which checks `sel.request_id !==
// row.request_id`) would let the WRONG row's updates through, causing the
// modal to be replaced by background requests — the exact "modal content
// changes to other requests while I'm debugging" bug the user reported.
//
// The pinned identity is set ONCE when the modal opens and NEVER changes
// until the modal closes. `updateOpenLogDetail` checks the incoming row
// against the PINNED identity (not `state.logs.selectedRow`), making the
// filter immune to any reassignment bugs in `selectedRow`.
// ----------
let pinnedRequestId: string | null = null;
let pinnedTraceId: string | null = null;

/** Pin the modal identity to the given request/trace pair. Called at the
 *  end of `openLogDetail` once the modal is (re)rendered. */
export function setPinnedIdentity(requestId: string, traceId: string): void {
  pinnedRequestId = requestId;
  pinnedTraceId = traceId;
}

/** Clear the pinned identity. Called when the modal is removed so
 *  subsequent WS events don't try to update a now-closed modal. */
export function clearPinnedIdentity(): void {
  pinnedRequestId = null;
  pinnedTraceId = null;
}

// Generation counter for `openLogDetail` race-condition protection. Each
// `openLogDetail` call captures the current generation; after the async
// `/usage/detail` fetch completes, the callback checks whether the generation
// is still current. If the user clicked another row in the meantime (which
// increments the generation), the stale fetch's result is discarded — it
// doesn't overwrite the modal the user is now looking at.
let openLogDetailGeneration: number = 0;

/** Increment the generation counter. Returns the new (current) generation.
 *  Call this at the START of `openLogDetail` to invalidate any in-flight
 *  fetch from a previous click. */
export function bumpOpenLogDetailGeneration(): number {
  openLogDetailGeneration += 1;
  return openLogDetailGeneration;
}

/** Returns true iff `gen` is the current generation (i.e. the caller is
 *  the most recent `openLogDetail` invocation). Call this AFTER an async
 *  await to decide whether to proceed with the result or discard it. */
export function isCurrentOpenLogDetailGeneration(gen: number): boolean {
  return gen === openLogDetailGeneration;
}

/** Returns true iff `row` matches the pinned modal identity (the row the
 *  user opened). When no modal is open (pinned identity is null), returns
 *  false so no update is applied.
 *
 *  STRICT trace_id matching: if the pinned identity has a trace_id, the
 *  incoming row MUST have the SAME trace_id. If the pinned identity has
 *  NO trace_id (empty/null), the incoming row MUST ALSO have no trace_id.
 *  This prevents a row with an empty trace_id from matching retries that
 *  have the same request_id but a non-empty trace_id (which would let
 *  sibling retry events bleed into the modal — the exact "model name
 *  changes while I'm debugging" bug). */
export function matchesPinnedModalIdentity(
  row: { request_id?: string; trace_id?: string } | null | undefined,
): boolean {
  if (pinnedRequestId === null) return false;
  if (!row) return false;
  // request_id MUST match (it's the primary identity).
  if (row.request_id !== pinnedRequestId) return false;
  // STRICT trace_id matching — no skipping. Normalize empty/null/undefined
  // to a single canonical value so "" === null === undefined.
  const pinnedTid = pinnedTraceId || "";
  const rowTid = row.trace_id || "";
  // If both trace_ids are empty, we cannot positively confirm identity.
  // Returning false here keeps the modal frozen on its snapshot rather
  // than risk overlaying data from a different request that happens to
  // share request_id with empty trace_id (HALLAZGO 6, rare in production
  // because the backend always emits trace_id, but defensive).
  if (pinnedTid === "" && rowTid === "") return false;
  if (pinnedTid !== rowTid) return false;
  return true;
}

// A row has complete detail if it carries a request body, a response
// body, or an error block. In-flight rows (only the request_id is
// known) return false so the caller can fetch the detail via
// /usage/detail. We also keep `requests[]` / `stages[]` as a fallback
// signal in case some older codepath still produces those.
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
