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

export async function showAddTarget(comboId: number): Promise<void> {
  if (!state.models || state.models.length === 0) state.models = await api("/models") as typeof state.models;
  const pResp = await api("/providers") as Provider[];
  const aResp = await api("/accounts") as Account[];
  const sResp = await api(`/combos/${comboId}/targets/valid-sub-combos`).catch(() => [] as ComboSummary[]) as ComboSummary[];
  const providers: Provider[] = pResp;
  const accounts: Account[] = aResp;
  const validSubCombos: ComboSummary[] = sResp;
  const modelOpts = (state.models || []).map((m: ModelWithFallbacks) => {
    const rowId = m.row_id;
    const upstreamId = m.model_id || m.id;
    const owner = m.provider_id || m.owned_by || "?";
    if (rowId == null) return "";
    return `<option value="${escapeAttr(String(rowId))}">#${rowId} — ${escapeHtml(upstreamId)} (${escapeHtml(owner)})</option>`;
  }).filter(Boolean).join("");
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
                <label for="target-model">Model</label>
                <select id="target-model" name="model_row_id" required>
                  ${modelOpts || '<option disabled>No models discovered yet — click "Refresh models" on the Providers tab first.</option>'}
                </select>
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
  const modelSel = document.getElementById("target-model") as HTMLSelectElement | null;
  const comboSel = document.getElementById("target-sub-combo") as HTMLSelectElement | null;
  if (!modelFields || !comboFields) return;
  if (kind === "combo") {
    modelFields.style.display = "none"; comboFields.style.display = "";
    if (modelSel) modelSel.disabled = true; if (comboSel) comboSel.disabled = false;
  } else {
    modelFields.style.display = ""; comboFields.style.display = "none";
    if (modelSel) modelSel.disabled = false; if (comboSel) comboSel.disabled = true;
  }
}

export function closeAddTarget(): void {
  const m = document.getElementById("add-target-modal");
  if (m) m.remove();
}

export function onTargetProviderChange(): void {
  const providerSel = document.getElementById("target-provider") as HTMLSelectElement | null;
  const modelSel = document.getElementById("target-model") as HTMLSelectElement | null;
  if (!providerSel || !modelSel) return;
  const provider = providerSel.value;
  const filtered = (state.models || []).filter((m) => m.provider_id === provider && m.active);
  if (!provider) { modelSel.innerHTML = '<option disabled selected>Select a provider first</option>'; return; }
  const opts = filtered.map((m: ModelWithFallbacks) => {
    const rowId = m.row_id;
    const upstreamId = m.model_id || m.id;
    if (rowId == null) return "";
    return `<option value="${escapeAttr(String(rowId))}">${escapeHtml(upstreamId)}${m.display_name ? " — " + escapeHtml(m.display_name) : ""}</option>`;
  }).filter(Boolean).join("");
  modelSel.innerHTML = opts || '<option disabled>No active models for this provider</option>';
}

export async function addTarget(comboId: number, e: Event): Promise<void> {
  const target = e.target;
  if (!(target instanceof HTMLFormElement)) return;
  const f = new FormData(target);
  const checked = document.querySelector('input[name="target_kind"]:checked') as HTMLInputElement | null;
  const kind = checked ? checked.value : "";
  let body: Record<string, unknown>;
  if (kind === "combo") {
    const subComboId = parseInt(String(f.get("sub_combo_id")));
    if (!subComboId) { alert("Select a sub-combo first."); return; }
    body = { provider_id: "combo", account_id: null, model_row_id: null, sub_combo_id: subComboId, priority_order: parseInt(String(f.get("priority_order"))) };
  } else {
    body = {
      provider_id: f.get("provider_id"),
      account_id: f.get("account_id") ? parseInt(String(f.get("account_id"))) : null,
      model_row_id: parseInt(String(f.get("model_row_id"))),
      sub_combo_id: null,
      priority_order: parseInt(String(f.get("priority_order"))),
    };
  }
  try {
    await api(`/combos/${comboId}/targets`, { method: "POST", body: JSON.stringify(body) });
    closeAddTarget();
    rerenderCurrentView();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    alert("Error: " + msg);
  }
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
