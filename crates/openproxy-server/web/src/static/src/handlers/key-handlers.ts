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
import { clearToken, isCurrentSessionKey } from "../state/auth.js";
import { html, render, type TemplateResult } from "lit-html";
import { showPlaintextKey } from "../components/key-display.js";
import {
  renderAllowedModelsChips,
  renderBlacklistedModelsChips,
  renderAllowedCombosChips,
  getAvailableProviders,
} from "../components/model-picker.js";
import type { Model, Provider, ApiKeyId, KeyQuotaCaps } from "../lib/types/api.js";
import { ensureCombosLoaded } from "../state/models-sync.js";
import { requestUpdate } from "../state/reactive.js";
import { showToast } from "../components/toast.js";
import { ensureModalRoot, showApiError } from "../lib/ui-utils.js";
import { showConfirm } from "../lib/show-confirm.js";
import { mutateAndRefresh } from "../lib/mutate.js";
import { beginButtonBusy, resolveBusyButton } from "../lib/button-busy.js";

interface KeyRow {
  id: ApiKeyId;
  label?: string | null;
  scopes?: string[];
  allowed_models?: string[] | null;
  allowed_combos?: number[] | null;
  blacklisted_providers?: string[] | null;
  allowed_providers?: string[] | null;
  blacklisted_models?: string[] | null;
  only_whitelisted?: boolean;
  expires_at?: string | null;
}

interface KeyBody {
  label: string | null;
  scopes: string[];
  allowed_models: string[] | null;
  allowed_combos: number[] | null;
  blacklisted_providers: string[] | null;
  allowed_providers: string[] | null;
  blacklisted_models: string[] | null;
  only_whitelisted: boolean;
  expires_at: string | null;
  /** Dollar quotas. `null` = remove/unlimited; object = upsert caps. */
  quota?: KeyQuotaCaps | null;
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

function onAddBlacklistProvider(e: Event): void {
  const select = e.target as HTMLSelectElement | null;
  if (!select || !select.value) return;
  const prov = select.value;
  const input = document.querySelector<HTMLInputElement>('input[name="blacklisted_providers"]');
  if (input) {
    const current = input.value.split(",").map((s) => s.trim()).filter(Boolean);
    if (!current.includes(prov)) {
      current.push(prov);
      input.value = current.join(", ");
    }
  }
  select.value = "";
}

function onAddWhitelistProvider(e: Event): void {
  const select = e.target as HTMLSelectElement | null;
  if (!select || !select.value) return;
  const prov = select.value;
  const input = document.querySelector<HTMLInputElement>('input[name="allowed_providers"]');
  if (input) {
    const current = input.value.split(",").map((s) => s.trim()).filter(Boolean);
    if (!current.includes(prov)) {
      current.push(prov);
      input.value = current.join(", ");
    }
  }
  select.value = "";
}

export function addCustomBlacklistedModel(): void {
  const customInput = document.getElementById("key-bl-models-custom") as HTMLInputElement | null;
  if (!customInput || !customInput.value.trim()) return;
  const pattern = customInput.value.trim();
  const hidden = document.querySelector<HTMLInputElement>('input[name="blacklisted_models"]');
  if (hidden) {
    const current = hidden.value.split(",").map((s) => s.trim()).filter(Boolean);
    if (!current.includes(pattern)) {
      current.push(pattern);
      hidden.value = current.join(",");
    }
  }
  customInput.value = "";
  renderBlacklistedModelsChips();
}

/** Enable/disable the sibling dollar amount when a quota window is toggled. */
function onQuotaToggle(e: Event): void {
  const target = e.target;
  if (!(target instanceof HTMLInputElement)) return;
  const row = target.closest(".key-form-inline-row");
  const amount = row?.querySelector('input[type="number"]') as HTMLInputElement | null;
  if (!amount) return;
  amount.disabled = !target.checked;
  if (!target.checked) amount.value = "";
}

export function keyFormTemplate({ mode, key, quota, wrapper, inline = false, onSubmit, onCancel }: {
  mode: "create" | "edit";
  key?: KeyRow;
  /** Existing dollar-quota caps (edit mode prefill); null = unlimited. */
  quota?: KeyQuotaCaps | null;
  wrapper: HTMLElement;
  inline?: boolean;
  onSubmit?: (e: Event) => void;
  onCancel?: () => void;
}): TemplateResult {
  const isEdit = mode === "edit" && key;
  const labelVal = isEdit ? (key!.label || "") : "";
  const scopes: string[] = isEdit ? (key!.scopes || []) : ["chat"];
  const fiveHourOn = !!(quota && quota.five_hour_usd_cap !== null && quota.five_hour_usd_cap !== undefined);
  const weekOn = !!(quota && quota.weekly_usd_cap !== null && quota.weekly_usd_cap !== undefined);
  const monthOn = !!(quota && quota.monthly_usd_cap !== null && quota.monthly_usd_cap !== undefined);
  const fiveHourVal = fiveHourOn ? String(quota!.five_hour_usd_cap) : "";
  const weekVal = weekOn ? String(quota!.weekly_usd_cap) : "";
  const monthVal = monthOn ? String(quota!.monthly_usd_cap) : "";
  let allowedModelsValue = "";
  if (isEdit && Array.isArray(key!.allowed_models)) {
    allowedModelsValue = key!.allowed_models.length === 0 ? " " : key!.allowed_models.join(",");
  }
  let allowedCombosValue = "";
  if (isEdit && Array.isArray(key!.allowed_combos)) {
    allowedCombosValue = key!.allowed_combos.length === 0 ? " " : key!.allowed_combos.join(",");
  }
  const blProvidersVal = isEdit && Array.isArray(key!.blacklisted_providers)
    ? key!.blacklisted_providers.join(", ")
    : "";
  const allowedProvidersVal = isEdit && Array.isArray(key!.allowed_providers)
    ? key!.allowed_providers.join(", ")
    : "";
  let blModelsVal = "";
  if (isEdit && Array.isArray(key!.blacklisted_models)) {
    blModelsVal = key!.blacklisted_models.join(",");
  }
  const onlyWhitelisted = isEdit && key!.only_whitelisted === true;
  const availableProviders = getAvailableProviders();
  const safeKey: KeyRow = key || { id: 0 };
  const title = isEdit ? `Edit API key #${safeKey.id}` : "Create API key";
  const submitLabel = isEdit ? "Save" : "Create key";
  const expiresAt = isEdit ? key!.expires_at : null;
  const expiryAmountVal = (isEdit && expiresAt) ? formatExpiryAmount(expiresAt) : "";
  const submitHandler = (e: Event): void => {
    e.preventDefault();
    if (onSubmit) { onSubmit(e); return; }
    if (isEdit) void updateKey(safeKey.id as number, e, wrapper);
    else void createKey(e, wrapper);
  };
  const cancel = (): void => { if (onCancel) onCancel(); else wrapper.remove(); };
  return html`
    <div class=${inline ? "key-properties-form key-form" : "modal-bg"}
         @click=${(e: Event) => { if (!inline && e.target === e.currentTarget) cancel(); }}>
      <div class=${inline ? "key-properties-content key-form-inline" : "modal modal-key-editor"}>
        ${inline ? html`` : html`<div class="modal-header">
          <h2>${title}</h2>
          <button type="button" class="close-btn" @click=${cancel} aria-label="Close">&times;</button>
        </div>`}
        <form class="key-form" @submit=${submitHandler}>
          <div class=${inline ? "key-properties-fields key-form-body" : "modal-body key-form-body"}>
            <section class="key-form-section" aria-labelledby="key-section-identity">
              <div class="key-form-section-head">
                ${inline
                  ? html`<h2 class="key-form-section-title" id="key-section-identity">Identity</h2>`
                  : html`<h3 class="key-form-section-title" id="key-section-identity">Identity</h3>`}
                <p class="key-form-section-hint">Name and lifetime</p>
              </div>
              <div class="key-form-grid key-form-grid--identity">
                <div class="field">
                  <label for="key-label">Label</label>
                  <input id="key-label" name="label" type="text" placeholder="my-app" .value=${labelVal} required>
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
            </section>
            <section class="key-form-section" aria-labelledby="key-section-scopes">
              <div class="key-form-section-head">
                ${inline
                  ? html`<h2 class="key-form-section-title" id="key-section-scopes">Scopes</h2>`
                  : html`<h3 class="key-form-section-title" id="key-section-scopes">Scopes</h3>`}
                <p class="key-form-section-hint">At least one</p>
              </div>
              <div class="scopes-list" role="group" aria-labelledby="key-section-scopes">
                <label class="scope-card" title="Can use /v1/chat/completions">
                  <input type="checkbox" name="scopes" value="chat" .checked=${scopes.includes("chat")}>
                  <span class="scope-name">chat</span>
                  <span class="scope-desc">Chat completions</span>
                </label>
                <label class="scope-card" title="Can open the dashboard: analytics of this key's own data, read-only (no manage, no logs)">
                  <input type="checkbox" name="scopes" value="analytics" .checked=${scopes.includes("analytics")}>
                  <span class="scope-name">analytics</span>
                  <span class="scope-desc">Dashboard · read-only</span>
                </label>
                <label class="scope-card" title="Can use /admin/* (CRUD providers, accounts, etc.)">
                  <input type="checkbox" name="scopes" value="manage" .checked=${scopes.includes("manage")}>
                  <span class="scope-name">manage</span>
                  <span class="scope-desc">Admin CRUD</span>
                </label>
                <label class="scope-card" title="Legacy alias of analytics: read-only usage endpoints (GET only)">
                  <input type="checkbox" name="scopes" value="read" .checked=${scopes.includes("read")}>
                  <span class="scope-name">read</span>
                  <span class="scope-desc">Legacy read-only</span>
                </label>
              </div>
            </section>
            <section class="key-form-section" aria-labelledby="key-section-model-access">
              <div class="key-form-section-head">
                ${inline
                  ? html`<h2 class="key-form-section-title" id="key-section-model-access">Model access</h2>`
                  : html`<h3 class="key-form-section-title" id="key-section-model-access">Model access</h3>`}
                <p class="key-form-section-hint">Limit what this key can call</p>
              </div>
              <div class="key-form-grid key-form-grid--access">
                <div class="field">
                  <div class="key-form-field-head">
                    <span class="field-label">Allowed models</span>
                    <span class="key-form-field-note">empty = all</span>
                  </div>
                  <div class="model-picker-display" id="model-picker-display"></div>
                  <input type="hidden" name="allowed_models" .value=${allowedModelsValue}>
                  <label class="scope-card key-form-toggle" title="Restrict this key to the models selected above. Leave off to keep the list additive (the blacklists below still apply).">
                    <input type="checkbox" name="only_whitelisted" .checked=${onlyWhitelisted}>
                    <span class="scope-name">Only these models</span>
                    <span class="scope-desc">strict mode</span>
                  </label>
                </div>
                <div class="field">
                  <div class="key-form-field-head">
                    <span class="field-label">Allowed combos</span>
                    <span class="key-form-field-note">empty = all</span>
                  </div>
                  <div class="model-picker-display" id="allowed-combos-display"></div>
                  <input type="hidden" name="allowed_combos" .value=${allowedCombosValue}>
                </div>
              </div>
            </section>
            <section class="key-form-section" aria-labelledby="key-section-restrictions">
              <div class="key-form-section-head">
                ${inline
                  ? html`<h2 class="key-form-section-title" id="key-section-restrictions">Restrictions</h2>`
                  : html`<h3 class="key-form-section-title" id="key-section-restrictions">Restrictions</h3>`}
                <p class="key-form-section-hint">Optional · comma-separated</p>
              </div>
              <div class="key-form-grid key-form-grid--restrictions">
                <div class="field">
                  <div class="key-form-field-head">
                    <span class="field-label">Allowed providers</span>
                    <span class="key-form-field-note">empty = all</span>
                  </div>
                  <select id="quick-wl-provider-select" aria-label="Add provider to the allowlist" @change=${onAddWhitelistProvider}>
                    <option value="">Add provider…</option>
                    ${availableProviders.map((p) => html`<option value="${p}">${p}</option>`)}
                  </select>
                  <input id="key-allowed-providers" name="allowed_providers" type="text" placeholder="xkiro, openai" .value=${allowedProvidersVal}>
                </div>
                <div class="field">
                  <div class="key-form-field-head">
                    <span class="field-label">Blocked providers</span>
                    <span class="key-form-field-note">empty = none</span>
                  </div>
                  <select id="quick-bl-provider-select" aria-label="Add provider to the blocklist" @change=${onAddBlacklistProvider}>
                    <option value="">Block provider…</option>
                    ${availableProviders.map((p) => html`<option value="${p}">${p}</option>`)}
                  </select>
                  <input id="key-bl-providers" name="blacklisted_providers" type="text" placeholder="openai, groq" .value=${blProvidersVal}>
                </div>
                <div class="field key-form-field--wide">
                  <div class="key-form-field-head">
                    <span class="field-label">Blocked model patterns</span>
                    <span class="key-form-field-note">wildcards ok</span>
                  </div>
                  <div class="model-picker-display" id="blacklisted-models-display"></div>
                  <input type="hidden" name="blacklisted_models" .value=${blModelsVal}>
                  <div class="key-form-inline-row">
                    <input id="key-bl-models-custom" placeholder="gpt-4*, *-preview">
                    <button type="button" class="small" @click=${addCustomBlacklistedModel}>+ Add</button>
                  </div>
                </div>
              </div>
            </section>
            <section class="key-form-section" aria-labelledby="key-section-quotas">
              <div class="key-form-section-head">
                ${inline
                  ? html`<h2 class="key-form-section-title" id="key-section-quotas">Dollar quotas</h2>`
                  : html`<h3 class="key-form-section-title" id="key-section-quotas">Dollar quotas</h3>`}
                <p class="key-form-section-hint">Metered at official model prices · off = unlimited</p>
              </div>
              <div class="key-form-grid key-form-grid--restrictions">
                <div class="field">
                  <div class="key-form-field-head">
                    <span class="field-label">Rolling 5 hours</span>
                    <span class="key-form-field-note">burst guard</span>
                  </div>
                  <div class="key-form-inline-row">
                    <label class="scope-card key-form-toggle">
                      <input type="checkbox" name="quota_5h_enabled" .checked=${fiveHourOn} @change=${onQuotaToggle}>
                      <span class="scope-name">Enable</span>
                    </label>
                    <input name="quota_5h_usd" type="number" min="0" step="0.01" placeholder="1.00" .value=${fiveHourVal} ?disabled=${!fiveHourOn}>
                  </div>
                </div>
                <div class="field">
                  <div class="key-form-field-head">
                    <span class="field-label">Weekly</span>
                    <span class="key-form-field-note">resets Mon 00:00 UTC</span>
                  </div>
                  <div class="key-form-inline-row">
                    <label class="scope-card key-form-toggle">
                      <input type="checkbox" name="quota_week_enabled" .checked=${weekOn} @change=${onQuotaToggle}>
                      <span class="scope-name">Enable</span>
                    </label>
                    <input name="quota_week_usd" type="number" min="0" step="0.01" placeholder="5.00" .value=${weekVal} ?disabled=${!weekOn}>
                  </div>
                </div>
                <div class="field">
                  <div class="key-form-field-head">
                    <span class="field-label">Monthly</span>
                    <span class="key-form-field-note">resets 1st 00:00 UTC</span>
                  </div>
                  <div class="key-form-inline-row">
                    <label class="scope-card key-form-toggle">
                      <input type="checkbox" name="quota_month_enabled" .checked=${monthOn} @change=${onQuotaToggle}>
                      <span class="scope-name">Enable</span>
                    </label>
                    <input name="quota_month_usd" type="number" min="0" step="0.01" placeholder="25.00" .value=${monthVal} ?disabled=${!monthOn}>
                  </div>
                </div>
              </div>
            </section>
          </div>
          <div class=${inline ? "key-properties-footer" : "modal-footer"}>
            <button type="button" @click=${cancel}>Cancel</button>
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
  if (!state.providers || state.providers.length === 0) {
    state.providers = await api("/providers") as Provider[];
  }
  await ensureCombosLoaded();
  const wrapper = document.createElement("div");
  ensureModalRoot().appendChild(wrapper);
  render(keyFormTemplate({ mode: "create", wrapper }), wrapper);
  renderAllowedModelsChips();
  renderBlacklistedModelsChips();
  renderAllowedCombosChips();
}

export async function showEditKey(id: number): Promise<void> {
  location.hash = `#/keys/${id}`;
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

/** Read a `keyForm` from the DOM and produce the JSON body sent to
 *  `/admin/keys` (POST) or `/admin/keys/:id` (PATCH).
 *
 *  Three-state encoding for the blacklist fields mirrors the Rust
 *  PATCH semantics:
 *   - empty input  → `null`     (don't touch the list)
 *   - " "          → `[]`       (explicitly clear every entry)
 *   - non-empty    → trimmed list of distinct entries
 *
 *  `calculateExpiry("never", ...)` returns `null` (clear the expiry)
 *  while any non-empty amount coerces to an ISO-8601 timestamp in
 *  the future. Empty amount → `null` (defensive).
 *
 *  Returns `null` when validation fails (no scopes selected). The
 *  caller is expected to surface the error via `showToast` and bail.
 *
 *  Pure w.r.t. the global DOM (only reads the form) and `Intl`/Date;
 *  no network calls. Exported for direct unit testing in
 *  `key-handlers.test.ts`. */
export function buildKeyBodyFromForm(form: HTMLFormElement): KeyBody | null {
  const scopes: string[] = Array.from(form.querySelectorAll<HTMLInputElement>('input[name="scopes"]:checked'))
    .map((input) => input.value);
  if (scopes.length === 0) { showToast("Pick at least one scope.", "error"); return null; }
  const allowedModelsEl = form.querySelector<HTMLInputElement>('input[name="allowed_models"]');
  const allowedModelsStr = allowedModelsEl ? allowedModelsEl.value : "";
  let allowedModels: string[] | null;
  if (allowedModelsStr === "") allowedModels = null;
  else if (allowedModelsStr === " ") allowedModels = [];
  else allowedModels = allowedModelsStr.split(",").map((s) => s.trim()).filter(Boolean);
  const allowedCombosEl = form.querySelector<HTMLInputElement>('input[name="allowed_combos"]');
  const allowedCombosStr = allowedCombosEl ? allowedCombosEl.value : "";
  let allowedCombos: number[] | null;
  if (allowedCombosStr === "") allowedCombos = null;
  else if (allowedCombosStr === " ") allowedCombos = [];
  else allowedCombos = allowedCombosStr.split(",").map((s) => parseInt(s.trim(), 10)).filter((n) => Number.isFinite(n));
  const blProvidersEl = form.querySelector<HTMLInputElement>('input[name="blacklisted_providers"]');
  const blProvidersStr = blProvidersEl ? blProvidersEl.value.trim() : "";
  let blacklistedProviders: string[] | null = null;
  if (blProvidersStr) {
    blacklistedProviders = blProvidersStr.split(",").map((s) => s.trim()).filter(Boolean);
  }
  const allowedProvidersEl = form.querySelector<HTMLInputElement>('input[name="allowed_providers"]');
  const allowedProvidersStr = allowedProvidersEl ? allowedProvidersEl.value.trim() : "";
  let allowedProviders: string[] | null = null;
  if (allowedProvidersStr) {
    allowedProviders = allowedProvidersStr.split(",").map((s) => s.trim()).filter(Boolean);
  }
  const blModelsEl = form.querySelector<HTMLInputElement>('input[name="blacklisted_models"]');
  const blModelsStr = blModelsEl ? blModelsEl.value.trim() : "";
  let blacklistedModels: string[] | null = null;
  if (blModelsStr) {
    blacklistedModels = blModelsStr.split(",").map((s) => s.trim()).filter(Boolean);
  }
  const amountEl = form.querySelector<HTMLInputElement>('input[name="expires_amount"]');
  const unitEl = form.querySelector<HTMLSelectElement>('select[name="expires_unit"]');
  const amount = amountEl ? amountEl.value : "";
  const unit = unitEl ? unitEl.value : "never";
  const expiresAt = calculateExpiry(amount, unit);
  const labelEl = form.querySelector<HTMLInputElement>('input[name="label"]');
  const labelRaw = labelEl ? labelEl.value : "";
  const label: string | null = (labelRaw || "").trim() || null;
  // Absent checkbox → false: forms rendered before this field existed (and the
  // unit tests that build a minimal form) never send the flag, which keeps the
  // additive semantics as the default.
  const onlyWhitelistedEl = form.querySelector<HTMLInputElement>('input[name="only_whitelisted"]');
  const onlyWhitelisted = onlyWhitelistedEl ? onlyWhitelistedEl.checked : false;
  // Dollar quotas: an enabled window with a positive amount contributes its
  // cap; everything else sends `null` (disabled). All disabled → `null`
  // (remove caps → unlimited).
  let quotaValid = true;
  const quotaNum = (enabledName: string, amountName: string): number | null => {
    const enabledEl = form.querySelector<HTMLInputElement>(`input[name="${enabledName}"]`);
    if (!enabledEl || !enabledEl.checked) return null;
    const amountEl = form.querySelector<HTMLInputElement>(`input[name="${amountName}"]`);
    const v = amountEl ? parseFloat(amountEl.value) : NaN;
    if (!Number.isFinite(v) || v <= 0) {
      showToast("Quota amounts must be positive numbers.", "error");
      quotaValid = false;
      return null;
    }
    return v;
  };
  const fiveHourUsd = quotaNum("quota_5h_enabled", "quota_5h_usd");
  const weeklyUsd = quotaNum("quota_week_enabled", "quota_week_usd");
  const monthlyUsd = quotaNum("quota_month_enabled", "quota_month_usd");
  if (!quotaValid) return null;
  const quota = (monthlyUsd === null && weeklyUsd === null && fiveHourUsd === null)
    ? null
    : {
        monthly_usd_cap: monthlyUsd,
        weekly_usd_cap: weeklyUsd,
        five_hour_usd_cap: fiveHourUsd,
      };
  return {
    label,
    scopes,
    allowed_models: allowedModels,
    allowed_combos: allowedCombos,
    blacklisted_providers: blacklistedProviders,
    allowed_providers: allowedProviders,
    blacklisted_models: blacklistedModels,
    only_whitelisted: onlyWhitelisted,
    expires_at: expiresAt,
    quota,
  };
}

export async function createKey(e: Event, wrapper?: HTMLElement): Promise<void> {
  const target = e.target;
  if (!(target instanceof HTMLFormElement)) return;
  const body = buildKeyBodyFromForm(target);
  if (!body) return;
  const busy = beginButtonBusy(resolveBusyButton(e), { label: "Creating…" });
  try {
    const result = await api("/keys", { method: "POST", body: JSON.stringify(body) }) as KeyPlaintextResponse;
    if (wrapper) wrapper.remove();
    else closeKeyForm("self", e);
    // Show the one-time secret first: a failed list refetch must never
    // delay or drop the only plaintext display. Same refresh pattern
    // as updateKey (plaintext-modal flows stay inline, see regenerateKey).
    showPlaintextKey(result.plaintext, result.key);
    state.apiKeys = await api("/keys") as typeof state.apiKeys;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, "Error");
  } finally {
    busy.release();
  }
}

export async function updateKey(id: number, e: Event, wrapper?: HTMLElement): Promise<void> {
  const target = e.target;
  if (!(target instanceof HTMLFormElement)) return;
  const body = buildKeyBodyFromForm(target);
  if (!body) return;
  const busy = beginButtonBusy(resolveBusyButton(e), { label: "Saving…" });
  try {
    await api("/keys/" + id, { method: "PATCH", body: JSON.stringify(body) });
    if (wrapper) wrapper.remove();
    else closeKeyForm("self", e);
    state.apiKeys = await api("/keys") as typeof state.apiKeys;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, "Error");
  } finally {
    busy.release();
  }
}

export async function regenerateKey(id: number, label: string | null): Promise<void> {
  const display = label || ("#" + id);
  if (!(await showConfirm({
    title: "Regenerate key",
    message: `Regenerate key "${display}"?\n\nThe current key will be invalidated immediately. You'll get a new plaintext key.`,
    danger: true,
    confirmLabel: "Regenerate",
  }))) return;
  const isSelf = isCurrentSessionKey(id);
  try {
    const result = await api(`/keys/${id}/regenerate`, { method: "POST" }) as KeyPlaintextResponse;
    if (isSelf) {
      clearToken();
    }
    showPlaintextKey(result.plaintext, result.key, () => {
      if (isSelf) {
        location.hash = "#/login";
      }
    });
  } catch (e: unknown) {
    showApiError(e, "Error");
  }
}

export async function revokeKey(id: number, label: string | null): Promise<void> {
  const display = label || ("#" + id);
  if (!(await showConfirm({
    title: "Revoke key",
    message: `Revoke key "${display}"?\n\nThe key will be deactivated immediately. Any client using it will get 401 errors. You can re-enable it later by editing the row.`,
    danger: true,
    confirmLabel: "Revoke",
  }))) return;
  const isSelf = isCurrentSessionKey(id);
  await mutateAndRefresh({
    apiCall: async () => {
      await api(`/keys/${id}/revoke`, { method: "POST" });
      if (isSelf) {
        clearToken();
        location.hash = "#/login";
        return;
      }
      state.apiKeys = await api("/keys") as typeof state.apiKeys;
    },
  });
  if (isSelf) {
    clearToken();
    location.hash = "#/login";
  }
}

export function viewKeyUsage(id: number): void {
  location.hash = `#/keys/${id}/usage`;
}

export async function deleteKey(id: number, label: string | null): Promise<void> {
  const display = label || ("#" + id);
  if (!(await showConfirm({
    title: "Delete key",
    message: `Delete key "${display}"?\n\nThis is irreversible. Historical usage rows will keep the api_key_id but the key row itself will be gone.`,
    danger: true,
    confirmLabel: "Delete",
  }))) return;
  const isSelf = isCurrentSessionKey(id);
  await mutateAndRefresh({
    apiCall: async () => {
      await api(`/keys/${id}`, { method: "DELETE" });
      if (isSelf) {
        clearToken();
        location.hash = "#/login";
        return;
      }
      state.apiKeys = (state.apiKeys || []).filter((k) => (k as { id: number }).id !== id);
    },
  });
  if (isSelf) {
    clearToken();
    location.hash = "#/login";
  }
}
