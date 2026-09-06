// views/providers/list.ts — providers grid view (the index
// `mountProviders({detailId: undefined})` path).
//
// Renders a card grid for every provider in `state.providers`,
// with a header (Refresh all + Add provider), an empty state,
// and an error banner when the fetch failed. The cards link to
// `#/providers/:id`, which mounts the detail view.

import { html, type TemplateResult } from 'lit-html';
import { state } from '../../state/index.js';
import { api } from '../../state/api.js';
import { requestUpdate } from '../../state/reactive.js';
import { showApiError } from '../../lib/ui-utils.js';
import { icons } from '../../lib/icons.js';
import { showCreateProvider } from '../../handlers/provider-handlers.js';
import { t } from '../../i18n/index.js';
import type { Account, Provider } from '../../lib/types/api.js';
import { loadError, renderProviderIcon } from './shared.js';

// ---- Handlers: grid ----

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

// ---- Templates ----

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
  const cards: TemplateResult =
    list.length === 0
      ? html`<div class="empty-state">
          <h3>${t("providers.list.empty.title")}</h3>
          <p>${t("providers.list.empty.subtitle")}</p>
          <button class="primary" @click=${onShowCreateProvider}>${icons.plus()} ${t("providers.list.btn.add")}</button>
        </div>`
      : html`<div class="provider-grid">${list.map((p) => {
          const accounts = (state.accounts || []).filter((a) => a.provider_id === p.id);
          return renderProviderCard(p, accounts);
        })}</div>`;
  return html`
    <div class="page-header"><h2>${t("providers.list.heading")}</h2>
      <div class="actions">
        <button @click=${onRefreshAllProviders}>${icons.refresh()} ${t("providers.list.btn.refresh_all")}</button>
        <button class="primary" @click=${onShowCreateProvider}>${icons.plus()} ${t("providers.list.btn.add")}</button>
      </div>
    </div>
    ${cards}
  `;
}