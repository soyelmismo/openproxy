// handlers/account-handlers.ts — create / delete accounts, open
// the create modal, set the health pill.
//
// Per spec §3 + §13.8 we no longer attach handlers to `window.*`.
// Each function is exported by name and registered in
// handlers/registry.ts so the central data-action shim can find it.
//
// Migrated to lit-html: modals are rendered into a wrapper `<div>`
// under `#modal-root` via `render()`. All `data-action` attributes
// are replaced with direct `@click` / `@submit` handlers; lit-html
// auto-escapes interpolation so we no longer call `escapeHtml` /
// `escapeAttr`.

import { html, render } from 'lit-html';
import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { requestUpdate } from "../state/reactive.js";
import { showToast } from "../components/toast.js";
import { ensureModalRoot, showApiError } from "../lib/ui-utils.js";

export function showCreateAccount(providerId: string): void {
  const wrapper = document.createElement("div");
  ensureModalRoot().appendChild(wrapper);
  render(html`
    <div class="modal-bg" id="create-account-modal"
         @click=${(e: Event) => { if (e.target === e.currentTarget) wrapper.remove(); }}>
      <div class="modal">
        <div class="modal-header">
          <h2>New account for ${providerId}</h2>
          <button type="button" class="close-btn" @click=${() => wrapper.remove()} aria-label="Close">&times;</button>
        </div>
        <form @submit=${(e: Event) => { e.preventDefault(); void createAccount(providerId, e, wrapper); }}>
          <div class="modal-body">
            <div class="field">
              <label for="account-label">Label</label>
              <input id="account-label" name="label" type="text" required>
            </div>
            <div class="field">
              <label for="account-secret">Secret / token (optional)</label>
              <input id="account-secret" name="secret" type="password" placeholder="paste the API key here">
            </div>
            <div class="field">
              <label for="account-scopes">Scopes (comma separated)</label>
              <input id="account-scopes" name="scopes" type="text" placeholder="chat,manage">
            </div>
          </div>
          <div class="modal-footer">
            <button type="button" @click=${() => wrapper.remove()}>Cancel</button>
            <button type="submit" class="primary">Create</button>
          </div>
        </form>
      </div>
    </div>
  `, wrapper);
}

export function closeCreateAccount(): void {
  const m = document.getElementById("create-account-modal");
  if (m) {
    // The modal lives inside a wrapper div we created in
    // showCreateAccount; remove the wrapper too so #modal-root
    // stays clean.
    const wrapper = m.parentElement;
    m.remove();
    if (wrapper && wrapper.children.length === 0 && wrapper.parentElement?.id === "modal-root") {
      wrapper.remove();
    }
  }
}

export async function createAccount(providerId: string, e: Event, wrapper?: HTMLElement): Promise<void> {
  const target = e.target;
  if (!(target instanceof HTMLFormElement)) return;
  const f = new FormData(target);
  const scopes = (f.get("scopes") || "").toString().split(",").map((s) => s.trim()).filter(Boolean);
  const body = {
    provider_id: providerId,
    label: f.get("label"),
    api_key: f.get("secret") || null,
    scopes,
  };
  try {
    await api("/accounts", { method: "POST", body: JSON.stringify(body) });
    state.accounts = await api("/accounts") as typeof state.accounts;
    if (wrapper) wrapper.remove(); else closeCreateAccount();
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, "Error");
  }
}

export async function deleteAccount(id: number): Promise<void> {
  if (!confirm("Delete account #" + id + "?")) return;
  try {
    await api("/accounts/" + id, { method: "DELETE" });
    state.accounts = await api("/accounts") as typeof state.accounts;
    requestUpdate();
  } catch (e: unknown) {
    showApiError(e, "Error");
  }
}

export async function testAccount(id: number): Promise<void> {
  try {
    const res = await api("/accounts/" + id + "/test", { method: "POST" }) as { status?: string; ok?: boolean } | null;
    showToast(`Account #${id}: ${res && res.status ? res.status : "tested"}`, res && res.ok ? "success" : "info");
  } catch (e: unknown) {
    showApiError(e, "Account test failed");
  }
}

export function showUpdateAccountKey(id: number): void {
  const wrapper = document.createElement("div");
  ensureModalRoot().appendChild(wrapper);
  render(html`
    <div class="modal-bg" id="update-account-key-modal"
         @click=${(e: Event) => { if (e.target === e.currentTarget) wrapper.remove(); }}>
      <div class="modal">
        <div class="modal-header">
          <h2>Update API key for account #${id}</h2>
          <button type="button" class="close-btn" @click=${() => wrapper.remove()} aria-label="Close">&times;</button>
        </div>
        <form @submit=${(e: Event) => { e.preventDefault(); void updateAccountKey(id, e, wrapper); }}>
          <div class="modal-body">
            <div class="field">
              <label for="account-key">New API key</label>
              <input id="account-key" name="api_key" type="password" placeholder="paste the new API key here">
            </div>
            <p><small>Leave empty and submit to <strong>clear</strong> the key (OAuth-only account).</small></p>
          </div>
          <div class="modal-footer">
            <button type="button" @click=${() => wrapper.remove()}>Cancel</button>
            <button type="submit" class="primary">Save key</button>
          </div>
        </form>
      </div>
    </div>
  `, wrapper);
}

export function closeUpdateAccountKey(): void {
  const m = document.getElementById("update-account-key-modal");
  if (m) {
    const wrapper = m.parentElement;
    m.remove();
    if (wrapper && wrapper.children.length === 0 && wrapper.parentElement?.id === "modal-root") {
      wrapper.remove();
    }
  }
}

export async function updateAccountKey(id: number, e: Event, wrapper?: HTMLElement): Promise<void> {
  const target = e.target;
  if (!(target instanceof HTMLFormElement)) return;
  const f = new FormData(target);
  const apiKey = f.get("api_key")?.toString().trim() || null;
  try {
    await api("/accounts/" + id + "/api-key", {
      method: "PUT",
      body: JSON.stringify({ api_key: apiKey }),
    });
    state.accounts = await api("/accounts") as typeof state.accounts;
    if (wrapper) wrapper.remove(); else closeUpdateAccountKey();
    // We do NOT call requestUpdate() here — the API key is
    // not displayed in the underlying accounts table, so there's
    // nothing visible to refresh. A full rebuild would close any
    // open `<select>` (e.g. the per-account health dropdown on a
    // sibling row) and steal focus from any input the user might
    // still be editing. Mirrors patchComboField in combo-handlers.ts.
  } catch (err: unknown) {
    showApiError(err, "Error");
  }
}

export async function updateAccountLabel(id: number, currentLabel: string): Promise<void> {
  const newLabel = prompt(`Rename account label:`, currentLabel || "");
  if (newLabel == null) return;
  const trimmed = newLabel.trim();
  if (trimmed === currentLabel) return;
  
  try {
    await api("/accounts/" + id + "/label", {
      method: "PATCH",
      body: JSON.stringify({ label: trimmed || null }),
    });
    state.accounts = await api("/accounts") as typeof state.accounts;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, "Error updating label");
  }
}

export async function copyAccountApiKey(id: number): Promise<void> {
  try {
    const res = await api("/accounts/" + id + "/api-key", { method: "GET" }) as { api_key?: string };
    if (res && res.api_key) {
      if (navigator.clipboard && window.isSecureContext) {
        await navigator.clipboard.writeText(res.api_key);
        showToast("API key copied to clipboard", "success");
      } else {
        const textArea = document.createElement("textarea");
        textArea.value = res.api_key;
        textArea.style.position = "fixed";
        textArea.style.left = "-999999px";
        document.body.appendChild(textArea);
        textArea.focus();
        textArea.select();
        try {
          document.execCommand('copy');
          showToast("API key copied to clipboard", "success");
        } catch (err) {
          prompt("Copy your API key:", res.api_key);
        }
        textArea.remove();
      }
    } else {
      showToast("No API key returned", "error");
    }
  } catch (err: unknown) {
    showApiError(err, "Error copying API key");
  }
}

