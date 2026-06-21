// views/analytics.ts — analytics dashboards: summary, latency
// percentiles, race stats, by-model breakdown, by-provider totals,
// and a providers × months cost matrix. All six endpoints are
// queried with a `?preset=` time-range so the dashboard reflects
// the selected window (default: `this_month`). The preset lives in
// the URL hash (`#/analytics?range=this_month`) so it survives a
// refresh; the router regex was widened to tolerate the trailing
// `?...` suffix.

import { api } from "../state/api.js";
import { escapeHtml, escapeAttr } from "../lib/escape.js";
import { pageHeader } from "../components/page-header.js";
import { card } from "../components/card.js";
import type {
  ByModelRow,
  ByProviderRow,
  MonthlyByProviderRow,
  UsagePreset,
  UsageSummary,
} from "../lib/types/api.js";

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

// Read the active preset from the URL hash. Stored as a `?range=`
// query suffix on the route (`#/analytics?range=this_month`) so the
// router still recognises the route. Falls back to `this_month`
// when the param is missing or holds an unknown value — that way a
// hand-typed typo doesn't 400 the whole view.
function presetFromHash(): UsagePreset {
  const m = location.hash.match(/[?&]range=([^&]+)/);
  if (m) {
    const v = decodeURIComponent(m[1] ?? "");
    if ((PRESETS as readonly string[]).includes(v)) {
      return v as UsagePreset;
    }
  }
  return "this_month";
}

// Swap the `range` param in the hash, preserving the route path and
// any other query params. Setting `location.hash` fires `hashchange`,
// which re-mounts the view via the router — that re-fetches every
// endpoint with the new preset. No manual re-render needed.
function setPresetInHash(p: UsagePreset): void {
  const hash = location.hash || "#/analytics";
  const qIdx = hash.indexOf("?");
  const path = qIdx >= 0 ? hash.slice(0, qIdx) : hash;
  const query = qIdx >= 0 ? hash.slice(qIdx + 1) : "";
  const params = new URLSearchParams(query);
  params.set("range", p);
  location.hash = `${path}?${params.toString()}`;
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
      if (p) setPresetInHash(p);
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

// Paint the page shell (header + preset selector + body) and wire
// the preset buttons. Centralised so the loading, success, and
// error paths all get a working selector.
function paint(preset: UsagePreset, body: string): void {
  const main = document.getElementById("main");
  if (!main) return;
  main.innerHTML = pageHeader({ title: "Analytics" }) +
    renderPresetSelector(preset) + body;
  wirePresetSelector();
}

export async function mountAnalytics(): Promise<void> {
  const main = document.getElementById("main");
  if (!main) return;
  const preset = presetFromHash();
  paint(preset, `<div class="loading">Loading...</div>`);
  try {
    // `custom` = no preset param → server returns the full history
    // (or the explicit from/to if we ever wire a date picker). Every
    // other preset maps to a (from, to) window server-side.
    const q = preset !== "custom" ? `?preset=${encodeURIComponent(preset)}` : "";
    const [summary, byModel, byProvider, monthlyByProvider, latency, races] = await Promise.all([
      api(`/usage/summary${q}`) as Promise<UsageSummary>,
      api(`/usage/by-model${q}`) as Promise<ByModelRow[]>,
      api(`/usage/by-provider${q}`) as Promise<ByProviderRow[]>,
      api(`/usage/monthly-by-provider${q}`) as Promise<MonthlyByProviderRow[]>,
      api(`/usage/latency${q}`) as Promise<LatencyPayload>,
      api(`/usage/races${q}`) as Promise<RaceStatsPayload>,
    ]);

    // Null-pricing warning: rows that consumed tokens but recorded
    // $0 cost (pricing was missing at record time). Surfaces
    // under-reporting so the operator knows to run models.dev sync.
    const nullPricingCount = summary.rows_with_null_pricing ?? 0;
    const nullPricingBanner = nullPricingCount > 0
      ? `<div class="banner banner-warning">⚠ ${nullPricingCount} rows had no pricing data (cost = $0). Run models.dev sync or manually set pricing to fix cost reporting.</div>`
      : "";

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

    paint(
      preset,
      nullPricingBanner + summaryBlock + latencyBlock + raceBlock +
        byModelBlock + byProviderBlock + monthlyBlock,
    );
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    paint(preset, `<div class="banner banner-error">${escapeHtml(msg)}</div>`);
  }
}
