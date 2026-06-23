// components/log-detail.ts — render the per-request log detail
// modal. The modal HTML is built and inserted into the DOM; the
// "Close" button uses data-action="closeLogDetailModal" and the
// backdrop itself uses data-action="closeLogDetailModal".
//
// Per spec §3 + §13.8 we do not use inline `onclick="window.X()"`
// handlers.
//
// Per SPEC_LOG_DETAIL_MODAL.md the Request and Response tabs render
// their content as friendly, collapsible sections; Errors and Raw
// keep using jsonSection.

import { state } from "../state/index.js";
import { escapeHtml } from "../lib/escape.js";
import { appendModal } from "../lib/dom.js";
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
  cost_usd?: number | null;
  is_streaming?: boolean;
  stream_complete?: boolean;
  /** Compression savings (0.0–100.0) or null when off. */
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

function formatJson(value: unknown): string {
  if (value == null) return "(empty)";
  let s: string;
  try { s = typeof value === "string" ? value : JSON.stringify(value, null, 2); }
  catch (_e: unknown) { s = String(value); }
  return escapeHtml(s);
}

function jsonSection(title: string, value: unknown, tabKey: string): string {
  return `<section class="log-detail-section" data-log-tab="${escapeHtml(tabKey)}">
    <h4>${escapeHtml(title)}</h4>
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
 *  caller wraps the returned string in `escapeHtml` (or feeds it
 *  to `formatJson`, which already escapes). */
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
 *  collapsible sections when the arguments parse as an object. */
function renderToolCallArgsBlock(args: unknown): string {
  if (args == null) return "";
  let parsed: Record<string, unknown> | null = null;
  if (typeof args === "string") {
    try { parsed = JSON.parse(args) as Record<string, unknown>; }
    catch (_e: unknown) { /* fall through to raw */ }
  } else if (typeof args === "object" && args !== null && !Array.isArray(args)) {
    parsed = args as Record<string, unknown>;
  }
  if (parsed != null && Object.keys(parsed).length > 0) {
    const parts: string[] = [];
    for (const [k, v] of Object.entries(parsed)) {
      parts.push(`<details class="log-detail-collapsible" open>
        <summary><span class="log-detail-key">${escapeHtml(k)}</span></summary>
        <pre class="json-viewer log-detail-collapsible-body">${formatJson(v)}</pre>
      </details>`);
    }
    return parts.join("\n        ");
  }
  // Fall back to raw pretty-print.
  const result = parseToolCallArguments(args);
  return result != null
    ? `<pre class="json-viewer log-detail-collapsible-body">${escapeHtml(result.pretty)}</pre>`
    : "";
}

/** Render a single "Message" (content) block. Returns "" when the
 *  content is null/empty — the caller omits the block entirely in
 *  that case, so a content-less response doesn't show an empty
 *  Message section. */
function renderMessageBlock(message: string): string {
  return `<details class="log-detail-collapsible" open>
    <summary>Message</summary>
    <pre class="json-viewer log-detail-collapsible-body">${escapeHtml(message)}</pre>
  </details>`;
}

/** Render a single "Reasoning" block. Same omit-when-empty contract
 *  as `renderMessageBlock`. */
function renderReasoningBlock(reasoning: string): string {
  return `<details class="log-detail-collapsible" open>
    <summary>Reasoning</summary>
    <pre class="json-viewer log-detail-collapsible-body">${escapeHtml(reasoning)}</pre>
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
function renderToolCallBlock(tc: ToolCall, index: number): string {
  const idTag = tc.id != null ? ` <span class="log-detail-key-meta">${escapeHtml(tc.id)}</span>` : "";
  const typeTag = tc.type != null && tc.type !== "function" ? ` <span class="log-detail-key-meta">${escapeHtml(tc.type)}</span>` : "";
  const summaryParts = `<span class="log-detail-tool-call-name">Tool call #${index + 1}: ${escapeHtml(tc.function.name)}</span>${idTag}${typeTag}`;
  const argsHtml = renderToolCallArgsBlock(tc.function.arguments);
  if (argsHtml.length > 0) {
    return `<details class="log-detail-collapsible">
      <summary>${summaryParts}</summary>
      ${argsHtml}
    </details>`;
  }
  // No arguments to show — render a non-collapsible header so the
  // tool call is still visible (its existence is information the
  // operator needs).
  return `<div class="log-detail-tool-call">
    <div class="log-detail-tool-call-header">
      ${summaryParts}
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
 *  but not the primary thing the operator wants to see. */
function renderOtherPropertiesBlock(props: Record<string, unknown>): string {
  const parts: string[] = [];
  for (const [k, v] of Object.entries(props)) {
    parts.push(`<details class="log-detail-collapsible">
      <summary><span class="log-detail-key">${escapeHtml(k)}</span></summary>
      <pre class="json-viewer log-detail-collapsible-body">${formatJson(v)}</pre>
    </details>`);
  }
  if (parts.length === 0) return "";
  return `<details class="log-detail-collapsible">
    <summary>Other properties (${parts.length})</summary>
    <div class="log-detail-tool-calls">${parts.join("\n      ")}</div>
  </details>`;
}

/** Render the "Raw response" block. ALWAYS collapsed by default —
 *  it's the escape hatch for "the structured blocks above didn't
 *  show me what I needed, let me see the raw JSON". */
function renderRawResponseBlock(response: unknown): string {
  return `<details class="log-detail-collapsible">
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
 *        but the response body is null (e.g. interrupted mid-stream). */
function renderResponseTab(
  response: unknown,
  streamingHint?: boolean,
  createdAt?: string,
  /** When true, the response was interrupted mid-stream — show a
   *  "Partial response" banner so the operator knows the response
   *  didn't complete normally even though there IS a body to
   *  inspect. Passed from the caller which reads
   *  `is_streaming && !stream_complete` (and the `partial` marker
   *  inside the JSON, when present). */
  isPartial?: boolean,
): string {
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
    return `<section class="log-detail-section" data-log-tab="response">
      <h4>Response</h4>
      <p class="muted log-detail-placeholder">${escapeHtml(placeholder)}</p>
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
  const partialBannerHtml = showPartialBanner
    ? `<div class="log-detail-partial-banner">⚠ Partial response — stream was interrupted before completion. The content below is what was received up to the point of failure.</div>`
    : "";

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
    return `<section class="log-detail-section" data-log-tab="response">
      <h4>Response</h4>
      ${partialBannerHtml}
      ${renderRawResponseBlock(response)}
    </section>`;
  }

  // Try to recognize an OpenAI chat-completion shape.
  const parsed = parseOpenAiChatResponse(response);
  if (parsed != null) {
    const blocks: string[] = [];
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
      blocks.push(renderOtherPropertiesBlock(parsed.otherProperties));
    }
    // Raw response — always present, collapsed by default. The
    // operator can expand it to see the original JSON when the
    // structured extraction missed something.
    blocks.push(renderRawResponseBlock(response));
    return `<section class="log-detail-section" data-log-tab="response">
      <h4>Response</h4>
      ${partialBannerHtml}
      ${blocks.join("\n      ")}
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
  const blocks: string[] = [];
  if (rawContent != null) blocks.push(renderMessageBlock(rawContent));
  if (rawReasoning != null) blocks.push(renderReasoningBlock(rawReasoning));
  for (let i = 0; i < rawToolCalls.length; i++) {
    const tc = rawToolCalls[i];
    if (tc != null) blocks.push(renderToolCallBlock(tc, i));
  }
  if (rawOtherProperties != null && Object.keys(rawOtherProperties).length > 0) {
    blocks.push(renderOtherPropertiesBlock(rawOtherProperties));
  }
  // Always show the raw response as the last block.
  blocks.push(renderRawResponseBlock(response));
  return `<section class="log-detail-section" data-log-tab="response">
    <h4>Response</h4>
    ${partialBannerHtml}
    ${blocks.join("\n      ")}
  </section>`;
}

/** Render the Request tab. Returns the full
 *  `<section data-log-tab="request">…</section>` HTML. Handles the
 *  empty / object / non-object fallback shapes per the spec.
 *
 *  `createdAt` is the row's `created_at` timestamp (ISO string). When
 *  the request body is null, we use it to compute the row's age and
 *  show a more helpful message: if the row is older than the
 *  recording TTL (5 min default), the body was likely pruned by
 *  `prune_expired_recording_bodies`; otherwise recording was OFF
 *  when the request was made. */
function renderRequestTab(requestBody: unknown, createdAt?: string): string {
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
    return `<section class="log-detail-section" data-log-tab="request">
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
    const blocks: string[] = [];

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
    const renderToolBlock = (tool: unknown, index: number): string => {
      if (tool == null || typeof tool !== "object" || Array.isArray(tool)) {
        return `<details class="log-detail-collapsible">
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
      const parts: string[] = [];
      if (description != null && !isEmptyValue(description)) {
        parts.push(`<details class="log-detail-collapsible">
          <summary>Description</summary>
          <pre class="json-viewer log-detail-collapsible-body">${escapeHtml(typeof description === "string" ? description : JSON.stringify(description, null, 2))}</pre>
        </details>`);
      }
      if (parameters != null && !isEmptyValue(parameters)) {
        parts.push(`<details class="log-detail-collapsible">
          <summary>Parameters</summary>
          <pre class="json-viewer log-detail-collapsible-body">${formatJson(parameters)}</pre>
        </details>`);
      }
      if (strict != null && !isEmptyValue(strict)) {
        parts.push(`<details class="log-detail-collapsible">
          <summary>Strict</summary>
          <pre class="json-viewer log-detail-collapsible-body">${formatJson(strict)}</pre>
        </details>`);
      }
      const extraKeys = Object.keys(t).filter((k) => k !== "type" && k !== "function");
      for (const ek of extraKeys) {
        const ev = t[ek];
        if (!isEmptyValue(ev)) {
          parts.push(`<details class="log-detail-collapsible">
            <summary>${escapeHtml(ek)}</summary>
            <pre class="json-viewer log-detail-collapsible-body">${formatJson(ev)}</pre>
          </details>`);
        }
      }
      return `<details class="log-detail-collapsible"${index === 0 ? " open" : ""}>
        <summary><span class="log-detail-tool-call-name">${escapeHtml(toolType)}</span> <span class="log-detail-key-meta">${escapeHtml(name)}</span></summary>
        ${parts.join("\n        ")}
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
        const toolBlocks = (value as unknown[]).map((t, i) => renderToolBlock(t, i)).join("");
        blocks.push(`<details class="log-detail-collapsible" open>
          <summary><span class="log-detail-key log-detail-key-pinned">tools</span> <span class="log-detail-key-meta">${value.length} tool(s)</span></summary>
          <div class="log-detail-messages">${toolBlocks}</div>
        </details>`);
        rendered.add(key);
        continue;
      }

      // Special handling for "messages": render each message as an
      // individual collapsible with its role in the summary.
      if (key === "messages" && Array.isArray(value) && value.length > 0) {
        const msgBlocks = (value as unknown[]).map((raw, i) => {
          if (raw == null || typeof raw !== "object" || Array.isArray(raw)) {
            return `<details class="log-detail-collapsible">
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
          const extraStr = extras.length > 0 ? ` <span class="log-detail-key-meta">${escapeHtml(extras.join(" · "))}</span>` : "";
          // Content preview: first 80 chars of the content string.
          let preview = "";
          if (typeof content === "string") {
            preview = content.length > 80 ? content.slice(0, 80) + "…" : content;
          } else if (content != null) {
            const s = typeof content === "string" ? content : JSON.stringify(content);
            preview = s.length > 80 ? s.slice(0, 80) + "…" : s;
          }
          const previewStr = preview.length > 0 ? ` <span class="log-detail-msg-preview">${escapeHtml(preview)}</span>` : "";
          return `<details class="log-detail-collapsible">
            <summary><span class="log-detail-role ${roleClass}">${escapeHtml(role)}</span>${extraStr}${previewStr}</summary>
            <pre class="json-viewer log-detail-collapsible-body">${formatJson(msg)}</pre>
          </details>`;
        }).join("");
        blocks.push(`<details class="log-detail-collapsible" open>
          <summary><span class="log-detail-key log-detail-key-pinned">messages</span> <span class="log-detail-key-meta">${value.length} message(s)</span></summary>
          <div class="log-detail-messages">${msgBlocks}</div>
        </details>`);
        rendered.add(key);
        continue;
      }

      const open = isPrimitive(value) ? " open" : "";
      const meta = metaText(value);
      const metaSpan = meta.length > 0
        ? ` <span class="log-detail-key-meta">${escapeHtml(meta)}</span>`
        : "";
      blocks.push(`<details class="log-detail-collapsible"${open}>
        <summary><span class="log-detail-key log-detail-key-pinned">${escapeHtml(key)}</span>${metaSpan}</summary>
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
      blocks.push(`<details class="log-detail-collapsible">
        <summary><span class="log-detail-key">${escapeHtml(key)}</span></summary>
        <pre class="json-viewer log-detail-collapsible-body">${formatJson(value)}</pre>
      </details>`);
    }

    return `<section class="log-detail-section" data-log-tab="request">
      <h4>Request</h4>
      ${blocks.join("\n      ")}
    </section>`;
  }

  // Array or primitive fallback.
  return `<section class="log-detail-section" data-log-tab="request">
    <h4>Request</h4>
    <details class="log-detail-collapsible" open>
      <summary>Raw request body</summary>
      <pre class="json-viewer log-detail-collapsible-body">${formatJson(body)}</pre>
    </details>
  </section>`;
}

export function renderLogDetailModal(log: LogDetailLog): string {
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

  return `
    <div id="log-detail-modal" class="modal-bg log-detail-modal" data-action="closeLogDetailModal">
      <div class="modal">
        <div class="modal-header">
          <h2>Log #${escapeHtml(String(requestId))}</h2>
          <button type="button" class="log-detail-copy-bundle-btn" data-action="copyDebugBundle" title="Copy a Markdown-formatted debug bundle with all request/response/error context — ready to paste into a bug report.">📋 Copy debug bundle</button>
          <button type="button" class="close-btn" data-action="closeLogDetailModal" aria-label="Close">&times;</button>
        </div>
        <div class="modal-body">
          <div class="log-detail-summary">
            <div><strong>Status:</strong> <span class="status-pill ${statusClass}">${escapeHtml(String(status))}</span></div>
            <div><strong>Provider:</strong> ${escapeHtml(String(provider))}</div>
            <div><strong>Account:</strong> ${escapeHtml(String(account))}</div>
            <div><strong>Combo:</strong> ${combo != null ? escapeHtml(String(combo)) : "—"}</div>
            <div><strong>Model:</strong> ${escapeHtml(String(model))}</div>
            <div><strong>Latency:</strong> ${escapeHtml(latency)}</div>
            <div><strong>Cost:</strong> ${costRaw != null ? escapeHtml(String(costRaw)) : "—"}</div>
            ${renderCompressionSummary(log)}
            <div><strong>Created:</strong> ${escapeHtml(String(createdAt))}</div>
            ${apiKeyId != null ? `<div><strong>API key:</strong> #${escapeHtml(String(apiKeyId))}</div>` : ""}
            ${userAgent ? `<div><strong>User-Agent:</strong> ${escapeHtml(String(userAgent))}</div>` : ""}
          </div>
          <div class="log-detail-tabs">
            <button class="detail-tab" data-action="logDetailTab" data-arg1="request">Request</button>
            <button class="detail-tab" data-action="logDetailTab" data-arg1="response">Response</button>
            <button class="detail-tab" data-action="logDetailTab" data-arg1="errors">Errors</button>
            <button class="detail-tab" data-action="logDetailTab" data-arg1="raw">Raw</button>
          </div>
          <div class="log-detail-content" id="log-detail-content">
            ${renderRequestTab(requestBody, createdAt)}
            ${renderResponseTab(response, isStreaming, createdAt, isPartial)}
            ${errors != null
              ? jsonSection("Errors", errors, "errors")
              : `<section class="log-detail-section" data-log-tab="errors">
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

// Initialize the log-detail tab UI: show only the first [data-log-tab]
// section, hide the remaining ones, and mark the first detail-tab as
// active. Centralized here so it can be re-invoked after any in-place
// re-render (e.g. updateOpenLogDetail replacing the modal HTML).
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

// `wired` flag a nivel módulo — el listener se registra UNA sola
// vez por sesión (no por modal abierto). El handler tabClickOnce
// tiene guard vía `closest()` que retorna early si no hay modal,
// así que dejarlo siempre montado es seguro y elimina el leak.
let wired: boolean = false;

function wireTabClickOnce(): void {
  if (wired) return;
  document.addEventListener("click", tabClickOnce);
  wired = true;
}

// Public API
export function showLogDetail(log: LogDetailLog): void {
  // BUG-STACK-1: if showLogDetail is called twice without an
  // intervening closeLogDetailModal, two .log-detail-modal elements
  // would stack on top of each other and confuse event delegation.
  // Remove any pre-existing modal before appending the new one.
  document.querySelector(".log-detail-modal")?.remove();
  const html: string = renderLogDetailModal(log);
  appendModal(html);
  wireTabClickOnce();
  initializeLogDetailTabs();
}

const tabClickOnce: (e: Event) => void = (e: Event) => {
  const target: EventTarget | null = e.target;
  if (!(target instanceof Element)) return;
  const tab: HTMLElement | null = target.closest(".detail-tab[data-action='logDetailTab']");
  if (!tab) return;
  // The central data-action shim (app.js) will also dispatch a
  // logDetailTab call. We let the shim do the work and return here
  // to avoid running the body twice. To support that, we just
  // update the active-tab indicator early and let the shim
  // handler deal with content visibility.
  document.querySelectorAll(".log-detail-tabs .detail-tab").forEach((t) => t.classList.toggle("active", t === tab));
};

// NOTE: ya no removemos el listener aquí. El listener vive toda
// la sesión (registrado una vez por wireTabClickOnce); el handler
// `tabClickOnce` retorna early vía `closest()` cuando no hay modal
// abierto, así que no hay costo de CPU ni comportamiento fantasma.
export function closeLogDetailModal(e: Event | null): void {
  // Close only if the click was on the backdrop itself or on the
  // explicit X button. The shim in app.js dispatches data-action via
  // `e.target.closest("[data-action]")`, so we re-derive the matched
  // element here to know which case we're in.
  if (!e || !e.target) return;
  const target: EventTarget = e.target;
  if (!(target instanceof Element)) return;
  const m: HTMLElement | null = target.closest(".log-detail-modal");
  if (!m) return;
  const matched: HTMLElement | null = target.closest("[data-action]");
  // Case 1: click was directly on the backdrop (the wrapper itself).
  // Use `target === m` (strict identity) so clicks on descendants
  // like the <pre> text body or the JSON viewer don't bubble up and
  // close the modal — only an actual click on the empty wrapper area
  // should close it. `target.closest("[data-action]")` would always
  // return the wrapper for descendants, so we cannot use it here.
  if (target === m) { m.remove(); return; }
  // Case 2: click was on the explicit X close button in the header.
  if (matched && matched.classList && matched.classList.contains("close-btn")) {
    m.remove(); return;
  }
  // Case 3: click was inside .modal on something else (tabs, content,
  // summary, etc.) with a different data-action — do nothing; the
  // other handler (e.g. logDetailTab) already handled the click.
}

/// Render the compression savings line for the log detail summary.
///
/// Header-only: shows the percentage prominently. Long technique
/// lists can overflow narrow modals, so the techniques string is
/// moved into the `title` attribute (hover tooltip) instead of
/// being rendered as visible text. The Raw tab keeps the
/// techniques visible because they're useful there.
function renderCompressionSummary(log: LogDetailLog): string {
  const pct = log.compression_savings_pct ?? null;
  if (pct == null || pct <= 0) return "";
  const tech = log.compression_techniques ?? "";
  const pctRounded = Math.round(pct);
  const pctText = `-${escapeHtml(String(pctRounded))}%`;
  if (tech.length > 0) {
    return `<div><strong>Compression:</strong> <span title="${escapeHtml(tech)}">${pctText}</span></div>`;
  }
  return `<div><strong>Compression:</strong> ${pctText}</div>`;
}

// Update the open log-detail modal with a new row (called from the
// WebSocket `row`/`stage` event handlers when the user has a row
// detail modal open and a new event for that request arrives).
// If no modal is open this is a no-op.
export function updateOpenLogDetail(row: LogDetailLog | null | undefined): void {
  if (!row) return;
  const modal: HTMLElement | null = document.querySelector(".log-detail-modal");
  if (!modal) return;
  const sel = state.logs.selectedRow as unknown as (LogDetailLog & { request_id?: string }) | null;
  if (!sel || sel.request_id !== row.request_id) return;
  const merged: Record<string, unknown> = { ...sel } as Record<string, unknown>;
  for (const [k, v] of Object.entries(row as Record<string, unknown>)) {
    if (v != null) merged[k] = v;
  }
  state.logs.selectedRow = merged as unknown as (typeof state.logs)["selectedRow"];
  const html: string = renderLogDetailModal(state.logs.selectedRow as unknown as LogDetailLog);
  modal.outerHTML = html;
  wireTabClickOnce();
  initializeLogDetailTabs();
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
 *  copy-pasteable. */
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
  lines.push(`- **Prompt tokens:** ${String(log.prompt_tokens ?? "—")}`);
  lines.push(`- **Completion tokens:** ${String(log.completion_tokens ?? "—")}`);
  lines.push(`- **API key ID:** ${String(log.api_key_id ?? "—")}`);
  lines.push(`- **Race lost:** ${String(log.race_lost ?? false)}`);
  if (log.compression_savings_pct != null) {
    lines.push(`- **Compression savings:** ${log.compression_savings_pct}%`);
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
      const toolStr = JSON.stringify(tool, null, 2);
      if (toolStr.length > MAX_TOOL_LEN) {
        truncatedTools.push(JSON.parse(toolStr.slice(0, MAX_TOOL_LEN).replace(/[^{}[\],:"]*$/, "") + `"… [truncated, ${toolStr.length - MAX_TOOL_LEN} more chars]"`));
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
 *  (e.g., very old browsers or strict permission policies). */
function showBundleInModal(bundle: string, headerMessage: string): void {
  // Reuse the modal infrastructure. Build a simple modal with a
  // <pre> containing the bundle and a close button.
  const modal: HTMLDivElement = document.createElement("div");
  modal.className = "modal-bg";
  modal.style.display = "flex";
  modal.innerHTML = `
    <div class="modal" style="max-width: 800px; max-height: 80vh; display: flex; flex-direction: column;">
      <div class="modal-header">
        <h2>Debug Bundle</h2>
        <button type="button" class="close-btn" data-action="closeModalBg" aria-label="Close">&times;</button>
      </div>
      <div class="modal-body" style="overflow: auto; padding: var(--space-3);">
        <p class="muted" style="margin-bottom: var(--space-2);">${escapeHtml(headerMessage)}</p>
        <pre class="json-viewer" style="white-space: pre-wrap; word-break: break-word; user-select: text; cursor: text; padding: var(--space-2); background: var(--color-surface-2); border-radius: var(--radius-sm); font-size: var(--fs-xs);">${escapeHtml(bundle)}</pre>
      </div>
    </div>
  `;
  // Click on backdrop closes the modal.
  modal.addEventListener("click", (e: MouseEvent) => {
    if (e.target === modal) modal.remove();
  });
  // Close button.
  const closeBtn: HTMLButtonElement | null = modal.querySelector(".close-btn");
  if (closeBtn) {
    closeBtn.addEventListener("click", () => modal.remove());
  }
  document.body.appendChild(modal);
}
