// components/model-bulk-actions.ts — the "N selected" bar that
// appears above the models table whenever the user has at least
// one checkbox ticked. The bar re-renders on every model-handler
// state change (via the parent view's re-render), so it can be a
// pure function of `state.selectedModels`.
//
// Each button is wired through the central data-action shim in
// handlers/registry.js. No inline onclick, no global functions.

export function renderBulkActionsBar(providerId: string): string {
  return `
    <div class="bulk-actions-bar">
      <span><strong>0</strong> selected</span>
      <button data-action="bulkEnableSelected" data-arg1="${providerId}">Enable selected</button>
      <button data-action="bulkDisableSelected" data-arg1="${providerId}">Disable selected</button>
      <button data-action="bulkTestSelected" data-arg1="${providerId}">Test selected</button>
      <button class="danger" data-action="bulkDeleteSelected" data-arg1="${providerId}">Delete selected</button>
      <button class="link" data-action="clearModelSelection">Clear selection</button>
    </div>
  `;
}
