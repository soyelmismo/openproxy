// components/render-op-modal.ts — reusable render function for operation modals
// (create/edit/delete). Plain lit-html template + DOM handle, not a Web Component:
// every consumer already renders via `render(template, container)`, so a LitElement
// would add ~30KB for no benefit (REFACTOR_SPEC.md Q7).
//
// Returns `{ el, close }`: `el` is the wrapper `<div>` appended to `#modal-root` (or
// `document.body`) that form-submit handlers can `remove()`, and `close()` tears down
// the wrapper, drops the global keydown listener, fires `onClose`, and is idempotent.
//
// Contract (REFACTOR_SPEC §4.1): focus trap on Tab/Shift+Tab; Escape and backdrop click
// close the modal; `role="dialog"` + `aria-modal="true"` + labelled header;
// `prefers-reduced-motion` handled by the existing `.modal` CSS animation rule.

import { html, render, type TemplateResult } from "lit-html";
import { ensureModalRoot } from "../lib/ui-utils.js";

export interface OpModalProps {
  title: string;
  body: TemplateResult;
  actions?: TemplateResult;
  danger?: boolean;
  onClose?: () => void;
}

/** Handle to a mounted modal. */
export interface OpModalHandle {
  /** The wrapper `<div>` containing the rendered `.modal-bg`. */
  el: HTMLElement;
  /** Tear down: remove DOM, drop the keydown listener, fire `onClose`. Idempotent. */
  close: () => void;
}

/** Render a modal into `#modal-root` and return a handle. Callers pass the returned `el`
 *  to their submit handler and call `el.remove()` (or `close()`) — the same wrapper-div
 *  pattern the rest of the dashboard uses; this version also wires escape key, backdrop
 *  click and focus trap. */
export function renderOpModal(props: OpModalProps): OpModalHandle {
  const { title, body, actions, danger = false, onClose } = props;

  const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const root = ensureModalRoot();
  const wrapper = document.createElement("div");
  root.appendChild(wrapper);

  const titleId = "op-modal-title";

  let closed = false;
  const close = (): void => {
    if (closed) return;
    closed = true;
    window.removeEventListener("keydown", onKeydown, true);
    render(html``, wrapper);
    wrapper.remove();
    if (opener && opener.isConnected) opener.focus();
    if (onClose) onClose();
  };

  const onKeydown = (e: KeyboardEvent): void => {
    if (e.key === "Escape") {
      e.stopPropagation();
      close();
      return;
    }
    if (e.key === "Tab") trapFocus(e);
  };

  const template: TemplateResult = html`
    <div
      class="modal-bg"
      role="dialog"
      aria-modal="true"
      aria-labelledby=${titleId}
      @click=${(e: Event) => {
        // Backdrop click only — `.modal` stops propagation in its own @click below.
        if (e.target === e.currentTarget) close();
      }}
    >
      <div class="modal" @click=${(e: Event) => e.stopPropagation()}>
        <div class="modal-header">
          <h2 id=${titleId}>${title}</h2>
          <button
            type="button"
            class="close-btn"
            aria-label="Close"
            @click=${() => close()}
          >&times;</button>
        </div>
        <div class="modal-body">${body}</div>
        ${actions
          ? html`<div class="modal-footer ${danger ? "danger" : ""}">${actions}</div>`
          : html``}
      </div>
    </div>
  `;

  render(template, wrapper);
  window.addEventListener("keydown", onKeydown, true);

  // Focus the first focusable so keyboard users land on a real control, not the backdrop.
  const firstFocusable = wrapper.querySelector<HTMLElement>(
    'input, select, textarea, button, a[href], [tabindex]:not([tabindex="-1"])',
  );
  if (firstFocusable) firstFocusable.focus();

  return { el: wrapper, close };
}

/** Focus trap: Tab/Shift+Tab cycle within the modal. The focusable list is resolved per
 *  event because the body (e.g. the key modal's dynamic scopes) can change the candidate
 *  set between keystrokes. */
function trapFocus(e: KeyboardEvent): void {
  const modal = (e.currentTarget as Window | null) ?? null;
  // `wrapper` isn't reachable from here (listener is bound to `window`), so walk up from
  // `document.activeElement` to the nearest `.modal-bg` — the modal being interacted with.
  const active = document.activeElement;
  const modalBg = active instanceof Element ? active.closest(".modal-bg") : null;
  const root = modalBg ?? document.querySelector(".modal-bg");
  if (!root) return;
  void modal;
  // No `offsetParent !== null` visibility filter: jsdom (where most dashboard unit tests
  // run) has no layout, so every `offsetParent` is null. The CSS `:disabled`/`:hidden`
  // selectors are also closer to user-visible truth.
  const focusables = Array.from(
    root.querySelectorAll<HTMLElement>(
      'a[href], button:not([disabled]), input:not([disabled]):not([type="hidden"]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])',
    ),
  );
  if (focusables.length === 0) {
    e.preventDefault();
    return;
  }
  const first = focusables[0];
  const last = focusables[focusables.length - 1];
  if (!first || !last) return;
  const goingBack = e.shiftKey;
  if (goingBack && document.activeElement === first) {
    e.preventDefault();
    last.focus();
  } else if (!goingBack && document.activeElement === last) {
    e.preventDefault();
    first.focus();
  }
}
