// components/bulk-actions.ts — render the "X selected" bar that
// shows up above a multi-select table when at least one row is
// checked. The caller wires the button onclicks.

import { escapeHtml } from "../lib/escape.js";

export interface BulkActionsBarProps {
  count: number;
  actions: string;
}

export function bulkActionsBar(props: BulkActionsBarProps): string {
  return `
    <div class="bulk-actions-bar">
      <span><strong>${escapeHtml(String(props.count))}</strong> selected</span>
      ${props.actions}
    </div>
  `;
}
