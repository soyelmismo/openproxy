// views/home/kpis.ts — KPI tile grid + sparkline builders.
//
// Renders the 6 KPI tiles (active requests, requests/min, p95 latency,
// success rate, tokens/min, cost/min) and provides sparkline data
// constructors for the mini uPlot charts inside each tile.

import { html, type TemplateResult } from "lit-html";
import type uPlot from "uplot";

import { t } from "../../i18n/index.js";
import type { Snapshot, SnapshotWindow } from "./types.js";
import type { ThroughputPoint, StatusCodePoint, LatencyPoint } from "../../state/live-store.js";
import { createSparkline, observeResize, CHART_COLORS } from "../../components/uplot-chart/index.js";

// ==========
// Formatters
// ==========

/** Compact number formatter: 1234 → "1.2k", 12345 → "12.3k", 1234567 → "1.2M". */
export function formatCompact(n: number): string {
  if (!Number.isFinite(n)) return "0";
  if (n < 1000) return String(Math.round(n));
  if (n < 10000) return (n / 1000).toFixed(1) + "k";
  if (n < 1_000_000) return Math.round(n / 1000) + "k";
  return (n / 1_000_000).toFixed(1) + "M";
}

/** Currency formatter for the Cost/min KPI tile. */
export function formatCostPerMin(usdPerMin: number): string {
  if (!Number.isFinite(usdPerMin) || usdPerMin <= 0) return "$0.00";
  if (usdPerMin < 0.01) return "$" + usdPerMin.toFixed(4);
  if (usdPerMin < 1) return "$" + usdPerMin.toFixed(3);
  return "$" + usdPerMin.toFixed(2);
}

/** Latency formatter: 1234ms → "1.2s", 850ms → "850ms". */
export function formatLatency(ms: number | null | undefined): string {
  if (ms == null || !Number.isFinite(ms)) return "—";
  if (ms >= 1000) return (ms / 1000).toFixed(1) + "s";
  return Math.round(ms) + "ms";
}

/** Cost formatter: 0.0023 → "$0.0023". */
export function formatCost(usd: number | null | undefined): string {
  if (usd == null || !Number.isFinite(usd)) return "—";
  if (usd < 0.01) return "$" + usd.toFixed(4);
  if (usd < 1) return "$" + usd.toFixed(3);
  return "$" + usd.toFixed(2);
}

/** Token count formatter: in/out as "1.2k/850". */
export function formatTokensInOut(inTok: number | null, outTok: number | null): string {
  const i: string = inTok != null ? formatCompact(inTok) : "0";
  const o: string = outTok != null ? formatCompact(outTok) : "0";
  return i + "/" + o;
}

// ==========
// KPI tile rendering
// ==========

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

export function renderKpiGrid(
  snapshot: Snapshot | null,
  windowSecs: SnapshotWindow,
): TemplateResult {
  const status = snapshot?.statusCodes.reduce((acc, point) => {
    acc.ok += point.s2xx;
    acc.errors += point.s4xx + point.s5xx;
    return acc;
  }, { ok: 0, errors: 0 }) ?? { ok: 0, errors: 0 };
  const responses: number = status.ok + status.errors;
  const bucketSecs: number = snapshot && snapshot.throughput.length > 0
    ? windowSecs / snapshot.throughput.length
    : 1;
  const windowRequests: number = snapshot?.throughput.reduce(
    (sum, point) => sum + point.rps * bucketSecs, 0,
  ) ?? 0;
  const success: string = responses > 0
    ? `${(status.ok / responses * 100).toFixed(1)}%`
    : "—";
  const successTone: string = responses === 0
    ? ""
    : status.ok / responses >= 0.99
    ? "is-good"
    : status.ok / responses >= 0.95
    ? "is-warn"
    : "is-bad";

  return html`<div class="home-kpi-grid">
    ${renderKpiTile(t("home.kpi.active_requests"), snapshot ? String(snapshot.activeRequests) : "—", t("home.kpi.in_flight"), null)}
    ${renderKpiTile(t("home.kpi.requests_per_min"), snapshot ? formatCompact(snapshot.requestsPerSec * 60) : "—", `${formatCompact(windowRequests)} ${t("home.kpi.in_window")}`, "spark-requests")}
    ${renderKpiTile(t("home.kpi.p95_latency"), snapshot && responses > 0 ? formatLatency(snapshot.p95LatencyMs) : "—", `${t("home.kpi.p50")} ${snapshot && responses > 0 ? formatLatency(snapshot.p50LatencyMs) : "—"}`, "spark-latency")}
    ${renderKpiTile(t("home.kpi.success_rate"), success, `${formatCompact(status.errors)} ${t("home.kpi.failed")}`, "spark-success", successTone)}
    ${renderKpiTile(t("home.kpi.tokens_per_min"), snapshot ? formatCompact(snapshot.tokensPerSec * 60) : "—", t("home.kpi.rolling_rate"), "spark-tokens")}
    ${renderKpiTile(t("home.kpi.cost_per_min"), snapshot ? formatCostPerMin(snapshot.costPerSec * 60) : "—", `${formatCostPerMin((snapshot?.costPerSec ?? 0) * 3600)} ${t("home.kpi.per_hour")}`, "spark-cost")}
  </div>`;
}

// ==========
// Sparkline creation (uPlot instances)
// ==========

const SPARKLINE_BUCKETS = 60;

export interface SparklineInstances {
  sparkRequests: uPlot;
  sparkSuccess: uPlot;
  sparkLatency: uPlot;
  sparkTokens: uPlot;
  sparkCost: uPlot;
}

/** Create the 5 KPI sparkline uPlot charts + ResizeObservers. Returns
 *  sparklines and a disposer array. Called after the first lit-html render. */
export function createSparklines(): {
  sparklines: SparklineInstances | null;
  disposers: Array<() => void>;
} {
  const sparkReqEl: HTMLElement | null = document.getElementById("spark-requests");
  const sparkSuccessEl: HTMLElement | null = document.getElementById("spark-success");
  const sparkLatencyEl: HTMLElement | null = document.getElementById("spark-latency");
  const sparkTokEl: HTMLElement | null = document.getElementById("spark-tokens");
  const sparkCostEl: HTMLElement | null = document.getElementById("spark-cost");

  if (!sparkReqEl || !sparkSuccessEl || !sparkLatencyEl || !sparkTokEl || !sparkCostEl) {
    return { sparklines: null, disposers: [] };
  }

  const s: SparklineInstances = {
    sparkRequests: createSparkline(sparkReqEl, CHART_COLORS.blue),
    sparkSuccess: createSparkline(sparkSuccessEl, CHART_COLORS.green),
    sparkLatency: createSparkline(sparkLatencyEl, CHART_COLORS.orange),
    sparkTokens: createSparkline(sparkTokEl, CHART_COLORS.green),
    sparkCost: createSparkline(sparkCostEl, CHART_COLORS.orange),
  };

  const disposers: Array<() => void> = [
    observeResize(s.sparkRequests, sparkReqEl),
    observeResize(s.sparkSuccess, sparkSuccessEl),
    observeResize(s.sparkLatency, sparkLatencyEl),
    observeResize(s.sparkTokens, sparkTokEl),
    observeResize(s.sparkCost, sparkCostEl),
  ];

  return { sparklines: s, disposers };
}

// ==========
// Sparkline data builders
// ==========

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
    xs[i] = i;
    ys[i] = p[field];
  }
  return [xs, ys];
}

function successSparkline(points: StatusCodePoint[]): uPlot.AlignedData {
  const xs: number[] = new Array<number>(points.length);
  const ys: Array<number | null> = new Array<number | null>(points.length);
  for (let i = 0; i < points.length; i++) {
    const p: StatusCodePoint = points[i]!;
    const total: number = p.s2xx + p.s4xx + p.s5xx;
    xs[i] = i;
    ys[i] = total > 0 ? p.s2xx / total * 100 : null;
  }
  return [xs, ys];
}

function latencySparkline(points: LatencyPoint[]): uPlot.AlignedData {
  return [points.map((_, i) => i), points.map((point) => point.p95 || null)];
}

/** Push snapshot data into the KPI sparkline uPlot instances. */
export function pushSparklineData(
  s: SparklineInstances,
  snapshot: Snapshot,
): void {
  const last60: ThroughputPoint[] = snapshot.throughput.slice(-SPARKLINE_BUCKETS);
  s.sparkRequests.setData(sparklineData(last60, "rps", SPARKLINE_BUCKETS));
  s.sparkSuccess.setData(successSparkline(snapshot.statusCodes.slice(-SPARKLINE_BUCKETS)));
  s.sparkLatency.setData(latencySparkline(snapshot.latency.slice(-SPARKLINE_BUCKETS)));
  s.sparkTokens.setData(sparklineData(last60, "tps", SPARKLINE_BUCKETS));
  s.sparkCost.setData(sparklineData(last60, "cps", SPARKLINE_BUCKETS));
}
