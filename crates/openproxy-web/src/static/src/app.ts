// src/app.ts — application entrypoint. Boots the theme, mounts
// the shell, installs the data-action dispatcher, starts the
// background poll, and navigates to the current hash.
//
// Per spec §3 + §13.8 there are no `window.foo = fn` global
// bridges and no inline `onclick="window.foo()"` handlers. A
// single document-level listener reads `data-action` + `data-arg-N`
// from each event target and dispatches via the HANDLERS map in
// handlers/registry.ts. See that file for the conventions.

import { bootstrapTheme } from "./state/theme.js";
import { mountShell } from "./components/shell.js";
import { loadSidebarCollapsedFromStorage } from "./components/sidebar.js";
import { startBgPoll } from "./state/bg-poll.js";
import { installRouter, navigate } from "./state/router.js";
import { HANDLERS, collectArgs } from "./handlers/registry.js";
import { state } from "./state/index.js";
import { logsGoPage } from "./views/logs.js";

// Click / change / submit shim. Looks for the closest ancestor
// carrying `data-action` and dispatches to HANDLERS[action]
// passing the collected data-arg-N values plus the event.
function dispatchFromElement(el: HTMLElement, event: Event, isSubmit = false): void {
  const action = el.dataset["action"];
  if (!action) return;
  const fn = HANDLERS[action];
  if (typeof fn !== "function") {
    console.warn("[data-action] no handler for", action);
    return;
  }
  const args: unknown[] = collectArgs(el);
  if (isSubmit) event.preventDefault();
  try {
    // The handlers' positional contracts are documented in
    // handlers/registry.ts; the shim only knows how to collect
    // the args, not how to type-check them.
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
  // Don't re-dispatch a click on a form's submit button — let the
  // `submit` listener below handle the form. Otherwise the button
  // would fire BOTH its own data-action AND bubble to the form.
  if (target.matches('button[type="submit"], input[type="submit"]')) return;
  dispatchFromElement(el, e, false);
});
document.addEventListener("change", (e: Event) => {
  const target = e.target;
  if (!(target instanceof Element)) return;
  const el = target.closest("[data-action]");
  if (!(el instanceof HTMLElement)) return;
  // If the changed element is INSIDE a form that owns a submit
  // handler, skip — only the form-level submit should fire that
  // handler. Without this, every select/input change inside a
  // modal would call `new FormData(thisInput)` and throw.
  if (el.tagName === "FORM" && el.dataset["action"]) return;
  dispatchFromElement(el, e, false);
});
// `input` is fired by text inputs on every keystroke. The old
// monolithic app.js used `oninput="updateProviderFilter(...)"`
// for the search box, so a user typing would see the table
// filter live. The change listener only fires on blur/enter,
// so without this the search box feels broken. We dispatch via
// the same shim and let the handler read e.target.value.
document.addEventListener("input", (e: Event) => {
  const target = e.target;
  if (!(target instanceof Element)) return;
  const el = target.closest("[data-action]");
  if (!(el instanceof HTMLElement)) return;
  // Skip if the event landed on a form ancestor — keystrokes
  // bubble up to the form before bubbling to whatever owns the
  // data-action, and form-level submit handlers do `new FormData`
  // on the target. They would explode on every keystroke.
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

bootstrapTheme();
// Hydrate the sidebar collapse flag from localStorage before the
// shell mounts, so the first renderSidebar() call already reflects
// the persisted user choice.
loadSidebarCollapsedFromStorage();
mountShell();
installRouter();
startBgPoll();
navigate();

// Expose the global `state` for the e2e suite (and operator
// debugging in the browser console). The dashboard is an internal
// admin tool — no public auth boundary is crossed by exposing
// the in-memory state object. The e2e tests at
// `tests/e2e/live-logs-retry.spec.ts` rely on this hook to
// inject synthetic `StageEvent`s and assert the per-attempt
// stage isolation introduced in the
// `fix(web): live-logs view — isolate per-attempt stage to its
// own row` gate.
declare global {
  interface Window {
    __openproxyState: typeof state;
    __openproxyLogsGoPage: typeof logsGoPage;
  }
}
window.__openproxyState = state;
window.__openproxyLogsGoPage = logsGoPage;
