// views/analytics/charts.ts — uPlot instances + lifecycle, preset/toolbar templates.

import { html, type TemplateResult } from "lit-html";
import { live } from "lit-html/directives/live.js";
import {
  buildDailyUsageChart,
  observeResize,
} from "../../components/uplot-chart/index.js";
import { t } from "../../i18n/index.js";
import { isFullAdmin } from "../../state/auth.js";
import { requestUpdate } from "../../state/reactive.js";
import {
  byDay,
  byModel,
  charts,
  dailyUsageData,
  PRESETS,
  PRESET_LABEL_KEYS,
  providers,
  apiKeys,
  rankMetric,
  RANK_LABEL_KEYS,
  RANK_METRICS,
  setCharts,
  setHashParams,
  setRankMetric,
  type AnalyticsCharts,
} from "./shared.js";

/** Create the uPlot chart instance after the first data-bearing render. Idempotent. */
export function createAnalyticsCharts(): void {
  if (charts) return;

  const dailyEl: HTMLElement | null = document.getElementById("chart-daily-usage");
  if (!dailyEl) {
    // Containers absent: the render hasn't committed. Caller defers via rAF.
    return;
  }

  const resizeDisposers: Array<() => void> = [];
  const dailyUsage = buildDailyUsageChart(dailyEl);
  resizeDisposers.push(observeResize(dailyUsage, dailyEl));

  const instance: AnalyticsCharts = { dailyUsage, resizeDisposers };
  setCharts(instance);

  dailyUsage.setData(dailyUsageData(byDay));
}

/** Destroy all uPlot instances + disconnect their ResizeObservers. */
export function destroyAnalyticsCharts(): void {
  if (!charts) return;
  for (const disposer of charts.resizeDisposers) {
    try { disposer(); } catch (e: unknown) {
      console.warn("[analytics] resize disposer threw:", e);
    }
  }
  try { charts.dailyUsage.destroy(); } catch (e: unknown) {
    console.warn("[analytics] dailyUsage.destroy threw:", e);
  }
  setCharts(null);
}

/** Re-create the charts if a re-render removed their canvas from the
 *  container: uPlot is mounted imperatively, outside lit's diff, so any
 *  template re-creation of `#chart-daily-usage` silently kills the canvas.
 *  Called after every view render; cheap when everything is alive. */
export function ensureAnalyticsChartsAlive(): void {
  const dailyEl: HTMLElement | null = document.getElementById("chart-daily-usage");
  if (!dailyEl || !dailyEl.isConnected) return;
  if (charts && dailyEl.querySelector(".uplot") !== null) return; // alive
  if (charts) destroyAnalyticsCharts();
  const dailyUsage = buildDailyUsageChart(dailyEl);
  const disposer = observeResize(dailyUsage, dailyEl);
  setCharts({ dailyUsage, resizeDisposers: [disposer] });
  dailyUsage.setData(dailyUsageData(byDay));
}

/** Recreate chart instances after a theme change. */
export function refreshAnalyticsChartTheme(): void {
  destroyAnalyticsCharts();
  requestAnimationFrame(() => createAnalyticsCharts());
}

// ── Toolbar templates ───────────────────────────────────────────────

/** Time-range preset selector (Today / 7d / 30d / …). */
export function renderPresetSelector(active: string): TemplateResult {
  return html`<div class="preset-selector" role="group" aria-label=${t("analytics.range_label")}>
    ${PRESETS.map((p) => html`<button
      class="preset-btn${p === active ? " active" : ""}"
      type="button"
      aria-pressed=${p === active ? "true" : "false"}
      @click=${() => setHashParams({ preset: p })}
    >${t(PRESET_LABEL_KEYS[p])}</button>`)}
  </div>`;
}

/** Filter bar with provider / API-key / model selects + preset range. */
export function renderAnalyticsToolbar(
  providerId: string,
  apiKeyId: string,
  modelId: string,
  preset: string,
): TemplateResult {
  const hasFilters = providerId !== "" || apiKeyId !== "" || modelId !== "";
  return html`<section class="analytics-toolbar" aria-label=${t("analytics.filters_label")}>
    <div class="analytics-filter-row">
      <label class="analytics-filter-field" for="analytics-provider-filter">
        <span>${t("analytics.filter.provider")}</span>
        <select id="analytics-provider-filter" class="filter-dropdown" .value=${providerId} @change=${(e: Event) => setHashParams({ providerId: (e.target as HTMLSelectElement).value })}>
          <option value="">${t("analytics.filter.all_providers")}</option>
          ${providers.map((p) => html`<option value=${p.id}>${p.name}</option>`)}
        </select>
      </label>
      ${isFullAdmin() ? html`
      <label class="analytics-filter-field" for="analytics-key-filter">
        <span>${t("analytics.filter.api_key")}</span>
        <select id="analytics-key-filter" class="filter-dropdown" .value=${live(apiKeyId)} @change=${(e: Event) => setHashParams({ apiKeyId: (e.target as HTMLSelectElement).value })}>
          <option value="" ?selected=${apiKeyId === ""}>${t("analytics.filter.all_api_keys")}</option>
          ${apiKeys.map((k) => html`<option value=${String(k.id)} ?selected=${apiKeyId === String(k.id)}>${k.label || k.key_prefix || "—"}${k.key_prefix ? ` · ${k.key_prefix}` : ""}</option>`)}
        </select>
      </label>` : html``}
      <div class="analytics-filter-field analytics-model-filter">
        <span>${t("analytics.filter.model")}</span>
        <button type="button" class="filter-dropdown analytics-model-trigger"
                aria-haspopup="listbox" aria-expanded=${modelMenuOpen ? "true" : "false"}
                @click=${(e: Event) => toggleModelMenu(e.currentTarget as HTMLElement)}>
          <span class="analytics-model-trigger-label">${modelId || t("analytics.filter.all_models")}</span>
          <small aria-hidden="true">▾</small>
        </button>
      </div>
      ${hasFilters ? html`<button class="analytics-clear-btn" type="button" @click=${() => setHashParams({ providerId: "", apiKeyId: "", modelId: "" })}>${t("analytics.filter.clear")}</button>` : html``}
    </div>
    <div class="analytics-range-row">
      <span class="analytics-range-label">${t("analytics.range_label")}</span>
      ${renderPresetSelector(preset)}
    </div>
    ${modelMenuOpen && modelMenuPos ? renderModelMenu(modelId) : html``}
  </section>`;
}

/** Segmented control that switches the ranking metric without refetching. */
export function renderRankMetricSwitch(): TemplateResult {
  return html`<div class="analytics-rank-switch preset-selector" role="group" aria-label=${t("analytics.rank.label")}>
    ${RANK_METRICS.map((m) => html`<button
      class="preset-btn${m === rankMetric ? " active" : ""}"
      type="button"
      aria-pressed=${m === rankMetric ? "true" : "false"}
      @click=${() => { setRankMetric(m); requestUpdate(); }}
    >${t(RANK_LABEL_KEYS[m])}</button>`)}
  </div>`;
}

// ── Model filter popover ────────────────────────────────────────────
// Themed popover cloned from the combos account-menu pattern
// (position:fixed + viewport clamp + backdrop close). The option universe
// comes from the live by-model rows, not the global catalog: only models
// with usage in the current range are offered.

let modelMenuOpen = false;
let modelMenuPos: { top: number; left: number } | null = null;

/** Clear popover state without rendering. Called on every view mount so a
 *  hash change (remount) never leaves a stale menu anchored to a dead rect. */
export function resetAnalyticsMenus(): void {
  modelMenuOpen = false;
  modelMenuPos = null;
}

interface ModelOption {
  id: string;
  providers: string[];
}

function modelOptions(): ModelOption[] {
  const byId = new Map<string, Set<string>>();
  for (const r of byModel) {
    if (!r.upstream_model_id) continue;
    let provs = byId.get(r.upstream_model_id);
    if (!provs) { provs = new Set(); byId.set(r.upstream_model_id, provs); }
    provs.add(r.provider_id);
  }
  return [...byId.entries()]
    .map(([id, provs]) => ({ id, providers: [...provs].sort() }))
    .sort((a, b) => a.id.localeCompare(b.id));
}

function closeModelMenu(): void {
  resetAnalyticsMenus();
  requestUpdate();
}

function toggleModelMenu(trigger: HTMLElement): void {
  if (modelMenuOpen) { closeModelMenu(); return; }
  const rect = trigger.getBoundingClientRect();
  const menuWidth = 300;
  const menuMaxHeight = 280;
  const margin = 8;
  let left = rect.left;
  if (left + menuWidth > window.innerWidth - margin) {
    left = Math.max(margin, window.innerWidth - menuWidth - margin);
  }
  let top = rect.bottom + 4;
  if (top + menuMaxHeight > window.innerHeight - margin) {
    const spaceAbove = rect.top - margin;
    if (spaceAbove > 150) top = Math.max(margin, rect.top - menuMaxHeight - 4);
  }
  modelMenuPos = { top, left };
  modelMenuOpen = true;
  requestUpdate();
}

function renderModelMenu(modelId: string): TemplateResult {
  const options = modelOptions();
  return html`
    <div class="analytics-model-backdrop" @click=${() => closeModelMenu()}></div>
    <div class="analytics-model-menu" role="listbox"
         style=${`top:${modelMenuPos!.top}px; left:${modelMenuPos!.left}px;`}
         @click=${(e: Event) => e.stopPropagation()}>
      <button type="button" role="option" aria-selected=${modelId === "" ? "true" : "false"}
              class="analytics-model-item${modelId === "" ? " active" : ""}"
              @click=${() => { closeModelMenu(); setHashParams({ modelId: "" }); }}>
        <span class="analytics-model-check">${modelId === "" ? "✓" : ""}</span>
        <span>${t("analytics.filter.all_models")}</span>
      </button>
      ${options.length === 0
        ? html`<div class="analytics-model-empty">${t("analytics.filter.model_empty")}</div>`
        : options.map((o) => html`
          <button type="button" role="option" aria-selected=${modelId === o.id ? "true" : "false"}
                  class="analytics-model-item${modelId === o.id ? " active" : ""}"
                  title=${o.id}
                  @click=${() => { closeModelMenu(); setHashParams({ modelId: o.id }); }}>
            <span class="analytics-model-check">${modelId === o.id ? "✓" : ""}</span>
            <span class="analytics-model-item-main">
              <strong>${o.id}</strong>
              <small>${o.providers.join(", ")}</small>
            </span>
          </button>`)}
    </div>`;
}
