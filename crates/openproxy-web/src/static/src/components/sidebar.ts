// components/sidebar.ts — renders the sidebar (brand, nav, health, collapse toggle).
// Migrated to lit-html: uses render() instead of innerHTML.

import { html, render, type TemplateResult } from 'lit-html';
import { unsafeHTML } from 'lit-html/directives/unsafe-html.js';
import { state } from "../state/index.js";
import { mountThemeToggle } from "./theme-toggle.js";

interface UiState { sidebarCollapsed?: boolean; }
type MutableDashboard = { ui?: UiState };
function mutableState(): MutableDashboard { return state as unknown as MutableDashboard; }

type NavIconName = "home" | "providers" | "combos" | "keys" | "analytics" | "logs" | "debug-logs" | "config";

interface SidebarLink { href: string; icon: NavIconName; label: string; }
interface SidebarGroup { label: string; links: SidebarLink[]; }

const HOME_LINK: SidebarLink = { href: "#/", icon: "home", label: "Home" };

const GROUPS: readonly SidebarGroup[] = [
  { label: "Inventory", links: [
    { href: "#/providers", icon: "providers", label: "Providers" },
    { href: "#/combos", icon: "combos", label: "Combos" },
    { href: "#/keys", icon: "keys", label: "API Keys" },
  ]},
  { label: "Insights", links: [
    { href: "#/analytics", icon: "analytics", label: "Analytics" },
    { href: "#/logs", icon: "logs", label: "Live Logs" },
    { href: "#/debug-logs", icon: "debug-logs", label: "Debug Logs" },
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
    analytics: `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><path d="M2 14 H14"/><rect x="3" y="9" width="2.5" height="5"/><rect x="6.75" y="5" width="2.5" height="9"/><rect x="10.5" y="7" width="2.5" height="7"/></svg>`,
    logs: `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" stroke-linecap="round"><path d="M2 8 H5 L7 3 L9 13 L11 8 H14"/></svg>`,
    "debug-logs": `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" stroke-linecap="round"><ellipse cx="8" cy="9" rx="3.5" ry="4"/><circle cx="8" cy="4" r="1.2"/><path d="M8 5 V5.5"/><path d="M4.5 7 L2 6 M4.5 9 L1.5 9 M4.5 11 L2.5 12.5"/><path d="M11.5 7 L14 6 M11.5 9 L14.5 9 M11.5 11 L13.5 12.5"/></svg>`,
    config: `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2"><circle cx="8" cy="8" r="2.5"/><path d="M8 1 V3.5 M8 12.5 V15 M1 8 H3.5 M12.5 8 H15 M3 3 L4.8 4.8 M11.2 11.2 L13 13 M3 13 L4.8 11.2 M11.2 4.8 L13 3"/></svg>`,
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
  return html`<a href=${l.href} data-nav=${l.href} title=${l.label}>
    <span class="nav-icon" aria-hidden="true">${navIconSvg(l.icon)}</span><span class="nav-label" ?hidden=${collapsed}> ${l.label}</span>
  </a>`;
}

export function renderSidebar(): void {
  const sb = document.querySelector(".sidebar");
  if (!sb) return;
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

export function loadSidebarCollapsedFromStorage(): void {
  const s = mutableState();
  if (s.ui && typeof s.ui.sidebarCollapsed === "boolean") return;
  let stored: string | null = null;
  try { stored = localStorage.getItem(STORAGE_KEY); } catch (_e: unknown) { stored = null; }
  s.ui = { ...(s.ui ?? {}), sidebarCollapsed: stored === "1" };
}
