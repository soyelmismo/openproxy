// views/analytics.ts — analytics dashboards: filter bar, time-range
// preset selector, summary, latency percentiles, race stats, daily
// usage chart, status distribution, by-model breakdown,
// by-provider totals, providers × months cost matrix, and a recent
// errors table. Every `/usage/*` endpoint is queried with a combined
// `?preset=X&provider_id=Y&api_key_id=Z` query built from the URL
// hash so the dashboard reflects the selected window + scope. The
// hash looks like `#/analytics?range=this_month&provider_id=openrouter&api_key_id=12`;
// the router regex tolerates the trailing `?...` suffix and the
// `hashchange` event re-mounts the view so every fetch picks up the
// new query string.
//
// B3 MIGRATION: all charts now use uPlot via `components/uplot-chart.ts`
// (the same wrapper the home dashboard uses). Replaces the old
// inline-SVG `dailyUsageChart` + `statusDonut` from
// `components/charts.ts`. The chart lifecycle follows the home view's
// pattern: instances are created once after the first data-bearing
// render, `setData(...)` is called on each fetch completion, and
// `destroy()` is called on view unmount. Tables (by-model, by-provider,
// monthly matrix, recent errors) remain tables — their data is
// naturally tabular and a bar chart would lose the multi-column detail
// (unique_requests, total_rows, tokens, cost) that the operator needs.

import { html, type TemplateResult } from "lit-html";
import type uPlot from "uplot";
import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { mountView, requestUpdate } from "../state/reactive.js";
import { t } from "../i18n/index.js";
import {
  buildDailyUsageChart,
  buildCategoryBarsChart,
  observeResize,
  CHART_COLORS,
} from "../components/uplot-chart.js";
import type {
  ByDayRow,
  ByModelRow,
  ByProviderRow,
  ByStatusRow,
  ErrorRow,
  MonthlyByProviderRow,
  Provider,
  UsagePreset,
  UsageSummary,
} from "../lib/types/api.js";

// The `/admin/api-keys` payload shape. Defined locally (not in
// lib/types/api.ts) because G3 only exported the core structs the
// rest of the dashboard already uses — the api-key row lives in a
// separate file on the Rust side. Mirrors the local interface in
// `views/keys.ts`. We only need the three columns the filter
// dropdown reads.
interface ApiKeyFilterRow {
  id: number;
  label: string | null;
  key_prefix: string | null;
}

// The latency / races endpoints don't have a dedicated type in
// lib/types/api.ts (the by-model / by-provider / monthly rows do).
// Both payloads are flat numeric objects — modelled as interfaces
// with optional number fields so the format-fallback branches in the
// renderer can read them as `null`/`undefined` when the upstream
// omitted them.
interface LatencyPayload {
  samples?: number;
  p50_connect_ms?: number | null;
  p95_connect_ms?: number | null;
  p50_ttft_ms?: number | null;
  p95_ttft_ms?: number | null;
  p50_total_ms?: number | null;
  p95_total_ms?: number | null;
}
interface RaceStatsPayload {
  total_races?: number;
  winners?: number;
  losers?: number;
}

// Ordered list of presets rendered as buttons in the time-range
// selector. The order is the same as the backend's `resolve_preset`
// match arms, grouped short-window → long-window → custom.
const PRESETS: readonly UsagePreset[] = [
  "today", "7d", "30d",
  "this_month", "last_month", "last_6_months",
  "ytd", "custom",
];

// Friendly labels for the preset buttons, keyed by the preset value.
// `custom` is labelled "All time" because the dashboard has no custom
// date-picker — selecting it sends no `?preset=`, so the server
// returns the full history.
const PRESET_LABEL_KEYS: Record<UsagePreset, string> = {
  today: "analytics.preset.today",
  "7d": "analytics.preset.7d",
  "30d": "analytics.preset.30d",
  this_month: "analytics.preset.this_month",
  last_month: "analytics.preset.last_month",
  last_6_months: "analytics.preset.last_6_months",
  ytd: "analytics.preset.ytd",
  custom: "analytics.preset.custom",
};

function fmtCost(v: number): string {
  return `$${v.toFixed(2)}`;
}

/** Truncate a string to `maxLen` characters, appending an ellipsis if
 *  truncated. Used for bar-chart category labels (model names can be
 *  30+ chars; the X axis only has room for ~14 before labels overlap
 *  even with 45° rotation). */
function truncate(s: string, maxLen: number): string {
  if (s.length <= maxLen) return s;
  return s.slice(0, maxLen - 1) + "…";
}

// ── URL hash parsing ────────────────────────────────────────────────
// The hash carries three params: `range` (preset), `provider_id`,
// and `api_key_id`. All three are optional; the preset defaults to
// `this_month` and the filters default to empty (= "all"). Setting
// any of them via `setHashParams` updates the hash, which fires
// `hashchange` and re-mounts the view — so every fetch picks up the
// new query string. No manual re-render needed.

interface AnalyticsHashParams {
  preset: UsagePreset;
  providerId: string;
  apiKeyId: string;
}

function parseHashParams(): AnalyticsHashParams {
  const hash = location.hash || "#/analytics";
  const qIdx = hash.indexOf("?");
  const query = qIdx >= 0 ? hash.slice(qIdx + 1) : "";
  const params = new URLSearchParams(query);
  const rangeRaw = params.get("range") || "this_month";
  const preset: UsagePreset = (PRESETS as readonly string[]).includes(rangeRaw)
    ? rangeRaw as UsagePreset
    : "this_month";
  return {
    preset,
    providerId: params.get("provider_id") || "",
    apiKeyId: params.get("api_key_id") || "",
  };
}

// Swap any subset of the hash params. Setting a filter to `""`
// deletes it from the URL so a cleared filter doesn't linger as
// `?provider_id=`. The path component of the hash is preserved.
function setHashParams(updates: Partial<AnalyticsHashParams>): void {
  const hash = location.hash || "#/analytics";
  const qIdx = hash.indexOf("?");
  const path = qIdx >= 0 ? hash.slice(0, qIdx) : hash;
  const query = qIdx >= 0 ? hash.slice(qIdx + 1) : "";
  const params = new URLSearchParams(query);
  if (updates.preset !== undefined) params.set("range", updates.preset);
  if (updates.providerId !== undefined) {
    if (updates.providerId) params.set("provider_id", updates.providerId);
    else params.delete("provider_id");
  }
  if (updates.apiKeyId !== undefined) {
    if (updates.apiKeyId) params.set("api_key_id", updates.apiKeyId);
    else params.delete("api_key_id");
  }
  const qs = params.toString();
  location.hash = qs ? `${path}?${qs}` : path;
}

// Build the combined query string used by every `/usage/*` fetch.
// Returns "" when there's nothing to filter on (preset=custom + no
// provider + no key) — the server then returns the full history.
// `extra` lets a caller append further params (e.g. `limit=10` for
// the errors endpoint).
function buildUsageQuery(
  preset: UsagePreset,
  providerId: string,
  apiKeyId: string,
  extra: Record<string, string> = {},
): string {
  const params = new URLSearchParams();
  if (preset !== "custom") params.set("preset", preset);
  if (providerId) params.set("provider_id", providerId);
  if (apiKeyId) params.set("api_key_id", apiKeyId);
  for (const [k, v] of Object.entries(extra)) {
    if (v) params.set(k, v);
  }
  const qs = params.toString();
  return qs ? `?${qs}` : "";
}

/** Shape returned by `pivotMonthlyByProvider` — a providers × months
 *  matrix with pre-computed totals. `cells` is a nested map keyed
 *  provider → month → row so the renderer can do O(1) lookups. */
interface MonthlyMatrix {
  providers: string[];
  months: string[];
  cells: Map<string, Map<string, MonthlyByProviderRow>>;
  totalsByProvider: Map<string, number>;
  totalsByMonth: Map<string, number>;
  grandTotal: number;
}

// Pivot the flat `MonthlyByProviderRow[]` into a providers × months
// matrix. Top N providers by total cost are kept as their own rows;
// the long tail is merged into a single "Other" row so the table
// stays readable when there are dozens of providers. Only the last
// `maxMonths` (default 6) are shown — older months are dropped so a
// wide `ytd`/all-time window doesn't produce a 24-column table.
// Per-row, per-column, and grand totals are all computed over the
// visible window so the totals row/column agree with the cells.
function pivotMonthlyByProvider(
  rows: MonthlyByProviderRow[],
  topN: number = 10,
  maxMonths: number = 6,
): MonthlyMatrix {
  // Group every row by provider → month.
  const allCells = new Map<string, Map<string, MonthlyByProviderRow>>();
  const monthsSet = new Set<string>();
  for (const r of rows) {
    monthsSet.add(r.month);
    const p = r.provider_id;
    if (!allCells.has(p)) allCells.set(p, new Map());
    allCells.get(p)!.set(r.month, r);
  }

  // Visible months = last `maxMonths` chronologically (oldest →
  // newest). When the data spans fewer months we show all of them.
  const allMonths = [...monthsSet].sort();
  const months = allMonths.slice(-maxMonths);
  const monthSet = new Set(months);

  // Providers active in the visible window, with total cost over
  // that window. Providers whose rows all fall outside the window
  // are dropped (irrelevant to the matrix).
  const totalsByProvider = new Map<string, number>();
  for (const [p, pCells] of allCells) {
    let t = 0;
    let hasVisible = false;
    for (const m of months) {
      const r = pCells.get(m);
      if (r) { hasVisible = true; t += r.total_cost_usd; }
    }
    if (hasVisible) totalsByProvider.set(p, t);
  }

  // Top N by total cost DESC; the rest fold into "Other".
  const sortedProviders = [...totalsByProvider.entries()]
    .sort((a, b) => b[1] - a[1]);
  const topProviders = sortedProviders.slice(0, topN).map(([p]) => p);
  const otherProviders = sortedProviders.slice(topN).map(([p]) => p);

  // Cells restricted to visible months (for both top + Other).
  const cells = new Map<string, Map<string, MonthlyByProviderRow>>();
  for (const p of topProviders) {
    const pCells = allCells.get(p);
    if (!pCells) continue;
    const restricted = new Map<string, MonthlyByProviderRow>();
    for (const [m, r] of pCells) if (monthSet.has(m)) restricted.set(m, r);
    cells.set(p, restricted);
  }

  // Merge "Other" into a single synthetic row over visible months.
  if (otherProviders.length > 0) {
    const otherMonths = new Map<string, MonthlyByProviderRow>();
    let otherTotal = 0;
    for (const p of otherProviders) {
      const pCells = allCells.get(p);
      if (!pCells) continue;
      for (const m of months) {
        const r = pCells.get(m);
        if (!r) continue;
        otherTotal += r.total_cost_usd;
        const existing = otherMonths.get(m);
        if (existing) {
          existing.unique_requests += r.unique_requests;
          existing.total_rows += r.total_rows;
          existing.total_prompt_tokens += r.total_prompt_tokens;
          existing.total_completion_tokens += r.total_completion_tokens;
          existing.total_cost_usd += r.total_cost_usd;
        } else {
          otherMonths.set(m, { ...r, provider_id: "Other" });
        }
      }
    }
    cells.set("Other", otherMonths);
    totalsByProvider.set("Other", otherTotal);
    topProviders.push("Other");
  }

  const totalsByMonth = new Map<string, number>();
  let grandTotal = 0;
  for (const m of months) {
    let mTotal = 0;
    for (const p of topProviders) {
      const r = cells.get(p)?.get(m);
      if (r) mTotal += r.total_cost_usd;
    }
    totalsByMonth.set(m, mTotal);
    grandTotal += mTotal;
  }

  return {
    providers: topProviders,
    months,
    cells,
    totalsByProvider,
    totalsByMonth,
    grandTotal,
  };
}

// ── Status grouping for the bar chart ───────────────────────────────
// The by-status endpoint returns one row per HTTP status code. The
// chart cares about four buckets: 2xx (success), 4xx (client error),
// 5xx (server error), Other (everything else — redirects, 0-status
// connection failures, etc.).
interface StatusBuckets {
  s2xx: number;
  s4xx: number;
  s5xx: number;
  other: number;
}
function groupByStatus(rows: ByStatusRow[]): StatusBuckets {
  let s2 = 0, s4 = 0, s5 = 0, other = 0;
  for (const r of rows) {
    if (r.status_code >= 200 && r.status_code < 300) s2 += r.count;
    else if (r.status_code >= 400 && r.status_code < 500) s4 += r.count;
    else if (r.status_code >= 500 && r.status_code < 600) s5 += r.count;
    else other += r.count;
  }
  return { s2xx: s2, s4xx: s4, s5xx: s5, other };
}

// ── Data preparation for uPlot ───────────────────────────────────────
// Each function converts a slice of the fetched API payload into
// uPlot's `AlignedData` format (`[xs, y1s, ...]`) plus the category
// labels (for bar charts). The chart instances are created with these
// labels baked in (via `buildCategoryBarsChart`'s closure-captured
// `labels` arg), so a label-set change requires a chart recreate —
// which the view's re-mount-on-hashchange already guarantees.

/** Parse a "YYYY-MM-DD" date string into a UTC-midnight timestamp in
 *  seconds (uPlot's X-axis units when `time: true`). */
function dateToSeconds(date: string): number {
  // `Date.parse("YYYY-MM-DDT00:00:00Z")` returns ms since epoch in UTC.
  // Appending the time + "Z" forces UTC interpretation; without it,
  // `Date.parse` is implementation-defined for date-only strings.
  const ms: number = Date.parse(date + "T00:00:00Z");
  if (!Number.isFinite(ms)) return 0;
  return ms / 1000;
}

/** Daily usage chart data: `[xs, reqs, cost]` where `xs` are UTC
 *  midnight timestamps. */
function dailyUsageData(rows: ByDayRow[]): uPlot.AlignedData {
  const xs: number[] = new Array<number>(rows.length);
  const reqs: number[] = new Array<number>(rows.length);
  const cost: number[] = new Array<number>(rows.length);
  for (let i = 0; i < rows.length; i++) {
    const r: ByDayRow = rows[i]!;
    xs[i] = dateToSeconds(r.date);
    reqs[i] = r.unique_requests;
    cost[i] = r.total_cost_usd;
  }
  return [xs, reqs, cost];
}

/** By-model bar chart data: top N models sorted by total cost DESC.
 *  Returns the data array + the labels (truncated model names). */
function byModelBars(rows: ByModelRow[]): { data: uPlot.AlignedData; labels: string[] } {
  const sorted = [...rows].sort((a, b) => b.total_cost_usd - a.total_cost_usd).slice(0, 10);
  const labels: string[] = sorted.map((r: ByModelRow) => truncate(r.upstream_model_id, 14));
  const xs: number[] = sorted.map((_, i: number) => i);
  const ys: number[] = sorted.map((r: ByModelRow) => r.total_cost_usd);
  return { data: [xs, ys], labels };
}

/** By-provider bar chart data: all providers sorted by total cost DESC. */
function byProviderBars(rows: ByProviderRow[]): { data: uPlot.AlignedData; labels: string[] } {
  const sorted = [...rows].sort((a, b) => b.total_cost_usd - a.total_cost_usd);
  const labels: string[] = sorted.map((r: ByProviderRow) => truncate(r.provider_id, 14));
  const xs: number[] = sorted.map((_, i: number) => i);
  const ys: number[] = sorted.map((r: ByProviderRow) => r.total_cost_usd);
  return { data: [xs, ys], labels };
}

/** Status-codes bar chart data: 4 buckets in fixed order
 *  (2xx / 4xx / 5xx / Other). Labels are the bucket names. */
function statusCodesBars(buckets: StatusBuckets): { data: uPlot.AlignedData; labels: string[] } {
  return {
    data: [[0, 1, 2, 3], [buckets.s2xx, buckets.s4xx, buckets.s5xx, buckets.other]],
    labels: ["2xx", "4xx", "5xx", "Other"],
  };
}

/** Latency bar chart data: 6 metrics in fixed order
 *  (p50 conn / p95 conn / p50 ttft / p95 ttft / p50 total / p95 total).
 *  Null values (no samples for that percentile) are coerced to 0 so
 *  the bar renders as a flat baseline rather than a gap. */
function latencyBars(payload: LatencyPayload): { data: uPlot.AlignedData; labels: string[] } {
  const vals: number[] = [
    payload.p50_connect_ms ?? 0,
    payload.p95_connect_ms ?? 0,
    payload.p50_ttft_ms ?? 0,
    payload.p95_ttft_ms ?? 0,
    payload.p50_total_ms ?? 0,
    payload.p95_total_ms ?? 0,
  ];
  return {
    data: [[0, 1, 2, 3, 4, 5], vals],
    labels: [
      t("analytics.latency.p50_connect"),
      t("analytics.latency.p95_connect"),
      t("analytics.latency.p50_ttft"),
      t("analytics.latency.p95_ttft"),
      t("analytics.latency.p50_total"),
      t("analytics.latency.p95_total"),
    ],
  };
}

// ---- View state ----

let loading = true;
let errorMsg: string | null = null;
let summary: UsageSummary | null = null;
let byDay: ByDayRow[] = [];
let byModel: ByModelRow[] = [];
let byProvider: ByProviderRow[] = [];
let byStatus: ByStatusRow[] = [];
let monthlyByProvider: MonthlyByProviderRow[] = [];
let latency: LatencyPayload | null = null;
let races: RaceStatsPayload | null = null;
let errors: ErrorRow[] = [];
let providers: Provider[] = [];
let apiKeys: ApiKeyFilterRow[] = [];

// ---- Chart instances + lifecycle ----
// Created after the first data-bearing lit-html render (so the
// chart-container `<div>`s exist in the DOM), populated via setData
// immediately after creation, and destroyed on view unmount.
interface AnalyticsCharts {
  dailyUsage: uPlot;
  byModel: uPlot;
  byProvider: uPlot;
  statusCodes: uPlot;
  latency: uPlot;
  resizeDisposers: Array<() => void>;
}
let charts: AnalyticsCharts | null = null;

/** Create the 5 uPlot chart instances after the first data-bearing
 *  render. Each chart's category labels are baked in from the fetched
 *  data, so a label-set change (e.g. user changes filters) requires
 *  destroying + recreating the chart — which the view's
 *  re-mount-on-hashchange already guarantees. */
function createAnalyticsCharts(): void {
  if (charts) return; // idempotent

  const dailyEl: HTMLElement | null = document.getElementById("chart-daily-usage");
  const byModelEl: HTMLElement | null = document.getElementById("chart-by-model");
  const byProviderEl: HTMLElement | null = document.getElementById("chart-by-provider");
  const statusEl: HTMLElement | null = document.getElementById("chart-status-codes");
  const latencyEl: HTMLElement | null = document.getElementById("chart-latency");

  if (!dailyEl || !byModelEl || !byProviderEl || !statusEl || !latencyEl) {
    // Containers not in the DOM yet — the loading state hasn't
    // cleared, or the render hasn't committed. The caller should
    // defer via requestAnimationFrame.
    return;
  }

  const resizeDisposers: Array<() => void> = [];

  // Daily usage — time-series line chart (requests + cost on dual axes).
  const dailyUsage: uPlot = buildDailyUsageChart(dailyEl);
  resizeDisposers.push(observeResize(dailyUsage, dailyEl));

  // By-model — categorical bar chart. Labels are truncated model IDs.
  const modelBars = byModelBars(byModel);
  const byModelChart: uPlot = buildCategoryBarsChart(
    byModelEl,
    modelBars.labels,
    CHART_COLORS.purple,
    (_u: uPlot, raw: number): string => "$" + raw.toFixed(4),
  );
  resizeDisposers.push(observeResize(byModelChart, byModelEl));

  // By-provider — categorical bar chart.
  const providerBars = byProviderBars(byProvider);
  const byProviderChart: uPlot = buildCategoryBarsChart(
    byProviderEl,
    providerBars.labels,
    CHART_COLORS.blue,
    (_u: uPlot, raw: number): string => "$" + raw.toFixed(4),
  );
  resizeDisposers.push(observeResize(byProviderChart, byProviderEl));

  // Status codes — categorical bar chart, 4 fixed buckets.
  const statusBars = statusCodesBars(groupByStatus(byStatus));
  const statusCodes: uPlot = buildCategoryBarsChart(
    statusEl,
    statusBars.labels,
    CHART_COLORS.green,
    null,
  );
  resizeDisposers.push(observeResize(statusCodes, statusEl));

  // Latency — categorical bar chart, 6 fixed metrics.
  const latencyChartData = latencyBars(latency ?? {});
  const latencyChart: uPlot = buildCategoryBarsChart(
    latencyEl,
    latencyChartData.labels,
    CHART_COLORS.orange,
    (_u: uPlot, raw: number): string => {
      if (!Number.isFinite(raw)) return "—";
      if (raw >= 1000) return (raw / 1000).toFixed(1) + "s";
      return Math.round(raw) + "ms";
    },
  );
  resizeDisposers.push(observeResize(latencyChart, latencyEl));

  charts = {
    dailyUsage,
    byModel: byModelChart,
    byProvider: byProviderChart,
    statusCodes,
    latency: latencyChart,
    resizeDisposers,
  };

  // Push the current data into the new charts immediately.
  charts.dailyUsage.setData(dailyUsageData(byDay));
  charts.byModel.setData(modelBars.data);
  charts.byProvider.setData(providerBars.data);
  charts.statusCodes.setData(statusBars.data);
  charts.latency.setData(latencyChartData.data);
}

/** Destroy all uPlot instances + disconnect their ResizeObservers.
 *  Called on view unmount. */
function destroyAnalyticsCharts(): void {
  if (!charts) return;
  for (const disposer of charts.resizeDisposers) {
    try { disposer(); } catch (e: unknown) {
      console.warn("[analytics] resize disposer threw:", e);
    }
  }
  try { charts.dailyUsage.destroy(); } catch (e: unknown) { console.warn("[analytics] dailyUsage.destroy threw:", e); }
  try { charts.byModel.destroy(); } catch (e: unknown) { console.warn("[analytics] byModel.destroy threw:", e); }
  try { charts.byProvider.destroy(); } catch (e: unknown) { console.warn("[analytics] byProvider.destroy threw:", e); }
  try { charts.statusCodes.destroy(); } catch (e: unknown) { console.warn("[analytics] statusCodes.destroy threw:", e); }
  try { charts.latency.destroy(); } catch (e: unknown) { console.warn("[analytics] latency.destroy threw:", e); }
  charts = null;
}

// ---- Templates ----

function renderPresetSelector(active: UsagePreset): TemplateResult {
  return html`<div class="preset-selector" role="group" aria-label=${t("analytics.range_label")}>
    ${PRESETS.map((p) => html`<button class="preset-btn${p === active ? " active" : ""}" type="button" @click=${() => setHashParams({ preset: p })}>${t(PRESET_LABEL_KEYS[p])}</button>`)}
  </div>`;
}

function renderAnalyticsFilters(providerId: string, apiKeyId: string): TemplateResult {
  return html`<div class="analytics-filters">
    <select class="filter-dropdown" .value=${providerId} @change=${(e: Event) => setHashParams({ providerId: (e.target as HTMLSelectElement).value })}>
      <option value="">${t("analytics.filter.all_providers")}</option>
      ${providers.map((p) => html`<option value=${p.id}>${p.name}</option>`)}
    </select>
    <select class="filter-dropdown" .value=${apiKeyId} @change=${(e: Event) => setHashParams({ apiKeyId: (e.target as HTMLSelectElement).value })}>
      <option value="">${t("analytics.filter.all_api_keys")}</option>
      ${apiKeys.map((k) => html`<option value=${String(k.id)}>${k.key_prefix || "—"} (${k.label || "—"})</option>`)}
    </select>
    <button class="btn-link" @click=${() => setHashParams({ providerId: "", apiKeyId: "" })}>${t("analytics.filter.clear")}</button>
  </div>`;
}

function card(title: string, body: TemplateResult): TemplateResult {
  return html`<section class="card"><div class="section-header"><h3>${title}</h3></div>${body}</section>`;
}

/** Chart card — same structure as the home dashboard's
 *  `.home-chart-card` (title + subtitle + container div). The
 *  container has a stable ID so lit-html preserves it across
 *  re-renders and the uPlot instance stays attached. */
function chartCard(title: string, subtitle: string, containerId: string): TemplateResult {
  return html`<section class="card analytics-chart-card">
    <div class="card-title">${title}</div>
    <div class="card-subtitle">${subtitle}</div>
    <div class="analytics-chart-container" id="${containerId}"></div>
  </section>`;
}

function renderSummaryBlock(): TemplateResult {
  if (!summary) return html``;
  return card(t("analytics.summary"), html`<div class="metrics">
    <div><label>${t("analytics.summary.unique_requests")}</label><div class="value">${summary.unique_requests}</div></div>
    <div><label>${t("analytics.summary.total_rows")}</label><div class="value">${summary.total_rows}</div></div>
    <div><label>${t("analytics.summary.winners")}</label><div class="value">${summary.winners}</div></div>
    <div><label>${t("analytics.summary.losers")}</label><div class="value">${summary.losers}</div></div>
    <div><label>${t("analytics.summary.errors")}</label><div class="value">${summary.errors}</div></div>
    <div><label>${t("analytics.summary.prompt_tokens")}</label><div class="value">${summary.total_prompt_tokens}</div></div>
    <div><label>${t("analytics.summary.completion_tokens")}</label><div class="value">${summary.total_completion_tokens}</div></div>
    <div><label>${t("analytics.summary.total_cost")}</label><div class="value">$${summary.total_cost_usd.toFixed(4)}</div></div>
    <div><label>${t("analytics.summary.avg_ttft")}</label><div class="value">${summary.avg_ttft_ms ? summary.avg_ttft_ms.toFixed(1) : "—"}</div></div>
  </div>`);
}

/** Race outcomes card — 3 stat blocks (Won / Lost / Total). Matches
 *  the home dashboard's race-outcomes pattern (3 numbers, not a
 *  donut) per the B3 spec. */
function renderRaceBlock(): TemplateResult {
  if (!races) return html``;
  const won: number = races.winners ?? 0;
  const lost: number = races.losers ?? 0;
  const total: number = races.total_races ?? 0;
  const pct: (n: number) => string = (n: number) =>
    total > 0 ? Math.round((n / total) * 100) + "%" : "—";

  return html`<section class="card analytics-chart-card">
    <div class="card-title">${t("analytics.chart.race_outcomes")}</div>
    <div class="card-subtitle">${t("analytics.chart.race_outcomes.subtitle")}</div>
    <div class="analytics-race-stats">
      <div class="analytics-race-stat analytics-race-stat-won">
        <div class="analytics-race-stat-value">${pct(won)}</div>
        <div class="analytics-race-stat-label">${t("analytics.chart.race_outcomes.won")}</div>
        <div class="analytics-race-stat-count">${won}</div>
      </div>
      <div class="analytics-race-stat analytics-race-stat-lost">
        <div class="analytics-race-stat-value">${pct(lost)}</div>
        <div class="analytics-race-stat-label">${t("analytics.chart.race_outcomes.lost")}</div>
        <div class="analytics-race-stat-count">${lost}</div>
      </div>
      <div class="analytics-race-stat analytics-race-stat-total">
        <div class="analytics-race-stat-value">${total}</div>
        <div class="analytics-race-stat-label">${t("analytics.chart.race_outcomes.total")}</div>
        <div class="analytics-race-stat-count">${t("analytics.latency.samples")}: ${races.total_races ?? "—"}</div>
      </div>
    </div>
  </section>`;
}

// Latency is rendered as a uPlot bar chart (6 metrics). The latency
// payload also carries a `samples` count — we surface it as a small
// caption under the chart card title so the operator knows how many
// requests the percentiles were computed over.
function renderLatencyCaption(): TemplateResult {
  if (!latency) return html``;
  const samples: string = latency.samples != null ? String(latency.samples) : "—";
  return html`<div class="card-subtitle">${t("analytics.chart.latency.subtitle")} · ${t("analytics.latency.samples")}: ${samples}</div>`;
}

function renderByModelTable(): TemplateResult {
  const body = byModel.length
    ? html`<table>
        <thead><tr><th>${t("analytics.table.col_provider")}</th><th>${t("analytics.table.col_model")}</th><th>${t("analytics.table.col_unique")}</th><th>${t("analytics.table.col_total")}</th><th>${t("analytics.table.col_cost")}</th></tr></thead>
        <tbody>${byModel.map((r) => html`<tr><td>${r.provider_id}</td><td>${r.upstream_model_id}</td><td>${r.unique_requests}</td><td>${r.total_rows}</td><td>${fmtCost(r.total_cost_usd)}</td></tr>`)}</tbody>
      </table>`
    : html`<p class="empty">${t("analytics.empty.no_usage")}</p>`;
  return card(t("analytics.chart.by_model"), body);
}

function renderByProviderTable(): TemplateResult {
  const body = byProvider.length
    ? html`<table>
        <thead><tr><th>${t("analytics.table.col_provider")}</th><th class="num">${t("analytics.table.col_unique")}</th><th class="num">${t("analytics.table.col_total")}</th><th class="num">${t("analytics.table.col_winners")}</th><th class="num">${t("analytics.table.col_prompt_tok")}</th><th class="num">${t("analytics.table.col_completion_tok")}</th><th class="num">${t("analytics.table.col_cost")}</th></tr></thead>
        <tbody>${byProvider.map((r) => html`<tr>
          <td>${r.provider_id}</td>
          <td class="num">${r.unique_requests}</td>
          <td class="num">${r.total_rows}</td>
          <td class="num">${r.winners}</td>
          <td class="num">${r.total_prompt_tokens}</td>
          <td class="num">${r.total_completion_tokens}</td>
          <td class="num">${fmtCost(r.total_cost_usd)}</td>
        </tr>`)}</tbody>
      </table>`
    : html`<p class="empty">${t("analytics.empty.no_usage")}</p>`;
  return card(t("analytics.chart.by_provider"), body);
}

// Render the providers × months cost matrix. Cells show cost (USD,
// 2dp) with token counts as a `title` tooltip. Empty cells render
// an em-dash. A totals column sits on the right and a totals row at
// the bottom. When the pivot is empty (no usage in the range), the
// card shows an empty-state message instead of an empty table.
function renderMonthlyMatrix(): TemplateResult {
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
  return card(t("analytics.monthly.title"), html`<table class="monthly-matrix">
    <thead>
      <tr><th>${t("analytics.monthly.col_provider")}</th>${pivot.months.map((m) => html`<th>${m}</th>`)}<th class="num">${t("analytics.monthly.col_total")}</th></tr>
    </thead>
    <tbody>${bodyRows}</tbody>
    <tfoot>
      <tr><th>${t("analytics.monthly.col_total")}</th>${footMonths}<th class="num">${fmtCost(pivot.grandTotal)}</th></tr>
    </tfoot>
  </table>`);
}

// ── Recent errors table ─────────────────────────────────────────────
// The errors endpoint returns the latest N rows whose status was 4xx
// or 5xx. The table is rendered as a normal card; rows are clickable
// → `#/logs?request_id=…` so the operator can jump straight to the
// live-logs view filtered to that request. The trace_id is shown in
// a `<code>` snippet under the message so it's copyable even if the
// router doesn't (yet) honour the `?request_id=` query suffix.
function renderRecentErrors(): TemplateResult {
  const slice = (errors || []).slice(0, 10);
  if (slice.length === 0) {
    return card(t("analytics.errors.title"), html`<p class="empty">${t("analytics.empty.no_errors")}</p>`);
  }
  const body = html`<table>
    <thead><tr><th>${t("analytics.errors.col_time")}</th><th>${t("analytics.errors.col_provider")}</th><th>${t("analytics.errors.col_model")}</th><th>${t("analytics.errors.col_status")}</th><th>${t("analytics.errors.col_message")}</th></tr></thead>
    <tbody>${slice.map((e) => {
      const href = `#/logs?request_id=${e.request_id || ""}`;
      return html`<tr class="clickable" @click=${() => { location.hash = href; }}>
        <td>${e.created_at || ""}</td>
        <td>${e.provider_id || ""}</td>
        <td>${e.upstream_model_id || ""}</td>
        <td><span class="status-pill err">${e.status_code || "—"}</span></td>
        <td>${e.error_msg_redacted || t("analytics.errors.no_message")}<br><small class="muted"><code>${e.trace_id || ""}</code></small></td>
      </tr>`;
    })}</tbody>
  </table>`;
  return card(t("analytics.errors.title"), body);
}

function renderBody(): TemplateResult {
  if (loading) return html`<div class="loading">${t("common.loading")}</div>`;
  if (errorMsg) return html`<div class="banner banner-error">${errorMsg}</div>`;
  if (!summary) return html`<div class="loading">${t("common.loading")}</div>`;

  // Null-pricing warning: rows that consumed tokens but recorded
  // $0 cost (pricing was missing at record time). Surfaces
  // under-reporting so the operator knows to run models.dev sync.
  const nullPricingCount = summary.rows_with_null_pricing ?? 0;
  const nullPricingBanner = nullPricingCount > 0
    ? html`<div class="banner banner-warning">${t("analytics.null_pricing_warning", { count: nullPricingCount })}</div>`
    : html``;

  // Charts grid: 2 columns on desktop, 1 on mobile. Daily usage
  // spans the full width (it's a time series that benefits from the
  // extra horizontal room). The 4 categorical bar charts pair up
  // (by-model | by-provider, status | latency). Race outcomes is a
  // full-width row of 3 stat blocks.
  return html`
    ${nullPricingBanner}
    <div class="analytics-charts-grid">
      <div class="analytics-chart-span-2">
        ${chartCard(t("analytics.chart.daily_usage"), t("analytics.chart.daily_usage.subtitle"), "chart-daily-usage")}
      </div>
      ${chartCard(t("analytics.chart.by_model"), t("analytics.chart.by_model.subtitle"), "chart-by-model")}
      ${chartCard(t("analytics.chart.by_provider"), t("analytics.chart.by_provider.subtitle"), "chart-by-provider")}
      ${chartCard(t("analytics.chart.status_codes"), t("analytics.chart.status_codes.subtitle"), "chart-status-codes")}
      <section class="card analytics-chart-card">
        <div class="card-title">${t("analytics.chart.latency")}</div>
        ${renderLatencyCaption()}
        <div class="analytics-chart-container" id="chart-latency"></div>
      </section>
      <div class="analytics-chart-span-2">
        ${renderRaceBlock()}
      </div>
    </div>
    ${renderSummaryBlock()}
    ${renderByModelTable()}
    ${renderByProviderTable()}
    ${renderMonthlyMatrix()}
    ${renderRecentErrors()}`;
}

function renderAnalytics(): TemplateResult {
  const { preset, providerId, apiKeyId } = parseHashParams();
  return html`
    <div class="page-header"><h2>${t("analytics.title")}</h2></div>
    ${renderAnalyticsFilters(providerId, apiKeyId)}
    ${renderPresetSelector(preset)}
    ${renderBody()}`;
}

// ---- Mount ----

export async function mountAnalytics(): Promise<(() => void) | void> {
  const el = document.getElementById("main");
  if (!el) return;

  // Reset view-local state on every mount. The previous mount's
  // charts were destroyed by its cleanup function; we start fresh.
  loading = true;
  errorMsg = null;
  charts = null;
  const cleanupReactive = mountView(el, renderAnalytics);

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

    summary = summaryResp;
    byModel = byModelResp;
    byProvider = byProviderResp;
    monthlyByProvider = monthlyByProviderResp;
    latency = latencyResp;
    races = racesResp;
    byDay = byDayResp;
    byStatus = byStatusResp;
    errors = errorsResp;
    providers = providersResp;
    apiKeys = apiKeysResp;

    if (providers) state.providers = providers;
    if (apiKeys) state.apiKeys = apiKeys;

    loading = false;
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
    errorMsg = e instanceof Error ? e.message : String(e);
    providers = state.providers || [];
    apiKeys = (state.apiKeys || []) as ApiKeyFilterRow[];
    loading = false;
    requestUpdate();
  }
  return () => {
    destroyAnalyticsCharts();
    cleanupReactive();
  };
}
