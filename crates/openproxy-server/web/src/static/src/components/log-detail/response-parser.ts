// components/log-detail/response-parser.ts — recognizes OpenAI / Anthropic
// response shapes, extracts structured content (message, reasoning, tool
// calls, other properties) and renders the per-response collapsible blocks
// that the log-detail Response tab composes.
//
// Depends only on json-prune.ts (formatJson) — no cycles.
//
// Split out of the former components/log-detail.ts monolith (Q19).

import { html, type TemplateResult } from "lit-html";
import { formatJson } from "./json-prune.js";

// ---- SPEC_LOG_DETAIL_MODAL constants and renderers ----

/** Displayed in the Response tab when `response_body_json` is null
 *  (i.e. for requests where the response was not recorded —
 *  recording was off, or the request was cancelled before a
 *  response arrived). */
export const NO_RESPONSE_PLACEHOLDER_TEXT =
  "No response body recorded.";

/** A single tool call extracted from an OpenAI chat-completion
 *  response's `choices[0].message.tool_calls[i]`. The `arguments`
 *  field is commonly a JSON-encoded string, hence `unknown`. */
interface ToolCall {
  id?: string;
  type?: string;
  function: { name: string; arguments: unknown };
}

function isMeaningfulProperty(val: unknown): boolean {
  if (val == null) return false;
  if (typeof val === "string") return val.length > 0;
  if (typeof val === "object" && !Array.isArray(val)) {
    return Object.keys(val as object).length > 0;
  }
  return true;
}

function collectFilteredProperties(
  source: Record<string, unknown>,
  ignoredKeys: Set<string>,
  prefix = "",
): Record<string, unknown> {
  const result: Record<string, unknown> = {};
  for (const [k, val] of Object.entries(source)) {
    if (ignoredKeys.has(k) || !isMeaningfulProperty(val)) continue;
    result[`${prefix}${k}`] = val;
  }
  return result;
}

function extractSingleToolCall(raw: unknown): ToolCall | null {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null;
  const tc = raw as Record<string, unknown>;
  const fn = tc["function"];
  if (!fn || typeof fn !== "object" || Array.isArray(fn)) return null;
  const fnObj = fn as Record<string, unknown>;
  const name: unknown = fnObj["name"];
  if (typeof name !== "string") return null;

  const tcCall: ToolCall = { function: { name, arguments: fnObj["arguments"] ?? null } };
  if (typeof tc["id"] === "string") tcCall.id = tc["id"];
  if (typeof tc["type"] === "string") tcCall.type = tc["type"];
  return tcCall;
}

function extractToolCalls(rawToolCalls: unknown): ToolCall[] {
  if (!Array.isArray(rawToolCalls)) return [];
  const calls: ToolCall[] = [];
  for (const raw of rawToolCalls) {
    const call = extractSingleToolCall(raw);
    if (call) calls.push(call);
  }
  return calls;
}

function extractReasoning(messageObj: Record<string, unknown> | null): string | null {
  if (!messageObj) return null;
  const candidates: unknown[] = [
    messageObj["reasoning_content"],
    messageObj["reasoning"],
    messageObj["reasoning_text"],
  ];
  for (const candidate of candidates) {
    if (typeof candidate === "string" && candidate.length > 0) {
      return candidate;
    }
  }
  return null;
}

function extractMessageContent(
  choiceMessage: Record<string, unknown> | null,
  choice: Record<string, unknown>,
): string | null {
  let message: string | null = null;
  if (choiceMessage != null) {
    const c: unknown = choiceMessage["content"];
    if (typeof c === "string") {
      message = c;
    } else if (c == null) {
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
  return message && message.length > 0 ? message : null;
}

const TOP_LEVEL_RESPONSE_IGNORED_KEYS = new Set(["choices"]);
const CHOICE_RESPONSE_IGNORED_KEYS = new Set(["message", "delta", "text"]);

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
export function parseOpenAiChatResponse(value: unknown): {
  message: string | null;
  reasoning: string | null;
  toolCalls: ToolCall[];
  otherProperties: Record<string, unknown> | null;
} | null {
  if (value == null || typeof value !== "object" || Array.isArray(value)) return null;
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

  const message = extractMessageContent(choiceMessage, choice);
  const reasoning = extractReasoning(choiceMessage);
  const toolCalls = choiceMessage != null ? extractToolCalls(choiceMessage["tool_calls"]) : [];

  const otherProperties: Record<string, unknown> = {
    ...collectFilteredProperties(v, TOP_LEVEL_RESPONSE_IGNORED_KEYS),
    ...collectFilteredProperties(choice, CHOICE_RESPONSE_IGNORED_KEYS, "choice."),
  };
  const otherPropsNonNull = Object.keys(otherProperties).length > 0 ? otherProperties : null;

  if (message == null && reasoning == null && toolCalls.length === 0 && otherPropsNonNull == null) {
    return null;
  }
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
export function renderRawResponseBlock(response: unknown): TemplateResult {
  return html`<details class="log-detail-collapsible">
    <summary>Raw response</summary>
    <pre class="json-viewer log-detail-collapsible-body">${formatJson(response)}</pre>
  </details>`;
}

/** Render the raw response stream body block (always open by default for debugging interrupted / empty streams). */
export function renderRawResponseBodyBlock(rawBody: string): TemplateResult {
  return html`<div class="log-detail-raw-stream-captured" style="margin-bottom: var(--space-md);">
    <h5 style="margin: 0 0 var(--space-xs) 0; font-size: var(--font-sm); color: var(--text-muted); font-weight: 600; text-transform: uppercase; letter-spacing: 0.05em;">Captured Raw Upstream Stream (Interrupted / Empty)</h5>
    <pre class="json-viewer" style="white-space: pre-wrap; word-break: break-all; max-height: 400px; overflow-y: auto; background: var(--bg-surface-2); font-family: var(--font-mono); font-size: var(--font-xs); padding: var(--space-sm); border-radius: var(--radius-sm); border: 1px solid var(--border-color);">${rawBody}</pre>
  </div>`;
}

/** Detect whether `response` was interrupted mid-stream by reading the
 *  `partial` marker the backend writes inside the first choice's
 *  message/delta. The `isPartial` arg is a caller-provided hint (it
 *  reads `is_streaming && !stream_complete`).
 *
 *  @param isPartial - When true, the response was interrupted
 *        mid-stream — show a "Partial response" banner so the
 *        operator knows the response didn't complete normally even
 *        though there IS a body to inspect. Passed from the caller
 *        which reads `is_streaming && !stream_complete` (and the
 *        `partial` marker inside the JSON, when present). */
export function detectPartialResponse(response: unknown, isPartial?: boolean): boolean {
  if (isPartial === true) return true;
  if (typeof response !== "object" || response === null || Array.isArray(response)) return false;
  const choices = (response as Record<string, unknown>)["choices"];
  if (!Array.isArray(choices) || choices.length === 0) return false;
  const c0 = choices[0] as Record<string, unknown> | undefined;
  if (!c0 || typeof c0 !== "object") return false;
  const msg = (c0["message"] ?? c0["delta"]) as Record<string, unknown> | undefined;
  if (!msg || typeof msg !== "object") return false;
  return msg["partial"] === true;
}

export function extractRawResponseBody(response: unknown): string | null {
  if (typeof response !== "object" || response === null || Array.isArray(response)) return null;
  const r = response as Record<string, unknown>;
  if (typeof r["raw_response_body"] === "string") return r["raw_response_body"];

  const choices = r["choices"];
  if (!Array.isArray(choices) || choices.length === 0) return null;
  const c0 = choices[0];
  if (!c0 || typeof c0 !== "object" || Array.isArray(c0)) return null;
  const choice = c0 as Record<string, unknown>;
  const msg = choice["message"] ?? choice["delta"] ?? null;
  if (!msg || typeof msg !== "object" || Array.isArray(msg)) return null;
  const m = msg as Record<string, unknown>;
  return typeof m["raw_response_body"] === "string" ? m["raw_response_body"] : null;
}

export function computeTtlExpiryHint(createdAt?: string, ttlSec = 300, bodyName = "response bodies"): string {
  if (createdAt == null || createdAt === "—") return "";
  const created = new Date(createdAt).getTime();
  if (Number.isNaN(created)) return "";
  const ageSec = (Date.now() - created) / 1000;
  if (ageSec <= ttlSec) return "";
  return ` This log is ${Math.round(ageSec / 60)} min old — ${bodyName} are pruned after the recording TTL (5 min default).`;
}

export function renderParsedResponseBlocks(
  parsed: { message: string | null; reasoning: string | null; toolCalls: ToolCall[]; otherProperties: Record<string, unknown> | null },
  response: unknown,
): TemplateResult[] {
  const blocks: TemplateResult[] = [];
  if (parsed.message != null) {
    blocks.push(renderMessageBlock(parsed.message));
  }
  if (parsed.reasoning != null) {
    blocks.push(renderReasoningBlock(parsed.reasoning));
  }
  for (let i = 0; i < parsed.toolCalls.length; i++) {
    const tc = parsed.toolCalls[i];
    if (tc != null) blocks.push(renderToolCallBlock(tc, i));
  }
  if (parsed.otherProperties != null) {
    const otherBlock = renderOtherPropertiesBlock(parsed.otherProperties);
    if (otherBlock != null) blocks.push(otherBlock);
  }
  blocks.push(renderRawResponseBlock(response));
  return blocks;
}

function extractFallbackResponseData(response: unknown): {
  content: string | null;
  reasoning: string | null;
  toolCalls: ToolCall[];
  otherProperties: Record<string, unknown> | null;
} {
  if (typeof response !== "object" || response === null || Array.isArray(response)) {
    return { content: null, reasoning: null, toolCalls: [], otherProperties: null };
  }
  const obj = response as Record<string, unknown>;
  let content: string | null = null;
  let reasoning: string | null = null;
  let toolCalls: ToolCall[] = [];
  let otherProperties: Record<string, unknown> | null = null;

  const choices: unknown = obj["choices"];
  if (Array.isArray(choices) && choices.length > 0) {
    const c0 = choices[0] as Record<string, unknown> | undefined;
    if (c0 && typeof c0 === "object") {
      const msg = (c0["message"] ?? c0["delta"]) as Record<string, unknown> | undefined;
      if (msg && typeof msg === "object") {
        if (typeof msg["content"] === "string" && msg["content"].length > 0) {
          content = msg["content"];
        }
        reasoning = extractReasoning(msg);
        if (Array.isArray(msg["tool_calls"])) {
          toolCalls = extractToolCalls(msg["tool_calls"]);
        }
      }
      const choiceProps = collectFilteredProperties(c0, CHOICE_RESPONSE_IGNORED_KEYS, "choice.");
      if (Object.keys(choiceProps).length > 0) {
        otherProperties = { ...(otherProperties ?? {}), ...choiceProps };
      }
    }
  }

  const topLevelProps = collectFilteredProperties(obj, TOP_LEVEL_RESPONSE_IGNORED_KEYS);
  if (Object.keys(topLevelProps).length > 0) {
    otherProperties = { ...(otherProperties ?? {}), ...topLevelProps };
  }

  return { content, reasoning, toolCalls, otherProperties };
}

export function renderFallbackResponseBlocks(response: unknown): TemplateResult[] {
  const { content, reasoning, toolCalls, otherProperties } = extractFallbackResponseData(response);
  const blocks: TemplateResult[] = [];
  if (content != null) blocks.push(renderMessageBlock(content));
  if (reasoning != null) blocks.push(renderReasoningBlock(reasoning));
  for (let i = 0; i < toolCalls.length; i++) {
    const tc = toolCalls[i];
    if (tc != null) blocks.push(renderToolCallBlock(tc, i));
  }
  if (otherProperties != null && Object.keys(otherProperties).length > 0) {
    const otherBlock = renderOtherPropertiesBlock(otherProperties);
    if (otherBlock != null) blocks.push(otherBlock);
  }
  blocks.push(renderRawResponseBlock(response));
  return blocks;
}

/** Count tool_use / tool_result blocks in an Anthropic-style message
 *  and collect the tool_result IDs. Used by the Request tab to badge
 *  Anthropic messages (consumed by index.ts). */
export function extractAnthropicToolStats(content: unknown): {
  toolUses: number;
  toolResults: number;
  resultIds: string[];
} {
  let toolUses = 0;
  let toolResults = 0;
  const resultIds: string[] = [];

  if (Array.isArray(content)) {
    for (const block of content) {
      if (block && typeof block === "object") {
        const b = block as Record<string, unknown>;
        if (b["type"] === "tool_use") toolUses++;
        if (b["type"] === "tool_result") {
          toolResults++;
          if (typeof b["tool_use_id"] === "string") resultIds.push(b["tool_use_id"]);
        }
      }
    }
  }

  return { toolUses, toolResults, resultIds };
}
