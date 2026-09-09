// views/analytics/index.ts — entry point for the analytics dashboard.
// Owns the top-level layout (toolbar + body), the data-fetch sequence
// on mount, and the chart lifecycle wiring. The KPI cards, ranking,
// status, latency, and race blocks are composed here (not in a
// sub-module) because they're cheap to render and they're the glue
// between the toolbar (charts.ts) and the tables (tables.ts).

import { html, type TemplateResult } from "lit-html";
import { state } from "../../state/index.js";
import { api } from "../../state/api.js";
import { mountView, requestUpdate } from "../../state/reactive.js";
import { t } from "../../i18n/index.js";
import {
  buildUsageQuery,
  byModel,
  byProvider,
  byStatus,
  errorMsg,
  fmtCost,
  fmtNumber,
  fmtPercent,
  fmtDuration,
  groupByStatus,
  latency,
  loading,
  metric,
  parseHashParams,
  races,
  setViewState,
  summary,
  type ApiKeyFilterRow,
  type LatencyPayload,
  type RaceStatsPayload,
  type RankingItem,
} from "./shared.js";
import {
  createAnalyticsCharts,
  destroyAnalyticsCharts,
  refreshAnalyticsChartTheme,
  renderAnalyticsToolbar,
} from "./charts.js";
import {
  renderByModelTable,
  renderByProviderTable,
  renderMonthlyMatrix,
  renderRecentErrors,
} from "./tables.js";
import type {
  ByDayRow,
  ByModelRow,
  ByProviderRow,
  ByStatusRow,
  ErrorRow,
  MonthlyByProviderRow,
  Provider,
  UsageSummary,
} from "../../lib/types/api.js";

// ── KPI metrics strip ──────────────────────────────────────────────

function renderMetrics(): TemplateResult {
  if (!summary) return html``;
  const buckets = groupByStatus(byStatus);
  const statusTotal = buckets.s2xx + buckets.s4xx + buckets.s5xx + buckets.other;
  const successRate = statusTotal > 0 ? buckets.s2xx / statusTotal : NaN;
  const totalTokens = summary.total_prompt_tokens + summary.total_completion_tokens;
  const costPerRequest = summary.unique_requests > 0 ? summary.total_cost_usd / summary.unique_requests : 0;
  const p95Total = latency?.p95_total_ms;
  const successTone = !Number.isFinite(successRate) ? "" : successRate >= 0.99 ? "is-good" : successRate >= 0.95 ? "is-warn" : "is-bad";
  return html`<div class="analytics-metrics" aria-label=${t("analytics.summary")}>
    ${metric(t("analytics.summary.unique_requests"), fmtNumber(summary.unique_requests), `${fmtNumber(summary.total_attempts)} ${t("analytics.metric.attempts")}`)}
    ${metric(t("analytics.metric.success_rate"), fmtPercent(successRate), `${fmtNumber(buckets.s4xx + buckets.s5xx)} ${t("analytics.metric.failed_responses")}`, successTone)}
    ${metric(t("analytics.metric.tokens"), fmtNumber(totalTokens), `${fmtNumber(summary.total_prompt_tokens)} in · ${fmtNumber(summary.total_completion_tokens)} out`)}
    ${metric("API Cache", fmtPercent(summary.total_prompt_tokens > 0 ? summary.total_cached_tokens / summary.total_prompt_tokens : 0), `${fmtNumber(summary.total_cached_tokens)} prompt tokens cached`, summary.total_cached_tokens > 0 ? "is-good" : "")}
    ${metric("Local Compression", fmtPercent((summary.avg_compression_savings_pct ?? 0) / 100), "Avg token savings when active", "is-good")}
    ${metric(t("analytics.summary.total_cost"), fmtCost(summary.total_cost_usd), `${fmtCost(costPerRequest)} ${t("analytics.metric.per_request")}`)}
    ${metric(t("analytics.metric.avg_success_total"), fmtDuration(summary.avg_success_total_ms), t("analytics.metric.avg_success_phases", { connect: fmtDuration(summary.avg_success_connect_ms), ttft: fmtDuration(summary.avg_success_ttft_ms) }))}
    ${metric(t("analytics.metric.avg_ttft"), fmtDuration(summary.avg_ttft_ms), `${fmtDuration(summary.avg_total_ms)} ${t("analytics.metric.avg_total")}`)}
    ${metric(t("analytics.metric.p95_latency"), fmtDuration(p95Total), `${fmtNumber(latency?.samples ?? 0)} ${t("analytics.metric.successful_samples")}`)}
  </div>`;
}

// ── Ranking list (used by model + provider strips) ──────────────────

function renderRanking(title: string, subtitle: string, items: RankingItem[]): TemplateResult {
  const ranked = [...items].sort((a, b) => b.cost - a.cost).slice(0, 8);
  const total = items.reduce((sum, item) => sum + item.cost, 0);
  const max = ranked[0]?.cost ?? 0;
  return html`<section class="card analytics-ranking-card">
    <div class="analytics-card-heading">
      <div><h3>${title}</h3><p>${subtitle}</p></div>
    </div>
    ${ranked.length === 0 ? html`<p class="empty">${t("analytics.empty.no_usage")}</p>` : html`
      <div class="analytics-ranking-list">
        ${ranked.map((item, index) => {
          const width = max > 0 ? Math.max(2, item.cost / max * 100) : 0;
          const share = total > 0 ? item.cost / total : 0;
          return html`<div class="analytics-ranking-row">
            <span class="analytics-rank">${index + 1}</span>
            <div class="analytics-ranking-main">
              <div class="analytics-ranking-copy">
                <span class="analytics-ranking-name" title=${item.name}>${item.name}</span>
                <span class="analytics-ranking-context">${item.context} · ${fmtNumber(item.requests)} req</span>
              </div>
              <div class="analytics-ranking-value"><strong>${fmtCost(item.cost)}</strong><span>${fmtPercent(share)}</span></div>
              <div class="analytics-ranking-track" aria-hidden="true"><span style=${`width:${width}%`}></span></div>
            </div>
          </div>`;
        })}
      </div>`}
  </section>`;
}

// ── Status health card ──────────────────────────────────────────────

function renderStatusHealth(): TemplateResult {
  const buckets = groupByStatus(byStatus);
  const entries = [
    { label: "2xx", value: buckets.s2xx, cls: "ok" },
    { label: "4xx", value: buckets.s4xx, cls: "warn" },
    { label: "5xx", value: buckets.s5xx, cls: "err" },
    { label: t("analytics.status.other"), value: buckets.other, cls: "other" },
  ];
  const total = entries.reduce((sum, item) => sum + item.value, 0);
  const successRate = total > 0 ? buckets.s2xx / total : NaN;
  const health = !Number.isFinite(successRate)
    ? t("analytics.status.no_data")
    : successRate >= 0.99 ? t("analytics.status.healthy")
    : successRate >= 0.95 ? t("analytics.status.attention")
    : t("analytics.status.degraded");
  const healthTone = !Number.isFinite(successRate) ? "neutral" : successRate >= 0.99 ? "good" : successRate >= 0.95 ? "warn" : "bad";
  return html`<section class="card analytics-status-card">
    <div class="analytics-card-heading"><div><h3>${t("analytics.chart.status_codes")}</h3><p>${t("analytics.chart.status_codes.subtitle")}</p></div><span class="analytics-health-label ${healthTone}">${health}</span></div>
    <div class="analytics-status-hero"><strong>${fmtPercent(successRate)}</strong><span>${t("analytics.metric.success_rate")}</span></div>
    <div class="analytics-status-track" aria-label=${t("analytics.chart.status_codes")}>
      ${entries.map((item) => html`<span class=${item.cls} style=${`width:${total > 0 ? item.value / total * 100 : 0}%`}></span>`)}
    </div>
    <div class="analytics-status-legend">
      ${entries.map((item) => html`<div><span class="analytics-status-dot ${item.cls}"></span><span>${item.label}</span><strong>${fmtNumber(item.value)}</strong><small>${fmtPercent(total > 0 ? item.value / total : NaN)}</small></div>`)}
    </div>
  </section>`;
}

// ── Latency percentiles card ────────────────────────────────────────

function renderLatencyBlock(): TemplateResult {
  const rows = [
    { label: t("analytics.latency.connect"), p50: latency?.p50_connect_ms, p95: latency?.p95_connect_ms },
    { label: t("analytics.latency.ttft"), p50: latency?.p50_ttft_ms, p95: latency?.p95_ttft_ms },
    { label: t("analytics.latency.total"), p50: latency?.p50_total_ms, p95: latency?.p95_total_ms },
  ];
  const max = Math.max(1, ...rows.map((row) => row.p95 ?? 0));
  return html`<section class="card analytics-latency-card">
    <div class="analytics-card-heading"><div><h3>${t("analytics.chart.latency")}</h3><p>${t("analytics.chart.latency.subtitle")} · ${fmtNumber(latency?.samples ?? 0)} ${t("analytics.latency.samples").toLowerCase()}</p></div></div>
    <div class="analytics-latency-key"><span class="p50">p50</span><span class="p95">p95</span></div>
    <div class="analytics-latency-list">
      ${rows.map((row) => html`<div class="analytics-latency-row">
        <div class="analytics-latency-row-head"><strong>${row.label}</strong><span><small>${fmtDuration(row.p50)}</small><b>${fmtDuration(row.p95)}</b></span></div>
        <div class="analytics-latency-track">
          <span class="p95" style=${`width:${(row.p95 ?? 0) / max * 100}%`}></span>
          <span class="p50" style=${`width:${(row.p50 ?? 0) / max * 100}%`}></span>
        </div>
      </div>`)}
    </div>
    <div class="analytics-throughput-foot"><span>${t("analytics.metric.generation_speed")}</span><strong>p50 ${latency?.p50_tokens_per_sec == null ? "—" : fmtNumber(latency.p50_tokens_per_sec)} tok/s</strong><strong>p95 ${latency?.p95_tokens_per_sec == null ? "—" : fmtNumber(latency.p95_tokens_per_sec)} tok/s</strong></div>
  </section>`;
}

// ── Race outcomes card ──────────────────────────────────────────────

function renderRaceBlock(): TemplateResult {
  const total = races?.total_races ?? 0;
  const winners = races?.winners ?? 0;
  const losers = races?.losers ?? 0;
  const completionRate = total > 0 ? winners / total : NaN;
  const extraAttempts = total > 0 ? losers / total : 0;
  return html`<section class="card analytics-race-card">
    <div class="analytics-card-heading"><div><h3>${t("analytics.chart.race_outcomes")}</h3><p>${t("analytics.chart.race_outcomes.subtitle")}</p></div></div>
    <div class="analytics-race-grid">
      <div><span>${t("analytics.chart.race_outcomes.total")}</span><strong>${fmtNumber(total)}</strong></div>
      <div><span>${t("analytics.race.completion")}</span><strong>${fmtPercent(completionRate)}</strong></div>
      <div><span>${t("analytics.race.avg_winner_position")}</span><strong>${races?.avg_winner_position == null ? "—" : `#${races.avg_winner_position.toFixed(1)}`}</strong></div>
      <div><span>${t("analytics.race.extra_attempts")}</span><strong>${total > 0 ? extraAttempts.toFixed(1) : "—"}</strong></div>
    </div>
    <div class="analytics-race-progress"><span style=${`width:${Number.isFinite(completionRate) ? Math.min(100, completionRate * 100) : 0}%`}></span></div>
    <p class="analytics-race-note">${fmtNumber(winners)} ${t("analytics.race.winners")} · ${fmtNumber(losers)} ${t("analytics.race.cancelled_contenders")}</p>
  </section>`;
}

// ── Body composition ────────────────────────────────────────────────

function renderBody(): TemplateResult {
  if (loading) return html`<div class="loading">${t("common.loading")}</div>`;
  if (errorMsg) return html`<div class="banner banner-error">${errorMsg}</div>`;
  if (!summary) return html`<div class="loading">${t("common.loading")}</div>`;

  const nullPricingCount = summary.rows_with_null_pricing ?? 0;
  const nullPricingBanner = nullPricingCount > 0
    ? html`<div class="banner banner-warning">${t("analytics.null_pricing_warning", { count: nullPricingCount })}</div>`
    : html``;

  const modelItems: RankingItem[] = byModel.map((row) => ({
    name: row.upstream_model_id,
    context: row.provider_id,
    requests: row.unique_requests,
    cost: row.total_cost_usd,
  }));
  const providerItems: RankingItem[] = byProvider.map((row) => ({
    name: row.provider_id,
    context: `${fmtNumber(row.total_prompt_tokens + row.total_completion_tokens)} tok`,
    requests: row.unique_requests,
    cost: row.total_cost_usd,
  }));

  return html`
    ${nullPricingBanner}
    ${renderMetrics()}
    <div class="analytics-primary-grid">
      <section class="card analytics-trend-card">
        <div class="analytics-card-heading"><div><h3>${t("analytics.chart.daily_usage")}</h3><p>${t("analytics.chart.daily_usage.subtitle")}</p></div></div>
        <div class="analytics-chart-container" id="chart-daily-usage"></div>
      </section>
      ${renderStatusHealth()}
    </div>
    <div class="analytics-insight-grid">
      ${renderRanking(t("analytics.chart.by_model"), t("analytics.chart.by_model.subtitle"), modelItems)}
      ${renderRanking(t("analytics.chart.by_provider"), t("analytics.chart.by_provider.subtitle"), providerItems)}
    </div>
    <div class="analytics-insight-grid">
      ${renderLatencyBlock()}
      ${renderRaceBlock()}
    </div>
    <details class="analytics-details">
      <summary><span>${t("analytics.details.title")}</span><small>${t("analytics.details.subtitle")}</small></summary>
      <div class="analytics-details-body">
        ${renderByModelTable()}
        ${renderByProviderTable()}
        ${renderMonthlyMatrix()}
      </div>
    </details>
    ${renderRecentErrors()}`;
}

function renderAnalytics(): TemplateResult {
  const { preset, providerId, apiKeyId } = parseHashParams();
  return html`
    <div class="analytics-dashboard">
      <div class="page-header analytics-header">
        <div><span class="page-eyebrow">${t("nav.analytics")}</span><h2>${t("analytics.title")}</h2><p>${t("analytics.subtitle")}</p></div>
      </div>
      ${renderAnalyticsToolbar(providerId, apiKeyId, preset)}
      ${renderBody()}
    </div>`;
}

// ── Mount ───────────────────────────────────────────────────────────

/** Mount the analytics view. Returns a cleanup function that tears
 *  down the chart instances + the theme-change listener. */
export async function mountAnalytics(): Promise<(() => void) | void> {
  const el = document.getElementById("main");
  if (!el) return;

  // Reset view-local state on every mount. The previous mount's
  // charts were destroyed by its cleanup function; we start fresh.
  setViewState({
    loading: true,
    errorMsg: null,
  });
  const cleanupReactive = mountView(el, renderAnalytics);
  document.addEventListener("themechange", refreshAnalyticsChartTheme);

  const { preset, providerId, apiKeyId } = parseHashParams();
  try {
    // Combined query string for every `/usage/*` fetch. The errors
    // endpoint additionally carries `limit=10` so we cap the table
    // at 10 rows (the server's default is 100).
    const usageQ = buildUsageQuery(preset, providerId, apiKeyId);
    const errorsQ = buildUsageQuery(preset, providerId, apiKeyId, { limit: "10" });
    const [
      summaryResp, byModelResp, byProviderResp, monthlyByProviderResp, latencyResp, racesResp,
      byDayResp, byStatusResp, errorsResp, providersResp, apiKeysResp,
    ] = await Promise.all([
      api(`/usage/summary${usageQ}`) as Promise<UsageSummary>,
      api(`/usage/by-model${usageQ}`) as Promise<ByModelRow[]>,
      api(`/usage/by-provider${usageQ}`) as Promise<ByProviderRow[]>,
      api(`/usage/monthly-by-provider${usageQ}`) as Promise<MonthlyByProviderRow[]>,
      api(`/usage/latency${usageQ}`) as Promise<LatencyPayload>,
      api(`/usage/races${usageQ}`) as Promise<RaceStatsPayload>,
      api(`/usage/by-day${usageQ}`) as Promise<ByDayRow[]>,
      api(`/usage/by-status${usageQ}`) as Promise<ByStatusRow[]>,
      api(`/usage/errors${errorsQ}`) as Promise<ErrorRow[]>,
      // Filter dropdown options — use the state cache when the
      // bg-poll has already populated it (the common case); fall
      // back to a direct fetch on a cold paint. Backfill the cache
      // after the fetch so the next navigation is instant.
      (state.providers && state.providers.length)
        ? Promise.resolve(state.providers)
        : api("/providers") as Promise<Provider[]>,
      (state.apiKeys && state.apiKeys.length)
        ? Promise.resolve(state.apiKeys as ApiKeyFilterRow[])
        : api("/keys") as Promise<ApiKeyFilterRow[]>,
    ]);

    setViewState({
      summary: summaryResp,
      byModel: byModelResp,
      byProvider: byProviderResp,
      monthlyByProvider: monthlyByProviderResp,
      latency: latencyResp,
      races: racesResp,
      byDay: byDayResp,
      byStatus: byStatusResp,
      errors: errorsResp,
      providers: providersResp,
      apiKeys: apiKeysResp,
      loading: false,
    });

    if (providersResp) state.providers = providersResp;
    if (apiKeysResp) state.apiKeys = apiKeysResp as typeof state.apiKeys;

    requestUpdate();
    // Create the uPlot charts after the data-bearing render commits.
    // `requestUpdate()` schedules a microtask re-render; the
    // `requestAnimationFrame` callback runs after the next paint, so
    // the chart-container `<div>`s exist by then. `createAnalyticsCharts`
    // is idempotent (no-op if `charts` is already set).
    requestAnimationFrame(() => {
      createAnalyticsCharts();
    });
  } catch (e: unknown) {
    setViewState({
      errorMsg: e instanceof Error ? e.message : String(e),
      providers: state.providers || [],
      apiKeys: (state.apiKeys || []) as ApiKeyFilterRow[],
      loading: false,
    });
    requestUpdate();
  }
  return () => {
    destroyAnalyticsCharts();
    document.removeEventListener("themechange", refreshAnalyticsChartTheme);
    cleanupReactive();
  };
}
