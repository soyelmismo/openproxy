// views/providers/detail.ts — provider detail view orchestrator.
//
// Split out of the former detail.ts monolith (FU1). This file now
// owns only the top-level `renderProviderDetail()` export and the
// detail-header sub-section (rename, edit endpoint, sync models,
// toggle active, delete). Every other section is delegated:
//
//   OAuth / connections → ./oauth.ts
//   Models section      → ./models.ts
//   Per-row models      → ./models-row.ts
//   Bulk actions        → ./models-bulk.ts
//   Custom model form   → ./custom-model.ts
//   UI-state accessors  → ./shared.ts
//
// `renderProviderDetail` remains the sole public entry point and the
// symbol imported by index.ts — no external contract changes.

import { html, type TemplateResult } from 'lit-html';
import { state } from '../../state/index.js';
import { api } from '../../state/api.js';
import { requestUpdate } from '../../state/reactive.js';
import { showToast } from '../../components/toast.js';
import { showApiError } from '../../lib/ui-utils.js';
import { showConfirm, showPrompt } from '../../lib/show-confirm.js';
import { icons } from '../../lib/icons.js';
import {
  editProviderEndpointPrompt,
  editProviderHeadersPrompt,
} from '../../handlers/provider-handlers.js';
import type { Provider } from '../../lib/types/api.js';
import {
  detailProviderId,
  loadError,
  renderProviderIcon,
  getProviderUi,
  setProviderUi,
  type ProviderDetailUiState,
} from './shared.js';
import { renderOAuthSection, renderConnectionsSection } from './oauth.js';
import { renderModelsSection } from './models.js';

// ================================
//  Detail header handlers
// ================================

async function onRenameProvider(
  providerId: string,
  currentName: string,
): Promise<void> {
  const newName = await showPrompt(
    `Rename provider "${providerId}"`,
    'New provider name:',
    currentName,
  );
  if (newName == null) return;
  const trimmed = newName.trim();
  if (trimmed === '') {
    showToast('Name cannot be empty', 'error');
    return;
  }
  if (trimmed === currentName) return;
  const collision = state.providers.find(
    (p) => p.id !== providerId && p.name === trimmed,
  );
  if (collision) {
    if (
      !(await showConfirm({
        title: 'Name collision',
        message: `A provider with this name already exists (${collision.id}). Use this name anyway?`,
        confirmLabel: 'Use anyway',
      }))
    )
      return;
  }
  try {
    await api('/providers/' + encodeURIComponent(providerId), {
      method: 'PATCH',
      body: JSON.stringify({ name: trimmed }),
    });
    state.providers = (await api('/providers')) as typeof state.providers;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

async function onEditBaseUrl(
  providerId: string,
  currentBaseUrl: string,
): Promise<void> {
  await editProviderEndpointPrompt(providerId, currentBaseUrl);
  requestUpdate();
}

async function onEditHeaders(
  providerId: string,
  currentHeadersJson: string | null | undefined,
): Promise<void> {
  await editProviderHeadersPrompt(providerId, currentHeadersJson);
  requestUpdate();
}

async function onRefreshProvider(
  providerId: string,
  e: Event | null,
): Promise<void> {
  const target =
    e && e.target && e.target instanceof HTMLButtonElement ? e.target : null;
  const btn: HTMLButtonElement | null = target;
  const original = btn ? btn.textContent : null;
  if (btn) {
    btn.disabled = true;
    btn.textContent = 'Refreshing...';
  }
  try {
    const result = (await api(
      '/providers/' + encodeURIComponent(providerId) + '/refresh',
      { method: 'POST' },
    )) as { models_refreshed?: number; new_model_ids?: string[] } | null;
    const n: number =
      result && typeof result.models_refreshed === 'number'
        ? result.models_refreshed
        : 0;
    const newIds: string[] =
      result && Array.isArray(result.new_model_ids) ? result.new_model_ids : [];
    const summary: string =
      n === 0
        ? `Nothing to refresh for ${providerId}.`
        : `Refreshed ${n} models for ${providerId}.`;
    const newSuffix: string =
      newIds.length === 0
        ? ''
        : newIds.length <= 3
          ? ` New: ${newIds.join(', ')}.`
          : ` New: ${newIds.slice(0, 3).join(', ')} (+${newIds.length - 3} more).`;
    showToast(summary + newSuffix, 'success');
    state.providers = (await api('/providers')) as typeof state.providers;
    state.models = (await api(
      '/models?provider_id=' + encodeURIComponent(providerId),
    )) as typeof state.models;
    state.modelsComplete = false;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  } finally {
    if (btn) {
      btn.disabled = false;
      btn.textContent = original;
    }
  }
}

async function onToggleProviderActive(
  providerId: string,
  newActive: boolean,
): Promise<void> {
  if (!newActive) {
    const ok = await showConfirm({
      title: 'Deactivate provider',
      message:
        `Deactivate provider "${providerId}"?\n\n` +
        `Its accounts and models will be preserved, but it won't be ` +
        `usable in combos until you reactivate it.`,
      danger: true,
      confirmLabel: 'Deactivate',
    });
    if (!ok) return;
  }
  try {
    await api('/providers/' + encodeURIComponent(providerId) + '/active', {
      method: 'POST',
      body: JSON.stringify({ active: newActive }),
    });
    state.providers = (await api('/providers')) as typeof state.providers;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

async function onConfirmDeleteProvider(providerId: string): Promise<void> {
  const typed = await showPrompt(
    'Delete provider',
    `Type the provider ID to confirm deletion: ${providerId}`,
  );
  if (typed !== providerId) {
    if (typed != null)
      showToast(
        `Provider id "${typed}" does not match. Nothing was deleted.`,
        'error',
      );
    return;
  }
  if (
    !(await showConfirm({
      title: 'Really delete?',
      message: `Really delete ${providerId}? This cascades to all its accounts and models.`,
      danger: true,
      confirmLabel: 'Delete',
    }))
  )
    return;
  try {
    await api('/providers/' + encodeURIComponent(providerId), {
      method: 'DELETE',
    });
    state.providers = state.providers.filter((p) => p.id !== providerId);
    state.models = state.models.filter((m) => m.provider_id !== providerId);
    state.accounts = state.accounts.filter((a) => a.provider_id !== providerId);
    location.hash = '#/providers';
  } catch (err: unknown) {
    showApiError(err, 'Cannot delete');
  }
}

// ================================
//  Detail header render
// ================================

function renderDetailHeader(provider: Provider): TemplateResult {
  const isDeletable = provider.metadata?.deletable ?? true;
  return html`
    <div class="provider-detail-header${provider.active ? '' : ' inactive'}">
      <div class="provider-detail-main">
        <div class="provider-icon icon-large" data-format=${provider.format}>${renderProviderIcon(provider)}</div>
        <div class="provider-detail-info">
          <div class="provider-detail-title-row">
            <h2>
              <span class="editable" title="Click to rename" @click=${() => onRenameProvider(provider.id, provider.name)}>${provider.name}</span>
              <small class="editable-pencil">${icons.pencil()}</small>
            </h2>
            <code class="provider-detail-id">${provider.id}</code>
            ${provider.active ? html`` : html`<span class="chip inactive-chip">inactive</span>`}
          </div>
          <div class="meta provider-detail-meta">
            <span class="chip format-chip" data-format=${provider.format}>${provider.format}</span>
            <span class="chip auth-chip">${provider.auth_type}</span>
            <span class="editable meta-link" title="Click to edit endpoint (base URL)" @click=${() => onEditBaseUrl(provider.id, provider.base_url)}>${provider.base_url}</span>
            <small class="editable" title="Click to edit endpoint (base URL)" style="cursor: pointer;" @click=${() => onEditBaseUrl(provider.id, provider.base_url)}>${icons.pencil()}</small>
            ${provider.extra_headers_json
              ? html`<span class="chip headers-chip" title=${provider.extra_headers_json} style="cursor: pointer;" @click=${() => onEditHeaders(provider.id, provider.extra_headers_json)}>headers (${Object.keys(JSON.parse(provider.extra_headers_json || '{}')).length})</span>`
              : html``}
          </div>
        </div>
      </div>
      <div class="actions provider-detail-actions">
        <button @click=${() => onEditBaseUrl(provider.id, provider.base_url)}>${icons.pencil()} Endpoint</button>
        <button @click=${() => onEditHeaders(provider.id, provider.extra_headers_json)}>${icons.pencil()} Headers</button>
        <button @click=${(e: Event) => onRefreshProvider(provider.id, e)}>${icons.refresh()} Sync Models</button>
        <button class="primary" @click=${() => onToggleProviderActive(provider.id, !provider.active)}>
          ${provider.active ? 'Deactivate' : 'Activate'}
        </button>
        ${!isDeletable
          ? html`<button class="locked" disabled title="Built-in providers cannot be deleted. Deactivate them instead.">${icons.key()} Delete</button>`
          : html`<button class="danger" @click=${() => onConfirmDeleteProvider(provider.id)}>Delete</button>`}
      </div>
    </div>
  `;
}

// ================================
//  Top-level detail template
// ================================

export function renderProviderDetail(): TemplateResult {
  if (loadError) {
    return html`<div class="banner banner-error">${loadError}</div>`;
  }
  if (!detailProviderId) return html`<div class="loading">Loading...</div>`;
  const provider = (state.providers || []).find(
    (p) => p.id === detailProviderId,
  );
  if (!provider) {
    if ((state.providers || []).length === 0) {
      return html`<div class="loading">Loading provider...</div>`;
    }
    return html`<div class="banner banner-error">Provider ${detailProviderId} not found. <a href="#/providers">← Back</a></div>`;
  }
  const accounts = (state.accounts || []).filter(
    (a) => a.provider_id === detailProviderId,
  );
  const providerModels = (state.models || []).filter(
    (m) => m.provider_id === detailProviderId,
  );
  if (!state.providerDetail[detailProviderId]) {
    setProviderUi(detailProviderId, {
      filter: 'all',
      search: '',
      sort: null,
      page: 1,
      pageSize: 50,
    });
  } else if (
    (state.providerDetail[detailProviderId] as Partial<ProviderDetailUiState>)
      .sort === undefined
  ) {
    const existing = state.providerDetail[detailProviderId] as Partial<ProviderDetailUiState>;
    setProviderUi(detailProviderId, {
      filter: existing.filter ?? 'all',
      search: existing.search ?? '',
      sort: null,
      page: existing.page ?? 1,
      pageSize: existing.pageSize ?? 50,
    });
  }
  const ui = getProviderUi(detailProviderId);
  return html`
    <div class="page-header"><a href="#/providers" class="back-link">← All providers</a><h2>${provider.name}</h2></div>
    ${renderDetailHeader(provider)}
    ${renderOAuthSection(provider)}
    ${renderConnectionsSection(provider, accounts)}
    ${renderModelsSection(provider, providerModels, ui)}
  `;
}
