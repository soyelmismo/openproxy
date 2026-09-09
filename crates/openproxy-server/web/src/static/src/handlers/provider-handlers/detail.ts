// handlers/provider-handlers/detail.ts — provider detail view operations.
//
// Contains: extra-headers editor, account health/quota operations.
// These are the handlers invoked from the provider detail page,
// not the list/grid.

import { state } from "../../state/index.js";
import { api } from "../../state/api.js";
import { html, render, type TemplateResult } from "lit-html";
import { requestUpdate } from "../../state/reactive.js";
import { ensureModalRoot, flashButton, showApiError } from "../../lib/ui-utils.js";
import { showConfirm } from "../../lib/show-confirm.js";
import { showToast } from "../../components/toast.js";
import { navigate } from "../../state/router.js";

// ===== Extra headers editor =====

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

export function editProviderHeadersPrompt(
  providerId: string,
  currentHeadersJson: string | null | undefined
): void {
  showEditProviderHeaders(providerId, currentHeadersJson);
}

// ===== Account health / quota =====

// POST /admin/accounts/:id/health — force-set the health flag.
export async function setHealth(id: number, e: Event | null): Promise<void> {
  const target = e && e.target && e.target instanceof HTMLSelectElement ? e.target : null;
  const health = target ? target.value : null;
  if (!health) return;
  try {
    await api("/accounts/" + id + "/health", {
      method: "POST",
      body: JSON.stringify({ health }),
    });
    const a = (state.accounts || []).find((x) => x.id === id);
    if (a) a.health_status = health as typeof a.health_status;
  } catch (err: unknown) {
    showApiError(err, "Error");
  }
}

// POST /admin/accounts/:id/refresh-quota — fetch a fresh quota.
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
    if (result && "model_details" in result && result.model_details != null) {
      const match = state.accounts.find((a: { id: number }) => a.id === accountId);
      if (match) {
        match.quota_model_details = result.model_details as import("../../lib/types/api.js").ModelQuotaDetail[];
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

// Walk every quota-capable account of a provider and refresh each.
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
  if (!(await showConfirm({
    title: "Refresh quota",
    message: `Refresh quota for ${supported.length} accounts?`,
    confirmLabel: "Refresh",
  }))) return;
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
