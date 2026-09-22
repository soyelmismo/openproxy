// views/quota-cell.ts — render the per-account "Quota" cell.
// Migrated to lit-html: returns TemplateResult.

import { html, type TemplateResult } from 'lit-html';
import type { Account, ModelQuotaDetail } from "../lib/types/api.js";

// Format reset time as a short exact relative hint.
const resetHint = (ts: string | null | undefined): string => {
  if (!ts) return "";
  try {
    let d: Date;
    const num = Number(ts);
    if (!isNaN(num) && num > 0) {
      d = new Date(num > 1e11 ? num : num * 1000);
    } else {
      d = new Date(ts);
    }
    if (isNaN(d.getTime())) return "";
    const now = Date.now();
    const diffMs = d.getTime() - now;
    if (diffMs <= 0) return " · resets soon";

    const diffHrs = Math.floor(diffMs / (1000 * 60 * 60));
    const diffMins = Math.floor((diffMs % (1000 * 60 * 60)) / (1000 * 60));

    if (diffHrs >= 24) {
      const diffDays = Math.floor(diffHrs / 24);
      const remainingHrs = diffHrs % 24;
      return ` · resets in ${diffDays}d ${remainingHrs}h`;
    }
    return ` · resets in ${diffHrs}h ${diffMins}m`;
  } catch { return ""; }
};

function getQuotaColor(pct: number | null): string {
  if (pct == null) return "unknown";
  if (pct > 80) return "danger";
  if (pct > 50) return "warn";
  return "ok";
}

function getQuotaText(used: number | null, limit: number | null): string {
  if (used == null) return "—";
  if (limit === 100) return `${used}% used`;
  return `${used} / ${limit ?? "—"}`;
}

function renderModelQuotaRows(details: ModelQuotaDetail[]): TemplateResult {
  return html`<details class="quota-model-details">
    <summary>Models (${details.length})</summary>
    <div class="quota-model-list">
      ${details.map((d) => {
        const pct = d.session_limit > 0 ? Math.round(d.session_used / d.session_limit * 100) : 0;
        const color = getQuotaColor(pct);
        return html`<div class="quota-model-row">
          <div class="quota-model-header">
            <span class="quota-model-name">${d.model_id}</span>
            <span class="quota-model-text">${pct}% used${resetHint(d.session_reset_at)}</span>
          </div>
          <div class="quota-bar mini ${color}">
            <div class="quota-bar-track">
              <div class="quota-bar-fill" style="width: ${Math.min(100, pct)}%"></div>
            </div>
          </div>
        </div>`;
      })}
    </div>
  </details>`;
}

interface ProviderSpecificMeta {
  credit_balance?: string | number;
  last_checkin_date?: string;
  streak_days?: number;
}

function parseProviderSpecific(raw: string | null | undefined): ProviderSpecificMeta | null {
  if (!raw) return null;
  try {
    return JSON.parse(raw) as ProviderSpecificMeta;
  } catch {
    return null;
  }
}

function formatCredits(val: string | number): string {
  const num = typeof val === "number" ? val : parseFloat(val);
  if (isNaN(num)) return String(val);
  return new Intl.NumberFormat().format(num);
}

function renderMetaBadges(meta: ProviderSpecificMeta | null): TemplateResult | null {
  if (!meta) return null;
  const badges: TemplateResult[] = [];

  if (meta.credit_balance != null && meta.credit_balance !== "") {
    const formatted = formatCredits(meta.credit_balance);
    badges.push(
      html`<span class="quota-tag credits" title="Available Credits">🪙 ${formatted} credits</span>`
    );
  }

  if (meta.last_checkin_date || meta.streak_days != null) {
    const todayStr = new Date().toISOString().slice(0, 10);
    const isToday = meta.last_checkin_date === todayStr;
    const streak = meta.streak_days ?? 0;
    if (isToday) {
      badges.push(
        html`<span class="quota-tag checkin-done" title="Daily check-in completed today">✓ Checked in${streak > 0 ? ` (${streak}d streak)` : ""}</span>`
      );
    } else {
      badges.push(
        html`<span class="quota-tag checkin-pending" title="Daily check-in pending for today">⏳ Check-in pending${streak > 0 ? ` (${streak}d streak)` : ""}</span>`
      );
    }
  }

  if (badges.length === 0) return null;
  return html`<div class="quota-meta-row">${badges}</div>`;
}

export function renderQuotaCell(a: Account): TemplateResult {
  if (a.quota_fetch_error) {
    return html`<div class="quota-cell error"><small>✗ ${a.quota_fetch_error}</small></div>`;
  }
  const meta = parseProviderSpecific(a.oauth_provider_specific);
  const badgesResult = renderMetaBadges(meta);

  if (a.quota_session_used == null && a.quota_weekly_used == null) {
    if (a.quota_plan_name || badgesResult) {
      return html`<div class="quota-cell">
        <div class="quota-plan-row">
          <small class="quota-plan">${a.quota_plan_name || "Free"}</small>
        </div>
        ${badgesResult}
      </div>`;
    }
    return html`<div class="quota-cell muted"><small>${a.quota_last_fetched_at ? "no quota data" : "quota: not fetched"}</small></div>`;
  }
  const sessionPct = (a.quota_session_limit && a.quota_session_limit > 0 && a.quota_session_used != null)
    ? Math.round(a.quota_session_used / a.quota_session_limit * 100) : null;
  const weeklyPct = (a.quota_weekly_limit && a.quota_weekly_limit > 0 && a.quota_weekly_used != null)
    ? Math.round(a.quota_weekly_used / a.quota_weekly_limit * 100) : null;

  const sessionColor = getQuotaColor(sessionPct);
  const weeklyColor = getQuotaColor(weeklyPct);
  const sessionText = getQuotaText(a.quota_session_used, a.quota_session_limit);
  const weeklyText = getQuotaText(a.quota_weekly_used, a.quota_weekly_limit);

  const monthlyDetail = a.quota_model_details?.find(
    (d) => d.model_id === "Monthly Limit" || d.model_id === "Monthly Window"
  );
  const otherModels = a.quota_model_details?.filter(
    (d) => d.model_id !== "Monthly Limit" && d.model_id !== "Monthly Window"
  );

  const monthlyPct = (monthlyDetail && monthlyDetail.session_limit > 0 && monthlyDetail.session_used != null)
    ? Math.round(monthlyDetail.session_used / monthlyDetail.session_limit * 100) : null;
  const monthlyColor = getQuotaColor(monthlyPct);
  const monthlyText = monthlyDetail ? getQuotaText(monthlyDetail.session_used, monthlyDetail.session_limit) : "";
  const sessionLabel = (a.provider_id === "claude" || a.provider_id === "anthropic")
    ? "5h Window"
    : (a.provider_id === "codebuddy" ? "Credits" : "Session Window");

  return html`<div class="quota-cell">
    <div class="quota-plan-row">
      ${a.quota_plan_name ? html`<small class="quota-plan">${a.quota_plan_name}</small>` : null}
    </div>
    <div class="quota-bar ${sessionColor}">
      <div class="quota-bar-header">
        <span class="quota-bar-label-left">${sessionLabel}</span>
        <span class="quota-bar-label-right">${sessionText}${resetHint(a.quota_session_reset_at)}</span>
      </div>
      <div class="quota-bar-track">
        <div class="quota-bar-fill" style="width: ${sessionPct == null ? 0 : Math.min(100, sessionPct)}%"></div>
      </div>
    </div>
    ${(a.quota_weekly_used != null || a.quota_weekly_limit != null) ? html`
    <div class="quota-bar ${weeklyColor}">
      <div class="quota-bar-header">
        <span class="quota-bar-label-left">Weekly Window</span>
        <span class="quota-bar-label-right">${weeklyText}${resetHint(a.quota_weekly_reset_at)}</span>
      </div>
      <div class="quota-bar-track">
        <div class="quota-bar-fill" style="width: ${weeklyPct == null ? 0 : Math.min(100, weeklyPct)}%"></div>
      </div>
    </div>` : null}
    ${monthlyDetail ? html`
    <div class="quota-bar ${monthlyColor}">
      <div class="quota-bar-header">
        <span class="quota-bar-label-left">${monthlyDetail.model_id}</span>
        <span class="quota-bar-label-right">${monthlyText}${resetHint(monthlyDetail.session_reset_at)}</span>
      </div>
      <div class="quota-bar-track">
        <div class="quota-bar-fill" style="width: ${monthlyPct == null ? 0 : Math.min(100, monthlyPct)}%"></div>
      </div>
    </div>` : null}
    ${otherModels && otherModels.length > 0 ? renderModelQuotaRows(otherModels) : null}
    ${badgesResult}
  </div>`;
}

