// views/providers/detail.ts — `renderProviderDetail()`, the provider detail view.
// Owns the detail header (rename, endpoint, sync, toggle, delete); OAuth and
// connections live in oauth.ts, models in models.ts / models-row.ts /
// models-bulk.ts / custom-model.ts, UI state in shared.ts.

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
} from '../../handlers/provider-handlers/index.js';
import type { Provider } from '../../lib/types/api.js';
import { FALLBACK_PROVIDER_FORMATS } from '../../lib/types/common.js';
import {
  detailProviderId,
  loadError,
  renderProviderIcon,
  getProviderUi,
  setProviderUi,
  type ProviderDetailUiState,
} from './shared.js';
import { renderConnectionsSection } from './oauth.js';
import { renderModelsSection } from './models.js';

let openTtlMenuProviderId: string | null = null;
let openTtlMenuPos: { top: number; left: number } | null = null;

if (typeof window !== 'undefined') {
  window.addEventListener('click', () => {
    if (openTtlMenuProviderId !== null) {
      openTtlMenuProviderId = null;
      openTtlMenuPos = null;
      requestUpdate();
    }
  });
}


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

async function onChangeProviderFormat(
  providerId: string,
  e: Event,
): Promise<void> {
  const target = e.target instanceof HTMLSelectElement ? e.target : null;
  if (!target) return;
  const newFormat = target.value;
  try {
    await api('/providers/' + encodeURIComponent(providerId), {
      method: 'PATCH',
      body: JSON.stringify({ format: newFormat }),
    });
    showToast(`Provider format updated to ${newFormat}`, 'success');
    state.providers = (await api('/providers')) as typeof state.providers;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error updating provider format');
  }
}

async function onCycleStreamMode(
  providerId: string,
  currentMode: string | undefined,
): Promise<void> {
  const modes: Array<'auto' | 'unary' | 'streaming'> = ['auto', 'unary', 'streaming'];
  const cur = currentMode || 'auto';
  const idx = modes.indexOf(cur as 'auto' | 'unary' | 'streaming');
  const nextMode = modes[(idx + 1) % modes.length];
  try {
    await api('/providers/' + encodeURIComponent(providerId), {
      method: 'PATCH',
      body: JSON.stringify({ stream_mode: nextMode }),
    });
    const label = nextMode === 'unary'
      ? 'unary (forced non-streaming)'
      : nextMode === 'streaming'
        ? 'streaming (forced SSE)'
        : 'auto';
    showToast(`Stream mode set to ${label}`, 'success');
    state.providers = (await api('/providers')) as typeof state.providers;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error updating stream mode');
  }
}

function getProviderTtlOptions(format: string, providerId: string): Array<{ value: string; label: string }> {
  // Provider-specific allowed prompt cache TTL options based on official API documentation
  const isAnthropicFamily = format === 'anthropic' || providerId === 'claude-code';
  const isResponsesFamily = format === 'responses' || providerId === 'codex';

  if (isAnthropicFamily) {
    return [
      { value: 'auto', label: 'auto (default / 5m)' },
      { value: '5m', label: '5m (ephemeral standard)' },
      { value: '1h', label: '1h (extended cache beta)' },
    ];
  }

  if (isResponsesFamily) {
    return [
      { value: 'auto', label: 'auto (default / 5-10m)' },
      { value: '12h', label: '12h (Responses API extended)' },
    ];
  }

  // Other providers (e.g. Gemini, Antigravity, OpenCode, OpenAI standard)
  return [
    { value: 'auto', label: 'auto (default)' },
  ];
}

async function onChangePromptCacheTtl(
  providerId: string,
  newTtl: string,
): Promise<void> {
  try {
    await api('/providers/' + encodeURIComponent(providerId), {
      method: 'PATCH',
      body: JSON.stringify({ prompt_cache_ttl: newTtl }),
    });
    showToast(`Prompt cache TTL set to ${newTtl}`, 'success');
    state.providers = (await api('/providers')) as typeof state.providers;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error updating prompt cache TTL');
  }
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
    if (n === 0) {
      showToast(`Nothing to refresh for ${providerId}.`, 'info');
    }
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


export function getHeadersChipLabel(raw?: string | null): string {
  if (!raw || !raw.trim()) return '';
  if (raw.trim() === '***redacted***') return 'headers (configured)';
  try {
    const parsed = JSON.parse(raw);
    if (typeof parsed === 'object' && parsed !== null && !Array.isArray(parsed)) {
      return `headers (${Object.keys(parsed).length})`;
    }
  } catch {
    // fallback for non-JSON or masked strings
  }
  return 'headers (configured)';
}

export function getHeadersChipTitle(raw?: string | null): string {
  if (!raw || !raw.trim()) return '';
  if (raw.trim() === '***redacted***') return 'Extra headers configured (redacted for security)';
  return raw;
}

export function renderDetailHeader(provider: Provider): TemplateResult {
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
            <select
              class="provider-format-select"
              title="Provider schema / protocol (OpenAI Chat Completions, Responses, Anthropic Messages, Gemini, etc.)"
              .value=${provider.format}
              @change=${(e: Event) => onChangeProviderFormat(provider.id, e)}
            >
              ${(state.formats?.provider_formats ?? FALLBACK_PROVIDER_FORMATS).map(
                (fmt) => html`<option value=${fmt.id} title=${fmt.description} ?selected=${provider.format === fmt.id}>${fmt.id}</option>`
              )}
            </select>
            <span class="chip auth-chip">${provider.auth_type}</span>
            <span class="editable meta-link" title="Click to edit endpoint (base URL)" @click=${() => onEditBaseUrl(provider.id, provider.base_url)}>${provider.base_url}</span>
            <small class="editable" title="Click to edit endpoint (base URL)" style="cursor: pointer;" @click=${() => onEditBaseUrl(provider.id, provider.base_url)}>${icons.pencil()}</small>
            <span
              class=${'chip stream-chip' + (provider.stream_mode === 'unary' ? ' warn' : provider.stream_mode === 'streaming' ? ' combo-chip' : '')}
              title="Stream mode: auto, unary (forced non-streaming), streaming (forced SSE). Click to cycle."
              style="cursor: pointer;"
              @click=${() => onCycleStreamMode(provider.id, provider.stream_mode)}
            >
              stream: ${provider.stream_mode || 'auto'}
            </span>
            <span
              class=${'chip ttl-chip' + (provider.prompt_cache_ttl && provider.prompt_cache_ttl !== 'auto' ? ' combo-chip' : '')}
              title="Prompt cache TTL: click to change"
              style="display: inline-flex; align-items: center; gap: 4px; padding: 2px 8px; cursor: pointer; position: relative;"
              @click=${(e: MouseEvent) => {
                e.stopPropagation();
                if (openTtlMenuProviderId === provider.id) {
                  openTtlMenuProviderId = null;
                  openTtlMenuPos = null;
                } else {
                  openTtlMenuProviderId = provider.id;
                  const el = e.currentTarget as HTMLElement;
                  const rect = el.getBoundingClientRect();
                  const menuWidth = 200;
                  const menuMaxHeight = 160;
                  const margin = 8;

                  let left = rect.left;
                  if (left + menuWidth > window.innerWidth - margin) {
                    left = Math.max(margin, window.innerWidth - menuWidth - margin);
                  }

                  let top = rect.bottom + 4;
                  if (top + menuMaxHeight > window.innerHeight - margin) {
                    const spaceAbove = rect.top - margin;
                    if (spaceAbove > 100) {
                      top = Math.max(margin, rect.top - menuMaxHeight - 4);
                    }
                  }
                  openTtlMenuPos = { top, left };
                }
                requestUpdate();
              }}
            >
              cache: ${provider.prompt_cache_ttl || 'auto'}
              <small style="opacity: 0.7; font-size: 0.7em;">▾</small>
            </span>
            ${openTtlMenuProviderId === provider.id && openTtlMenuPos
              ? html`
                <div
                  class="target-accounts-backdrop"
                  @click=${(e: Event) => {
                    e.stopPropagation();
                    openTtlMenuProviderId = null;
                    openTtlMenuPos = null;
                    requestUpdate();
                  }}
                ></div>
                <div
                  class="target-accounts-menu"
                  style="top:${openTtlMenuPos.top}px; left:${openTtlMenuPos.left}px; width: 220px;"
                  @click=${(e: Event) => e.stopPropagation()}
                >
                  ${getProviderTtlOptions(provider.format, provider.id).map(
                    (opt) => html`
                      <div
                        class="target-accounts-item"
                        style="padding: 6px 12px; cursor: pointer; display: flex; align-items: center; justify-content: space-between; font-weight: ${(provider.prompt_cache_ttl || 'auto') === opt.value ? '600' : 'normal'}; background: ${(provider.prompt_cache_ttl || 'auto') === opt.value ? 'var(--color-surface-hover, rgba(255,255,255,0.06))' : 'transparent'};"
                        @click=${() => {
                          openTtlMenuProviderId = null;
                          openTtlMenuPos = null;
                          void onChangePromptCacheTtl(provider.id, opt.value);
                        }}
                      >
                        <span>${opt.label}</span>
                        ${(provider.prompt_cache_ttl || 'auto') === opt.value ? html`<span style="color: var(--color-primary, #38bdf8)">✓</span>` : html``}
                      </div>
                    `
                  )}
                </div>
              `
              : html``}
            ${provider.extra_headers_json
              ? html`<span class="chip headers-chip" title=${getHeadersChipTitle(provider.extra_headers_json)} style="cursor: pointer;" @click=${() => onEditHeaders(provider.id, provider.extra_headers_json)}>${getHeadersChipLabel(provider.extra_headers_json)}</span>`
              : html``}
          </div>
        </div>
      </div>
      <div class="actions provider-detail-actions">
        <button @click=${() => onCycleStreamMode(provider.id, provider.stream_mode)} title="Cycle stream mode: auto -> unary -> streaming">
          ${icons.lightning()} Stream: ${provider.stream_mode || 'auto'}
        </button>
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
    ${renderConnectionsSection(provider, accounts)}
    ${renderModelsSection(provider, providerModels, ui)}
  `;
}
