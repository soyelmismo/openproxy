// views/keys.js — API keys list. The create / edit / regen /
// revoke / delete handlers live in handlers/key-handlers.js; the
// create/edit modal HTML is built there too (buildModalHtml). This
// view is responsible for the table only.
//
// Per spec §3 + §13.8 we do not use inline `onclick="window.X()"`
// handlers. Buttons carry `data-action="X" data-arg-N="..."` and
// the document-level shim in app.js dispatches them.

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { escapeHtml, escapeAttr } from "../lib/escape.js";
import { pageHeader } from "../components/page-header.js";

export async function mountKeys() {
  const main = document.getElementById("main");
  main.innerHTML = pageHeader({ title: "API Keys" }) + `<div class="loading">Loading...</div>`;
  try {
    const [keys, models] = await Promise.all([api("/keys"), api("/models")]);
    state.apiKeys = keys;
    state.models = models;
  } catch (e) {
    main.innerHTML = pageHeader({ title: "API Keys" }) +
      `<div class="banner banner-error">${escapeHtml(e.message)}</div>`;
    return;
  }
  const keys = state.apiKeys || [];
  let body = "";
  if (keys.length === 0) {
    body = `<p class="empty">No API keys yet. Create one to authenticate clients.</p>`;
  } else {
    body = `<table>
      <thead><tr><th>Label</th><th>Prefix</th><th>Scopes</th><th>Allowed models</th><th>Status</th><th>Last used</th><th>Created</th><th>Actions</th></tr></thead>
      <tbody>`;
    for (const k of keys) {
      const scopes = (k.scopes || []).join(", ") || "—";
      let allowedModels = "all";
      if (k.allowed_models === null || k.allowed_models === undefined) allowedModels = "all";
      else if (Array.isArray(k.allowed_models) && k.allowed_models.length === 0) allowedModels = "(empty)";
      else if (Array.isArray(k.allowed_models)) allowedModels = k.allowed_models.length + " models";
      const isActive = k.is_active && !k.revoked_at;
      const statusClass = isActive ? "on" : "off";
      const statusText = k.revoked_at ? "revoked" : (k.is_active ? "active" : "inactive");
      const createdBy = k.created_by ? ` <small>(${escapeHtml(k.created_by)})</small>` : "";
      const labelAttr = escapeAttr(k.label || "");
      body += `
        <tr>
          <td>${escapeHtml(k.label || "—")}${createdBy}</td>
          <td><code>${escapeHtml(k.key_prefix || "—")}</code></td>
          <td>${escapeHtml(scopes)}</td>
          <td>${escapeHtml(allowedModels)}</td>
          <td><span class="status-pill ${statusClass}">${statusText}</span></td>
          <td>${escapeHtml(k.last_used_at || "never")}</td>
          <td>${escapeHtml(k.created_at || "—")}</td>
          <td>
            <button class="small" data-action="showEditKey" data-arg1="${k.id}">Edit</button>
            <button class="small" data-action="regenerateKey" data-arg1="${k.id}" data-arg2="${labelAttr}">Regenerate</button>
            <button class="small" data-action="viewKeyUsage" data-arg1="${k.id}">Usage</button>
            ${k.is_active && !k.revoked_at ? `<button class="small" data-action="revokeKey" data-arg1="${k.id}" data-arg2="${labelAttr}">Revoke</button>` : ""}
            <button class="small danger" data-action="deleteKey" data-arg1="${k.id}" data-arg2="${labelAttr}">Delete</button>
          </td>
        </tr>
      `;
    }
    body += `</tbody></table>`;
  }
  main.innerHTML = pageHeader({
    title: "API Keys",
    actions: `<button class="primary" data-action="showCreateKey">+ Create key</button>`,
  }) + body;
}
