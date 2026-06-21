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
 *  renderResponseTab. */
function parseOpenAiChatResponse(value: unknown): { message: string | null; reasoning: string | null; toolCalls: ToolCall[] } | null {
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

  // If we got neither message, reasoning, nor tool calls, this is
  // not a chat-completion we recognize — fall through.
  if (message == null && reasoning == null && toolCalls.length === 0) return null;
  return { message, reasoning, toolCalls };
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

/** Render the Response tab. Handles null, string, and object inputs,
 *  picking the empty / chat-completion / "Raw response" shape per
 *  the spec. Performs the string-to-JSON normalization before
 *  calling parseOpenAiChatResponse.
 *  @param streamingHint - set to true when the request is streaming
 *        but the response body is null (e.g. interrupted mid-stream). */
function renderResponseTab(response: unknown, streamingHint?: boolean): string {
  // Empty state.
  if (response == null) {
    const placeholder = streamingHint
      ? "Response body not captured (streaming request may have been interrupted)."
      : NO_RESPONSE_PLACEHOLDER_TEXT;
    return `<section class="log-detail-section" data-log-tab="response">
      <h4>Response</h4>
      <p class="muted log-detail-placeholder">${escapeHtml(placeholder)}</p>
    </section>`;
  }

  // String: try to parse as JSON; on success, recurse with parsed
  // value; on failure, format the raw string in a "Raw response"
  // collapsible.
  if (typeof response === "string") {
    let parsed: unknown = null;
    let parsedOk = false;
    try { parsed = JSON.parse(response); parsedOk = true; }
    catch (_e: unknown) { parsedOk = false; }
    if (parsedOk && parsed != null && typeof parsed === "object") {
      return renderResponseTab(parsed);
    }
    return `<section class="log-detail-section" data-log-tab="response">
      <h4>Response</h4>
      <details class="log-detail-collapsible" open>
        <summary>Raw response</summary>
        <pre class="json-viewer log-detail-collapsible-body">${escapeHtml(response)}</pre>
      </details>
    </section>`;
  }

  // Object/array: try to recognize an OpenAI chat-completion shape.
  const parsed = parseOpenAiChatResponse(response);
  if (parsed != null) {
    const blocks: string[] = [];
    if (parsed.message != null) {
      blocks.push(`<details class="log-detail-collapsible" open>
        <summary>Message</summary>
        <pre class="json-viewer log-detail-collapsible-body">${escapeHtml(parsed.message)}</pre>
      </details>`);
    }
    if (parsed.reasoning != null) {
      blocks.push(`<details class="log-detail-collapsible" open>
        <summary>Reasoning</summary>
        <pre class="json-viewer log-detail-collapsible-body">${escapeHtml(parsed.reasoning)}</pre>
      </details>`);
    }
    if (parsed.toolCalls.length > 0) {
      const callBlocks = parsed.toolCalls.map((tc, i) => {
        const idTag = tc.id != null ? ` <span class="log-detail-key-meta">${escapeHtml(tc.id)}</span>` : "";
        const typeTag = tc.type != null && tc.type !== "function" ? ` <span class="log-detail-key-meta">${escapeHtml(tc.type)}</span>` : "";
        const summaryParts = `<span class="log-detail-tool-call-name">${escapeHtml(tc.function.name)}</span>${idTag}${typeTag}`;
        const argsHtml = renderToolCallArgsBlock(tc.function.arguments);
        if (argsHtml.length > 0) {
          return `<details class="log-detail-collapsible"${i === 0 ? " open" : ""}>
            <summary>${summaryParts}</summary>
            ${argsHtml}
          </details>`;
        }
        return `<div class="log-detail-tool-call">
          <div class="log-detail-tool-call-header">
            ${summaryParts}
          </div>
        </div>`;
      }).join("");
      blocks.push(`<details class="log-detail-collapsible" open>
        <summary>Tool calls (${parsed.toolCalls.length})</summary>
        <div class="log-detail-tool-calls">${callBlocks}</div>
      </details>`);
    }
    if (blocks.length === 0) {
      // Defensive: parseOpenAiChatResponse would have returned null
      // in this case, but keep the fallback just in case.
      return `<section class="log-detail-section" data-log-tab="response">
        <h4>Response</h4>
        <details class="log-detail-collapsible" open>
          <summary>Raw response</summary>
          <pre class="json-viewer log-detail-collapsible-body">${formatJson(response)}</pre>
        </details>
      </section>`;
    }
    return `<section class="log-detail-section" data-log-tab="response">
      <h4>Response</h4>
      ${blocks.join("\n      ")}
    </section>`;
  }

  // Fallback: not a recognized chat-completion. Try to extract
  // whatever content we can from the raw object — message,
  // reasoning, and tool calls — and only show the full "Raw
  // response" block when there is nothing structured to display.
  let rawContent: string | null = null;
  let rawReasoning: string | null = null;
  let rawToolCalls: ToolCall[] = [];
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
          if (typeof content === "string") rawContent = content || null;
          const rc: unknown = msg["reasoning_content"];
          if (typeof rc === "string") rawReasoning = rc;
          else {
            const r: unknown = msg["reasoning"];
            if (typeof r === "string") rawReasoning = r;
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
      }
    }
  }
  const blocks: string[] = [];
  if (rawContent != null) {
    blocks.push(`<details class="log-detail-collapsible" open>
      <summary>Message</summary>
      <pre class="json-viewer log-detail-collapsible-body">${escapeHtml(rawContent)}</pre>
    </details>`);
  }
  if (rawReasoning != null) {
    blocks.push(`<details class="log-detail-collapsible" open>
      <summary>Reasoning</summary>
      <pre class="json-viewer log-detail-collapsible-body">${escapeHtml(rawReasoning)}</pre>
    </details>`);
  }
  if (rawToolCalls.length > 0) {
    const callBlocks = rawToolCalls.map((tc, i) => {
      const idTag = tc.id != null ? ` <span class="log-detail-key-meta">${escapeHtml(tc.id)}</span>` : "";
      const typeTag = tc.type != null && tc.type !== "function" ? ` <span class="log-detail-key-meta">${escapeHtml(tc.type)}</span>` : "";
      const summaryParts = `<span class="log-detail-tool-call-name">${escapeHtml(tc.function.name)}</span>${idTag}${typeTag}`;
      const argsHtml = renderToolCallArgsBlock(tc.function.arguments);
      if (argsHtml.length > 0) {
        return `<details class="log-detail-collapsible"${i === 0 ? " open" : ""}>
          <summary>${summaryParts}</summary>
          ${argsHtml}
        </details>`;
      }
      return `<div class="log-detail-tool-call">
        <div class="log-detail-tool-call-header">
          ${summaryParts}
        </div>
      </div>`;
    }).join("");
    blocks.push(`<details class="log-detail-collapsible" open>
      <summary>Tool calls (${rawToolCalls.length})</summary>
      <div class="log-detail-tool-calls">${callBlocks}</div>
    </details>`);
  }
  // Only show the full "Raw response" when we couldn't extract
  // any structured blocks above.
  if (blocks.length === 0) {
    blocks.push(`<details class="log-detail-collapsible" open>
      <summary>Raw response</summary>
      <pre class="json-viewer log-detail-collapsible-body">${formatJson(response)}</pre>
    </details>`);
  }
  return `<section class="log-detail-section" data-log-tab="response">
    <h4>Response</h4>
    ${blocks.join("\n      ")}
  </section>`;
}

/** Render the Request tab. Returns the full
 *  `<section data-log-tab="request">…</section>` HTML. Handles the
 *  empty / object / non-object fallback shapes per the spec. */
function renderRequestTab(requestBody: unknown): string {
  // Empty state: same predicate as the previous inline logic.
  const hasRequestBody: boolean = requestBody != null
    && !(typeof requestBody === "string" && requestBody.trim() === "")
    && !(typeof requestBody === "object" && requestBody !== null
         && !Array.isArray(requestBody) && Object.keys(requestBody as object).length === 0
         && JSON.stringify(requestBody) === "{}");
  if (!hasRequestBody) {
    return `<section class="log-detail-section" data-log-tab="request">
      <h4>Request</h4>
      <p class="muted">No request body recorded.</p>
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
            ${renderRequestTab(requestBody)}
            ${renderResponseTab(response, isStreaming)}
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
