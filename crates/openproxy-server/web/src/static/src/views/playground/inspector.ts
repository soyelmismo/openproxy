// views/playground/inspector.ts — Response inspector panel + sidebar.
//
// Renders the tabbed response inspector (formatted / raw JSON / headers /
// stream chunks), metrics bar, formatted response per modality, and the
// right inspector sidebar (target/auth selector, hyperparameters card).
//
// All reads/writes go through the shared PlaygroundState reference.

import { html, type TemplateResult } from 'lit-html';
import { unsafeHTML } from 'lit-html/directives/unsafe-html.js';
import { state } from '../../state/index.js';
import { showToast } from '../../components/toast.js';
import { requestUpdate } from '../../state/reactive.js';
import { api } from '../../state/api.js';
import { icons } from '../../lib/icons.js';
import type { Model, Provider, Account } from '../../lib/types/api.js';
import type { PlaygroundState } from './shared.js';
import {
  inferModelTypeFrontend,
  extractThinkingProcess,
  copyText,
} from './shared.js';
import { renderMarkdownAndMath } from '../../lib/markdown.js';
import { generateCurlCommand } from './curl.js';
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

// =========================================================================
// Metrics Bar
// =========================================================================

export function renderMetricsBar(st: PlaygroundState): TemplateResult {
  const status = st.currentMetrics.statusCode;
  const isOk = status !== null && status >= 200 && status < 300;
  const isErr = (status !== null && status >= 400) || st.responseError !== null;

  let tokensPerSec: string | null = null;
  if (st.currentMetrics.completionTokens && st.currentMetrics.totalLatencyMs && st.currentMetrics.totalLatencyMs > 0) {
    const elapsedSec = (st.currentMetrics.totalLatencyMs - (st.currentMetrics.ttftMs || 0)) / 1000;
    if (elapsedSec > 0.05) {
      tokensPerSec = `${(st.currentMetrics.completionTokens / elapsedSec).toFixed(1)} t/s`;
    }
  }

  return html`
    <div class="playground-metrics-bar">
      <div class="playground-metric">
        <span class="metric-label">Status</span>
        <span class="status-pill ${isOk ? 'on' : isErr ? 'off' : ''}">
          ${status ? `${status} ${st.currentMetrics.statusText || ''}` : st.responseError ? 'Error' : '—'}
        </span>
      </div>

      <div class="playground-metric">
        <span class="metric-label">Latency</span>
        <span class="metric-value">${st.currentMetrics.totalLatencyMs !== null ? `${st.currentMetrics.totalLatencyMs} ms` : '—'}</span>
      </div>

      ${st.currentMetrics.ttftMs !== null
        ? html`
            <div class="playground-metric">
              <span class="metric-label">TTFT</span>
              <span class="metric-value">${st.currentMetrics.ttftMs} ms</span>
            </div>
          `
        : html``}

      ${tokensPerSec !== null
        ? html`
            <div class="playground-metric">
              <span class="metric-label">Speed</span>
              <span class="metric-value">${tokensPerSec}</span>
            </div>
          `
        : html``}

      ${st.currentMetrics.totalTokens !== null
        ? html`
            <div class="playground-metric">
              <span class="metric-label">Tokens</span>
              <span class="metric-value">
                ${st.currentMetrics.totalTokens}
                ${st.currentMetrics.promptTokens !== null ? html`<small class="text-muted">(${st.currentMetrics.promptTokens}p / ${st.currentMetrics.completionTokens || 0}c)</small>` : html``}
              </span>
            </div>
          `
        : st.currentMetrics.payloadSizeBytes !== null
        ? html`
            <div class="playground-metric">
              <span class="metric-label">Size</span>
              <span class="metric-value">${Math.round(st.currentMetrics.payloadSizeBytes / 10.24) / 100} KB</span>
            </div>
          `
        : html``}
    </div>
  `;
}

// =========================================================================
// Formatted Response (per modality)
// =========================================================================

export function renderFormattedResponse(st: PlaygroundState): TemplateResult {
  if (st.responseError) {
    const errorDetails = (typeof st.parsedResponseJson === 'object' && st.parsedResponseJson !== null)
      ? st.parsedResponseJson
      : null;
    const curl = generateCurlCommand(st);

    return html`
      <div class="banner banner-error" style="display: flex; flex-direction: column; gap: var(--space-3); padding: var(--space-3); border-radius: var(--radius-md);">
        <div style="display: flex; align-items: center; justify-content: space-between; gap: var(--space-2);">
          <h4 style="margin: 0; color: var(--color-error); display: flex; align-items: center; gap: var(--space-2);">
            <span>⚠️ Request Failed</span>
            ${st.currentMetrics.statusCode
              ? html`<span class="badge" style="background: var(--color-error); color: #fff;">HTTP ${st.currentMetrics.statusCode}</span>`
              : html``}
          </h4>
          <button class="button small" @click=${() => { st.activeResponseTab = 'raw'; requestUpdate(); }}>
            View Raw Error Body →
          </button>
        </div>

        <p style="margin: 0; font-family: var(--font-mono); font-size: var(--fs-sm); word-break: break-word;">${st.responseError}</p>

        ${errorDetails
          ? html`
              <div style="padding: var(--space-2); background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-sm);">
                <div style="font-weight: 600; font-size: var(--fs-xs); margin-bottom: var(--space-1); color: var(--color-text-muted);">
                  Upstream / Server Error Object:
                </div>
                <pre class="playground-code-view" style="margin: 0; max-height: 180px; font-size: var(--fs-xs);"><code>${JSON.stringify(errorDetails, null, 2)}</code></pre>
              </div>
            `
          : html``}

        <div style="padding: var(--space-2); background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-sm);">
          <div style="display: flex; align-items: center; justify-content: space-between; margin-bottom: var(--space-1);">
            <span style="font-weight: 600; font-size: var(--fs-xs); color: var(--color-text-muted);">Sent Request (Payload & Command):</span>
            <button class="button small" @click=${() => copyText(curl, 'cURL command')}>Copy cURL</button>
          </div>
          <pre class="playground-code-view" style="margin: 0; max-height: 180px; font-size: var(--fs-xs);"><code>${curl}</code></pre>
        </div>
      </div>
    `;
  }

  if (st.modality === 'chat') {
    const { reasoning, content, isThinking } = extractThinkingProcess(
      st.streamedReasoningContent,
      st.streamedChatContent,
      st.isLoading,
    );

    if (!reasoning && !content && !st.isLoading) {
      return html`<div class="playground-empty-response">Ready to send. Click "Run" or press Ctrl+Enter to test chat completion.</div>`;
    }

    return html`
      <div class="playground-chat-response">
        ${reasoning
          ? html`
              <div class="playground-reasoning-card ${st.reasoningExpanded ? 'expanded' : 'collapsed'}">
                <div
                  class="playground-reasoning-header"
                  @click=${() => {
                    st.reasoningExpanded = !st.reasoningExpanded;
                    requestUpdate();
                  }}
                  title="${st.reasoningExpanded ? 'Click to collapse reasoning' : 'Click to expand reasoning'}"
                >
                  <div class="reasoning-title-group">
                    <span class="reasoning-icon">${icons.embedding()}</span>
                    <span class="reasoning-title">Thinking Process</span>
                    ${isThinking
                      ? html`
                          <span class="thinking-pulse-dot" title="Thinking in progress…"></span>
                          <span class="thinking-status-text">Thinking…</span>
                        `
                      : html`<span class="thinking-done-badge">Completed</span>`}
                  </div>
                  <button class="reasoning-toggle-btn" type="button">
                    ${st.reasoningExpanded ? 'Hide' : 'Show'}
                    <span class="reasoning-chevron">${st.reasoningExpanded ? icons.caretUp() : icons.caretDown()}</span>
                  </button>
                </div>
                ${st.reasoningExpanded
                  ? html`
                      <div class="playground-reasoning-body md-formatted-content">
                        ${unsafeHTML(renderMarkdownAndMath(reasoning))}
                      </div>
                    `
                  : null}
              </div>
            `
          : null}

        <div class="playground-chat-bubble assistant">
          <div class="bubble-header">
            <span class="role-badge">Assistant</span>
            ${st.isLoading && !isThinking
              ? html`<span class="badge badge-info"><span class="pulse-dot"></span> Generating…</span>`
              : (st.isLoading && isThinking
                  ? html`<span class="badge badge-info"><span class="pulse-dot"></span> Reasoning…</span>`
                  : html``)}
          </div>
          <div class="bubble-content md-formatted-content">
            ${content
              ? unsafeHTML(renderMarkdownAndMath(content))
              : (st.isLoading
                  ? html`<span class="bubble-placeholder-text">Drafting answer…</span>`
                  : html``)}
          </div>
        </div>
      </div>
    `;
  }

  if (st.modality === 'image') {
    const data = (st.parsedResponseJson as { data?: Array<{ url?: string; b64_json?: string; revised_prompt?: string }> })?.data;
    if (!data || data.length === 0) {
      return html`<div class="playground-empty-response">No images generated yet. Configure prompt and click "Run".</div>`;
    }

    return html`
      <div class="playground-image-gallery">
        ${data.map((item, idx) => {
          const imgSrc = item.b64_json ? `data:image/png;base64,${item.b64_json}` : (item.url || '');
          const isB64 = Boolean(item.b64_json);
          return html`
            <div class="playground-image-card">
              <div class="img-preview-wrap" @click=${() => { st.lightboxImageUrl = imgSrc; requestUpdate(); }}>
                <img src=${imgSrc} alt="Generated image ${idx + 1}" loading="lazy" />
                <span class="zoom-hint">${icons.search()} Click to zoom</span>
              </div>
              ${item.revised_prompt
                ? html`<p class="image-revised-prompt"><small>${item.revised_prompt}</small></p>`
                : html``}
              <div class="image-actions">
                <button
                  class="button small"
                  @click=${() => sendImageToInpainting(st, item.b64_json || item.url || '', isB64)}
                  title="Load image into Inpainting / Edit mode"
                >
                  Send to Inpaint
                </button>
                <button
                  class="button small"
                  @click=${() => copyText(item.b64_json ? item.b64_json : (item.url || ''), 'Image data')}
                  title="Copy base64 / URL"
                >
                  ${icons.copy()} Copy Data
                </button>
                <a class="button small primary" href=${imgSrc} download="generated-image-${idx + 1}.png" target="_blank">
                  ${icons.export()} Download PNG
                </a>
              </div>
            </div>
          `;
        })}
      </div>
    `;
  }

  if (st.modality === 'embedding') {
    const data = (st.parsedResponseJson as { data?: Array<{ embedding?: number[]; index?: number }> })?.data;
    if (!data || data.length === 0) {
      return html`<div class="playground-empty-response">No embeddings generated yet. Enter input text and click "Run".</div>`;
    }

    return html`
      <div class="playground-embedding-results">
        ${data.map((emb, idx) => {
          const vector = emb.embedding || [];
          const preview = vector.slice(0, 8);
          return html`
            <div class="playground-vector-card">
              <h4>Embedding #${idx + 1} (${vector.length} dimensions)</h4>
              <div class="playground-vector-preview">
                [${preview.map((v) => v.toFixed(6)).join(', ')}${vector.length > 8 ? ', …' : ''}]
              </div>
              <div class="playground-vector-bars">
                ${vector.slice(0, 80).map((v) => {
                  const heightPct = Math.min(100, Math.max(5, Math.abs(v) * 500));
                  const isPos = v >= 0;
                  return html`<div class="vector-bar ${isPos ? 'pos' : 'neg'}" style="height: ${heightPct}%;" title="${v}"></div>`;
                })}
              </div>
            </div>
          `;
        })}
      </div>
    `;
  }

  if (st.modality === 'audio') {
    if (!st.rawResponseText && !st.parsedResponseJson) {
      return html`<div class="playground-empty-response">No transcription output yet. Select audio file and click "Run".</div>`;
    }

    const transcribedText = typeof st.parsedResponseJson === 'object' && st.parsedResponseJson !== null && 'text' in st.parsedResponseJson
      ? (st.parsedResponseJson as { text: string }).text
      : st.rawResponseText;

    return html`
      <div class="playground-audio-transcription">
        <div class="transcription-box">
          <div style="display: flex; justify-content: space-between; align-items: center; margin-bottom: var(--space-2);">
            <h4 style="margin: 0;">Transcribed Text</h4>
            <button class="button small" @click=${() => copyText(transcribedText, 'Transcription')}>
              Copy Text
            </button>
          </div>
          <p style="white-space: pre-wrap; font-size: var(--fs-md); line-height: 1.6; margin: 0;">${transcribedText}</p>
        </div>
      </div>
    `;
  }

  return html`<div class="playground-empty-response">No response to display.</div>`;
}

// =========================================================================
// Image → Inpainting transfer
// =========================================================================

async function sendImageToInpainting(st: PlaygroundState, b64OrUrl: string, isBase64: boolean): Promise<void> {
  try {
    let file: File;
    if (isBase64) {
      const cleanB64 = b64OrUrl.replace(/^data:image\/\w+;base64,/, '');
      const byteCharacters = atob(cleanB64);
      const byteNumbers = new Array(byteCharacters.length);
      for (let i = 0; i < byteCharacters.length; i++) {
        byteNumbers[i] = byteCharacters.charCodeAt(i);
      }
      const byteArray = new Uint8Array(byteNumbers);
      const blob = new Blob([byteArray], { type: 'image/png' });
      file = new File([blob], `inpaint-${Date.now()}.png`, { type: 'image/png' });
    } else {
      const res = await fetch(b64OrUrl);
      const blob = await res.blob();
      file = new File([blob], `inpaint-${Date.now()}.png`, { type: blob.type || 'image/png' });
    }
    st.imageSourceFile = file;
    st.imageMode = 'edit';
    showToast('Image loaded into Inpainting / Edit mode!', 'success');
    requestUpdate();
  } catch (err) {
    showToast(`Failed to load image for inpainting: ${err}`, 'error');
  }
}

// =========================================================================
// Response Inspector (tabbed panel embedded inside workspaces)
// =========================================================================

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
            Formatted
          </button>
          <button
            class="detail-tab ${st.activeResponseTab === 'raw' ? 'active' : ''}"
            @click=${() => {
              st.activeResponseTab = 'raw';
              requestUpdate();
            }}
          >
            Raw JSON
          </button>
          <button
            class="detail-tab ${st.activeResponseTab === 'headers' ? 'active' : ''}"
            @click=${() => {
              st.activeResponseTab = 'headers';
              requestUpdate();
            }}
          >
            Headers
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
                  Stream (${st.streamChunks.length})
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
            title="Copy response body"
          >
            Copy
          </button>
        </div>
      </div>

      ${renderMetricsBar(st)}

      <div class="playground-response-body">
        ${st.activeResponseTab === 'formatted'
          ? renderFormattedResponse(st)
          : st.activeResponseTab === 'raw'
          ? html`
              <pre class="playground-code-view"><code>${st.parsedResponseJson ? JSON.stringify(st.parsedResponseJson, null, 2) : (st.rawResponseText || 'No response received yet.')}</code></pre>
            `
          : st.activeResponseTab === 'headers'
          ? html`
              <table class="playground-headers-table">
                <thead><tr><th>Header</th><th>Value</th></tr></thead>
                <tbody>
                  ${Object.keys(st.responseHeaders).length === 0
                    ? html`<tr><td colspan="2" class="text-muted">No headers available</td></tr>`
                    : Object.entries(st.responseHeaders).map(
                        ([k, v]) => html`<tr><td><code>${k}</code></td><td>${v}</td></tr>`,
                      )}
                </tbody>
              </table>
            `
          : html`
              <div class="playground-stream-log">
                ${st.streamChunks.length === 0
                  ? html`<p class="text-muted" style="padding: var(--space-4); text-align: center;">No stream chunks captured yet.</p>`
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

// =========================================================================
// Inspector Sidebar (right panel: target/auth + hyperparams)
// =========================================================================

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
          <h4>Target & Authentication</h4>
          <span class="badge badge-info">${st.modality.toUpperCase()}</span>
        </div>

        <div class="playground-sidebar-body">
          <!-- API Key Source -->
          <div class="field">
            <label class="field-label">API Key Auth</label>
            <select
              .value=${st.keySource}
              @change=${(e: Event) => {
                st.keySource = (e.target as HTMLSelectElement).value as typeof st.keySource;
                requestUpdate();
              }}
            >
              <option value="session">Admin Session Token</option>
              ${apiKeys.map(
                (k) => html`<option value="key:${k.key_prefix}">${k.label || 'API Key'} (${k.key_prefix}…)</option>`,
              )}
              <option value="custom">Custom Bearer Key</option>
            </select>
          </div>

          ${st.keySource === 'custom'
            ? html`
                <div class="field">
                  <label class="field-label">Custom Bearer Key</label>
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
            <label class="field-label">Provider</label>
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
              <option value="">(Auto / Any Provider)</option>
              ${providers.map((p) => html`<option value=${p.id}>${p.name || p.id}</option>`)}
            </select>
          </div>

          <!-- Account Dropdown Select -->
          <div class="field">
            <label class="field-label">Account Routing</label>
            <select
              .value=${st.selectedAccountId}
              @change=${(e: Event) => {
                st.selectedAccountId = (e.target as HTMLSelectElement).value;
                requestUpdate();
              }}
            >
              <option value="">(Auto / Priority Routing)</option>
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
              <label class="field-label" style="margin-bottom: 0;">Model Target</label>
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
                    ↻ Refresh
                  </button>`
                : html``}
            </div>

            <!-- Real-time Textual Search Input -->
            <div style="position: relative; margin-bottom: 6px;">
              <input
                type="text"
                placeholder="Search or type custom model..."
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
                        ${st.selectedModelId ? `Custom: ${st.selectedModelId}` : 'No matching models (using input)'}
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
          <h4>Parameters</h4>
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
