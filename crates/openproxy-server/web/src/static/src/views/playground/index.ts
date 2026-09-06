// views/playground/index.ts — Playground orchestrator & dispatcher.
//
// Owns the single PlaygroundState instance and all cross-cutting lifecycle:
//   - `mountPlayground()` (public entry point, imported by router.ts)
//   - `renderPlayground()` (top-level template + per-modality dispatch)
//   - Studio header (status badge, modality selector, quick actions)
//   - Request execution dispatcher + image/embedding/audio executors
//   - Keyboard shortcut handler + mount/unmount wiring
//
// Renders workspaces from chat.ts / image.ts / inspector.ts and passes the
// shared mutable PlaygroundState to each.

import { html, type TemplateResult } from 'lit-html';
import { state } from '../../state/index.js';
import { api } from '../../state/api.js';
import { getToken } from '../../state/auth.js';
import { requestUpdate } from '../../state/reactive.js';
import { createView } from '../../lib/view-utils.js';
import { showToast } from '../../components/toast.js';
import { icons } from '../../lib/icons.js';
import { copyToClipboard } from '../../lib/clipboard.js';
import type { Model, Provider, Account, Combo } from '../../lib/types/api.js';
import type { ModalityType, PlaygroundState } from './shared.js';
import {
  createInitialPlaygroundState,
  bindPlaygroundState,
  generateId,
} from './shared.js';
import { getEffectiveApiKeyFromState, generateCurlCommand } from './curl.js';
import { renderChatWorkspace, executeChatRequest } from './chat.js';
import { renderImageStudioWorkspace } from './image.js';
import {
  renderResponseInspector,
  renderInspectorSidebar,
  ensureDefaultModel,
} from './inspector.js';

export type { ModalityType } from './shared.js';
export type { ResponseTab, ChatMessage, RequestMetrics, StreamChunkItem } from './shared.js';

// =========================================================================
// Shared module-local state (mutable, passed by reference to sub-modules)
// =========================================================================

let stateInternal: PlaygroundState = createInitialPlaygroundState();
bindPlaygroundState(stateInternal);

// The router imports `mountPlayground`; expose the state shape for tests.
export { stateInternal as playgroundState };

// =========================================================================
// Helpers
// =========================================================================

function resetExecutionState(): void {
  stateInternal.rawResponseText = '';
  stateInternal.parsedResponseJson = null;
  stateInternal.responseError = null;
  stateInternal.streamChunks = [];
  stateInternal.currentMetrics = {
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

function clearPlayground(): void {
  const modality = stateInternal.modality;
  if (modality === 'chat') {
    stateInternal.chatMessages = [{ id: generateId(), role: 'user', content: '' }];
    stateInternal.composerContent = '';
    stateInternal.streamedChatContent = '';
    stateInternal.streamedReasoningContent = '';
  } else if (modality === 'image') {
    stateInternal.imagePrompt = '';
    stateInternal.imageNegativePrompt = '';
    stateInternal.imageSourceFile = null;
    stateInternal.imageMaskFile = null;
  } else if (modality === 'embedding') {
    stateInternal.embeddingInput = '';
  } else if (modality === 'audio') {
    stateInternal.audioFile = null;
    stateInternal.audioPrompt = '';
  }
  resetExecutionState();
  requestUpdate();
  showToast('Playground cleared', 'info');
}

// =========================================================================
// Request dispatcher
// =========================================================================

async function executeRequest(): Promise<void> {
  if (stateInternal.isLoading) return;

  const key = getEffectiveApiKeyFromState(stateInternal);
  if (!key && !getToken()) {
    showToast('Please provide an API Key or log in to send requests', 'error');
    return;
  }

  ensureDefaultModel(stateInternal);
  const effectiveModel = stateInternal.selectedModelId || stateInternal.customModelInput.trim();
  if (!effectiveModel) {
    showToast('Please select or enter a Model Target', 'error');
    return;
  }

  // If composer has text in chat mode, commit it as a new user message before executing
  if (stateInternal.modality === 'chat' && stateInternal.composerContent.trim().length > 0) {
    stateInternal.chatMessages.push({
      id: generateId(),
      role: 'user',
      content: stateInternal.composerContent.trim(),
    });
    stateInternal.composerContent = '';
  }

  stateInternal.isLoading = true;
  stateInternal.responseError = null;
  stateInternal.rawResponseText = '';
  stateInternal.parsedResponseJson = null;
  stateInternal.streamChunks = [];
  stateInternal.streamedChatContent = '';
  stateInternal.streamedReasoningContent = '';
  stateInternal.responseHeaders = {};
  stateInternal.currentMetrics = {
    statusCode: null,
    statusText: null,
    ttftMs: null,
    totalLatencyMs: null,
    promptTokens: null,
    completionTokens: null,
    totalTokens: null,
    payloadSizeBytes: null,
  };
  requestUpdate();

  stateInternal.abortController = new AbortController();
  const startTime = performance.now();

  try {
    if (stateInternal.modality === 'chat') {
      await executeChatRequest(stateInternal, key, effectiveModel, startTime);
    } else if (stateInternal.modality === 'image') {
      await executeImageRequest(key, effectiveModel);
    } else if (stateInternal.modality === 'embedding') {
      await executeEmbeddingRequest(key, effectiveModel);
    } else if (stateInternal.modality === 'audio') {
      await executeAudioRequest(key, effectiveModel);
    }
  } catch (err: unknown) {
    if (stateInternal.abortController?.signal.aborted) {
      stateInternal.responseError = 'Request stopped by user.';
    } else {
      const msg = err instanceof Error ? err.message : String(err);
      stateInternal.responseError = msg;
      showToast(`Error: ${msg}`, 'error');
    }
  } finally {
    stateInternal.currentMetrics.totalLatencyMs = Math.round(performance.now() - startTime);
    stateInternal.isLoading = false;
    stateInternal.abortController = null;
    requestUpdate();
  }
}

async function executeImageRequest(key: string, model: string): Promise<void> {
  let endpoint = '/v1/images/generations';
  let reqInit: RequestInit;

  const headers: Record<string, string> = {};
  if (key) headers['Authorization'] = `Bearer ${key}`;
  if (stateInternal.selectedAccountId) headers['x-openproxy-account'] = stateInternal.selectedAccountId;

  if (stateInternal.imageMode === 'generation') {
    if (!stateInternal.imagePrompt.trim()) {
      throw new Error('Please enter a prompt for image generation.');
    }
    const payload: Record<string, unknown> = {
      model: model || 'dall-e-3',
      prompt: stateInternal.imagePrompt.trim(),
      n: stateInternal.imageN,
      size: stateInternal.imageSize,
      quality: stateInternal.imageQuality,
      response_format: stateInternal.imageResponseFormat,
    };
    if (stateInternal.imageNegativePrompt.trim()) {
      payload['negative_prompt'] = stateInternal.imageNegativePrompt.trim();
    }
    if (stateInternal.imageSeed !== null && !isNaN(stateInternal.imageSeed)) {
      payload['seed'] = stateInternal.imageSeed;
    }
    if (stateInternal.imageAspectRatio) {
      payload['aspect_ratio'] = stateInternal.imageAspectRatio;
    }
    if (stateInternal.imagePostProcessing.length > 0) {
      payload['post_processing'] = stateInternal.imagePostProcessing;
    }
    headers['Content-Type'] = 'application/json';
    reqInit = {
      method: 'POST',
      headers,
      body: JSON.stringify(payload),
    };
  } else if (stateInternal.imageMode === 'edit') {
    if (!stateInternal.imageSourceFile) {
      throw new Error('Please select a source image file to edit.');
    }
    if (!stateInternal.imagePrompt.trim()) {
      throw new Error('Please enter a prompt describing the edits.');
    }
    endpoint = '/v1/images/edits';
    const formData = new FormData();
    formData.append('image', stateInternal.imageSourceFile, stateInternal.imageSourceFile.name);
    if (stateInternal.imageMaskFile) {
      formData.append('mask', stateInternal.imageMaskFile, stateInternal.imageMaskFile.name);
    }
    formData.append('prompt', stateInternal.imagePrompt.trim());
    formData.append('model', model || 'dall-e-2');
    formData.append('n', String(stateInternal.imageN));
    formData.append('size', stateInternal.imageSize);
    formData.append('quality', stateInternal.imageQuality);
    formData.append('response_format', stateInternal.imageResponseFormat);
    formData.append('denoising_strength', String(stateInternal.imageDenoisingStrength));
    if (stateInternal.imageSourceProcessing) {
      formData.append('source_processing', stateInternal.imageSourceProcessing);
    }
    for (const pp of stateInternal.imagePostProcessing) {
      formData.append('post_processing', pp);
    }
    if (stateInternal.imageNegativePrompt.trim()) {
      formData.append('negative_prompt', stateInternal.imageNegativePrompt.trim());
    }
    if (stateInternal.imageSeed !== null && !isNaN(stateInternal.imageSeed)) {
      formData.append('seed', String(stateInternal.imageSeed));
    }
    reqInit = {
      method: 'POST',
      headers,
      body: formData,
    };
  } else {
    if (!stateInternal.imageSourceFile) {
      throw new Error('Please select a source image file to create variations.');
    }
    endpoint = '/v1/images/variations';
    const formData = new FormData();
    formData.append('image', stateInternal.imageSourceFile, stateInternal.imageSourceFile.name);
    if (stateInternal.imageMaskFile) {
      formData.append('mask', stateInternal.imageMaskFile, stateInternal.imageMaskFile.name);
    }
    if (stateInternal.imagePrompt.trim()) {
      formData.append('prompt', stateInternal.imagePrompt.trim());
    }
    formData.append('model', model || 'dall-e-2');
    formData.append('n', String(stateInternal.imageN));
    formData.append('size', stateInternal.imageSize);
    formData.append('quality', stateInternal.imageQuality);
    formData.append('response_format', stateInternal.imageResponseFormat);
    formData.append('denoising_strength', String(stateInternal.imageDenoisingStrength));
    if (stateInternal.imageSourceProcessing) {
      formData.append('source_processing', stateInternal.imageSourceProcessing);
    }
    for (const pp of stateInternal.imagePostProcessing) {
      formData.append('post_processing', pp);
    }
    if (stateInternal.imageNegativePrompt.trim()) {
      formData.append('negative_prompt', stateInternal.imageNegativePrompt.trim());
    }
    if (stateInternal.imageSeed !== null && !isNaN(stateInternal.imageSeed)) {
      formData.append('seed', String(stateInternal.imageSeed));
    }
    reqInit = {
      method: 'POST',
      headers,
      body: formData,
    };
  }

  if (stateInternal.abortController) {
    reqInit.signal = stateInternal.abortController.signal;
  }

  const response = await fetch(endpoint, reqInit);

  stateInternal.currentMetrics.statusCode = response.status;
  stateInternal.currentMetrics.statusText = response.statusText;
  response.headers.forEach((v, k) => {
    stateInternal.responseHeaders[k] = v;
  });

  const text = await response.text();
  stateInternal.rawResponseText = text;
  stateInternal.currentMetrics.payloadSizeBytes = new Blob([text]).size;

  try {
    stateInternal.parsedResponseJson = JSON.parse(text);
  } catch {
    stateInternal.parsedResponseJson = null;
  }

  if (!response.ok) {
    throw new Error(`HTTP ${response.status}: ${text}`);
  }
}

async function executeEmbeddingRequest(key: string, model: string): Promise<void> {
  if (!stateInternal.embeddingInput.trim()) {
    throw new Error('Please enter text to generate embeddings for.');
  }

  let inputData: string | string[] = stateInternal.embeddingInput.trim();
  if (stateInternal.embeddingIsArray) {
    inputData = stateInternal.embeddingInput
      .split('\n')
      .map((s) => s.trim())
      .filter((s) => s.length > 0);
  }

  const payload: Record<string, unknown> = {
    model: model || 'text-embedding-3-small',
    input: inputData,
    encoding_format: stateInternal.embeddingEncodingFormat,
  };
  if (stateInternal.embeddingDimensions !== null && stateInternal.embeddingDimensions > 0) {
    payload['dimensions'] = stateInternal.embeddingDimensions;
  }

  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
  };
  if (key) headers['Authorization'] = `Bearer ${key}`;
  if (stateInternal.selectedAccountId) headers['x-openproxy-account'] = stateInternal.selectedAccountId;

  const reqInit: RequestInit = {
    method: 'POST',
    headers,
    body: JSON.stringify(payload),
  };
  if (stateInternal.abortController) {
    reqInit.signal = stateInternal.abortController.signal;
  }

  const response = await fetch('/v1/embeddings', reqInit);

  stateInternal.currentMetrics.statusCode = response.status;
  stateInternal.currentMetrics.statusText = response.statusText;
  response.headers.forEach((v, k) => {
    stateInternal.responseHeaders[k] = v;
  });

  const text = await response.text();
  stateInternal.rawResponseText = text;
  stateInternal.currentMetrics.payloadSizeBytes = new Blob([text]).size;

  try {
    const json = JSON.parse(text);
    stateInternal.parsedResponseJson = json;
    if (json?.usage) {
      stateInternal.currentMetrics.promptTokens = json.usage.prompt_tokens ?? null;
      stateInternal.currentMetrics.totalTokens = json.usage.total_tokens ?? null;
    }
  } catch {
    stateInternal.parsedResponseJson = null;
  }

  if (!response.ok) {
    throw new Error(`HTTP ${response.status}: ${text}`);
  }
}

async function executeAudioRequest(key: string, model: string): Promise<void> {
  if (!stateInternal.audioFile) {
    throw new Error('Please select an audio file to transcribe.');
  }

  const formData = new FormData();
  formData.append('file', stateInternal.audioFile, stateInternal.audioFile.name);
  formData.append('model', model || 'whisper-1');
  if (stateInternal.audioPrompt.trim()) formData.append('prompt', stateInternal.audioPrompt.trim());
  if (stateInternal.audioLanguage.trim()) formData.append('language', stateInternal.audioLanguage.trim());
  formData.append('temperature', String(stateInternal.audioTemperature));
  formData.append('response_format', stateInternal.audioResponseFormat);

  const headers: Record<string, string> = {};
  if (key) headers['Authorization'] = `Bearer ${key}`;
  if (stateInternal.selectedAccountId) headers['x-openproxy-account'] = stateInternal.selectedAccountId;

  const reqInit: RequestInit = {
    method: 'POST',
    headers,
    body: formData,
  };
  if (stateInternal.abortController) {
    reqInit.signal = stateInternal.abortController.signal;
  }

  const response = await fetch('/v1/audio/transcriptions', reqInit);

  stateInternal.currentMetrics.statusCode = response.status;
  stateInternal.currentMetrics.statusText = response.statusText;
  response.headers.forEach((v, k) => {
    stateInternal.responseHeaders[k] = v;
  });

  const text = await response.text();
  stateInternal.rawResponseText = text;
  stateInternal.currentMetrics.payloadSizeBytes = new Blob([text]).size;

  try {
    stateInternal.parsedResponseJson = JSON.parse(text);
  } catch {
    stateInternal.parsedResponseJson = text;
  }

  if (!response.ok) {
    throw new Error(`HTTP ${response.status}: ${text}`);
  }
}

function cancelRequest(): void {
  if (stateInternal.abortController) {
    stateInternal.abortController.abort();
  }
}

function copyText(text: string, label = 'Content'): void {
  const notify = (): void => {
    showToast(`${label} copied to clipboard!`, 'info');
  };
  copyToClipboard(text)
    .then(notify)
    .catch(() => {
      showToast('Copy failed — please copy manually', 'error');
    });
}

function copyCurlToClipboard(): void {
  const curl = generateCurlCommand(stateInternal);
  copyText(curl, 'cURL command');
}

// =========================================================================
// Studio Header
// =========================================================================

function renderStudioHeader(): TemplateResult {
  const status = stateInternal.currentMetrics.statusCode;
  const isOk = status !== null && status >= 200 && status < 300;
  const isErr = (status !== null && status >= 400) || stateInternal.responseError !== null;

  const switchModality = (modality: ModalityType): void => {
    stateInternal.modality = modality;
    stateInternal.selectedModelId = '';
    ensureDefaultModel(stateInternal);
    requestUpdate();
  };

  return html`
    <div class="playground-studio-header">
      <div class="playground-studio-title-area">
        <div class="playground-title-row">
          <h2 class="playground-studio-title">Playground</h2>
          ${stateInternal.isLoading
            ? html`<span class="playground-live-badge live-generating"><span class="pulse-dot"></span> Generating…</span>`
            : isOk
            ? html`<span class="playground-live-badge live-done"><span class="status-dot"></span> Ready (${stateInternal.currentMetrics.totalLatencyMs || 0}ms)</span>`
            : isErr
            ? html`<span class="playground-live-badge live-error">${icons.warning()} ${status ? `HTTP ${status}` : 'Error'}</span>`
            : html`<span class="playground-live-badge live-idle"><span class="status-dot"></span> Idle</span>`}
        </div>
      </div>

      <!-- Segmented Modality Selector -->
      <div class="playground-segmented-control" role="tablist">
        <button
          class="segmented-item ${stateInternal.modality === 'chat' ? 'active' : ''}"
          @click=${() => switchModality('chat')}
        >
          <span class="seg-icon">${icons.chat()}</span> Chat
        </button>
        <button
          class="segmented-item ${stateInternal.modality === 'image' ? 'active' : ''}"
          @click=${() => switchModality('image')}
        >
          <span class="seg-icon">${icons.image()}</span> Image Studio
        </button>
        <button
          class="segmented-item ${stateInternal.modality === 'embedding' ? 'active' : ''}"
          @click=${() => switchModality('embedding')}
        >
          <span class="seg-icon">${icons.embedding()}</span> Embeddings
        </button>
        <button
          class="segmented-item ${stateInternal.modality === 'audio' ? 'active' : ''}"
          @click=${() => switchModality('audio')}
        >
          <span class="seg-icon">${icons.audio()}</span> Audio Transcription
        </button>
      </div>

      <!-- Quick Action Bar -->
      <div class="playground-studio-actions">
        ${stateInternal.isLoading
          ? html`<button class="playground-run-btn btn-danger" @click=${cancelRequest}>
              ${icons.pause()} Stop
            </button>`
          : html`<button class="playground-run-btn btn-primary" @click=${() => void executeRequest()} title="Execute Request (Ctrl+Enter)">
              ${icons.play()} Run <kbd class="playground-kbd">Ctrl+↵</kbd>
            </button>`}
        <button class="playground-action-btn" @click=${copyCurlToClipboard} title="Copy as cURL command">
          ${icons.copy()} Copy cURL
        </button>
        <button class="playground-action-btn text-muted" @click=${clearPlayground} title="Clear conversation or inputs">
          ${icons.trash()} Clear
        </button>
      </div>
    </div>
  `;
}

// =========================================================================
// Embedding & Audio workspaces (small, kept in orchestrator)
// =========================================================================

function renderEmbeddingWorkspace(): TemplateResult {
  return html`
    <div class="playground-workspace-column">
      <div class="playground-card">
        <div class="playground-card-header">
          <h3>Embedding Input Vectorizer</h3>
          <button
            class="button small"
            @click=${() => {
              stateInternal.embeddingInput =
                'Vector databases allow semantic similarity search across embeddings.';
              requestUpdate();
            }}
          >
            Sample Text
          </button>
        </div>

        <div class="field">
          <div style="display: flex; justify-content: space-between; align-items: center;">
            <label class="field-label">Input Text</label>
            <label class="playground-switch-label" style="font-size: var(--fs-xs);">
              <input
                type="checkbox"
                ?checked=${stateInternal.embeddingIsArray}
                @change=${(e: Event) => {
                  stateInternal.embeddingIsArray = (e.target as HTMLInputElement).checked;
                  requestUpdate();
                }}
              />
              <span>${stateInternal.embeddingIsArray ? 'Batch mode (newline separated)' : 'Single string'}</span>
            </label>
          </div>
          <textarea
            rows="6"
            placeholder="Enter text strings to vectorize into dense float embeddings…"
            .value=${stateInternal.embeddingInput}
            @input=${(e: Event) => {
              stateInternal.embeddingInput = (e.target as HTMLTextAreaElement).value;
            }}
          ></textarea>
        </div>
      </div>

      ${renderResponseInspector(stateInternal)}
    </div>
  `;
}

function renderAudioWorkspace(): TemplateResult {
  return html`
    <div class="playground-workspace-column">
      <div class="playground-card">
        <div class="playground-card-header">
          <h3>Audio Transcription (Whisper)</h3>
        </div>

        <div class="field">
          <label class="field-label">Upload Audio (.mp3, .wav, .m4a, .ogg, .webm)</label>
          <div class="playground-file-dropzone">
            <input
              type="file"
              accept="audio/*"
              @change=${(e: Event) => {
                const input = e.target as HTMLInputElement;
                if (input.files && input.files[0]) {
                  stateInternal.audioFile = input.files[0];
                  requestUpdate();
                }
              }}
            />
            ${stateInternal.audioFile
              ? html`
                  <div class="playground-file-info">
                    <strong>Selected:</strong> ${stateInternal.audioFile.name} (${Math.round(stateInternal.audioFile.size / 1024)} KB)
                    <audio controls src=${URL.createObjectURL(stateInternal.audioFile)} style="margin-top: var(--space-2); width: 100%;"></audio>
                  </div>
                `
              : html`<p class="text-muted">${icons.audio()} Click or drag an audio file here</p>`}
          </div>
        </div>

        <div class="field" style="margin-top: var(--space-2);">
          <label class="field-label">Prompt Guide / Context Vocabulary (Optional)</label>
          <input
            type="text"
            placeholder="Optional glossary or context to guide transcription…"
            .value=${stateInternal.audioPrompt}
            @input=${(e: Event) => {
              stateInternal.audioPrompt = (e.target as HTMLInputElement).value;
            }}
          />
        </div>
      </div>

      ${renderResponseInspector(stateInternal)}
    </div>
  `;
}

// =========================================================================
// Main Playground View
// =========================================================================

function renderPlayground(): TemplateResult {
  if (stateInternal.loadError) {
    return html`
      <div class="page-header"><h2>Playground</h2></div>
      <div class="banner banner-error">${stateInternal.loadError}</div>
    `;
  }

  let workspace: TemplateResult;
  const modality = stateInternal.modality;
  if (modality === 'chat') {
    workspace = renderChatWorkspace(stateInternal);
  } else if (modality === 'image') {
    workspace = renderImageStudioWorkspace(stateInternal);
  } else if (modality === 'embedding') {
    workspace = renderEmbeddingWorkspace();
  } else {
    workspace = renderAudioWorkspace();
  }

  return html`
    <div class="playground-studio-wrapper">
      ${renderStudioHeader()}

      <div class="playground-studio-grid">
        <div class="playground-main-area">
          ${workspace}
        </div>

        ${renderInspectorSidebar(stateInternal)}
      </div>
    </div>
  `;
}

// =========================================================================
// Global keyboard shortcut: Ctrl+Enter (or Cmd+Enter) runs the request.
// =========================================================================

function handleGlobalKeydown(e: KeyboardEvent): void {
  if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') {
    const active = document.activeElement;
    if (active && (active.tagName === 'TEXTAREA' || active.tagName === 'INPUT' || active.tagName === 'BODY')) {
      e.preventDefault();
      if (!stateInternal.isLoading) {
        void executeRequest();
      }
    }
  }
}

function handleRunShortcut(): void {
  if (!stateInternal.isLoading) {
    void executeRequest();
  }
}

// =========================================================================
// Mount function (public entry point — imported by router.ts)
// =========================================================================

export async function mountPlayground(): Promise<(() => void) | void> {
  stateInternal.loadError = null;
  window.addEventListener('keydown', handleGlobalKeydown);
  window.addEventListener('playground:run', handleRunShortcut);

  const cleanup = await createView(
    renderPlayground,
    async () => {
      const [models, providers, combos, keys, accounts] = await Promise.all([
        api('/models') as Promise<Model[]>,
        (state.providers.length === 0 ? api('/providers') : Promise.resolve(state.providers)) as Promise<Provider[]>,
        (state.combos.length === 0 ? api('/combos') : Promise.resolve(state.combos)) as Promise<Combo[]>,
        (state.apiKeys.length === 0 ? api('/keys') : Promise.resolve(state.apiKeys)) as Promise<typeof state.apiKeys>,
        (state.accounts.length === 0 ? api('/accounts') : Promise.resolve(state.accounts)) as Promise<Account[]>,
      ]);
      state.models = models;
      state.modelsComplete = true;
      state.providers = providers;
      state.combos = combos;
      state.apiKeys = keys;
      state.accounts = accounts;
      ensureDefaultModel(stateInternal);
    },
    (msg) => {
      stateInternal.loadError = msg;
    },
  );

  return () => {
    window.removeEventListener('keydown', handleGlobalKeydown);
    window.removeEventListener('playground:run', handleRunShortcut);
    if (cleanup) cleanup();
  };
}
