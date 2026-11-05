// lit-html re-rendering, replacing the old innerHTML-based
// `rerenderCurrentView()`. A diff touches only the nodes that changed, so
// <select> dropdowns stay open, inputs keep focus, and scroll survives.
//
// A view mounts via `mountView(container, renderFn)`; the router or panel host
// captures the returned cleanup. Multiple concurrent containers can be mounted
// (e.g. workspace tiles), and updates coalesce into a single microtask broadcast.

import { render, type TemplateResult } from 'lit-html';

interface MountEntry {
  renderFn: () => TemplateResult;
  renderBroken: boolean;
  generation: number;
}

const mounts = new Map<HTMLElement, MountEntry>();
const retiredTargets = new WeakSet<HTMLElement>();

let updateScheduled = false;
let nextGeneration = 0;

/** Mount a view into `container`. Returns a cleanup function.
 *
 * If `container` was retired via `unmountView`, registration is rejected
 * and an empty cleanup function is returned.
 */
export function mountView(
  container: HTMLElement,
  renderFn: () => TemplateResult,
): () => void {
  if (retiredTargets.has(container)) {
    return () => {};
  }

  const generation = ++nextGeneration;
  mounts.set(container, {
    renderFn,
    renderBroken: false,
    generation,
  });

  try {
    render(renderFn(), container);
  } catch (e) {
    console.error("[openproxy] mountView initial render() threw:", e);
    const entry = mounts.get(container);
    if (entry && entry.generation === generation) {
      entry.renderBroken = true;
    }
  }

  return () => {
    const entry = mounts.get(container);
    if (entry && entry.generation === generation) {
      mounts.delete(container);
    }
  };
}

/** Synchronously deregisters and retires a mount target.
 *
 * Marks `container` in `retiredTargets` so late async mount calls targeting
 * this detached element cannot re-register. Drops its render entry and resets
 * lit-html internal parts if present.
 */
export function unmountView(container: HTMLElement): void {
  retiredTargets.add(container);
  mounts.delete(container);
  try {
    delete (container as { _$litPart$?: unknown })._$litPart$;
  } catch {
    // Ignore in non-extensible objects
  }
}

function renderEntry(container: HTMLElement, entry: MountEntry): void {
  if (!container.isConnected) {
    return;
  }
  // If not broken and has no children, wait for initial render or broken recovery
  if (!entry.renderBroken && container.childNodes.length === 0) {
    return;
  }
  const gen = entry.generation;
  try {
    if (entry.renderBroken) {
      // Reset lit-html's internal refs before re-rendering.
      delete (container as { _$litPart$?: unknown })._$litPart$;
      container.innerHTML = '';
      entry.renderBroken = false;
    }
    render(entry.renderFn(), container);
  } catch (e) {
    console.error("[openproxy] requestUpdate render() threw:", e);
    entry.renderBroken = true;
    // Retry on the next microtask for this container
    queueMicrotask(() => {
      const current = mounts.get(container);
      if (current && current.generation === gen && container.isConnected) {
        try {
          delete (container as { _$litPart$?: unknown })._$litPart$;
          container.innerHTML = '';
          render(current.renderFn(), container);
          current.renderBroken = false;
        } catch (e2) {
          console.error("[openproxy] recovery render also failed:", e2);
          current.renderBroken = true;
        }
      }
    });
  }
}

/** Schedule a re-render on the next microtask. Coalesces calls across all mounted containers. */
export function requestUpdate(): void {
  if (updateScheduled) return;
  updateScheduled = true;
  queueMicrotask(() => {
    updateScheduled = false;
    // Snapshot the entries in case rendering modifies mounts
    const entries = Array.from(mounts.entries());
    for (const [container, entry] of entries) {
      if (mounts.get(container) === entry) {
        renderEntry(container, entry);
      }
    }
  });
}

/** Force an immediate synchronous re-render across all mounted containers. */
export function forceUpdate(): void {
  updateScheduled = false;
  const entries = Array.from(mounts.entries());
  for (const [container, entry] of entries) {
    if (mounts.get(container) === entry) {
      if (!container.isConnected) {
        continue;
      }
      if (!entry.renderBroken && container.childNodes.length === 0) {
        continue;
      }
      try {
        if (entry.renderBroken) {
          delete (container as { _$litPart$?: unknown })._$litPart$;
          container.innerHTML = '';
          entry.renderBroken = false;
        }
        render(entry.renderFn(), container);
      } catch (e) {
        console.error("[openproxy] forceUpdate render() threw:", e);
        entry.renderBroken = true;
      }
    }
  }
}
