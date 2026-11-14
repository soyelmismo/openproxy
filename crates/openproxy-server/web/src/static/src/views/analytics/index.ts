// views/analytics/index.ts — layout, KPI/status/latency/race blocks, mount sequence.

import { html, nothing, type TemplateResult } from "lit-html";
import { state } from "../../state/index.js";
import { api } from "../../state/api.js";
import { mountView, requestUpdate } from "../../state/reactive.js";
import { t } from "../../i18n/index.js";
import { isFullAdmin, getProfile } from "../../state/auth.js";
import { loadKeyKinds } from "../../lib/keys-api.js";
import {
  buildUsageQuery,
  byModel,
  byProvider,
  byStatus,
  clearRankExclusions,
  errorMsg,
  exclusionsExtra,
  fmtCost,
  fmtNumber,
  fmtPercent,
  fmtDuration,
  formatRankValue,
  groupByStatus,
  keyKindExtra,
  latency,
  loading,
  metric,
  parseHashParams,
  races,
  rankMetric,
  rankMetricValue,
  RANK_LABEL_KEYS,
  setRankMetric,
  setViewState,
  summary,
  toggleRankExclusion,
  excludedModels,
  excludedProviders,
  type ApiKeyFilterRow,
  type LatencyPayload,
  type RaceStatsPayload,
  type RankingItem,
} from "./shared.js";
import {
  createAnalyticsCharts,
  destroyAnalyticsCharts,
  ensureAnalyticsChartsAlive,
  refreshAnalyticsChartTheme,
  renderAnalyticsToolbar,
  renderRankMetricSwitch,
  resetAnalyticsMenus,
} from "./charts.js";
import { renderComboHealth } from "./combo-health.js";
import {
  renderByModelTable,
  renderByProviderTable,
  renderMonthlyMatrix,
  renderRecentErrors,
} from "./tables.js";
import type {
  ByComboRow,
  ByDayRow,
  ByModelRow,
  ByProviderRow,
  ByStatusRow,
  ErrorRow,
  KeyQuotaListItem,
  KeyQuotaResponse,
  KeyQuotaStatus,
  MonthlyByProviderRow,
  PlanStatus,
  Provider,
  UsageSummary,
} from "../../lib/types/api.js";

// ── KPI metrics strip ──────────────────────────────────────────────

/// Customer strip: requests, tokens and spend. Everything else in the admin
/// strip (success/latency averages, cache internals, local compression) is
/// operator telemetry and the server no longer sends those fields.
function renderClientMetrics(): TemplateResult {
  if (!summary) return html``;
  const totalTokens = summary.total_prompt_tokens + summary.total_completion_tokens;
  const costPerRequest = summary.unique_requests > 0 ? summary.total_cost_usd / summary.unique_requests : 0;
  return html`<div class="analytics-metrics" aria-label=${t("analytics.summary")}>
    ${metric(t("analytics.summary.unique_requests"), fmtNumber(summary.unique_requests), t("analytics.metric.requests_hint"))}
    ${metric(t("analytics.metric.tokens"), fmtNumber(totalTokens), `${fmtNumber(summary.total_prompt_tokens)} in · ${fmtNumber(summary.total_completion_tokens)} out`)}
    ${metric(t("analytics.summary.total_cost"), fmtCost(summary.total_cost_usd), `${fmtCost(costPerRequest)} ${t("analytics.metric.per_request")}`)}
  </div>`;
}

function renderMetrics(): TemplateResult {
  if (!summary) return html``;
  if (!isFullAdmin()) return renderClientMetrics();
  // Requests KPI counts only requests with a successful row; failures feed
  // the success rate and the failed counter. The all/failed fields come
  // from the newer backend; fall back gracefully against an older one.
  const allRequests = summary.all_requests ?? summary.total_rows;
  const failedRequests = summary.failed_requests ?? Math.max(0, summary.errors);
  const successRate = allRequests > 0 ? summary.unique_requests / allRequests : NaN;
  const totalTokens = summary.total_prompt_tokens + summary.total_completion_tokens;
  const costPerRequest = summary.unique_requests > 0 ? summary.total_cost_usd / summary.unique_requests : 0;
  const p95Total = latency?.p95_total_ms;
  const successTone = !Number.isFinite(successRate) ? "" : successRate >= 0.95 ? "is-good" : successRate >= 0.85 ? "is-warn" : "is-bad";
  return html`<div class="analytics-metrics" aria-label=${t("analytics.summary")}>
    ${metric(t("analytics.summary.unique_requests"), fmtNumber(summary.unique_requests), `${fmtNumber(summary.total_attempts)} ${t("analytics.metric.attempts")}`)}
    ${metric(t("analytics.metric.success_rate"), fmtPercent(successRate), `${fmtNumber(failedRequests)} ${t("analytics.metric.failed_responses")}`, successTone)}
    ${metric(t("analytics.metric.tokens"), fmtNumber(totalTokens), `${fmtNumber(summary.total_prompt_tokens)} in · ${fmtNumber(summary.total_completion_tokens)} out`)}
    ${metric("API Cache", fmtPercent(summary.total_prompt_tokens > 0 ? summary.total_cached_tokens / summary.total_prompt_tokens : 0), `${fmtNumber(summary.total_cached_tokens)} prompt tokens cached`, summary.total_cached_tokens > 0 ? "is-good" : "")}
    ${metric("Local Compression", fmtPercent((summary.avg_compression_savings_pct ?? 0) / 100), "Avg token savings when active", "is-good")}
    ${metric(t("analytics.summary.total_cost"), fmtCost(summary.total_cost_usd), `${fmtCost(costPerRequest)} ${t("analytics.metric.per_request")}`)}
    ${metric(t("analytics.metric.avg_success_total"), fmtDuration(summary.avg_success_total_ms), t("analytics.metric.avg_success_phases", { connect: fmtDuration(summary.avg_success_connect_ms), ttft: fmtDuration(summary.avg_success_ttft_ms) }))}
    ${metric(t("analytics.metric.avg_ttft"), fmtDuration(summary.avg_ttft_ms), `${fmtDuration(summary.avg_total_ms)} ${t("analytics.metric.avg_total")}`)}
    ${metric(t("analytics.metric.p95_latency"), fmtDuration(p95Total), `${fmtNumber(latency?.samples ?? 0)} ${t("analytics.metric.successful_samples")}`)}
  </div>`;
}

// ── Dollar-quota panel (5h / weekly / monthly) ─────────────────────

// Client portal: own-key status from `/usage/quota` (null while loading or on failure).
let quotaStatus: KeyQuotaStatus | null = null;
// Admin: keys with a quota row (labels) + the live spend of the picked key.
let adminQuotaKeys: KeyQuotaListItem[] | null = null;
let adminQuota: KeyQuotaResponse | null = null;
let adminQuotaKeyId: number | null = null;
let countdownTimer: number | null = null;

async function loadQuotaStatus(): Promise<void> {
  try {
    quotaStatus = await api("/usage/quota") as KeyQuotaStatus;
  } catch {
    quotaStatus = null;
  }
  requestUpdate();
}

/** Admin: keys that carry a quota row. Labels only — the picked key always
 *  comes from the toolbar filter, never auto-selected here. */
async function loadAdminQuotaKeys(): Promise<void> {
  try {
    const resp = await api("/keys/quotas") as { quotas: KeyQuotaListItem[] };
    adminQuotaKeys = resp.quotas ?? [];
  } catch {
    adminQuotaKeys = [];
  }
  requestUpdate();
}

/** Fetch (admin-only) the live quota status of one key. */
async function loadAdminKeyQuota(keyId: number): Promise<void> {
  adminQuotaKeyId = keyId;
  try {
    adminQuota = await api(`/keys/${keyId}/quota`) as KeyQuotaResponse;
  } catch {
    adminQuota = null;
  }
  requestUpdate();
}

// SQLite UTC "YYYY-MM-DD HH:MM:SS" and ISO stamps both parse here.
function parseQuotaTs(ts: string | null | undefined): Date | null {
  if (!ts) return null;
  const iso = ts.includes("T") ? ts : `${ts.replace(" ", "T")}Z`;
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? null : d;
}

function formatCountdown(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  const parts: string[] = [];
  if (d > 0) parts.push(`${d}d`);
  if (d > 0 || h > 0) parts.push(`${h}h`);
  if (d > 0 || h > 0 || m > 0) parts.push(`${m}m`);
  parts.push(`${s % 60}s`);
  return parts.join(" ");
}

function countdownText(targetIso: string, kind: "reset" | "expire" = "reset"): string {
  const d = new Date(targetIso);
  if (Number.isNaN(d.getTime())) return "";
  const diff = d.getTime() - Date.now();
  if (diff <= 0) return kind === "expire" ? "expired" : "resetting…";
  return kind === "expire" ? `expires in ${formatCountdown(diff)}` : `resets in ${formatCountdown(diff)}`;
}

/** Live-tick every countdown hint in the panel (1s DOM updates, no re-render). */
function startCountdownTicker(): void {
  if (countdownTimer !== null) return;
  const tick = (): void => {
    document.querySelectorAll<HTMLElement>("[data-quota-countdown]").forEach((el) => {
      const attr = el.dataset["quotaCountdown"];
      if (!attr) return;
      const kind = el.dataset["quotaCountdownKind"] === "expire" ? "expire" : "reset";
      el.textContent = countdownText(attr, kind);
    });
  };
  tick();
  countdownTimer = window.setInterval(tick, 1000);
}

function stopCountdownTicker(): void {
  if (countdownTimer !== null) {
    clearInterval(countdownTimer);
    countdownTimer = null;
  }
}

interface AnalyticsQuotaWindow {
  title: string;
  cap: number | null;
  spent: number;
  /** ISO instant the window resets; null = no scheduled reset. */
  targetIso: string | null;
  /** Hint shown when the window has a cap but no running reset. */
  idleHint: string;
}

// Provider-style quota window: percentage + usage bar + live countdown.
function quotaWindowHtml(w: AnalyticsQuotaWindow): TemplateResult {
  const pct = w.cap && w.cap > 0 ? Math.round((w.spent / w.cap) * 100) : null;
  const color = pct == null ? "" : pct > 80 ? "danger" : pct > 50 ? "warn" : "ok";
  const numberClass = pct != null && pct >= 100 ? "used" : pct != null && pct > 80 ? "warn" : "available";
  const target = w.targetIso ? parseQuotaTs(w.targetIso) : null;
  const targetIso = target ? target.toISOString() : "";
  const staticHint = pct == null ? "window off" : w.idleHint;
  return html`<div class="quota-window">
    <div class="quota-window-header">
      <span class="quota-window-title">${w.title}</span>
      <span class="quota-window-numbers ${numberClass}">${pct == null ? "∞" : `${pct}%`}</span>
    </div>
    <div class="quota-bar mini ${color}">
      <div class="quota-bar-track" role="meter" aria-label="${w.title}" aria-valuenow="${pct ?? 0}" aria-valuemin="0" aria-valuemax="100">
        <div class="quota-bar-fill" style="width: ${pct == null ? 0 : Math.min(100, Math.max(0, pct))}%"></div>
      </div>
    </div>
    <small class="quota-window-hint" data-quota-countdown="${targetIso}">${targetIso ? countdownText(targetIso) : staticHint}</small>
  </div>`;
}

/** The plan bar: its credited balance IS the monthly bound, so it renders as
 *  a percentage of the credit spent, with the plan expiry as the countdown. */
function planBalanceHtml(plan: PlanStatus): TemplateResult {
  const used = Math.max(0, plan.credit_usd - plan.balance_usd);
  const pct = plan.credit_usd > 0 ? Math.round((used / plan.credit_usd) * 100) : 0;
  const color = pct > 80 ? "danger" : pct > 50 ? "warn" : "ok";
  const numberClass = pct >= 100 ? "used" : pct > 80 ? "warn" : "available";
  const target = parseQuotaTs(plan.expires_at);
  const targetIso = target ? target.toISOString() : "";
  const hint = targetIso ? countdownText(targetIso, "expire") : "no expiry";
  return html`<div class="quota-window">
    <div class="quota-window-header">
      <span class="quota-window-title">${plan.name} balance</span>
      <span class="quota-window-numbers ${numberClass}">${pct}%</span>
    </div>
    <div class="quota-bar mini ${color}">
      <div class="quota-bar-track" role="meter" aria-label="${plan.name} balance" aria-valuenow="${pct}" aria-valuemin="0" aria-valuemax="100">
        <div class="quota-bar-fill" style="width: ${Math.min(100, Math.max(0, pct))}%"></div>
      </div>
    </div>
    <small class="quota-window-hint" data-quota-countdown="${targetIso}" data-quota-countdown-kind="expire">${hint}</small>
  </div>`;
}

export function quotaWindowList(spend: KeyQuotaStatus): TemplateResult[] {
  const windows: TemplateResult[] = [];
  if (spend.plan) windows.push(planBalanceHtml(spend.plan));
  // A window without a cap has nothing to show: no bar, no phantom countdown.
  // The 5h window runs only when its anchor is live: the backend sends
  // `resets_at` (anchor + 5h) then; without it, no window is running and the
  // next request opens one — there is no countdown ticking on an idle key.
  if (spend.five_hours.cap_usd != null) {
    windows.push(quotaWindowHtml({ title: "5h Window", cap: spend.five_hours.cap_usd, spent: spend.five_hours.spent_usd, targetIso: spend.five_hours.resets_at, idleHint: "no window running" }));
  }
  if (spend.week.cap_usd != null) {
    windows.push(quotaWindowHtml({ title: "Weekly Window", cap: spend.week.cap_usd, spent: spend.week.spent_usd, targetIso: spend.week.resets_at, idleHint: "next window" }));
  }
  return windows;
}

function quotaPanelCard(label: string | null, spend: KeyQuotaStatus): TemplateResult {
  startCountdownTicker();
  return html`<section class="card analytics-quota-card">
    <div class="analytics-card-heading"><div><h3>Usage & limits</h3><p>${label ? `${label} · ` : ""}plan balance · optional window caps · time to reset</p></div></div>
    <div class="family-quota-grid">
      ${quotaWindowList(spend)}
    </div>
  </section>`;
}

function renderQuotaPanel(): TemplateResult {
  if (isFullAdmin()) {
    // Admin: ONLY show the panel for the API key explicitly picked in the
    // toolbar filter. No auto-selection, no key tabs — the picker IS the
    // selection. Nothing selected = nothing rendered.
    if (adminQuotaKeyId === null) return html``;
    if (adminQuota === null || adminQuota.api_key_id !== adminQuotaKeyId) return html``;
    if (!adminQuota.enabled && !adminQuota.spend.plan) {
      return html`<p class="empty">This key has no plan or dollar quotas (unlimited).</p>`;
    }
    const label = adminQuotaKeys?.find((r) => r.api_key_id === adminQuotaKeyId)?.label ?? null;
    return quotaPanelCard(label, adminQuota.spend);
  }

  if (!quotaStatus || (!quotaStatus.enabled && !quotaStatus.plan)) return html``;
  return quotaPanelCard(null, quotaStatus);
}

// ── Ranking list (used by model + provider strips) ──────────────────

function renderRanking(
  title: string,
  subtitle: string,
  items: RankingItem[],
  kind: "models" | "providers",
): TemplateResult {
  const ranked = [...items]
    .sort((a, b) => rankMetricValue(b, rankMetric) - rankMetricValue(a, rankMetric))
    .slice(0, 8);
  const excludedSet = kind === "models" ? excludedModels : excludedProviders;
  const total = items.reduce((sum, item) => sum + rankMetricValue(item, rankMetric), 0);
  const max = ranked.length > 0 ? rankMetricValue(ranked[0]!, rankMetric) : 0;
  return html`<section class="card analytics-ranking-card">
    <div class="analytics-card-heading">
      <div><h3>${title}</h3><p>${subtitle}</p></div>
      ${excludedSet.size > 0 ? html`<button type="button" class="btn btn-sm analytics-rank-clear" @click=${onClearExclusions}>${t("analytics.rank.clear_exclusions", { n: excludedSet.size })}</button>` : nothing}
    </div>
    ${ranked.length === 0 ? html`<p class="empty">${t("analytics.empty.no_usage")}</p>` : html`
      <div class="analytics-ranking-list">
        ${ranked.map((item, index) => {
          const value = rankMetricValue(item, rankMetric);
          const width = max > 0 ? Math.max(2, value / max * 100) : 0;
          const share = total > 0 ? value / total : 0;
          const excluded = excludedSet.has(item.name);
          return html`<div
            class="analytics-ranking-row${excluded ? " is-excluded" : ""}"
            role="button"
            tabindex="0"
            title=${excluded ? t("analytics.rank.include_hint") : t("analytics.rank.exclude_hint")}
            @click=${() => onRankToggle(kind, item.name)}
            @keydown=${(e: KeyboardEvent) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); onRankToggle(kind, item.name); } }}
          >
            <span class="analytics-rank">${index + 1}</span>
            <div class="analytics-ranking-main">
              <div class="analytics-ranking-copy">
                <span class="analytics-ranking-name" title=${item.name}>${item.name}</span>
                ${excluded ? html`<span class="analytics-rank-excluded-badge">${t("analytics.rank.excluded_badge")}</span>` : nothing}
                <span class="analytics-ranking-context">${item.context ? `${item.context} · ` : ""}${fmtNumber(item.requests)} req</span>
              </div>
              <div class="analytics-ranking-value"><strong>${formatRankValue(rankMetric, value)}</strong><span>${fmtPercent(share)}</span></div>
              <div class="analytics-ranking-track" aria-hidden="true"><span style=${`width:${width}%`}></span></div>
            </div>
          </div>`;
        })}
      </div>`}
  </section>`;
}

// ── Status health card ──────────────────────────────────────────────

function renderStatusHealth(): TemplateResult {
  const buckets = groupByStatus(byStatus);
  const entries = [
    { label: "2xx", value: buckets.s2xx, cls: "ok" },
    { label: "4xx", value: buckets.s4xx, cls: "warn" },
    { label: "5xx", value: buckets.s5xx, cls: "err" },
    { label: t("analytics.status.other"), value: buckets.other, cls: "other" },
  ];
  const total = entries.reduce((sum, item) => sum + item.value, 0);
  const successRate = summary && summary.unique_requests > 0
    ? summary.winners / summary.unique_requests
    : (total > 0 ? buckets.s2xx / total : NaN);
  const health = !Number.isFinite(successRate)
    ? t("analytics.status.no_data")
    : successRate >= 0.95 ? t("analytics.status.healthy")
    : successRate >= 0.85 ? t("analytics.status.attention")
    : t("analytics.status.degraded");
  const healthTone = !Number.isFinite(successRate) ? "neutral" : successRate >= 0.95 ? "good" : successRate >= 0.85 ? "warn" : "bad";
  return html`<section class="card analytics-status-card">
    <div class="analytics-card-heading"><div><h3>${t("analytics.chart.status_codes")}</h3><p>${t("analytics.chart.status_codes.subtitle")}</p></div><span class="analytics-health-label ${healthTone}">${health}</span></div>
    <div class="analytics-status-hero"><strong>${fmtPercent(successRate)}</strong><span>${t("analytics.metric.success_rate")}</span></div>
    <div class="analytics-status-track" aria-label=${t("analytics.chart.status_codes")}>
      ${entries.map((item) => html`<span class=${item.cls} style=${`width:${total > 0 ? item.value / total * 100 : 0}%`}></span>`)}
    </div>
    <div class="analytics-status-legend">
      ${entries.map((item) => html`<div><span class="analytics-status-dot ${item.cls}"></span><span>${item.label}</span><strong>${fmtNumber(item.value)}</strong><small>${fmtPercent(total > 0 ? item.value / total : NaN)}</small></div>`)}
    </div>
  </section>`;
}

// ── Latency percentiles card ────────────────────────────────────────

function renderLatencyBlock(): TemplateResult {
  const rows = [
    { label: t("analytics.latency.connect"), p50: latency?.p50_connect_ms, p95: latency?.p95_connect_ms },
    { label: t("analytics.latency.ttft"), p50: latency?.p50_ttft_ms, p95: latency?.p95_ttft_ms },
    { label: t("analytics.latency.total"), p50: latency?.p50_total_ms, p95: latency?.p95_total_ms },
  ];
  const max = Math.max(1, ...rows.map((row) => row.p95 ?? 0));
  return html`<section class="card analytics-latency-card">
    <div class="analytics-card-heading"><div><h3>${t("analytics.chart.latency")}</h3><p>${t("analytics.chart.latency.subtitle")} · ${fmtNumber(latency?.samples ?? 0)} ${t("analytics.latency.samples").toLowerCase()}</p></div></div>
    <div class="analytics-latency-key"><span class="p50">p50</span><span class="p95">p95</span></div>
    <div class="analytics-latency-list">
      ${rows.map((row) => html`<div class="analytics-latency-row">
        <div class="analytics-latency-row-head"><strong>${row.label}</strong><span><small>${fmtDuration(row.p50)}</small><b>${fmtDuration(row.p95)}</b></span></div>
        <div class="analytics-latency-track">
          <span class="p95" style=${`width:${(row.p95 ?? 0) / max * 100}%`}></span>
          <span class="p50" style=${`width:${(row.p50 ?? 0) / max * 100}%`}></span>
        </div>
      </div>`)}
    </div>
    <div class="analytics-throughput-foot"><span>${t("analytics.metric.generation_speed")}</span><strong>p50 ${latency?.p50_tokens_per_sec == null ? "—" : fmtNumber(latency.p50_tokens_per_sec)} tok/s</strong><strong>p95 ${latency?.p95_tokens_per_sec == null ? "—" : fmtNumber(latency.p95_tokens_per_sec)} tok/s</strong></div>
  </section>`;
}

// ── Race outcomes card ──────────────────────────────────────────────

function renderRaceBlock(): TemplateResult {
  const total = races?.total_races ?? 0;
  const winners = races?.winners ?? 0;
  const losers = races?.losers ?? 0;
  const completionRate = total > 0 ? winners / total : NaN;
  const extraAttempts = total > 0 ? losers / total : 0;
  return html`<section class="card analytics-race-card">
    <div class="analytics-card-heading"><div><h3>${t("analytics.chart.race_outcomes")}</h3><p>${t("analytics.chart.race_outcomes.subtitle")}</p></div></div>
    <div class="analytics-race-grid">
      <div><span>${t("analytics.chart.race_outcomes.total")}</span><strong>${fmtNumber(total)}</strong></div>
      <div><span>${t("analytics.race.completion")}</span><strong>${fmtPercent(completionRate)}</strong></div>
      <div><span>${t("analytics.race.avg_winner_position")}</span><strong>${races?.avg_winner_position == null ? "—" : `#${races.avg_winner_position.toFixed(1)}`}</strong></div>
      <div><span>${t("analytics.race.extra_attempts")}</span><strong>${total > 0 ? extraAttempts.toFixed(1) : "—"}</strong></div>
    </div>
    <div class="analytics-race-progress"><span style=${`width:${Number.isFinite(completionRate) ? Math.min(100, completionRate * 100) : 0}%`}></span></div>
    <p class="analytics-race-note">${fmtNumber(winners)} ${t("analytics.race.winners")} · ${fmtNumber(losers)} ${t("analytics.race.cancelled_contenders")}</p>
  </section>`;
}

// ── Body composition ────────────────────────────────────────────────

// The details section holds ~4k nodes (two dense grids + the monthly
// matrix). Rendering it while collapsed paid a full template diff on every
// live-update tick, so the body mounts only when first expanded and stays.
let detailsExpanded = false;

function onDetailsToggle(e: Event): void {
  const el = e.target as HTMLElement;
  if (!el.classList.contains("analytics-details") || detailsExpanded) return;
  detailsExpanded = true;
  requestUpdate();
}

function renderBody(): TemplateResult {
  if (loading) return html`<div class="loading">${t("common.loading")}</div>`;
  if (errorMsg) return html`<div class="banner banner-error">${errorMsg}</div>`;
  if (!summary) return html`<div class="loading">${t("common.loading")}</div>`;

  const nullPricingCount = summary.rows_with_null_pricing ?? 0;
  const nullPricingBanner = isFullAdmin() && nullPricingCount > 0
    ? html`<div class="banner banner-warning">${t("analytics.null_pricing_warning", { count: nullPricingCount })}</div>`
    : html``;

  const modelItems: RankingItem[] = byModel.map((row) => ({
    name: row.upstream_model_id,
    context: row.provider_id,
    requests: row.unique_requests,
    tokens: row.total_prompt_tokens + row.total_completion_tokens,
    cost: row.total_cost_usd,
  }));
  const providerItems: RankingItem[] = byProvider.map((row) => ({
    name: row.provider_id,
    context: `${fmtNumber(row.total_prompt_tokens + row.total_completion_tokens)} tok`,
    requests: row.unique_requests,
    tokens: row.total_prompt_tokens + row.total_completion_tokens,
    cost: row.total_cost_usd,
  }));
  const rankSubtitle = t("analytics.rank.by", { metric: t(RANK_LABEL_KEYS[rankMetric]) });

  const trendCard = html`<section class="card analytics-trend-card">
    <div class="analytics-card-heading"><div><h3>${t("analytics.chart.daily_usage")}</h3><p>${t("analytics.chart.daily_usage.subtitle")}</p></div></div>
    <div class="analytics-chart-container" id="chart-daily-usage"></div>
  </section>`;
  const rankHead = html`<div class="analytics-rank-head">
    <span class="analytics-rank-head-label">${t("analytics.rank.label")}</span>
    ${renderRankMetricSwitch()}
  </div>`;

  // Customer dashboard: spend, the daily trend and the per-model split. Status
  // distribution, latency percentiles, race outcomes, combo health and the
  // detailed tables are operator telemetry; the server does not send their
  // payloads to a customer key at all, so nothing here may depend on them.
  if (!isFullAdmin()) {
    return html`
      ${nullPricingBanner}
      ${renderMetrics()}
      ${renderQuotaPanel()}
      <div class="analytics-primary-grid">
        ${trendCard}
      </div>
      <div class="analytics-insight-grid">
        ${rankHead}
        ${renderRanking(t("analytics.chart.by_model"), rankSubtitle, modelItems, "models")}
      </div>`;
  }

  return html`
    ${nullPricingBanner}
    ${renderMetrics()}
    ${renderQuotaPanel()}
    <div class="analytics-primary-grid">
      ${trendCard}
      ${renderStatusHealth()}
    </div>
    <div class="analytics-insight-grid">
      ${rankHead}
      ${renderRanking(t("analytics.chart.by_model"), rankSubtitle, modelItems, "models")}
      ${isFullAdmin()
        ? renderRanking(t("analytics.chart.by_provider"), rankSubtitle, providerItems, "providers")
        : nothing}
    </div>
    <div class="analytics-insight-grid">
      ${renderLatencyBlock()}
      ${renderRaceBlock()}
    </div>
    ${renderComboHealth()}
    <details class="analytics-details" .open=${detailsExpanded} @toggle=${onDetailsToggle}>
      <summary><span>${t("analytics.details.title")}</span><small>${t("analytics.details.subtitle")}</small></summary>
      ${detailsExpanded ? html`
      <div class="analytics-details-body">
        ${renderByModelTable()}
        ${renderByProviderTable()}
        ${renderMonthlyMatrix()}
      </div>` : nothing}
    ${renderRecentErrors()}`;
}

function renderAnalytics(): TemplateResult {
  // Post-commit self-heal: if this re-render removed the uPlot canvas from
  // its container (lit re-created the chart div), rebuild it. Runs after the
  // render() call in mountView commits, before the browser paints.
  queueMicrotask(() => {
    if (mountedEl?.isConnected) ensureAnalyticsChartsAlive();
  });
  const { preset, providerId, apiKeyId, modelId, keyKind } = parseHashParams();
  return html`
    <div class="analytics-dashboard">
      <div class="page-header analytics-header">
        <div class="analytics-title-wrap">
          <h2 class="view-identity" title=${t("analytics.subtitle")}>${t("analytics.title")}</h2>
        </div>
      </div>
      ${renderAnalyticsToolbar(providerId, apiKeyId, modelId, preset, keyKind)}
      ${renderBody()}
    </div>`;
}

// ── Mount ───────────────────────────────────────────────────────────

export async function mountAnalytics(
  target?: HTMLElement | null | string,
): Promise<(() => void) | void> {
  const el = (target instanceof HTMLElement) ? target : document.getElementById("main");
  if (!el || !el.isConnected) return;

  mountedEl = el;
  // Fresh start: the previous mount's charts died with its cleanup fn.
  setViewState({
    loading: true,
    errorMsg: null,
  });
  // A hash change remounts the view: clear any popover left anchored to a
  // trigger rect that no longer exists.
  resetAnalyticsMenus();
  const cleanupReactive = mountView(el, renderAnalytics);
  document.addEventListener("themechange", refreshAnalyticsChartTheme);

  const { rank } = parseHashParams();
  setRankMetric(rank);
  if (!(await fetchAndApply(el, { showLoading: true }))) {
    cleanupReactive();
    mountedEl = null;
    return () => {
      stopCountdownTicker();
      destroyAnalyticsCharts();
      document.removeEventListener("themechange", refreshAnalyticsChartTheme);
    };
  }
  return () => {
    stopCountdownTicker();
    destroyAnalyticsCharts();
    document.removeEventListener("themechange", refreshAnalyticsChartTheme);
    cleanupReactive();
    if (mountedEl === el) mountedEl = null;
  };
}

// ── Data fetch (mount + ranking exclusion toggles) ──────────────────

let mountedEl: HTMLElement | null = null;

/** Fetch every analytics payload and paint. Returns `false` when the view
 *  was unmounted mid-flight (the caller must run its cleanup). */
async function fetchAndApply(
  el: HTMLElement,
  opts: { showLoading: boolean } = { showLoading: false },
): Promise<boolean> {
  if (opts.showLoading) setViewState({ loading: true, errorMsg: null });
  const { preset, providerId, apiKeyId, modelId, keyKind } = parseHashParams();
  try {
    // Shared by every `/usage/*` fetch; errors adds `limit=10` to cap the table.
    // Ranking exclusions ride along on every aggregate.
    const modelFilter = modelId ? { model_id: modelId } : {};
    const usageQ = buildUsageQuery(preset, providerId, apiKeyId, { ...modelFilter, ...keyKindExtra(keyKind), ...exclusionsExtra() });
    const errorsQ = buildUsageQuery(preset, providerId, apiKeyId, { limit: "10", ...modelFilter, ...keyKindExtra(keyKind), ...exclusionsExtra() });
    const [
      summaryResp, byModelResp, byProviderResp, monthlyByProviderResp, latencyResp, racesResp,
      byDayResp, byStatusResp, byComboResp, errorsResp, providersResp, apiKeysResp, kindsResp,
    ] = await Promise.all([
      api(`/usage/summary${usageQ}`) as Promise<UsageSummary>,
      api(`/usage/by-model${usageQ}`) as Promise<ByModelRow[]>,
      // Analytics-scope keys: by-provider collapses onto the same public
      // names as by-model, so the redundant list (and its fetch) is skipped.
      isFullAdmin()
        ? (api(`/usage/by-provider${usageQ}`) as Promise<ByProviderRow[]>)
        : Promise.resolve([] as ByProviderRow[]),
      // Operator-only telemetry. A customer key is not allowed on these
      // routes at all, so it must not request them: an unlisted route answers
      // 403 and would turn a healthy dashboard into an error banner.
      isFullAdmin()
        ? (api(`/usage/monthly-by-provider${usageQ}`) as Promise<MonthlyByProviderRow[]>)
        : Promise.resolve([] as MonthlyByProviderRow[]),
      isFullAdmin()
        ? (api(`/usage/latency${usageQ}`) as Promise<LatencyPayload>)
        : Promise.resolve(null as LatencyPayload | null),
      isFullAdmin()
        ? (api(`/usage/races${usageQ}`) as Promise<RaceStatsPayload>)
        : Promise.resolve(null as RaceStatsPayload | null),
      api(`/usage/by-day${usageQ}`) as Promise<ByDayRow[]>,
      isFullAdmin()
        ? (api(`/usage/by-status${usageQ}`) as Promise<ByStatusRow[]>)
        : Promise.resolve([] as ByStatusRow[]),
      isFullAdmin()
        ? (api(`/usage/by-combo${usageQ}`) as Promise<ByComboRow[]>)
        : Promise.resolve([] as ByComboRow[]),
      isFullAdmin()
        ? (api(`/usage/errors${errorsQ}`) as Promise<ErrorRow[]>)
        : Promise.resolve([] as ErrorRow[]),
      // Prefer the bg-poll's cache; fall back to a direct fetch on a cold paint.
      // Non-admin keys cannot access /providers or /keys (they are strictly forbidden).
      isFullAdmin()
        ? ((state.providers && state.providers.length)
            ? Promise.resolve(state.providers)
            : api("/providers") as Promise<Provider[]>)
        : Promise.resolve([] as Provider[]),
      isFullAdmin()
        ? ((state.apiKeys && state.apiKeys.length)
            ? Promise.resolve(state.apiKeys as ApiKeyFilterRow[])
            : api("/keys") as Promise<ApiKeyFilterRow[]>)
        : Promise.resolve(
            getProfile()?.key_id != null
              ? ([{
                  id: getProfile()!.key_id!,
                  key_prefix: getProfile()!.key_prefix,
                  label: getProfile()!.label,
                }] as ApiKeyFilterRow[])
              : ([] as ApiKeyFilterRow[])
          ),
      // Ownership scope for the keys dropdown; also refreshes the shared kind
      // cache so `kindOf(id)` stays right even if another view rewrites
      // `state.apiKeys` later.
      isFullAdmin()
        ? loadKeyKinds()
        : Promise.resolve([] as { id: number; kind: string }[]),
    ]);

    if (!el.isConnected) return false;

    const kindById = new Map<number, string>(kindsResp.map((r) => [r.id, r.kind]));
    const keysScoped: ApiKeyFilterRow[] = apiKeysResp.map((k) => ({
      ...k,
      kind: kindById.get(k.id) ?? k.kind ?? "personal",
    }));

    setViewState({
      summary: summaryResp,
      byModel: byModelResp,
      byProvider: byProviderResp,
      monthlyByProvider: monthlyByProviderResp,
      latency: latencyResp,
      races: racesResp,
      byDay: byDayResp,
      byStatus: byStatusResp,
      byCombo: byComboResp,
      errors: errorsResp,
      providers: providersResp,
      apiKeys: keysScoped,
      loading: false,
    });

    if (providersResp) state.providers = providersResp;
    if (keysScoped.length) state.apiKeys = keysScoped as typeof state.apiKeys;

    // Dollar quotas: clients read their own status; admins get the picked
    // key's live spend. No picker selection = no panel at all.
    if (isFullAdmin()) {
      const selectedKeyId = apiKeyId ? Number(apiKeyId) : NaN;
      void loadAdminQuotaKeys();
      if (Number.isNaN(selectedKeyId)) {
        adminQuotaKeyId = null;
        adminQuota = null;
      } else if (adminQuotaKeyId !== selectedKeyId) {
        void loadAdminKeyQuota(selectedKeyId);
      }
    } else {
      void loadQuotaStatus();
      if (adminQuotaKeys !== null) {
        adminQuotaKeys = null;
        adminQuota = null;
        adminQuotaKeyId = null;
      }
    }

    requestUpdate();
    // rAF runs after the `requestUpdate()` re-render paints, so the chart
    // containers exist. `createAnalyticsCharts` is idempotent.
    requestAnimationFrame(() => {
      if (!el.isConnected) return;
      createAnalyticsCharts();
    });
    return true;
  } catch (e: unknown) {
    if (!el.isConnected) return false;
    setViewState({
      errorMsg: e instanceof Error ? e.message : String(e),
      providers: state.providers || [],
      apiKeys: (state.apiKeys || []) as ApiKeyFilterRow[],
      loading: false,
    });
    requestUpdate();
    return true;
  }
}

/** Toggle a ranking entry out of (or back into) every aggregate and refresh. */
function onRankToggle(kind: "models" | "providers", id: string): void {
  toggleRankExclusion(kind, id);
  const el = mountedEl;
  if (el && el.isConnected) void fetchAndApply(el, { showLoading: false });
}

function onClearExclusions(): void {
  clearRankExclusions();
  const el = mountedEl;
  if (el && el.isConnected) void fetchAndApply(el, { showLoading: false });
}
