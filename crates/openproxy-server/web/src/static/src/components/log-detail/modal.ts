// Log-detail modal: `renderLogDetailModal` template, open/close/re-render lifecycle,
// tab click handling, the clock-tick re-render subscription, and the E2E test hook.
//
// KNOWN CYCLE (index ↔ modal): the tab-body renderers come from index.ts, which only
// re-exports this module; the cycle resolves because those hoisted function declarations
// run at render time, not during module evaluation.

import { html, render, type TemplateResult } from "lit-html";
import { state } from "../../state/index.js";
import { api } from "../../lib/api.js";
import { ensureModalRoot } from "../../lib/ui-utils.js";
import { icons, endpointIcon } from "../../lib/icons.js";
import { liveLogsStore } from "../../state/live-logs-store.js";
import { clockStore } from "../../state/clock-store.js";
import {
  bumpOpenLogDetailGeneration,
  clearPinnedIdentity,
  getActiveLogDetailTab,
  getLogDetailMarkdown,
  setLogDetailMarkdown,
  getPinnedTraceId,
  isCurrentOpenLogDetailGeneration,
  matchesPinnedModalIdentity,
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
  stringStatusPillClass,
} from "./index.js";

export function getAccountDisplay(
  accountId: string | number | null,
  accountLabelDirect?: string | null
): { text: string; title?: string } {
  const direct = accountLabelDirect?.trim();
  if (direct) {
    const title = accountId != null && accountId !== "" && accountId !== "—" ? `Account #${accountId}` : undefined;
    return title ? { text: direct, title } : { text: direct };
  }
  if (accountId != null && accountId !== "" && accountId !== "—") {
    const match = (state.accounts || []).find((a) => String(a.id) === String(accountId));
    const label = match?.label?.trim();
    if (label) {
      return {
        text: label,
        title: `Account #${accountId}`,
      };
    }
    return { text: String(accountId) };
  }
  return { text: "—" };
}

export function renderLogDetailModal(log: LogDetailLog): TemplateResult {
  // /usage/detail returns canonical names (status_code, total_ms, upstream_model_id) while the
  // modal expects richer ones (status, latency_ms, model, cost, requests, response, errors, meta),
  // so a table row and a detail row render identically.
  const detail: Record<string, unknown> = (log.detail as Record<string, unknown>) || {};
  const meta: Record<string, unknown> = (log.meta as Record<string, unknown>) || (detail["meta"] as Record<string, unknown>) || (log as Record<string, unknown>);
  const response: unknown = log.response ?? detail["response"] ?? log.response_body_json ?? null;
  const isStreaming: boolean = !!((log as Record<string, unknown>)["is_streaming"]);
  // A stream that never completed is "partial": the backend persisted what it had.
  // renderResponseTab shows a banner for it.
  const streamComplete: boolean = !!((log as Record<string, unknown>)["stream_complete"]);
  const isPartial: boolean = isStreaming && !streamComplete;
  // `log.error_message` comes from the recent-rows endpoint; `log.error_msg` and
  // `log.error_msg_redacted` from the detail endpoint (usage.rs).
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
  // UsageDetailRow is flat: `request_body_json` is already a parsed serde_json::Value, and the
  // `requests[]` / `stages[]` arrays it never had are not fabricated here.
  const requestBody: unknown = log.request_body_json != null
    ? log.request_body_json
    : (detail["request_body_json"] != null ? detail["request_body_json"] : null);
  const provider: string = log.provider_id || (readString(meta, "provider_id") ?? "—");
  const accountId: string | number | null = log.account_id != null ? log.account_id : meta["account_id"] != null ? (meta["account_id"] as string | number) : null;
  const accountLabelDirect: string | null | undefined = (log.account_label as string | undefined) ?? (detail["account_label"] as string | undefined) ?? (meta["account_label"] as string | undefined);
  const accountDisplay = getAccountDisplay(accountId, accountLabelDirect);
  const comboRaw: unknown = log.combo_id ?? meta["combo_id"];
  const combo: string | number | null = comboRaw != null && (typeof comboRaw === "string" || typeof comboRaw === "number") ? comboRaw : null;
  const model: string = log.model_id || log.upstream_model || log.upstream_model_id || (readString(meta, "model_id") ?? "—");
  const costRaw: number | null = log.cost != null ? log.cost
    : (log.usage && log.usage.cost != null ? log.usage.cost
      : (log.cost_usd != null ? log.cost_usd : null));
  const status: string = log.status || (log.status_code != null ? String(log.status_code) : "—");
  const statusClass: string = stringStatusPillClass(
    log.status_code != null
      ? (log.status_code >= 200 && log.status_code < 300 ? "ok" : (log.status_code >= 400 ? "error" : "warn"))
      : (log.status || "warn")
  );
  const requestId: string | number = log.request_id || log.id || "—";
  const createdAt: string = log.created_at || log.timestamp || "—";
  const apiKeyIdRaw: unknown = log.api_key_id ?? meta["api_key_id"];
  const apiKeyId: string | number | null = apiKeyIdRaw != null && (typeof apiKeyIdRaw === "string" || typeof apiKeyIdRaw === "number") ? apiKeyIdRaw : null;
  const comboNameFromState = combo != null && Array.isArray(state.combos)
    ? state.combos.find((c) => String(c.id) === String(combo))?.name
    : null;
  const comboText: string = combo != null
    ? (comboNameFromState ? `${comboNameFromState} (#${combo})` : String(combo))
    : "—";
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

  // Latency: "3,277 ms (ttft 8ms)"
  const ttftMs = (log as Record<string, unknown>)["time_to_first_token_ms"]
    ?? (log as Record<string, unknown>)["ttft_ms"]
    ?? (attempt as Record<string, unknown> | undefined)?.["ttft_ms"]
    ?? (meta as Record<string, unknown>)["ttft_ms"];
  const latVal = log.latency_ms ?? log.total_ms ?? log.elapsed_ms;
  const latencyDisplay = latVal != null
    ? `${latVal} ms${ttftMs != null ? ` (ttft ${ttftMs}ms)` : ""}`
    : "—";

  // Tokens: "6,897↓ 150↑ (7,047 tot)"
  const promptTokens = log.prompt_tokens;
  const compTokens = log.completion_tokens;
  const totalTokens = (log as Record<string, unknown>)["total_tokens"] as number | undefined
    ?? ((promptTokens != null || compTokens != null) ? ((promptTokens ?? 0) + (compTokens ?? 0)) : null);
  const promptEstimated = log.prompt_tokens_estimated ? "≈" : "";
  const compEstimated = log.completion_tokens_estimated ? "≈" : "";
  const tokensDisplay = (promptTokens != null || compTokens != null || totalTokens != null)
    ? `${promptEstimated}${promptTokens != null ? promptTokens.toLocaleString() : "0"}↓ ${compEstimated}${compTokens != null ? compTokens.toLocaleString() : "0"}↑ (${totalTokens != null ? totalTokens.toLocaleString() : "0"} tot)`
    : "—";

  // Speed: "45.9 tok/s"
  const speedDisplay = log.tokens_per_sec != null ? `${log.tokens_per_sec.toFixed(1)} tok/s` : "—";

  // Cost: "$0.0000"
  const costDisplay = costRaw != null
    ? (typeof costRaw === "number" ? `$${costRaw.toFixed(4)}` : `$${Number(costRaw).toFixed(4)}`)
    : "$0.0000";

  const apiKeyDisplay = apiKeyId != null ? `#${String(apiKeyId)}` : "—";


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
          <div class="log-detail-summary desktop-summary">
            <div><strong>Status:</strong> <span class="status-pill ${statusClass}">${String(status)}</span></div>
            <div><strong>Endpoint:</strong> <span title="HTTP Entry: POST ${endpointPath} (${endpointKind})"><code style="font-size:0.85em;padding:1px 4px;background:var(--color-surface-2);border-radius:0;">POST ${endpointPath}</code> <span class="log-type-tag log-type-tag--${endpointKind}" style="font-size:0.75em;padding:1px 5px;margin-left:4px;">${endpointIcon(endpointKind)} ${endpointKind}</span></span></div>
            <div><strong>Provider:</strong> ${String(provider)}</div>
            <div><strong>Model:</strong> ${String(model)}</div>
            <div><strong>Latency:</strong> <span class="mono-val">${latencyDisplay}</span></div>
            <div><strong>Tokens:</strong> <span class="mono-val"${compressionTooltip ? html` title=${compressionTooltip}` : ""}>${tokensDisplay}</span></div>
            <div><strong>Speed:</strong> <span class="mono-val">${speedDisplay}</span></div>
            <div><strong>Cost:</strong> <span class="mono-val">${costDisplay}</span></div>
            <div><strong>Account:</strong> ${accountDisplay.title ? html`<span title=${accountDisplay.title}>${accountDisplay.text}</span>` : accountDisplay.text}</div>
            <div><strong>Combo:</strong> ${comboText}</div>
            <div><strong>API Key:</strong> ${apiKeyDisplay}</div>
            <div><strong>Created:</strong> ${String(createdAt)}</div>
            ${log.pii_redacted ? html`<div><strong>PII:</strong> <span class="status-pill" style="background:rgba(168, 85, 247, 0.15); color: #c084fc; border: 1px solid rgba(168, 85, 247, 0.3); font-weight: 500; display: inline-flex; align-items: center; gap: 4px;" title="Entities redacted before upstream">${icons.eye()} ${log.pii_redacted}</span></div>` : ""}
          </div>

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
              <span class="kpi-sub">Key ${apiKeyDisplay} · ${costDisplay}${log.pii_redacted ? ` · PII (${log.pii_redacted})` : ""}</span>
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
        <label class="log-detail-markdown-toggle">
          <span>Markdown</span>
          <button id="log-detail-markdown-toggle" type="button" role="switch"
            aria-label="Format messages as Markdown" aria-checked=${getLogDetailMarkdown() ? "true" : "false"}
            class="toggle-btn ${getLogDetailMarkdown() ? "on" : "off"}"
            @click=${() => {
              setLogDetailMarkdown(!getLogDetailMarkdown());
              renderModal();
              logDetailTabClick(getActiveLogDetailTab());
            }}><span class="toggle-thumb"></span></button>
        </label>
        <button class="btn-copy-tab btn-copy-action" type="button" @click=${(e: Event) => { void copyRawJson(rawJson, e); }} title="Copiar log JSON">
          ${icons.copy()} Copiar
        </button>
      </div>
    </div>
  `;
}

/** Click handler for the `.detail-tab` buttons: toggles which `[data-log-tab]` section is
 *  visible (mutually exclusive) and marks the clicked button `.active`. */
export function logDetailTabClick(which: string, _e?: Event): void {
  setActiveLogDetailTab(which);

  document.querySelectorAll("#log-detail-content [data-log-tab]").forEach((sec) => {
    const el = sec as HTMLElement;
    el.style.display = (sec.getAttribute("data-log-tab") === which) ? "" : "none";
  });

  document.querySelectorAll(".tabs-toolbar .detail-tab, .tabs-and-actions-bar .detail-tab, .log-detail-tabs .detail-tab").forEach((btn) => {
    const b = btn as HTMLElement;
    b.classList.toggle("active", b.getAttribute("data-arg1") === which);
  });
}

// Show only the first [data-log-tab] section, hide the rest, and mark the first tab active.
export function initializeLogDetailTabs(): void {
  setActiveLogDetailTab("request");
  logDetailTabClick("request");
}

let pinnedSnapshot: LogDetailLog | null = null;

/** Remove the `.log-detail-modal` element AND its wrapper host div, keeping `#modal-root`
 *  clean so the next modal opens in a fresh wrapper. */
function removeLogDetailModal(m: HTMLElement): void {
  const wrapper = m.parentElement;
  m.remove();
  if (wrapper && wrapper.children.length === 0 && wrapper.parentElement?.id === "modal-root") {
    wrapper.remove();
  }
  // Clear the pin so later WS events skip the closed modal.
  clearPinnedIdentity();
  pinnedSnapshot = null;
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
  pinnedSnapshot = null;
  const typedRow = row as Record<string, unknown> | undefined;
  const isFinalized = typedRow != null && typedRow["terminal"] && typedRow["row"];

  const fallbackAttemptKey = traceId || (requestId ? `${requestId}:unknown` : id);

  if (isFinalized || row == null) {
    const loaded = await liveLogsStore.fetchLogDetail(id, traceId, fallbackAttemptKey);
    if (loaded && isCurrentOpenLogDetailGeneration(gen)) {
      renderModal();
    }
  }

  if (!state.accounts || state.accounts.length === 0) {
    void (api("/accounts") as Promise<typeof state.accounts>).then((accs) => {
      if (Array.isArray(accs) && isCurrentOpenLogDetailGeneration(gen)) {
        state.accounts = accs;
        renderModal();
      }
    }).catch(() => {});
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


  renderModal();
  initializeLogDetailTabs();
}

function renderModal() {
  if (!state.logs.selectedIdentity) return;
  const wrapper = document.querySelector(".log-detail-modal-wrapper");
  if (!wrapper) return;

  if (pinnedSnapshot && !getPinnedTraceId()) {
    render(renderLogDetailModal(pinnedSnapshot), wrapper as HTMLElement);
    return;
  }

  const attempt = liveLogsStore.selectDetail(state.logs.selectedIdentity);
  if (!attempt) return;

  // The WS `log` row is the SSOT for live state; `attempt.detail` carries the heavy
  // /usage/detail payloads. Merge with `log` winning, except where a payload is null in the WS event.
  const detailObj = attempt?.detail as Record<string, unknown> | undefined;
  const log = attempt.row;
  const safeAttempt = { ...attempt, detail: undefined, row: undefined };
  const rawLogObj = log as unknown as Record<string, unknown> | null;
  const rawAttempt = attempt as unknown as Record<string, unknown>;
  const logObj = detailObj && log ? {
    ...detailObj,
    ...log,
    account_id: detailObj['account_id'] ?? rawLogObj?.['account_id'],
    account_label: detailObj['account_label'],
    request_body_json: detailObj['request_body_json'] ?? log.request_body_json,
    response_body_json: detailObj['response_body_json'] ?? log.response_body_json,
    request_headers: detailObj['request_headers'] ?? log.request_headers,
    response_headers: detailObj['response_headers'] ?? log.response_headers,
    stages: [safeAttempt],
    detail: undefined
  } : {
    ...(detailObj ?? {}),
    id: attempt.rowId ?? detailObj?.['id'],
    request_id: attempt.requestId,
    trace_id: attempt.traceId,
    status_code: attempt.statusCode ?? detailObj?.['status_code'],
    total_ms: attempt.elapsedMsAtEvent,
    provider_id: attempt.providerId,
    upstream_model_id: attempt.upstreamModelId,
    error_message: attempt.error,
    account_id: detailObj?.['account_id'] ?? rawAttempt['account_id'],
    account_label: detailObj?.['account_label'],
    request_body_json: detailObj?.['request_body_json'],
    response_body_json: detailObj?.['response_body_json'],
    stages: [safeAttempt]
  };
  if (!getPinnedTraceId() && !pinnedSnapshot) {
    pinnedSnapshot = logObj as LogDetailLog;
  }
  render(renderLogDetailModal(logObj as LogDetailLog), wrapper as HTMLElement);
}

// Re-render on clock tick so live latency updates
clockStore.subscribe(() => {
  if (state.logs.selectedIdentity) {
    renderModal();
  }
});

export function showLogDetail(_log: LogDetailLog): void {

}

export function closeLogDetailModal(e: Event | null): void {
  // lit-html binds the handler to both the backdrop and the X button, so `target ===
  // closest('.log-detail-modal')` and `closest('.close-btn')` distinguish the two valid origins.
  if (!e || !e.target) return;
  const target: EventTarget = e.target;
  if (!(target instanceof Element)) return;
  const m: HTMLElement | null = target.closest(".log-detail-modal");
  if (!m) return;
  // Case 1: the backdrop itself. Strict identity, so clicks on a descendant (<pre> body,
  // JSON viewer) do not close.
  if (target === m) { removeLogDetailModal(m); return; }
  // Case 2: the header's X button.
  const closeBtn: HTMLElement | null = target.closest(".close-btn");
  if (closeBtn && m.contains(closeBtn)) {
    removeLogDetailModal(m); return;
  }
  // Case 3: anything else inside .modal; its own handler already ran.
}

export function updateOpenLogDetail(row: LogDetailLog | null | undefined): void {
  if (state.logs.selectedIdentity) {
    if (row && !matchesPinnedModalIdentity(row)) return;
    renderModal();
  }
}

// E2E hook for simulating WS events while the modal is open. `declare global` gives the tests
// type-safe access without their own cast, matching the `__openproxyState` / `__openproxyLogsGoPage`
// hooks in app.ts.
declare global {
  interface Window {
    __openproxyUpdateLogDetail?: typeof updateOpenLogDetail;
  }
}
if (typeof window !== "undefined") {
  window.__openproxyUpdateLogDetail = updateOpenLogDetail;
}
