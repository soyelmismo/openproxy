// components/log-detail.ts — render the per-request log detail
// modal. The modal HTML is built and inserted into the DOM; the
// "Close" button uses data-action="closeLogDetailModal" and the
// backdrop itself uses data-action="closeLogDetailModal"
//
// Per spec §3 + §13.8 we do not use inline `onclick="window.X()"`
// handlers.

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
  const hasRequestBody: boolean = requestBody != null
    && !(typeof requestBody === "string" && requestBody.trim() === "")
    && !(typeof requestBody === "object" && requestBody !== null
         && !Array.isArray(requestBody) && Object.keys(requestBody as object).length === 0
         && JSON.stringify(requestBody) === "{}");
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
            <section class="log-detail-section" data-log-tab="request">
              <h4>Request</h4>
              ${hasRequestBody
                ? `<pre class="json-viewer">${formatJson(requestBody)}</pre>`
                : `<p class="muted">No request body recorded.</p>`}
            </section>
            ${jsonSection("Response", response, "response")}
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
  const firstTab: HTMLElement | null = modal.querySelector("[data-log-detail-tab]");
  modal.querySelectorAll("[data-log-detail-tab]").forEach((t) => t.classList.remove("active"));
  if (firstTab) firstTab.classList.add("active");
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

// Update the open log-detail modal with a new row (called from the
// WebSocket `row`/`stage` event handlers when the user has a row
// detail modal open and a new event for that request arrives).
// If no modal is open this is a no-op.
export function updateOpenLogDetail(row: LogDetailLog | null | undefined): void {
  if (!row) return;
  const modal: HTMLElement | null = document.querySelector(".log-detail-modal");
  if (!modal) return; // No detail modal open; nothing to update.
  // Merge new fields into selectedRow so a follow-up render picks them up.
  const sel = state.logs.selectedRow as unknown as (LogDetailLog & { request_id?: string }) | null;
  if (sel && sel.request_id === row.request_id) {
    state.logs.selectedRow = { ...sel, ...row } as unknown as (typeof state.logs)["selectedRow"];
  }
  // Re-render the modal body in place: replace the entire .modal-bg
  // with the freshly rendered HTML so event delegation finds the
  // new data-action attributes.
  const html: string = renderLogDetailModal((state.logs.selectedRow as unknown as LogDetailLog) || row);
  modal.outerHTML = html;
  // Re-bind the tab click shim (idempotent — wired flag prevents
  // the listener from being registered more than once per session).
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
