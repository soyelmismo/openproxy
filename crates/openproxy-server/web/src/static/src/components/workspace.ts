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
import { beginTileDrag, beginViewDrag, beginResizeDrag, type DragHooks, type GripCorner } from "./workspace-drag.js";
import {
  WORKSPACE_KEY, VIEWS, GRID, MAX_PANELS, activeTile, closeTile,
  openSplit, restoreWorkspace, routeMeta, safeRoute, clampRect, overlaps, planDrop, swapTiles, assignTileRoute, placeTile, setTileRect, spawnTile,
  type DropPlan, type Rect, type Tile, type WorkspaceState,
} from "../state/workspace.js";

let workspace: WorkspaceState | null = null;
let initialized = false;
let menu: string | null = null;

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
  // A maximized tile keeps its decision: every navigation loads INTO it (the
  // background tiles are hidden, not gone). A family owned elsewhere trades
  // routes with the hero instead of stealing focus back to the arrange.
  if (workspace.maximized && !split) {
    const hero = activeTile(workspace);
    const owner = workspace.tiles.find(tile => routeMeta(tile.route).route === routeMeta(route).route);
    if (owner && owner.id !== hero.id) owner.route = hero.route;
    hero.route = route;
    workspace.focused = hero.id;
    menu = null;
    changed();
    return;
  }
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



/* Keep in sync with workspace.css: the grid's padding and its 6x6 gap. */
const WS_PAD = 6;
const WS_GAP = 8;
let ghostEl: HTMLElement | null = null;
function gridMetrics(): { colW: number; rowH: number } | null {
  const grid = document.querySelector<HTMLElement>(".workspace-grid");
  if (!grid || grid.clientWidth <= 0 || grid.clientHeight <= 0) return null;
  return {
    colW: (grid.clientWidth - WS_PAD * 2 - WS_GAP * (GRID.cols - 1)) / GRID.cols,
    rowH: (grid.clientHeight - WS_PAD * 2 - WS_GAP * (GRID.rows - 1)) / GRID.rows,
  };
}
/** Tile floors in real pixels: a panel below either floor is unusable, so
 *  ghosts and commits snap up to the smallest span that clears both floors. */
const MIN_TILE_W = 220;
const MIN_TILE_H = 120;
function minSpans(): { w: number; h: number } {
  const m = gridMetrics();
  if (!m) return { w: 2, h: 2 };
  return {
    w: Math.min(GRID.cols, Math.max(1, Math.ceil((MIN_TILE_W + WS_GAP) / (m.colW + WS_GAP)))),
    h: Math.min(GRID.rows, Math.max(1, Math.ceil((MIN_TILE_H + WS_GAP) / (m.rowH + WS_GAP)))),
  };
}
/** A plan is only show-able when every panel it leaves on canvas clears the
 *  real pixel floors: the dropped tile itself and every carved neighbour. A
 *  1-column remnant on a small canvas is unusable, so it reads as "does not
 *  fit" instead of forcing a squeeze. */
function planUsable(plan: DropPlan | null, min: { w: number; h: number }, withRect = false): plan is DropPlan {
  if (!plan) return false;
  if (withRect && (plan.rect.w < min.w || plan.rect.h < min.h)) return false;
  for (const r of plan.carves.values()) {
    if (r.w < min.w || r.h < min.h) return false;
  }
  return true;
}
function cellFromPoint(x: number, y: number): { cx: number; cy: number } | null {
  const grid = document.querySelector<HTMLElement>(".workspace-grid");
  const m = gridMetrics();
  if (!grid || !m) return null;
  const box = grid.getBoundingClientRect();
  if (x < box.left || x > box.right || y < box.top || y > box.bottom) return null;
  const cx = Math.floor((x - box.left - WS_PAD) / (m.colW + WS_GAP));
  const cy = Math.floor((y - box.top - WS_PAD) / (m.rowH + WS_GAP));
  return { cx: Math.min(GRID.cols - 1, Math.max(0, cx)), cy: Math.min(GRID.rows - 1, Math.max(0, cy)) };
}
function tileAtPoint(x: number, y: number): Tile | null {
  if (!workspace) return null;
  const id = document.elementFromPoint(x, y)?.closest<HTMLElement>(".workspace-tile")?.dataset["panel"];
  return id ? workspace.tiles.find(tile => tile.id === id) ?? null : null;
}
/** Is the pointer on the seam between two panels? The seam is the gap band at
 *  a cell boundary: dropping a view there means "insert between these two",
 *  not "load into this one". Returns the pair when both sides hold a tile. */
function seamBetween(x: number, y: number): { a: Tile; b: Tile } | null {
  if (!workspace) return null;
  const cell = cellFromPoint(x, y);
  const grid = document.querySelector<HTMLElement>(".workspace-grid");
  const m = gridMetrics();
  if (!cell || !grid || !m) return null;
  const box = grid.getBoundingClientRect();
  const left = box.left + WS_PAD + cell.cx * (m.colW + WS_GAP);
  const top = box.top + WS_PAD + cell.cy * (m.rowH + WS_GAP);
  const at = (cx: number, cy: number): Tile | null => {
    if (cx < 0 || cy < 0 || cx >= GRID.cols || cy >= GRID.rows) return null;
    return workspace!.tiles.find(t =>
      t.rect.x <= cx && cx < t.rect.x + t.rect.w && t.rect.y <= cy && cy < t.rect.y + t.rect.h) ?? null;
  };
  const pairs: [Tile | null, Tile | null][] = [];
  if (x - left <= 10) pairs.push([at(cell.cx - 1, cell.cy), at(cell.cx, cell.cy)]);
  if (left + m.colW - x <= 10) pairs.push([at(cell.cx, cell.cy), at(cell.cx + 1, cell.cy)]);
  if (y - top <= 10) pairs.push([at(cell.cx, cell.cy - 1), at(cell.cx, cell.cy)]);
  if (top + m.rowH - y <= 10) pairs.push([at(cell.cx, cell.cy), at(cell.cx, cell.cy + 1)]);
  for (const [a, b] of pairs) if (a && b && a.id !== b.id) return { a, b };
  return null;
}
/** Candidate placements across a seam. A drop that straddles a seam can be
 *  slid along it: on an asymmetric pair only the wide neighbour can give, so
 *  the winning rect is the one whose plan carves a single side and keeps the
 *  most area. Returns null when no slide clears the pixel floors. */
function seamPlan(seam: { a: Tile; b: Tile }, size: { w: number; h: number }, cell: { cx: number; cy: number }, draggedId = "spawn-probe"): Rect | null {
  if (!workspace) return null;
  const min = minSpans();
  const vertical = seam.a.rect.x !== seam.b.rect.x;
  const bx = vertical ? Math.max(seam.a.rect.x, seam.b.rect.x) : 0;
  const by = vertical ? 0 : Math.max(seam.a.rect.y, seam.b.rect.y);
  const seen = new Set<string>();
  let best: { rect: Rect; score: number } | null = null;
  for (let i = 0; i <= size.w + size.h; i++) {
    const start = vertical ? bx - i : by - i;
    const rect = clampRect(vertical
      ? { x: start, y: cell.cy - Math.floor((size.h - 1) / 2), w: size.w, h: size.h }
      : { x: cell.cx - Math.floor((size.w - 1) / 2), y: start, w: size.w, h: size.h });
    const key = `${rect.x},${rect.y},${rect.w},${rect.h}`;
    if (seen.has(key)) continue;
    seen.add(key);
    // noMoves: the commit carves in place, so the preview must score the same
    // carve-only plans — otherwise a wider canvas can elect a relocation the
    // drop will never take. withRect: the committed tile must clear the px
    // floors too — a seam strip narrower than the floor is never usable.
    const plan = planDrop(workspace.tiles, draggedId, rect, { noMoves: true });
    if (!planUsable(plan, min, true)) continue;
    // Fair donation: the seam insert reads as "split here" when the two sides
    // keep balanced remainders; a lopsided pair slides so only the roomy side
    // gives. Then the largest surviving panel; then closest to the pointer.
    const ra = plan.carves.get(seam.a.id) ?? seam.a.rect;
    const rb = plan.carves.get(seam.b.id) ?? seam.b.rect;
    const asym = Math.abs(ra.w * ra.h - rb.w * rb.h);
    const kept = Math.min(...[...plan.carves.values()].map(r => r.w * r.h), 999);
    const dist = vertical ? Math.abs(rect.x - cell.cx) : Math.abs(rect.y - cell.cy);
    const score = -asym * 100 + kept * 10 - dist;
    if (!best || score > best.score) best = { rect: plan.rect, score };
  }
  return best?.rect ?? null;
}
/** The live drop preview: a translucent rect gliding across the canvas, so the
 *  landing spot is animated before anything commits. */
function showGhost(rect: Rect, mode: "move" | "swap" | "load" | "free" | "full"): void {
  const grid = document.querySelector<HTMLElement>(".workspace-grid");
  if (!grid) return;
  if (!ghostEl || !ghostEl.isConnected || ghostEl.parentElement !== grid) {
    ghostEl?.remove();
    ghostEl = document.createElement("div");
    ghostEl.className = "workspace-drop-ghost";
    ghostEl.setAttribute("aria-hidden", "true");
    ghostEl.style.transition = "none";
    grid.appendChild(ghostEl);
    requestAnimationFrame(() => { if (ghostEl) ghostEl.style.transition = ""; });
  }
  const m = gridMetrics();
  if (!m) return;
  ghostEl.classList.remove("mode-move", "mode-swap", "mode-load", "mode-free", "mode-full");
  ghostEl.classList.add(`mode-${mode}`);
  ghostEl.style.left = `${Math.round(WS_PAD + rect.x * (m.colW + WS_GAP))}px`;
  ghostEl.style.top = `${Math.round(WS_PAD + rect.y * (m.rowH + WS_GAP))}px`;
  ghostEl.style.width = `${Math.round(rect.w * m.colW + (rect.w - 1) * WS_GAP)}px`;
  ghostEl.style.height = `${Math.round(rect.h * m.rowH + (rect.h - 1) * WS_GAP)}px`;
}
function hideGhost(): void { ghostEl?.remove(); ghostEl = null; }
/** FLIP: commit a state change, repaint, then animate every tile that moved
 *  from its old box to the new one. Cheap — two reads, one layout pass. */
function flip(mutate: () => void): void {
  const grid = document.querySelector<HTMLElement>(".workspace-grid");
  if (!grid) { mutate(); changed(); return; }
  const before = new Map<string, { left: number; top: number }>();
  for (const el of grid.querySelectorAll<HTMLElement>(".workspace-tile")) {
    const id = el.dataset["panel"];
    if (id) before.set(id, el.getBoundingClientRect());
  }
  mutate();
  changed();
  if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) return;
  for (const el of grid.querySelectorAll<HTMLElement>(".workspace-tile")) {
    const id = el.dataset["panel"];
    const start = id ? before.get(id) : undefined;
    if (!start) continue;
    const end = el.getBoundingClientRect();
    const dx = Math.round(start.left - end.left);
    const dy = Math.round(start.top - end.top);
    if (Math.abs(dx) < 1 && Math.abs(dy) < 1) continue;
    el.animate([{ transform: `translate(${dx}px, ${dy}px)` }, { transform: "translate(0, 0)" }],
      { duration: 220, easing: "cubic-bezier(0.2, 0.8, 0.2, 1)" });
  }
}
/** Ghost + drop semantics while a panel header is dragged. The ghost keeps the
 *  dragged tile's size, centers on the hovered cell; one tile underneath swaps,
 *  empty space relocates. */
/** The tile whose CENTRAL region contains the pointer, for direct rect swap.
 *  Dropping on the middle half of a neighbour means "take its place": the swap
 *  fires regardless of how many further tiles the dragged rect would brush. */
function swapTargetAt(x: number, y: number, draggedId: string): Tile | null {
  if (!workspace) return null;
  const el = document.elementFromPoint(x, y)?.closest<HTMLElement>(".workspace-tile[data-panel]");
  const id = el?.dataset["panel"];
  if (!id || id === draggedId) return null;
  const tile = workspace.tiles.find(t => t.id === id);
  if (!tile || !el) return null;
  const b = el.getBoundingClientRect();
  if (b.width <= 0 || b.height <= 0) return null;
  const rx = (x - b.left) / b.width, ry = (y - b.top) / b.height;
  return rx >= 0.25 && rx <= 0.75 && ry >= 0.25 && ry <= 0.75 ? tile : null;
}
function tileDragHooks(dragged: Tile): DragHooks {
  const pending = (x: number, y: number): { rect: Rect; swapWith: Tile | null; fits: boolean } | null => {
    const cell = cellFromPoint(x, y);
    if (!cell || !workspace) return null;
    const min = minSpans();
    const centerSwap = swapTargetAt(x, y, dragged.id);
    if (centerSwap) return { rect: centerSwap.rect, swapWith: centerSwap, fits: true };
    const size = { w: Math.max(dragged.rect.w, min.w), h: Math.max(dragged.rect.h, min.h) };
    const rect = clampRect({
      x: cell.cx - Math.floor((size.w - 1) / 2),
      y: cell.cy - Math.floor((size.h - 1) / 2),
      w: size.w, h: size.h,
    });
    const hits = workspace.tiles.filter(tile => tile.id !== dragged.id && overlaps(tile.rect, rect));
    const swapWith = hits.length === 1 ? hits[0]! : null;
    if (!swapWith) {
      const plan = planDrop(workspace.tiles, dragged.id, rect);
      if (planUsable(plan, minSpans())) return { rect: plan!.rect, swapWith: null, fits: true };
      // Asymmetric neighbours: slide the panel along the seam so only the side
      // with room gives up space.
      const seam = seamBetween(x, y);
      const slid = seam ? seamPlan(seam, size, cell, dragged.id) : null;
      if (slid) return { rect: slid, swapWith: null, fits: true };
      return { rect, swapWith: null, fits: false };
    }
    return { rect, swapWith, fits: true };
  };
  return {
    onFrame: (x, y, _hover, payload) => {
      const hit = pending(x, y);
      if (!hit) { hideGhost(); return payload.kind === "tile" ? t("workspace.drag_panel") : payload.label; }
      showGhost(hit.rect, hit.swapWith ? "swap" : hit.fits ? "move" : "full");
      if (hit.swapWith) return t("workspace.drop_swap", { index: (workspace?.tiles.indexOf(hit.swapWith) ?? 0) + 1 });
      return hit.fits ? t("workspace.drop_free") : t("workspace.drop_bad");
    },
    onDrop: (x, y) => {
      const hit = pending(x, y);
      hideGhost();
      if (!hit || !hit.fits || !workspace) return;
      flip(() => {
        if (hit.swapWith) swapTiles(workspace!, dragged.id, hit.swapWith.id);
        else placeTile(workspace!, dragged.id, hit.rect);
      });
    },
  };
}
/** Ghost + drop semantics while a sidebar view is dragged: load into the panel
 *  under the pointer, or spawn a new panel on empty canvas if there is room. */
function viewDragHooks(route: string): DragHooks {
  const pending = (x: number, y: number): { rect: Rect; mode: "load" | "free" | "full"; target: Tile | null; carve?: boolean; heroTo?: { id: string; rect: Rect } } | null => {
    const target = tileAtPoint(x, y);
    if (!workspace) return null;
    const cell = cellFromPoint(x, y);
    const seam = seamBetween(x, y);
    const min = minSpans();
    // Maximized hero: it VISUALLY covers the whole canvas, so the split runs
    // on the full grid regardless of the hero's stored rect — the drop takes
    // the pointer's half and the hero is re-rect'd to the other half, which
    // also unmaximizes (spawnTile clears the flag). This is how a fresh
    // maximized dashboard grows its arrange.
    if (workspace.maximized) {
      const hero = workspace.tiles.find(t2 => t2.id === workspace!.maximized);
      const heroBox = hero ? document.querySelector<HTMLElement>(`.workspace-tile[data-panel="${hero.id}"]`) : null;
      if (!hero || !heroBox) return null;
      const b = heroBox.getBoundingClientRect();
      if (x < b.left || x > b.right || y < b.top || y > b.bottom) return null;
      if (min.w > 3 || min.h > 3) return { rect: hero.rect, mode: "load", target: hero };
      const rx = (x - b.left) / b.width, ry = (y - b.top) / b.height;
      const vertical = Math.min(rx, 1 - rx) <= Math.min(ry, 1 - ry);
      const left = rx < 0.5, top = ry < 0.5;
      const drop = vertical ? { x: left ? 0 : 3, y: 0, w: 3, h: GRID.rows } : { x: 0, y: top ? 0 : 3, w: GRID.cols, h: 3 };
      const other = vertical ? { x: left ? 3 : 0, y: 0, w: 3, h: GRID.rows } : { x: 0, y: top ? 3 : 0, w: GRID.cols, h: 3 };
      return { rect: drop, mode: "free", target: null, carve: true, heroTo: { id: hero.id, rect: other } };
    }
    // The canvas is full: no sidebar drop can place anything, on any surface.
    if (workspace.tiles.length >= MAX_PANELS) {
      if (target) return { rect: target.rect, mode: "full", target: null };
      if (!cell) return null;
      return { rect: clampRect({ x: cell.cx - 1, y: cell.cy - 1, w: 3, h: 3 }), mode: "full", target: null };
    }
    if (target && cell && !seam) {
      // Dragging a view means PLACING a new panel, never just retargeting: the
      // tile under the pointer splits at the pointer's nearest edge (that axis
      // first, then the other one when the px floors refuse the halves). Load
      // remains only as the fallback for a tile too small to give a half, and
      // the corner picker still retargets a maximized hero.
      const t0 = target.rect;
      const dx = Math.min(cell.cx - t0.x, t0.x + t0.w - 1 - cell.cx);
      const dy = Math.min(cell.cy - t0.y, t0.y + t0.h - 1 - cell.cy);
      const vsplit = { x: cell.cx < t0.x + t0.w / 2 ? t0.x : t0.x + (t0.w - Math.floor(t0.w / 2)), y: t0.y, w: Math.floor(t0.w / 2), h: t0.h };
      const hsplit = { x: t0.x, y: cell.cy < t0.y + t0.h / 2 ? t0.y : t0.y + (t0.h - Math.floor(t0.h / 2)), w: t0.w, h: Math.floor(t0.h / 2) };
      for (const cand of dx <= dy ? [vsplit, hsplit] : [hsplit, vsplit]) {
        // noMoves: the split must carve THIS tile in place; a relocate plan
        // would draw the split rect while silently teleporting the tile away.
        const plan = planDrop(workspace.tiles, "spawn-probe", clampRect(cand), { noMoves: true });
        if (planUsable(plan, min, true)) return { rect: plan!.rect, mode: "free", target: null, carve: true };
      }
      return { rect: target.rect, mode: "load", target };
    }
    if (!cell) return target ? { rect: target.rect, mode: "load", target } : null;
    if (seam) {
      // Slide the strip across the seam: asymmetric pairs let only the wide
      // side give, symmetric ones split the band evenly. The insert is at
      // least two columns wide — one column reads as a sliver, not as
      // "insert between" — and grows when the px floors demand it.
      const rect = seamPlan(seam, { w: Math.max(min.w, 2), h: min.h }, cell);
      if (rect) return { rect, mode: "free", target: null, carve: true };
      return target ? { rect: target.rect, mode: "load", target } : { rect: clampRect({ x: cell.cx - 1, y: cell.cy - 1, w: 3, h: 3 }), mode: "full", target: null };
    }
    const rect = clampRect({ x: cell.cx - 1, y: cell.cy - 1, w: 3, h: 3 });
    const plan = planDrop(workspace.tiles, "spawn-probe", rect);
    if (planUsable(plan, min, true)) return { rect: plan.rect, mode: "free", target: null };
    // The strip does not clear the floors: fall back to loading the panel.
    if (target) return { rect: target.rect, mode: "load", target };
    return { rect, mode: "full", target: null };
  };
  return {
    onFrame: (x, y) => {
      const hit = pending(x, y);
      if (!hit) { hideGhost(); return ""; }
      showGhost(hit.rect, hit.mode);
      if (hit.mode === "load") return t("workspace.drop_load", { index: (workspace?.tiles.indexOf(hit.target!) ?? 0) + 1 });
      return hit.mode === "free" ? t("workspace.drop_free") : t("workspace.drop_full");
    },
    onDrop: (x, y) => {
      const hit = pending(x, y);
      hideGhost();
      if (!hit || !workspace) return;
      flip(() => {
        if (hit.heroTo) {
          const hero = workspace!.tiles.find(t2 => t2.id === hit.heroTo!.id);
          if (hero) hero.rect = hit.heroTo.rect;
          spawnTile(workspace!, route, hit.rect, { noMoves: true });
          return;
        }
        if (hit.target) assignTileRoute(workspace!, hit.target.id, route);
        else if (hit.mode === "free") spawnTile(workspace!, route, hit.rect, hit.carve ? { noMoves: true } : undefined);
      });
    },
  };
}
/** Ghost + commit semantics while a grip (corner or edge) resizes a panel.
 *  The opposite edge stays anchored, the moving edge never crosses it, and a
 *  neighbor stops the edge: a resize can never displace or reorder anything. */
function resizeDragHooks(id: string, corner: GripCorner): DragHooks {
  const start = workspace?.tiles.find(tile => tile.id === id)?.rect;
  if (!start) return { onDrop: () => {} };
  const movesX = corner !== "n" && corner !== "s";
  const movesY = corner !== "e" && corner !== "w";
  // The anchor is the edge that STAYS: for w/n grips it is the opposite edge,
  // for e/s grips it is the fixed left/top edge. Getting this backwards was
  // teleporting the tile on every resize.
  const anchorX = corner.includes("w") ? start.x + start.w - 1 : start.x;
  const anchorY = corner.includes("n") ? start.y + start.h - 1 : start.y;
  const growsRight = !corner.includes("w");
  const growsDown = !corner.includes("n");
  /** How far the moving edge may travel on one axis before touching a
   *  neighbor that overlaps the perpendicular range. */
  const clipLimit = (perp: { x: number; y: number; w: number; h: number }, axis: "x" | "y"): number => {
    // The bound starts at the CANVAS EDGE THE EDGE MOVES TOWARD: far edge for
    // right/down growth, zero for left/up growth. Starting both at the far
    // edge sent left/up grips (nw, w, n, sw) teleporting across the canvas.
    let limit = (axis === "x" ? growsRight : growsDown)
      ? (axis === "x" ? GRID.cols - 1 : GRID.rows - 1)
      : 0;
    for (const tile of workspace?.tiles ?? []) {
      if (tile.id === id) continue;
      const r = tile.rect;
      const across = axis === "x"
        ? r.y < perp.y + perp.h && perp.y < r.y + r.h
        : r.x < perp.x + perp.w && perp.x < r.x + r.w;
      if (!across) continue;
      if (axis === "x") {
        if (growsRight && r.x > anchorX) limit = Math.min(limit, r.x - 1);
        if (!growsRight && r.x + r.w - 1 < anchorX) limit = Math.max(limit, r.x + r.w);
      } else {
        if (growsDown && r.y > anchorY) limit = Math.min(limit, r.y - 1);
        if (!growsDown && r.y + r.h - 1 < anchorY) limit = Math.max(limit, r.y + r.h);
      }
    }
    return limit;
  };
  const pending = (x: number, y: number): Rect | null => {
    const cell = cellFromPoint(x, y);
    if (!cell) return null;
    const min = minSpans();
    let nx = start.x, nw = start.w, ny = start.y, nh = start.h;
    if (movesY) {
      const limit = clipLimit({ x: start.x, y: start.y, w: start.w, h: start.h }, "y");
      const edge = growsDown ? Math.min(Math.max(cell.cy, anchorY), limit) : Math.max(Math.min(cell.cy, anchorY), limit);
      const span = growsDown ? edge - anchorY + 1 : anchorY - edge + 1;
      nh = Math.max(1, Math.min(Math.max(span, min.h), Math.abs(limit - anchorY) + 1));
      ny = growsDown ? anchorY : anchorY - (nh - 1);
    }
    if (movesX) {
      const limit = clipLimit({ x: start.x, y: ny, w: start.w, h: nh }, "x");
      const edge = growsRight ? Math.min(Math.max(cell.cx, anchorX), limit) : Math.max(Math.min(cell.cx, anchorX), limit);
      const span = growsRight ? edge - anchorX + 1 : anchorX - edge + 1;
      nw = Math.max(1, Math.min(Math.max(span, min.w), Math.abs(limit - anchorX) + 1));
      nx = growsRight ? anchorX : anchorX - (nw - 1);
    }
    return clampRect({ x: nx, y: ny, w: nw, h: nh });
  };
  return {
    onFrame: (x, y) => {
      const rect = pending(x, y);
      if (!rect) return "";
      showGhost(rect, "move");
      return `${rect.w} \u00d7 ${rect.h}`;
    },
    onDrop: (x, y) => {
      const rect = pending(x, y);
      hideGhost();
      if (!rect || !workspace) return;
      flip(() => setTileRect(workspace!, id, rect));
    },
  };
}
/** Four corners to reshape freely, four edges to stretch one axis. */
const GRIPS = ["nw", "ne", "sw", "se", "n", "e", "s", "w"] as const;
function gripTemplate(tile: Tile, corner: GripCorner): TemplateResult {
  return html`<span class="tile-grip grip-${corner}" role="separator" aria-label=${t("workspace.resize_panel")}
    @pointerdown=${(e: PointerEvent) => {
      focus(tile.id);
      if (workspace?.maximized) return;
      beginResizeDrag(e, tile.id, corner, t("workspace.resize_panel"), { ...resizeDragHooks(tile.id, corner), onCancel: hideGhost });
    }}></span>`;
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
  const rect = tile.rect;
  return html`<section class="workspace-tile ${s.focused === tile.id ? "is-focused" : ""}"
    data-panel=${tile.id} data-index=${index} ?hidden=${hidden}
    style=${`grid-column:${rect.x + 1}/span ${rect.w};grid-row:${rect.y + 1}/span ${rect.h}`}
    aria-label=${t("workspace.panel_label", { index: index + 1, label })} @pointerdown=${() => focus(tile.id)} @focusin=${() => focus(tile.id)}>
    <header class="tile-header" data-drag-handle aria-roledescription=${t("workspace.drag_panel")}
      @pointerdown=${(e: PointerEvent) => {
        focus(tile.id);
        if (s.maximized || !isFullAdmin()) return;
        beginTileDrag(e, tile.id, t("workspace.drag_panel_label", { label }), { ...tileDragHooks(tile), onCancel: hideGhost });
      }}>
      <div class="tile-view-picker">
        <button class="tile-title" aria-haspopup="menu" aria-expanded=${menu === tile.id}
          @click=${() => toggleMenu(tile.id)}>${icon(tile.route)}<span>${label}</span>${icons.caretDown()}</button>
        ${menu === tile.id ? viewMenu(tile.id) : ""}
      </div>
      <div class="tile-actions">
        <span class="tile-position" title=${t("workspace.panel_number")}>${index + 1}</span>
        <button class="workspace-icon-button" aria-label=${s.maximized === tile.id ? t("workspace.restore_panel") : t("workspace.maximize_panel")}
          title=${s.maximized === tile.id ? t("workspace.restore_panel") : t("workspace.maximize_panel")}
          @click=${() => { s.focused = tile.id; s.maximized = s.maximized === tile.id ? null : tile.id; changed(); }}>${maximizeIcon(s.maximized === tile.id)}</button>
        ${s.tiles.length > 1 ? html`<button class="workspace-icon-button" aria-label=${t("workspace.close_panel")} title=${t("workspace.close_panel")}
          @click=${() => { closeTile(s, tile.id); changed(); }}>${icons.close()}</button>` : ""}
      </div>
    </header>
    ${s.maximized !== tile.id && isFullAdmin() ? (GRIPS as readonly GripCorner[]).map(corner => gripTemplate(tile, corner)) : ""}
    <div class="tile-content" data-tile=${tile.id}></div>
  </section>`;
}

/** Tile density tokens: recomputed per tile per resize, quantized to 0.5px
 *  steps so most drag frames write nothing. Mirrors the old cqw clamps. */
function writeTileTokens(el: HTMLElement): void {
  const w = el.clientWidth;
  if (w <= 0) return;
  // A wide, short quadrant needs compact rhythm too. Preserve type and control
  // sizes, and recover space from section gaps rather than shrinking content.
  const compact = el.clientHeight < 480;
  el.classList.toggle("is-short", compact);
  const q = (v: number): string => `${Math.round(v * 2) / 2}px`;
  const s = el.style;
  s.setProperty("--tile-pad", q(Math.min(compact ? 10 : 14, Math.max(8, w * 0.018))));
  s.setProperty("--space-1", q(Math.min(4, Math.max(2, w * 0.0035))));
  s.setProperty("--space-2", q(Math.min(8, Math.max(4, w * 0.0055))));
  s.setProperty("--space-3", q(Math.min(compact ? 8 : 12, Math.max(6, w * 0.008))));
  s.setProperty("--space-4", q(Math.min(compact ? 10 : 14, Math.max(8, w * 0.01))));
  s.setProperty("--space-5", compact ? "12px" : "16px");
  s.setProperty("--space-6", compact ? "16px" : "20px");
}

let tilesRaf = 0;
function scheduleTileTokens(): void {
  if (tilesRaf) return;
  tilesRaf = requestAnimationFrame(() => {
    tilesRaf = 0;
    for (const el of document.querySelectorAll<HTMLElement>(".tile-content")) writeTileTokens(el);
  });
}

function paint(): void {
  scheduleTileTokens();
  const root = document.getElementById("workspace");
  if (!root || !workspace) return;
  const s = workspace;
  // Hibernation signal for continuous view cycles (logs ticks, config polls):
  // while a tile is maximized, every other tile is backgrounded. The body
  // carries the hero id so a view can ask about its own tile cheaply.
  document.body.classList.toggle("workspace-maximized", s.maximized !== null);
  document.body.dataset["maximizedTile"] = s.maximized ?? "";
  render(html`<div class="workspace-desk layout-${s.layout}">
      <div class="workspace-mobile-panels" role="group" aria-label=${t("workspace.switch_panel")}>${s.tiles.map((t, i) => html`<button class=${t.id === s.focused ? "active" : ""} aria-pressed=${t.id === s.focused}
        @click=${() => { s.maximized = null; focus(t.id); paint(); }}>${i + 1}. ${viewLabel(t.route, routeMeta(t.route).label)}</button>`)}</div>
      <div class="workspace-grid layout-${s.layout} ${s.maximized ? "is-maximized" : ""}"
        aria-label=${t("workspace.tiling_workspace")}>
        ${repeat(s.tiles, t => t.id, tileTemplate)}
      </div>
      <footer class="workspace-status"><span>${t("workspace.panel_active", { index: s.tiles.findIndex(t => t.id === s.focused) + 1 })}</span><span>${s.maximized ? t("workspace.status_maximized") : t("workspace.status_saved")}</span></footer>
    </div>`, root);
  for (const t of s.tiles) {
    const panel = root.querySelector<HTMLElement>(`[data-panel="${t.id}"]`);
    panel?.classList.toggle("mobile-active", t.id === s.focused);
  }
  reconcileNativePanels(root, s.tiles);
  paintSeams(root);
}
async function loadResources(): Promise<void> {
  if (!isFullAdmin()) {
    // Non-admin keys cannot read combos, providers or keys.
    return;
  }
  try {
    const [combos, providers, keys] = await Promise.all([api("/combos"), api("/providers"), api("/keys")]);
    if (!Array.isArray(combos) || !Array.isArray(providers) || !Array.isArray(keys)) throw new Error("Invalid resource response");
    state.combos = combos as typeof state.combos;
    state.providers = providers as typeof state.providers;
    state.apiKeys = keys as typeof state.apiKeys;
  } catch { /* the stores keep their previous values; views show their own errors */ }
}
/** Tenant (non-admin) workspace route: pin the single tile to whichever of
 *  the tenant-allowed views the hash asked for. Admin-only hashes fall back
 *  to analytics — the router guard redirects those anyway. */
function tenantRouteFor(route: string): string {
  if (route === "#/models" || route.startsWith("#/models?")) return "#/models";
  if (route === "#/playground" || route.startsWith("#/playground?")) return "#/playground";
  return "#/analytics";
}

export function mountWorkspace(route: string): void {
  if (!workspace) {
    if (!isFullAdmin()) {
      // Force clean single-panel workspace strictly containing the tenant views
      workspace = {
        version: 1,
        layout: "single",
        tiles: [{ id: "tile-1", route: tenantRouteFor(route), rect: { x: 0, y: 0, w: 6, h: 6 } }],
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
    // Same tenant pinning for an already-initialized workspace: follow the
    // hash when it names a tenant-allowed view, otherwise reset to analytics.
    const tenantRoute = tenantRouteFor(route);
    workspace.tiles = [{ id: "tile-1", route: tenantRoute, rect: { x: 0, y: 0, w: 6, h: 6 } }];
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
  // Tile density tokens. The clamped cqw values moved out of CSS: resolving
  // container units per element per resize frame (every descendant that
  // touches a spacing token) made tile dragging cost ~75ms of layout per
  // frame. The workspace now writes plain pixel tokens once per tile per
  // resize, and the whole subtree inherits ordinary lengths.
  const gridEl = document.querySelector<HTMLElement>(".workspace-grid");
  if (gridEl) new ResizeObserver(scheduleTileTokens).observe(gridEl);
  // Drag a view from either sidebar onto a panel to load it there. Delegated
  // at document level because both sidebars render outside this component.
  document.addEventListener("pointerdown", (e: PointerEvent) => {
    if (!workspace || !isFullAdmin()) return;
    if (e.pointerType !== "mouse" || e.button !== 0) return;
    const target = e.target;
    if (!(target instanceof Element)) return;
    const link = target.closest<HTMLElement>(".sidebar nav a[data-nav], .context-resources nav a[href^='#/']");
    if (!link || link.closest(".context-resource-row .workspace-icon-button")) return;
    const route = link.getAttribute("href") || link.dataset["nav"] || "";
    if (!safeRoute(route)) return;
    beginViewDrag(e, link, route, viewLabel(route, routeMeta(route).label), { ...viewDragHooks(route), onCancel: hideGhost });
  });
  document.addEventListener("click", (e: MouseEvent) => {
    const target = e.target;
    if (!(target instanceof Element)) return;
    const link = target.closest<HTMLAnchorElement>(".sidebar a[data-nav], .tile-content a[href^=\"#/\"]");
    if (link && !e.defaultPrevented && e.button === 0 && !e.ctrlKey && !e.metaKey && !e.altKey && safeRoute(link.hash)) {
      e.preventDefault();
      navigateWorkspace(link.hash, e.shiftKey);
      document.body.classList.remove("mobile-nav-open");
    } else if (menu && !target.closest(".tile-view-picker")) { menu = null; paint(); }
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

/** Shared-boundary resize: for every pair of neighbours that share an edge
 *  segment, a thin handle lives ON the seam; dragging it moves the border so
 *  one panel grows while the other shrinks by the same amount. Nothing else
 *  moves: the handle clamps at the pixel floors and at any third tile. */
type Seam = { a: Tile; b: Tile; vertical: boolean; s0: number; s1: number };
function seamList(): Seam[] {
  if (!workspace || workspace.maximized) return [];
  const out: Seam[] = [];
  const tiles = workspace.tiles;
  for (let i = 0; i < tiles.length; i++) {
    for (let j = i + 1; j < tiles.length; j++) {
      const a = tiles[i]!, b = tiles[j]!;
      if (a.rect.x + a.rect.w === b.rect.x || b.rect.x + b.rect.w === a.rect.x) {
        const s0 = Math.max(a.rect.y, b.rect.y), s1 = Math.min(a.rect.y + a.rect.h, b.rect.y + b.rect.h);
        if (s1 > s0) { out.push({ a, b, vertical: true, s0, s1 }); continue; }
      }
      if (a.rect.y + a.rect.h === b.rect.y || b.rect.y + b.rect.h === a.rect.y) {
        const s0 = Math.max(a.rect.x, b.rect.x), s1 = Math.min(a.rect.x + a.rect.w, b.rect.x + b.rect.w);
        if (s1 > s0) out.push({ a, b, vertical: false, s0, s1 });
      }
    }
  }
  return out;
}
function paintSeams(root: HTMLElement): void {
  root.querySelectorAll(".workspace-seam").forEach(el => el.remove());
  const grid = root.querySelector<HTMLElement>(".workspace-grid");
  const m = gridMetrics();
  if (!grid || !m || !workspace || workspace.maximized) return;
  for (const seam of seamList()) {
    const vertical = seam.vertical;
    const leftT = vertical
      ? (seam.a.rect.x + seam.a.rect.w === seam.b.rect.x ? seam.a : seam.b)
      : (seam.a.rect.y + seam.a.rect.h === seam.b.rect.y ? seam.a : seam.b);
    const rightT = leftT === seam.a ? seam.b : seam.a;
    const el = document.createElement("div");
    el.className = "workspace-seam " + (vertical ? "seam-v" : "seam-h");
    el.dataset["a"] = leftT.id;
    el.dataset["b"] = rightT.id;
    el.tabIndex = 0;
    el.setAttribute("role", "separator");
    el.setAttribute("aria-orientation", vertical ? "vertical" : "horizontal");
    el.setAttribute("aria-label", `Resize ${routeMeta(leftT.route).label} and ${routeMeta(rightT.route).label} panels`);
    const borderCell = vertical ? rightT.rect.x : rightT.rect.y;
    const place = (d: number): void => {
      if (vertical) {
        el.style.left = `${Math.round(WS_PAD + (borderCell + d) * (m.colW + WS_GAP) - WS_GAP / 2 - 4)}px`;
        el.style.top = `${Math.round(WS_PAD + seam.s0 * (m.rowH + WS_GAP))}px`;
        el.style.width = "9px";
        el.style.height = `${Math.round(Math.max((seam.s1 - seam.s0) * m.rowH + (seam.s1 - seam.s0 - 1) * WS_GAP, 28))}px`;
      } else {
        el.style.top = `${Math.round(WS_PAD + (borderCell + d) * (m.rowH + WS_GAP) - WS_GAP / 2 - 4)}px`;
        el.style.left = `${Math.round(WS_PAD + seam.s0 * (m.colW + WS_GAP))}px`;
        el.style.height = "9px";
        el.style.width = `${Math.round(Math.max((seam.s1 - seam.s0) * m.colW + (seam.s1 - seam.s0 - 1) * WS_GAP, 28))}px`;
      }
    };
    place(0);
    el.addEventListener("pointerdown", (ev) => {
      ev.preventDefault();
      ev.stopPropagation();
      el.setPointerCapture(ev.pointerId);
      const start = vertical ? ev.clientX : ev.clientY;
      const aW0 = vertical ? leftT.rect.w : leftT.rect.h;
      const b0 = vertical ? rightT.rect.x : rightT.rect.y;
      const bW0 = vertical ? rightT.rect.w : rightT.rect.h;
      const min = minSpans();
      const step = vertical ? m.colW + WS_GAP : m.rowH + WS_GAP;
      let last = 0;
      const valid = (d: number): boolean => {
        if (d === 0) return true;
        const aW = aW0 + d, bW = bW0 - d;
        if (aW < min.w || bW < min.w) return false;
        const ra = vertical ? { ...leftT.rect, w: aW } : { ...leftT.rect, h: aW };
        const rb = vertical ? { ...rightT.rect, x: b0 + d, w: bW } : { ...rightT.rect, y: b0 + d, h: bW };
        for (const t of workspace!.tiles) {
          if (t.id === leftT.id || t.id === rightT.id) continue;
          if (overlaps(t.rect, ra) || overlaps(t.rect, rb)) return false;
        }
        return true;
      };
      const preview = (d: number): void => {
        const aEl = grid.querySelector<HTMLElement>(`.workspace-tile[data-panel="${leftT.id}"]`);
        const bEl = grid.querySelector<HTMLElement>(`.workspace-tile[data-panel="${rightT.id}"]`);
        if (aEl) aEl.style.gridArea = vertical
          ? `${leftT.rect.y + 1} / ${leftT.rect.x + 1} / span ${leftT.rect.h} / span ${aW0 + d}`
          : `${leftT.rect.y + 1} / ${leftT.rect.x + 1} / span ${aW0 + d} / span ${leftT.rect.w}`;
        if (bEl) bEl.style.gridArea = vertical
          ? `${rightT.rect.y + 1} / ${b0 + d + 1} / span ${rightT.rect.h} / span ${bW0 - d}`
          : `${b0 + d + 1} / ${rightT.rect.x + 1} / span ${bW0 - d} / span ${rightT.rect.w}`;
        place(d);
      };
      const onMove = (e: PointerEvent): void => {
        const want = Math.round(((vertical ? e.clientX : e.clientY) - start) / step);
        let d = want;
        while (d !== 0 && !valid(d)) d -= Math.sign(want) || 1;
        if (valid(d)) { last = d; preview(d); }
      };
      const finish = (): void => {
        el.removeEventListener("pointermove", onMove);
        el.removeEventListener("pointerup", finish);
        el.removeEventListener("pointercancel", finish);
        if (!workspace || last === 0) { paint(); return; }
        setTileRect(workspace, leftT.id, vertical
          ? { ...leftT.rect, w: aW0 + last }
          : { ...leftT.rect, h: aW0 + last });
        setTileRect(workspace, rightT.id, vertical
          ? { ...rightT.rect, x: b0 + last, w: bW0 - last }
          : { ...rightT.rect, y: b0 + last, h: bW0 - last });
        changed();
      };
      el.addEventListener("pointermove", onMove);
      el.addEventListener("pointerup", finish);
      el.addEventListener("pointercancel", finish);
    });
    el.addEventListener("keydown", (ev) => {
      const delta = ev.key === "ArrowRight" || ev.key === "ArrowDown" ? 1 : ev.key === "ArrowLeft" || ev.key === "ArrowUp" ? -1 : 0;
      if (!delta || !workspace) return;
      ev.preventDefault();
      const vertical = seam.vertical;
      const aW0 = vertical ? leftT.rect.w : leftT.rect.h;
      const b0 = vertical ? rightT.rect.x : rightT.rect.y;
      const bW0 = vertical ? rightT.rect.w : rightT.rect.h;
      const min = minSpans();
      const aW = aW0 + delta, bW = bW0 - delta;
      if (aW < min.w || bW < min.w) return;
      setTileRect(workspace, leftT.id, vertical ? { ...leftT.rect, w: aW } : { ...leftT.rect, h: aW });
      setTileRect(workspace, rightT.id, vertical ? { ...rightT.rect, x: b0 + delta, w: bW } : { ...rightT.rect, y: b0 + delta, h: bW });
      changed();
    });
    grid.appendChild(el);
  }
}
