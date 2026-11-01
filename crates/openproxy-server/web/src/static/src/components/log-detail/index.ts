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
import { renderEmbeddingVectorCards, type EmbeddingEntry } from "../embedding-vectors.js";
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

    if (key === "input") {
      // An embedding request is the only wire shape that carries a top-level
      // `input` holding text, ids, or multimodal parts. A dedicated renderer
      // avoids dumping a base64 blob into the generic JSON viewer.
      const embedInput = renderEmbeddingInput(value);
      if (embedInput != null) {
        blocks.push(embedInput);
        rendered.add(key);
        continue;
      }
    }

    blocks.push(html`<details class="log-detail-collapsible">
      <summary><span class="log-detail-key">${key}</span></summary>
      <pre class="json-viewer log-detail-collapsible-body">${formatJson(value)}</pre>
    </details>`);
  }

  return blocks;
}

// ---------- embedding input / output (log-detail) ----------

/** A media URL from a canonical embedding part, rendered as a bounded badge.
 *  `data:` URLs are shown only by kind and length — embedding one into the
 *  page would flash a megabyte of base64 into the DOM. A small inline image is
 *  the exception: a thumbnail is worth a thousand chars. */
const INLINE_IMAGE_MAX_CHARS = 262_144; // 256 KiB of base64

function renderEmbeddingMediaBadge(url: string): TemplateResult {
  const isData = url.startsWith("data:");
  const mime = isData ? url.slice(5, url.indexOf(";") > -1 ? url.indexOf(";") : url.indexOf(",")) : "";
  const isImage = url.startsWith("data:image/") || /\.(png|jpe?g|gif|webp|bmp|svg)(\?|$)/i.test(url);
  if (isImage) {
    // Show a thumbnail only for a reasonably-sized payload; a huge data URL
    // gets a badge naming it instead of a DOM-sized base64 blob.
    if (!isData || url.length <= INLINE_IMAGE_MAX_CHARS) {
      return html`<div class="log-detail-embedding-media">
        <img src=${url} alt="embedding input" loading="lazy" style="max-width:180px;max-height:180px;border-radius:var(--radius-sm);border:1px solid var(--color-border);display:block;" />
        <span class="log-detail-embedding-badge">${isData ? `inline image (${url.length.toLocaleString()} chars)` : "image URL"}</span>
      </div>`;
    }
    return html`<div class="log-detail-embedding-media">
      <span class="log-detail-embedding-badge">inline image (${url.length.toLocaleString()} chars)</span>
    </div>`;
  }
  const kind = isData && mime ? mime : "media";
  return html`<span class="log-detail-embedding-badge">${isData ? `inline ${kind} (${url.length.toLocaleString()} chars)` : html`<a href=${url} target="_blank" rel="noopener">${url}</a>`}</span>`;
}

/** One canonical `EmbeddingPart` ({type:"text"|"image"|"audio"|"video"|"document", …})
 *  or a dialect that names a `text`/`content`/`url` field. Returns null when the
 *  value is not a part the renderer understands, so the caller can fall back. */
function renderEmbeddingPart(part: unknown): TemplateResult | null {
  if (typeof part === "string") {
    return part.trim() === "" ? null : html`<div class="log-detail-embedding-text">${part}</div>`;
  }
  if (part == null || typeof part !== "object" || Array.isArray(part)) return null;
  const p = part as Record<string, unknown>;

  // Text first, matching the backend's normalization order.
  for (const key of ["text", "content"]) {
    const text = p[key];
    if (typeof text === "string" && text !== "") {
      return html`<div class="log-detail-embedding-text">${text}</div>`;
    }
  }

  // A media part: the canonical `url`, or the flattened dialect fields.
  const url =
    (typeof p["url"] === "string" ? p["url"] : null) ??
    (typeof p["image_url"] === "string" ? p["image_url"] : null) ??
    (p["image_url"] && typeof p["image_url"] === "object" && typeof (p["image_url"] as Record<string, unknown>)["url"] === "string"
      ? (p["image_url"] as Record<string, unknown>)["url"] as string : null) ??
    (p["file_url"] && typeof p["file_url"] === "object" && typeof (p["file_url"] as Record<string, unknown>)["url"] === "string"
      ? (p["file_url"] as Record<string, unknown>)["url"] as string : null);
  if (typeof url === "string" && url !== "") {
    const kind = typeof p["type"] === "string" ? p["type"] : "media";
    return html`<div class="log-detail-embedding-media-part">
      <span class="log-detail-embedding-badge">${kind}</span>
      ${renderEmbeddingMediaBadge(url)}
    </div>`;
  }
  return null;
}

/** A top-level `input` of an embedding request, or null when it does not match
 *  any shape the renderer can lay out — letting the generic JSON viewer keep it. */
function renderEmbeddingInput(value: unknown): TemplateResult | null {
  if (value == null) return null;
  if (typeof value === "string") {
    const text = value;
    return html`<details class="log-detail-collapsible" open>
      <summary><span class="log-detail-key">input</span> <span class="log-detail-key-meta">text</span></summary>
      <div class="log-detail-embedding-text">${text}</div>
    </details>`;
  }

  const parts: TemplateResult[] = [];
  const metaBits: string[] = [];

  const push = (node: TemplateResult | null) => { if (node != null) parts.push(node); };

  if (Array.isArray(value)) {
    if (value.length === 0) return null;
    const first = value[0];
    if (typeof first === "number") {
      return html`<details class="log-detail-collapsible">
        <summary><span class="log-detail-key">input</span> <span class="log-detail-key-meta">${value.length} token id(s)</span></summary>
        <pre class="json-viewer log-detail-collapsible-body">${formatJson(value)}</pre>
      </details>`;
    }
    if (typeof first === "string") {
      value.forEach((s, i) => push(s !== "" ? html`<div class="log-detail-embedding-text" title="item ${i + 1}">${s}</div>` : null));
      metaBits.push(`${value.length} text item(s)`);
    } else if (first != null && typeof first === "object") {
      // Either a parts array (PartArrays) or a single multimodal item (Parts).
      const isNestedBatch = Array.isArray(first);
      const items = isNestedBatch ? (value as unknown[]) : [value];
      items.forEach((item, itemIdx) => {
        const itemParts = Array.isArray(item) ? item : [item];
        const rendered = itemParts.map(renderEmbeddingPart).filter((x): x is TemplateResult => x != null);
        if (rendered.length === 0) return;
        const content = rendered.length === 1
          ? rendered[0]
          : html`<div class="log-detail-embedding-parts">${rendered}</div>`;
        push(html`<div class="log-detail-embedding-item" title="item ${itemIdx + 1}">${content}</div>`);
      });
      if (parts.length === 0) return null;
      metaBits.push(`${items.length} item(s)`);
    } else {
      return null;
    }
  } else if (typeof value === "object" && !Array.isArray(value)) {
    // A single part object not wrapped in an array (defensive).
    const node = renderEmbeddingPart(value);
    if (node == null) return null;
    push(node);
    metaBits.push("1 part");
  } else {
    return null;
  }

  const meta = metaBits.length > 0 ? html` <span class="log-detail-key-meta">${metaBits.join(" · ")}</span>` : null;
  return html`<details class="log-detail-collapsible" open>
    <summary><span class="log-detail-key">input</span>${meta}</summary>
    <div class="log-detail-embedding-input">${parts}</div>
  </details>`;
}

/** The `data[]` block of an embeddings response as vector cards. */
function renderEmbeddingResponseLog(response: Record<string, unknown>): TemplateResult {
  const data = response["data"];
  if (!Array.isArray(data) || data.length === 0) {
    return html`<p class="muted">No embedding vectors in response.</p>`;
  }
  return html`<details class="log-detail-collapsible" open>
    <summary>Embedding vectors (${data.length})</summary>
    ${renderEmbeddingVectorCards(data as EmbeddingEntry[], html`${data.length} embedding(s)`)}
  </details>`;
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
  const blocks: TemplateResult[] = parsed != null
    ? renderParsedResponseBlocks(parsed, response)
    : isEmbeddingResponse(response)
      ? [renderEmbeddingResponseLog(response), renderRawResponseBlock(response)]
      : isDecisionResponse(response)
        ? [renderDecisionResponseLog(response), renderRawResponseBlock(response)]
        : renderFallbackResponseBlocks(response);

  return html`<section class="log-detail-section" data-log-tab="response">
    <h4>Response</h4>
    ${partialBanner}
    ${rawResponseBodyBlock}
    ${blocks}
  </section>`;
}

/** A response body that is a decision / SystemOne answer map (`{answers:{…}}`). */
function isDecisionResponse(response: unknown): response is Record<string, unknown> {
  if (response == null || typeof response !== "object" || Array.isArray(response)) return false;
  const r = response as Record<string, unknown>;
  const answers = r["answers"];
  return answers != null && typeof answers === "object" && !Array.isArray(answers);
}

/** Structured collapsible for decision / SystemOne answers. */
function renderDecisionResponseLog(response: Record<string, unknown>): TemplateResult {
  const answers = (response["answers"] || {}) as Record<string, unknown>;
  const entries = Object.entries(answers);
  if (entries.length === 0) {
    return html`<p class="muted">No decision answers in response.</p>`;
  }
  return html`<details class="log-detail-collapsible" open>
    <summary><span class="log-detail-key">Decisions / Answers</span> <span class="log-detail-key-meta">${entries.length} question(s)</span></summary>
    <div class="log-detail-collapsible-body" style="display: flex; flex-direction: column; gap: var(--space-2); margin-top: var(--space-2);">
      ${entries.map(([key, val]) => {
        const v = val as Record<string, unknown>;
        const choice = v?.["choice"] ?? v?.["response"] ?? "—";
        const conf = typeof v?.["confidence"] === "number" ? Math.round((v["confidence"] as number) * 100) : null;
        const probs = v?.["probabilities"] && typeof v["probabilities"] === "object" ? v["probabilities"] as Record<string, number> : null;
        const isYes = String(choice).toLowerCase() === "yes";
        const badgeColor = isYes ? "var(--color-success)" : "var(--color-text-muted)";
        const badgeBg = isYes ? "rgba(16, 185, 129, 0.15)" : "var(--color-surface-2)";

        return html`
          <div style="display: flex; flex-direction: column; gap: var(--space-1); padding: var(--space-2); background: var(--color-surface-2); border-radius: var(--radius-sm); border: 1px solid var(--color-border);">
            <div style="display: flex; align-items: center; justify-content: space-between;">
              <span style="font-weight: 600; font-family: var(--font-mono); font-size: var(--fs-sm); color: var(--color-text);">${key}</span>
              <span class="badge" style="font-weight: 700; font-family: var(--font-mono); font-size: var(--fs-xs); padding: 2px 8px; border-radius: 4px; background: ${badgeBg}; color: ${badgeColor}; border: 1px solid var(--color-border);">${String(choice).toUpperCase()}</span>
            </div>
            ${conf != null || probs != null ? html`
              <div style="display: flex; gap: var(--space-3); font-size: var(--fs-xs); color: var(--color-text-muted); margin-top: 2px;">
                ${conf != null ? html`<span>confidence: <strong style="color: var(--color-text);">${conf}%</strong></span>` : null}
                ${probs != null ? Object.entries(probs).map(([opt, p]) => html`<span>${opt}: ${Math.round(p * 100)}%</span>`) : null}
              </div>
            ` : null}
          </div>
        `;
      })}
    </div>
  </details>`;
}

/** A response body that is an embedding vector list (`{data:[{embedding:[…]}]}`),
 *  detected before the generic fallback so the Response tab can draw the same
 *  vector visualization the playground uses. */
function isEmbeddingResponse(response: unknown): response is Record<string, unknown> {
  if (response == null || typeof response !== "object" || Array.isArray(response)) return false;
  const r = response as Record<string, unknown>;
  const data = r["data"];
  if (!Array.isArray(data)) return false;
  return data.some(
    (e) => e != null && typeof e === "object" && !Array.isArray(e)
      && Array.isArray((e as Record<string, unknown>)["embedding"]),
  );
}
