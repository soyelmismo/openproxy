// components/uplot-chart.ts
// ============================================================================
// F6.2: Thin wrapper around uPlot for the live dashboard (F6).
//
// uPlot (https://github.com/leeoniya/uPlot) is a ~50 KB minified time-series
// chart library with zero dependencies. It's the standard choice for live
// time-series charts in 2024+: considerably faster than Chart.js for high-
// frequency updates, and far smaller than ECharts.
//
// This wrapper provides:
//   - `createLiveChart(container, opts)` — create a uPlot instance with
//     sensible defaults for live data (no legend, no select, no drag cursor).
//   - `createSparkline(container, color)` — minimal uPlot for KPI tile
//     sparklines (no axes, no legend, no cursor).
//   - `observeResize(u, container)` — ResizeObserver-driven sizing so the
//     chart re-flows when its container changes width (responsive grid).
//   - `injectUplotCss()` — idempotent injection of uPlot's stylesheet via a
//     <style> tag so we don't need a separate <link> in index.html.
//   - `CHART_COLORS` — palette aligned with the project's design tokens
//     (--color-info, --color-success, --color-warn, --color-error, ...).
//   - Helper builders for the 4 chart types the home view uses
//     (`buildThroughputChart`, `buildStatusCodesChart`, `buildLatencyChart`).
//
// Lifecycle
// ---------
// uPlot instances are created ONCE when the home view first mounts, then
// `setData(...)` is called on each throttled re-render (max 4 Hz) to push
// new points. We never recreate the chart — `setData` is O(n) where n is
// the number of points (≤ 1800 for a 30-min window at 5-second resolution).
//
// Resize handling: a single ResizeObserver per chart watches the container
// element. On size change, `u.setSize({ width, height })` is called, which
// triggers uPlot's internal re-layout (cheap — it doesn't recreate the
// canvas, just resizes it and redraws).
//
// CSS injection: uPlot's CSS (axis labels, legend, cursor crosshair, grid)
// is loaded as a string via the `text` loader (see build.mjs) and injected
// into a <style> tag at first chart creation. Idempotent — subsequent calls
// are no-ops. This avoids shipping a separate .css file alongside app.js.
// ============================================================================

import uPlot from "uplot";
import uplotCss from "uplot/dist/uPlot.min.css";

// ----------------------------------------------------------------------------
// CSS variable resolver
// ----------------------------------------------------------------------------
//
// TRIPLE-FIX (Bug 2): uPlot renders axis text + grid + ticks on a Canvas
// 2D context (`ctx.fillText`, `ctx.strokeLine` etc.). Canvas 2D
// `fillStyle` / `strokeStyle` do NOT resolve CSS variables — passing
// `"var(--color-text-muted)"` is silently ignored and the canvas falls
// back to its default (black `#000000`). This made every axis label,
// tick value, and gridline render as black text/lines in dark mode,
// where the chart background is dark — visually invisible.
//
// The fix: resolve the CSS variables to actual color strings at chart
// creation time via `getComputedStyle(document.documentElement)`.
// Charts are created ONCE per view mount (see lifecycle note above),
// so the colors are read at mount time. If the user toggles the theme
// (light ↔ dark) while a chart is on screen, the chart's colors will
// NOT update until the view re-mounts. This is an acceptable MVP
// limitation — a future iteration could observe `theme-toggle` events
// and call `u.axes[i].stroke = cssVar(...)` + `u.redraw()` on every
// existing chart. Documented here so a future maintainer knows.

/** Resolve a CSS custom property to its computed value at call time.
 *  Returns a sensible dark-grey fallback if `window` is unavailable
 *  (SSR) or the property is not defined. */
function cssVar(name: string): string {
  if (typeof window === "undefined") return "#5a5a5a";
  const v: string = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || "#5a5a5a";
}

// ----------------------------------------------------------------------------
// CSS injection
// ----------------------------------------------------------------------------

let cssInjected = false;

/** Inject uPlot's stylesheet into <head> as a <style> tag. Idempotent. */
export function injectUplotCss(): void {
  if (cssInjected) return;
  if (typeof document === "undefined") return;
  const style: HTMLStyleElement = document.createElement("style");
  style.setAttribute("data-uplot-css", "");
  style.textContent = uplotCss;
  document.head.appendChild(style);
  cssInjected = true;
}

// ----------------------------------------------------------------------------
// Palette — aligned with tokens.css / themes.css
// ----------------------------------------------------------------------------

/** Chart series colors. Kept in sync with the design tokens; if the theme
 *  changes these colors, the charts pick them up on the next mount (the
 *  values are read at module load, NOT at theme-change time — for live
 *  theme switching we'd need to read CSS custom properties via
 *  `getComputedStyle(document.documentElement)`. Out of scope for F6. */
export const CHART_COLORS = {
  blue: "#2563eb",
  green: "#16a34a",
  orange: "#ea580c",
  red: "#dc2626",
  purple: "#7c3aed",
  gray: "#64748b",
  status2xx: "#16a34a",
  status4xx: "#f59e0b",
  status5xx: "#dc2626",
} as const;

// ----------------------------------------------------------------------------
// Types
// ----------------------------------------------------------------------------

/** A data series for uPlot. The first array is the X-axis (timestamps in
 *  seconds since epoch); subsequent arrays are the Y values per series.
 *  Matches uPlot's `AlignedData` type. */
export type ChartData = uPlot.AlignedData;

/** Input shape for `createLiveChart`. The caller provides series, scales,
 *  and axes — the wrapper fills in the rest (legend, cursor, select).
 *
 *  We use the concrete `uPlot.Series[]` / `uPlot.Scales` / `uPlot.Axis[]`
 *  types (not `uPlot.Options["series"]` etc.) because the Options-keyed
 *  variants are `T | undefined` (the Options interface marks them
 *  optional). The strict `exactOptionalPropertyTypes` tsconfig flag then
 *  rejects assigning `undefined` back to the optional `Options.series` /
 *  `Options.scales` / `Options.axes` fields. Using the concrete types
 *  sidesteps that — these properties are always defined when we build the
 *  final Options object. */
export interface LiveChartOpts {
  series: uPlot.Series[];
  scales: uPlot.Scales;
  axes: uPlot.Axis[];
  initialData?: ChartData;
  legend?: uPlot.Legend;
}

// ----------------------------------------------------------------------------
// Live chart (full-size, axes visible)
// ----------------------------------------------------------------------------

/** Sensible defaults shared by all full-size charts. Keeps a live legend
 *  for exact hover values while disabling selection and drag-to-zoom.
 *
 *  We construct the full Options object in one go (rather than mutating
 *  a base) because the strict tsconfig has `exactOptionalPropertyTypes` —
 *  assigning `undefined` to an optional property is an error. */
function buildOptions(
  width: number,
  height: number,
  series: uPlot.Series[],
  scales: uPlot.Scales,
  axes: uPlot.Axis[],
  legend: uPlot.Legend,
): uPlot.Options {
  return {
    width,
    height,
    legend,
    cursor: {
      drag: { x: false, y: false },
      // Keep the cursor focus ring (shows X/Y values on hover) — useful
      // for inspecting a specific point in time. No drag-to-zoom.
    },
    // `Select` extends `BBox` which requires left/top/width/height —
    // we provide zeros alongside `show: false` to satisfy the type.
    select: { show: false, left: 0, top: 0, width: 0, height: 0 },
    padding: [34, 10, 2, 4],
    series,
    scales,
    axes,
  };
}

/** Create a uPlot instance for live time-series data. The series / scales /
 *  axes config is passed in by the caller (the home view uses the
 *  `buildThroughputChart` / `buildStatusCodesChart` / `buildLatencyChart`
 *  helpers below to construct these).
 *
 *  The chart starts empty (`[[]]` data) and is populated via `setData(...)`
 *  on each throttled re-render. We never recreate the chart — `setData`
 *  is the only mutation path. */
export function createLiveChart(container: HTMLElement, opts: LiveChartOpts): uPlot {
  injectUplotCss();
  // CRITICAL: read the container's ACTUAL size, not a fallback.
  // If clientWidth is 0 (container not yet laid out by the browser),
  // defer creation by one animation frame so the layout has a chance
  // to compute. Creating a uPlot with a 600px default when the
  // container is actually 400px causes the canvas to overflow, which
  // pushes the layout wider, which triggers ResizeObserver, which
  // calls setSize with the new (larger) width — the chart-grows-
  // without-bound bug.
  const w: number = Math.max(100, container.clientWidth);
  const h: number = Math.max(80, container.clientHeight || 200);
  const data: ChartData = opts.initialData ?? [[]];
  const u: uPlot = new uPlot(
    buildOptions(
      w,
      h,
      opts.series,
      opts.scales,
      opts.axes,
      opts.legend ?? { show: true, live: true },
    ),
    data,
    container,
  );
  // Force a resize on the next frame — the container may have been
  // laid out between the `clientWidth` read above and now. This also
  // catches the case where uPlot's own CSS (`.uplot { width: min-content }`,
  // overridden by our global `.uplot { width: 100% !important }`)
  // causes a transient size mismatch on first paint.
  requestAnimationFrame(() => {
    resizeChart(u, container);
  });
  return u;
}

// ----------------------------------------------------------------------------
// Sparkline (minimal — no axes, no legend, no cursor)
// ----------------------------------------------------------------------------

/** Create a tiny sparkline uPlot for KPI tile thumbnails. Single series,
 *  no axes, no legend, no cursor — just the line. The caller populates it
 *  via `setData([[xs...], [ys...]])` on each re-render.
 *
 *  The X values can be anything (we use indices 0..n-1); the X axis is
 *  hidden, so the scale doesn't matter. */
export function createSparkline(container: HTMLElement, color: string): uPlot {
  injectUplotCss();
  const w: number = Math.max(40, container.clientWidth);
  const h: number = Math.max(16, container.clientHeight || 24);
  const opts: uPlot.Options = {
    width: w,
    height: h,
    legend: { show: false },
    cursor: { show: false },
    select: { show: false, left: 0, top: 0, width: 0, height: 0 },
    padding: [0, 0, 0, 0],
    series: [
      {}, // X-axis (hidden)
      {
        stroke: color,
        width: 1,
        points: { show: false },
      },
    ],
    scales: {
      x: { time: false },
      // Pin the Y range to [min, max] but always at least 1 unit tall so
      // a flat series (e.g. all zeros) still renders a visible line.
      y: {
        auto: true,
        range: (_u: uPlot, min: number, max: number): [number, number] => {
          if (!Number.isFinite(min) || !Number.isFinite(max)) return [0, 1];
          if (max === min) return [min, min + 1];
          return [min, max];
        },
      },
    },
    axes: [
      { show: false },
      { show: false },
    ],
  };
  const u: uPlot = new uPlot(opts, [[]], container);
  // Same rAF resize as createLiveChart — see that function for the
  // rationale (container may not be laid out yet at creation time).
  requestAnimationFrame(() => {
    resizeChart(u, container);
  });
  return u;
}

// ----------------------------------------------------------------------------
// Resize handling
// ----------------------------------------------------------------------------

/** Resize a uPlot instance to fill its container. Called on initial mount
 *  and on every ResizeObserver callback. Cheap — uPlot's `setSize` just
 *  resizes the canvas and redraws; it doesn't recreate the chart. */
export function resizeChart(u: uPlot, container: HTMLElement): void {
  // Read the container's CONTENT width (excluding padding) via
  // clientWidth. If the container has padding, clientWidth is already
  // the inner size. If clientWidth is 0 (container display:none or
  // not laid out), skip — the ResizeObserver will fire again when
  // the container becomes visible.
  const w: number = container.clientWidth;
  const h: number = container.clientHeight;
  if (w === 0 || h === 0) return;
  const cw: number = Math.max(100, w);
  const ch: number = Math.max(80, h);
  // Guard against no-op resizes (uPlot triggers a full redraw on every
  // setSize call, even if the size hasn't changed).
  if (u.width === cw && u.height === ch) return;
  u.setSize({ width: cw, height: ch });
}

/** Attach a ResizeObserver that keeps the chart sized to its container.
 *  Returns a disposer — call it on view unmount to release the observer.
 *
 *  Falls back to `window.resize` if ResizeObserver is unavailable (very
 *  old browsers — uPlot's targets are evergreen, so this is defensive).
 *
 *  DEBOUNCE: ResizeObserver can fire in a tight loop if the chart's
 *  own setSize() triggers a container reflow (which re-fires the
 *  observer). We debounce with a requestAnimationFrame coalescer + a
 *  guard in `resizeChart` that skips the call if the size hasn't
 *  actually changed. Without this, the chart's canvas reflowing the
 *  container could re-fire the observer 60+ times/sec.
 *
 *  INITIAL SIZING PASS: `ro.observe(container)` causes the observer
 *  to fire once asynchronously with the container's current size.
 *  That fire is enough in the common case, but it can deliver a 0x0
 *  size if the container is briefly `display:none` or not yet laid
 *  out at observe() time (the chart is then stuck at the fallback
 *  600x200 from `createLiveChart` until the next real resize event).
 *  We schedule an explicit `resizeChart()` via rAF right after
 *  observe() so the chart self-corrects on the next frame even if
 *  the observer's initial fire is delayed or delivers 0x0. The
 *  `resizeChart` no-op guard makes this redundant in the happy path
 *  but costs nothing. */
export function observeResize(u: uPlot, container: HTMLElement): () => void {
  if (typeof ResizeObserver === "undefined") {
    const handler = (): void => resizeChart(u, container);
    window.addEventListener("resize", handler);
    // Initial sizing pass for the no-ResizeObserver fallback.
    requestAnimationFrame(handler);
    return () => window.removeEventListener("resize", handler);
  }
  let rafId: number | null = null;
  const scheduleResize = (): void => {
    if (rafId !== null) return;
    rafId = requestAnimationFrame(() => {
      rafId = null;
      resizeChart(u, container);
    });
  };
  const ro: ResizeObserver = new ResizeObserver(scheduleResize);
  ro.observe(container);
  // Explicit initial sizing pass — see the docstring above. Uses the
  // same rAF coalescer so it merges with any observer fire that
  // happened to land first.
  scheduleResize();
  return () => {
    if (rafId !== null) {
      cancelAnimationFrame(rafId);
      rafId = null;
    }
    ro.disconnect();
  };
}

// ----------------------------------------------------------------------------
// Chart builders — one per chart on the home dashboard
// ----------------------------------------------------------------------------

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

/** Requests and tokens per second on independent Y axes. */
export function buildThroughputChart(container: HTMLElement): uPlot {
  return createLiveChart(container, {
    series: [
      { label: "Time" },
      {
        label: "Requests",
        stroke: CHART_COLORS.blue,
        width: 2,
        points: { show: false },
        scale: "rps",
        value: formatRate,
      },
      {
        label: "Tokens",
        stroke: CHART_COLORS.green,
        width: 2,
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
        size: 40,
      },
      {
        scale: "tps",
        side: 1, // right
        grid: { show: false },
        stroke: CHART_COLORS.green,
        font: "10px 'Courier New', monospace",
        values: compactNumber,
        size: 40,
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
        points: { show: false },
        value: formatCount,
      },
      {
        label: "Client error",
        stroke: CHART_COLORS.status4xx,
        width: 2,
        points: { show: false },
        value: formatCount,
      },
      {
        label: "Server error",
        stroke: CHART_COLORS.status5xx,
        width: 2,
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
        points: { show: false },
        fill: "rgba(37, 99, 235, 0.10)",
        value: formatDuration,
      },
      {
        label: "p95",
        stroke: CHART_COLORS.orange,
        width: 2,
        points: { show: false },
        value: formatDuration,
      },
      {
        label: "p99",
        stroke: CHART_COLORS.red,
        width: 2,
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

// ----------------------------------------------------------------------------
// Builders used by the analytics view (B3)
// ----------------------------------------------------------------------------

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
