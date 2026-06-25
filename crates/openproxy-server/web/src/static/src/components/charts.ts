//! Inline SVG chart components — no external library, keeps the bundle lean.
//!
//! All charts are pure functions that return an SVG string. The caller
//! injects it into the DOM via `innerHTML`.
//!
//! Design: follows the Dell 1996 retro aesthetic — 1px black strokes,
//! square data points, no soft shadows, no gradients. Chart series
//! colors come from the `--chart-*` CSS custom properties.

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

// ── Sparkline ───────────────────────────────────────────────────────

/** A tiny inline sparkline for table cells. ~60×16px. */
export function sparkline(values: number[], color: string = CHART_COLORS[0]!): string {
  if (values.length === 0) return "";
  const w = 60;
  const h = 16;
  const max = Math.max(1, ...values);
  const min = Math.min(0, ...values);
  const range = max - min || 1;
  const step = values.length > 1 ? w / (values.length - 1) : 0;
  const path = values
    .map((v, i) => `${i === 0 ? "M" : "L"} ${(i * step).toFixed(1)} ${(h - ((v - min) / range) * h).toFixed(1)}`)
    .join(" ");
  return `<svg class="sparkline" viewBox="0 0 ${w} ${h}" style="width:${w}px;height:${h}px;display:inline-block;vertical-align:middle;">
    <path d="${path}" fill="none" stroke="${color}" stroke-width="1" />
  </svg>`;
}

// ── Status distribution donut ───────────────────────────────────────

export interface StatusSlice {
  label: string;
  count: number;
  color: string;
}

/** A donut chart showing HTTP status distribution. ~160×160px. */
export function statusDonut(slices: StatusSlice[]): string {
  const total = slices.reduce((s, x) => s + x.count, 0);
  if (total === 0) {
    return `<div class="chart-empty muted">No data.</div>`;
  }
  const cx = 80, cy = 80, r = 60, rInner = 35;
  let angle = -Math.PI / 2; // start at top
  const arcs: string[] = [];
  for (const s of slices) {
    const frac = s.count / total;
    if (frac === 0) continue;
    const end = angle + frac * 2 * Math.PI;
    const largeArc = frac > 0.5 ? 1 : 0;
    const x1 = cx + r * Math.cos(angle);
    const y1 = cy + r * Math.sin(angle);
    const x2 = cx + r * Math.cos(end);
    const y2 = cy + r * Math.sin(end);
    const xi1 = cx + rInner * Math.cos(end);
    const yi1 = cy + rInner * Math.sin(end);
    const xi2 = cx + rInner * Math.cos(angle);
    const yi2 = cy + rInner * Math.sin(angle);
    arcs.push(
      `<path d="M ${x1.toFixed(1)} ${y1.toFixed(1)} A ${r} ${r} 0 ${largeArc} 1 ${x2.toFixed(1)} ${y2.toFixed(1)} L ${xi1.toFixed(1)} ${yi1.toFixed(1)} A ${rInner} ${rInner} 0 ${largeArc} 0 ${xi2.toFixed(1)} ${yi2.toFixed(1)} Z" fill="${s.color}" stroke="var(--color-border)" stroke-width="0.5" />`
    );
    angle = end;
  }
  const pct = (n: number) => `${((n / total) * 100).toFixed(0)}%`;
  const legend = slices
    .filter(s => s.count > 0)
    .map(s => `<div class="donut-legend-item"><span class="donut-dot" style="background:${s.color}"></span>${s.label}: <strong>${s.count}</strong> <span class="muted">(${pct(s.count)})</span></div>`)
    .join("");
  return `<div class="donut-wrap">
    <svg viewBox="0 0 160 160" style="width:140px;height:140px;">${arcs.join("")}</svg>
    <div class="donut-legend">${legend}</div>
  </div>`;
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

// ── KPI tile ────────────────────────────────────────────────────────

export interface KpiTileOpts {
  label: string;
  value: string;
  /** Optional sub-value (e.g. "↑ 12% vs yesterday"). */
  sub?: string;
  /** Optional trend: "up" | "down" | null. */
  trend?: "up" | "down" | null;
  /** Optional CSS class for the value color. */
  valueClass?: string;
}

/** Render a single KPI tile as an HTML string. */
export function kpiTile(opts: KpiTileOpts): string {
  const trendIcon = opts.trend === "up" ? "▲" : opts.trend === "down" ? "▼" : "";
  const trendClass = opts.trend === "up" ? "kpi-trend-up" : opts.trend === "down" ? "kpi-trend-down" : "";
  const sub = opts.sub ? `<div class="kpi-sub ${trendClass}">${trendIcon} ${opts.sub}</div>` : "";
  return `<div class="kpi-tile">
    <div class="kpi-label">${opts.label}</div>
    <div class="kpi-value ${opts.valueClass || ""}">${opts.value}</div>
    ${sub}
  </div>`;
}

// ── Preset selector ─────────────────────────────────────────────────

export interface PresetSelectorOpts {
  current: string;
  onChange: (preset: string) => void;
}

const PRESETS: Array<{ value: string; label: string }> = [
  { value: "today", label: "Today" },
  { value: "7d", label: "7 days" },
  { value: "30d", label: "30 days" },
  { value: "this_month", label: "This month" },
  { value: "last_month", label: "Last month" },
  { value: "last_6_months", label: "6 months" },
  { value: "ytd", label: "YTD" },
  { value: "custom", label: "All time" },
];

/** Render a preset selector button group. */
export function presetSelector(opts: PresetSelectorOpts): string {
  const buttons = PRESETS.map(p =>
    `<button class="preset-btn ${p.value === opts.current ? "active" : ""}" data-preset="${p.value}">${p.label}</button>`
  ).join("");
  return `<div class="preset-selector">${buttons}</div>`;
}

/** Wire up preset selector button clicks. Call after innerHTML. */
export function wirePresetSelector(container: HTMLElement, onChange: (preset: string) => void): void {
  container.querySelectorAll<HTMLButtonElement>(".preset-btn").forEach(btn => {
    btn.addEventListener("click", () => onChange(btn.dataset["preset"] || "custom"));
  });
}
