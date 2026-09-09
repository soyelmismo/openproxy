// views/playground/inspector.ts — Response inspector panel + sidebar.
//
// Renders the tabbed response inspector (formatted / raw JSON / headers /
// stream chunks) and the right inspector sidebar (target/auth selector,
// hyperparameters card). Formatted response content and the metrics bar live
// in formatted-response.ts and metrics-bar.ts respectively.
//
// All reads/writes go through the shared PlaygroundState reference.

import { html, type TemplateResult } from 'lit-html';
import { t } from '../../i18n/index.js';
import { state } from '../../state/index.js';
import { showToast } from '../../components/toast.js';
import { requestUpdate } from '../../state/reactive.js';
import { api } from '../../state/api.js';
import type { Model, Provider, Account } from '../../lib/types/api.js';
import type { PlaygroundState } from './shared.js';
import {
  inferModelTypeFrontend,
  copyText,
} from './shared.js';
import { renderMetricsBar } from './metrics-bar.js';
import { renderFormattedResponse } from './formatted-response.js';
import {
  renderChatHyperparams,
  renderImageHyperparams,
  renderEmbeddingHyperparams,
  renderAudioHyperparams,
} from './hyperparams.js';

function getFilteredModels(st: PlaygroundState): Array<{ id: string; name: string; type: string; provider: string; isCombo?: boolean }> {
  const models = (state.models as Model[]) || [];
  const combos = (state.combos as import('../../lib/types/api.js').Combo[]) || [];
  const result: Array<{ id: string; name: string; type: string; provider: string; isCombo?: boolean }> = [];

  const matchingModels = models
    .filter((m) => m.active !== false)
    .filter((m) => !st.selectedProviderId || m.provider_id === st.selectedProviderId);

  if (!st.selectedProviderId) {
    for (const c of combos) {
      const comboType = inferModelTypeFrontend(c.name, 'chat');
      if (comboType === st.modality) {
        result.push({
          id: `combo:${c.name}`,
          name: `[Combo] ${c.name} (${c.strategy})`,
          type: comboType,
          provider: 'combo',
          isCombo: true,
        });
      }
    }
  }

  for (const m of matchingModels) {
    const inferredType = inferModelTypeFrontend(m.model_id, m.model_type);
    if (inferredType === st.modality) {
      result.push({
        id: m.model_id,
        name: m.display_name ? `${m.display_name} (${m.model_id})` : m.model_id,
        type: inferredType,
        provider: m.provider_id,
      });
    }
  }

  return result;
}

function ensureDefaultModel(st: PlaygroundState): void {
  const models = getFilteredModels(st);
  if (models.length > 0) {
    const exists = models.some((m) => m.id === st.selectedModelId);
    if (!exists && models[0]) {
      st.selectedModelId = models[0].id;
    }
  }
}

// Re-export ensureDefaultModel for use by index.ts
export { ensureDefaultModel };

// ==========
// Response Inspector (tabbed panel embedded inside workspaces)
// ==========

export function renderResponseInspector(st: PlaygroundState): TemplateResult {
  return html`
    <div class="playground-response-panel">
      <div class="playground-response-panel-header">
        <div class="detail-tabs" role="tablist" style="margin-bottom: 0;">
          <button
            class="detail-tab ${st.activeResponseTab === 'formatted' ? 'active' : ''}"
            @click=${() => {
              st.activeResponseTab = 'formatted';
              requestUpdate();
            }}
          >
             ${t('playground.inspector.formatted')}
          </button>
          <button
            class="detail-tab ${st.activeResponseTab === 'raw' ? 'active' : ''}"
            @click=${() => {
              st.activeResponseTab = 'raw';
              requestUpdate();
            }}
          >
             ${t('playground.inspector.raw_json')}
          </button>
          <button
            class="detail-tab ${st.activeResponseTab === 'headers' ? 'active' : ''}"
            @click=${() => {
              st.activeResponseTab = 'headers';
              requestUpdate();
            }}
          >
             ${t('playground.inspector.headers')}
          </button>
          ${st.modality === 'chat' && st.chatStream
            ? html`
                <button
                  class="detail-tab ${st.activeResponseTab === 'stream' ? 'active' : ''}"
                  @click=${() => {
                    st.activeResponseTab = 'stream';
                    requestUpdate();
                  }}
                >
                   ${t('playground.inspector.stream', { count: st.streamChunks.length })}
                </button>
              `
            : html``}
        </div>

        <div class="playground-actions-row">
          <button
            class="button small"
            @click=${() => {
              if (st.rawResponseText) {
                copyText(st.rawResponseText, 'Raw response');
              }
            }}
             title=${t('playground.inspector.copy_response')}
           >
             ${t('playground.inspector.copy')}
          </button>
        </div>
      </div>

      ${renderMetricsBar(st)}

      <div class="playground-response-body">
        ${st.activeResponseTab === 'formatted'
          ? renderFormattedResponse(st)
          : st.activeResponseTab === 'raw'
          ? html`
               <pre class="playground-code-view"><code>${st.parsedResponseJson ? JSON.stringify(st.parsedResponseJson, null, 2) : (st.rawResponseText || t('playground.inspector.no_response'))}</code></pre>
            `
          : st.activeResponseTab === 'headers'
          ? html`
              <table class="playground-headers-table">
                 <thead><tr><th>${t('playground.inspector.header')}</th><th>${t('playground.inspector.value')}</th></tr></thead>
                <tbody>
                  ${Object.keys(st.responseHeaders).length === 0
                     ? html`<tr><td colspan="2" class="text-muted">${t('playground.inspector.no_headers')}</td></tr>`
                    : Object.entries(st.responseHeaders).map(
                        ([k, v]) => html`<tr><td><code>${k}</code></td><td>${v}</td></tr>`,
                      )}
                </tbody>
              </table>
            `
          : html`
              <div class="playground-stream-log">
                ${st.streamChunks.length === 0
                   ? html`<p class="text-muted" style="padding: var(--space-4); text-align: center;">${t('playground.inspector.no_stream')}</p>`
                  : st.streamChunks.map(
                      (c) => html`
                        <div class="stream-chunk-row">
                          <span class="chunk-idx">#${c.index}</span>
                          <span class="chunk-time">+${c.timestampMs}ms</span>
                          <code class="chunk-content">${c.delta || c.raw}</code>
                        </div>
                      `,
                    )}
              </div>
            `}
      </div>
    </div>
  `;
}

// ==========
// Inspector Sidebar (right panel: target/auth + hyperparams)
// ==========

export function renderInspectorSidebar(st: PlaygroundState): TemplateResult {
  const providers = (state.providers as Provider[]) || [];
  const accounts = (state.accounts as Account[]) || [];
  const filteredModels = getFilteredModels(st);
  const apiKeys = (state.apiKeys as Array<{ id: number; label: string | null; key_prefix: string | null }>) || [];

  const matchingAccounts = st.selectedProviderId
    ? accounts.filter((a) => a.provider_id === st.selectedProviderId)
    : accounts;

  return html`
    <div class="playground-inspector-sidebar">
      <!-- Target & Auth Card -->
      <div class="playground-sidebar-card">
        <div class="playground-sidebar-header">
           <h4>${t('playground.sidebar.target_title')}</h4>
          <span class="badge badge-info">${st.modality.toUpperCase()}</span>
        </div>

        <div class="playground-sidebar-body">
          <!-- API Key Source -->
          <div class="field">
             <label class="field-label">${t('playground.sidebar.api_key_auth')}</label>
            <select
              .value=${st.keySource}
              @change=${(e: Event) => {
                st.keySource = (e.target as HTMLSelectElement).value as typeof st.keySource;
                requestUpdate();
              }}
            >
               <option value="session">${t('playground.sidebar.session_token')}</option>
              ${apiKeys.map(
                (k) => html`<option value="key:${k.key_prefix}">${k.label || 'API Key'} (${k.key_prefix}…)</option>`,
              )}
              <option value="custom">Custom Bearer Key</option>
            </select>
          </div>

          ${st.keySource === 'custom'
            ? html`
                <div class="field">
                  <label class="field-label">${t('playground.sidebar.custom_key')}</label>
                  <input
                    type="password"
                    placeholder="sk-..."
                    .value=${st.customApiKey}
                    @input=${(e: Event) => {
                      st.customApiKey = (e.target as HTMLInputElement).value;
                    }}
                  />
                </div>
              `
            : html``}

          <!-- Provider Selector -->
          <div class="field">
             <label class="field-label">${t('playground.sidebar.provider')}</label>
            <select
              .value=${st.selectedProviderId}
              @change=${(e: Event) => {
                st.selectedProviderId = (e.target as HTMLSelectElement).value;
                if (st.selectedAccountId) {
                  const acc = accounts.find((a) => String(a.id) === st.selectedAccountId);
                  if (acc && st.selectedProviderId && acc.provider_id !== st.selectedProviderId) {
                    st.selectedAccountId = '';
                  }
                }
                const models = getFilteredModels(st);
                if (models.length > 0 && models[0]) {
                  st.selectedModelId = models[0].id;
                }
                requestUpdate();
              }}
            >
               <option value="">${t('playground.sidebar.any_provider')}</option>
              ${providers.map((p) => html`<option value=${p.id}>${p.name || p.id}</option>`)}
            </select>
          </div>

          <!-- Account Dropdown Select -->
          <div class="field">
             <label class="field-label">${t('playground.sidebar.account_routing')}</label>
            <select
              .value=${st.selectedAccountId}
              @change=${(e: Event) => {
                st.selectedAccountId = (e.target as HTMLSelectElement).value;
                requestUpdate();
              }}
            >
               <option value="">${t('playground.sidebar.auto_routing')}</option>
              ${matchingAccounts.map(
                (a) => html`<option value=${String(a.id)}>
                  #${a.id} ${a.label ? `(${a.label})` : ''} — [${a.health_status}]
                </option>`,
              )}
            </select>
          </div>

          <!-- Model Target: Textual Search + Filtered Dropdown -->
          <div class="field">
            <div style="display: flex; justify-content: space-between; align-items: center; margin-bottom: 4px;">
               <label class="field-label" style="margin-bottom: 0;">${t('playground.sidebar.model_target')}</label>
              ${st.selectedProviderId
                ? html`<button
                    class="btn-text-action"
                    @click=${async () => {
                      try {
                        showToast(`Discovering models for ${st.selectedProviderId}...`, 'info');
                        await api(`/providers/${encodeURIComponent(st.selectedProviderId)}/refresh`, { method: 'POST' });
                        state.models = (await api('/models')) as Model[];
                        state.modelsComplete = true;
                        ensureDefaultModel(st);
                        showToast(`Models refreshed!`, 'info');
                        requestUpdate();
                      } catch (err) {
                        showToast(String(err), 'error');
                      }
                    }}
                  >
                     ${t('playground.sidebar.refresh')}
                  </button>`
                : html``}
            </div>

            <!-- Real-time Textual Search Input -->
            <div style="position: relative; margin-bottom: 6px;">
              <input
                type="text"
                 placeholder=${t('playground.sidebar.model_search')}
                .value=${st.modelSearchQuery}
                @input=${(e: Event) => {
                  st.modelSearchQuery = (e.target as HTMLInputElement).value;
                  if (st.modelSearchQuery.trim()) {
                    st.selectedModelId = st.modelSearchQuery.trim();
                  }
                  requestUpdate();
                }}
              />
              ${st.modelSearchQuery
                ? html`<button
                    type="button"
                    style="position: absolute; right: 8px; top: 50%; transform: translateY(-50%); background: none; border: none; font-size: 0.8rem; cursor: pointer; color: var(--color-text-muted);"
                    @click=${() => {
                      st.modelSearchQuery = '';
                      requestUpdate();
                    }}
                    title="Clear search"
                  >
                    ✕
                  </button>`
                : html``}
            </div>

            <!-- Model Dropdown (Filtered by textual search query) -->
            ${(() => {
              const displayedModels = st.modelSearchQuery.trim()
                ? filteredModels.filter(
                    (m) =>
                      m.id.toLowerCase().includes(st.modelSearchQuery.trim().toLowerCase()) ||
                      m.name.toLowerCase().includes(st.modelSearchQuery.trim().toLowerCase()) ||
                      (m.provider || '').toLowerCase().includes(st.modelSearchQuery.trim().toLowerCase()),
                  )
                : filteredModels;

              return html`
                <select
                  .value=${st.selectedModelId}
                  @change=${(e: Event) => {
                    st.selectedModelId = (e.target as HTMLSelectElement).value;
                    st.modelSearchQuery = '';
                    requestUpdate();
                  }}
                >
                  ${displayedModels.length === 0
                    ? html`<option value=${st.selectedModelId || st.modelSearchQuery}>
                         ${st.selectedModelId ? `Custom: ${st.selectedModelId}` : t('playground.sidebar.no_models')}
                      </option>`
                    : displayedModels.map(
                        (m) =>
                          html`<option value=${m.id}>
                            ${m.provider ? `${m.provider} / ` : ''}${m.name}
                          </option>`,
                      )}
                </select>
              `;
            })()}
          </div>
        </div>
      </div>

      <!-- Hyperparameters & Settings Card -->
      <div class="playground-sidebar-card">
        <div class="playground-sidebar-header">
           <h4>${t('playground.sidebar.parameters')}</h4>
        </div>

        <div class="playground-sidebar-body">
          ${st.modality === 'chat'
            ? renderChatHyperparams(st)
            : st.modality === 'image'
            ? renderImageHyperparams(st)
            : st.modality === 'embedding'
            ? renderEmbeddingHyperparams(st)
            : renderAudioHyperparams(st)}
        </div>
      </div>
    </div>
  `;
}