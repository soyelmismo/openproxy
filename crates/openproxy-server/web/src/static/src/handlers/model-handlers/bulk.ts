// handlers/model-handlers/bulk.ts — bulk enable / disable / test /
// delete handlers operating on `state.selectedModels`.

import { state } from "../../state/index.js";
import { api } from "../../state/api.js";
import { html, render } from "lit-html";
import { syncModelRowActive, updateFilterTabCounts, syncSelectAllCheckbox } from "../../components/model-table.js";
import { statusPillClass } from "../../lib/constants.js";
import type { Model } from "../../lib/types/api.js";
import { requestUpdate } from "../../state/reactive.js";
import { showConfirm } from "../../lib/show-confirm.js";
import { TestResult, updateBulkBar } from "./row.js";

// ===== Bulk enable / disable / test / delete =====

async function bulkSetSelected(_providerId: string, active: boolean): Promise<void> {
  const ids = Array.from(state.selectedModels);
  if (ids.length === 0) return;
  if (!(await showConfirm({
    title: active ? "Enable models" : "Disable models",
    message: `${active ? "Enable" : "Disable"} ${ids.length} models?`,
    confirmLabel: active ? "Enable" : "Disable",
  }))) return;
  // Per-row toggle in parallel: each toggle is its own atomic
  // UPDATE on the server. The previous bulk-toggle endpoint
  // applied to *all* non-custom rows of the provider, which is
  // exactly the over-broad behavior the per-row selection is
  // meant to escape.
  await Promise.all(ids.map((rowId) =>
    api("/models/" + rowId + "/toggle", {
      method: "POST",
      body: JSON.stringify({ active }),
    }).catch((err: unknown) => console.error("Failed toggle", rowId, err))
  ));
  state.models = await api("/models") as Model[];
  // Targeted DOM patch — for each toggled row, sync the
  // active-state UI in place. Clear the selection (uncheck all,
  // remove `selected` classes, hide the bulk bar, reset master
  // checkbox). We do NOT call requestUpdate() — a full
  // rebuild would close any open `<select>` and steal focus from
  // the search input. Mirrors patchComboField in combo-handlers.ts.
  for (const rowId of ids) {
    const rid = Number(rowId);
    if (!Number.isFinite(rid)) continue;
    const m = (state.models || []).find((x) => x.row_id === rid);
    if (m) syncModelRowActive(rid, m.active);
  }
  state.selectedModels.clear();
  document.querySelectorAll<HTMLInputElement>(
    '#models-tbody input[type="checkbox"]'
  ).forEach((cb) => { cb.checked = false; });
  document.querySelectorAll("tr[id^='model-row-'].selected").forEach((row) => {
    row.classList.remove("selected");
  });
  updateBulkBar();
  syncSelectAllCheckbox([]);
  // Refresh the (All / Active / Inactive) counts on the filter
  // tabs so the totals reflect the new state.
  const ctx = state.currentView && state.currentView.context;
  if (ctx) {
    const allProviderModels = (state.models || []).filter((mm) => mm.provider_id === ctx);
    updateFilterTabCounts(ctx, allProviderModels);
  }
}

export function bulkEnableSelected(_providerId: string): Promise<void> { return bulkSetSelected(_providerId, true); }
export function bulkDisableSelected(_providerId: string): Promise<void> { return bulkSetSelected(_providerId, false); }

export async function bulkTestSelected(_providerId: string): Promise<void> {
  const ids = Array.from(state.selectedModels);
  if (ids.length === 0) return;
  if (!(await showConfirm({
    title: "Test models",
    message: `Test ${ids.length} models sequentially?`,
    confirmLabel: "Test",
  }))) return;
  for (const rowId of ids) {
    try {
      const btn = document.getElementById(`test-btn-${rowId}`) as HTMLButtonElement | null;
      if (btn) {
        btn.disabled = true;
        btn.textContent = "Testing...";
      }
      const result = (await api(`/models/${rowId}/test`, { method: "POST" })) as TestResult;
      const row = document.getElementById(`model-row-${rowId}`);
      if (row) {
        const cell = row.querySelector(".last-test-cell");
        if (cell instanceof HTMLElement) {
          render(html`<span class="status-pill ${statusPillClass(result.status)}">${result.status}</span> <small>${result.elapsed_ms}ms</small>`, cell);
        }
      }
      if (btn) {
        if (result.status >= 200 && result.status < 300) {
          btn.textContent = "✓";
          btn.style.background = "#a6e3a1";
        } else {
          btn.textContent = "✗ " + result.status;
          btn.style.background = "#f38ba8";
        }
        setTimeout(() => {
          btn.textContent = "Test";
          btn.style.background = "";
          btn.disabled = false;
        }, 1500);
      }
    } catch (err: unknown) {
      console.error("Test failed", rowId, err);
    }
  }
  // Refresh the models cache so the background poll is a no-op
  // and the next render shows the up-to-date last_test_* columns.
  state.models = await api("/models") as Model[];
}

export async function bulkDeleteSelected(_providerId: string): Promise<void> {
  const ids = Array.from(state.selectedModels);
  if (ids.length === 0) return;
  if (!(await showConfirm({
    title: "Delete models",
    message: `Delete ${ids.length} models? This cannot be undone.`,
    danger: true,
    confirmLabel: "Delete",
  }))) return;
  await Promise.all(ids.map((rowId) =>
    api("/models/" + rowId, { method: "DELETE" })
      .catch((err: unknown) => console.error("Failed delete", rowId, err))
  ));
  state.models = await api("/models") as Model[];
  state.selectedModels.clear();
  requestUpdate();
}
