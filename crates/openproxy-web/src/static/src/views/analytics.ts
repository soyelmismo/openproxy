// views/analytics.ts — analytics dashboards: filter bar, time-range
// preset selector, summary, latency percentiles, race stats, daily
// usage chart, status distribution donut, by-model breakdown,
// by-provider totals, providers × months cost matrix, and a recent
// errors table. Every `/usage/*` endpoint is queried with a combined
// `?preset=X&provider_id=Y&api_key_id=Z` query built from the URL
// hash so the dashboard reflects the selected window + scope. The
// hash looks like `#/analytics?range=this_month&provider_id=openrouter&api_key_id=12`;
// the router regex tolerates the trailing `?...` suffix and the
// `hashchange` event re-mounts the view so every fetch picks up the
// new query string. After each fetch resolves we call
// `requestUpdate()` so lit-html re-renders with the fresh data.
//
// MIGRATED to lit-html for atomic DOM updates — see views/combos.ts
// for the reference pattern.

import { html, type TemplateResult } from "lit-html";
import { unsafeHTML } from "lit-html/directives/unsafe-html.js";
import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { mountView, requestUpdate } from "../state/reactive.js";
import {
  dailyUsageChart,
  statusDonut,
  type StatusSlice,
} from "../components/charts.js";
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

// Friendly labels for the preset buttons. `custom` is labelled
// "All time" because the dashboard has no custom date-picker —
// selecting it sends no `?preset=`, so the server returns the full
// history.
const PRESET_LABELS: Record<UsagePreset, string> = {
  today: "Today",
  "7d": "7 days",
  "30d": "30 days",
  this_month: "This month",
  last_month: "Last month",
  last_6_months: "6 months",
  ytd: "YTD",
  custom: "All time",
};

function fmtMs(v: number | null | undefined): string {
  return v == null ? "—" : `${v.toFixed(0)} ms`;
}

function fmtCost(v: number): string {
  return `$${v.toFixed(2)}`;
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

// ── Status grouping for the donut ───────────────────────────────────
// The by-status endpoint returns one row per HTTP status code. The
// donut cares about four buckets: 2xx (success), 4xx (client error),
// 5xx (server error), Other (everything else — redirects, 0-status
// connection failures, etc.). Colors come from the theme tokens so
// the donut adapts to light/dark.
function groupByStatus(rows: ByStatusRow[]): StatusSlice[] {
  let s2 = 0, s4 = 0, s5 = 0, other = 0;
  for (const r of rows) {
    if (r.status_code >= 200 && r.status_code < 300) s2 += r.count;
    else if (r.status_code >= 400 && r.status_code < 500) s4 += r.count;
    else if (r.status_code >= 500 && r.status_code < 600) s5 += r.count;
    else other += r.count;
  }
  return [
    { label: "2xx", count: s2, color: "var(--color-success)" },
    { label: "4xx", count: s4, color: "var(--color-warn)" },
    { label: "5xx", count: s5, color: "var(--color-error)" },
    { label: "Other", count: other, color: "var(--color-text-muted)" },
  ];
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

// ---- Templates ----

function renderPresetSelector(active: UsagePreset): TemplateResult {
  return html`<div class="preset-selector" role="group" aria-label="Time range">
    ${PRESETS.map((p) => html`<button class="preset-btn${p === active ? " active" : ""}" type="button" @click=${() => setHashParams({ preset: p })}>${PRESET_LABELS[p]}</button>`)}
  </div>`;
}

function renderAnalyticsFilters(providerId: string, apiKeyId: string): TemplateResult {
  return html`<div class="analytics-filters">
    <select class="filter-dropdown" @change=${(e: Event) => setHashParams({ providerId: (e.target as HTMLSelectElement).value })}>
      <option value="" ?selected=${providerId === ""}>All providers</option>
      ${providers.map((p) => html`<option value=${p.id} ?selected=${p.id === providerId}>${p.name}</option>`)}
    </select>
    <select class="filter-dropdown" @change=${(e: Event) => setHashParams({ apiKeyId: (e.target as HTMLSelectElement).value })}>
      <option value="" ?selected=${apiKeyId === ""}>All API keys</option>
      ${apiKeys.map((k) => html`<option value=${String(k.id)} ?selected=${String(k.id) === apiKeyId}>${k.key_prefix || "—"} (${k.label || "—"})</option>`)}
    </select>
    <button class="btn-link" @click=${() => setHashParams({ providerId: "", apiKeyId: "" })}>Clear filters</button>
  </div>`;
}

function card(title: string, body: TemplateResult): TemplateResult {
  return html`<section class="card"><div class="section-header"><h3>${title}</h3></div>${body}</section>`;
}

function chartCard(title: string, bodyHtml: string): TemplateResult {
  return html`<section class="card chart-card">
    <div class="card-title">${title}</div>
    <div class="card-body">${unsafeHTML(bodyHtml)}</div>
  </section>`;
}

function renderSummaryBlock(): TemplateResult {
  if (!summary) return html``;
  return card("Summary", html`<div class="metrics">
    <div><label>Unique requests</label><div class="value">${summary.unique_requests}</div></div>
    <div><label>Total rows</label><div class="value">${summary.total_rows}</div></div>
    <div><label>Winners</label><div class="value">${summary.winners}</div></div>
    <div><label>Losers</label><div class="value">${summary.losers}</div></div>
    <div><label>Errors</label><div class="value">${summary.errors}</div></div>
    <div><label>Prompt tokens</label><div class="value">${summary.total_prompt_tokens}</div></div>
    <div><label>Completion tokens</label><div class="value">${summary.total_completion_tokens}</div></div>
    <div><label>Total cost USD</label><div class="value">$${summary.total_cost_usd.toFixed(4)}</div></div>
    <div><label>Avg TTFT ms</label><div class="value">${summary.avg_ttft_ms ? summary.avg_ttft_ms.toFixed(1) : "—"}</div></div>
  </div>`);
}

function renderLatencyBlock(): TemplateResult {
  if (!latency) return html``;
  return card("Latency percentiles (winners only)", html`<div class="metrics">
    <div><label>Samples</label><div class="value">${latency.samples ?? "—"}</div></div>
    <div><label>p50 connect ms</label><div class="value">${fmtMs(latency.p50_connect_ms)}</div></div>
    <div><label>p95 connect ms</label><div class="value">${fmtMs(latency.p95_connect_ms)}</div></div>
    <div><label>p50 TTFT ms</label><div class="value">${fmtMs(latency.p50_ttft_ms)}</div></div>
    <div><label>p95 TTFT ms</label><div class="value">${fmtMs(latency.p95_ttft_ms)}</div></div>
    <div><label>p50 total ms</label><div class="value">${fmtMs(latency.p50_total_ms)}</div></div>
    <div><label>p95 total ms</label><div class="value">${fmtMs(latency.p95_total_ms)}</div></div>
  </div>`);
}

function renderRaceBlock(): TemplateResult {
  if (!races) return html``;
  return card("Race stats", html`<div class="metrics">
    <div><label>Total races</label><div class="value">${races.total_races ?? "—"}</div></div>
    <div><label>Winners</label><div class="value">${races.winners ?? "—"}</div></div>
    <div><label>Losers</label><div class="value">${races.losers ?? "—"}</div></div>
  </div>`);
}

function renderByModelBlock(): TemplateResult {
  const body = byModel.length
    ? html`<table>
        <thead><tr><th>Provider</th><th>Model</th><th>Unique</th><th>Total</th><th>Cost USD</th></tr></thead>
        <tbody>${byModel.map((r) => html`<tr><td>${r.provider_id}</td><td>${r.upstream_model_id}</td><td>${r.unique_requests}</td><td>${r.total_rows}</td><td>${fmtCost(r.total_cost_usd)}</td></tr>`)}</tbody>
      </table>`
    : html`<p class="empty">No usage in this range.</p>`;
  return card("By model", body);
}

function renderByProviderBlock(): TemplateResult {
  const body = byProvider.length
    ? html`<table>
        <thead><tr><th>Provider</th><th class="num">Unique</th><th class="num">Total</th><th class="num">Winners</th><th class="num">Prompt tok</th><th class="num">Completion tok</th><th class="num">Cost USD</th></tr></thead>
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
    : html`<p class="empty">No usage in this range.</p>`;
  return card("Usage by provider", body);
}

// Render the providers × months cost matrix. Cells show cost (USD,
// 2dp) with token counts as a `title` tooltip. Empty cells render
// an em-dash. A totals column sits on the right and a totals row at
// the bottom. When the pivot is empty (no usage in the range), the
// card shows an empty-state message instead of an empty table.
function renderMonthlyMatrix(): TemplateResult {
  const pivot = pivotMonthlyByProvider(monthlyByProvider);
  if (pivot.providers.length === 0 || pivot.months.length === 0) {
    return card("Monthly usage by provider", html`<p class="empty">No usage in this range.</p>`);
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
    const t = pivot.totalsByMonth.get(m) ?? 0;
    return html`<th class="num">${fmtCost(t)}</th>`;
  });
  return card("Monthly usage by provider", html`<table class="monthly-matrix">
    <thead>
      <tr><th>Provider</th>${pivot.months.map((m) => html`<th>${m}</th>`)}<th class="num">Total</th></tr>
    </thead>
    <tbody>${bodyRows}</tbody>
    <tfoot>
      <tr><th>Total</th>${footMonths}<th class="num">${fmtCost(pivot.grandTotal)}</th></tr>
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
    return card("Recent errors", html`<p class="empty">No errors in this range.</p>`);
  }
  const body = html`<table>
    <thead><tr><th>Time</th><th>Provider</th><th>Model</th><th>Status</th><th>Error message</th></tr></thead>
    <tbody>${slice.map((e) => {
      const href = `#/logs?request_id=${e.request_id || ""}`;
      return html`<tr class="clickable" @click=${() => { location.hash = href; }}>
        <td>${e.created_at || ""}</td>
        <td>${e.provider_id || ""}</td>
        <td>${e.upstream_model_id || ""}</td>
        <td><span class="status-pill err">${e.status_code || "—"}</span></td>
        <td>${e.error_msg_redacted || "(no message)"}<br><small class="muted"><code>${e.trace_id || ""}</code></small></td>
      </tr>`;
    })}</tbody>
  </table>`;
  return card("Recent errors", body);
}

function renderBody(): TemplateResult {
  if (loading) return html`<div class="loading">Loading...</div>`;
  if (errorMsg) return html`<div class="banner banner-error">${errorMsg}</div>`;
  if (!summary) return html`<div class="loading">Loading...</div>`;

  // Null-pricing warning: rows that consumed tokens but recorded
  // $0 cost (pricing was missing at record time). Surfaces
  // under-reporting so the operator knows to run models.dev sync.
  const nullPricingCount = summary.rows_with_null_pricing ?? 0;
  const nullPricingBanner = nullPricingCount > 0
    ? html`<div class="banner banner-warning">⚠ ${nullPricingCount} rows had no pricing data (cost = $0). Run models.dev sync or manually set pricing to fix cost reporting.</div>`
    : html``;

  const dailyChartBlock = chartCard("Daily usage", dailyUsageChart(byDay));
  const donutBlock = card("Status distribution", html`${unsafeHTML(statusDonut(groupByStatus(byStatus)))}`);
  const summaryDonutRow = html`<div class="home-row">${renderSummaryBlock()}${donutBlock}</div>`;

  return html`
    ${nullPricingBanner}
    ${dailyChartBlock}
    ${summaryDonutRow}
    ${renderLatencyBlock()}
    ${renderRaceBlock()}
    ${renderByModelBlock()}
    ${renderByProviderBlock()}
    ${renderMonthlyMatrix()}
    ${renderRecentErrors()}`;
}

function renderAnalytics(): TemplateResult {
  const { preset, providerId, apiKeyId } = parseHashParams();
  return html`
    <div class="page-header"><h2>Analytics</h2></div>
    ${renderAnalyticsFilters(providerId, apiKeyId)}
    ${renderPresetSelector(preset)}
    ${renderBody()}`;
}

// ---- Mount ----

export async function mountAnalytics(): Promise<(() => void) | void> {
  const el = document.getElementById("main");
  if (!el) return;

  loading = true;
  errorMsg = null;
  const cleanup = mountView(el, renderAnalytics);

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
  } catch (e: unknown) {
    errorMsg = e instanceof Error ? e.message : String(e);
    providers = state.providers || [];
    apiKeys = (state.apiKeys || []) as ApiKeyFilterRow[];
    loading = false;
    requestUpdate();
  }
  return cleanup;
}
