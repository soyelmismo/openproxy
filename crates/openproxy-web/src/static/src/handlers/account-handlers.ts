// handlers/account-handlers.ts — create / delete accounts, open
// the create modal, set the health pill.
//
// Per spec §3 + §13.8 we no longer attach handlers to `window.*`.
// Each function is exported by name and registered in
// handlers/registry.ts so the central data-action shim can find it.

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { escapeHtml, escapeAttr } from "../lib/escape.js";
import { appendModal } from "../lib/dom.js";
import { showToast } from "../components/toast.js";
import { rerenderCurrentView } from "../state/router.js";

export function showCreateAccount(providerId: string): void {
  const html = `
    <div class="modal-bg" id="create-account-modal" data-action="closeCreateAccount" data-arg1="self">
      <div class="modal">
        <div class="modal-header">
          <h2>New account for ${escapeHtml(providerId)}</h2>
          <button type="button" class="close-btn" data-action="closeCreateAccount" aria-label="Close">&times;</button>
        </div>
        <form data-action="createAccount" data-arg1="${escapeAttr(providerId)}">
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
            <button type="button" data-action="closeCreateAccount">Cancel</button>
            <button type="submit" class="primary">Create</button>
          </div>
        </form>
      </div>
    </div>
  `;
  // Mount on <body> (not #main) so the 3s background poll's
  // rerenderCurrentView() — which replaces #main's innerHTML —
  // does not destroy the modal mid-edit. See lib/dom.ts appendModal.
  appendModal(html);
}

export function closeCreateAccount(): void {
  const m = document.getElementById("create-account-modal");
  if (m) m.remove();
}

export async function createAccount(providerId: string, e: Event): Promise<void> {
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
    closeCreateAccount();
    rerenderCurrentView();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
  }
}

export async function deleteAccount(id: number): Promise<void> {
  if (!confirm("Delete account #" + id + "?")) return;
  try {
    await api("/accounts/" + id, { method: "DELETE" });
    state.accounts = await api("/accounts") as typeof state.accounts;
    rerenderCurrentView();
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    showToast("Error: " + msg, "error");
  }
}

export async function testAccount(id: number): Promise<void> {
  try {
    const res = await api("/accounts/" + id + "/test", { method: "POST" }) as { status?: string; ok?: boolean } | null;
    showToast(`Account #${id}: ${res && res.status ? res.status : "tested"}`, res && res.ok ? "success" : "info");
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    showToast(`Account test failed: ${msg}`, "error");
  }
}

export function showUpdateAccountKey(id: number): void {
  const html = `
    <div class="modal-bg" id="update-account-key-modal" data-action="closeUpdateAccountKey" data-arg1="self">
      <div class="modal">
        <div class="modal-header">
          <h2>Update API key for account #${id}</h2>
          <button type="button" class="close-btn" data-action="closeUpdateAccountKey" aria-label="Close">&times;</button>
        </div>
        <form data-action="updateAccountKey" data-arg1="${id}">
          <div class="modal-body">
            <div class="field">
              <label for="account-key">New API key</label>
              <input id="account-key" name="api_key" type="password" placeholder="paste the new API key here">
            </div>
            <p><small>Leave empty and submit to <strong>clear</strong> the key (OAuth-only account).</small></p>
          </div>
          <div class="modal-footer">
            <button type="button" data-action="closeUpdateAccountKey">Cancel</button>
            <button type="submit" class="primary">Save key</button>
          </div>
        </form>
      </div>
    </div>
  `;
  appendModal(html);
}

export function closeUpdateAccountKey(): void {
  const m = document.getElementById("update-account-key-modal");
  if (m) m.remove();
}

export async function updateAccountKey(id: number, e: Event): Promise<void> {
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
    closeUpdateAccountKey();
    // We do NOT call rerenderCurrentView() here — the API key is
    // not displayed in the underlying accounts table, so there's
    // nothing visible to refresh. A full rebuild would close any
    // open `<select>` (e.g. the per-account health dropdown on a
    // sibling row) and steal focus from any input the user might
    // still be editing. Mirrors patchComboField in combo-handlers.ts.
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast("Error: " + msg, "error");
  }
}
