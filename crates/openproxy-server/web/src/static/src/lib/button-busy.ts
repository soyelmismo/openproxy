// lib/button-busy.ts — shared execution feedback for async button actions.
//
// One contract, used imperatively (handler code) and declaratively (lit templates):
//
//   button[aria-busy="true"]        busy state; styles/forms.css must NOT dim the button
//   button[data-button-busy="true"] same state as a data hook (optional styling/debug)
//   .button-busy-spinner            inline spinner wrapper (animated by forms.css)
//   .button-busy-spinner-icon       the inline SVG itself (also carries `animate-spin`)
//   .button-busy-label              the "Processing…" text span
//
// `beginButtonBusy` replaces the button's innerHTML with spinner + label and restores the
// original markup byte-for-byte on release, so icon+text buttons survive untouched. It also
// disables the button (double-click guard) and is re-entrant: nested calls refcount and only
// the outermost release restores the DOM.
//
// Resolving the button: the event is always preferred (`submitter` → `currentTarget` → closest
// button from `target`). Lit nulls `currentTarget` after dispatch, so callers must capture the
// button synchronously before their first `await` — `resolveBusyButton(e)` does that for them.
// When a handler is invoked without an event (legacy call sites in views), a short-lived
// click tracker picks up the button the user just pressed; keyboard activation fires a click
// too, so Enter/Space on a button works the same way.

import { html, type TemplateResult } from "lit-html";

export interface ButtonBusyOptions {
  /** Text shown next to the spinner. Defaults to "Processing…". */
  label?: string;
}

export interface ButtonBusyHandle {
  readonly button: HTMLButtonElement | null;
  /** Idempotent; nested handles must all be released before the DOM is restored. */
  release: () => void;
}

interface BusyState {
  depth: number;
  html: string;
  disabled: boolean;
  ariaBusy: string | null;
}

const DEFAULT_LABEL = "Processing…";

// Keep the SVG inline: no icon dependency, no network request, currentColor-aware.
const SPINNER_HTML =
  '<span class="button-busy-spinner" aria-hidden="true">' +
  '<svg class="button-busy-spinner-icon animate-spin" width="14" height="14" viewBox="0 0 16 16" ' +
  'fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true">' +
  '<path d="M8 1.5a6.5 6.5 0 1 1-6.5 6.5"/></svg></span>';

const busyStates = new WeakMap<HTMLButtonElement, BusyState>();

// Last button activated by a real click/keyboard activation. Handlers called without an event
// (the legacy `@click=${() => handler(id)}` call sites) still get feedback; the freshness window
// keeps a programmatic call minutes later from stealing an unrelated button.
let lastActivated: { button: HTMLButtonElement; at: number } | null = null;
const TRACKER_TTL_MS = 1500;

if (typeof document !== "undefined") {
  document.addEventListener(
    "click",
    (e: Event) => {
      const target = e.target;
      if (!(target instanceof Element)) return;
      const button = target.closest("button");
      if (button instanceof HTMLButtonElement) {
        lastActivated = { button, at: Date.now() };
      }
    },
    true,
  );
}

/** Resolve the button an async action should render its busy state on. */
export function resolveBusyButton(e?: Event | null): HTMLButtonElement | null {
  if (e) {
    if (typeof SubmitEvent !== "undefined" && e instanceof SubmitEvent) {
      if (e.submitter instanceof HTMLButtonElement) return e.submitter;
    }
    const current = e.currentTarget;
    if (current instanceof HTMLButtonElement) return current;
    const target = e.target;
    if (target instanceof HTMLButtonElement) return target;
    if (target instanceof Element) {
      const closest = target.closest("button");
      if (closest instanceof HTMLButtonElement) return closest;
    }
    if (target instanceof HTMLFormElement) {
      const submit = target.querySelector<HTMLButtonElement>('button[type="submit"], input[type="submit"]');
      if (submit instanceof HTMLButtonElement) return submit;
    }
  }
  if (lastActivated && Date.now() - lastActivated.at <= TRACKER_TTL_MS && lastActivated.button.isConnected) {
    return lastActivated.button;
  }
  const active = typeof document !== "undefined" ? document.activeElement : null;
  return active instanceof HTMLButtonElement ? active : null;
}

/** Enter the busy state on `button`. Always call `release()` in a `finally`. */
export function beginButtonBusy(
  button: HTMLButtonElement | null,
  options: ButtonBusyOptions = {},
): ButtonBusyHandle {
  const handle: ButtonBusyHandle = { button, release: () => { /* no button → no-op */ } };
  if (!button) return handle;

  const existing = busyStates.get(button);
  if (existing) {
    existing.depth += 1;
  } else {
    busyStates.set(button, {
      depth: 1,
      html: button.innerHTML,
      disabled: button.disabled,
      ariaBusy: button.getAttribute("aria-busy"),
    });
    button.setAttribute("aria-busy", "true");
    button.setAttribute("data-button-busy", "true");
    button.disabled = true;
    button.innerHTML = SPINNER_HTML;
    const label = document.createElement("span");
    label.className = "button-busy-label";
    label.textContent = options.label || DEFAULT_LABEL;
    button.appendChild(label);
  }

  let released = false;
  handle.release = () => {
    if (released) return;
    released = true;
    const state = busyStates.get(button);
    if (!state) return;
    state.depth -= 1;
    if (state.depth > 0) return;
    busyStates.delete(button);
    button.innerHTML = state.html;
    button.disabled = state.disabled;
    if (state.ariaBusy === null) button.removeAttribute("aria-busy");
    else button.setAttribute("aria-busy", state.ariaBusy);
    button.removeAttribute("data-button-busy");
  };
  return handle;
}

/** Run `fn` with the button busy; the release happens on success and on throw. */
export async function withButtonBusy<T>(
  button: HTMLButtonElement | null,
  options: ButtonBusyOptions,
  fn: () => Promise<T>,
): Promise<T> {
  const handle = beginButtonBusy(button, options);
  try {
    return await fn();
  } finally {
    handle.release();
  }
}

/** Spinner markup for declarative (lit) call sites that own their own busy flag. */
export function busySpinnerTemplate(): TemplateResult {
  return html`<span class="button-busy-spinner" aria-hidden="true"
    ><svg
      class="button-busy-spinner-icon animate-spin"
      width="14"
      height="14"
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      stroke-width="1.8"
      stroke-linecap="round"
      aria-hidden="true"
    >
      <path d="M8 1.5a6.5 6.5 0 1 1-6.5 6.5" />
    </svg></span
  >`;
}
