// Pointer drag for the tiling workspace. The engine owns only the gesture:
// threshold, badge, listeners, click-eat. Placement semantics — ghost preview,
// cell math, drop resolution — live with the component via hooks. Mouse-only
// on purpose: touch keeps native scrolling, and a threshold keeps clicks intact.

export type GripCorner = "nw" | "ne" | "sw" | "se" | "n" | "s" | "e" | "w";
export type DragPayload =
  | { kind: "tile"; tileId: string; label: string }
  | { kind: "view"; route: string; label: string }
  | { kind: "resize"; tileId: string; corner: GripCorner; label: string };

export interface DragHooks {
  /** Per-frame pointer feedback. Return a badge text; "" keeps the payload label. */
  onFrame?: (x: number, y: number, tileId: string | null, payload: DragPayload) => string;
  onDrop: (x: number, y: number, tileId: string | null, payload: DragPayload) => void;
  /** Abort path (Escape, pointercancel): release per-drag visuals like the ghost. */
  onCancel?: () => void;
}

const THRESHOLD = 6;
let active = false;
let dragging = false;
let startX = 0;
let startY = 0;
let source: HTMLElement | null = null;
let payload: DragPayload | null = null;
let hooks: DragHooks | null = null;
let hoverId: string | null = null;
let badge: HTMLElement | null = null;
let raf = 0;
let lastEvent: PointerEvent | null = null;
let suppressClick = false;

function ensureBadge(): HTMLElement {
  if (badge?.isConnected) return badge;
  badge = document.createElement("div");
  badge.className = "workspace-drag-badge";
  badge.setAttribute("aria-hidden", "true");
  document.body.appendChild(badge);
  return badge;
}
function paintBadge(x: number, y: number, text: string): void {
  const el = ensureBadge();
  el.textContent = text;
  el.style.transform = `translate3d(${Math.round(x + 14)}px, ${Math.round(y + 18)}px, 0)`;
}
function clearHover(): void {
  if (!hoverId) return;
  document.querySelector(`[data-panel="${CSS.escape(hoverId)}"]`)?.classList.remove("drop-target");
  hoverId = null;
}
function targetAt(x: number, y: number, sourceId: string | null): string | null {
  const el = document.elementFromPoint(x, y)?.closest<HTMLElement>(".workspace-tile");
  const id = el?.dataset["panel"] ?? null;
  return id && id !== sourceId ? id : null;
}
function cleanup(): void {
  const ending = hooks;
  active = false;
  dragging = false;
  payload = null;
  hooks = null;
  lastEvent = null;
  ending?.onCancel?.();
  if (raf) { cancelAnimationFrame(raf); raf = 0; }
  window.removeEventListener("pointermove", onMove);
  window.removeEventListener("pointerup", onUp);
  window.removeEventListener("pointercancel", onCancel);
  window.removeEventListener("keydown", onKey, true);
  document.body.classList.remove("workspace-dragging");
  source?.classList.remove("drag-source");
  source?.removeEventListener("dragstart", stopNative);
  source = null;
  clearHover();
  badge?.remove();
  badge = null;
  if (suppressClick) {
    // The pointerup that ends a drag on a link still synthesizes a click;
    // eat exactly one so a drag never navigates.
    const eat = (e: MouseEvent): void => { e.preventDefault(); e.stopPropagation(); };
    document.addEventListener("click", eat, { capture: true, once: true });
    suppressClick = false;
  }
}
function activate(): void {
  dragging = true;
  document.body.classList.add("workspace-dragging");
  source?.classList.add("drag-source");
}
function refresh(): void {
  raf = 0;
  if (!dragging || !lastEvent || !payload) return;
  const x = lastEvent.clientX;
  const y = lastEvent.clientY;
  const id = targetAt(x, y, payload.kind === "tile" ? payload.tileId : null);
  if (id !== hoverId) {
    clearHover();
    if (id) {
      document.querySelector(`[data-panel="${CSS.escape(id)}"]`)?.classList.add("drop-target");
      hoverId = id;
    }
  }
  let text = payload.label;
  if (hooks?.onFrame) text = hooks.onFrame(x, y, id, payload) || text;
  paintBadge(x, y, text);
}
function onMove(e: PointerEvent): void {
  if (!active || !payload) return;
  if (e.pointerType !== "mouse") return;
  lastEvent = e;
  if (!dragging) {
    if (Math.hypot(e.clientX - startX, e.clientY - startY) < THRESHOLD) return;
    activate();
  }
  e.preventDefault();
  if (!raf) raf = requestAnimationFrame(refresh);
}
function onUp(e: PointerEvent): void {
  if (!active || !payload) { cleanup(); return; }
  const drop = dragging;
  const x = e.clientX;
  const y = e.clientY;
  const id = drop ? targetAt(x, y, payload.kind === "tile" ? payload.tileId : null) : null;
  const dropPayload = payload;
  const handler = hooks;
  if (drop && dropPayload.kind === "view") suppressClick = true;
  cleanup();
  if (drop && handler) handler.onDrop(x, y, id, dropPayload);
}
function onCancel(): void { cleanup(); }
function onKey(e: KeyboardEvent): void {
  if (e.key === "Escape" && active) { e.preventDefault(); e.stopPropagation(); cleanup(); }
}
function begin(e: PointerEvent, sourceEl: HTMLElement, next: DragPayload, nextHooks: DragHooks): void {
  if (active || e.button !== 0 || e.pointerType !== "mouse") return;
  active = true;
  dragging = false;
  suppressClick = false;
  startX = e.clientX;
  startY = e.clientY;
  payload = next;
  hooks = nextHooks;
  source = sourceEl;
  // Native HTML5 drag would hijack the gesture before our threshold fires.
  sourceEl.addEventListener("dragstart", stopNative);
  try { sourceEl.setPointerCapture(e.pointerId); } catch { /* element may vanish; window listeners still run */ }
  window.addEventListener("pointermove", onMove);
  window.addEventListener("pointerup", onUp);
  window.addEventListener("pointercancel", onCancel);
  window.addEventListener("keydown", onKey, true);
}
function stopNative(e: Event): void {
  if (active) e.preventDefault();
}
/** Drag a panel by its header: the hooks resolve swap vs free placement.
 *  Buttons inside the header (view picker, maximize, close) keep their clicks. */
export function beginTileDrag(e: PointerEvent, tileId: string, label: string, dragHooks: DragHooks): void {
  if (e.target instanceof Element && e.target.closest("button, a, input, select, textarea, .workspace-menu, .workspace-divider, .tile-grip")) return;
  if (!(e.currentTarget instanceof HTMLElement)) return;
  begin(e, e.currentTarget, { kind: "tile", tileId, label }, dragHooks);
}
/** Drag a corner grip to resize a panel live; hooks compute the pending rect. */
export function beginResizeDrag(e: PointerEvent, tileId: string, corner: GripCorner, label: string, dragHooks: DragHooks): void {
  if (!(e.currentTarget instanceof HTMLElement)) return;
  begin(e, e.currentTarget, { kind: "resize", tileId, corner, label }, dragHooks);
}
/** Drag a sidebar view link: hooks decide load-into-panel vs spawn-on-empty-cell.
 *  The source arrives explicitly because callers delegate from a document listener. */
export function beginViewDrag(e: PointerEvent, source: HTMLElement, route: string, label: string, dragHooks: DragHooks): void {
  begin(e, source, { kind: "view", route, label }, dragHooks);
}
export function dragInProgress(): boolean { return active; }
