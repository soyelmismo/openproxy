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
  blue: "#2b5a78",   // --color-info (sky dark)
  green: "#4d7c2a",  // --color-success (sage)
  orange: "#a86a00", // --color-warn (peach dark)
  red: "#b21f1f",    // --color-error (salmon dark)
  purple: "#6a26a4", // --c-purple-stripe
  gray: "#5a5a5a",   // --color-text-muted
  // Semantic for status codes
  status2xx: "#4d7c2a",  // green
  status4xx: "#a86a00",  // orange
  status5xx: "#b21f1f",  // red
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
}

// ----------------------------------------------------------------------------
// Live chart (full-size, axes visible)
// ----------------------------------------------------------------------------

/** Sensible defaults shared by all full-size charts. Disables the legend
 *  (we render our own), the select box (zoom is overkill for live data),
 *  and the drag cursor (panning would scroll the page).
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
): uPlot.Options {
  return {
    width,
    height,
    legend: { show: false },
    cursor: {
      drag: { x: false, y: false },
      // Keep the cursor focus ring (shows X/Y values on hover) — useful
      // for inspecting a specific point in time. No drag-to-zoom.
    },
    // `Select` extends `BBox` which requires left/top/width/height —
    // we provide zeros alongside `show: false` to satisfy the type.
    select: { show: false, left: 0, top: 0, width: 0, height: 0 },
    padding: [8, 8, 0, 0],
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
  const w: number = Math.max(100, container.clientWidth || 600);
  const h: number = Math.max(80, container.clientHeight || 200);
  const data: ChartData = opts.initialData ?? [[]];
  const u: uPlot = new uPlot(
    buildOptions(w, h, opts.series, opts.scales, opts.axes),
    data,
    container,
  );
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
  const w: number = Math.max(40, container.clientWidth || 100);
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
  return new uPlot(opts, [[]], container);
}

// ----------------------------------------------------------------------------
// Resize handling
// ----------------------------------------------------------------------------

/** Resize a uPlot instance to fill its container. Called on initial mount
 *  and on every ResizeObserver callback. Cheap — uPlot's `setSize` just
 *  resizes the canvas and redraws; it doesn't recreate the chart. */
export function resizeChart(u: uPlot, container: HTMLElement): void {
  const w: number = Math.max(100, container.clientWidth);
  const h: number = Math.max(80, container.clientHeight);
  // Guard against no-op resizes (uPlot triggers a full redraw on every
  // setSize call, even if the size hasn't changed).
  if (u.width === w && u.height === h) return;
  u.setSize({ width: w, height: h });
}

/** Attach a ResizeObserver that keeps the chart sized to its container.
 *  Returns a disposer — call it on view unmount to release the observer.
 *
 *  Falls back to `window.resize` if ResizeObserver is unavailable (very
 *  old browsers — uPlot's targets are evergreen, so this is defensive). */
export function observeResize(u: uPlot, container: HTMLElement): () => void {
  if (typeof ResizeObserver === "undefined") {
    const handler = (): void => resizeChart(u, container);
    window.addEventListener("resize", handler);
    return () => window.removeEventListener("resize", handler);
  }
  // Debounce isn't needed — ResizeObserver already batches callbacks per
  // frame. uPlot's redraw is cheap (sub-millisecond for ≤1800 points).
  const ro: ResizeObserver = new ResizeObserver(() => {
    resizeChart(u, container);
  });
  ro.observe(container);
  return () => ro.disconnect();
}

// ----------------------------------------------------------------------------
// Chart builders — one per chart on the home dashboard
// ----------------------------------------------------------------------------

/** X-axis ticks formatter for time-series charts. uPlot's default is fine
 *  but we override to show "HH:MM:SS" (the live dashboard is about recent
 *  activity, not dates). */
function timeFormatter(scaleSecs: number): (u: uPlot, vals: number[]) => string[] {
  // For short windows (1m, 5m), show seconds. For long windows (30m),
  // show minutes:seconds.
  return (_u: uPlot, vals: number[]): string[] => {
    return vals.map((v: number) => {
      if (!Number.isFinite(v)) return "";
      const d: Date = new Date(v * 1000);
      const hh: string = String(d.getHours()).padStart(2, "0");
      const mm: string = String(d.getMinutes()).padStart(2, "0");
      if (scaleSecs <= 300) {
        const ss: string = String(d.getSeconds()).padStart(2, "0");
        return `${hh}:${mm}:${ss}`;
      }
      return `${hh}:${mm}`;
    });
  };
}

/** Number formatter for axis ticks. Uses compact notation (1.2k, 8.2k). */
function compactNumber(_u: uPlot, vals: number[]): string[] {
  return vals.map((v: number) => {
    if (!Number.isFinite(v)) return "";
    if (Math.abs(v) >= 1_000_000) return (v / 1_000_000).toFixed(1) + "M";
    if (Math.abs(v) >= 1_000) return (v / 1_000).toFixed(1) + "k";
    if (Number.isInteger(v)) return String(v);
    return v.toFixed(1);
  });
}

/** uPlot's bar path builder, or null if unavailable (the property is
 *  optional in the type defs). We null-check before assigning to a
 *  series' `paths` field (which is non-optional under
 *  `exactOptionalPropertyTypes`). */
function barsPathBuilder(): uPlot.Series.PathBuilder | null {
  const factory: uPlot.Series.BarsPathBuilderFactory | undefined = uPlot.paths.bars;
  if (!factory) return null;
  return factory({ size: [0.6, 100], align: 0 });
}

/** Returns true if the status-codes chart was built with the bars plugin
 *  (and therefore expects cumulative-sum stacked data), false if it fell
 *  back to the line renderer (which expects original-count data).
 *
 *  The home view calls this once after building the chart to decide which
 *  data preparation path to use on every `setData`. */
export function isStatusChartBarsMode(): boolean {
  return uPlot.paths.bars !== undefined;
}

/** Throughput chart: 3 line series (rps / tps / cps) over time.
 *  - rps on the left Y-axis (auto-scaled).
 *  - tps on the right Y-axis (auto-scaled, typically much larger than rps).
 *  - cps on the right Y-axis too — but cost is usually µ$/sec so the right
 *    axis shows dollars; the values are tiny compared to tps, so we plot
 *    cps × 1000 (i.e. "millicents/sec") on the same axis as tps. The
 *    legend/label notes this scaling.
 *
 *  This is a pragmatic compromise — dual right-axes (one for tps, one for
 *  cps) would be confusing visually, and showing cps on its own axis that
 *  auto-scales to ~0.001 would make the line invisible. */
export function buildThroughputChart(container: HTMLElement, windowSecs: number): uPlot {
  return createLiveChart(container, {
    series: [
      {}, // X
      {
        label: "rps",
        stroke: CHART_COLORS.blue,
        width: 1.5,
        points: { show: false },
        scale: "rps",
      },
      {
        label: "tps",
        stroke: CHART_COLORS.green,
        width: 1.5,
        points: { show: false },
        scale: "tps",
      },
      {
        label: "cps",
        stroke: CHART_COLORS.orange,
        width: 1.5,
        points: { show: false },
        // Scale cps × 1000 so it shares the right axis with tps without
        // being invisible. The legend in the chart header notes this.
        scale: "tps",
        // Map values: input is $/sec; we plot as millicents/sec for
        // visibility. uPlot's `value` formatter is for the cursor readout.
        value: (_u: uPlot, raw: number): string => {
          return "$" + (raw / 1000).toFixed(5) + "/s";
        },
      },
    ],
    scales: {
      x: { time: true },
      rps: { auto: true },
      tps: { auto: true },
    },
    axes: [
      {
        grid: { stroke: "var(--color-border-soft)", width: 1 },
        ticks: { stroke: "var(--color-border)", width: 1 },
        stroke: "var(--color-text-muted)",
        font: "10px 'Courier New', monospace",
        values: timeFormatter(windowSecs),
      },
      {
        scale: "rps",
        side: 3, // left
        grid: { show: false },
        stroke: "var(--color-text-muted)",
        font: "10px 'Courier New', monospace",
        values: compactNumber,
        size: 40,
      },
      {
        scale: "tps",
        side: 1, // right
        grid: { show: false },
        stroke: "var(--color-text-muted)",
        font: "10px 'Courier New', monospace",
        values: compactNumber,
        size: 40,
      },
    ],
  });
}

/** Status codes chart: 3 series (2xx / 4xx / 5xx) per bucket. Rendered as
 *  vertical bars (uPlot's `paths.bars` factory) on a single shared Y-axis
 *  (count per bucket).
 *
 *  Stacking: uPlot doesn't have built-in stacked bars in core. The standard
 *  recipe (https://github.com/leeoniya/uPlot/blob/master/demos/stacked-bars.js)
 *  uses cumulative-sum data + reverse-order rendering so each series' bar
 *  is drawn from 0 to its (cumulative) value, with the tallest drawn first
 *  so subsequent shorter bars cover the bottom portion, leaving the top
 *  slice visible. We implement that recipe in the home view's data
 *  preparation (`stackStatusData`): the data array passed to `setData` is
 *  `[xs, total_cumulative, mid_cumulative, s2xx]` so series[1] (5xx) is the
 *  tallest, series[3] (2xx) is the shortest.
 *
 *  The series array here matches that order: series[1] = "5xx" (drawn
 *  first, bottom layer), series[2] = "4xx" (drawn second, covers bottom
 *  of 5xx), series[3] = "2xx" (drawn last, covers bottom of 4xx). The
 *  visible slices from bottom-to-top are: 2xx, 4xx, 5xx — matching the
 *  natural reading order. */
export function buildStatusCodesChart(container: HTMLElement, windowSecs: number): uPlot {
  const paths: uPlot.Series.PathBuilder | null = barsPathBuilder();
  // We construct the series objects without `paths` if the bars factory
  // isn't available — `exactOptionalPropertyTypes` requires this branch.
  if (paths !== null) {
    return createLiveChart(container, {
      series: [
        {}, // X
        {
          // series[1] = 5xx cumulative (tallest, drawn FIRST so others
          // stack on top of it). Label/color match the 5xx semantic.
          label: "5xx",
          stroke: CHART_COLORS.status5xx,
          fill: "rgba(178, 31, 31, 0.85)",
          width: 1,
          points: { show: false },
          paths,
        },
        {
          // series[2] = 4xx cumulative (s2xx + s4xx). Drawn second, covers
          // the bottom (s2xx + s4xx) of series[1]'s bar.
          label: "4xx",
          stroke: CHART_COLORS.status4xx,
          fill: "rgba(168, 106, 0, 0.85)",
          width: 1,
          points: { show: false },
          paths,
        },
        {
          // series[3] = 2xx original count. Drawn last, covers the bottom
          // (s2xx) of series[2]'s bar.
          label: "2xx",
          stroke: CHART_COLORS.status2xx,
          fill: "rgba(77, 124, 42, 0.85)",
          width: 1,
          points: { show: false },
          paths,
        },
      ],
      scales: {
        x: { time: true },
        y: { auto: true },
      },
      axes: [
        {
          grid: { stroke: "var(--color-border-soft)", width: 1 },
          ticks: { stroke: "var(--color-border)", width: 1 },
          stroke: "var(--color-text-muted)",
          font: "10px 'Courier New', monospace",
          values: timeFormatter(windowSecs),
        },
        {
          side: 3, // left
          grid: { show: false },
          stroke: "var(--color-text-muted)",
          font: "10px 'Courier New', monospace",
          values: compactNumber,
          size: 40,
        },
      ],
    });
  }
  // Fallback: line renderer (bars factory unavailable — uPlot builds
  // without the bars plugin, which is rare but possible). The 3 series
  // are rendered as overlapping area charts with translucent fills.
  return createLiveChart(container, {
    series: [
      {},
      {
        label: "2xx",
        stroke: CHART_COLORS.status2xx,
        fill: "rgba(77, 124, 42, 0.25)",
        width: 1.5,
        points: { show: false },
      },
      {
        label: "4xx",
        stroke: CHART_COLORS.status4xx,
        fill: "rgba(168, 106, 0, 0.25)",
        width: 1.5,
        points: { show: false },
      },
      {
        label: "5xx",
        stroke: CHART_COLORS.status5xx,
        fill: "rgba(178, 31, 31, 0.25)",
        width: 1.5,
        points: { show: false },
      },
    ],
    scales: {
      x: { time: true },
      y: { auto: true },
    },
    axes: [
      {
        grid: { stroke: "var(--color-border-soft)", width: 1 },
        ticks: { stroke: "var(--color-border)", width: 1 },
        stroke: "var(--color-text-muted)",
        font: "10px 'Courier New', monospace",
        values: timeFormatter(windowSecs),
      },
      {
        side: 3,
        grid: { show: false },
        stroke: "var(--color-text-muted)",
        font: "10px 'Courier New', monospace",
        values: compactNumber,
        size: 40,
      },
    ],
  });
}

/** Latency chart: 3 line series (p50 / p95 / p99) in milliseconds, single
 *  shared Y-axis. */
export function buildLatencyChart(container: HTMLElement, windowSecs: number): uPlot {
  return createLiveChart(container, {
    series: [
      {}, // X
      {
        label: "p50",
        stroke: CHART_COLORS.blue,
        width: 1.5,
        points: { show: false },
        fill: "rgba(43, 90, 120, 0.15)",
      },
      {
        label: "p95",
        stroke: CHART_COLORS.orange,
        width: 1.5,
        points: { show: false },
        fill: "rgba(168, 106, 0, 0.10)",
      },
      {
        label: "p99",
        stroke: CHART_COLORS.red,
        width: 1.5,
        points: { show: false },
      },
    ],
    scales: {
      x: { time: true },
      y: { auto: true },
    },
    axes: [
      {
        grid: { stroke: "var(--color-border-soft)", width: 1 },
        ticks: { stroke: "var(--color-border)", width: 1 },
        stroke: "var(--color-text-muted)",
        font: "10px 'Courier New', monospace",
        values: timeFormatter(windowSecs),
      },
      {
        side: 3,
        grid: { show: false },
        stroke: "var(--color-text-muted)",
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
