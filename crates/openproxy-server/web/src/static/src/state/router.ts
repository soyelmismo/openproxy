// Hash-based router. Navigation goes through the HANDLERS map
// (`data-action="navigate"`), so no global `navigate()` is installed.

import { html, render } from 'lit-html';
import { state } from "./index.js";
import { startBgPoll } from "./bg-poll.js";
import { isPanelDocument } from "./workspace.js";
import { mountWorkspace, unmountWorkspace } from "../components/workspace.js";

import { isLoggedIn, isFullAdmin } from "./auth.js";
import { renderSidebar } from "../components/sidebar.js";
import { mountHome } from "../views/home/index.js";
import { mountCombos } from "../views/combos.js";
import { mountKeys } from "../views/keys.js";
import { mountKeyUsage } from "../views/key-usage.js";
import { mountKeyDetail } from "../views/key-detail.js";
import { mountLogs } from "../views/logs.js";
import { mountDebugLogs } from "../views/debug-logs.js";
import { mountLogin } from "../views/login.js";
import { mountProxies } from "../views/proxies.js";
import { mountProxySources } from "../views/proxy-sources.js";

// Heavy views load on first navigation, keeping the core bundle small.

function lazyMount(
  importFn: () => Promise<{ [key: string]: unknown }>,
  exportName: string,
): ViewMount {
  return async (ctx: string) => {
    const mod = await importFn();
    const mount = mod[exportName] as ViewMount;
    return mount(ctx);
  };
}

export type RouteName =
  | "home"
  | "providers"
  | "provider-detail"
  | "combos"
  | "combo-detail"
  | "keys"
  | "key-detail"
  | "key-usage"
  | "analytics"
  | "logs"
  | "debug-logs"
  | "config"
  | "notifications"
  | "login"
  | "proxies"
  | "proxy-sources"
  | "playground";

export type ViewMount = (ctx: string) => unknown;

interface Route {
  name: RouteName;
  pattern: RegExp;
  mount: ViewMount;
}

const ROUTES: readonly Route[] = [
  { name: "home", pattern: /^#?\/?$/, mount: mountHome as ViewMount },
  { name: "providers", pattern: /^#?\/providers$/, mount: lazyMount(
    () => import('../views/providers/index.js'), 'mountProviders',
  ) },
  { name: "provider-detail", pattern: /^#?\/providers\/(.+)$/, mount: (async (ctx: string) => {
    const mod = await import('../views/providers/index.js');
    return mod.mountProviders({ detailId: decodeURIComponent(ctx) });
  }) as ViewMount },
  { name: "combos", pattern: /^#?\/combos$/, mount: mountCombos as ViewMount },
  { name: "combo-detail", pattern: /^#?\/combos\/(\d+)$/, mount: ((ctx: string) => mountCombos({ detailId: parseInt(ctx, 10) })) as ViewMount },
  { name: "keys", pattern: /^#?\/keys$/, mount: mountKeys as ViewMount },
  { name: "key-detail", pattern: /^#?\/keys\/(\d+)$/, mount: ((ctx: string) => mountKeyDetail(parseInt(ctx, 10))) as ViewMount },
  { name: "key-usage", pattern: /^#?\/keys\/(\d+)\/usage$/, mount: ((ctx: string) => mountKeyUsage(parseInt(ctx, 10))) as ViewMount },
  // `?range=<preset>` in the hash keeps the selected range across a refresh.
  // `mountAnalytics` reads the preset off `location.hash`.
  { name: "analytics", pattern: /^#?\/analytics(?:\?.*)?$/, mount: lazyMount(
    () => import('../views/analytics/index.js'), 'mountAnalytics',
  ) },
  { name: "logs", pattern: /^#?\/logs(?:\?.*)?$/, mount: mountLogs as ViewMount },
  // mountDebugLogs returns a cleanup that cancels its poll timer; navigate()
  // calls it before the next mount so the timer does not leak.
  { name: "debug-logs", pattern: /^#?\/debug-logs$/, mount: (() => {
    // debug-logs has no sub-routes, so the hash context is ignored.
    const main: HTMLElement | null = document.getElementById("main");
    if (!main) return;
    return mountDebugLogs(main);
  }) as ViewMount },
  { name: "config", pattern: /^#?\/config$/, mount: lazyMount(
    () => import('../views/config/index.js'), 'mountConfig',
  ) },
  { name: "proxies", pattern: /^#?\/proxies$/, mount: mountProxies as ViewMount },
  { name: "proxy-sources", pattern: /^#?\/proxy-sources$/, mount: mountProxySources as ViewMount },
  { name: "playground", pattern: /^#?\/playground$/, mount: lazyMount(
    () => import('../views/playground/index.js'), 'mountPlayground',
  ) },
  // Its cleanup unsubscribes from the notifications store, so navigating
  // away does not leak the listener.
  { name: "notifications", pattern: /^#?\/notifications$/, mount: lazyMount(
    () => import('../views/notifications/index.js'), 'mountNotifications',
  ) },
  // The only route reachable without a token.
  { name: "login", pattern: /^#?\/login\/?$/, mount: mountLogin as ViewMount },
];

export interface ParsedHash {
  name: RouteName;
  context: string;
  mount: ViewMount;
}

// Cleanup returned by the mounted view, invoked before the next mount so
// timers and sockets do not leak across navigations.
let currentCleanup: (() => void) | null = null;

export function parseHash(hash: string): ParsedHash | null {
  for (const r of ROUTES) {
    const m: RegExpMatchArray | null = (hash || "").match(r.pattern);
    // A capture group (e.g. `#/providers/:id`) becomes the context; routes
    // without one get an empty string.
    if (m) return { name: r.name, context: m[1] ?? "", mount: r.mount };
  }
  return null;
}

/** Mount a real SPA view into a native tile, never another document. */
export async function mountRouteInto(route: string, target: HTMLElement): Promise<(() => void) | void> {
  const parsed = parseHash(route);
  if (!parsed || !target.isConnected) return;
  target.dataset['view'] = parsed.name;
  switch (parsed.name) {
    case 'home': return mountHome(target);
    case 'providers': case 'provider-detail': {
      const { mountProviders } = await import('../views/providers/index.js');
      if (!target.isConnected) return;
      return mountProviders(parsed.name === 'provider-detail' ? { detailId: decodeURIComponent(parsed.context) } : {}, target);
    }
    case 'combos': case 'combo-detail': return mountCombos(parsed.name === 'combo-detail' ? { detailId: Number(parsed.context) } : {}, target);
    case 'keys': return mountKeys(target);
    case 'key-detail': return mountKeyDetail(Number(parsed.context), target);
    case 'key-usage': return mountKeyUsage(Number(parsed.context), target);
    case 'logs': return mountLogs(target, route);
    case 'debug-logs': return mountDebugLogs(target);
    case 'proxies': return mountProxies(target);
    case 'proxy-sources': return mountProxySources(target);
    case 'analytics': {
      const { mountAnalytics } = await import('../views/analytics/index.js');
      if (target.isConnected) return mountAnalytics(target);
      return;
    }
    case 'config': {
      const { mountConfig } = await import('../views/config/index.js');
      if (target.isConnected) return mountConfig(target);
      return;
    }
    case 'notifications': {
      const { mountNotifications } = await import('../views/notifications/index.js');
      if (target.isConnected) return mountNotifications(target);
      return;
    }
    case 'playground': {
      const { mountPlayground } = await import('../views/playground/index.js');
      if (target.isConnected) return mountPlayground(target);
      return;
    }
    case 'login': return;
  }
}

export function navigate(): void {
  const r: ParsedHash | null = parseHash(location.hash);
  if (!r) { location.hash = "#/"; return; }
  // Auth gate: the login route is the only one reachable without a token, and
  // a logged-in user landing on `#/login` goes back to `#/` so a bookmarked
  // login URL does not strand them. Each redirect fires `hashchange`, which
  // re-enters `navigate()` past this gate.
  const loggedIn: boolean = isLoggedIn();
  if (!loggedIn && r.name !== "login") {
    location.hash = "#/login";
    return;
  }
  if (loggedIn && r.name === "login") {
    location.hash = isFullAdmin() ? "#/" : "#/analytics";
    return;
  }
  // Restricted tenant enforcement: non-admin keys can ONLY access the analytics view.
  // Any attempt to navigate to #/keys, #/config, #/logs, #/debug-logs, etc. routes directly to #/analytics.
  if (loggedIn && !isFullAdmin() && r.name !== "analytics") {
    location.hash = "#/analytics";
    return;
  }
  if (r.name === "login") {
    document.dispatchEvent(new Event("openproxy:close-modals"));
    document.querySelectorAll(".modal-bg").forEach((el: Element) => {
      const parent: HTMLElement | null = el.parentElement;
      if (parent && parent !== document.body && parent.id !== "modal-root") {
        parent.remove();
      } else {
        el.remove();
      }
    });
  }
  // Hides the sidebar on the login page; see the `body.on-login-page` rule in
  // styles/layout.css.
  document.body.classList.toggle("on-login-page", r.name === "login");
  document.body.dataset["view"] = r.name;
  state.currentView = { name: r.name, context: r.context };
  // First render happens at boot while logged out, when
  // `maybeBootstrapNotifications()` returns early. Re-rendering here is what
  // bootstraps the notifications store and the WS after login.
  renderSidebar();
  // Sidebar active state
  document.querySelectorAll(".sidebar nav a").forEach((a: Element) => {
    a.classList.toggle("active", "#" + (a.getAttribute("href") || "").replace(/^#/, "") === location.hash);
  });
  // Run the previous view's cleanup before mounting the new one.
  if (currentCleanup !== null) {
    try { currentCleanup(); } catch (e: unknown) {
      console.warn("[router] previous view cleanup threw", e);
    }
    currentCleanup = null;
  }
  const workspaceRoot = document.getElementById("workspace");
  const useWorkspace = !isPanelDocument() && r.name !== "login" && workspaceRoot !== null;
  document.body.classList.toggle("workspace-host", useWorkspace);
  if (workspaceRoot) workspaceRoot.hidden = !useWorkspace;
  const mainRoot = document.getElementById("main");
  if (mainRoot) mainRoot.hidden = useWorkspace;
  if (useWorkspace) {
    mountWorkspace(location.hash || "#/");
    startBgPoll();
    return;
  }
  if (r.name === "login" && !isPanelDocument()) unmountWorkspace();
  // The mount may return a cleanup function or a Promise of one.
  Promise.resolve(r.mount(r.context)).then((ret: unknown) => {
    if (typeof ret === "function") {
      currentCleanup = ret as () => void;
    } else {
      currentCleanup = null;
    }
    if (isPanelDocument()) window.parent.postMessage({ type: "openproxy:panel-ready" }, location.origin);
  }).catch((e: unknown) => {
    const main: HTMLElement | null = document.getElementById("main");
    if (main) {
      const msg: string = (e instanceof Error) ? e.message : String(e);
      delete (main as { _$litPart$?: unknown })._$litPart$;
      main.innerHTML = '';
      render(html`<div class="banner banner-error">Error: ${msg}</div>`, main);
      if (isPanelDocument()) window.parent.postMessage({ type: "openproxy:panel-ready" }, location.origin);
    }
  });
  // Idempotent: restarts the interval, so navigation is a natural hook.
  startBgPoll();
}

// Coalesces rapid calls into one navigate() on the next macrotask.
let rerenderTimer: ReturnType<typeof setTimeout> | null = null;

export function rerenderCurrentView(): void {
  // Skip the re-render while a form control has focus: a DOM rebuild would
  // close the dropdown and steal focus. State is already updated by the
  // handler, and the next natural re-render catches up.
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

/** Immediate re-render, bypassing the focus guard and debounce. */
export function forceRerenderCurrentView(): void {
  if (rerenderTimer !== null) {
    clearTimeout(rerenderTimer);
    rerenderTimer = null;
  }
  navigate();
}

// The router stays off `window.*`: the only paths in are the HANDLERS map
// and direct import.
export function installRouter(): void {
  window.addEventListener("hashchange", navigate);
  window.addEventListener("popstate", navigate);
}
