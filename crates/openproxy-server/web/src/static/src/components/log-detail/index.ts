// Public facade plus the Request / Response tab renderers. The rest is
// re-exported so external callers (views/logs.ts, handlers/registry.ts) keep
// importing from "../components/log-detail.js".
//
// KNOWN CYCLE (index ↔ modal): modal.ts calls the tab renderers below while
// composing the modal template; this module touches modal.ts only through
// `export { ... } from "./modal.js"`. The cycle resolves under ESM live
// bindings because neither side calls the other at module-evaluation time.

import { html, type TemplateResult } from "lit-html";
import { formatJson } from "./json-prune.js";
import { renderLogMessageContent, renderLogMessagePayload } from "./message-content.js";
import {
  computeTtlExpiryHint,
  detectPartialResponse,
  extractAnthropicToolStats,
  extractRawResponseBody,
  NO_RESPONSE_PLACEHOLDER_TEXT,
  parseResponsePayload,
  renderFallbackResponseBlocks,
  renderParsedResponseBlocks,
  renderRawResponseBlock,
  renderRawResponseBodyBlock,
} from "./response-parser.js";

// Public API
export {
  parseAnthropicMessagesResponse,
  parseOpenAiChatResponse,
  parseResponsePayload,
} from "./response-parser.js";
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

/** String status ("ok" | "error" | "timeout") to a CSS pill class. Distinct
 *  from the numeric HTTP-code mapping in lib/constants.ts. */

/** Maps string status values ("ok"|"error"|"timeout"...) to CSS pill classes.
 *  Note: distinct from lib/constants.ts statusPillClass which maps numeric HTTP codes. */
export function stringStatusPillClass(s: string | null | undefined): string {
  if (s === "ok" || s === "success") return "ok";
  if (s === "error" || s === "failed" || s === "unhealthy") return "err";
  if (s === "timeout" || s === "rate_limited" || s === "degraded") return "warn";
  return "warn";
}

/** String field of a record, or null. Used by modal.ts for meta lookups. */
export function readString(o: Record<string, unknown> | null | undefined, k: string): string | null {
  if (!o) return null;
  const v: unknown = o[k];
  return typeof v === "string" ? v : null;
}

/** A `<section data-log-tab>` with a pretty-printed JSON viewer, for the
 *  Errors / Raw tabs. */
export function jsonSection(title: string, value: unknown, tabKey: string): TemplateResult {
  return html`<section class="log-detail-section" data-log-tab=${tabKey}>
    <h4>${title}</h4>
    <pre class="json-viewer">${formatJson(value)}</pre>
  </section>`;
}

/** Request-body keys rendered first, in this order, tagged
 *  `log-detail-key-pinned`. */
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
    ${renderLogMessagePayload(msg)}
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
      ${key === "system" ? renderLogMessageContent(value)
        : html`<pre class="json-viewer log-detail-collapsible-body">${formatJson(value)}</pre>`}
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

/** Request tab, handling the empty / object / non-object body shapes.
 *
 *  With a null body, `createdAt` dates the row so the message can say whether
 *  the TTL prune took it (past 5 min) or recording was simply off. */
export function renderRequestTab(requestBody: unknown, createdAt?: string): TemplateResult {

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


  let body: unknown = requestBody;
  if (typeof body === "string") {
    const trimmed = body.trimStart();
    if (trimmed.startsWith("{") || trimmed.startsWith("[")) {
      try { body = JSON.parse(body); }
      catch (_e: unknown) { /* fall through with the raw string */ }
    }
  }


  if (body != null && typeof body === "object" && !Array.isArray(body)) {
    const blocks = renderObjectRequestBody(body as Record<string, unknown>);
    return html`<section class="log-detail-section" data-log-tab="request">
      <h4>Request</h4>
      ${blocks}
    </section>`;
  }


  return html`<section class="log-detail-section" data-log-tab="request">
    <h4>Request</h4>
    <details class="log-detail-collapsible" open>
      <summary>Raw request body</summary>
      <pre class="json-viewer log-detail-collapsible-body">${formatJson(body)}</pre>
    </details>
  </section>`;
}

/** Response tab for null, string, and object inputs.
 *
 *  Sections are independent: message, reasoning, one collapsible per tool
 *  call, remaining properties, and a raw block that is always present even
 *  when content and tool_calls are both empty, so a `finish_reason:
 *  "tool_calls"` response whose calls arrived in an earlier streamed chunk
 *  still shows evidence the request succeeded.
 *
 *  `streamingHint` marks a streaming request with no body. `isPartial` marks
 *  a stream cut short, which raises the partial banner even though a body
 *  exists to inspect. */
export function renderResponseTab(
  response: unknown,
  streamingHint?: boolean,
  createdAt?: string,
  isPartial?: boolean,
): TemplateResult {

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


  const parsed = parseResponsePayload(response);
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
