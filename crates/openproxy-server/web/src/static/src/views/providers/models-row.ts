// views/providers/models-row.ts — per-row handlers keyed on `row_id` (the
// server-side primary key) plus `renderModelRow`, which emits the coherent dense
// entry (`<article class="provider-entry model-card-row">`) and wires the per-row callbacks.

import { html, type TemplateResult } from 'lit-html';
import { state } from '../../state/index.js';
import { api } from '../../state/api.js';
import { requestUpdate } from '../../state/reactive.js';
import { showToast } from '../../components/toast.js';
import { flashButton, showApiError } from '../../lib/ui-utils.js';
import { copyToClipboard } from '../../lib/clipboard.js';
import { showConfirm } from '../../lib/show-confirm.js';
import { icons } from '../../lib/icons.js';
import { statusPillClass } from '../../lib/constants.js';
import { formatContextBadge } from '../../lib/format.js';
import { renderCapabilityBadges } from './shared.js';
import type { Model } from '../../lib/types/api.js';
import { FALLBACK_TARGET_FORMATS } from '../../lib/types/common.js';
import { renderClientProfileSelect } from '../../components/client-profile-select.js';
import type { ClientProfile } from '../../lib/types/client-profile.js';
import { t } from '../../i18n/index.js';


export async function onChangeModelFormat(rowId: number, e: Event): Promise<void> {
  const target = e.target instanceof HTMLSelectElement ? e.target : null;
  if (!target) return;
  const newFormat = target.value;
  try {
    await api(`/models/${rowId}`, {
      method: 'PATCH',
      body: JSON.stringify({ target_format: newFormat }),
    });
    const m = (state.models || []).find((x) => x.row_id === rowId);
    if (m) m.target_format = newFormat as any;
    showToast(`Model format updated to ${newFormat}`, 'success');
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

export async function onChangeModelClientProfile(rowId: number, newProfile: ClientProfile | null): Promise<void> {
  try {
    await api(`/models/${rowId}`, {
      method: 'PATCH',
      body: JSON.stringify({ client_profile: newProfile }),
    });
    const m = (state.models || []).find((x) => x.row_id === rowId);
    if (m) m.client_profile = newProfile;
    showToast(newProfile ? `Client profile set to ${newProfile}` : 'Client profile cleared', 'success');
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error updating client profile');
  }
}

export async function onChangeModelType(rowId: number, e: Event): Promise<void> {
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

export async function onChangeModelStreaming(rowId: number, e: Event): Promise<void> {
  const target = e.target instanceof HTMLSelectElement ? e.target : null;
  if (!target) return;
  const val = target.value;
  const streaming = val === 'streaming' ? true : val === 'unary' ? false : null;
  try {
    await api(`/models/${rowId}`, {
      method: 'PATCH',
      body: JSON.stringify({ streaming }),
    });
    const m = (state.models || []).find((x) => x.row_id === rowId);
    if (m) {
      let caps: Record<string, unknown> = {};
      try {
        if (m.capabilities_json) caps = JSON.parse(m.capabilities_json) as Record<string, unknown>;
      } catch {}
      if (streaming === null) {
        delete caps['streaming'];
      } else {
        caps['streaming'] = streaming;
      }
      m.capabilities_json = Object.keys(caps).length > 0 ? JSON.stringify(caps) : null;
    }
    showToast(`Model streaming updated`, 'success');
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, 'Error');
  }
}

export async function onToggleModel(rowId: number, newActive: boolean): Promise<void> {
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

export async function onTestModel(rowId: number, e: Event | null): Promise<void> {
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

export async function onDeleteModel(rowId: number): Promise<void> {
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

export async function onCopyUsageModel(text: string, e: Event): Promise<void> {
  e.preventDefault();
  e.stopPropagation();
  try {
    await copyToClipboard(text);
    showToast('Copied: ' + text, 'success');
  } catch {
    showToast('Failed to copy', 'error');
  }
}


export function renderModelRow(m: Model): TemplateResult {
  const isSelected: boolean = (state.selectedModels as Set<number>).has(
    m.row_id,
  );
  const lastTestStatusBadge: TemplateResult =
    m.last_test_status != null
      ? html`<span class=${'status-pill ' + statusPillClass(m.last_test_status)} title=${m.last_test_at || ''}>${String(m.last_test_status)}</span>`
      : html`<span class="muted" title="never">never</span>`;

  const providerAccounts = (state.accounts || []).filter(
    (a) => a.provider_id === m.provider_id,
  );
  const aliveProxies = (state.proxies || []).filter(
    (p) => p.status === 'alive',
  );

  const usageModel = `${m.provider_id}/${m.model_id}`;

  let modelStreaming: boolean | undefined = undefined;
  if (m.capabilities_json) {
    try {
      const parsed = JSON.parse(m.capabilities_json) as Record<string, unknown>;
      if (typeof parsed['streaming'] === 'boolean') modelStreaming = parsed['streaming'];
    } catch {}
  }

  const selectAria = `${t('providers.detail.entry.select')} ${usageModel}`;

  return html`<article
    class=${'provider-entry model-card-row ' + (m.active ? '' : 'inactive') + (isSelected ? ' selected' : '')}
    id=${`model-row-${m.row_id}`}
    role="listitem"
  >
    <details class="model-entry-card" id=${`model-details-${m.row_id}`}>
      <summary class="model-entry-row" title="Click to view & configure model details">
        <div class="model-entry-select" @click=${(e: Event) => e.stopPropagation()}>
          <input
            type="checkbox"
            aria-label=${selectAria}
            .checked=${isSelected}
            @change=${(e: Event) => onToggleModelSelection(m.row_id, e)}
          />
        </div>

        <div class="model-entry-main">
          <div class="model-entry-title-line">
            <code
              class="model-usage-code"
              title="Click to copy usage model"
              @click=${(e: Event) => {
                e.stopPropagation();
                void onCopyUsageModel(usageModel, e);
              }}
            >${usageModel}</code>
            ${m.custom ? html`<span class="badge custom">custom</span>` : html``}
            ${m.client_profile ? html`<span class="model-profile-badge" title="Spoofed Client Profile">${m.client_profile}</span>` : html``}
          </div>

          <div class="model-entry-sub-line">
            <code>${m.target_format || 'openai'}</code>
            <span class="dot">·</span>
            <span>${formatContextBadge(m.context_length)} ctx</span>
            <span class="dot">·</span>
            <span>${formatContextBadge(m.max_output_tokens)} out</span>
            <span class="dot">·</span>
            <span>${lastTestStatusBadge}</span>
            ${m.capabilities_json || m.model_type ? html`
              <span class="dot">·</span>
              <span class="model-entry-caps">
                ${renderCapabilityBadges(m.capabilities_json, m.model_type)}
              </span>
            ` : html``}
          </div>
        </div>

        <div class="model-entry-controls" @click=${(e: Event) => e.stopPropagation()}>
          <button
            type="button"
            class="small primary"
            id=${`test-btn-${m.row_id}`}
            title="Execute live test request"
            @click=${(e: Event) => onTestModel(m.row_id, e)}
          >${icons.flask()} Test</button>

          <button
            type="button"
            role="switch"
            aria-checked=${m.active ? 'true' : 'false'}
            aria-label=${m.active ? 'Disable model' : 'Enable model'}
            class="toggle-btn ${m.active ? 'on' : 'off'}"
            title=${m.active ? 'Active (click to disable)' : 'Inactive (click to enable)'}
            @click=${() => onToggleModel(m.row_id, !m.active)}
          >
            <span class="toggle-thumb"></span>
          </button>
        </div>

        <span class="model-entry-chevron" aria-hidden="true">${icons.chevronRight()}</span>
      </summary>

      <div class="model-entry-drawer">
        <!-- Panel 1: Protocol & Routing Config (Structure D) -->
        <div class="model-config-panel">
          <div class="model-config-title">Protocol & Routing Config</div>
          <div class="model-config-grid">
            <label>
              <span>${t('providers.detail.entry.format')}</span>
              <select
                class="model-format-select"
                title="Model target format / wire protocol"
                .value=${m.target_format || 'openai'}
                @change=${(e: Event) => onChangeModelFormat(m.row_id, e)}
              >
                ${(state.formats?.target_formats ?? FALLBACK_TARGET_FORMATS).map(
                  (fmt) => html`<option value=${fmt.id} title=${fmt.description} ?selected=${(m.target_format || 'openai') === fmt.id}>${fmt.id}</option>`
                )}
              </select>
            </label>

            <label>
              <span>Client Profile</span>
              ${renderClientProfileSelect({
                className: 'model-format-select',
                value: m.client_profile,
                allowInherit: false,
                noneLabel: 'None (direct / default)',
                onChange: (newProfile) => onChangeModelClientProfile(m.row_id, newProfile),
              })}
            </label>

            <label>
              <span>${t('providers.detail.entry.modality')}</span>
              <select
                class="model-type-select"
                title="Model modality / type (Chat, Image, Embedding, Audio, Rerank, Decision)"
                .value=${m.model_type || 'chat'}
                @change=${(e: Event) => onChangeModelType(m.row_id, e)}
              >
                <option value="chat" ?selected=${!m.model_type || m.model_type === 'chat'}>Chat</option>
                <option value="image" ?selected=${m.model_type === 'image'}>Image</option>
                <option value="embedding" ?selected=${m.model_type === 'embedding'}>Embedding</option>
                <option value="audio" ?selected=${m.model_type === 'audio'}>Audio</option>
                <option value="rerank" ?selected=${m.model_type === 'rerank'}>Rerank</option>
                <option value="decision" ?selected=${m.model_type === 'decision'}>Decision</option>
              </select>
            </label>

            <label>
              <span>${t('providers.detail.entry.stream')}</span>
              <select
                class="model-type-select"
                title="Stream override: auto (provider default), unary (forced non-streaming), stream (forced SSE)"
                @change=${(e: Event) => onChangeModelStreaming(m.row_id, e)}
              >
                <option value="auto" ?selected=${modelStreaming === undefined}>auto stream</option>
                <option value="unary" ?selected=${modelStreaming === false}>unary</option>
                <option value="streaming" ?selected=${modelStreaming === true}>stream</option>
              </select>
            </label>
          </div>
        </div>

        <!-- Panel 2: Live Test Harness Target (Structure D) -->
        <div class="model-config-panel">
          <div class="model-config-title">Test Harness Target</div>
          <div class="model-config-grid">
            <label>
              <span>${t('providers.detail.entry.account')}</span>
              <select id=${`test-account-${m.row_id}`} class="model-account-select">
                <option value="">(Default Account)</option>
                ${providerAccounts.map((a) => html`<option value=${a.id}>${a.label || `Account #${a.id}`}</option>`)}
              </select>
            </label>

            <label>
              <span>${t('providers.detail.entry.proxy')}</span>
              <select id=${`test-proxy-${m.row_id}`} class="model-proxy-select">
                <option value="">(No Proxy)</option>
                ${aliveProxies.map((p) => html`<option value=${p.id}>${p.host}:${p.port} (${p.latency_ms || '?'}ms)</option>`)}
              </select>
            </label>
          </div>
        </div>

        <!-- Footer Actions (Structure D) -->
        <div class="model-config-footer">
          <div class="model-config-footer-left">
            ${m.family ? html`<span>Family: <code>${m.family}</code></span>` : html``}
          </div>
          <div class="model-config-footer-right">
            <button
              class="small danger"
              @click=${() => onDeleteModel(m.row_id)}
              title="Delete model"
            >${icons.close()} Delete</button>
          </div>
        </div>
      </div>
    </details>
  </article>`;
}


export function onToggleModelSelection(rowId: number, e: Event | null): void {
  const target =
    e && e.target && e.target instanceof HTMLInputElement ? e.target : null;
  const checked = target ? target.checked : false;
  if (checked) state.selectedModels.add(rowId);
  else state.selectedModels.delete(rowId);
  requestUpdate();
}
