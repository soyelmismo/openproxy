// views/providers/oauth.ts — OAuth PKCE / Device-Code flow handlers,
// connections (accounts) section handlers and their render templates.
//
// Split out of the former detail.ts monolith (FU1).
//
// Functions moved:
//   onOAuthStartPKCE, onOAuthStartDeviceCode, onOAuthSubmitManualCallback,
//   onCopyAuthUrl, onShowCreateAccount, onShowUpdateAccountKey,
//   onSetHealth, onRefreshAccountQuota, onRefreshAllQuotas,
//   onDeleteAccount, onApplyLocalCli,
//   renderOAuthSection, renderConnectionsSection.

import { html, type TemplateResult } from 'lit-html';
import { state } from '../../state/index.js';
import { api } from '../../state/api.js';
import { requestUpdate } from '../../state/reactive.js';
import { showToast } from '../../components/toast.js';
import { flashButton, showApiError } from '../../lib/ui-utils.js';
import { showConfirm } from '../../lib/show-confirm.js';
import { icons } from '../../lib/icons.js';
import {
  showCreateAccount,
  showUpdateAccountKey,
  updateAccountLabel,
  copyAccountApiKey,
} from '../../handlers/account-handlers.js';
import { renderQuotaCell } from '../quota-cell.js';
import type { Account, HealthStatus, Provider } from '../../lib/types/api.js';

// ==========================================
//  Connection / account handlers
// ==========================================

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

// ================================
//  Render: OAuth section
// ================================

export function renderOAuthSection(_provider: Provider): TemplateResult {
  return html``;
}

// ================================
//  Render: Connections section
// ================================

export function renderConnectionsSection(
  provider: Provider,
  accounts: Account[],
): TemplateResult {
  const hasQuota = provider.metadata?.supports_quota ?? false;
  const body: TemplateResult =
    accounts.length === 0
      ? html`<div class="table-wrap"><table class="accounts-table responsive-card-table"><tbody><tr><td colspan="6" class="empty-row">No accounts. Add an API key to start using this provider.</td></tr></tbody></table></div>`
      : html`<div class="table-wrap"><table class="accounts-table responsive-card-table">
          <thead><tr><th>Label</th><th>Priority</th><th>Health</th><th>Quota</th><th>Created</th><th>Actions</th></tr></thead>
          <tbody>${accounts.map((a) => {
            const quotaCell: TemplateResult = hasQuota
              ? html`<td class="col-account-quota" data-label="Quota">${renderQuotaCell(a)}</td>`
              : html`<td class="col-account-quota" data-label="Quota"><div class="quota-cell muted"><small>not supported by this provider</small></div></td>`;
            return html`<tr class="account-card-row">
              <td class="col-account-label" data-label="Account">
                <div class="account-title-row">
                  <span class="editable account-name" title="Click to rename label" @click=${() => updateAccountLabel(a.id, a.label || a.email || '')}>
                    ${a.label || a.email || '—'}
                  </span>
                  <small class="account-edit-icon" title="Rename label">${icons.pencil()}</small>
                </div>
              </td>
              <td class="col-account-priority" data-label="Priority">
                <span class="account-priority-badge">Priority ${a.priority}</span>
              </td>
              <td class="col-account-health" data-label="Health">
                <select class=${'health-select ' + (a.health_status || 'unknown')} @change=${(e: Event) => onSetHealth(a.id, e)}>
                  <option value="healthy" ?selected=${a.health_status === 'healthy'}>healthy</option>
                  <option value="degraded" ?selected=${a.health_status === 'degraded'}>degraded</option>
                  <option value="unhealthy" ?selected=${a.health_status === 'unhealthy'}>unhealthy</option>
                </select>
              </td>
              ${quotaCell}
              <td class="col-account-created" data-label="Created">
                <span class="account-created-text">${a.created_at || '—'}</span>
              </td>
              <td class="col-account-actions" data-label="Actions">
                <div class="account-actions-wrap">
                  ${hasQuota ? html`<button class="small" @click=${(e: Event) => onRefreshAccountQuota(a.id, e)}>${icons.refresh()} Quota</button>` : html``}
                  ${provider.id === 'antigravity' ? html`<button class="small" @click=${() => onApplyLocalCli(a.id)}>${icons.desktop()} Apply Local</button>` : html``}
                  <button class="small" title="Copy API Key" @click=${() => copyAccountApiKey(a.id)}>${icons.copy()} Copy</button>
                  <button class="small" @click=${() => onShowUpdateAccountKey(a.id)}>${icons.key()} Key</button>
                  <button class="small danger" @click=${() => onDeleteAccount(a.id)}>Delete</button>
                </div>
              </td>
            </tr>`;
          })}</tbody>
        </table></div>`;
  const toolbar: TemplateResult = html`<div>
      ${hasQuota ? html`<button @click=${() => onRefreshAllQuotas(provider.id)}>${icons.refresh()} Refresh all quotas</button>` : html``}
      <button class="primary" @click=${() => onShowCreateAccount(provider.id)}>${icons.plus()} Add account</button>
    </div>`;
  return html`<section class="detail-section">
    <div class="section-header"><h3>Connections (${accounts.length})</h3>${toolbar}</div>
    ${body}
  </section>`;
}
