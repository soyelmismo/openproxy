// components/modal.ts — small helper to render a backdrop + dialog.
// Clicking the backdrop closes it (data-action="closeModalBg");
// clicking inside the dialog is left to its own data-action or
// plain event handlers (we don't add an inline stopPropagation
// because the central click shim dispatches once per element via
// closest() and would still work even without it).
//
// Per spec §3 + §13.8 we do not use inline `onclick="window.X()"`
// handlers. There is no window.__closeModal anymore: the
// generic closeModalBg action (in handlers/registry.js) removes
// the closest .modal-bg from the click target.
//
// Migrated to lit-html: returns a `TemplateResult`. `body` and
// `footer` are raw HTML strings from callers, so they are
// embedded via `unsafeHTML`. The `id` and `title` go through
// normal `${...}` interpolation. The `id` attribute is omitted
// entirely when not provided (via lit-html's `nothing` sentinel)
// so we don't emit `id=""` on the backdrop.

import { html, nothing, type TemplateResult } from "lit-html";
import { unsafeHTML } from "lit-html/directives/unsafe-html.js";

export interface ModalProps {
  id?: string;
  title?: string;
  body: string;
  footer?: string;
  onClose?: "navigate" | string;
}

export function modal(props: ModalProps = { body: "" }): TemplateResult {
  const { id, title, body, footer, onClose } = props;
  // The `onClose` callback used to be a string like "navigate()"
  // inlined into onclick. We keep the parameter for API compatibility
  // but translate it into a data-action="closeAndNavigate" hint on
  // the X button, falling back to closeModalBg. The data-action
  // for the backdrop is closeModalBg.
  const closeAction: string = onClose === "navigate" ? "closeAndNavigate" : "closeModalBg";
  return html`
    <div
      class="modal-bg"
      id=${id ? id : nothing}
      data-action="closeModalBg"
      data-arg1="self"
    >
      <div class="modal">
        <div class="modal-header">
          <h2>${title || ""}</h2>
          <button type="button" class="close-btn" data-action="${closeAction}" aria-label="Close">
            &times;
          </button>
        </div>
        ${unsafeHTML(body)}
        ${footer ? html`<div class="modal-footer">${unsafeHTML(footer)}</div>` : null}
      </div>
    </div>
  `;
}
