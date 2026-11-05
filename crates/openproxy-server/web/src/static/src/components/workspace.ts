// Native tiling host. Views share the document, with explicit targets and teardown.
import { html, svg, render, type TemplateResult } from "lit-html";
import { repeat } from "lit-html/directives/repeat.js";
import { icons } from "../lib/icons.js";
import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { clearToken, isLoggedIn, isFullAdmin } from "../state/auth.js";
import { renderSidebar } from "./sidebar.js";
import { bootstrapTheme } from "../state/theme.js";
import { t } from "../i18n/index.js";
import { parseHash } from "../state/router.js";
import { requestUpdate } from "../state/reactive.js";
import { clearNativePanels, reconcileNativePanels } from "./native-panels.js";
import {
  WORKSPACE_KEY, VIEWS, PRESETS, activeTile, applyPreset, changeLayout, closeTile,
  openSplit, restoreWorkspace, routeMeta, safeRoute, clampSplit,
  type Layout, type Tile, type WorkspaceState,
} from "../state/workspace.js";

let workspace: WorkspaceState | null = null;
let initialized = false;
let menu: string | null = null;
let search = "";
let resourcesError = false;
let resourcesLoading = false;
export function workspaceMounted(): boolean { return workspace !== null; }
export function unmountWorkspace(): void {
  clearNativePanels();
  const root = document.getElementById("workspace");
  if (root) render(null, root);
  workspace = null;
  menu = null;
}
function persist(): void { try { localStorage.setItem(WORKSPACE_KEY, JSON.stringify(workspace)); } catch { /* session remains usable */ } }
function updateHash(): void {
  if (!workspace) return;
  const route = activeTile(workspace).route;
  const parsed = parseHash(route);
  if (parsed) state.currentView = { name: parsed.name, context: parsed.context };
  if (location.hash !== route) history.replaceState(null, "", route);
  renderSidebar();
}
function changed(): void { persist(); paint(); updateHash(); }
function focus(id: string): void {
  if (!workspace || workspace.focused === id) return;
  workspace.focused = id;
  menu = null;
  changed();
}
export function navigateWorkspace(route: string, split = false): void {
  if (!workspace || !safeRoute(route)) return;
  // Legacy family stores are single-owner. Reuse their existing panel instead
  // of allowing two owners to overwrite IDs, handlers and lifecycle state.
  const existing = workspace.tiles.find(tile => routeMeta(tile.route).route === routeMeta(route).route);
  if (existing) { workspace.focused = existing.id; existing.route = route; workspace.maximized = null; }
  else if (split) openSplit(workspace, route);
  else activeTile(workspace).route = route;
  menu = null;
  changed();
}
function icon(route: string): TemplateResult {
  switch (routeMeta(route).route) {
    case "#/providers": return icons.navProviders();
    case "#/combos": return icons.navCombos();
    case "#/keys": return icons.navKeys();
    case "#/logs": return icons.navLogs();
    case "#/analytics": return icons.navAnalytics();
    case "#/playground": return icons.navPlayground();
    case "#/config": return icons.navConfig();
    case "#/notifications": return icons.navNotifications();
    case "#/debug-logs": return icons.navDebugLogs();
    default: return icons.desktop();
  }
}
function layoutIcon(count: number): TemplateResult {
  return html`<svg class="icon" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2" aria-hidden="true"><rect x="1.5" y="2" width="13" height="12" rx="1.5"/>${count > 1 ? svg`<path d="M8 2v12"/>` : ""}${count === 3 ? svg`<path d="M8 8h6.5"/>` : count === 4 ? svg`<path d="M1.5 8h13"/>` : ""}</svg>`;
}
function maximizeIcon(restore: boolean): TemplateResult {
  return html`<svg class="icon" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" aria-hidden="true">${restore ? svg`<rect x="4" y="2" width="9" height="9" rx="1"/><path d="M2 5v8h8"/>` : svg`<path d="M2 6V2h4M10 2h4v4M14 10v4h-4M6 14H2v-4"/>`}</svg>`;
}
function toggleMenu(name: string): void { menu = menu === name ? null : name; paint(); }
function viewLabel(route: string, fallback: string): string {
  const v = routeMeta(route);
  const routeKey = v.route.replace(/^#\/?/, "").replace(/[^a-zA-Z0-9_-]/g, "_") || "dashboard";
  const mapped = routeKey === "dashboard" ? "dashboard" :
    routeKey === "providers" ? "providers" :
    routeKey === "combos" ? "combos" :
    routeKey === "keys" ? "keys" :
    routeKey === "proxies" ? "proxies" :
    routeKey === "proxy-sources" ? "proxy_sources" :
    routeKey === "logs" ? "logs" :
    routeKey === "analytics" ? "analytics" :
    routeKey === "debug-logs" ? "debug_logs" :
    routeKey === "playground" ? "playground" :
    routeKey === "notifications" ? "notifications" :
    routeKey === "config" ? "config" : routeKey;
  const translated = t(`workspace.view.${mapped}`);
  return translated !== `workspace.view.${mapped}` ? translated : fallback;
}

function groupLabel(group: string): string {
  const key = group === "Overview" ? "overview" :
    group === "Routing" ? "routing" :
    group === "Access & network" ? "access_network" :
    group === "Observability" ? "observability" :
    group === "Tools" ? "tools" :
    group === "System" ? "system" : "";
  if (!key) return group;
  const translated = t(`workspace.group.${key}`);
  return translated !== `workspace.group.${key}` ? translated : group;
}

function presetLabel(label: string): string {
  const key = label === "Routing desk" ? "routing_desk" :
    label === "Observe & inspect" ? "observe_inspect" :
    label === "Operations" ? "operations" :
    label === "Overview grid" ? "overview_grid" : "";
  if (!key) return label;
  const translated = t(`workspace.preset.${key}`);
  return translated !== `workspace.preset.${key}` ? translated : label;
}

function layoutTitle(index: number): string {
  const keys = [
    "workspace.layout_single",
    "workspace.layout_split",
    "workspace.layout_triple",
    "workspace.layout_grid",
  ];
  return t(keys[index] || "workspace.layout_single");
}

function viewMenu(id: string): TemplateResult {
  const allowedViews = isFullAdmin()
    ? VIEWS
    : VIEWS.filter(v => v.route === "#/analytics");

  return html`<div class="workspace-menu view-menu" role="menu" aria-label=${t("workspace.panel_view")}>${allowedViews.map(v => html`
    <button role="menuitem" @click=${() => {
      if (!workspace) return;
      workspace.focused = id;
      navigateWorkspace(v.route);
    }}>${icon(v.route)} ${viewLabel(v.route, v.label)}</button>`)}</div>`;
}
function tileTemplate(tile: Tile, index: number): TemplateResult {
  const s = workspace!;
  const hidden = s.maximized !== null ? s.maximized !== tile.id : false;
  const label = viewLabel(tile.route, routeMeta(tile.route).label);
  return html`<section class="workspace-tile ${s.focused === tile.id ? "is-focused" : ""}"
    data-panel=${tile.id} data-index=${index} ?hidden=${hidden}
    aria-label=${t("workspace.panel_label", { index: index + 1, label })} @pointerdown=${() => focus(tile.id)} @focusin=${() => focus(tile.id)}>
    <header class="tile-header" @pointerdown=${() => focus(tile.id)}>
      <div class="tile-view-picker">
        <button class="tile-title" aria-haspopup="menu" aria-expanded=${menu === tile.id}
          @click=${() => toggleMenu(tile.id)}>${icon(tile.route)}<span>${label}</span>${icons.caretDown()}</button>
        ${menu === tile.id ? viewMenu(tile.id) : ""}
      </div>
      <div class="tile-actions">
        <span class="tile-position" title=${t("workspace.panel_number")}>${index + 1}</span>
        ${s.tiles.length > 1 ? html`<button class="workspace-icon-button" aria-label=${s.maximized === tile.id ? t("workspace.restore_panel") : t("workspace.maximize_panel")}
          title=${s.maximized === tile.id ? t("workspace.restore_panel") : t("workspace.maximize_panel")}
          @click=${() => { s.focused = tile.id; s.maximized = s.maximized === tile.id ? null : tile.id; changed(); }}>${maximizeIcon(s.maximized === tile.id)}</button>
          <button class="workspace-icon-button" aria-label=${t("workspace.close_panel")} title=${t("workspace.close_panel")}
            @click=${() => { closeTile(s, tile.id); changed(); }}>${icons.close()}</button>` : ""}
      </div>
    </header>
    <div class="tile-content" data-tile=${tile.id}></div>
  </section>`;
}
function resize(axis: "column" | "row", e: PointerEvent): void {
  if (!workspace || e.button !== 0) return;
  e.preventDefault();
  const grid = document.querySelector<HTMLElement>(".workspace-grid")!;
  const rect = grid.getBoundingClientRect();
  const target = e.currentTarget as HTMLElement;
  target.setPointerCapture(e.pointerId);
  document.body.classList.add("workspace-resizing");
  const move = (ev: PointerEvent): void => {
    const start = axis === "column" ? rect.left : rect.top;
    const size = axis === "column" ? rect.width : rect.height;
    workspace![axis] = clampSplit(((axis === "column" ? ev.clientX : ev.clientY) - start) / Math.max(1, size) * 100);
    grid.style.setProperty(axis === "column" ? "--workspace-column" : "--workspace-row", `${workspace![axis]}%`);
  };
  const stop = (): void => {
    target.removeEventListener("pointermove", move);
    target.removeEventListener("pointerup", stop);
    target.removeEventListener("pointercancel", stop);
    target.removeEventListener("lostpointercapture", stop);
    document.body.classList.remove("workspace-resizing");
    changed();
  };
  target.addEventListener("pointermove", move);
  target.addEventListener("pointerup", stop);
  target.addEventListener("pointercancel", stop);
  target.addEventListener("lostpointercapture", stop);
}
function divider(axis: "column" | "row"): TemplateResult {
  return html`<div class="workspace-divider divider-${axis}" role="separator" tabindex="0"
    aria-label=${t(axis === "column" ? "workspace.resize_columns" : "workspace.resize_rows")}
    aria-orientation=${axis === "column" ? "vertical" : "horizontal"}
    aria-valuenow=${Math.round(workspace![axis])} aria-valuemin="25" aria-valuemax="75"
    @pointerdown=${(e: PointerEvent) => resize(axis, e)}
    @dblclick=${() => { workspace![axis] = 50; changed(); }}
    @keydown=${(e: KeyboardEvent) => {
      const direction = ["ArrowLeft", "ArrowUp"].includes(e.key) ? -1 : ["ArrowRight", "ArrowDown"].includes(e.key) ? 1 : 0;
      if (!direction && e.key !== "Home") return;
      e.preventDefault();
      workspace![axis] = e.key === "Home" ? 50 : clampSplit(workspace![axis] + direction * (e.shiftKey ? 10 : 2));
      changed();
    }}></div>`;
}
function contextualResources(): TemplateResult {
  if (!isFullAdmin()) {
    return html`<div class="context-resources"></div>`;
  }
  const route = routeMeta(activeTile(workspace!).route).route;
  const needle = search.toLowerCase();
  const combos = route === "#/combos";
  const providers = route === "#/providers";
  const keys = route === "#/keys";
  const hasResources = combos || providers || keys;
  const heading = hasResources ? viewLabel(route, routeMeta(route).label) : t("workspace.quick_access");
  const list = combos ? state.combos.map(c => ({ route: `#/combos/${c.id}`, label: c.name, sub: c.strategy })) :
    providers ? state.providers.map(p => ({ route: `#/providers/${encodeURIComponent(p.id)}`, label: p.name || p.id, sub: p.id })) :
    keys ? state.apiKeys.map(k => ({ route: `#/keys/${k.id}`, label: k.label || `#${k.id}`, sub: `${k.key_prefix || `#${k.id}`} · ${t(k.revoked_at ? "keys.list.status.revoked" : k.is_active ? "keys.list.status.active" : "keys.list.status.inactive")}` })) : [];
  return html`<div class="context-resources">
    <div class="context-section-heading"><strong>${heading}</strong>
      ${hasResources ? html`<button class="workspace-icon-button" title=${t("workspace.refresh_resources")} aria-label=${t("workspace.refresh_resources")} @click=${loadResources}>${icons.refresh()}</button>` : ""}</div>
    ${hasResources ? html`<label class="context-search">${icons.search()}<input aria-label=${t("workspace.search_aria")} placeholder=${t("workspace.search_placeholder")} .value=${search}
      @input=${(e: Event) => { search = (e.target as HTMLInputElement).value; paintContext(); }}></label>
      ${resourcesError ? html`<p class="context-hint">${t("workspace.load_error")} <button @click=${loadResources}>${t("common.retry")}</button></p>` : resourcesLoading && list.length === 0 ? html`<p class="context-hint" role="status">${t("workspace.loading")}</p>` : ""}
      <nav aria-label=${t("common.nav_resources", { defaultValue: "Resources" }) !== "common.nav_resources" ? t("common.nav_resources") : "Resources"}>${list.filter(v => v.label.toLowerCase().includes(needle)).map(v => html`<div class="context-resource-row">
        <a href=${v.route} @click=${(e: MouseEvent) => { if (e.ctrlKey || e.metaKey) return; e.preventDefault(); navigateWorkspace(v.route, e.shiftKey); }}
          class=${activeTile(workspace!).route === v.route ? "active" : ""}><span>${v.label}</span><small>${v.sub}</small></a>
        <button class="workspace-icon-button" title=${t("workspace.open_in_split")} aria-label=${t("workspace.open_resource_in_split", { label: v.label })} @click=${() => navigateWorkspace(v.route, true)}>${layoutIcon(2)}</button>
      </div>`)}</nav>
      ${!resourcesLoading && !resourcesError && list.filter(v => v.label.toLowerCase().includes(needle)).length === 0 ? html`<p class="context-hint">${t("workspace.no_matches")}</p>` : ""}` :
      html`<nav aria-label=${t("workspace.quick_access")}>${VIEWS.filter(v => v.group === routeMeta(activeTile(workspace!).route).group).map(v => html`<a href=${v.route} @click=${(e: Event) => { e.preventDefault(); navigateWorkspace(v.route); }}>${icon(v.route)} ${viewLabel(v.route, v.label)}</a>`)}</nav>`}
  </div>`;
}

function contextTemplate(): TemplateResult {
  const s = workspace!;
  return html`<header class="context-header"><div><strong>${t("workspace.title")}</strong><small>${t(s.tiles.length === 1 ? "workspace.panels_open_one" : "workspace.panels_open_other", { count: s.tiles.length })}</small></div>
    <button class="workspace-icon-button" aria-label=${t("workspace.hide_context")} title=${t("workspace.hide_context")} @click=${() => { s.contextCollapsed = true; changed(); }}>${icons.chevronsLeft()}</button></header>
    <div class="context-panel-list" role="group" aria-label=${t("workspace.open_panels")}>${s.tiles.map((t, i) => html`<button class=${t.id === s.focused ? "active" : ""}
      aria-pressed=${t.id === s.focused} @click=${() => { s.maximized = null; focus(t.id); paint(); }}>
      <span class="context-panel-number">${i + 1}</span>${icon(t.route)}<span>${viewLabel(t.route, routeMeta(t.route).label)}</span></button>`)}</div>
    ${contextualResources()}
    <footer class="context-footer"><span>${t("workspace.hint_click")}</span><span>${t("workspace.hint_shift")}</span><span>${t("workspace.hint_drag")}</span></footer>`;
}
function paintContext(): void {
  const root = document.getElementById("workspace-context");
  if (!root || !workspace) return;
  render(contextTemplate(), root);
}
function paint(): void {
  const root = document.getElementById("workspace");
  if (!root || !workspace) return;
  const s = workspace;
  root.classList.toggle("context-collapsed", s.contextCollapsed);
  render(html`<aside id="workspace-context" class="workspace-context" aria-label="Workspace context" ?hidden=${s.contextCollapsed}></aside>
    <div class="workspace-desk">
      <header class="workspace-toolbar">
        <div class="workspace-toolbar-title">${s.contextCollapsed ? html`<button class="workspace-icon-button" aria-label=${t("workspace.show_context")} title=${t("workspace.show_context")}
          @click=${() => { s.contextCollapsed = false; changed(); }}>${icons.chevronsRight()}</button>` : ""}<strong>${groupLabel(routeMeta(activeTile(s).route).group)}</strong><span class="workspace-desktop-hint">${t("workspace.desktop_hint")}</span></div>
        ${isFullAdmin() ? html`
        <div class="workspace-toolbar-controls">
          <div class="workspace-layouts" role="group" aria-label=${t("workspace.panel_layout")}>${(["single", "split", "triple", "grid"] as Layout[]).map((mode, i) => html`
            <button class=${s.layout === mode ? "active" : ""} aria-pressed=${s.layout === mode} aria-label=${t(i === 0 ? "workspace.layout_panels_one" : "workspace.layout_panels_other", { count: i + 1 })}
              title=${layoutTitle(i)}
              @click=${() => { changeLayout(s, mode); changed(); }}>${layoutIcon(i + 1)}<span>${i + 1}</span></button>`)}</div>
          <div class="workspace-presets"><button class="workspace-preset-trigger" aria-haspopup="menu" aria-expanded=${menu === "presets"} @click=${() => toggleMenu("presets")}>${t("workspace.layouts_trigger")} ${icons.caretDown()}</button>
            ${menu === "presets" ? html`<div class="workspace-menu" role="menu" aria-label=${t("workspace.presets_label")}>${PRESETS.map(p => html`<button role="menuitem" @click=${() => { applyPreset(s, p); menu = null; changed(); }}>${layoutIcon(p.routes.length)} ${presetLabel(p.label)}</button>`)}</div>` : ""}</div>
        </div>` : ""}
      </header>
      <div class="workspace-mobile-panels" role="group" aria-label=${t("workspace.switch_panel")}>${s.tiles.map((t, i) => html`<button class=${t.id === s.focused ? "active" : ""} aria-pressed=${t.id === s.focused}
        @click=${() => { s.maximized = null; focus(t.id); paint(); }}>${i + 1}. ${viewLabel(t.route, routeMeta(t.route).label)}</button>`)}</div>
      <div class="workspace-grid layout-${s.layout} ${s.maximized ? "is-maximized" : ""}" style=${`--workspace-column:${s.column}%;--workspace-row:${s.row}%`}
        aria-label=${t("workspace.tiling_workspace")}>
        ${repeat(s.tiles, t => t.id, tileTemplate)}
        ${s.tiles.length > 1 && !s.maximized ? divider("column") : ""}
        ${s.tiles.length > 2 && !s.maximized ? divider("row") : ""}
      </div>
      <footer class="workspace-status"><span>${t("workspace.panel_active", { index: s.tiles.findIndex(t => t.id === s.focused) + 1 })}</span><span>${s.maximized ? t("workspace.status_maximized") : t("workspace.status_saved")}</span></footer>
    </div>`, root);
  for (const t of s.tiles) {
    const panel = root.querySelector<HTMLElement>(`[data-panel="${t.id}"]`);
    panel?.classList.toggle("mobile-active", t.id === s.focused);
  }
  reconcileNativePanels(root, s.tiles);
  paintContext();
}
async function loadResources(): Promise<void> {
  if (resourcesLoading) return;
  if (!isFullAdmin()) {
    // Non-admin keys cannot read combos, providers or keys.
    resourcesLoading = false;
    resourcesError = false;
    return;
  }
  resourcesLoading = true;
  resourcesError = false;
  paintContext();
  try {
    const [combos, providers, keys] = await Promise.all([api("/combos"), api("/providers"), api("/keys")]);
    if (!Array.isArray(combos) || !Array.isArray(providers) || !Array.isArray(keys)) throw new Error("Invalid resource response");
    state.combos = combos as typeof state.combos;
    state.providers = providers as typeof state.providers;
    state.apiKeys = keys as typeof state.apiKeys;
  } catch { resourcesError = true; }
  finally { resourcesLoading = false; paintContext(); }
}
export function mountWorkspace(route: string): void {
  if (!workspace) {
    if (!isFullAdmin()) {
      // Force clean single-panel workspace strictly containing analytics
      workspace = {
        version: 1,
        layout: "single",
        tiles: [{ id: "tile-1", route: "#/analytics" }],
        focused: "tile-1",
        maximized: null,
        column: 50,
        row: 50,
        contextCollapsed: true,
      };
    } else {
      let raw: string | null = null;
      try { raw = localStorage.getItem(WORKSPACE_KEY); } catch { /* no persistence */ }
      workspace = restoreWorkspace(raw, route);
      if (safeRoute(route) && route !== "#/") {
        const existing = workspace.tiles.find(tile => routeMeta(tile.route).route === routeMeta(route).route);
        if (existing) workspace.focused = existing.id;
        activeTile(workspace).route = route;
      }
    }
  } else if (!isFullAdmin()) {
    workspace.tiles = [{ id: "tile-1", route: "#/analytics" }];
    workspace.focused = "tile-1";
    workspace.layout = "single";
    workspace.contextCollapsed = true;
  } else if (safeRoute(route) && activeTile(workspace).route !== route) {
    navigateWorkspace(route);
  }
  paint();
  updateHash();
  requestUpdate();
  if (initialized) return;
  initialized = true;
  document.addEventListener("click", (e: MouseEvent) => {
    const target = e.target;
    if (!(target instanceof Element)) return;
    const link = target.closest<HTMLAnchorElement>(".sidebar a[data-nav], .tile-content a[href^=\"#/\"]");
    if (link && !e.defaultPrevented && e.button === 0 && !e.ctrlKey && !e.metaKey && !e.altKey && safeRoute(link.hash)) {
      e.preventDefault();
      navigateWorkspace(link.hash, e.shiftKey);
      document.body.classList.remove("mobile-nav-open");
    } else if (menu && !target.closest(".tile-view-picker, .workspace-presets")) { menu = null; paint(); }
  });
  document.addEventListener("openproxy:mutated", () => { void loadResources(); requestUpdate(); });
  document.addEventListener("keydown", (e: KeyboardEvent) => {
    if (e.key === "Escape" && !document.querySelector(".modal-bg")) { menu = null; if (workspace) workspace.maximized = null; changed(); return; }
    const target = e.target as HTMLElement | null;
    const popup = target?.closest<HTMLElement>(".workspace-menu");
    if (popup && ["ArrowDown", "ArrowUp", "Home", "End"].includes(e.key)) {
      const items = [...popup.querySelectorAll<HTMLButtonElement>("button")];
      const current = items.indexOf(target as HTMLButtonElement);
      const next = e.key === "Home" ? 0 : e.key === "End" ? items.length - 1 :
        (current + (e.key === "ArrowDown" ? 1 : -1) + items.length) % items.length;
      e.preventDefault(); items[next]?.focus();
    }
  });
  window.addEventListener("storage", e => {
    if (e.key === "openproxy_admin_token" && !e.newValue) window.location.assign("/admin/#/login");
    if (e.key === "openproxy-theme") bootstrapTheme();
  });
  void loadResources();
}

/** Exact-origin, source-checked counterpart in the host, no token in messages or URLs. */
export function installPanelBridge(): void {
  document.body.classList.add("embedded-panel");
  const notify = (type: string): void => window.parent.postMessage({ type, route: location.hash }, location.origin);
  document.addEventListener("pointerdown", () => notify("openproxy:panel-focus"), true);
  window.addEventListener("focus", () => notify("openproxy:panel-focus"));
  window.addEventListener("hashchange", () => {
    if (location.hash.startsWith("#/login")) notify("openproxy:panel-logout");
    else notify("openproxy:panel-route");
  });
  window.addEventListener("storage", e => {
    if (e.key === "openproxy-theme") bootstrapTheme();
    if (e.key === "openproxy_admin_token") {
      if (!e.newValue) { clearToken(); notify("openproxy:panel-logout"); }
      else window.location.reload();
    }
  });
  if (!isLoggedIn()) notify("openproxy:panel-logout");
  notify("openproxy:panel-route");
}
