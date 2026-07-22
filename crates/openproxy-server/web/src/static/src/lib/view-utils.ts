// lib/view-utils.ts — shared view lifecycle helpers.
//
// Eliminates the repeated mount pattern across simple views:
//   1. document.getElementById("main") + null guard
//   2. mountView(container, renderFn) — immediate first render
//   3. await loadData() — fetch data / populate state
//   4. requestUpdate() — re-render with fetched data (or error state)
//   5. return cleanup function
//
// Views that have extra lifecycle (subscriptions, background refresh,
// WebSocket connections, chart instances, custom cleanup) should NOT
// use this helper — handle them manually.

import { type TemplateResult } from "lit-html";
import { mountView, requestUpdate } from "../state/reactive.js";

/**
 * Shared view lifecycle: mount lit-html into `#main`, run an async
 * data loader, and return the cleanup function.
 *
 * - Mounts `renderFn` immediately (user sees loading skeleton).
 * - Awaits `loadData()` which should populate module-local state.
 * - Calls `requestUpdate()` to re-render with the fetched data.
 * - If `loadData()` throws, `setError(msg)` is called with the
 *   error message so the re-render shows the error state.
 * - Returns the cleanup function that tears down the lit-html container.
 */
export async function createView(
  renderFn: () => TemplateResult,
  loadData: () => Promise<void>,
  setError?: (msg: string) => void,
): Promise<(() => void) | void> {
  const main = document.getElementById("main");
  if (!main) return;
  const cleanup = mountView(main, renderFn);
  try {
    await loadData();
  } catch (e: unknown) {
    if (setError) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }
  requestUpdate();
  return cleanup;
}
