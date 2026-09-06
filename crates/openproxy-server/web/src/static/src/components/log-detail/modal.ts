// components/log-detail/modal.ts — the log-detail modal lifecycle, template
// and tab UI. Owns `renderLogDetailModal` (the public template), the
// open/close/re-render lifecycle (openLogDetail, closeLogDetailModal,
// removeLogDetailModal, renderModal, updateOpenLogDetail, showLogDetail),
// the tab click handling (logDetailTabClick / initializeLogDetailTabs), the
// clock-tick re-render subscription, and the E2E test window hook.
//
// KNOWN CYCLE (index ↔ modal): this module imports the tab-body renderers
// (renderRequestTab, renderResponseTab, jsonSection, statusPillClass,
// readString) from index.ts to compose the modal template; index.ts only
// references this module via `export { ... }` re-export statements (it never
// *calls* anything here). The cycle is safe because modal.ts calls those
// hoisted function declarations only at render time (never during module
// evaluation) and index.ts makes no runtime calls into modal.ts.
//
// Split out of the former components/log-detail.ts monolith (Q19).

import { html, render, type TemplateResult } from "lit-html";
import { state } from "../../state/index.js";
import { ensureModalRoot } from "../../lib/ui-utils.js";
import { icons, endpointIcon } from "../../lib/icons.js";
import { liveLogsStore } from "../../state/live-logs-store.js";
import { clockStore } from "../../state/clock-store.js";
import {
  bumpOpenLogDetailGeneration,
  clearPinnedIdentity,
  getActiveLogDetailTab,
  isCurrentOpenLogDetailGeneration,
  setActiveLogDetailTab,
  setPinnedIdentity,
  type LogDetailLog,
} from "./state.js";
import { copyDebugBundle, copyRawJson } from "./debug-bundle.js";
import {
  jsonSection,
  readString,
  renderRequestTab,
  renderResponseTab,
  statusPillClass,
} from "./index.js";

export function renderLogDetailModal(log: LogDetailLog): TemplateResult {
  // Normalize the row shape: the backend's /usage/detail returns
  // status_code, total_ms, upstream_model_id, etc., but the modal
  // originally assumed a richer payload (status, latency_ms, model,
  // cost, requests, response, errors, meta). Map the backend's
  // canonical field names onto the modal's expected shape so a row
  // from the table — which only has the live-update shape — looks
  // the same in the modal as a row from /usage/detail.
  const detail: Record<string, unknown> = (log.detail as Record<string, unknown>) || {};
  const meta: Record<string, unknown> = (log.meta as Record<string, unknown>) || (detail["meta"] as Record<string, unknown>) || (log as Record<string, unknown>);
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

  const isInflight: boolean = log.id === 0 || log.id == null;
  const attempt = log.stages?.[0] as Record<string, unknown> | undefined;
  const synthesizedError = isInflight
    ? (attempt ? `Request in progress — current stage: ${attempt['stage']}` : "Request in progress...")
    : null;

  const errors: unknown = log.errors
    || log.error
    || log.error_msg
    || log.error_message
    || log.error_msg_redacted
    || log.error_message_redacted
    || detailErrors
    || synthesizedError
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
  const comboText: string = combo != null ? String(combo) : "—";
  const endpointKind: string = (log.endpoint_kind || (detail["endpoint_kind"] as string) || (meta["endpoint_kind"] as string) || "chat").toLowerCase();
  const endpointPath: string = endpointKind === "audio"
    ? "/v1/audio/transcriptions"
    : endpointKind === "image"
    ? "/v1/images/generations"
    : endpointKind === "embedding"
    ? "/v1/embeddings"
    : endpointKind === "video"
    ? "/v1/video/generations"
    : "/v1/chat/completions";

  // TTFT & Latency calculation: "3,277 ms (ttft 8ms)"
  const ttftMs = (log as Record<string, unknown>)["time_to_first_token_ms"]
    ?? (log as Record<string, unknown>)["ttft_ms"]
    ?? (attempt as Record<string, unknown> | undefined)?.["ttft_ms"]
    ?? (meta as Record<string, unknown>)["ttft_ms"];
  const latVal = log.latency_ms ?? log.total_ms ?? log.elapsed_ms;
  const latencyDisplay = latVal != null
    ? `${latVal} ms${ttftMs != null ? ` (ttft ${ttftMs}ms)` : ""}`
    : "—";

  // Tokens calculation: "6,897↓ 150↑ (7,047 tot)"
  const promptTokens = log.prompt_tokens;
  const compTokens = log.completion_tokens;
  const totalTokens = (log as Record<string, unknown>)["total_tokens"] as number | undefined
    ?? ((promptTokens != null || compTokens != null) ? ((promptTokens ?? 0) + (compTokens ?? 0)) : null);
  const promptEstimated = log.prompt_tokens_estimated ? "≈" : "";
  const compEstimated = log.completion_tokens_estimated ? "≈" : "";
  const tokensDisplay = (promptTokens != null || compTokens != null || totalTokens != null)
    ? `${promptEstimated}${promptTokens != null ? promptTokens.toLocaleString() : "0"}↓ ${compEstimated}${compTokens != null ? compTokens.toLocaleString() : "0"}↑ (${totalTokens != null ? totalTokens.toLocaleString() : "0"} tot)`
    : "—";

  // Speed calculation: "45.9 tok/s"
  const speedDisplay = log.tokens_per_sec != null ? `${log.tokens_per_sec.toFixed(1)} tok/s` : "—";

  // Cost calculation: "$0.0000"
  const costDisplay = costRaw != null
    ? (typeof costRaw === "number" ? `$${costRaw.toFixed(4)}` : `$${Number(costRaw).toFixed(4)}`)
    : "$0.0000";

  const apiKeyDisplay = apiKeyId != null ? `#${String(apiKeyId)}` : "—";

  // Compression savings info
  const pct = log.compression_savings_pct ?? null;
  const tech = log.compression_techniques ?? "";
  const pctTextVal = pct != null ? (pct < 1 ? pct.toFixed(2) : Math.round(pct).toString()) : "";
  const compressionTooltip = pct != null && pct > 0
    ? `Savings: -${pctTextVal}% tok (BPE cl100k_base)${tech.length > 0 ? " — " + tech : ""}`
    : "";

  const currentActiveTab = getActiveLogDetailTab();

  return html`
    <div id="log-detail-modal" class="modal-bg log-detail-modal" @click=${(e: Event) => closeLogDetailModal(e)}>
      <div class="modal">
        <div class="modal-header">
          <div style="display:flex;align-items:center;gap:var(--space-2);min-width:0;flex:1 1 auto;overflow:hidden;">
            <h2 style="margin:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;">Log #${String(requestId)}</h2>
            <button type="button" class="log-detail-copy-bundle-btn" @click=${() => { void copyDebugBundle(); }} title="Copy a Markdown-formatted debug bundle with all request/response/error context — ready to paste into a bug report.">${icons.copy()} Copy debug bundle</button>
          </div>
          <button type="button" class="close-btn" @click=${(e: Event) => closeLogDetailModal(e)} aria-label="Close">${icons.close()}</button>
        </div>
        <div class="modal-body">
          <!-- Resumen: Desktop 4 columnas -->
          <div class="log-detail-summary desktop-summary">
            <div><strong>Status:</strong> <span class="status-pill ${statusClass}">${String(status)}</span></div>
            <div><strong>Endpoint:</strong> <span title="HTTP Entry: POST ${endpointPath} (${endpointKind})"><code style="font-size:0.85em;padding:1px 4px;background:var(--color-surface-2);border-radius:0;">POST ${endpointPath}</code> <span class="log-type-tag log-type-tag--${endpointKind}" style="font-size:0.75em;padding:1px 5px;margin-left:4px;">${endpointIcon(endpointKind)} ${endpointKind}</span></span></div>
            <div><strong>Provider:</strong> ${String(provider)}</div>
            <div><strong>Model:</strong> ${String(model)}</div>
            <div><strong>Latency:</strong> <span class="mono-val">${latencyDisplay}</span></div>
            <div><strong>Tokens:</strong> <span class="mono-val"${compressionTooltip ? html` title=${compressionTooltip}` : ""}>${tokensDisplay}</span></div>
            <div><strong>Speed:</strong> <span class="mono-val">${speedDisplay}</span></div>
            <div><strong>Cost:</strong> <span class="mono-val">${costDisplay}</span></div>
            <div><strong>Account:</strong> ${String(account)}</div>
            <div><strong>Combo:</strong> ${comboText}</div>
            <div><strong>API Key:</strong> ${apiKeyDisplay}</div>
            <div><strong>Created:</strong> ${String(createdAt)}</div>
          </div>

          <!-- Resumen: Mobile 2x2 Mini-Cards -->
          <div class="mobile-modal-kpi-grid">
            <div class="m-kpi-card">
              <span class="kpi-label">Petición & Estado</span>
              <span class="kpi-val status-${statusClass}">${status} · ${endpointKind}</span>
              <span class="kpi-sub">POST ${endpointPath}</span>
            </div>
            <div class="m-kpi-card">
              <span class="kpi-label">Enrutamiento</span>
              <span class="kpi-val">${String(provider)}</span>
              <span class="kpi-sub mono">${String(model)}</span>
            </div>
            <div class="m-kpi-card">
              <span class="kpi-label">Rendimiento</span>
              <span class="kpi-val">${latencyDisplay}</span>
              <span class="kpi-sub">TTFT: ${ttftMs != null ? `${Number(ttftMs)}ms` : "0ms"} · ${speedDisplay}</span>
            </div>
            <div class="m-kpi-card">
              <span class="kpi-label">Uso & Metadata</span>
              <span class="kpi-val">${tokensDisplay}</span>
              <span class="kpi-sub">Key ${apiKeyDisplay} · ${costDisplay}</span>
            </div>
          </div>

          ${renderLogDetailTabs(currentActiveTab, log)}
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

function renderLogDetailTabs(currentTab: string, rawJson: unknown): TemplateResult {
  return html`
    <div class="tabs-toolbar tabs-and-actions-bar">
      <div class="tabs-group log-detail-tabs-group log-detail-tabs">
        <button class="detail-tab ${currentTab === "request" ? "active" : ""}" data-arg1="request" data-action="logDetailTab" @click=${(e: Event) => logDetailTabClick("request", e)}>Request</button>
        <button class="detail-tab ${currentTab === "response" ? "active" : ""}" data-arg1="response" data-action="logDetailTab" @click=${(e: Event) => logDetailTabClick("response", e)}>Response</button>
        <button class="detail-tab ${currentTab === "errors" ? "active" : ""}" data-arg1="errors" data-action="logDetailTab" @click=${(e: Event) => logDetailTabClick("errors", e)}>Errors</button>
        <button class="detail-tab ${currentTab === "raw" ? "active" : ""}" data-arg1="raw" data-action="logDetailTab" @click=${(e: Event) => logDetailTabClick("raw", e)}>Raw</button>
      </div>
      <div class="tab-actions-right">
        <button class="btn-copy-tab btn-copy-action" type="button" @click=${(e: Event) => { void copyRawJson(rawJson, e); }} title="Copiar log JSON">
          ${icons.copy()} Copiar
        </button>
      </div>
    </div>
  `;
}

/** Click handler for the `.detail-tab` buttons. Toggles which
 *  `#log-detail-content [data-log-tab]` section is visible (mutually
 *  exclusive) AND marks the clicked button as `.active`. */
export function logDetailTabClick(which: string, _e?: Event): void {
  setActiveLogDetailTab(which);
  // Update section visibility in DOM
  document.querySelectorAll("#log-detail-content [data-log-tab]").forEach((sec) => {
    const el = sec as HTMLElement;
    el.style.display = (sec.getAttribute("data-log-tab") === which) ? "" : "none";
  });
  // Update active tab buttons
  document.querySelectorAll(".tabs-toolbar .detail-tab, .tabs-and-actions-bar .detail-tab, .log-detail-tabs .detail-tab").forEach((btn) => {
    const b = btn as HTMLElement;
    b.classList.toggle("active", b.getAttribute("data-arg1") === which);
  });
}

// Initialize the log-detail tab UI: show only the first [data-log-tab]
// section, hide the remaining ones, and mark the first detail-tab as active.
export function initializeLogDetailTabs(): void {
  setActiveLogDetailTab("request");
  logDetailTabClick("request");
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
  clearPinnedIdentity();
  state.logs.selectedIdentity = null;
}

// Public API
export async function openLogDetail(
  id: string,
  requestId: string,
  traceId: string,
  row?: unknown // AttemptState (from logs.ts)
): Promise<void> {
  const gen = bumpOpenLogDetailGeneration();
  const typedRow = row as Record<string, unknown> | undefined;
  const isFinalized = typedRow != null && typedRow["terminal"] && typedRow["row"];

  const fallbackAttemptKey = traceId || (requestId ? `${requestId}:unknown` : id);

  if (isFinalized || row == null) {
    const loaded = await liveLogsStore.fetchLogDetail(id, traceId, fallbackAttemptKey);
    if (loaded && isCurrentOpenLogDetailGeneration(gen)) {
      renderModal();
    }
  }

  if (!isCurrentOpenLogDetailGeneration(gen)) return;

  const hasValidId = Boolean(id && id !== "0");
  state.logs.selectedIdentity = hasValidId ? { kind: "row_id", id: Number(id) } : { kind: "attempt", attemptKey: fallbackAttemptKey };
  setPinnedIdentity(requestId, traceId);

  const root = ensureModalRoot();
  let wrapper = document.querySelector(".log-detail-modal-wrapper") as HTMLElement | null;
  if (!wrapper) {
    wrapper = document.createElement("div");
    wrapper.className = "log-detail-modal-wrapper";
    root.appendChild(wrapper);
  }

  // We re-render immediately.
  renderModal();
  initializeLogDetailTabs();
}

function renderModal() {
  if (!state.logs.selectedIdentity) return;
  const attempt = liveLogsStore.selectDetail(state.logs.selectedIdentity);
  if (!attempt) return;

  const wrapper = document.querySelector(".log-detail-modal-wrapper");
  if (!wrapper) return;

  // Create a LogDetailLog compatible object for the modal view
  // The WS `log` row is the SSOT for live state, while `attempt.detail`
  // contains the heavy payloads from the `/usage/detail` snapshot. We merge
  // them, ensuring `log` properties take precedence, except for payloads which
  // might be omitted (null) in WS events.
  const detailObj = attempt?.detail as Record<string, unknown> | undefined;
  const log = attempt.row;
  const safeAttempt = { ...attempt, detail: undefined, row: undefined };
  const logObj = detailObj && log ? {
    ...detailObj,
    ...log,
    request_body_json: detailObj['request_body_json'] ?? log.request_body_json,
    response_body_json: detailObj['response_body_json'] ?? log.response_body_json,
    request_headers: detailObj['request_headers'] ?? log.request_headers,
    response_headers: detailObj['response_headers'] ?? log.response_headers,
    stages: [safeAttempt],
    detail: undefined
  } : {
    id: attempt.rowId,
    request_id: attempt.requestId,
    trace_id: attempt.traceId,
    status_code: attempt.statusCode,
    total_ms: attempt.elapsedMsAtEvent,
    provider_id: attempt.providerId,
    upstream_model_id: attempt.upstreamModelId,
    error_message: attempt.error,
    request_body_json: detailObj?.['request_body_json'],
    response_body_json: detailObj?.['response_body_json'],
    stages: [safeAttempt]
  };
  render(renderLogDetailModal(logObj as LogDetailLog), wrapper as HTMLElement);
}

// Re-render modal on clock tick so live latency updates
clockStore.subscribe(() => {
  if (state.logs.selectedIdentity) {
    renderModal();
  }
});

export function showLogDetail(_log: LogDetailLog): void {
  // Legacy compatibility, unused in new flow
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

export function updateOpenLogDetail(_row: LogDetailLog | null | undefined): void {
  if (state.logs.selectedIdentity) {
    renderModal();
  }
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
