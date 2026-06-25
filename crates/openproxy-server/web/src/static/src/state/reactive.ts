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

/** Mount a view into `container`. Returns a cleanup function. */
export function mountView(
  container: HTMLElement,
  renderFn: () => TemplateResult,
): () => void {
  currentContainer = container;
  currentRenderFn = renderFn;
  render(renderFn(), container);
  return () => {
    if (currentContainer === container) {
      currentContainer = null;
      currentRenderFn = null;
    }
  };
}

/** Schedule a re-render on the next microtask. Coalesces calls. */
export function requestUpdate(): void {
  if (updateScheduled) return;
  updateScheduled = true;
  queueMicrotask(() => {
    updateScheduled = false;
    if (currentContainer && currentRenderFn) {
      render(currentRenderFn(), currentContainer);
    }
  });
}

/** Force an immediate synchronous re-render. */
export function forceUpdate(): void {
  updateScheduled = false;
  if (currentContainer && currentRenderFn) {
    render(currentRenderFn(), currentContainer);
  }
}
