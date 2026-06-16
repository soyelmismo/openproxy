// components/log-detail.js — render the per-request log detail
// modal. The modal HTML is built and inserted into the DOM; the
// "Close" button uses data-action="closeLogDetailModal" and the
// backdrop itself uses data-action="closeLogDetailModal"
//
// Per spec §3 + §13.8 we do not use inline `onclick="window.X()"`
// handlers.

import { state } from "../state/index.js";
import { escapeHtml } from "../lib/escape.js";
import { appendModal } from "../lib/dom.js";

function statusPillClass(s) {
  if (s === "ok" || s === "success") return "ok";
  if (s === "error" || s === "failed" || s === "unhealthy") return "err";
  if (s === "timeout" || s === "rate_limited" || s === "degraded") return "warn";
  return "warn";
}

function formatJson(value) {
  if (value == null) return "(empty)";
  let s;
  try { s = typeof value === "string" ? value : JSON.stringify(value, null, 2); }
  catch { s = String(value); }
  return escapeHtml(s);
}

function jsonSection(title, value, tabKey) {
  return `<section class="log-detail-section" data-log-tab="${escapeHtml(tabKey)}">
    <h4>${escapeHtml(title)}</h4>
    <pre class="json-viewer">${formatJson(value)}</pre>
  </section>`;
}

export function renderLogDetailModal(log) {
  // Normalize the row shape: the backend's /usage/detail returns
  // status_code, total_ms, upstream_model_id, etc., but the modal
  // originally assumed a richer payload (status, latency_ms, model,
  // cost, requests, response, errors, meta). Map the backend's
  // canonical field names onto the modal's expected shape so a row
  // from the table — which only has the live-update shape — looks
  // the same in the modal as a row from /usage/detail.
  const detail = log.detail || {};
  const meta = log.meta || detail.meta || log;
  const response = log.response || detail.response || log.response_body_json || null;
  const errors = log.errors || log.error || log.error_msg || detail.errors || null;
  // The backend's UsageDetailRow has a flat shape: it exposes
  // `request_body_json` (a serde_json::Value, already parsed) instead of
  // the older `requests[]` / `stages[]` arrays, which the UsageDetailRow
  // struct never had. We display the request body as a pretty JSON viewer.
  const requestBody = log.request_body_json != null
    ? log.request_body_json
    : (detail.request_body_json != null ? detail.request_body_json : null);
  const hasRequestBody = requestBody != null
    && !(typeof requestBody === "string" && requestBody.trim() === "")
    && !(typeof requestBody === "object" && requestBody !== null
         && !Array.isArray(requestBody) && Object.keys(requestBody).length === 0
         && JSON.stringify(requestBody) === "{}");
  const provider = log.provider_id || meta.provider_id || "—";
  const account = log.account_id || meta.account_id || "—";
  const combo = log.combo_id || meta.combo_id || null;
  const model = log.model_id || log.upstream_model || log.upstream_model_id || meta.model_id || "—";
  const latency = log.latency_ms != null ? `${log.latency_ms} ms`
    : (log.total_ms != null ? `${log.total_ms} ms`
    : (log.elapsed_ms != null ? `${log.elapsed_ms} ms` : "—"));
  const cost = log.cost != null ? log.cost : (log.usage?.cost ?? log.cost_usd ?? null);
  const status = log.status || (log.status_code != null ? String(log.status_code) : "—");
  const statusClass = statusPillClass(
    log.status_code != null
      ? (log.status_code >= 200 && log.status_code < 300 ? "ok" : (log.status_code >= 400 ? "error" : "warn"))
      : (log.status || "warn")
  );
  const requestId = log.request_id || log.id || "—";
  const createdAt = log.created_at || log.timestamp || "—";
  const apiKeyId = log.api_key_id || meta?.api_key_id || null;
  const userAgent = log.user_agent || meta?.user_agent || null;

  return `
    <div class="modal-bg log-detail-modal" data-action="closeLogDetailModal">
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
            <div><strong>Cost:</strong> ${cost != null ? escapeHtml(String(cost)) : "—"}</div>
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
            ${errors != null ? jsonSection("Errors", errors, "errors") : ""}
            ${jsonSection("Raw log", log, "raw")}
          </div>
        </div>
      </div>
    </div>
  `;
}

// Public API
export function showLogDetail(log) {
  const html = renderLogDetailModal(log);
  appendModal(html);
  const content = document.getElementById("log-detail-content");
  if (content) {
    const firstTab = document.querySelector(".log-detail-tabs .detail-tab");
    if (firstTab) firstTab.classList.add("active");
    const firstSection = content.querySelector("[data-log-tab]");
    if (firstSection) firstSection.style.display = "";
  }
  document.addEventListener("click", tabClickOnce);
}

let tabClickOnce = (e) => {
  const tab = e.target.closest && e.target.closest(".detail-tab[data-action='logDetailTab']");
  if (!tab) return;
  // The central data-action shim (app.js) will also dispatch a
  // logDetailTab call. We let the shim do the work and return here
  // to avoid running the body twice. To support that, we just
  // update the active-tab indicator early and let the shim
  // handler deal with content visibility.
  document.querySelectorAll(".log-detail-tabs .detail-tab").forEach((t) => t.classList.toggle("active", t === tab));
};

export function closeLogDetailModal(e) {
  // If invoked from the backdrop, the click target is the .modal-bg
  // itself (event delegation looks for the closest [data-action] and
  // finds the backdrop). If invoked from the X button, the target is
  // the button — but the closest [data-action] is still the modal-bg
  // because the button is a child without its own data-action.
  // Either way we just remove the modal-bg and we're done.
  document.removeEventListener("click", tabClickOnce);
  const m = document.querySelector(".log-detail-modal");
  if (m) m.remove();
}

// Update the open log-detail modal with a new row (called from the
// WebSocket `row`/`stage` event handlers when the user has a row
// detail modal open and a new event for that request arrives).
// If no modal is open this is a no-op.
export function updateOpenLogDetail(row) {
  if (!row) return;
  const modal = document.querySelector(".log-detail-modal");
  if (!modal) return; // No detail modal open; nothing to update.
  // Merge new fields into selectedRow so a follow-up render picks them up.
  if (state.logs.selectedRow && state.logs.selectedRow.request_id === row.request_id) {
    state.logs.selectedRow = { ...state.logs.selectedRow, ...row };
  }
  // Re-render the modal body in place: replace the entire .modal-bg
  // with the freshly rendered HTML so event delegation finds the
  // new data-action attributes.
  const html = renderLogDetailModal(state.logs.selectedRow || row);
  modal.outerHTML = html;
  // Re-bind the tab click shim.
  document.addEventListener("click", tabClickOnce);
}

// A row has complete detail if it carries a request body, a response
// body, or an error block. In-flight rows (only the request_id is
// known) return false so the caller can fetch the detail via
// /usage/detail. We also keep `requests[]` / `stages[]` as a fallback
// signal in case some older codepath still produces those.
export function hasCompleteLogDetail(row) {
  if (!row) return false;
  if (row.request_body_json != null) return true;
  if (row.response_body_json != null) return true;
  if (Array.isArray(row.requests) && row.requests.length > 0) return true;
  if (Array.isArray(row.stages) && row.stages.length > 0) return true;
  if (row.response != null) return true;
  if (row.errors != null || row.error != null || row.error_msg != null) return true;
  if (row.detail && (row.detail.response != null || row.detail.request_body_json != null
        || (Array.isArray(row.detail.requests) && row.detail.requests.length > 0))) return true;
  return false;
}
