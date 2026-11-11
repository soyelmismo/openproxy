// Pure workspace state. Routes, not server payloads or credentials, are persisted.
// Panels live on a free 6x6 canvas: every tile carries its own rect, so the
// user can drop, swap and resize anywhere — the named layouts are just presets.
export type Layout = "single" | "split" | "triple" | "grid" | "penta" | "hex";
export const LAYOUTS: readonly Layout[] = ["single", "split", "triple", "grid", "penta", "hex"];
export const GRID = { cols: 6, rows: 6 } as const;
export interface Rect { x: number; y: number; w: number; h: number }
export interface Tile { id: string; route: string; rect: Rect }
export interface WorkspaceState {
  version: 1;
  layout: Layout;
  tiles: Tile[];
  focused: string;
  maximized: string | null;
  column: number;
  row: number;
  contextCollapsed: boolean;
}
export const WORKSPACE_KEY = "openproxy:workspace:v1";
export const VIEWS = [
  { route: "#/", label: "Dashboard", group: "Overview" },
  { route: "#/providers", label: "Providers", group: "Routing" },
  { route: "#/combos", label: "Combos", group: "Routing" },
  { route: "#/keys", label: "API Keys", group: "Access & network" },
  { route: "#/proxies", label: "Free Proxies", group: "Access & network" },
  { route: "#/proxy-sources", label: "Proxy Sources", group: "Access & network" },
  { route: "#/logs", label: "Live Logs", group: "Observability" },
  { route: "#/analytics", label: "Analytics", group: "Observability" },
  { route: "#/models", label: "Models", group: "Tools" },
  { route: "#/debug-logs", label: "Debug Logs", group: "Observability" },
  { route: "#/playground", label: "Playground", group: "Tools" },
  { route: "#/notifications", label: "Notifications", group: "System" },
  { route: "#/config", label: "Config", group: "System" },
] as const;
/** Canonical shapes for the toolbar presets, in panel-count order. */
export const SHAPES: Record<Layout, Rect[]> = {
  single: [{ x: 0, y: 0, w: 6, h: 6 }],
  split: [{ x: 0, y: 0, w: 3, h: 6 }, { x: 3, y: 0, w: 3, h: 6 }],
  triple: [{ x: 0, y: 0, w: 2, h: 6 }, { x: 2, y: 0, w: 4, h: 3 }, { x: 2, y: 3, w: 4, h: 3 }],
  grid: [{ x: 0, y: 0, w: 3, h: 3 }, { x: 3, y: 0, w: 3, h: 3 }, { x: 0, y: 3, w: 3, h: 3 }, { x: 3, y: 3, w: 3, h: 3 }],
  penta: [{ x: 0, y: 0, w: 2, h: 6 }, { x: 2, y: 0, w: 2, h: 3 }, { x: 4, y: 0, w: 2, h: 3 }, { x: 2, y: 3, w: 2, h: 3 }, { x: 4, y: 3, w: 2, h: 3 }],
  hex: [{ x: 0, y: 0, w: 2, h: 3 }, { x: 2, y: 0, w: 2, h: 3 }, { x: 4, y: 0, w: 2, h: 3 }, { x: 0, y: 3, w: 2, h: 3 }, { x: 2, y: 3, w: 2, h: 3 }, { x: 4, y: 3, w: 2, h: 3 }],
};
export const PRESETS: { label: string; layout: Layout; routes: string[] }[] = [
  { label: "Routing desk", layout: "split", routes: ["#/combos", "#/playground"] },
  { label: "Observe & inspect", layout: "split", routes: ["#/logs", "#/debug-logs"] },
  { label: "Operations", layout: "triple", routes: ["#/providers", "#/logs", "#/analytics"] },
  { label: "Overview grid", layout: "grid", routes: ["#/", "#/combos", "#/logs", "#/providers"] },
  { label: "Command center", layout: "hex", routes: ["#/logs", "#/analytics", "#/", "#/providers", "#/combos", "#/keys"] },
];
const COUNTS: Record<Layout, number> = { single: 1, split: 2, triple: 3, grid: 4, penta: 5, hex: 6 };
/** Panel budget: a tile per family, six deep. */
export const MAX_PANELS = 6;
let sequence = 0;
function tile(route: string): Tile {
  return { id: `tile-${Date.now()}-${++sequence}`, route, rect: { x: 0, y: 0, w: 3, h: 3 } };
}
export function safeRoute(value: unknown): value is string {
  return typeof value === "string" && value.length <= 2048 &&
    /^#\/(?:$|providers(?:\/[^?#]+)?$|combos(?:\/\d+)?$|keys(?:\/\d+(?:\/usage)?)?$|(?:analytics|logs)(?:\?[^#]*)?$|(?:debug-logs|config|notifications|proxies|proxy-sources|playground|models)(?:\?[^#]*)?$)/.test(value);
}
export function routeMeta(route: string): { route: string; label: string; group: string } {
  return VIEWS.find(v => v.route !== "#/" && (route === v.route || route.startsWith(v.route + "/") || route.startsWith(v.route + "?"))) ?? VIEWS[0]!;
}
export function clampSplit(value: number): number { return Math.min(75, Math.max(25, Number.isFinite(value) ? value : 50)); }
export function rectValid(value: unknown): value is Rect {
  if (!value || typeof value !== "object") return false;
  const r = value as Record<string, unknown>;
  const int = (v: unknown): v is number => typeof v === "number" && Number.isInteger(v);
  if (!int(r["x"]) || !int(r["y"]) || !int(r["w"]) || !int(r["h"])) return false;
  return r["w"] >= 1 && r["h"] >= 1 && r["w"] <= GRID.cols && r["h"] <= GRID.rows &&
    r["x"] >= 0 && r["y"] >= 0 && r["x"] + r["w"] <= GRID.cols && r["y"] + r["h"] <= GRID.rows;
}
export function clampRect(rect: Rect): Rect {
  const w = Math.min(GRID.cols, Math.max(1, Math.round(rect.w)));
  const h = Math.min(GRID.rows, Math.max(1, Math.round(rect.h)));
  return { x: Math.min(GRID.cols - w, Math.max(0, Math.round(rect.x))), y: Math.min(GRID.rows - h, Math.max(0, Math.round(rect.y))), w, h };
}
export function overlaps(a: Rect, b: Rect): boolean {
  return a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
}
/** First top-left spot where a `size` rect fits without touching the given tiles. */
export function findFreeRect(tiles: readonly Tile[], size: { w: number; h: number }, exceptId = ""): Rect | null {
  const w = Math.min(GRID.cols, Math.max(1, Math.round(size.w)));
  const h = Math.min(GRID.rows, Math.max(1, Math.round(size.h)));
  for (let y = 0; y <= GRID.rows - h; y++) {
    for (let x = 0; x <= GRID.cols - w; x++) {
      const candidate = { x, y, w, h };
      if (!tiles.some(t => t.id !== exceptId && overlaps(t.rect, candidate))) return candidate;
    }
  }
  return null;
}
/** Simulate placing `rect` for tile `draggedId` (which may not be in `tiles`
 *  yet, e.g. a spawn). Every tile the rect touches must relocate to a free
 *  spot of its own size; when one cannot, the drop does not fit and the
 *  caller must reject it. Returns the relocation map, or null. */
export function resolveFit(tiles: readonly Tile[], draggedId: string, rect: Rect): Map<string, Rect> | null {
  const next = clampRect(rect);
  if (next.w > GRID.cols || next.h > GRID.rows) return null;
  const future = new Map<string, Rect>(tiles.map(t => [t.id, { ...t.rect }]));
  future.set(draggedId, next);
  const collides = (candidate: Rect, selfId: string): boolean =>
    [...future.entries()].some(([id, r]) => id !== selfId && overlaps(r, candidate));
  const moves = new Map<string, Rect>();
  for (const t of tiles) {
    if (t.id === draggedId || !overlaps(future.get(t.id)!, next)) continue;
    const size = { w: t.rect.w, h: t.rect.h };
    let spot: Rect | null = null;
    for (let y = 0; y <= GRID.rows - size.h && !spot; y++) {
      for (let x = 0; x <= GRID.cols - size.w && !spot; x++) {
        const candidate = { x, y, w: size.w, h: size.h };
        if (!collides(candidate, t.id)) spot = candidate;
      }
    }
    if (!spot) return null;
    future.set(t.id, spot);
    moves.set(t.id, spot);
  }
  return moves;
}
export interface DropPlan {
  /** The rect the drop lands at: the requested one, or the grown strip that
   *  fits cleanly between the neighbours. */
  rect: Rect;
  /** Tiles that slide intact to a new rect (relocation). */
  moves: Map<string, Rect>;
  /** Tiles that shrink, giving up the strip the drop covers (carve). */
  carves: Map<string, Rect>;
}
/** How a touched tile gives up the strip a drop covers. A cut is only
 *  representable when it reaches a full edge in one axis: a full-height strip
 *  leaves the left or right part, a full-width strip leaves the top or bottom
 *  part. The larger side survives, so a neighbour never shrinks more than the
 *  insertion needs. Anything else would leave an L-shape and is refused. */
function carveRect(t: Tile, d: Rect): Rect | null {
  const r = t.rect;
  const ix = Math.max(r.x, d.x), iy = Math.max(r.y, d.y);
  const ix2 = Math.min(r.x + r.w, d.x + d.w), iy2 = Math.min(r.y + r.h, d.y + d.h);
  if (ix >= ix2 || iy >= iy2) return null;
  const out: Rect[] = [];
  if (iy2 - iy === r.h) {
    if (ix > r.x) out.push({ x: r.x, y: r.y, w: ix - r.x, h: r.h });
    if (ix2 < r.x + r.w) out.push({ x: ix2, y: r.y, w: r.x + r.w - ix2, h: r.h });
  }
  if (ix2 - ix === r.w) {
    if (iy > r.y) out.push({ x: r.x, y: r.y, w: r.w, h: iy - r.y });
    if (iy2 < r.y + r.h) out.push({ x: r.x, y: iy2, w: r.w, h: r.y + r.h - iy2 });
  }
  if (!out.length) return null;
  out.sort((a, b) => b.w * b.h - a.w * a.h);
  return out[0]!;
}
/** Carve every touched tile around `rect`. Refuses any result that overlaps
 *  (including the drop itself) or that cannot be expressed in rectangles. */
function tryCarve(tiles: readonly Tile[], draggedId: string, rect: Rect): DropPlan | null {
  const carves = new Map<string, Rect>();
  for (const tile of tiles) {
    if (tile.id === draggedId) continue;
    if (!overlaps(tile.rect, rect)) continue;
    const cut = carveRect(tile, rect);
    if (!cut) return null;
    carves.set(tile.id, cut);
  }
  const after = tiles.map(tile => ({ ...tile, rect: carves.get(tile.id) ?? tile.rect }));
  for (let i = 0; i < after.length; i++) {
    for (let j = i + 1; j < after.length; j++) {
      if (overlaps(after[i]!.rect, after[j]!.rect)) return null;
    }
    if (overlaps(after[i]!.rect, rect)) return null;
  }
  return { rect, moves: new Map(), carves };
}
/** A drop between two tiles needs a CLEAN STRIP, not the exact pointer rect:
 *  a 2x1 ghost over a 3+3 seam would cut an L out of both neighbours, which no
 *  rect set can express. Growing the drop to the full height (or width) turns
 *  the insertion into one edge cut per neighbour, and the min-size check then
 *  decides if the strip is actually usable. */
function *dropCandidates(rect: Rect): IterableIterator<Rect> {
  yield rect;
  yield { ...rect, y: 0, h: GRID.rows };
  yield { ...rect, x: 0, w: GRID.cols };
}
/** Decide how the canvas absorbs a drop of `rect` at tile `draggedId`. First
 *  choice: slide every touched tile to a free spot intact. When the canvas
 *  cannot find room, the automated fallback CARVES: neighbours give up the
 *  strip the drop covers, so a panel lands between two. The returned rect is
 *  what the caller must draw as ghost and commit on drop. `noMoves` skips the
 *  relocation pass entirely: a split drop on a tile must CARVE that tile in
 *  place, never teleport it to a distant free hole. */
export function planDrop(tiles: readonly Tile[], draggedId: string, rect: Rect, opts?: { noMoves?: boolean }): DropPlan | null {
  const next = clampRect(rect);
  if (!opts?.noMoves) {
    const moves = resolveFit(tiles, draggedId, next);
    if (moves) return { rect: next, moves, carves: new Map() };
  }
  for (const cand of dropCandidates(next)) {
    const plan = tryCarve(tiles, draggedId, cand);
    if (plan) return plan;
  }
  return null;
}
function applyPlan(s: WorkspaceState, id: string, rect: Rect, plan: DropPlan): void {
  for (const tile of s.tiles) {
    const move = plan.moves.get(tile.id) ?? plan.carves.get(tile.id);
    if (move) tile.rect = move;
  }
  const target = s.tiles.find(t => t.id === id)!;
  target.rect = rect;
  s.focused = id;
}
/** Move or resize a tile to an exact rect. The result is planned first:
 *  touched tiles slide intact when they fit, are carved when the canvas has no
 *  room, and the drop is refused when neither resolves cleanly. */
export function placeTile(s: WorkspaceState, id: string, rect: Rect): void {
  const target = s.tiles.find(t => t.id === id);
  if (!target) return;
  const next = clampRect(rect);
  if (next.x === target.rect.x && next.y === target.rect.y && next.w === target.rect.w && next.h === target.rect.h) return;
  const plan = planDrop(s.tiles, id, next);
  if (!plan) return;
  applyPlan(s, id, plan.rect, plan);
}
/** Resize a tile to an exact rect WITHOUT touching any other tile. The caller
 *  (grip drag) clips the rect against neighbors, so this is always overlap-free.
 *  Rearranging other panels is a move-only behavior, never a resize side effect. */
export function setTileRect(s: WorkspaceState, id: string, rect: Rect): void {
  const target = s.tiles.find(t => t.id === id);
  if (!target) return;
  target.rect = clampRect(rect);
  s.focused = id;
}
/** Exchange two tiles' rects. Ids stay bound to their views, so the mounted
 *  panels travel with the DOM sections and nothing remounts or loses state. */
export function swapTiles(s: WorkspaceState, idA: string, idB: string): void {
  if (idA === idB) return;
  const a = s.tiles.find(t => t.id === idA);
  const b = s.tiles.find(t => t.id === idB);
  if (!a || !b) return;
  [a.rect, b.rect] = [b.rect, a.rect];
}
/** Swap rects with the previous/next tile in array order (keyboard movers). */
export function moveTile(s: WorkspaceState, id: string, delta: number): void {
  const from = s.tiles.findIndex(t => t.id === id);
  if (from < 0) return;
  const to = Math.min(s.tiles.length - 1, Math.max(0, from + Math.trunc(delta)));
  if (to === from) return;
  swapTiles(s, id, s.tiles[to]!.id);
}
/** Open a view in a brand-new tile at `rect` when the canvas has room. A view
 *  family already open elsewhere is focused instead of duplicated. */
export function spawnTile(s: WorkspaceState, route: string, rect: Rect, opts?: { noMoves?: boolean }): boolean {
  if (!safeRoute(route) || s.tiles.length >= MAX_PANELS) return false;
  const family = routeMeta(route).route;
  const owner = s.tiles.find(t => routeMeta(t.route).route === family);
  if (owner) { assignTileRoute(s, owner.id, route); return true; }
  const next = tile(route);
  next.rect = clampRect(rect);
  const plan = planDrop(s.tiles, next.id, next.rect, opts);
  if (!plan) return false;
  next.rect = plan.rect;
  s.tiles.push(next);
  applyPlan(s, next.id, plan.rect, plan);
  s.focused = next.id;
  s.maximized = null;
  s.layout = LAYOUTS[Math.min(LAYOUTS.length, s.tiles.length) - 1]!;
  return true;
}
export function createWorkspace(route = "#/"): WorkspaceState {
  const first = tile(safeRoute(route) ? route : "#/");
  first.rect = { ...SHAPES.single[0]! };
  // A fresh workspace starts as the maximized dashboard: the user shapes the
  // arrange by dragging views in (each drop splits the canvas, up to 6 tiles).
  return { version: 1, layout: "single", tiles: [first], focused: first.id, maximized: first.id, column: 54, row: 52, contextCollapsed: false };
}
export function restoreWorkspace(raw: string | null, route = "#/"): WorkspaceState {
  const fallback = createWorkspace(route);
  try {
    const value: unknown = JSON.parse(raw ?? "null");
    if (!value || typeof value !== "object") return fallback;
    const x = value as Record<string, unknown>;
    if (!Array.isArray(x["tiles"])) return fallback;
    const entries = x["tiles"] as unknown[];
    // The layout name is a hint, not a gate: an unknown or renamed value
    // infers from the tile count instead of discarding the whole arrange.
    const layout = (Object.hasOwn(COUNTS, String(x["layout"]))
      ? x["layout"]
      : LAYOUTS[Math.min(LAYOUTS.length, Math.max(1, entries.length)) - 1]) as Layout;
    const shape = SHAPES[layout];
    const clean: Tile[] = [];
    const families = new Set<string>();
    for (const entry of entries.slice(0, MAX_PANELS)) {
      if (!entry || typeof entry !== "object") continue;
      const v = entry as Tile;
      if (typeof v.id !== "string" || !/^[a-zA-Z0-9-]{1,100}$/.test(v.id) || !safeRoute(v.route)) continue;
      // A repeated family loses the DUPLICATE, never the whole arrange.
      const family = routeMeta(v.route).route;
      if (families.has(family)) continue;
      families.add(family);
      const fallbackRect = shape[clean.length];
      const rect = rectValid(v.rect) ? clampRect(v.rect)
        : fallbackRect ? { ...fallbackRect }
        : findFreeRect(clean, { w: 3, h: 3 }, v.id) ?? { x: 0, y: 0, w: 3, h: 3 };
      clean.push({ id: v.id, route: v.route, rect });
    }
    if (clean.length === 0) {
      try { if (raw) localStorage.setItem(WORKSPACE_KEY + ":orphan", raw); } catch { /* best effort */ }
      return fallback;
    }
    // The layout field tracks the tile count (the toolbar reads it); a
    // salvaged count keeps its own name instead of the payload's.
    const finalLayout = (COUNTS[layout] === clean.length ? layout : LAYOUTS[Math.min(LAYOUTS.length, clean.length) - 1]) as Layout;
    // Resolve any overlap deterministically, without compacting deliberate holes.
    const placed: Tile[] = [];
    for (const t of clean) {
      if (placed.some(p => overlaps(p.rect, t.rect))) {
        const spot = findFreeRect(placed, t.rect, t.id) ??
          findFreeRect(placed, { w: Math.min(2, t.rect.w), h: Math.min(2, t.rect.h) }, t.id) ??
          findFreeRect(placed, { w: 1, h: 1 }, t.id);
        if (spot) t.rect = spot;
      }
      placed.push(t);
    }
    // The maximized decision survives reload too: any id that names one of
    // the restored tiles keeps the hero up; anything else falls back to none.
    return { version: 1, layout: finalLayout, tiles: clean, focused: clean.some(v => v.id === x["focused"]) ? String(x["focused"]) : clean[0]!.id,
      maximized: clean.some(v => v.id === x["maximized"]) ? String(x["maximized"]) : null,
      column: clampSplit(Number(x["column"])), row: clampSplit(Number(x["row"])), contextCollapsed: x["contextCollapsed"] === true };
  } catch {
    // A payload that cannot even be parsed is kept aside instead of being
    // silently overwritten by the fallback on the next changed().
    try { if (raw) localStorage.setItem(WORKSPACE_KEY + ":orphan", raw); } catch { /* best effort */ }
    return fallback;
  }
}
export function activeTile(s: WorkspaceState): Tile { return s.tiles.find(t => t.id === s.focused) ?? s.tiles[0]!; }
/** Apply a preset shape: tiles keep their routes, slots snap to the shape. */
export function changeLayout(s: WorkspaceState, layout: Layout): void {
  const shape = SHAPES[layout];
  if (layout === "single") s.tiles = [activeTile(s)];
  else {
    s.tiles = s.tiles.slice(0, shape.length);
    const defaults = ["#/logs", "#/", "#/combos", "#/providers", "#/analytics", "#/keys"];
    while (s.tiles.length < shape.length) s.tiles.push(tile(defaults.find(r => !s.tiles.some(t => routeMeta(t.route).route === routeMeta(r).route)) ?? "#/providers"));
  }
  s.tiles.forEach((t, i) => { t.rect = { ...shape[i]! }; });
  s.layout = layout;
  s.maximized = null;
  if (!s.tiles.some(t => t.id === s.focused)) s.focused = s.tiles[0]!.id;
}
export function openSplit(s: WorkspaceState, route: string): void {
  if (!safeRoute(route)) return;
  const existing = s.tiles.find(t => routeMeta(t.route).route === routeMeta(route).route);
  if (existing) { existing.route = route; s.focused = existing.id; s.maximized = null; return; }
  if (s.tiles.length < MAX_PANELS) {
    const next = tile(route);
    next.rect = findFreeRect(s.tiles, { w: 3, h: 3 }, next.id) ?? { x: 0, y: 0, w: 3, h: 3 };
    s.tiles.push(next);
    s.focused = next.id;
    s.layout = LAYOUTS[s.tiles.length - 1]!;
  } else { activeTile(s).route = route; }
  s.maximized = null;
}
export function closeTile(s: WorkspaceState, id: string): void {
  if (s.tiles.length < 2) return;
  s.tiles = s.tiles.filter(t => t.id !== id);
  if (!s.tiles.some(t => t.id === s.focused)) s.focused = s.tiles[0]!.id;
  s.layout = LAYOUTS[s.tiles.length - 1]!;
  s.maximized = null;
}
/** Load a view into a specific tile (drop target). A view family already open
 *  elsewhere swaps rects with the target instead of opening a second owner. */
export function assignTileRoute(s: WorkspaceState, id: string, route: string): void {
  if (!safeRoute(route) || !s.tiles.some(t => t.id === id)) return;
  const family = routeMeta(route).route;
  const owner = s.tiles.find(t => routeMeta(t.route).route === family);
  if (owner && owner.id !== id) { swapTiles(s, owner.id, id); s.focused = owner.id; }
  else {
    const target = s.tiles.find(t => t.id === id)!;
    target.route = route;
    s.focused = id;
  }
  // A drop onto the hero tile keeps the maximization decision.
  if (s.maximized !== null && s.maximized !== id) s.maximized = null;
}
export function applyPreset(s: WorkspaceState, preset: typeof PRESETS[number]): void {
  s.tiles = preset.routes.map(tile);
  const shape = SHAPES[preset.layout];
  s.tiles.forEach((t, i) => { t.rect = { ...shape[i]! }; });
  s.layout = preset.layout;
  s.focused = s.tiles[0]!.id;
  s.maximized = null;
  s.column = 54;
  s.row = 52;
}
export function isPanelDocument(): boolean { return /^\/admin\/panel\/?$/.test(location.pathname); }
