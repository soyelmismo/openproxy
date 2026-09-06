//! Inline SVG daily-usage chart — no external library, keeps the
//! bundle lean. Pure function returning an SVG string; the caller
//! injects it into the DOM via `innerHTML`.
//!
//! Design: follows the Dell 1996 retro aesthetic — 1px black strokes,
//! square data points, no soft shadows, no gradients. Chart series
//! colors come from the `--chart-*` CSS custom properties.
//!
//! Interactive charts (line/sparkline/time-series) live in
//! `uplot-chart.ts` (uPlot).

import type { ByDayRow } from "../lib/types/api";

// ── Color palette ────────────────────────────────────────────────────
// Repurposes the catalog tint family as data-series colors.
const CHART_COLORS = [
  "#e91d2a", // dell red (primary)
  "#4d7c2a", // sage (success)
  "#a86a00", // peach (warn)
  "#2b5a78", // sky (info)
  "#6a26a4", // purple
  "#b21f1f", // salmon (error)
  "#5a8a2a", // olive
  "#7a5a3a", // steel
];

// ── Daily usage line chart ──────────────────────────────────────────

export interface DailyChartOpts {
  /** Height of the chart area in px (excluding labels). Default 200. */
  height?: number;
  /** Width of the chart area in px. If omitted, uses 100% (responsive). */
  width?: number;
  /** Whether to show the cost axis (right side). Default true. */
  showCost?: boolean;
}

/**
 * Render a dual-axis daily usage chart as inline SVG.
 *
 * Left axis: unique requests (line).
 * Right axis: cost USD (bars).
 * Bottom axis: dates.
 *
 * The SVG uses `viewBox` so it scales responsively.
 */
export function dailyUsageChart(
  rows: ByDayRow[],
  opts: DailyChartOpts = {},
): string {
  if (rows.length === 0) {
    return `<div class="chart-empty muted">No data for the selected range.</div>`;
  }

  const h = opts.height ?? 200;
  const w = opts.width ?? 800;
  const padL = 50;   // left axis labels
  const padR = opts.showCost !== false ? 55 : 15;
  const padT = 10;
  const padB = 30;   // bottom date labels
  const plotW = w - padL - padR;
  const plotH = h - padT - padB;

  const maxReqs = Math.max(1, ...rows.map(r => r.unique_requests));
  const maxCost = Math.max(0.01, ...rows.map(r => r.total_cost_usd));

  const n = rows.length;
  const barW = n > 1 ? Math.max(2, plotW / n * 0.6) : plotW * 0.5;
  const stepX = n > 1 ? plotW / (n - 1) : 0;

  // Scale functions.
  const x = (i: number) => padL + (n > 1 ? i * stepX : plotW / 2);
  const yReqs = (v: number) => padT + plotH - (v / maxReqs) * plotH;
  const yCost = (v: number) => padT + plotH - (v / maxCost) * plotH;

  // Build the requests line path.
  const linePath = rows
    .map((r, i) => `${i === 0 ? "M" : "L"} ${x(i).toFixed(1)} ${yReqs(r.unique_requests).toFixed(1)}`)
    .join(" ");

  // Build cost bars.
  const bars = rows.map((r, i) => {
    const bx = x(i) - barW / 2;
    const by = yCost(r.total_cost_usd);
    const bh = padT + plotH - by;
    return `<rect x="${bx.toFixed(1)}" y="${by.toFixed(1)}" width="${barW.toFixed(1)}" height="${bh.toFixed(1)}" fill="${CHART_COLORS[1]}" opacity="0.3" />`;
  }).join("");

  // Build data points (circles on the line).
  const dots = rows.map((r, i) =>
    `<circle cx="${x(i).toFixed(1)}" cy="${yReqs(r.unique_requests).toFixed(1)}" r="2.5" fill="${CHART_COLORS[0]}" stroke="#000" stroke-width="0.5" />`
  ).join("");

  // Y-axis labels (requests — left).
  const reqTicks = 4;
  const reqLabels: string[] = [];
  for (let t = 0; t <= reqTicks; t++) {
    const val = Math.round(maxReqs * t / reqTicks);
    const yp = yReqs(val);
    reqLabels.push(
      `<text x="${padL - 6}" y="${yp + 3}" text-anchor="end" class="chart-axis-label">${formatTick(val)}</text>` +
      `<line x1="${padL}" y1="${yp}" x2="${w - padR}" y2="${yp}" stroke="var(--color-border-soft)" stroke-width="0.5" stroke-dasharray="2,3" />`
    );
  }

  // Y-axis labels (cost — right).
  const costLabels: string[] = [];
  if (opts.showCost !== false) {
    const costTicks = 4;
    for (let t = 0; t <= costTicks; t++) {
      const val = maxCost * t / costTicks;
      const yp = yCost(val);
      costLabels.push(
        `<text x="${w - padR + 6}" y="${yp + 3}" text-anchor="start" class="chart-axis-label">$${formatCost(val)}</text>`
      );
    }
  }

  // X-axis labels (dates) — show ~6 labels max to avoid crowding.
  const xLabels: string[] = [];
  const labelEvery = Math.max(1, Math.ceil(n / 6));
  rows.forEach((r, i) => {
    if (i % labelEvery === 0 || i === n - 1) {
      const label = r.date.slice(5); // "MM-DD"
      xLabels.push(
        `<text x="${x(i)}" y="${h - 8}" text-anchor="middle" class="chart-axis-label">${label}</text>`
      );
    }
  });

  // Axis lines.
  const axisLine = `stroke="var(--color-border)" stroke-width="1"`;

  return `<svg class="chart-svg" viewBox="0 0 ${w} ${h}" preserveAspectRatio="xMidYMid meet" style="width:100%;height:${h}px;">
    ${bars}
    <path d="${linePath}" fill="none" stroke="${CHART_COLORS[0]}" stroke-width="1.5" />
    ${dots}
    ${reqLabels.join("")}
    ${costLabels.join("")}
    ${xLabels.join("")}
    <line x1="${padL}" y1="${padT}" x2="${padL}" y2="${padT + plotH}" ${axisLine} />
    <line x1="${padL}" y1="${padT + plotH}" x2="${w - padR}" y2="${padT + plotH}" ${axisLine} />
  </svg>`;
}

// ── Helpers ─────────────────────────────────────────────────────────

function formatTick(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

function formatCost(n: number): string {
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`;
  if (n >= 1) return n.toFixed(2);
  if (n > 0) return n.toFixed(3);
  return "0";
}
