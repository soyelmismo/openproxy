// handlers/model-handlers/bulk.ts — bulk enable / disable / test / delete over
// `state.selectedModels`.

import { state } from "../../state/index.js";
import { api } from "../../state/api.js";
import { html, render } from "lit-html";
import { syncModelRowActive, updateFilterTabCounts, syncSelectAllCheckbox } from "../../components/model-table.js";
import { statusPillClass } from "../../lib/constants.js";
import type { Model } from "../../lib/types/api.js";
import { requestUpdate } from "../../state/reactive.js";
import { showConfirm } from "../../lib/show-confirm.js";
import { flashButton, showApiError } from "../../lib/ui-utils.js";
import { beginButtonBusy, resolveBusyButton } from "../../lib/button-busy.js";
import { TestResult, updateBulkBar } from "./row.js";

// ── Bulk enable / disable / test / delete ──────────────────────────────────

async function bulkSetSelected(_providerId: string, active: boolean, e?: Event | null): Promise<void> {
  const ids = Array.from(state.selectedModels);
  if (ids.length === 0) return;
  if (!(await showConfirm({
    title: active ? "Enable models" : "Disable models",
    message: `${active ? "Enable" : "Disable"} ${ids.length} models?`,
    confirmLabel: active ? "Enable" : "Disable",
  }))) return;
  const busy = beginButtonBusy(resolveBusyButton(e), { label: active ? "Enabling…" : "Disabling…" });
  try {
    // Per-row toggles run in parallel; each is its own atomic server UPDATE. The old bulk
    // endpoint applied to *all* non-custom rows of the provider — exactly the over-broad behavior
    // per-row selection exists to avoid.
    await Promise.all(ids.map((rowId) =>
      api("/models/" + rowId + "/toggle", {
        method: "POST",
        body: JSON.stringify({ active }),
      }).catch((err: unknown) => console.error("Failed toggle", rowId, err))
    ));
    state.models = await api("/models") as Model[];
    // Targeted DOM patch: sync each toggled row's active-state UI, then clear the selection
    // (uncheck all, drop `selected` classes, hide the bulk bar, reset the master checkbox). No
    // requestUpdate() — a full rebuild closes an open <select> and steals search focus.
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
    // Refresh the (All / Active / Inactive) counts on the filter tabs.
    const ctx = state.currentView && state.currentView.context;
    if (ctx) {
      const allProviderModels = (state.models || []).filter((mm) => mm.provider_id === ctx);
      updateFilterTabCounts(ctx, allProviderModels);
    }
  } finally {
    busy.release();
  }
}

export function bulkEnableSelected(_providerId: string, e?: Event | null): Promise<void> {
  return bulkSetSelected(_providerId, true, e);
}
export function bulkDisableSelected(_providerId: string, e?: Event | null): Promise<void> {
  return bulkSetSelected(_providerId, false, e);
}

export async function bulkTestSelected(_providerId: string, e?: Event | null): Promise<void> {
  const ids = Array.from(state.selectedModels);
  if (ids.length === 0) return;
  if (!(await showConfirm({
    title: "Test models",
    message: `Test ${ids.length} models sequentially?`,
    confirmLabel: "Test",
  }))) return;
  const busy = beginButtonBusy(resolveBusyButton(e), { label: "Testing…" });
  try {
    for (const rowId of ids) {
      const btn = document.getElementById(`test-btn-${rowId}`) as HTMLButtonElement | null;
      const originalHtml = btn ? btn.innerHTML : "";
      const rowBusy = beginButtonBusy(btn, { label: "Testing…" });
      let flash: { text: string; color: string } | null = null;
      try {
        const result = (await api(`/models/${rowId}/test`, { method: "POST" })) as TestResult;
        const row = document.getElementById(`model-row-${rowId}`);
        if (row) {
          const cell = row.querySelector(".last-test-cell");
          if (cell instanceof HTMLElement) {
            render(html`<span class="status-pill ${statusPillClass(result.status)}">${result.status}</span> <small>${result.elapsed_ms}ms</small>`, cell);
          }
        }
        if (result.status >= 200 && result.status < 300) {
          flash = { text: "✓", color: "#a6e3a1" };
        } else {
          flash = { text: "✗ " + result.status, color: "#f38ba8" };
        }
      } catch (err: unknown) {
        console.error("Test failed", rowId, err);
        flash = { text: "✗", color: "#f38ba8" };
      } finally {
        // Always leave the row button usable: the old code only re-enabled it on the success
        // path, so one network error left it disabled on "Testing..." forever.
        rowBusy.release();
        if (btn) {
          if (flash) {
            flashButton(btn, flash.text, flash.color);
            btn.disabled = true;
            setTimeout(() => {
              btn.disabled = false;
              if (btn.isConnected) btn.innerHTML = originalHtml;
            }, 1500);
          }
        }
      }
    }
    // Refresh the models cache so the background poll is a no-op and the next render shows the
    // up-to-date last_test_* columns.
    state.models = await api("/models") as Model[];
  } catch (err: unknown) {
    showApiError(err, "Error refreshing models");
  } finally {
    busy.release();
  }
}

export async function bulkDeleteSelected(_providerId: string, e?: Event | null): Promise<void> {
  const ids = Array.from(state.selectedModels);
  if (ids.length === 0) return;
  if (!(await showConfirm({
    title: "Delete models",
    message: `Delete ${ids.length} models? This cannot be undone.`,
    danger: true,
    confirmLabel: "Delete",
  }))) return;
  const busy = beginButtonBusy(resolveBusyButton(e), { label: "Deleting…" });
  try {
    await Promise.all(ids.map((rowId) =>
      api("/models/" + rowId, { method: "DELETE" })
        .catch((err: unknown) => console.error("Failed delete", rowId, err))
    ));
    state.models = await api("/models") as Model[];
    state.selectedModels.clear();
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, "Error deleting models");
  } finally {
    busy.release();
  }
}
