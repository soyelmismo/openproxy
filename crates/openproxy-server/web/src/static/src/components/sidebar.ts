// components/sidebar.ts — renders the sidebar (brand, nav, health, collapse toggle).
// Migrated to lit-html: uses render() instead of innerHTML.

import { html, render, type TemplateResult } from 'lit-html';
import { unsafeHTML } from 'lit-html/directives/unsafe-html.js';
import { state } from "../state/index.js";
import { mountThemeToggle } from "./theme-toggle.js";
import { t } from "../i18n/index.js";
import {
  initNotificationsStore,
  getUnreadCount,
  onUnreadCountChange,
} from "../state/notifications-store.js";
// B1 (Bug 3): the sidebar now also renders a badge on the "Debug
// Logs" link showing the count of unviewed WARN+ERROR entries in
// the server's debug-log ring buffer. The store polls every 30s
// (independent of the debug-logs view's own 2s poll) so the badge
// reflects new errors even when the user isn't on the Debug Logs
// page.
import {
  initDebugLogsStore,
  getUnviewedWarnErrorCount,
  onUnviewedWarnErrorCountChange,
} from "../state/debug-logs-store.js";
// DASHBOARD-FIX (Bug 2 / Step 2f): the sidebar now renders a Logout
// button in its footer. The button calls `clearToken()` (wipes the
// localStorage key + the in-memory cache) and navigates to `#/login`,
// which the router's auth gate lets through because `isLoggedIn()`
// is now false.
import { clearToken, isLoggedIn } from "../state/auth.js";

interface UiState { sidebarCollapsed?: boolean; }
type MutableDashboard = { ui?: UiState };
function mutableState(): MutableDashboard { return state as unknown as MutableDashboard; }

type NavIconName = "home" | "providers" | "combos" | "keys" | "analytics" | "logs" | "debug-logs" | "config" | "notifications" | "proxies";

interface SidebarLink {
  href: string;
  icon: NavIconName;
  label: string;
  /** Optional badge key. When set, the sidebar renders a small red
   *  pill next to the label whose numeric value comes from the
   *  corresponding store. Hidden when the value is 0. */
  badgeKind?: "notifications-unread" | "debug-logs-unviewed";
}
interface SidebarGroup { label: string; links: SidebarLink[]; }

const HOME_LINK: SidebarLink = { href: "#/", icon: "home", label: "Home" };

const GROUPS: readonly SidebarGroup[] = [
  { label: "Inventory", links: [
    { href: "#/providers", icon: "providers", label: "Providers" },
    { href: "#/combos", icon: "combos", label: "Combos" },
    { href: "#/keys", icon: "keys", label: "API Keys" },
    { href: "#/proxies", icon: "proxies", label: "Free Proxies" },
  ]},
  { label: "Insights", links: [
    { href: "#/analytics", icon: "analytics", label: "Analytics" },
    { href: "#/logs", icon: "logs", label: "Live Logs" },
    { href: "#/notifications", icon: "notifications", label: "Notifications", badgeKind: "notifications-unread" },
    { href: "#/debug-logs", icon: "debug-logs", label: "Debug Logs", badgeKind: "debug-logs-unviewed" },
  ]},
  { label: "System", links: [
    { href: "#/config", icon: "config", label: "Config" },
  ]},
];

function navIconSvg(name: NavIconName): TemplateResult {
  const svgs: Record<NavIconName, string> = {
    home: `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><path d="M2 7 L8 2 L14 7 V14 H2 Z"/><path d="M6 14 V10 H10 V14"/></svg>`,
    providers: `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><rect x="2" y="3" width="12" height="3" rx="0.5"/><rect x="2" y="7" width="12" height="3" rx="0.5"/><rect x="2" y="11" width="12" height="3" rx="0.5"/><circle cx="4" cy="4.5" r="0.4" fill="currentColor" stroke="none"/><circle cx="4" cy="8.5" r="0.4" fill="currentColor" stroke="none"/><circle cx="4" cy="12.5" r="0.4" fill="currentColor" stroke="none"/></svg>`,
    combos: `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><rect x="2" y="2" width="9" height="9" rx="1"/><rect x="5" y="5" width="9" height="9" rx="1"/></svg>`,
    keys: `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><circle cx="5" cy="5" r="2.5"/><path d="M6.8 6.8 L13 13"/><path d="M11 11 L13 9"/><path d="M9 13 L11 11"/></svg>`,
    proxies: `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" stroke-linecap="round"><circle cx="8" cy="4" r="2.5"/><circle cx="3" cy="12" r="2.5"/><circle cx="13" cy="12" r="2.5"/><path d="M5.5 10.5 L6.5 9 M10.5 10.5 L9.5 9"/></svg>`,
    analytics: `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><path d="M2 14 H14"/><rect x="3" y="9" width="2.5" height="5"/><rect x="6.75" y="5" width="2.5" height="9"/><rect x="10.5" y="7" width="2.5" height="7"/></svg>`,
    logs: `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" stroke-linecap="round"><path d="M2 8 H5 L7 3 L9 13 L11 8 H14"/></svg>`,
    "debug-logs": `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" stroke-linecap="round"><ellipse cx="8" cy="9" rx="3.5" ry="4"/><circle cx="8" cy="4" r="1.2"/><path d="M8 5 V5.5"/><path d="M4.5 7 L2 6 M4.5 9 L1.5 9 M4.5 11 L2.5 12.5"/><path d="M11.5 7 L14 6 M11.5 9 L14.5 9 M11.5 11 L13.5 12.5"/></svg>`,
    config: `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><circle cx="8" cy="8" r="2.5"/><path d="M8 1 V3.5 M8 12.5 V15 M1 8 H3.5 M12.5 8 H15 M3 3 L4.8 4.8 M11.2 11.2 L13 13 M3 13 L4.8 11.2 M11.2 4.8 L13 3"/></svg>`,
    // Bell-ish glyph. The dot is filled via stroke="currentColor" so
    // it inherits the active link colour.
    notifications: `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" stroke-linecap="round"><path d="M3 12 H13 L11.5 10 V7 a3.5 3.5 0 0 0 -7 0 V10 Z"/><path d="M6.5 12 V12.5 a1.5 1.5 0 0 0 3 0 V12"/></svg>`,
  };
  return html`${unsafeHTML(svgs[name] || "")}`;
}

function isActive(href: string): boolean {
  if (href === "#/") return location.hash === "#/" || location.hash === "";
  return location.hash.startsWith(href);
}

function applyActiveState(): void {
  const sb = document.querySelector(".sidebar");
  if (!sb) return;
  sb.querySelectorAll("nav a").forEach((a: Element) => {
    const aEl = a as HTMLElement;
    aEl.classList.toggle("active", isActive(aEl.getAttribute("href") || ""));
  });
}

const STORAGE_KEY = "openproxy:sidebarCollapsed";

function renderLink(l: SidebarLink, collapsed: boolean): TemplateResult {
  // The notifications badge lives next to the nav label. It is hidden
  // when the count is 0 (lit-html `nothing` sentinel — emits no DOM
  // node, so the layout doesn't shift when the count drops to 0).
  // B1 (Bug 3): added the `debug-logs-unviewed` badge kind, which
  // surfaces the count of unviewed WARN+ERROR entries in the
  // server's debug-log ring buffer so discovery failures (and other
  // WARN-level events) are visible without navigating to the Debug
  // Logs view.
  let badge: TemplateResult = html``;
  if (l.badgeKind === "notifications-unread") {
    const count: number = getUnreadCount();
    if (count > 0) {
      // When collapsed, show only the count pill (no label). The pill
      // sits in the same flex row so it visually replaces the label.
      const display: string = count > 99 ? "99+" : String(count);
      badge = html`<span class="sidebar-badge ${collapsed ? "collapsed" : ""}" title=${t("notifications.unread_count", { count })}>${display}</span>`;
    }
  } else if (l.badgeKind === "debug-logs-unviewed") {
    const count: number = getUnviewedWarnErrorCount();
    if (count > 0) {
      // Same red pill style as the notifications badge (the base
      // `.sidebar-badge` class already uses `var(--color-error)`
      // as the background). The title gives hover-help in case the
      // user wonders what the number means.
      const display: string = count > 99 ? "99+" : String(count);
      badge = html`<span class="sidebar-badge ${collapsed ? "collapsed" : ""}" title=${count + " unviewed WARN/ERROR debug log entries"}>${display}</span>`;
    }
  }
  return html`<a href=${l.href} data-nav=${l.href} title=${l.label}>
    <span class="nav-icon" aria-hidden="true">${navIconSvg(l.icon)}</span><span class="nav-label" ?hidden=${collapsed}> ${l.label}</span>${badge}
  </a>`;
}

let storeBootstrapped: boolean = false;
let debugLogsStoreBootstrapped: boolean = false;

/** Initialise the notifications store + WS subscription the first
 *  time the sidebar renders. Idempotent — safe to call from every
 *  `renderSidebar()`. The store bootstraps the WS, the 30s poll, and
 *  the ws-bus subscription; we then subscribe to count changes so
 *  the badge re-renders on every update.
 *
 *  IMPORTANT: we only bootstrap when the user is logged in. On the
 *  login page the sidebar is hidden via CSS (`body.on-login-page`),
 *  but `renderSidebar()` still runs (it's called by `mountShell()`
 *  at boot, before the router's auth gate redirects to #/login).
 *  Without this guard, `initNotificationsStore()` would open the
 *  WebSocket — which fails with 401 because there's no token yet,
 *  producing the "Firefox no puede establecer una conexión con el
 *  servidor en ws://.../admin/ws" console error on the login screen.
 *  The store is lazily bootstrapped on the first `renderSidebar()`
 *  call that happens AFTER login (when `isLoggedIn()` returns true). */
function maybeBootstrapNotifications(): void {
  if (storeBootstrapped) return;
  if (!isLoggedIn()) return;
  storeBootstrapped = true;
  initNotificationsStore();
  // Re-render the sidebar on every count change so the badge stays
  // in sync. The notifications view also subscribes to count changes
  // for its own header badge — both fire on every change, which is
  // fine (lit-html's diff is cheap).
  onUnreadCountChange(() => {
    // Only re-render the sidebar — the view handles its own updates.
    renderSidebar();
  });
}

/** Initialise the debug-logs store the first time the sidebar
 *  renders (after login). Idempotent. Mirrors the
 *  `maybeBootstrapNotifications` guard — the 30s poll hits an
 *  authenticated endpoint, so we don't want to start it until the
 *  user is logged in (otherwise it would 401 every 30s before
 *  login). */
function maybeBootstrapDebugLogs(): void {
  if (debugLogsStoreBootstrapped) return;
  if (!isLoggedIn()) return;
  debugLogsStoreBootstrapped = true;
  initDebugLogsStore();
  // Re-render the sidebar on every unviewed-count change so the
  // badge reflects new WARN+ERROR entries as they arrive.
  onUnviewedWarnErrorCountChange(() => {
    renderSidebar();
  });
}

export function renderSidebar(): void {
  const sb = document.querySelector(".sidebar");
  if (!sb) return;
  maybeBootstrapNotifications();
  maybeBootstrapDebugLogs();
  const health = state.health;
  const legacyHealthClass = !health ? "loading" : (health.status === "ok" || health.status === "healthy") ? "ok" : "error";
  const dotClass = !health ? "warn" : (health.status === "ok" || health.status === "healthy") ? "ok" : "err";
  const healthText = !health ? "—" : (health.status === "ok" || health.status === "healthy") ? "healthy" : (health.status || "down");
  const collapsed = !!mutableState().ui?.sidebarCollapsed;
  document.body.classList.toggle("sidebar-collapsed", collapsed);
  const toggleLabel = collapsed ? "→" : "←";

  render(html`
    <div class="brand">
      <span class="nav-icon" aria-hidden="true">${navIconSvg("home")}</span><span class="nav-label" ?hidden=${collapsed}> OpenProxy</span>
    </div>
    <nav>${renderLink(HOME_LINK, collapsed)}${GROUPS.map((g: SidebarGroup) => html`
      <div class="sidebar-nav-group">
        <div class="sidebar-nav-group-label" ?hidden=${collapsed}>${g.label}</div>
        ${g.links.map((l: SidebarLink) => renderLink(l, collapsed))}
      </div>`)}</nav>
    <div class="health">
      Health: <span id="health-status" class=${legacyHealthClass}><span class="health-dot ${dotClass}"></span> ${healthText}</span>
    </div>
    <div class="sidebar-footer">
      <button class="sidebar-toggle" type="button" data-action="toggleSidebar"
              title=${collapsed ? "Expand sidebar" : "Collapse sidebar"}
              aria-label=${collapsed ? "Expand sidebar" : "Collapse sidebar"}>${toggleLabel}</button>
      <span id="theme-toggle-slot"></span>
      <button class="sidebar-logout" type="button" data-action="logout"
              title=${t("nav.logout")}
              aria-label=${t("nav.logout")}
              ?hidden=${collapsed}>${t("nav.logout")}</button>
    </div>
  `, sb as HTMLElement);
  applyActiveState();
  mountThemeToggle();
}

window.addEventListener("hashchange", () => { queueMicrotask(applyActiveState); });
queueMicrotask(applyActiveState);

export function toggleSidebar(): void {
  const s = mutableState();
  const nextCollapsed = !s.ui?.sidebarCollapsed;
  s.ui = { ...(s.ui ?? {}), sidebarCollapsed: nextCollapsed };
  try { localStorage.setItem(STORAGE_KEY, nextCollapsed ? "1" : "0"); } catch (_e: unknown) {}
  renderSidebar();
}

/** Wipe the stored admin token and bounce to the login route.
 *  Registered as `data-action="logout"` in `handlers/registry.ts`
 *  so the sidebar button can dispatch via the same shim every
 *  other data-action uses. We deliberately don't also stop the
 *  bg-poll or close the WS here — `navigate()` re-evaluates on
 *  hashchange, the auth gate redirects to login, and the login
 *  view's mount path doesn't call `startBgPoll()` (it's already
 *  running from boot, but its 401s are silently swallowed by
 *  `bg-poll.ts::healthTick`'s catch). The WS, if connected,
 *  will be torn down by its own close handler when the server
 *  rejects the next frame — and `state/ws.ts::connectLogsWebSocket`
 *  won't be re-invoked until the user logs in again and a
 *  live-store-viewing route is mounted. */
export function logout(): void {
  clearToken();
  location.hash = "#/login";
}

export function loadSidebarCollapsedFromStorage(): void {
  const s = mutableState();
  if (s.ui && typeof s.ui.sidebarCollapsed === "boolean") return;
  let stored: string | null = null;
  try { stored = localStorage.getItem(STORAGE_KEY); } catch (_e: unknown) { stored = null; }
  s.ui = { ...(s.ui ?? {}), sidebarCollapsed: stored === "1" };
}
