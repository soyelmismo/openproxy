// components/modal.js — small helper to render a backdrop + dialog.
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

import { escapeHtml, escapeAttr } from "../lib/escape.js";

export function modal({ id, title, body, footer, onClose } = {}) {
  const safeId = id ? ` id="${escapeAttr(id)}"` : "";
  // The `onClose` callback used to be a string like "navigate()"
  // inlined into onclick. We keep the parameter for API compatibility
  // but translate it into a data-action="closeAndNavigate" hint on
  // the X button, falling back to closeModalBg. The data-action
  // for the backdrop is closeModalBg.
  const closeAction = onClose === "navigate" ? "closeAndNavigate" : "closeModalBg";
  return `
    <div class="modal-bg"${safeId} data-action="closeModalBg" data-arg1="self">
      <div class="modal">
        <div class="modal-header">
          <h2>${escapeHtml(title || "")}</h2>
          <button type="button" class="close-btn" data-action="${closeAction}" aria-label="Close">&times;</button>
        </div>
        ${body}
        ${footer ? `<div class="modal-footer">${footer}</div>` : ""}
      </div>
    </div>
  `;
}
