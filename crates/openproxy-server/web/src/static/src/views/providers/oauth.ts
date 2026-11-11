// views/providers/oauth.ts — OAuth PKCE / device-code flow handlers, the
// connections (accounts) section, and their render templates.
//
// Accounts render as the approved "D" list: one dense row per account with a
// drag handle (animated reorder via account-drag.ts), click-to-rename with
// auto-save, a small pause toggle next to the name, the full health dropdown
// pinned to the right on wide containers, quota below, and every action
// visible — no hidden editor form.

import { html, type TemplateResult } from 'lit-html';
import { repeat } from 'lit-html/directives/repeat.js';
import { state } from '../../state/index.js';
import { api } from '../../state/api.js';
import { requestUpdate } from '../../state/reactive.js';
import { showToast } from '../../components/toast.js';
import { flashButton, showApiError } from '../../lib/ui-utils.js';
import { showConfirm } from '../../lib/show-confirm.js';
import { icons } from '../../lib/icons.js';
import { t } from '../../i18n/index.js';
import {
  showCreateAccount,
  showUpdateAccountKey,
  showReauthAccount,
  copyAccountApiKey,
} from '../../handlers/account-handlers.js';
import { renderQuotaCell } from '../quota-cell.js';
import { resolveAccountTier } from '../../lib/view-utils.js';
import { initAccountDrag } from './account-drag.js';
import type { Account, HealthStatus, Provider } from '../../lib/types/api.js';

async function onSetHealth(id: number, e: Event | null): Promise<void> {
  const target =
    e && e.target && e.target instanceof HTMLSelectElement ? e.target : null;
  const health = target ? (target.value as HealthStatus) : null;
  if (!health) return;
  try {
    await api('/accounts/' + id + '/health', {
      method: 'POST',
      body: JSON.stringify({ health }),
    });
    const a = (state.accounts || []).find((x) => x.id === id);
    if (a) a.health_status = health;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

async function onToggleAccountActive(
  id: number,
  currentActive: boolean,
): Promise<void> {
  const nextActive = !currentActive;
  try {
    await api('/accounts/' + id + '/active', {
      method: 'POST',
      body: JSON.stringify({ active: nextActive }),
    });
    const a = (state.accounts || []).find((x) => x.id === id);
    if (a) a.active = nextActive;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error toggling account active state');
  }
}

async function onRefreshAccountQuota(
  accountId: number,
  e: Event | null,
): Promise<void> {
  const target =
    e && e.target && e.target instanceof HTMLButtonElement ? e.target : null;
  const btn: HTMLButtonElement | null = target;
  const oldText = btn ? btn.textContent : null;
  if (btn) {
    btn.disabled = true;
    btn.textContent = '...';
  }
  try {
    const result = (await api('/accounts/' + accountId + '/refresh-quota', {
      method: 'POST',
    })) as
      | { supported?: boolean; error?: string; model_details?: Array<unknown> }
      | null;
    if (result && result.supported === false) {
      if (btn) flashButton(btn, 'n/a', '#9399b2');
    } else if (result && result.error) {
      if (btn) flashButton(btn, '✗ err', '#f38ba8');
    } else {
      if (btn) flashButton(btn, '✓', '#a6e3a1');
    }
    state.accounts = (await api('/accounts')) as typeof state.accounts;
    if (result && 'model_details' in result && result.model_details != null) {
      const match = state.accounts.find((a: { id: number }) => a.id === accountId);
      if (match) {
        match.quota_model_details =
          result.model_details as import('../../lib/types/api.js').ModelQuotaDetail[];
      }
    }
    requestUpdate();
  } catch (err: unknown) {
    if (btn) flashButton(btn, '✗', '#f38ba8');
    showApiError(err, 'Error');
  } finally {
    if (btn) {
      setTimeout(() => {
        btn.disabled = false;
        btn.textContent = oldText;
      }, 1500);
    }
  }
}

async function onRefreshAllQuotas(providerId: string): Promise<void> {
  const accounts = (state.accounts || []).filter(
    (a) => a.provider_id === providerId,
  );
  const supported = accounts.filter((a) => {
    const p = state.providers.find((pp) => pp.id === a.provider_id);
    return p?.metadata?.supports_quota === true;
  });
  if (supported.length === 0) {
    showToast(`No accounts with quota support for ${providerId}.`, 'info');
    return;
  }
  if (
    !(await showConfirm({
      title: 'Refresh quota',
      message: `Refresh quota for ${supported.length} accounts?`,
      confirmLabel: 'Refresh',
    }))
  )
    return;
  for (const a of supported) {
    try {
      await api('/accounts/' + a.id + '/refresh-quota', { method: 'POST' });
    } catch (err: unknown) {
      console.error('Failed to refresh quota for', a.id, err);
    }
  }
  state.accounts = (await api('/accounts')) as typeof state.accounts;
  requestUpdate();
  showToast('Quotas refreshed.', 'success');
}

function onShowCreateAccount(providerId: string): void {
  showCreateAccount(providerId);
}

function onShowUpdateAccountKey(id: number): void {
  showUpdateAccountKey(id);
}

function onShowReauthAccount(id: number): void {
  showReauthAccount(id);
}

async function onDeleteAccount(id: number): Promise<void> {
  try {
    await api('/accounts/' + id, { method: 'DELETE' });
    state.accounts = state.accounts.filter((a) => a.id !== id);
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

async function onApplyLocalCli(accountId: number): Promise<void> {
  try {
    const res = (await api(`/accounts/${accountId}/apply-local-cli`, {
      method: 'POST',
    })) as { success?: boolean; path?: string };
    if (res && res.success) {
      showToast(`Credentials applied locally to ${res.path}`, 'success');
    }
  } catch (err: unknown) {
    showApiError(err, 'Error applying credentials to local CLI');
  }
}

/**
 * Inline rename: clicking the account name swaps the button for an input.
 * Enter or blur saves automatically, Escape cancels, empty restores.
 */
function onBeginAccountRename(e: MouseEvent): void {
  const button = e.currentTarget as HTMLElement;
  const editor = button.closest('.account-name-editor');
  const input = editor?.querySelector<HTMLInputElement>('.account-name-input');
  if (!input || !editor) return;
  button.hidden = true;
  input.hidden = false;
  input.focus();
  input.select();
}

function finishAccountRename(
  input: HTMLInputElement,
  accountId: number,
  currentName: string,
  cancel: boolean,
): void {
  if (input.hidden) return;
  const editor = input.closest('.account-name-editor');
  const button = editor?.querySelector<HTMLButtonElement>(
    '.account-name-button',
  );
  if (button) button.hidden = false;
  input.hidden = true;
  const next = input.value.trim();
  input.value = currentName;
  if (cancel) return;
  if (!next) {
    showToast(t('providers.detail.entry.label_empty'), 'error');
    return;
  }
  if (next !== currentName) {
    void saveAccountLabel(accountId, next);
  }
}

function onAccountRenameKeydown(
  accountId: number,
  currentName: string,
  e: KeyboardEvent,
): void {
  if (e.key !== 'Enter' && e.key !== 'Escape') return;
  e.preventDefault();
  const input = e.currentTarget as HTMLInputElement;
  finishAccountRename(input, accountId, currentName, e.key === 'Escape');
  const editor = input.closest('.account-name-editor');
  editor
    ?.querySelector<HTMLButtonElement>('.account-name-button')
    ?.focus();
}

function onAccountRenameBlur(
  accountId: number,
  currentName: string,
  e: FocusEvent,
): void {
  finishAccountRename(
    e.target as HTMLInputElement,
    accountId,
    currentName,
    false,
  );
}

async function saveAccountLabel(id: number, label: string): Promise<void> {
  try {
    await api('/accounts/' + id + '/label', {
      method: 'PATCH',
      body: JSON.stringify({ label }),
    });
    const a = (state.accounts || []).find((x) => x.id === id);
    if (a) a.label = label;
    requestUpdate();
    showToast(t('providers.detail.entry.label_updated'), 'success');
  } catch (err: unknown) {
    showApiError(err, 'Error updating label');
  }
}

export function renderOAuthSection(_provider: Provider): TemplateResult {
  return html``;
}

export function renderConnectionsSection(
  provider: Provider,
  accounts: Account[],
): TemplateResult {
  const hasQuota = provider.metadata?.supports_quota ?? false;
  const body: TemplateResult =
    accounts.length === 0
      ? html`<div class="empty-text">No accounts. Add an API key to start using this provider.</div>`
      : html`<div class="accounts-list" role="list">
          ${repeat(
            accounts,
            (a) => a.id,
            (a, aIdx) => {
              const isOAuth =
                a.auth_type === 'oauth' || provider.auth_type === 'oauth';
              const paused = a.active === false;
              const health = a.health_status || 'unknown';
              const displayName = a.label || a.email || '—';
              const tier = resolveAccountTier(a, provider);
              const authLabel = isOAuth
                ? t('providers.detail.entry.auth_oauth')
                : t('providers.detail.entry.auth_api_key');
              const pauseLabel = paused
                ? t('providers.detail.entry.resume')
                : t('providers.detail.entry.pause');
              return html`<article
                class=${'provider-entry account-card-row' +
                (paused ? ' account-row-paused' : '')}
                role="listitem"
                data-drag-id=${String(a.id)}
                data-provider=${provider.id}>
                <div class="account-heading">
                  <button
                    type="button"
                    class="account-drag-handle"
                    aria-label=${t('providers.detail.entry.drag_label', {
                      name: displayName,
                    })}
                    title=${t('providers.detail.entry.drag_hint')}>
                    ${icons.dragHandle()}
                  </button>
                  <div class="account-identity">
                    <div class="account-name-row">
                      <span class="account-name-editor">
                        <button
                          type="button"
                          class="account-name-button"
                          title=${t('providers.detail.entry.rename_hint')}
                          aria-label=${t(
                            'providers.detail.entry.rename_label',
                            { name: displayName },
                          )}
                          @click=${(e: MouseEvent) =>
                            onBeginAccountRename(e)}>
                          <span class="account-name-text" title=${displayName}>${displayName}</span>
                          ${icons.pencil()}
                        </button>
                        <input
                          class="account-name-input"
                          type="text"
                          maxlength="120"
                          hidden
                          aria-label=${t(
                            'providers.detail.entry.name_label',
                          )}
                          .value=${displayName}
                          @keydown=${(e: KeyboardEvent) =>
                            onAccountRenameKeydown(a.id, displayName, e)}
                          @blur=${(e: FocusEvent) =>
                            onAccountRenameBlur(a.id, displayName, e)} />
                      </span>
                      <button
                        type="button"
                        class="account-pause"
                        aria-pressed=${String(paused)}
                        aria-label=${pauseLabel}
                        title=${pauseLabel}
                        @click=${() =>
                          void onToggleAccountActive(a.id, !paused)}>
                        ${paused ? icons.play() : icons.pause()}
                      </button>
                    </div>
                    <div class="provider-entry-meta">
                      ${tier
                        ? html`<span class="account-plan">${tier}</span>`
                        : html``}
                      <span>${authLabel}</span>
                      <span class="account-priority">${t(
                        'providers.detail.entry.priority',
                        { index: aIdx + 1 },
                      )}</span>
                      <span title=${a.created_at || ''}>${a.created_at?.slice(
                        0,
                        10,
                      ) || '—'}</span>
                    </div>
                  </div>
                  <select
                    class="account-health-select"
                    data-health=${health}
                    aria-label=${t('providers.detail.entry.health')}
                    @change=${(e: Event) => onSetHealth(a.id, e)}>
                    <option value="healthy" ?selected=${health === 'healthy'}>
                      ${t('providers.detail.entry.health_healthy')}
                    </option>
                    <option value="degraded" ?selected=${health === 'degraded'}>
                      ${t('providers.detail.entry.health_degraded')}
                    </option>
                    <option
                      value="unhealthy"
                      ?selected=${health === 'unhealthy'}>
                      ${t('providers.detail.entry.health_unhealthy')}
                    </option>
                  </select>
                </div>

                ${hasQuota
                  ? html`<div class="provider-entry-quota">
                      ${renderQuotaCell(a)}
                    </div>`
                  : html``}

                <div class="provider-entry-actions">
                  ${hasQuota
                    ? html`<button
                        class="small"
                        @click=${(e: Event) =>
                          onRefreshAccountQuota(a.id, e)}>
                        ${icons.refresh()} Quota
                      </button>`
                    : html``}
                  ${provider.id === 'antigravity' ||
                  provider.id === 'claude-code' ||
                  provider.id === 'claude'
                    ? html`<button
                        class="small"
                        @click=${() => onApplyLocalCli(a.id)}>
                        ${icons.desktop()} Apply Local
                      </button>`
                    : html``}
                  ${isOAuth
                    ? html`<button
                        class="small"
                        title="Re-authenticate OAuth account"
                        @click=${() => onShowReauthAccount(a.id)}>
                        ${icons.refresh()} Reauth
                      </button>`
                    : html`<button
                        class="small"
                        title="Copy API Key"
                        @click=${() => copyAccountApiKey(a.id)}>
                        ${icons.copy()} Copy
                      </button>
                      <button
                        class="small"
                        title="Update API key"
                        @click=${() => onShowUpdateAccountKey(a.id)}>
                        ${icons.key()} Key
                      </button>`}
                  <button class="small danger" @click=${() => onDeleteAccount(a.id)}>
                    Delete
                  </button>
                </div>
              </article>`;
            },
          )}
        </div>`;
  const toolbar: TemplateResult = html`<div>
      ${hasQuota
        ? html`<button @click=${() => onRefreshAllQuotas(provider.id)}>
            ${icons.refresh()} Refresh all quotas
          </button>`
        : html``}
      <button class="primary" @click=${() => onShowCreateAccount(provider.id)}>
        ${icons.plus()} Add account
      </button>
    </div>`;
  return html`<section class="detail-section">
    <div class="section-header"><h3>Connections (${accounts.length})</h3>${toolbar}</div>
    ${body}
  </section>`;
}

if (typeof document !== 'undefined') {
  initAccountDrag();
}
