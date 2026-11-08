// views/analytics/combo-health.ts — combo health card: request-level success
// rate, client-visible error rate, and the two average waits.
//
// The distinction that matters: a request that retried and recovered is a
// success (with a longer wait); a request the combo never covered is the only
// thing counted as an error. `avg_ok_ms` / `avg_error_ms` price those two
// outcomes in wall-clock time, so a combo that is "99% healthy" but takes 60s
// to give up is visible at a glance.

import { html, type TemplateResult } from "lit-html";
import { t } from "../../i18n/index.js";
import { byCombo, fmtDuration, fmtNumber, fmtPercent } from "./shared.js";
import { renderDataGrid, type GridColumn } from "../../components/data-grid.js";
import type { ByComboRow } from "../../lib/types/api.js";

/** Health tone from the success rate: 99%+ good, 95%+ warn, below is bad. */
function healthTone(successRatePct: number): "good" | "warn" | "bad" {
  return successRatePct >= 99 ? "good" : successRatePct >= 95 ? "warn" : "bad";
}

function renderColumns(): GridColumn<ByComboRow>[] {
  return [
    {
      key: "combo_name",
      label: t("analytics.combos.col_combo"),
      min: 120,
      elastic: true,
      render: (r) => html`<a class="analytics-combo-link" href="#/combos/${r.combo_id}">
        <strong>${r.combo_name || `#${r.combo_id}`}</strong>
      </a>`,
    },
    {
      key: "requests",
      label: t("analytics.combos.col_requests"),
      max: 100,
      fold: 3,
      render: (r) => html`<span class="dg-num">${fmtNumber(r.requests)}</span>`,
    },
    {
      key: "success_rate_pct",
      label: t("analytics.combos.col_success"),
      max: 110,
      fold: 1,
      render: (r) => html`<span class="dg-num combo-health-cell ${healthTone(r.success_rate_pct)}">
        <span class="combo-health-dot"></span>${r.success_rate_pct.toFixed(1)}%
      </span>`,
    },
    {
      key: "error_rate_pct",
      label: t("analytics.combos.col_errors"),
      max: 110,
      fold: 2,
      render: (r) => html`<span
        class="dg-num ${r.error_requests > 0 ? "combo-error-cell" : ""}"
        title="${fmtNumber(r.error_requests)} / ${fmtNumber(r.requests)}"
      >${r.error_requests > 0 ? r.error_rate_pct.toFixed(1) + "%" : "—"}</span>`,
    },
    {
      key: "avg_ok_ms",
      label: t("analytics.combos.col_ok_time"),
      max: 110,
      fold: 4,
      render: (r) => html`<span class="dg-num combo-time-ok">${fmtDuration(r.avg_ok_ms)}</span>`,
    },
    {
      key: "avg_error_ms",
      label: t("analytics.combos.col_err_time"),
      max: 110,
      fold: 5,
      render: (r) => html`<span class="dg-num combo-time-err">${fmtDuration(r.avg_error_ms)}</span>`,
    },
    {
      key: "avg_attempts",
      label: t("analytics.combos.col_attempts"),
      max: 100,
      fold: 6,
      render: (r) => html`<span class="dg-num">${r.avg_attempts.toFixed(2)}</span>`,
    },
  ];
}

/** Compact two-bar row: how much of the traffic succeeded, and how the
 *  average wait compares between a success and a total failure. */
function renderSummary(rows: ByComboRow[]): TemplateResult {
  const withTraffic = rows.filter((r) => r.requests > 0);
  if (withTraffic.length === 0) return html``;
  const totalRequests = withTraffic.reduce((sum, r) => sum + r.requests, 0);
  const totalErrors = withTraffic.reduce((sum, r) => sum + r.error_requests, 0);
  // Weighted means: cheap on screen and honest across a mixed set of combos.
  const okWeighted = withTraffic.reduce((sum, r) => sum + (r.avg_ok_ms ?? 0) * r.ok_requests, 0);
  const okCount = withTraffic.reduce((sum, r) => sum + r.ok_requests, 0);
  const errWeighted = withTraffic.reduce((sum, r) => sum + (r.avg_error_ms ?? 0) * r.error_requests, 0);
  const errCount = withTraffic.reduce((sum, r) => sum + r.error_requests, 0);
  const avgOk = okCount > 0 ? okWeighted / okCount : null;
  const avgErr = errCount > 0 ? errWeighted / errCount : null;
  const successRate = totalRequests > 0 ? 1 - totalErrors / totalRequests : NaN;
  const worst = [...withTraffic]
    .filter((r) => r.requests >= 20)
    .sort((a, b) => a.success_rate_pct - b.success_rate_pct)[0];

  return html`<div class="combo-health-summary">
    <div class="combo-health-kpi">
      <span>${t("analytics.combos.col_success")}</span>
      <strong class=${healthTone(successRate * 100)}>${fmtPercent(successRate)}</strong>
      <small>${fmtNumber(totalErrors)} ${t("analytics.metric.failed_responses")}</small>
    </div>
    <div class="combo-health-kpi">
      <span>${t("analytics.combos.col_ok_time")}</span>
      <strong class="combo-time-ok">${fmtDuration(avgOk)}</strong>
      <small>${fmtNumber(okCount)} ${t("analytics.combos.col_success").toLowerCase()}</small>
    </div>
    <div class="combo-health-kpi">
      <span>${t("analytics.combos.col_err_time")}</span>
      <strong class="combo-time-err">${fmtDuration(avgErr)}</strong>
      <small>${fmtNumber(errCount)} ${t("analytics.combos.col_errors").toLowerCase()}</small>
    </div>
    ${worst
      ? html`<div class="combo-health-kpi combo-health-worst">
          <span>${t("analytics.combos.health")}</span>
          <strong class=${healthTone(worst.success_rate_pct)}>${worst.combo_name || `#${worst.combo_id}`}</strong>
          <small>${worst.success_rate_pct.toFixed(1)}%</small>
        </div>`
      : html``}
  </div>`;
}

/** Combo health section for the analytics dashboard. */
export function renderComboHealth(): TemplateResult {
  if (byCombo.length === 0) {
    return html`<section class="card analytics-combo-card">
      <div class="analytics-card-heading">
        <div><h3>${t("analytics.combos.title")}</h3><p>${t("analytics.combos.subtitle")}</p></div>
      </div>
      <p class="empty">${t("analytics.combos.empty")}</p>
    </section>`;
  }

  const body = renderDataGrid<ByComboRow>({
    id: "analytics-by-combo-grid",
    className: "analytics-by-combo-grid",
    columns: renderColumns(),
    rows: byCombo,
    rowKey: (r) => r.combo_id,
    emptyMessage: t("analytics.combos.empty"),
    onAction: () => {},
  });

  return html`<section class="card analytics-combo-card">
    <div class="analytics-card-heading">
      <div>
        <h3>${t("analytics.combos.title")}</h3>
        <p>${t("analytics.combos.subtitle")}</p>
      </div>
    </div>
    ${renderSummary(byCombo)}
    <div class="analytics-table-wrap">${body}</div>
    <div class="combo-health-notes">
      <p>${t("analytics.combos.errors_note")}</p>
      <p>${t("analytics.combos.times_note")}</p>
    </div>
  </section>`;
}