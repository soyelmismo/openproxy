// views/playground/studio-header.ts — Playground studio header.
//
// Renders the top bar: status badge (idle / generating / ready / error),
// segmented modality selector, and quick actions (run/stop, copy cURL, clear).

import { html, type TemplateResult } from 'lit-html';
import { requestUpdate } from '../../state/reactive.js';
import { showToast } from '../../components/toast.js';
import { icons } from '../../lib/icons.js';
import { copyToClipboard } from '../../lib/clipboard.js';
import { t } from '../../i18n/index.js';
import type { ModalityType, PlaygroundState } from './shared.js';
import { generateId } from './shared.js';
import { generateCurlCommand } from './curl.js';

function resetExecutionState(st: PlaygroundState): void {
  st.rawResponseText = '';
  st.parsedResponseJson = null;
  st.responseError = null;
  st.streamChunks = [];
  st.currentMetrics = {
    statusCode: null,
    statusText: null,
    ttftMs: null,
    totalLatencyMs: null,
    promptTokens: null,
    completionTokens: null,
    totalTokens: null,
    payloadSizeBytes: null,
  };
}

export function clearPlayground(st: PlaygroundState): void {
  const modality = st.modality;
  if (modality === 'chat') {
    st.chatMessages = [{ id: generateId(), role: 'user', content: '' }];
    st.composerContent = '';
    st.streamedChatContent = '';
    st.streamedReasoningContent = '';
  } else if (modality === 'image') {
    st.imagePrompt = '';
    st.imageNegativePrompt = '';
    st.imageSourceFile = null;
    st.imageMaskFile = null;
  } else if (modality === 'embedding') {
    st.embeddingInput = '';
  } else if (modality === 'audio') {
    st.audioFile = null;
    st.audioPrompt = '';
  }
  resetExecutionState(st);
  requestUpdate();
  showToast(t('playground.header.cleared'), 'info');
}

function copyTextToClipboard(text: string, label = 'Content'): void {
  copyToClipboard(text)
    .then(() => showToast(`${label} copied to clipboard!`, 'info'))
    .catch(() => showToast('Copy failed — please copy manually', 'error'));
}

function renderStatusBadge(st: PlaygroundState): TemplateResult {
  const status = st.currentMetrics.statusCode;
  const isOk = status !== null && status >= 200 && status < 300;
  const isErr = (status !== null && status >= 400) || st.responseError !== null;

  if (st.isLoading) {
    return html`<span class="playground-live-badge live-generating"><span class="pulse-dot"></span> ${t('playground.status.generating')}</span>`;
  }
  if (isOk) {
    return html`<span class="playground-live-badge live-done"><span class="status-dot"></span> ${t('playground.status.ready', { ms: st.currentMetrics.totalLatencyMs || 0 })}</span>`;
  }
  if (isErr) {
    return html`<span class="playground-live-badge live-error">${icons.warning()} ${status ? `HTTP ${status}` : t('playground.metrics.error')}</span>`;
  }
  return html`<span class="playground-live-badge live-idle"><span class="status-dot"></span> ${t('playground.status.idle')}</span>`;
}

interface StudioHeaderCallbacks {
  executeRequest: () => Promise<void>;
  ensureDefaultModel: (st: PlaygroundState) => void;
}

export function renderStudioHeader(st: PlaygroundState, callbacks: StudioHeaderCallbacks): TemplateResult {
  const { executeRequest, ensureDefaultModel } = callbacks;
  const cancelRequest = (): void => {
    st.abortController?.abort();
  };
  const copyCurlToClipboard = (): void => {
    copyTextToClipboard(generateCurlCommand(st), 'cURL command');
  };
  const switchModality = (modality: ModalityType): void => {
    st.modality = modality;
    st.selectedModelId = '';
    ensureDefaultModel(st);
    requestUpdate();
  };

  return html`
    <div class="playground-studio-header">
      <div class="playground-studio-title-area">
        <div class="playground-title-row">
          <h2 class="playground-studio-title">${t('playground.title')}</h2>
          ${renderStatusBadge(st)}
        </div>
      </div>

      <!-- Segmented Modality Selector -->
      <div class="playground-segmented-control" role="tablist">
        <button
          class="segmented-item ${st.modality === 'chat' ? 'active' : ''}"
          @click=${() => switchModality('chat')}
        >
          <span class="seg-icon">${icons.chat()}</span> ${t('playground.modality.chat')}
        </button>
        <button
          class="segmented-item ${st.modality === 'image' ? 'active' : ''}"
          @click=${() => switchModality('image')}
        >
          <span class="seg-icon">${icons.image()}</span> ${t('playground.modality.image')}
        </button>
        <button
          class="segmented-item ${st.modality === 'embedding' ? 'active' : ''}"
          @click=${() => switchModality('embedding')}
        >
          <span class="seg-icon">${icons.embedding()}</span> ${t('playground.modality.embedding')}
        </button>
        <button
          class="segmented-item ${st.modality === 'audio' ? 'active' : ''}"
          @click=${() => switchModality('audio')}
        >
          <span class="seg-icon">${icons.audio()}</span> ${t('playground.modality.audio')}
        </button>
      </div>

      <!-- Quick Action Bar -->
      <div class="playground-studio-actions">
        ${st.isLoading
          ? html`<button class="playground-run-btn btn-danger" @click=${cancelRequest}>
              ${icons.pause()} ${t('playground.header.stop')}
            </button>`
          : html`<button class="playground-run-btn btn-primary" @click=${() => void executeRequest()} title="Execute Request (Ctrl+Enter)">
              ${icons.play()} ${t('playground.header.run')} <kbd class="playground-kbd">Ctrl+↵</kbd>
            </button>`}
        <button class="playground-action-btn" @click=${copyCurlToClipboard} title="Copy as cURL command">
          ${icons.copy()} ${t('playground.header.copy_curl')}
        </button>
        <button class="playground-action-btn text-muted" @click=${() => clearPlayground(st)} title="Clear conversation or inputs">
          ${icons.trash()} ${t('playground.header.clear')}
        </button>
      </div>
    </div>
  `;
}