// components/log-detail.ts — render the per-request log detail
// modal. The modal is built as a lit-html `TemplateResult` and
// rendered into a wrapper div under `#modal-root`. Closes happen
// via `@click` handlers (lit-html) instead of `data-action`.
//
// Per spec §3 + §13.8 we do not use inline `onclick="window.X()"`
// handlers.
//
// Per SPEC_LOG_DETAIL_MODAL.md the Request and Response tabs render
// their content as friendly, collapsible sections; Errors and Raw
// keep using jsonSection.

import { html, render, type TemplateResult } from "lit-html";
import { state } from "../state/index.js";
import { showToast } from "./toast.js";

/** Loose shape for the `log` arg in renderLogDetailModal. The
 *  modal accepts the long-poll row shape (RecentUsageRow) and the
 *  detail-endpoint row shape (UsageDetailRow) — they overlap but
 *  neither is a strict superset. We model the union as an open
 *  record so the various `||` lookups in the body still typecheck
 *  without losing field-level narrowing. */
interface LogDetailLog {
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

function statusPillClass(s: string | null | undefined): string {
  if (s === "ok" || s === "success") return "ok";
  if (s === "error" || s === "failed" || s === "unhealthy") return "err";
  if (s === "timeout" || s === "rate_limited" || s === "degraded") return "warn";
  return "warn";
}

/** Pretty-print a JSON value for display inside a `<pre>` tag.
 *  lit-html auto-escapes the returned string when interpolated
 *  via `${...}`, so we no longer escape here. Returns "(empty)"
 *  for null/undefined so the viewer always has something to
 *  render. */
function formatJson(value: unknown): string {
  if (value == null) return "(empty)";
  let s: string;
  try { s = typeof value === "string" ? value : JSON.stringify(value, null, 2); }
  catch (_e: unknown) { s = String(value); }
  return s;
}

function jsonSection(title: string, value: unknown, tabKey: string): TemplateResult {
  return html`<section class="log-detail-section" data-log-tab=${tabKey}>
    <h4>${title}</h4>
    <pre class="json-viewer">${formatJson(value)}</pre>
  </section>`;
}

function readString(o: Record<string, unknown> | null | undefined, k: string): string | null {
  if (!o) return null;
  const v: unknown = o[k];
  return typeof v === "string" ? v : null;
}

// ---- SPEC_LOG_DETAIL_MODAL constants and renderers ----

/** Displayed in the Response tab when `response_body_json` is null
 *  (i.e. for requests where the response was not recorded —
 *  recording was off, or the request was cancelled before a
 *  response arrived). */
const NO_RESPONSE_PLACEHOLDER_TEXT =
  "No response body recorded.";

/** Top-level keys in the Request body that are rendered first, in
 *  this fixed order, and given the `log-detail-key-pinned` class so
 *  operators can spot them quickly. */
const PINNED_REQUEST_KEYS: readonly string[] = [
  "model", "messages", "tools", "temperature", "stream", "max_tokens",
];

/** Whitelist of LIVE fields the WS broadcast actually carries that
 *  `updateOpenLogDetail` should overlay onto the modal's snapshot.
 *  Identity/immutable fields (`id`, `request_id`, `trace_id`,
 *  `created_at`) and enriched fields (`request_body_json`,
 *  `response_body_json`, `request_headers`, `response_headers` —
 *  stripped by `redact_for_broadcast`) are intentionally NOT here.
 *  Hoisted to module scope so the Set is built once, not on every WS
 *  event. `error_message` is special-cased in the overlay loop
 *  (always overlay, even null) so the synthetic "Request in progress"
 *  message set by `openLogDetail` for inflight rows is cleared when
 *  the real row arrives with `error_message: null` (success). */
const OVERLAYABLE_FIELDS: ReadonlySet<string> = new Set<string>([
  "status_code", "total_ms", "connect_ms", "ttft_ms",
  "prompt_tokens", "completion_tokens", "cost_usd",
  "is_streaming", "stream_complete", "race_lost",
  "race_total", "race_attempts", "client_response",
  "error_message", "stop_reason",
  "compression_savings_pct", "compression_techniques",
  "provider_id", "upstream_model_id",
  "prompt_tokens_estimated", "completion_tokens_estimated",
]);

/** A single tool call extracted from an OpenAI chat-completion
 *  response's `choices[0].message.tool_calls[i]`. The `arguments`
 *  field is commonly a JSON-encoded string, hence `unknown`. */
interface ToolCall {
  id?: string;
  type?: string;
  function: { name: string; arguments: unknown };
}

/** Try to recognize an OpenAI chat-completion shape. Returns null if
 *  the value is not a recognized chat-completion (so the caller can
 *  fall through to a "Raw response" block). Only ever called with
 *  a non-string value — the string normalization happens in
 *  renderResponseTab.
 *
 *  The returned `otherProperties` field carries the response-level
 *  metadata (id, model, object, created, usage, system_fingerprint,
 *  service_tier, …) AND the choice-level metadata (index,
 *  finish_reason, logprobs) so the caller can render them in a
 *  separate collapsible "Other properties" block. This is important
 *  for responses where `content` is null and `tool_calls` is empty
 *  (e.g. a `finish_reason: "tool_calls"` response whose tool calls
 *  were emitted in a prior chunk of a streamed turn) — without
 *  surfacing `usage` and `finish_reason`, the Response tab would
 *  show only "Raw response" and the operator would have to expand
 *  it to see the request actually succeeded. */
function parseOpenAiChatResponse(value: unknown): {
  message: string | null;
  reasoning: string | null;
  toolCalls: ToolCall[];
  otherProperties: Record<string, unknown> | null;
} | null {
  if (value == null) return null;
  if (typeof value !== "object" || Array.isArray(value)) return null;
  const v = value as Record<string, unknown>;
  if (!Array.isArray(v["choices"]) || v["choices"].length < 1) return null;
  const firstChoice = v["choices"][0];
  if (firstChoice == null || typeof firstChoice !== "object" || Array.isArray(firstChoice)) return null;
  const choice = firstChoice as Record<string, unknown>;
  const messageObj = choice["message"] ?? choice["delta"] ?? null;
  const choiceMessage: Record<string, unknown> | null =
    messageObj != null && typeof messageObj === "object" && !Array.isArray(messageObj)
      ? (messageObj as Record<string, unknown>)
      : null;

  // Message: read from `message.content`, falling back to `text`, then
  // to `delta.content` (for partially-accumulated streaming responses
  // where finish() may have left content as null). Empty-string
  // content is valid (the assistant chose not to reply) — treat it
  // as non-null so the formatted blocks render instead of the raw
  // fallback.
  let message: string | null = null;
  if (choiceMessage != null) {
    const c: unknown = choiceMessage["content"];
    if (typeof c === "string") {
      message = c;
    } else if (c == null) {
      // Try delta.content and response-level text as fallbacks.
      const d: unknown = choice["delta"];
      if (d && typeof d === "object" && !Array.isArray(d)) {
        const dc: unknown = (d as Record<string, unknown>)["content"];
        if (typeof dc === "string") message = dc;
      }
      if (message == null) {
        const t: unknown = choice["text"];
        if (typeof t === "string" && t.length > 0) message = t;
      }
    }
  } else {
    const t: unknown = choice["text"];
    if (typeof t === "string" && t.length > 0) message = t;
  }
  // Only show non-empty messages — empty string content means "the
  // model replied with no text", which is information we surface via
  // finish_reason / tool_calls, not via an empty Message block.
  if (message != null && message.length === 0) message = null;

  // Reasoning: try reasoning_content -> reasoning -> reasoning_text.
  let reasoning: string | null = null;
  if (choiceMessage != null) {
    const candidates: unknown[] = [
      choiceMessage["reasoning_content"],
      choiceMessage["reasoning"],
      choiceMessage["reasoning_text"],
    ];
    for (const candidate of candidates) {
      if (typeof candidate === "string" && candidate.length > 0) {
        reasoning = candidate;
        break;
      }
    }
  }

  // Tool calls: must be an array of objects with a function.
  const toolCalls: ToolCall[] = [];
  if (choiceMessage != null && Array.isArray(choiceMessage["tool_calls"])) {
    for (const raw of choiceMessage["tool_calls"] as unknown[]) {
      if (raw == null || typeof raw !== "object" || Array.isArray(raw)) continue;
      const tc = raw as Record<string, unknown>;
      const fn = tc["function"];
      if (fn == null || typeof fn !== "object" || Array.isArray(fn)) continue;
      const fnObj = fn as Record<string, unknown>;
      const name: unknown = fnObj["name"];
      if (typeof name !== "string") continue;
      const tcCall: ToolCall = { function: { name, arguments: fnObj["arguments"] ?? null } };
      const idStr: unknown = tc["id"];
      if (typeof idStr === "string") tcCall.id = idStr;
      const typeStr: unknown = tc["type"];
      if (typeof typeStr === "string") tcCall.type = typeStr;
      toolCalls.push(tcCall);
    }
  }

  // Collect "other properties" — everything in the response object
  // EXCEPT the structured fields we already extracted. This includes:
  //   - top-level: id, object, created, model, usage,
  //     system_fingerprint, service_tier, …
  //   - choice-level: index, finish_reason, logprobs, …
  // We surface them in a collapsible "Other properties" block so the
  // operator can see at a glance that the request completed (via
  // finish_reason) and what it cost (via usage), even when content
  // and tool_calls are both empty.
  const otherProperties: Record<string, unknown> = {};
  // Top-level keys except `choices` (which we render structurally).
  for (const [k, val] of Object.entries(v)) {
    if (k === "choices") continue;
    if (val == null) continue;
    // Skip empty objects / empty strings — they add noise without
    // adding signal.
    if (typeof val === "string" && val.length === 0) continue;
    if (typeof val === "object" && val !== null && !Array.isArray(val)
        && Object.keys(val as object).length === 0) continue;
    otherProperties[k] = val;
  }
  // Choice-level keys except `message` / `delta` (rendered
  // structurally) and `text` (folded into `message` above).
  for (const [k, val] of Object.entries(choice)) {
    if (k === "message" || k === "delta" || k === "text") continue;
    if (val == null) continue;
    if (typeof val === "string" && val.length === 0) continue;
    if (typeof val === "object" && val !== null && !Array.isArray(val)
        && Object.keys(val as object).length === 0) continue;
    // Namespaced so they don't collide with top-level keys of the
    // same name (e.g. `index`).
    otherProperties[`choice.${k}`] = val;
  }
  const otherPropsNonNull = Object.keys(otherProperties).length > 0
    ? otherProperties
    : null;

  // If we got neither message, reasoning, tool calls, NOR other
  // properties, this is not a chat-completion we recognize — fall
  // through.
  if (message == null && reasoning == null
      && toolCalls.length === 0 && otherPropsNonNull == null) return null;
  return { message, reasoning, toolCalls, otherProperties: otherPropsNonNull };
}

/** Pretty-print a tool-call `arguments` field for display. The
 *  returned string is fed to a `<pre>` element — lit-html will
 *  auto-escape it. */
function parseToolCallArguments(args: unknown): { pretty: string } | null {
  if (args == null) return null;
  if (typeof args === "string") {
    try {
      const parsed: unknown = JSON.parse(args);
      return { pretty: JSON.stringify(parsed, null, 2) };
    } catch (_e: unknown) {
      return { pretty: args };
    }
  }
  if (Array.isArray(args) || (typeof args === "object" && args !== null)) {
    try {
      return { pretty: JSON.stringify(args, null, 2) };
    } catch (_e: unknown) {
      return { pretty: String(args) };
    }
  }
  // Primitive number/boolean.
  return { pretty: String(args) };
}

/** Render a single tool-call arguments block with structured
 *  collapsible sections when the arguments parse as an object.
 *  Returns null when there's nothing to render — the caller omits
 *  the block entirely in that case. */
function renderToolCallArgsBlock(args: unknown): TemplateResult | null {
  if (args == null) return null;
  let parsed: Record<string, unknown> | null = null;
  if (typeof args === "string") {
    try { parsed = JSON.parse(args) as Record<string, unknown>; }
    catch (_e: unknown) { /* fall through to raw */ }
  } else if (typeof args === "object" && args !== null && !Array.isArray(args)) {
    parsed = args as Record<string, unknown>;
  }
  if (parsed != null && Object.keys(parsed).length > 0) {
    return html`${Object.entries(parsed).map(([k, v]) => html`
      <details class="log-detail-collapsible" open>
        <summary><span class="log-detail-key">${k}</span></summary>
        <pre class="json-viewer log-detail-collapsible-body">${formatJson(v)}</pre>
      </details>`)}`;
  }
  // Fall back to raw pretty-print.
  const result = parseToolCallArguments(args);
  return result != null
    ? html`<pre class="json-viewer log-detail-collapsible-body">${result.pretty}</pre>`
    : null;
}

/** Render a single "Message" (content) block. Returns null when the
 *  content is null/empty — the caller omits the block entirely in
 *  that case, so a content-less response doesn't show an empty
 *  Message section. */
function renderMessageBlock(message: string): TemplateResult {
  return html`<details class="log-detail-collapsible" open>
    <summary>Message</summary>
    <pre class="json-viewer log-detail-collapsible-body">${message}</pre>
  </details>`;
}

/** Render a single "Reasoning" block. Same omit-when-empty contract
 *  as `renderMessageBlock`. */
function renderReasoningBlock(reasoning: string): TemplateResult {
  return html`<details class="log-detail-collapsible" open>
    <summary>Reasoning</summary>
    <pre class="json-viewer log-detail-collapsible-body">${reasoning}</pre>
  </details>`;
}

/** Render a single tool call as an independent collapsible block.
 *  Each tool call gets its own `<details>` at the top level of the
 *  Response tab — they are NOT nested under a parent "Tool calls"
 *  collapsible. This makes it easy to expand/collapse each one
 *  independently and keeps the visible height of the tab low when
 *  there are many tool calls.
 *
 *  `index` is the 0-based position in the tool_calls array, used
 *  only to label the summary ("Tool call #1", "Tool call #2", …)
 *  so the operator can correlate with the upstream's index field. */
function renderToolCallBlock(tc: ToolCall, index: number): TemplateResult {
  const idTag: TemplateResult | null = tc.id != null
    ? html` <span class="log-detail-key-meta">${tc.id}</span>`
    : null;
  const typeTag: TemplateResult | null = (tc.type != null && tc.type !== "function")
    ? html` <span class="log-detail-key-meta">${tc.type}</span>`
    : null;
  const argsHtml = renderToolCallArgsBlock(tc.function.arguments);
  if (argsHtml != null) {
    return html`<details class="log-detail-collapsible">
      <summary><span class="log-detail-tool-call-name">Tool call #${index + 1}: ${tc.function.name}</span>${idTag}${typeTag}</summary>
      ${argsHtml}
    </details>`;
  }
  // No arguments to show — render a non-collapsible header so the
  // tool call is still visible (its existence is information the
  // operator needs).
  return html`<div class="log-detail-tool-call">
    <div class="log-detail-tool-call-header">
      <span class="log-detail-tool-call-name">Tool call #${index + 1}: ${tc.function.name}</span>${idTag}${typeTag}
    </div>
  </div>`;
}

/** Render the "Other properties" block: every response-level and
 *  choice-level field that isn't part of the structured content /
 *  reasoning / tool_calls extraction (e.g. `id`, `model`, `object`,
 *  `created`, `usage`, `system_fingerprint`, `service_tier`,
 *  `choice.finish_reason`, `choice.index`, `choice.logprobs`).
 *
 *  Collapsed by default — these fields are useful for debugging
 *  but not the primary thing the operator wants to see.
 *  Returns null when `props` is empty. */
function renderOtherPropertiesBlock(props: Record<string, unknown>): TemplateResult | null {
  const entries = Object.entries(props);
  if (entries.length === 0) return null;
  const parts: TemplateResult[] = entries.map(([k, v]) => html`
    <details class="log-detail-collapsible">
      <summary><span class="log-detail-key">${k}</span></summary>
      <pre class="json-viewer log-detail-collapsible-body">${formatJson(v)}</pre>
    </details>`);
  return html`<details class="log-detail-collapsible">
    <summary>Other properties (${entries.length})</summary>
    <div class="log-detail-tool-calls">${parts}</div>
  </details>`;
}

/** Render the "Raw response" block. ALWAYS collapsed by default —
 *  it's the escape hatch for "the structured blocks above didn't
 *  show me what I needed, let me see the raw JSON". */
function renderRawResponseBlock(response: unknown): TemplateResult {
  return html`<details class="log-detail-collapsible">
    <summary>Raw response</summary>
    <pre class="json-viewer log-detail-collapsible-body">${formatJson(response)}</pre>
  </details>`;
}

/** Render the Response tab. Handles null, string, and object inputs.
 *
 *  Layout (top to bottom):
 *    1. Message (content) — only if non-empty
 *    2. Reasoning — only if non-empty
 *    3. Each tool call as its own collapsible block (collapsed by
 *       default), one per tool call
 *    4. Other properties (id, model, usage, finish_reason, …) —
 *       collapsed by default
 *    5. Raw response — collapsed by default, ALWAYS present
 *
 *  Each section is independent: a response with content + tool_calls
 *  shows all three; a response with only tool_calls shows just the
 *  tool calls + other properties + raw; a response with empty
 *  content and no tool_calls (e.g. a `finish_reason: "tool_calls"`
 *  response whose tool_calls were emitted in a prior streamed chunk)
 *  still shows other properties + raw so the operator can see the
 *  request actually succeeded.
 *
 *  @param streamingHint - set to true when the request is streaming
 *        but the response body is null (e.g. interrupted mid-stream).
 *  @param isPartial - When true, the response was interrupted
 *        mid-stream — show a "Partial response" banner so the
 *        operator knows the response didn't complete normally even
 *        though there IS a body to inspect. Passed from the caller
 *        which reads `is_streaming && !stream_complete` (and the
 *        `partial` marker inside the JSON, when present). */
function renderResponseTab(
  response: unknown,
  streamingHint?: boolean,
  createdAt?: string,
  isPartial?: boolean,
): TemplateResult {
  // Empty state.
  if (response == null) {
    let placeholder = streamingHint
      ? "Response body not captured (streaming request may have been interrupted)."
      : NO_RESPONSE_PLACEHOLDER_TEXT;
    // Bug fix: same expiry hint as the request tab — if the row is
    // older than the recording TTL, the response body was pruned.
    if (createdAt != null && createdAt !== "—") {
      const created = new Date(createdAt).getTime();
      if (!Number.isNaN(created)) {
        const ageSec = (Date.now() - created) / 1000;
        if (ageSec > 300) {
          placeholder += ` This log is ${Math.round(ageSec / 60)} min old — response bodies are pruned after the recording TTL (5 min default).`;
        }
      }
    }
    return html`<section class="log-detail-section" data-log-tab="response">
      <h4>Response</h4>
      <p class="muted log-detail-placeholder">${placeholder}</p>
    </section>`;
  }

  // Detect the `partial` marker inside the JSON itself (set by the
  // backend's `ResponseAccumulator::mark_partial()`). This is the
  // authoritative signal — even if the caller didn't pass
  // `isPartial`, we can detect it from the response body.
  let partialFromJson = false;
  if (typeof response === "object" && response !== null && !Array.isArray(response)) {
    const choices = (response as Record<string, unknown>)["choices"];
    if (Array.isArray(choices) && choices.length > 0) {
      const c0 = choices[0] as Record<string, unknown> | undefined;
      if (c0 && typeof c0 === "object") {
        const msg = (c0["message"] ?? c0["delta"]) as Record<string, unknown> | undefined;
        if (msg && typeof msg === "object") {
          const p: unknown = msg["partial"];
          if (p === true) partialFromJson = true;
        }
      }
    }
  }
  const showPartialBanner = isPartial === true || partialFromJson;
  const partialBanner: TemplateResult | null = showPartialBanner
    ? html`<div class="log-detail-partial-banner">⚠ Partial response — stream was interrupted before completion. The content below is what was received up to the point of failure.</div>`
    : null;

  // String: try to parse as JSON; on success, recurse with parsed
  // value; on failure, show the raw string in a collapsible.
  if (typeof response === "string") {
    let parsed: unknown = null;
    let parsedOk = false;
    try { parsed = JSON.parse(response); parsedOk = true; }
    catch (_e: unknown) { parsedOk = false; }
    if (parsedOk && parsed != null && typeof parsed === "object") {
      return renderResponseTab(parsed, streamingHint, createdAt, isPartial);
    }
    return html`<section class="log-detail-section" data-log-tab="response">
      <h4>Response</h4>
      ${partialBanner}
      ${renderRawResponseBlock(response)}
    </section>`;
  }

  // Try to recognize an OpenAI chat-completion shape.
  const parsed = parseOpenAiChatResponse(response);
  if (parsed != null) {
    const blocks: TemplateResult[] = [];
    // Message (content) — only if non-empty.
    if (parsed.message != null) {
      blocks.push(renderMessageBlock(parsed.message));
    }
    // Reasoning — only if non-empty.
    if (parsed.reasoning != null) {
      blocks.push(renderReasoningBlock(parsed.reasoning));
    }
    // Each tool call as its own top-level collapsible (collapsed by
    // default). Previously these were nested under a parent
    // "Tool calls (N)" collapsible, which forced the operator to
    // expand two levels to see any tool call's arguments.
    for (let i = 0; i < parsed.toolCalls.length; i++) {
      const tc = parsed.toolCalls[i];
      if (tc != null) blocks.push(renderToolCallBlock(tc, i));
    }
    // Other properties (id, model, usage, finish_reason, …).
    if (parsed.otherProperties != null) {
      const otherBlock = renderOtherPropertiesBlock(parsed.otherProperties);
      if (otherBlock != null) blocks.push(otherBlock);
    }
    // Raw response — always present, collapsed by default. The
    // operator can expand it to see the original JSON when the
    // structured extraction missed something.
    blocks.push(renderRawResponseBlock(response));
    return html`<section class="log-detail-section" data-log-tab="response">
      <h4>Response</h4>
      ${partialBanner}
      ${blocks}
    </section>`;
  }

  // Fallback: not a recognized chat-completion shape. Try to
  // extract content / reasoning / tool_calls from the raw object
  // via the same logic as parseOpenAiChatResponse but without the
  // strict shape check, then always show the raw response at the
  // end.
  let rawContent: string | null = null;
  let rawReasoning: string | null = null;
  let rawToolCalls: ToolCall[] = [];
  let rawOtherProperties: Record<string, unknown> | null = null;
  if (typeof response === "object" && response !== null && !Array.isArray(response)) {
    const obj = response as Record<string, unknown>;
    // Try choices[0].message.content or delta.content at top level.
    const choices: unknown = obj["choices"];
    if (Array.isArray(choices) && choices.length > 0) {
      const c0 = choices[0] as Record<string, unknown> | undefined;
      if (c0 && typeof c0 === "object") {
        const msg = (c0["message"] ?? c0["delta"]) as Record<string, unknown> | undefined;
        if (msg && typeof msg === "object") {
          const content: unknown = msg["content"];
          if (typeof content === "string" && content.length > 0) rawContent = content;
          const rc: unknown = msg["reasoning_content"];
          if (typeof rc === "string" && rc.length > 0) rawReasoning = rc;
          else {
            const r: unknown = msg["reasoning"];
            if (typeof r === "string" && r.length > 0) rawReasoning = r;
          }
          // Extract tool calls from the message/delta object.
          if (Array.isArray(msg["tool_calls"])) {
            for (const raw of msg["tool_calls"] as unknown[]) {
              if (raw == null || typeof raw !== "object" || Array.isArray(raw)) continue;
              const tc = raw as Record<string, unknown>;
              const fn = tc["function"];
              if (fn == null || typeof fn !== "object" || Array.isArray(fn)) continue;
              const fnObj = fn as Record<string, unknown>;
              const name: unknown = fnObj["name"];
              if (typeof name !== "string") continue;
              const tcCall: ToolCall = { function: { name, arguments: fnObj["arguments"] ?? null } };
              const idStr: unknown = tc["id"];
              if (typeof idStr === "string") tcCall.id = idStr;
              const typeStr: unknown = tc["type"];
              if (typeof typeStr === "string") tcCall.type = typeStr;
              rawToolCalls.push(tcCall);
            }
          }
        }
        // Collect choice-level "other properties" (finish_reason, etc.)
        const choiceProps: Record<string, unknown> = {};
        for (const [k, val] of Object.entries(c0)) {
          if (k === "message" || k === "delta" || k === "text") continue;
          if (val == null) continue;
          if (typeof val === "string" && val.length === 0) continue;
          if (typeof val === "object" && val !== null && !Array.isArray(val)
              && Object.keys(val as object).length === 0) continue;
          choiceProps[`choice.${k}`] = val;
        }
        if (Object.keys(choiceProps).length > 0) {
          rawOtherProperties = { ...(rawOtherProperties ?? {}), ...choiceProps };
        }
      }
    }
    // Collect top-level "other properties" (id, model, usage, etc.)
    const topLevelProps: Record<string, unknown> = {};
    for (const [k, val] of Object.entries(obj)) {
      if (k === "choices") continue;
      if (val == null) continue;
      if (typeof val === "string" && val.length === 0) continue;
      if (typeof val === "object" && val !== null && !Array.isArray(val)
          && Object.keys(val as object).length === 0) continue;
      topLevelProps[k] = val;
    }
    if (Object.keys(topLevelProps).length > 0) {
      rawOtherProperties = { ...(rawOtherProperties ?? {}), ...topLevelProps };
    }
  }
  const blocks: TemplateResult[] = [];
  if (rawContent != null) blocks.push(renderMessageBlock(rawContent));
  if (rawReasoning != null) blocks.push(renderReasoningBlock(rawReasoning));
  for (let i = 0; i < rawToolCalls.length; i++) {
    const tc = rawToolCalls[i];
    if (tc != null) blocks.push(renderToolCallBlock(tc, i));
  }
  if (rawOtherProperties != null && Object.keys(rawOtherProperties).length > 0) {
    const otherBlock = renderOtherPropertiesBlock(rawOtherProperties);
    if (otherBlock != null) blocks.push(otherBlock);
  }
  // Always show the raw response as the last block.
  blocks.push(renderRawResponseBlock(response));
  return html`<section class="log-detail-section" data-log-tab="response">
    <h4>Response</h4>
    ${partialBanner}
    ${blocks}
  </section>`;
}

/** Render the Request tab. Returns the full
 *  `<section data-log-tab="request">…</section>` TemplateResult.
 *  Handles the empty / object / non-object fallback shapes per
 *  the spec.
 *
 *  `createdAt` is the row's `created_at` timestamp (ISO string).
 *  When the request body is null, we use it to compute the row's
 *  age and show a more helpful message: if the row is older than
 *  the recording TTL (5 min default), the body was likely pruned
 *  by `prune_expired_recording_bodies`; otherwise recording was
 *  OFF when the request was made. */
function renderRequestTab(requestBody: unknown, createdAt?: string): TemplateResult {
  // Empty state: same predicate as the previous inline logic.
  const hasRequestBody: boolean = requestBody != null
    && !(typeof requestBody === "string" && requestBody.trim() === "")
    && !(typeof requestBody === "object" && requestBody !== null
         && !Array.isArray(requestBody) && Object.keys(requestBody as object).length === 0
         && JSON.stringify(requestBody) === "{}");
  if (!hasRequestBody) {
    // Bug fix: distinguish "never recorded" from "expired by TTL".
    // The recording TTL default is 300s (5 min). If the row is
    // older than that, the body was pruned by the background
    // sweep; the operator should know this so they don't think
    // the dashboard is broken.
    let expiryHint = "";
    if (createdAt != null && createdAt !== "—") {
      const created = new Date(createdAt).getTime();
      if (!Number.isNaN(created)) {
        const ageSec = (Date.now() - created) / 1000;
        if (ageSec > 300) {
          expiryHint = ` This log is ${Math.round(ageSec / 60)} min old — request/response bodies are pruned after the recording TTL (5 min default) to bound DB growth. Re-run the request with recording ON to capture a fresh copy.`;
        }
      }
    }
    return html`<section class="log-detail-section" data-log-tab="request">
      <h4>Request</h4>
      <p class="muted">No request body recorded.${expiryHint}</p>
    </section>`;
  }

  // Normalize string bodies that look like JSON.
  let body: unknown = requestBody;
  if (typeof body === "string") {
    const trimmed = body.trimStart();
    if (trimmed.startsWith("{") || trimmed.startsWith("[")) {
      try { body = JSON.parse(body); }
      catch (_e: unknown) { /* fall through with the raw string */ }
    }
  }

  // Object body (non-array): per-key collapsibles.
  if (body != null && typeof body === "object" && !Array.isArray(body)) {
    const obj = body as Record<string, unknown>;
    const rendered = new Set<string>();
    const blocks: TemplateResult[] = [];

    const isEmptyValue = (v: unknown): boolean => {
      if (v == null) return true;
      if (typeof v === "string" && v.trim() === "") return true;
      if (Array.isArray(v) && v.length === 0) return true;
      if (typeof v === "object" && !Array.isArray(v) && Object.keys(v as object).length === 0) return true;
      return false;
    };
    const isPrimitive = (v: unknown): boolean => {
      const t = typeof v;
      return v == null || t === "string" || t === "number" || t === "boolean";
    };
    const metaText = (v: unknown): string => {
      if (Array.isArray(v)) {
        return v.length === 0 ? "empty" : (v.length === 1 ? "1 item" : `${v.length} items`);
      }
      if (v != null && typeof v === "object") {
        const keys = Object.keys(v as object).length;
        return keys === 0 ? "empty" : (keys === 1 ? "1 key" : `${keys} keys`);
      }
      return "";
    };

    /** Render a single tool definition (from `tools[]`) */
    const renderToolBlock = (tool: unknown, index: number): TemplateResult => {
      if (tool == null || typeof tool !== "object" || Array.isArray(tool)) {
        return html`<details class="log-detail-collapsible">
          <summary>Tool #${index + 1}</summary>
          <pre class="json-viewer log-detail-collapsible-body">${formatJson(tool)}</pre>
        </details>`;
      }
      const t = tool as Record<string, unknown>;
      const toolType: string = typeof t["type"] === "string" ? t["type"] : "function";
      const fn = t["function"] as Record<string, unknown> | undefined;
      const name: string = fn && typeof fn["name"] === "string" ? fn["name"] : `#${index + 1}`;
      const description: unknown = fn?.["description"];
      const parameters: unknown = fn?.["parameters"];
      const strict: unknown = fn?.["strict"];
      const parts: TemplateResult[] = [];
      if (description != null && !isEmptyValue(description)) {
        parts.push(html`<details class="log-detail-collapsible">
          <summary>Description</summary>
          <pre class="json-viewer log-detail-collapsible-body">${typeof description === "string" ? description : JSON.stringify(description, null, 2)}</pre>
        </details>`);
      }
      if (parameters != null && !isEmptyValue(parameters)) {
        parts.push(html`<details class="log-detail-collapsible">
          <summary>Parameters</summary>
          <pre class="json-viewer log-detail-collapsible-body">${formatJson(parameters)}</pre>
        </details>`);
      }
      if (strict != null && !isEmptyValue(strict)) {
        parts.push(html`<details class="log-detail-collapsible">
          <summary>Strict</summary>
          <pre class="json-viewer log-detail-collapsible-body">${formatJson(strict)}</pre>
        </details>`);
      }
      const extraKeys = Object.keys(t).filter((k) => k !== "type" && k !== "function");
      for (const ek of extraKeys) {
        const ev = t[ek];
        if (!isEmptyValue(ev)) {
          parts.push(html`<details class="log-detail-collapsible">
            <summary>${ek}</summary>
            <pre class="json-viewer log-detail-collapsible-body">${formatJson(ev)}</pre>
          </details>`);
        }
      }
      return html`<details class="log-detail-collapsible" ?open=${index === 0}>
        <summary><span class="log-detail-tool-call-name">${toolType}</span> <span class="log-detail-key-meta">${name}</span></summary>
        ${parts}
      </details>`;
    };

    for (const key of PINNED_REQUEST_KEYS) {
      if (!Object.prototype.hasOwnProperty.call(obj, key)) continue;
      const value = obj[key];

      // Skip empty values (null, empty string, empty array, empty object).
      if (isEmptyValue(value)) continue;

      // Special handling for "tools": render each tool definition as
      // its own collapsible with name, description, parameters.
      if (key === "tools" && Array.isArray(value) && value.length > 0) {
        const toolBlocks: TemplateResult[] = (value as unknown[]).map((t, i) => renderToolBlock(t, i));
        blocks.push(html`<details class="log-detail-collapsible" open>
          <summary><span class="log-detail-key log-detail-key-pinned">tools</span> <span class="log-detail-key-meta">${value.length} tool(s)</span></summary>
          <div class="log-detail-messages">${toolBlocks}</div>
        </details>`);
        rendered.add(key);
        continue;
      }

      // Special handling for "messages": render each message as an
      // individual collapsible with its role in the summary.
      if (key === "messages" && Array.isArray(value) && value.length > 0) {
        const msgBlocks: TemplateResult[] = (value as unknown[]).map((raw, i) => {
          if (raw == null || typeof raw !== "object" || Array.isArray(raw)) {
            return html`<details class="log-detail-collapsible">
              <summary>Message #${i + 1}</summary>
              <pre class="json-viewer log-detail-collapsible-body">${formatJson(raw)}</pre>
            </details>`;
          }
          const msg = raw as Record<string, unknown>;
          const role: string = typeof msg["role"] === "string" ? msg["role"] : "unknown";
          const content = msg["content"];
          const toolCallId = msg["tool_call_id"];
          const toolCalls = msg["tool_calls"];
          const name = msg["name"];
          // Build a compact summary line.
          const roleClass = role === "system" ? "log-detail-role-system"
            : role === "assistant" ? "log-detail-role-assistant"
            : role === "user" ? "log-detail-role-user"
            : role === "tool" ? "log-detail-role-tool"
            : "";
          const extras: string[] = [];
          if (typeof name === "string") extras.push(name);
          if (typeof toolCallId === "string") extras.push(`tool_call_id: ${toolCallId}`);
          if (Array.isArray(toolCalls) && toolCalls.length > 0) extras.push(`${toolCalls.length} tool call(s)`);
          const extraStr: TemplateResult | null = extras.length > 0
            ? html` <span class="log-detail-key-meta">${extras.join(" · ")}</span>`
            : null;
          // Content preview: first 80 chars of the content string.
          let preview = "";
          if (typeof content === "string") {
            preview = content.length > 80 ? content.slice(0, 80) + "…" : content;
          } else if (content != null) {
            const s = typeof content === "string" ? content : JSON.stringify(content);
            preview = s.length > 80 ? s.slice(0, 80) + "…" : s;
          }
          const previewStr: TemplateResult | null = preview.length > 0
            ? html` <span class="log-detail-msg-preview">${preview}</span>`
            : null;
          return html`<details class="log-detail-collapsible">
            <summary><span class="log-detail-role ${roleClass}">${role}</span>${extraStr}${previewStr}</summary>
            <pre class="json-viewer log-detail-collapsible-body">${formatJson(msg)}</pre>
          </details>`;
        });
        blocks.push(html`<details class="log-detail-collapsible" open>
          <summary><span class="log-detail-key log-detail-key-pinned">messages</span> <span class="log-detail-key-meta">${value.length} message(s)</span></summary>
          <div class="log-detail-messages">${msgBlocks}</div>
        </details>`);
        rendered.add(key);
        continue;
      }

      const open = isPrimitive(value);
      const meta = metaText(value);
      const metaSpan: TemplateResult | null = meta.length > 0
        ? html` <span class="log-detail-key-meta">${meta}</span>`
        : null;
      blocks.push(html`<details class="log-detail-collapsible" ?open=${open}>
        <summary><span class="log-detail-key log-detail-key-pinned">${key}</span>${metaSpan}</summary>
        <pre class="json-viewer log-detail-collapsible-body">${formatJson(value)}</pre>
      </details>`);
      rendered.add(key);
    }

    // Remaining keys in original insertion order. Skip empty values
    // (null, empty string, empty array, empty object).
    for (const key of Object.keys(obj)) {
      if (rendered.has(key)) continue;
      const value = obj[key];
      if (isEmptyValue(value)) continue;
      blocks.push(html`<details class="log-detail-collapsible">
        <summary><span class="log-detail-key">${key}</span></summary>
        <pre class="json-viewer log-detail-collapsible-body">${formatJson(value)}</pre>
      </details>`);
    }

    return html`<section class="log-detail-section" data-log-tab="request">
      <h4>Request</h4>
      ${blocks}
    </section>`;
  }

  // Array or primitive fallback.
  return html`<section class="log-detail-section" data-log-tab="request">
    <h4>Request</h4>
    <details class="log-detail-collapsible" open>
      <summary>Raw request body</summary>
      <pre class="json-viewer log-detail-collapsible-body">${formatJson(body)}</pre>
    </details>
  </section>`;
}

export function renderLogDetailModal(log: LogDetailLog): TemplateResult {
  // Normalize the row shape: the backend's /usage/detail returns
  // status_code, total_ms, upstream_model_id, etc., but the modal
  // originally assumed a richer payload (status, latency_ms, model,
  // cost, requests, response, errors, meta). Map the backend's
  // canonical field names onto the modal's expected shape so a row
  // from the table — which only has the live-update shape — looks
  // the same in the modal as a row from /usage/detail.
  const detail: Record<string, unknown> = (log.detail as Record<string, unknown>) || {};
  const meta: Record<string, unknown> = (log.meta as Record<string, unknown>) || detail["meta"] as Record<string, unknown> || (log as unknown as Record<string, unknown>);
  const response: unknown = log.response ?? detail["response"] ?? log.response_body_json ?? null;
  const isStreaming: boolean = !!((log as Record<string, unknown>)["is_streaming"]);
  // A streaming request that didn't complete is "partial" — the
  // backend persisted whatever was accumulated up to the point of
  // failure. Pass this to renderResponseTab so it shows a banner.
  const streamComplete: boolean = !!((log as Record<string, unknown>)["stream_complete"]);
  const isPartial: boolean = isStreaming && !streamComplete;
  // Read from the most specific to the least specific. `log.error_message`
  // comes from the recent-rows endpoint (RecentUsageRow.error_message
  // in usage.rs); `log.error_msg` / `log.error_msg_redacted` come from
  // the detail endpoint (UsageDetailRow.error_msg in usage.rs).
  const detailErrors: unknown = (detail as Record<string, unknown>)["errors"];
  const errors: unknown = log.errors
    || log.error
    || log.error_msg
    || log.error_message
    || log.error_msg_redacted
    || log.error_message_redacted
    || detailErrors
    || null;
  // The backend's UsageDetailRow has a flat shape: it exposes
  // `request_body_json` (a serde_json::Value, already parsed) instead of
  // the older `requests[]` / `stages[]` arrays, which the UsageDetailRow
  // struct never had. We display the request body as a pretty JSON viewer.
  const requestBody: unknown = log.request_body_json != null
    ? log.request_body_json
    : (detail["request_body_json"] != null ? detail["request_body_json"] : null);
  const provider: string = log.provider_id || (readString(meta, "provider_id") ?? "—");
  const account: string | number | null = log.account_id != null ? log.account_id : meta["account_id"] != null ? (meta["account_id"] as string | number) : "—";
  const comboRaw: unknown = log.combo_id ?? meta["combo_id"];
  const combo: string | number | null = comboRaw != null && (typeof comboRaw === "string" || typeof comboRaw === "number") ? comboRaw : null;
  const model: string = log.model_id || log.upstream_model || log.upstream_model_id || (readString(meta, "model_id") ?? "—");
  const latency: string = log.latency_ms != null ? `${log.latency_ms} ms`
    : (log.total_ms != null ? `${log.total_ms} ms`
    : (log.elapsed_ms != null ? `${log.elapsed_ms} ms` : "—"));
  const costRaw: number | null = log.cost != null ? log.cost
    : (log.usage && log.usage.cost != null ? log.usage.cost
    : (log.cost_usd != null ? log.cost_usd : null));
  const status: string = log.status || (log.status_code != null ? String(log.status_code) : "—");
  const statusClass: string = statusPillClass(
    log.status_code != null
      ? (log.status_code >= 200 && log.status_code < 300 ? "ok" : (log.status_code >= 400 ? "error" : "warn"))
      : (log.status || "warn")
  );
  const requestId: string | number = log.request_id || log.id || "—";
  const createdAt: string = log.created_at || log.timestamp || "—";
  const apiKeyIdRaw: unknown = log.api_key_id ?? meta["api_key_id"];
  const apiKeyId: string | number | null = apiKeyIdRaw != null && (typeof apiKeyIdRaw === "string" || typeof apiKeyIdRaw === "number") ? apiKeyIdRaw : null;
  const userAgent: string | null = log.user_agent || readString(meta, "user_agent");

  const apiKeyIdBlock: TemplateResult | null = apiKeyId != null
    ? html`<div><strong>API key:</strong> #${String(apiKeyId)}</div>`
    : null;
  const userAgentBlock: TemplateResult | null = userAgent
    ? html`<div><strong>User-Agent:</strong> ${String(userAgent)}</div>`
    : null;
  const comboText: string = combo != null ? String(combo) : "—";
  const costText: string = costRaw != null ? String(costRaw) : "—";

  return html`
    <div id="log-detail-modal" class="modal-bg log-detail-modal" @click=${(e: Event) => closeLogDetailModal(e)}>
      <div class="modal">
        <div class="modal-header">
          <h2>Log #${String(requestId)}</h2>
          <button type="button" class="log-detail-copy-bundle-btn" @click=${() => { void copyDebugBundle(); }} title="Copy a Markdown-formatted debug bundle with all request/response/error context — ready to paste into a bug report.">📋 Copy debug bundle</button>
          <button type="button" class="close-btn" @click=${(e: Event) => closeLogDetailModal(e)} aria-label="Close">&times;</button>
        </div>
        <div class="modal-body">
          <div class="log-detail-summary">
            <div><strong>Status:</strong> <span class="status-pill ${statusClass}">${String(status)}</span></div>
            <div><strong>Provider:</strong> ${String(provider)}</div>
            <div><strong>Account:</strong> ${String(account)}</div>
            <div><strong>Combo:</strong> ${comboText}</div>
            <div><strong>Model:</strong> ${String(model)}</div>
            <div><strong>Latency:</strong> ${latency}</div>
            <div><strong>Prompt tokens:</strong> ${log.prompt_tokens_estimated ? "≈" : ""}${log.prompt_tokens ?? "—"}</div>
            <div><strong>Completion tokens:</strong> ${log.completion_tokens_estimated ? "≈" : ""}${log.completion_tokens ?? "—"}</div>
            <div><strong>Tokens/sec:</strong> ${log.tokens_per_sec != null ? log.tokens_per_sec.toFixed(1) : "—"}</div>
            <div><strong>Cost:</strong> ${costText}</div>
            ${renderCompressionSummary(log)}
            <div><strong>Created:</strong> ${String(createdAt)}</div>
            ${apiKeyIdBlock}
            ${userAgentBlock}
          </div>
          <div class="log-detail-tabs">
            <button class="detail-tab" data-arg1="request" @click=${(e: Event) => logDetailTabClick("request", e)}>Request</button>
            <button class="detail-tab" data-arg1="response" @click=${(e: Event) => logDetailTabClick("response", e)}>Response</button>
            <button class="detail-tab" data-arg1="errors" @click=${(e: Event) => logDetailTabClick("errors", e)}>Errors</button>
            <button class="detail-tab" data-arg1="raw" @click=${(e: Event) => logDetailTabClick("raw", e)}>Raw</button>
          </div>
          <div class="log-detail-content" id="log-detail-content">
            ${renderRequestTab(requestBody, createdAt)}
            ${renderResponseTab(response, isStreaming, createdAt, isPartial)}
            ${errors != null
              ? jsonSection("Errors", errors, "errors")
              : html`<section class="log-detail-section" data-log-tab="errors">
                   <h4>Errors</h4>
                   <p class="muted">No errors recorded.</p>
                 </section>`}
            ${jsonSection("Raw log", log, "raw")}
          </div>
        </div>
      </div>
    </div>
  `;
}

/** Click handler for the `.detail-tab` buttons. Toggles which
 *  `#log-detail-content [data-log-tab]` section is visible (mutually
 *  exclusive) AND marks the clicked button as `.active`.
 *
 *  Lit-html's `@click` wiring means we no longer need a separate
 *  document-level listener (`tabClickOnce` / `wireTabClickOnce` in
 *  the pre-migration code). */
function logDetailTabClick(which: string, e: Event): void {
  // Update section visibility.
  document.querySelectorAll("#log-detail-content [data-log-tab]").forEach((sec) => {
    const el = sec as HTMLElement;
    el.style.display = (sec.getAttribute("data-log-tab") === which) ? "" : "none";
  });
  // Update the active-tab indicator on the clicked button.
  const btn = e.currentTarget;
  if (btn instanceof Element) {
    const tabsContainer = btn.closest(".log-detail-tabs");
    if (tabsContainer != null) {
      tabsContainer.querySelectorAll(".detail-tab").forEach((t) => {
        t.classList.toggle("active", t === btn);
      });
    }
  }
}

// Initialize the log-detail tab UI: show only the first [data-log-tab]
// section, hide the remaining ones, and mark the first detail-tab as
// active. Centralized here so it can be re-invoked after any in-place
// re-render (e.g. updateOpenLogDetail re-rendering the modal).
function initializeLogDetailTabs(): void {
  const modal: HTMLElement | null = document.getElementById("log-detail-modal");
  if (!modal) return;
  const sections: NodeListOf<Element> = modal.querySelectorAll("[data-log-tab]");
  if (sections.length === 0) return;
  // Hide all but the first; mark first as active.
  sections.forEach((s, i) => {
    const sEl: HTMLElement = s as HTMLElement;
    sEl.style.display = i === 0 ? "" : "none";
  });
  // BUG-INIT-1: the rendered tab buttons are `.detail-tab` inside
  // `.log-detail-tabs` (data-action="logDetailTab"), not anything
  // marked with `data-log-detail-tab`. Pick the first `.detail-tab`
  // child of the tabs container.
  const tabsContainer: HTMLElement | null = modal.querySelector(".log-detail-tabs");
  if (tabsContainer != null) {
    tabsContainer.querySelectorAll(".detail-tab").forEach((t) => t.classList.remove("active"));
    const firstTab: HTMLElement | null = tabsContainer.querySelector(".detail-tab");
    if (firstTab) firstTab.classList.add("active");
  }
}

/** Get or create the `#modal-root` container that holds all
 *  dashboard modals. Lives at `<body>` level so a re-render of
 *  `#main` doesn't destroy the modal. */
function ensureModalRoot(): HTMLElement {
  let root = document.getElementById("modal-root");
  if (!root) {
    root = document.createElement("div");
    root.id = "modal-root";
    // z-index 1000 puts modals above the page chrome without
    // needing !important hacks. The .modal-bg rule in CSS already
    // uses position: fixed; this just ensures stacking order.
    root.style.cssText = "position:relative;z-index:1000;";
    document.body.appendChild(root);
  }
  return root;
}

/** Remove a `.log-detail-modal` element AND its wrapper parent (the
 *  empty `<div>` we created in `showLogDetail` to host the rendered
 *  TemplateResult). Keeps `#modal-root` clean so the next modal
 *  opens in a fresh wrapper. */
function removeLogDetailModal(m: HTMLElement): void {
  const wrapper = m.parentElement;
  m.remove();
  if (wrapper && wrapper.children.length === 0 && wrapper.parentElement?.id === "modal-root") {
    wrapper.remove();
  }
  // Clear the pinned identity so subsequent WS events don't try to
  // update a now-closed modal. Without this, `updateOpenLogDetail`
  // would see no `.log-detail-modal` in the DOM and bail early
  // anyway, but clearing the pin is belt-and-suspenders and makes
  // the lifecycle explicit.
  pinnedRequestId = null;
  pinnedTraceId = null;
  // Clear the snapshot so copyDebugBundle and other readers don't see
  // stale data after the modal is closed (HALLAZGO 1).
  state.logs.selectedRow = null;
}

// ----------------------------------------------------------------------------
// Pinned modal identity — the IMMUTABLE request_id + trace_id of the row the
// user opened. Set in `showLogDetail`, cleared in `removeLogDetailModal`.
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
// ----------------------------------------------------------------------------
let pinnedRequestId: string | null = null;
let pinnedTraceId: string | null = null;

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

// ----------------------------------------------------------------------------
// SNAPSHOT HELPER — creates a shallow copy of a row so the modal's data
// is DECOUPLED from the live row objects in `state.logs.rows` /
// `state.logs.inflightByTraceId`.
//
// WHY: `state.logs.rows` and `state.logs.inflightByTraceId` contain LIVE
// row objects that can be mutated in place by:
//   - `handleStageEvent` (inflight placeholders: `existing.upstream_model_id = ...`)
//   - `mergeLogsByDescId` (creates new objects via spread, but the OLD
//     objects may still be referenced by `selectedRow`)
//
// If `state.logs.selectedRow` points to a LIVE row object, any in-place
// mutation is visible to the modal (via `copyDebugBundle` reading
// `selectedRow`, or via `updateOpenLogDetail` merging into `selectedRow`).
// This is the root cause of the "model name and latency change while I'm
// debugging" bug — the modal's data source was a LIVE reference to an
// inflight placeholder that was being mutated by stage events.
//
// The fix: every time we set `selectedRow`, we create a SNAPSHOT (shallow
// copy). The snapshot is a NEW object — mutations to the original row
// don't affect it. `updateOpenLogDetail` merges into the snapshot, creating
// a NEW snapshot each time, so the modal's data is always a stable point-
// in-time view of the row the user opened.
// ----------------------------------------------------------------------------
function snapshotRow<T>(row: T): T {
  if (row == null || typeof row !== "object") return row;
  // Deep-clone so nested objects (request_body_json, response_body_json,
  // request_headers, response_headers) are disconnected from any live
  // row reference. Shallow copy left nested objects as SHARED references,
  // which is fragile if any future code path mutates them in-place
  // (HALLAZGO 4 — no active bug today, but defensive hardening).
  return JSON.parse(JSON.stringify(row));
}

// Public API
export function showLogDetail(log: LogDetailLog): void {
  // BUG-STACK-1: if showLogDetail is called twice without an
  // intervening closeLogDetailModal, two .log-detail-modal elements
  // would stack on top of each other and confuse event delegation.
  // Remove any pre-existing modal before appending the new one.
  const existing = document.querySelector(".log-detail-modal");
  if (existing instanceof HTMLElement) {
    removeLogDetailModal(existing);
  }
  // CRITICAL: set state.logs.selectedRow to a SNAPSHOT of `log`, NOT a
  // reference to the live row object. The snapshot is a NEW object —
  // mutations to the original row (e.g. inflight placeholders being
  // updated by stage events) don't affect the modal's data. This is the
  // core fix for the "modal content changes while I'm debugging" bug.
  state.logs.selectedRow = snapshotRow(log) as unknown as (typeof state.logs)["selectedRow"];
  // PIN the identity of the row the user opened. This is the IMMUTABLE
  // source of truth for `updateOpenLogDetail`'s filter — once pinned,
  // only events for THIS row (same request_id, same trace_id) can
  // update the modal. Background requests with different request_ids
  // are rejected by the filter, even if `state.logs.selectedRow` is
  // somehow reassigned by a race condition.
  pinnedRequestId = log.request_id ?? null;
  pinnedTraceId = log.trace_id ?? null;
  // Debug: log the pinned identity so we can verify the filter is correct.
  console.debug(
    "[openproxy log-detail] showLogDetail: pinned request_id=%s trace_id=%s",
    pinnedRequestId, pinnedTraceId,
  );
  const root = ensureModalRoot();
  // Render the modal into a fresh wrapper div so lit-html can
  // diff efficiently on updateOpenLogDetail re-renders.
  const wrapper = document.createElement("div");
  root.appendChild(wrapper);
  render(renderLogDetailModal(state.logs.selectedRow as unknown as LogDetailLog), wrapper);
  initializeLogDetailTabs();
}

export function closeLogDetailModal(e: Event | null): void {
  // Close only if the click was on the backdrop itself or on the
  // explicit X button. With lit-html's `@click` wiring, the handler
  // is bound to BOTH the backdrop and the close button — we use
  // `e.target === closest('.log-detail-modal')` (strict identity,
  // so clicks on descendants like the <pre> text body or the JSON
  // viewer don't bubble up and close the modal) and
  // `closest('.close-btn')` to detect the two valid close origins.
  if (!e || !e.target) return;
  const target: EventTarget = e.target;
  if (!(target instanceof Element)) return;
  const m: HTMLElement | null = target.closest(".log-detail-modal");
  if (!m) return;
  // Case 1: click was directly on the backdrop (the wrapper itself).
  // Use `target === m` (strict identity) so clicks on descendants
  // like the <pre> text body or the JSON viewer don't bubble up and
  // close the modal — only an actual click on the empty wrapper area
  // should close it.
  if (target === m) { removeLogDetailModal(m); return; }
  // Case 2: click was on the explicit X close button in the header.
  const closeBtn: HTMLElement | null = target.closest(".close-btn");
  if (closeBtn && m.contains(closeBtn)) {
    removeLogDetailModal(m); return;
  }
  // Case 3: click was inside .modal on something else (tabs, content,
  // summary, etc.) with a different click handler — do nothing; the
  // other handler (e.g. logDetailTabClick) already handled the click.
}

/// Render the compression savings line for the log detail summary.
///
/// Header-only: shows the percentage prominently. Long technique
/// lists can overflow narrow modals, so the techniques string is
/// moved into the `title` attribute (hover tooltip) instead of
/// being rendered as visible text. The Raw tab keeps the
/// techniques visible because they're useful there.
/// Returns null when compression is off (so the summary line is
/// omitted entirely).
function renderCompressionSummary(log: LogDetailLog): TemplateResult | null {
  const pct = log.compression_savings_pct ?? null;
  if (pct == null || pct <= 0) return null;
  const tech = log.compression_techniques ?? "";
  const pctRounded = Math.round(pct);
  const pctText = `-${pctRounded}% tok`;
  const tooltip = `Token savings: ${pctRounded}% (BPE cl100k_base)${tech.length > 0 ? " — " + tech : ""}`;
  return html`<div><strong>Compression:</strong> <span title=${tooltip}>${pctText}</span></div>`;
}

// Update the open log-detail modal with a new row (called from the
// WebSocket `row`/`stage` event handlers when the user has a row
// detail modal open and a new event for that request arrives).
// If no modal is open this is a no-op.
//
// CRITICAL: this function uses the PINNED IDENTITY (request_id + trace_id
// captured in `showLogDetail`) to decide whether the incoming row belongs
// to the open modal — NOT `state.logs.selectedRow`. The pinned identity is
// immutable for the lifetime of the modal, so background requests with a
// different request_id are ALWAYS rejected, even if `selectedRow` is
// somehow reassigned by a race condition. This is the fix for the
// "modal content is replaced by other entries while I'm debugging" bug.
export function updateOpenLogDetail(row: LogDetailLog | null | undefined): void {
  if (!row) return;
  const modal: HTMLElement | null = document.querySelector(".log-detail-modal");
  if (!modal) return;
  // The modal lives inside a wrapper div we created in showLogDetail.
  // Re-render into the wrapper to let lit-html diff efficiently.
  const wrapper = modal.parentElement;
  if (!wrapper) return;
  // PINNED IDENTITY CHECK: reject any row that doesn't match the
  // request_id AND trace_id of the row the user opened. This is the
  // immutable source of truth — `state.logs.selectedRow` is NOT consulted
  // for the filter because it can be reassigned by race conditions.
  //
  // DEBUG: log rejected updates so we can verify the filter is working.
  // If the user reports the modal is still changing, these logs will
  // show exactly which rows are being rejected and why.
  if (!matchesPinnedModalIdentity(row as { request_id?: string; trace_id?: string })) {
    console.debug(
      "[openproxy log-detail] updateOpenLogDetail REJECTED: incoming request_id=%s trace_id=%s — pinned request_id=%s trace_id=%s",
      (row as { request_id?: string }).request_id,
      (row as { trace_id?: string }).trace_id,
      pinnedRequestId, pinnedTraceId,
    );
    return;
  }
  const sel = state.logs.selectedRow as unknown as
    | (LogDetailLog & { request_id?: string; trace_id?: string })
    | null;
  // CRITICAL FIX: if `selectedRow` doesn't match the pinned identity
  // (e.g. because `openLogDetail` reassigned it to a NEW row during an
  // async fetch), DO NOT fall back to using the incoming `row` as the
  // merge base. The incoming `row` from a WS event is NOT enriched —
  // it lacks `request_body_json` / `response_body_json` (stripped by
  // `redact_for_broadcast`). Using it as the base would CLOBBER the
  // enriched data we fetched via `/usage/detail`, causing the modal to
  // show "No request body recorded" — the exact bug the user reported.
  //
  // Instead, return early. The modal stays frozen at its last known
  // good snapshot. The incoming WS event's data is NOT lost — it's
  // still merged into `state.logs.rows` by `handleLogsMessage`, so the
  // TABLE shows the live data. Only the MODAL is frozen, which is the
  // desired behavior (the user is debugging a specific request).
  if (!sel || !matchesPinnedModalIdentity(sel as { request_id?: string; trace_id?: string })) {
    console.debug(
      "[openproxy log-detail] updateOpenLogDetail SKIP: selectedRow doesn't match pinned identity — modal stays frozen at last snapshot",
    );
    return;
  }
  const base: LogDetailLog = sel;
  // Create a NEW merged snapshot — never mutate the original row objects.
  // The spread `{ ...base }` creates a shallow copy, so the merge doesn't
  // mutate `base` (which is `sel` = the previous `selectedRow` snapshot).
  // Only overlay NON-NULL fields from the incoming `row` — this preserves
  // enriched fields (request_body_json, etc.) that the WS event doesn't
  // carry, while still updating live fields (status_code, total_ms, etc.).
  const merged: Record<string, unknown> = { ...base } as Record<string, unknown>;
  // Overlay only LIVE fields the WS event actually carries. Skip
  // identity/immutable fields (id, request_id, trace_id, created_at)
  // and enriched fields (request_body_json, response_body_json,
  // request_headers, response_headers) that the broadcast redacts.
  // Special-case error_message: always overlay (even null) so the
  // synthetic "Request in progress" message set by openLogDetail
  // for inflight rows is cleared when the real row arrives with
  // error_message: null (success). Without this, the modal shows
  // status=200 but error="Request in progress — current stage:
  // started" — contradictory data (the user-reported "modal se
  // bugea" bug, HALLAZGO 5).
  for (const [k, v] of Object.entries(row as Record<string, unknown>)) {
    if (!OVERLAYABLE_FIELDS.has(k)) continue;
    if (k === "error_message") {
      merged[k] = v;
      continue;
    }
    if (v != null) merged[k] = v;
  }
  // SNAPSHOT: store a shallow copy of the merged result so the modal's
  // data is decoupled from any live row objects. Without this, if `merged`
  // happened to share a reference with a row in `state.logs.rows` (e.g.
  // via `base` being a live row), subsequent mutations to that row would
  // be visible to the modal.
  state.logs.selectedRow = snapshotRow(merged) as unknown as (typeof state.logs)["selectedRow"];
  // Debug: log accepted updates so we can verify only same-request updates
  // go through.
  console.debug(
    "[openproxy log-detail] updateOpenLogDetail ACCEPTED: request_id=%s trace_id=%s model=%s total_ms=%s",
    (row as { request_id?: string }).request_id,
    (row as { trace_id?: string }).trace_id,
    (merged as { upstream_model_id?: unknown }).upstream_model_id,
    (merged as { total_ms?: unknown }).total_ms,
  );
  // Preserve the active tab across re-renders. The previous code
  // called initializeLogDetailTabs() unconditionally, which reset the
  // active tab to the first one on every WS event — even legitimate
  // same-row updates caused the user's tab selection to be lost.
  // Instead, only initialize tabs on first open (handled in
  // showLogDetail), and on updates just re-render the modal body
  // without touching the tab state.
  render(renderLogDetailModal(state.logs.selectedRow as unknown as LogDetailLog), wrapper);
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

/** Build a Markdown-formatted "debug bundle" string for `log`,
 *  containing every field an operator would need to file a bug
 *  report: request_id, trace_id, timestamps, status, provider,
 *  model, latency, cost, error message, request body, response
 *  body (including partial responses), request headers, response
 *  headers, and the full raw row.
 *
 *  The bundle is a single string with fenced ```json blocks so it
 *  pastes cleanly into GitHub issues, Slack, or any other
 *  Markdown-aware surface. Sensitive headers (Authorization,
 *  x-api-key, etc.) are already redacted by the backend before
 *  the row reaches the dashboard — we don't re-redact here, but
 *  we DO truncate very large bodies (>10 KB) to keep the bundle
 *  copy-pasteable.
 *
 *  NOTE: This function builds a Markdown STRING, not HTML — it's
 *  copied to the clipboard, not rendered. Stay with string
 *  concatenation; do NOT migrate to lit-html. */
export function buildDebugBundle(log: LogDetailLog): string {
  const lines: string[] = [];
  const detail: Record<string, unknown> = (log.detail as Record<string, unknown>) || {};

  // Summary header.
  lines.push("# OpenProxy Debug Bundle");
  lines.push("");
  lines.push(`Generated: ${new Date().toISOString()}`);
  lines.push("");

  // If this is an in-flight placeholder (id=0), add a prominent
  // banner so the operator knows the row hasn't been persisted to
  // the DB yet — the null fields below are NOT a recording failure,
  // they're a consequence of the row not existing yet.
  const isInflight: boolean = log.id === 0 || log.id == null;
  if (isInflight) {
    lines.push("> ⚠ **This request is still in progress (or its usage row was never written).**");
    lines.push("> Fields marked `—` below are not yet available because no database row");
    lines.push("> exists for this request. The proxy will record a row when the stream");
    lines.push("> completes, fails, or times out (default idle-chunk timeout: 120s).");
    lines.push("");
    // Include the latest stage event so the operator has something
    // actionable — at least they can see which phase the request is
    // stuck in.
    const traceId: string | undefined = log.trace_id ?? undefined;
    const stageEvent: unknown = traceId
      ? (state.logs.stagesByTraceId as Map<string, unknown>).get(traceId)
      : undefined;
    if (stageEvent && typeof stageEvent === "object") {
      const se = stageEvent as Record<string, unknown>;
      lines.push("## Last Known Stage");
      lines.push("");
      lines.push(`- **Stage:** ${String(se["stage"] ?? "—")}`);
      lines.push(`- **Elapsed ms:** ${String(se["elapsed_ms"] ?? "—")}`);
      lines.push(`- **Connect ms:** ${String(se["connect_ms"] ?? "—")}`);
      lines.push(`- **TTFT ms:** ${String(se["ttft_ms"] ?? "—")}`);
      lines.push(`- **Status code:** ${String(se["status_code"] ?? "—")}`);
      lines.push(`- **Timestamp:** ${String(se["timestamp"] ?? "—")}`);
      if (se["error"]) {
        lines.push(`- **Error:** ${String(se["error"])}`);
      }
      lines.push("");
    }
  }

  // Identity fields.
  lines.push("## Identity");
  lines.push("");
  lines.push(`- **Request ID:** ${String(log.request_id ?? "—")}`);
  lines.push(`- **Trace ID:** ${String(log.trace_id ?? "—")}`);
  lines.push(`- **Usage ID:** ${String(log.id ?? "—")}`);
  lines.push(`- **Created:** ${String(log.created_at ?? "—")}`);
  lines.push("");

  // Request metadata.
  lines.push("## Request Metadata");
  lines.push("");
  lines.push(`- **Provider:** ${String(log.provider_id ?? "—")}`);
  lines.push(`- **Model:** ${String(log.upstream_model_id ?? "—")}`);
  lines.push(`- **Status:** ${String(log.status_code ?? "—")}`);
  lines.push(`- **Streaming:** ${String(log.is_streaming ?? false)}`);
  lines.push(`- **Stream complete:** ${String(log.stream_complete ?? false)}`);
  lines.push(`- **Total ms:** ${String(log.total_ms ?? "—")}`);
  lines.push(`- **Cost USD:** ${String(log.cost_usd ?? "—")}`);
  lines.push(`- **Prompt tokens:** ${String(log.prompt_tokens ?? "—")}${log.prompt_tokens_estimated ? " (estimated)" : ""}`);
  lines.push(`- **Completion tokens:** ${String(log.completion_tokens ?? "—")}${log.completion_tokens_estimated ? " (estimated)" : ""}`);
  lines.push(`- **API key ID:** ${String(log.api_key_id ?? "—")}`);
  lines.push(`- **Race lost:** ${String(log.race_lost ?? false)}`);
  if (log.compression_savings_pct != null) {
    lines.push(`- **Compression savings:** ${log.compression_savings_pct}% (token-based, BPE cl100k_base)`);
  }
  if (log.compression_techniques) {
    lines.push(`- **Compression techniques:** ${log.compression_techniques}`);
  }
  lines.push("");

  // Error.
  const errorMsg: string | null =
    (typeof log.error_message === "string" && log.error_message.length > 0) ? log.error_message :
    (typeof log.error_msg === "string" && log.error_msg.length > 0) ? log.error_msg :
    (typeof log.error_msg_redacted === "string" && log.error_msg_redacted.length > 0) ? log.error_msg_redacted :
    null;
  if (errorMsg) {
    lines.push("## Error");
    lines.push("");
    lines.push("```");
    lines.push(errorMsg);
    lines.push("```");
    lines.push("");
  }

  // Request body — truncate only the `messages` array (which can be
  // huge), keeping all other fields (model, stream, temperature,
  // tools, max_tokens, etc.) intact. The user needs to see the full
  // request structure to debug, but the message content is usually
  // not the issue and can be very large.
  const requestBody: unknown = log.request_body_json ?? detail["request_body_json"];
  if (requestBody != null) {
    lines.push("## Request Body");
    lines.push("");
    lines.push("```json");
    lines.push(summarizeRequestBody(requestBody));
    lines.push("```");
    lines.push("");
  }

  // Response body (may be partial — the backend marks it).
  const responseBody: unknown = log.response_body_json ?? detail["response"] ?? log.response;
  if (responseBody != null) {
    const isPartial = !!(log.is_streaming && !log.stream_complete);
    lines.push(isPartial ? "## Response Body (PARTIAL — stream was interrupted)" : "## Response Body");
    lines.push("");
    lines.push("```json");
    lines.push(truncateForBundle(typeof responseBody === "string" ? responseBody : JSON.stringify(responseBody, null, 2)));
    lines.push("```");
    lines.push("");
  }

  // Request headers (already redacted by the backend).
  const requestHeaders: unknown = log.request_headers ?? detail["request_headers"];
  if (requestHeaders != null) {
    lines.push("## Request Headers (redacted)");
    lines.push("");
    lines.push("```json");
    lines.push(truncateForBundle(typeof requestHeaders === "string" ? requestHeaders : JSON.stringify(requestHeaders, null, 2)));
    lines.push("```");
    lines.push("");
  }

  // Response headers.
  const responseHeaders: unknown = log.response_headers ?? detail["response_headers"];
  if (responseHeaders != null) {
    lines.push("## Response Headers");
    lines.push("");
    lines.push("```json");
    lines.push(truncateForBundle(typeof responseHeaders === "string" ? responseHeaders : JSON.stringify(responseHeaders, null, 2)));
    lines.push("```");
    lines.push("");
  }

  // Raw log row (everything we have).
  lines.push("## Raw Log Row");
  lines.push("");
  lines.push("```json");
  lines.push(truncateForBundle(JSON.stringify(log, null, 2)));
  lines.push("```");
  lines.push("");

  return lines.join("\n");
}

/** Truncate a string to ~10 KB for the debug bundle. Larger bodies
 *  make the bundle uncopy-pasteable. The truncation marker makes it
 *  obvious that data was cut. */
function truncateForBundle(s: string): string {
  const MAX = 10 * 1024;
  if (s.length <= MAX) return s;
  return s.slice(0, MAX) + `\n\n… [truncated, ${s.length - MAX} more bytes omitted]`;
}

/** Summarize a request body for the debug bundle. Truncates only the
 *  `messages` array (which can be huge — full conversation history),
 *  keeping all other fields (model, stream, temperature, tools,
 *  max_tokens, etc.) intact. Each message is truncated to ~500 chars
 *  with a marker if longer. This gives the operator enough context
 *  to see what was sent without blowing up the bundle size.
 *
 *  If the body is a string (not parsed JSON), tries to parse it first;
 *  if that fails, falls back to `truncateForBundle`. */
function summarizeRequestBody(body: unknown): string {
  // If it's a string, try to parse it as JSON first.
  let parsed: unknown = body;
  if (typeof body === "string") {
    try { parsed = JSON.parse(body); }
    catch (_e: unknown) {
      // Not JSON — just truncate the raw string.
      return truncateForBundle(body);
    }
  }
  if (parsed == null || typeof parsed !== "object" || Array.isArray(parsed)) {
    // Not an object — just truncate.
    return truncateForBundle(typeof parsed === "string" ? parsed : JSON.stringify(parsed, null, 2));
  }
  // Clone the object so we can mutate the messages array.
  const obj: Record<string, unknown> = JSON.parse(JSON.stringify(parsed)) as Record<string, unknown>;
  const messages: unknown = obj["messages"];
  if (Array.isArray(messages)) {
    const MAX_MSG_LEN = 500;
    const MAX_MESSAGES = 20;
    const truncatedMessages: unknown[] = [];
    const arr = messages as unknown[];
    const showCount = Math.min(arr.length, MAX_MESSAGES);
    for (let i = 0; i < showCount; i++) {
      const msg = arr[i];
      if (msg && typeof msg === "object" && !Array.isArray(msg)) {
        const msgObj = { ...(msg as Record<string, unknown>) };
        const content = msgObj["content"];
        if (typeof content === "string" && content.length > MAX_MSG_LEN) {
          msgObj["content"] = content.slice(0, MAX_MSG_LEN) + `… [truncated, ${content.length - MAX_MSG_LEN} more chars]`;
        } else if (Array.isArray(content)) {
          // Multimodal content — truncate each part.
          msgObj["content"] = (content as unknown[]).map((part: unknown) => {
            if (part && typeof part === "object" && !Array.isArray(part)) {
              const partObj = { ...(part as Record<string, unknown>) };
              const text = partObj["text"];
              if (typeof text === "string" && text.length > MAX_MSG_LEN) {
                partObj["text"] = text.slice(0, MAX_MSG_LEN) + `… [truncated, ${text.length - MAX_MSG_LEN} more chars]`;
              }
              return partObj;
            }
            return part;
          });
        }
        truncatedMessages.push(msgObj);
      } else {
        truncatedMessages.push(msg);
      }
    }
    if (arr.length > MAX_MESSAGES) {
      truncatedMessages.push(`… [${arr.length - MAX_MESSAGES} more messages omitted]`);
    }
    obj["messages"] = truncatedMessages;
  }
  // Also truncate the `tools` array if present — can be large.
  const tools: unknown = obj["tools"];
  if (Array.isArray(tools)) {
    const MAX_TOOLS = 5;
    const MAX_TOOL_LEN = 500;
    const arr = tools as unknown[];
    const truncatedTools: unknown[] = [];
    const showCount = Math.min(arr.length, MAX_TOOLS);
    for (let i = 0; i < showCount; i++) {
      const tool = arr[i];
      if (tool && typeof tool === "object" && !Array.isArray(tool)) {
        const t = tool as Record<string, unknown>;
        if (t["type"] === "function" && t["function"] && typeof t["function"] === "object") {
          const f = t["function"] as Record<string, unknown>;
          truncatedTools.push({
            type: "function",
            function: {
              name: f["name"],
              description: typeof f["description"] === "string" && f["description"].length > 100
                ? f["description"].slice(0, 100) + "..."
                : f["description"],
              parameters: "… [schema omitted]"
            }
          });
          continue;
        }
      }
      const toolStr = JSON.stringify(tool, null, 2);
      if (toolStr.length > MAX_TOOL_LEN) {
        truncatedTools.push(`${toolStr.slice(0, MAX_TOOL_LEN)}… [truncated, ${toolStr.length - MAX_TOOL_LEN} more chars]`);
      } else {
        truncatedTools.push(tool);
      }
    }
    if (arr.length > MAX_TOOLS) {
      truncatedTools.push(`… [${arr.length - MAX_TOOLS} more tools omitted]`);
    }
    obj["tools"] = truncatedTools;
  }
  return JSON.stringify(obj, null, 2);
}

/** Handler for the "Copy debug bundle" button. Reads the currently-
 *  selected log row from `state.logs.selectedRow`, builds the
 *  bundle, and writes it to the clipboard. Shows a toast for
 *  success / failure.
 *
 *  Includes a fallback for non-secure contexts (HTTP, not localhost)
 *  where `navigator.clipboard` is unavailable: creates a hidden
 *  textarea, selects it, and calls `document.execCommand("copy")`. */
export async function copyDebugBundle(): Promise<void> {
  const row = state.logs.selectedRow as unknown as LogDetailLog | null;
  if (!row) {
    showToast("No log row selected.", "warning");
    return;
  }
  const bundle: string = buildDebugBundle(row);
  try {
    // Try the modern Clipboard API first. Requires a secure context
    // (HTTPS or localhost). If unavailable or it throws, fall back
    // to the deprecated execCommand approach.
    if (navigator.clipboard && typeof navigator.clipboard.writeText === "function") {
      await navigator.clipboard.writeText(bundle);
      showToast("Debug bundle copied to clipboard.", "success");
      return;
    }
  } catch (err) {
    // Fall through to the execCommand fallback. Log the Clipboard
    // API error for debugging but don't surface it to the user yet
    // — the fallback might still work.
    console.warn("[openproxy] navigator.clipboard.writeText failed, falling back to execCommand:", err);
  }
  // Fallback: hidden textarea + execCommand("copy"). Deprecated but
  // works in non-secure contexts and older browsers.
  try {
    const textarea: HTMLTextAreaElement = document.createElement("textarea");
    textarea.value = bundle;
    textarea.style.position = "fixed";
    textarea.style.left = "-9999px";
    textarea.style.top = "0";
    textarea.setAttribute("readonly", "");
    document.body.appendChild(textarea);
    textarea.select();
    const ok: boolean = document.execCommand("copy");
    document.body.removeChild(textarea);
    if (ok) {
      showToast("Debug bundle copied to clipboard.", "success");
    } else {
      // execCommand returned false — copy failed. Show the bundle in
      // a modal so the user can manually select+copy.
      showBundleInModal(bundle, "Copy failed — select the text below and press Ctrl+C");
      showToast("Copy failed — bundle shown in a window for manual copy.", "warning");
    }
  } catch (err) {
    console.error("[openproxy] execCommand copy also failed:", err);
    showBundleInModal(bundle, `Copy failed: ${String(err)} — select the text below and press Ctrl+C`);
    showToast(`Copy failed — bundle shown in a window for manual copy.`, "warning");
  }
}

/** Last-resort fallback: show the bundle in a modal window so the
 *  user can manually select and copy the text. Used when both
 *  `navigator.clipboard` and `document.execCommand("copy")` fail
 *  (e.g., very old browsers or strict permission policies).
 *
 *  Built with lit-html `render()` instead of `innerHTML` so the
 *  bundle text is properly escaped (no XSS risk from a body that
 *  happens to contain HTML characters). */
function showBundleInModal(bundle: string, headerMessage: string): void {
  // Reuse the modal infrastructure. Build a simple modal with a
  // <pre> containing the bundle and a close button.
  const wrapper = document.createElement("div");
  document.body.appendChild(wrapper);
  render(html`
    <div class="modal-bg" @click=${(e: Event) => { if (e.target === wrapper.firstElementChild) { wrapper.remove(); } }}>
      <div class="modal" style="max-width: 800px; max-height: 80vh; display: flex; flex-direction: column;">
        <div class="modal-header">
          <h2>Debug Bundle</h2>
          <button type="button" class="close-btn" @click=${() => wrapper.remove()} aria-label="Close">&times;</button>
        </div>
        <div class="modal-body" style="overflow: auto; padding: var(--space-3);">
          <p class="muted" style="margin-bottom: var(--space-2);">${headerMessage}</p>
          <pre class="json-viewer" style="white-space: pre-wrap; word-break: break-word; user-select: text; cursor: text; padding: var(--space-2); background: var(--color-surface-2); border-radius: var(--radius-sm); font-size: var(--fs-xs);">${bundle}</pre>
        </div>
      </div>
    </div>
  `, wrapper);
}

// Expose for E2E tests so they can simulate WS events arriving while
// the modal is open (regression coverage for the "modal se bugea" bug).
// Declared via `declare global` so tests get type-safe access without
// their own `as any` cast, consistent with the `__openproxyState` /
// `__openproxyLogsGoPage` hooks in app.ts.
declare global {
  interface Window {
    __openproxyUpdateLogDetail?: typeof updateOpenLogDetail;
  }
}
if (typeof window !== "undefined") {
  window.__openproxyUpdateLogDetail = updateOpenLogDetail;
}
