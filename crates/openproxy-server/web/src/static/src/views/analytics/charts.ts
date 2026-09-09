// views/analytics/charts.ts — chart instances + lifecycle, preset selector
// toolbar, and the chart/data-binding glue. uPlot daily-usage chart and
// the time-range preset buttons live here. The view-local `charts`
// reference is held in `shared.ts`; this module owns the
// create/destroy cycle and the post-render `requestAnimationFrame` call
// that re-creates the chart when the data refreshes.

import { html, type TemplateResult } from "lit-html";
import {
  buildDailyUsageChart,
  observeResize,
} from "../../components/uplot-chart/index.js";
import { t } from "../../i18n/index.js";
import {
  byDay,
  charts,
  dailyUsageData,
  PRESETS,
  PRESET_LABEL_KEYS,
  providers,
  apiKeys,
  setCharts,
  setHashParams,
  type AnalyticsCharts,
} from "./shared.js";

/** Create the uPlot chart instance after the first data-bearing
 *  render. Idempotent — no-op if `charts` is already set. */
export function createAnalyticsCharts(): void {
  if (charts) return;

  const dailyEl: HTMLElement | null = document.getElementById("chart-daily-usage");
  if (!dailyEl) {
    // Containers not in the DOM yet — the loading state hasn't
    // cleared, or the render hasn't committed. The caller should
    // defer via requestAnimationFrame.
    return;
  }

  const resizeDisposers: Array<() => void> = [];
  const dailyUsage = buildDailyUsageChart(dailyEl);
  resizeDisposers.push(observeResize(dailyUsage, dailyEl));

  const instance: AnalyticsCharts = { dailyUsage, resizeDisposers };
  setCharts(instance);

  // Push the current data into the new chart immediately.
  dailyUsage.setData(dailyUsageData(byDay));
}

/** Destroy all uPlot instances + disconnect their ResizeObservers.
 *  Called on view unmount. */
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

/** Filter bar with provider / API-key selects + preset range. */
export function renderAnalyticsToolbar(
  providerId: string,
  apiKeyId: string,
  preset: string,
): TemplateResult {
  const hasFilters = providerId !== "" || apiKeyId !== "";
  return html`<section class="analytics-toolbar" aria-label=${t("analytics.filters_label")}>
    <div class="analytics-filter-row">
      <label class="analytics-filter-field" for="analytics-provider-filter">
        <span>${t("analytics.filter.provider")}</span>
        <select id="analytics-provider-filter" class="filter-dropdown" .value=${providerId} @change=${(e: Event) => setHashParams({ providerId: (e.target as HTMLSelectElement).value })}>
          <option value="">${t("analytics.filter.all_providers")}</option>
          ${providers.map((p) => html`<option value=${p.id}>${p.name}</option>`)}
        </select>
      </label>
      <label class="analytics-filter-field" for="analytics-key-filter">
        <span>${t("analytics.filter.api_key")}</span>
        <select id="analytics-key-filter" class="filter-dropdown" .value=${apiKeyId} @change=${(e: Event) => setHashParams({ apiKeyId: (e.target as HTMLSelectElement).value })}>
          <option value="">${t("analytics.filter.all_api_keys")}</option>
          ${apiKeys.map((k) => html`<option value=${String(k.id)}>${k.label || k.key_prefix || "—"}${k.key_prefix ? ` · ${k.key_prefix}` : ""}</option>`)}
        </select>
      </label>
      ${hasFilters ? html`<button class="analytics-clear-btn" type="button" @click=${() => setHashParams({ providerId: "", apiKeyId: "" })}>${t("analytics.filter.clear")}</button>` : html``}
    </div>
    <div class="analytics-range-row">
      <span class="analytics-range-label">${t("analytics.range_label")}</span>
      ${renderPresetSelector(preset)}
    </div>
  </section>`;
}
