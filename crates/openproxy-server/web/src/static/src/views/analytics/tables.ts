// views/analytics/tables.ts — by-model, by-provider, cost-matrix, and recent-errors tables.

import { html, type TemplateResult } from "lit-html";
import { t } from "../../i18n/index.js";
import {
  byModel,
  byProvider,
  card,
  errors,
  fmtCost,
  fmtDateTime,
  fmtNumber,
  monthlyByProvider,
  pivotMonthlyByProvider,
} from "./shared.js";
import { renderDataGrid, type GridColumn } from "../../components/data-grid.js";
import type { ByModelRow, ByProviderRow } from "../../lib/types/api.js";

/** By-model breakdown table rendered with renderDataGrid (dense 32px rows). */
export function renderByModelTable(): TemplateResult {
  if (byModel.length === 0) {
    return card(t("analytics.chart.by_model"), html`<p class="empty">${t("analytics.empty.no_usage")}</p>`);
  }

  const columns: GridColumn<ByModelRow>[] = [
    {
      key: "provider_id",
      label: t("analytics.table.col_provider"),
      min: 100,
      max: 160,
      fold: 3,
      render: (r) => html`<strong class="analytics-provider-cell">${r.provider_id}</strong>`,
    },
    {
      key: "upstream_model_id",
      label: t("analytics.table.col_model"),
      min: 140,
      elastic: true,
      render: (r) => html`<code class="analytics-model-code dg-mono" title=${r.upstream_model_id}>${r.upstream_model_id}</code>`,
    },
    {
      key: "unique_requests",
      label: t("analytics.table.col_unique"),
      max: 110,
      fold: 2,
      render: (r) => html`<span class="dg-num">${fmtNumber(r.unique_requests)}</span>`,
    },
    {
      key: "tokens",
      label: t("analytics.metric.tokens"),
      max: 120,
      fold: 1,
      render: (r) => html`<span class="dg-num">${fmtNumber(r.total_prompt_tokens + r.total_completion_tokens)}</span>`,
    },
    {
      key: "compression",
      label: "Compress",
      max: 100,
      fold: 4,
      render: (r) => html`<span class="dg-num analytics-compress-good">${r.avg_compression_savings_pct != null ? `${r.avg_compression_savings_pct < 1 && r.avg_compression_savings_pct > 0 ? r.avg_compression_savings_pct.toFixed(2) : Math.round(r.avg_compression_savings_pct)}%` : "—"}</span>`,
    },
    {
      key: "cost",
      label: t("analytics.table.col_cost"),
      min: 90,
      render: (r) => html`<span class="dg-num analytics-cost-cell">${fmtCost(r.total_cost_usd)}</span>`,
    },
  ];

  const body = renderDataGrid<ByModelRow>({
    id: "analytics-by-model-grid",
    className: "analytics-by-model-grid",
    columns,
    rows: byModel,
    rowKey: (r) => `${r.provider_id}:${r.upstream_model_id}`,
    emptyMessage: t("analytics.empty.no_usage"),
    onAction: () => {},
  });

  return card(t("analytics.chart.by_model"), body);
}

/** By-provider breakdown table rendered with renderDataGrid (dense 32px rows). */
export function renderByProviderTable(): TemplateResult {
  if (byProvider.length === 0) {
    return card(t("analytics.chart.by_provider"), html`<p class="empty">${t("analytics.empty.no_usage")}</p>`);
  }

  const columns: GridColumn<ByProviderRow>[] = [
    {
      key: "provider_id",
      label: t("analytics.table.col_provider"),
      min: 120,
      elastic: true,
      render: (r) => html`<strong class="analytics-provider-cell">${r.provider_id}</strong>`,
    },
    {
      key: "unique_requests",
      label: t("analytics.table.col_unique"),
      max: 110,
      fold: 3,
      render: (r) => html`<span class="dg-num">${fmtNumber(r.unique_requests)}</span>`,
    },
    {
      key: "total_rows",
      label: t("analytics.table.col_total"),
      max: 110,
      fold: 4,
      render: (r) => html`<span class="dg-num">${fmtNumber(r.total_rows)}</span>`,
    },
    {
      key: "winners",
      label: t("analytics.table.col_winners"),
      max: 110,
      fold: 5,
      render: (r) => html`<span class="dg-num">${fmtNumber(r.winners)}</span>`,
    },
    {
      key: "prompt_tok",
      label: t("analytics.table.col_prompt_tok"),
      max: 120,
      fold: 1,
      render: (r) => html`<span class="dg-num">${fmtNumber(r.total_prompt_tokens)}</span>`,
    },
    {
      key: "completion_tok",
      label: t("analytics.table.col_completion_tok"),
      max: 120,
      fold: 2,
      render: (r) => html`<span class="dg-num">${fmtNumber(r.total_completion_tokens)}</span>`,
    },
    {
      key: "compression",
      label: "Compress",
      max: 100,
      fold: 6,
      render: (r) => html`<span class="dg-num analytics-compress-good">${r.avg_compression_savings_pct != null ? `${r.avg_compression_savings_pct < 1 && r.avg_compression_savings_pct > 0 ? r.avg_compression_savings_pct.toFixed(2) : Math.round(r.avg_compression_savings_pct)}%` : "—"}</span>`,
    },
    {
      key: "cost",
      label: t("analytics.table.col_cost"),
      min: 90,
      render: (r) => html`<span class="dg-num analytics-cost-cell">${fmtCost(r.total_cost_usd)}</span>`,
    },
  ];

  const body = renderDataGrid<ByProviderRow>({
    id: "analytics-by-provider-grid",
    className: "analytics-by-provider-grid",
    columns,
    rows: byProvider,
    rowKey: (r) => r.provider_id,
    emptyMessage: t("analytics.empty.no_usage"),
    onAction: () => {},
  });

  return card(t("analytics.chart.by_provider"), body);
}

/** Providers × months cost matrix. Cells show cost (USD) with
 *  token counts as a `title` tooltip; per-row, per-column, and grand
 *  totals are computed by the shared `pivotMonthlyByProvider` helper. */
export function renderMonthlyMatrix(): TemplateResult {
  const pivot = pivotMonthlyByProvider(monthlyByProvider);
  if (pivot.providers.length === 0 || pivot.months.length === 0) {
    return card(t("analytics.monthly.title"), html`<p class="empty">${t("analytics.empty.no_usage")}</p>`);
  }
  const bodyRows = pivot.providers.map((p) => {
    const pCells = pivot.cells.get(p);
    const tds = pivot.months.map((m) => {
      const r = pCells?.get(m);
      if (!r) return html`<td class="num">—</td>`;
      const title = `${r.unique_requests} unique / ${r.total_rows} rows · ${r.total_prompt_tokens} prompt tok · ${r.total_completion_tokens} completion tok`;
      return html`<td class="num" title=${title}>${fmtCost(r.total_cost_usd)}</td>`;
    });
    const total = pivot.totalsByProvider.get(p) ?? 0;
    return html`<tr><td class="col-provider"><strong>${p}</strong></td>${tds}<td class="num total">${fmtCost(total)}</td></tr>`;
  });
  const footMonths = pivot.months.map((m) => {
    const tt = pivot.totalsByMonth.get(m) ?? 0;
    return html`<th class="num">${fmtCost(tt)}</th>`;
  });
  return card(t("analytics.monthly.title"), html`<div class="analytics-table-wrap table-wrap"><table class="monthly-matrix">
    <thead>
      <tr><th class="col-provider">${t("analytics.monthly.col_provider")}</th>${pivot.months.map((m) => html`<th class="num">${m}</th>`)}<th class="num">${t("analytics.monthly.col_total")}</th></tr>
    </thead>
    <tbody>${bodyRows}</tbody>
    <tfoot>
      <tr><th class="col-provider">${t("analytics.monthly.col_total")}</th>${footMonths}<th class="num">${fmtCost(pivot.grandTotal)}</th></tr>
    </tfoot>
  </table></div>`);
}

/** Recent errors table (latest 10 rows). Each row is clickable, which mirrors the
 *  request into the log-detail modal and navigates to `#/logs?request_id=…`
 *  filtered to it. Renders a mobile card variant alongside the desktop table. */
export function renderRecentErrors(): TemplateResult {
  const slice = (errors || []).slice(0, 10);
  if (slice.length === 0) {
    return card(t("analytics.errors.title"), html`<p class="empty">${t("analytics.empty.no_errors")}</p>`);
  }
  const body = html`<div class="analytics-table-wrap table-wrap"><table class="analytics-errors-table responsive-card-table">
    <thead><tr><th class="col-err-time">${t("analytics.errors.col_time")}</th><th class="col-err-provider">${t("analytics.errors.col_provider")}</th><th class="col-err-model">${t("analytics.errors.col_model")}</th><th class="col-err-status">${t("analytics.errors.col_status")}</th><th class="col-err-msg">${t("analytics.errors.col_message")}</th><th class="col-err-action"><span class="sr-only">${t("analytics.errors.view")}</span></th></tr></thead>
    <tbody>${slice.map((e) => {
      const href = `#/logs?request_id=${e.request_id || ""}`;
      const msg = e.error_msg_redacted || t("analytics.errors.no_message");
      const traceId = e.trace_id || "";
      const timeText = fmtDateTime(e.created_at || "");

      return html`<tr class="analytics-error-card-row">
        <!-- Desktop Table Cells -->
        <td class="col-err-time" data-label="Time"><time datetime=${e.created_at || ""}>${timeText}</time></td>
        <td class="col-err-provider" data-label="Provider"><strong>${e.provider_id || ""}</strong></td>
        <td class="analytics-model-cell col-err-model" data-label="Model" title=${e.upstream_model_id || ""}><code>${e.upstream_model_id || ""}</code></td>
        <td class="col-err-status" data-label="Status"><span class="status-pill err">${e.status_code || "—"}</span></td>
        <td class="col-err-msg" data-label="Message"><span class="analytics-err-text">${msg}</span><br><small class="muted"><code class="analytics-trace-code">${traceId}</code></small></td>
        <td class="col-err-action"><a class="analytics-error-link" href=${href} aria-label=${t("analytics.errors.view")}>→ Inspect</a></td>

        <!-- Mobile Card Structure -->
        <td class="mobile-analytics-error-card-cell">
          <!-- Línea 1: Status + Provider/Model + Icono Ojo de Inspección -->
          <div class="e-card-line-1">
            <div class="e-card-target">
              <span class="e-card-status-pill">${e.status_code || "—"}</span>
              <span class="e-card-provider">${e.provider_id || ""}</span>
              <span class="e-card-model" title="${e.upstream_model_id || ""}">/ ${e.upstream_model_id || ""}</span>
            </div>
            <a
              class="analytics-error-link"
              href=${href}
              title="Inspect request in live logs"
              aria-label="Inspect request in live logs"
            >
              <svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                <path d="M1.5 8s2.5-5 6.5-5 6.5 5 6.5 5-2.5 5-6.5 5-6.5-5z"></path>
                <circle cx="8" cy="8" r="2.5"></circle>
              </svg>
            </a>
          </div>

          <!-- Línea 2: Mensaje de Error + Trace ID & Fecha -->
          <div class="e-card-line-2">
            <div class="e-card-msg-text" title="${msg}">${msg}</div>
            <div class="e-card-subline">
              <code class="e-card-trace-id" title="${traceId}">${traceId}</code>
              <span>${timeText}</span>
            </div>
          </div>
        </td>
      </tr>`;
    })}</tbody>
  </table></div>`;
  return card(t("analytics.errors.title"), body);
}
