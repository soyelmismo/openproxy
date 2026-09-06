// views/home/charts.ts — main uPlot charts (throughput, status codes, latency)
// and the race outcomes card. Provides creation, destruction, data preparation
// functions and the window selector template.

import { html, type TemplateResult } from "lit-html";
import type uPlot from "uplot";

import { t } from "../../i18n/index.js";
import type { Snapshot, SnapshotWindow } from "./types.js";
import type {
  ThroughputPoint,
  StatusCodePoint,
  LatencyPoint,
} from "../../state/live-store.js";
import {
  buildThroughputChart,
  buildStatusCodesChart,
  buildLatencyChart,
  observeResize,
} from "../../components/uplot-chart/index.js";
import { formatCompact, formatLatency } from "./kpis.js";

// ==========
// Data preparation for uPlot
// ==========

/** Convert throughput points into `[time, requests/s, tokens/s]`. */
export function throughputData(points: ThroughputPoint[]): uPlot.AlignedData {
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

export function statusCodesData(points: StatusCodePoint[]): uPlot.AlignedData {
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
export function latencyData(points: LatencyPoint[]): uPlot.AlignedData {
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

// ==========
// Chart card template
// ==========

function renderChartCard(title: string, subtitle: string, id: string, stat: string): TemplateResult {
  return html`<section class="card home-chart-card">
    <div class="home-card-heading"><div><h3>${title}</h3><p>${subtitle}</p></div><span class="home-chart-stat">${stat}</span></div>
    <div class="home-chart-container" id=${id}></div>
  </section>`;
}

// ==========
// Race outcomes card
// ==========

/** Race outcomes card — 3 stat blocks (Won via race / Lost race / Single-
 *  target) with percentages. Uses option (c) from the spec: 3 stat
 *  blocks instead of a donut chart (uPlot is time-series focused; a
 *  half-baked donut would be worse than clean stat blocks). */
function renderRaceOutcomesCard(snapshot: Snapshot | null): TemplateResult {
  const won: number = snapshot ? snapshot.raceOutcomes.won : 0;
  const lost: number = snapshot ? snapshot.raceOutcomes.lost : 0;
  const single: number = snapshot ? snapshot.raceOutcomes.single : 0;
  const raced: number = won + lost;
  const total: number = raced + single;
  const raceShare: number = total > 0 ? raced / total : 0;
  const winShare: number = raced > 0 ? won / raced : 0;

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

/** Render the 4-item charts grid (throughput, latency, status codes, race). */
export function renderChartsGrid(
  snapshot: Snapshot | null,
  hasResponses: boolean,
): TemplateResult {
  return html`<div class="home-charts-grid">
    ${renderChartCard(t("home.chart.throughput"), t("home.chart.throughput.subtitle"), "chart-throughput", snapshot ? `${formatCompact(snapshot.requestsPerSec * 60)} rpm` : "—")}
    ${renderChartCard(t("home.chart.latency"), t("home.chart.latency.subtitle"), "chart-latency", snapshot && hasResponses ? `p95 ${formatLatency(snapshot.p95LatencyMs)}` : "—")}
    ${renderChartCard(t("home.chart.status_codes"), t("home.chart.status_codes.subtitle"), "chart-status-codes", snapshot && hasResponses ? `${(snapshot.successRate * 100).toFixed(1)}%` : "—")}
    ${renderRaceOutcomesCard(snapshot)}
  </div>`;
}

// ==========
// Window selector
// ==========

/** Window selector — segmented control with 1m / 5m / 30m buttons. */
export function renderWindowSelector(
  windowSecs: SnapshotWindow,
  onWindowChange: (newWindow: SnapshotWindow) => void,
): TemplateResult {
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

// ==========
// Chart lifecycle (create / destroy / push data)
// ==========

/** Create the 3 main uPlot charts + ResizeObservers. Returns charts and
 *  a disposer array. Called after the first lit-html render. */
export function createMainCharts(): {
  charts: uPlot[];
  disposers: Array<() => void>;
} {
  const throughputEl: HTMLElement | null = document.getElementById("chart-throughput");
  const statusEl: HTMLElement | null = document.getElementById("chart-status-codes");
  const latencyEl: HTMLElement | null = document.getElementById("chart-latency");

  if (!throughputEl || !statusEl || !latencyEl) {
    return { charts: [], disposers: [] };
  }

  const disposers: Array<() => void> = [];
  const throughput: uPlot = buildThroughputChart(throughputEl);
  disposers.push(observeResize(throughput, throughputEl));

  const statusCodes: uPlot = buildStatusCodesChart(statusEl);
  disposers.push(observeResize(statusCodes, statusEl));

  const latency: uPlot = buildLatencyChart(latencyEl);
  disposers.push(observeResize(latency, latencyEl));

  return { charts: [throughput, statusCodes, latency], disposers };
}

/** Push snapshot data into the 3 main uPlot charts. */
export function pushMainChartData(
  mainCharts: uPlot[],
  snapshot: Snapshot,
): void {
  const throughput = mainCharts[0];
  const statusCodes = mainCharts[1];
  const latency = mainCharts[2];
  if (!throughput || !statusCodes || !latency) return;
  throughput.setData(throughputData(snapshot.throughput));
  statusCodes.setData(statusCodesData(snapshot.statusCodes));
  latency.setData(latencyData(snapshot.latency));
}
