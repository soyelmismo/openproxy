// views/home.ts — live dashboard (F6).
//
// Replaces the old "Overview" page (which fetched `/usage/summary`,
// `/usage/recent?limit=5`, `/usage/by-day?preset=7d` and rendered a static
// 7-day chart + 4 KPI tiles) with a real-time live dashboard that consumes
// the F5 live-store. The dashboard renders:
//   - 4 KPI tiles (Active requests, Requests/min, Tokens/min, Cost/min)
//     each with a live sparkline (last 60 buckets).
//   - A window selector (1m / 5m / 30m) that controls the snapshot window.
//   - 4 main charts (uPlot): Throughput, Status codes (stacked bars),
//     Latency (p50/p95/p99), Race outcomes (3 stat blocks).
//   - A live activity feed (last 20 rows) with scroll preservation.
//   - A connection state banner above the KPIs.
//
// Architecture
// ------------
// - `mountHome()` is called by the router. It mounts the live-store (F5),
//   subscribes to its throttled re-render callback (4 Hz max), and creates
//   the 4 main uPlot charts + 4 KPI sparklines after the first lit-html
//   render. Returns a cleanup function that unsubscribes, destroys the
//   charts, and unmounts the live-store.
// - The render function `renderHome()` reads module-local state
//   (`currentSnapshot`, `currentConnectionState`, `windowSecs`) and
//   produces a lit-html TemplateResult. Chart container `<div>`s have
//   stable IDs so lit-html preserves them across re-renders — the uPlot
//   instances stay attached.
// - On each live-store subscriber callback:
//   1. Read `getSnapshot(windowSecs)` → store in `currentSnapshot`.
//   2. Read `getConnectionState()` → store in `currentConnectionState`.
//   3. Save the activity feed's scroll position (for post-render restore).
//   4. Call `requestUpdate()` → lit-html patches the KPI numbers, banner,
//      and activity feed rows.
//   5. Schedule a `requestAnimationFrame` callback to (a) push the new
//      data into the uPlot charts via `setData`, (b) restore the activity
//      feed scroll position.
//
// uPlot lifecycle
// ---------------
// The 8 uPlot instances (4 main + 4 sparklines) are created ONCE after the
// first lit-html render, then `setData(...)` is called on each re-render.
// We never recreate the charts. Resize is handled per-chart via a single
// `ResizeObserver` (created in `observeResize`).

import { html, type TemplateResult } from "lit-html";
import { ref } from "lit-html/directives/ref.js";
import type uPlot from "uplot";

import { mountView, requestUpdate } from "../state/reactive.js";
import { t } from "../i18n/index.js";
import { statusPillClass } from "../lib/constants.js";
import type { RecentUsageRow } from "../lib/types/api.js";
import {
  mountLiveStore,
  subscribe,
  getSnapshot,
  getConnectionState,
  type Snapshot,
  type SnapshotWindow,
  type LiveConnectionState,
  type ThroughputPoint,
  type StatusCodePoint,
  type LatencyPoint,
} from "../state/live-store.js";
import {
  buildThroughputChart,
  buildStatusCodesChart,
  buildLatencyChart,
  createSparkline,
  observeResize,
  CHART_COLORS,
} from "../components/uplot-chart.js";

// ============================================================================
// Module-local state
// ============================================================================

/** The latest snapshot from the live-store. Null before the first
 *  subscriber callback fires (the view renders placeholder "—" values
 *  in that case). */
let currentSnapshot: Snapshot | null = null;

/** The latest connection state. Defaults to "disconnected" before the
 *  first subscriber callback. */
let currentConnectionState: LiveConnectionState = "disconnected";

/** The active snapshot window (1m / 5m / 30m). Default 5m. Persisted
 *  across mounts via localStorage so the user's preference survives
 *  navigation away and back. */
let windowSecs: SnapshotWindow = 300;

/** The uPlot instances + their resize observers. Created after the first
 *  lit-html render, destroyed on view unmount. Null before creation. */
interface ChartInstances {
  throughput: uPlot;
  statusCodes: uPlot;
  latency: uPlot;
  sparkRequests: uPlot;
  sparkSuccess: uPlot;
  sparkLatency: uPlot;
  sparkTokens: uPlot;
  sparkCost: uPlot;
  resizeDisposers: Array<() => void>;
}
let charts: ChartInstances | null = null;

/** Live-store subscription disposer. Captured so the cleanup function
 *  can release it on view unmount. */
let unsubLive: (() => void) | null = null;

/** Live-store mount disposer. Captured so the cleanup function can
 *  unmount the store on view unmount (decrements the refcount; the WS
 *  is closed only when the last consumer unmounts). */
let disposeStore: (() => void) | null = null;

/** Lit-html reactive cleanup. Captured so the cleanup function can
 *  release the container. */
let cleanupReactive: (() => void) | null = null;

/** The activity feed scroll container. Captured via `ref` directive so
 *  we can save/restore its scrollTop across lit-html re-renders. */
let activityFeedEl: HTMLElement | null = null;

/** Saved scroll state for the activity feed. Set in the subscriber
 *  callback (before lit-html render), restored in the post-render
 *  `requestAnimationFrame`. */
interface SavedScroll {
  scrollTop: number;
  scrollHeight: number;
}
let savedScroll: SavedScroll | null = null;

// ============================================================================
// Formatters
// ============================================================================

/** Compact number formatter: 1234 → "1.2k", 12345 → "12.3k", 1234567 → "1.2M".
 *  Used for KPI tile values (requests/min, tokens/min). */
function formatCompact(n: number): string {
  if (!Number.isFinite(n)) return "0";
  if (n < 1000) return String(Math.round(n));
  if (n < 10000) return (n / 1000).toFixed(1) + "k";
  if (n < 1_000_000) return Math.round(n / 1000) + "k";
  return (n / 1_000_000).toFixed(1) + "M";
}

/** Currency formatter for the Cost/min KPI tile.
 *  - < $0.01 → 4 decimal places (e.g. $0.0023)
 *  - < $1 → 3 decimal places (e.g. $0.423)
 *  - ≥ $1 → 2 decimal places (e.g. $1.23) */
function formatCostPerMin(usdPerMin: number): string {
  if (!Number.isFinite(usdPerMin) || usdPerMin <= 0) return "$0.00";
  if (usdPerMin < 0.01) return "$" + usdPerMin.toFixed(4);
  if (usdPerMin < 1) return "$" + usdPerMin.toFixed(3);
  return "$" + usdPerMin.toFixed(2);
}

/** Latency formatter for the activity feed: 1234ms → "1.2s", 850ms → "850ms". */
function formatLatency(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms)) return "—";
  if (ms >= 1000) return (ms / 1000).toFixed(1) + "s";
  return Math.round(ms) + "ms";
}

/** Cost formatter for the activity feed: 0.0023 → "$0.0023". */
function formatCost(usd: number | null | undefined): string {
  if (usd == null || !Number.isFinite(usd)) return "—";
  if (usd < 0.01) return "$" + usd.toFixed(4);
  if (usd < 1) return "$" + usd.toFixed(3);
  return "$" + usd.toFixed(2);
}

/** Token count formatter for the activity feed: in/out as "1.2k/850". */
function formatTokensInOut(inTok: number | null, outTok: number | null): string {
  const i: string = inTok != null ? formatCompact(inTok) : "0";
  const o: string = outTok != null ? formatCompact(outTok) : "0";
  return i + "/" + o;
}

/** Timestamp formatter for the activity feed: ISO string → "HH:MM:SS". */
function formatTime(iso: string): string {
  if (!iso) return "—";
  // Parse the ISO string and format as HH:MM:SS in the user's local
  // timezone. `new Date(iso)` handles the ISO 8601 parsing.
  const d: Date = new Date(iso);
  if (Number.isNaN(d.getTime())) return "—";
  const hh: string = String(d.getHours()).padStart(2, "0");
  const mm: string = String(d.getMinutes()).padStart(2, "0");
  const ss: string = String(d.getSeconds()).padStart(2, "0");
  return `${hh}:${mm}:${ss}`;
}

// ============================================================================
// Data preparation for uPlot
// ============================================================================

/** Convert throughput points into `[time, requests/s, tokens/s]`. */
function throughputData(points: ThroughputPoint[]): uPlot.AlignedData {
  const xs: number[] = new Array<number>(points.length);
  const rps: number[] = new Array<number>(points.length);
  const tps: number[] = new Array<number>(points.length);
  for (let i = 0; i < points.length; i++) {
    const p: ThroughputPoint = points[i]!;
    xs[i] = p.t / 1000;
    rps[i] = p.rps;
    tps[i] = p.tps;
  }
  return [xs, rps, tps];
}

function statusCodesData(points: StatusCodePoint[]): uPlot.AlignedData {
  const xs: number[] = new Array<number>(points.length);
  const s2xx: number[] = new Array<number>(points.length);
  const s4xx: number[] = new Array<number>(points.length);
  const s5xx: number[] = new Array<number>(points.length);
  for (let i = 0; i < points.length; i++) {
    const p: StatusCodePoint = points[i]!;
    xs[i] = p.t / 1000;
    s2xx[i] = p.s2xx;
    s4xx[i] = p.s4xx;
    s5xx[i] = p.s5xx;
  }
  return [xs, s2xx, s4xx, s5xx];
}

/** Convert latency points into uPlot's `AlignedData` format:
 *  `[xs, p50, p95, p99]`. */
function latencyData(points: LatencyPoint[]): uPlot.AlignedData {
  const xs: number[] = new Array<number>(points.length);
  const p50: number[] = new Array<number>(points.length);
  const p95: number[] = new Array<number>(points.length);
  const p99: number[] = new Array<number>(points.length);
  for (let i = 0; i < points.length; i++) {
    const p: LatencyPoint = points[i]!;
    xs[i] = p.t / 1000;
    p50[i] = p.p50;
    p95[i] = p.p95;
    p99[i] = p.p99;
  }
  return [xs, p50, p95, p99];
}

/** Take the last N entries of a throughput array. Used for the KPI
 *  sparklines (which show a 60-bucket thumbnail of the relevant metric). */
function sparklineData(
  points: ThroughputPoint[],
  field: "rps" | "tps" | "cps",
  n: number,
): uPlot.AlignedData {
  const start: number = Math.max(0, points.length - n);
  const len: number = points.length - start;
  const xs: number[] = new Array<number>(len);
  const ys: number[] = new Array<number>(len);
  for (let i = 0; i < len; i++) {
    const p: ThroughputPoint = points[start + i]!;
    xs[i] = i; // sparkline X axis is hidden — just use indices
    ys[i] = p[field];
  }
  return [xs, ys];
}

function successSparkline(points: StatusCodePoint[]): uPlot.AlignedData {
  const xs: number[] = new Array<number>(points.length);
  const ys: Array<number | null> = new Array<number | null>(points.length);
  for (let i = 0; i < points.length; i++) {
    const p = points[i]!;
    const total = p.s2xx + p.s4xx + p.s5xx;
    xs[i] = i;
    ys[i] = total > 0 ? p.s2xx / total * 100 : null;
  }
  return [xs, ys];
}

function latencySparkline(points: LatencyPoint[]): uPlot.AlignedData {
  return [points.map((_, i) => i), points.map((point) => point.p95 || null)];
}

// ============================================================================
// Templates
// ============================================================================

/** Connection state banner — shown above the KPIs. Hidden when connected
 *  (the green dot in the header is enough). */
function renderConnectionBanner(state: LiveConnectionState): TemplateResult {
  if (state === "connected") return html``;
  if (state === "connecting") {
    return html`<div class="home-banner home-banner-warn">
      <span class="home-banner-icon">↻</span>
      <span>${t("home.connecting")}</span>
    </div>`;
  }
  // disconnected
  return html`<div class="home-banner home-banner-error">
    <span class="home-banner-icon">⚠</span>
    <span>${t("home.disconnected")}</span>
  </div>`;
}

/** Header dot — green when connected, yellow when connecting, red when
 *  disconnected. Rendered inline in the page header. */
function renderConnectionDot(state: LiveConnectionState): TemplateResult {
  const cls: string = state === "connected"
    ? "home-conn-dot home-conn-dot-ok"
    : state === "connecting"
    ? "home-conn-dot home-conn-dot-warn"
    : "home-conn-dot home-conn-dot-err";
  const title = state === "connected" ? t("home.connected") : state === "connecting" ? t("home.connecting") : t("home.disconnected");
  return html`<span class="${cls}" title=${title}></span>`;
}

/** Window selector — segmented control with 1m / 5m / 30m buttons. The
 *  active button is highlighted. Clicking a button updates `windowSecs`
 *  and triggers a re-render + chart data refresh. */
function renderWindowSelector(): TemplateResult {
  const windows: ReadonlyArray<{ value: SnapshotWindow; label: string }> = [
    { value: 60, label: t("home.window.1m") },
    { value: 300, label: t("home.window.5m") },
    { value: 1800, label: t("home.window.30m") },
  ];
  return html`<div class="home-window-selector" role="group">
    ${windows.map((w) => html`
      <button
        type="button"
        class="home-window-btn ${w.value === windowSecs ? "active" : ""}"
        aria-pressed=${w.value === windowSecs ? "true" : "false"}
        @click=${() => onWindowChange(w.value)}
      >${w.label}</button>
    `)}
  </div>`;
}

function renderKpiTile(
  label: string,
  value: string,
  meta: string,
  sparklineId: string | null,
  tone: string = "",
): TemplateResult {
  return html`<div class="home-kpi-tile ${tone}">
    <div class="home-kpi-label">${label}</div>
    <div class="home-kpi-value">${value}</div>
    <div class="home-kpi-meta">${meta}</div>
    ${sparklineId
      ? html`<div class="home-kpi-spark" id=${sparklineId}></div>`
      : html`<div class="home-kpi-live"><span></span>${t("home.kpi.current")}</div>`}
  </div>`;
}

function renderKpiGrid(snapshot: Snapshot | null): TemplateResult {
  const status = snapshot?.statusCodes.reduce((acc, point) => {
    acc.ok += point.s2xx;
    acc.errors += point.s4xx + point.s5xx;
    return acc;
  }, { ok: 0, errors: 0 }) ?? { ok: 0, errors: 0 };
  const responses = status.ok + status.errors;
  const bucketSecs = snapshot && snapshot.throughput.length > 0 ? windowSecs / snapshot.throughput.length : 1;
  const windowRequests = snapshot?.throughput.reduce((sum, point) => sum + point.rps * bucketSecs, 0) ?? 0;
  const success = responses > 0 ? `${(status.ok / responses * 100).toFixed(1)}%` : "—";
  const successTone = responses === 0 ? "" : status.ok / responses >= 0.99 ? "is-good" : status.ok / responses >= 0.95 ? "is-warn" : "is-bad";

  return html`<div class="home-kpi-grid">
    ${renderKpiTile(t("home.kpi.active_requests"), snapshot ? String(snapshot.activeRequests) : "—", t("home.kpi.in_flight"), null)}
    ${renderKpiTile(t("home.kpi.requests_per_min"), snapshot ? formatCompact(snapshot.requestsPerSec * 60) : "—", `${formatCompact(windowRequests)} ${t("home.kpi.in_window")}`, "spark-requests")}
    ${renderKpiTile(t("home.kpi.success_rate"), success, `${formatCompact(status.errors)} ${t("home.kpi.failed")}`, "spark-success", successTone)}
    ${renderKpiTile(t("home.kpi.p95_latency"), snapshot && responses > 0 ? formatLatency(snapshot.p95LatencyMs) : "—", `${t("home.kpi.p50")} ${snapshot && responses > 0 ? formatLatency(snapshot.p50LatencyMs) : "—"}`, "spark-latency")}
    ${renderKpiTile(t("home.kpi.tokens_per_min"), snapshot ? formatCompact(snapshot.tokensPerSec * 60) : "—", t("home.kpi.rolling_rate"), "spark-tokens")}
    ${renderKpiTile(t("home.kpi.cost_per_min"), snapshot ? formatCostPerMin(snapshot.costPerSec * 60) : "—", `${formatCostPerMin((snapshot?.costPerSec ?? 0) * 3600)} ${t("home.kpi.per_hour")}`, "spark-cost")}
  </div>`;
}

/** Race outcomes card — 3 stat blocks (Won via race / Lost race / Single-
 *  target) with percentages. We use option (c) from the spec: 3 stat
 *  blocks instead of a donut chart (uPlot is time-series focused; a
 *  half-baked donut would be worse than clean stat blocks). */
function renderRaceOutcomesCard(snapshot: Snapshot | null): TemplateResult {
  const won: number = snapshot ? snapshot.raceOutcomes.won : 0;
  const lost: number = snapshot ? snapshot.raceOutcomes.lost : 0;
  const single: number = snapshot ? snapshot.raceOutcomes.single : 0;
  const raced: number = won + lost;
  const total: number = raced + single;
  const raceShare = total > 0 ? raced / total : 0;
  const winShare = raced > 0 ? won / raced : 0;

  return html`<section class="card home-chart-card">
    <div class="home-card-heading"><div><h3>${t("home.chart.race_outcomes")}</h3><p>${t("home.chart.race_outcomes.subtitle")}</p></div><span class="home-chart-stat">${raced > 0 ? `${(winShare * 100).toFixed(1)}%` : "—"}</span></div>
    <div class="home-race-summary">
      <div class="home-race-hero"><strong>${raced > 0 ? `${(winShare * 100).toFixed(1)}%` : "—"}</strong><span>${t("home.race.winning_attempts")}</span></div>
      <div class="home-race-bars" aria-label=${t("home.chart.race_outcomes")}>
        <div><span>${t("home.chart.race_outcomes.won")}</span><strong>${formatCompact(won)}</strong><i><b class="won" style=${`width:${winShare * 100}%`}></b></i></div>
        <div><span>${t("home.chart.race_outcomes.lost")}</span><strong>${formatCompact(lost)}</strong><i><b class="lost" style=${`width:${raced > 0 ? lost / raced * 100 : 0}%`}></b></i></div>
        <div><span>${t("home.chart.race_outcomes.single")}</span><strong>${formatCompact(single)}</strong><i><b class="single" style=${`width:${total > 0 ? single / total * 100 : 0}%`}></b></i></div>
      </div>
    </div>
    <div class="home-race-foot">${(raceShare * 100).toFixed(1)}% ${t("home.race.traffic_raced")}</div>
  </section>`;
}

function renderActivityRow(r: RecentUsageRow): TemplateResult {
  const cls: string = statusPillClass(r.status_code);
  return html`<a class="home-activity-row" href=${`#/logs?request_id=${encodeURIComponent(r.request_id || "")}`} data-id=${r.id}>
    <span class="home-activity-time">${formatTime(r.created_at)}</span>
    <span class="home-activity-model" title="${r.upstream_model_id || ""}">${r.upstream_model_id || "—"}</span>
    <span class="home-activity-provider" title="${r.provider_id || ""}">${r.provider_id || "—"}</span>
    <span class="home-activity-status"><span class="status-pill ${cls}">${r.status_code ?? "—"}</span></span>
    <span class="home-activity-latency">${formatLatency(r.total_ms)}</span>
    <span class="home-activity-tokens">${formatTokensInOut(r.prompt_tokens, r.completion_tokens)}</span>
    <span class="home-activity-cost">${formatCost(r.cost_usd)}</span>
  </a>`;
}

/** Activity feed — scrollable list of the last 20 rows. Uses `repeat`
 *  with row id as the key so lit-html reuses DOM nodes (essential for
 *  scroll preservation: existing rows keep their identity, new rows are
 *  inserted at the top). */
function renderActivityFeed(snapshot: Snapshot | null): TemplateResult {
  const rows: RecentUsageRow[] = snapshot ? snapshot.recentRows : [];
  const body: TemplateResult = rows.length === 0
    ? html`<div class="home-activity-empty muted">${t("home.activity_feed.empty")}</div>`
    : html`<div class="home-activity-list" ${ref(activityFeedRef)}>
        ${rows.map((r: RecentUsageRow) => renderActivityRow(r))}
      </div>`;

  return html`<section class="card home-activity-card">
    <div class="home-card-heading"><div><h3>${t("home.activity_feed")}</h3><p>${t("home.activity_feed.subtitle")}</p></div><a href="#/logs">${t("home.activity_feed.view_all")} →</a></div>
    <div class="home-activity-header">
      <span>${t("home.activity_feed.col_time") || "Time"}</span>
      <span>${t("home.activity_feed.col_model") || "Model"}</span>
      <span>${t("home.activity_feed.col_provider") || "Provider"}</span>
      <span>${t("home.activity_feed.col_status") || "Status"}</span>
      <span>${t("home.activity_feed.col_latency") || "Latency"}</span>
      <span>${t("home.activity_feed.col_tokens") || "Tokens (in/out)"}</span>
      <span>${t("home.activity_feed.col_cost") || "Cost"}</span>
    </div>
    ${body}
  </section>`;
}

/** Callback ref that captures the activity feed scroll container. Used
 *  for scroll preservation across re-renders. */
function activityFeedRef(el: Element | undefined): void {
  activityFeedEl = el instanceof HTMLElement ? el : null;
}

function renderChartCard(title: string, subtitle: string, id: string, stat: string): TemplateResult {
  return html`<section class="card home-chart-card">
    <div class="home-card-heading"><div><h3>${title}</h3><p>${subtitle}</p></div><span class="home-chart-stat">${stat}</span></div>
    <div class="home-chart-container" id=${id}></div>
  </section>`;
}

// ----------------------------------------------------------------------------
// Main render function
// ----------------------------------------------------------------------------

function renderHome(): TemplateResult {
  const snapshot: Snapshot | null = currentSnapshot;
  const conn: LiveConnectionState = currentConnectionState;
  const hasResponses = snapshot?.statusCodes.some((point) => point.s2xx + point.s4xx + point.s5xx > 0) ?? false;

  return html`
    <div class="home-dashboard">
      <div class="page-header home-header">
        <div class="home-header-text">
          <span class="page-eyebrow">${t("common.realtime")}</span>
          <h2>${t("home.title")}${renderConnectionDot(conn)}</h2>
          <p class="home-subtitle muted">${t("home.subtitle")}</p>
        </div>
        <div class="home-header-actions">${renderWindowSelector()}</div>
      </div>
      ${renderConnectionBanner(conn)}
      ${renderKpiGrid(snapshot)}
      <div class="home-charts-grid">
        ${renderChartCard(t("home.chart.throughput"), t("home.chart.throughput.subtitle"), "chart-throughput", snapshot ? `${formatCompact(snapshot.requestsPerSec * 60)} rpm` : "—")}
        ${renderChartCard(t("home.chart.status_codes"), t("home.chart.status_codes.subtitle"), "chart-status-codes", snapshot && hasResponses ? `${(snapshot.successRate * 100).toFixed(1)}%` : "—")}
        ${renderChartCard(t("home.chart.latency"), t("home.chart.latency.subtitle"), "chart-latency", snapshot && hasResponses ? `p95 ${formatLatency(snapshot.p95LatencyMs)}` : "—")}
        ${renderRaceOutcomesCard(snapshot)}
      </div>
      ${renderActivityFeed(snapshot)}
    </div>
  `;
}

// ============================================================================
// Live-store subscriber callback
// ============================================================================

/** Called by the live-store on each throttled re-render (max 4 Hz). Reads
 *  the new snapshot, saves the activity feed scroll position, triggers a
 *  lit-html re-render, and schedules a post-render callback to push the
 *  new data into the uPlot charts + restore the scroll position. */
function onLiveUpdate(): void {
  // Save the activity feed scroll state BEFORE the lit-html re-render.
  // After the re-render, we'll restore it (adjusted for any new rows
  // prepended at the top) so the user's view is pinned to the same
  // content.
  if (activityFeedEl) {
    savedScroll = {
      scrollTop: activityFeedEl.scrollTop,
      scrollHeight: activityFeedEl.scrollHeight,
    };
  }

  // Read the new snapshot + connection state.
  currentSnapshot = getSnapshot(windowSecs);
  currentConnectionState = getConnectionState();

  // Trigger the lit-html re-render (microtask-coalesced). This patches
  // the KPI numbers, banner, activity feed rows, etc.
  requestUpdate();

  // Schedule a post-render callback to push the new data into the uPlot
  // charts and restore the activity feed scroll position. rAF runs after
  // the next paint, so the DOM is fully laid out by then.
  requestAnimationFrame(() => {
    pushSnapshotToCharts();
    restoreActivityScroll();
  });
}

/** Push the current snapshot's data into the main charts and KPI
 *  sparklines via `setData`. Called once after the charts are created
 *  (initial paint) and on every live-store update. */
function pushSnapshotToCharts(): void {
  if (!charts) return;
  const snapshot: Snapshot | null = currentSnapshot;
  if (!snapshot) return;

  // Main charts
  charts.throughput.setData(throughputData(snapshot.throughput));
  charts.statusCodes.setData(statusCodesData(snapshot.statusCodes));
  charts.latency.setData(latencyData(snapshot.latency));

  // Sparklines — last 60 buckets of each metric.
  const last60: ThroughputPoint[] = snapshot.throughput.slice(-60);
  charts.sparkRequests.setData(sparklineData(last60, "rps", 60));
  charts.sparkSuccess.setData(successSparkline(snapshot.statusCodes.slice(-60)));
  charts.sparkLatency.setData(latencySparkline(snapshot.latency.slice(-60)));
  charts.sparkTokens.setData(sparklineData(last60, "tps", 60));
  charts.sparkCost.setData(sparklineData(last60, "cps", 60));
}

/** Restore the activity feed scroll position after a lit-html re-render.
 *  If the user was at scrollTop=0, leave it (newest row appears at top).
 *  If they'd scrolled down, add the delta in scrollHeight to scrollTop so
 *  their view is pinned to the same content. */
function restoreActivityScroll(): void {
  if (!activityFeedEl || !savedScroll) return;
  const newScrollHeight: number = activityFeedEl.scrollHeight;
  const delta: number = newScrollHeight - savedScroll.scrollHeight;
  // Only adjust if the user had scrolled away from the top. If they were
  // at the top (scrollTop === 0), keep them there so they see the new
  // row appear at the top.
  if (savedScroll.scrollTop > 0 && delta !== 0) {
    activityFeedEl.scrollTop = savedScroll.scrollTop + delta;
  }
  savedScroll = null;
}

// ============================================================================
// Chart lifecycle
// ============================================================================

/** Create the main uPlot charts and KPI sparklines. Called after the
 *  first lit-html render (so the chart container `<div>`s exist in the
 *  DOM). Each chart gets a ResizeObserver that keeps it sized to its
 *  container. */
function createCharts(): void {
  if (charts) return; // idempotent

  const throughputEl: HTMLElement | null = document.getElementById("chart-throughput");
  const statusEl: HTMLElement | null = document.getElementById("chart-status-codes");
  const latencyEl: HTMLElement | null = document.getElementById("chart-latency");
  const sparkReqEl: HTMLElement | null = document.getElementById("spark-requests");
  const sparkSuccessEl: HTMLElement | null = document.getElementById("spark-success");
  const sparkLatencyEl: HTMLElement | null = document.getElementById("spark-latency");
  const sparkTokEl: HTMLElement | null = document.getElementById("spark-tokens");
  const sparkCostEl: HTMLElement | null = document.getElementById("spark-cost");

  if (!throughputEl || !statusEl || !latencyEl
      || !sparkReqEl || !sparkSuccessEl || !sparkLatencyEl || !sparkTokEl || !sparkCostEl) {
    // Containers not in the DOM yet — the first lit-html render hasn't
    // completed. The caller should defer via requestAnimationFrame.
    return;
  }

  const resizeDisposers: Array<() => void> = [];

  const throughput: uPlot = buildThroughputChart(throughputEl);
  resizeDisposers.push(observeResize(throughput, throughputEl));

  const statusCodes: uPlot = buildStatusCodesChart(statusEl);
  resizeDisposers.push(observeResize(statusCodes, statusEl));

  const latency: uPlot = buildLatencyChart(latencyEl);
  resizeDisposers.push(observeResize(latency, latencyEl));

  const sparkRequests: uPlot = createSparkline(sparkReqEl, CHART_COLORS.blue);
  const sparkSuccess: uPlot = createSparkline(sparkSuccessEl, CHART_COLORS.green);
  const sparkLatency: uPlot = createSparkline(sparkLatencyEl, CHART_COLORS.orange);
  const sparkTokens: uPlot = createSparkline(sparkTokEl, CHART_COLORS.green);
  const sparkCost: uPlot = createSparkline(sparkCostEl, CHART_COLORS.orange);

  charts = {
    throughput,
    statusCodes,
    latency,
    sparkRequests,
    sparkSuccess,
    sparkLatency,
    sparkTokens,
    sparkCost,
    resizeDisposers,
  };

  // Push the current snapshot (if any) into the new charts.
  pushSnapshotToCharts();
}

/** Destroy all uPlot instances + disconnect their ResizeObservers. Called
 *  on view unmount. */
function destroyCharts(): void {
  if (!charts) return;
  for (const disposer of charts.resizeDisposers) {
    try { disposer(); } catch (e: unknown) {
      console.warn("[home] resize disposer threw:", e);
    }
  }
  try { charts.throughput.destroy(); } catch (e: unknown) { console.warn("[home] throughput.destroy threw:", e); }
  try { charts.statusCodes.destroy(); } catch (e: unknown) { console.warn("[home] statusCodes.destroy threw:", e); }
  try { charts.latency.destroy(); } catch (e: unknown) { console.warn("[home] latency.destroy threw:", e); }
  try { charts.sparkRequests.destroy(); } catch (e: unknown) { console.warn("[home] sparkRequests.destroy threw:", e); }
  try { charts.sparkSuccess.destroy(); } catch (e: unknown) { console.warn("[home] sparkSuccess.destroy threw:", e); }
  try { charts.sparkLatency.destroy(); } catch (e: unknown) { console.warn("[home] sparkLatency.destroy threw:", e); }
  try { charts.sparkTokens.destroy(); } catch (e: unknown) { console.warn("[home] sparkTokens.destroy threw:", e); }
  try { charts.sparkCost.destroy(); } catch (e: unknown) { console.warn("[home] sparkCost.destroy threw:", e); }
  charts = null;
}

function refreshChartTheme(): void {
  destroyCharts();
  requestAnimationFrame(() => createCharts());
}

// ============================================================================
// Event handlers
// ============================================================================

/** Window selector button click handler. Updates `windowSecs` and triggers
 *  an immediate chart data refresh (so the user sees the new window's data
 *  without waiting for the next live-store update). */
function onWindowChange(newWindow: SnapshotWindow): void {
  if (newWindow === windowSecs) return;
  windowSecs = newWindow;
  // Read a fresh snapshot for the new window and trigger a re-render.
  currentSnapshot = getSnapshot(windowSecs);
  requestUpdate();
  // The charts will be updated in the next onLiveUpdate callback, but we
  // can also push immediately for snappy UX.
  requestAnimationFrame(() => {
    pushSnapshotToCharts();
    restoreActivityScroll();
  });
}

// ============================================================================
// Mount
// ============================================================================

export async function mountHome(): Promise<(() => void) | void> {
  const main: HTMLElement | null = document.getElementById("main");
  if (!main) return;

  // Reset view-local state on every mount.
  currentSnapshot = null;
  currentConnectionState = "disconnected";
  charts = null;
  activityFeedEl = null;
  savedScroll = null;

  // Mount the live-store. First consumer opens the WS + rehydrates from
  // /usage/recent. The store stays mounted across quick navigations
  // (home → logs → home) so the data stays warm.
  disposeStore = mountLiveStore();

  // Subscribe to live-store updates. The store calls subscribers on a
  // throttled cadence (max 4 Hz).
  unsubLive = subscribe(onLiveUpdate);

  // Mount the lit-html view. `mountView` registers the render function
  // with the reactive system so `requestUpdate()` (called from
  // `onLiveUpdate`) triggers a microtask-coalesced re-render.
  cleanupReactive = mountView(main, renderHome);
  document.addEventListener("themechange", refreshChartTheme);
  onLiveUpdate();

  // Create the uPlot charts after the first lit-html render. We use
  // `requestAnimationFrame` (rather than `queueMicrotask`) so the
  // browser has laid out the chart containers — `clientWidth` /
  // `clientHeight` are correct by then.
  requestAnimationFrame(() => {
    createCharts();
    // The live-store may have already delivered a snapshot by the time
    // the charts are created (the first subscriber callback can fire
    // before the rAF). `pushSnapshotToCharts` is idempotent (no-op if
    // `currentSnapshot` is null), so calling it here is safe.
    pushSnapshotToCharts();
  });

  // Cleanup: destroy charts → unsubscribe → unmount store → release
  // lit-html container. Order matters: charts first (so they don't
  // try to update after the store is gone), then store, then lit-html.
  return () => {
    destroyCharts();
    document.removeEventListener("themechange", refreshChartTheme);
    if (unsubLive) { unsubLive(); unsubLive = null; }
    if (disposeStore) { disposeStore(); disposeStore = null; }
    if (cleanupReactive) { cleanupReactive(); cleanupReactive = null; }
    activityFeedEl = null;
    savedScroll = null;
  };
}
