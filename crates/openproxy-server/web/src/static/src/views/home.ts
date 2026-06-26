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
  isStatusChartBarsMode,
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
  sparkActive: uPlot;
  sparkRequests: uPlot;
  sparkTokens: uPlot;
  sparkCost: uPlot;
  resizeDisposers: Array<() => void>;
  /** True if the status-codes chart was built with the bars plugin (and
   *  therefore expects cumulative-sum stacked data). */
  statusBarsMode: boolean;
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

/** Convert throughput points into uPlot's `AlignedData` format:
 *  `[xs, rps, tps, cps_scaled]`. The cps series is multiplied by 1000 so
 *  it shares the right axis with tps without being invisible (cost is
 *  typically µ$/sec). */
function throughputData(points: ThroughputPoint[]): uPlot.AlignedData {
  const xs: number[] = new Array<number>(points.length);
  const rps: number[] = new Array<number>(points.length);
  const tps: number[] = new Array<number>(points.length);
  const cps: number[] = new Array<number>(points.length);
  for (let i = 0; i < points.length; i++) {
    const p: ThroughputPoint = points[i]!;
    // uPlot's default `ms: 1e-3` means X values are in seconds.
    xs[i] = p.t / 1000;
    rps[i] = p.rps;
    tps[i] = p.tps;
    // cps × 1000 → "millicents/sec" on the tps axis. The cursor readout
    // reverses this scaling (see the `value` formatter in
    // `buildThroughputChart`).
    cps[i] = p.cps * 1000;
  }
  return [xs, rps, tps, cps];
}

/** Convert status-code points into uPlot's `AlignedData` format.
 *
 *  Two modes:
 *  - **Bars mode** (stacked): data is `[xs, s2xx+s4xx+s5xx, s2xx+s4xx, s2xx]`.
 *    The series are rendered in array order: series[1] (5xx total) drawn
 *    first (tallest), series[2] (4xx cumulative) drawn second, series[3]
 *    (2xx) drawn last. Each series' bar is drawn from 0 to its value, so
 *    later (shorter) bars overwrite the bottom portion of earlier (taller)
 *    bars, leaving only the top slice of each visible — that's the stacked
 *    look. The visible slices bottom-to-top are: 2xx, 4xx, 5xx.
 *  - **Line mode** (fallback): data is `[xs, s2xx, s4xx, s5xx]` — original
 *    counts, 3 separate line series. */
function statusCodesData(points: StatusCodePoint[], barsMode: boolean): uPlot.AlignedData {
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
  if (barsMode) {
    // Cumulative sums for stacking. The series array in
    // `buildStatusCodesChart` (bars branch) is ordered:
    //   [X, 5xx-cumulative, 4xx-cumulative, 2xx-original]
    // We compute the cumulatives here.
    const total: number[] = new Array<number>(points.length);
    const mid: number[] = new Array<number>(points.length);
    for (let i = 0; i < points.length; i++) {
      total[i] = s2xx[i]! + s4xx[i]! + s5xx[i]!;
      mid[i] = s2xx[i]! + s4xx[i]!;
    }
    return [xs, total, mid, s2xx];
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

/** Build a flat-line sparkline for the Active Requests KPI. We don't have
 *  historical active-request counts (the live-store only tracks the current
 *  `activeRequests.size`, not a time series), so we plot a flat line at
 *  the current value. The visual is a horizontal line — boring but
 *  truthful (we don't fabricate history). */
function activeRequestsSparkline(currentCount: number): uPlot.AlignedData {
  // 60 points, all at the current value.
  const xs: number[] = new Array<number>(60);
  const ys: number[] = new Array<number>(60);
  for (let i = 0; i < 60; i++) {
    xs[i] = i;
    ys[i] = currentCount;
  }
  return [xs, ys];
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
  return html`<span class="${cls}" title="${t("home.connected")}"></span>`;
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
        @click=${() => onWindowChange(w.value)}
      >${w.label}</button>
    `)}
  </div>`;
}

/** KPI tile — big number, label, and a sparkline container. The sparkline
 *  container has a stable ID so the uPlot instance created in it persists
 *  across lit-html re-renders. */
function renderKpiTile(
  label: string,
  value: string,
  sparklineId: string,
  trendClass: string = "",
): TemplateResult {
  return html`<div class="home-kpi-tile">
    <div class="home-kpi-label">${label}</div>
    <div class="home-kpi-value ${trendClass}">${value}</div>
    <div class="home-kpi-spark" id="${sparklineId}"></div>
  </div>`;
}

/** KPI grid — 4 tiles. The values are computed from the current snapshot.
 *  If the snapshot is null (before the first live-store update), the
 *  values are "—" placeholders. */
function renderKpiGrid(snapshot: Snapshot | null): TemplateResult {
  const activeReqs: string = snapshot ? String(snapshot.activeRequests) : "—";
  const reqsPerMin: string = snapshot
    ? formatCompact(snapshot.requestsPerSec * 60)
    : "—";
  const tokensPerMin: string = snapshot
    ? formatCompact(snapshot.tokensPerSec * 60)
    : "—";
  const costPerMin: string = snapshot
    ? formatCostPerMin(snapshot.costPerSec * 60)
    : "—";

  return html`<div class="home-kpi-grid">
    ${renderKpiTile(t("home.kpi.active_requests"), activeReqs, "spark-active")}
    ${renderKpiTile(t("home.kpi.requests_per_min"), reqsPerMin, "spark-requests")}
    ${renderKpiTile(t("home.kpi.tokens_per_min"), tokensPerMin, "spark-tokens")}
    ${renderKpiTile(t("home.kpi.cost_per_min"), costPerMin, "spark-cost")}
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
  const total: number = won + lost + single;
  const pct: (n: number) => string = (n: number) =>
    total > 0 ? Math.round((n / total) * 100) + "%" : "—";

  return html`<section class="card home-chart-card">
    <div class="card-title">${t("home.chart.race_outcomes")}</div>
    <div class="card-subtitle">${t("home.chart.race_outcomes.subtitle")}</div>
    <div class="home-race-stats">
      <div class="home-race-stat home-race-stat-won">
        <div class="home-race-stat-value">${pct(won)}</div>
        <div class="home-race-stat-label">${t("home.chart.race_outcomes.won")}</div>
        <div class="home-race-stat-count">${won}</div>
      </div>
      <div class="home-race-stat home-race-stat-lost">
        <div class="home-race-stat-value">${pct(lost)}</div>
        <div class="home-race-stat-label">${t("home.chart.race_outcomes.lost")}</div>
        <div class="home-race-stat-count">${lost}</div>
      </div>
      <div class="home-race-stat home-race-stat-single">
        <div class="home-race-stat-value">${pct(single)}</div>
        <div class="home-race-stat-label">${t("home.chart.race_outcomes.single")}</div>
        <div class="home-race-stat-count">${single}</div>
      </div>
    </div>
  </section>`;
}

/** Activity feed row — single recent-usage row rendered as a flex row. */
function renderActivityRow(r: RecentUsageRow, _index: number): TemplateResult {
  const cls: string = statusPillClass(r.status_code);
  return html`<div class="home-activity-row" data-id="${r.id}">
    <span class="home-activity-time">${formatTime(r.created_at)}</span>
    <span class="home-activity-model" title="${r.upstream_model_id || ""}">${r.upstream_model_id || "—"}</span>
    <span class="home-activity-provider" title="${r.provider_id || ""}">${r.provider_id || "—"}</span>
    <span class="home-activity-status"><span class="status-pill ${cls}">${r.status_code ?? "—"}</span></span>
    <span class="home-activity-latency">${formatLatency(r.total_ms)}</span>
    <span class="home-activity-tokens">${formatTokensInOut(r.prompt_tokens, r.completion_tokens)}</span>
    <span class="home-activity-cost">${formatCost(r.cost_usd)}</span>
  </div>`;
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
        ${rows.map((r: RecentUsageRow, i: number) => html`<div data-key=${r.id}>${renderActivityRow(r, i)}</div>`)}
      </div>`;

  return html`<section class="card home-activity-card">
    <div class="card-title">${t("home.activity_feed")}</div>
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

// ----------------------------------------------------------------------------
// Main render function
// ----------------------------------------------------------------------------

function renderHome(): TemplateResult {
  const snapshot: Snapshot | null = currentSnapshot;
  const conn: LiveConnectionState = currentConnectionState;

  return html`
    <div class="home-dashboard">
      <div class="page-header home-header">
        <div class="home-header-text">
          <h2>${t("home.title")}${renderConnectionDot(conn)}</h2>
          <p class="home-subtitle muted">${t("home.subtitle")}</p>
        </div>
        <div class="home-header-actions">${renderWindowSelector()}</div>
      </div>
      ${renderConnectionBanner(conn)}
      ${renderKpiGrid(snapshot)}
      <div class="home-charts-grid">
        <section class="card home-chart-card">
          <div class="card-title">${t("home.chart.throughput")}</div>
          <div class="card-subtitle">${t("home.chart.throughput.subtitle")}</div>
          <div class="home-chart-container" id="chart-throughput"></div>
        </section>
        <section class="card home-chart-card">
          <div class="card-title">${t("home.chart.status_codes")}</div>
          <div class="card-subtitle">${t("home.chart.status_codes.subtitle")}</div>
          <div class="home-chart-container" id="chart-status-codes"></div>
        </section>
        <section class="card home-chart-card">
          <div class="card-title">${t("home.chart.latency")}</div>
          <div class="card-subtitle">${t("home.chart.latency.subtitle")}</div>
          <div class="home-chart-container" id="chart-latency"></div>
        </section>
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

/** Push the current snapshot's data into the 4 main uPlot charts + 4
 *  sparklines via `setData`. Called once after the charts are created
 *  (initial paint) and on every live-store update. */
function pushSnapshotToCharts(): void {
  if (!charts) return;
  const snapshot: Snapshot | null = currentSnapshot;
  if (!snapshot) return;

  // Main charts
  charts.throughput.setData(throughputData(snapshot.throughput));
  charts.statusCodes.setData(
    statusCodesData(snapshot.statusCodes, charts.statusBarsMode),
  );
  charts.latency.setData(latencyData(snapshot.latency));

  // Sparklines — last 60 buckets of each metric.
  const last60: ThroughputPoint[] = snapshot.throughput.slice(-60);
  charts.sparkActive.setData(activeRequestsSparkline(snapshot.activeRequests));
  charts.sparkRequests.setData(sparklineData(last60, "rps", 60));
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

/** Create the 4 main uPlot charts + 4 KPI sparklines. Called after the
 *  first lit-html render (so the chart container `<div>`s exist in the
 *  DOM). Each chart gets a ResizeObserver that keeps it sized to its
 *  container. */
function createCharts(): void {
  if (charts) return; // idempotent

  const throughputEl: HTMLElement | null = document.getElementById("chart-throughput");
  const statusEl: HTMLElement | null = document.getElementById("chart-status-codes");
  const latencyEl: HTMLElement | null = document.getElementById("chart-latency");
  const sparkActiveEl: HTMLElement | null = document.getElementById("spark-active");
  const sparkReqEl: HTMLElement | null = document.getElementById("spark-requests");
  const sparkTokEl: HTMLElement | null = document.getElementById("spark-tokens");
  const sparkCostEl: HTMLElement | null = document.getElementById("spark-cost");

  if (!throughputEl || !statusEl || !latencyEl
      || !sparkActiveEl || !sparkReqEl || !sparkTokEl || !sparkCostEl) {
    // Containers not in the DOM yet — the first lit-html render hasn't
    // completed. The caller should defer via requestAnimationFrame.
    return;
  }

  const resizeDisposers: Array<() => void> = [];

  const throughput: uPlot = buildThroughputChart(throughputEl, windowSecs);
  resizeDisposers.push(observeResize(throughput, throughputEl));

  const statusCodes: uPlot = buildStatusCodesChart(statusEl, windowSecs);
  resizeDisposers.push(observeResize(statusCodes, statusEl));

  const latency: uPlot = buildLatencyChart(latencyEl, windowSecs);
  resizeDisposers.push(observeResize(latency, latencyEl));

  const sparkActive: uPlot = createSparkline(sparkActiveEl, CHART_COLORS.blue);
  const sparkRequests: uPlot = createSparkline(sparkReqEl, CHART_COLORS.blue);
  const sparkTokens: uPlot = createSparkline(sparkTokEl, CHART_COLORS.green);
  const sparkCost: uPlot = createSparkline(sparkCostEl, CHART_COLORS.orange);

  charts = {
    throughput,
    statusCodes,
    latency,
    sparkActive,
    sparkRequests,
    sparkTokens,
    sparkCost,
    resizeDisposers,
    statusBarsMode: isStatusChartBarsMode(),
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
  try { charts.sparkActive.destroy(); } catch (e: unknown) { console.warn("[home] sparkActive.destroy threw:", e); }
  try { charts.sparkRequests.destroy(); } catch (e: unknown) { console.warn("[home] sparkRequests.destroy threw:", e); }
  try { charts.sparkTokens.destroy(); } catch (e: unknown) { console.warn("[home] sparkTokens.destroy threw:", e); }
  try { charts.sparkCost.destroy(); } catch (e: unknown) { console.warn("[home] sparkCost.destroy threw:", e); }
  charts = null;
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
    if (unsubLive) { unsubLive(); unsubLive = null; }
    if (disposeStore) { disposeStore(); disposeStore = null; }
    if (cleanupReactive) { cleanupReactive(); cleanupReactive = null; }
    activityFeedEl = null;
    savedScroll = null;
  };
}
