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
// new query string. No manual re-render is needed.

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { escapeHtml, escapeAttr } from "../lib/escape.js";
import { pageHeader } from "../components/page-header.js";
import { card } from "../components/card.js";
import {
  dailyUsageChart,
  statusDonut,
  type StatusSlice,
} from "../components/charts.js";
import {
  analyticsFilters,
  wireAnalyticsFilters,
} from "../components/filter-bar.js";
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

// Render the time-range selector as a row of buttons. The active
// preset gets `.active` so the CSS can highlight it. Buttons carry
// `data-preset` rather than `data-action` — the view wires them
// directly via addEventListener (see `wirePresetSelector`), so they
// don't need a registry entry.
function renderPresetSelector(active: UsagePreset): string {
  const buttons = PRESETS.map((p) =>
    `<button class="preset-btn${p === active ? " active" : ""}" data-preset="${escapeHtml(p)}" type="button">${escapeHtml(PRESET_LABELS[p])}</button>`,
  ).join("");
  return `<div class="preset-selector" role="group" aria-label="Time range">${buttons}</div>`;
}

function wirePresetSelector(): void {
  const main = document.getElementById("main");
  if (!main) return;
  main.querySelectorAll<HTMLButtonElement>("button.preset-btn[data-preset]").forEach((btn) => {
    btn.addEventListener("click", () => {
      const p = btn.dataset["preset"] as UsagePreset | undefined;
      if (p) setHashParams({ preset: p });
    });
  });
}

// ── Chart-card wrapper ──────────────────────────────────────────────
// `card()` produces `.section-header` markup; the `.card.chart-card`
// CSS variant expects `.card-title` + `.card-body` children instead
// (and zero-padding so an SVG can stretch edge-to-edge). Small local
// helper so the daily chart gets the full-width styling the design
// calls for.
function chartCard(title: string, body: string): string {
  return `<section class="card chart-card">
    <div class="card-title">${escapeHtml(title)}</div>
    <div class="card-body">${body}</div>
  </section>`;
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

// ── Recent errors table ─────────────────────────────────────────────
// The errors endpoint returns the latest N rows whose status was 4xx
// or 5xx. The table is rendered as a normal card; rows are clickable
// → `#/logs?request_id=…` so the operator can jump straight to the
// live-logs view filtered to that request. The trace_id is shown in
// a `<code>` snippet under the message so it's copyable even if the
// router doesn't (yet) honour the `?request_id=` query suffix.
function renderRecentErrors(errors: ErrorRow[]): string {
  if (errors.length === 0) {
    return card("Recent errors", `<p class="empty">No errors in this range.</p>`);
  }
  const rows = errors.map((e) => {
    const time = escapeHtml(e.created_at || "");
    const prov = escapeHtml(e.provider_id || "");
    const model = escapeHtml(e.upstream_model_id || "");
    const status = e.status_code || "—";
    const msg = escapeHtml(e.error_msg_redacted || "(no message)");
    const trace = escapeHtml(e.trace_id || "");
    const href = escapeAttr(`#/logs?request_id=${e.request_id || ""}`);
    return `<tr class="clickable" data-href="${href}">
      <td>${time}</td>
      <td>${prov}</td>
      <td>${model}</td>
      <td><span class="status-pill err">${status}</span></td>
      <td>${msg}<br><small class="muted"><code>${trace}</code></small></td>
    </tr>`;
  }).join("");
  return card("Recent errors", `
    <table>
      <thead><tr><th>Time</th><th>Provider</th><th>Model</th><th>Status</th><th>Error message</th></tr></thead>
      <tbody>${rows}</tbody>
    </table>
  `);
}

// Wire up the `.clickable` rows in the recent-errors table (and any
// other clickable row that carries a `data-href`). Reads the URL
// from `data-href` and assigns `location.hash` so the router picks
// it up — same path the preset buttons take.
function wireClickableRows(): void {
  const main = document.getElementById("main");
  if (!main) return;
  main.querySelectorAll<HTMLElement>(".clickable[data-href]").forEach((el) => {
    el.addEventListener("click", () => {
      const href = el.dataset["href"];
      if (href) location.hash = href;
    });
  });
}

// Render the providers × months cost matrix. Cells show cost (USD,
// 2dp) with token counts as a `title` tooltip. Empty cells render
// an em-dash. A totals column sits on the right and a totals row at
// the bottom. When the pivot is empty (no usage in the range), the
// card shows an empty-state message instead of an empty table.
function renderMonthlyMatrix(pivot: MonthlyMatrix): string {
  if (pivot.providers.length === 0 || pivot.months.length === 0) {
    return card("Monthly usage by provider", `<p class="empty">No usage in this range.</p>`);
  }
  const headMonths = pivot.months.map((m) => `<th>${escapeHtml(m)}</th>`).join("");
  const bodyRows = pivot.providers.map((p) => {
    const pCells = pivot.cells.get(p);
    const tds = pivot.months.map((m) => {
      const r = pCells?.get(m);
      if (!r) return `<td class="num">—</td>`;
      const title = `${r.unique_requests} unique / ${r.total_rows} rows · ${r.total_prompt_tokens} prompt tok · ${r.total_completion_tokens} completion tok`;
      return `<td class="num" title="${escapeAttr(title)}">${fmtCost(r.total_cost_usd)}</td>`;
    }).join("");
    const total = pivot.totalsByProvider.get(p) ?? 0;
    return `<tr><td>${escapeHtml(p)}</td>${tds}<td class="num total">${fmtCost(total)}</td></tr>`;
  }).join("");
  const footMonths = pivot.months.map((m) => {
    const t = pivot.totalsByMonth.get(m) ?? 0;
    return `<th class="num">${fmtCost(t)}</th>`;
  }).join("");
  return card("Monthly usage by provider", `
    <table class="monthly-matrix">
      <thead>
        <tr><th>Provider</th>${headMonths}<th class="num">Total</th></tr>
      </thead>
      <tbody>${bodyRows}</tbody>
      <tfoot>
        <tr><th>Total</th>${footMonths}<th class="num">${fmtCost(pivot.grandTotal)}</th></tr>
      </tfoot>
    </table>
  `);
}

// Paint the page shell (header + filter bar + preset selector + body)
// and wire the preset buttons, the filter dropdowns, and any
// clickable rows. Centralised so the loading, success, and error
// paths all get a working selector + filter bar.
function paint(
  preset: UsagePreset,
  providerId: string,
  apiKeyId: string,
  providers: Provider[],
  apiKeys: ApiKeyFilterRow[],
  body: string,
): void {
  const main = document.getElementById("main");
  if (!main) return;
  const providerOptions = providers.map((p) => ({ value: p.id, label: p.name }));
  const keyOptions = apiKeys.map((k) => ({
    value: String(k.id),
    label: `${k.key_prefix || "—"} (${k.label || "—"})`,
  }));
  main.innerHTML =
    pageHeader({ title: "Analytics" }) +
    analyticsFilters({
      providers: providerOptions,
      apiKeys: keyOptions,
      selectedProvider: providerId,
      selectedKeyId: apiKeyId,
    }) +
    renderPresetSelector(preset) +
    body;
  wirePresetSelector();
  wireAnalyticsFilters(
    main,
    (id: string) => setHashParams({ providerId: id }),
    (id: string) => setHashParams({ apiKeyId: id }),
    () => setHashParams({ providerId: "", apiKeyId: "" }),
  );
  wireClickableRows();
}

export async function mountAnalytics(): Promise<void> {
  const main = document.getElementById("main");
  if (!main) return;
  const { preset, providerId, apiKeyId } = parseHashParams();
  paint(preset, providerId, apiKeyId, [], [], `<div class="loading">Loading...</div>`);
  try {
    // Combined query string for every `/usage/*` fetch. The errors
    // endpoint additionally carries `limit=10` so we cap the table
    // at 10 rows (the server's default is 100).
    const usageQ = buildUsageQuery(preset, providerId, apiKeyId);
    const errorsQ = buildUsageQuery(preset, providerId, apiKeyId, { limit: "10" });
    const [
      summary, byModel, byProvider, monthlyByProvider, latency, races,
      byDay, byStatus, errors, providers, apiKeys,
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

    if (providers) state.providers = providers;
    if (apiKeys) state.apiKeys = apiKeys;
    const apiKeyRows: ApiKeyFilterRow[] = (apiKeys || []) as ApiKeyFilterRow[];

    // Null-pricing warning: rows that consumed tokens but recorded
    // $0 cost (pricing was missing at record time). Surfaces
    // under-reporting so the operator knows to run models.dev sync.
    const nullPricingCount = summary.rows_with_null_pricing ?? 0;
    const nullPricingBanner = nullPricingCount > 0
      ? `<div class="banner banner-warning">⚠ ${nullPricingCount} rows had no pricing data (cost = $0). Run models.dev sync or manually set pricing to fix cost reporting.</div>`
      : "";

    // Daily usage chart — full-width, edge-to-edge via .chart-card.
    const dailyChartBlock = chartCard("Daily usage", dailyUsageChart(byDay));

    const summaryBlock = card("Summary", `
      <div class="metrics">
        <div><label>Unique requests</label><div class="value">${summary.unique_requests}</div></div>
        <div><label>Total rows</label><div class="value">${summary.total_rows}</div></div>
        <div><label>Winners</label><div class="value">${summary.winners}</div></div>
        <div><label>Losers</label><div class="value">${summary.losers}</div></div>
        <div><label>Errors</label><div class="value">${summary.errors}</div></div>
        <div><label>Prompt tokens</label><div class="value">${summary.total_prompt_tokens}</div></div>
        <div><label>Completion tokens</label><div class="value">${summary.total_completion_tokens}</div></div>
        <div><label>Total cost USD</label><div class="value">$${summary.total_cost_usd.toFixed(4)}</div></div>
        <div><label>Avg TTFT ms</label><div class="value">${summary.avg_ttft_ms ? summary.avg_ttft_ms.toFixed(1) : "—"}</div></div>
      </div>
    `);
    // Status distribution donut — half-width card alongside the
    // summary. Wrapped in `.home-row` so the two cards share a
    // responsive 2-column grid (collapses to 1 column on narrow
    // viewports).
    const donutBlock = card("Status distribution", statusDonut(groupByStatus(byStatus)));
    const summaryDonutRow = `<div class="home-row">${summaryBlock}${donutBlock}</div>`;

    const latencyBlock = card("Latency percentiles (winners only)", `
      <div class="metrics">
        <div><label>Samples</label><div class="value">${latency.samples ?? "—"}</div></div>
        <div><label>p50 connect ms</label><div class="value">${fmtMs(latency.p50_connect_ms)}</div></div>
        <div><label>p95 connect ms</label><div class="value">${fmtMs(latency.p95_connect_ms)}</div></div>
        <div><label>p50 TTFT ms</label><div class="value">${fmtMs(latency.p50_ttft_ms)}</div></div>
        <div><label>p95 TTFT ms</label><div class="value">${fmtMs(latency.p95_ttft_ms)}</div></div>
        <div><label>p50 total ms</label><div class="value">${fmtMs(latency.p50_total_ms)}</div></div>
        <div><label>p95 total ms</label><div class="value">${fmtMs(latency.p95_total_ms)}</div></div>
      </div>
    `);
    const raceBlock = card("Race stats", `
      <div class="metrics">
        <div><label>Total races</label><div class="value">${races.total_races ?? "—"}</div></div>
        <div><label>Winners</label><div class="value">${races.winners ?? "—"}</div></div>
        <div><label>Losers</label><div class="value">${races.losers ?? "—"}</div></div>
      </div>
    `);
    const byModelRows = byModel.map((r) =>
      `<tr><td>${escapeHtml(r.provider_id)}</td><td>${escapeHtml(r.upstream_model_id)}</td><td>${r.unique_requests}</td><td>${r.total_rows}</td><td>${fmtCost(r.total_cost_usd)}</td></tr>`,
    ).join("");
    const byModelBlock = card("By model", `
      ${byModel.length ? `<table>
        <thead><tr><th>Provider</th><th>Model</th><th>Unique</th><th>Total</th><th>Cost USD</th></tr></thead>
        <tbody>${byModelRows}</tbody>
      </table>` : `<p class="empty">No usage in this range.</p>`}
    `);
    const byProviderRows = byProvider.map((r) =>
      `<tr>
        <td>${escapeHtml(r.provider_id)}</td>
        <td class="num">${r.unique_requests}</td>
        <td class="num">${r.total_rows}</td>
        <td class="num">${r.winners}</td>
        <td class="num">${r.total_prompt_tokens}</td>
        <td class="num">${r.total_completion_tokens}</td>
        <td class="num">${fmtCost(r.total_cost_usd)}</td>
      </tr>`,
    ).join("");
    const byProviderBlock = card("Usage by provider", `
      ${byProvider.length ? `<table>
        <thead><tr><th>Provider</th><th class="num">Unique</th><th class="num">Total</th><th class="num">Winners</th><th class="num">Prompt tok</th><th class="num">Completion tok</th><th class="num">Cost USD</th></tr></thead>
        <tbody>${byProviderRows}</tbody>
      </table>` : `<p class="empty">No usage in this range.</p>`}
    `);
    const monthlyBlock = renderMonthlyMatrix(pivotMonthlyByProvider(monthlyByProvider));

    // Recent errors — bottom of the page, capped at 10 rows
    // client-side (the server's default limit is 100, our query
    // sends `limit=10` for forward-compat).
    const errorsBlock = renderRecentErrors((errors || []).slice(0, 10));

    paint(
      preset, providerId, apiKeyId, providers, apiKeyRows,
      nullPricingBanner + dailyChartBlock + summaryDonutRow +
        latencyBlock + raceBlock +
        byModelBlock + byProviderBlock + monthlyBlock +
        errorsBlock,
    );
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    paint(
      preset, providerId, apiKeyId,
      state.providers || [], (state.apiKeys || []) as ApiKeyFilterRow[],
      `<div class="banner banner-error">${escapeHtml(msg)}</div>`,
    );
  }
}
