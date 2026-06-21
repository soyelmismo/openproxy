// views/home.ts — operator-focused landing dashboard. Replaces the
// old inventory-grid layout (5 full-width count cards) with a
// four-tile KPI row (requests / cost / error-rate / avg-TTFT for
// "today"), a two-column row (health + recent usage), a 7-day daily
// usage chart, a quick-actions bar, and a small footer line with the
// inventory counts. The KPI + chart data come from the same
// `/usage/*` endpoints the analytics view uses; the inventory
// counts and health come from the existing state cache (backfilled
// with a direct fetch on a cold paint, same pattern as before).

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { escapeHtml } from "../lib/escape.js";
import { statusPillClass } from "../lib/constants.js";
import { pageHeader } from "../components/page-header.js";
import { card } from "../components/card.js";
import { dailyUsageChart, kpiTile } from "../components/charts.js";
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

// `card()` produces `.section-header` markup; the `.card.chart-card`
// CSS variant expects `.card-title` + `.card-body` children instead
// (with zero padding so the SVG stretches edge-to-edge). Mirrors the
// helper in views/analytics.ts.
function chartCard(title: string, body: string): string {
  return `<section class="card chart-card">
    <div class="card-title">${escapeHtml(title)}</div>
    <div class="card-body">${body}</div>
  </section>`;
}

// Wire up any `.clickable[data-href]` rows so clicking jumps to the
// URL in `data-href` (sets `location.hash`, the router does the
// rest). Used by the recent-usage table to deep-link into `#/logs`.
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

export async function mountHome(): Promise<void> {
  const main = document.getElementById("main");
  if (!main) return;
  main.innerHTML = pageHeader({ title: "Overview" }) +
    `<div class="loading">Loading...</div>`;
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
  // The recent-usage + summary + by-day endpoints can return an
  // error (e.g. when the usage table is empty/missing). Each
  // `.catch` returns an empty/zero-valued fallback so the rest of
  // the render keeps working.
  const [providers, accounts, models, combos, keys, recent, summary, byDay] = await Promise.all([
    (state.providers && state.providers.length) ? Promise.resolve(state.providers) : api("/providers") as Promise<Provider[]>,
    (state.accounts  && state.accounts.length)  ? Promise.resolve(state.accounts)  : api("/accounts") as Promise<Account[]>,
    (state.models    && state.models.length)    ? Promise.resolve(state.models)    : api("/models") as Promise<Model[]>,
    (state.combos    && state.combos.length)    ? Promise.resolve(state.combos)    : api("/combos") as Promise<Combo[]>,
    (state.apiKeys   && state.apiKeys.length)   ? Promise.resolve(state.apiKeys)   : api("/keys") as Promise<unknown[]>,
    api("/usage/recent?limit=5").catch((): RecentUsageRow[] => []),
    api("/usage/summary?preset=today").catch((): UsageSummary => NULL_SUMMARY) as Promise<UsageSummary>,
    api("/usage/by-day?preset=7d").catch((): ByDayRow[] => []) as Promise<ByDayRow[]>,
  ]) as readonly [Provider[], Account[], Model[], Combo[], unknown[], RecentUsageRow[], UsageSummary, ByDayRow[]];
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

  // ── KPI row (4 tiles, "today" window) ──────────────────────────
  // Error rate is `errors / total_rows * 100` — guarded against
  // divide-by-zero. Cost uses 4dp to surface the small per-request
  // numbers typical of LLM pricing. Avg-TTFT shows "—" when the
  // summary has no TTFT samples (all requests errored before TTFT).
  const errorRatePct = summary.total_rows > 0
    ? (summary.errors / summary.total_rows) * 100
    : 0;
  const kpiHtml = [
    kpiTile({ label: "Requests today", value: String(summary.unique_requests) }),
    kpiTile({ label: "Cost today", value: `$${summary.total_cost_usd.toFixed(4)}` }),
    kpiTile({
      label: "Error rate",
      value: `${errorRatePct.toFixed(1)}%`,
      valueClass: errorRatePct > 5 ? "kpi-trend-down" : "",
    }),
    kpiTile({
      label: "Avg TTFT",
      value: summary.avg_ttft_ms != null ? `${summary.avg_ttft_ms.toFixed(0)}ms` : "—",
    }),
  ].join("");
  const kpiRow = `<div class="home-kpi-row">${kpiHtml}</div>`;

  // ── Two-column row: Health + Recent usage ──────────────────────
  const healthCard = card("Health", `
    <p>Status: <strong>${state.health ? escapeHtml(state.health.status) : "—"}</strong></p>
    ${state.health && state.health.message ? `<p class="muted">${escapeHtml(state.health.message)}</p>` : ""}
  `);
  // Recent usage rows are clickable → `#/logs?id=N` so the operator
  // can jump straight to the live-logs view with that row selected.
  const recentRows = (recent || []).map((r) => {
    const cls = statusPillClass(r.status_code);
    const href = `#/logs?id=${r.id}`;
    return `<tr class="clickable" data-href="${escapeHtml(href)}">
      <td>${escapeHtml(r.created_at || "")}</td>
      <td>${escapeHtml(r.provider_id || "")}</td>
      <td>${escapeHtml(r.upstream_model_id || "")}</td>
      <td><span class="status-pill ${cls}">${r.status_code ?? "—"}</span></td>
      <td>${r.total_ms || 0}ms</td>
      <td>$${(r.cost_usd || 0).toFixed(4)}</td>
    </tr>`;
  }).join("");
  const recentCard = card("Recent usage", `
    ${recent.length ? `<table>
      <thead><tr><th>Time</th><th>Provider</th><th>Model</th><th>Status</th><th>Latency</th><th>Cost</th></tr></thead>
      <tbody>${recentRows}</tbody>
    </table>` : `<p class="empty">No recent requests yet.</p>`}
    <p style="margin-top:0.5rem;"><a href="#/logs">Open live logs →</a></p>
  `);
  const homeRow = `<div class="home-row">${healthCard}${recentCard}</div>`;

  // ── Daily usage chart (last 7 days) ────────────────────────────
  // Reuses the analytics `dailyUsageChart` (dual-axis: requests line
  // + cost bars). Renders full-width inside a `.chart-card`.
  const chartBlock = chartCard("Last 7 days", dailyUsageChart(byDay));

  // ── Quick actions ──────────────────────────────────────────────
  const quickActions = `<div class="quick-actions">
    <a href="#/providers">+ Provider</a>
    <a href="#/combos">+ Combo</a>
    <a href="#/keys">+ API Key</a>
  </div>`;

  // ── Footer inventory line ──────────────────────────────────────
  // Replaces the old inventory card grid. One line, muted, centered.
  const footerLine = `<p class="muted" style="text-align:center;font-size:var(--fs-xs);margin-top:var(--space-4);">
    ${providers.length} providers · ${accounts.length} accounts · ${models.length} models · ${combos.length} combos · ${keys.length} keys
  </p>`;

  main.innerHTML = pageHeader({ title: "Overview" }) +
    kpiRow + homeRow + chartBlock + quickActions + footerLine;
  wireClickableRows();
}
