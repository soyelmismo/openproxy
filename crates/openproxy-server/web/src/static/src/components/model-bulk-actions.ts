// components/model-bulk-actions.ts — the "N selected" bar that
// appears above the models table whenever the user has at least
// one checkbox ticked. The bar re-renders on every model-handler
// state change (via the parent view's re-render), so it can be a
// pure function of `state.selectedModels`.
//
// Migrated to lit-html: returns a `TemplateResult` and wires the
// button click handlers directly via `@click` (no more
// `data-action` / registry dispatch). The handlers live in
// `handlers/model-handlers.ts`; importing them creates a module
// cycle, but the cycle is safe because the imported bindings are
// only referenced at click time (runtime), never at module
// top-level. The "0 selected" count is patched in place by
// `updateBulkBar` in model-handlers.ts (same as before).

import { html, type TemplateResult } from "lit-html";
import {
  bulkEnableSelected,
  bulkDisableSelected,
  bulkTestSelected,
  bulkDeleteSelected,
  clearModelSelection,
} from "../handlers/model-handlers.js";

export function renderBulkActionsBar(providerId: string): TemplateResult {
  return html`
    <div class="bulk-actions-bar">
      <span><strong>0</strong> selected</span>
      <button @click=${() => bulkEnableSelected(providerId)}>Enable selected</button>
      <button @click=${() => bulkDisableSelected(providerId)}>Disable selected</button>
      <button @click=${() => bulkTestSelected(providerId)}>Test selected</button>
      <button class="danger" @click=${() => bulkDeleteSelected(providerId)}>Delete selected</button>
      <button class="link" @click=${clearModelSelection}>Clear selection</button>
    </div>
  `;
}
