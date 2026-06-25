// components/bulk-actions.ts — render the "X selected" bar that
// shows up above a multi-select table when at least one row is
// checked. The caller wires the button onclicks.
//
// Migrated to lit-html: returns a `TemplateResult`. The count is
// rendered via `${...}` (auto-escaped). `props.actions` is a raw
// HTML string built by the caller, so it is embedded via
// `unsafeHTML` to keep the buttons as elements.

import { html, type TemplateResult } from "lit-html";
import { unsafeHTML } from "lit-html/directives/unsafe-html.js";

export interface BulkActionsBarProps {
  count: number;
  /** Raw HTML string of action buttons. */
  actions: string;
}

export function bulkActionsBar(props: BulkActionsBarProps): TemplateResult {
  return html`
    <div class="bulk-actions-bar">
      <span><strong>${props.count}</strong> selected</span>
      ${unsafeHTML(props.actions)}
    </div>
  `;
}
