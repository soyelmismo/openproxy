// handlers/provider-handlers/list.ts — provider CRUD + list operations.
//
// Contains: refresh, create, delete, rename, toggle active,
// edit endpoint, and bulk model toggle — the operations that
// belong to the provider list/grid level.

import { navigate } from "../../state/router.js";
import { state } from "../../state/index.js";
import { api } from "../../state/api.js";
import { html, render, type TemplateResult } from "lit-html";
import { syncModelRowActive, updateFilterTabCounts } from "../../components/model-table.js";
import { requestUpdate } from "../../state/reactive.js";
import { ensureModalRoot, showApiError } from "../../lib/ui-utils.js";
import { showConfirm, showPrompt } from "../../lib/show-confirm.js";
import { showToast } from "../../components/toast.js";
import { extractApiErrorMessage } from "../../lib/escape.js";

interface RefreshResult {
  models_refreshed?: number;
  new_model_ids?: string[];
}

// POST /admin/providers/:id/refresh — re-discover the model
// list for one provider.
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
    const summary = n === 0
      ? `Nothing to refresh for ${providerId}.`
      : `Refreshed ${n} models for ${providerId}.`;
    const newSuffix = newIds.length === 0
      ? ""
      : newIds.length <= 3
        ? ` New: ${newIds.join(", ")}.`
        : ` New: ${newIds.slice(0, 3).join(", ")} (+${newIds.length - 3} more).`;
    showToast(summary + newSuffix, "success");
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

// Walk every provider and POST to its /refresh endpoint.
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
    if (wrapper) wrapper.remove(); else closeCreateProvider();
    state.providers = await api("/providers") as typeof state.providers;
    navigate();
  } catch (err: unknown) {
    showApiError(err, "Error");
  }
}

// ===== Delete provider =====

export async function deleteProvider(id: string): Promise<void> {
  if (!(await showConfirm({
    title: "Delete provider",
    message: `Delete provider ${id}? This will cascade-delete its accounts and models.`,
    danger: true,
    confirmLabel: "Delete",
  }))) return;
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

export async function confirmDeleteProvider(providerId: string): Promise<void> {
  const typed = await showPrompt("Delete provider", `Type the provider ID to confirm deletion: ${providerId}`);
  if (typed !== providerId) {
    if (typed != null) {
      showToast(`Provider id "${typed}" does not match. Nothing was deleted.`, "warning");
    }
    return;
  }
  if (!(await showConfirm({
    title: "Really delete?",
    message: `Really delete ${providerId}? This cascades to all its accounts and models.`,
    danger: true,
    confirmLabel: "Delete",
  }))) return;
  try {
    await api("/providers/" + encodeURIComponent(providerId), { method: "DELETE" });
    state.providers = state.providers.filter((p) => p.id !== providerId);
    state.models = state.models.filter((m) => m.provider_id !== providerId);
    state.accounts = state.accounts.filter((a) => a.provider_id !== providerId);
    location.hash = "#/providers";
  } catch (err: unknown) {
    const friendly = extractApiErrorMessage(err) || (err instanceof Error ? err.message : String(err));
    showToast("Cannot delete: " + friendly, "error");
  }
}

// ===== Toggle active / rename =====

export async function toggleProviderActive(providerId: string, newActive: boolean): Promise<void> {
  if (!newActive) {
    const ok = await showConfirm({
      title: "Deactivate provider",
      message:
        `Deactivate provider "${providerId}"?\n\n` +
        `Its accounts and models will be preserved, but it won't be ` +
        `usable in combos until you reactivate it.`,
      danger: true,
      confirmLabel: "Deactivate",
    });
    if (!ok) return;
  }
  try {
    await api("/providers/" + encodeURIComponent(providerId) + "/active", {
      method: "POST",
      body: JSON.stringify({ active: newActive }),
    });
    state.providers = await api("/providers") as typeof state.providers;
    navigate();
  } catch (err: unknown) {
    showApiError(err, "Error");
  }
}

export async function renameProviderPrompt(providerId: string, currentName: string): Promise<void> {
  const newName = await showPrompt(`Rename provider "${providerId}"`, "New provider name:", currentName);
  if (newName == null) return;
  const trimmed = newName.trim();
  if (trimmed === "") {
    showToast("Name cannot be empty", "error");
    return;
  }
  if (trimmed === currentName) return;

  const collision = state.providers.find(
    (p) => p.id !== providerId && p.name === trimmed,
  );
  if (collision) {
    const ok = await showConfirm({
      title: "Name collision",
      message:
        `A provider with this name already exists (${collision.id}). ` +
        `Use this name anyway?`,
      confirmLabel: "Use anyway",
    });
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
  const newUrl = await showPrompt(
    `Edit endpoint for provider "${providerId}"`,
    "Base URL:\n(e.g., https://api.openai.com/v1)",
    currentBaseUrl,
  );
  if (newUrl == null) return;
  const trimmed = newUrl.trim();
  if (trimmed === "") {
    showToast("Base URL cannot be empty", "error");
    return;
  }
  if (trimmed === currentBaseUrl) return;

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
  if (!(await showConfirm({
    title: active ? "Enable models" : "Disable models",
    message: msg,
    confirmLabel: active ? "Enable" : "Disable",
  }))) return;
  try {
    await api("/models/bulk-toggle", {
      method: "POST",
      body: JSON.stringify({ provider_id: providerId, active }),
    });
    state.models = await api("/models") as typeof state.models;
    const allProviderModels = (state.models || []).filter((m) => m.provider_id === providerId);
    for (const m of allProviderModels.filter((m) => !m.custom)) {
      syncModelRowActive(m.row_id, m.active);
    }
    updateFilterTabCounts(providerId, allProviderModels);
  } catch (err: unknown) {
    showApiError(err, "Error");
  }
}
