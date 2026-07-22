// views/key-usage.ts — per-key usage recap. Reuses the
// /keys/:id/usage + /usage/by-day?api_key_id=... endpoints.
//
// MIGRATED to lit-html for atomic DOM updates. The view fetches the
// headline metrics on mount, holds them in module-local state, and
// asks `requestUpdate()` to re-render via lit-html's diffing. No
// `innerHTML` is assigned directly.
//
// The view shows a KPI summary row (requests / cost / error rate /
// avg latency / last used), a daily-usage bar+line chart reusing the
// analytics dailyUsageChart, and a detailed metrics table. The
// chart's data comes from `/usage/by-day?api_key_id=${id}` so the
// operator sees the full history of the key at a glance.

import { html, type TemplateResult } from 'lit-html';
import { unsafeHTML } from 'lit-html/directives/unsafe-html.js';
import { api } from "../state/api.js";
import { createView } from "../lib/view-utils.js";
import { dailyUsageChart } from "../components/charts.js";
import type { ByDayRow, UsageSummary } from "../lib/types/api.js";

// Shape of the payload from /keys/:id/usage. The server hands back
// a small headline object (the per-key `UsageSummary` from
// `core_api_keys::usage_summary`) plus the full `usage::summary`
// roll-up so the dashboard has access to avg_ttft_ms / avg_total_ms
// for the latency KPI. We keep both sub-objects optional so a
// partially populated response doesn't crash the render.
interface KeyUsageHead {
  key?: {
    total_rows?: number;
    unique_requests?: number;
    errors?: number;
    total_cost_usd?: number;
    last_used_at?: string | null;
  } | null;
  summary?: UsageSummary | null;
}

// ---- Module-local state ----
// Captured by the render closure. `loadError` is set when the
// initial fetch fails so the template can swap the loading view
// for an inline banner (matches the previous innerHTML behaviour).
// `byDay` carries the daily-usage rows for the chart; it stays
// `null` until its fetch resolves so the template can show a
// loading placeholder for the chart independently of the headline
// metrics.
let keyId: number = 0;
let head: KeyUsageHead | null = null;
let byDay: ByDayRow[] | null = null;
let loadError: string | null = null;

// Format helpers — keep the table compact and align with the
// formatting used by the analytics dashboard (cost at 4dp, latency
// in ms with a `—` fallback for nulls, error rate as a percentage
// with 1dp).
function fmtCost(v: number): string {
  return `$${v.toFixed(4)}`;
}

function fmtMs(v: number | null | undefined): string {
  if (v == null) return "—";
  return `${v.toFixed(0)} ms`;
}

function fmtPct(num: number, denom: number): string {
  if (!denom) return "0.0%";
  return `${((num / denom) * 100).toFixed(1)}%`;
}

// Render a single KPI tile. The `valueClass` arg lets us colour the
// error rate red when it's high, mirroring the home dashboard's
// KPI trend styling.
function renderKpiTile(label: string, value: string, valueClass = ""): TemplateResult {
  return html`<div class="kpi-tile">
    <div class="kpi-label">${label}</div>
    <div class="kpi-value ${valueClass}">${value}</div>
  </div>`;
}

// Render the daily-usage chart block. The chart is only rendered
// when `byDay` is non-null (i.e. the fetch has resolved, even if
// empty). While the fetch is in-flight we show a `Loading...`
// placeholder; if the response is an empty array, `dailyUsageChart`
// renders its own `No data for the selected range.` message.
function renderChartBlock(): TemplateResult {
  if (byDay === null) {
    return html`<section class="card chart-card">
      <div class="card-title">Daily usage</div>
      <div class="card-body"><div class="loading">Loading...</div></div>
    </section>`;
  }
  return html`<section class="card chart-card">
    <div class="card-title">Daily usage</div>
    <div class="card-body">${unsafeHTML(dailyUsageChart(byDay))}</div>
  </section>`;
}

function renderKeyUsage(): TemplateResult {
  if (loadError) {
    return html`
      <div class="page-header"><a href="#/keys" class="back-link">← All keys</a><h2>API key #${keyId}</h2></div>
      <div class="banner banner-error">${loadError}</div>
    `;
  }
  if (!head) {
    return html`
      <div class="page-header"><a href="#/keys" class="back-link">← All keys</a><h2>API key #${keyId}</h2></div>
      <div class="loading">Loading...</div>
    `;
  }

  // The `key` sub-object carries the per-key headline numbers
  // (cheaper query — single row from the api_keys usage_summary).
  // The `summary` sub-object carries the full usage::summary roll-up
  // (includes avg_ttft_ms / avg_total_ms / winners / losers / token
  // counts). Both are optional; we fall back to 0 so the KPI tiles
  // render "0" rather than crashing on `.toFixed()` of `undefined`.
  const k = head.key ?? {};
  const s: Partial<UsageSummary> = head.summary ?? {};
  const unique: number = k.unique_requests ?? s.unique_requests ?? 0;
  const total: number = k.total_rows ?? s.total_rows ?? 0;
  const errors: number = k.errors ?? s.errors ?? 0;
  const cost: number = k.total_cost_usd ?? s.total_cost_usd ?? 0;
  const avgLatency: number | null = s.avg_total_ms ?? null;
  const avgTtft: number | null = s.avg_ttft_ms ?? null;
  const last: string = k.last_used_at ?? "never";
  const promptTok: number = s.total_prompt_tokens ?? 0;
  const completionTok: number = s.total_completion_tokens ?? 0;
  const winners: number = s.winners ?? 0;
  const losers: number = s.losers ?? 0;

  // Error rate — used both for the KPI tile and for the table row.
  const errorRatePct = total > 0 ? (errors / total) * 100 : 0;

  // KPI row: requests / cost / error rate / avg latency. The error
  // rate tile turns red when it exceeds 5%, matching the home
  // dashboard's threshold.
  const kpiRow = html`<div class="home-kpi-row">
    ${renderKpiTile("Total requests", String(unique))}
    ${renderKpiTile("Total cost", fmtCost(cost))}
    ${renderKpiTile("Error rate", `${errorRatePct.toFixed(1)}%`, errorRatePct > 5 ? "kpi-trend-down" : "")}
    ${renderKpiTile("Avg latency", fmtMs(avgLatency))}
  </div>`;

  return html`
    <div class="page-header"><a href="#/keys" class="back-link">← All keys</a><h2>API key #${keyId} usage</h2></div>
    ${kpiRow}
    ${renderChartBlock()}
    <section class="detail-section">
      <div class="section-header"><h3>Headline metrics</h3></div>
      <table>
        <tbody>
          <tr><th>Total rows</th><td>${total}</td></tr>
          <tr><th>Unique requests</th><td>${unique}</td></tr>
          <tr><th>Winners</th><td>${winners}</td></tr>
          <tr><th>Losers</th><td>${losers}</td></tr>
          <tr><th>Errors (4xx/5xx)</th><td>${errors} (${fmtPct(errors, total)})</td></tr>
          <tr><th>Total cost (USD)</th><td>${fmtCost(cost)}</td></tr>
          <tr><th>Prompt tokens</th><td>${promptTok}</td></tr>
          <tr><th>Completion tokens</th><td>${completionTok}</td></tr>
          <tr><th>Avg TTFT</th><td>${fmtMs(avgTtft)}</td></tr>
          <tr><th>Avg total latency</th><td>${fmtMs(avgLatency)}</td></tr>
          <tr><th>Last used</th><td>${last}</td></tr>
        </tbody>
      </table>
    </section>
    <p class="empty"><small>Filter the global Analytics page with <code>?api_key_id=${keyId}</code> for per-(provider, model) breakdown.</small></p>
  `;
}

export async function mountKeyUsage(id: number): Promise<(() => void) | void> {
  keyId = id;
  head = null;
  byDay = null;
  loadError = null;
  return createView(
    renderKeyUsage,
    async () => {
      const [headResp, byDayResp] = await Promise.all([
        api(`/keys/${id}/usage`) as Promise<KeyUsageHead>,
        api(`/usage/by-day?api_key_id=${id}`).catch((): ByDayRow[] => []) as Promise<ByDayRow[]>,
      ]);
      head = headResp;
      byDay = byDayResp;
    },
    (msg) => { loadError = msg; },
  );
}
