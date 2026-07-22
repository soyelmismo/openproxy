// handlers/key-handlers.ts — API key CRUD: create, edit, regen,
// revoke, delete, toggle expiry, build body. The create/edit
// modal is rendered here with lit-html. The form is submitted via
// a direct `@submit` handler (closure-captured `wrapper`).
//
// Per spec §3 + §13.8 we do not attach to `window.*`. Functions
// are exported and registered in handlers/registry.ts.
//
// Migrated to lit-html: the create/edit modal is rendered into a
// wrapper `<div>` under `#modal-root` via `render()`. All
// `data-action` attributes have been replaced with direct
// `@click` / `@submit` / `@change` handlers; lit-html auto-escapes
// the model id / label / scopes so we no longer call `escapeHtml` /
// `escapeAttr`.

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { html, render, type TemplateResult } from "lit-html";
import { showPlaintextKey } from "../components/key-display.js";
import { renderAllowedModelsChips } from "../components/model-picker.js";
import type { Model, ApiKeyId } from "../lib/types/api.js";
import { requestUpdate } from "../state/reactive.js";
import { showToast } from "../components/toast.js";
import { ensureModalRoot } from "../lib/ui-utils.js";

interface KeyRow {
  id: ApiKeyId;
  label?: string | null;
  scopes?: string[];
  allowed_models?: string[];
  expires_at?: string | null;
}

interface KeyBody {
  label: string | null;
  scopes: string[];
  allowed_models: string[] | null;
  expires_at: string | null;
}

interface KeyPlaintextResponse {
  plaintext: string;
  key: { label?: string | null; key_prefix?: string | null };
}

function formatExpiryAmount(iso: string): string {
  if (!iso) return "";
  const ms = new Date(iso).getTime() - new Date().getTime();
  if (!isFinite(ms) || ms < 0) return "";
  const days = Math.floor(ms / (1000 * 60 * 60 * 24));
  if (days >= 365) return String(Math.floor(days / 365));
  if (days >= 30) return String(Math.floor(days / 30));
  return String(Math.max(1, days));
}

function keyFormTemplate({ mode, key, wrapper }: { mode: "create" | "edit"; key?: KeyRow; wrapper: HTMLElement }): TemplateResult {
  const isEdit = mode === "edit" && key;
  const labelVal = isEdit ? (key!.label || "") : "";
  const scopes: string[] = isEdit ? (key!.scopes || []) : ["chat"];
  let allowedModelsValue = "";
  if (isEdit && Array.isArray(key!.allowed_models)) {
    allowedModelsValue = key!.allowed_models.length === 0 ? " " : key!.allowed_models.join(",");
  }
  const safeKey: KeyRow = key || { id: 0 };
  const title = isEdit ? `Edit API key #${safeKey.id}` : "Create API key";
  const submitLabel = isEdit ? "Save" : "Create key";
  const expiresAt = isEdit ? key!.expires_at : null;
  const expiryAmountVal = (isEdit && expiresAt) ? formatExpiryAmount(expiresAt) : "";
  const submitHandler = (e: Event): void => {
    e.preventDefault();
    if (isEdit) void updateKey(safeKey.id as number, e, wrapper);
    else void createKey(e, wrapper);
  };
  return html`
    <div class="modal-bg"
         @click=${(e: Event) => { if (e.target === e.currentTarget) wrapper.remove(); }}>
      <div class="modal">
        <div class="modal-header">
          <h2>${title}</h2>
          <button type="button" class="close-btn" @click=${() => wrapper.remove()} aria-label="Close">&times;</button>
        </div>
        <form @submit=${submitHandler}>
          <div class="modal-body">
            <div class="field">
              <label for="key-label">Label</label>
              <input id="key-label" name="label" type="text" placeholder="my-app" .value=${labelVal} required>
            </div>
            <div class="field">
              <span class="field-label">Scopes</span>
              <div class="scopes-list">
                <label class="scope-card">
                  <input type="checkbox" name="scopes" value="chat" .checked=${scopes.includes("chat")}>
                  <span class="scope-name">chat</span>
                  <span class="scope-desc">Can use /v1/chat/completions</span>
                </label>
                <label class="scope-card">
                  <input type="checkbox" name="scopes" value="manage" .checked=${scopes.includes("manage")}>
                  <span class="scope-name">manage</span>
                  <span class="scope-desc">Can use /admin/* (CRUD providers, accounts, etc.)</span>
                </label>
                <label class="scope-card">
                  <input type="checkbox" name="scopes" value="read" .checked=${scopes.includes("read")}>
                  <span class="scope-name">read</span>
                  <span class="scope-desc">Can use analytics endpoints (GET only)</span>
                </label>
              </div>
            </div>
            <div class="field">
              <span class="field-label">Allowed models (empty = all)</span>
              <div class="model-picker-display" id="model-picker-display">
                <span class="muted">Loading...</span>
              </div>
              <input type="hidden" name="allowed_models" .value=${allowedModelsValue}>
            </div>
            <div class="field">
              <label for="key-expires-amount">Expires in</label>
              <div class="expiry-row">
                <input id="key-expires-amount" type="number" name="expires_amount" min="1" max="999" placeholder="30"
                       .value=${expiryAmountVal}
                       ?disabled=${!isEdit || !expiresAt}>
                <select name="expires_unit" @change=${toggleExpiryAmount}>
                  <option value="days" ?selected=${!!(isEdit && expiresAt)}>days</option>
                  <option value="months" ?selected=${!isEdit || !expiresAt}>months</option>
                  <option value="years">years</option>
                  <option value="never" ?selected=${!isEdit || !expiresAt}>never</option>
                </select>
              </div>
            </div>
          </div>
          <div class="modal-footer">
            <button type="button" @click=${() => wrapper.remove()}>Cancel</button>
            <button type="submit" class="primary">${submitLabel}</button>
          </div>
        </form>
      </div>
    </div>
  `;
}

export async function showCreateKey(): Promise<void> {
  if (!state.modelsComplete) {
    state.models = await api("/models") as Model[];
    state.modelsComplete = true;
  }
  const wrapper = document.createElement("div");
  ensureModalRoot().appendChild(wrapper);
  render(keyFormTemplate({ mode: "create", wrapper }), wrapper);
  renderAllowedModelsChips();
}

export async function showEditKey(id: number): Promise<void> {
  if (!state.modelsComplete) {
    state.models = await api("/models") as Model[];
    state.modelsComplete = true;
  }
  let key: KeyRow;
  try { key = await api("/keys/" + id) as KeyRow; }
  catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    alert("Error: " + msg);
    return;
  }
  const wrapper = document.createElement("div");
  ensureModalRoot().appendChild(wrapper);
  render(keyFormTemplate({ mode: "edit", key, wrapper }), wrapper);
  renderAllowedModelsChips();
}

// Closes the key form modal. The optional first arg is the legacy
// `data-arg1="self"` placeholder; the second arg is the DOM event
// from the central data-action shim. Both are now unused — we
// remove any key-form modal-bg in the DOM.
export function closeKeyForm(_selfPlaceholder?: string, e?: Event | null): void {
  const target = e && e.target ? e.target : null;
  const modalBg = target instanceof Element ? target.closest(".modal-bg") : null;
  if (modalBg) {
    const wrapper = modalBg.parentElement;
    modalBg.remove();
    if (wrapper && wrapper.children.length === 0 && wrapper.parentElement?.id === "modal-root") wrapper.remove();
  } else {
    // Fallback: remove all key-form modals (should only be one).
    document.querySelectorAll(".modal-bg").forEach((el) => {
      if (el.querySelector("form")) el.remove();
    });
  }
  const picker = document.getElementById("model-picker-modal");
  if (picker) picker.style.display = "none";
}

export function toggleExpiryAmount(e: Event | null): void {
  const target = e && e.target ? e.target : null;
  if (!(target instanceof HTMLElement)) return;
  const select = target as HTMLSelectElement;
  const row = select.parentElement;
  if (!row) return;
  const amount = row.querySelector('input[name="expires_amount"]') as HTMLInputElement | null;
  if (!amount) return;
  amount.disabled = select.value === "never";
  if (select.value === "never") amount.value = "";
}

function calculateExpiry(amount: string, unit: string): string | null {
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

function buildKeyBodyFromForm(form: HTMLFormElement): KeyBody | null {
  const scopes: string[] = Array.from(form.querySelectorAll<HTMLInputElement>('input[name="scopes"]:checked'))
    .map((input) => input.value);
  if (scopes.length === 0) { showToast("Pick at least one scope.", "error"); return null; }
  const allowedModelsEl = form.querySelector<HTMLInputElement>('input[name="allowed_models"]');
  const allowedModelsStr = allowedModelsEl ? allowedModelsEl.value : "";
  let allowedModels: string[] | null;
  if (allowedModelsStr === "") allowedModels = null;
  else if (allowedModelsStr === " ") allowedModels = [];
  else allowedModels = allowedModelsStr.split(",").map((s) => s.trim()).filter(Boolean);
  const amountEl = form.querySelector<HTMLInputElement>('input[name="expires_amount"]');
  const unitEl = form.querySelector<HTMLSelectElement>('select[name="expires_unit"]');
  const amount = amountEl ? amountEl.value : "";
  const unit = unitEl ? unitEl.value : "never";
  const expiresAt = calculateExpiry(amount, unit);
  const labelEl = form.querySelector<HTMLInputElement>('input[name="label"]');
  const labelRaw = labelEl ? labelEl.value : "";
  const label: string | null = (labelRaw || "").trim() || null;
  return { label, scopes, allowed_models: allowedModels, expires_at: expiresAt };
}

export async function createKey(e: Event, wrapper?: HTMLElement): Promise<void> {
  const target = e.target;
  if (!(target instanceof HTMLFormElement)) return;
  const body = buildKeyBodyFromForm(target);
  if (!body) return;
  try {
    const result = await api("/keys", { method: "POST", body: JSON.stringify(body) }) as KeyPlaintextResponse;
    if (wrapper) wrapper.remove();
    else closeKeyForm("self", { target } as unknown as Event);
    showPlaintextKey(result.plaintext, result.key);
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    alert("Error: " + msg);
  }
}

export async function updateKey(id: number, e: Event, wrapper?: HTMLElement): Promise<void> {
  const target = e.target;
  if (!(target instanceof HTMLFormElement)) return;
  const body = buildKeyBodyFromForm(target);
  if (!body) return;
  try {
    await api("/keys/" + id, { method: "PATCH", body: JSON.stringify(body) });
    if (wrapper) wrapper.remove();
    else closeKeyForm("self", { target } as unknown as Event);
    state.apiKeys = await api("/keys") as typeof state.apiKeys;
    requestUpdate();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    alert("Error: " + msg);
  }
}

export async function regenerateKey(id: number, label: string | null): Promise<void> {
  const display = label || ("#" + id);
  if (!confirm(`Regenerate key "${display}"?\n\nThe current key will be invalidated immediately. You'll get a new plaintext key.`)) return;
  try {
    const result = await api(`/keys/${id}/regenerate`, { method: "POST" }) as KeyPlaintextResponse;
    showPlaintextKey(result.plaintext, result.key);
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    alert("Error: " + msg);
  }
}

export async function revokeKey(id: number, label: string | null): Promise<void> {
  const display = label || ("#" + id);
  if (!confirm(`Revoke key "${display}"?\n\nThe key will be deactivated immediately. Any client using it will get 401 errors. You can re-enable it later by editing the row.`)) return;
  try {
    await api(`/keys/${id}/revoke`, { method: "POST" });
    state.apiKeys = await api("/keys") as typeof state.apiKeys;
    requestUpdate();
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    alert("Error: " + msg);
  }
}

export function viewKeyUsage(id: number): void {
  location.hash = `#/keys/${id}/usage`;
}

export async function deleteKey(id: number, label: string | null): Promise<void> {
  const display = label || ("#" + id);
  if (!confirm(`Delete key "${display}"?\n\nThis is irreversible. Historical usage rows will keep the api_key_id but the key row itself will be gone.`)) return;
  try {
    await api(`/keys/${id}`, { method: "DELETE" });
    state.apiKeys = (state.apiKeys || []).filter((k) => (k as { id: number }).id !== id);
    requestUpdate();
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    alert("Error: " + msg);
  }
}
