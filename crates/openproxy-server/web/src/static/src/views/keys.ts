// views/keys.ts — API keys list.
//
// The create/edit modal HTML is still built by `handlers/key-handlers.ts`
// (it lives at <body> level so it survives re-renders). Regenerate /
// revoke / delete are written locally so they can use `showToast()` for
// errors and call `requestUpdate()` instead of `rerenderCurrentView()`.

import { html, type TemplateResult } from 'lit-html';
import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { clearToken, isCurrentSessionKey } from "../state/auth.js";
import { requestUpdate } from "../state/reactive.js";
import { createView } from "../lib/view-utils.js";
import { showToast } from "../components/toast.js";
import { showConfirm } from "../lib/show-confirm.js";
import { showCreateKey, showEditKey } from "../handlers/key-handlers.js";
import { showPlaintextKey } from "../components/key-display.js";
import { icons } from "../lib/icons.js";
import { t } from "../i18n/index.js";
import type { Model, ApiKey } from "../lib/types/api.js";
import { renderResponsiveCardTable, type ResponsiveColumn } from "../components/render-responsive-card-table.js";

// Mirrors the columns `/admin/api-keys` serialises. `allowed_models`:
// null = all, [] = empty whitelist, [...] = explicit list.
type ApiKeyRow = ApiKey;

// POST /keys/:id/regenerate response.
interface KeyPlaintextResponse {
  plaintext: string;
  key: { label?: string | null; key_prefix?: string | null } | null;
}

let loadError: string | null = null;


function onShowCreateKey(): void { void showCreateKey(); }
function onShowEditKey(id: number): void { void showEditKey(id); }
function onViewKeyUsage(id: number): void {
  location.hash = `#/keys/${id}/usage`;
}

async function onRegenerateKey(id: number, label: string | null): Promise<void> {
  const display = label || ("#" + id);
  if (!(await showConfirm({
    title: t("keys.list.confirm.regenerate.title"),
    message: t("keys.list.confirm.regenerate.message", { label: display }),
    danger: true,
    confirmLabel: t("keys.list.confirm.regenerate.confirm"),
  }))) return;
  const isSelf = isCurrentSessionKey(id);
  try {
    const result = (await api(`/keys/${id}/regenerate`, { method: "POST" })) as KeyPlaintextResponse;
    if (isSelf) {
      clearToken();
    }
    showPlaintextKey(result.plaintext, result.key, () => {
      if (isSelf) {
        location.hash = "#/login";
      }
    });
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    showToast(t("keys.list.toast.error_prefix") + msg, "error");
  }
}

async function onRevokeKey(id: number, label: string | null): Promise<void> {
  const display = label || ("#" + id);
  if (!(await showConfirm({
    title: t("keys.list.confirm.revoke.title"),
    message: t("keys.list.confirm.revoke.message", { label: display }),
    danger: true,
    confirmLabel: t("keys.list.confirm.revoke.confirm"),
  }))) return;
  const isSelf = isCurrentSessionKey(id);
  try {
    await api(`/keys/${id}/revoke`, { method: "POST" });
    if (isSelf) {
      clearToken();
      location.hash = "#/login";
      return;
    }
    state.apiKeys = await api("/keys") as typeof state.apiKeys;
    requestUpdate();
  } catch (e: unknown) {
    if (isSelf) {
      clearToken();
      location.hash = "#/login";
      return;
    }
    const msg = e instanceof Error ? e.message : String(e);
    showToast(t("keys.list.toast.error_prefix") + msg, "error");
  }
}

async function onDeleteKey(id: number, label: string | null): Promise<void> {
  const display = label || ("#" + id);
  if (!(await showConfirm({
    title: t("keys.list.confirm.delete.title"),
    message: t("keys.list.confirm.delete.message", { label: display }),
    danger: true,
    confirmLabel: t("keys.list.confirm.delete.confirm"),
  }))) return;
  const isSelf = isCurrentSessionKey(id);
  try {
    await api(`/keys/${id}`, { method: "DELETE" });
    if (isSelf) {
      clearToken();
      location.hash = "#/login";
      return;
    }
    state.apiKeys = (state.apiKeys || []).filter((k) => (k as { id: number }).id !== id);
    requestUpdate();
  } catch (e: unknown) {
    if (isSelf) {
      clearToken();
      location.hash = "#/login";
      return;
    }
    const msg = e instanceof Error ? e.message : String(e);
    showToast(t("keys.list.toast.error_prefix") + msg, "error");
  }
}


function formatAllowedModels(k: ApiKeyRow): string {
  let allowedModels: string;
  if (k.allowed_models === null || k.allowed_models === undefined) {
    allowedModels = t("keys.list.cell.allowed_all");
  } else if (Array.isArray(k.allowed_models) && k.allowed_models.length === 0) {
    allowedModels = t("keys.list.cell.allowed_empty");
  } else if (Array.isArray(k.allowed_models)) {
    allowedModels = t("keys.list.cell.allowed_models", { count: k.allowed_models.length });
  } else {
    allowedModels = t("keys.list.cell.allowed_all");
  }

  const blBadges: string[] = [];
  if (Array.isArray(k.blacklisted_providers) && k.blacklisted_providers.length > 0) {
    blBadges.push(t("keys.list.cell.blacklist_providers", { list: k.blacklisted_providers.join(",") }));
  }
  if (Array.isArray(k.blacklisted_models) && k.blacklisted_models.length > 0) {
    blBadges.push(t("keys.list.cell.blacklist_models", { count: k.blacklisted_models.length }));
  }

  const suffix = blBadges.length > 0 ? ` (${blBadges.join("; ")})` : "";
  // Strict mode changes what the cell means: the allowlist is now the outer
  // gate, so an empty list is "nothing" rather than "all".
  return k.only_whitelisted
    ? `${t("keys.list.cell.only_whitelisted")}${suffix}`
    : `${allowedModels}${suffix}`;
}

function formatKeyStatus(k: ApiKeyRow) {
  const isActive = k.is_active && !k.revoked_at;
  if (k.revoked_at) return { text: t("keys.list.status.revoked"), cssClass: "off inactive" };
  if (isActive) return { text: t("keys.list.status.active"), cssClass: "on active" };
  return { text: t("keys.list.status.inactive"), cssClass: "off inactive" };
}


function renderKeys(): TemplateResult {
  if (loadError) {
    return html`
      <div class="page-header"><h2>${t("keys.list.heading")}</h2>
        <div class="actions"><button class="primary" @click=${onShowCreateKey}>${icons.plus()} ${t("keys.list.btn.create")}</button></div>
      </div>
      <div class="banner banner-error">${loadError}</div>
    `;
  }
  const keys: ApiKeyRow[] = (state.apiKeys as ApiKeyRow[]) || [];
  const columns: ResponsiveColumn<ApiKeyRow>[] = [
    {
      key: "label",
      label: t("keys.list.col.label"),
      render: (k) => {
        const label: string = k.label || "—";
        const createdBy: TemplateResult = k.created_by ? html` <small class="key-created-by">(${k.created_by})</small>` : html``;
        return html`<div class="key-label-wrapper"><strong class="key-name">${label}</strong>${createdBy}</div>`;
      },
    },
    {
      key: "key_prefix",
      label: t("keys.list.col.prefix"),
      render: (k) => html`<code class="key-prefix-code">${k.key_prefix || "—"}</code>`,
    },
    {
      key: "scopes",
      label: t("keys.list.col.scopes"),
      render: (k) => html`<span class="key-scopes-text">${(k.scopes || []).join(", ") || "—"}</span>`,
    },
    {
      key: "allowed_models",
      label: t("keys.list.col.allowed_models"),
      render: (k) => html`<span class="chip key-restriction-chip">${formatAllowedModels(k)}</span>`,
    },
    {
      key: "status",
      label: t("keys.list.col.status"),
      render: (k) => {
        const { text, cssClass } = formatKeyStatus(k);
        return html`<span class=${"status-pill " + cssClass}>${text}</span>`;
      },
    },
    {
      key: "last_used_at",
      label: t("keys.list.col.last_used"),
      render: (k) => html`<span class="key-meta-time">${k.last_used_at || t("keys.list.cell.last_used_never")}</span>`,
    },
    {
      key: "created_at",
      label: t("keys.list.col.created"),
      render: (k) => html`<span class="key-meta-time">${k.created_at || "—"}</span>`,
    },
    {
      key: "actions",
      label: t("keys.list.col.actions"),
      render: (k) => {
        const isActive: boolean = k.is_active && !k.revoked_at;
        return html`
          <div class="key-actions-wrap">
            <button class="small" @click=${() => onShowEditKey(k.id)}>${icons.pencil()} ${t("keys.list.btn.edit")}</button>
            <button class="small" @click=${() => onRegenerateKey(k.id, k.label)}>${icons.refresh()} ${t("keys.list.btn.regenerate")}</button>
            <button class="small" @click=${() => onViewKeyUsage(k.id)}>${icons.lightning()} ${t("keys.list.btn.usage")}</button>
            ${isActive
              ? html`<button class="small" @click=${() => onRevokeKey(k.id, k.label)}>${t("keys.list.btn.revoke")}</button>`
              : html``}
            <button class="small danger" @click=${() => onDeleteKey(k.id, k.label)}>${icons.trash()} ${t("keys.list.btn.delete")}</button>
          </div>
        `;
      },
    },
  ];
  const body: TemplateResult = keys.length === 0
    ? html`<p class="empty">${t("keys.list.empty")}</p>`
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
    <div class="page-header"><h2>${t("keys.list.heading")}</h2>
      <div class="actions"><button class="primary" @click=${onShowCreateKey}>${icons.plus()} ${t("keys.list.btn.create")}</button></div>
    </div>
    ${body}
  `;
}


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