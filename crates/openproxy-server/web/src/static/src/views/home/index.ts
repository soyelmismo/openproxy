// views/home/index.ts — live-dashboard orchestrator: mounts the live store,
// re-renders at up to 4 Hz, and creates the main charts + sparklines.

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

// Module-local state

let currentSnapshot: Snapshot | null = null;
let currentConnectionState: LiveConnectionState = "disconnected";
let windowSecs: SnapshotWindow = 300;

/** uPlot instances + resize observers. Null before the first render. */
let activeMainCharts: uPlot[] | null = null;
let activeSparklines: SparklineInstances | null = null;
let resizeDisposers: Array<() => void> = [];

let unsubLive: (() => void) | null = null;
let disposeStore: (() => void) | null = null;
let cleanupReactive: (() => void) | null = null;

// Main render function

function renderHome(): TemplateResult {
  const snapshot: Snapshot | null = currentSnapshot;
  const conn: LiveConnectionState = currentConnectionState;
  const hasResponses: boolean = snapshot?.statusCodes.some(
    (point) => point.s2xx + point.s4xx + point.s5xx > 0,
  ) ?? false;

  return html`
    <div class="home-dashboard">
      <div class="page-header home-header">
        <div class="home-header-text" title=${t("home.subtitle")}>
          <h2 class="view-identity">${t("home.title")}</h2>
          <span class="home-live-status">${renderConnectionDot(conn)}${t("common.realtime")}</span>
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

// Live-store subscriber callback

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

// Window change handler

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

// Theme refresh

function refreshChartTheme(): void {
  destroyAllCharts();
  requestAnimationFrame(() => createAllCharts());
}

// Chart lifecycle

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

// Mount

export async function mountHome(
  target?: HTMLElement | null | string,
): Promise<(() => void) | void> {
  const container = (target instanceof HTMLElement) ? target : document.getElementById("main");
  if (!container || !container.isConnected) return;

  // Reset view-local state on every mount.
  currentSnapshot = null;
  currentConnectionState = "disconnected";
  activeMainCharts = null;
  activeSparklines = null;
  resizeDisposers = [];
  resetActivityScroll();

  // First consumer opens the WS and rehydrates from /usage/recent; the store
  // survives quick navigations to stay warm.
  disposeStore = mountLiveStore();

  // Subscribers fire on a throttled cadence (max 4 Hz).
  unsubLive = subscribe(onLiveUpdate);

  cleanupReactive = mountView(container, renderHome);
  document.addEventListener("themechange", refreshChartTheme);
  onLiveUpdate();

  // rAF, not queueMicrotask: clientWidth/clientHeight are only valid after layout.
  requestAnimationFrame(() => {
    if (!container.isConnected) return;
    createAllCharts();
    pushSnapshotToCharts();
  });

  // Cleanup order: charts, subscription, store, lit-html container.
  return () => {
    destroyAllCharts();
    document.removeEventListener("themechange", refreshChartTheme);
    if (unsubLive) { unsubLive(); unsubLive = null; }
    if (disposeStore) { disposeStore(); disposeStore = null; }
    if (cleanupReactive) { cleanupReactive(); cleanupReactive = null; }
    resetActivityScroll();
  };
}
