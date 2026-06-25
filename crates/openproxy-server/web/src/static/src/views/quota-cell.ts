// views/quota-cell.ts — render the per-account "Quota" cell.
// Migrated to lit-html: returns TemplateResult.

import { html, type TemplateResult } from 'lit-html';
import type { Account, ModelQuotaDetail } from "../lib/types/api.js";

function renderModelQuotaRows(details: ModelQuotaDetail[]): TemplateResult {
  return html`<details class="quota-model-details" open>
    <summary>Models (${details.length})</summary>
    ${details.map((d) => {
      const pct = d.session_limit > 0 ? Math.round(d.session_used / d.session_limit * 100) : 0;
      const color = pct > 80 ? "danger" : pct > 50 ? "warn" : "ok";
      return html`<div class="quota-model-row ${color}">
        <span class="quota-model-name">${d.model_id}</span>
        <div class="quota-bar mini ${color}"><div class="quota-bar-fill" style="width: ${Math.min(100, pct)}%"></div></div>
        <span class="quota-model-text">${pct}% used${d.session_reset_at ? " · resets " + d.session_reset_at : ""}</span>
      </div>`;
    })}
  </details>`;
}

export function renderQuotaCell(a: Account): TemplateResult {
  if (a.quota_fetch_error) {
    return html`<div class="quota-cell error"><small>✗ ${a.quota_fetch_error}</small></div>`;
  }
  if (a.quota_session_used == null && a.quota_weekly_used == null) {
    return html`<div class="quota-cell muted"><small>${a.quota_last_fetched_at ? "no quota data" : "quota: not fetched"}</small></div>`;
  }
  const sessionPct = (a.quota_session_limit && a.quota_session_limit > 0 && a.quota_session_used != null)
    ? Math.round(a.quota_session_used / a.quota_session_limit * 100) : null;
  const weeklyPct = (a.quota_weekly_limit && a.quota_weekly_limit > 0 && a.quota_weekly_used != null)
    ? Math.round(a.quota_weekly_used / a.quota_weekly_limit * 100) : null;
  const sessionColor = sessionPct == null ? "unknown" : sessionPct > 80 ? "danger" : sessionPct > 50 ? "warn" : "ok";
  const weeklyColor = weeklyPct == null ? "unknown" : weeklyPct > 80 ? "danger" : weeklyPct > 50 ? "warn" : "ok";
  const isPct = (used: number | null, limit: number | null): boolean => limit === 100 && used != null;
  const sessionText = a.quota_session_used == null ? "—" : isPct(a.quota_session_used, a.quota_session_limit) ? `${a.quota_session_used}% used` : `${a.quota_session_used} / ${a.quota_session_limit ?? "—"}`;
  const weeklyText = a.quota_weekly_used == null ? "—" : isPct(a.quota_weekly_used, a.quota_weekly_limit) ? `${a.quota_weekly_used}% used` : `${a.quota_weekly_used} / ${a.quota_weekly_limit ?? "—"}`;
  return html`<div class="quota-cell">
    ${a.quota_plan_name ? html`<small class="quota-plan">${a.quota_plan_name}</small>` : null}
    <div class="quota-bar ${sessionColor}"><div class="quota-bar-fill" style="width: ${sessionPct == null ? 0 : Math.min(100, sessionPct)}%"></div><span>session: ${sessionText}</span></div>
    <div class="quota-bar ${weeklyColor}"><div class="quota-bar-fill" style="width: ${weeklyPct == null ? 0 : Math.min(100, weeklyPct)}%"></div><span>weekly: ${weeklyText}</span></div>
    ${a.quota_model_details && a.quota_model_details.length > 0 ? renderModelQuotaRows(a.quota_model_details) : null}
  </div>`;
}
