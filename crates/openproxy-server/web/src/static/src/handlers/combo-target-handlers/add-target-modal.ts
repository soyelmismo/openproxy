// handlers/combo-target-handlers/add-target-modal.ts — add-target modal: templates, model
// search/checkbox, global search, add logic. Split from combo-target-handlers.ts to keep
// each module < 600 LOC.
// Split from combo-target-handlers.ts to keep each module < 600 LOC.

import { state } from "../../state/index.js";
import { api } from "../../state/api.js";
import { html, render, type TemplateResult } from "lit-html";
import type { Account, Model, Combo, ComboSummary, ComboTargetWithModel, Provider } from "../../lib/types/api.js";
import { showToast } from "../../components/toast.js";
import { ensureModalRoot, showApiError } from "../../lib/ui-utils.js";
import { invalidateSubCombo } from "../../views/combos/subcombo-group.js";

// Local helper type for the model shape this modal deals with.
type ModelWithFallbacks = Model & { id?: string; owned_by?: string };

// ── Add-target modal: existing-targets cache ───────────────────────────────
// The combo's current target `model_row_id` values; `buildGlobalSearchGroups` skips any
// model whose `row_id` is in this set.
let existingTargetModelRowIds: Set<number> = new Set();
let nextTargetPriority: number = 1;

export function computeNextTargetPriority(targets: Array<{ priority_order?: number }>): number {
  if (!targets || targets.length === 0) return 1;
  const maxPriority = targets.reduce((max, t) => {
    const p = typeof t.priority_order === "number" && Number.isFinite(t.priority_order) ? t.priority_order : 0;
    return Math.max(max, p);
  }, 0);
  return maxPriority + 1;
}

// ── Templates ──────────────────────────────────────────────────────────────

export function subComboOptionsTemplate(subCombos: ComboSummary[], totalOtherCombos: number): TemplateResult {
  if (subCombos.length === 0) {
    if (totalOtherCombos === 0) {
      return html`<option value="" disabled selected>No other combos exist to nest</option>`;
    }
    return html`<option value="" disabled selected>All existing combos would create a cycle</option>`;
  }
  return html`
    <option value="" disabled selected>— select a sub-combo —</option>
    ${subCombos.map((c) => html`<option value=${c.id}>${c.name} (id ${c.id})</option>`)}
  `;
}

export function subComboHelpTemplate(subCombosCount: number, totalOtherCombos: number, comboId: number): TemplateResult {
  if (subCombosCount === 0) {
    if (totalOtherCombos === 0) {
      return html`<small class="hint" style="color: var(--color-warn);">No other combos exist yet. Create another combo first before you can nest it as a sub-combo.</small>`;
    }
    return html`<small class="hint" style="color: var(--color-warn);">All existing combos (${totalOtherCombos}) are already part of this combo's hierarchy and would create a cycle with combo ${comboId}.</small>`;
  }
  return html`<small>Only combos that won't close a cycle with combo ${comboId} are listed.</small>`;
}

function addTargetTemplate(
  comboId: number,
  validSubCombos: ComboSummary[],
  totalOtherCombos: number,
  wrapper: HTMLElement,
  initialPriority: number = nextTargetPriority,
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
              <label>Target Type</label>
              <div class="radio-group">
                <label><input type="radio" name="target_kind" value="model" checked @change=${onTargetKindChange}> Model</label>
                <label><input type="radio" name="target_kind" value="combo" @change=${onTargetKindChange}> Sub-combo</label>
              </div>
            </div>
            <div id="model-fields">
              <div class="field">
                <label for="target-model-search">Search Models</label>
                <div class="model-search-bar">
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
              ${validSubCombos.length === 0 ? html`
                <div class="banner banner-info" style="margin: 0 0 var(--space-3); padding: var(--space-2) var(--space-3); font-size: var(--fs-sm);">
                  ${totalOtherCombos === 0
                    ? "No other combos exist in the system. Create another combo first to use it as a sub-combo."
                    : `All existing combos (${totalOtherCombos}) would create a cycle with combo ${comboId}.`}
                </div>
              ` : ""}
              <div class="field">
                <label for="target-sub-combo">Sub-combo</label>
                <select id="target-sub-combo" name="sub_combo_id" ?disabled=${validSubCombos.length === 0} @change=${updateAddButtonLabel}>
                  ${subComboOptionsTemplate(validSubCombos, totalOtherCombos)}
                </select>
                ${subComboHelpTemplate(validSubCombos.length, totalOtherCombos, comboId)}
              </div>
              <div class="field">
                <label for="target-priority">Priority</label>
                <input id="target-priority" name="priority_order" type="number" value=${String(initialPriority)} required>
              </div>
              <div class="field">
                <label for="target-sub-combo-desc">Routing Criteria / Description</label>
                <input id="target-sub-combo-desc" name="description" type="text" placeholder="Routing description (optional)">
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

function modelCheckboxListTemplate(models: ModelWithFallbacks[], initialPriority: number = nextTargetPriority): TemplateResult {
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
                 value=${String(initialPriority)}
                 min="1"
                 step="1"
                 title="Priority (lower = preferred)">
          <input class="target-per-model-description cw-input"
                 name="target_description_${rowId}"
                 data-model-row=${String(rowId)}
                 type="text"
                 placeholder="Routing description (optional)..."
                 title="Semantic routing criteria / description for decision mode">
        </div>
      </div>
    `;
  })}`;
}

// ── Global model search ────────────────────────────────────────────────────

function globalModelSearchTemplate(groups: Map<string, ModelWithFallbacks[]>, initialPriority: number = nextTargetPriority): TemplateResult {
  if (groups.size === 0) {
    return html`<p class="model-checkbox-empty">No active models match your search.</p>`;
  }
  const providerIds = [...groups.keys()].sort();
  return html`${providerIds.map((p) => {
    const models = groups.get(p) ?? [];
    return html`
      <div class="model-checkbox-group">
        <div class="model-checkbox-group-header">${p}</div>
        ${modelCheckboxListTemplate(models, initialPriority)}
      </div>
    `;
  })}`;
}

/** Whether a model matches the search query. Supports a direct model id or display name
 *  ("Qwen3.8", "gpt-4o"), provider-prefixed searches ("hcnsec/Qwen3.8-Flash-Next",
 *  "openai:gpt-4o"), a partial provider + model token ("hcnsec/flash"), quoted queries
 *  ('"hcnsec/Qwen3.8-Flash-Next"') and a provider-only prefix ("hcnsec/"). */
export function modelMatchesSearch(
  m: { provider_id?: string; model_id?: string; display_name?: string | null; id?: string },
  query: string,
): boolean {
  const q = query.replace(/["']/g, "").trim().toLowerCase();
  if (!q) return true;

  const provider = (m.provider_id || "").toLowerCase();
  const modelId = (m.model_id || m.id || "").toLowerCase();
  const displayName = (m.display_name || "").toLowerCase();

  const qualifiedSlash = `${provider}/${modelId}`;
  const qualifiedColon = `${provider}:${modelId}`;
  const qualifiedName = `${provider}/${displayName}`;
  const haystack = `${qualifiedSlash} ${qualifiedColon} ${qualifiedName} ${modelId} ${displayName} ${provider}`;

  const tokens = q.split(/\s+/).filter(Boolean);
  return tokens.every((token) => {
    if (haystack.includes(token)) return true;
    const sep = token.includes("/") ? "/" : token.includes(":") ? ":" : null;
    if (sep) {
      const idx = token.indexOf(sep);
      const provPart = token.slice(0, idx);
      const modelPart = token.slice(idx + 1);
      if (provPart && modelPart) {
        const provMatches = provider.includes(provPart);
        const modelMatches = modelId.includes(modelPart) || displayName.includes(modelPart);
        if (provMatches && modelMatches) return true;
      }
    }
    return false;
  });
}

// Group the models matching the search query by provider.
export function buildGlobalSearchGroups(
  query: string,
  models: ModelWithFallbacks[] = (state.models || []) as ModelWithFallbacks[],
  excludedRowIds: Set<number> = existingTargetModelRowIds,
  providers: Provider[] = (state.providers || []) as Provider[],
): Map<string, ModelWithFallbacks[]> {
  const activeProviders = new Set<string>();
  const hasProvidersList = Array.isArray(providers) && providers.length > 0;
  if (hasProvidersList) {
    for (const p of providers) {
      if (p.active !== false) {
        activeProviders.add(p.id);
      }
    }
  }

  const groups = new Map<string, ModelWithFallbacks[]>();
  for (const m of models) {
    if (!m.active) continue;
    if (hasProvidersList && !activeProviders.has(m.provider_id)) continue;
    if (m.row_id != null && excludedRowIds.has(m.row_id)) continue;
    if (!modelMatchesSearch(m, query)) continue;
    const p: string = m.provider_id;
    if (!groups.has(p)) groups.set(p, []);
    groups.get(p)!.push(m);
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
    const subComboSelect = document.getElementById("target-sub-combo") as HTMLSelectElement | null;
    const isSubComboDisabled = Boolean(subComboSelect?.disabled);
    const hasSelection = Boolean(subComboSelect?.value);
    btn.disabled = isSubComboDisabled || !hasSelection;
    btn.textContent = "Add";
  } else {
    btn.disabled = false;
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

// ── Exported handlers ──────────────────────────────────────────────────────

export async function showAddTarget(comboId: number): Promise<void> {
  const [modelsResp, accountsResp, providersResp, sResp, allCombosResp] = await Promise.all([
    state.modelsComplete ? Promise.resolve(state.models) : (api("/models") as Promise<typeof state.models>),
    api("/accounts").catch(() => [] as Account[]) as Promise<Account[]>,
    api("/providers").catch(() => [] as Provider[]) as Promise<Provider[]>,
    api(`/combos/${comboId}/targets/valid-sub-combos`).catch(() => [] as ComboSummary[]) as Promise<ComboSummary[]>,
    api("/combos").catch(() => [] as Combo[]) as Promise<Combo[]>,
  ]);
  state.models = modelsResp;
  state.modelsComplete = true;
  state.accounts = accountsResp;
  state.providers = providersResp;
  const validSubCombos: ComboSummary[] = sResp;
  if (Array.isArray(allCombosResp) && allCombosResp.length > 0) {
    state.combos = allCombosResp;
  }
  const totalOtherCombos = (allCombosResp || []).filter((c) => c && c.id !== comboId).length;
  let existingTargets: ComboTargetWithModel[] = [];
  try {
    const existing: unknown = await api(`/combos/${comboId}/targets`);
    existingTargets = Array.isArray(existing) ? (existing as ComboTargetWithModel[]) : [];
    existingTargetModelRowIds = new Set(
      existingTargets
        .map((t) => t.model_row_id)
        .filter((id): id is number => typeof id === "number"),
    );
  } catch {
    existingTargetModelRowIds = new Set();
    existingTargets = [];
  }
  nextTargetPriority = computeNextTargetPriority(existingTargets);

  const wrapper = document.createElement("div");
  ensureModalRoot().appendChild(wrapper);
  render(addTargetTemplate(comboId, validSubCombos, totalOtherCombos, wrapper, nextTargetPriority), wrapper);
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
  let checkedOrder = 0;
  allCheckboxes.forEach((cb) => {
    const item = cb.closest(".model-checkbox-item") as HTMLElement | null;
    if (item) {
      updateModelControlsVisibility(cb.checked, item);
      if (cb.checked) {
        const priorityInput = item.querySelector(".target-per-model-priority") as HTMLInputElement | null;
        if (priorityInput && (!priorityInput.value || priorityInput.value === "100" || priorityInput.value === String(nextTargetPriority))) {
          priorityInput.value = String(nextTargetPriority + checkedOrder);
        }
        checkedOrder++;
      }
    }
  });
}

export function selectAllModelsInModal(): void {
  const checkboxes = document.querySelectorAll<HTMLInputElement>(
    "#target-model-list input[name='model_row_ids']"
  );
  let checkedIndex = 0;
  checkboxes.forEach((cb) => {
    cb.checked = true;
    const item = cb.closest(".model-checkbox-item") as HTMLElement | null;
    const priorityInput = item?.querySelector(".target-per-model-priority") as HTMLInputElement | null;
    if (priorityInput) {
      priorityInput.value = String(nextTargetPriority + checkedIndex);
    }
    checkedIndex++;
  });
  onModelCheckboxChange();
}

export function deselectAllModelsInModal(): void {
  const checkboxes = document.querySelectorAll<HTMLInputElement>(
    "#target-model-list input[name='model_row_ids']"
  );
  checkboxes.forEach((cb) => { cb.checked = false; });
  onModelCheckboxChange();
}

/** Read the "add as sub-combo" branch of the add-target form into the JSON body for
 *  `POST /admin/combos/:id/targets`.
 *
 *  Follows the `AddTargetInput` XOR contract: `sub_combo_id` is set while `model_row_id` and
 *  `account_id` are explicitly `null` (not omitted — the Rust deserializer rejects
 *  `Option`/`Some(null)` mismatches). `provider_id` is the literal `"combo"` marker.
 *
 *  Pure (form reads + `parseInt`); exported for `add-target-modal.test.ts`. */
export function buildSubComboTargetBodyFromForm(form: HTMLFormElement) {
  const f = new FormData(form);
  const desc = String(f.get("description") || "").trim();
  const body: {
    provider_id: string;
    account_id: null;
    model_row_id: null;
    sub_combo_id: number;
    priority_order: number;
    description?: string;
  } = {
    provider_id: "combo",
    account_id: null,
    model_row_id: null,
    sub_combo_id: parseInt(String(f.get("sub_combo_id"))),
    priority_order: parseInt(String(f.get("priority_order"))),
  };
  if (desc) {
    body.description = desc;
  }
  return body;
}

export async function addTarget(comboId: number, e: Event, wrapper?: HTMLElement): Promise<void> {
  const target = e.target;
  if (!(target instanceof HTMLFormElement)) return;
  const checked = document.querySelector('input[name="target_kind"]:checked') as HTMLInputElement | null;
  const kind = checked ? checked.value : "";

  if (kind === "combo") {
    const body = buildSubComboTargetBodyFromForm(target);
    if (!body.sub_combo_id) { showToast("Select a sub-combo first.", "error"); return; }
    try {
      await api(`/combos/${comboId}/targets`, { method: "POST", body: JSON.stringify(body) });
      invalidateSubCombo(comboId);
      if (wrapper) wrapper.remove(); else closeAddTarget();
      const { refreshComboDetail } = await import("../../views/combos.js");
      await refreshComboDetail();
      const { forceRerenderCurrentView } = await import("../../state/router.js");
      forceRerenderCurrentView();
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
    const descInput = item?.querySelector(".target-per-model-description") as HTMLInputElement | null;
    const accountIdRaw = accountSel && accountSel.value ? parseInt(accountSel.value, 10) : NaN;
    const accountId: number | null = Number.isNaN(accountIdRaw) ? null : accountIdRaw;
    const priorityOrder = priorityInput ? (parseInt(priorityInput.value, 10) || nextTargetPriority + i) : (nextTargetPriority + i);
    const description = descInput && descInput.value.trim() ? descInput.value.trim() : null;
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
      description,
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
    invalidateSubCombo(comboId);
    const { refreshComboDetail } = await import("../../views/combos.js");
    await refreshComboDetail();
    const { forceRerenderCurrentView } = await import("../../state/router.js");
    forceRerenderCurrentView();
  }
}
