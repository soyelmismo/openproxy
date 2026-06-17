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
import type { Account } from "../lib/types/api.js";

export function renderQuotaCell(a: Account): string {
  // Error path: a previous fetch failed. The message is bounded by
  // the server (it puts the upstream error text in
  // `quota_fetch_error`), but we still escape it before injecting
  // into the DOM.
  if (a.quota_fetch_error) {
    return `<div class="quota-cell error"><small>✗ ${escapeHtml(a.quota_fetch_error)}</small></div>`;
  }
  // No usable data: distinguish "we tried, the upstream said
  // nothing" from "we never tried". The former shows
  // `quota_last_fetched_at`, the latter does not. We treat the
  // quota as "absent" only when BOTH the session and the weekly
  // USED values are missing — an OpenRouter key with no configured
  // limit (limit=null) but a real usage of 0 still has a used
  // counter, so it should fall through to the bar renderer with a
  // "—" limit rather than being hidden behind "no quota data".
  if (a.quota_session_used == null && a.quota_weekly_used == null) {
    if (a.quota_last_fetched_at) {
      return `<div class="quota-cell muted"><small>no quota data</small></div>`;
    }
    return `<div class="quota-cell muted"><small>quota: not fetched</small></div>`;
  }
  // Render the two bars. We render even when only one of the two
  // quotas is present (the server may know session but not weekly,
  // or vice versa) — the missing side is dashed and shows "—".
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

  // When the limit is exactly 100 the parser is in percent-fallback
  // mode (the upstream shipped only the remaining-percent field).
  // The bar math is identical, but the label should make it clear
  // we're showing an estimate rather than a raw "X / N" call count.
  const isPct = (used: number | null, limit: number | null): boolean => limit === 100 && used != null;
  const sessionText = a.quota_session_used == null ? "—"
    : isPct(a.quota_session_used, a.quota_session_limit)
      ? `${a.quota_session_used}% used`
      : `${a.quota_session_used} / ${a.quota_session_limit ?? "—"}`;
  const weeklyText = a.quota_weekly_used == null ? "—"
    : isPct(a.quota_weekly_used, a.quota_weekly_limit)
      ? `${a.quota_weekly_used}% used`
      : `${a.quota_weekly_used} / ${a.quota_weekly_limit ?? "—"}`;

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
    </div>
  `;
}
