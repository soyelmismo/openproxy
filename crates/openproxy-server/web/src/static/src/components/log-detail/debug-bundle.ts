// components/log-detail/debug-bundle.ts — builds Markdown "debug bundles"
// for the log-detail modal, handles clipboard copy and the last-resort
// modal fallback.
//
// Depends on state.ts (LogDetailLog, pinned identity) — no cycles.
//
// Split out of the former components/log-detail.ts monolith (Q19).

import { html, render } from "lit-html";
import { showToast } from "../toast.js";
import { copyToClipboard } from "../../lib/clipboard.js";
import { state } from "../../state/index.js";
import { liveLogsStore } from "../../state/live-logs-store.js";
import type { LogDetailLog } from "./state.js";

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
  // Also truncate the `system` field if present
  if (obj["system"] != null) {
    const sys = obj["system"];
    const MAX_MSG_LEN = 500;
    if (typeof sys === "string" && sys.length > MAX_MSG_LEN) {
      obj["system"] = sys.slice(0, MAX_MSG_LEN) + `… [truncated, ${sys.length - MAX_MSG_LEN} more chars]`;
    } else if (Array.isArray(sys)) {
      const truncatedSys: unknown[] = [];
      const showCount = Math.min(sys.length, 10);
      for (let i = 0; i < showCount; i++) {
        const block = sys[i];
        if (block && typeof block === "object" && !Array.isArray(block)) {
          const b = { ...(block as Record<string, unknown>) };
          const text = b["text"];
          if (typeof text === "string" && text.length > MAX_MSG_LEN) {
            b["text"] = text.slice(0, MAX_MSG_LEN) + `… [truncated, ${text.length - MAX_MSG_LEN} more chars]`;
          }
          truncatedSys.push(b);
        } else {
          truncatedSys.push(block);
        }
      }
      if (sys.length > 10) {
        truncatedSys.push(`… [${sys.length - 10} more system blocks omitted]`);
      }
      obj["system"] = truncatedSys;
    }
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
    const attempt = state.logs.selectedIdentity ? liveLogsStore.selectDetail(state.logs.selectedIdentity) : null;
    const stageEvent: unknown = attempt;
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
  const bundleEndpointKind: string = (log.endpoint_kind || (detail["endpoint_kind"] as string) || "chat").toLowerCase();
  const bundleEndpointPath: string = bundleEndpointKind === "audio"
    ? "/v1/audio/transcriptions"
    : bundleEndpointKind === "image"
    ? "/v1/images/generations"
    : bundleEndpointKind === "embedding"
    ? "/v1/embeddings"
    : bundleEndpointKind === "video"
    ? "/v1/video/generations"
    : "/v1/chat/completions";
  lines.push(`- **Endpoint:** POST ${bundleEndpointPath} (${bundleEndpointKind})`);
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

  // Raw log row (everything we have, cleaned of nested duplicates).
  const cleanLog: Record<string, unknown> = Object.assign({}, log as Record<string, unknown>);
  delete cleanLog["detail"];
  if (Array.isArray(cleanLog["stages"])) {
    cleanLog["stages"] = (cleanLog["stages"] as Record<string, unknown>[]).map((s: Record<string, unknown>) => {
      if (s && typeof s === "object") {
        const copy = { ...s };
        delete copy["row"];
        delete copy["detail"];
        return copy;
      }
      return s;
    });
  }

  lines.push("## Raw Log Row");
  lines.push("");
  lines.push("```json");
  lines.push(truncateForBundle(JSON.stringify(cleanLog, null, 2)));
  lines.push("```");
  lines.push("");

  return lines.join("\n");
}

export async function copyRawJson(rawJson: unknown, _e?: Event): Promise<void> {
  const text = typeof rawJson === "string" ? rawJson : JSON.stringify(rawJson, null, 2);
  try {
    await copyToClipboard(text);
    showToast("Log JSON copiado al portapapeles.", "success");
  } catch (_err) {
    showToast("No se pudo copiar el JSON al portapapeles.", "warning");
  }
}

/** Last-resort fallback: show the bundle in a modal window so the
 *  user can manually select and copy the text. Used when
 *  `navigator.clipboard` is unavailable or fails.
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

/** Handler for the "Copy debug bundle" button. Reads the currently-
 *  selected log row from `state.logs.selectedRow`, builds the
 *  bundle, and writes it to the clipboard. Shows a toast for
 *  success / failure.
 *
 *  Delegates the clipboard write (and its HTTP fallback) to
 *  `lib/clipboard.ts`. If both paths fail, shows the bundle in a
 *  modal so the user can manually select+copy. */
export async function copyDebugBundle(): Promise<void> {
  const attempt = state.logs.selectedIdentity ? liveLogsStore.selectDetail(state.logs.selectedIdentity) : null;
  let row: LogDetailLog | null = null;
  if (attempt) {
    const safeAttempt = { ...attempt, detail: undefined, row: undefined };
    if (attempt.row) {
      row = { ...attempt.row, stages: [safeAttempt] } as LogDetailLog;
    } else {
      row = {
        id: attempt.rowId,
        request_id: attempt.requestId,
        trace_id: attempt.traceId,
        status_code: attempt.statusCode,
        total_ms: attempt.elapsedMsAtEvent,
        provider_id: attempt.providerId,
        upstream_model_id: attempt.upstreamModelId,
        stages: [safeAttempt]
      } as LogDetailLog;
    }
  }

  if (!row) {
    showToast("No log row selected.", "warning");
    return;
  }
  const bundle: string = buildDebugBundle(row);
  try {
    await copyToClipboard(bundle);
    showToast("Debug bundle copied to clipboard.", "success");
  } catch (_err) {
    // Last-resort fallback: Show the bundle in a modal so the user can manually select+copy.
    showBundleInModal(bundle, "Copy failed — select the text below and press Ctrl+C");
    showToast("Copy unavailable — bundle shown in a window for manual copy.", "warning");
  }
}
