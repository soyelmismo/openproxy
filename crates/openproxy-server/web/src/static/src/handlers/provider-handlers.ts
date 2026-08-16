// handlers/provider-handlers.ts — provider-level handlers.
//
// Per spec §3 + §13.8 we do not attach to `window.*`. Every
// function here is exported by name and registered in
// handlers/registry.ts so the central data-action shim can find
// it.
//
// Migrated to lit-html: the create-provider modal is rendered
// into a wrapper `<div>` under `#modal-root` via `render()`. All
// `data-action` attributes have been replaced with direct
// `@click` / `@submit` handlers; lit-html auto-escapes the
// provider id so we no longer call `escapeHtml` / `escapeAttr`.

import { navigate } from "../state/router.js";
import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { html, render, type TemplateResult } from "lit-html";
import { extractApiErrorMessage } from "../lib/escape.js";
import { syncModelRowActive, updateFilterTabCounts } from "../components/model-table.js";
import { requestUpdate } from "../state/reactive.js";
import { ensureModalRoot, flashButton, showApiError } from "../lib/ui-utils.js";
import { showToast } from "../components/toast.js";

interface RefreshResult {
  models_refreshed?: number;
  new_model_ids?: string[];
}

// POST /admin/providers/:id/refresh — re-discover the model
// list for one provider. The button is disabled and relabeled
// "Refreshing..." while in flight. The optional `e` parameter
// lets the data-action shim pass the triggering element so the
// UI feedback is scoped to the button the user clicked.
export async function refreshProvider(providerId: string, e: Event | null): Promise<void> {
  const target = e && e.target && e.target instanceof HTMLButtonElement ? e.target : null;
  const btn: HTMLButtonElement | null = target;
  const original = btn ? btn.textContent : null;
  if (btn) {
    btn.disabled = true;
    btn.textContent = "Refreshing...";
  }
  try {
    const result = (await api(
      "/providers/" + encodeURIComponent(providerId) + "/refresh",
      { method: "POST" },
    )) as RefreshResult | null;
    const n = (result && typeof result.models_refreshed === "number")
      ? result.models_refreshed
      : 0;
    const newIds: string[] = (result && Array.isArray(result.new_model_ids))
      ? result.new_model_ids
      : [];
    // Compose a toast that surfaces the headline count plus a
    // short list of any newly-discovered model_ids. When the
    // refresh found nothing new (the common case for a steady-
    // state provider) we fall back to the previous "Refreshed N
    // models" wording so the UI doesn't suddenly get chatty.
    const summary = n === 0
      ? `Nothing to refresh for ${providerId}.`
      : `Refreshed ${n} models for ${providerId}.`;
    const newSuffix = newIds.length === 0
      ? ""
      : newIds.length <= 3
        ? ` New: ${newIds.join(", ")}.`
        : ` New: ${newIds.slice(0, 3).join(", ")} (+${newIds.length - 3} more).`;
    showToast(summary + newSuffix, "success");
    // Force a refetch instead of relying on the polling interval —
    // the user explicitly asked for fresh data.
    state.providers = await api("/providers") as typeof state.providers;
    state.models = await api("/models?provider_id=" + encodeURIComponent(providerId)) as typeof state.models;
    state.modelsComplete = false;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, "Error");
  } finally {
    if (btn) {
      btn.disabled = false;
      btn.textContent = original;
    }
  }
}

// Walk every provider and POST to its /refresh endpoint. Per-
// provider failures are logged but don't abort the loop — a
// single misbehaving upstream shouldn't block the rest.
export async function refreshAllProviders(): Promise<void> {
  try {
    const providers = await api("/providers") as Array<{ id: string }>;
    for (const p of providers) {
      try {
        await api("/providers/" + encodeURIComponent(p.id) + "/refresh", { method: "POST" });
      } catch (err: unknown) {
        console.error("Failed to refresh", p.id, err);
      }
    }
    state.providers = await api("/providers") as typeof state.providers;
    state.models = await api("/models") as typeof state.models;
    state.modelsComplete = true;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, "Error");
  }
}

// ===== Create provider =====

function createProviderTemplate(wrapper: HTMLElement): TemplateResult {
  return html`
    <div class="modal-bg" id="create-provider-modal"
         @click=${(e: Event) => { if (e.target === e.currentTarget) wrapper.remove(); }}>
      <div class="modal">
        <div class="modal-header">
          <h2>New provider</h2>
          <button type="button" class="close-btn" @click=${() => wrapper.remove()} aria-label="Close">&times;</button>
        </div>
        <form @submit=${(e: Event) => { e.preventDefault(); void createProvider(e, wrapper); }}>
          <div class="modal-body">
            <div class="field">
              <label for="provider-id">ID</label>
              <input id="provider-id" name="id" type="text" required placeholder="openrouter">
            </div>
            <div class="field">
              <label for="provider-name">Name</label>
              <input id="provider-name" name="name" type="text" required placeholder="OpenRouter">
            </div>
            <div class="field">
              <label for="provider-base-url">Base URL</label>
              <input id="provider-base-url" name="base_url" type="text" required placeholder="https://openrouter.ai/api/v1">
            </div>
            <div class="field">
              <label for="provider-auth">Auth</label>
              <select id="provider-auth" name="auth_type">
                <option value="bearer">bearer</option>
                <option value="x-api-key">x-api-key</option>
              </select>
            </div>
            <div class="field">
              <label for="provider-format">Format</label>
              <select id="provider-format" name="format">
                <option value="openai">openai</option>
                <option value="anthropic">anthropic</option>
                <option value="mixed">mixed</option>
              </select>
            </div>
            <div class="field">
              <label for="provider-extra-headers">Extra Headers (JSON)</label>
              <textarea id="provider-extra-headers" name="extra_headers_json" rows="3" placeholder='{"User-Agent": "Mozilla/5.0...", "Origin": "https://..."}' style="width: 100%; font-family: monospace; font-size: 0.85em; resize: vertical;"></textarea>
            </div>
          </div>
          <div class="modal-footer">
            <button type="button" @click=${() => wrapper.remove()}>Cancel</button>
            <button type="submit" class="primary">Create</button>
          </div>
        </form>
      </div>
    </div>
  `;
}

export function showCreateProvider(): void {
  const wrapper = document.createElement("div");
  ensureModalRoot().appendChild(wrapper);
  render(createProviderTemplate(wrapper), wrapper);
}

export function closeCreateProvider(): void {
  const m = document.getElementById("create-provider-modal");
  if (m) {
    const wrapper = m.parentElement;
    m.remove();
    if (wrapper && wrapper.children.length === 0 && wrapper.parentElement?.id === "modal-root") {
      wrapper.remove();
    }
  }
}

export async function createProvider(e: Event, wrapper?: HTMLElement): Promise<void> {
  const target = e.target;
  if (!(target instanceof HTMLFormElement)) return;
  const f = new FormData(target);
  const entries = Object.fromEntries(f);
  const extraHeaders = entries["extra_headers_json"];
  if (typeof extraHeaders === "string" && extraHeaders.trim() === "") {
    delete entries["extra_headers_json"];
  }
  try {
    await api("/providers", {
      method: "POST",
      body: JSON.stringify(entries),
    });
    // Close the modal FIRST — the POST succeeded, the provider is
    // persisted. If the subsequent GET refresh fails (transient
    // network blip), the next bg-poll will pick up the new row.
    if (wrapper) wrapper.remove(); else closeCreateProvider();
    state.providers = await api("/providers") as typeof state.providers;
    navigate();
  } catch (err: unknown) {
    showApiError(err, "Error");
  }
}

// ===== Delete provider =====

// Soft-confirm path: kept because some callers (and old URLs)
// still hit `window.deleteProvider`. The dashboard's "Delete"
// button uses `confirmDeleteProvider` (two-step: type the id).
export async function deleteProvider(id: string): Promise<void> {
  if (!confirm(`Delete provider ${id}? This will cascade-delete its accounts and models.`)) return;
  try {
    await api("/providers/" + encodeURIComponent(id), { method: "DELETE" });
    state.providers = state.providers.filter((p) => p.id !== id);
    state.models = state.models.filter((m) => m.provider_id !== id);
    state.accounts = state.accounts.filter((a) => a.provider_id !== id);
    navigate();
  } catch (err: unknown) {
    showApiError(err, "Error");
  }
}

// Two-step confirmation: type the provider id verbatim, then a
// final "Really?" prompt. The typed step is enough friction to
// catch most misclicks. The second step is a plain confirm for
// the final go-ahead.
export async function confirmDeleteProvider(providerId: string): Promise<void> {
  const typed = prompt(`Type the provider ID to confirm deletion: ${providerId}`);
  if (typed !== providerId) {
    if (typed != null) {
      alert(`Provider id "${typed}" does not match. Nothing was deleted.`);
    }
    return;
  }
  if (!confirm(`Really delete ${providerId}? This cascades to all its accounts and models.`)) return;
  try {
    await api("/providers/" + encodeURIComponent(providerId), { method: "DELETE" });
    state.providers = state.providers.filter((p) => p.id !== providerId);
    state.models = state.models.filter((m) => m.provider_id !== providerId);
    state.accounts = state.accounts.filter((a) => a.provider_id !== providerId);
    // The user just deleted the provider they're looking at: send
    // them back to the providers grid.
    location.hash = "#/providers";
  } catch (err: unknown) {
    // The server returns `{"error": {"code", "message"}}` for a
    // 4xx. The most common rejection on this path is a built-in
    // (which the UI normally hides via the "🔒 Delete (built-in)"
    // button, but the server is the source of truth and might
    // reject for any other validation reason). Show the message
    // verbatim so the operator sees "cannot be deleted. Use
    // POST .../active to deactivate it" instead of a generic
    // "Error: 400: ...".
    const friendly = extractApiErrorMessage(err) || (err instanceof Error ? err.message : String(err));
    alert("Cannot delete: " + friendly);
  }
}

// ===== Toggle active / rename =====

// Deactivating a provider is the soft, reversible alternative to
// deleting it: the row stays in the DB (accounts and models
// preserved), and reactivation brings everything back. The
// button just flips `active` via the dedicated endpoint.
//
// Reactivation skips the confirm — going from "off" to "on" is
// safe and the user clearly intended it by clicking "Activate".
export async function toggleProviderActive(providerId: string, newActive: boolean): Promise<void> {
  if (!newActive) {
    const ok = confirm(
      `Deactivate provider "${providerId}"?\n\n` +
      `Its accounts and models will be preserved, but it won't be ` +
      `usable in combos until you reactivate it.`
    );
    if (!ok) return;
  }
  try {
    await api("/providers/" + encodeURIComponent(providerId) + "/active", {
      method: "POST",
      body: JSON.stringify({ active: newActive }),
    });
    // Refetch providers so the card / detail reflects the new state.
    state.providers = await api("/providers") as typeof state.providers;
    navigate();
  } catch (err: unknown) {
    showApiError(err, "Error");
  }
}

// The `name` field is a *display* label — the `id` is the slug
// used in URLs and FKs, so the rename only touches `name`. PATCH
// `/admin/providers/:id` already exists in the backend, this
// is just the UX.
export async function renameProviderPrompt(providerId: string, currentName: string): Promise<void> {
  const newName = prompt(`Rename provider "${providerId}":`, currentName);
  if (newName == null) return; // cancel
  const trimmed = newName.trim();
  if (trimmed === "") {
    showToast("Name cannot be empty", "error");
    return;
  }
  if (trimmed === currentName) return; // no-op

  // Names are not unique in the schema (only ids are), so a name
  // collision is allowed — we just warn so the operator can notice.
  const collision = state.providers.find(
    (p) => p.id !== providerId && p.name === trimmed,
  );
  if (collision) {
    const ok = confirm(
      `A provider with this name already exists (${collision.id}). ` +
      `Use this name anyway?`
    );
    if (!ok) return;
  }

  try {
    await api("/providers/" + encodeURIComponent(providerId), {
      method: "PATCH",
      body: JSON.stringify({ name: trimmed }),
    });
    state.providers = await api("/providers") as typeof state.providers;
    navigate();
  } catch (err: unknown) {
    showApiError(err, "Error");
  }
}

export async function editProviderEndpointPrompt(providerId: string, currentBaseUrl: string): Promise<void> {
  const newUrl = prompt(`Edit endpoint (base URL) for provider "${providerId}":\n(e.g., https://api.openai.com/v1)`, currentBaseUrl);
  if (newUrl == null) return; // cancel
  const trimmed = newUrl.trim();
  if (trimmed === "") {
    showToast("Base URL cannot be empty", "error");
    return;
  }
  if (trimmed === currentBaseUrl) return; // no-op

  try {
    await api("/providers/" + encodeURIComponent(providerId), {
      method: "PATCH",
      body: JSON.stringify({ base_url: trimmed }),
    });
    state.providers = await api("/providers") as typeof state.providers;
    showToast("Provider endpoint updated", "success");
    navigate();
  } catch (err: unknown) {
    showApiError(err, "Error");
  }
}

interface HeaderRow {
  key: string;
  value: string;
}

export function showEditProviderHeaders(
  providerId: string,
  currentHeadersJson: string | null | undefined
): void {
  const root = ensureModalRoot();
  const wrapper = document.createElement("div");
  root.appendChild(wrapper);

  let initialRows: HeaderRow[] = [];
  if (currentHeadersJson && currentHeadersJson.trim() !== "") {
    try {
      const parsed = JSON.parse(currentHeadersJson.trim());
      if (typeof parsed === "object" && parsed !== null && !Array.isArray(parsed)) {
        initialRows = Object.entries(parsed).map(([k, v]) => ({
          key: k,
          value: typeof v === "string" ? v : JSON.stringify(v),
        }));
      }
    } catch {
      // Ignored: fallback to empty visual rows
    }
  }

  let rows: HeaderRow[] = [...initialRows];
  let rawMode = false;
  let rawJson = currentHeadersJson ? JSON.stringify(JSON.parse(currentHeadersJson), null, 2) : "";
  let errorMsg: string | null = null;
  let isSaving = false;

  const presets = [
    {
      label: "+ User-Agent (Chrome)",
      key: "User-Agent",
      value:
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
    },
    { label: "+ Accept (JSON)", key: "Accept", value: "application/json" },
    { label: "+ Origin", key: "Origin", value: "https://example.com" },
    { label: "+ Referer", key: "Referer", value: "https://example.com" },
    { label: "+ Authorization", key: "Authorization", value: "Bearer your-token-here" },
  ];

  function syncRowsToRaw(): void {
    const obj: Record<string, string> = {};
    for (const r of rows) {
      if (r.key.trim()) {
        obj[r.key.trim()] = r.value;
      }
    }
    rawJson = Object.keys(obj).length > 0 ? JSON.stringify(obj, null, 2) : "";
  }

  function syncRawToRows(): boolean {
    const trimmed = rawJson.trim();
    if (trimmed === "") {
      rows = [];
      errorMsg = null;
      return true;
    }
    try {
      const parsed = JSON.parse(trimmed);
      if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
        errorMsg = "JSON must be an object with key-value pairs";
        return false;
      }
      rows = Object.entries(parsed).map(([k, v]) => ({
        key: k,
        value: typeof v === "string" ? v : JSON.stringify(v),
      }));
      errorMsg = null;
      return true;
    } catch (e: unknown) {
      errorMsg = "Invalid JSON syntax: " + (e instanceof Error ? e.message : String(e));
      return false;
    }
  }

  function updateView(): void {
    render(template(), wrapper);
  }

  function addRow(key = "", value = ""): void {
    rows.push({ key, value });
    syncRowsToRaw();
    errorMsg = null;
    updateView();
  }

  function removeRow(index: number): void {
    rows.splice(index, 1);
    syncRowsToRaw();
    errorMsg = null;
    updateView();
  }

  function updateRowKey(index: number, newKey: string): void {
    if (rows[index]) {
      rows[index].key = newKey;
      syncRowsToRaw();
    }
  }

  function updateRowValue(index: number, newVal: string): void {
    if (rows[index]) {
      rows[index].value = newVal;
      syncRowsToRaw();
    }
  }

  function applyPreset(preset: { key: string; value: string }): void {
    const existing = rows.find(
      (r) => r.key.trim().toLowerCase() === preset.key.toLowerCase()
    );
    if (existing) {
      existing.value = preset.value;
    } else {
      rows.push({ key: preset.key, value: preset.value });
    }
    syncRowsToRaw();
    errorMsg = null;
    updateView();
  }

  function toggleMode(): void {
    if (!rawMode) {
      syncRowsToRaw();
      rawMode = true;
    } else {
      if (!syncRawToRows()) {
        updateView();
        return;
      }
      rawMode = false;
    }
    updateView();
  }

  async function handleSave(): Promise<void> {
    if (rawMode) {
      if (!syncRawToRows()) {
        updateView();
        return;
      }
    }

    const obj: Record<string, string> = {};
    for (const r of rows) {
      const k = r.key.trim();
      if (!k) continue;
      obj[k] = r.value;
    }

    const payload = Object.keys(obj).length > 0 ? JSON.stringify(obj) : null;

    isSaving = true;
    errorMsg = null;
    updateView();

    try {
      await api("/providers/" + encodeURIComponent(providerId), {
        method: "PATCH",
        body: JSON.stringify({ extra_headers_json: payload }),
      });
      state.providers = (await api("/providers")) as typeof state.providers;
      showToast("Provider headers updated", "success");
      wrapper.remove();
      navigate();
    } catch (err: unknown) {
      isSaving = false;
      errorMsg = err instanceof Error ? err.message : String(err);
      updateView();
    }
  }

  function template(): TemplateResult {
    return html`
      <div
        class="modal-bg"
        id="edit-headers-modal"
        @click=${(e: Event) => {
          if (e.target === e.currentTarget && !isSaving) wrapper.remove();
        }}
      >
        <div class="modal headers-editor-modal">
          <div class="modal-header">
            <div>
              <h2>Extra Headers</h2>
              <small style="color: var(--color-text-muted); font-family: var(--font-mono);"
                >provider: ${providerId}</small
              >
            </div>
            <button
              type="button"
              class="close-btn"
              ?disabled=${isSaving}
              @click=${() => wrapper.remove()}
              aria-label="Close"
            >
              &times;
            </button>
          </div>

          <div class="modal-body">
            <p style="margin-top: 0; font-size: var(--fs-xs); color: var(--color-text-muted);">
              Custom HTTP headers attached to all outbound requests (inference and discovery) for
              this provider.
            </p>

            <div class="headers-presets">
              <span class="headers-presets-label">Quick Presets:</span>
              ${presets.map(
                (p) => html`
                  <button
                    type="button"
                    class="headers-preset-chip"
                    @click=${() => applyPreset(p)}
                    title="Add ${p.key}"
                  >
                    ${p.label}
                  </button>
                `
              )}
            </div>

            <div class="headers-editor-toolbar">
              <div style="display: flex; gap: var(--space-2); align-items: center;">
                <button type="button" class="small" ?disabled=${rawMode} @click=${() => addRow()}>
                  + Add Header
                </button>
                ${rows.length > 0 && !rawMode
                  ? html`
                      <button
                        type="button"
                        class="small danger"
                        @click=${() => {
                          rows = [];
                          syncRowsToRaw();
                          updateView();
                        }}
                      >
                        Clear All
                      </button>
                    `
                  : html``}
              </div>
              <button type="button" class="small secondary" @click=${toggleMode}>
                ${rawMode ? "Switch to Visual Rows" : "Edit Raw JSON"}
              </button>
            </div>

            ${errorMsg
              ? html`
                  <div
                    class="badge badge-error"
                    style="width: 100%; margin-bottom: var(--space-3); padding: var(--space-2); box-sizing: border-box;"
                  >
                    ${errorMsg}
                  </div>
                `
              : html``}
            ${rawMode
              ? html`
                  <div class="field">
                    <textarea
                      class="headers-raw-textarea"
                      .value=${rawJson}
                      @input=${(e: Event) => {
                        rawJson = (e.target as HTMLTextAreaElement).value;
                      }}
                      placeholder='{\n  "User-Agent": "Mozilla/5.0...",\n  "Accept": "application/json"\n}'
                    ></textarea>
                  </div>
                `
              : html`
                  ${rows.length === 0
                    ? html`
                        <div class="headers-empty-state">
                          No extra headers configured. Click <strong>+ Add Header</strong> or pick a
                          preset above.
                        </div>
                      `
                    : html`
                        <div class="headers-rows-container">
                          <div
                            style="display: flex; gap: var(--space-2); font-size: var(--fs-xs); font-weight: 600; color: var(--color-text-muted); margin-bottom: var(--space-2); text-transform: uppercase; letter-spacing: 0.04em;"
                          >
                            <span style="flex: 0 0 35%;">Header Name</span>
                            <span style="flex: 1;">Header Value</span>
                            <span style="width: 32px;"></span>
                          </div>
                          ${rows.map(
                            (row, idx) => html`
                              <div class="headers-row">
                                <input
                                  type="text"
                                  class="headers-key-input"
                                  placeholder="e.g. User-Agent"
                                  .value=${row.key}
                                  @input=${(e: Event) =>
                                    updateRowKey(idx, (e.target as HTMLInputElement).value)}
                                  required
                                />
                                <input
                                  type="text"
                                  class="headers-val-input"
                                  placeholder="value"
                                  .value=${row.value}
                                  @input=${(e: Event) =>
                                    updateRowValue(idx, (e.target as HTMLInputElement).value)}
                                />
                                <button
                                  type="button"
                                  class="headers-delete-btn"
                                  title="Delete header"
                                  @click=${() => removeRow(idx)}
                                >
                                  &times;
                                </button>
                              </div>
                            `
                          )}
                        </div>
                      `}
                `}
          </div>

          <div class="modal-footer">
            <button type="button" ?disabled=${isSaving} @click=${() => wrapper.remove()}>
              Cancel
            </button>
            <button
              type="button"
              class="primary"
              ?disabled=${isSaving}
              @click=${handleSave}
            >
              ${isSaving ? "Saving..." : "Save Headers"}
            </button>
          </div>
        </div>
      </div>
    `;
  }

  updateView();
}

export async function editProviderHeadersPrompt(
  providerId: string,
  currentHeadersJson: string | null | undefined
): Promise<void> {
  showEditProviderHeaders(providerId, currentHeadersJson);
}

// ===== Bulk toggle (enable/disable all non-custom models) =====

export async function bulkToggleModels(providerId: string, active: boolean): Promise<void> {
  const models = (state.models || []).filter((m) => m.provider_id === providerId);
  const customCount = models.filter((m) => m.custom).length;
  const toToggleCount = models.filter((m) => !m.custom && m.active !== active).length;
  if (toToggleCount === 0) {
    showToast("Nothing to toggle.", "info");
    return;
  }
  const msg = active
    ? `Enable ${toToggleCount} non-custom models? (${customCount} custom models will not be touched)`
    : `Disable ${toToggleCount} non-custom models? (${customCount} custom models will not be touched)`;
  if (!confirm(msg)) return;
  try {
    await api("/models/bulk-toggle", {
      method: "POST",
      body: JSON.stringify({ provider_id: providerId, active }),
    });
    state.models = await api("/models") as typeof state.models;
    // Targeted DOM patch — for each non-custom model row of this
    // provider, sync the row's active-state UI in place. We do
    // NOT call requestUpdate() because a full rebuild would
    // close any open `<select>` / steal focus from the search
    // input the user may still be editing. The patch touches only
    // the rows whose state changed; everything else stays put.
    // Mirrors the patchComboField pattern in combo-handlers.ts.
    const allProviderModels = (state.models || []).filter((m) => m.provider_id === providerId);
    for (const m of allProviderModels.filter((m) => !m.custom)) {
      syncModelRowActive(m.row_id, m.active);
    }
    // Refresh the (All / Active / Inactive) counts on the filter
    // tabs so the totals reflect the new state.
    updateFilterTabCounts(providerId, allProviderModels);
  } catch (err: unknown) {
    showApiError(err, "Error");
  }
}

// ===== Account health / quota =====

// POST /admin/accounts/:id/health — force-set the health
// flag. The select's value is read off the change event, not from
// data-arg, so the shim passes the event and we read `e.target.value`.
export async function setHealth(id: number, e: Event | null): Promise<void> {
  const target = e && e.target && e.target instanceof HTMLSelectElement ? e.target : null;
  const health = target ? target.value : null;
  if (!health) return;
  try {
    await api("/accounts/" + id + "/health", {
      method: "POST",
      body: JSON.stringify({ health }),
    });
    // Update the cached account so the background poll's diff is
    // a no-op and the next render is correct.
    const a = (state.accounts || []).find((x) => x.id === id);
    if (a) a.health_status = health as typeof a.health_status;
  } catch (err: unknown) {
    showApiError(err, "Error");
    // Don't re-render on error — see patchComboField in
    // combo-handlers.ts for the rationale. The user's select
    // already shows their choice; a re-render would close any
    // other open dropdown on the page and steal focus from any
    // input the user might still be editing.
  }
}

// POST /admin/accounts/:id/refresh-quota — fetch a fresh
// quota. The button gets a coloured flash so the click feels
// acknowledged even when the request takes a few seconds.
export async function refreshAccountQuota(accountId: number, e: Event | null): Promise<void> {
  const target = e && e.target && e.target instanceof HTMLButtonElement ? e.target : null;
  const btn: HTMLButtonElement | null = target;
  const oldText = btn ? btn.textContent : null;
  if (btn) {
    btn.disabled = true;
    btn.textContent = "...";
  }
  try {
    const result = (await api("/accounts/" + accountId + "/refresh-quota", { method: "POST" })) as
      { supported?: boolean; error?: string; model_details?: Array<unknown> } | null;
    if (result && result.supported === false) {
      if (btn) flashButton(btn, "n/a", "#9399b2");
    } else if (result && result.error) {
      if (btn) flashButton(btn, "✗ err", "#f38ba8");
    } else {
      if (btn) flashButton(btn, "✓", "#a6e3a1");
    }
    state.accounts = await api("/accounts") as typeof state.accounts;
    // Restore model_details (not persisted in DB) from the refresh
    // response back into the account so the quota cell can render it.
    if (result && "model_details" in result && result.model_details != null) {
      const match = state.accounts.find((a: { id: number }) => a.id === accountId);
      if (match) {
        (match as unknown as Record<string, unknown>)["quota_model_details"] = result.model_details;
      }
    }
    requestUpdate();
  } catch (err: unknown) {
    if (btn) flashButton(btn, "✗", "#f38ba8");
    showApiError(err, "Error");
  } finally {
    if (btn) {
      setTimeout(() => { btn.disabled = false; btn.textContent = oldText; }, 1500);
    }
  }
}

// Walk every quota-capable account of a provider and refresh
// each. The "not supported" alert only appears when there's
// actually nothing to refresh.
export async function refreshAllQuotas(providerId: string): Promise<void> {
  const accounts = (state.accounts || []).filter((a) => a.provider_id === providerId);
  const supported = accounts.filter((a) => {
    const p = state.providers.find((p) => p.id === a.provider_id);
    return p?.metadata?.supports_quota === true;
  });
  if (supported.length === 0) {
    showToast("No accounts with quota support for " + providerId + ".", "info");
    return;
  }
  if (!confirm(`Refresh quota for ${supported.length} accounts?`)) return;
  for (const a of supported) {
    try {
      await api("/accounts/" + a.id + "/refresh-quota", { method: "POST" });
    } catch (err: unknown) {
      console.error("Failed to refresh quota for", a.id, err);
    }
  }
  state.accounts = await api("/accounts") as typeof state.accounts;
  requestUpdate();
  showToast("Quotas refreshed.", "success");
}
