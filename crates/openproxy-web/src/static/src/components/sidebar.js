// components/sidebar.js — renders the sidebar (brand, nav, health,
// collapse toggle). Re-renders the nav when the route changes so
// the active link stays in sync. The collapse state is held in
// `state.ui.sidebarCollapsed` and persisted to localStorage so the
// user's choice survives reloads and route changes.
//
// Icons are single unicode characters (no SVG, no font icons) per
// the sidebar-collapse spec. Each link's `icon` is rendered
// unconditionally; the `label` span is hidden via the [hidden]
// attribute when the sidebar is collapsed, and a body class
// (`sidebar-collapsed`) is toggled to drive the CSS column width.

import { state } from "../state/index.js";
import { mountThemeToggle } from "./theme-toggle.js";

// One unicode char per link. Pick visually distinct glyphs that
// render in a regular browser font (no ZWJ emoji, no SVG).
const LINKS = [
  { href: "#/",           icon: "⌂", label: "Home" },
  { href: "#/providers",  icon: "⛓", label: "Providers" },
  { href: "#/combos",     icon: "⌘", label: "Combos" },
  { href: "#/keys",       icon: "⚿", label: "API Keys" },
  { href: "#/analytics",  icon: "▦", label: "Analytics" },
  { href: "#/logs",       icon: "◐", label: "Live Logs" },
  { href: "#/config",     icon: "⚙", label: "Config" },
];

// localStorage key for the persisted collapse state. Read once at
// boot from app.js (see loadSidebarCollapsedFromStorage below) so
// the very first renderSidebar() call already sees the right value.
const STORAGE_KEY = "openproxy:sidebarCollapsed";

export function renderSidebar() {
  const sb = document.querySelector(".sidebar");
  if (!sb) return;
  const health = state.health;
  const healthClass = !health ? "loading"
    : (health.status === "ok" || health.status === "healthy") ? "ok"
    : "error";
  const healthText = !health ? "🟡 —"
    : healthClass === "ok" ? "🟢 healthy"
    : "🔴 " + (health.status || "down");
  // Read the persisted collapse flag. We default to false when the
  // field is missing so the very first visit shows the full
  // expanded sidebar.
  const collapsed = !!(state.ui && state.ui.sidebarCollapsed);
  // Drive the CSS column width via a body class so the grid in
  // layout.css can swap to the narrow column.
  document.body.classList.toggle("sidebar-collapsed", collapsed);
  // Toggle label: "«" when expanded (click collapses), "»" when
  // collapsed (click expands). Plain unicode; renders in mono.
  const toggleLabel = collapsed ? "»" : "«";
  const navLinks = LINKS.map((l) => `
    <a href="${l.href}" data-nav="${l.href}" title="${l.label}">
      <span class="nav-icon" aria-hidden="true">${l.icon}</span><span class="nav-label"${collapsed ? " hidden" : ""}> ${l.label}</span>
    </a>`).join("");
  sb.innerHTML = `
    <div class="brand">
      <span class="nav-icon" aria-hidden="true">⌂</span><span class="nav-label"${collapsed ? " hidden" : ""}> OpenProxy</span>
    </div>
    <nav>${navLinks}</nav>
    <div class="health">
      Health: <span id="health-status" class="${healthClass}">${healthText}</span>
    </div>
    <button class="sidebar-toggle" type="button" data-action="toggleSidebar"
            title="${collapsed ? "Expand sidebar" : "Collapse sidebar"}"
            aria-label="${collapsed ? "Expand sidebar" : "Collapse sidebar"}">${toggleLabel}</button>
  `;
  // The router also toggles .active on each nav link inside
  // navigate(), but a re-render of the sidebar (e.g. from the
  // collapse toggle) blows away those classes. Re-apply the active
  // class from the current hash so the sidebar stays self-consistent
  // after a render that didn't go through the router.
  const hash = location.hash || "#/";
  sb.querySelectorAll("nav a").forEach((a) => {
    const href = a.getAttribute("href") || "";
    a.classList.toggle("active", href === hash);
  });
  // Mount the theme toggle as a sibling of the health pill.
  mountThemeToggle();
}

// Re-exported for the toggle action in handlers/registry.js. We
// keep the persistence + re-render logic here (the place that
// knows about localStorage and the sidebar DOM) so the registry
// stays a thin action dispatcher.
export function toggleSidebar() {
  state.ui = state.ui || {};
  state.ui.sidebarCollapsed = !state.ui.sidebarCollapsed;
  try {
    localStorage.setItem(STORAGE_KEY, state.ui.sidebarCollapsed ? "1" : "0");
  } catch (_) {
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
export function loadSidebarCollapsedFromStorage() {
  if (state.ui && typeof state.ui.sidebarCollapsed === "boolean") return;
  let stored = null;
  try {
    stored = localStorage.getItem(STORAGE_KEY);
  } catch (_) {
    stored = null;
  }
  state.ui = state.ui || {};
  state.ui.sidebarCollapsed = stored === "1";
}
