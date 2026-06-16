// src/app.js — application entrypoint. Boots the theme, mounts
// the shell, installs the data-action dispatcher, starts the
// background poll, and navigates to the current hash.
//
// Per spec §3 + §13.8 there are no `window.foo = fn` global
// bridges and no inline `onclick="window.foo()"` handlers. A
// single document-level listener reads `data-action` + `data-arg-N`
// from each event target and dispatches via the HANDLERS map in
// handlers/registry.js. See that file for the conventions.

import { bootstrapTheme } from "./state/theme.js";
import { mountShell } from "./components/shell.js";
import { loadSidebarCollapsedFromStorage } from "./components/sidebar.js";
import { startBgPoll } from "./state/bg-poll.js";
import { installRouter, navigate } from "./state/router.js";
import { HANDLERS, collectArgs } from "./handlers/registry.js";

// Click / change / submit shim. Looks for the closest ancestor
// carrying `data-action` and dispatches to HANDLERS[action]
// passing the collected data-arg-N values plus the event.
function dispatchFromElement(el, event, isSubmit = false) {
  if (!el || !el.dataset || !el.dataset.action) return;
  const action = el.dataset.action;
  const fn = HANDLERS[action];
  if (typeof fn !== "function") {
    console.warn("[data-action] no handler for", action);
    return;
  }
  const args = collectArgs(el);
  if (isSubmit) event.preventDefault();
  try {
    fn(...args, event);
  } catch (err) {
    console.error("[data-action] handler threw for", action, err);
  }
}

document.addEventListener("click", (e) => {
  const el = e.target.closest("[data-action]");
  if (!el) return;
  // Don't re-dispatch a click on a form's submit button — let the
  // `submit` listener below handle the form. Otherwise the button
  // would fire BOTH its own data-action AND bubble to the form.
  if (e.target.matches('button[type="submit"], input[type="submit"]')) return;
  dispatchFromElement(el, e, false);
});
document.addEventListener("change", (e) => {
  const el = e.target.closest("[data-action]");
  if (!el) return;
  // If the changed element is INSIDE a form that owns a submit
  // handler, skip — only the form-level submit should fire that
  // handler. Without this, every select/input change inside a
  // modal would call `new FormData(thisInput)` and throw.
  if (el.tagName === "FORM" && el.dataset.action) return;
  dispatchFromElement(el, e, false);
});
// `input` is fired by text inputs on every keystroke. The old
// monolithic app.js used `oninput="updateProviderFilter(...)"`
// for the search box, so a user typing would see the table
// filter live. The change listener only fires on blur/enter,
// so without this the search box feels broken. We dispatch via
// the same shim and let the handler read e.target.value.
document.addEventListener("input", (e) => {
  const el = e.target.closest("[data-action]");
  if (!el) return;
  // Skip if the event landed on a form ancestor — keystrokes
  // bubble up to the form before bubbling to whatever owns the
  // data-action, and form-level submit handlers do `new FormData`
  // on the target. They would explode on every keystroke.
  if (el.tagName === "FORM" && el.dataset.action) return;
  dispatchFromElement(el, e, false);
});
document.addEventListener("submit", (e) => {
  const el = e.target.closest("[data-action]");
  if (!el) return;
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
navigate(window.location.hash);
