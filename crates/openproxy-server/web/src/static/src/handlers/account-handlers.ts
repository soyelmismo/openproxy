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

import { html } from 'lit-html';
import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { requestUpdate } from "../state/reactive.js";
import { showToast } from "../components/toast.js";
import { copyToClipboard } from "../lib/clipboard.js";
import { showApiError } from "../lib/ui-utils.js";
import { showConfirm, showPrompt } from "../lib/show-confirm.js";
import { mutateAndRefresh } from "../lib/mutate.js";
import { renderOpModal } from "../components/render-op-modal.js";

function summarizeApiKey(key: string): string {
  const trimmed = key.trim();
  if (trimmed.length <= 10) return trimmed;
  const prefix = trimmed.slice(0, 6);
  const suffix = trimmed.slice(-4);
  return `${prefix}...${suffix}`;
}

export function showCreateAccount(providerId: string): void {
  const handle = renderOpModal({
    title: `New account for ${providerId}`,
    body: html`
      <div class="field">
        <label for="account-label">Label (optional)</label>
        <input id="account-label" name="label" type="text" placeholder="auto-generated from key if empty">
      </div>
      <div class="field">
        <label for="account-secret">API Key / Secret</label>
        <input id="account-secret" name="secret" type="password" placeholder="paste the API key here">
      </div>
      <div class="field">
        <label for="account-scopes">Scopes (comma separated)</label>
        <input id="account-scopes" name="scopes" type="text" placeholder="chat,manage">
      </div>
    `,
    actions: html`
      <button type="button" @click=${() => handle.close()}>Cancel</button>
      <button type="submit" class="primary"
        @click=${(e: Event) => { e.preventDefault(); void createAccount(providerId, e, handle.close); }}>
        Create
      </button>
    `,
  });
}

export function closeCreateAccount(): void {
  // Backwards-compat shim: the registry still exposes this as a
  // data-action target. renderOpModal owns the wrapper, so we just
  // close the most recent modal-bg in the modal-root.
  const root = document.getElementById("modal-root");
  if (!root) return;
  const last = root.lastElementChild;
  if (!last) return;
  // Find the close button on the rendered dialog and synthesize a
  // click — that goes through renderOpModal's own close() path so
  // the keydown listener is also torn down.
  const closeBtn = last.querySelector<HTMLButtonElement>(".close-btn");
  closeBtn?.click();
}

export async function createAccount(providerId: string, e: Event, close?: () => void): Promise<void> {
  const target = e.target;
  if (!(target instanceof HTMLFormElement)) return;
  const f = new FormData(target);
  const rawLabel = (f.get("label") || "").toString().trim();
  const rawSecret = (f.get("secret") || "").toString().trim();
  const label = rawLabel || (rawSecret ? summarizeApiKey(rawSecret) : null);
  const scopes = (f.get("scopes") || "").toString().split(",").map((s) => s.trim()).filter(Boolean);
  const body = {
    provider_id: providerId,
    label,
    api_key: rawSecret || null,
    scopes,
  };
  try {
    await api("/accounts", { method: "POST", body: JSON.stringify(body) });
    state.accounts = await api("/accounts") as typeof state.accounts;
    if (close) close(); else closeCreateAccount();
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, "Error");
  }
}

export async function deleteAccount(id: number): Promise<void> {
  if (!(await showConfirm({
    title: "Delete account",
    message: "Delete account #" + id + "?",
    danger: true,
    confirmLabel: "Delete",
  }))) return;
  await mutateAndRefresh({
    apiCall: async () => {
      await api("/accounts/" + id, { method: "DELETE" });
      state.accounts = await api("/accounts") as typeof state.accounts;
    },
  });
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
  const handle = renderOpModal({
    title: `Update API key for account #${id}`,
    body: html`
      <div class="field">
        <label for="account-key">New API key</label>
        <input id="account-key" name="api_key" type="password" placeholder="paste the new API key here">
      </div>
      <p><small>Leave empty and submit to <strong>clear</strong> the key (OAuth-only account).</small></p>
    `,
    actions: html`
      <button type="button" @click=${() => handle.close()}>Cancel</button>
      <button type="submit" class="primary"
        @click=${(e: Event) => { e.preventDefault(); void updateAccountKey(id, e, handle.close); }}>
        Save key
      </button>
    `,
  });
}

export function closeUpdateAccountKey(): void {
  // Backwards-compat shim: the registry still exposes this as a
  // data-action target. renderOpModal owns the wrapper, so we just
  // close the most recent modal-bg in the modal-root.
  const root = document.getElementById("modal-root");
  if (!root) return;
  const last = root.lastElementChild;
  if (!last) return;
  const closeBtn = last.querySelector<HTMLButtonElement>(".close-btn");
  closeBtn?.click();
}

export async function updateAccountKey(id: number, e: Event, close?: () => void): Promise<void> {
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
    if (close) close(); else closeUpdateAccountKey();
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
  const newLabel = await showPrompt("Rename account label", "New label:", currentLabel || "");
  if (newLabel == null) return;
  const trimmed = newLabel.trim();
  if (trimmed === currentLabel) return;

  await mutateAndRefresh({
    apiCall: async () => {
      await api("/accounts/" + id + "/label", {
        method: "PATCH",
        body: JSON.stringify({ label: trimmed || null }),
      });
      state.accounts = await api("/accounts") as typeof state.accounts;
    },
    errorMessage: "Error updating label",
  });
}

export async function copyAccountApiKey(id: number): Promise<void> {
  try {
    const res = await api("/accounts/" + id + "/api-key", { method: "GET" }) as { api_key?: string };
    if (res && res.api_key) {
      try {
        await copyToClipboard(res.api_key);
        showToast("API key copied to clipboard", "success");
      } catch (_e: unknown) {
        // Both navigator.clipboard and execCommand failed — show a
        // read-only modal so the operator can manually select+copy.
        showApiKeyDisplay(res.api_key);
      }
    } else {
      showToast("No API key returned", "error");
    }
  } catch (err: unknown) {
    showApiError(err, "Error copying API key");
  }
}

/**
 * Display an API key in a read-only modal so the operator can
 * manually select and copy it. Used as a last-resort fallback when
 * both clipboard paths (navigator.clipboard + execCommand) fail.
 */
function showApiKeyDisplay(apiKey: string): void {
  const handle = renderOpModal({
    title: "API Key Created",
    body: html`
      <p style="margin: 0 0 var(--space-3); color: var(--color-text-muted); font-size: var(--fs-sm);">
        Copy this key. It will not be shown again.
      </p>
      <input
        type="text"
        readonly
        .value=${apiKey}
        style="width: 100%; padding: var(--space-2); font-family: var(--font-mono); font-size: var(--fs-sm); background: var(--color-surface-2); border: 1px solid var(--color-border); border-radius: var(--radius-sm); color: var(--color-text); user-select: all; cursor: text;"
      />
    `,
    actions: html`
      <button type="button" @click=${() => handle.close()}>Close</button>
      <button type="button" class="primary"
        @click=${async () => {
          try {
            await copyToClipboard(apiKey);
            showToast("API key copied to clipboard", "success");
            handle.close();
          } catch {
            showToast("Copy failed — select the text in the field above and press Ctrl+C", "warning");
          }
        }}
      >Copy</button>
    `,
  });
}

