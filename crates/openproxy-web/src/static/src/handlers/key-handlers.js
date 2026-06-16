// handlers/key-handlers.js — API key CRUD: create, edit, regen,
// revoke, delete, toggle expiry, build body. The create/edit
// modal HTML is built here (kept small, ~70 lines). The form is
// dispatched via the central data-action shim in app.js; the form
// is submitted through `data-action="createKey" data-arg1=...`
// (or `updateKey` with the key id).
//
// Per spec §3 + §13.8 we do not attach to `window.*`. Functions
// are exported and registered in handlers/registry.js.

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { escapeHtml, escapeAttr } from "../lib/escape.js";
import { appendModal } from "../lib/dom.js";
import { showPlaintextKey } from "../components/key-display.js";
import { renderAllowedModelsChips } from "../components/model-picker.js";
import { rerenderCurrentView } from "../state/router.js";

function buildModalHtml({ mode, key }) {
  const isEdit = mode === "edit";
  const labelVal = isEdit ? (key.label || "") : "";
  const scopes = isEdit ? (key.scopes || []) : ["chat"];
  let allowedModelsValue = "";
  if (isEdit && Array.isArray(key.allowed_models)) {
    allowedModelsValue = key.allowed_models.length === 0 ? " " : key.allowed_models.join(",");
  }
  const title = isEdit ? `Edit API key #${key.id}` : "Create API key";
  const formAction = isEdit ? "updateKey" : "createKey";
  const formExtraArg = isEdit ? ` data-arg1="${escapeAttr(String(key.id))}"` : "";
  return `
    <div class="modal-bg" data-action="closeKeyForm" data-arg1="self">
      <div class="modal">
        <div class="modal-header">
          <h2>${escapeHtml(title)}</h2>
          <button type="button" class="close-btn" data-action="closeKeyForm" data-arg1="self" aria-label="Close">&times;</button>
        </div>
        <form data-action="${formAction}"${formExtraArg}>
          <div class="modal-body">
            <div class="field">
              <label for="key-label">Label</label>
              <input id="key-label" name="label" type="text" placeholder="my-app" value="${escapeAttr(labelVal)}" required>
            </div>
            <div class="field">
              <span class="field-label">Scopes</span>
              <div class="scopes-list">
                <label class="scope-item">
                  <input type="checkbox" name="scopes" value="chat" ${scopes.includes("chat") ? "checked" : ""}>
                  <div class="scope-info"><strong>chat</strong><small>Can use /v1/chat/completions</small></div>
                </label>
                <label class="scope-item">
                  <input type="checkbox" name="scopes" value="manage" ${scopes.includes("manage") ? "checked" : ""}>
                  <div class="scope-info"><strong>manage</strong><small>Can use /v1/admin/* (CRUD providers, accounts, etc.)</small></div>
                </label>
                <label class="scope-item">
                  <input type="checkbox" name="scopes" value="read" ${scopes.includes("read") ? "checked" : ""}>
                  <div class="scope-info"><strong>read</strong><small>Can use analytics endpoints (GET only)</small></div>
                </label>
              </div>
            </div>
            <div class="field">
              <span class="field-label">Allowed models (empty = all)</span>
              <div class="model-picker-display" id="model-picker-display">
                <span class="muted">all models</span>
                <button type="button" class="link-btn" data-action="openModelPickerModal">Edit</button>
              </div>
              <input type="hidden" name="allowed_models" value="${escapeAttr(allowedModelsValue)}">
            </div>
            <div class="field">
              <label for="key-expires-amount">Expires in</label>
              <div class="expiry-row">
                <input id="key-expires-amount" type="number" name="expires_amount" min="1" max="999" placeholder="30" ${isEdit && key.expires_at ? `value="${escapeAttr(String(formatExpiryAmount(key.expires_at)))}"` : ""}>
                <select name="expires_unit" data-action="toggleExpiryAmount">
                  <option value="days" ${isEdit && key.expires_at ? "selected" : ""}>days</option>
                  <option value="months" ${!isEdit || !key.expires_at ? "selected" : ""}>months</option>
                  <option value="years">years</option>
                  <option value="never" ${!isEdit || !key.expires_at ? "selected" : ""}>never</option>
                </select>
              </div>
            </div>
          </div>
          <div class="modal-footer">
            <button type="button" data-action="closeKeyForm" data-arg1="self">Cancel</button>
            <button type="submit" class="primary">${isEdit ? "Save" : "Create key"}</button>
          </div>
        </form>
      </div>
    </div>
  `;
}

function formatExpiryAmount(iso) {
  if (!iso) return "";
  const ms = new Date(iso) - new Date();
  if (!isFinite(ms) || ms < 0) return "";
  const days = Math.floor(ms / (1000 * 60 * 60 * 24));
  if (days >= 365) return Math.floor(days / 365);
  if (days >= 30) return Math.floor(days / 30);
  return Math.max(1, days);
}

export async function showCreateKey() {
  if (!state.models || state.models.length === 0) state.models = await api("/models");
  appendModal(buildModalHtml({ mode: "create" }));
  renderAllowedModelsChips();
}

export async function showEditKey(id) {
  if (!state.models || state.models.length === 0) state.models = await api("/models");
  let key;
  try { key = await api("/keys/" + id); }
  catch (e) { alert("Error: " + e.message); return; }
  appendModal(buildModalHtml({ mode: "edit", key }));
  renderAllowedModelsChips();
}

// Closes the key form modal. The first arg is a placeholder
// (data-arg1="self") reserved for future "clicked-by-element"
// telemetry; the handler finds the modal-bg from the event target.
export function closeKeyForm(_selfPlaceholder, e) {
  const modalBg = (e && e.target && e.target.closest) ? e.target.closest(".modal-bg") : null;
  if (modalBg) modalBg.remove();
  else {
    // Fallback: remove all key-form modals (should only be one).
    document.querySelectorAll(".modal-bg").forEach((el) => {
      if (el.querySelector('form[data-action="createKey"], form[data-action="updateKey"]')) el.remove();
    });
  }
  const picker = document.getElementById("model-picker-modal");
  if (picker) picker.style.display = "none";
}

export function toggleExpiryAmount(e) {
  const select = e && e.target ? e.target : null;
  if (!select) return;
  const row = select.parentElement;
  const amount = row.querySelector('input[name="expires_amount"]');
  if (!amount) return;
  amount.disabled = select.value === "never";
  if (select.value === "never") amount.value = "";
}

function calculateExpiry(amount, unit) {
  if (unit === "never" || !amount) return null;
  const n = parseInt(amount, 10);
  if (!Number.isFinite(n) || n <= 0) return null;
  const now = new Date();
  if (unit === "days") now.setDate(now.getDate() + n);
  else if (unit === "months") now.setMonth(now.getMonth() + n);
  else if (unit === "years") now.setFullYear(now.getFullYear() + n);
  else return null;
  return now.toISOString();
}

function buildKeyBodyFromForm(form) {
  const scopes = Array.from(form.querySelectorAll('input[name="scopes"]:checked'))
    .map((input) => input.value);
  if (scopes.length === 0) { alert("Pick at least one scope."); return null; }
  const allowedModelsStr = (form.querySelector('input[name="allowed_models"]').value || "");
  let allowedModels;
  if (allowedModelsStr === "") allowedModels = null;
  else if (allowedModelsStr === " ") allowedModels = [];
  else allowedModels = allowedModelsStr.split(",").map((s) => s.trim()).filter(Boolean);
  const amount = form.querySelector('input[name="expires_amount"]').value;
  const unit = form.querySelector('select[name="expires_unit"]').value;
  const expiresAt = calculateExpiry(amount, unit);
  const label = (form.querySelector('input[name="label"]').value || "").trim() || null;
  return { label, scopes, allowed_models: allowedModels, expires_at: expiresAt };
}

export async function createKey(e) {
  const body = buildKeyBodyFromForm(e.target);
  if (!body) return;
  try {
    const result = await api("/keys", { method: "POST", body: JSON.stringify(body) });
    closeKeyForm("self", { target: e.target });
    showPlaintextKey(result.plaintext, result.key);
  } catch (err) { alert("Error: " + err.message); }
}

export async function updateKey(id, e) {
  const body = buildKeyBodyFromForm(e.target);
  if (!body) return;
  try {
    await api("/keys/" + id, { method: "PATCH", body: JSON.stringify(body) });
    closeKeyForm("self", { target: e.target });
    state.apiKeys = await api("/keys");
    rerenderCurrentView();
  } catch (err) { alert("Error: " + err.message); }
}

export async function regenerateKey(id, label) {
  const display = label || ("#" + id);
  if (!confirm(`Regenerate key "${display}"?\n\nThe current key will be invalidated immediately. You'll get a new plaintext key.`)) return;
  try {
    const result = await api(`/keys/${id}/regenerate`, { method: "POST" });
    showPlaintextKey(result.plaintext, result.key);
  } catch (e) { alert("Error: " + e.message); }
}

export async function revokeKey(id, label) {
  const display = label || ("#" + id);
  if (!confirm(`Revoke key "${display}"?\n\nThe key will be deactivated immediately. Any client using it will get 401 errors. You can re-enable it later by editing the row.`)) return;
  try {
    await api(`/keys/${id}/revoke`, { method: "POST" });
    state.apiKeys = await api("/keys");
    rerenderCurrentView();
  } catch (e) { alert("Error: " + e.message); }
}

export function viewKeyUsage(id) {
  location.hash = `#/keys/${id}/usage`;
}

export async function deleteKey(id, label) {
  const display = label || ("#" + id);
  if (!confirm(`Delete key "${display}"?\n\nThis is irreversible. Historical usage rows will keep the api_key_id but the key row itself will be gone.`)) return;
  try {
    await api(`/keys/${id}`, { method: "DELETE" });
    state.apiKeys = (state.apiKeys || []).filter((k) => k.id !== id);
    rerenderCurrentView();
  } catch (e) { alert("Error: " + e.message); }
}
