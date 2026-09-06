// views/providers/detail.ts — provider detail view (the
// `mountProviders({detailId})` path).
//
// Renders a full detail page for a single provider:
//   - Header (icon, name, base_url, actions)
//   - OAuth PKCE / Device Code section (when applicable)
//   - Connections table (accounts, quota, health)
//   - Models table (filter, sort, search, pagination, bulk actions)
//   - Mobile card rows (responsive variant of the models table)
//
// All event handlers live in this module (no external handlers/ import)
// because they mutate `state` in place and call `requestUpdate()` to
// trigger lit-html's diffing — the same pattern as the monolithic
// `providers.ts` before this split.

import { html, type TemplateResult } from 'lit-html';
import { state } from '../../state/index.js';
import { api } from '../../state/api.js';
import { requestUpdate } from '../../state/reactive.js';
import { showToast } from '../../components/toast.js';
import { flashButton, showApiError } from '../../lib/ui-utils.js';
import { copyToClipboard } from '../../lib/clipboard.js';
import { showConfirm, showPrompt } from '../../lib/show-confirm.js';
import { icons } from '../../lib/icons.js';
import {
  editProviderEndpointPrompt,
  editProviderHeadersPrompt,
} from '../../handlers/provider-handlers.js';
import {
  showCreateAccount,
  showUpdateAccountKey,
  updateAccountLabel,
  copyAccountApiKey,
} from '../../handlers/account-handlers.js';
import { showCustomModelForm } from '../../components/model-custom-form.js';
import { OAuthLogin } from '../../handlers/oauth-handlers.js';
import { renderQuotaCell } from '../quota-cell.js';
import { statusPillClass } from '../../lib/constants.js';
import { formatContextBadge } from '../../lib/format.js';
import {
  applySort,
  SORTABLE_COLUMNS,
  type ModelSort,
  type SortableColumn,
} from '../../components/model-table.js';
import type { Account, Model, Provider, HealthStatus } from '../../lib/types/api.js';
import {
  detailProviderId,
  loadError,
  renderProviderIcon,
  type ProviderDetailUiState,
} from './shared.js';

// ---- UI-state helpers (per-provider filter/search/sort/page) ----

function getProviderUi(providerId: string): ProviderDetailUiState {
  const raw = state.providerDetail[providerId] as
    | Partial<ProviderDetailUiState>
    | undefined;
  return {
    filter: raw?.filter ?? 'all',
    search: raw?.search ?? '',
    sort: raw?.sort ?? null,
    page: raw?.page ?? 1,
    pageSize: raw?.pageSize ?? 50,
  };
}

function setProviderUi(providerId: string, ui: ProviderDetailUiState): void {
  state.providerDetail[providerId] = ui;
}

// ---- Capability badges (local to detail — also exists in model-table.ts) ----

function renderCapabilityBadges(
  json: string | null | undefined,
  modelType?: string | null,
): TemplateResult {
  const badges: TemplateResult[] = [];
  if (modelType && modelType !== 'chat') {
    badges.push(html`<span class="cap-badge">${modelType}</span>`);
  }
  if (json != null) {
    let caps: unknown;
    if (typeof json === 'string') {
      try {
        caps = JSON.parse(json) as unknown;
      } catch {
        // ignore — fall through with caps = undefined; renderCapabilityBadges
        // handles both null and undefined gracefully below.
      }
    } else {
      caps = json;
    }
    if (caps && typeof caps === 'object') {
      const c = caps as Record<string, unknown>;
      if (c['vision']) badges.push(html`<span class="cap-badge">vision</span>`);
      if (c['tool_calling']) badges.push(html`<span class="cap-badge">tools</span>`);
      if (c['reasoning']) badges.push(html`<span class="cap-badge">reasoning</span>`);
      if (c['thinking']) badges.push(html`<span class="cap-badge">thinking</span>`);
      if (c['structured_output']) badges.push(html`<span class="cap-badge">json</span>`);
      if (c['attachment']) badges.push(html`<span class="cap-badge">attach</span>`);
    }
  }
  return badges.length > 0 ? html`${badges}` : html`<span class="muted">—</span>`;
}

// ============================================================
// Handlers: detail header
// ============================================================

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

// ---- Handlers: OAuth section ----

function onOAuthStartPKCE(providerId: string): void {
  void OAuthLogin.startPKCE(providerId);
}
function onOAuthStartDeviceCode(providerId: string): void {
  void OAuthLogin.startDeviceCode(providerId);
}
function onOAuthSubmitManualCallback(): void {
  void OAuthLogin.submitManualCallback();
}

function onCopyAuthUrl(): void {
  const el = document.getElementById('oauth-auth-url') as HTMLInputElement | null;
  if (el) {
    copyToClipboard(el.value || '').catch(() => {
      /* ignore — silent best-effort */
    });
  }
}

// ---- Handlers: connections (accounts) ----

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

// ============================================================
// Handlers: models section
// ============================================================

async function onBulkToggleModels(
  providerId: string,
  active: boolean,
): Promise<void> {
  const models = (state.models || []).filter(
    (m) => m.provider_id === providerId,
  );
  const customCount = models.filter((m) => m.custom).length;
  const toToggleCount = models.filter((m) => !m.custom && m.active !== active)
    .length;
  if (toToggleCount === 0) {
    showToast('Nothing to toggle.', 'info');
    return;
  }
  const msg = active
    ? `Enable ${toToggleCount} non-custom models? (${customCount} custom models will not be touched)`
    : `Disable ${toToggleCount} non-custom models? (${customCount} custom models will not be touched)`;
  if (
    !(await showConfirm({
      title: active ? 'Enable models' : 'Disable models',
      message: msg,
      confirmLabel: active ? 'Enable' : 'Disable',
    }))
  )
    return;
  try {
    await api('/models/bulk-toggle', {
      method: 'POST',
      body: JSON.stringify({ provider_id: providerId, active }),
    });
    state.models = (await api(
      '/models?provider_id=' + encodeURIComponent(providerId),
    )) as typeof state.models;
    state.modelsComplete = false;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

function onShowCustomModelForm(providerId: string): void {
  showCustomModelForm(providerId);
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

function onUpdateProviderSearch(providerId: string, e: Event): void {
  const target = e.target;
  const value = target instanceof HTMLInputElement ? target.value : '';
  const ui = getProviderUi(providerId);
  ui.search = value;
  ui.page = 1;
  setProviderUi(providerId, ui);
  requestUpdate();
}

function onUpdateProviderFilter(
  providerId: string,
  filter: 'all' | 'active' | 'inactive',
): void {
  const ui = getProviderUi(providerId);
  ui.filter = filter;
  ui.page = 1;
  setProviderUi(providerId, ui);
  requestUpdate();
}

function onCycleProviderSort(providerId: string, sortKey: string): void {
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

function onSetProviderPage(providerId: string, page: number): void {
  const ui = getProviderUi(providerId);
  ui.page = Math.max(1, page);
  setProviderUi(providerId, ui);
  requestUpdate();
}

function onToggleModelSelection(rowId: number, e: Event | null): void {
  const target =
    e && e.target && e.target instanceof HTMLInputElement ? e.target : null;
  const checked = target ? target.checked : false;
  if (checked) state.selectedModels.add(rowId);
  else state.selectedModels.delete(rowId);
  requestUpdate();
}

function onToggleSelectAllModels(e: Event | null): void {
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

function onClearModelSelection(): void {
  state.selectedModels.clear();
  requestUpdate();
}

async function onBulkSetSelected(
  providerId: string,
  active: boolean,
): Promise<void> {
  const ids = Array.from(state.selectedModels).map((n) => Number(n));
  if (ids.length === 0) return;
  if (
    !(await showConfirm({
      title: active ? 'Enable models' : 'Disable models',
      message: `${active ? 'Enable' : 'Disable'} ${ids.length} models?`,
      confirmLabel: active ? 'Enable' : 'Disable',
    }))
  )
    return;
  try {
    await Promise.all(
      ids.map((rowId) =>
        api('/models/' + rowId + '/toggle', {
          method: 'POST',
          body: JSON.stringify({ active }),
        }).catch((err: unknown) => console.error('Failed toggle', rowId, err)),
      ),
    );
    state.models = (await api(
      '/models?provider_id=' + encodeURIComponent(providerId),
    )) as Model[];
    state.modelsComplete = false;
    state.selectedModels.clear();
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

function onBulkEnableSelected(providerId: string): Promise<void> {
  return onBulkSetSelected(providerId, true);
}
function onBulkDisableSelected(providerId: string): Promise<void> {
  return onBulkSetSelected(providerId, false);
}

async function onBulkTestSelected(providerId: string): Promise<void> {
  void providerId;
  const ids = Array.from(state.selectedModels).map((n) => Number(n));
  if (ids.length === 0) return;
  if (
    !(await showConfirm({
      title: 'Test models',
      message: `Test ${ids.length} models sequentially?`,
      confirmLabel: 'Test',
    }))
  )
    return;
  try {
    for (const rowId of ids) {
      const btn = document.getElementById(
        `test-btn-${rowId}`,
      ) as HTMLButtonElement | null;
      if (btn) {
        btn.disabled = true;
        btn.textContent = 'Testing...';
      }

      const accountSelect = document.getElementById(
        `test-account-${rowId}`,
      ) as HTMLSelectElement | null;
      const proxySelect = document.getElementById(
        `test-proxy-${rowId}`,
      ) as HTMLSelectElement | null;
      const accountId =
        accountSelect && accountSelect.value
          ? parseInt(accountSelect.value, 10)
          : null;
      const proxyId =
        proxySelect && proxySelect.value ? proxySelect.value : null;

      const result = (await api(`/models/${rowId}/test`, {
        method: 'POST',
        body: JSON.stringify({ account_id: accountId, proxy_id: proxyId }),
      })) as { status: number; elapsed_ms: number; row_id?: number };
      const m = (state.models || []).find((x) => x.row_id === rowId);
      if (m) {
        m.last_test_status = result.status;
        m.last_test_at = new Date().toISOString();
      }
      if (btn) {
        if (result.status >= 200 && result.status < 300) {
          btn.textContent = '✓';
          btn.style.background = '#a6e3a1';
        } else {
          btn.textContent = '✗ ' + result.status;
          btn.style.background = '#f38ba8';
        }
        setTimeout(() => {
          btn.textContent = 'Test';
          btn.style.background = '';
          btn.disabled = false;
        }, 1500);
      }
    }
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

async function onBulkDeleteSelected(providerId: string): Promise<void> {
  const ids = Array.from(state.selectedModels).map((n) => Number(n));
  if (ids.length === 0) return;
  if (
    !(await showConfirm({
      title: 'Delete models',
      message: `Delete ${ids.length} models? This cannot be undone.`,
      danger: true,
      confirmLabel: 'Delete',
    }))
  )
    return;
  try {
    await Promise.all(
      ids.map((rowId) =>
        api('/models/' + rowId, { method: 'DELETE' }).catch((err: unknown) =>
          console.error('Failed delete', rowId, err),
        ),
      ),
    );
    state.models = (await api(
      '/models?provider_id=' + encodeURIComponent(providerId),
    )) as Model[];
    state.modelsComplete = false;
    state.selectedModels.clear();
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

async function onBulkSetModalitySelected(newType: string): Promise<void> {
  const ids = Array.from(state.selectedModels).map((n) => Number(n));
  if (ids.length === 0) return;
  try {
    await Promise.all(
      ids.map((rowId) =>
        api(`/models/${rowId}`, {
          method: 'PATCH',
          body: JSON.stringify({ model_type: newType }),
        }),
      ),
    );
    for (const rowId of ids) {
      const m = (state.models || []).find((x) => x.row_id === rowId);
      if (m) m.model_type = newType;
    }
    showToast(`Tagged ${ids.length} models as ${newType}`, 'success');
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

async function onChangeModelType(rowId: number, e: Event): Promise<void> {
  const target = e.target instanceof HTMLSelectElement ? e.target : null;
  if (!target) return;
  const newType = target.value;
  try {
    await api(`/models/${rowId}`, {
      method: 'PATCH',
      body: JSON.stringify({ model_type: newType }),
    });
    const m = (state.models || []).find((x) => x.row_id === rowId);
    if (m) m.model_type = newType;
    showToast(`Model modality updated to ${newType}`, 'success');
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

async function onToggleModel(rowId: number, newActive: boolean): Promise<void> {
  try {
    await api('/models/' + rowId + '/toggle', {
      method: 'POST',
      body: JSON.stringify({ active: newActive }),
    });
    const m = (state.models || []).find((x) => x.row_id === rowId);
    if (m) m.active = newActive;
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

async function onTestModel(rowId: number, e: Event | null): Promise<void> {
  const btn = (e && e.target instanceof HTMLButtonElement
    ? e.target
    : null) as HTMLButtonElement | null;
  if (!btn) return;
  const oldText = btn.textContent;
  btn.disabled = true;
  btn.textContent = 'Testing...';

  const accountSelect =
    (document.getElementById(`test-account-${rowId}`) ||
      document.getElementById(
        `test-account-m-${rowId}`,
      )) as HTMLSelectElement | null;
  const proxySelect =
    (document.getElementById(`test-proxy-${rowId}`) ||
      document.getElementById(
        `test-proxy-m-${rowId}`,
      )) as HTMLSelectElement | null;
  const accountId =
    accountSelect && accountSelect.value
      ? parseInt(accountSelect.value, 10)
      : null;
  const proxyId =
    proxySelect && proxySelect.value ? proxySelect.value : null;

  try {
    const result = (await api(`/models/${rowId}/test`, {
      method: 'POST',
      body: JSON.stringify({ account_id: accountId, proxy_id: proxyId }),
    })) as { status: number; elapsed_ms: number; row_id?: number };
    const rid = result.row_id ?? rowId;
    const m = (state.models || []).find((x) => x.row_id === rid);
    if (m) {
      m.last_test_status = result.status;
      m.last_test_at = new Date().toISOString();
    }
    if (result.status >= 200 && result.status < 300) {
      flashButton(btn, '✓', '#a6e3a1');
    } else if (result.status === 0) {
      flashButton(btn, '✗ net', '#f38ba8');
    } else {
      flashButton(btn, '✗ ' + result.status, '#f38ba8');
    }
    requestUpdate();
  } catch (err: unknown) {
    flashButton(btn, '✗', '#f38ba8');
    showApiError(err, 'Test failed');
  } finally {
    setTimeout(() => {
      btn.disabled = false;
      btn.textContent = oldText;
    }, 1500);
  }
}

async function onDeleteModel(rowId: number): Promise<void> {
  if (
    !(await showConfirm({
      title: 'Delete model',
      message:
        'Delete this model? Combo targets referencing it will be removed too.',
      danger: true,
      confirmLabel: 'Delete',
    }))
  )
    return;
  try {
    await api(`/models/${rowId}`, { method: 'DELETE' });
    state.models = state.models.filter((m) => m.row_id !== rowId);
    state.selectedModels.delete(rowId);
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

async function onCopyUsageModel(text: string, e: Event): Promise<void> {
  e.preventDefault();
  e.stopPropagation();
  try {
    await copyToClipboard(text);
    showToast('Copied: ' + text, 'success');
  } catch {
    showToast('Failed to copy', 'error');
  }
}

// ============================================================
// Detail sub-section render functions
// ============================================================

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

function renderOAuthSection(provider: Provider): TemplateResult {
  if (provider.auth_type !== 'oauth') return html``;
  const buttons: TemplateResult[] = [];
  if (
    provider.oauth_flows?.includes('pkce') ||
    provider.oauth_flows?.includes('auth_code')
  ) {
    buttons.push(
      html`<button class="primary" @click=${() => onOAuthStartPKCE(provider.id)}>Log in with ${provider.name || provider.id}</button>`,
    );
  }
  if (provider.oauth_flows?.includes('device')) {
    buttons.push(
      html`<button class="primary" @click=${() => onOAuthStartDeviceCode(provider.id)}>Log in with ${provider.name || provider.id}</button>`,
    );
  }
  return html`
    <section class="detail-section">
      <div class="section-header"><h3>OAuth login</h3></div>
      <div class="oauth-buttons">${buttons}</div>
      <div id="oauth-device-info" style="display:none;"></div>
      <div id="oauth-manual-section" class="oauth-manual-card" style="display:none;">
        <h4>1. Authorize</h4>
        <p>Open this URL in a new tab and complete the login:</p>
        <div class="oauth-manual-url">
          <input id="oauth-auth-url" type="text" readonly>
          <button type="button" class="btn-secondary" @click=${onCopyAuthUrl}>Copy</button>
        </div>
        <h4>2. Paste the callback URL</h4>
        <p>After the OAuth provider redirects, copy the full URL from your address bar and paste it here:</p>
        <div class="oauth-manual-input">
          <input id="oauth-callback-input" type="text" placeholder="https://...">
          <button type="button" class="primary" @click=${onOAuthSubmitManualCallback}>Submit</button>
        </div>
      </div>
    </section>
  `;
}

function renderConnectionsSection(
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

function renderModelRow(m: Model): TemplateResult {
  const isSelected: boolean = (state.selectedModels as Set<number>).has(
    m.row_id,
  );
  const lastTest: TemplateResult =
    m.last_test_status != null
      ? html`<span class=${'status-pill ' + statusPillClass(m.last_test_status)}>${String(m.last_test_status)}</span> <small class="last-test-time">${m.last_test_at || ''}</small>`
      : html`<span class="muted">never</span>`;

  const providerAccounts = (state.accounts || []).filter(
    (a) => a.provider_id === m.provider_id,
  );
  const aliveProxies = (state.proxies || []).filter(
    (p) => p.status === 'alive',
  );

  const usageModel = `${m.provider_id}/${m.model_id}`;

  return html`<tr id=${`model-row-${m.row_id}`} class=${'model-card-row ' + (m.active ? '' : 'inactive') + (isSelected ? ' selected' : '')}>
    <td class="col-model-select"><input type="checkbox" ?checked=${isSelected} @change=${(e: Event) => onToggleModelSelection(m.row_id, e)}></td>
    <td class="col-model-name" data-label="Model">
      <div class="model-name-wrapper">
        <code class="model-usage-code" title="Click to copy usage model" @click=${(e: Event) => onCopyUsageModel(usageModel, e)}>${usageModel}</code>
        ${m.custom ? html`<span class="badge custom">custom</span>` : html``}
      </div>
    </td>
    <td class="col-model-format" data-label="Format"><span class="chip format-chip">${m.target_format || '—'}</span></td>
    <td class="col-model-context" data-label="Context"><span class="model-spec-val">${formatContextBadge(m.context_length)}</span></td>
    <td class="col-model-max-output" data-label="Max Output"><span class="model-spec-val">${formatContextBadge(m.max_output_tokens)}</span></td>
    <td class="col-model-modality" data-label="Modality & Capabilities">
      <div class="model-modality-block">
        <div class="model-type-row">
          <select
            class="model-type-select"
            title="Model modality / type (Chat, Image, Embedding, Audio, Rerank)"
            .value=${m.model_type || 'chat'}
            @change=${(e: Event) => onChangeModelType(m.row_id, e)}
          >
            <option value="chat" ?selected=${!m.model_type || m.model_type === 'chat'}>Chat</option>
            <option value="image" ?selected=${m.model_type === 'image'}>Image</option>
            <option value="embedding" ?selected=${m.model_type === 'embedding'}>Embedding</option>
            <option value="audio" ?selected=${m.model_type === 'audio'}>Audio</option>
            <option value="rerank" ?selected=${m.model_type === 'rerank'}>Rerank</option>
          </select>
        </div>
        <div class="model-caps-row">
          ${renderCapabilityBadges(m.capabilities_json, m.model_type)}
          ${m.family ? html` <small class="muted model-family-tag">${m.family}</small>` : html``}
        </div>
      </div>
    </td>
    <td class="col-model-status" data-label="Status"><span class=${'status-pill ' + (m.active ? 'on' : 'off')}>${m.active ? 'active' : 'inactive'}</span></td>
    <td class="col-model-test-status last-test-cell" data-label="Last test">${lastTest}</td>
    <td class="col-model-actions" data-label="Actions">
      <div class="model-actions-block">
        <div class="model-test-selectors">
          <select id=${`test-account-${m.row_id}`} class="model-account-select">
            <option value="">(Default Account)</option>
            ${providerAccounts.map((a) => html`<option value=${a.id}>${a.label || `Account #${a.id}`}</option>`)}
          </select>
          <select id=${`test-proxy-${m.row_id}`} class="model-proxy-select">
            <option value="">(No Proxy)</option>
            ${aliveProxies.map((p) => html`<option value=${p.id}>${p.host}:${p.port} (${p.latency_ms || '?'}ms)</option>`)}
          </select>
        </div>
        <div class="model-btn-group">
          <button class="small primary" id=${`test-btn-${m.row_id}`} @click=${(e: Event) => onTestModel(m.row_id, e)}>${icons.flask()} Test</button>
          <button class="small" @click=${() => onToggleModel(m.row_id, !m.active)}>${m.active ? 'Disable' : 'Enable'}</button>
          <button class="small danger" @click=${() => onDeleteModel(m.row_id)} title="Delete model">${icons.close()}</button>
        </div>
      </div>
    </td>

    <!-- Mobile Card Structure -->
    <td class="mobile-model-card-cell">
      <div class="card-top-header">
        <div class="card-title-area">
          <input
            type="checkbox"
            .checked=${isSelected}
            @change=${(e: Event) => onToggleModelSelection(m.row_id, e)}
          />
          <span
            class="card-model-title"
            title="Click to copy usage model name"
            @click=${(e: Event) => onCopyUsageModel(usageModel, e)}
          >
            ${usageModel}
          </span>
          ${m.custom ? html`<span class="badge custom" style="font-size:0.62rem;padding:0 3px;">custom</span>` : html``}
        </div>
        <span class="card-status-pill ${m.active ? 'active' : 'inactive'}">
          ${m.active ? 'active' : 'inactive'}
        </span>
      </div>

      <div class="card-meta-row">
        <span class="meta-chip accent">${m.target_format || 'openai'}</span>
        <span class="meta-chip">${formatContextBadge(m.context_length)} ctx</span>
        <span class="meta-chip">${formatContextBadge(m.max_output_tokens)} out</span>
        ${renderCapabilityBadges(m.capabilities_json, m.model_type)}
        ${m.family ? html`<span class="meta-chip">${m.family}</span>` : html``}
      </div>

      <div class="card-config-box">
        <div class="config-item">
          <span class="config-val-upstream" title="${m.model_id}">
            ${m.model_id}
          </span>
        </div>
        <div class="config-item">
          <span class="config-label">Test</span>
          <span class="config-val">${lastTest}</span>
        </div>
      </div>

      <div class="card-routes-grid">
        <select class="custom-mobile-select" id=${`test-account-m-${m.row_id}`} @change=${(e: Event) => {
          const el = document.getElementById(`test-account-${m.row_id}`) as HTMLSelectElement;
          if (el) el.value = (e.target as HTMLSelectElement).value;
        }}>
          <option value="">(Default Account)</option>
          ${providerAccounts.map((acc) => html`<option value="${acc.id}">${acc.label || `Account #${acc.id}`}</option>`)}
        </select>
        <select class="custom-mobile-select" id=${`test-proxy-m-${m.row_id}`} @change=${(e: Event) => {
          const el = document.getElementById(`test-proxy-${m.row_id}`) as HTMLSelectElement;
          if (el) el.value = (e.target as HTMLSelectElement).value;
        }}>
          <option value="">(No Proxy)</option>
          ${aliveProxies.map((prx) => html`<option value="${prx.id}">${prx.host}:${prx.port} (${prx.latency_ms || '?'}ms)</option>`)}
        </select>
      </div>

      <div class="card-actions-bar">
        <button class="btn-card primary" @click=${(e: Event) => {
          const accM = document.getElementById(`test-account-m-${m.row_id}`) as HTMLSelectElement;
          const prxM = document.getElementById(`test-proxy-m-${m.row_id}`) as HTMLSelectElement;
          const accD = document.getElementById(`test-account-${m.row_id}`) as HTMLSelectElement;
          const prxD = document.getElementById(`test-proxy-${m.row_id}`) as HTMLSelectElement;
          if (accM && accD) accD.value = accM.value;
          if (prxM && prxD) prxD.value = prxM.value;
          onTestModel(m.row_id, e);
        }}>⚡ Test</button>
        <button class="btn-card" @click=${() => onToggleModel(m.row_id, !m.active)}>
          ${m.active ? 'Disable' : 'Enable'}
        </button>
        <button class="btn-card danger" @click=${() => onDeleteModel(m.row_id)} title="Delete">✕</button>
      </div>
    </td>
  </tr>`;
}

// ============================================================
// Top-level detail template
// ============================================================

function renderModelsSection(
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