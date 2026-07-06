// state/reactive.ts — lit-html based reactive rendering system.
//
// Replaces the old innerHTML-based rerenderCurrentView() with
// lit-html's diff-based render(). Only the DOM nodes that actually
// changed are updated — <select> dropdowns stay open, <input> fields
// keep focus, scroll position is preserved.
//
// Architecture:
// - Each view mounts via `mountView(container, renderFn)`.
// - `renderFn` returns a lit-html `TemplateResult`.
// - `requestUpdate()` schedules a microtask-coalesced re-render.
// - The router calls `mountView` for each route and captures the
//   returned cleanup function.

import { render, type TemplateResult } from 'lit-html';

let currentContainer: HTMLElement | null = null;
let currentRenderFn: (() => TemplateResult) | null = null;
let updateScheduled = false;
let renderBroken = false;

/** Mount a view into `container`. Returns a cleanup function. */
export function mountView(
  container: HTMLElement,
  renderFn: () => TemplateResult,
): () => void {
  currentContainer = container;
  currentRenderFn = renderFn;
  renderBroken = false;
  render(renderFn(), container);
  return () => {
    if (currentContainer === container) {
      currentContainer = null;
      currentRenderFn = null;
    }
  };
}

/** Schedule a re-render on the next microtask. Coalesces calls.
 *
 *  CRASH RECOVERY: if the previous render() threw (lit-html's repeat
 *  directive can crash when the DOM is in an inconsistent state),
 *  lit-html's internal state is corrupted — subsequent renders will
 *  also crash. To recover, we clear the container completely
 *  (render `nothing` to tear down lit-html's internal refs, then
 *  clear innerHTML as a belt-and-suspenders reset) before doing a
 *  fresh render. This is equivalent to a full unmount + remount. */
export function requestUpdate(): void {
  if (updateScheduled) return;
  updateScheduled = true;
  queueMicrotask(() => {
    updateScheduled = false;
    if (
      !currentContainer ||
      !currentRenderFn ||
      !currentContainer.isConnected ||
      currentContainer.childNodes.length === 0
    ) {
      return;
    }
    try {
      if (renderBroken) {
        // Previous render crashed — lit-html's internal state is
        // corrupted. Reset by clearing the container and deleting its
        // lit-html part reference, then do a fresh render from scratch.
        delete (currentContainer as any)._$litPart$;
        currentContainer.innerHTML = '';
        renderBroken = false;
      }
      render(currentRenderFn(), currentContainer);
    } catch (e) {
      console.error("[openproxy] requestUpdate render() threw:", e);
      renderBroken = true;
      // Schedule a recovery render on the next microtask.
      updateScheduled = true;
      queueMicrotask(() => {
        updateScheduled = false;
        if (currentContainer && currentRenderFn && currentContainer.isConnected) {
          try {
            delete (currentContainer as any)._$litPart$;
            currentContainer.innerHTML = '';
            render(currentRenderFn(), currentContainer);
            renderBroken = false;
          } catch (e2) {
            console.error("[openproxy] recovery render also failed:", e2);
            renderBroken = true;
          }
        }
      });
    }
  });
}

/** Force an immediate synchronous re-render. */
export function forceUpdate(): void {
  updateScheduled = false;
  if (
    !currentContainer ||
    !currentRenderFn ||
    !currentContainer.isConnected ||
    currentContainer.childNodes.length === 0
  ) {
    return;
  }
  try {
    if (renderBroken) {
      delete (currentContainer as any)._$litPart$;
      currentContainer.innerHTML = '';
      renderBroken = false;
    }
    render(currentRenderFn(), currentContainer);
  } catch (e) {
    console.error("[openproxy] forceUpdate render() threw:", e);
    renderBroken = true;
  }
}
