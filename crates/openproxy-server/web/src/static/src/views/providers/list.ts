// views/providers/list.ts — card grid for every provider, with a refresh-all
// header, empty state, and error banner. Cards link to `#/providers/:id`.

import { html, type TemplateResult } from 'lit-html';
import { state } from '../../state/index.js';
import { api } from '../../state/api.js';
import { requestUpdate } from '../../state/reactive.js';
import { showApiError } from '../../lib/ui-utils.js';
import { icons } from '../../lib/icons.js';
import { showCreateProvider } from '../../handlers/provider-handlers/index.js';
import { t } from '../../i18n/index.js';
import type { Account, Provider } from '../../lib/types/api.js';
import { loadError, renderProviderIcon } from './shared.js';

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


function renderProviderCard(p: Provider, accounts: Account[]): TemplateResult {
  const unhealthyAccs = accounts.filter((a) => a.health_status === 'unhealthy').length;
  const activeModels = p.active_models ?? 0;
  const totalModels = p.total_models ?? 0;
  const cardClasses: string = [
    'provider-card',
    unhealthyAccs > 0 ? 'has-errors' : '',
    p.active ? '' : 'inactive',
  ]
    .filter(Boolean)
    .join(' ');
  return html`<a href="#/providers/${encodeURIComponent(p.id)}" class=${cardClasses}>
    <div class="provider-card-header">
      <div class="provider-icon" data-format=${p.format}>${renderProviderIcon(p)}</div>
      <div class="provider-info">
        <h3>${p.name}${p.active ? html`` : html` <small class="inactive-suffix">${t("providers.list.card.inactive_suffix")}</small>`}</h3>
        <code>${p.id}</code>
      </div>
    </div>
    <div class="provider-card-body">
      <div class="capabilities">
        <span class="chip" data-format=${p.format}>${p.format}</span>
        <span class="chip">${p.auth_type}</span>
      </div>
    </div>
    <div class="provider-card-footer">
      <div class="stat">
        <label>${t("providers.list.card.accounts")}</label>
        <value>${accounts.length}</value>
        ${unhealthyAccs > 0 ? html`<span class="badge badge-error">${t("providers.list.card.unhealthy_badge", { count: unhealthyAccs })}</span>` : html``}
      </div>
      <div class="stat">
        <label>${t("providers.list.card.models")}</label>
        <value>${activeModels}/${totalModels}</value>
      </div>
    </div>
  </a>`;
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

  const cards: TemplateResult =
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
        : html`<div class="provider-grid">${filteredList.map((p) => {
            const accounts = (state.accounts || []).filter((a) => a.provider_id === p.id);
            return renderProviderCard(p, accounts);
          })}</div>`;
  return html`
    <div class="page-header">
      <div class="page-header-title">
        <h2>${t("providers.list.heading")}</h2>
        <div class="filter-tabs" role="tablist" aria-label="Provider status filter">
          <button
            type="button"
            class=${'filter-tab ' + (currentFilter === 'all' ? 'active' : '')}
            @click=${() => onUpdateListFilter('all')}
          >
            ${t("providers.list.filter.all")} (${list.length})
          </button>
          <button
            type="button"
            class=${'filter-tab ' + (currentFilter === 'active' ? 'active' : '')}
            @click=${() => onUpdateListFilter('active')}
          >
            ${t("providers.list.filter.active")} (${activeCount})
          </button>
          <button
            type="button"
            class=${'filter-tab ' + (currentFilter === 'inactive' ? 'active' : '')}
            @click=${() => onUpdateListFilter('inactive')}
          >
            ${t("providers.list.filter.inactive")} (${inactiveCount})
          </button>
        </div>
      </div>
      <div class="actions">
        <button @click=${onRefreshAllProviders}>${icons.refresh()} ${t("providers.list.btn.refresh_all")}</button>
        <button class="primary" @click=${onShowCreateProvider}>${icons.plus()} ${t("providers.list.btn.add")}</button>
      </div>
    </div>
    ${cards}
  `;
}