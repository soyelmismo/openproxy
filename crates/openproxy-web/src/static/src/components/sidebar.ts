// components/sidebar.ts — renders the sidebar (brand, nav, health,
// collapse toggle). Re-renders the nav when the route changes so
// the active link stays in sync. The collapse state is held in
// `state.ui.sidebarCollapsed` and persisted to localStorage so the
// user's choice survives reloads and route changes.
//
// Icons are inline 16×16 SVGs (1px stroke, `currentColor`) returned
// from `navIcon(name)`. They render consistently across OSes unlike
// the previous unicode glyphs (⌂ ⛓ ⌘ ⚿ ▦ ◐ ⚙). The `.nav-icon` span
// is sized 16×16 by `views.css` and the SVG fills it.
//
// Active-link matching is *prefix* so a sub-route like
// `#/providers/openai` keeps "Providers" highlighted. The router in
// `state/router.ts` still applies an exact-match `.active` class
// inside `navigate()` (we can't edit the router from here), so we
// re-apply prefix-match after each `hashchange` via a deferred
// `queueMicrotask`. That microtask runs after the router's sync
// listener completes, overriding the exact-match application.

import { state } from "../state/index.js";
import { mountThemeToggle } from "./theme-toggle.js";
import { escapeHtml } from "../lib/escape.js";

// `state.ui` holds the per-session UI prefs (currently just the
// sidebar collapse flag). The shape is open on purpose: handlers
// may add more fields. We cast through `unknown` to keep the
// dashboard's `DashboardState` type free of optional UI fields
// (out of G4 scope to add them).
interface UiState {
  sidebarCollapsed?: boolean;
}
type MutableDashboard = { ui?: UiState };
function mutableState(): MutableDashboard {
  return state as unknown as MutableDashboard;
}

type NavIconName = "home" | "providers" | "combos" | "keys" | "analytics" | "logs" | "debug-logs" | "config";

interface SidebarLink {
  href: string;
  icon: NavIconName;
  label: string;
}

interface SidebarGroup {
  label: string;
  links: SidebarLink[];
}

// Home lives above the groups (no group label).
const HOME_LINK: SidebarLink = { href: "#/", icon: "home", label: "Home" };

const GROUPS: readonly SidebarGroup[] = [
  {
    label: "Inventory",
    links: [
      { href: "#/providers", icon: "providers", label: "Providers" },
      { href: "#/combos",    icon: "combos",    label: "Combos" },
      { href: "#/keys",      icon: "keys",      label: "API Keys" },
    ],
  },
  {
    label: "Insights",
    links: [
      { href: "#/analytics", icon: "analytics", label: "Analytics" },
      { href: "#/logs",      icon: "logs",      label: "Live Logs" },
      { href: "#/debug-logs", icon: "debug-logs", label: "Debug Logs" },
    ],
  },
  {
    label: "System",
    links: [
      { href: "#/config",    icon: "config",    label: "Config" },
    ],
  },
];

// Inline 16×16 SVG icons. 1px stroke, `currentColor`, no fill so the
// icon inherits the link's text colour (including the active state).
// Kept deliberately minimal — these are nav glyphs, not hero artwork.
function navIcon(name: NavIconName): string {
  switch (name) {
    case "home":
      // House outline + door.
      return `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><path d="M2 7 L8 2 L14 7 V14 H2 Z"/><path d="M6 14 V10 H10 V14"/></svg>`;
    case "providers":
      // Server stack: three horizontal slabs with a status dot each.
      return `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><rect x="2" y="3" width="12" height="3" rx="0.5"/><rect x="2" y="7" width="12" height="3" rx="0.5"/><rect x="2" y="11" width="12" height="3" rx="0.5"/><circle cx="4" cy="4.5" r="0.4" fill="currentColor" stroke="none"/><circle cx="4" cy="8.5" r="0.4" fill="currentColor" stroke="none"/><circle cx="4" cy="12.5" r="0.4" fill="currentColor" stroke="none"/></svg>`;
    case "combos":
      // Two overlapping rounded rectangles (a shuffle/layered look).
      return `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><rect x="2" y="2" width="9" height="9" rx="1"/><rect x="5" y="5" width="9" height="9" rx="1"/></svg>`;
    case "keys":
      // Key bow (circle) + shaft + two teeth.
      return `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><circle cx="5" cy="5" r="2.5"/><path d="M6.8 6.8 L13 13"/><path d="M11 11 L13 9"/><path d="M9 13 L11 11"/></svg>`;
    case "analytics":
      // Bar chart: baseline + three bars of different heights.
      return `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><path d="M2 14 H14"/><rect x="3" y="9"  width="2.5" height="5"/><rect x="6.75" y="5" width="2.5" height="9"/><rect x="10.5" y="7" width="2.5" height="7"/></svg>`;
    case "logs":
      // Pulse / heartbeat line.
      return `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" stroke-linecap="round"><path d="M2 8 H5 L7 3 L9 13 L11 8 H14"/></svg>`;
    case "debug-logs":
      // Bug silhouette: oval body + head circle + three legs per
      // side. Distinguishes Debug Logs from Live Logs at a glance.
      return `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" stroke-linecap="round"><ellipse cx="8" cy="9" rx="3.5" ry="4"/><circle cx="8" cy="4" r="1.2"/><path d="M8 5 V5.5"/><path d="M4.5 7 L2 6 M4.5 9 L1.5 9 M4.5 11 L2.5 12.5"/><path d="M11.5 7 L14 6 M11.5 9 L14.5 9 M11.5 11 L13.5 12.5"/></svg>`;
    case "config":
      // Gear: inner circle + 8 teeth around it.
      return `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><circle cx="8" cy="8" r="2.5"/><path d="M8 1 V3.5 M8 12.5 V15 M1 8 H3.5 M12.5 8 H15 M3 3 L4.8 4.8 M11.2 11.2 L13 13 M3 13 L4.8 11.2 M11.2 4.8 L13 3"/></svg>`;
    default:
      return "";
  }
}

// Prefix-match active state. `#/` is special-cased so Home is only
// active on the empty/root hash (otherwise it would match every
// route via `startsWith`).
function isActive(href: string): boolean {
  if (href === "#/") return location.hash === "#/" || location.hash === "";
  return location.hash.startsWith(href);
}

// Re-apply `.active` on every sidebar nav link based on the current
// hash. Called from `renderSidebar` after the innerHTML is replaced
// and from the `hashchange` microtask below to override the router's
// exact-match application.
function applyActiveState(): void {
  const sb: HTMLElement | null = document.querySelector(".sidebar");
  if (!sb) return;
  sb.querySelectorAll("nav a").forEach((a: Element) => {
    const aEl: HTMLElement = a as HTMLElement;
    const href: string = aEl.getAttribute("href") || "";
    aEl.classList.toggle("active", isActive(href));
  });
}

// localStorage key for the persisted collapse state. Read once at
// boot from app.js (see loadSidebarCollapsedFromStorage below) so
// the very first renderSidebar() call already sees the right value.
const STORAGE_KEY = "openproxy:sidebarCollapsed";

function renderLink(l: SidebarLink, collapsed: boolean): string {
  return `<a href="${l.href}" data-nav="${l.href}" title="${l.label}">
      <span class="nav-icon" aria-hidden="true">${navIcon(l.icon)}</span><span class="nav-label"${collapsed ? " hidden" : ""}> ${l.label}</span>
    </a>`;
}

export function renderSidebar(): void {
  const sb: HTMLElement | null = document.querySelector(".sidebar");
  if (!sb) return;
  const health: { status: string; message?: string } | null = state.health;
  // Legacy `#health-status` class (ok/error/loading) — kept because
  // layout.css styles the text colour via these three classes. The
  // new `.health-dot` classes (ok/warn/err) drive the dot fill.
  const legacyHealthClass: string = !health ? "loading"
    : (health.status === "ok" || health.status === "healthy") ? "ok"
    : "error";
  const dotClass: string = !health ? "warn"
    : (health.status === "ok" || health.status === "healthy") ? "ok"
    : "err";
  const healthText: string = !health ? "—"
    : (health.status === "ok" || health.status === "healthy") ? "healthy"
    : (health.status || "down");
  // Read the persisted collapse flag. We default to false when the
  // field is missing so the very first visit shows the full
  // expanded sidebar.
  const collapsed: boolean = !!mutableState().ui?.sidebarCollapsed;
  // Drive the CSS column width via a body class so the grid in
  // layout.css can swap to the narrow column.
  document.body.classList.toggle("sidebar-collapsed", collapsed);
  // Toggle label: "«" when expanded (click collapses), "»" when
  // collapsed (click expands). Plain unicode; renders in mono.
  const toggleLabel: string = collapsed ? "»" : "«";
  const homeLink: string = renderLink(HOME_LINK, collapsed);
  const groupsHtml: string = GROUPS.map((g: SidebarGroup) => `
      <div class="sidebar-nav-group">
        <div class="sidebar-nav-group-label"${collapsed ? " hidden" : ""}>${escapeHtml(g.label)}</div>
        ${g.links.map((l: SidebarLink) => renderLink(l, collapsed)).join("")}
      </div>`).join("");
  sb.innerHTML = `
    <div class="brand">
      <span class="nav-icon" aria-hidden="true">${navIcon("home")}</span><span class="nav-label"${collapsed ? " hidden" : ""}> OpenProxy</span>
    </div>
    <nav>${homeLink}${groupsHtml}</nav>
    <div class="health">
      Health: <span id="health-status" class="${legacyHealthClass}"><span class="health-dot ${dotClass}"></span> ${escapeHtml(healthText)}</span>
    </div>
    <div class="sidebar-footer">
      <button class="sidebar-toggle" type="button" data-action="toggleSidebar"
              title="${collapsed ? "Expand sidebar" : "Collapse sidebar"}"
              aria-label="${collapsed ? "Expand sidebar" : "Collapse sidebar"}">${toggleLabel}</button>
      <span id="theme-toggle-slot"></span>
    </div>
  `;
  // Apply prefix-match active state from the current hash. The
  // router also toggles .active inside navigate() but uses exact
  // match; the hashchange microtask below overrides that on every
  // navigation. We call it here too so a re-render from
  // toggleSidebar / theme-toggle stays self-consistent.
  applyActiveState();
  // Mount the theme toggle as a sibling of the health pill.
  mountThemeToggle();
}

// Override the router's exact-match active-state application.
//
// sidebar.ts is imported (and this listener registered) BEFORE
// installRouter() runs in app.ts, so on `hashchange` our listener
// fires FIRST (in registration order). We schedule a microtask that
// runs AFTER all sync `hashchange` listeners (including the router's
// `navigate`) complete, then re-applies prefix-match — overriding
// whatever the router just set.
//
// The initial `navigate()` call from app.ts is NOT a `hashchange`
// event, so we also queue one microtask at module load to cover the
// first paint.
window.addEventListener("hashchange", () => {
  queueMicrotask(applyActiveState);
});
queueMicrotask(applyActiveState);

// Re-exported for the toggle action in handlers/registry.js. We
// keep the persistence + re-render logic here (the place that
// knows about localStorage and the sidebar DOM) so the registry
// stays a thin action dispatcher.
export function toggleSidebar(): void {
  const s: MutableDashboard = mutableState();
  const nextCollapsed: boolean = !s.ui?.sidebarCollapsed;
  s.ui = { ...(s.ui ?? {}), sidebarCollapsed: nextCollapsed };
  try {
    localStorage.setItem(STORAGE_KEY, s.ui.sidebarCollapsed ? "1" : "0");
  } catch (_e: unknown) {
    // localStorage can throw in private modes or when storage is
    // disabled. The UI still works in-memory; we just can't
    // persist the choice.
  }
  renderSidebar();
}

// Called once from app.js BEFORE the first renderSidebar() so the
// initial paint already reflects the persisted choice. We only read
// localStorage if state.ui is not yet set — handlers may have
// already initialised it by the time we run, and we don't want to
// stomp a programmatic value.
export function loadSidebarCollapsedFromStorage(): void {
  const s: MutableDashboard = mutableState();
  if (s.ui && typeof s.ui.sidebarCollapsed === "boolean") return;
  let stored: string | null = null;
  try {
    stored = localStorage.getItem(STORAGE_KEY);
  } catch (_e: unknown) {
    stored = null;
  }
  s.ui = { ...(s.ui ?? {}), sidebarCollapsed: stored === "1" };
}
