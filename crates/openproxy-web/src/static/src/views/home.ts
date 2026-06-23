// views/home.ts — operator-focused landing dashboard. Replaces the
// old inventory-grid layout (5 full-width count cards) with a
// four-tile KPI row (requests / cost / error-rate / avg-TTFT for
// "today"), a two-column row (health + recent usage), a 7-day daily
// usage chart, a quick-actions bar, and a small footer line with the
// inventory counts.
//
// MIGRATED to lit-html for atomic DOM updates. lit-html diffs the
// template against the previous render and only patches the DOM
// nodes that actually changed — inputs keep focus, selects stay
// open, scroll is preserved. See views/combos.ts for the reference
// pattern.

import { html, type TemplateResult } from "lit-html";
import { unsafeHTML } from "lit-html/directives/unsafe-html.js";
import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { mountView, requestUpdate } from "../state/reactive.js";
import { statusPillClass } from "../lib/constants.js";
import { dailyUsageChart } from "../components/charts.js";
import type {
  Account,
  ByDayRow,
  Combo,
  Model,
  Provider,
  RecentUsageRow,
  UsageSummary,
} from "../lib/types/api.js";

// Zero-valued summary used when the `/usage/summary` fetch fails
// (e.g. the usage table is empty/missing on a fresh install). All
// numeric fields are 0 so the KPI tiles render "0" rather than
// crashing on `.toFixed()` of `undefined`. `avg_ttft_ms` stays null
// so the Avg-TTFT tile shows an em-dash.
const NULL_SUMMARY: UsageSummary = {
  unique_requests: 0,
  total_rows: 0,
  total_attempts: 0,
  winners: 0,
  losers: 0,
  errors: 0,
  total_prompt_tokens: 0,
  total_completion_tokens: 0,
  total_cost_usd: 0,
  avg_ttft_ms: null,
  avg_total_ms: 0,
  rows_with_null_pricing: 0,
};

// ---- View state ----

let loading = true;
let summary: UsageSummary = NULL_SUMMARY;
let recent: RecentUsageRow[] = [];
let byDay: ByDayRow[] = [];
let providers: Provider[] = [];
let accounts: Account[] = [];
let models: Model[] = [];
let combos: Combo[] = [];
let keys: unknown[] = [];

// ---- Templates ----

function renderKpiTile(label: string, value: string, valueClass = ""): TemplateResult {
  return html`<div class="kpi-tile">
    <div class="kpi-label">${label}</div>
    <div class="kpi-value ${valueClass}">${value}</div>
  </div>`;
}

function renderHealthCard(): TemplateResult {
  return html`<section class="card">
    <div class="section-header"><h3>Health</h3></div>
    <p>Status: <strong>${state.health ? state.health.status : "—"}</strong></p>
    ${state.health && state.health.message
      ? html`<p class="muted">${state.health.message}</p>`
      : html``}
  </section>`;
}

function renderRecentCard(): TemplateResult {
  // Recent usage rows are clickable → `#/logs?id=N` so the operator
  // can jump straight to the live-logs view with that row selected.
  const body: TemplateResult = recent.length
    ? html`<table>
        <thead><tr><th>Time</th><th>Provider</th><th>Model</th><th>Status</th><th>Latency</th><th>Cost</th></tr></thead>
        <tbody>
          ${recent.map((r) => {
            const cls = statusPillClass(r.status_code);
            const href = `#/logs?id=${r.id}`;
            return html`<tr class="clickable" @click=${() => { location.hash = href; }}>
              <td>${r.created_at || ""}</td>
              <td>${r.provider_id || ""}</td>
              <td>${r.upstream_model_id || ""}</td>
              <td><span class="status-pill ${cls}">${r.status_code ?? "—"}</span></td>
              <td>${r.total_ms || 0}ms</td>
              <td>$${(r.cost_usd || 0).toFixed(4)}</td>
            </tr>`;
          })}
        </tbody>
      </table>`
    : html`<p class="empty">No recent requests yet.</p>`;
  return html`<section class="card">
    <div class="section-header"><h3>Recent usage</h3></div>
    ${body}
    <p style="margin-top:0.5rem;"><a href="#/logs">Open live logs →</a></p>
  </section>`;
}

function renderHome(): TemplateResult {
  if (loading) {
    return html`<div class="page-header"><h2>Overview</h2></div>
      <div class="loading">Loading...</div>`;
  }

  // Error rate is `errors / total_rows * 100` — guarded against
  // divide-by-zero. Cost uses 4dp to surface the small per-request
  // numbers typical of LLM pricing. Avg-TTFT shows "—" when the
  // summary has no TTFT samples (all requests errored before TTFT).
  const errorRatePct = summary.total_rows > 0
    ? (summary.errors / summary.total_rows) * 100
    : 0;

  const kpiRow = html`<div class="home-kpi-row">
    ${renderKpiTile("Requests today", String(summary.unique_requests))}
    ${renderKpiTile("Cost today", `$${summary.total_cost_usd.toFixed(4)}`)}
    ${renderKpiTile("Error rate", `${errorRatePct.toFixed(1)}%`, errorRatePct > 5 ? "kpi-trend-down" : "")}
    ${renderKpiTile("Avg TTFT", summary.avg_ttft_ms != null ? `${summary.avg_ttft_ms.toFixed(0)}ms` : "—")}
  </div>`;

  const homeRow = html`<div class="home-row">${renderHealthCard()}${renderRecentCard()}</div>`;

  // Daily usage chart — full-width, edge-to-edge via `.chart-card`.
  // `dailyUsageChart` returns an SVG string; we inject it via
  // `unsafeHTML` (the chart not yet migrated to lit-html). The SVG
  // is built entirely from numeric server data — no user-controlled
  // content — so unsafe injection is safe here.
  const chartBlock = html`<section class="card chart-card">
    <div class="card-title">Last 7 days</div>
    <div class="card-body">${unsafeHTML(dailyUsageChart(byDay))}</div>
  </section>`;

  const quickActions = html`<div class="quick-actions">
    <a href="#/providers">+ Provider</a>
    <a href="#/combos">+ Combo</a>
    <a href="#/keys">+ API Key</a>
  </div>`;

  const footerLine = html`<p class="muted" style="text-align:center;font-size:var(--fs-xs);margin-top:var(--space-4);">
    ${providers.length} providers · ${accounts.length} accounts · ${models.length} models · ${combos.length} combos · ${keys.length} keys
  </p>`;

  return html`
    <div class="page-header"><h2>Overview</h2></div>
    ${kpiRow}${homeRow}${chartBlock}${quickActions}${footerLine}`;
}

// ---- Mount ----

export async function mountHome(): Promise<(() => void) | void> {
  const el = document.getElementById("main");
  if (!el) return;

  // The home view used to read counts and health straight from
  // `state.*`, on the assumption that the 3s background poll had
  // already populated them. After the poll stopped re-rendering
  // (so it wouldn't destroy input focus) that assumption broke:
  // a cold paint of `#/` would show all counts as 0 and
  // `Status: —`, and the user had to wait for the next poll to
  // re-render — which never came. So now the home view fetches
  // what it needs at mount time, just like providers.ts does.
  // The state cache is still consulted first as a fast path for
  // the common "user just navigated here from another view that
  // already fetched" case.
  loading = true;
  const cleanup = mountView(el, renderHome);

  // The recent-usage + summary + by-day endpoints can return an
  // error (e.g. when the usage table is empty/missing). Each
  // `.catch` returns an empty/zero-valued fallback so the rest of
  // the render keeps working.
  const [
    providersResp, accountsResp, modelsResp, combosResp, keysResp,
    recentResp, summaryResp, byDayResp,
  ] = await Promise.all([
    (state.providers && state.providers.length) ? Promise.resolve(state.providers) : api("/providers") as Promise<Provider[]>,
    (state.accounts  && state.accounts.length)  ? Promise.resolve(state.accounts)  : api("/accounts") as Promise<Account[]>,
    (state.models    && state.models.length)    ? Promise.resolve(state.models)    : api("/models") as Promise<Model[]>,
    (state.combos    && state.combos.length)    ? Promise.resolve(state.combos)    : api("/combos") as Promise<Combo[]>,
    (state.apiKeys   && state.apiKeys.length)   ? Promise.resolve(state.apiKeys)   : api("/keys") as Promise<unknown[]>,
    api("/usage/recent?limit=5").catch((): RecentUsageRow[] => []),
    api("/usage/summary?preset=today").catch((): UsageSummary => NULL_SUMMARY) as Promise<UsageSummary>,
    api("/usage/by-day?preset=7d").catch((): ByDayRow[] => []) as Promise<ByDayRow[]>,
  ]) as readonly [Provider[], Account[], Model[], Combo[], unknown[], RecentUsageRow[], UsageSummary, ByDayRow[]];

  providers = providersResp;
  accounts = accountsResp;
  models = modelsResp;
  combos = combosResp;
  keys = keysResp;
  recent = recentResp;
  summary = summaryResp;
  byDay = byDayResp;

  // Backfill the state caches so the next navigation to home (or
  // any view that reads these) gets the data without a re-fetch.
  // The bg-poll will overwrite these with fresher values on its
  // 3s tick — that's expected.
  if (providers) state.providers = providers;
  if (accounts)  state.accounts  = accounts;
  if (models)    state.models    = models;
  if (combos)    state.combos    = combos;
  if (keys)      state.apiKeys   = keys;

  // Pull the health from the state cache (the bg-poll's
  // healthTick populates this on a 1s tick). If the user landed
  // on home on a fresh page load, the health may be null until
  // the first healthTick fires; in that case we kick one off
  // explicitly so the "Health" card shows real data on the first
  // paint rather than "—".
  if (!state.health) {
    try { state.health = await api("/health") as { status: string; message?: string }; } catch (_e) { /* keep null */ }
  }

  loading = false;
  requestUpdate();
  return cleanup;
}
