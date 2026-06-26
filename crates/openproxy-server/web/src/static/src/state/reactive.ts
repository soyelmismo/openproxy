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

/** Schedule a re-render on the next microtask. Coalesces calls.
 *
 *  CRASH FIX: checks that the container is actually connected to the
 *  DOM before rendering. During boot, the WS opens (via
 *  initNotificationsStore in the sidebar) BEFORE the router mounts
 *  the first view. If a WS message arrives during this window and
 *  triggers requestUpdate(), lit-html tries to render into a
 *  container that either doesn't exist yet or was cleaned up —
 *  causing "can't access property 'data', nextSibling is null"
 *  (a lit-html internal crash in the repeat() directive).
 *
 *  The `isConnected` check (Node.isConnected) returns true only when
 *  the element is in the document. This is a cheap O(1) check that
 *  prevents the crash without needing per-view guards. */
export function requestUpdate(): void {
  if (updateScheduled) return;
  updateScheduled = true;
  queueMicrotask(() => {
    updateScheduled = false;
    // Guard: only render if the container is connected AND has
    // already been initialized by a mountView() call (has childNodes).
    // During boot, the WS opens before the first view mounts — the
    // #main div exists and isConnected=true, but it's empty. Calling
    // render() on an empty container with a template that uses
    // repeat() causes lit-html to crash ("nextSibling is null")
    // because the directive expects to diff against existing DOM nodes.
    // The childNodes.length > 0 check ensures we only re-render
    // containers that have already been initialized.
    if (
      currentContainer &&
      currentRenderFn &&
      currentContainer.isConnected &&
      currentContainer.childNodes.length > 0
    ) {
      try {
        render(currentRenderFn(), currentContainer);
      } catch (e) {
        // lit-html can crash if the DOM is in an inconsistent state
        // (e.g. a view was unmounted between the requestUpdate()
        // call and the microtask). Log the error but don't propagate
        // — the next requestUpdate() will try again with a clean DOM.
        console.error("[openproxy] requestUpdate render() threw:", e);
      }
    }
  });
}

/** Force an immediate synchronous re-render. */
export function forceUpdate(): void {
  updateScheduled = false;
  if (
    currentContainer &&
    currentRenderFn &&
    currentContainer.isConnected &&
    currentContainer.childNodes.length > 0
  ) {
    try {
      render(currentRenderFn(), currentContainer);
    } catch (e) {
      console.error("[openproxy] forceUpdate render() threw:", e);
    }
  }
}
