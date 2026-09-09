// views/providers/models-row.ts — per-row model handlers and the
// renderModelRow template.
//
// Split out of the former detail.ts monolith (FU1). Each handler
// operates on a single `row_id` (the server-side primary key), while
// renderModelRow produces the full <tr> for a model row (desktop + mobile
// card variant) and wires the per-row button @click callbacks back here.

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

// ---- Per-row action handlers ----

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

// ---- renderModelRow (desktop + mobile card) ----

export function renderModelRow(m: Model): TemplateResult {
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

// ---- Per-provider checkbox helpers (exported for models.ts renderModelsSection) ----

export function onToggleModelSelection(rowId: number, e: Event | null): void {
  const target =
    e && e.target && e.target instanceof HTMLInputElement ? e.target : null;
  const checked = target ? target.checked : false;
  if (checked) state.selectedModels.add(rowId);
  else state.selectedModels.delete(rowId);
  requestUpdate();
}
