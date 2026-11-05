// Pure workspace state. Routes, not server payloads or credentials, are persisted.
export type Layout = "single" | "split" | "triple" | "grid";
export interface Tile { id: string; route: string }
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
  { route: "#/debug-logs", label: "Debug Logs", group: "Observability" },
  { route: "#/playground", label: "Playground", group: "Tools" },
  { route: "#/notifications", label: "Notifications", group: "System" },
  { route: "#/config", label: "Config", group: "System" },
] as const;
export const PRESETS: { label: string; layout: Layout; routes: string[] }[] = [
  { label: "Routing desk", layout: "split", routes: ["#/combos", "#/playground"] },
  { label: "Observe & inspect", layout: "split", routes: ["#/logs", "#/debug-logs"] },
  { label: "Operations", layout: "triple", routes: ["#/providers", "#/logs", "#/analytics"] },
  { label: "Overview grid", layout: "grid", routes: ["#/", "#/combos", "#/logs", "#/providers"] },
];
const COUNTS: Record<Layout, number> = { single: 1, split: 2, triple: 3, grid: 4 };
let sequence = 0;
function tile(route: string): Tile { return { id: `tile-${Date.now()}-${++sequence}`, route }; }
export function safeRoute(value: unknown): value is string {
  return typeof value === "string" && value.length <= 2048 &&
    /^#\/(?:$|providers(?:\/[^?#]+)?$|combos(?:\/\d+)?$|keys(?:\/\d+(?:\/usage)?)?$|(?:analytics|logs)(?:\?[^#]*)?$|(?:debug-logs|config|notifications|proxies|proxy-sources|playground)$)/.test(value);
}
export function routeMeta(route: string): { route: string; label: string; group: string } {
  return VIEWS.find(v => v.route !== "#/" && (route === v.route || route.startsWith(v.route + "/") || route.startsWith(v.route + "?"))) ?? VIEWS[0]!;
}
export function clampSplit(value: number): number { return Math.min(75, Math.max(25, Number.isFinite(value) ? value : 50)); }
export function createWorkspace(route = "#/"): WorkspaceState {
  const first = tile(safeRoute(route) ? route : "#/");
  return { version: 1, layout: "single", tiles: [first], focused: first.id, maximized: null, column: 54, row: 52, contextCollapsed: false };
}
export function restoreWorkspace(raw: string | null, route = "#/"): WorkspaceState {
  const fallback = createWorkspace(route);
  try {
    const value: unknown = JSON.parse(raw ?? "null");
    if (!value || typeof value !== "object") return fallback;
    const x = value as Record<string, unknown>;
    if (x["version"] !== 1 || !Object.hasOwn(COUNTS, String(x["layout"])) || !Array.isArray(x["tiles"])) return fallback;
    const layout = x["layout"] as Layout;
    const tiles = x["tiles"].slice(0, 4).filter((v: unknown): v is Tile => !!v && typeof v === "object" &&
      typeof (v as Tile).id === "string" && /^[a-zA-Z0-9-]{1,100}$/.test((v as Tile).id) && safeRoute((v as Tile).route))
      .map(v => ({ id: v.id, route: v.route }));
    if (tiles.length !== COUNTS[layout] || new Set(tiles.map(v => v.id)).size !== tiles.length ||
      new Set(tiles.map(v => routeMeta(v.route).route)).size !== tiles.length) return fallback;
    return { version: 1, layout, tiles, focused: tiles.some(v => v.id === x["focused"]) ? String(x["focused"]) : tiles[0]!.id,
      maximized: null, column: clampSplit(Number(x["column"])), row: clampSplit(Number(x["row"])), contextCollapsed: x["contextCollapsed"] === true };
  } catch { return fallback; }
}
export function activeTile(s: WorkspaceState): Tile { return s.tiles.find(t => t.id === s.focused) ?? s.tiles[0]!; }
export function changeLayout(s: WorkspaceState, layout: Layout): void {
  const count = COUNTS[layout];
  if (layout === "single") s.tiles = [activeTile(s)];
  else {
    s.tiles = s.tiles.slice(0, count);
    const defaults = ["#/logs", "#/", "#/combos", "#/providers"];
    while (s.tiles.length < count) s.tiles.push(tile(defaults.find(r => !s.tiles.some(t => routeMeta(t.route).route === routeMeta(r).route)) ?? "#/providers"));
  }
  s.layout = layout;
  s.maximized = null;
  if (!s.tiles.some(t => t.id === s.focused)) s.focused = s.tiles[0]!.id;
}
export function openSplit(s: WorkspaceState, route: string): void {
  if (!safeRoute(route)) return;
  const existing = s.tiles.find(t => routeMeta(t.route).route === routeMeta(route).route);
  if (existing) { existing.route = route; s.focused = existing.id; s.maximized = null; return; }
  if (s.tiles.length < 4) {
    const next = tile(route);
    s.tiles.push(next);
    s.focused = next.id;
    s.layout = (["single", "split", "triple", "grid"] as Layout[])[s.tiles.length - 1]!;
  } else { activeTile(s).route = route; }
  s.maximized = null;
}
export function closeTile(s: WorkspaceState, id: string): void {
  if (s.tiles.length < 2) return;
  s.tiles = s.tiles.filter(t => t.id !== id);
  if (!s.tiles.some(t => t.id === s.focused)) s.focused = s.tiles[0]!.id;
  s.layout = (["single", "split", "triple", "grid"] as Layout[])[s.tiles.length - 1]!;
  s.maximized = null;
}
export function applyPreset(s: WorkspaceState, preset: typeof PRESETS[number]): void {
  s.tiles = preset.routes.map(tile);
  s.layout = preset.layout;
  s.focused = s.tiles[0]!.id;
  s.maximized = null;
  s.column = 54;
  s.row = 52;
}
export function isPanelDocument(): boolean { return /^\/admin\/panel\/?$/.test(location.pathname); }
