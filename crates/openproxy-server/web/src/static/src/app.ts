// src/app.ts — application entrypoint. Boots the theme, mounts the shell, installs the
// data-action dispatcher, starts the background poll and navigates to the current hash.
//
// Per spec §3 + §13.8 there are no `window.foo = fn` global bridges and no inline
// `onclick="window.foo()"` handlers: a single document-level listener reads `data-action` +
// `data-arg-N` off each event target and dispatches via the HANDLERS map in
// handlers/registry.ts (see that file for the conventions).

import { bootstrapTheme } from "./state/theme.js";
import { mountShell } from "./components/shell.js";
import { loadSidebarCollapsedFromStorage } from "./components/sidebar.js";
import { installMotionProvider } from "./components/motion-provider.js";
import { startBgPoll } from "./state/bg-poll.js";
import { installRouter, navigate } from "./state/router.js";
import { HANDLERS, collectArgs } from "./handlers/registry.js";
import { state } from "./state/index.js";
import { logsGoPage } from "./views/logs.js";
import { loadLang } from "./i18n/index.js";
import { liveLogsStore } from "./state/live-logs-store.js";
import { initModelsSync } from "./state/models-sync.js";
import { isPanelDocument } from "./state/workspace.js";
import { installPanelBridge } from "./components/workspace.js";

// Expose the global `state` for the e2e suite (and operator console debugging). The dashboard is an
// internal admin tool, so exposing the in-memory state object crosses no public auth boundary.
// `tests/e2e/live-logs-retry.spec.ts` relies on this hook to inject synthetic `StageEvent`s and
// assert the per-attempt stage isolation.
declare global {
  interface Window {
    __openproxyState: typeof state;
    __openproxyLogsGoPage: typeof logsGoPage;
    __liveLogsStore: typeof liveLogsStore;
  }
}
window.__openproxyState = state;
window.__openproxyLogsGoPage = logsGoPage;
window.__liveLogsStore = liveLogsStore;

// Click / change / submit shim: find the closest ancestor carrying `data-action` and dispatch to
// HANDLERS[action] with the collected data-arg-N values plus the event.
function dispatchFromElement(el: HTMLElement, event: Event, isSubmit = false): void {
  const action = el.dataset["action"];
  if (!action) return;
  const fn = (HANDLERS as Record<string, unknown>)[action];
  if (typeof fn !== "function") {
    console.warn("[data-action] no handler for", action);
    return;
  }
  const args: unknown[] = collectArgs(el);
  if (isSubmit) event.preventDefault();
  try {
    // The handlers' positional contracts are documented in handlers/registry.ts; the shim only
    // collects args, it cannot type-check them.
    fn(...args, event);
  } catch (err: unknown) {
    console.error("[data-action] handler threw for", action, err);
  }
}

document.addEventListener("click", (e: Event) => {
  const target = e.target;
  if (!(target instanceof Element)) return;
  const el = target.closest("[data-action]");
  if (!(el instanceof HTMLElement)) return;
  // Don't re-dispatch a click on a form's submit button — the `submit` listener below handles
  // the form. Otherwise the button would fire its own data-action AND bubble to the form.
  if (target.matches('button[type="submit"], input[type="submit"]')) return;
  dispatchFromElement(el, e, false);
});
document.addEventListener("change", (e: Event) => {
  const target = e.target;
  if (!(target instanceof Element)) return;
  const el = target.closest("[data-action]");
  if (!(el instanceof HTMLElement)) return;
  // If the changed element is INSIDE a form that owns a submit handler, skip — only the
  // form-level submit should fire. Otherwise every select/input change inside a modal would call
  // `new FormData(thisInput)` and throw.
  if (el.tagName === "FORM" && el.dataset["action"]) return;
  dispatchFromElement(el, e, false);
});
// `input` fires on text inputs per keystroke, which the live table filter relies on (the change
// listener only fires on blur/enter, so without this the search box feels broken). Dispatch via
// the same shim and let the handler read e.target.value.
document.addEventListener("input", (e: Event) => {
  const target = e.target;
  if (!(target instanceof Element)) return;
  const el = target.closest("[data-action]");
  if (!(el instanceof HTMLElement)) return;
  // Skip if the event landed on a form ancestor — keystrokes bubble to the form before bubbling
  // to whatever owns the data-action, and form-level submit handlers do `new FormData` on the
  // target, which would explode on every keystroke.
  if (el.tagName === "FORM" && el.dataset["action"]) return;
  dispatchFromElement(el, e, false);
});
document.addEventListener("submit", (e: Event) => {
  const target = e.target;
  if (!(target instanceof Element)) return;
  const el = target.closest("[data-action]");
  if (!(el instanceof HTMLElement)) return;
  dispatchFromElement(el, e, true);
});

// Boot sequence. `loadLang('en')` runs BEFORE the first render so any view calling
// `t('nav.dashboard')` finds a populated strings table. If the fetch fails (server down,
// /admin/i18n 5xx) `loadLang` swallows the error and `t()` returns the raw key — the fail-loud
// behavior we want in development — and the rest of the boot proceeds.
//
// The await is not an indefinite block: the fetch has no explicit timeout but the browser's
// default network timeout applies, and a hung connection still lets the boot proceed once it
// errors. A future hardening pass could add an `AbortController` + 2s timeout, but for an MVP
// this is fine — the i18n pack is ~5 KB and is served from the same origin as the SPA shell.
async function boot(): Promise<void> {
  installMotionProvider();
  await loadLang("en");
  bootstrapTheme();
  // Hydrate the sidebar collapse flag from localStorage before the shell mounts, so the first
  // renderSidebar() call already reflects the persisted choice.
  if (isPanelDocument()) installPanelBridge();
  else {
    loadSidebarCollapsedFromStorage();
    // The navigation rail is compact by default beside the contextual sidebar.
    try { if (localStorage.getItem("openproxy:sidebarCollapsed") === null) state.ui = { ...state.ui, sidebarCollapsed: true }; } catch { /* session preference only */ }
  }
  mountShell();
  installRouter();
  startBgPoll();
  initModelsSync();
  navigate();
}
void boot();

