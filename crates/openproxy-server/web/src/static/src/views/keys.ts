// views/keys.ts — API keys list.
//
// MIGRATED to lit-html for atomic DOM updates. The create / edit /
// regen / revoke / delete handlers are wired directly to @click
// listeners; the create/edit modal HTML is still built by
// `handlers/key-handlers.ts` (it lives at <body> level so it
// survives re-renders). Regenerate / revoke / delete are written
// locally so they can use `showToast()` for errors (no `alert()`)
// and call `requestUpdate()` instead of `rerenderCurrentView()`.

import { html, type TemplateResult } from 'lit-html';
import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { requestUpdate } from "../state/reactive.js";
import { createView } from "../lib/view-utils.js";
import { showToast } from "../components/toast.js";
import { showCreateKey, showEditKey } from "../handlers/key-handlers.js";
import { showPlaintextKey } from "../components/key-display.js";
import type { Model } from "../lib/types/api.js";

// The api_key row shape. Defined locally (not in lib/types/api.ts)
// because the server-side `pub struct ApiKey` lives in a separate
// file (`crates/openproxy-core/src/api_keys.rs`) and G3 only
// exported the core ids/enums/structs that the rest of the
// dashboard already uses. This interface mirrors the columns the
// `/admin/api-keys` endpoint serialises — `id`, `label`,
// `key_prefix`, `scopes` (array of strings), `allowed_models`
// (null = all, [] = empty whitelist, [...]= explicit list),
// `is_active`, `revoked_at`, `last_used_at`, `created_at`,
// `created_by`. Everything is nullable where the DB allows it.
interface ApiKeyRow {
  id: number;
  label: string | null;
  key_prefix: string | null;
  scopes: string[] | null;
  allowed_models: string[] | null | unknown;
  is_active: boolean;
  revoked_at: string | null;
  last_used_at: string | null;
  created_at: string | null;
  created_by: string | null;
}

// Shape of the POST /keys/:id/regenerate response.
interface KeyPlaintextResponse {
  plaintext: string;
  key: { label?: string | null; key_prefix?: string | null } | null;
}

// ---- Module-local state ----
let loadError: string | null = null;

// ---- Handlers ----

function onShowCreateKey(): void { void showCreateKey(); }
function onShowEditKey(id: number): void { void showEditKey(id); }
function onViewKeyUsage(id: number): void {
  location.hash = `#/keys/${id}/usage`;
}

async function onRegenerateKey(id: number, label: string | null): Promise<void> {
  const display = label || ("#" + id);
  if (!confirm(`Regenerate key "${display}"?\n\nThe current key will be invalidated immediately. You'll get a new plaintext key.`)) return;
  try {
    const result = (await api(`/keys/${id}/regenerate`, { method: "POST" })) as KeyPlaintextResponse;
    showPlaintextKey(result.plaintext, result.key);
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    showToast("Error: " + msg, "error");
  }
}

async function onRevokeKey(id: number, label: string | null): Promise<void> {
  const display = label || ("#" + id);
  if (!confirm(`Revoke key "${display}"?\n\nThe key will be deactivated immediately. Any client using it will get 401 errors. You can re-enable it later by editing the row.`)) return;
  try {
    await api(`/keys/${id}/revoke`, { method: "POST" });
    state.apiKeys = await api("/keys") as typeof state.apiKeys;
    requestUpdate();
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    showToast("Error: " + msg, "error");
  }
}

async function onDeleteKey(id: number, label: string | null): Promise<void> {
  const display = label || ("#" + id);
  if (!confirm(`Delete key "${display}"?\n\nThis is irreversible. Historical usage rows will keep the api_key_id but the key row itself will be gone.`)) return;
  try {
    await api(`/keys/${id}`, { method: "DELETE" });
    state.apiKeys = (state.apiKeys || []).filter((k) => (k as { id: number }).id !== id);
    requestUpdate();
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    showToast("Error: " + msg, "error");
  }
}

// ---- Templates ----

function renderKeyRow(k: ApiKeyRow): TemplateResult {
  const scopes: string = (k.scopes || []).join(", ") || "—";
  let allowedModels: string = "all";
  if (k.allowed_models === null || k.allowed_models === undefined) allowedModels = "all";
  else if (Array.isArray(k.allowed_models) && k.allowed_models.length === 0) allowedModels = "(empty)";
  else if (Array.isArray(k.allowed_models)) allowedModels = k.allowed_models.length + " models";
  const isActive: boolean = k.is_active && !k.revoked_at;
  const statusClass: string = isActive ? "on" : "off";
  const statusText: string = k.revoked_at ? "revoked" : (k.is_active ? "active" : "inactive");
  const label: string = k.label || "—";
  const createdBy: TemplateResult = k.created_by ? html` <small>(${k.created_by})</small>` : html``;
  return html`
    <tr>
      <td>${label}${createdBy}</td>
      <td><code>${k.key_prefix || "—"}</code></td>
      <td>${scopes}</td>
      <td>${allowedModels}</td>
      <td><span class="status-pill ${statusClass}">${statusText}</span></td>
      <td>${k.last_used_at || "never"}</td>
      <td>${k.created_at || "—"}</td>
      <td>
        <button class="small" @click=${() => onShowEditKey(k.id)}>Edit</button>
        <button class="small" @click=${() => onRegenerateKey(k.id, k.label)}>Regenerate</button>
        <button class="small" @click=${() => onViewKeyUsage(k.id)}>Usage</button>
        ${k.is_active && !k.revoked_at
          ? html`<button class="small" @click=${() => onRevokeKey(k.id, k.label)}>Revoke</button>`
          : html``}
        <button class="small danger" @click=${() => onDeleteKey(k.id, k.label)}>Delete</button>
      </td>
    </tr>
  `;
}

function renderKeys(): TemplateResult {
  if (loadError) {
    return html`
      <div class="page-header"><h2>API Keys</h2>
        <div class="actions"><button class="primary" @click=${onShowCreateKey}>+ Create key</button></div>
      </div>
      <div class="banner banner-error">${loadError}</div>
    `;
  }
  const keys: ApiKeyRow[] = (state.apiKeys as ApiKeyRow[]) || [];
  const body: TemplateResult = keys.length === 0
    ? html`<p class="empty">No API keys yet. Create one to authenticate clients.</p>`
    : html`<table>
        <thead><tr><th>Label</th><th>Prefix</th><th>Scopes</th><th>Allowed models</th><th>Status</th><th>Last used</th><th>Created</th><th>Actions</th></tr></thead>
        <tbody>${keys.map(renderKeyRow)}</tbody>
      </table>`;
  return html`
    <div class="page-header"><h2>API Keys</h2>
      <div class="actions"><button class="primary" @click=${onShowCreateKey}>+ Create key</button></div>
    </div>
    ${body}
  `;
}

// ---- Mount ----

export async function mountKeys(): Promise<(() => void) | void> {
  loadError = null;
  return createView(
    renderKeys,
    async () => {
      const [keys, models] = await Promise.all([
        api("/keys") as Promise<ApiKeyRow[]>,
        api("/models") as Promise<Model[]>,
      ]);
      state.apiKeys = keys;
      state.models = models;
    },
    (msg) => { loadError = msg; },
  );
}
