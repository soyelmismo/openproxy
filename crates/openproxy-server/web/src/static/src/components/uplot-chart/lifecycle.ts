// uPlot instance lifecycle: create, resize, dispose, plus shared rendering
// primitives (Catmull-Rom spline, default Options builder, sparkline factory).
//
// uPlot's stylesheet is bundled into `dist/app.css` (see `styles/index.css`)
// rather than injected as a runtime <style>: the dashboard CSP
// (`style-src-elem 'self'`) makes inline style elements inert.

import uPlot from "uplot";

/** A data series for uPlot. The first array is the X-axis (timestamps in
 *  seconds since epoch); subsequent arrays are the Y values per series.
 *  Matches uPlot's `AlignedData` type. */
export type ChartData = uPlot.AlignedData;

/** Input for `createLiveChart`. The wrapper fills in legend, cursor, select.
 *
 *  The concrete `uPlot.Series[]` / `Scales` / `Axis[]` types, not the
 *  `Options["series"]` variants, because those are `T | undefined` and
 *  `exactOptionalPropertyTypes` then rejects assigning `undefined` back. These
 *  properties are always defined when the Options object is built. */
export interface LiveChartOpts {
  series: uPlot.Series[];
  scales: uPlot.Scales;
  axes: uPlot.Axis[];
  initialData?: ChartData;
  legend?: uPlot.Legend;
}

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
      // The focus ring shows X/Y on hover, useful for a specific instant.
    },
    // Select extends BBox, which requires all four box fields.
    select: { show: false, left: 0, top: 0, width: 0, height: 0 },
    padding: [32, 20, 10, 14],
    series,
    scales,
    axes,
  };
}

/** uPlot instance for live time-series data. Starts empty (`[[]]`) and is fed
 *  through `setData(...)`; the instance is never recreated. */
export function createLiveChart(container: HTMLElement, opts: LiveChartOpts): uPlot {
  // Read the container's real size. Creating a uPlot at the 600px default for
  // a 400px container overflows the canvas, which widens the layout, which
  // re-fires ResizeObserver, which grows the chart again without bound.
  const w: number = container.clientWidth || 300;
  const h: number = container.clientHeight || 200;
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
  // Layout may have landed between the clientWidth read and now, and uPlot's
  // own `.uplot { width: min-content }` (overridden by our global
  // `.uplot { width: 100% !important }`) can mismatch briefly on first paint.
  requestAnimationFrame(() => {
    resizeChart(u, container);
  });
  return u;
}

/** Axis typography shared by every chart: a modern system sans at a small,
 *  quiet size. The old `10px Courier New` read as a 2010s console and is the
 *  single biggest contributor to the "dated" chart look. */
export const AXIS_FONT = "10.5px system-ui, -apple-system, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif";

/** Vertical gradient fill for area-style series: color at `topAlpha` at the
 *  plot's top edge, fully transparent at its bottom. Resolved per chart, so
 *  canvas gradients track the theme without recreation. */
export function areaFill(
  color: string,
  topAlpha = 0.22,
): (u: uPlot, seriesIdx: number) => CanvasGradient | string {
  return (u: uPlot, _seriesIdx: number): CanvasGradient | string => {
    const m = /^#([0-9a-fA-F]{6})$/.exec(color);
    if (!m) return color;
    const n = parseInt(m[1]!, 16);
    const r = (n >> 16) & 255;
    const g = (n >> 8) & 255;
    const b = n & 255;
    const { top, height } = u.bbox;
    // Empty charts (no data yet, or a zero-size plot) carry a non-finite or
    // degenerate bbox; createLinearGradient throws on NaN, so fall back to a
    // flat fill until the chart has a real plotting area.
    if (!Number.isFinite(top) || !Number.isFinite(height) || height <= 0) {
      return "rgba(" + r + ", " + g + ", " + b + ", " + topAlpha + ")";
    }
    const grad = u.ctx.createLinearGradient(0, top, 0, top + height);
    grad.addColorStop(0, "rgba(" + r + ", " + g + ", " + b + ", " + topAlpha + ")");
    grad.addColorStop(1, "rgba(" + r + ", " + g + ", " + b + ", 0)");
    return grad;
  };
}

/** Smooth Catmull-Rom cubic spline path builder for uPlot. */
export function smoothSpline(): uPlot.Series.PathBuilder {
  return (u: uPlot, seriesIdx: number, idx0: number, idx1: number): uPlot.Series.Paths | null => {
    const xdata = u.data[0];
    const ydata = u.data[seriesIdx];
    if (!xdata || !ydata || xdata.length === 0) return null;

    const scaleKey = u.series[seriesIdx]?.scale || "y";
    const stroke = new Path2D();
    const fill = new Path2D();

    const points: Array<[number, number]> = [];
    for (let i = idx0; i <= idx1; i++) {
      const val = ydata[i];
      if (val != null && Number.isFinite(val)) {
        const x = u.valToPos(xdata[i]!, "x", true);
        const y = u.valToPos(val, scaleKey, true);
        points.push([x, y]);
      }
    }

    if (points.length === 0) return null;

    stroke.moveTo(points[0]![0], points[0]![1]);
    if (points.length === 1) {
      stroke.lineTo(points[0]![0], points[0]![1]);
    } else if (points.length === 2) {
      stroke.lineTo(points[1]![0], points[1]![1]);
    } else {
      for (let i = 0; i < points.length - 1; i++) {
        const p0 = points[i === 0 ? i : i - 1]!;
        const p1 = points[i]!;
        const p2 = points[i + 1]!;
        const p3 = points[i + 2 < points.length ? i + 2 : i + 1]!;

        const cp1x = p1[0] + (p2[0] - p0[0]) / 6;
        const cp1y = p1[1] + (p2[1] - p0[1]) / 6;
        const cp2x = p2[0] - (p3[0] - p1[0]) / 6;
        const cp2y = p2[1] - (p3[1] - p1[1]) / 6;

        stroke.bezierCurveTo(cp1x, cp1y, cp2x, cp2y, p2[0], p2[1]);
      }
    }

    const hasFill = Boolean(u.series[seriesIdx]?.fill);
    if (hasFill) {
      fill.addPath(stroke);
      const bottomY = u.valToPos(0, scaleKey, true);
      const lastPoint = points[points.length - 1]!;
      const firstPoint = points[0]!;
      fill.lineTo(lastPoint[0], bottomY);
      fill.lineTo(firstPoint[0], bottomY);
      fill.closePath();
    }

    return {
      stroke,
      fill: hasFill ? fill : null,
    };
  };
}

export const smoothPath: uPlot.Series.PathBuilder = smoothSpline();

/** Create a tiny sparkline uPlot for KPI tile thumbnails. Single series,
 *  no axes, no legend, no cursor — just the line. The caller populates it
 *  via `setData([[xs...], [ys...]])` on each re-render.
 *
 *  The X values can be anything (we use indices 0..n-1); the X axis is
 *  hidden, so the scale doesn't matter. */
export function createSparkline(container: HTMLElement, color: string): uPlot {
  const w: number = container.clientWidth || 100;
  const h: number = container.clientHeight || 34;
  const opts: uPlot.Options = {
    width: w,
    height: h,
    legend: { show: false },
    cursor: { show: false },
    select: { show: false, left: 0, top: 0, width: 0, height: 0 },
    padding: [4, 0, 4, 0],
    series: [
      {}, // X-axis (hidden)
      {
        stroke: color,
        width: 1.5,
        paths: smoothPath,
        points: { show: false },
      },
    ],
    scales: {
      x: { time: false },
      // Pad so zero and peak never clip against the container edges.
      y: {
        auto: true,
        range: (_u: uPlot, min: number, max: number): [number, number] => {
          if (!Number.isFinite(min) || !Number.isFinite(max)) return [0, 1];
          if (max === min) return [Math.max(0, min - 0.5), min + 0.5];
          const pad = (max - min) * 0.12;
          return [Math.max(0, min - pad), max + pad];
        },
      },
    },
    axes: [
      { show: false },
      { show: false },
    ],
  };
  const u: uPlot = new uPlot(opts, [[]], container);
  // Same rAF resize as createLiveChart, which covers the case where the
  // container is not laid out yet at creation time.
  requestAnimationFrame(() => {
    resizeChart(u, container);
  });
  return u;
}


export function resizeChart(u: uPlot, container: HTMLElement): void {
  const w: number = container.clientWidth;
  const h: number = container.clientHeight;
  if (w <= 0 || h <= 0) return;
  // Every setSize redraws in full, even when the size is unchanged.
  if (u.width === w && u.height === h) return;
  u.setSize({ width: w, height: h });
}

/** ResizeObserver keeping the chart sized to its container. Returns a disposer
 *  for view unmount. Falls back to `window.resize` without ResizeObserver.
 *
 *  Debounced through a rAF coalescer plus the no-op guard in `resizeChart`:
 *  otherwise the chart's own setSize reflow can re-fire the observer 60+ times
 *  per second.
 *
 *  The explicit rAF after `observe()` covers the observer's initial fire
 *  arriving 0x0 while the container is hidden, which would otherwise strand
 *  the chart at the createLiveChart fallback size. */
export function observeResize(u: uPlot, container: HTMLElement): () => void {
  if (typeof ResizeObserver === "undefined") {
    const handler = (): void => resizeChart(u, container);
    window.addEventListener("resize", handler);
    // Initial sizing pass, mirroring the ResizeObserver branch below.
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
  // Initial sizing pass, through the same rAF coalescer so it merges with an
  // observer fire that landed first.
  scheduleResize();
  return () => {
    if (rafId !== null) {
      cancelAnimationFrame(rafId);
      rafId = null;
    }
    ro.disconnect();
  };
}
