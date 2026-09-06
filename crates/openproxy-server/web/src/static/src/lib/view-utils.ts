// lib/view-utils.ts — shared view lifecycle helpers.
//
// Two patterns are supported:
//
// 1. GENERIC (Q15): createView<T>({ loader, render, ... }) — handles
//    the full lifecycle: loading → success/empty/error, with optional
//    post-load hooks and cleanup. Suitable for one-shot data-fetching
//    views (combos, config, keys, proxies, etc.).
//
// 2. LEGACY: createView(renderFn, loadData, setError?) — positional-
//    args form retained for backward compatibility. Existing callers
//    (keys, key-usage, proxy-sources, proxies, playground, config)
//    continue to use this form unchanged.
//
// Views with complex lifecycle (polling loops, WebSocket subscriptions,
// chart instances, multi-phase background refresh) should NOT use
// createView — handle them manually. Candidates for future migration
// once their lifecycle is simplified:
//   - debug-logs.ts — chained-setTimeout polling with epoch-based
//     cancellation; receives a container param instead of #main
//   - notifications.ts — WS subscriptions, cursor pagination, DnD
//     overlay lifecycle
//   - analytics.ts — multi-fetch, uPlot chart create/destroy cycle
//   - providers.ts — dual-mode (grid + detail), background refresh
//   - home.ts — live-store WebSocket, uPlot sparklines + charts
//   - logs.ts — WebSocket message handler, recording state toggle

import { html, type TemplateResult } from "lit-html";
import { mountView, requestUpdate } from "../state/reactive.js";

/**
 * Options for the generic `createView<T>` overload (Q15).
 *
 * Handles the full loading → success/empty/error lifecycle.
 * Returns a cleanup function that tears down the lit-html container
 * and runs any custom cleanup provided via options.
 *
 * @template T — The data type returned by the loader.
 */
export interface ViewFactoryOptions<T> {
  /** Container to mount into. Defaults to `document.getElementById("main")`. */
  container?: HTMLElement | null;
  /** Async data loader. Called after the loading skeleton is shown. */
  loader: () => Promise<T>;
  /** Render function called with the loaded data on success. */
  render: (data: T) => TemplateResult;
  /** Shown while the loader is in flight. Default: `<div class="loading">Loading...</div>`. */
  loading?: () => TemplateResult;
  /** Returns true if data should trigger the empty phase.
   *  Default: `Array.isArray(data) && data.length === 0`. */
  empty?: (data: T) => boolean;
  /** Template for the empty state. */
  emptyMessage?: () => TemplateResult;
  /** Custom error rendering. Default: `<div class="banner banner-error">`. */
  error?: (err: unknown) => TemplateResult;
  /** Called after a successful load, before the first render with data. */
  onLoaded?: (data: T) => void;
  /** Called on cleanup/teardown (view unmount). */
  cleanup?: () => void;
}

const DEFAULT_LOADING = (): TemplateResult =>
  html`<div class="loading">Loading...</div>`;
const DEFAULT_EMPTY = (): TemplateResult =>
  html`<p class="empty">No data.</p>`;
const DEFAULT_ERROR = (err: unknown): TemplateResult =>
  html`<div class="banner banner-error">${err instanceof Error ? err.message : String(err)}</div>`;

/**
 * Generic view lifecycle factory (Q15).
 *
 * Mounts a loading skeleton immediately, runs the async loader, then
 * transitions to the success, empty, or error phase. Returns a cleanup
 * function that tears down the lit-html container and runs any custom
 * cleanup provided via options.
 */
export async function createView<T>(
  options: ViewFactoryOptions<T>,
): Promise<() => void>;

/**
 * Legacy positional-args overload (backward compatible).
 *
 * Mounts `renderFn` immediately (user sees loading skeleton).
 * Awaits `loadData()` which should populate module-local state.
 * Calls `requestUpdate()` to re-render with the fetched data.
 * If `loadData()` throws, `setError(msg)` is called with the error
 * message so the re-render shows the error state.
 * Returns the cleanup function that tears down the lit-html container.
 */
export async function createView(
  renderFn: () => TemplateResult,
  loadData: () => Promise<void>,
  setError?: (msg: string) => void,
): Promise<(() => void) | void>;

export async function createView<T>(
  optionsOrRenderFn: ViewFactoryOptions<T> | (() => TemplateResult),
  loadData?: () => Promise<void>,
  setError?: (msg: string) => void,
): Promise<(() => void) | void> {
  // --- Generic pattern (options object) ---
  if (typeof optionsOrRenderFn !== "function") {
    const opts = optionsOrRenderFn;
    const container = opts.container ?? document.getElementById("main");
    if (!container) return () => {};

    let phase: "loading" | "success" | "error" | "empty" = "loading";
    let data!: T;
    let loadErr: unknown;

    const renderPhase = (): TemplateResult => {
      switch (phase) {
        case "loading":
          return (opts.loading ?? DEFAULT_LOADING)();
        case "error":
          return (opts.error ?? DEFAULT_ERROR)(loadErr);
        case "empty":
          return (opts.emptyMessage ?? DEFAULT_EMPTY)();
        case "success":
          return opts.render(data);
      }
    };

    const cleanupView = mountView(container, renderPhase);
    try {
      data = await opts.loader();
      const isEmpty =
        opts.empty !== undefined
          ? opts.empty(data)
          : Array.isArray(data) && data.length === 0;
      if (isEmpty) {
        phase = "empty";
      } else {
        opts.onLoaded?.(data);
        phase = "success";
      }
    } catch (e: unknown) {
      loadErr = e;
      phase = "error";
    }
    requestUpdate();
    return () => {
      opts.cleanup?.();
      cleanupView();
    };
  }

  // --- Legacy pattern (positional args) ---
  const renderFn = optionsOrRenderFn;
  const main = document.getElementById("main");
  if (!main) return () => {};
  const cleanup = mountView(main, renderFn);
  try {
    await loadData!();
  } catch (e: unknown) {
    if (setError) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }
  requestUpdate();
  return cleanup;
}
