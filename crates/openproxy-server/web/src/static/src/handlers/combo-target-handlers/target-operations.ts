// handlers/combo-target-handlers/target-operations.ts — target
// PATCH, selection sync, bulk actions, single-target CRUD (delete,
// cooldown reset, priority reorder), weight update.
//
// Split from combo-target-handlers.ts to keep each module < 600 LOC.

import { state } from "../../state/index.js";
import { api } from "../../state/api.js";
import { html, render, type TemplateResult } from "lit-html";
import { requestUpdate } from "../../state/reactive.js";
import { showToast } from "../../components/toast.js";
import { showApiError } from "../../lib/ui-utils.js";
import { showConfirm } from "../../lib/show-confirm.js";

// ---- PATCH helper (no re-render) ----
//
// Mirrors `patchComboField` in combo-handlers.ts: send the PATCH,
// swallow the success path (the DOM already reflects the user's
// choice — see `updateTargetWeight` below), and surface errors via
// a toast instead of `alert()` + `requestUpdate()`. The
// original `requestUpdate()` was the root cause of the
// "me cierra el dropdown" bug: a full DOM rebuild would close any
// open `<select>` (priority mode, cooldown mode) and steal focus
// from any `<input>` (weight, race size) the user was still editing.
async function patchTargetField(
  comboId: number,
  targetId: number,
  field: string,
  value: unknown,
): Promise<void> {
  try {
    await api(`/combos/${comboId}/targets/${targetId}`, {
      method: "PATCH",
      body: JSON.stringify({ [field]: value }),
    });
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    console.error("[openproxy] combo target PATCH failed:", msg);
    showApiError(err, "Error");
  }
}

// Targeted DOM patch for the multi-select checkbox UI on the
// targets table. Toggles each row's `selected` class, refreshes
// the master "select all" checkbox indeterminate state, and
// re-paints the "N selected / Delete selected / Clear selection"
// bulk-action bar — all without a full re-render.
function syncTargetSelectionUI(comboId: number): void {
  const checkboxes = Array.from(
    document.querySelectorAll<HTMLInputElement>(
      '#targets-tbody input[type="checkbox"][data-target-id]'
    )
  );
  for (const cb of checkboxes) {
    const id = parseInt(cb.getAttribute("data-target-id") || "", 10);
    if (Number.isNaN(id)) continue;
    const row = document.querySelector(`tr[data-drag-id="${id}"]`);
    if (row) row.classList.toggle("selected", state.selectedTargets.has(id));
  }

  const master = document.getElementById("target-select-all") as HTMLInputElement | null;
  if (master) {
    const visibleIds = checkboxes
      .map((cb) => parseInt(cb.getAttribute("data-target-id") || "", 10))
      .filter((id) => !Number.isNaN(id));
    if (visibleIds.length === 0) {
      master.checked = false;
      master.indeterminate = false;
    } else {
      const selectedVisible = visibleIds.filter((id) => state.selectedTargets.has(id)).length;
      if (selectedVisible === 0) { master.checked = false; master.indeterminate = false; }
      else if (selectedVisible === visibleIds.length) { master.checked = true; master.indeterminate = false; }
      else { master.checked = false; master.indeterminate = true; }
    }
  }

  const tbody = document.getElementById("targets-tbody");
  const section = tbody ? tbody.closest("section") : null;
  if (!section) return;
  const count = state.selectedTargets.size;
  let barWrapper = section.querySelector<HTMLDivElement>(".bulk-actions-bar-wrapper");
  if (count === 0) {
    if (barWrapper) barWrapper.remove();
    return;
  }
  if (!barWrapper) {
    barWrapper = document.createElement("div");
    barWrapper.className = "bulk-actions-bar-wrapper";
    const table = section.querySelector("table");
    if (table) table.insertAdjacentElement("beforebegin", barWrapper);
    else return;
  }
  render(bulkActionsBarTemplate(comboId, count), barWrapper);
}

function bulkActionsBarTemplate(comboId: number, count: number): TemplateResult {
  return html`
    <div class="bulk-actions-bar">
      <span><strong>${count}</strong> selected</span>
      <button class="danger" @click=${() => { void bulkDeleteSelectedTargets(comboId); }}>Delete selected</button>
      <button class="link" @click=${clearTargetSelection}>Clear selection</button>
    </div>
  `;
}

// Read the comboId off any table row in the targets table.
function comboIdFromTargetsTable(): number | null {
  const row = document.querySelector("tr[data-combo-id]");
  if (!row) return null;
  const raw = row.getAttribute("data-combo-id");
  if (raw == null) return null;
  const id = parseInt(raw, 10);
  return Number.isNaN(id) ? null : id;
}

// ---- Single-target CRUD ----

export async function deleteTarget(comboId: number, targetId: number): Promise<void> {
  if (!(await showConfirm({
    title: "Delete target",
    message: "Delete target " + targetId + "?",
    danger: true,
    confirmLabel: "Delete",
  }))) return;
  try {
    await api(`/combos/${comboId}/targets/${targetId}`, { method: "DELETE" });
    showToast("Target deleted.", "success");
    const { forceRerenderCurrentView } = await import("../../state/router.js");
    forceRerenderCurrentView();
  } catch (e: unknown) {
    showApiError(e, "Error");
  }
}

export async function resetCooldown(comboId: number, targetId: number): Promise<void> {
  try {
    await api(`/combos/${comboId}/targets/${targetId}/clear-cooldown`, { method: "POST" });
    const row = document.querySelector(`tr[data-drag-id="${targetId}"]`);
    if (row) {
      const badge = row.querySelector(".badge-cooldown");
      if (badge) badge.remove();
      const resetBtn = row.querySelector<HTMLButtonElement>('button[title="Clear cooldown"]');
      if (resetBtn) resetBtn.remove();
    }
  } catch (e: unknown) {
    showApiError(e, "Could not clear cooldown");
  }
}

export async function changePriority(comboId: number, targetId: number, delta: number): Promise<void> {
  try {
    const targets = await api(`/combos/${comboId}/targets`) as Array<{ id: number; priority_order: number }>;
    const sorted = [...targets].sort((a, b) => a.priority_order - b.priority_order);
    const idx = sorted.findIndex((t) => t.id === targetId);
    if (idx < 0) return;
    const swapIdx = idx + delta;
    if (swapIdx < 0 || swapIdx >= sorted.length) return;
    const a = sorted[idx];
    const b = sorted[swapIdx];
    if (!a || !b) return;
    sorted[idx] = b;
    sorted[swapIdx] = a;
    await api(`/combos/${comboId}/targets/reorder`, { method: "POST", body: JSON.stringify({ target_ids: sorted.map((t) => t.id) }) });
    requestUpdate();
  } catch (e: unknown) {
    showApiError(e, "Error reordering");
  }
}

/** `PATCH /admin/combos/:id/targets/:tid` — update a target's weight
 *  for the `weighted` priority mode (migration 000035). */
export async function updateTargetWeight(comboId: number, targetId: number, e: Event | null): Promise<void> {
  if (e && e.type === "input") return;
  const raw = e && e.target ? (e.target as HTMLInputElement).value.trim() : "";
  const val: number = raw === "" ? 1 : parseInt(raw, 10);
  if (!Number.isFinite(val) || val <= 0) {
    console.error("[openproxy] weight must be a positive integer");
    showToast("Weight must be a positive integer", "error");
    return;
  }
  await patchTargetField(comboId, targetId, "weight", val);
}

// ---- Multi-select / bulk actions ----

export function toggleTargetSelection(targetId: number, e: Event | null): void {
  const target = e && e.target ? e.target : null;
  const checked = target instanceof HTMLInputElement ? target.checked : false;
  if (checked) state.selectedTargets.add(targetId);
  else state.selectedTargets.delete(targetId);
  const comboId = comboIdFromTargetsTable();
  if (comboId != null) syncTargetSelectionUI(comboId);
}

export function toggleSelectAllTargets(e: Event | null): void {
  const target = e && e.target ? e.target : null;
  const checked = target instanceof HTMLInputElement ? target.checked : false;
  const visible = Array.from(document.querySelectorAll<HTMLInputElement>(
    '#targets-tbody input[type="checkbox"][data-target-id]'
  ))
    .map((cb) => parseInt(cb.getAttribute("data-target-id") || "", 10))
    .filter((id) => !Number.isNaN(id));
  if (checked) for (const id of visible) state.selectedTargets.add(id);
  else for (const id of visible) state.selectedTargets.delete(id);
  const comboId = comboIdFromTargetsTable();
  if (comboId != null) syncTargetSelectionUI(comboId);
}

export function clearTargetSelection(): void {
  state.selectedTargets.clear();
  document.querySelectorAll<HTMLInputElement>(
    '#targets-tbody input[type="checkbox"][data-target-id]'
  ).forEach((cb) => { cb.checked = false; });
  document.querySelectorAll("tr[data-drag-id].selected").forEach((row) => {
    row.classList.remove("selected");
  });
  const barWrapper = document.querySelector(".bulk-actions-bar-wrapper");
  if (barWrapper) barWrapper.remove();
  const master = document.getElementById("target-select-all") as HTMLInputElement | null;
  if (master) { master.checked = false; master.indeterminate = false; }
}

export async function bulkDeleteSelectedTargets(comboId: number): Promise<void> {
  const ids = Array.from(state.selectedTargets);
  if (ids.length === 0) return;
  if (!(await showConfirm({
    title: "Delete targets",
    message: `Delete ${ids.length} targets? This cannot be undone.`,
    danger: true,
    confirmLabel: "Delete",
  }))) return;
  await Promise.all(ids.map((tid) =>
    api(`/combos/${comboId}/targets/${tid}`, { method: "DELETE" })
      .catch((e: unknown) => console.error("Failed delete target", tid, e))
  ));
  state.selectedTargets.clear();
  requestUpdate();
}
