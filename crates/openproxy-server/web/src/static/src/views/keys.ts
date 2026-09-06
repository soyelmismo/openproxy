// views/keys.ts — API keys list.
//
// MIGRATED to lit-html for atomic DOM updates. The create / edit /
// regen / revoke / delete handlers are wired directly to @click
// listeners; the create/edit modal HTML is still built by
// `handlers/key-handlers.ts` (it lives at <body> level so it
// survives re-renders). Regenerate / revoke / delete are written
// locally so they can use `showToast()` for errors and call
// `requestUpdate()` instead of `rerenderCurrentView()`.

import { html, type TemplateResult } from 'lit-html';
import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { requestUpdate } from "../state/reactive.js";
import { createView } from "../lib/view-utils.js";
import { showToast } from "../components/toast.js";
import { showConfirm } from "../lib/show-confirm.js";
import { showCreateKey, showEditKey } from "../handlers/key-handlers.js";
import { showPlaintextKey } from "../components/key-display.js";
import { icons } from "../lib/icons.js";
import type { Model, ApiKey } from "../lib/types/api.js";
import { renderResponsiveCardTable, type ResponsiveColumn } from "../components/render-responsive-card-table.js";

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
type ApiKeyRow = ApiKey;

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
  if (!(await showConfirm({
    title: "Regenerate key",
    message: `Regenerate key "${display}"?\n\nThe current key will be invalidated immediately. You'll get a new plaintext key.`,
    danger: true,
    confirmLabel: "Regenerate",
  }))) return;
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
  if (!(await showConfirm({
    title: "Revoke key",
    message: `Revoke key "${display}"?\n\nThe key will be deactivated immediately. Any client using it will get 401 errors. You can re-enable it later by editing the row.`,
    danger: true,
    confirmLabel: "Revoke",
  }))) return;
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
  if (!(await showConfirm({
    title: "Delete key",
    message: `Delete key "${display}"?\n\nThis is irreversible. Historical usage rows will keep the api_key_id but the key row itself will be gone.`,
    danger: true,
    confirmLabel: "Delete",
  }))) return;
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

function renderKeys(): TemplateResult {
  if (loadError) {
    return html`
      <div class="page-header"><h2>API Keys</h2>
        <div class="actions"><button class="primary" @click=${onShowCreateKey}>${icons.plus()} Create key</button></div>
      </div>
      <div class="banner banner-error">${loadError}</div>
    `;
  }
  const keys: ApiKeyRow[] = (state.apiKeys as ApiKeyRow[]) || [];
  const columns: ResponsiveColumn<ApiKeyRow>[] = [
    {
      key: "label",
      label: "Label",
      render: (k) => {
        const label: string = k.label || "—";
        const createdBy: TemplateResult = k.created_by ? html` <small class="key-created-by">(${k.created_by})</small>` : html``;
        return html`<div class="key-label-wrapper"><strong class="key-name">${label}</strong>${createdBy}</div>`;
      },
    },
    {
      key: "key_prefix",
      label: "Prefix",
      render: (k) => html`<code class="key-prefix-code">${k.key_prefix || "—"}</code>`,
    },
    {
      key: "scopes",
      label: "Scopes",
      render: (k) => html`<span class="key-scopes-text">${(k.scopes || []).join(", ") || "—"}</span>`,
    },
    {
      key: "allowed_models",
      label: "Allowed models",
      render: (k) => {
        let allowedModels: string = "all";
        if (k.allowed_models === null || k.allowed_models === undefined) allowedModels = "all";
        else if (Array.isArray(k.allowed_models) && k.allowed_models.length === 0) allowedModels = "(empty)";
        else if (Array.isArray(k.allowed_models)) allowedModels = k.allowed_models.length + " models";
        const blBadges: string[] = [];
        if (Array.isArray(k.blacklisted_providers) && k.blacklisted_providers.length > 0) {
          blBadges.push(`!prov: ${k.blacklisted_providers.join(",")}`);
        }
        if (Array.isArray(k.blacklisted_models) && k.blacklisted_models.length > 0) {
          blBadges.push(`!models: ${k.blacklisted_models.length}`);
        }
        const restrictions: string = blBadges.length > 0
          ? `${allowedModels} (${blBadges.join("; ")})`
          : allowedModels;
        return html`<span class="chip key-restriction-chip">${restrictions}</span>`;
      },
    },
    {
      key: "status",
      label: "Status",
      render: (k) => {
        const isActive: boolean = k.is_active && !k.revoked_at;
        const statusClass: string = isActive ? "on active" : "off inactive";
        const statusText: string = k.revoked_at ? "revoked" : (k.is_active ? "active" : "inactive");
        return html`<span class=${"status-pill " + statusClass}>${statusText}</span>`;
      },
    },
    {
      key: "last_used_at",
      label: "Last used",
      render: (k) => html`<span class="key-meta-time">${k.last_used_at || "never"}</span>`,
    },
    {
      key: "created_at",
      label: "Created",
      render: (k) => html`<span class="key-meta-time">${k.created_at || "—"}</span>`,
    },
    {
      key: "actions",
      label: "Actions",
      render: (k) => {
        const isActive: boolean = k.is_active && !k.revoked_at;
        return html`
          <div class="key-actions-wrap">
            <button class="small" @click=${() => onShowEditKey(k.id)}>${icons.pencil()} Edit</button>
            <button class="small" @click=${() => onRegenerateKey(k.id, k.label)}>${icons.refresh()} Regenerate</button>
            <button class="small" @click=${() => onViewKeyUsage(k.id)}>${icons.lightning()} Usage</button>
            ${isActive
              ? html`<button class="small" @click=${() => onRevokeKey(k.id, k.label)}>Revoke</button>`
              : html``}
            <button class="small danger" @click=${() => onDeleteKey(k.id, k.label)}>${icons.trash()} Delete</button>
          </div>
        `;
      },
    },
  ];
  const body: TemplateResult = keys.length === 0
    ? html`<p class="empty">No API keys yet. Create one to authenticate clients.</p>`
    : html`
        ${renderResponsiveCardTable<ApiKeyRow>({
          columns,
          rows: keys,
          rowKey: (k) => k.id,
          rowClass: (k) => (k.is_active && !k.revoked_at) ? "active" : "inactive revoked",
          className: "keys-table api-keys-table",
        })}
      `;
  return html`
    <div class="page-header"><h2>API Keys</h2>
      <div class="actions"><button class="primary" @click=${onShowCreateKey}>${icons.plus()} Create key</button></div>
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
