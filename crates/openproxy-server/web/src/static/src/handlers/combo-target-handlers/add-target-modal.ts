// handlers/combo-target-handlers/add-target-modal.ts — add-target
// modal: templates, model search/checkbox, global search, add logic.
//
// Split from combo-target-handlers.ts to keep each module < 600 LOC.

import { state } from "../../state/index.js";
import { api } from "../../state/api.js";
import { html, render, type TemplateResult } from "lit-html";
import type { Account, Model, ComboSummary, ComboTargetWithModel } from "../../lib/types/api.js";
import { requestUpdate } from "../../state/reactive.js";
import { showToast } from "../../components/toast.js";
import { ensureModalRoot, showApiError } from "../../lib/ui-utils.js";

// Local helper type for the model shape that the add-target modal
// deals with.
type ModelWithFallbacks = Model & { id?: string; owned_by?: string };

// ---- Add-target modal: existing-targets cache ----
//
// Stash the combo's current target `model_row_id` values here.
// `buildGlobalSearchGroups` skips any model whose `row_id` is in
// this set.
let existingTargetModelRowIds: Set<number> = new Set();

// ---- Templates ----

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

// Filter accounts to those owned by the given provider.
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
            btn.textContent = "...";
            try {
              const result = await api(`/models/${rowId}/test`, { method: "POST" }) as { status: number; elapsed_ms?: number };
              btn.textContent = result.status >= 200 && result.status < 300 ? "OK" : "ERR";
              btn.style.color = result.status >= 200 && result.status < 300 ? "var(--color-success)" : "var(--color-error)";
            } catch { btn.textContent = "ERR"; btn.style.color = "var(--color-error)"; }
            setTimeout(() => { btn.disabled = false; btn.textContent = "Test"; btn.style.color = ""; }, 3000);
          }}>Test</button>
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

function globalModelSearchTemplate(groups: Map<string, ModelWithFallbacks[]>): TemplateResult {
  if (groups.size === 0) {
    return html`<p class="model-checkbox-empty">No active models match your search.</p>`;
  }
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

// Build the grouped-by-provider map of models matching the search query.
function buildGlobalSearchGroups(query: string): Map<string, ModelWithFallbacks[]> {
  const q = query.trim().toLowerCase();
  const groups = new Map<string, ModelWithFallbacks[]>();
  for (const m of (state.models || [])) {
    if (!m.active) continue;
    if (m.row_id != null && existingTargetModelRowIds.has(m.row_id)) continue;
    if (q) {
      const haystack = `${m.model_id || ""} ${m.display_name || ""} ${m.provider_id || ""}`.toLowerCase();
      const tokens = q.split(/\s+/).filter(Boolean);
      if (!tokens.every((t) => haystack.includes(t))) continue;
    }
    const p: string = m.provider_id;
    if (!groups.has(p)) groups.set(p, []);
    groups.get(p)!.push(m as ModelWithFallbacks);
  }
  return groups;
}

// Show or hide the per-model account + priority controls.
function updateModelControlsVisibility(checked: boolean, item: HTMLElement): void {
  const controls = item.querySelector(".model-checkbox-controls") as HTMLElement | null;
  if (controls) controls.style.display = checked ? "" : "none";
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

// ---- Exported handlers ----

export async function showAddTarget(comboId: number): Promise<void> {
  if (!state.modelsComplete) {
    state.models = await api("/models") as typeof state.models;
    state.modelsComplete = true;
  }
  state.accounts = await api("/accounts") as Account[];
  const sResp = await api(`/combos/${comboId}/targets/valid-sub-combos`).catch(() => [] as ComboSummary[]) as ComboSummary[];
  const validSubCombos: ComboSummary[] = sResp;
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
  renderInitialModelList();
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

export function onTargetModelSearch(): void {
  const searchEl = document.getElementById("target-model-search") as HTMLInputElement | null;
  const modelList = document.getElementById("target-model-list");
  const countEl = document.getElementById("model-checkbox-count");
  if (!searchEl || !modelList) return;

  const query = searchEl.value;
  if (query.trim() === "") {
    renderInitialModelList();
    return;
  }

  const groups = buildGlobalSearchGroups(query);
  render(globalModelSearchTemplate(groups), modelList);
  if (countEl) {
    const checked = document.querySelectorAll<HTMLInputElement>(
      "#target-model-list input[name='model_row_ids']:checked"
    );
    countEl.textContent = `${checked.length} selected`;
  }
  updateAddButtonLabel();
}

export function onModelCheckboxChange(): void {
  const countEl = document.getElementById("model-checkbox-count");
  if (!countEl) return;
  const checked = document.querySelectorAll<HTMLInputElement>(
    "#target-model-list input[name='model_row_ids']:checked"
  );
  countEl.textContent = `${checked.length} selected`;
  updateAddButtonLabel();
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

export async function addTarget(comboId: number, e: Event, wrapper?: HTMLElement): Promise<void> {
  const target = e.target;
  if (!(target instanceof HTMLFormElement)) return;
  const f = new FormData(target);
  const checked = document.querySelector('input[name="target_kind"]:checked') as HTMLInputElement | null;
  const kind = checked ? checked.value : "";

  if (kind === "combo") {
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
  const checkedBoxes = document.querySelectorAll<HTMLInputElement>(
    "#target-model-list input[name='model_row_ids']:checked"
  );
  const modelRowIds = Array.from(checkedBoxes).map((cb) => parseInt(cb.value, 10))
    .filter((id) => !Number.isNaN(id));

  if (modelRowIds.length === 0) {
    showToast("Select at least one model.", "error");
    return;
  }

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
    const item = cb.closest(".model-checkbox-item") as HTMLElement | null;
    const accountSel = item?.querySelector(".target-per-model-account") as HTMLSelectElement | null;
    const priorityInput = item?.querySelector(".target-per-model-priority") as HTMLInputElement | null;
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

  if (errors.length > 0 && added > 0) {
    showToast(`Added ${added} target(s), but ${errors.length} failed: ${errors.join("; ")}`, "warning");
    if (wrapper) wrapper.remove(); else closeAddTarget();
  } else if (errors.length > 0) {
    showToast(`All ${errors.length} target(s) failed: ${errors.join("; ")}`, "error");
  } else {
    showToast(`Added ${added} target(s) successfully.`, "success");
    if (wrapper) wrapper.remove(); else closeAddTarget();
  }

  if (added > 0 && comboId) {
    const { forceRerenderCurrentView } = await import("../../state/router.js");
    forceRerenderCurrentView();
  }
}
