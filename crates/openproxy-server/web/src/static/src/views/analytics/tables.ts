// views/analytics/tables.ts — table renderers for the analytics view:
// by-model ranking, by-provider ranking, monthly providers × months
// cost matrix, and the recent-errors table. All four are tabular
// (multi-column data) so a chart would lose detail; this module
// emits the same `<table>` markup the previous monolithic analytics.ts
// produced, just factored out so the entry point stays small.

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

/** By-model breakdown table. */
export function renderByModelTable(): TemplateResult {
  const body = byModel.length
    ? html`<div class="analytics-table-wrap table-wrap"><table class="by-model-table responsive-card-table">
        <thead><tr><th>${t("analytics.table.col_provider")}</th><th>${t("analytics.table.col_model")}</th><th class="num">${t("analytics.table.col_unique")}</th><th class="num">${t("analytics.metric.tokens")}</th><th class="num">Local Compress</th><th class="num">${t("analytics.table.col_cost")}</th></tr></thead>
        <tbody>${byModel.map((r) => html`<tr class="analytics-stat-card-row">
          <td class="col-stat-provider" data-label="Provider"><strong>${r.provider_id}</strong></td>
          <td class="analytics-model-cell col-stat-model" data-label="Model" title=${r.upstream_model_id}><code>${r.upstream_model_id}</code></td>
          <td class="num col-stat-unique" data-label="Unique reqs">${fmtNumber(r.unique_requests)}</td>
          <td class="num col-stat-tokens" data-label="Tokens">${fmtNumber(r.total_prompt_tokens + r.total_completion_tokens)}</td>
          <td class="num col-stat-compress" data-label="Compress" style="color: var(--color-success)">${r.avg_compression_savings_pct != null ? `${r.avg_compression_savings_pct < 1 && r.avg_compression_savings_pct > 0 ? r.avg_compression_savings_pct.toFixed(2) : Math.round(r.avg_compression_savings_pct)}%` : "—"}</td>
          <td class="num col-stat-cost" data-label="Cost">${fmtCost(r.total_cost_usd)}</td>
        </tr>`)}</tbody>
      </table></div>`
    : html`<p class="empty">${t("analytics.empty.no_usage")}</p>`;
  return card(t("analytics.chart.by_model"), body);
}

/** By-provider breakdown table. */
export function renderByProviderTable(): TemplateResult {
  const body = byProvider.length
    ? html`<div class="analytics-table-wrap table-wrap"><table class="by-provider-table responsive-card-table">
        <thead><tr><th>${t("analytics.table.col_provider")}</th><th class="num">${t("analytics.table.col_unique")}</th><th class="num">${t("analytics.table.col_total")}</th><th class="num">${t("analytics.table.col_winners")}</th><th class="num">${t("analytics.table.col_prompt_tok")}</th><th class="num">${t("analytics.table.col_completion_tok")}</th><th class="num">Local Compress</th><th class="num">${t("analytics.table.col_cost")}</th></tr></thead>
        <tbody>${byProvider.map((r) => html`<tr class="analytics-stat-card-row">
          <td class="col-stat-provider" data-label="Provider"><strong>${r.provider_id}</strong></td>
          <td class="num col-stat-unique" data-label="Unique reqs">${fmtNumber(r.unique_requests)}</td>
          <td class="num col-stat-total" data-label="Total rows">${fmtNumber(r.total_rows)}</td>
          <td class="num col-stat-winners" data-label="Winners">${fmtNumber(r.winners)}</td>
          <td class="num col-stat-prompt" data-label="Prompt tok">${fmtNumber(r.total_prompt_tokens)}</td>
          <td class="num col-stat-completion" data-label="Compl tok">${fmtNumber(r.total_completion_tokens)}</td>
          <td class="num col-stat-compress" data-label="Compress" style="color: var(--color-success)">${r.avg_compression_savings_pct != null ? `${r.avg_compression_savings_pct < 1 && r.avg_compression_savings_pct > 0 ? r.avg_compression_savings_pct.toFixed(2) : Math.round(r.avg_compression_savings_pct)}%` : "—"}</td>
          <td class="num col-stat-cost" data-label="Cost">${fmtCost(r.total_cost_usd)}</td>
        </tr>`)}</tbody>
      </table></div>`
    : html`<p class="empty">${t("analytics.empty.no_usage")}</p>`;
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
    return html`<tr><td>${p}</td>${tds}<td class="num total">${fmtCost(total)}</td></tr>`;
  });
  const footMonths = pivot.months.map((m) => {
    const tt = pivot.totalsByMonth.get(m) ?? 0;
    return html`<th class="num">${fmtCost(tt)}</th>`;
  });
  return card(t("analytics.monthly.title"), html`<div class="analytics-table-wrap table-wrap"><table class="monthly-matrix">
    <thead>
      <tr><th>${t("analytics.monthly.col_provider")}</th>${pivot.months.map((m) => html`<th>${m}</th>`)}<th class="num">${t("analytics.monthly.col_total")}</th></tr>
    </thead>
    <tbody>${bodyRows}</tbody>
    <tfoot>
      <tr><th>${t("analytics.monthly.col_total")}</th>${footMonths}<th class="num">${fmtCost(pivot.grandTotal)}</th></tr>
    </tfoot>
  </table></div>`);
}

/** Recent errors table (latest 10 rows). Each row is clickable →
 *  `#/logs?request_id=…` to jump to the live-logs view filtered to
 *  that request. Renders a mobile card variant alongside the desktop
 *  table cells. */
export function renderRecentErrors(): TemplateResult {
  const slice = (errors || []).slice(0, 10);
  if (slice.length === 0) {
    return card(t("analytics.errors.title"), html`<p class="empty">${t("analytics.empty.no_errors")}</p>`);
  }
  const body = html`<div class="analytics-table-wrap table-wrap"><table class="analytics-errors-table responsive-card-table">
    <thead><tr><th>${t("analytics.errors.col_time")}</th><th>${t("analytics.errors.col_provider")}</th><th>${t("analytics.errors.col_model")}</th><th>${t("analytics.errors.col_status")}</th><th>${t("analytics.errors.col_message")}</th><th><span class="sr-only">${t("analytics.errors.view")}</span></th></tr></thead>
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
        <td class="col-err-msg" data-label="Message">${msg}<br><small class="muted"><code>${traceId}</code></small></td>
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
