// Same-document view ownership. Retired targets never become a later route's target.
import { html, render } from 'lit-html';
import { mountRouteInto } from '../state/router.js';
import { unmountView } from '../state/reactive.js';
import { routeMeta, type Tile } from '../state/workspace.js';

interface MountedPanel {
  route: string;
  target: HTMLElement;
  disposed: boolean;
  cleanup: (() => void) | null;
}
const panels = new Map<string, MountedPanel>();
// Existing views contain family-local stores. Serialize their teardown/remount,
// including late async completion, without blocking unrelated live views.
const pending = new Map<string, Promise<void>>();
function dispose(panel: MountedPanel): void {
  panel.disposed = true;
  unmountView(panel.target);
  panel.cleanup?.();
  panel.cleanup = null;
  panel.target.remove();
}
export function clearNativePanels(): void {
  for (const panel of panels.values()) dispose(panel);
  panels.clear();
}
export function reconcileNativePanels(root: HTMLElement, tiles: Tile[]): void {
  const alive = new Set(tiles.map(tile => tile.id));
  for (const [id, panel] of panels) {
    if (!alive.has(id)) { dispose(panel); panels.delete(id); }
  }
  for (const tile of tiles) {
    const previous = panels.get(tile.id);
    if (previous?.route === tile.route) continue;
    if (previous) dispose(previous);
    const host = root.querySelector<HTMLElement>(`[data-tile="${tile.id}"]`);
    if (!host) continue;
    const target = document.createElement('div');
    target.className = 'tile-view';
    target.dataset['route'] = tile.route;
    host.appendChild(target);
    const panel: MountedPanel = { route: tile.route, target, disposed: false, cleanup: null };
    panels.set(tile.id, panel);
    const family = routeMeta(tile.route).route;
    const task = (pending.get(family) ?? Promise.resolve()).then(async () => {
      if (panel.disposed || !target.isConnected) return;
      try {
        const cleanup = await mountRouteInto(panel.route, target);
        if (typeof cleanup === 'function') {
          if (panel.disposed) cleanup();
          else panel.cleanup = cleanup;
        }
      } catch (error) {
        if (!panel.disposed) render(html`<div class="banner banner-error" role="alert">${error instanceof Error ? error.message : String(error)}</div>`, target);
      }
    });
    pending.set(family, task);
    void task.finally(() => { if (pending.get(family) === task) pending.delete(family); });
  }
}
