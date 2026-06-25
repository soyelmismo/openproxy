// state/router.ts — hash-based router. The original app used
// inline `onclick="navigate()"` everywhere; we now route
// navigation through data-action="navigate" so the global is
// not needed. The router still exports a navigate() function
// and a rerenderCurrentView() helper (used by the bg-poll and
// by handlers that need to repaint).

import { html, render } from 'lit-html';
import { state } from "./index.js";
import { startBgPoll } from "./bg-poll.js";
import { stopLogLatencyTicker } from "./ticker.js";
// DASHBOARD-FIX (Bug 2 / Step 2e): the router now imports the auth
// helper `isLoggedIn` and the login view `mountLogin`. `navigate()`
// below gates every route on `isLoggedIn()` except `login`; a logged-
// out user is redirected to `#/login` before any view mounts, so the
// bg-poll / api calls / WS that would otherwise 401 never fire.
import { isLoggedIn } from "./auth.js";
// Re-render the sidebar after route changes so it picks up the
// post-login state (notifications store bootstrap, badge count, etc.).
import { renderSidebar } from "../components/sidebar.js";
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
import { mountDebugLogs } from "../views/debug-logs.js";
import { mountNotifications } from "../views/notifications.js";
import { mountLogin } from "../views/login.js";

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
  | "debug-logs"
  | "config"
  | "notifications"
  | "login";

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
  // Debug Logs polls `/admin/debug/logs` on a 2s chained-setTimeout
  // loop. The view returns a cleanup function that cancels the
  // pending poll timer; `navigate()` stores it in `currentCleanup`
  // and invokes it before the next mount so the timer doesn't leak
  // across navigations.
  { name: "debug-logs", pattern: /^#?\/debug-logs$/, mount: (() => {
    // The router calls ViewMount with the hash context; the
    // debug-logs view doesn't use it (it has no sub-routes), so
    // we accept and ignore it. The view mounts into #main and
    // returns a cleanup function that cancels its poll timer;
    // `navigate()` captures that and calls it before the next
    // mount.
    const main: HTMLElement | null = document.getElementById("main");
    if (!main) return;
    return mountDebugLogs(main);
  }) as ViewMount },
  { name: "config", pattern: /^#?\/config$/, mount: mountConfig as ViewMount },
  // Notifications tray (F4). The view mounts at `#/notifications` and
  // manages its own state (list, filter, DnD overlay, WS subscription
  // via the notifications store). It returns a cleanup function that
  // unsubscribes from the store's event stream so navigating away
  // doesn't leak the listener.
  { name: "notifications", pattern: /^#?\/notifications$/, mount: mountNotifications as ViewMount },
  // Login (DASHBOARD-FIX Bug 2/Step 2d). The ONLY route accessible
  // without a token — the auth gate in `navigate()` redirects every
  // other route here when `isLoggedIn()` is false. Pattern is
  // `#/login` (and the bare `#/login/` form, harmless). The view
  // itself is in `views/login.ts`.
  { name: "login", pattern: /^#?\/login\/?$/, mount: mountLogin as ViewMount },
];

export interface ParsedHash {
  name: RouteName;
  context: string;
  mount: ViewMount;
}

// The cleanup function returned by the currently-mounted view (if
// any). `navigate()` invokes it before mounting the next view so
// resources like setTimeout handles, WebSocket connections, etc.
// don't leak across navigations. Views that return void/Promise<void>
// leave this as null — no-op.
let currentCleanup: (() => void) | null = null;

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
  // ---- Auth gate (DASHBOARD-FIX Bug 2/Step 2e) ----
  // The dashboard now requires a manage-scope API key for every
  // /admin/api/* call and the /admin/ws upgrade. The login view is
  // the only screen that should mount without a token; every other
  // route is redirected there until the user logs in. We also
  // redirect a logged-in user away from `#/login` so a stale URL
  // (e.g. bookmarked) doesn't strand them on the login form after
  // they've already authenticated.
  //
  // Setting `location.hash` triggers a `hashchange` event, which
  // calls `navigate()` again — same pattern as the `if (!r)` branch
  // above. The re-entry falls through this gate (route is now
  // "login" / not-"login" respectively) and proceeds to mount.
  const loggedIn: boolean = isLoggedIn();
  if (!loggedIn && r.name !== "login") {
    location.hash = "#/login";
    return;
  }
  if (loggedIn && r.name === "login") {
    location.hash = "#/";
    return;
  }
  // Toggle a body class so CSS can hide the sidebar on the login
  // page (the sidebar's nav links would otherwise bounce back to
  // #/login via the auth gate, which is confusing on the login
  // screen). See styles/layout.css for the `body.on-login-page`
  // rule.
  document.body.classList.toggle("on-login-page", r.name === "login");
  state.currentView = { name: r.name, context: r.context };
  // Re-render the sidebar on every route change. This is critical
  // for the post-login flow: the sidebar was first rendered at boot
  // (by `mountShell()`) when the user was NOT logged in, so
  // `maybeBootstrapNotifications()` returned early without opening
  // the WS or starting the notifications poll. After a successful
  // login, `navigate()` is called again (via `location.hash = "#/"`
  // in `views/login.ts`), and this `renderSidebar()` call is the
  // first one that runs with `isLoggedIn()` true — so the
  // notifications store finally bootstraps and the WS opens with
  // the token attached.
  renderSidebar();
  // Sidebar active state
  document.querySelectorAll(".sidebar nav a").forEach((a: Element) => {
    a.classList.toggle("active", "#" + (a.getAttribute("href") || "").replace(/^#/, "") === location.hash);
  });
  // Call the previous view's cleanup before mounting the new one.
  // The cleanup function is returned by views that own resources
  // (e.g. `mountDebugLogs` returns a function that cancels its
  // poll timer). Views that return void/Promise<void> leave
  // `currentCleanup` as null — no-op.
  if (currentCleanup !== null) {
    try { currentCleanup(); } catch (e: unknown) {
      console.warn("[router] previous view cleanup threw", e);
    }
    currentCleanup = null;
  }
  // Mount the new view. Errors render into #main as a small banner.
  // The mount may return a cleanup function (sync) or a Promise
  // that resolves to one (async) — we capture either.
  Promise.resolve(r.mount(r.context)).then((ret: unknown) => {
    if (typeof ret === "function") {
      currentCleanup = ret as () => void;
    } else {
      currentCleanup = null;
    }
  }).catch((e: unknown) => {
    const main: HTMLElement | null = document.getElementById("main");
    if (main) {
      const msg: string = (e instanceof Error) ? e.message : String(e);
      render(html`<div class="banner banner-error">Error: ${msg}</div>`, main);
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

// Debounce timer for rerenderCurrentView. Multiple rapid calls
// (e.g., a PATCH that triggers state updates) coalesce into a
// single navigate() call on the next macrotask. This prevents
// redundant DOM rebuilds when several handlers fire in quick
// succession.
let rerenderTimer: ReturnType<typeof setTimeout> | null = null;

export function rerenderCurrentView(): void {
  // CRITICAL UX FIX: if the user is currently interacting with a
  // form element (input, select, textarea), a full DOM rebuild
  // would close their dropdown, steal focus, and make the UI feel
  // broken. This is the root cause of the "me cierra el dropdown"
  // bug the user reported across the entire dashboard.
  //
  // When the focused element is a form control, we SKIP the
  // re-render entirely. The state has already been updated
  // optimistically by the handler, so the data is correct — the
  // DOM will catch up on the next natural re-render (page
  // navigation, bg-poll tick, or when the user clicks elsewhere).
  const active: Element | null = document.activeElement;
  if (active instanceof HTMLInputElement
      || active instanceof HTMLSelectElement
      || active instanceof HTMLTextAreaElement) {
    return;
  }
  // Debounce: coalesce multiple rapid calls into one. If a timer
  // is already pending, this call is a no-op — the pending timer
  // will fire and render the latest state.
  if (rerenderTimer !== null) return;
  rerenderTimer = setTimeout(() => {
    rerenderTimer = null;
    navigate();
  }, 0);
}

/** Force an immediate re-render, bypassing the focus guard and
 *  debounce. Use for structural changes (create, delete, navigate)
 *  where the DOM genuinely needs to rebuild. */
export function forceRerenderCurrentView(): void {
  if (rerenderTimer !== null) {
    clearTimeout(rerenderTimer);
    rerenderTimer = null;
  }
  navigate();
}

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
