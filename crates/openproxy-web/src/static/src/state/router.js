// state/router.js — hash-based router. The original app used
// inline `onclick="navigate()"` everywhere; we now route
// navigation through data-action="navigate" so the global is
// not needed. The router still exports a navigate() function
// and a rerenderCurrentView() helper (used by the bg-poll and
// by handlers that need to repaint).

import { state } from "./index.js";
import { startBgPoll } from "./bg-poll.js";
import { stopLogLatencyTicker } from "./ticker.js";
import { mountHome } from "../views/home.js";
import { mountProviders } from "../views/providers.js";
import { mountCombos } from "../views/combos.js";
import { mountKeys } from "../views/keys.js";
import { mountKeyUsage } from "../views/key-usage.js";
import { mountAnalytics } from "../views/analytics.js";
import { mountLogs } from "../views/logs.js";
import { mountConfig } from "../views/config.js";

const ROUTES = [
  { name: "home", pattern: /^#?\/?$/, mount: mountHome },
  { name: "providers", pattern: /^#?\/providers$/, mount: mountProviders },
  { name: "provider-detail", pattern: /^#?\/providers\/(.+)$/, mount: (ctx) => mountProviders({ detailId: decodeURIComponent(ctx) }) },
  { name: "combos", pattern: /^#?\/combos$/, mount: mountCombos },
  { name: "combo-detail", pattern: /^#?\/combos\/(\d+)$/, mount: (ctx) => mountCombos({ detailId: parseInt(ctx, 10) }) },
  { name: "keys", pattern: /^#?\/keys$/, mount: mountKeys },
  { name: "key-usage", pattern: /^#?\/keys\/(\d+)\/usage$/, mount: (ctx) => mountKeyUsage(parseInt(ctx, 10)) },
  { name: "analytics", pattern: /^#?\/analytics$/, mount: mountAnalytics },
  { name: "logs", pattern: /^#?\/logs$/, mount: mountLogs },
  { name: "config", pattern: /^#?\/config$/, mount: mountConfig },
];

export function parseHash(hash) {
  for (const r of ROUTES) {
    const m = (hash || "").match(r.pattern);
    if (m) return { name: r.name, context: m[1], mount: r.mount };
  }
  return null;
}

// Top-level navigation. Mirrors the original `window.navigate()`
// behaviour: re-resolve the current hash and re-mount the view.
export function navigate() {
  const r = parseHash(location.hash);
  if (!r) { location.hash = "#/"; return; }
  state.currentView = { name: r.name, context: r.context };
  // Sidebar active state
  document.querySelectorAll(".sidebar nav a").forEach((a) => {
    a.classList.toggle("active", "#" + (a.getAttribute("href") || "").replace(/^#/, "") === location.hash);
  });
  // Mount the new view. Errors render into #main as a small banner.
  Promise.resolve(r.mount(r.context)).catch((e) => {
    document.getElementById("main").innerHTML =
      `<div class="banner banner-error">Error: ${(e && e.message) || e}</div>`;
  });
  // Stop the live-logs latency ticker if the user has navigated
  // away from `#/logs` — otherwise it keeps running at 10 Hz forever.
  if (r.name !== "logs") stopLogLatencyTicker();
  // (Re)start the background poll. It is idempotent: re-calling
  // clears any existing interval and starts a fresh one, so the
  // navigation is the natural place to do it.
  startBgPoll();
}

export function rerenderCurrentView() { navigate(); }

// Wire the hashchange event exactly once. Called from app.js boot.
//
// The router's `navigate()` and `rerenderCurrentView()` are reached
// from views/handlers via the HANDLERS map (data-action="navigate")
// in handlers/registry.js. They used to be on `window.*` for the
// inline onclick handlers; those are gone now. We deliberately do
// NOT set window.navigate / window.rerenderCurrentView here so the
// only path to them is through data-action or direct import.
export function installRouter() {
  window.addEventListener("hashchange", navigate);
}
