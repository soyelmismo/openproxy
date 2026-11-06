// views/providers/list.ts — dense listing for every provider based on renderDataGrid.
// Includes status filter tabs, refresh-all header, empty state, error banner,
// inline rename, right-click context menu, hover quick actions, and bulk delete.

import { html, type TemplateResult } from 'lit-html';
import { state } from '../../state/index.js';
import { api } from '../../state/api.js';
import { requestUpdate } from '../../state/reactive.js';
import { showApiError } from '../../lib/ui-utils.js';
import { showToast } from '../../components/toast.js';
import { showConfirm, showPrompt } from '../../lib/show-confirm.js';
import { icons } from '../../lib/icons.js';
import {
  showCreateProvider,
  refreshProvider,
  toggleProviderActive,
} from '../../handlers/provider-handlers/index.js';
import { t } from '../../i18n/index.js';
import type { Account, Provider } from '../../lib/types/api.js';
import {
  loadError,
  renderProviderIcon,
  getProviderCountryCode,
} from './shared.js';
import {
  renderDataGrid,
  type GridColumn,
} from '../../components/data-grid.js';

export type ProviderListFilter = 'all' | 'active' | 'inactive';
const PROVIDERS_LIST_FILTER_STORAGE_KEY = 'openproxy.providers.filter';

export function getProviderListFilter(): ProviderListFilter {
  try {
    const raw = localStorage.getItem(PROVIDERS_LIST_FILTER_STORAGE_KEY);
    if (raw === 'active' || raw === 'inactive' || raw === 'all') {
      return raw;
    }
  } catch {
    // localStorage unavailable/restricted
  }
  return 'all';
}

export function setProviderListFilter(filter: ProviderListFilter): void {
  try {
    localStorage.setItem(PROVIDERS_LIST_FILTER_STORAGE_KEY, filter);
  } catch {
    // localStorage unavailable/restricted
  }
}

function onUpdateListFilter(filter: ProviderListFilter): void {
  setProviderListFilter(filter);
  requestUpdate();
}

async function onRefreshAllProviders(): Promise<void> {
  try {
    const providers = (await api('/providers')) as Array<{ id: string }>;
    for (const p of providers) {
      try {
        await api('/providers/' + encodeURIComponent(p.id) + '/refresh', { method: 'POST' });
      } catch (err: unknown) {
        console.error('Failed to refresh', p.id, err);
      }
    }
    state.providers = (await api('/providers')) as typeof state.providers;
    state.models = (await api('/models')) as typeof state.models;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

function onShowCreateProvider(): void {
  showCreateProvider();
}

function onOpenProviderDetail(providerId: string): void {
  location.hash = `#/providers/${encodeURIComponent(providerId)}`;
}

async function onRenameProviderInline(p: Provider, newName: string): Promise<void> {
  const trimmed = newName.trim();
  if (!trimmed) {
    showToast('Name cannot be empty', 'error');
    return;
  }
  if (trimmed === p.name) return;

  const collision = (state.providers || []).find(
    (other) => other.id !== p.id && other.name === trimmed,
  );
  if (collision) {
    const ok = await showConfirm({
      title: 'Name collision',
      message: `A provider with this name already exists (${collision.id}). Use this name anyway?`,
      confirmLabel: 'Use anyway',
    });
    if (!ok) return;
  }

  try {
    await api('/providers/' + encodeURIComponent(p.id), {
      method: 'PATCH',
      body: JSON.stringify({ name: trimmed }),
    });
    state.providers = ((state.providers || []) as Provider[]).map((row) =>
      row.id === p.id ? { ...row, name: trimmed } : row,
    ) as typeof state.providers;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

async function onEditBaseUrlPrompt(p: Provider): Promise<void> {
  const newUrl = await showPrompt(
    `Edit endpoint for provider "${p.id}"`,
    'Base URL:\n(e.g., https://api.openai.com/v1)',
    p.base_url,
  );
  if (newUrl == null) return;
  const trimmed = newUrl.trim();
  if (trimmed === '') {
    showToast('Base URL cannot be empty', 'error');
    return;
  }
  if (trimmed === p.base_url) return;

  try {
    await api('/providers/' + encodeURIComponent(p.id), {
      method: 'PATCH',
      body: JSON.stringify({ base_url: trimmed }),
    });
    state.providers = ((state.providers || []) as Provider[]).map((row) =>
      row.id === p.id ? { ...row, base_url: trimmed } : row,
    ) as typeof state.providers;
    showToast('Provider endpoint updated', 'success');
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

async function onDeleteProvider(p: Provider): Promise<void> {
  if (p.metadata?.deletable === false) {
    showToast('Built-in providers cannot be deleted. Deactivate them instead.', 'warning');
    return;
  }

  const typed = await showPrompt('Delete provider', `Type the provider ID to confirm deletion: ${p.id}`);
  if (typed !== p.id) {
    if (typed != null) {
      showToast(`Provider id "${typed}" does not match. Nothing was deleted.`, 'warning');
    }
    return;
  }

  if (
    !(await showConfirm({
      title: 'Really delete?',
      message: `Really delete ${p.id}? This cascades to all its accounts and models.`,
      danger: true,
      confirmLabel: 'Delete',
    }))
  )
    return;

  try {
    await api('/providers/' + encodeURIComponent(p.id), { method: 'DELETE' });
    state.providers = (state.providers || []).filter((row) => row.id !== p.id);
    state.models = (state.models || []).filter((m) => m.provider_id !== p.id);
    state.accounts = (state.accounts || []).filter((a) => a.provider_id !== p.id);
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

async function onBulkDeleteProviders(picked: Provider[]): Promise<void> {
  if (picked.length === 0) return;
  const nonDeletable = picked.filter((p) => p.metadata?.deletable === false);
  const deletable = picked.filter((p) => p.metadata?.deletable !== false);

  if (deletable.length === 0) {
    showToast('Built-in providers cannot be deleted. Deactivate them instead.', 'warning');
    return;
  }

  if (
    !(await showConfirm({
      title: `Delete ${deletable.length} providers`,
      message: `Really delete ${deletable.map((p) => p.id).join(', ')}? This cascades to all their accounts and models.`,
      danger: true,
      confirmLabel: 'Delete',
    }))
  )
    return;

  const failed: string[] = [];
  for (const p of deletable) {
    try {
      await api('/providers/' + encodeURIComponent(p.id), { method: 'DELETE' });
      state.providers = (state.providers || []).filter((row) => row.id !== p.id);
      state.models = (state.models || []).filter((m) => m.provider_id !== p.id);
      state.accounts = (state.accounts || []).filter((a) => a.provider_id !== p.id);
    } catch {
      failed.push(p.id);
    }
  }
  requestUpdate();
  if (failed.length > 0) {
    showToast(`Failed to delete: ${failed.join(', ')}`, 'error');
  }
  void nonDeletable;
}

function getProviderQuotaSummary(accounts: Account[]): {
  text: string;
  level: 'ok' | 'warn' | 'danger' | 'none';
  title?: string;
} {
  const withQuota = accounts.filter(
    (a) => a.quota_session_limit != null || a.quota_weekly_limit != null,
  );
  if (withQuota.length === 0) {
    return { text: '—', level: 'none' };
  }

  const pcts: number[] = [];
  for (const a of withQuota) {
    if (a.quota_session_used != null && a.quota_session_limit != null && a.quota_session_limit > 0) {
      pcts.push((a.quota_session_used / a.quota_session_limit) * 100);
    } else if (a.quota_weekly_used != null && a.quota_weekly_limit != null && a.quota_weekly_limit > 0) {
      pcts.push((a.quota_weekly_used / a.quota_weekly_limit) * 100);
    }
  }

  if (pcts.length === 0) {
    return { text: '—', level: 'none' };
  }

  const maxPct = Math.round(Math.max(...pcts));
  const level = maxPct > 80 ? 'danger' : maxPct > 50 ? 'warn' : 'ok';
  return {
    text: `${maxPct}%`,
    level,
    title: `Peak quota usage: ${maxPct}% across ${withQuota.length} quota-enabled account(s)`,
  };
}

export function renderProvidersGrid(): TemplateResult {
  if (loadError) {
    return html`
      <div class="page-header"><h2>${t("providers.list.heading")}</h2>
        <div class="actions">
          <button @click=${onRefreshAllProviders}>${t("providers.list.btn.refresh_all")}</button>
          <button class="primary" @click=${onShowCreateProvider}>+ ${t("providers.list.btn.add")}</button>
        </div>
      </div>
      <div class="banner banner-error">${loadError}</div>
    `;
  }
  const list = state.providers || [];
  const currentFilter = getProviderListFilter();
  const activeCount = list.filter((p) => p.active).length;
  const inactiveCount = list.length - activeCount;

  const filteredList = list.filter((p) => {
    if (currentFilter === 'active') return p.active;
    if (currentFilter === 'inactive') return !p.active;
    return true;
  });

  const columns: GridColumn<Provider>[] = [
    {
      key: 'name',
      label: t('providers.title'),
      min: 160,
      elastic: true,
      render: (p) => html`
        <div class="provider-name-cell">
          <span class="provider-icon-wrapper" data-format=${p.format}>
            ${renderProviderIcon(p, 'small')}
          </span>
          <a
            class="provider-name-text"
            href="#/providers/${encodeURIComponent(p.id)}"
            @click=${(e: MouseEvent) => {
              e.stopPropagation();
            }}
          >
            ${p.name}
          </a>
          <code class="provider-id-code">${p.id}</code>
          ${p.active ? html`` : html`<small class="dg-dim">${t("providers.list.card.inactive_suffix")}</small>`}
        </div>
      `,
    },
    {
      key: 'country',
      label: t('proxies.table.col_country'),
      max: 80,
      fold: 3,
      render: (p) => {
        const cc = getProviderCountryCode(p);
        return html`<code class="provider-country-code">${cc}</code>`;
      },
    },
    {
      key: 'accounts',
      label: t('providers.list.card.accounts'),
      max: 120,
      fold: 2,
      render: (p) => {
        const accounts = (state.accounts || []).filter((a) => a.provider_id === p.id);
        const unhealthy = accounts.filter((a) => a.health_status === 'unhealthy').length;
        return html`
          <span class="dg-dim">${accounts.length}</span>
          ${unhealthy > 0
            ? html`<span class="badge badge-error provider-stat-badge">${t('providers.list.card.unhealthy_badge', { count: unhealthy })}</span>`
            : html``}
        `;
      },
    },
    {
      key: 'models',
      label: t('providers.list.card.models'),
      max: 110,
      fold: 1,
      render: (p) => {
        const active = p.active_models ?? 0;
        const total = p.total_models ?? 0;
        return html`<span class="dg-mono">${active}/${total}</span>`;
      },
    },
    {
      key: 'quota',
      label: t('config.quota.title'),
      max: 100,
      fold: 4,
      render: (p) => {
        const accounts = (state.accounts || []).filter((a) => a.provider_id === p.id);
        const q = getProviderQuotaSummary(accounts);
        if (q.level === 'none') {
          return html`<span class="dg-dim">—</span>`;
        }
        return html`
          <span
            class="provider-quota-badge quota-${q.level}"
            title=${q.title ?? ''}
          >
            ${q.text}
          </span>
        `;
      },
    },
    {
      key: 'status',
      label: t('keys.list.col.status'),
      render: (p) => {
        const statusText = p.active
          ? t('keys.list.status.active')
          : t('keys.list.status.inactive');
        return html`<span
          class="dg-dot ${p.active ? 'on' : 'off'}"
          title=${statusText}
          aria-label=${statusText}
        ></span>`;
      },
    },
  ];

  const gridContent: TemplateResult =
    list.length === 0
      ? html`<div class="empty-state">
          <h3>${t("providers.list.empty.title")}</h3>
          <p>${t("providers.list.empty.subtitle")}</p>
          <button class="primary" @click=${onShowCreateProvider}>${icons.plus()} ${t("providers.list.btn.add")}</button>
        </div>`
      : filteredList.length === 0
        ? html`<div class="empty-state">
            <h3>${t("providers.list.empty.filtered")}</h3>
            <button class="small" @click=${() => onUpdateListFilter('all')}>${t("providers.list.filter.all")} (${list.length})</button>
          </div>`
        : renderDataGrid<Provider>({
            id: 'providers-grid',
            className: 'providers-table',
            columns,
            rows: filteredList,
            rowKey: (p) => p.id,
            rowClass: (p) => (p.active ? 'active' : 'inactive'),
            emptyMessage: t('providers.list.empty.filtered'),
            renameable: (p) => p.name,
            onRename: (p, value) => {
              void onRenameProviderInline(p, value);
            },
            quickActions: () => [
              {
                action: 'open',
                title: t('keys.list.btn.edit'),
                icon: icons.pencil(),
              },
              {
                action: 'refresh',
                title: t('common.refresh'),
                icon: icons.refresh(),
              },
              {
                action: 'delete',
                title: t('common.delete'),
                icon: icons.trash(),
                danger: true,
              },
            ],
            menuItems: (p) => [
              { action: 'open', label: t('grid.menu.properties') },
              { action: 'rename', label: t('grid.menu.rename'), shortcut: 'F2' },
              { action: 'copy_id', label: t('grid.menu.copy_prefix') },
              { separator: true },
              { action: 'refresh', label: t('common.refresh') },
              { action: 'edit_endpoint', label: 'Edit endpoint...' },
              {
                action: 'toggle_active',
                label: p.active ? 'Deactivate' : 'Activate',
              },
              { separator: true },
              {
                action: 'delete',
                label: t('common.delete'),
                shortcut: 'Supr',
                danger: true,
                disabled: p.metadata?.deletable === false,
              },
            ],
            onAction: (p, action) => {
              switch (action) {
                case 'rename':
                  return; // handled through inline editing
                case 'open':
                  return onOpenProviderDetail(p.id);
                case 'copy_id':
                  void navigator.clipboard?.writeText(p.id);
                  showToast(t('grid.toast.copied'));
                  return;
                case 'refresh':
                  return void refreshProvider(p.id, null);
                case 'edit_endpoint':
                  return void onEditBaseUrlPrompt(p);
                case 'toggle_active':
                  return void toggleProviderActive(p.id, !p.active);
                case 'delete':
                  return void onDeleteProvider(p);
              }
            },
            bulkActions: [
              {
                key: 'delete',
                label: t('common.delete'),
                danger: true,
                run: (picked) => void onBulkDeleteProviders(picked),
              },
            ],
            bulkCountLabel: (n) => t('grid.bulk.selected', { n }),
            __refresh: () => requestUpdate(),
          });

  return html`
    <div class="providers-toolbar" role="toolbar" aria-label=${t("providers.list.heading")}>
      <div class="providers-toolbar-filters">
        <div class="filter-tabs" role="group" aria-label="Provider status filter">
          <button
            type="button"
            class=${'filter-tab ' + (currentFilter === 'all' ? 'active' : '')}
            aria-pressed=${currentFilter === 'all'}
            @click=${() => onUpdateListFilter('all')}
          >
            ${t("providers.list.filter.all")} (${list.length})
          </button>
          <button
            type="button"
            class=${'filter-tab ' + (currentFilter === 'active' ? 'active' : '')}
            aria-pressed=${currentFilter === 'active'}
            @click=${() => onUpdateListFilter('active')}
          >
            ${t("providers.list.filter.active")} (${activeCount})
          </button>
          <button
            type="button"
            class=${'filter-tab ' + (currentFilter === 'inactive' ? 'active' : '')}
            aria-pressed=${currentFilter === 'inactive'}
            @click=${() => onUpdateListFilter('inactive')}
          >
            ${t("providers.list.filter.inactive")} (${inactiveCount})
          </button>
        </div>
      </div>
      <div class="actions">
        <button aria-label=${t("providers.list.btn.refresh_all")} title=${t("providers.list.btn.refresh_all")} @click=${onRefreshAllProviders}>${icons.refresh()}<span>${t("providers.list.btn.refresh_all")}</span></button>
        <button class="primary" aria-label=${t("providers.list.btn.add")} title=${t("providers.list.btn.add")} @click=${onShowCreateProvider}>${icons.plus()}<span>${t("providers.list.btn.add")}</span></button>
      </div>
    </div>
    ${gridContent}
  `;
}
