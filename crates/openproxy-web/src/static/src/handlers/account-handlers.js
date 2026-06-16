// handlers/account-handlers.js — create / delete accounts, open
// the create modal, set the health pill.
//
// Per spec §3 + §13.8 we no longer attach handlers to `window.*`.
// Each function is exported by name and registered in
// handlers/registry.js so the central data-action shim can find it.

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { escapeHtml, escapeAttr } from "../lib/escape.js";
import { appendModal } from "../lib/dom.js";
import { showToast } from "../components/toast.js";
import { rerenderCurrentView } from "../state/router.js";

export function showCreateAccount(providerId) {
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
  // does not destroy the modal mid-edit. See lib/dom.js appendModal.
  appendModal(html);
}

export function closeCreateAccount() {
  const m = document.getElementById("create-account-modal");
  if (m) m.remove();
}

export async function createAccount(providerId, e) {
  const f = new FormData(e.target);
  const scopes = (f.get("scopes") || "").toString().split(",").map((s) => s.trim()).filter(Boolean);
  const body = {
    provider_id: providerId,
    label: f.get("label"),
    secret: f.get("secret") || null,
    scopes,
  };
  try {
    await api("/accounts", { method: "POST", body: JSON.stringify(body) });
    state.accounts = await api("/accounts");
    closeCreateAccount();
    rerenderCurrentView();
  } catch (err) { alert("Error: " + err.message); }
}

export async function deleteAccount(id) {
  if (!confirm("Delete account #" + id + "?")) return;
  try {
    await api("/accounts/" + id, { method: "DELETE" });
    state.accounts = await api("/accounts");
    rerenderCurrentView();
  } catch (e) { alert("Error: " + e.message); }
}

export async function testAccount(id) {
  try {
    const res = await api("/accounts/" + id + "/test", { method: "POST" });
    showToast(`Account #${id}: ${res && res.status ? res.status : "tested"}`, res && res.ok ? "success" : "info");
  } catch (e) {
    showToast(`Account test failed: ${e.message}`, "error");
  }
}
