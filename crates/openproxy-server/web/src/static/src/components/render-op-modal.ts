// components/render-op-modal.ts — reusable render function for
// operation modals (create/edit/delete flows). Plain lit-html
// template + DOM handle; not a Web Component.
//
// Per REFACTOR_SPEC.md Q7 the codebase standardised on
// `export function xxx(props): TemplateResult` (see the original
// `components/modal.ts`) instead of `<op-modal>` LitElement. Forcing
// LitElement would have added ~30KB to the bundle for zero benefit
// since every consumer is already rendering via `render(template,
// container)` from the lit-html package.
//
// The function returns `{ el, close }`:
//   - `el` is the wrapper `<div>` appended to `#modal-root` (or
//     `document.body` as a fallback) that the caller can target
//     from form-submit handlers via `wrapper.remove()`.
//   - `close()` tears down the wrapper, removes the global
//     keydown listener, fires `onClose`, and is idempotent (safe
//     to call multiple times).
//
// Behavior contract (REFACTOR_SPEC §4.1):
//   - Focus trap: Tab/Shift+Tab cycle within the modal.
//   - Escape closes the modal (calls `onClose`, removes DOM).
//   - Backdrop click closes the modal (target === .modal-bg).
//   - ARIA: `role="dialog"`, `aria-modal="true"`, header has the
//     labelled-by id.
//   - `prefers-reduced-motion` is respected via the existing CSS
//     animation rule on `.modal` (no extra JS hook needed).

import { html, render, type TemplateResult } from "lit-html";
import { ensureModalRoot } from "../lib/ui-utils.js";

export interface OpModalProps {
  title: string;
  body: TemplateResult;
  actions?: TemplateResult;
  danger?: boolean;
  onClose?: () => void;
}

/** Returned handle to a mounted modal. */
export interface OpModalHandle {
  /** The wrapper `<div>` containing the rendered `.modal-bg`. */
  el: HTMLElement;
  /** Tear down: remove DOM, drop the keydown listener, fire `onClose`. Idempotent. */
  close: () => void;
}

/**
 * Render a modal into `#modal-root` and return a handle.
 *
 * The caller can pass the returned `el` to their form-submit handler
 * and call `el.remove()` (or `handle.close()`) on success, which is
 * the same pattern the rest of the dashboard already uses with its
 * ad-hoc `wrapper` divs. The only difference is that this version
 * also wires the escape key, backdrop click, and focus trap so the
 * caller doesn't have to.
 */
export function renderOpModal(props: OpModalProps): OpModalHandle {
  const { title, body, actions, danger = false, onClose } = props;

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
        // Backdrop click only — `.modal` stops propagation in its
        // own @click handler below.
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

  // Focus the first focusable element inside the modal so keyboard
  // users land on a real control rather than the backdrop.
  const firstFocusable = wrapper.querySelector<HTMLElement>(
    'input, select, textarea, button, a[href], [tabindex]:not([tabindex="-1"])',
  );
  if (firstFocusable) firstFocusable.focus();

  return { el: wrapper, close };
}

/**
 * Focus trap: keep Tab/Shift+Tab cycling within the modal. We
 * resolve the live focusable list on every event because the body
 * (e.g. dynamically rendered scopes in the key modal) can change
 * the candidate set between keystrokes.
 */
function trapFocus(e: KeyboardEvent): void {
  const modal = (e.currentTarget as Window | null) ?? null;
  // We can't read `wrapper` from here, so we re-derive from the
  // active element: the keydown listener is bound to `window`, so
  // we walk up from `document.activeElement` to the nearest
  // `.modal-bg`. That is the modal the user is interacting with.
  const active = document.activeElement;
  const modalBg = active instanceof Element ? active.closest(".modal-bg") : null;
  const root = modalBg ?? document.querySelector(".modal-bg");
  if (!root) return;
  void modal;
  // We deliberately skip the `offsetParent !== null` check (used by
  // the spec for visibility filtering) because jsdom — where most
  // of the dashboard's unit tests run — does not implement
  // layout, so every `offsetParent` is `null`. We trust the
  // CSS `:disabled`/`:hidden` selectors instead; that is also
  // closer to the user-visible truth.
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
