// views/providers/models.ts — models section of the provider detail page.
//
// Split out of the former detail.ts monolith (FU1). This module
// owns the models-section template (filter bar, table, bulk actions,
// pagination) plus the UI-state setters (search / filter / sort / page)
// and the per-provider auto-activate / proxy-settings handlers.
//
// Per-row handlers and renderModelRow live in models-row.ts.
// Bulk operation handlers live in models-bulk.ts.
// The custom-model form adapter lives in custom-model.ts.

import { html, type TemplateResult } from 'lit-html';
import { state } from '../../state/index.js';
import { api } from '../../state/api.js';
import { requestUpdate } from '../../state/reactive.js';
import { showApiError } from '../../lib/ui-utils.js';
import { icons } from '../../lib/icons.js';
import {
  applySort,
  SORTABLE_COLUMNS,
  type ModelSort,
  type SortableColumn,
} from '../../components/model-table.js';
import type { Model, Provider } from '../../lib/types/api.js';
import {
  detailProviderId,
  getProviderUi,
  setProviderUi,
  type ProviderDetailUiState,
} from './shared.js';
import { onShowCustomModelForm } from './custom-model.js';
import {
  renderModelRow,
} from './models-row.js';
import {
  onBulkToggleModels,
  onBulkEnableSelected,
  onBulkDisableSelected,
  onBulkSetModalitySelected,
  onBulkTestSelected,
  onBulkDeleteSelected,
} from './models-bulk.js';

// ---- UI-state setters (per-provider filter/search/sort/page) ----

export function onToggleSelectAllModels(e: Event | null): void {
  const target =
    e && e.target && e.target instanceof HTMLInputElement ? e.target : null;
  const checked = target ? target.checked : false;
  if (!detailProviderId) return;
  const ui = getProviderUi(detailProviderId);
  const searchLower = (ui.search || '').toLowerCase();
  const visible = (state.models || [])
    .filter((m) => m.provider_id === detailProviderId)
    .filter((m) => {
      if (ui.filter === 'active' && !m.active) return false;
      if (ui.filter === 'inactive' && m.active) return false;
      if (searchLower && !m.model_id.toLowerCase().includes(searchLower))
        return false;
      return true;
    })
    .map((m) => m.row_id);
  if (checked) {
    for (const id of visible) state.selectedModels.add(id);
  } else {
    for (const id of visible) state.selectedModels.delete(id);
  }
  requestUpdate();
}

export function onClearModelSelection(): void {
  state.selectedModels.clear();
  requestUpdate();
}

export function onUpdateProviderSearch(providerId: string, e: Event): void {
  const target = e.target;
  const value = target instanceof HTMLInputElement ? target.value : '';
  const ui = getProviderUi(providerId);
  ui.search = value;
  ui.page = 1;
  setProviderUi(providerId, ui);
  requestUpdate();
}

export function onUpdateProviderFilter(
  providerId: string,
  filter: 'all' | 'active' | 'inactive',
): void {
  const ui = getProviderUi(providerId);
  ui.filter = filter;
  ui.page = 1;
  setProviderUi(providerId, ui);
  requestUpdate();
}

export function onCycleProviderSort(providerId: string, sortKey: string): void {
  const ui = getProviderUi(providerId);
  const current = ui.sort;
  let next: ModelSort | null = null;
  if (!current || current.key !== sortKey) {
    next = { key: sortKey, dir: 'asc' };
  } else if (current.dir === 'asc') {
    next = { key: sortKey, dir: 'desc' };
  } else {
    next = null;
  }
  ui.sort = next;
  ui.page = 1;
  setProviderUi(providerId, ui);
  requestUpdate();
}

export function onSetProviderPage(providerId: string, page: number): void {
  const ui = getProviderUi(providerId);
  ui.page = Math.max(1, page);
  setProviderUi(providerId, ui);
  requestUpdate();
}

// ---- Per-provider settings handlers ----

async function onUpdateAutoActivate(
  providerId: string,
  e: Event | null,
): Promise<void> {
  if (e && e.type === 'input') return;
  const target =
    e && e.target && e.target instanceof HTMLInputElement ? e.target : null;
  const value = target ? target.value : '';
  const body = {
    auto_activate_keyword: value && value.trim() ? value.trim() : null,
  };
  try {
    await api(`/providers/${encodeURIComponent(providerId)}`, {
      method: 'PATCH',
      body: JSON.stringify(body),
    });
    state.providers = (await api('/providers')) as typeof state.providers;
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

async function onUpdateUseProxies(
  providerId: string,
  e: Event,
): Promise<void> {
  const target = e.target instanceof HTMLInputElement ? e.target : null;
  if (!target) return;
  const value = target.checked;
  const body = { use_proxies: value };
  try {
    await api(`/providers/${encodeURIComponent(providerId)}`, {
      method: 'PATCH',
      body: JSON.stringify(body),
    });
    state.providers = (await api('/providers')) as typeof state.providers;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

async function onUpdateProxyRotationErrors(
  providerId: string,
  e: Event,
): Promise<void> {
  if (e.type === 'input') return;
  const target = e.target instanceof HTMLInputElement ? e.target : null;
  if (!target) return;
  const value = target.value.trim();
  const body = {
    proxy_rotation_errors: value || '429,connect_error,timeout',
  };
  try {
    await api(`/providers/${encodeURIComponent(providerId)}`, {
      method: 'PATCH',
      body: JSON.stringify(body),
    });
    state.providers = (await api('/providers')) as typeof state.providers;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

async function onUpdateProxyRotationMode(
  providerId: string,
  e: Event,
): Promise<void> {
  const target = e.target instanceof HTMLSelectElement ? e.target : null;
  if (!target) return;
  const value = target.value.trim();
  const body = { proxy_rotation_mode: value || 'global' };
  try {
    await api(`/providers/${encodeURIComponent(providerId)}`, {
      method: 'PATCH',
      body: JSON.stringify(body),
    });
    state.providers = (await api('/providers')) as typeof state.providers;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

async function onToggleIncrementalRace(
  providerId: string,
  e: Event,
): Promise<void> {
  const target = e.target instanceof HTMLInputElement ? e.target : null;
  if (!target) return;
  const newMode = target.checked ? 'incremental_race' : 'global';
  try {
    await api(`/providers/${encodeURIComponent(providerId)}`, {
      method: 'PATCH',
      body: JSON.stringify({ proxy_rotation_mode: newMode }),
    });
    state.providers = (await api('/providers')) as typeof state.providers;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

// ---- Render: sortable <th> helper ----

function renderSortableTh(
  col: SortableColumn,
  sort: ModelSort | null,
  providerId: string,
): TemplateResult {
  const isActive = !!(sort && sort.key === col.key);
  const indicator: string = isActive
    ? sort && sort.dir === 'desc'
      ? ' ▼'
      : ' ▲'
    : '';
  return html`<th class=${'sortable' + (isActive ? ' sorted' : '')} @click=${() => onCycleProviderSort(providerId, col.key)}>${col.label}<span class="sort-indicator">${indicator}</span></th>`;
}

// ---- Render: models section (filter bar + table + bulk actions + pagination) ----

export function renderModelsSection(
  provider: Provider,
  providerModels: Model[],
  ui: ProviderDetailUiState,
): TemplateResult {
  const activeModels = providerModels.filter((m) => m.active).length;
  const searchLower = (ui.search || '').toLowerCase();
  const filtered = providerModels.filter((m) => {
    if (ui.filter === 'active' && !m.active) return false;
    if (ui.filter === 'inactive' && m.active) return false;
    if (searchLower && !m.model_id.toLowerCase().includes(searchLower))
      return false;
    return true;
  });
  const sorted = applySort(filtered, ui.sort);
  const totalFiltered = sorted.length;
  const pageSize = ui.pageSize || 50;
  const totalPages = Math.max(1, Math.ceil(totalFiltered / pageSize));
  const currentPage = Math.min(Math.max(1, ui.page || 1), totalPages);
  const startIdx = (currentPage - 1) * pageSize;
  const paginatedModels = sorted.slice(startIdx, startIdx + pageSize);

  const visibleRowIds: number[] = paginatedModels.map((m) => m.row_id);
  const selectedVisible: number = visibleRowIds.filter(
    (id) => (state.selectedModels as Set<number>).has(id),
  ).length;
  const allSelected: boolean =
    visibleRowIds.length > 0 && selectedVisible === visibleRowIds.length;
  const indeterminate: boolean = selectedVisible > 0 && !allSelected;
  const bulkBar: TemplateResult =
    state.selectedModels.size > 0
      ? html`<div class="bulk-actions-bar">
          <span><strong>${state.selectedModels.size}</strong> selected</span>
          <button @click=${() => onBulkEnableSelected(provider.id)}>Enable selected</button>
          <button @click=${() => onBulkDisableSelected(provider.id)}>Disable selected</button>
          <button @click=${() => onBulkSetModalitySelected('chat')}>${icons.chat()} Tag Chat</button>
          <button @click=${() => onBulkSetModalitySelected('image')}>${icons.image()} Tag Image</button>
          <button @click=${() => onBulkSetModalitySelected('embedding')}>${icons.embedding()} Tag Embedding</button>
          <button @click=${() => onBulkSetModalitySelected('audio')}>${icons.audio()} Tag Audio</button>
          <button @click=${() => onBulkTestSelected(provider.id)}>Test selected</button>
          <button class="danger" @click=${() => onBulkDeleteSelected(provider.id)}>Delete selected</button>
          <button class="link" @click=${onClearModelSelection}>Clear selection</button>
        </div>`
      : html``;

  const paginationBar: TemplateResult =
    totalFiltered > pageSize
      ? html`
        <div style="display: flex; justify-content: space-between; align-items: center; padding: 0.75rem 0.5rem; margin-top: 0.5rem; font-size: var(--fs-sm); color: var(--color-text-muted);">
          <div>Showing ${startIdx + 1}–${Math.min(startIdx + pageSize, totalFiltered)} of ${totalFiltered} models</div>
          <div style="display: flex; gap: 0.5rem; align-items: center;">
            <button class="small" ?disabled=${currentPage <= 1} @click=${() => onSetProviderPage(provider.id, 1)} title="First page">${icons.chevronsLeft()} First</button>
            <button class="small" ?disabled=${currentPage <= 1} @click=${() => onSetProviderPage(provider.id, currentPage - 1)} title="Previous page">${icons.chevronLeft()} Prev</button>
            <span style="padding: 0 0.5rem; font-weight: bold; color: var(--color-text);">Page ${currentPage} / ${totalPages}</span>
            <button class="small" ?disabled=${currentPage >= totalPages} @click=${() => onSetProviderPage(provider.id, currentPage + 1)} title="Next page">Next ${icons.chevronRight()}</button>
            <button class="small" ?disabled=${currentPage >= totalPages} @click=${() => onSetProviderPage(provider.id, totalPages)} title="Last page">Last ${icons.chevronsRight()}</button>
          </div>
        </div>`
      : html``;

  return html`
    <section class="detail-section">
      <div class="section-header">
        <h3>Models (${activeModels}/${providerModels.length} active)</h3>
        <div>
          <button @click=${() => onBulkToggleModels(provider.id, true)}>Enable all</button>
          <button @click=${() => onBulkToggleModels(provider.id, false)}>Disable all</button>
          <button class="primary" @click=${() => onShowCustomModelForm(provider.id)}>${icons.plus()} Custom model</button>
        </div>
      </div>

      <div class="auto-activate-bar">
        <label>
          Auto-activate on refresh:
          <input type="text"
                 placeholder="(empty = enable all)"
                 .value=${provider.auto_activate_keyword || ''}
                 @change=${(e: Event) => onUpdateAutoActivate(provider.id, e)}
                 @input=${(e: Event) => onUpdateAutoActivate(provider.id, e)}>
        </label>
        <small>Models whose ID contains this string are auto-enabled on refresh. Empty = enable all new models.</small>
      </div>

      <div class="auto-activate-bar" style="margin-top: 1rem; display: flex; gap: 2rem; align-items: center; flex-wrap: wrap;">
        <label style="display: flex; align-items: center; gap: 0.5rem; margin: 0; font-weight: normal; cursor: pointer;">
          <input type="checkbox"
                 .checked=${!!provider.use_proxies}
                 @change=${(e: Event) => onUpdateUseProxies(provider.id, e)}>
          Use proxies for this provider
        </label>
        ${provider.use_proxies ? html`
          <label style="display: flex; align-items: center; gap: 0.5rem; margin: 0; font-weight: normal; flex: 1;">
            Rotate proxy on errors:
            <input type="text"
                   style="flex: 1; max-width: 300px; padding: 0.25rem 0.5rem; font-size: var(--fs-sm); border: var(--border-w) var(--border-style) var(--color-border); border-radius: var(--radius-sm); background: var(--color-surface); color: var(--color-text);"
                   placeholder="429,connect_error,timeout"
                   .value=${provider.proxy_rotation_errors || '429,connect_error,timeout'}
                   @change=${(e: Event) => onUpdateProxyRotationErrors(provider.id, e)}
                   @input=${(e: Event) => onUpdateProxyRotationErrors(provider.id, e)}>
          </label>
          <label style="display: flex; align-items: center; gap: 0.5rem; margin: 0; font-weight: normal; cursor: pointer;">
            <input type="checkbox"
                   .checked=${provider.proxy_rotation_mode === 'incremental_race'}
                   @change=${(e: Event) => onToggleIncrementalRace(provider.id, e)}>
            Incremental race (2 &rarr; 4 &rarr; 8 &rarr; 16)
          </label>
          <label style="display: flex; align-items: center; gap: 0.5rem; margin: 0; font-weight: normal;">
            Rotation mode:
            <select style="padding: 0.25rem 0.5rem; font-size: var(--fs-sm); border: var(--border-w) var(--border-style) var(--color-border); border-radius: var(--radius-sm); background: var(--color-surface); color: var(--color-text);"
                    @change=${(e: Event) => onUpdateProxyRotationMode(provider.id, e)}>
              <option value="global" ?selected=${provider.proxy_rotation_mode === 'global'}>Global (shared)</option>
              <option value="account" ?selected=${provider.proxy_rotation_mode === 'account'}>Per Account (unique)</option>
              <option value="incremental_race" ?selected=${provider.proxy_rotation_mode === 'incremental_race'}>Incremental Race</option>
              <option value="none" ?selected=${provider.proxy_rotation_mode === 'none'}>None (disabled)</option>
            </select>
          </label>
          ${provider.current_proxy_id ? html`
            <span style="font-size: var(--fs-sm); color: var(--color-text-muted); background: var(--color-surface-soft); padding: 0.25rem 0.5rem; border-radius: var(--radius-sm);">
              Bound Proxy: <code>${provider.current_proxy_id}</code>
            </span>
          ` : html`
            <span style="font-size: var(--fs-sm); color: var(--color-warn); background: var(--color-warn-soft); padding: 0.25rem 0.5rem; border-radius: var(--radius-sm);">
              No active proxy bound
            </span>
          `}
        ` : html``}
      </div>

      <div class="filter-bar">
        <input type="text" placeholder="Search models..." .value=${ui.search || ''}
               @input=${(e: Event) => onUpdateProviderSearch(provider.id, e)}>
        <div class="filter-tabs">
          <button class=${'filter-tab ' + (ui.filter === 'all' ? 'active' : '')} @click=${() => onUpdateProviderFilter(provider.id, 'all')}>All (${providerModels.length})</button>
          <button class=${'filter-tab ' + (ui.filter === 'active' ? 'active' : '')} @click=${() => onUpdateProviderFilter(provider.id, 'active')}>Active (${activeModels})</button>
          <button class=${'filter-tab ' + (ui.filter === 'inactive' ? 'active' : '')} @click=${() => onUpdateProviderFilter(provider.id, 'inactive')}>Inactive (${providerModels.length - activeModels})</button>
        </div>
      </div>

      ${bulkBar}

      <div class="table-wrap">
        <table class="models-table responsive-card-table">
          <thead><tr>
            <th><input type="checkbox" .checked=${allSelected} .indeterminate=${indeterminate} @change=${(e: Event) => onToggleSelectAllModels(e)}></th>
            ${SORTABLE_COLUMNS.map((c: SortableColumn) => renderSortableTh(c, ui.sort, provider.id))}
            <th>Capabilities</th><th>Status</th><th>Last test</th><th>Actions</th>
          </tr></thead>
          <tbody id="models-tbody">
            ${paginatedModels.length === 0
              ? html`<tr><td colspan="10" class="empty-row">No models match the filter.</td></tr>`
              : html`${paginatedModels.map((m: Model) => renderModelRow(m))}`}
          </tbody>
        </table>
      </div>
      ${paginationBar}
    </section>
  `;
}
