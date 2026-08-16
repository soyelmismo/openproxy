// views/playground.ts — API Playground & Request Tester view.
//
// Allows interactive testing of all OpenProxy endpoints:
//   - POST /v1/chat/completions (with SSE streaming & sync)
//   - POST /v1/images/generations (with image rendering)
//   - POST /v1/embeddings (with vector dimension & float inspection)
//   - POST /v1/audio/transcriptions (with audio upload & text rendering)
//
// Supports target selection (Provider, Model, Combo, Account), active API keys,
// custom bearer tokens, live stream inspection, and cURL export.

import { html, type TemplateResult } from 'lit-html';
import { state } from '../state/index.js';
import { api } from '../state/api.js';
import { getToken } from '../state/auth.js';
import { requestUpdate } from '../state/reactive.js';
import { createView } from '../lib/view-utils.js';
import { showToast } from '../components/toast.js';
import type { Model, Provider, Account, Combo } from '../lib/types/api.js';

export type ModalityType = 'chat' | 'image' | 'embedding' | 'audio';
export type ResponseTab = 'formatted' | 'raw' | 'headers' | 'stream';

export interface ChatMessage {
  id: string;
  role: 'system' | 'user' | 'assistant';
  content: string;
}

export interface StreamChunkItem {
  index: number;
  delta: string;
  timestampMs: number;
  raw: string;
}

export interface RequestMetrics {
  statusCode: number | null;
  statusText: string | null;
  ttftMs: number | null;
  totalLatencyMs: number | null;
  promptTokens: number | null;
  completionTokens: number | null;
  totalTokens: number | null;
  payloadSizeBytes: number | null;
}

// Module-local state
let modality: ModalityType = 'chat';
let activeResponseTab: ResponseTab = 'formatted';
let selectedProviderId = '';
let selectedModelId = '';
let keySource: 'session' | 'key' | 'custom' = 'session';
let selectedApiKeyPrefix = '';
let customApiKey = '';

// Chat parameters
let chatMessages: ChatMessage[] = [
  { id: 'msg-1', role: 'system', content: 'You are a helpful AI assistant.' },
  { id: 'msg-2', role: 'user', content: 'Hello! Please summarize what you can do.' },
];
let chatTemperature = 0.7;
let chatMaxTokens: number | null = null;
let chatStream = true;

// Image parameters
let imagePrompt = 'A serene futuristic digital city with neon reflections and lush trees, cinematic lighting';
let imageNegativePrompt = 'blurry, low quality, distorted, artifacts';
let imageSize = '1024x1024';
let imageQuality = 'standard';
let imageN = 1;
let imageSeed: number | null = null;
let imageResponseFormat: 'url' | 'b64_json' = 'b64_json';

// Embedding parameters
let embeddingInput = 'The quick brown fox jumps over the lazy dog.';
let embeddingIsArray = false;

// Audio parameters
let audioFile: File | null = null;
let audioPrompt = '';
let audioLanguage = '';
let audioTemperature = 0.0;
let audioResponseFormat = 'json';

// Execution & Response state
let isLoading = false;
let abortController: AbortController | null = null;
let currentMetrics: RequestMetrics = {
  statusCode: null,
  statusText: null,
  ttftMs: null,
  totalLatencyMs: null,
  promptTokens: null,
  completionTokens: null,
  totalTokens: null,
  payloadSizeBytes: null,
};
let responseHeaders: Record<string, string> = {};
let rawResponseText = '';
let parsedResponseJson: unknown = null;
let responseError: string | null = null;
let streamChunks: StreamChunkItem[] = [];
let streamedChatContent = '';
let loadError: string | null = null;

// Helpers
function generateId(): string {
  return 'msg-' + Math.random().toString(36).substring(2, 9);
}

function getEffectiveApiKey(): string {
  if (keySource === 'session') {
    return getToken() || '';
  }
  if (keySource === 'custom') {
    return customApiKey.trim();
  }
  // Registered key prefix matching
  const keys = (state.apiKeys as Array<{ key_prefix?: string; id?: number; label?: string }>) || [];
  const found = keys.find((k) => k.key_prefix === selectedApiKeyPrefix);
  if (found && found.key_prefix) {
    // If user has a matching prefix, we might not have plaintext, but session or custom can be used
    return customApiKey.trim() || getToken() || '';
  }
  return getToken() || '';
}

function getFilteredModels(): Array<{ id: string; name: string; type: string; provider: string; isCombo?: boolean }> {
  const models = (state.models as Model[]) || [];
  const combos = (state.combos as Combo[]) || [];
  const result: Array<{ id: string; name: string; type: string; provider: string; isCombo?: boolean }> = [];

  // Filter by provider if selected
  const matchingModels = selectedProviderId
    ? models.filter((m) => m.provider_id === selectedProviderId)
    : models;

  // Add combos if chat modality and no provider filter (combos span multiple providers)
  if (modality === 'chat' && !selectedProviderId) {
    for (const c of combos) {
      result.push({
        id: `combo:${c.name}`,
        name: `[Combo] ${c.name} (${c.strategy})`,
        type: 'combo',
        provider: 'combo',
        isCombo: true,
      });
    }
  }

  // Filter models by modality
  for (const m of matchingModels) {
    const mType = (m.model_type || 'chat').toLowerCase();
    let matchesModality = false;

    if (modality === 'chat') {
      matchesModality = mType === 'chat' || mType === 'mixed' || mType === '' || !mType;
    } else if (modality === 'image') {
      matchesModality = mType === 'image' || m.model_id.toLowerCase().includes('dall-e') || m.model_id.toLowerCase().includes('flux') || m.model_id.toLowerCase().includes('sd');
    } else if (modality === 'embedding') {
      matchesModality = mType === 'embedding' || m.model_id.toLowerCase().includes('embed');
    } else if (modality === 'audio') {
      matchesModality = mType === 'audio' || m.model_id.toLowerCase().includes('whisper') || m.model_id.toLowerCase().includes('audio');
    }

    if (matchesModality || !mType) {
      result.push({
        id: m.model_id,
        name: m.display_name ? `${m.display_name} (${m.model_id})` : m.model_id,
        type: m.model_type || 'chat',
        provider: m.provider_id,
      });
    }
  }

  // If no models matched strictly for this modality, offer all models so the user isn't blocked
  if (result.length === 0) {
    for (const m of matchingModels) {
      result.push({
        id: m.model_id,
        name: m.display_name ? `${m.display_name} (${m.model_id})` : m.model_id,
        type: m.model_type || 'chat',
        provider: m.provider_id,
      });
    }
  }

  return result;
}

function ensureDefaultModel(): void {
  const models = getFilteredModels();
  if (models.length > 0) {
    const exists = models.some((m) => m.id === selectedModelId);
    if (!exists && models[0]) {
      selectedModelId = models[0].id;
    }
  }
}

// Request dispatcher
async function executeRequest(): Promise<void> {
  if (isLoading) return;

  const key = getEffectiveApiKey();
  if (!key && !getToken()) {
    showToast('Please provide an API Key or log in to send requests', 'error');
    return;
  }

  ensureDefaultModel();
  if (!selectedModelId) {
    showToast('Please select or enter a Model', 'error');
    return;
  }

  isLoading = true;
  responseError = null;
  rawResponseText = '';
  parsedResponseJson = null;
  streamChunks = [];
  streamedChatContent = '';
  responseHeaders = {};
  currentMetrics = {
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

  abortController = new AbortController();
  const startTime = performance.now();

  try {
    if (modality === 'chat') {
      await executeChatRequest(key, startTime);
    } else if (modality === 'image') {
      await executeImageRequest(key);
    } else if (modality === 'embedding') {
      await executeEmbeddingRequest(key);
    } else if (modality === 'audio') {
      await executeAudioRequest(key);
    }
  } catch (err: unknown) {
    if (abortController?.signal.aborted) {
      responseError = 'Request aborted by user.';
    } else {
      const msg = err instanceof Error ? err.message : String(err);
      responseError = msg;
      showToast(`Error: ${msg}`, 'error');
    }
  } finally {
    currentMetrics.totalLatencyMs = Math.round(performance.now() - startTime);
    isLoading = false;
    abortController = null;
    requestUpdate();
  }
}

async function executeChatRequest(key: string, startTime: number): Promise<void> {
  const messages = chatMessages
    .filter((m) => m.content.trim().length > 0)
    .map((m) => ({ role: m.role, content: m.content }));

  if (messages.length === 0) {
    throw new Error('Please add at least one message with content.');
  }

  const payload: Record<string, unknown> = {
    model: selectedModelId,
    messages,
    temperature: chatTemperature,
    stream: chatStream,
  };
  if (chatMaxTokens !== null && chatMaxTokens > 0) {
    payload['max_tokens'] = chatMaxTokens;
  }

  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
  };
  if (key) {
    headers['Authorization'] = `Bearer ${key}`;
  }

  const reqInit: RequestInit = {
    method: 'POST',
    headers,
    body: JSON.stringify(payload),
  };
  if (abortController) {
    reqInit.signal = abortController.signal;
  }

  const response = await fetch('/v1/chat/completions', reqInit);

  currentMetrics.statusCode = response.status;
  currentMetrics.statusText = response.statusText;
  response.headers.forEach((v, k) => {
    responseHeaders[k] = v;
  });

  if (!response.ok) {
    const errorText = await response.text();
    rawResponseText = errorText;
    try {
      parsedResponseJson = JSON.parse(errorText);
    } catch {
      parsedResponseJson = null;
    }
    throw new Error(`HTTP ${response.status}: ${errorText}`);
  }

  if (chatStream && response.body) {
    const reader = response.body.getReader();
    const decoder = new TextDecoder('utf-8');
    let buffer = '';
    let firstTokenTime: number | null = null;
    let chunkIndex = 0;

    while (true) {
      const { done, value } = await reader.read();
      if (done) break;

      const textChunk = decoder.decode(value, { stream: true });
      buffer += textChunk;
      currentMetrics.payloadSizeBytes = (currentMetrics.payloadSizeBytes || 0) + value.byteLength;

      const lines = buffer.split('\n');
      buffer = lines.pop() || '';

      for (const line of lines) {
        const trimmed = line.trim();
        if (!trimmed || trimmed.startsWith(':')) continue; // keepalive or comment

        if (trimmed.startsWith('data: ')) {
          const dataStr = trimmed.substring(6).trim();
          if (dataStr === '[DONE]') {
            continue;
          }

          try {
            const parsed = JSON.parse(dataStr);
            const delta = parsed?.choices?.[0]?.delta?.content || '';

            if (delta) {
              if (firstTokenTime === null) {
                firstTokenTime = performance.now();
                currentMetrics.ttftMs = Math.round(firstTokenTime - startTime);
              }
              streamedChatContent += delta;
            }

            if (parsed?.usage) {
              currentMetrics.promptTokens = parsed.usage.prompt_tokens ?? currentMetrics.promptTokens;
              currentMetrics.completionTokens = parsed.usage.completion_tokens ?? currentMetrics.completionTokens;
              currentMetrics.totalTokens = parsed.usage.total_tokens ?? currentMetrics.totalTokens;
            }

            streamChunks.push({
              index: ++chunkIndex,
              delta,
              timestampMs: Math.round(performance.now() - startTime),
              raw: dataStr,
            });
            requestUpdate();
          } catch {
            // Unparseable frame
          }
        }
      }
    }

    rawResponseText = streamedChatContent;
  } else {
    // Non-streaming response
    const text = await response.text();
    rawResponseText = text;
    currentMetrics.payloadSizeBytes = new Blob([text]).size;
    try {
      const json = JSON.parse(text);
      parsedResponseJson = json;
      streamedChatContent = json?.choices?.[0]?.message?.content || '';
      if (json?.usage) {
        currentMetrics.promptTokens = json.usage.prompt_tokens ?? null;
        currentMetrics.completionTokens = json.usage.completion_tokens ?? null;
        currentMetrics.totalTokens = json.usage.total_tokens ?? null;
      }
    } catch {
      streamedChatContent = text;
    }
  }
}

async function executeImageRequest(key: string): Promise<void> {
  if (!imagePrompt.trim()) {
    throw new Error('Please enter a prompt for image generation.');
  }

  const payload: Record<string, unknown> = {
    model: selectedModelId || 'dall-e-3',
    prompt: imagePrompt.trim(),
    n: imageN,
    size: imageSize,
    quality: imageQuality,
    response_format: imageResponseFormat,
  };
  if (imageNegativePrompt.trim()) {
    payload['negative_prompt'] = imageNegativePrompt.trim();
  }
  if (imageSeed !== null && !isNaN(imageSeed)) {
    payload['seed'] = imageSeed;
  }

  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
  };
  if (key) headers['Authorization'] = `Bearer ${key}`;

  const reqInit: RequestInit = {
    method: 'POST',
    headers,
    body: JSON.stringify(payload),
  };
  if (abortController) {
    reqInit.signal = abortController.signal;
  }

  const response = await fetch('/v1/images/generations', reqInit);

  currentMetrics.statusCode = response.status;
  currentMetrics.statusText = response.statusText;
  response.headers.forEach((v, k) => {
    responseHeaders[k] = v;
  });

  const text = await response.text();
  rawResponseText = text;
  currentMetrics.payloadSizeBytes = new Blob([text]).size;

  try {
    parsedResponseJson = JSON.parse(text);
  } catch {
    parsedResponseJson = null;
  }

  if (!response.ok) {
    throw new Error(`HTTP ${response.status}: ${text}`);
  }
}

async function executeEmbeddingRequest(key: string): Promise<void> {
  if (!embeddingInput.trim()) {
    throw new Error('Please enter text to generate embeddings for.');
  }

  let inputData: string | string[] = embeddingInput.trim();
  if (embeddingIsArray) {
    inputData = embeddingInput
      .split('\n')
      .map((s) => s.trim())
      .filter((s) => s.length > 0);
  }

  const payload = {
    model: selectedModelId || 'text-embedding-3-small',
    input: inputData,
  };

  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
  };
  if (key) headers['Authorization'] = `Bearer ${key}`;

  const reqInit: RequestInit = {
    method: 'POST',
    headers,
    body: JSON.stringify(payload),
  };
  if (abortController) {
    reqInit.signal = abortController.signal;
  }

  const response = await fetch('/v1/embeddings', reqInit);

  currentMetrics.statusCode = response.status;
  currentMetrics.statusText = response.statusText;
  response.headers.forEach((v, k) => {
    responseHeaders[k] = v;
  });

  const text = await response.text();
  rawResponseText = text;
  currentMetrics.payloadSizeBytes = new Blob([text]).size;

  try {
    const json = JSON.parse(text);
    parsedResponseJson = json;
    if (json?.usage) {
      currentMetrics.promptTokens = json.usage.prompt_tokens ?? null;
      currentMetrics.totalTokens = json.usage.total_tokens ?? null;
    }
  } catch {
    parsedResponseJson = null;
  }

  if (!response.ok) {
    throw new Error(`HTTP ${response.status}: ${text}`);
  }
}

async function executeAudioRequest(key: string): Promise<void> {
  if (!audioFile) {
    throw new Error('Please select an audio file to transcribe.');
  }

  const formData = new FormData();
  formData.append('file', audioFile, audioFile.name);
  formData.append('model', selectedModelId || 'whisper-1');
  if (audioPrompt.trim()) formData.append('prompt', audioPrompt.trim());
  if (audioLanguage.trim()) formData.append('language', audioLanguage.trim());
  formData.append('temperature', String(audioTemperature));
  formData.append('response_format', audioResponseFormat);

  const headers: Record<string, string> = {};
  if (key) headers['Authorization'] = `Bearer ${key}`;

  const reqInit: RequestInit = {
    method: 'POST',
    headers,
    body: formData,
  };
  if (abortController) {
    reqInit.signal = abortController.signal;
  }

  const response = await fetch('/v1/audio/transcriptions', reqInit);

  currentMetrics.statusCode = response.status;
  currentMetrics.statusText = response.statusText;
  response.headers.forEach((v, k) => {
    responseHeaders[k] = v;
  });

  const text = await response.text();
  rawResponseText = text;
  currentMetrics.payloadSizeBytes = new Blob([text]).size;

  try {
    parsedResponseJson = JSON.parse(text);
  } catch {
    parsedResponseJson = text;
  }

  if (!response.ok) {
    throw new Error(`HTTP ${response.status}: ${text}`);
  }
}

function cancelRequest(): void {
  if (abortController) {
    abortController.abort();
  }
}

function generateCurlCommand(): string {
  const key = getEffectiveApiKey() || '<YOUR_API_KEY>';
  const host = window.location.origin;

  if (modality === 'chat') {
    const messages = chatMessages.map((m) => ({ role: m.role, content: m.content }));
    const body = JSON.stringify(
      {
        model: selectedModelId || 'gpt-4o',
        messages,
        temperature: chatTemperature,
        stream: chatStream,
      },
      null,
      2,
    );
    return `curl -X POST "${host}/v1/chat/completions" \\\n  -H "Authorization: Bearer ${key}" \\\n  -H "Content-Type: application/json" \\\n  -d '${body.replace(/'/g, "'\\''")}'`;
  } else if (modality === 'image') {
    const body = JSON.stringify(
      {
        model: selectedModelId || 'dall-e-3',
        prompt: imagePrompt,
        size: imageSize,
        quality: imageQuality,
        n: imageN,
        response_format: imageResponseFormat,
      },
      null,
      2,
    );
    return `curl -X POST "${host}/v1/images/generations" \\\n  -H "Authorization: Bearer ${key}" \\\n  -H "Content-Type: application/json" \\\n  -d '${body.replace(/'/g, "'\\''")}'`;
  } else if (modality === 'embedding') {
    const body = JSON.stringify(
      {
        model: selectedModelId || 'text-embedding-3-small',
        input: embeddingInput,
      },
      null,
      2,
    );
    return `curl -X POST "${host}/v1/embeddings" \\\n  -H "Authorization: Bearer ${key}" \\\n  -H "Content-Type: application/json" \\\n  -d '${body.replace(/'/g, "'\\''")}'`;
  } else {
    return `curl -X POST "${host}/v1/audio/transcriptions" \\\n  -H "Authorization: Bearer ${key}" \\\n  -F "file=@audio.mp3" \\\n  -F "model=${selectedModelId || 'whisper-1'}"`;
  }
}

function copyCurlToClipboard(): void {
  const curl = generateCurlCommand();
  if (navigator.clipboard) {
    navigator.clipboard.writeText(curl).then(() => {
      showToast('cURL command copied to clipboard!', 'info');
    }).catch(() => {});
  }
}

// Templates & Renderers
function renderTargetControls(): TemplateResult {
  const providers = (state.providers as Provider[]) || [];
  const accounts = (state.accounts as Account[]) || [];
  const filteredModels = getFilteredModels();
  const apiKeys = (state.apiKeys as Array<{ id: number; label: string | null; key_prefix: string | null }>) || [];

  const matchingAccounts = selectedProviderId
    ? accounts.filter((a) => a.provider_id === selectedProviderId)
    : [];

  return html`
    <div class="playground-card playground-target-config">
      <div class="playground-card-header">
        <h3>Target & Authentication</h3>
        <span class="badge badge-info">${modality.toUpperCase()}</span>
      </div>

      <div class="playground-grid-3">
        <!-- API Key Selection -->
        <div class="field">
          <label class="field-label">API Key Auth</label>
          <select
            .value=${keySource}
            @change=${(e: Event) => {
              keySource = (e.target as HTMLSelectElement).value as typeof keySource;
              requestUpdate();
            }}
          >
            <option value="session">Active Admin Session Token</option>
            ${apiKeys.length > 0 ? html`<option value="key">Registered API Key</option>` : html``}
            <option value="custom">Custom Bearer Token</option>
          </select>
        </div>

        ${keySource === 'key'
          ? html`
              <div class="field">
                <label class="field-label">Select Registered Key</label>
                <select
                  .value=${selectedApiKeyPrefix}
                  @change=${(e: Event) => {
                    selectedApiKeyPrefix = (e.target as HTMLSelectElement).value;
                    requestUpdate();
                  }}
                >
                  <option value="">— Choose Key —</option>
                  ${apiKeys.map(
                    (k) => html`<option value=${k.key_prefix || ''}>${k.label || 'Unnamed'} (${k.key_prefix || '—'})</option>`,
                  )}
                </select>
              </div>
            `
          : keySource === 'custom'
          ? html`
              <div class="field">
                <label class="field-label">Custom Bearer Key</label>
                <input
                  type="password"
                  placeholder="sk-..."
                  .value=${customApiKey}
                  @input=${(e: Event) => {
                    customApiKey = (e.target as HTMLInputElement).value;
                  }}
                />
              </div>
            `
          : html`
              <div class="field">
                <label class="field-label">Active Session</label>
                <div class="playground-pill-text">Using current session auth</div>
              </div>
            `}

        <!-- Provider Filter -->
        <div class="field">
          <label class="field-label">Filter by Provider</label>
          <select
            .value=${selectedProviderId}
            @change=${(e: Event) => {
              selectedProviderId = (e.target as HTMLSelectElement).value;
              selectedModelId = '';
              ensureDefaultModel();
              requestUpdate();
            }}
          >
            <option value="">All Providers (${providers.length})</option>
            ${providers.map(
              (p) => html`<option value=${p.id}>${p.name || p.id} ${p.active ? '' : '(inactive)'}</option>`,
            )}
          </select>
        </div>
      </div>

      <div class="playground-grid-2">
        <!-- Model Selection -->
        <div class="field">
          <label class="field-label">Target Model / Combo</label>
          <div class="playground-input-group">
            <select
              .value=${selectedModelId}
              @change=${(e: Event) => {
                selectedModelId = (e.target as HTMLSelectElement).value;
                requestUpdate();
              }}
            >
              ${filteredModels.length === 0
                ? html`<option value="">No models available for ${modality}</option>`
                : filteredModels.map(
                    (m) => html`<option value=${m.id}>${m.name} [${m.provider}]</option>`,
                  )}
            </select>
            <input
              type="text"
              placeholder="Or custom model ID..."
              .value=${selectedModelId}
              @input=${(e: Event) => {
                selectedModelId = (e.target as HTMLInputElement).value;
              }}
            />
          </div>
          <small class="text-muted">Select from discovered models/combos or type custom model string</small>
        </div>

        <!-- Account Info (Optional) -->
        <div class="field">
          <label class="field-label">Provider Accounts</label>
          ${matchingAccounts.length > 0
            ? html`
                <div class="playground-account-pills">
                  ${matchingAccounts.map(
                    (a) => html`
                      <span class="status-pill ${a.health_status === 'healthy' ? 'on' : 'off'}" title="Priority: ${a.priority}">
                        ${a.label || '#' + a.id} (${a.health_status})
                      </span>
                    `,
                  )}
                </div>
              `
            : html`<span class="text-muted" style="font-size: var(--fs-xs); margin-top: 6px;">${selectedProviderId ? 'No accounts configured for this provider' : 'Select a provider to inspect active accounts'}</span>`}
        </div>
      </div>
    </div>
  `;
}

function renderChatConfig(): TemplateResult {
  return html`
    <div class="playground-card">
      <div class="playground-card-header">
        <h3>Chat Messages & Parameters</h3>
        <div class="playground-actions-row">
          <button
            class="small"
            @click=${() => {
              chatMessages.push({ id: generateId(), role: 'user', content: '' });
              requestUpdate();
            }}
          >
            + Add Message
          </button>
          <button
            class="small"
            @click=${() => {
              chatMessages = [
                { id: generateId(), role: 'system', content: 'You are a helpful coding assistant.' },
                { id: generateId(), role: 'user', content: 'Write a fast rust function to calculate fibonacci.' },
              ];
              requestUpdate();
            }}
          >
            Preset: Code
          </button>
          <button
            class="small danger"
            @click=${() => {
              chatMessages = [{ id: generateId(), role: 'user', content: '' }];
              requestUpdate();
            }}
          >
            Clear
          </button>
        </div>
      </div>

      <!-- Messages list -->
      <div class="playground-messages-list">
        ${chatMessages.map((msg) => html`
          <div class="playground-message-row role-${msg.role}">
            <div class="playground-msg-header">
              <select
                class="playground-role-select"
                .value=${msg.role}
                @change=${(e: Event) => {
                  msg.role = (e.target as HTMLSelectElement).value as typeof msg.role;
                  requestUpdate();
                }}
              >
                <option value="system">System</option>
                <option value="user">User</option>
                <option value="assistant">Assistant</option>
              </select>
              <button
                class="close-btn"
                title="Remove message"
                @click=${() => {
                  chatMessages = chatMessages.filter((m) => m.id !== msg.id);
                  if (chatMessages.length === 0) {
                    chatMessages.push({ id: generateId(), role: 'user', content: '' });
                  }
                  requestUpdate();
                }}
              >
                ×
              </button>
            </div>
            <textarea
              class="playground-msg-textarea"
              placeholder="Enter ${msg.role} prompt..."
              rows=${msg.role === 'system' ? 2 : 4}
              .value=${msg.content}
              @input=${(e: Event) => {
                msg.content = (e.target as HTMLTextAreaElement).value;
              }}
            ></textarea>
          </div>
        `)}
      </div>

      <!-- Parameters Row -->
      <div class="playground-grid-3" style="margin-top: var(--space-4); border-top: var(--border-w) solid var(--color-border-soft); padding-top: var(--space-3);">
        <div class="field">
          <label class="field-label">Temperature: <code>${chatTemperature}</code></label>
          <input
            type="range"
            min="0"
            max="2"
            step="0.05"
            .value=${String(chatTemperature)}
            @input=${(e: Event) => {
              chatTemperature = parseFloat((e.target as HTMLInputElement).value);
              requestUpdate();
            }}
          />
        </div>

        <div class="field">
          <label class="field-label">Max Tokens</label>
          <input
            type="number"
            placeholder="Unlimited / Model default"
            .value=${chatMaxTokens !== null ? String(chatMaxTokens) : ''}
            @input=${(e: Event) => {
              const val = (e.target as HTMLInputElement).value;
              chatMaxTokens = val ? parseInt(val, 10) : null;
            }}
          />
        </div>

        <div class="field">
          <label class="field-label">Stream Response (SSE)</label>
          <label class="playground-switch-label">
            <input
              type="checkbox"
              ?checked=${chatStream}
              @change=${(e: Event) => {
                chatStream = (e.target as HTMLInputElement).checked;
                requestUpdate();
              }}
            />
            <span>${chatStream ? 'SSE Streaming ON' : 'Non-Streaming JSON'}</span>
          </label>
        </div>
      </div>
    </div>
  `;
}

function renderImageConfig(): TemplateResult {
  return html`
    <div class="playground-card">
      <div class="playground-card-header">
        <h3>Image Generation Parameters</h3>
        <div class="playground-actions-row">
          <button
            class="small"
            @click=${() => {
              imagePrompt = 'A cozy cyberpunk coffee shop in Tokyo with neon rain reflections, anime style';
              requestUpdate();
            }}
          >
            Preset: Cyberpunk
          </button>
          <button
            class="small"
            @click=${() => {
              imagePrompt = 'A hyper-realistic studio portrait of a majestic owl wearing a gold monocle, 8k octane render';
              requestUpdate();
            }}
          >
            Preset: Realistic
          </button>
        </div>
      </div>

      <div class="field">
        <label class="field-label">Prompt (Required)</label>
        <textarea
          rows="3"
          placeholder="Describe the image you want to generate..."
          .value=${imagePrompt}
          @input=${(e: Event) => {
            imagePrompt = (e.target as HTMLInputElement).value;
          }}
        ></textarea>
      </div>

      <div class="field">
        <label class="field-label">Negative Prompt (Optional)</label>
        <input
          type="text"
          placeholder="low resolution, blurry, bad anatomy..."
          .value=${imageNegativePrompt}
          @input=${(e: Event) => {
            imageNegativePrompt = (e.target as HTMLInputElement).value;
          }}
        />
      </div>

      <div class="playground-grid-4">
        <div class="field">
          <label class="field-label">Size / Ratio</label>
          <select
            .value=${imageSize}
            @change=${(e: Event) => {
              imageSize = (e.target as HTMLSelectElement).value;
            }}
          >
            <option value="1024x1024">1024x1024 (1:1)</option>
            <option value="1792x1024">1792x1024 (16:9)</option>
            <option value="1024x1792">1024x1792 (9:16)</option>
            <option value="512x512">512x512 (Fast)</option>
          </select>
        </div>

        <div class="field">
          <label class="field-label">Quality</label>
          <select
            .value=${imageQuality}
            @change=${(e: Event) => {
              imageQuality = (e.target as HTMLSelectElement).value;
            }}
          >
            <option value="standard">Standard</option>
            <option value="hd">HD / High Detail</option>
          </select>
        </div>

        <div class="field">
          <label class="field-label">Count (n)</label>
          <select
            .value=${String(imageN)}
            @change=${(e: Event) => {
              imageN = parseInt((e.target as HTMLSelectElement).value, 10);
            }}
          >
            <option value="1">1 image</option>
            <option value="2">2 images</option>
            <option value="4">4 images</option>
          </select>
        </div>

        <div class="field">
          <label class="field-label">Format</label>
          <select
            .value=${imageResponseFormat}
            @change=${(e: Event) => {
              imageResponseFormat = (e.target as HTMLSelectElement).value as typeof imageResponseFormat;
            }}
          >
            <option value="b64_json">Base64 JSON</option>
            <option value="url">URL</option>
          </select>
        </div>
      </div>
    </div>
  `;
}

function renderEmbeddingConfig(): TemplateResult {
  return html`
    <div class="playground-card">
      <div class="playground-card-header">
        <h3>Embedding Input Parameters</h3>
        <div class="playground-actions-row">
          <button
            class="small"
            @click=${() => {
              embeddingInput = 'Vector databases allow semantic similarity search across embeddings.';
              requestUpdate();
            }}
          >
            Sample Text
          </button>
        </div>
      </div>

      <div class="field">
        <div style="display: flex; justify-content: space-between; align-items: center;">
          <label class="field-label">Input Text</label>
          <label class="playground-switch-label" style="font-size: var(--fs-xs);">
            <input
              type="checkbox"
              ?checked=${embeddingIsArray}
              @change=${(e: Event) => {
                embeddingIsArray = (e.target as HTMLInputElement).checked;
                requestUpdate();
              }}
            />
            <span>${embeddingIsArray ? 'Batch mode (newline separated)' : 'Single string'}</span>
          </label>
        </div>
        <textarea
          rows="6"
          placeholder="Enter text string to embed..."
          .value=${embeddingInput}
          @input=${(e: Event) => {
            embeddingInput = (e.target as HTMLTextAreaElement).value;
          }}
        ></textarea>
        <small class="text-muted">Calculates high-dimensional vector representations via <code>/v1/embeddings</code></small>
      </div>
    </div>
  `;
}

function renderAudioConfig(): TemplateResult {
  return html`
    <div class="playground-card">
      <div class="playground-card-header">
        <h3>Audio Transcription (Whisper)</h3>
      </div>

      <div class="field">
        <label class="field-label">Upload Audio File (.mp3, .wav, .m4a, .ogg, .webm)</label>
        <div class="playground-file-dropzone">
          <input
            type="file"
            accept="audio/*"
            @change=${(e: Event) => {
              const input = e.target as HTMLInputElement;
              if (input.files && input.files[0]) {
                audioFile = input.files[0];
                requestUpdate();
              }
            }}
          />
          ${audioFile
            ? html`
                <div class="playground-file-info">
                  <strong>Selected:</strong> ${audioFile.name} (${Math.round(audioFile.size / 1024)} KB)
                  <audio controls src=${URL.createObjectURL(audioFile)} style="margin-top: var(--space-2); width: 100%;"></audio>
                </div>
              `
            : html`
                <p class="text-muted">Click or drag an audio file here to test transcription</p>
              `}
        </div>
      </div>

      <div class="playground-grid-3">
        <div class="field">
          <label class="field-label">Prompt Guide (Optional)</label>
          <input
            type="text"
            placeholder="Context or glossary..."
            .value=${audioPrompt}
            @input=${(e: Event) => {
              audioPrompt = (e.target as HTMLInputElement).value;
            }}
          />
        </div>

        <div class="field">
          <label class="field-label">Language (ISO-639-1)</label>
          <input
            type="text"
            placeholder="e.g. en, es, fr..."
            .value=${audioLanguage}
            @input=${(e: Event) => {
              audioLanguage = (e.target as HTMLInputElement).value;
            }}
          />
        </div>

        <div class="field">
          <label class="field-label">Response Format</label>
          <select
            .value=${audioResponseFormat}
            @change=${(e: Event) => {
              audioResponseFormat = (e.target as HTMLSelectElement).value;
            }}
          >
            <option value="json">json</option>
            <option value="text">text</option>
            <option value="verbose_json">verbose_json</option>
            <option value="srt">srt</option>
            <option value="vtt">vtt</option>
          </select>
        </div>
      </div>
    </div>
  `;
}

function renderMetricsBar(): TemplateResult {
  const status = currentMetrics.statusCode;
  const isOk = status !== null && status >= 200 && status < 300;
  const isErr = status !== null && status >= 400;

  return html`
    <div class="playground-metrics-bar">
      <div class="playground-metric">
        <span class="metric-label">Status</span>
        <span class="status-pill ${isOk ? 'on' : isErr ? 'off' : ''}">
          ${status ? `${status} ${currentMetrics.statusText || ''}` : '—'}
        </span>
      </div>

      <div class="playground-metric">
        <span class="metric-label">Latency</span>
        <span class="metric-value">${currentMetrics.totalLatencyMs !== null ? `${currentMetrics.totalLatencyMs} ms` : '—'}</span>
      </div>

      ${currentMetrics.ttftMs !== null
        ? html`
            <div class="playground-metric">
              <span class="metric-label">TTFT</span>
              <span class="metric-value">${currentMetrics.ttftMs} ms</span>
            </div>
          `
        : html``}

      ${currentMetrics.totalTokens !== null
        ? html`
            <div class="playground-metric">
              <span class="metric-label">Tokens</span>
              <span class="metric-value">
                ${currentMetrics.totalTokens}
                ${currentMetrics.promptTokens !== null ? html`<small>(${currentMetrics.promptTokens}p / ${currentMetrics.completionTokens || 0}c)</small>` : html``}
              </span>
            </div>
          `
        : currentMetrics.payloadSizeBytes !== null
        ? html`
            <div class="playground-metric">
              <span class="metric-label">Size</span>
              <span class="metric-value">${Math.round(currentMetrics.payloadSizeBytes / 10.24) / 100} KB</span>
            </div>
          `
        : html``}
    </div>
  `;
}

function renderFormattedResponse(): TemplateResult {
  if (responseError) {
    const errorDetails = (typeof parsedResponseJson === 'object' && parsedResponseJson !== null)
      ? parsedResponseJson
      : null;
    const curl = generateCurlCommand();

    return html`
      <div class="banner banner-error" style="display: flex; flex-direction: column; gap: var(--space-3); padding: var(--space-3); border-radius: var(--radius-md);">
        <div style="display: flex; align-items: center; justify-content: space-between; gap: var(--space-2);">
          <h4 style="margin: 0; color: var(--color-error); display: flex; align-items: center; gap: var(--space-2);">
            <span>⚠️ Request Failed</span>
            ${currentMetrics.statusCode
              ? html`<span class="badge" style="background: var(--color-error); color: #fff;">HTTP ${currentMetrics.statusCode}</span>`
              : html``}
          </h4>
          <button class="small" @click=${() => { activeResponseTab = 'raw'; requestUpdate(); }}>
            View Raw Error Body →
          </button>
        </div>

        <p style="margin: 0; font-family: var(--font-mono); font-size: var(--fs-sm); word-break: break-word;">${responseError}</p>

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
            <button class="small" @click=${copyCurlToClipboard}>Copy cURL</button>
          </div>
          <pre class="playground-code-view" style="margin: 0; max-height: 180px; font-size: var(--fs-xs);"><code>${curl}</code></pre>
        </div>
      </div>
    `;
  }

  if (modality === 'chat') {
    if (!streamedChatContent && !isLoading) {
      return html`<div class="playground-empty-response">Ready to send. Click "Send Request" to test chat completion.</div>`;
    }

    return html`
      <div class="playground-chat-response">
        <div class="playground-chat-bubble assistant">
          <div class="bubble-header">
            <span class="role-badge">Assistant</span>
            ${isLoading ? html`<span class="badge badge-info">Generating…</span>` : html``}
          </div>
          <div class="bubble-content" style="white-space: pre-wrap; font-family: var(--font-sans); line-height: 1.6;">
            ${streamedChatContent}
          </div>
        </div>
      </div>
    `;
  }

  if (modality === 'image') {
    const data = (parsedResponseJson as { data?: Array<{ url?: string; b64_json?: string; revised_prompt?: string }> })?.data;
    if (!data || data.length === 0) {
      return html`<div class="playground-empty-response">No images generated yet.</div>`;
    }

    return html`
      <div class="playground-image-gallery">
        ${data.map((item, idx) => {
          const imgSrc = item.b64_json ? `data:image/png;base64,${item.b64_json}` : (item.url || '');
          return html`
            <div class="playground-image-card">
              <img src=${imgSrc} alt="Generated image ${idx + 1}" loading="lazy" />
              ${item.revised_prompt
                ? html`<p class="image-revised-prompt"><small>${item.revised_prompt}</small></p>`
                : html``}
              <div class="image-actions">
                <a class="button small" href=${imgSrc} download="generated-image-${idx + 1}.png" target="_blank">Download</a>
              </div>
            </div>
          `;
        })}
      </div>
    `;
  }

  if (modality === 'embedding') {
    const data = (parsedResponseJson as { data?: Array<{ embedding?: number[]; index?: number }> })?.data;
    if (!data || data.length === 0) {
      return html`<div class="playground-empty-response">No embeddings generated yet.</div>`;
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
              <!-- Vector magnitude / bar visualization -->
              <div class="playground-vector-bars">
                ${vector.slice(0, 60).map((v) => {
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

  if (modality === 'audio') {
    if (!rawResponseText && !parsedResponseJson) {
      return html`<div class="playground-empty-response">No transcription output yet.</div>`;
    }

    const transcribedText = typeof parsedResponseJson === 'object' && parsedResponseJson !== null && 'text' in parsedResponseJson
      ? (parsedResponseJson as { text: string }).text
      : rawResponseText;

    return html`
      <div class="playground-audio-transcription">
        <div class="transcription-box">
          <h4>Transcribed Text</h4>
          <p style="white-space: pre-wrap; font-size: var(--fs-md); line-height: 1.6;">${transcribedText}</p>
        </div>
      </div>
    `;
  }

  return html`<div class="playground-empty-response">No response to display.</div>`;
}

function renderResponseViewer(): TemplateResult {
  return html`
    <div class="playground-card playground-response-panel">
      <div class="playground-card-header">
        <div class="detail-tabs" role="tablist">
          <button
            class="detail-tab ${activeResponseTab === 'formatted' ? 'active' : ''}"
            @click=${() => {
              activeResponseTab = 'formatted';
              requestUpdate();
            }}
          >
            Formatted Output
          </button>
          <button
            class="detail-tab ${activeResponseTab === 'raw' ? 'active' : ''}"
            @click=${() => {
              activeResponseTab = 'raw';
              requestUpdate();
            }}
          >
            Raw JSON / Text
          </button>
          <button
            class="detail-tab ${activeResponseTab === 'headers' ? 'active' : ''}"
            @click=${() => {
              activeResponseTab = 'headers';
              requestUpdate();
            }}
          >
            Headers
          </button>
          ${modality === 'chat' && chatStream
            ? html`
                <button
                  class="detail-tab ${activeResponseTab === 'stream' ? 'active' : ''}"
                  @click=${() => {
                    activeResponseTab = 'stream';
                    requestUpdate();
                  }}
                >
                  SSE Chunks (${streamChunks.length})
                </button>
              `
            : html``}
        </div>

        <div class="playground-actions-row">
          <button class="small" @click=${copyCurlToClipboard} title="Copy as cURL command">
            Copy cURL
          </button>
          <button
            class="small"
            @click=${() => {
              if (rawResponseText && navigator.clipboard) {
                navigator.clipboard.writeText(rawResponseText).then(() => {
                  showToast('Response copied!', 'info');
                }).catch(() => {});
              }
            }}
          >
            Copy Response
          </button>
        </div>
      </div>

      ${renderMetricsBar()}

      <div class="playground-response-body">
        ${activeResponseTab === 'formatted'
          ? renderFormattedResponse()
          : activeResponseTab === 'raw'
          ? html`
              <pre class="playground-code-view"><code>${parsedResponseJson ? JSON.stringify(parsedResponseJson, null, 2) : (rawResponseText || 'No response received yet.')}</code></pre>
            `
          : activeResponseTab === 'headers'
          ? html`
              <table class="playground-headers-table">
                <thead><tr><th>Header</th><th>Value</th></tr></thead>
                <tbody>
                  ${Object.keys(responseHeaders).length === 0
                    ? html`<tr><td colspan="2" class="text-muted">No headers available</td></tr>`
                    : Object.entries(responseHeaders).map(
                        ([k, v]) => html`<tr><td><code>${k}</code></td><td>${v}</td></tr>`,
                      )}
                </tbody>
              </table>
            `
          : html`
              <div class="playground-stream-log">
                ${streamChunks.length === 0
                  ? html`<p class="text-muted">No stream chunks captured yet.</p>`
                  : streamChunks.map(
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

function renderPlayground(): TemplateResult {
  if (loadError) {
    return html`
      <div class="page-header"><h2>API Playground & Tester</h2></div>
      <div class="banner banner-error">${loadError}</div>
    `;
  }

  return html`
    <div class="page-header">
      <h2>API Playground & Tester</h2>
      <div class="actions">
        ${isLoading
          ? html`<button class="danger" @click=${cancelRequest}>Cancel In-Flight</button>`
          : html`<button class="primary" @click=${executeRequest}>▶ Send Request</button>`}
      </div>
    </div>

    <!-- Modality Selector Bar -->
    <div class="playground-modality-bar">
      <button
        class="modality-btn ${modality === 'chat' ? 'active' : ''}"
        @click=${() => {
          modality = 'chat';
          selectedModelId = '';
          ensureDefaultModel();
          requestUpdate();
        }}
      >
        <span class="modality-icon">💬</span> Chat Completions
      </button>
      <button
        class="modality-btn ${modality === 'image' ? 'active' : ''}"
        @click=${() => {
          modality = 'image';
          selectedModelId = '';
          ensureDefaultModel();
          requestUpdate();
        }}
      >
        <span class="modality-icon">🎨</span> Image Generations
      </button>
      <button
        class="modality-btn ${modality === 'embedding' ? 'active' : ''}"
        @click=${() => {
          modality = 'embedding';
          selectedModelId = '';
          ensureDefaultModel();
          requestUpdate();
        }}
      >
        <span class="modality-icon">📐</span> Embeddings
      </button>
      <button
        class="modality-btn ${modality === 'audio' ? 'active' : ''}"
        @click=${() => {
          modality = 'audio';
          selectedModelId = '';
          ensureDefaultModel();
          requestUpdate();
        }}
      >
        <span class="modality-icon">🎙️</span> Audio Transcription
      </button>
    </div>

    <!-- Main 2-Column Playground Layout -->
    <div class="playground-layout">
      <div class="playground-left-col">
        ${renderTargetControls()}
        ${modality === 'chat'
          ? renderChatConfig()
          : modality === 'image'
          ? renderImageConfig()
          : modality === 'embedding'
          ? renderEmbeddingConfig()
          : renderAudioConfig()}
      </div>

      <div class="playground-right-col">
        ${renderResponseViewer()}
      </div>
    </div>
  `;
}

// Mount function
export async function mountPlayground(): Promise<(() => void) | void> {
  loadError = null;
  return createView(
    renderPlayground,
    async () => {
      // Ensure models, providers, combos, and keys are loaded
      const [models, providers, combos, keys, accounts] = await Promise.all([
        (state.models.length === 0 ? api('/models') : Promise.resolve(state.models)) as Promise<Model[]>,
        (state.providers.length === 0 ? api('/providers') : Promise.resolve(state.providers)) as Promise<Provider[]>,
        (state.combos.length === 0 ? api('/combos') : Promise.resolve(state.combos)) as Promise<Combo[]>,
        (state.apiKeys.length === 0 ? api('/keys') : Promise.resolve(state.apiKeys)) as Promise<unknown[]>,
        (state.accounts.length === 0 ? api('/accounts') : Promise.resolve(state.accounts)) as Promise<Account[]>,
      ]);
      state.models = models;
      state.providers = providers;
      state.combos = combos;
      state.apiKeys = keys;
      state.accounts = accounts;
      ensureDefaultModel();
    },
    (msg) => { loadError = msg; },
  );
}
