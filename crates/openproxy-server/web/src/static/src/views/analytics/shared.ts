// views/analytics/shared.ts — shared types, state, formatters, URL helpers,
// data transforms, and utility templates used across the analytics sub-modules.

import { html, type TemplateResult } from "lit-html";
import type uPlot from "uplot";
import type {
  ByDayRow,
  ByModelRow,
  ByProviderRow,
  ByStatusRow,
  ErrorRow,
  MonthlyByProviderRow,
  Provider,
  UsagePreset,
  UsageSummary,
} from "../../lib/types/api.js";

// ── Interfaces ──────────────────────────────────────────────────────

/** API key row shape used by the filter dropdown. */
export interface ApiKeyFilterRow {
  id: number;
  label: string | null;
  key_prefix: string | null;
}

/** Flat numeric payload from the `/usage/latency` endpoint. */
export interface LatencyPayload {
  samples?: number;
  p50_connect_ms?: number | null;
  p95_connect_ms?: number | null;
  p50_ttft_ms?: number | null;
  p95_ttft_ms?: number | null;
  p50_total_ms?: number | null;
  p95_total_ms?: number | null;
  p50_tokens_per_sec?: number | null;
  p95_tokens_per_sec?: number | null;
}

/** Flat numeric payload from the `/usage/races` endpoint. */
export interface RaceStatsPayload {
  total_races?: number;
  winners?: number;
  losers?: number;
  avg_winner_position?: number | null;
  avg_ttft_savings_ms?: number | null;
  wins_by_target?: Array<[number, number]>;
}

/** Pivoted providers × months matrix with pre-computed totals. */
export interface MonthlyMatrix {
  providers: string[];
  months: string[];
  cells: Map<string, Map<string, MonthlyByProviderRow>>;
  totalsByProvider: Map<string, number>;
  totalsByMonth: Map<string, number>;
  grandTotal: number;
}

/** Grouped status-code buckets for the health chart. */
export interface StatusBuckets {
  s2xx: number;
  s4xx: number;
  s5xx: number;
  other: number;
}

/** Hash query-string parameters parsed from `location.hash`. */
export interface AnalyticsHashParams {
  preset: UsagePreset;
  providerId: string;
  apiKeyId: string;
}

/** Ranking list item used by `renderRanking`. */
export interface RankingItem {
  name: string;
  context: string;
  requests: number;
  cost: number;
}

// ── Presets ─────────────────────────────────────────────────────────

/** Ordered presets rendered as buttons in the time-range selector. */
export const PRESETS: readonly UsagePreset[] = [
  "today", "7d", "30d",
  "this_month", "last_month", "last_6_months",
  "ytd", "custom",
];

/** Friendly labels for the preset buttons, keyed by preset value. */
export const PRESET_LABEL_KEYS = {
  today: "analytics.preset.today",
  "7d": "analytics.preset.7d",
  "30d": "analytics.preset.30d",
  this_month: "analytics.preset.this_month",
  last_month: "analytics.preset.last_month",
  last_6_months: "analytics.preset.last_6_months",
  ytd: "analytics.preset.ytd",
  custom: "analytics.preset.custom",
} satisfies Record<UsagePreset, string>;

// ── Formatters ──────────────────────────────────────────────────────

export function fmtCost(v: number): string {
  if (!Number.isFinite(v)) return "$0.00";
  if (v !== 0 && Math.abs(v) < 0.01) return `$${v.toFixed(4)}`;
  return new Intl.NumberFormat(undefined, {
    style: "currency",
    currency: "USD",
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  }).format(v);
}

export function fmtNumber(v: number): string {
  return new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 }).format(v);
}

export function fmtPercent(v: number): string {
  return Number.isFinite(v) ? `${(v * 100).toFixed(v === 0 || v >= 0.1 ? 1 : 2)}%` : "—";
}

export function fmtDuration(v: number | null | undefined): string {
  if (v == null || !Number.isFinite(v)) return "—";
  return v >= 1000 ? `${(v / 1000).toFixed(2)}s` : `${Math.round(v)}ms`;
}

export function fmtDateTime(v: string): string {
  const date = new Date(v);
  if (Number.isNaN(date.getTime())) return "—";
  return new Intl.DateTimeFormat(undefined, {
    month: "short", day: "numeric", hour: "2-digit", minute: "2-digit",
  }).format(date);
}

// ── URL hash helpers ────────────────────────────────────────────────

/** Parse `location.hash` into structured analytics parameters. */
export function parseHashParams(): AnalyticsHashParams {
  const hash = location.hash || "#/analytics";
  const qIdx = hash.indexOf("?");
  const query = qIdx >= 0 ? hash.slice(qIdx + 1) : "";
  const params = new URLSearchParams(query);
  const rangeRaw = params.get("range") || "this_month";
  const preset: UsagePreset = (PRESETS as readonly string[]).includes(rangeRaw)
    ? rangeRaw as UsagePreset
    : "this_month";
  return {
    preset,
    providerId: params.get("provider_id") || "",
    apiKeyId: params.get("api_key_id") || "",
  };
}

/** Swap any subset of the hash params. Setting a filter to `""`
 *  deletes it from the URL so a cleared filter doesn't linger. */
export function setHashParams(updates: Partial<AnalyticsHashParams>): void {
  const hash = location.hash || "#/analytics";
  const qIdx = hash.indexOf("?");
  const path = qIdx >= 0 ? hash.slice(0, qIdx) : hash;
  const query = qIdx >= 0 ? hash.slice(qIdx + 1) : "";
  const params = new URLSearchParams(query);
  if (updates.preset !== undefined) params.set("range", updates.preset);
  if (updates.providerId !== undefined) {
    if (updates.providerId) params.set("provider_id", updates.providerId);
    else params.delete("provider_id");
  }
  if (updates.apiKeyId !== undefined) {
    if (updates.apiKeyId) params.set("api_key_id", updates.apiKeyId);
    else params.delete("api_key_id");
  }
  const qs = params.toString();
  location.hash = qs ? `${path}?${qs}` : path;
}

/** Build the combined query string used by every `/usage/*` fetch. */
export function buildUsageQuery(
  preset: UsagePreset,
  providerId: string,
  apiKeyId: string,
  extra: Record<string, string> = {},
): string {
  const params = new URLSearchParams();
  if (preset !== "custom") params.set("preset", preset);
  if (providerId) params.set("provider_id", providerId);
  if (apiKeyId) params.set("api_key_id", apiKeyId);
  for (const [k, v] of Object.entries(extra)) {
    if (v) params.set(k, v);
  }
  const qs = params.toString();
  return qs ? `?${qs}` : "";
}

// ── Data transforms ─────────────────────────────────────────────────

/** Pivot `MonthlyByProviderRow[]` into a providers × months matrix
 *  with top-N providers and visible-month windowing. */
export function pivotMonthlyByProvider(
  rows: MonthlyByProviderRow[],
  topN: number = 10,
  maxMonths: number = 6,
): MonthlyMatrix {
  const allCells = new Map<string, Map<string, MonthlyByProviderRow>>();
  const monthsSet = new Set<string>();
  for (const r of rows) {
    monthsSet.add(r.month);
    const p = r.provider_id;
    if (!allCells.has(p)) allCells.set(p, new Map());
    allCells.get(p)!.set(r.month, r);
  }

  const allMonths = [...monthsSet].sort();
  const months = allMonths.slice(-maxMonths);
  const monthSet = new Set(months);

  const totalsByProvider = new Map<string, number>();
  for (const [p, pCells] of allCells) {
    let t = 0;
    let hasVisible = false;
    for (const m of months) {
      const r = pCells.get(m);
      if (r) { hasVisible = true; t += r.total_cost_usd; }
    }
    if (hasVisible) totalsByProvider.set(p, t);
  }

  const sortedProviders = [...totalsByProvider.entries()]
    .sort((a, b) => b[1] - a[1]);
  const topProviders = sortedProviders.slice(0, topN).map(([p]) => p);
  const otherProviders = sortedProviders.slice(topN).map(([p]) => p);

  const cells = new Map<string, Map<string, MonthlyByProviderRow>>();
  for (const p of topProviders) {
    const pCells = allCells.get(p);
    if (!pCells) continue;
    const restricted = new Map<string, MonthlyByProviderRow>();
    for (const [m, r] of pCells) if (monthSet.has(m)) restricted.set(m, r);
    cells.set(p, restricted);
  }

  if (otherProviders.length > 0) {
    const otherMonths = new Map<string, MonthlyByProviderRow>();
    let otherTotal = 0;
    for (const p of otherProviders) {
      const pCells = allCells.get(p);
      if (!pCells) continue;
      for (const m of months) {
        const r = pCells.get(m);
        if (!r) continue;
        otherTotal += r.total_cost_usd;
        const existing = otherMonths.get(m);
        if (existing) {
          existing.unique_requests += r.unique_requests;
          existing.total_rows += r.total_rows;
          existing.total_prompt_tokens += r.total_prompt_tokens;
          existing.total_completion_tokens += r.total_completion_tokens;
          existing.total_cost_usd += r.total_cost_usd;
        } else {
          otherMonths.set(m, { ...r, provider_id: "Other" });
        }
      }
    }
    cells.set("Other", otherMonths);
    totalsByProvider.set("Other", otherTotal);
    topProviders.push("Other");
  }

  const totalsByMonth = new Map<string, number>();
  let grandTotal = 0;
  for (const m of months) {
    let mTotal = 0;
    for (const p of topProviders) {
      const r = cells.get(p)?.get(m);
      if (r) mTotal += r.total_cost_usd;
    }
    totalsByMonth.set(m, mTotal);
    grandTotal += mTotal;
  }

  return {
    providers: topProviders,
    months,
    cells,
    totalsByProvider,
    totalsByMonth,
    grandTotal,
  };
}

/** Group by-status rows into 2xx / 4xx / 5xx / other buckets. */
export function groupByStatus(rows: ByStatusRow[]): StatusBuckets {
  let s2 = 0, s4 = 0, s5 = 0, other = 0;
  for (const r of rows) {
    if (r.status_code >= 200 && r.status_code < 300) s2 += r.count;
    else if (r.status_code >= 400 && r.status_code < 500) s4 += r.count;
    else if (r.status_code >= 500 && r.status_code < 600) s5 += r.count;
    else other += r.count;
  }
  return { s2xx: s2, s4xx: s4, s5xx: s5, other };
}

/** Parse "YYYY-MM-DD" into a UTC-midnight timestamp (seconds) for uPlot. */
export function dateToSeconds(date: string): number {
  const ms: number = Date.parse(date + "T00:00:00Z");
  if (!Number.isFinite(ms)) return 0;
  return ms / 1000;
}

/** Convert daily usage rows into uPlot `AlignedData`: [xs, reqs, errors, cost]. */
export function dailyUsageData(rows: ByDayRow[]): uPlot.AlignedData {
  const xs: number[] = new Array<number>(rows.length);
  const reqs: number[] = new Array<number>(rows.length);
  const errors: number[] = new Array<number>(rows.length);
  const cost: number[] = new Array<number>(rows.length);
  for (let i = 0; i < rows.length; i++) {
    const r: ByDayRow = rows[i]!;
    xs[i] = dateToSeconds(r.date);
    reqs[i] = r.unique_requests;
    errors[i] = r.errors;
    cost[i] = r.total_cost_usd;
  }
  return [xs, reqs, errors, cost];
}

// ── View-local mutable state ────────────────────────────────────────
// Reset on every mount; read by all render functions via live bindings.

export let loading = true;
export let errorMsg: string | null = null;
export let summary: UsageSummary | null = null;
export let byDay: ByDayRow[] = [];
export let byModel: ByModelRow[] = [];
export let byProvider: ByProviderRow[] = [];
export let byStatus: ByStatusRow[] = [];
export let monthlyByProvider: MonthlyByProviderRow[] = [];
export let latency: LatencyPayload | null = null;
export let races: RaceStatsPayload | null = null;
export let errors: ErrorRow[] = [];
export let providers: Provider[] = [];
export let apiKeys: ApiKeyFilterRow[] = [];

/** Bulk-set view-local state. Importing modules read live bindings
 *  so the updated values are visible after this call. */
export function setViewState(next: {
  loading?: boolean;
  errorMsg?: string | null;
  summary?: UsageSummary | null;
  byDay?: ByDayRow[];
  byModel?: ByModelRow[];
  byProvider?: ByProviderRow[];
  byStatus?: ByStatusRow[];
  monthlyByProvider?: MonthlyByProviderRow[];
  latency?: LatencyPayload | null;
  races?: RaceStatsPayload | null;
  errors?: ErrorRow[];
  providers?: Provider[];
  apiKeys?: ApiKeyFilterRow[];
}): void {
  if (next.loading !== undefined) loading = next.loading;
  if (next.errorMsg !== undefined) errorMsg = next.errorMsg;
  if (next.summary !== undefined) summary = next.summary;
  if (next.byDay !== undefined) byDay = next.byDay;
  if (next.byModel !== undefined) byModel = next.byModel;
  if (next.byProvider !== undefined) byProvider = next.byProvider;
  if (next.byStatus !== undefined) byStatus = next.byStatus;
  if (next.monthlyByProvider !== undefined) monthlyByProvider = next.monthlyByProvider;
  if (next.latency !== undefined) latency = next.latency;
  if (next.races !== undefined) races = next.races;
  if (next.errors !== undefined) errors = next.errors;
  if (next.providers !== undefined) providers = next.providers;
  if (next.apiKeys !== undefined) apiKeys = next.apiKeys;
}

// ── Chart lifecycle state ───────────────────────────────────────────

export interface AnalyticsCharts {
  dailyUsage: uPlot;
  resizeDisposers: Array<() => void>;
}
export let charts: AnalyticsCharts | null = null;

export function setCharts(c: AnalyticsCharts | null): void {
  charts = c;
}

// ── Utility templates ───────────────────────────────────────────────

export function card(title: string, body: TemplateResult, className: string = ""): TemplateResult {
  return html`<section class="card analytics-data-card ${className}">
    <div class="analytics-card-heading"><h3>${title}</h3></div>
    ${body}
  </section>`;
}

export function metric(label: string, value: string, meta: string, tone: string = ""): TemplateResult {
  return html`<div class="analytics-metric ${tone}">
    <span class="analytics-metric-label">${label}</span>
    <strong class="analytics-metric-value">${value}</strong>
    <span class="analytics-metric-meta">${meta}</span>
  </div>`;
}
