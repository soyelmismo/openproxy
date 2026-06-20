// handlers/combo-target-handlers.ts — combo target CRUD: show
// the add-target modal, the model/provider cascade, the
// add/delete/reorder/reset-cooldown handlers. Split out of
// combo-handlers.js so neither file crosses the 300-LOC cap.
//
// Per spec §3 + §13.8 we do not attach to `window.*`.

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { escapeHtml, escapeAttr } from "../lib/escape.js";
import { appendModal } from "../lib/dom.js";
import { rerenderCurrentView } from "../state/router.js";
import type { Provider, Account, Model, ComboSummary } from "../lib/types/api.js";

// Local helper type for the model shape that the add-target modal
// deals with. The original code accepted `m.model_id || m.id` and
// `m.provider_id || m.owned_by` (OpenAI-style) as fallbacks, so we
// model the row loosely on top of the strict `Model` type.
type ModelWithFallbacks = Model & { id?: string; owned_by?: string };

// ---- Drag-and-Drop module state ----
let dragSourceId: number | null = null;
let dragComboId: number | null = null;
let dropPlaceholder: HTMLTableRowElement | null = null;
let dragFromHandle = false;

function removePlaceholder(): void {
  if (dropPlaceholder && dropPlaceholder.parentNode) {
    dropPlaceholder.parentNode.removeChild(dropPlaceholder);
  }
  dropPlaceholder = null;
}

function readOrderFromDOM(tbody: HTMLElement): number[] {
  const ids: number[] = [];
  for (const row of tbody.querySelectorAll("tr[data-drag-id]")) {
    const id = parseInt(row.getAttribute("data-drag-id") || "", 10);
    if (!Number.isNaN(id)) ids.push(id);
  }
  return ids;
}

function onDragStart(e: DragEvent): void {
  const row = (e.target as HTMLElement).closest("tr[data-drag-id]");
  if (!row) return;

  // CRITICAL FIX C1: use mousedown flag instead of e.target.closest(".drag-handle")
  if (!dragFromHandle) { e.preventDefault(); return; }
  dragFromHandle = false;

  dragSourceId = parseInt(row.getAttribute("data-drag-id") || "", 10);
  dragComboId = parseInt(row.getAttribute("data-combo-id") || "", 10);

  if (Number.isNaN(dragSourceId) || Number.isNaN(dragComboId)) {
    dragSourceId = null;
    dragComboId = null;
    return;
  }

  row.classList.add("dnd-dragging");
  if (e.dataTransfer) {
    e.dataTransfer.effectAllowed = "move";
    e.dataTransfer.setData("text/plain", String(dragSourceId));

    // Custom drag image: semi-transparent clone of the row
    const rect = row.getBoundingClientRect();
    e.dataTransfer.setDragImage(row, e.clientX - rect.left, e.clientY - rect.top);
  }
}

function onDragOver(e: DragEvent): void {
  e.preventDefault();
  if (e.dataTransfer) e.dataTransfer.dropEffect = "move";

  const tbody = document.getElementById("targets-tbody");
  const row = (e.target as HTMLElement).closest("tr[data-drag-id]");
  if (!tbody || !row || !dragSourceId) return;

  const targetId = parseInt(row.getAttribute("data-drag-id") || "", 10);
  if (targetId === dragSourceId) return;

  const rect = row.getBoundingClientRect();
  const midpoint = rect.top + rect.height / 2;
  const insertBefore = e.clientY < midpoint;

  removePlaceholder();

  dropPlaceholder = document.createElement("tr");
  dropPlaceholder.className = "dnd-placeholder";
  dropPlaceholder.innerHTML = `<td colspan="7"></td>`;

  if (insertBefore) {
    tbody.insertBefore(dropPlaceholder, row);
  } else {
    tbody.insertBefore(dropPlaceholder, row.nextSibling);
  }
}

function onDragEnter(_e: DragEvent): void {
  // No-op — dragover handles positioning.
}

function onDragLeave(e: DragEvent): void {
  const related = e.relatedTarget as HTMLElement | null;
  const tbody = document.getElementById("targets-tbody");
  if (tbody && related && tbody.contains(related)) return;
  removePlaceholder();
}

// CRITICAL FIX C2: compute drop position from e.clientY + bounding rects,
// NOT from e.target.closest() which fails when dropping on placeholder.
async function onDrop(e: DragEvent): Promise<void> {
  e.preventDefault();
  if (dragSourceId === null || dragComboId === null) { removePlaceholder(); return; }

  const tbody = document.getElementById("targets-tbody");
  removePlaceholder(); // Remove AFTER we've saved what we need
  if (!tbody) return;

  // Find drop position from mouse Y vs row midpoints
  const rows = [...tbody.querySelectorAll("tr[data-drag-id]")];
  let targetRow: Element | null = null;
  let insertAfter = false;
  for (const r of rows) {
    const rect = r.getBoundingClientRect();
    if (e.clientY <= rect.top + rect.height / 2) {
      targetRow = r;
      break;
    }
  }
  if (!targetRow) {
    // Cursor is past all rows — insert at end
    insertAfter = true;
    targetRow = rows[rows.length - 1] || null;
  }
  if (!targetRow) return;

  const dropTargetId = parseInt(targetRow.getAttribute("data-drag-id") || "", 10);
  if (Number.isNaN(dropTargetId) || dropTargetId === dragSourceId) return;

  const orderedIds = readOrderFromDOM(tbody);
  const fromIdx = orderedIds.indexOf(dragSourceId);
  const toIdx = orderedIds.indexOf(dropTargetId);
  if (fromIdx < 0 || toIdx < 0) return;

  const newOrder = [...orderedIds];
  newOrder.splice(fromIdx, 1);
  const adjustedIdx = insertAfter
    ? newOrder.indexOf(dropTargetId) + 1
    : newOrder.indexOf(dropTargetId);
  newOrder.splice(adjustedIdx, 0, dragSourceId);

  const comboId = dragComboId;
  try {
    await api(`/combos/${comboId}/targets/reorder`, {
      method: "POST",
      body: JSON.stringify({ target_ids: newOrder }),
    });
    rerenderCurrentView();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    alert("Error reordering: " + msg);
  }
}

function onDragEnd(_e: DragEvent): void {
  dragSourceId = null;
  dragComboId = null;
  removePlaceholder();

  const tbody = document.getElementById("targets-tbody");
  if (tbody) {
    tbody.querySelectorAll(".dnd-dragging").forEach((el) =>
      el.classList.remove("dnd-dragging")
    );
  }
}

export function initDragAndDrop(): void {
  const tbody = document.getElementById("targets-tbody");
  if (!tbody) return;

  // CRITICAL FIX C1: track mousedown on .drag-handle for drag guard
  tbody.addEventListener("mousedown", (e) => {
    dragFromHandle = !!(e.target as HTMLElement).closest(".drag-handle");
  });

  tbody.addEventListener("dragstart", onDragStart);
  tbody.addEventListener("dragover", onDragOver);
  tbody.addEventListener("dragenter", onDragEnter);
  tbody.addEventListener("dragleave", onDragLeave);
  tbody.addEventListener("drop", onDrop);
  tbody.addEventListener("dragend", onDragEnd);
}

export async function showAddTarget(comboId: number): Promise<void> {
  if (!state.models || state.models.length === 0) state.models = await api("/models") as typeof state.models;
  const pResp = await api("/providers") as Provider[];
  const aResp = await api("/accounts") as Account[];
  const sResp = await api(`/combos/${comboId}/targets/valid-sub-combos`).catch(() => [] as ComboSummary[]) as ComboSummary[];
  const providers: Provider[] = pResp;
  const accounts: Account[] = aResp;
  const validSubCombos: ComboSummary[] = sResp;
  const subComboOpts = (validSubCombos || []).map((c: ComboSummary) =>
    `<option value="${c.id}">${escapeHtml(c.name)} (id ${c.id})</option>`
  ).join("");
  const subComboEmpty = subComboOpts
    ? ""
    : "<option disabled>No other combos exist (or every other combo would create a cycle).</option>";
  const html = `
    <div class="modal-bg" id="add-target-modal" data-action="closeAddTarget" data-arg1="self">
      <div class="modal">
        <div class="modal-header">
          <h2>Add target to combo ${comboId}</h2>
          <button type="button" class="close-btn" data-action="closeAddTarget" aria-label="Close">&times;</button>
        </div>
        <form data-action="addTarget" data-arg1="${comboId}">
          <div class="modal-body">
            <div class="field">
              <label>Target type</label>
              <div class="radio-group">
                <label><input type="radio" name="target_kind" value="model" checked data-action="onTargetKindChange"> Model</label>
                <label><input type="radio" name="target_kind" value="combo" data-action="onTargetKindChange"> Sub-combo</label>
              </div>
            </div>
            <div id="model-fields">
              <div class="field">
                <label for="target-provider">Provider</label>
                <select id="target-provider" name="provider_id" data-action="onTargetProviderChange" required>
                  <option value="">Select provider...</option>
                  ${providers.map((p) => `<option value="${escapeAttr(p.id)}">${escapeHtml(p.name || p.id)}</option>`).join("")}
                </select>
              </div>
              <div class="field">
                <label for="target-account">Account (optional, leave blank to rotate)</label>
                <select id="target-account" name="account_id">
                  <option value="">— rotate —</option>
                  ${accounts.map((a) => `<option value="${a.id}">${escapeHtml(a.provider_id)}/${escapeHtml(a.label || String(a.id))}</option>`).join("")}
                </select>
              </div>
              <div class="field">
                <label>Models <small>(select one or more)</small></label>
                <div class="model-checkbox-header">
                  <button type="button" class="link" data-action="selectAllModelsInModal">Select all</button>
                  <button type="button" class="link" data-action="deselectAllModelsInModal">Deselect all</button>
                  <span class="model-checkbox-count" id="model-checkbox-count">0 selected</span>
                </div>
                <div class="model-checkbox-list" id="target-model-list">
                </div>
              </div>
            </div>
            <div id="combo-fields" style="display: none">
              <div class="field">
                <label for="target-sub-combo">Sub-combo</label>
                <select id="target-sub-combo" name="sub_combo_id" disabled>
                  ${subComboOpts || subComboEmpty}
                </select>
                <small>Only combos that won't close a cycle with combo ${comboId} are listed.</small>
              </div>
            </div>
            <div class="field">
              <label for="target-priority">Priority</label>
              <input id="target-priority" name="priority_order" type="number" value="100" required>
            </div>
          </div>
          <div class="modal-footer">
            <button type="button" data-action="closeAddTarget">Cancel</button>
            <button type="submit" class="primary">Add</button>
          </div>
        </form>
      </div>
    </div>
  `;
  appendModal(html);
  onTargetProviderChange();
}

export function onTargetKindChange(): void {
  const checked = document.querySelector('input[name="target_kind"]:checked') as HTMLInputElement | null;
  const kind = checked ? checked.value : "";
  const modelFields = document.getElementById("model-fields");
  const comboFields = document.getElementById("combo-fields");
  if (!modelFields || !comboFields) return;
  if (kind === "combo") {
    modelFields.style.display = "none"; comboFields.style.display = "";
  } else {
    modelFields.style.display = ""; comboFields.style.display = "none";
  }
  updateAddButtonLabel();
}

export function closeAddTarget(): void {
  const m = document.getElementById("add-target-modal");
  if (m) m.remove();
}

export function onTargetProviderChange(): void {
  const providerSel = document.getElementById("target-provider") as HTMLSelectElement | null;
  const modelList = document.getElementById("target-model-list");
  const countEl = document.getElementById("model-checkbox-count");
  if (!providerSel || !modelList) return;

  const provider = providerSel.value;
  if (!provider) {
    modelList.innerHTML = '<p class="model-checkbox-empty">Select a provider first</p>';
    if (countEl) countEl.textContent = "0 selected";
    updateAddButtonLabel();
    return;
  }

  const filtered = (state.models || []).filter(
    (m) => m.provider_id === provider && m.active
  );

  if (filtered.length === 0) {
    modelList.innerHTML = '<p class="model-checkbox-empty">No active models for this provider</p>';
    if (countEl) countEl.textContent = "0 selected";
    updateAddButtonLabel();
    return;
  }

  modelList.innerHTML = filtered.map((m: ModelWithFallbacks) => {
    const rowId = m.row_id;
    const upstreamId = m.model_id || m.id;
    if (rowId == null) return "";
    const label = m.display_name
      ? `${escapeHtml(String(upstreamId))} — ${escapeHtml(m.display_name)}`
      : escapeHtml(String(upstreamId));
    return `<label class="model-checkbox-item">
      <input type="checkbox" name="model_row_ids" value="${escapeAttr(String(rowId))}">
      <span class="model-checkbox-id">${label}</span>
    </label>`;
  }).filter(Boolean).join("");

  if (countEl) countEl.textContent = "0 selected";

  // WARNING FIX W4: use change event listener instead of data-action to avoid double-fire
  modelList.querySelectorAll("input[name='model_row_ids']").forEach((cb) => {
    cb.addEventListener("change", onModelCheckboxChange);
  });

  updateAddButtonLabel();
}

// WARNING FIX W4: handler bound via addEventListener, not data-action
export function onModelCheckboxChange(): void {
  const countEl = document.getElementById("model-checkbox-count");
  if (!countEl) return;
  const checked = document.querySelectorAll<HTMLInputElement>(
    "#target-model-list input[name='model_row_ids']:checked"
  );
  countEl.textContent = `${checked.length} selected`;
  updateAddButtonLabel();
}

export function selectAllModelsInModal(): void {
  const checkboxes = document.querySelectorAll<HTMLInputElement>(
    "#target-model-list input[name='model_row_ids']"
  );
  checkboxes.forEach((cb) => { cb.checked = true; });
  onModelCheckboxChange();
}

export function deselectAllModelsInModal(): void {
  const checkboxes = document.querySelectorAll<HTMLInputElement>(
    "#target-model-list input[name='model_row_ids']"
  );
  checkboxes.forEach((cb) => { cb.checked = false; });
  onModelCheckboxChange();
}

function updateAddButtonLabel(): void {
  const btn = document.querySelector<HTMLButtonElement>(
    "#add-target-modal button[type='submit']"
  );
  if (!btn) return;
  const checked = document.querySelectorAll<HTMLInputElement>(
    "#target-model-list input[name='model_row_ids']:checked"
  );
  const kind = (document.querySelector('input[name="target_kind"]:checked') as HTMLInputElement)?.value;
  if (kind === "combo") {
    btn.textContent = "Add";
  } else {
    btn.textContent = checked.length > 0 ? `Add ${checked.length} target${checked.length > 1 ? "s" : ""}` : "Add";
  }
}

export async function addTarget(comboId: number, e: Event): Promise<void> {
  const target = e.target;
  if (!(target instanceof HTMLFormElement)) return;
  const f = new FormData(target);
  const checked = document.querySelector('input[name="target_kind"]:checked') as HTMLInputElement | null;
  const kind = checked ? checked.value : "";

  if (kind === "combo") {
    // Sub-combo: single-add (unchanged)
    const subComboId = parseInt(String(f.get("sub_combo_id")));
    if (!subComboId) { alert("Select a sub-combo first."); return; }
    const body = {
      provider_id: "combo",
      account_id: null,
      model_row_id: null,
      sub_combo_id: subComboId,
      priority_order: parseInt(String(f.get("priority_order"))),
    };
    try {
      await api(`/combos/${comboId}/targets`, { method: "POST", body: JSON.stringify(body) });
      closeAddTarget();
      rerenderCurrentView();
    } catch (err: unknown) {
      const msg = err instanceof Error ? err.message : String(err);
      alert("Error: " + msg);
    }
    return;
  }

  // Model: multi-select batch add
  const checkedBoxes = document.querySelectorAll<HTMLInputElement>(
    "#target-model-list input[name='model_row_ids']:checked"
  );
  const modelRowIds = Array.from(checkedBoxes).map((cb) => parseInt(cb.value, 10))
    .filter((id) => !Number.isNaN(id));

  if (modelRowIds.length === 0) {
    alert("Select at least one model.");
    return;
  }

  const accountId = f.get("account_id") ? parseInt(String(f.get("account_id"))) : null;
  const basePriority = parseInt(String(f.get("priority_order")));
  const providerId = String(f.get("provider_id"));

  let added = 0;
  const errors: string[] = [];

  // WARNING FIX W1: assign incrementing priorities: basePriority + index
  for (let i = 0; i < modelRowIds.length; i++) {
    const modelRowId = modelRowIds[i];
    const body = {
      provider_id: providerId,
      account_id: accountId,
      model_row_id: modelRowId,
      sub_combo_id: null,
      priority_order: basePriority + i,
    };
    try {
      await api(`/combos/${comboId}/targets`, { method: "POST", body: JSON.stringify(body) });
      added++;
    } catch (err: unknown) {
      const msg = err instanceof Error ? err.message : String(err);
      errors.push(`Model row #${modelRowId}: ${msg}`);
    }
  }

  // WARNING FIX W2: show errors BEFORE closing modal, only close on success
  if (errors.length > 0 && added > 0) {
    alert(`Added ${added} target(s), but ${errors.length} failed:\n${errors.join("\n")}`);
    closeAddTarget();
  } else if (errors.length > 0) {
    alert(`All ${errors.length} target(s) failed:\n${errors.join("\n")}`);
    // Don't close — let user retry
  } else {
    closeAddTarget();
  }

  rerenderCurrentView();
}

export async function deleteTarget(comboId: number, targetId: number): Promise<void> {
  if (!confirm("Delete target " + targetId + "?")) return;
  try {
    await api(`/combos/${comboId}/targets/${targetId}`, { method: "DELETE" });
    rerenderCurrentView();
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    alert("Error: " + msg);
  }
}

export async function resetCooldown(comboId: number, targetId: number): Promise<void> {
  try {
    await api(`/combos/${comboId}/targets/${targetId}/clear-cooldown`, { method: "POST" });
    rerenderCurrentView();
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    alert("Could not clear cooldown: " + msg);
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
    rerenderCurrentView();
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    alert("Error reordering: " + msg);
  }
}

export function toggleTargetSelection(targetId: number, e: Event | null): void {
  const target = e && e.target ? e.target : null;
  const checked = target instanceof HTMLInputElement ? target.checked : false;
  if (checked) state.selectedTargets.add(targetId);
  else state.selectedTargets.delete(targetId);
  rerenderCurrentView();
}

export function toggleSelectAllTargets(e: Event | null): void {
  const target = e && e.target ? e.target : null;
  const checked = target instanceof HTMLInputElement ? target.checked : false;
  const visible = Array.from(document.querySelectorAll('#targets-tbody input[type="checkbox"]'))
    .map((cb) => parseInt(cb.getAttribute("data-target-id") || "", 10))
    .filter((id) => !Number.isNaN(id));
  if (checked) for (const id of visible) state.selectedTargets.add(id);
  else for (const id of visible) state.selectedTargets.delete(id);
  rerenderCurrentView();
}

export function clearTargetSelection(): void {
  state.selectedTargets.clear();
  rerenderCurrentView();
}

export async function bulkDeleteSelectedTargets(comboId: number): Promise<void> {
  const ids = Array.from(state.selectedTargets);
  if (ids.length === 0) return;
  if (!confirm(`Delete ${ids.length} targets? This cannot be undone.`)) return;
  await Promise.all(ids.map((tid) =>
    api(`/combos/${comboId}/targets/${tid}`, { method: "DELETE" })
      .catch((e: unknown) => console.error("Failed delete target", tid, e))
  ));
  state.selectedTargets.clear();
  rerenderCurrentView();
}
