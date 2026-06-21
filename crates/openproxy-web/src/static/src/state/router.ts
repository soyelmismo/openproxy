// state/router.ts — hash-based router. The original app used
// inline `onclick="navigate()"` everywhere; we now route
// navigation through data-action="navigate" so the global is
// not needed. The router still exports a navigate() function
// and a rerenderCurrentView() helper (used by the bg-poll and
// by handlers that need to repaint).

import { state } from "./index.js";
import { startBgPoll } from "./bg-poll.js";
import { stopLogLatencyTicker } from "./ticker.js";
// View mounts stay .js for now (out of G3 scope). tsc+Bundler
// resolves the import to the .js at runtime; the not-yet-typed
// returns flow through as `unknown`, which is fine for the
// router since we don't introspect what they return.
import { mountHome } from "../views/home.js";
import { mountProviders } from "../views/providers.js";
import { mountCombos } from "../views/combos.js";
import { mountKeys } from "../views/keys.js";
import { mountKeyUsage } from "../views/key-usage.js";
import { mountAnalytics } from "../views/analytics.js";
import { mountLogs } from "../views/logs.js";
import { mountConfig } from "../views/config.js";

export type RouteName =
  | "home"
  | "providers"
  | "provider-detail"
  | "combos"
  | "combo-detail"
  | "keys"
  | "key-usage"
  | "analytics"
  | "logs"
  | "config";

export type ViewMount = (ctx: string) => unknown;

interface Route {
  name: RouteName;
  pattern: RegExp;
  mount: ViewMount;
}

const ROUTES: readonly Route[] = [
  { name: "home", pattern: /^#?\/?$/, mount: mountHome as ViewMount },
  { name: "providers", pattern: /^#?\/providers$/, mount: mountProviders as ViewMount },
  { name: "provider-detail", pattern: /^#?\/providers\/(.+)$/, mount: ((ctx: string) => mountProviders({ detailId: decodeURIComponent(ctx) })) as ViewMount },
  { name: "combos", pattern: /^#?\/combos$/, mount: mountCombos as ViewMount },
  { name: "combo-detail", pattern: /^#?\/combos\/(\d+)$/, mount: ((ctx: string) => mountCombos({ detailId: parseInt(ctx, 10) })) as ViewMount },
  { name: "keys", pattern: /^#?\/keys$/, mount: mountKeys as ViewMount },
  { name: "key-usage", pattern: /^#?\/keys\/(\d+)\/usage$/, mount: ((ctx: string) => mountKeyUsage(parseInt(ctx, 10))) as ViewMount },
  // The analytics view carries a `?range=<preset>` query suffix in
  // the hash (e.g. `#/analytics?range=this_month`) so the selected
  // time-range survives a refresh. The pattern tolerates an optional
  // trailing `?...` without capturing it — `mountAnalytics` reads
  // the preset directly off `location.hash`. Other routes are
  // unaffected.
  { name: "analytics", pattern: /^#?\/analytics(?:\?.*)?$/, mount: mountAnalytics as ViewMount },
  { name: "logs", pattern: /^#?\/logs$/, mount: mountLogs as ViewMount },
  { name: "config", pattern: /^#?\/config$/, mount: mountConfig as ViewMount },
];

export interface ParsedHash {
  name: RouteName;
  context: string;
  mount: ViewMount;
}

export function parseHash(hash: string): ParsedHash | null {
  for (const r of ROUTES) {
    const m: RegExpMatchArray | null = (hash || "").match(r.pattern);
    // Any match is a valid route. Routes with a capture group (e.g.
    // `#/providers/:id`) carry the captured string as context; routes
    // without a group (e.g. `#/providers`, `#/`) get an empty context.
    // The previous `m[1] !== undefined` check wrongly rejected every
    // top-level route (7 of 10) and left <main> empty.
    if (m) return { name: r.name, context: m[1] ?? "", mount: r.mount };
  }
  return null;
}

// Top-level navigation. Mirrors the original `window.navigate()`
// behaviour: re-resolve the current hash and re-mount the view.
export function navigate(): void {
  const r: ParsedHash | null = parseHash(location.hash);
  if (!r) { location.hash = "#/"; return; }
  state.currentView = { name: r.name, context: r.context };
  // Sidebar active state
  document.querySelectorAll(".sidebar nav a").forEach((a: Element) => {
    a.classList.toggle("active", "#" + (a.getAttribute("href") || "").replace(/^#/, "") === location.hash);
  });
  // Mount the new view. Errors render into #main as a small banner.
  Promise.resolve(r.mount(r.context)).catch((e: unknown) => {
    const main: HTMLElement | null = document.getElementById("main");
    if (main) {
      const msg: string = (e instanceof Error) ? e.message : String(e);
      main.innerHTML = `<div class="banner banner-error">Error: ${msg}</div>`;
    }
  });
  // Stop the live-logs latency ticker if the user has navigated
  // away from `#/logs` — otherwise it keeps running at 10 Hz forever.
  if (r.name !== "logs") stopLogLatencyTicker();
  // (Re)start the background poll. It is idempotent: re-calling
  // clears any existing interval and starts a fresh one, so the
  // navigation is the natural place to do it.
  startBgPoll();
}

export function rerenderCurrentView(): void { navigate(); }

// Wire the hashchange event exactly once. Called from app.js boot.
//
// The router's `navigate()` and `rerenderCurrentView()` are reached
// from views/handlers via the HANDLERS map (data-action="navigate")
// in handlers/registry.js. They used to be on `window.*` for the
// inline onclick handlers; those are gone now. We deliberately do
// NOT set window.navigate / window.rerenderCurrentView here so the
// only path to them is through data-action or direct import.
export function installRouter(): void {
  window.addEventListener("hashchange", navigate);
}
