// views/providers/models.ts — models section: filter bar, table, bulk actions,
// pagination, UI-state setters, and the auto-activate / proxy handlers.

import { html, type TemplateResult } from 'lit-html';
import { state } from '../../state/index.js';
import { api } from '../../state/api.js';
import { requestUpdate } from '../../state/reactive.js';
import { showApiError } from '../../lib/ui-utils.js';
import { showToast } from '../../components/toast.js';
import { icons } from '../../lib/icons.js';
import { t } from '../../i18n/index.js';
import { statusPillClass } from '../../lib/constants.js';
import { formatContextBadge } from '../../lib/format.js';
import { renderCapabilityBadges } from './shared.js';
import { renderDataGrid, toggleGridRowExpansion, type GridColumn } from '../../components/data-grid.js';
import type { Model, Provider } from '../../lib/types/api.js';
import {
  getProviderUi,
  setProviderUi,
  type ProviderDetailUiState,
} from './shared.js';
import { onShowCustomModelForm } from './custom-model.js';
import {
  onToggleModel,
  onTestModel,
  onDeleteModel,
  onCopyUsageModel,
  onChangeModelType,
  renderModelConfigDrawer,
} from './models-row.js';
import {
  onBulkToggleModels,
  onBulkEnableSelected,
  onBulkDisableSelected,
  onBulkTestSelected,
  onBulkDeleteSelected,
} from './models-bulk.js';

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

export function onSetProviderPage(providerId: string, page: number): void {
  const ui = getProviderUi(providerId);
  ui.page = Math.max(1, page);
  setProviderUi(providerId, ui);
  requestUpdate();
}

// "notify on keyword matches only" toggle. Only sent alongside a keyword;
// clearing it (onUpdateAutoActivate) hides the toggle, so the field is never
// sent alone. Three-state: unset -> null on clear.
async function onToggleNotifKeywordOnly(
  providerId: string,
  next: boolean,
): Promise<void> {
  const body = { notif_keyword_only: next ? true : null };
  try {
    await api(`/providers/${encodeURIComponent(providerId)}`, {
      method: 'PATCH',
      body: JSON.stringify(body),
    });
    const p = (state.providers || []).find((x) => x.id === providerId);
    if (p) p.notif_keyword_only = next;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

async function onTogglePruneModels(
  providerId: string,
  e: Event,
): Promise<void> {
  const target = e.target instanceof HTMLInputElement ? e.target : null;
  if (!target) return;
  const body = { prune_models: target.checked };
  try {
    await api(`/providers/${encodeURIComponent(providerId)}`, {
      method: 'PATCH',
      body: JSON.stringify(body),
    });
    const p = (state.providers || []).find((x) => x.id === providerId);
    if (p) p.prune_models = target.checked;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

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

async function onToggleDirectFirst(
  providerId: string,
  e: Event,
): Promise<void> {
  const target = e.target instanceof HTMLInputElement ? e.target : null;
  if (!target) return;
  const value = target.checked;
  const body: { direct_first: boolean; current_proxy_id?: null } = {
    direct_first: value,
  };
  if (value) {
    body.current_proxy_id = null;
  }
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

async function onResetToDirectIp(providerId: string): Promise<void> {
  try {
    await api(`/providers/${encodeURIComponent(providerId)}`, {
      method: 'PATCH',
      body: JSON.stringify({ current_proxy_id: null }),
    });
    state.providers = (await api('/providers')) as typeof state.providers;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}


export function renderProviderModelSettings(
  provider: Provider,
): TemplateResult {
  return html`
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

    ${provider.auto_activate_keyword && provider.auto_activate_keyword.trim()
      ? html`<div class="auto-activate-bar" style="margin-top: 0.5rem; display: flex; align-items: center; gap: 0.5rem; flex-wrap: wrap;">
          <label style="display: flex; align-items: center; gap: 0.5rem; margin: 0; font-weight: normal; cursor: pointer;">
            <button type="button" role="switch" aria-checked=${provider.notif_keyword_only ? 'true' : 'false'}
                    class="toggle-btn ${provider.notif_keyword_only ? 'on' : 'off'}"
                    @click=${() => { void onToggleNotifKeywordOnly(provider.id, !provider.notif_keyword_only); }}>
              <span class="toggle-thumb"></span>
            </button>
            ${t('providers.detail.notif_keyword_only')}
          </label>
          <small>${provider.notif_keyword_only
            ? t('providers.detail.notif_keyword_only_help_on')
            : t('providers.detail.notif_keyword_only_help_off')}</small>
        </div>`
      : html``}

    <div class="auto-activate-bar" style="margin-top: 1rem; display: flex; gap: 2rem; align-items: center; flex-wrap: wrap;">
      <label style="display: flex; align-items: center; gap: 0.5rem; margin: 0; font-weight: normal; cursor: pointer;" title="When enabled (default), models omitted from upstream /models are deleted (unless pinned in combos). Disable for volatile/P2P providers like antseed to retain all discovered models.">
        <input type="checkbox"
               .checked=${provider.prune_models !== false}
               @change=${(e: Event) => onTogglePruneModels(provider.id, e)}>
        Prune missing models on refresh
      </label>
      <label style="display: flex; align-items: center; gap: 0.5rem; margin: 0; font-weight: normal; cursor: pointer;">
        <input type="checkbox"
               .checked=${!!provider.use_proxies}
               @change=${(e: Event) => onUpdateUseProxies(provider.id, e)}>
        Use proxies for this provider
      </label>
      ${provider.use_proxies ? html`
        <label style="display: flex; align-items: center; gap: 0.5rem; margin: 0; font-weight: normal; cursor: pointer;" title="Send requests directly using server real IP first; rotate to proxy pool only when proxy rotation errors appear">
          <input type="checkbox"
                 .checked=${!!provider.direct_first}
                 @change=${(e: Event) => onToggleDirectFirst(provider.id, e)}>
          Use real IP first (rotate to proxy on error)
        </label>
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
          <select class="provider-rotation-select"
                  @change=${(e: Event) => onUpdateProxyRotationMode(provider.id, e)}>
            <option value="global" ?selected=${provider.proxy_rotation_mode === 'global'}>Global (shared)</option>
            <option value="account" ?selected=${provider.proxy_rotation_mode === 'account'}>Per Account (unique)</option>
            <option value="incremental_race" ?selected=${provider.proxy_rotation_mode === 'incremental_race'}>Incremental Race</option>
            <option value="none" ?selected=${provider.proxy_rotation_mode === 'none'}>None (disabled)</option>
          </select>
        </label>
        ${provider.current_proxy_id ? html`
          <span style="display: inline-flex; align-items: center; gap: 0.5rem; font-size: var(--fs-sm); color: var(--color-text-muted); background: var(--color-surface-soft); padding: 0.25rem 0.5rem; border-radius: var(--radius-sm);">
            <span>Bound Proxy: <code>${provider.current_proxy_id}</code></span>
            ${provider.direct_first ? html`
              <button type="button" class="btn btn-xs" style="padding: 0.1rem 0.4rem; font-size: 0.75rem;" title="Clear bound proxy and return to direct real IP" @click=${() => onResetToDirectIp(provider.id)}>
                Reset to real IP
              </button>
            ` : html``}
          </span>
        ` : html`
          <span style="font-size: var(--fs-sm); color: ${provider.direct_first ? 'var(--color-primary, #38bdf8)' : 'var(--color-warn)'}; background: ${provider.direct_first ? 'var(--color-primary-soft, rgba(56, 189, 248, 0.1))' : 'var(--color-warn-soft)'}; padding: 0.25rem 0.5rem; border-radius: var(--radius-sm);">
            ${provider.direct_first ? 'Using real IP (direct first)' : 'No active proxy bound'}
          </span>
        `}
      ` : html``}
    </div>
  `;
}

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

  const columns: GridColumn<Model>[] = [
    {
      key: 'model_id',
      label: t('common.filter') || 'Model',
      min: 150,
      elastic: true,
      render: (m) => {
        const usageModel = `${m.provider_id}/${m.model_id}`;
        return html`
          <div class="model-name-cell">
            <code
              class="model-usage-code"
              title=${usageModel + ' · Click to copy usage model'}
              @click=${(e: Event) => {
                e.stopPropagation();
                void onCopyUsageModel(usageModel, e);
              }}
            >${usageModel}</code>
            ${m.custom ? html`<span class="badge custom">custom</span>` : html``}
            ${m.client_profile ? html`<span class="model-profile-badge" title="Spoofed Client Profile">${m.client_profile}</span>` : html``}
          </div>
        `;
      },
    },
    {
      key: 'modality',
      label: t('providers.detail.entry.modality') || 'Modality',
      max: 95,
      fold: 4,
      render: (m) => html`<span class="chip">${(m.model_type || 'chat').toUpperCase()}</span>`,
    },
    {
      key: 'format',
      label: t('providers.detail.entry.format') || 'Format',
      max: 95,
      fold: 5,
      render: (m) => html`<code class="dg-mono">${m.target_format || 'openai'}</code>`,
    },
    {
      key: 'context',
      label: 'Context',
      max: 90,
      fold: 2,
      render: (m) => html`<span class="dg-mono">${formatContextBadge(m.context_length)}</span>`,
    },
    {
      key: 'output',
      label: 'Output',
      max: 85,
      fold: 3,
      render: (m) => html`<span class="dg-mono">${formatContextBadge(m.max_output_tokens)}</span>`,
    },
    {
      key: 'capabilities',
      label: 'Caps',
      max: 130,
      fold: 6,
      render: (m) => html`
        <span class="model-caps-wrap">
          ${renderCapabilityBadges(m.capabilities_json, m.model_type)}
        </span>
      `,
    },
    {
      key: 'last_test',
      label: 'Test',
      max: 80,
      fold: 1,
      render: (m) => {
        if (m.last_test_status == null) return html`<span class="dg-dim">never</span>`;
        return html`<span class=${'status-pill ' + statusPillClass(m.last_test_status)} title=${m.last_test_at || ''}>${String(m.last_test_status)}</span>`;
      },
    },
    {
      key: 'status',
      label: t('keys.list.col.status') || 'Status',
      render: (m) => {
        const text = m.active ? 'Active' : 'Inactive';
        return html`<span class="dg-dot ${m.active ? 'on' : 'off'}" title=${text} aria-label=${text}></span>`;
      },
    },
  ];

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

      <div class="filter-bar">
        <input type="text" placeholder="Search models..." .value=${ui.search || ''}
               @input=${(e: Event) => onUpdateProviderSearch(provider.id, e)}>
        <div class="filter-tabs">
          <button class=${'filter-tab ' + (ui.filter === 'all' ? 'active' : '')} @click=${() => onUpdateProviderFilter(provider.id, 'all')}>All (${providerModels.length})</button>
          <button class=${'filter-tab ' + (ui.filter === 'active' ? 'active' : '')} @click=${() => onUpdateProviderFilter(provider.id, 'active')}>Active (${activeModels})</button>
          <button class=${'filter-tab ' + (ui.filter === 'inactive' ? 'active' : '')} @click=${() => onUpdateProviderFilter(provider.id, 'inactive')}>Inactive (${providerModels.length - activeModels})</button>
        </div>
      </div>

      <div class="models-list" id="models-tbody">
        ${filtered.length === 0
          ? html`<p class="empty-row">No models match the filter.</p>`
          : renderDataGrid<Model>({
              id: `provider-models-${provider.id}`,
              className: 'provider-models-table models-table',
              columns,
              rows: filtered,
              rowKey: (m) => m.row_id,
              rowClass: (m) => (m.active ? 'active' : 'inactive'),
              emptyMessage: 'No models match the filter.',
              // a plain click on a model opens its full config drawer:
              // format, client profile, modality, streaming, test harness
              // target and delete — one row open at a time.
              expandedRow: (m) => renderModelConfigDrawer(m),
              quickActions: () => [
                { action: 'test', title: 'Test model', icon: icons.flask() },
                { action: 'toggle', title: 'Toggle active', icon: icons.lightning() },
                { action: 'delete', title: t('common.delete'), icon: icons.trash(), danger: true },
              ],
              menuItems: (m) => [
                { action: 'configure', label: 'Configure model' },
                { action: 'copy_usage', label: 'Copy usage model', shortcut: 'C' },
                { action: 'test', label: 'Test model', shortcut: 'T' },
                { action: 'toggle', label: m.active ? 'Disable model' : 'Enable model' },
                { separator: true },
                { action: 'tag_chat', label: 'Tag Chat' },
                { action: 'tag_image', label: 'Tag Image' },
                { action: 'tag_embedding', label: 'Tag Embedding' },
                { separator: true },
                { action: 'delete', label: t('common.delete'), shortcut: 'Supr', danger: true },
              ],
              onAction: (m, action, rowEl) => {
                switch (action) {
                  case 'configure': {
                    toggleGridRowExpansion(`provider-models-${provider.id}`, m.row_id);
                    requestUpdate();
                    return;
                  }
                  case 'copy_usage': {
                    void navigator.clipboard?.writeText(`${m.provider_id}/${m.model_id}`);
                    showToast(t('grid.toast.copied'));
                    return;
                  }
                  case 'test': return void onTestModel(m.row_id, rowEl.querySelector<HTMLButtonElement>('[title="Test model"]'));
                  case 'toggle': return void onToggleModel(m.row_id, !m.active);
                  case 'tag_chat': {
                    const mockEvt = { target: { value: 'chat' } } as unknown as Event;
                    return void onChangeModelType(m.row_id, mockEvt);
                  }
                  case 'tag_image': {
                    const mockEvt = { target: { value: 'image' } } as unknown as Event;
                    return void onChangeModelType(m.row_id, mockEvt);
                  }
                  case 'tag_embedding': {
                    const mockEvt = { target: { value: 'embedding' } } as unknown as Event;
                    return void onChangeModelType(m.row_id, mockEvt);
                  }
                  case 'delete': return void onDeleteModel(m.row_id);
                }
              },
              bulkActions: [
                {
                  key: 'enable',
                  label: 'Enable',
                  run: (picked) => {
                    state.selectedModels = new Set(picked.map((x) => x.row_id));
                    void onBulkEnableSelected(provider.id);
                  },
                },
                {
                  key: 'disable',
                  label: 'Disable',
                  run: (picked) => {
                    state.selectedModels = new Set(picked.map((x) => x.row_id));
                    void onBulkDisableSelected(provider.id);
                  },
                },
                {
                  key: 'test',
                  label: 'Test',
                  run: (picked) => {
                    state.selectedModels = new Set(picked.map((x) => x.row_id));
                    void onBulkTestSelected(provider.id);
                  },
                },
                {
                  key: 'delete',
                  label: t('common.delete'),
                  danger: true,
                  run: (picked) => {
                    state.selectedModels = new Set(picked.map((x) => x.row_id));
                    void onBulkDeleteSelected(provider.id);
                  },
                },
              ],
              bulkCountLabel: (n) => t('grid.bulk.selected', { n }),
              __refresh: () => requestUpdate(),
            })}
      </div>
    </section>
  `;
}
