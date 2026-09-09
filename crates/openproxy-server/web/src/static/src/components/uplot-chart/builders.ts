// components/uplot-chart/builders.ts
// ==========
// Chart builders — one per chart on the home dashboard and the
// analytics view. Each builder returns a uPlot instance configured
// for a specific data shape. Internal helpers (`timeFormatter`,
// `formatRate`, `compactValue`, `dateFormatter`, ...) are shared
// across builders but kept module-private: they are not part of the
// public API surface.
//
// Public API: `buildThroughputChart`, `buildStatusCodesChart`,
// `buildLatencyChart`, `buildDailyUsageChart`.

import uPlot from "uplot";

import { CHART_COLORS, cssVar } from "./colors.js";
import {
  createLiveChart,
  smoothPath,
} from "./lifecycle.js";

// ----------
// Axis formatters & value formatters — used by the home-view builders
// ----------

/** X-axis ticks formatter for time-series charts. uPlot's default is fine
 *  but we override to show "HH:MM:SS" (the live dashboard is about recent
 *  activity, not dates). */
function timeFormatter(u: uPlot, vals: number[]): string[] {
  const scale = u.scales["x"];
  const span: number = (scale?.max ?? 0) - (scale?.min ?? 0);
  return vals.map((v: number) => {
    if (!Number.isFinite(v)) return "";
    const d: Date = new Date(v * 1000);
    const hh: string = String(d.getHours()).padStart(2, "0");
    const mm: string = String(d.getMinutes()).padStart(2, "0");
    if (span <= 300) {
      const ss: string = String(d.getSeconds()).padStart(2, "0");
      return `${hh}:${mm}:${ss}`;
    }
    return `${hh}:${mm}`;
  });
}

function formatRate(_u: uPlot, raw: number): string {
  if (!Number.isFinite(raw)) return "";
  return compactValue(raw) + "/s";
}

function formatCount(_u: uPlot, raw: number): string {
  return Number.isFinite(raw) ? compactValue(raw) : "";
}

function formatDuration(_u: uPlot, raw: number): string {
  if (!Number.isFinite(raw)) return "";
  return raw >= 1000 ? (raw / 1000).toFixed(2) + "s" : Math.round(raw) + "ms";
}

function compactValue(v: number): string {
  if (Math.abs(v) >= 1_000_000) return (v / 1_000_000).toFixed(1) + "M";
  if (Math.abs(v) >= 1_000) return (v / 1_000).toFixed(1) + "k";
  if (Number.isInteger(v)) return String(v);
  return v.toFixed(2);
}

/** Number formatter for axis ticks. Uses compact notation (1.2k, 8.2k). */
function compactNumber(_u: uPlot, vals: number[]): string[] {
  return vals.map((v: number) => Number.isFinite(v) ? compactValue(v) : "");
}

function positiveRange(_u: uPlot, _min: number, max: number): [number, number] {
  if (!Number.isFinite(max) || max <= 0) return [0, 1];
  return [0, max * 1.05];
}

// ----------
// Home dashboard builders
// ----------

/** Requests and tokens per second on independent Y axes. */
export function buildThroughputChart(container: HTMLElement): uPlot {
  return createLiveChart(container, {
    series: [
      { label: "Time" },
      {
        label: "Requests",
        stroke: CHART_COLORS.blue,
        width: 2,
        paths: smoothPath,
        points: { show: false },
        scale: "rps",
        value: formatRate,
      },
      {
        label: "Tokens",
        stroke: CHART_COLORS.green,
        width: 2,
        paths: smoothPath,
        points: { show: false },
        scale: "tps",
        value: formatRate,
      },
    ],
    scales: {
      x: { time: true },
      rps: { auto: true, range: positiveRange },
      tps: { auto: true, range: positiveRange },
    },
    axes: [
      {
        grid: { stroke: cssVar("--color-border-soft"), width: 1 },
        ticks: { stroke: cssVar("--color-border"), width: 1 },
        stroke: cssVar("--color-text-muted"),
        font: "10px 'Courier New', monospace",
        values: timeFormatter,
      },
      {
        scale: "rps",
        side: 3, // left
        grid: { show: false },
        stroke: CHART_COLORS.blue,
        font: "10px 'Courier New', monospace",
        values: compactNumber,
        size: 44,
      },
      {
        scale: "tps",
        side: 1, // right
        grid: { show: false },
        stroke: CHART_COLORS.green,
        font: "10px 'Courier New', monospace",
        values: compactNumber,
        size: 52,
      },
    ],
  });
}

/** Successful, client-error, and server-error responses per bucket. */
export function buildStatusCodesChart(container: HTMLElement): uPlot {
  return createLiveChart(container, {
    series: [
      { label: "Time" },
      {
        label: "Successful",
        stroke: CHART_COLORS.status2xx,
        fill: "rgba(22, 163, 74, 0.12)",
        width: 2,
        paths: smoothPath,
        points: { show: false },
        value: formatCount,
      },
      {
        label: "Client error",
        stroke: CHART_COLORS.status4xx,
        width: 2,
        paths: smoothPath,
        points: { show: false },
        value: formatCount,
      },
      {
        label: "Server error",
        stroke: CHART_COLORS.status5xx,
        width: 2,
        paths: smoothPath,
        points: { show: false },
        value: formatCount,
      },
    ],
    scales: {
      x: { time: true },
      y: { auto: true, range: positiveRange },
    },
    axes: [
      {
        grid: { stroke: cssVar("--color-border-soft"), width: 1 },
        ticks: { stroke: cssVar("--color-border"), width: 1 },
        stroke: cssVar("--color-text-muted"),
        font: "10px 'Courier New', monospace",
        values: timeFormatter,
      },
      {
        side: 3,
        grid: { show: false },
        stroke: cssVar("--color-text-muted"),
        font: "10px 'Courier New', monospace",
        values: compactNumber,
        size: 40,
      },
    ],
  });
}

/** Latency chart: 3 line series (p50 / p95 / p99) in milliseconds, single
 *  shared Y-axis. */
export function buildLatencyChart(container: HTMLElement): uPlot {
  return createLiveChart(container, {
    series: [
      { label: "Time" },
      {
        label: "p50",
        stroke: CHART_COLORS.blue,
        width: 2,
        paths: smoothPath,
        points: { show: false },
        fill: "rgba(37, 99, 235, 0.10)",
        value: formatDuration,
      },
      {
        label: "p95",
        stroke: CHART_COLORS.orange,
        width: 2,
        paths: smoothPath,
        points: { show: false },
        value: formatDuration,
      },
      {
        label: "p99",
        stroke: CHART_COLORS.red,
        width: 2,
        paths: smoothPath,
        points: { show: false },
        value: formatDuration,
      },
    ],
    scales: {
      x: { time: true },
      y: { auto: true, range: positiveRange },
    },
    axes: [
      {
        grid: { stroke: cssVar("--color-border-soft"), width: 1 },
        ticks: { stroke: cssVar("--color-border"), width: 1 },
        stroke: cssVar("--color-text-muted"),
        font: "10px 'Courier New', monospace",
        values: timeFormatter,
      },
      {
        side: 3,
        grid: { show: false },
        stroke: cssVar("--color-text-muted"),
        font: "10px 'Courier New', monospace",
        values: (_u: uPlot, vals: number[]): string[] => {
          return vals.map((v: number) => {
            if (!Number.isFinite(v)) return "";
            if (v >= 1000) return (v / 1000).toFixed(1) + "s";
            return Math.round(v) + "ms";
          });
        },
        size: 40,
      },
    ],
  });
}

// ----------
// Analytics-view builder (B3)
// ----------

/** Compact, unambiguous UTC dates. Includes the year for long ranges. */
function dateFormatter(u: uPlot, vals: number[]): string[] {
  const scale = u.scales["x"];
  const spanDays: number = ((scale?.max ?? 0) - (scale?.min ?? 0)) / 86_400;
  let previous = "";
  return vals.map((v: number) => {
    if (!Number.isFinite(v)) return "";
    const d: Date = new Date(v * 1000);
    if (Number.isNaN(d.getTime())) return "";
    const label = new Intl.DateTimeFormat(undefined, spanDays > 370
      ? { month: "short", year: "2-digit", timeZone: "UTC" }
      : { month: "short", day: "numeric", timeZone: "UTC" }).format(d);
    if (label === previous) return "";
    previous = label;
    return label;
  });
}

/** Cost formatter for the right Y-axis of the daily-usage chart. Uses
 *  3 decimal places for sub-dollar amounts (typical for daily cost) and
 *  2 decimals for ≥ $1. */
function costAxisFormatter(_u: uPlot, vals: number[]): string[] {
  return vals.map((v: number) => {
    if (!Number.isFinite(v)) return "";
    if (v >= 1) return "$" + v.toFixed(1);
    if (v > 0) return "$" + v.toFixed(3);
    return "$0";
  });
}

/** Daily requests, errors, and cost on two Y axes. */
export function buildDailyUsageChart(container: HTMLElement): uPlot {
  return createLiveChart(container, {
    series: [
      { label: "Date" },
      {
        label: "Requests",
        stroke: CHART_COLORS.blue,
        width: 2,
        points: { show: true, size: 5, width: 2 },
        fill: "rgba(37, 99, 235, 0.10)",
        scale: "reqs",
        value: formatCount,
      },
      {
        label: "Errors",
        stroke: CHART_COLORS.red,
        width: 2,
        points: { show: true, size: 5, width: 2 },
        scale: "reqs",
        value: formatCount,
      },
      {
        label: "Cost",
        stroke: CHART_COLORS.orange,
        width: 2,
        points: { show: true, size: 5, width: 2 },
        scale: "cost",
        value: (_u: uPlot, raw: number): string => Number.isFinite(raw) ? "$" + raw.toFixed(4) : "",
      },
    ],
    scales: {
      x: { time: true },
      reqs: { auto: true, range: positiveRange },
      cost: { auto: true, range: positiveRange },
    },
    axes: [
      {
        grid: { stroke: cssVar("--color-border-soft"), width: 1 },
        ticks: { stroke: cssVar("--color-border"), width: 1 },
        stroke: cssVar("--color-text-muted"),
        font: "10px 'Courier New', monospace",
        values: dateFormatter,
      },
      {
        scale: "reqs",
        side: 3, // left
        grid: { show: false },
        stroke: CHART_COLORS.blue,
        font: "10px 'Courier New', monospace",
        values: compactNumber,
        size: 40,
      },
      {
        scale: "cost",
        side: 1, // right
        grid: { show: false },
        stroke: CHART_COLORS.orange,
        font: "10px 'Courier New', monospace",
        values: costAxisFormatter,
        size: 44,
      },
    ],
  });
}
