// components/log-detail/index.ts — public facade + the Request / Response
// tab renderers (and their local helpers). Everything else is re-exported
// here so external callers (views/logs.ts, handlers/registry.ts) keep
// importing from "../components/log-detail.js" with no API change.
//
// SPLIT LAYOUT (Q19):
//   index.ts            — facade + renderRequestTab/renderResponseTab + helpers
//   json-prune.ts       — formatJson + prune*/truncate* helpers
//   response-parser.ts  — OpenAI/Anthropic response parsing + block renderers
//   modal.ts            — renderLogDetailModal + modal lifecycle + clock/window hooks
//   debug-bundle.ts     — buildDebugBundle + summarize/truncate + copy handlers
//   state.ts            — LogDetailLog type + pinned identity + generation + active tab
//
// KNOWN CYCLE (index ↔ modal): modal.ts calls the tab renderers exported from
// this module (renderRequestTab, renderResponseTab, jsonSection,
// statusPillClass, readString) when it composes the modal template; this
// module references modal.ts only through `export { ... } from "./modal.js"`
// re-export statements (no runtime calls in index.ts). Because index.ts never
// invokes a modal function and modal.ts only calls these hoisted function
// declarations at render time (never at module-evaluation time), the cycle
// resolves safely under ESM live-binding semantics.

import { html, type TemplateResult } from "lit-html";
import { formatJson } from "./json-prune.js";
import {
  computeTtlExpiryHint,
  detectPartialResponse,
  extractAnthropicToolStats,
  extractRawResponseBody,
  NO_RESPONSE_PLACEHOLDER_TEXT,
  parseOpenAiChatResponse,
  renderFallbackResponseBlocks,
  renderParsedResponseBlocks,
  renderRawResponseBlock,
  renderRawResponseBodyBlock,
} from "./response-parser.js";

// Public API — re-exported unchanged so the former `log-detail.ts`
// module surface is preserved (same function names, same signatures).
export type { LogDetailLog } from "./state.js";
export {
  bumpOpenLogDetailGeneration,
  hasCompleteLogDetail,
  isCurrentOpenLogDetailGeneration,
  matchesPinnedModalIdentity,
} from "./state.js";
export { buildDebugBundle, copyDebugBundle, copyRawJson } from "./debug-bundle.js";
export {
  closeLogDetailModal,
  initializeLogDetailTabs,
  logDetailTabClick,
  openLogDetail,
  renderLogDetailModal,
  showLogDetail,
  updateOpenLogDetail,
} from "./modal.js";

// ---- local helpers used only by the tab renderers below ----

/** Shared by renderRequestTab / renderResponseTab consumers (modal.ts). */
export function statusPillClass(s: string | null | undefined): string {
  if (s === "ok" || s === "success") return "ok";
  if (s === "error" || s === "failed" || s === "unhealthy") return "err";
  if (s === "timeout" || s === "rate_limited" || s === "degraded") return "warn";
  return "warn";
}

/** Read a string field out of a record, or null when absent/non-string.
 *  Used by renderLogDetailModal (modal.ts) for meta lookups. */
export function readString(o: Record<string, unknown> | null | undefined, k: string): string | null {
  if (!o) return null;
  const v: unknown = o[k];
  return typeof v === "string" ? v : null;
}

/** Render a `<section data-log-tab>` with a pretty-printed JSON viewer.
 *  Used by renderLogDetailModal (modal.ts) for the Errors / Raw tabs. */
export function jsonSection(title: string, value: unknown, tabKey: string): TemplateResult {
  return html`<section class="log-detail-section" data-log-tab=${tabKey}>
    <h4>${title}</h4>
    <pre class="json-viewer">${formatJson(value)}</pre>
  </section>`;
}

/** Top-level keys in the Request body that are rendered first, in
 *  this fixed order, and given the `log-detail-key-pinned` class so
 *  operators can spot them quickly. */
const PINNED_REQUEST_KEYS: readonly string[] = [
  "model", "system", "messages", "tools", "temperature", "stream", "max_tokens",
];

const ROLE_CLASS_MAP: Record<string, string> = {
  system: "log-detail-role-system",
  assistant: "log-detail-role-assistant",
  user: "log-detail-role-user",
  tool: "log-detail-role-tool",
};

function getRoleBadgeClass(role: string): string {
  return ROLE_CLASS_MAP[role] ?? "";
}

function isEmptyValue(v: unknown): boolean {
  if (v == null) return true;
  if (typeof v === "string" && v.trim() === "") return true;
  if (Array.isArray(v) && v.length === 0) return true;
  if (typeof v === "object" && !Array.isArray(v) && Object.keys(v as object).length === 0) return true;
  return false;
}

function isPrimitive(v: unknown): boolean {
  const t = typeof v;
  return v == null || t === "string" || t === "number" || t === "boolean";
}

function metaText(v: unknown): string {
  if (Array.isArray(v)) {
    return v.length === 0 ? "empty" : (v.length === 1 ? "1 item" : `${v.length} items`);
  }
  if (v != null && typeof v === "object") {
    const keys = Object.keys(v as object).length;
    return keys === 0 ? "empty" : (keys === 1 ? "1 key" : `${keys} keys`);
  }
  return "";
}

function formatMessageExtras(
  name: unknown,
  toolCallId: unknown,
  toolCalls: unknown,
  toolUses: number,
  toolResults: number,
  resultIds: string[],
): string[] {
  const extras: string[] = [];
  if (typeof name === "string") extras.push(name);
  if (typeof toolCallId === "string") extras.push(`tool_call_id: ${toolCallId}`);
  if (Array.isArray(toolCalls) && toolCalls.length > 0) extras.push(`${toolCalls.length} tool call(s)`);
  if (toolUses > 0) extras.push(`${toolUses} tool call(s)`);
  if (toolResults > 0) {
    let resStr = `${toolResults} tool result(s)`;
    if (resultIds.length > 0) {
      const ids = resultIds.map((id) => id.split("-")[0] + "…").join(", ");
      resStr += ` (${ids})`;
    }
    extras.push(resStr);
  }
  return extras;
}

function extractMessagePreview(content: unknown, toolUses: number, toolResults: number): string {
  if (typeof content === "string") {
    return content.length > 80 ? content.slice(0, 80) + "…" : content;
  }
  if (Array.isArray(content)) {
    const textBlock = content.find(
      (b) => b && typeof b === "object" && b["type"] === "text" && typeof b["text"] === "string",
    ) as Record<string, unknown> | undefined;
    if (textBlock && typeof textBlock["text"] === "string") {
      const text = textBlock["text"];
      return text.length > 80 ? text.slice(0, 80) + "…" : text;
    }
    if (toolUses > 0) return "[tool_use]";
    if (toolResults > 0) return "[tool_result]";
    const s = JSON.stringify(content);
    return s.length > 80 ? s.slice(0, 80) + "…" : s;
  }
  if (content != null) {
    const s = JSON.stringify(content);
    return s.length > 80 ? s.slice(0, 80) + "…" : s;
  }
  return "";
}

function renderSingleMessageCollapsible(raw: unknown, index: number): TemplateResult {
  if (raw == null || typeof raw !== "object" || Array.isArray(raw)) {
    return html`<details class="log-detail-collapsible">
      <summary>Message #${index + 1}</summary>
      <pre class="json-viewer log-detail-collapsible-body">${formatJson(raw)}</pre>
    </details>`;
  }
  const msg = raw as Record<string, unknown>;
  const role: string = typeof msg["role"] === "string" ? msg["role"] : "unknown";
  const content = msg["content"];
  const roleClass = getRoleBadgeClass(role);

  const { toolUses, toolResults, resultIds } = extractAnthropicToolStats(content);
  const extras = formatMessageExtras(
    msg["name"],
    msg["tool_call_id"],
    msg["tool_calls"],
    toolUses,
    toolResults,
    resultIds,
  );
  const extraStr: TemplateResult | null = extras.length > 0
    ? html` <span class="log-detail-key-meta">${extras.join(" · ")}</span>`
    : null;

  const preview = extractMessagePreview(content, toolUses, toolResults);
  const previewStr: TemplateResult | null = preview.length > 0
    ? html` <span class="log-detail-msg-preview">${preview}</span>`
    : null;

  return html`<details class="log-detail-collapsible">
    <summary><span class="log-detail-role ${roleClass}">${role}</span>${extraStr}${previewStr}</summary>
    <pre class="json-viewer log-detail-collapsible-body">${formatJson(msg)}</pre>
  </details>`;
}

function renderSingleToolDefinition(tool: unknown, index: number): TemplateResult {
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
}

function renderObjectRequestBody(obj: Record<string, unknown>): TemplateResult[] {
  const rendered = new Set<string>();
  const blocks: TemplateResult[] = [];

  for (const key of PINNED_REQUEST_KEYS) {
    if (!Object.prototype.hasOwnProperty.call(obj, key)) continue;
    const value = obj[key];
    if (isEmptyValue(value)) continue;

    if (key === "tools" && Array.isArray(value) && value.length > 0) {
      const toolBlocks: TemplateResult[] = (value as unknown[]).map((t, i) => renderSingleToolDefinition(t, i));
      blocks.push(html`<details class="log-detail-collapsible" open>
        <summary><span class="log-detail-key log-detail-key-pinned">tools</span> <span class="log-detail-key-meta">${value.length} tool(s)</span></summary>
        <div class="log-detail-messages">${toolBlocks}</div>
      </details>`);
      rendered.add(key);
      continue;
    }

    if (key === "messages" && Array.isArray(value) && value.length > 0) {
      const msgBlocks: TemplateResult[] = (value as unknown[]).map((raw, i) => renderSingleMessageCollapsible(raw, i));
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

  for (const key of Object.keys(obj)) {
    if (rendered.has(key)) continue;
    const value = obj[key];
    if (isEmptyValue(value)) continue;
    blocks.push(html`<details class="log-detail-collapsible">
      <summary><span class="log-detail-key">${key}</span></summary>
      <pre class="json-viewer log-detail-collapsible-body">${formatJson(value)}</pre>
    </details>`);
  }

  return blocks;
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
export function renderRequestTab(requestBody: unknown, createdAt?: string): TemplateResult {
  // Empty state: same predicate as the previous inline logic.
  const hasRequestBody: boolean = requestBody != null
    && !(typeof requestBody === "string" && requestBody.trim() === "")
    && !(typeof requestBody === "object" && requestBody !== null
      && !Array.isArray(requestBody) && Object.keys(requestBody as object).length === 0
      && JSON.stringify(requestBody) === "{}");
  if (!hasRequestBody) {
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
    const blocks = renderObjectRequestBody(body as Record<string, unknown>);
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
export function renderResponseTab(
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
    placeholder += computeTtlExpiryHint(createdAt);
    return html`<section class="log-detail-section" data-log-tab="response">
      <h4>Response</h4>
      <p class="muted log-detail-placeholder">${placeholder}</p>
    </section>`;
  }

  const showPartialBanner = detectPartialResponse(response, isPartial);
  const partialBanner: TemplateResult | null = showPartialBanner
    ? html`<div class="log-detail-partial-banner">⚠ Partial response — stream was interrupted before completion. The content below is what was received up to the point of failure.</div>`
    : null;

  const rawResponseBody = extractRawResponseBody(response);
  const rawResponseBodyBlock: TemplateResult | null = rawResponseBody
    ? renderRawResponseBodyBlock(rawResponseBody)
    : null;

  // String: try to parse as JSON; on success, recurse with parsed
  // value; on failure, show the raw string in a collapsible.
  if (typeof response === "string") {
    let parsed: Record<string, unknown> | null = null;
    try {
      parsed = JSON.parse(response) as Record<string, unknown>;
    } catch (_e: unknown) {
      parsed = null;
    }
    if (parsed != null && typeof parsed === "object") {
      return renderResponseTab(parsed, streamingHint, createdAt, isPartial);
    }
    return html`<section class="log-detail-section" data-log-tab="response">
      <h4>Response</h4>
      ${partialBanner}
      ${rawResponseBodyBlock}
      ${renderRawResponseBlock(response)}
    </section>`;
  }

  // Try to recognize an OpenAI chat-completion shape.
  const parsed = parseOpenAiChatResponse(response);
  const blocks = parsed != null
    ? renderParsedResponseBlocks(parsed, response)
    : renderFallbackResponseBlocks(response);

  return html`<section class="log-detail-section" data-log-tab="response">
    <h4>Response</h4>
    ${partialBanner}
    ${rawResponseBodyBlock}
    ${blocks}
  </section>`;
}
