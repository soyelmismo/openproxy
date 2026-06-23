// state/reactive.ts — lit-html based reactive rendering.
//
// Replaces the old innerHTML-based rerenderCurrentView() with
// lit-html's diff-based render(). Only the DOM nodes that actually
// changed are updated — <select> dropdowns stay open, <input> fields
// keep focus, scroll position is preserved.
//
// Usage in a view:
//   import { html } from 'lit-html';
//   import { mountView } from '../state/reactive.js';
//
//   export function mountCombos(el: HTMLElement): () => void {
//     return mountView(el, () => renderCombos());
//   }
//
//   function renderCombos() {
//     return html`<div>...</div>`;
//   }
//
// When state changes, call `requestUpdate()` — it schedules a
// re-render on the next microtask, coalescing multiple calls into
// one. The re-render uses lit-html's `render()` which diffs the
// template result against the previous one and applies only the
// minimal DOM mutations.

import { render, type TemplateResult } from 'lit-html';

/** The container element for the current view. Set by `mountView`. */
let currentContainer: HTMLElement | null = null;

/** The render function for the current view. Returns a lit-html
 *  TemplateResult. */
let currentRenderFn: (() => TemplateResult) | null = null;

/** Whether an update is already scheduled. Prevents multiple
 *  microtask scheduling when state changes rapidly. */
let updateScheduled = false;

/** Mount a view into `container`. Returns a cleanup function that
 *  unmounts it. The `renderFn` is called immediately and whenever
 *  `requestUpdate()` is called (until cleanup). */
export function mountView(
  container: HTMLElement,
  renderFn: () => TemplateResult,
): () => void {
  currentContainer = container;
  currentRenderFn = renderFn;
  // Initial render.
  render(renderFn(), container);
  return () => {
    if (currentContainer === container) {
      currentContainer = null;
      currentRenderFn = null;
    }
  };
}

/** Schedule a re-render of the current view on the next microtask.
 *  Multiple calls within the same tick are coalesced into one
 *  render — this is the key to performance: if 5 state updates
 *  happen synchronously, only ONE lit-html diff+patch runs. */
export function requestUpdate(): void {
  if (updateScheduled) return;
  updateScheduled = true;
  // Use queueMicrotask for minimal latency. lit-html's render is
  // synchronous, so this runs before the next paint.
  queueMicrotask(() => {
    updateScheduled = false;
    if (currentContainer && currentRenderFn) {
      render(currentRenderFn(), currentContainer);
    }
  });
}

/** Force an immediate re-render (bypassing the microtask queue).
 *  Used when the caller needs the DOM to reflect the new state
 *  synchronously (e.g., before showing a modal). */
export function forceUpdate(): void {
  updateScheduled = false;
  if (currentContainer && currentRenderFn) {
    render(currentRenderFn(), currentContainer);
  }
}
