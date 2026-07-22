// handlers/combo-target-handlers.ts — combo target CRUD: show
// the add-target modal, the model/provider cascade, the
// add/delete/reorder/reset-cooldown handlers. Split out of
// combo-handlers.js so neither file crosses the 300-LOC cap.
//
// Per spec §3 + §13.8 we do not attach to `window.*`.
//
// Migrated to lit-html: the add-target modal, the model
// checkbox list, the bulk-actions bar and the drag-drop
// placeholder are all rendered via `render()`. All `data-action`
// attributes have been replaced with direct `@click` / `@submit`
// / `@change` handlers; lit-html auto-escapes ids / labels so we
// no longer call `escapeHtml` / `escapeAttr`.

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { html, render, type TemplateResult } from "lit-html";
import type { Account, Model, ComboSummary, ComboTargetWithModel } from "../lib/types/api.js";
import { requestUpdate } from "../state/reactive.js";
import { showToast } from "../components/toast.js";
import { ensureModalRoot, showApiError } from "../lib/ui-utils.js";

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
    // Combo targets are not stored in `state` (they're fetched on
    // demand by the combo detail view), so the only "state" we
    // can patch here is the input's value itself, which the user
    // has already typed. The DOM is correct — no re-render needed.
    // The next natural re-render (page nav, target add/delete)
    // will pick up the server's authoritative value.
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    // Don't re-render — the user might be editing another field.
    // A re-render would lose their focus and unsaved changes.
    console.error("[openproxy] combo target PATCH failed:", msg);
    showApiError(err, "Error");
  }
}

// Targeted DOM patch for the multi-select checkbox UI on the
// targets table. Toggles each row's `selected` class, refreshes
// the master "select all" checkbox indeterminate state, and
// re-paints the "N selected / Delete selected / Clear selection"
// bulk-action bar — all without a full re-render. The bar's
// template mirrors `views/combos.ts` so the look stays consistent.
function syncTargetSelectionUI(comboId: number): void {
  // 1. Toggle each visible row's `selected` class from the
  //    current state of `state.selectedTargets`.
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

  // 2. Sync the master "select all" checkbox (indeterminate state).
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

  // 3. Re-paint the bulk-action bar (show / hide / count).
  const tbody = document.getElementById("targets-tbody");
  const section = tbody ? tbody.closest("section") : null;
  if (!section) return;
  const count = state.selectedTargets.size;
  // The bar lives in the same <section> as the tbody, inserted
  // just before the table. We re-render via lit-html so the
  // buttons keep their `@click` handlers attached (a plain
  // innerHTML rebuild would lose them).
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

// Read the comboId off any table row in the targets table. Used
// by selection handlers that don't receive the comboId as an arg.
function comboIdFromTargetsTable(): number | null {
  const row = document.querySelector("tr[data-combo-id]");
  if (!row) return null;
  const raw = row.getAttribute("data-combo-id");
  if (raw == null) return null;
  const id = parseInt(raw, 10);
  return Number.isNaN(id) ? null : id;
}

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

// Number of columns in the targets table. Computed once on
// `initDragAndDrop` from the `<thead>` so the drop placeholder's
// `<td colspan>` matches the actual layout (which changes when the
// weighted mode adds a Weight column).
let dropPlaceholderColspan = 8;

// ---- Add-target modal: existing-targets cache ----
//
// The combo detail view stores its targets in a module-private
// `detailTargets` array in `views/combos.ts`, so this handler file
// can't read them directly. To filter models that are already in the
// combo out of the "Add target" search results, we fetch the combo's
// targets once in `showAddTarget` and stash their `model_row_id`
// values here. `buildGlobalSearchGroups` then skips any model whose
// `row_id` is in this set. Sub-combo targets have `model_row_id =
// null` and are intentionally not excluded — they don't conflict
// with adding the same upstream model directly.
let existingTargetModelRowIds: Set<number> = new Set();

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
  render(html`<td colspan=${dropPlaceholderColspan}></td>`, dropPlaceholder);

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
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, "Error reordering");
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

  // Compute the column count from the table's `<thead>` so the drop
  // placeholder's `<td colspan>` matches the actual layout. The
  // weighted priority mode adds a Weight column (8 → 9). Falls back
  // to 8 (the legacy column count) if the thead can't be read.
  const table = tbody.closest("table");
  const ths = table ? table.querySelectorAll("thead th") : null;
  if (ths && ths.length > 0) dropPlaceholderColspan = ths.length;

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

function subComboOptionsTemplate(subCombos: ComboSummary[]): TemplateResult {
  if (subCombos.length === 0) {
    return html`<option disabled>No other combos exist (or every other combo would create a cycle).</option>`;
  }
  return html`${subCombos.map((c) => html`<option value=${c.id}>${c.name} (id ${c.id})</option>`)}`;
}

function addTargetTemplate(
  comboId: number,
  validSubCombos: ComboSummary[],
  wrapper: HTMLElement,
): TemplateResult {
  return html`
    <div class="modal-bg" id="add-target-modal"
         @click=${(e: Event) => { if (e.target === e.currentTarget) wrapper.remove(); }}>
      <div class="modal">
        <div class="modal-header">
          <h2>Add target to combo ${comboId}</h2>
          <button type="button" class="close-btn" @click=${() => wrapper.remove()} aria-label="Close">&times;</button>
        </div>
        <form @submit=${(e: Event) => { e.preventDefault(); void addTarget(comboId, e, wrapper); }}>
          <div class="modal-body">
            <div class="field">
              <label>Target type</label>
              <div class="radio-group">
                <label><input type="radio" name="target_kind" value="model" checked @change=${() => onTargetKindChange()}> Model</label>
                <label><input type="radio" name="target_kind" value="combo" @change=${() => onTargetKindChange()}> Sub-combo</label>
              </div>
            </div>
            <div id="model-fields">
              <div class="field">
                <label>Models <small>(select one or more — set account + priority per row)</small></label>
                <div class="model-search-wrap">
                  <input type="text" id="target-model-search" placeholder="Search all models across providers (e.g. gpt)…" @input=${onTargetModelSearch}>
                  <small class="model-search-hint">Empty search shows all active models from all providers, grouped by provider.</small>
                </div>
                <div class="model-checkbox-header">
                  <button type="button" class="link" @click=${selectAllModelsInModal}>Select all</button>
                  <button type="button" class="link" @click=${deselectAllModelsInModal}>Deselect all</button>
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
                  ${subComboOptionsTemplate(validSubCombos)}
                </select>
                <small>Only combos that won't close a cycle with combo ${comboId} are listed.</small>
              </div>
              <div class="field">
                <label for="target-priority">Priority</label>
                <input id="target-priority" name="priority_order" type="number" value="100" required>
              </div>
            </div>
          </div>
          <div class="modal-footer">
            <button type="button" @click=${() => wrapper.remove()}>Cancel</button>
            <button type="submit" class="primary">Add</button>
          </div>
        </form>
      </div>
    </div>
  `;
}

export async function showAddTarget(comboId: number): Promise<void> {
  if (!state.modelsComplete) {
    state.models = await api("/models") as typeof state.models;
    state.modelsComplete = true;
  }
  // The per-model account selects read from `state.accounts`
  // directly (see `accountsForProviderTemplate`), so the modal
  // template no longer takes `accounts` as a parameter. We still
  // need to make sure the global cache is populated — otherwise
  // every model would see an empty `<option value="">— rotate —</option>`
  // list and the user couldn't pin a specific account.
  state.accounts = await api("/accounts") as Account[];
  const sResp = await api(`/combos/${comboId}/targets/valid-sub-combos`).catch(() => [] as ComboSummary[]) as ComboSummary[];
  const validSubCombos: ComboSummary[] = sResp;
  // Fetch the combo's current targets so `buildGlobalSearchGroups`
  // can filter out models that are already in the combo. We stash
  // their `model_row_id` values in `existingTargetModelRowIds`; the
  // filter itself runs in `buildGlobalSearchGroups` so both the
  // initial render and every keystroke search respect it. Failures
  // here are non-fatal — an empty set just means no filtering.
  try {
    const existing: unknown = await api(`/combos/${comboId}/targets`);
    existingTargetModelRowIds = new Set(
      (Array.isArray(existing) ? (existing as ComboTargetWithModel[]) : [])
        .map((t) => t.model_row_id)
        .filter((id): id is number => typeof id === "number"),
    );
  } catch {
    existingTargetModelRowIds = new Set();
  }
  const wrapper = document.createElement("div");
  ensureModalRoot().appendChild(wrapper);
  render(addTargetTemplate(comboId, validSubCombos, wrapper), wrapper);
  // Render the modal's default view: all active models grouped by
  // provider. The search box is wired to the same `buildGlobalSearchGroups`
  // helper, so an empty query here and an empty query there are identical.
  renderInitialModelList();
}

function renderInitialModelList(): void {
  const modelList = document.getElementById("target-model-list");
  const countEl = document.getElementById("model-checkbox-count");
  if (!modelList) return;
  const groups = buildGlobalSearchGroups("");
  render(globalModelSearchTemplate(groups), modelList);
  if (countEl) {
    const checked = document.querySelectorAll<HTMLInputElement>(
      "#target-model-list input[name='model_row_ids']:checked"
    );
    countEl.textContent = `${checked.length} selected`;
  }
  updateAddButtonLabel();
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
  if (m) {
    const wrapper = m.parentElement;
    m.remove();
    if (wrapper && wrapper.children.length === 0 && wrapper.parentElement?.id === "modal-root") {
      wrapper.remove();
    }
  }
}

// Filter the global `state.accounts` to just those owned by the
// given provider. The per-model account select only offers
// accounts that actually match the model's provider — picking an
// account from a different provider would always fail server-side
// (the target row carries a single `provider_id`).
function accountsForProviderTemplate(providerId: string): TemplateResult {
  const matches = (state.accounts || []).filter((a) => a.provider_id === providerId);
  return html`
    <option value="">— rotate —</option>
    ${matches.map((a) => html`<option value=${String(a.id)}>${a.provider_id}/${a.label || a.email || String(a.id)}</option>`)}
  `;
}

function modelCheckboxListTemplate(models: ModelWithFallbacks[]): TemplateResult {
  if (models.length === 0) {
    return html`<p class="model-checkbox-empty">No active models for this provider</p>`;
  }
  return html`${models.map((m) => {
    const rowId = m.row_id;
    const upstreamId = m.model_id || m.id;
    const providerId = m.provider_id;
    if (rowId == null) return html``;
    // The wrapper is a <div> (not a <label>) so clicking the
    // sibling account select / priority input doesn't toggle the
    // checkbox. The visible label only wraps the checkbox + name
    // + test button. Controls are rendered hidden and toggled by
    // `updateModelControlsVisibility` on check/uncheck.
    return html`
      <div class="model-checkbox-item">
        <label class="model-checkbox-main">
          <input type="checkbox" name="model_row_ids" value=${String(rowId)} @change=${onModelCheckboxChange}>
          <span class="model-checkbox-id">${m.display_name ? html`${upstreamId} — ${m.display_name}` : html`${String(upstreamId)}`}</span>
          <button type="button" class="small model-test-btn" title="Test this model" @click=${async (e: Event) => {
            e.preventDefault();
            e.stopPropagation();
            const btn = e.target as HTMLButtonElement;
            btn.disabled = true;
            btn.textContent = "⏳";
            try {
              const result = await api(`/models/${rowId}/test`, { method: "POST" }) as { status: number; elapsed_ms?: number };
              btn.textContent = result.status >= 200 && result.status < 300 ? "✓" : "✗";
              btn.style.color = result.status >= 200 && result.status < 300 ? "var(--color-success)" : "var(--color-error)";
            } catch { btn.textContent = "✗"; btn.style.color = "var(--color-error)"; }
            setTimeout(() => { btn.disabled = false; btn.textContent = "🧪"; btn.style.color = ""; }, 3000);
          }}>🧪</button>
        </label>
        <div class="model-checkbox-controls"
             style="display: none"
             @click=${(e: Event) => e.stopPropagation()}>
          <select class="target-per-model-account"
                  name="target_account_${rowId}"
                  data-model-row=${String(rowId)}
                  title="Account (rotate if blank)">
            ${accountsForProviderTemplate(providerId)}
          </select>
          <input class="target-per-model-priority"
                 name="target_priority_${rowId}"
                 data-model-row=${String(rowId)}
                 type="number"
                 value="100"
                 min="1"
                 step="1"
                 title="Priority (lower = preferred)">
        </div>
      </div>
    `;
  })}`;
}

// ---- Global model search ----
//
// The user can type a query (e.g. "gpt") into the search box at the
// top of the model list to filter ALL active models from ALL
// providers. Results are grouped by provider so the user can see at
// a glance which provider each match belongs to. Selecting models
// from multiple providers at once is supported — `addTarget` looks
// up each model's `provider_id` from `state.models` so the right
// provider is sent to the backend.
//
// Empty query → all active models from all providers (the default
// view when the modal opens). The same `buildGlobalSearchGroups`
// helper is used for both the initial render and the on-input
// re-render, so the two paths are guaranteed to be in sync.

function globalModelSearchTemplate(groups: Map<string, ModelWithFallbacks[]>): TemplateResult {
  if (groups.size === 0) {
    return html`<p class="model-checkbox-empty">No active models match your search.</p>`;
  }
  // Stable ordering: sort providers alphabetically so the user can
  // scan the list predictably.
  const providerIds = [...groups.keys()].sort();
  return html`${providerIds.map((p) => {
    const models = groups.get(p) ?? [];
    return html`
      <div class="model-checkbox-group">
        <div class="model-checkbox-group-header">${p}</div>
        ${modelCheckboxListTemplate(models)}
      </div>
    `;
  })}`;
}

// Build the grouped-by-provider map of models matching the search
// query. The query matches case-insensitively against `model_id`,
// `display_name`, and `provider_id`. Whitespace-separated tokens are
// AND-combined: a model matches only if every token appears as a
// substring somewhere in those three fields. Inactive models are
// excluded (they can't be selected as a combo target anyway).
//
// Models already in the combo (their `row_id` is in
// `existingTargetModelRowIds`, populated by `showAddTarget`) are
// also excluded — there's no point offering the user a model they
// just added, and adding the same `model_row_id` twice would create
// a duplicate target row in the combo.
function buildGlobalSearchGroups(query: string): Map<string, ModelWithFallbacks[]> {
  const q = query.trim().toLowerCase();
  const groups = new Map<string, ModelWithFallbacks[]>();
  for (const m of (state.models || [])) {
    if (!m.active) continue;
    if (m.row_id != null && existingTargetModelRowIds.has(m.row_id)) continue;
    if (q) {
      const haystack = `${m.model_id || ""} ${m.display_name || ""} ${m.provider_id || ""}`.toLowerCase();
      const tokens = q.split(/\s+/).filter(Boolean);
      // Every token must be a substring of the haystack.
      if (!tokens.every((t) => haystack.includes(t))) continue;
    }
    const p: string = m.provider_id;
    if (!groups.has(p)) groups.set(p, []);
    groups.get(p)!.push(m as ModelWithFallbacks);
  }
  return groups;
}

export function onTargetModelSearch(): void {
  const searchEl = document.getElementById("target-model-search") as HTMLInputElement | null;
  const modelList = document.getElementById("target-model-list");
  const countEl = document.getElementById("model-checkbox-count");
  if (!searchEl || !modelList) return;

  const query = searchEl.value;
  // Empty query → restore the all-providers grouped view (the default
  // also used when the modal first opens).
  if (query.trim() === "") {
    renderInitialModelList();
    return;
  }

  const groups = buildGlobalSearchGroups(query);
  render(globalModelSearchTemplate(groups), modelList);
  // The count display reflects the user's current selections, which
  // persist across re-renders because the checkbox `value` is the
  // stable `row_id`. We re-count the checked boxes so the count
  // stays accurate even after a re-render.
  if (countEl) {
    const checked = document.querySelectorAll<HTMLInputElement>(
      "#target-model-list input[name='model_row_ids']:checked"
    );
    countEl.textContent = `${checked.length} selected`;
  }
  updateAddButtonLabel();
}

// Show or hide the per-model account + priority controls. The
// controls live inside the same `.model-checkbox-item` as the
// checkbox; we toggle their `display` here so the row only
// shows them when the checkbox is on. The visual indent (see
// `.model-checkbox-controls` in components.css) makes it clear
// the controls belong to the row above.
function updateModelControlsVisibility(checked: boolean, item: HTMLElement): void {
  const controls = item.querySelector(".model-checkbox-controls") as HTMLElement | null;
  if (controls) controls.style.display = checked ? "" : "none";
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
  // Sync the visibility of every row's per-model controls so
  // unchecked rows hide their account/priority inputs and
  // checked rows show them.
  const allCheckboxes = document.querySelectorAll<HTMLInputElement>(
    "#target-model-list input[name='model_row_ids']"
  );
  allCheckboxes.forEach((cb) => {
    const item = cb.closest(".model-checkbox-item") as HTMLElement | null;
    if (item) updateModelControlsVisibility(cb.checked, item);
  });
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

export async function addTarget(comboId: number, e: Event, wrapper?: HTMLElement): Promise<void> {
  const target = e.target;
  if (!(target instanceof HTMLFormElement)) return;
  const f = new FormData(target);
  const checked = document.querySelector('input[name="target_kind"]:checked') as HTMLInputElement | null;
  const kind = checked ? checked.value : "";

  if (kind === "combo") {
    // Sub-combo: single-add (unchanged)
    const subComboId = parseInt(String(f.get("sub_combo_id")));
    if (!subComboId) { showToast("Select a sub-combo first.", "error"); return; }
    const body = {
      provider_id: "combo",
      account_id: null,
      model_row_id: null,
      sub_combo_id: subComboId,
      priority_order: parseInt(String(f.get("priority_order"))),
    };
    try {
      await api(`/combos/${comboId}/targets`, { method: "POST", body: JSON.stringify(body) });
      if (wrapper) wrapper.remove(); else closeAddTarget();
      requestUpdate();
    } catch (err: unknown) {
      showApiError(err, "Error");
    }
    return;
  }

  // Model: multi-select batch add.
  //
  // Account and priority are now per-model — read each from the
  // row's own controls (`.target-per-model-account` and
  // `.target-per-model-priority`) so the user can pin a specific
  // account on one row and let another row rotate freely.
  // `provider_id` still comes from `state.models` since the global
  // search lets the user mix models from multiple providers in one
  // batch — there's no single form-level provider to read.
  const checkedBoxes = document.querySelectorAll<HTMLInputElement>(
    "#target-model-list input[name='model_row_ids']:checked"
  );
  const modelRowIds = Array.from(checkedBoxes).map((cb) => parseInt(cb.value, 10))
    .filter((id) => !Number.isNaN(id));

  if (modelRowIds.length === 0) {
    showToast("Select at least one model.", "error");
    return;
  }

  // Build a row_id → provider_id lookup from `state.models` so the
  // global-search workflow (where the user can select models from
  // multiple providers in a single batch) sends the correct
  // `provider_id` for each target. Falls back to the model's own
  // `provider_id` field when the lookup misses — the model already
  // carries it as part of the row data.
  const rowIdToProvider = new Map<number, string>();
  for (const m of (state.models || [])) {
    if (m.row_id != null) rowIdToProvider.set(m.row_id, m.provider_id);
  }

  let added = 0;
  const errors: string[] = [];

  for (let i = 0; i < checkedBoxes.length; i++) {
    const cb = checkedBoxes[i]!;
    const modelRowId = parseInt(cb.value, 10);
    if (Number.isNaN(modelRowId)) continue;
    // Find this row's sibling controls. The controls live inside
    // the same `.model-checkbox-item` wrapper as the checkbox.
    const item = cb.closest(".model-checkbox-item") as HTMLElement | null;
    const accountSel = item?.querySelector(".target-per-model-account") as HTMLSelectElement | null;
    const priorityInput = item?.querySelector(".target-per-model-priority") as HTMLInputElement | null;
    // Empty string ("— rotate —") or blank input → null. A user-typed
    // non-numeric value would NaN — treat as null rather than crash.
    const accountIdRaw = accountSel && accountSel.value ? parseInt(accountSel.value, 10) : NaN;
    const accountId: number | null = Number.isNaN(accountIdRaw) ? null : accountIdRaw;
    const priorityOrder = priorityInput ? (parseInt(priorityInput.value, 10) || 100) : 100;
    const providerForModel = rowIdToProvider.get(modelRowId) ?? "";
    if (!providerForModel) {
      errors.push(`Model row #${modelRowId}: could not determine provider — ensure the model exists in the models cache.`);
      continue;
    }
    const body = {
      provider_id: providerForModel,
      account_id: accountId,
      model_row_id: modelRowId,
      sub_combo_id: null,
      priority_order: priorityOrder,
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
    showToast(`Added ${added} target(s), but ${errors.length} failed: ${errors.join("; ")}`, "warning");
    if (wrapper) wrapper.remove(); else closeAddTarget();
  } else if (errors.length > 0) {
    showToast(`All ${errors.length} target(s) failed: ${errors.join("; ")}`, "error");
    // Don't close — let user retry
  } else {
    showToast(`Added ${added} target(s) successfully.`, "success");
    if (wrapper) wrapper.remove(); else closeAddTarget();
  }

  // Re-fetch the combo's targets so the table reflects the new entries.
  // The combo detail view reads from a local `detailTargets` array that
  // is NOT in the global `state` — calling `requestUpdate()` alone won't
  // refresh it. We use forceRerenderCurrentView() which calls navigate()
  // directly, bypassing the hashchange requirement (setting location.hash
  // to the same URL is a no-op — no hashchange fires).
  if (added > 0 && comboId) {
    const { forceRerenderCurrentView } = await import("../state/router.js");
    forceRerenderCurrentView();
  }
}

export async function deleteTarget(comboId: number, targetId: number): Promise<void> {
  if (!confirm("Delete target " + targetId + "?")) return;
  try {
    await api(`/combos/${comboId}/targets/${targetId}`, { method: "DELETE" });
    showToast("Target deleted.", "success");
    // Re-mount the combo detail view to refresh the targets table.
    const { forceRerenderCurrentView } = await import("../state/router.js");
    forceRerenderCurrentView();
  } catch (e: unknown) {
    showApiError(e, "Error");
  }
}

export async function resetCooldown(comboId: number, targetId: number): Promise<void> {
  try {
    await api(`/combos/${comboId}/targets/${targetId}/clear-cooldown`, { method: "POST" });
    // Targeted DOM patch: hide the cooldown badge and the reset
    // button on the affected row. We do NOT call
    // requestUpdate() — see `patchTargetField` above for
    // the rationale (a full rebuild would close any open
    // `<select>` on a sibling row, e.g. the weight input or the
    // priority-mode dropdown). The next background poll / page
    // nav will reconcile the "N of M in cooldown" banner above
    // the table.
    const row = document.querySelector(`tr[data-drag-id="${targetId}"]`);
    if (row) {
      const badge = row.querySelector(".badge-cooldown");
      if (badge) badge.remove();
      // The reset-cooldown button is rendered by views/combos.ts with
      // `title="Clear cooldown"` and a `@click` handler — we look it
      // up by title because the old `data-action="resetCooldown"`
      // attribute no longer exists in the lit-html view template.
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

export function toggleTargetSelection(targetId: number, e: Event | null): void {
  const target = e && e.target ? e.target : null;
  const checked = target instanceof HTMLInputElement ? target.checked : false;
  if (checked) state.selectedTargets.add(targetId);
  else state.selectedTargets.delete(targetId);
  // Targeted DOM patch — toggle the row's `selected` class and
  // refresh the bulk-action bar + master checkbox without a full
  // re-render (which would close any open `<select>` / steal focus
  // from any `<input>` on a sibling row). The checkbox is already
  // toggled by the browser.
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
  // Targeted DOM patch — the master checkbox is already toggled
  // by the browser; we just sync the row classes + bulk bar.
  const comboId = comboIdFromTargetsTable();
  if (comboId != null) syncTargetSelectionUI(comboId);
}

export function clearTargetSelection(): void {
  state.selectedTargets.clear();
  // Targeted DOM patch: uncheck every visible checkbox, drop every
  // row's `selected` class, and remove the bulk-action bar. The
  // master "select all" checkbox is also reset. A full re-render
  // would close any open `<select>` on the page; the targeted
  // patch preserves the rest of the DOM.
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
  if (!confirm(`Delete ${ids.length} targets? This cannot be undone.`)) return;
  await Promise.all(ids.map((tid) =>
    api(`/combos/${comboId}/targets/${tid}`, { method: "DELETE" })
      .catch((e: unknown) => console.error("Failed delete target", tid, e))
  ));
  state.selectedTargets.clear();
  requestUpdate();
}

/** `PATCH /admin/combos/:id/targets/:tid` — update a target's weight
 *  for the `weighted` priority mode (migration 000035). The dashboard
 *  fires this from the Weight column's `<input type="number">` in
 *  `views/combos.ts`.
 *
 *  The generic data-action dispatcher fires for both "input" (per
 *  keystroke) and "change" (blur/enter); we filter on "input" so we
 *  don't PATCH on every keystroke. Empty input resets to the default
 *  weight of 1. The backend rejects weights `<= 0` with a 400.
 *
 *  We delegate to `patchTargetField` so the success path does NOT
 *  call `requestUpdate()` (the input already shows the value
 *  the user typed) and errors surface via a toast instead of
 *  `alert()` + re-render. See `patchComboField` in combo-handlers.ts
 *  for the full rationale. */
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
