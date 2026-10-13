// Recognizes OpenAI / Anthropic response shapes, extracts structured content
// (message, reasoning, tool calls, other properties), and renders the
// per-response collapsible blocks the Response tab composes. Depends only on
// json-prune.ts (formatJson).

import { html, type TemplateResult } from "lit-html";
import { formatJson } from "./json-prune.js";
import { renderLogMessageText } from "./message-content.js";


/** Shown in the Response tab when `response_body_json` is null. */
export const NO_RESPONSE_PLACEHOLDER_TEXT =
  "No response body recorded.";

/** One `choices[0].message.tool_calls[i]` entry. `arguments` is commonly a
 *  JSON-encoded string, hence `unknown`. */
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

/** Recognize an OpenAI chat-completion shape, or null so the caller can fall
 *  through to a "Raw response" block. Only ever called with a non-string value;
 *  renderResponseTab normalizes strings first.
 *
 *  `otherProperties` merges response-level metadata (id, model, object,
 *  created, usage, system_fingerprint, service_tier) with choice-level
 *  metadata (index, finish_reason, logprobs). Without it, a `finish_reason:
 *  "tool_calls"` response whose calls arrived in an earlier streamed chunk
 *  would show only "Raw response", hiding the usage and finish reason that
 *  prove the request succeeded. */
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

/** Pretty-print a tool-call `arguments` field. The result goes into a `<pre>`,
 *  which lit-html escapes. */
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

/** A tool-call arguments block with structured collapsibles when the arguments
 *  parse as an object. Null when there is nothing to render, and the caller
 *  omits the block. */
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
  // Fall back to the raw pretty-print.
  const result = parseToolCallArguments(args);
  return result != null
    ? html`<pre class="json-viewer log-detail-collapsible-body">${result.pretty}</pre>`
    : null;
}

/** A "Message" (content) block, or null when empty so the caller omits the
 *  section entirely. */
function renderMessageBlock(message: string): TemplateResult {
  return html`<details class="log-detail-collapsible" open>
    <summary>Message</summary>
    ${renderLogMessageText(message)}
  </details>`;
}

/** A "Reasoning" block. Same omit-when-empty contract as renderMessageBlock. */
function renderReasoningBlock(reasoning: string): TemplateResult {
  return html`<details class="log-detail-collapsible" open>
    <summary>Reasoning</summary>
    ${renderLogMessageText(reasoning)}
  </details>`;
}

/** One tool call as its own top-level collapsible in the Response tab, not
 *  nested under a parent "Tool calls" block, so each expands independently and
 *  the tab stays short with many calls.
 *
 *  `index` is the 0-based position in tool_calls, used only to label the
 *  summary so the operator can match the upstream's index field. */
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
    // No arguments: still render a non-collapsible header, since the call's
    // existence carries information.
  return html`<div class="log-detail-tool-call">
    <div class="log-detail-tool-call-header">
      <span class="log-detail-tool-call-name">Tool call #${index + 1}: ${tc.function.name}</span>${idTag}${typeTag}
    </div>
  </div>`;
}

/** "Other properties": every response- and choice-level field outside the
 *  structured content / reasoning / tool_calls extraction. Collapsed by
 *  default, null when empty. */
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

/** "Raw response", always collapsed: the escape hatch for whatever the
 *  structured blocks did not show. */
export function renderRawResponseBlock(response: unknown): TemplateResult {
  return html`<details class="log-detail-collapsible">
    <summary>Raw response</summary>
    <pre class="json-viewer log-detail-collapsible-body">${formatJson(response)}</pre>
  </details>`;
}

/** Raw response stream body, open by default for debugging interrupted or
 *  empty streams. */
export function renderRawResponseBodyBlock(rawBody: string): TemplateResult {
  return html`<div class="log-detail-raw-stream-captured" style="margin-bottom: var(--space-3);">
    <h5 style="margin: 0 0 var(--space-1) 0; font-size: var(--fs-sm); color: var(--color-text-muted); font-weight: 600; text-transform: uppercase; letter-spacing: 0.05em;">Captured Raw Upstream Stream (Interrupted / Empty)</h5>
    <pre class="json-viewer" style="white-space: pre-wrap; word-break: break-all; max-height: 400px; overflow-y: auto; background: var(--color-surface-2); font-family: var(--font-mono); font-size: var(--fs-xs); padding: var(--space-2); border-radius: var(--radius-sm); border: 1px solid var(--color-border);">${rawBody}</pre>
  </div>`;
}

/** Whether `response` was cut short, from the `partial` marker the backend
 *  writes in the first choice's message/delta, plus the caller's
 *  `isPartial` hint (`is_streaming && !stream_complete`). */
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

/** Count tool_use / tool_result blocks in an Anthropic-style message and
 *  collect the tool_result IDs, for the Request tab's message badges. */
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
