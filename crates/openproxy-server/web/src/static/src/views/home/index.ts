// views/home/index.ts — live dashboard (F6).
//
// Mounts the live-store (F5), subscribes to its throttled re-render
// callback (4 Hz max), and creates the 4 main uPlot charts + 4 KPI
// sparklines after the first lit-html render. Returns a cleanup function
// that unsubscribes, destroys the charts, and unmounts the live-store.
//
// This file is the orchestrator — it coordinates the sub-modules:
//   - banner.ts: WS connection status banner + header dot
//   - kpis.ts: KPI tile grid + sparkline builders
//   - charts.ts: main uPlot charts + race card + window selector
//   - activity-feed.ts: recent rows + scroll preservation

import { html, type TemplateResult } from "lit-html";
import type uPlot from "uplot";

import { mountView, requestUpdate } from "../../state/reactive.js";
import { t } from "../../i18n/index.js";
import {
  mountLiveStore,
  subscribe,
  getSnapshot,
  getConnectionState,
} from "../../state/live-store.js";
import type { Snapshot, SnapshotWindow, LiveConnectionState } from "./types.js";

import { renderConnectionBanner, renderConnectionDot } from "./banner.js";
import {
  renderKpiGrid,
  createSparklines,
  pushSparklineData,
} from "./kpis.js";
import type { SparklineInstances } from "./kpis.js";
import {
  renderChartsGrid,
  renderWindowSelector,
  createMainCharts,
  pushMainChartData,
} from "./charts.js";
import {
  renderActivityFeed,
  saveActivityScroll,
  restoreActivityScroll,
  resetActivityScroll,
} from "./activity-feed.js";

// ==========
// Module-local state
// ==========

let currentSnapshot: Snapshot | null = null;
let currentConnectionState: LiveConnectionState = "disconnected";
let windowSecs: SnapshotWindow = 300;

/** uPlot instances + their resize observers. Created after the first
 *  lit-html render, destroyed on view unmount. Null before creation. */
let activeMainCharts: uPlot[] | null = null;
let activeSparklines: SparklineInstances | null = null;
let resizeDisposers: Array<() => void> = [];

let unsubLive: (() => void) | null = null;
let disposeStore: (() => void) | null = null;
let cleanupReactive: (() => void) | null = null;

// ==========
// Main render function
// ==========

function renderHome(): TemplateResult {
  const snapshot: Snapshot | null = currentSnapshot;
  const conn: LiveConnectionState = currentConnectionState;
  const hasResponses: boolean = snapshot?.statusCodes.some(
    (point) => point.s2xx + point.s4xx + point.s5xx > 0,
  ) ?? false;

  return html`
    <div class="home-dashboard">
      <div class="page-header home-header">
        <div class="home-header-text">
          <span class="page-eyebrow">${t("common.realtime")}</span>
          <h2>${t("home.title")}${renderConnectionDot(conn)}</h2>
          <p class="home-subtitle muted">${t("home.subtitle")}</p>
        </div>
        <div class="home-header-actions">${renderWindowSelector(windowSecs, onWindowChange)}</div>
      </div>
      ${renderConnectionBanner(conn)}
      ${renderKpiGrid(snapshot, windowSecs)}
      ${renderChartsGrid(snapshot, hasResponses)}
      ${renderActivityFeed(snapshot)}
    </div>
  `;
}

// ==========
// Live-store subscriber callback
// ==========

function onLiveUpdate(): void {
  saveActivityScroll();
  currentSnapshot = getSnapshot(windowSecs);
  currentConnectionState = getConnectionState();
  requestUpdate();
  requestAnimationFrame(() => {
    pushSnapshotToCharts();
    restoreActivityScroll();
  });
}

/** Push the current snapshot's data into the main charts + sparklines. */
function pushSnapshotToCharts(): void {
  const snapshot: Snapshot | null = currentSnapshot;
  if (!snapshot) return;
  if (activeMainCharts) pushMainChartData(activeMainCharts, snapshot);
  if (activeSparklines) pushSparklineData(activeSparklines, snapshot);
}

// ==========
// Window change handler
// ==========

function onWindowChange(newWindow: SnapshotWindow): void {
  if (newWindow === windowSecs) return;
  windowSecs = newWindow;
  currentSnapshot = getSnapshot(windowSecs);
  requestUpdate();
  requestAnimationFrame(() => {
    pushSnapshotToCharts();
    restoreActivityScroll();
  });
}

// ==========
// Theme refresh
// ==========

function refreshChartTheme(): void {
  destroyAllCharts();
  requestAnimationFrame(() => createAllCharts());
}

// ==========
// Chart lifecycle
// ==========

function destroyAllCharts(): void {
  // Disconnect ResizeObservers first.
  for (const disposer of resizeDisposers) {
    try { disposer(); } catch (e: unknown) {
      console.warn("[home] resize disposer threw:", e);
    }
  }
  resizeDisposers = [];
  // Destroy main charts.
  if (activeMainCharts) {
    for (const chart of activeMainCharts) {
      try { chart.destroy(); } catch (e: unknown) {
        console.warn("[home] chart.destroy threw:", e);
      }
    }
    activeMainCharts = null;
  }
  // Destroy sparklines.
  if (activeSparklines) {
    for (const key of Object.keys(activeSparklines) as Array<keyof SparklineInstances>) {
      try { activeSparklines[key].destroy(); } catch (e: unknown) {
        console.warn(`[home] ${String(key)}.destroy threw:`, e);
      }
    }
    activeSparklines = null;
  }
}

function createAllCharts(): void {
  if (activeMainCharts) return; // idempotent
  const main = createMainCharts();
  activeMainCharts = main.charts.length > 0 ? main.charts : null;
  resizeDisposers.push(...main.disposers);

  const sparks = createSparklines();
  activeSparklines = sparks.sparklines;
  resizeDisposers.push(...sparks.disposers);
}

// ==========
// Mount
// ==========

export async function mountHome(): Promise<(() => void) | void> {
  const main: HTMLElement | null = document.getElementById("main");
  if (!main) return;

  // Reset view-local state on every mount.
  currentSnapshot = null;
  currentConnectionState = "disconnected";
  activeMainCharts = null;
  activeSparklines = null;
  resizeDisposers = [];
  resetActivityScroll();

  // Mount the live-store. First consumer opens the WS + rehydrates from
  // /usage/recent. The store stays mounted across quick navigations
  // (home → logs → home) so the data stays warm.
  disposeStore = mountLiveStore();

  // Subscribe to live-store updates. The store calls subscribers on a
  // throttled cadence (max 4 Hz).
  unsubLive = subscribe(onLiveUpdate);

  // Mount the lit-html view.
  cleanupReactive = mountView(main, renderHome);
  document.addEventListener("themechange", refreshChartTheme);
  onLiveUpdate();

  // Create the uPlot charts after the first lit-html render. We use
  // requestAnimationFrame (rather than queueMicrotask) so the browser
  // has laid out the chart containers — clientWidth / clientHeight are
  // correct by then.
  requestAnimationFrame(() => {
    createAllCharts();
    pushSnapshotToCharts();
  });

  // Cleanup: destroy charts → unsubscribe → unmount store → release
  // lit-html container.
  return () => {
    destroyAllCharts();
    document.removeEventListener("themechange", refreshChartTheme);
    if (unsubLive) { unsubLive(); unsubLive = null; }
    if (disposeStore) { disposeStore(); disposeStore = null; }
    if (cleanupReactive) { cleanupReactive(); cleanupReactive = null; }
    resetActivityScroll();
  };
}
