// views/provider-detail.ts — the provider detail view. This is
// the densest screen in the dashboard: header (icon / name / id /
// format / auth_type / base URL / Refresh models / Activate /
// Delete), OAuth login section, Connections table (accounts with
// health + quota), and the Models table (search / filter tabs /
// bulk action bar / per-row test / enable / delete).
//
// Per spec §3 + §13.8 we do not use inline onclick. Every
// interactive element is wired via data-action / data-arg-N.
//
// State is stored on `state.providerDetail[providerId]` so the
// search box and active/inactive tab survive across navigations
// (matches the old app.js UX). The selection is keyed off
// `state.selectedModels` and is cleared when the user navigates
// to a different provider.

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { escapeHtml, escapeAttr } from "../lib/escape.js";
import { pageHeader } from "../components/page-header.js";
import { card } from "../components/card.js";
import { renderBulkActionsBar } from "../components/model-bulk-actions.js";
import {
  renderModelRows,
  getVisibleModelRowIds,
  updateFilterTabCounts,
  syncSelectAllCheckbox,
  SORTABLE_COLUMNS,
  applySort,
  renderSortableTh,
  type ModelSort,
} from "../components/model-table.js";
import {
  BUILTIN_PROVIDER_IDS,
  OAUTH_PROVIDER_IDS,
  OAUTH_PKCE_PROVIDERS,
  OAUTH_DEVICE_CODE_PROVIDERS,
  providerHasQuota,
} from "../lib/constants.js";
import { renderQuotaCell } from "./quota-cell.js";
import type { Account, Model, Provider } from "../lib/types/api.js";

// Per-provider UI state stored on `state.providerDetail[providerId]`.
// The state is open (`Record<string, unknown>`) on `DashboardState` so
// the dashboard can add more per-provider fields without a type
// change; we narrow locally here. `filter` is the active tab
// ("all"|"active"|"inactive"), `search` is the search box text,
// `sort` is the per-column sort applied on top of the filter.
interface ProviderDetailUiState {
  filter: "all" | "active" | "inactive";
  search: string;
  sort: ModelSort | null;
}

function getProviderUi(providerId: string): ProviderDetailUiState {
  // `state.providerDetail[providerId]` is typed as
  // `Record<string, unknown>`. The migration history (see G2/G3
  // notes in state/index.ts) deliberately kept this map loose to
  // let handlers add fields without a type churn. Here we narrow
  // the three known fields and let the rest stay in the underlying
  // record (so handlers that read a different field still work).
  const raw = state.providerDetail[providerId] as Partial<ProviderDetailUiState> | undefined;
  return {
    filter: raw?.filter ?? "all",
    search: raw?.search ?? "",
    sort: raw?.sort ?? null,
  };
}

function setProviderUi(providerId: string, ui: ProviderDetailUiState): void {
  state.providerDetail[providerId] = ui as unknown as Record<string, unknown>;
}

// Render the header row: icon, name (clickable → rename), id,
// format chip, auth chip, base URL link, plus the action toolbar
// (Refresh models, Activate/Deactivate, Delete).
function renderHeader(provider: Provider): string {
  const isBuiltin = BUILTIN_PROVIDER_IDS.includes(provider.id);
  return `
    <div class="provider-detail-header${provider.active ? "" : " inactive"}">
      <div class="provider-icon icon-large" data-format="${escapeAttr(provider.format)}">${getProviderIconHtml(provider.id, provider.format)}</div>
      <div style="flex:1; min-width:0;">
        <h2><span class="editable" data-action="renameProviderPrompt" data-arg1="${escapeAttr(provider.id)}" data-arg2="${escapeAttr(provider.name)}" title="Click to rename">${escapeHtml(provider.name)}</span> <small>✎</small></h2>
        <code>${escapeHtml(provider.id)}</code>
        <div class="meta">
          <span class="chip" data-format="${escapeAttr(provider.format)}">${escapeHtml(provider.format)}</span>
          <span class="chip">${escapeHtml(provider.auth_type)}</span>
          <a href="${escapeAttr(provider.base_url)}" target="_blank" rel="noopener" class="meta-link">${escapeHtml(provider.base_url)}</a>
          ${provider.active ? "" : '<span class="chip inactive-chip">inactive</span>'}
        </div>
      </div>
      <div class="actions">
        <button data-action="refreshProvider" data-arg1="${escapeAttr(provider.id)}" data-arg2="self">↻ Refresh models</button>
        <button class="primary" data-action="toggleProviderActive" data-arg1="${escapeAttr(provider.id)}" data-arg2="${!provider.active}">
          ${provider.active ? "Deactivate" : "Activate"}
        </button>
        ${isBuiltin
          ? '<button class="locked" disabled title="Built-in providers cannot be deleted. Deactivate them instead.">🔒 Delete (built-in)</button>'
          : `<button class="danger small" data-action="confirmDeleteProvider" data-arg1="${escapeAttr(provider.id)}">Delete</button>`}
      </div>
    </div>
  `;
}

// Three built-in providers get distinct visual markers; custom
// providers fall back to the first letter of their id.
function getProviderIconHtml(providerId: string, _format: string): string {
  const knownLogos: Record<string, string> = { openrouter: "🟢", minimax: "🟡", "opencode-zen": "🟣" };
  const glyph = knownLogos[providerId] || ((providerId[0] || "?").toUpperCase());
  return `<span class="provider-emoji">${glyph}</span>`;
}

// Render the OAuth login section for OAuth-capable providers. The
// existing handlers (oauthStartPKCE, oauthStartDeviceCode) take
// care of the rest; we just render the entry points.
function renderOAuthSection(provider: Provider): string {
  if (!OAUTH_PROVIDER_IDS.includes(provider.id)) return "";
  const buttons: string[] = [];
  if (OAUTH_PKCE_PROVIDERS.includes(provider.id)) {
    buttons.push(`<button class="primary" data-action="oauthStartPKCE" data-arg1="${escapeAttr(provider.id)}">Log in with ${escapeHtml(provider.name || provider.id)}</button>`);
  }
  if (OAUTH_DEVICE_CODE_PROVIDERS.includes(provider.id)) {
    buttons.push(`<button class="primary" data-action="oauthStartDeviceCode" data-arg1="${escapeAttr(provider.id)}">Log in with ${escapeHtml(provider.name || provider.id)}</button>`);
  }
  return `
    <section class="detail-section">
      <div class="section-header"><h3>OAuth login</h3></div>
      <div class="oauth-buttons">${buttons.join(" ")}</div>
      <div id="oauth-device-info" style="display:none;"></div>
      <div id="oauth-manual-section" class="oauth-manual-card" style="display:none;">
        <h4>1. Authorize</h4>
        <p>Open this URL in a new tab and complete the login:</p>
        <div class="oauth-manual-url">
          <input id="oauth-auth-url" type="text" readonly>
          <button type="button" class="btn-secondary" data-action="copyAuthUrl">Copy</button>
        </div>
        <h4>2. Paste the callback URL</h4>
        <p>After the OAuth provider redirects, copy the full URL from your address bar and paste it here:</p>
        <div class="oauth-manual-input">
          <input id="oauth-callback-input" type="text" placeholder="https://...">
          <button type="button" class="primary" data-action="oauthSubmitManualCallback">Submit</button>
        </div>
      </div>
    </section>
  `;
}

// Render the Connections (accounts) table. Mirrors the old
// renderProviderDetail() body: per-row health dropdown, quota
// cell, and a per-row refresh-quota + delete button.
function renderConnectionsSection(provider: Provider, accounts: Account[]): string {
  const hasQuota = providerHasQuota(provider.id);
  const toolbar = `
    <div>
      ${hasQuota ? `<button data-action="refreshAllQuotas" data-arg1="${escapeAttr(provider.id)}">↻ Refresh all quotas</button>` : ""}
      <button class="primary" data-action="showCreateAccount" data-arg1="${escapeAttr(provider.id)}">+ Add account</button>
    </div>
  `;
  let body = "";
  if (accounts.length === 0) {
    body = `<table><tbody><tr><td colspan="6" class="empty-row">No accounts. Add an API key to start using this provider.</td></tr></tbody></table>`;
  } else {
    const rows = accounts.map((a) => {
      const quotaCell = hasQuota
        ? renderQuotaCell(a)
        : '<div class="quota-cell muted"><small>not supported by this provider</small></div>';
      return `
        <tr>
          <td>${escapeHtml(a.label || "—")}</td>
          <td>${a.priority}</td>
          <td>
            <select data-action="setHealth" data-arg1="${a.id}" class="health-select ${escapeAttr(a.health_status || "unknown")}">
              <option value="healthy" ${a.health_status === "healthy" ? "selected" : ""}>healthy</option>
              <option value="degraded" ${a.health_status === "degraded" ? "selected" : ""}>degraded</option>
              <option value="unhealthy" ${a.health_status === "unhealthy" ? "selected" : ""}>unhealthy</option>
            </select>
          </td>
          <td>${quotaCell}</td>
          <td>${escapeHtml(a.created_at || "—")}</td>
          <td>
            ${hasQuota ? `<button class="small" data-action="refreshAccountQuota" data-arg1="${a.id}">↻ Quota</button>` : ""}
            <button class="small danger" data-action="deleteAccount" data-arg1="${a.id}">Delete</button>
          </td>
        </tr>
      `;
    }).join("");
    body = `
      <table>
        <thead><tr><th>Label</th><th>Priority</th><th>Health</th><th>Quota</th><th>Created</th><th>Actions</th></tr></thead>
        <tbody>${rows}</tbody>
      </table>
    `;
  }
  return card(`Connections (${accounts.length})`, toolbar + body, { variant: "detail", actions: "" });
}

// Render the Models section: header with bulk toggles, auto-
// activate keyword input, search/filter bar, bulk action bar,
// and the table itself. The table is split into a <thead> that
// stays stable across partial re-paints and a <tbody id="models-
// tbody"> that updateProviderFilter() re-paints in place so the
// search input keeps focus.
function renderModelsSection(provider: Provider, providerModels: Model[], activeModels: number, ui: ProviderDetailUiState): string {
  const searchLower = (ui.search || "").toLowerCase();
  const filtered = providerModels.filter((m) => {
    if (ui.filter === "active" && !m.active) return false;
    if (ui.filter === "inactive" && m.active) return false;
    if (searchLower && !m.model_id.toLowerCase().includes(searchLower)) return false;
    return true;
  });
  // Apply the per-provider sort (if any). Operates on the
  // post-filter list so the visible rows are what gets sorted; the
  // filter tabs (All/Active/Inactive) and the search box keep
  // their existing semantics independently of the sort state.
  const sorted = applySort(filtered, ui.sort);
  const bulkBar = state.selectedModels.size > 0 ? renderBulkActionsBar(provider.id) : "";
  return `
    <section class="detail-section">
      <div class="section-header">
        <h3>Models (${activeModels}/${providerModels.length} active)</h3>
        <div>
          <button data-action="bulkToggleModels" data-arg1="${escapeAttr(provider.id)}" data-arg2="true">Enable all</button>
          <button data-action="bulkToggleModels" data-arg1="${escapeAttr(provider.id)}" data-arg2="false">Disable all</button>
          <button class="primary" data-action="showCustomModelForm" data-arg1="${escapeAttr(provider.id)}">+ Custom model</button>
        </div>
      </div>

      <div class="auto-activate-bar">
        <label>
          Auto-activate on refresh:
          <input type="text"
                 id="auto-activate-input-${escapeAttr(provider.id)}"
                 placeholder="(empty = enable all)"
                 value="${escapeAttr(provider.auto_activate_keyword || "")}"
                 data-action="updateAutoActivate" data-arg1="${escapeAttr(provider.id)}">
        </label>
        <small>Models whose ID contains this string are auto-enabled on refresh. Empty = enable all new models.</small>
      </div>

      <div class="filter-bar">
        <input type="text" id="search-input-${escapeAttr(provider.id)}" placeholder="Search models..." value="${escapeAttr(ui.search || "")}"
               data-action="updateProviderFilter" data-arg1="${escapeAttr(provider.id)}" data-arg2="search">
        <div class="filter-tabs">
          <button id="filter-tab-${escapeAttr(provider.id)}-all" class="filter-tab ${ui.filter === "all" ? "active" : ""}" data-action="updateProviderFilter" data-arg1="${escapeAttr(provider.id)}" data-arg2="filter" data-arg3="all">All (${providerModels.length})</button>
          <button id="filter-tab-${escapeAttr(provider.id)}-active" class="filter-tab ${ui.filter === "active" ? "active" : ""}" data-action="updateProviderFilter" data-arg1="${escapeAttr(provider.id)}" data-arg2="filter" data-arg3="active">Active (${activeModels})</button>
          <button id="filter-tab-${escapeAttr(provider.id)}-inactive" class="filter-tab ${ui.filter === "inactive" ? "active" : ""}" data-action="updateProviderFilter" data-arg1="${escapeAttr(provider.id)}" data-arg2="filter" data-arg3="inactive">Inactive (${providerModels.length - activeModels})</button>
        </div>
      </div>

      ${bulkBar}

      <table>
        <thead><tr>
          <th><input type="checkbox" id="model-select-all" data-action="toggleSelectAllModels"></th>
          ${SORTABLE_COLUMNS.map((c) => renderSortableTh(c, ui.sort, provider.id)).join("")}
          <th>Capabilities</th><th>Status</th><th>Actions</th>
        </tr></thead>
        <tbody id="models-tbody">
          ${sorted.length === 0
            ? `<tr><td colspan="10" class="empty-row">No models match the filter.</td></tr>`
            : renderModelRows(sorted)}
        </tbody>
      </table>
    </section>
  `;
}

// Top-level: render the full provider detail view. Caller is
// expected to have populated state.providers / state.accounts /
// state.models. We fetch on a cold paint (matching the old
// app.js UX) and re-render from cache on a background re-render.
export async function renderProviderDetail(providerId: string): Promise<void> {
  // Switching providers always starts with an empty selection — the
  // visible row_ids live in the previous provider's table, and a
  // bulk-action on those would silently hit the wrong models. But
  // re-renders triggered by the user interacting with checkboxes /
  // the filter / the search / the background poll must NOT clear
  // the in-progress selection.
  if (state.selectedModelsProvider !== providerId) {
    state.selectedModels.clear();
    state.selectedModelsProvider = providerId;
  }
  // Cold paint: fetch. Warm re-render (from cache after navigate()
  // or a re-render via updateProviderFilter): skip the network.
  if (state.providers.length === 0) {
    const [providers, accounts, models] = await Promise.all([
      api("/providers") as Promise<Provider[]>,
      api("/accounts") as Promise<Account[]>,
      api("/models") as Promise<Model[]>,
    ]);
    state.providers = providers;
    state.accounts = accounts;
    state.models = models;
  }
  const provider = (state.providers || []).find((p) => p.id === providerId);
  if (!provider) {
    const el = document.getElementById("main");
    if (el) el.innerHTML =
      `<div class="banner banner-error">Provider ${escapeHtml(providerId)} not found. <a href="#/providers">← Back</a></div>`;
    return;
  }
  const accounts = (state.accounts || []).filter((a) => a.provider_id === providerId);
  const providerModels = (state.models || []).filter((m) => m.provider_id === providerId);
  const activeModels = providerModels.filter((m) => m.active).length;

  // Per-provider UI state. Default to "all" / empty search on first
  // visit; keep the user's previous selection on subsequent visits.
  // `sort` is `{ key, dir }` or `null` for upstream order.
  if (!state.providerDetail[providerId]) {
    setProviderUi(providerId, { filter: "all", search: "", sort: null });
  } else if ((state.providerDetail[providerId] as Partial<ProviderDetailUiState>).sort === undefined) {
    // Backfill the `sort` field for providers visited before the
    // sortable-headers feature landed; otherwise their first click
    // would treat the missing field as "already in the desc state"
    // (since `!undefined.dir` is ambiguous).
    const existing = state.providerDetail[providerId] as Partial<ProviderDetailUiState>;
    setProviderUi(providerId, { filter: existing.filter ?? "all", search: existing.search ?? "", sort: null });
  }
  const ui = getProviderUi(providerId);

  const header = pageHeader({ title: provider.name, back: { href: "#/providers", label: "← All providers" } });
  const body = [
    renderHeader(provider),
    renderOAuthSection(provider),
    renderConnectionsSection(provider, accounts),
    renderModelsSection(provider, providerModels, activeModels, ui),
  ].join("");
  const el = document.getElementById("main");
  if (el) el.innerHTML = header + body;

  // After the table is in the DOM, sync the master "select all"
  // checkbox state with reality. We can't rely on the static
  // `checked` attribute because (a) the master checkbox's
  // onchange re-renders the page and drops its `checked` state, and
  // (b) we want an indeterminate visual when only some visible rows
  // are selected. The DOM lookup runs after the innerHTML write
  // below, in a `queueMicrotask`.
  queueMicrotask(() => {
    const visible = getVisibleModelRowIds();
    syncSelectAllCheckbox(visible);
    updateFilterTabCounts(providerId, providerModels);
  });
}
