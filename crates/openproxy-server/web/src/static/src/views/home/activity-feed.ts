// views/home/activity-feed.ts — recent rows list + scroll preservation.
//
// Renders the last 20 usage rows in a scrollable list with scroll
// position preservation across re-renders (new rows prepend at top
// without scrolling the user away from their position).

import { html, type TemplateResult } from "lit-html";
import { ref } from "lit-html/directives/ref.js";

import { t } from "../../i18n/index.js";
import { statusPillClass } from "../../lib/constants.js";
import type { RecentUsageRow } from "../../lib/types/api.js";
import type { Snapshot, SavedScroll } from "./types.js";
import {
  formatLatency,
  formatCost,
  formatTokensInOut,
} from "./kpis.js";

// ==========
// Scroll state
// ==========

/** The activity feed scroll container. Captured via `ref` directive so
 *  we can save/restore its scrollTop across lit-html re-renders. */
let activityFeedEl: HTMLElement | null = null;

/** Saved scroll state for the activity feed. Set in the subscriber
 *  callback (before lit-html render), restored in the post-render
 *  `requestAnimationFrame`. */
let savedScroll: SavedScroll | null = null;

/** Callback ref that captures the activity feed scroll container. */
function activityFeedRef(el: Element | undefined): void {
  activityFeedEl = el instanceof HTMLElement ? el : null;
}

// ==========
// Row rendering
// ==========

function formatTime(iso: string): string {
  if (!iso) return "—";
  const d: Date = new Date(iso);
  if (Number.isNaN(d.getTime())) return "—";
  const hh: string = String(d.getHours()).padStart(2, "0");
  const mm: string = String(d.getMinutes()).padStart(2, "0");
  const ss: string = String(d.getSeconds()).padStart(2, "0");
  return `${hh}:${mm}:${ss}`;
}

function renderActivityRow(r: RecentUsageRow): TemplateResult {
  const cls: string = statusPillClass(r.status_code);
  const timeStr: string = formatTime(r.created_at);
  const tokensStr: string = formatTokensInOut(r.prompt_tokens, r.completion_tokens);
  const latencyStr: string = formatLatency(r.total_ms);
  const cost: number = r.cost_usd ?? 0;
  const provider: string = r.provider_id || "—";
  const model: string = r.upstream_model_id || "—";
  const provModel: string = `${provider} / ${model}`;

  return html`<a class="home-activity-row" href=${`#/logs?request_id=${encodeURIComponent(r.request_id || "")}`} data-id=${r.id}>
    <div class="home-desktop-cells">
      <span class="home-activity-time">${timeStr}</span>
      <span class="home-activity-model" title="${model}">${model}</span>
      <span class="home-activity-provider" title="${provider}">${provider}</span>
      <span class="home-activity-status"><span class="status-pill ${cls}">${r.status_code ?? "—"}</span></span>
      <span class="home-activity-latency">${latencyStr}</span>
      <span class="home-activity-tokens">${tokensStr}</span>
      <span class="home-activity-cost">${formatCost(r.cost_usd)}</span>
    </div>
    <div class="home-mobile-card">
      <div class="home-m-line-1">
        <div class="home-m-left">
          <span class="home-activity-status"><span class="status-pill ${cls}">${r.status_code ?? "—"}</span></span>
          <span class="home-activity-time">${timeStr}</span>
        </div>
        <div class="home-m-right">
          <span class="home-activity-tokens">${tokensStr}</span>
          <span class="home-activity-latency">${latencyStr}</span>
          ${cost > 0 ? html`<span class="home-activity-cost">${formatCost(cost)}</span>` : ""}
        </div>
      </div>
      <div class="home-m-line-2">
        <div class="home-m-left">
          <span class="home-activity-model-prov" title="${provModel}">
            <span class="prov-label">${provider}</span> / ${model}
          </span>
        </div>
      </div>
    </div>
  </a>`;
}

// ==========
// Feed rendering
// ==========

/** Activity feed — scrollable list of the last 20 rows. Uses `repeat`
 *  with row id as the key so lit-html reuses DOM nodes (essential for
 *  scroll preservation: existing rows keep their identity, new rows are
 *  inserted at the top). */
export function renderActivityFeed(snapshot: Snapshot | null): TemplateResult {
  const rows: RecentUsageRow[] = snapshot ? snapshot.recentRows : [];
  const body: TemplateResult = rows.length === 0
    ? html`<div class="home-activity-empty muted">${t("home.activity_feed.empty")}</div>`
    : html`<div class="home-activity-list" ${ref(activityFeedRef)}>
        ${rows.map((r: RecentUsageRow) => renderActivityRow(r))}
      </div>`;

  return html`<section class="card home-activity-card">
    <div class="home-card-heading"><div><h3>${t("home.activity_feed")}</h3><p>${t("home.activity_feed.subtitle")}</p></div><a href="#/logs">${t("home.activity_feed.view_all")} →</a></div>
    <div class="home-activity-header">
      <span>${t("home.activity_feed.col_time") || "Time"}</span>
      <span>${t("home.activity_feed.col_model") || "Model"}</span>
      <span>${t("home.activity_feed.col_provider") || "Provider"}</span>
      <span>${t("home.activity_feed.col_status") || "Status"}</span>
      <span>${t("home.activity_feed.col_latency") || "Latency"}</span>
      <span>${t("home.activity_feed.col_tokens") || "Tokens (in/out)"}</span>
      <span>${t("home.activity_feed.col_cost") || "Cost"}</span>
    </div>
    ${body}
  </section>`;
}

// ==========
// Scroll preservation
// ==========

/** Save the activity feed scroll state BEFORE a lit-html re-render. */
export function saveActivityScroll(): void {
  if (activityFeedEl) {
    savedScroll = {
      scrollTop: activityFeedEl.scrollTop,
      scrollHeight: activityFeedEl.scrollHeight,
    };
  }
}

/** Restore the activity feed scroll position after a lit-html re-render.
 *  If the user was at scrollTop=0, leave it (newest row appears at top).
 *  If they'd scrolled down, add the delta in scrollHeight to scrollTop so
 *  their view is pinned to the same content. */
export function restoreActivityScroll(): void {
  if (!activityFeedEl || !savedScroll) return;
  const newScrollHeight: number = activityFeedEl.scrollHeight;
  const delta: number = newScrollHeight - savedScroll.scrollHeight;
  if (savedScroll.scrollTop > 0 && delta !== 0) {
    activityFeedEl.scrollTop = savedScroll.scrollTop + delta;
  }
  savedScroll = null;
}

/** Reset scroll state. Called on mount to clear stale state. */
export function resetActivityScroll(): void {
  activityFeedEl = null;
  savedScroll = null;
}
