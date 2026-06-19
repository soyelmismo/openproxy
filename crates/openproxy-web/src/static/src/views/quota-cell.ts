// views/quota-cell.ts — render the per-account "Quota" cell. The
// data lives on the Account struct (the server stamps it via
// `POST /v1/admin/accounts/:id/refresh-quota`), so rendering is
// just a read of `state.accounts[i].quota_*` — there's no per-cell
// network call. The refresh button is the only place that triggers
// a write back to the server.
//
// Mirrors the old app.js `renderQuotaCell()` 1:1, split out so
// the connections table in the provider detail view can include
// it without a 200-line function in views/provider-detail.ts.

import { escapeHtml } from "../lib/escape.js";
import type { Account, ModelQuotaDetail } from "../lib/types/api.js";

function renderModelQuotaRows(details: ModelQuotaDetail[]): string {
  const rows = details.map((d) => {
    const pct = d.session_limit > 0
      ? Math.round(d.session_used / d.session_limit * 100)
      : 0;
    const color = pct > 80 ? "danger" : pct > 50 ? "warn" : "ok";
    return `<div class="quota-model-row ${color}">
      <span class="quota-model-name">${escapeHtml(d.model_id)}</span>
      <div class="quota-bar mini ${color}">
        <div class="quota-bar-fill" style="width: ${Math.min(100, pct)}%"></div>
      </div>
      <span class="quota-model-text">${pct}% used${d.session_reset_at ? " · resets " + escapeHtml(d.session_reset_at) : ""}</span>
    </div>`;
  }).join("");
  return `<details class="quota-model-details" open>
    <summary>Models (${details.length})</summary>
    ${rows}
  </details>`;
}

export function renderQuotaCell(a: Account): string {
  // Error path: a previous fetch failed.
  if (a.quota_fetch_error) {
    return `<div class="quota-cell error"><small>✗ ${escapeHtml(a.quota_fetch_error)}</small></div>`;
  }
  // No usable data.
  if (a.quota_session_used == null && a.quota_weekly_used == null) {
    if (a.quota_last_fetched_at) {
      return `<div class="quota-cell muted"><small>no quota data</small></div>`;
    }
    return `<div class="quota-cell muted"><small>quota: not fetched</small></div>`;
  }
  // Aggregate bars.
  const sessionPct = (a.quota_session_limit && a.quota_session_limit > 0 && a.quota_session_used != null)
    ? Math.round(a.quota_session_used / a.quota_session_limit * 100)
    : null;
  const weeklyPct = (a.quota_weekly_limit && a.quota_weekly_limit > 0 && a.quota_weekly_used != null)
    ? Math.round(a.quota_weekly_used / a.quota_weekly_limit * 100)
    : null;
  const sessionColor = sessionPct == null ? "unknown"
    : sessionPct > 80 ? "danger"
    : sessionPct > 50 ? "warn" : "ok";
  const weeklyColor = weeklyPct == null ? "unknown"
    : weeklyPct > 80 ? "danger"
    : weeklyPct > 50 ? "warn" : "ok";

  const isPct = (used: number | null, limit: number | null): boolean => limit === 100 && used != null;
  const sessionText = a.quota_session_used == null ? "—"
    : isPct(a.quota_session_used, a.quota_session_limit)
      ? `${a.quota_session_used}% used`
      : `${a.quota_session_used} / ${a.quota_session_limit ?? "—"}`;
  const weeklyText = a.quota_weekly_used == null ? "—"
    : isPct(a.quota_weekly_used, a.quota_weekly_limit)
      ? `${a.quota_weekly_used}% used`
      : `${a.quota_weekly_used} / ${a.quota_weekly_limit ?? "—"}`;

  // Per-model breakdown (Antigravity family).
  const modelHtml = a.quota_model_details && a.quota_model_details.length > 0
    ? renderModelQuotaRows(a.quota_model_details)
    : "";

  return `
    <div class="quota-cell">
      ${a.quota_plan_name ? `<small class="quota-plan">${escapeHtml(a.quota_plan_name)}</small>` : ""}
      <div class="quota-bar ${sessionColor}">
        <div class="quota-bar-fill" style="width: ${sessionPct == null ? 0 : Math.min(100, sessionPct)}%"></div>
        <span>session: ${sessionText}</span>
      </div>
      <div class="quota-bar ${weeklyColor}">
        <div class="quota-bar-fill" style="width: ${weeklyPct == null ? 0 : Math.min(100, weeklyPct)}%"></div>
        <span>weekly: ${weeklyText}</span>
      </div>
      ${modelHtml}
    </div>
  `;
}
