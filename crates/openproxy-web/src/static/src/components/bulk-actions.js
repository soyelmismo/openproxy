// components/bulk-actions.js — render the "X selected" bar that
// shows up above a multi-select table when at least one row is
// checked. The caller wires the button onclicks.

import { escapeHtml } from "../lib/escape.js";

export function bulkActionsBar({ count, actions }) {
  return `
    <div class="bulk-actions-bar">
      <span><strong>${count}</strong> selected</span>
      ${actions}
    </div>
  `;
}
