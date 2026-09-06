// components/sidebar.ts — renders the sidebar (brand, nav, health, collapse toggle).
// Migrated to lit-html: uses render() instead of innerHTML.

import { html, render, type TemplateResult } from 'lit-html';
import { state } from "../state/index.js";
import { mountThemeToggle } from "./theme-toggle.js";
import { t } from "../i18n/index.js";
import { icons } from "../lib/icons.js";
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
import { disconnectLogsWebSocket } from "../state/ws.js";

function mutableState() { return state; }

type NavIconName = "home" | "providers" | "combos" | "keys" | "playground" | "analytics" | "logs" | "debug-logs" | "config" | "notifications" | "proxies" | "proxy-sources";

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

/** Q18: the 12 sidebar navigation glyphs are centralized in
 *  `lib/icons.ts` (section "Navigation (sidebar)"). This lookup maps
 *  the route-level nav names to their icon constructors so the
 *  sidebar contains no inline SVG markup. */
const NAV_ICONS: Record<NavIconName, (cls?: string) => TemplateResult> = {
  home: icons.navHome,
  providers: icons.navProviders,
  combos: icons.navCombos,
  keys: icons.navKeys,
  playground: icons.navPlayground,
  analytics: icons.navAnalytics,
  logs: icons.navLogs,
  "debug-logs": icons.navDebugLogs,
  config: icons.navConfig,
  notifications: icons.navNotifications,
  proxies: icons.navProxies,
  "proxy-sources": icons.navProxySources,
};

/** Resolve a nav icon by name via the centralized `icons` registry. */
function navIcon(name: NavIconName): TemplateResult {
  return NAV_ICONS[name]();
}

const HOME_LINK: SidebarLink = { href: "#/", icon: "home", label: "Home" };

const GROUPS: readonly SidebarGroup[] = [
  { label: "Inventory", links: [
    { href: "#/providers", icon: "providers", label: "Providers" },
    { href: "#/combos", icon: "combos", label: "Combos" },
    { href: "#/keys", icon: "keys", label: "API Keys" },
    { href: "#/playground", icon: "playground", label: "Playground" },
    { href: "#/proxies", icon: "proxies", label: "Free Proxies" },
    { href: "#/proxy-sources", icon: "proxy-sources", label: "Proxy Sources" },
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
    <span class="nav-icon" aria-hidden="true">${navIcon(l.icon)}</span><span class="nav-label" ?hidden=${collapsed}> ${l.label}</span>${badge}
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

let mobileNavOpen: boolean = false;

export function toggleMobileNav(): void {
  mobileNavOpen = !mobileNavOpen;
  renderSidebar();
}

export function closeMobileNav(): void {
  if (mobileNavOpen) {
    mobileNavOpen = false;
    renderSidebar();
  }
}

function handleNavClick(): void {
  if (window.innerWidth <= 768) {
    closeMobileNav();
  }
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
  document.body.classList.toggle("mobile-nav-open", mobileNavOpen);
  const toggleIcon = collapsed ? icons.chevronsRight() : icons.chevronsLeft();

  render(html`
    <div class="mobile-topbar mobile-only">
      <div class="brand">
        <span>OpenProxy</span>
      </div>
      <div class="mobile-topbar-actions">
        <span class="health-dot ${dotClass}" title="Health: ${healthText}"></span>
        <button class="mobile-nav-toggle" type="button" data-action="toggleMobileNav"
                aria-label=${mobileNavOpen ? "Close navigation" : "Open navigation"}>
          ${mobileNavOpen ? icons.close() : icons.menu()}
        </button>
      </div>
    </div>

    <div class="sidebar-backdrop mobile-only" @click=${closeMobileNav} ?hidden=${!mobileNavOpen}></div>

    <div class="sidebar-drawer">
      <div class="brand desktop-only">
        <span class="nav-label" ?hidden=${collapsed}>OpenProxy</span>
        ${collapsed ? html`<span>OP</span>` : html``}
      </div>
      <div class="mobile-drawer-header mobile-only">
        <div class="brand">OpenProxy</div>
        <button class="mobile-nav-close" type="button" data-action="toggleMobileNav" aria-label="Close menu">${icons.close()}</button>
      </div>
      <nav @click=${handleNavClick}>${renderLink(HOME_LINK, collapsed)}${GROUPS.map((g: SidebarGroup) => html`
        <div class="sidebar-nav-group">
          <div class="sidebar-nav-group-label" ?hidden=${collapsed}>${g.label}</div>
          ${g.links.map((l: SidebarLink) => renderLink(l, collapsed))}
        </div>`)}</nav>
      <div class="health">
        ${collapsed
          ? html`<span id="health-status" class=${legacyHealthClass} title="Health: ${healthText}"><span class="health-dot ${dotClass}"></span></span>`
          : html`Health: <span id="health-status" class=${legacyHealthClass}><span class="health-dot ${dotClass}"></span> ${healthText}</span>`}
      </div>
      <div class="sidebar-footer">
        <button class="sidebar-toggle desktop-only" type="button" data-action="toggleSidebar"
                title=${collapsed ? "Expand sidebar" : "Collapse sidebar"}
                aria-label=${collapsed ? "Expand sidebar" : "Collapse sidebar"}>${toggleIcon}</button>
        <span id="theme-toggle-slot"></span>
        <button class="sidebar-logout" type="button" data-action="logout"
                title=${t("nav.logout")}
                aria-label=${t("nav.logout")}
                ?hidden=${collapsed}>${icons.logout()} ${t("nav.logout")}</button>
      </div>
    </div>
  `, sb as HTMLElement);
  applyActiveState();
  mountThemeToggle();
}

window.addEventListener("hashchange", () => {
  closeMobileNav();
  queueMicrotask(applyActiveState);
});
queueMicrotask(applyActiveState);

window.addEventListener("keydown", (e: KeyboardEvent) => {
  if (e.key === "Escape" && mobileNavOpen) {
    closeMobileNav();
  }
});

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
  disconnectLogsWebSocket();
  clearToken();
  location.hash = "#/login";
}

export function loadSidebarCollapsedFromStorage(): void {
  const s = mutableState();
  let stored: string | null = null;
  try { stored = localStorage.getItem(STORAGE_KEY); } catch (_e: unknown) { stored = null; }
  if (stored !== null) {
    s.ui = { ...(s.ui ?? {}), sidebarCollapsed: stored === "1" };
  }
}
