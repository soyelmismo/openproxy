// views/keys.ts — API keys list.
//
// Creation stays in a modal, while Edit navigates to the integrated key-detail view.
// Regenerate / revoke / delete are written locally so they can use showToast()
// and refresh the list through requestUpdate().

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
import { renderDataGrid, type GridColumn } from "../components/data-grid.js";

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

// Inline rename (double-click / F2 / context menu). PATCHes just the label;
// the backend accepts partial PATCH bodies, the rest of the key stays as is.
async function onRenameKey(id: number, label: string): Promise<void> {
  try {
    await api(`/keys/${id}`, { method: "PATCH", body: JSON.stringify({ label }) });
    state.apiKeys = ((state.apiKeys || []) as ApiKeyRow[]).map((k) => (k.id === id ? { ...k, label } : k)) as typeof state.apiKeys;
    requestUpdate();
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    showToast(t("keys.list.toast.error_prefix") + msg, "error");
  }
}

// One confirmation for the whole selection, then one DELETE per key. Keys that
// turn out to be the current session key are skipped rather than logged out:
// a bulk action must never nuke the operator's own session silently.
async function onBulkDeleteKeys(picked: ApiKeyRow[]): Promise<void> {
  if (picked.length === 0) return;
  const sessionKeys = picked.filter((k) => isCurrentSessionKey(k.id));
  const deletable = picked.filter((k) => !isCurrentSessionKey(k.id));
  if (deletable.length === 0) {
    showToast(t("keys.list.bulk.only_session"), "error");
    return;
  }
  if (!(await showConfirm({
    title: t("keys.list.bulk.confirm_delete.title", { count: deletable.length }),
    message: t("keys.list.bulk.confirm_delete.message", { list: deletable.map((k) => k.label || "#" + k.id).join(", ") }),
    danger: true,
    confirmLabel: t("keys.list.confirm.delete.confirm"),
  }))) return;
  const failed: string[] = [];
  for (const k of deletable) {
    try {
      await api(`/keys/${k.id}`, { method: "DELETE" });
      state.apiKeys = ((state.apiKeys || []) as ApiKeyRow[]).filter((x) => x.id !== k.id) as typeof state.apiKeys;
    } catch {
      failed.push(k.label || "#" + k.id);
    }
  }
  requestUpdate();
  if (failed.length > 0) showToast(t("keys.list.bulk.partial_error", { list: failed.join(", ") }), "error");
  void sessionKeys;
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
  if (Array.isArray(k.allowed_combos) && k.allowed_combos.length > 0) {
    blBadges.push(t("keys.list.cell.allowed_combos", { count: k.allowed_combos.length }));
  } else if (Array.isArray(k.allowed_combos) && k.allowed_combos.length === 0) {
    blBadges.push("combos: 0");
  }
  if (Array.isArray(k.allowed_providers) && k.allowed_providers.length > 0) {
    blBadges.push(t("keys.list.cell.whitelist_providers", { list: k.allowed_providers.join(",") }));
  }
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

  const columns: GridColumn<ApiKeyRow>[] = [
    {
      key: "label",
      label: t("keys.list.col.label"),
      min: 100,
      grow: 2.5,
      render: (k) => html`<strong class="key-name">${k.label || "—"}</strong>${k.created_by ? html`<small class="key-created-by">(${k.created_by})</small>` : ""}`,
    },
    {
      key: "key_prefix",
      label: t("keys.list.col.prefix"),
      min: 85,
      grow: 1.5,
      fold: 3,
      render: (k) => html`<code class="key-prefix-code">${k.key_prefix || "—"}</code>`,
    },
    {
      key: "scopes",
      label: t("keys.list.col.scopes"),
      min: 70,
      grow: 1.2,
      render: (k) => html`${(k.scopes || []).map((s) => html`<span class="chip">${s}</span>`) || "—"}`,
    },
    {
      key: "allowed_models",
      label: t("keys.list.col.allowed_models"),
      min: 90,
      fold: 1,
      render: (k) => html`<span class="chip key-restriction-chip dg-mono">${formatAllowedModels(k)}</span>`,
    },
    {
      key: "last_used_at",
      label: t("keys.list.col.last_used"),
      min: 70,
      fold: 2,
      render: (k) => html`<span class="dg-dim">${k.last_used_at || t("keys.list.cell.last_used_never")}</span>`,
    },
    {
      key: "status",
      label: t("keys.list.col.status"),
      render: (k) => {
        const { text, cssClass } = formatKeyStatus(k);
        const on = cssClass.includes("on");
        return html`<span class="dg-dot ${on ? "on" : "off"}" title=${text} aria-label=${text}></span>`;
      },
    },
  ];

  const body: TemplateResult = keys.length === 0
    ? html`<p class="empty">${t("keys.list.empty")}</p>`
    : html`
        ${renderDataGrid<ApiKeyRow>({
          id: "keys-grid",
          className: "keys-table api-keys-table",
          columns,
          rows: keys,
          rowKey: (k) => k.id,
          rowClass: (k) => (k.is_active && !k.revoked_at) ? "active" : "inactive revoked",
          emptyMessage: t("keys.list.empty"),
          renameable: (k) => k.label || "",
          onRename: (k, value) => { void onRenameKey(k.id, value); },
          quickActions: () => [
            { action: "edit", title: t("keys.list.btn.edit"), icon: icons.pencil() },
            { action: "usage", title: t("keys.list.btn.usage"), icon: icons.lightning() },
            { action: "delete", title: t("keys.list.btn.delete"), icon: icons.trash(), danger: true },
          ],
          menuItems: (k) => [
            { action: "rename", label: t("grid.menu.rename"), shortcut: "F2" },
            { action: "copy_prefix", label: t("grid.menu.copy_prefix") },
            { action: "usage", label: t("keys.list.btn.usage"), shortcut: "U" },
            { separator: true },
            { action: "edit", label: t("keys.list.btn.edit") },
            { action: "regenerate", label: t("keys.list.btn.regenerate") },
            { separator: true },
            ...(k.is_active && !k.revoked_at ? [{ action: "revoke", label: t("keys.list.btn.revoke") }] : []),
            { action: "delete", label: t("keys.list.btn.delete"), shortcut: "Supr", danger: true },
          ],
          onAction: (k, action) => {
            switch (action) {
              case "rename": return; // handled through inline editing
              case "copy_prefix": void navigator.clipboard?.writeText(k.key_prefix || ""); showToast(t("grid.toast.copied")); return;
              case "edit": return onShowEditKey(k.id);
              case "usage": return onViewKeyUsage(k.id);
              case "regenerate": return void onRegenerateKey(k.id, k.label);
              case "revoke": return void onRevokeKey(k.id, k.label);
              case "delete": return void onDeleteKey(k.id, k.label);
            }
          },
          bulkActions: [
            { key: "delete", label: t("keys.list.btn.delete"), danger: true, run: (picked) => void onBulkDeleteKeys(picked) },
          ],
          bulkCountLabel: (n) => t("grid.bulk.selected", { n }),
          __refresh: () => requestUpdate(),
        })}
      `;
  return html`
    <div class="page-header"><h2>${t("keys.list.heading")}</h2>
      <div class="actions"><button class="primary" @click=${onShowCreateKey}>${icons.plus()} ${t("keys.list.btn.create")}</button></div>
    </div>
    ${body}
  `;
}


export async function mountKeys(
  target?: HTMLElement | null | string,
): Promise<(() => void) | void> {
  const container = (target instanceof HTMLElement) ? target : document.getElementById("main");
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
    container,
  );
}