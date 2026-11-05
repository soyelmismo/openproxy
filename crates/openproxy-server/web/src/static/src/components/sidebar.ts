// components/sidebar.ts — renders the sidebar (brand, nav, health, collapse toggle).

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
// Sidebar also badges the "Debug Logs" link with unviewed WARN+ERROR ring-buffer
// entries. The store polls every 30s, independent of the debug-logs view's 2s poll.
import {
  initDebugLogsStore,
  getUnviewedWarnErrorCount,
  onUnviewedWarnErrorCountChange,
} from "../state/debug-logs-store.js";
// Footer Logout button: `clearToken()` wipes the localStorage key + in-memory cache and
// navigates to `#/login`, which the router's auth gate admits once `isLoggedIn()` is false.
import { clearToken, isLoggedIn, isFullAdmin } from "../state/auth.js";
import { disconnectLogsWebSocket } from "../state/ws.js";
import { isPanelDocument } from "../state/workspace.js";

function mutableState() { return state; }

type NavIconName = "home" | "providers" | "combos" | "keys" | "playground" | "analytics" | "logs" | "debug-logs" | "config" | "notifications" | "proxies" | "proxy-sources";

interface SidebarLink {
  href: string;
  icon: NavIconName;
  label: string;
  /** Badge key: renders a red pill with the matching store's count, hidden when 0. */
  badgeKind?: "notifications-unread" | "debug-logs-unviewed";
}
interface SidebarGroup { label: string; links: SidebarLink[]; }

/** Nav glyphs come from `lib/icons.ts` ("Navigation (sidebar)") so the sidebar has no inline SVG. */
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

/** Resolve a nav icon through the centralized `icons` registry. */
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
    const active = isActive(aEl.getAttribute("href") || "");
    aEl.classList.toggle("active", active);
    // a11y: mark the active link for screen readers (no visual context available).
    if (active) {
      aEl.setAttribute("aria-current", "page");
    } else {
      aEl.removeAttribute("aria-current");
    }
  });
}

const STORAGE_KEY = "openproxy:sidebarCollapsed";

function renderLink(l: SidebarLink, collapsed: boolean): TemplateResult {
  // Badges render next to the nav label; count 0 uses lit-html's `nothing` sentinel so
  // no DOM node is emitted and the layout does not shift. `debug-logs-unviewed` surfaces
  // unviewed WARN+ERROR ring-buffer entries without navigating to the Debug Logs view.
  let badge: TemplateResult = html``;
  if (l.badgeKind === "notifications-unread") {
    const count: number = getUnreadCount();
    if (count > 0) {
      // Collapsed: the pill sits in the label's flex row, so it stands in for the label.
      const display: string = count > 99 ? "99+" : String(count);
      badge = html`<span class="sidebar-badge ${collapsed ? "collapsed" : ""}" title=${t("notifications.unread_count", { count })}>${display}</span>`;
    }
  } else if (l.badgeKind === "debug-logs-unviewed") {
    const count: number = getUnviewedWarnErrorCount();
    if (count > 0) {
      // Same red pill as the notifications badge (`.sidebar-badge` already uses
      // `var(--color-error)`); the title explains the number.
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

/** Bootstrap the notifications store + WS on first sidebar render. Idempotent.
 *
 *  Only after login: on the login page the sidebar is CSS-hidden but
 *  `renderSidebar()` still runs (mountShell() calls it before the auth gate), so
 *  bootstrapping earlier would open a WebSocket with no token and log a 401
 *  `ws://…/admin/ws` console error on the login screen. */
function maybeBootstrapNotifications(): void {
  if (storeBootstrapped) return;
  if (!isLoggedIn() || !isFullAdmin()) return;
  storeBootstrapped = true;
  initNotificationsStore();
  // Re-render on every count change; the notifications view also subscribes, which is fine
  // (lit-html's diff is cheap).
  onUnreadCountChange(() => {
    // Only the sidebar re-renders; the view handles its own updates.
    renderSidebar();
  });
}

/** Bootstrap the debug-logs store on first render. Idempotent. Gated on login for the
 *  same reason as `maybeBootstrapNotifications`: the 30s poll hits an authenticated
 *  endpoint and would 401 every 30s before login. */
function maybeBootstrapDebugLogs(): void {
  if (debugLogsStoreBootstrapped) return;
  if (!isLoggedIn() || !isFullAdmin()) return;
  debugLogsStoreBootstrapped = true;
  initDebugLogsStore();
  // Re-render on every unviewed-count change so the badge tracks new WARN+ERROR entries.
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
  if (isPanelDocument()) return;
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
      <nav @click=${handleNavClick}>${isFullAdmin() ? renderLink(HOME_LINK, collapsed) : html``}${
        isFullAdmin()
          ? GROUPS.map((g: SidebarGroup) => html`
              <div class="sidebar-nav-group">
                <div class="sidebar-nav-group-label" ?hidden=${collapsed}>${g.label}</div>
                ${g.links.map((l: SidebarLink) => renderLink(l, collapsed))}
              </div>`)
          : html`
              <div class="sidebar-nav-group">
                <div class="sidebar-nav-group-label" ?hidden=${collapsed}>Insights</div>
                ${renderLink({ href: "#/analytics", icon: "analytics", label: "Analytics" }, collapsed)}
              </div>`
      }</nav>
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
  s.ui = { ...s.ui, sidebarCollapsed: nextCollapsed };
  try { localStorage.setItem(STORAGE_KEY, nextCollapsed ? "1" : "0"); } catch {}
  renderSidebar();
}

/** Wipe the stored admin token and bounce to `#/login`; registered as `data-action="logout"`
 *  in `handlers/registry.ts` so it dispatches through the same shim as every other action.
 *  We deliberately don't stop the bg-poll or close the WS: the auth gate redirects to
 *  login, `healthTick`'s catch swallows the poll's 401s, and the WS tears itself down when
 *  the server rejects the next frame. `connectLogsWebSocket` is not re-invoked until the
 *  user logs back in and mounts a live-store route. */
export function logout(): void {
  disconnectLogsWebSocket();
  clearToken();
  location.hash = "#/login";
}

export function loadSidebarCollapsedFromStorage(): void {
  const s = mutableState();
  let stored: string | null = null;
  try { stored = localStorage.getItem(STORAGE_KEY); } catch { stored = null; }
  if (stored !== null) {
    s.ui = { ...s.ui, sidebarCollapsed: stored === "1" };
  }
}
