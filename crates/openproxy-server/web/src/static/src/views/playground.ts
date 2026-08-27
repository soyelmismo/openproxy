// views/playground.ts — API Playground & Request Tester view.
//
// Sophisticated studio layout (AI Studio / Anthropic Console / OpenAI Playground inspired):
//   - Header: Live status badge, latency, segmented modality control, quick action bar (Run/Stop, Copy cURL, Clear).
//   - Main Workspace:
//       * Chat: Collapsible system instructions, interactive message cards with roles, token estimation,
//         directive helper chips, multiline composer with Ctrl+Enter, integrated tabbed response inspector.
//       * Image Studio: Generation, Edition (Inpainting/Img2Img) and Variations, Diffusion directive chips,
//         high-res lightbox, Download PNG, Copy Base64, Send to Inpainting.
//       * Embeddings & Audio: Interactive vector visualizer & audio dropzone with player and formatted transcription.
//       * Response Inspector: Pro metrics bar (Status, Latency, TTFT, Tokens/sec, Prompt/Completion tokens) +
//         Formatted / Raw JSON / Response Headers / Stream Chunks Timeline.
//   - Inspector Sidebar (340px):
//       * Target & Auth: API Key, Provider, Account select dropdown [health], Model select / discovery.
//       * Hyperparameters: Synchronized sliders + numeric inputs, preset tokens (512, 2k, 4k, 8k), SSE toggle.

import { html, type TemplateResult } from 'lit-html';
import { unsafeHTML } from 'lit-html/directives/unsafe-html.js';
import { state } from '../state/index.js';
import { api } from '../state/api.js';
import { getToken } from '../state/auth.js';
import { requestUpdate } from '../state/reactive.js';
import { createView } from '../lib/view-utils.js';
import { showToast } from '../components/toast.js';
import { icons } from '../lib/icons.js';
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
let selectedAccountId = '';
let selectedModelId = '';
let customModelInput = '';
let modelSearchQuery = '';
let keySource: 'session' | 'key' | 'custom' = 'session';
let selectedApiKeyPrefix = '';
let customApiKey = '';

// Chat parameters
let systemInstructionsExpanded = true;
let systemInstruction = 'You are a helpful, expert AI assistant with direct, clear responses.';
let chatMessages: ChatMessage[] = [
  { id: 'msg-1', role: 'user', content: 'Hello! Please summarize what capabilities and endpoints you provide.' },
];
let composerContent = '';
let chatTemperature = 0.7;
let chatTopP: number | null = null;
let chatMaxTokens: number | null = 2048;
let chatFrequencyPenalty = 0;
let chatPresencePenalty = 0;
let chatSeed: number | null = null;
let chatStop = '';
let chatStream = true;
let chatResponseFormat: 'text' | 'json_object' = 'text';

// Image parameters
let imageMode: 'generation' | 'edit' | 'variation' = 'generation';
let imagePrompt = 'A serene futuristic digital city with neon reflections and lush trees, cinematic lighting';
let imageNegativePrompt = 'blurry, low quality, distorted, artifacts';
let imageSize = '1024x1024';
let imageQuality = 'standard';
let imageN = 1;
let imageSeed: number | null = null;
let imageAspectRatio = '1:1';
let imageDenoisingStrength = 0.6;
let imageSourceProcessing = '';
let imagePostProcessing: string[] = [];
let imageResponseFormat: 'url' | 'b64_json' = 'b64_json';
let imageSourceFile: File | null = null;
let imageMaskFile: File | null = null;
let lightboxImageUrl: string | null = null;

// Embedding parameters
let embeddingInput = 'Vector databases allow high-dimensional semantic similarity search across embeddings.';
let embeddingIsArray = false;
let embeddingDimensions: number | null = null;
let embeddingEncodingFormat: 'float' | 'base64' = 'float';

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
let streamedReasoningContent = '';
let reasoningExpanded = true;
let loadError: string | null = null;

// Helpers
function generateId(): string {
  return 'msg-' + Math.random().toString(36).substring(2, 9);
}

function estimateTokens(text: string): number {
  if (!text) return 0;
  return Math.max(1, Math.ceil(text.trim().length / 4));
}

function addChatMessage(role: 'system' | 'user' | 'assistant' = 'user', content = ''): void {
  chatMessages.push({ id: generateId(), role, content });
  requestUpdate();
}

function clearPlayground(): void {
  if (modality === 'chat') {
    chatMessages = [{ id: generateId(), role: 'user', content: '' }];
    composerContent = '';
    streamedChatContent = '';
    streamedReasoningContent = '';
  } else if (modality === 'image') {
    imagePrompt = '';
    imageNegativePrompt = '';
    imageSourceFile = null;
    imageMaskFile = null;
  } else if (modality === 'embedding') {
    embeddingInput = '';
  } else if (modality === 'audio') {
    audioFile = null;
    audioPrompt = '';
  }
  rawResponseText = '';
  parsedResponseJson = null;
  responseError = null;
  streamChunks = [];
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
  showToast('Playground cleared', 'info');
}

function removeChatMessage(id: string): void {
  chatMessages = chatMessages.filter((m) => m.id !== id);
  if (chatMessages.length === 0) {
    chatMessages.push({ id: generateId(), role: 'user', content: '' });
  }
  requestUpdate();
}

function copyText(text: string, label = 'Content'): void {
  const notify = () => {
    showToast(`${label} copied to clipboard!`, 'info');
  };

  if (navigator.clipboard && typeof navigator.clipboard.writeText === 'function') {
    navigator.clipboard
      .writeText(text)
      .then(notify)
      .catch(() => {
        fallbackCopyText(text, notify);
      });
  } else {
    fallbackCopyText(text, notify);
  }
}

function fallbackCopyText(text: string, cb?: () => void): void {
  try {
    const textArea = document.createElement('textarea');
    textArea.value = text;
    textArea.style.position = 'fixed';
    textArea.style.left = '-9999px';
    textArea.style.top = '0';
    textArea.style.opacity = '0';
    textArea.setAttribute('readonly', '');
    document.body.appendChild(textArea);
    textArea.focus();
    textArea.select();
    const successful = document.execCommand('copy');
    document.body.removeChild(textArea);
    if (successful) {
      if (cb) cb();
    } else {
      showToast('Copy failed — please copy manually', 'error');
    }
  } catch (_err) {
    showToast('Copy failed — please copy manually', 'error');
  }
}

function insertChatDirective(directive: string): void {
  if (composerContent.trim().length > 0) {
    composerContent = `${composerContent.trim()}\n${directive}`;
  } else if (chatMessages.length > 0) {
    const last = chatMessages[chatMessages.length - 1];
    if (last && last.role === 'user') {
      last.content = last.content ? `${last.content.trim()}\n${directive}` : directive;
    } else {
      composerContent = directive;
    }
  } else {
    composerContent = directive;
  }
  requestUpdate();
}

function insertImageDirective(directive: string): void {
  imagePrompt = imagePrompt.trim() ? `${imagePrompt.trim()} ${directive}` : directive;
  requestUpdate();
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
    return customApiKey.trim() || getToken() || '';
  }
  return getToken() || '';
}

function inferModelTypeFrontend(modelId: string, rawType?: string | null): ModalityType | 'rerank' {
  const t = (rawType || '').toLowerCase();
  if (t === 'image' || t === 'embedding' || t === 'audio' || t === 'rerank') {
    return t as ModalityType | 'rerank';
  }
  const idLower = modelId.toLowerCase();
  if (idLower.includes('deepgram')) return 'audio';
  if (
    idLower.includes('gpt-4o-audio') ||
    idLower.includes('gpt-4-audio') ||
    idLower.includes('qwen-audio-chat') ||
    idLower.includes('qwen2-audio-instruct') ||
    idLower.includes('stepaudio-2.5-chat') ||
    idLower.includes('stepaudio-2.5-realtime')
  ) {
    return 'chat';
  }
  if (
    idLower.includes('whisper') ||
    idLower.includes('speechify') ||
    idLower.includes('melotts') ||
    idLower.includes('melo-tts') ||
    idLower.includes('kokoro') ||
    idLower.includes('fish-audio') ||
    idLower.includes('fish-speech') ||
    idLower.includes('chattts') ||
    idLower.includes('cosyvoice') ||
    idLower.includes('openvoice') ||
    idLower.includes('parler-tts') ||
    idLower.includes('speechmatics') ||
    idLower.includes('tts-1') ||
    idLower.includes('inworld-tts') ||
    idLower.includes('elevenlabs') ||
    idLower.includes('eleven-labs') ||
    idLower.includes('eleven_multilingual') ||
    idLower.includes('stable-audio') ||
    idLower.includes('musicgen') ||
    idLower.includes('audioldm') ||
    idLower.includes('seamless-m4t') ||
    idLower.includes('sensevoice') ||
    idLower.includes('voxtral-mini-tts') ||
    idLower.includes('xai-tts') ||
    (idLower.includes('telnyx-') && idLower.includes('tts')) ||
    idLower.endsWith('-tts') ||
    idLower.endsWith('_tts') ||
    idLower.endsWith('/tts') ||
    idLower.includes('-tts-') ||
    idLower.includes('_tts_') ||
    idLower.includes('/tts-') ||
    idLower.includes('preview-tts') ||
    idLower.includes('-tts-preview') ||
    idLower.endsWith('-asr') ||
    idLower.includes('-asr-') ||
    idLower.includes('-asr')
  ) {
    return 'audio';
  }
  if (idLower.includes('rerank')) return 'rerank';
  if (
    idLower.includes('text-embedding') ||
    idLower.includes('embedding') ||
    idLower.includes('embeddings') ||
    idLower.includes('embedder') ||
    idLower.includes('model2vec') ||
    idLower.includes('bge-') ||
    idLower.includes('/bge-') ||
    idLower.includes('bge_') ||
    idLower.includes('bge.') ||
    idLower.includes('embed-qa') ||
    idLower.includes('embedcode') ||
    idLower.includes('pplx-embed') ||
    idLower.includes('mistral-embed') ||
    idLower.includes('codestral-embed') ||
    idLower.includes('arctic-embed') ||
    idLower.includes('nomic-embed') ||
    idLower.includes('voyage-embed') ||
    idLower.includes('nv-embed') ||
    idLower.includes('gte-') ||
    idLower.includes('e5-') ||
    idLower.includes('embed-v') ||
    (idLower.includes('embed') &&
      !idLower.includes('embedded-') &&
      !idLower.includes('embed_chat') &&
      !idLower.includes('embeddable'))
  ) {
    return 'embedding';
  }
  if (idLower.includes('diffusiongemma') || idLower.includes('sdft')) return 'chat';
  if (
    idLower.includes('dall-e') ||
    idLower.includes('dalle') ||
    idLower.includes('midjourney') ||
    idLower.includes('ideogram') ||
    idLower.includes('recraft') ||
    idLower.includes('flux') ||
    idLower.includes('sdxl') ||
    idLower.includes('stable-diffusion') ||
    idLower.includes('stable_diffusion') ||
    idLower.includes('stablediffusion') ||
    idLower.includes('stable-image') ||
    idLower.includes('sd-turbo') ||
    idLower.includes('sdxl-turbo') ||
    idLower.includes('sd-1.5') ||
    idLower.includes('sd-2.1') ||
    idLower.includes('sd-3') ||
    idLower.includes('sd-3.5') ||
    idLower.includes('sd3') ||
    idLower.includes('sd3.5') ||
    idLower.includes('imagen-') ||
    idLower.includes('imagen/') ||
    idLower.startsWith('imagen-') ||
    idLower === 'imagen' ||
    idLower.includes('dreamshaper') ||
    idLower.includes('pony') ||
    idLower.includes('animagine') ||
    idLower.includes('zavychroma') ||
    idLower.includes('novafast') ||
    idLower.includes('albedobase') ||
    idLower.includes('edge of realism') ||
    idLower.includes('zeipher female') ||
    idLower.includes('mhxl') ||
    idLower.includes('rag illustrious') ||
    idLower.includes('mistoon anime') ||
    idLower.includes('bb95 furry') ||
    idLower.includes('camelliamix') ||
    idLower.includes('anything v3') ||
    idLower.includes('anything v5') ||
    idLower.includes('perfect world') ||
    idLower.includes('abyss orangemix') ||
    idLower.includes('stable cascade') ||
    idLower.includes('playbookxl') ||
    idLower.includes('rundiffusion') ||
    idLower.includes('playground-v2') ||
    idLower.includes('kandinsky') ||
    idLower.includes('kolors') ||
    idLower.includes('auraflow') ||
    idLower.includes('lumina-image') ||
    idLower.includes('hunyuan-dit') ||
    idLower.includes('pixart') ||
    idLower.includes('cogview') ||
    idLower.includes('gameart') ||
    idLower.includes('art of mtg') ||
    idLower.includes('duchaiten') ||
    idLower.includes('duc haiten') ||
    idLower.includes('nai-diffusion') ||
    idLower.includes('diffusion')
  ) {
    return 'image';
  }
  return 'chat';
}

function getFilteredModels(): Array<{ id: string; name: string; type: string; provider: string; isCombo?: boolean }> {
  const models = (state.models as Model[]) || [];
  const combos = (state.combos as Combo[]) || [];
  const result: Array<{ id: string; name: string; type: string; provider: string; isCombo?: boolean }> = [];

  // Filter active models only, and by provider if selected
  const matchingModels = models
    .filter((m) => m.active !== false)
    .filter((m) => !selectedProviderId || m.provider_id === selectedProviderId);

  // Add combos if combo matches the current modality and no provider filter (combos span multiple providers)
  if (!selectedProviderId) {
    for (const c of combos) {
      const comboType = inferModelTypeFrontend(c.name, 'chat');
      if (comboType === modality) {
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

  // Filter models by modality strictly
  for (const m of matchingModels) {
    const inferredType = inferModelTypeFrontend(m.model_id, m.model_type);
    if (inferredType === modality) {
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

function ensureDefaultModel(): void {
  const models = getFilteredModels();
  if (models.length > 0) {
    const exists = models.some((m) => m.id === selectedModelId);
    if (!exists && models[0]) {
      selectedModelId = models[0].id;
    }
  }
}

async function sendImageToInpainting(b64OrUrl: string, isBase64: boolean): Promise<void> {
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
    imageSourceFile = file;
    imageMode = 'edit';
    showToast('Image loaded into Inpainting / Edit mode!', 'success');
    requestUpdate();
  } catch (err) {
    showToast(`Failed to load image for inpainting: ${err}`, 'error');
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
  const effectiveModel = selectedModelId || customModelInput.trim();
  if (!effectiveModel) {
    showToast('Please select or enter a Model Target', 'error');
    return;
  }

  // If composer has text in chat mode, commit it as a new user message before executing
  if (modality === 'chat' && composerContent.trim().length > 0) {
    chatMessages.push({ id: generateId(), role: 'user', content: composerContent.trim() });
    composerContent = '';
  }

  isLoading = true;
  responseError = null;
  rawResponseText = '';
  parsedResponseJson = null;
  streamChunks = [];
  streamedChatContent = '';
  streamedReasoningContent = '';
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
      await executeChatRequest(key, effectiveModel, startTime);
    } else if (modality === 'image') {
      await executeImageRequest(key, effectiveModel);
    } else if (modality === 'embedding') {
      await executeEmbeddingRequest(key, effectiveModel);
    } else if (modality === 'audio') {
      await executeAudioRequest(key, effectiveModel);
    }
  } catch (err: unknown) {
    if (abortController?.signal.aborted) {
      responseError = 'Request stopped by user.';
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

async function executeChatRequest(key: string, model: string, startTime: number): Promise<void> {
  const messages: Array<{ role: string; content: string }> = [];

  // Include system instructions if provided
  if (systemInstruction.trim().length > 0) {
    messages.push({ role: 'system', content: systemInstruction.trim() });
  }

  // Include thread messages
  for (const m of chatMessages) {
    if (m.content.trim().length > 0) {
      messages.push({ role: m.role, content: m.content });
    }
  }

  if (messages.length === 0) {
    throw new Error('Please add at least one message or system prompt.');
  }

  const payload: Record<string, unknown> = {
    model,
    messages,
    temperature: chatTemperature,
    stream: chatStream,
  };
  if (chatTopP !== null) {
    payload['top_p'] = chatTopP;
  }
  if (chatMaxTokens !== null && chatMaxTokens > 0) {
    payload['max_tokens'] = chatMaxTokens;
  }
  if (chatFrequencyPenalty !== 0) {
    payload['frequency_penalty'] = chatFrequencyPenalty;
  }
  if (chatPresencePenalty !== 0) {
    payload['presence_penalty'] = chatPresencePenalty;
  }
  if (chatSeed !== null) {
    payload['seed'] = chatSeed;
  }
  if (chatStop.trim()) {
    const stops = chatStop.split(',').map((s) => s.trim()).filter((s) => s.length > 0);
    if (stops.length > 0) payload['stop'] = stops.length === 1 ? stops[0] : stops;
  }
  if (chatResponseFormat === 'json_object') {
    payload['response_format'] = { type: 'json_object' };
  }

  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
  };
  if (key) {
    headers['Authorization'] = `Bearer ${key}`;
  }
  if (selectedAccountId) {
    headers['x-openproxy-account'] = selectedAccountId;
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
        if (!trimmed || trimmed.startsWith(':')) continue;

        if (trimmed.startsWith('data: ')) {
          const dataStr = trimmed.substring(6).trim();
          if (dataStr === '[DONE]') {
            continue;
          }

          try {
            const parsed = JSON.parse(dataStr);
            const delta = parsed?.choices?.[0]?.delta?.content || '';
            const reasoningDelta =
              parsed?.choices?.[0]?.delta?.reasoning_content ||
              parsed?.choices?.[0]?.delta?.reasoning ||
              parsed?.choices?.[0]?.delta?.thought ||
              parsed?.choices?.[0]?.delta?.thinking ||
              parsed?.choices?.[0]?.reasoning_content ||
              '';

            if (reasoningDelta) {
              if (firstTokenTime === null) {
                firstTokenTime = performance.now();
                currentMetrics.ttftMs = Math.round(firstTokenTime - startTime);
              }
              streamedReasoningContent += reasoningDelta;
            }

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
              delta: delta || reasoningDelta,
              timestampMs: Math.round(performance.now() - startTime),
              raw: dataStr,
            });
            requestUpdate();
          } catch {
            // ignore parse error on partial chunks
          }
        }
      }
    }
    rawResponseText = streamedChatContent;
  } else {
    const text = await response.text();
    rawResponseText = text;
    currentMetrics.payloadSizeBytes = new Blob([text]).size;

    try {
      const json = JSON.parse(text);
      parsedResponseJson = json;
      const content = json?.choices?.[0]?.message?.content || '';
      const reasoning =
        json?.choices?.[0]?.message?.reasoning_content ||
        json?.choices?.[0]?.message?.reasoning ||
        json?.choices?.[0]?.message?.thought ||
        json?.choices?.[0]?.message?.thinking ||
        json?.choices?.[0]?.reasoning_content ||
        '';
      streamedChatContent = content;
      streamedReasoningContent = reasoning;

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

async function executeImageRequest(key: string, model: string): Promise<void> {
  let endpoint = '/v1/images/generations';
  let reqInit: RequestInit;

  const headers: Record<string, string> = {};
  if (key) headers['Authorization'] = `Bearer ${key}`;
  if (selectedAccountId) headers['x-openproxy-account'] = selectedAccountId;

  if (imageMode === 'generation') {
    if (!imagePrompt.trim()) {
      throw new Error('Please enter a prompt for image generation.');
    }
    const payload: Record<string, unknown> = {
      model: model || 'dall-e-3',
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
    if (imageAspectRatio) {
      payload['aspect_ratio'] = imageAspectRatio;
    }
    if (imagePostProcessing.length > 0) {
      payload['post_processing'] = imagePostProcessing;
    }
    headers['Content-Type'] = 'application/json';
    reqInit = {
      method: 'POST',
      headers,
      body: JSON.stringify(payload),
    };
  } else if (imageMode === 'edit') {
    if (!imageSourceFile) {
      throw new Error('Please select a source image file to edit.');
    }
    if (!imagePrompt.trim()) {
      throw new Error('Please enter a prompt describing the edits.');
    }
    endpoint = '/v1/images/edits';
    const formData = new FormData();
    formData.append('image', imageSourceFile, imageSourceFile.name);
    if (imageMaskFile) {
      formData.append('mask', imageMaskFile, imageMaskFile.name);
    }
    formData.append('prompt', imagePrompt.trim());
    formData.append('model', model || 'dall-e-2');
    formData.append('n', String(imageN));
    formData.append('size', imageSize);
    formData.append('quality', imageQuality);
    formData.append('response_format', imageResponseFormat);
    formData.append('denoising_strength', String(imageDenoisingStrength));
    if (imageSourceProcessing) {
      formData.append('source_processing', imageSourceProcessing);
    }
    for (const pp of imagePostProcessing) {
      formData.append('post_processing', pp);
    }
    if (imageNegativePrompt.trim()) {
      formData.append('negative_prompt', imageNegativePrompt.trim());
    }
    if (imageSeed !== null && !isNaN(imageSeed)) {
      formData.append('seed', String(imageSeed));
    }
    reqInit = {
      method: 'POST',
      headers,
      body: formData,
    };
  } else {
    if (!imageSourceFile) {
      throw new Error('Please select a source image file to create variations.');
    }
    endpoint = '/v1/images/variations';
    const formData = new FormData();
    formData.append('image', imageSourceFile, imageSourceFile.name);
    if (imageMaskFile) {
      formData.append('mask', imageMaskFile, imageMaskFile.name);
    }
    if (imagePrompt.trim()) {
      formData.append('prompt', imagePrompt.trim());
    }
    formData.append('model', model || 'dall-e-2');
    formData.append('n', String(imageN));
    formData.append('size', imageSize);
    formData.append('quality', imageQuality);
    formData.append('response_format', imageResponseFormat);
    formData.append('denoising_strength', String(imageDenoisingStrength));
    if (imageSourceProcessing) {
      formData.append('source_processing', imageSourceProcessing);
    }
    for (const pp of imagePostProcessing) {
      formData.append('post_processing', pp);
    }
    if (imageNegativePrompt.trim()) {
      formData.append('negative_prompt', imageNegativePrompt.trim());
    }
    if (imageSeed !== null && !isNaN(imageSeed)) {
      formData.append('seed', String(imageSeed));
    }
    reqInit = {
      method: 'POST',
      headers,
      body: formData,
    };
  }

  if (abortController) {
    reqInit.signal = abortController.signal;
  }

  const response = await fetch(endpoint, reqInit);

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

async function executeEmbeddingRequest(key: string, model: string): Promise<void> {
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

  const payload: Record<string, unknown> = {
    model: model || 'text-embedding-3-small',
    input: inputData,
    encoding_format: embeddingEncodingFormat,
  };
  if (embeddingDimensions !== null && embeddingDimensions > 0) {
    payload['dimensions'] = embeddingDimensions;
  }

  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
  };
  if (key) headers['Authorization'] = `Bearer ${key}`;
  if (selectedAccountId) headers['x-openproxy-account'] = selectedAccountId;

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

async function executeAudioRequest(key: string, model: string): Promise<void> {
  if (!audioFile) {
    throw new Error('Please select an audio file to transcribe.');
  }

  const formData = new FormData();
  formData.append('file', audioFile, audioFile.name);
  formData.append('model', model || 'whisper-1');
  if (audioPrompt.trim()) formData.append('prompt', audioPrompt.trim());
  if (audioLanguage.trim()) formData.append('language', audioLanguage.trim());
  formData.append('temperature', String(audioTemperature));
  formData.append('response_format', audioResponseFormat);

  const headers: Record<string, string> = {};
  if (key) headers['Authorization'] = `Bearer ${key}`;
  if (selectedAccountId) headers['x-openproxy-account'] = selectedAccountId;

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
  const model = selectedModelId || customModelInput.trim() || 'gpt-4o';
  const accountHeader = selectedAccountId ? ` \\\n  -H "x-openproxy-account: ${selectedAccountId}"` : '';

  if (modality === 'chat') {
    const messages: Array<{ role: string; content: string }> = [];
    if (systemInstruction.trim()) messages.push({ role: 'system', content: systemInstruction.trim() });
    for (const m of chatMessages) {
      if (m.content.trim()) messages.push({ role: m.role, content: m.content });
    }
    const payload: Record<string, unknown> = {
      model,
      messages,
      temperature: chatTemperature,
      stream: chatStream,
    };
    if (chatTopP !== null) payload['top_p'] = chatTopP;
    if (chatMaxTokens !== null && chatMaxTokens > 0) payload['max_tokens'] = chatMaxTokens;
    if (chatFrequencyPenalty !== 0) payload['frequency_penalty'] = chatFrequencyPenalty;
    if (chatPresencePenalty !== 0) payload['presence_penalty'] = chatPresencePenalty;
    if (chatSeed !== null) payload['seed'] = chatSeed;
    if (chatStop.trim()) {
      const stops = chatStop.split(',').map((s) => s.trim()).filter((s) => s.length > 0);
      if (stops.length > 0) payload['stop'] = stops.length === 1 ? stops[0] : stops;
    }
    if (chatResponseFormat === 'json_object') {
      payload['response_format'] = { type: 'json_object' };
    }
    const body = JSON.stringify(payload, null, 2);
    return `curl -X POST "${host}/v1/chat/completions" \\\n  -H "Authorization: Bearer ${key}" \\\n  -H "Content-Type: application/json"${accountHeader} \\\n  -d '${body.replace(/'/g, "'\\''")}'`;
  } else if (modality === 'image') {
    if (imageMode === 'generation') {
      const payload: Record<string, unknown> = {
        model,
        prompt: imagePrompt,
        size: imageSize,
        quality: imageQuality,
        n: imageN,
        response_format: imageResponseFormat,
      };
      if (imageNegativePrompt.trim()) payload['negative_prompt'] = imageNegativePrompt.trim();
      if (imageSeed !== null && !isNaN(imageSeed)) payload['seed'] = imageSeed;
      if (imageAspectRatio) payload['aspect_ratio'] = imageAspectRatio;
      if (imagePostProcessing.length > 0) payload['post_processing'] = imagePostProcessing;
      const body = JSON.stringify(payload, null, 2);
      return `curl -X POST "${host}/v1/images/generations" \\\n  -H "Authorization: Bearer ${key}" \\\n  -H "Content-Type: application/json"${accountHeader} \\\n  -d '${body.replace(/'/g, "'\\''")}'`;
    } else if (imageMode === 'edit') {
      let cmd = `curl -X POST "${host}/v1/images/edits" \\\n  -H "Authorization: Bearer ${key}"${accountHeader} \\\n  -F "image=@${imageSourceFile ? imageSourceFile.name : 'image.png'}" \\\n  -F "prompt=${imagePrompt}" \\\n  -F "model=${model}" \\\n  -F "size=${imageSize}" \\\n  -F "quality=${imageQuality}" \\\n  -F "n=${imageN}" \\\n  -F "denoising_strength=${imageDenoisingStrength}"`;
      if (imageMaskFile) cmd += ` \\\n  -F "mask=@${imageMaskFile.name}"`;
      if (imageSourceProcessing) cmd += ` \\\n  -F "source_processing=${imageSourceProcessing}"`;
      for (const pp of imagePostProcessing) cmd += ` \\\n  -F "post_processing=${pp}"`;
      if (imageNegativePrompt.trim()) cmd += ` \\\n  -F "negative_prompt=${imageNegativePrompt.trim()}"`;
      if (imageSeed !== null && !isNaN(imageSeed)) cmd += ` \\\n  -F "seed=${imageSeed}"`;
      return cmd;
    } else {
      let cmd = `curl -X POST "${host}/v1/images/variations" \\\n  -H "Authorization: Bearer ${key}"${accountHeader} \\\n  -F "image=@${imageSourceFile ? imageSourceFile.name : 'image.png'}" \\\n  -F "model=${model}" \\\n  -F "size=${imageSize}" \\\n  -F "quality=${imageQuality}" \\\n  -F "n=${imageN}" \\\n  -F "denoising_strength=${imageDenoisingStrength}"`;
      if (imageMaskFile) cmd += ` \\\n  -F "mask=@${imageMaskFile.name}"`;
      if (imagePrompt.trim()) cmd += ` \\\n  -F "prompt=${imagePrompt.trim()}"`;
      if (imageSourceProcessing) cmd += ` \\\n  -F "source_processing=${imageSourceProcessing}"`;
      for (const pp of imagePostProcessing) cmd += ` \\\n  -F "post_processing=${pp}"`;
      if (imageNegativePrompt.trim()) cmd += ` \\\n  -F "negative_prompt=${imageNegativePrompt.trim()}"`;
      if (imageSeed !== null && !isNaN(imageSeed)) cmd += ` \\\n  -F "seed=${imageSeed}"`;
      return cmd;
    }
  } else if (modality === 'embedding') {
    const payload: Record<string, unknown> = {
      model,
      input: embeddingInput,
      encoding_format: embeddingEncodingFormat,
    };
    if (embeddingDimensions !== null && embeddingDimensions > 0) {
      payload['dimensions'] = embeddingDimensions;
    }
    const body = JSON.stringify(payload, null, 2);
    return `curl -X POST "${host}/v1/embeddings" \\\n  -H "Authorization: Bearer ${key}" \\\n  -H "Content-Type: application/json"${accountHeader} \\\n  -d '${body.replace(/'/g, "'\\''")}'`;
  } else {
    return `curl -X POST "${host}/v1/audio/transcriptions" \\\n  -H "Authorization: Bearer ${key}"${accountHeader} \\\n  -F "file=@${audioFile ? audioFile.name : 'audio.mp3'}" \\\n  -F "model=${model}"`;
  }
}

function copyCurlToClipboard(): void {
  const curl = generateCurlCommand();
  copyText(curl, 'cURL command');
}

// ---------------------------------------------------------------------------
// VIEW RENDERERS
// ---------------------------------------------------------------------------

function renderStudioHeader(): TemplateResult {
  const status = currentMetrics.statusCode;
  const isOk = status !== null && status >= 200 && status < 300;
  const isErr = (status !== null && status >= 400) || responseError !== null;

  return html`
    <div class="playground-studio-header">
      <div class="playground-studio-title-area">
        <div class="playground-title-row">
          <h2 class="playground-studio-title">Playground</h2>
          ${isLoading
            ? html`<span class="playground-live-badge live-generating"><span class="pulse-dot"></span> Generating…</span>`
            : isOk
            ? html`<span class="playground-live-badge live-done"><span class="status-dot"></span> Ready (${currentMetrics.totalLatencyMs || 0}ms)</span>`
            : isErr
            ? html`<span class="playground-live-badge live-error">${icons.warning()} ${status ? `HTTP ${status}` : 'Error'}</span>`
            : html`<span class="playground-live-badge live-idle"><span class="status-dot"></span> Idle</span>`}
        </div>
      </div>

      <!-- Segmented Modality Selector -->
      <div class="playground-segmented-control" role="tablist">
        <button
          class="segmented-item ${modality === 'chat' ? 'active' : ''}"
          @click=${() => {
            modality = 'chat';
            selectedModelId = '';
            ensureDefaultModel();
            requestUpdate();
          }}
        >
          <span class="seg-icon">${icons.chat()}</span> Chat
        </button>
        <button
          class="segmented-item ${modality === 'image' ? 'active' : ''}"
          @click=${() => {
            modality = 'image';
            selectedModelId = '';
            ensureDefaultModel();
            requestUpdate();
          }}
        >
          <span class="seg-icon">${icons.image()}</span> Image Studio
        </button>
        <button
          class="segmented-item ${modality === 'embedding' ? 'active' : ''}"
          @click=${() => {
            modality = 'embedding';
            selectedModelId = '';
            ensureDefaultModel();
            requestUpdate();
          }}
        >
          <span class="seg-icon">${icons.embedding()}</span> Embeddings
        </button>
        <button
          class="segmented-item ${modality === 'audio' ? 'active' : ''}"
          @click=${() => {
            modality = 'audio';
            selectedModelId = '';
            ensureDefaultModel();
            requestUpdate();
          }}
        >
          <span class="seg-icon">${icons.audio()}</span> Audio Transcription
        </button>
      </div>

      <!-- Quick Action Bar -->
      <div class="playground-studio-actions">
        ${isLoading
          ? html`<button class="playground-run-btn btn-danger" @click=${cancelRequest}>
              ${icons.pause()} Stop
            </button>`
          : html`<button class="playground-run-btn btn-primary" @click=${executeRequest} title="Execute Request (Ctrl+Enter)">
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

function renderChatWorkspace(): TemplateResult {
  return html`
    <div class="playground-workspace-column">
      <!-- Collapsible System Instructions -->
      <div class="playground-system-card ${systemInstructionsExpanded ? 'expanded' : 'collapsed'}">
        <div
          class="playground-system-header"
          @click=${() => {
            systemInstructionsExpanded = !systemInstructionsExpanded;
            requestUpdate();
          }}
        >
          <div class="header-left">
            <span class="system-tag">SYSTEM</span>
            <span class="system-title">System Instructions</span>
          </div>
          <button class="system-toggle-btn" type="button">
            ${systemInstructionsExpanded ? icons.caretUp() : icons.caretDown()}
          </button>
        </div>
        ${systemInstructionsExpanded
          ? html`
              <div class="playground-system-body">
                <textarea
                  class="playground-system-textarea"
                  rows="3"
                  placeholder="Enter system prompt / behavioral guidelines..."
                  .value=${systemInstruction}
                  @input=${(e: Event) => {
                    systemInstruction = (e.target as HTMLTextAreaElement).value;
                  }}
                ></textarea>
              </div>
            `
          : html``}
      </div>

      <!-- Interactive Messages Thread -->
      <div class="playground-messages-thread">
        ${chatMessages.map((msg, index) => {
          const approxTokens = estimateTokens(msg.content);
          return html`
            <div class="playground-msg-card role-${msg.role}">
              <div class="playground-msg-card-header">
                <div class="msg-card-meta">
                  <select
                    class="playground-role-badge-select select-${msg.role}"
                    .value=${msg.role}
                    @change=${(e: Event) => {
                      msg.role = (e.target as HTMLSelectElement).value as ChatMessage['role'];
                      requestUpdate();
                    }}
                  >
                    <option value="user">User</option>
                    <option value="assistant">Assistant</option>
                    <option value="system">System</option>
                  </select>
                  <span class="msg-token-badge">~${approxTokens} tokens</span>
                  <span class="msg-index-num">#${index + 1}</span>
                </div>
                <div class="msg-card-actions">
                  <button
                    class="icon-btn"
                    title="Copy message"
                    @click=${() => copyText(msg.content, 'Message')}
                  >
                    ${icons.copy()}
                  </button>
                  <button
                    class="icon-btn danger"
                    title="Delete message"
                    @click=${() => removeChatMessage(msg.id)}
                  >
                    ${icons.close()}
                  </button>
                </div>
              </div>
              <textarea
                class="playground-msg-card-textarea"
                rows=${Math.max(2, Math.min(10, Math.ceil(msg.content.length / 80)))}
                placeholder="Message content..."
                .value=${msg.content}
                @input=${(e: Event) => {
                  msg.content = (e.target as HTMLTextAreaElement).value;
                }}
              ></textarea>
            </div>
          `;
        })}
      </div>

      <!-- Directives & Prompt Helpers Bar -->
      <div class="playground-directives-bar">
        <span class="directives-label">Directives:</span>
        <button class="directive-chip" @click=${() => insertChatDirective('Respond exclusively in valid, parseable JSON.')}>
          ${icons.plus()} JSON Mode
        </button>
        <button class="directive-chip" @click=${() => insertChatDirective('Please format all code inside fenced markdown blocks with syntax highlighting.')}>
          ${icons.plus()} Code formatting
        </button>
        <button class="directive-chip" @click=${() => insertChatDirective('Use clear Markdown headers, bold highlights, and clean bullet points.')}>
          ${icons.plus()} Markdown
        </button>
        <button class="directive-chip" @click=${() => insertChatDirective('Be direct, concise, and eliminate conversational filler.')}>
          ${icons.plus()} Concise
        </button>
        <button class="directive-chip" @click=${() => insertChatDirective('Think step-by-step and provide detailed reasoning.')}>
          ${icons.plus()} Step-by-step
        </button>
        <button class="directive-chip-add" @click=${() => addChatMessage('user', '')}>
          ${icons.plus()} Add Message
        </button>
      </div>

      <!-- Modern Bottom Composer -->
      <div class="playground-composer-card">
        <textarea
          class="playground-composer-textarea"
          rows="3"
          placeholder="Type a message or instruction... (Press Ctrl+Enter to send)"
          .value=${composerContent}
          @input=${(e: Event) => {
            composerContent = (e.target as HTMLTextAreaElement).value;
          }}
          @keydown=${(e: KeyboardEvent) => {
            if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') {
              e.preventDefault();
              void executeRequest();
            }
          }}
        ></textarea>
        <div class="playground-composer-footer">
          <span class="composer-shortcut-hint">
            <kbd>Ctrl</kbd> + <kbd>Enter</kbd> to run
          </span>
          <div class="composer-actions">
            <button
              class="button small"
              @click=${() => {
                if (composerContent.trim()) {
                  chatMessages.push({ id: generateId(), role: 'user', content: composerContent.trim() });
                  composerContent = '';
                  requestUpdate();
                }
              }}
            >
              ${icons.plus()} Add to Thread
            </button>
            <button class="button primary small" @click=${executeRequest}>
              Send Prompt ${icons.send()}
            </button>
          </div>
        </div>
      </div>

      <!-- Integrated Tabbed Inspector & Response Viewer -->
      ${renderResponseInspector()}
    </div>
  `;
}

function renderImageStudioWorkspace(): TemplateResult {
  return html`
    <div class="playground-workspace-column">
      <!-- Mode Segmented Control -->
      <div class="playground-submode-bar">
        <button
          class="submode-btn ${imageMode === 'generation' ? 'active' : ''}"
          @click=${() => { imageMode = 'generation'; requestUpdate(); }}
        >
          Text-to-Image (/generations)
        </button>
        <button
          class="submode-btn ${imageMode === 'edit' ? 'active' : ''}"
          @click=${() => { imageMode = 'edit'; requestUpdate(); }}
        >
          Inpainting & Img2Img (/edits)
        </button>
        <button
          class="submode-btn ${imageMode === 'variation' ? 'active' : ''}"
          @click=${() => { imageMode = 'variation'; requestUpdate(); }}
        >
          Image Variations (/variations)
        </button>
      </div>

      <!-- File Dropzones for Inpainting / Variations -->
      ${imageMode === 'edit' || imageMode === 'variation'
        ? html`
            <div class="playground-grid-2" style="margin-bottom: var(--space-3);">
              <div class="field">
                <label class="field-label">Source Base Image (Required)</label>
                <div class="playground-file-dropzone">
                  <input
                    type="file"
                    accept="image/png, image/jpeg, image/webp"
                    @change=${(e: Event) => {
                      const input = e.target as HTMLInputElement;
                      if (input.files && input.files[0]) {
                        imageSourceFile = input.files[0];
                        requestUpdate();
                      }
                    }}
                  />
                  ${imageSourceFile
                    ? html`
                        <div class="file-loaded-info">
                          <small><strong>Loaded:</strong> ${imageSourceFile.name} (${Math.round(imageSourceFile.size / 1024)} KB)</small>
                          <button
                            class="button small danger"
                            @click=${(e: Event) => {
                              e.stopPropagation();
                              imageSourceFile = null;
                              requestUpdate();
                            }}
                          >
                            Remove
                          </button>
                        </div>
                      `
                    : html`<p class="text-muted" style="margin:0;">📁 Click or drop base image (PNG/JPEG)</p>`}
                </div>
              </div>

              <div class="field">
                <label class="field-label">Alpha Mask (Optional for Inpainting)</label>
                <div class="playground-file-dropzone">
                  <input
                    type="file"
                    accept="image/png, image/webp"
                    @change=${(e: Event) => {
                      const input = e.target as HTMLInputElement;
                      if (input.files && input.files[0]) {
                        imageMaskFile = input.files[0];
                        requestUpdate();
                      }
                    }}
                  />
                  ${imageMaskFile
                    ? html`
                        <div class="file-loaded-info">
                          <small><strong>Mask:</strong> ${imageMaskFile.name} (${Math.round(imageMaskFile.size / 1024)} KB)</small>
                          <button
                            class="button small danger"
                            @click=${(e: Event) => {
                              e.stopPropagation();
                              imageMaskFile = null;
                              requestUpdate();
                            }}
                          >
                            Remove
                          </button>
                        </div>
                      `
                    : html`<p class="text-muted" style="margin:0;">🎭 Click or drop transparency mask (PNG)</p>`}
                </div>
              </div>
            </div>
          `
        : html``}

      <!-- Prompt Editor -->
      <div class="playground-image-prompt-card">
        <label class="field-label">
          Prompt ${imageMode === 'variation' ? '(Optional Guidance)' : '(Required)'}
        </label>
        <textarea
          class="playground-image-prompt-textarea"
          rows="3"
          placeholder="A majestic cinematic dragon perched atop a neon-lit cyber tower, hyperdetailed, 8k..."
          .value=${imagePrompt}
          @input=${(e: Event) => {
            imagePrompt = (e.target as HTMLTextAreaElement).value;
          }}
        ></textarea>

        <!-- Diffusion / Horde Directive Chips -->
        <div class="playground-directives-bar" style="margin-top: var(--space-2); margin-bottom: 0;">
          <span class="directives-label">Directives:</span>
          <button class="directive-chip" @click=${() => insertImageDirective('<lora:name:1.0>')}>
            + &lt;lora:...&gt;
          </button>
          <button class="directive-chip" @click=${() => insertImageDirective('--sampler k_euler_a')}>
            + --sampler
          </button>
          <button class="directive-chip" @click=${() => insertImageDirective('--steps 30')}>
            + --steps
          </button>
          <button class="directive-chip" @click=${() => insertImageDirective('--cfg 7.5')}>
            + --cfg
          </button>
          <button class="directive-chip" @click=${() => insertImageDirective('--hires')}>
            + --hires
          </button>
          <button class="directive-chip" @click=${() => insertImageDirective('--no blurry, distorted, lowres')}>
            + --no
          </button>
          <button class="directive-chip" @click=${() => insertImageDirective('--post RealESRGAN_x4plus')}>
            + --post
          </button>
        </div>

        <div class="field" style="margin-top: var(--space-3);">
          <label class="field-label">Negative Prompt (Elements to avoid)</label>
          <input
            type="text"
            placeholder="blurry, distorted, artifacts, lowres, text, watermark..."
            .value=${imageNegativePrompt}
            @input=${(e: Event) => {
              imageNegativePrompt = (e.target as HTMLInputElement).value;
            }}
          />
        </div>
      </div>

      <!-- Lightbox Modal Viewer -->
      ${lightboxImageUrl
        ? html`
            <div class="playground-lightbox-overlay" @click=${() => { lightboxImageUrl = null; requestUpdate(); }}>
              <div class="playground-lightbox-modal" @click=${(e: Event) => e.stopPropagation()}>
                <div class="lightbox-header">
                  <span>High Resolution View</span>
                  <button class="icon-btn" @click=${() => { lightboxImageUrl = null; requestUpdate(); }}>✕</button>
                </div>
                <img src=${lightboxImageUrl} alt="High resolution preview" class="lightbox-img" />
                <div class="lightbox-footer">
                  <a class="button small primary" href=${lightboxImageUrl} download="image.png" target="_blank">Download High-Res</a>
                </div>
              </div>
            </div>
          `
        : html``}

      <!-- Integrated Response / Gallery Inspector -->
      ${renderResponseInspector()}
    </div>
  `;
}

function renderEmbeddingWorkspace(): TemplateResult {
  return html`
    <div class="playground-workspace-column">
      <div class="playground-card">
        <div class="playground-card-header">
          <h3>Embedding Input Vectorizer</h3>
          <button
            class="button small"
            @click=${() => {
              embeddingInput = 'Vector databases allow semantic similarity search across embeddings.';
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
            placeholder="Enter text strings to vectorize into dense float embeddings..."
            .value=${embeddingInput}
            @input=${(e: Event) => {
              embeddingInput = (e.target as HTMLTextAreaElement).value;
            }}
          ></textarea>
        </div>
      </div>

      ${renderResponseInspector()}
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
              : html`<p class="text-muted">${icons.audio()} Click or drag an audio file here</p>`}
          </div>
        </div>

        <div class="field" style="margin-top: var(--space-2);">
          <label class="field-label">Prompt Guide / Context Vocabulary (Optional)</label>
          <input
            type="text"
            placeholder="Optional glossary or context to guide transcription..."
            .value=${audioPrompt}
            @input=${(e: Event) => {
              audioPrompt = (e.target as HTMLInputElement).value;
            }}
          />
        </div>
      </div>

      ${renderResponseInspector()}
    </div>
  `;
}

function renderMetricsBar(): TemplateResult {
  const status = currentMetrics.statusCode;
  const isOk = status !== null && status >= 200 && status < 300;
  const isErr = (status !== null && status >= 400) || responseError !== null;

  // Tokens per second calculation
  let tokensPerSec: string | null = null;
  if (currentMetrics.completionTokens && currentMetrics.totalLatencyMs && currentMetrics.totalLatencyMs > 0) {
    const elapsedSec = (currentMetrics.totalLatencyMs - (currentMetrics.ttftMs || 0)) / 1000;
    if (elapsedSec > 0.05) {
      tokensPerSec = `${(currentMetrics.completionTokens / elapsedSec).toFixed(1)} t/s`;
    }
  }

  return html`
    <div class="playground-metrics-bar">
      <div class="playground-metric">
        <span class="metric-label">Status</span>
        <span class="status-pill ${isOk ? 'on' : isErr ? 'off' : ''}">
          ${status ? `${status} ${currentMetrics.statusText || ''}` : responseError ? 'Error' : '—'}
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

      ${tokensPerSec !== null
        ? html`
            <div class="playground-metric">
              <span class="metric-label">Speed</span>
              <span class="metric-value">${tokensPerSec}</span>
            </div>
          `
        : html``}

      ${currentMetrics.totalTokens !== null
        ? html`
            <div class="playground-metric">
              <span class="metric-label">Tokens</span>
              <span class="metric-value">
                ${currentMetrics.totalTokens}
                ${currentMetrics.promptTokens !== null ? html`<small class="text-muted">(${currentMetrics.promptTokens}p / ${currentMetrics.completionTokens || 0}c)</small>` : html``}
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

// =============================================================================
// Markdown, Math (LaTeX / KaTeX style), and Reasoning / Thinking Formatter
// =============================================================================

export interface ExtractedThinking {
  reasoning: string;
  content: string;
  isThinking: boolean;
}

export function extractThinkingProcess(
  explicitReasoning: string,
  chatContent: string,
  isLoadingChat: boolean
): ExtractedThinking {
  let reasoning = explicitReasoning || '';
  let content = chatContent || '';

  // Extract completed <think>...</think> and <thought>...</thought> tags
  const thinkTagRegex = /<(?:think|thought)>([\s\S]*?)<\/(?:think|thought)>/gi;
  let match: RegExpExecArray | null;
  while ((match = thinkTagRegex.exec(content)) !== null) {
    const matchedReasoning = (match[1] ?? '').trim();
    if (matchedReasoning) {
      reasoning = reasoning ? `${reasoning}\n\n${matchedReasoning}` : matchedReasoning;
    }
  }
  content = content.replace(thinkTagRegex, '').trim();

  // Check for unclosed <think> or <thought> tag (during streaming)
  const openTagMatch = content.match(/<(?:think|thought)>([\s\S]*)$/i);
  let unclosedThinking = false;
  if (openTagMatch) {
    unclosedThinking = true;
    const tagIndex = openTagMatch.index ?? 0;
    const remainingReasoning = openTagMatch[1] ?? '';
    content = content.substring(0, tagIndex).trim();
    reasoning = reasoning ? `${reasoning}\n\n${remainingReasoning}` : remainingReasoning;
  }

  const isThinking = isLoadingChat && (unclosedThinking || (!content && Boolean(reasoning)));

  return {
    reasoning: reasoning.trim(),
    content: content.trim(),
    isThinking,
  };
}

function escapeHtml(str: string): string {
  return str
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

const LATEX_SYMBOL_PAIRS: [string, string][] = [
  ['\\longleftrightarrow', '⟷'],
  ['\\Longleftrightarrow', '⟺'],
  ['\\longrightarrow', '⟶'],
  ['\\Longrightarrow', '⟹'],
  ['\\leftrightarrow', '↔'],
  ['\\Leftrightarrow', '⇔'],
  ['\\rightarrow', '→'],
  ['\\Rightarrow', '⇒'],
  ['\\leftarrow', '←'],
  ['\\Leftarrow', '⇐'],
  ['\\subseteq', '⊆'],
  ['\\supseteq', '⊇'],
  ['\\setminus', '∖'],
  ['\\emptyset', '∅'],
  ['\\varnothing', '∅'],
  ['\\nexists', '∄'],
  ['\\approx', '≈'],
  ['\\equiv', '≡'],
  ['\\propto', '∝'],
  ['\\notin', '∉'],
  ['\\subset', '⊂'],
  ['\\supset', '⊃'],
  ['\\forall', '∀'],
  ['\\exists', '∃'],
  ['\\partial', '∂'],
  ['\\nabla', '∇'],
  ['\\infty', '∞'],
  ['\\times', '×'],
  ['\\cdot', '·'],
  ['\\div', '÷'],
  ['\\pm', '±'],
  ['\\mp', '∓'],
  ['\\le', '≤'],
  ['\\leq', '≤'],
  ['\\ge', '≥'],
  ['\\geq', '≥'],
  ['\\ne', '≠'],
  ['\\neq', '≠'],
  ['\\ll', '≪'],
  ['\\gg', '≫'],
  ['\\in', '∈'],
  ['\\cap', '∩'],
  ['\\cup', '∪'],
  ['\\sum', '<span class="math-op">∑</span>'],
  ['\\prod', '<span class="math-op">∏</span>'],
  ['\\iint', '<span class="math-op">∬</span>'],
  ['\\iiint', '<span class="math-op">∭</span>'],
  ['\\oint', '<span class="math-op">∮</span>'],
  ['\\int', '<span class="math-op">∫</span>'],
  ['\\to', '→'],
  ['\\implies', '⇒'],
  ['\\iff', '⇔'],
  ['\\mapsto', '↦'],
  ['\\ldots', '…'],
  ['\\cdots', '⋯'],
  ['\\ddots', '⋱'],
  ['\\vdots', '⋮'],
  ['\\dots', '…'],
  ['\\alpha', 'α'],
  ['\\beta', 'β'],
  ['\\gamma', 'γ'],
  ['\\delta', 'δ'],
  ['\\epsilon', 'ε'],
  ['\\varepsilon', 'ε'],
  ['\\zeta', 'ζ'],
  ['\\eta', 'η'],
  ['\\theta', 'θ'],
  ['\\vartheta', 'ϑ'],
  ['\\iota', 'ι'],
  ['\\kappa', 'κ'],
  ['\\lambda', 'λ'],
  ['\\mu', 'μ'],
  ['\\nu', 'ν'],
  ['\\xi', 'ξ'],
  ['\\pi', 'π'],
  ['\\varpi', 'ϖ'],
  ['\\rho', 'ρ'],
  ['\\varrho', 'ϱ'],
  ['\\sigma', 'σ'],
  ['\\varsigma', 'ς'],
  ['\\tau', 'τ'],
  ['\\upsilon', 'υ'],
  ['\\phi', 'φ'],
  ['\\varphi', 'ϕ'],
  ['\\chi', 'χ'],
  ['\\psi', 'ψ'],
  ['\\omega', 'ω'],
  ['\\Gamma', 'Γ'],
  ['\\Delta', 'Δ'],
  ['\\Theta', 'Θ'],
  ['\\Lambda', 'Λ'],
  ['\\Xi', 'Ξ'],
  ['\\Pi', 'Π'],
  ['\\Sigma', 'Σ'],
  ['\\Upsilon', 'Υ'],
  ['\\Phi', 'Φ'],
  ['\\Psi', 'Ψ'],
  ['\\Omega', 'Ω'],
  ['\\deg', '°'],
  ['\\circ', '°'],
  ['\\angle', '∠'],
  ['\\perp', '⊥'],
  ['\\mid', '|'],
  ['\\parallel', '∥'],
  ['\\sim', '∼'],
  ['\\ast', '∗'],
  ['\\star', '⋆'],
];

function parseFractions(str: string): string {
  let changed = true;
  let iterations = 0;
  while (changed && iterations < 8) {
    iterations++;
    const next = str.replace(/\\(?:d?frac)\{([^{}]+)\}\{([^{}]+)\}/g, (_m, num, den) => {
      return `<span class="math-frac"><span class="math-num">${num}</span><span class="math-den">${den}</span></span>`;
    });
    changed = next !== str;
    str = next;
  }
  return str;
}

function parseSqrt(str: string): string {
  let changed = true;
  let iterations = 0;
  while (changed && iterations < 8) {
    iterations++;
    let next = str.replace(/\\sqrt\[([^{}\]]+)\]\{([^{}]+)\}/g, (_m, deg, rad) => {
      return `<span class="math-sqrt"><sup class="math-sqrt-deg">${deg}</sup><span class="math-sqrt-sign">√</span><span class="math-sqrt-radicand">${rad}</span></span>`;
    });
    next = next.replace(/\\sqrt\{([^{}]+)\}/g, (_m, rad) => {
      return `<span class="math-sqrt"><span class="math-sqrt-sign">√</span><span class="math-sqrt-radicand">${rad}</span></span>`;
    });
    changed = next !== str;
    str = next;
  }
  return str;
}

function parseScripts(str: string): string {
  // Superscripts with braces and single character
  str = str.replace(/\^{([^{}]+)}/g, '<sup>$1</sup>');
  str = str.replace(/\^([a-zA-Z0-9+\-α-ωΑ-Ω])/g, '<sup>$1</sup>');
  // Subscripts with braces and single character
  str = str.replace(/_{([^{}]+)}/g, '<sub>$1</sub>');
  str = str.replace(/_([a-zA-Z0-9+\-α-ωΑ-Ω])/g, '<sub>$1</sub>');
  return str;
}

function formatLatexMath(latex: string, isDisplay: boolean): string {
  let math = latex.trim();

  math = parseFractions(math);
  math = parseSqrt(math);
  math = parseScripts(math);

  math = math.replace(/\\text\{([^{}]+)\}/g, '<span class="math-text">$1</span>');
  math = math.replace(/\\mathbf\{([^{}]+)\}/g, '<strong>$1</strong>');
  math = math.replace(/\\mathit\{([^{}]+)\}/g, '<em>$1</em>');
  math = math.replace(/\\mathrm\{([^{}]+)\}/g, '<span class="math-rm">$1</span>');
  math = math.replace(/\\mathbb\{([^{}]+)\}/g, '<span class="math-bb">$1</span>');

  for (const [cmd, sym] of LATEX_SYMBOL_PAIRS) {
    math = math.split(cmd).join(sym);
  }

  math = math.replace(/\\,/g, '&thinsp;');
  math = math.replace(/\\;/g, '&ensp;');
  math = math.replace(/\\quad/g, '&emsp;');
  math = math.replace(/\\qquad/g, '&emsp;&emsp;');
  math = math.replace(/\\ /g, ' ');
  math = math.replace(/\\([a-zA-Z]+)/g, '$1');

  if (isDisplay) {
    return `<div class="math-block"><div class="math-inner">${math}</div></div>`;
  }
  return `<span class="math-inline">${math}</span>`;
}

function parseMarkdownTables(text: string): string {
  const lines = text.split('\n');
  const result: string[] = [];
  let i = 0;

  while (i < lines.length) {
    const line = lines[i] ?? '';
    const nextLine = lines[i + 1] ?? '';

    if (
      line.trim().startsWith('|') &&
      line.trim().endsWith('|') &&
      nextLine.trim().startsWith('|') &&
      nextLine.includes('---')
    ) {
      const headerCols = line
        .trim()
        .slice(1, -1)
        .split('|')
        .map((c) => c.trim());
      i += 2; // skip header & separator line

      const rows: string[][] = [];
      while (i < lines.length) {
        const curLine = lines[i] ?? '';
        if (!curLine.trim().startsWith('|') || !curLine.trim().endsWith('|')) {
          break;
        }
        const rowCols = curLine
          .trim()
          .slice(1, -1)
          .split('|')
          .map((c) => c.trim());
        rows.push(rowCols);
        i++;
      }

      let tableHtml = '<div class="md-table-wrap"><table class="md-table"><thead><tr>';
      for (const col of headerCols) {
        tableHtml += `<th>${col}</th>`;
      }
      tableHtml += '</tr></thead><tbody>';
      for (const row of rows) {
        tableHtml += '<tr>';
        for (let c = 0; c < headerCols.length; c++) {
          tableHtml += `<td>${row[c] !== undefined ? row[c] : ''}</td>`;
        }
        tableHtml += '</tr>';
      }
      tableHtml += '</tbody></table></div>';
      result.push(tableHtml);
    } else {
      result.push(line);
      i++;
    }
  }

  return result.join('\n');
}

function parseMarkdownLists(text: string): string {
  const lines = text.split('\n');
  const result: string[] = [];
  let i = 0;

  while (i < lines.length) {
    const line = lines[i] ?? '';
    const isUl = /^(\s*)[-*+]\s+(.*)$/.exec(line);
    const isOl = /^(\s*)\d+\.\s+(.*)$/.exec(line);

    if (isUl) {
      result.push('<ul class="md-list">');
      while (i < lines.length) {
        const cur = lines[i] ?? '';
        const ulMatch = /^(\s*)[-*+]\s+(.*)$/.exec(cur);
        if (!ulMatch) break;
        result.push(`<li>${ulMatch[2] ?? ''}</li>`);
        i++;
      }
      result.push('</ul>');
    } else if (isOl) {
      result.push('<ol class="md-list">');
      while (i < lines.length) {
        const cur = lines[i] ?? '';
        const olMatch = /^(\s*)\d+\.\s+(.*)$/.exec(cur);
        if (!olMatch) break;
        result.push(`<li>${olMatch[2] ?? ''}</li>`);
        i++;
      }
      result.push('</ol>');
    } else {
      result.push(line);
      i++;
    }
  }

  return result.join('\n');
}

function parseMarkdownBlockquotes(text: string): string {
  const lines = text.split('\n');
  const result: string[] = [];
  let i = 0;

  while (i < lines.length) {
    const line = lines[i] ?? '';
    if (/^&gt;\s?(.*)$/.test(line)) {
      const bqLines: string[] = [];
      while (i < lines.length) {
        const cur = lines[i] ?? '';
        if (!/^&gt;\s?(.*)$/.test(cur)) break;
        const m = /^&gt;\s?(.*)$/.exec(cur);
        bqLines.push(m ? (m[1] ?? '') : '');
        i++;
      }
      result.push(`<blockquote class="md-blockquote"><p>${bqLines.join('<br />')}</p></blockquote>`);
    } else {
      result.push(line);
      i++;
    }
  }

  return result.join('\n');
}

// Global code block copy handler
if (typeof window !== 'undefined') {
  const win = window as Window & typeof globalThis & { __copyPlaygroundCode?: (btn: HTMLElement) => void };
  if (!win.__copyPlaygroundCode) {
    win.__copyPlaygroundCode = (btn: HTMLElement) => {
      const encoded = btn.getAttribute('data-code') || '';
      const code = decodeURIComponent(encoded);
      const onCopied = () => {
        const originalText = btn.textContent;
        btn.textContent = 'Copied!';
        btn.classList.add('copied');
        setTimeout(() => {
          btn.textContent = originalText;
          btn.classList.remove('copied');
        }, 1600);
      };

      if (navigator.clipboard && typeof navigator.clipboard.writeText === 'function') {
        navigator.clipboard.writeText(code).then(onCopied).catch(() => {
          fallbackCopyText(code, onCopied);
        });
      } else {
        fallbackCopyText(code, onCopied);
      }
    };
  }
}

export function renderMarkdownAndMath(rawText: string): string {
  if (!rawText) return '';

  const placeholders: Map<string, string> = new Map();
  let placeholderCounter = 0;

  const createPlaceholder = (content: string, prefix = 'PH'): string => {
    const id = `@@@${prefix}_${placeholderCounter++}_${Math.random().toString(36).substring(2, 7)}@@@`;
    placeholders.set(id, content);
    return id;
  };

  // Step 1: Pre-process Code Blocks (preserve raw formatting)
  let text = rawText;
  text = text.replace(/```([a-zA-Z0-9_\-#+.]*)\n?([\s\S]*?)(?:```|$)/g, (_match, lang, code) => {
    const cleanLang = (lang || '').trim().toLowerCase();
    const cleanCode = code.replace(/\n$/, '');
    const escapedCode = escapeHtml(cleanCode);
    const encodedForCopy = encodeURIComponent(cleanCode);
    const codeBlockHtml = `<div class="md-code-block"><div class="md-code-header"><span class="md-code-lang">${escapeHtml(cleanLang || 'code')}</span><button class="md-copy-btn" type="button" onclick="window.__copyPlaygroundCode(this)" data-code="${encodedForCopy}">Copy</button></div><pre><code class="language-${escapeHtml(cleanLang || 'plaintext')}">${escapedCode}</code></pre></div>`;
    return createPlaceholder(codeBlockHtml, 'CODE');
  });

  // Step 2: Pre-process Display Math ($$...$$ and \[...\])
  text = text.replace(/(?:\$\$|\\\[)([\s\S]*?)(?:\$\$|\\\]|$)/g, (_match, mathContent) => {
    if (!mathContent.trim()) return '';
    const formatted = formatLatexMath(mathContent, true);
    return createPlaceholder(formatted, 'MATH_DISP');
  });

  // Step 3: Pre-process Inline Math ($...$ and \(...\))
  text = text.replace(/\$([^\$\s](?:[^\$]*?[^\$\s])?)\$/g, (_match, mathContent) => {
    if (/^\d+(?:\.\d+)?$/.test(mathContent.trim())) {
      return `$${mathContent}$`;
    }
    const formatted = formatLatexMath(mathContent, false);
    return createPlaceholder(formatted, 'MATH_INL');
  });
  text = text.replace(/\\\(([\s\S]*?)\\\)/g, (_match, mathContent) => {
    const formatted = formatLatexMath(mathContent, false);
    return createPlaceholder(formatted, 'MATH_INL');
  });

  // Step 4: Pre-process Inline Code (`code`)
  text = text.replace(/`([^`]+)`/g, (_match, codeContent) => {
    const escaped = escapeHtml(codeContent);
    return createPlaceholder(`<code class="md-inline-code">${escaped}</code>`, 'INLINE_CODE');
  });

  // Step 5: Escape remaining text to guarantee HTML safety
  text = escapeHtml(text);

  // Step 6: Parse Markdown Tables
  text = parseMarkdownTables(text);

  // Step 7: Parse Headings
  text = text.replace(/^#### (.*$)/gm, '<h4 class="md-h4">$1</h4>');
  text = text.replace(/^### (.*$)/gm, '<h3 class="md-h3">$1</h3>');
  text = text.replace(/^## (.*$)/gm, '<h2 class="md-h2">$1</h2>');
  text = text.replace(/^# (.*$)/gm, '<h1 class="md-h1">$1</h1>');

  // Step 8: Parse Blockquotes
  text = parseMarkdownBlockquotes(text);

  // Step 9: Parse Horizontal Rules
  text = text.replace(/^(?:---|\*\*\*|___)\s*$/gm, '<hr class="md-hr" />');

  // Step 10: Parse Lists
  text = parseMarkdownLists(text);

  // Step 11: Parse Inline Markdown (Bold, Italic, Strikethrough, Links)
  text = text.replace(/\*\*(.*?)\*\*/g, '<strong>$1</strong>');
  text = text.replace(/__(.*?)__/g, '<strong>$1</strong>');
  text = text.replace(/(^|[^\*])\*([^\*]+)\*([^\*]|$)/g, '$1<em>$2</em>$3');
  text = text.replace(/(^|[^_])_([^_]+)_([^_]|$)/g, '$1<em>$2</em>$3');
  text = text.replace(/~~(.*?)~~/g, '<del>$1</del>');
  text = text.replace(/\[([^\]]+)\]\(([^)"]+)\)/g, '<a href="$2" target="_blank" rel="noopener noreferrer" class="md-link">$1</a>');

  // Step 12: Paragraphs and Line Breaks
  const blocks = text.split(/\n\n+/);
  const formattedBlocks = blocks.map((block) => {
    const trimmed = block.trim();
    if (!trimmed) return '';
    if (
      trimmed.startsWith('<h1') ||
      trimmed.startsWith('<h2') ||
      trimmed.startsWith('<h3') ||
      trimmed.startsWith('<h4') ||
      trimmed.startsWith('<hr') ||
      trimmed.startsWith('<ul') ||
      trimmed.startsWith('<ol') ||
      trimmed.startsWith('<blockquote') ||
      trimmed.startsWith('<div class="md-table-wrap"') ||
      trimmed.startsWith('@@@CODE_') ||
      trimmed.startsWith('@@@MATH_DISP_')
    ) {
      return trimmed;
    }
    const withBreaks = trimmed.replace(/\n/g, '<br />');
    return `<p class="md-p">${withBreaks}</p>`;
  });

  let result = formattedBlocks.filter(Boolean).join('\n');

  // Step 13: Restore all Placeholders
  for (const [id, originalHtml] of placeholders.entries()) {
    result = result.split(id).join(originalHtml);
  }

  return result;
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
          <button class="button small" @click=${() => { activeResponseTab = 'raw'; requestUpdate(); }}>
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
            <button class="button small" @click=${copyCurlToClipboard}>Copy cURL</button>
          </div>
          <pre class="playground-code-view" style="margin: 0; max-height: 180px; font-size: var(--fs-xs);"><code>${curl}</code></pre>
        </div>
      </div>
    `;
  }

  if (modality === 'chat') {
    const { reasoning, content, isThinking } = extractThinkingProcess(
      streamedReasoningContent,
      streamedChatContent,
      isLoading
    );

    if (!reasoning && !content && !isLoading) {
      return html`<div class="playground-empty-response">Ready to send. Click "Run" or press Ctrl+Enter to test chat completion.</div>`;
    }

    return html`
      <div class="playground-chat-response">
        ${reasoning
          ? html`
              <div class="playground-reasoning-card ${reasoningExpanded ? 'expanded' : 'collapsed'}">
                <div
                  class="playground-reasoning-header"
                  @click=${() => {
                    reasoningExpanded = !reasoningExpanded;
                    requestUpdate();
                  }}
                  title="${reasoningExpanded ? 'Click to collapse reasoning' : 'Click to expand reasoning'}"
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
                    ${reasoningExpanded ? 'Hide' : 'Show'}
                    <span class="reasoning-chevron">${reasoningExpanded ? icons.caretUp() : icons.caretDown()}</span>
                  </button>
                </div>
                ${reasoningExpanded
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
            ${isLoading && !isThinking
              ? html`<span class="badge badge-info"><span class="pulse-dot"></span> Generating…</span>`
              : (isLoading && isThinking
                  ? html`<span class="badge badge-info"><span class="pulse-dot"></span> Reasoning…</span>`
                  : html``)}
          </div>
          <div class="bubble-content md-formatted-content">
            ${content
              ? unsafeHTML(renderMarkdownAndMath(content))
              : (isLoading
                  ? html`<span class="bubble-placeholder-text">Drafting answer…</span>`
                  : html``)}
          </div>
        </div>
      </div>
    `;
  }

  if (modality === 'image') {
    const data = (parsedResponseJson as { data?: Array<{ url?: string; b64_json?: string; revised_prompt?: string }> })?.data;
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
              <div class="img-preview-wrap" @click=${() => { lightboxImageUrl = imgSrc; requestUpdate(); }}>
                <img src=${imgSrc} alt="Generated image ${idx + 1}" loading="lazy" />
                <span class="zoom-hint">${icons.search()} Click to zoom</span>
              </div>
              ${item.revised_prompt
                ? html`<p class="image-revised-prompt"><small>${item.revised_prompt}</small></p>`
                : html``}
              <div class="image-actions">
                <button
                  class="button small"
                  @click=${() => sendImageToInpainting(item.b64_json || item.url || '', isB64)}
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

  if (modality === 'embedding') {
    const data = (parsedResponseJson as { data?: Array<{ embedding?: number[]; index?: number }> })?.data;
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

  if (modality === 'audio') {
    if (!rawResponseText && !parsedResponseJson) {
      return html`<div class="playground-empty-response">No transcription output yet. Select audio file and click "Run".</div>`;
    }

    const transcribedText = typeof parsedResponseJson === 'object' && parsedResponseJson !== null && 'text' in parsedResponseJson
      ? (parsedResponseJson as { text: string }).text
      : rawResponseText;

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

function renderResponseInspector(): TemplateResult {
  return html`
    <div class="playground-response-panel">
      <div class="playground-response-panel-header">
        <div class="detail-tabs" role="tablist" style="margin-bottom: 0;">
          <button
            class="detail-tab ${activeResponseTab === 'formatted' ? 'active' : ''}"
            @click=${() => {
              activeResponseTab = 'formatted';
              requestUpdate();
            }}
          >
            Formatted
          </button>
          <button
            class="detail-tab ${activeResponseTab === 'raw' ? 'active' : ''}"
            @click=${() => {
              activeResponseTab = 'raw';
              requestUpdate();
            }}
          >
            Raw JSON
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
                  Stream (${streamChunks.length})
                </button>
              `
            : html``}
        </div>

        <div class="playground-actions-row">
          <button
            class="button small"
            @click=${() => {
              if (rawResponseText) {
                copyText(rawResponseText, 'Raw response');
              }
            }}
            title="Copy response body"
          >
            Copy
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
                  ? html`<p class="text-muted" style="padding: var(--space-4); text-align: center;">No stream chunks captured yet.</p>`
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

// ---------------------------------------------------------------------------
// RIGHT INSPECTOR SIDEBAR (340px Fixed)
// ---------------------------------------------------------------------------

function renderInspectorSidebar(): TemplateResult {
  const providers = (state.providers as Provider[]) || [];
  const accounts = (state.accounts as Account[]) || [];
  const filteredModels = getFilteredModels();
  const apiKeys = (state.apiKeys as Array<{ id: number; label: string | null; key_prefix: string | null }>) || [];

  const matchingAccounts = selectedProviderId
    ? accounts.filter((a) => a.provider_id === selectedProviderId)
    : accounts;

  return html`
    <div class="playground-inspector-sidebar">
      <!-- Target & Auth Card -->
      <div class="playground-sidebar-card">
        <div class="playground-sidebar-header">
          <h4>Target & Authentication</h4>
          <span class="badge badge-info">${modality.toUpperCase()}</span>
        </div>

        <div class="playground-sidebar-body">
          <!-- API Key Source -->
          <div class="field">
            <label class="field-label">API Key Auth</label>
            <select
              .value=${keySource}
              @change=${(e: Event) => {
                keySource = (e.target as HTMLSelectElement).value as typeof keySource;
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

          ${keySource === 'custom'
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
            : html``}

          <!-- Provider Selector -->
          <div class="field">
            <label class="field-label">Provider</label>
            <select
              .value=${selectedProviderId}
              @change=${(e: Event) => {
                selectedProviderId = (e.target as HTMLSelectElement).value;
                if (selectedAccountId) {
                  const acc = accounts.find((a) => String(a.id) === selectedAccountId);
                  if (acc && selectedProviderId && acc.provider_id !== selectedProviderId) {
                    selectedAccountId = '';
                  }
                }
                const models = getFilteredModels();
                if (models.length > 0 && models[0]) {
                  selectedModelId = models[0].id;
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
              .value=${selectedAccountId}
              @change=${(e: Event) => {
                selectedAccountId = (e.target as HTMLSelectElement).value;
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
              ${selectedProviderId
                ? html`<button
                    class="btn-text-action"
                    @click=${async () => {
                      try {
                        showToast(`Discovering models for ${selectedProviderId}...`, 'info');
                        await api(`/providers/${encodeURIComponent(selectedProviderId)}/refresh`, { method: 'POST' });
                        state.models = (await api('/models')) as Model[];
                        state.modelsComplete = true;
                        ensureDefaultModel();
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
                .value=${modelSearchQuery}
                @input=${(e: Event) => {
                  modelSearchQuery = (e.target as HTMLInputElement).value;
                  if (modelSearchQuery.trim()) {
                    selectedModelId = modelSearchQuery.trim();
                  }
                  requestUpdate();
                }}
              />
              ${modelSearchQuery
                ? html`<button
                    type="button"
                    style="position: absolute; right: 8px; top: 50%; transform: translateY(-50%); background: none; border: none; font-size: 0.8rem; cursor: pointer; color: var(--color-text-muted);"
                    @click=${() => {
                      modelSearchQuery = '';
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
              const displayedModels = modelSearchQuery.trim()
                ? filteredModels.filter(
                    (m) =>
                      m.id.toLowerCase().includes(modelSearchQuery.trim().toLowerCase()) ||
                      m.name.toLowerCase().includes(modelSearchQuery.trim().toLowerCase()) ||
                      (m.provider || '').toLowerCase().includes(modelSearchQuery.trim().toLowerCase()),
                  )
                : filteredModels;

              return html`
                <select
                  .value=${selectedModelId}
                  @change=${(e: Event) => {
                    selectedModelId = (e.target as HTMLSelectElement).value;
                    modelSearchQuery = '';
                    requestUpdate();
                  }}
                >
                  ${displayedModels.length === 0
                    ? html`<option value=${selectedModelId || modelSearchQuery}>
                        ${selectedModelId ? `Custom: ${selectedModelId}` : 'No matching models (using input)'}
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
          ${modality === 'chat'
            ? renderChatHyperparams()
            : modality === 'image'
            ? renderImageHyperparams()
            : modality === 'embedding'
            ? renderEmbeddingHyperparams()
            : renderAudioHyperparams()}
        </div>
      </div>
    </div>
  `;
}

function renderChatHyperparams(): TemplateResult {
  return html`
    <!-- Temperature Slider + Input -->
    <div class="field">
      <div class="field-header-row">
        <label class="field-label">Temperature</label>
        <input
          type="number"
          class="compact-number-input"
          min="0"
          max="2"
          step="0.05"
          .value=${String(chatTemperature)}
          @input=${(e: Event) => {
            chatTemperature = Math.max(0, Math.min(2, parseFloat((e.target as HTMLInputElement).value) || 0));
            requestUpdate();
          }}
        />
      </div>
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

    <!-- Max Output Tokens + Presets -->
    <div class="field">
      <div class="field-header-row">
        <label class="field-label">Max Output Tokens</label>
        <input
          type="number"
          class="compact-number-input"
          placeholder="2048"
          .value=${chatMaxTokens !== null ? String(chatMaxTokens) : ''}
          @input=${(e: Event) => {
            const val = (e.target as HTMLInputElement).value;
            chatMaxTokens = val ? parseInt(val, 10) : null;
            requestUpdate();
          }}
        />
      </div>
      <div class="token-presets-row">
        <button class="preset-pill ${chatMaxTokens === 512 ? 'active' : ''}" @click=${() => { chatMaxTokens = 512; requestUpdate(); }}>512</button>
        <button class="preset-pill ${chatMaxTokens === 2048 ? 'active' : ''}" @click=${() => { chatMaxTokens = 2048; requestUpdate(); }}>2k</button>
        <button class="preset-pill ${chatMaxTokens === 4096 ? 'active' : ''}" @click=${() => { chatMaxTokens = 4096; requestUpdate(); }}>4k</button>
        <button class="preset-pill ${chatMaxTokens === 8192 ? 'active' : ''}" @click=${() => { chatMaxTokens = 8192; requestUpdate(); }}>8k</button>
      </div>
    </div>

    <!-- Top P Slider + Input -->
    <div class="field">
      <div class="field-header-row">
        <label class="field-label">Top P</label>
        <input
          type="number"
          class="compact-number-input"
          min="0"
          max="1"
          step="0.05"
          .value=${String(chatTopP ?? 1)}
          @input=${(e: Event) => {
            const v = parseFloat((e.target as HTMLInputElement).value);
            chatTopP = isNaN(v) || v >= 1 ? null : Math.max(0, v);
            requestUpdate();
          }}
        />
      </div>
      <input
        type="range"
        min="0"
        max="1"
        step="0.05"
        .value=${String(chatTopP ?? 1)}
        @input=${(e: Event) => {
          const v = parseFloat((e.target as HTMLInputElement).value);
          chatTopP = v === 1 ? null : v;
          requestUpdate();
        }}
      />
    </div>

    <!-- SSE Stream Toggle -->
    <div class="field">
      <label class="field-label">Streaming Response</label>
      <label class="playground-switch-label">
        <input
          type="checkbox"
          ?checked=${chatStream}
          @change=${(e: Event) => {
            chatStream = (e.target as HTMLInputElement).checked;
            requestUpdate();
          }}
        />
        <span>${chatStream ? 'SSE Stream Enabled' : 'Sync Single JSON'}</span>
      </label>
    </div>

    <!-- Frequency Penalty -->
    <div class="field">
      <div class="field-header-row">
        <label class="field-label">Frequency Penalty</label>
        <input
          type="number"
          class="compact-number-input"
          min="-2"
          max="2"
          step="0.1"
          .value=${String(chatFrequencyPenalty)}
          @input=${(e: Event) => {
            chatFrequencyPenalty = parseFloat((e.target as HTMLInputElement).value) || 0;
            requestUpdate();
          }}
        />
      </div>
      <input
        type="range"
        min="-2"
        max="2"
        step="0.1"
        .value=${String(chatFrequencyPenalty)}
        @input=${(e: Event) => {
          chatFrequencyPenalty = parseFloat((e.target as HTMLInputElement).value);
          requestUpdate();
        }}
      />
    </div>

    <!-- Presence Penalty -->
    <div class="field">
      <div class="field-header-row">
        <label class="field-label">Presence Penalty</label>
        <input
          type="number"
          class="compact-number-input"
          min="-2"
          max="2"
          step="0.1"
          .value=${String(chatPresencePenalty)}
          @input=${(e: Event) => {
            chatPresencePenalty = parseFloat((e.target as HTMLInputElement).value) || 0;
            requestUpdate();
          }}
        />
      </div>
      <input
        type="range"
        min="-2"
        max="2"
        step="0.1"
        .value=${String(chatPresencePenalty)}
        @input=${(e: Event) => {
          chatPresencePenalty = parseFloat((e.target as HTMLInputElement).value);
          requestUpdate();
        }}
      />
    </div>

    <!-- Response Format -->
    <div class="field">
      <label class="field-label">Response Format</label>
      <select
        .value=${chatResponseFormat}
        @change=${(e: Event) => {
          chatResponseFormat = (e.target as HTMLSelectElement).value as typeof chatResponseFormat;
          requestUpdate();
        }}
      >
        <option value="text">Text (Default)</option>
        <option value="json_object">JSON Object</option>
      </select>
    </div>

    <!-- Seed & Stop -->
    <div class="field">
      <label class="field-label">Seed (Optional)</label>
      <input
        type="number"
        placeholder="e.g. 42"
        .value=${chatSeed !== null ? String(chatSeed) : ''}
        @input=${(e: Event) => {
          const val = (e.target as HTMLInputElement).value;
          chatSeed = val ? parseInt(val, 10) : null;
        }}
      />
    </div>

    <div class="field">
      <label class="field-label">Stop Sequences</label>
      <input
        type="text"
        placeholder="e.g. \\n, END, ###"
        .value=${chatStop}
        @input=${(e: Event) => {
          chatStop = (e.target as HTMLInputElement).value;
        }}
      />
    </div>
  `;
}

function renderImageHyperparams(): TemplateResult {
  return html`
    <!-- Size / Resolution -->
    <div class="field">
      <label class="field-label">Resolution & Aspect Ratio</label>
      <select
        .value=${imageSize}
        @change=${(e: Event) => {
          imageSize = (e.target as HTMLSelectElement).value;
          requestUpdate();
        }}
      >
        <option value="1024x1024">1024x1024 (1:1 Square)</option>
        <option value="1792x1024">1792x1024 (16:9 Cinema)</option>
        <option value="1024x1792">1024x1792 (9:16 Portrait)</option>
        <option value="1024x680">1024x680 (3:2 35mm)</option>
        <option value="680x1024">680x1024 (2:3 Portrait)</option>
        <option value="1024x768">1024x768 (4:3 Standard)</option>
        <option value="768x1024">768x1024 (3:4 Document)</option>
        <option value="512x512">512x512 (Fast)</option>
      </select>
    </div>

    <!-- Aspect Ratio Parameter -->
    <div class="field">
      <label class="field-label">Aspect Ratio</label>
      <select
        .value=${imageAspectRatio}
        @change=${(e: Event) => {
          imageAspectRatio = (e.target as HTMLSelectElement).value;
          requestUpdate();
        }}
      >
        <option value="1:1">1:1 (Square)</option>
        <option value="16:9">16:9 (Landscape)</option>
        <option value="9:16">9:16 (Portrait)</option>
        <option value="3:2">3:2 (Photo)</option>
        <option value="2:3">2:3 (Photo)</option>
        <option value="4:3">4:3 (Display)</option>
        <option value="3:4">3:4 (Display)</option>
      </select>
    </div>

    <!-- Quality & Count -->
    <div class="field">
      <label class="field-label">Quality</label>
      <select
        .value=${imageQuality}
        @change=${(e: Event) => {
          imageQuality = (e.target as HTMLSelectElement).value;
          requestUpdate();
        }}
      >
        <option value="standard">Standard</option>
        <option value="hd">HD / High Detail</option>
      </select>
    </div>

    <div class="field">
      <label class="field-label">Image Count (n)</label>
      <select
        .value=${String(imageN)}
        @change=${(e: Event) => {
          imageN = parseInt((e.target as HTMLSelectElement).value, 10);
          requestUpdate();
        }}
      >
        <option value="1">1 image</option>
        <option value="2">2 images</option>
        <option value="4">4 images</option>
      </select>
    </div>

    <!-- Denoising Strength for Inpainting / Edit -->
    ${imageMode === 'edit' || imageMode === 'variation'
      ? html`
          <div class="field">
            <div class="field-header-row">
              <label class="field-label">Denoising Strength</label>
              <input
                type="number"
                class="compact-number-input"
                min="0"
                max="1"
                step="0.05"
                .value=${String(imageDenoisingStrength)}
                @input=${(e: Event) => {
                  imageDenoisingStrength = Math.max(0, Math.min(1, parseFloat((e.target as HTMLInputElement).value) || 0));
                  requestUpdate();
                }}
              />
            </div>
            <input
              type="range"
              min="0"
              max="1"
              step="0.05"
              .value=${String(imageDenoisingStrength)}
              @input=${(e: Event) => {
                imageDenoisingStrength = parseFloat((e.target as HTMLInputElement).value);
                requestUpdate();
              }}
            />
          </div>

          <div class="field">
            <label class="field-label">Source Processing</label>
            <select
              .value=${imageSourceProcessing}
              @change=${(e: Event) => {
                imageSourceProcessing = (e.target as HTMLSelectElement).value;
                requestUpdate();
              }}
            >
              <option value="">(Auto: Inpaint if mask, img2img otherwise)</option>
              <option value="img2img">img2img (Guided variation)</option>
              <option value="inpainting">inpainting (Masked replacement)</option>
              <option value="outpainting">outpainting (Canvas extension)</option>
            </select>
          </div>
        `
      : html``}

    <!-- Post-Processing / Upscalers (Cumulative Array) -->
    <div class="field">
      <div style="display: flex; justify-content: space-between; align-items: center; margin-bottom: 4px;">
        <label class="field-label" style="margin-bottom: 0;">
          Post-Processing & Upscalers ${imagePostProcessing.length > 0 ? `(${imagePostProcessing.length} active)` : ''}
        </label>
        ${imagePostProcessing.length > 0
          ? html`<button
              type="button"
              class="btn-text-action"
              @click=${() => {
                imagePostProcessing = [];
                requestUpdate();
              }}
            >
              Clear
            </button>`
          : html``}
      </div>
      <div style="display: flex; flex-direction: column; gap: 4px; margin-top: 4px;">
        ${[
          { id: 'RealESRGAN_x4plus', label: 'RealESRGAN 4x', desc: '4x Upscaler' },
          { id: 'GFPGAN', label: 'GFPGAN', desc: 'Face Restoration' },
          { id: 'CodeFormers', label: 'CodeFormers', desc: 'Face Quality Fix' },
          { id: 'NMKD_Siax', label: 'NMKD Siax', desc: 'Detail Enhancement' },
          { id: '4x_AnimeSharp', label: '4x AnimeSharp', desc: '2D / Anime Upscaler' },
        ].map((pp) => {
          const isSelected = imagePostProcessing.includes(pp.id);
          return html`
            <label
              style="display: flex; align-items: center; justify-content: space-between; gap: 8px; font-size: 0.8rem; padding: 4px 8px; border-radius: var(--radius-sm); background: ${isSelected ? 'var(--color-surface-hover, rgba(56,189,248,0.1))' : 'transparent'}; border: 1px solid ${isSelected ? 'var(--color-primary)' : 'var(--color-border)'}; cursor: pointer; user-select: none;"
            >
              <div style="display: flex; align-items: center; gap: 8px;">
                <input
                  type="checkbox"
                  .checked=${isSelected}
                  @change=${(e: Event) => {
                    const checked = (e.target as HTMLInputElement).checked;
                    if (checked) {
                      if (!imagePostProcessing.includes(pp.id)) {
                        imagePostProcessing = [...imagePostProcessing, pp.id];
                      }
                    } else {
                      imagePostProcessing = imagePostProcessing.filter((id) => id !== pp.id);
                    }
                    requestUpdate();
                  }}
                />
                <span style="font-weight: ${isSelected ? '600' : '400'}; color: ${isSelected ? 'var(--color-text-emphasis)' : 'var(--color-text)'};">
                  ${pp.label}
                </span>
              </div>
              <span style="font-size: 0.72rem; color: var(--color-text-muted);">${pp.desc}</span>
            </label>
          `;
        })}
      </div>
    </div>

    <!-- Seed & Format -->
    <div class="field">
      <label class="field-label">Deterministic Seed</label>
      <input
        type="number"
        placeholder="Random if empty"
        .value=${imageSeed !== null ? String(imageSeed) : ''}
        @input=${(e: Event) => {
          const val = (e.target as HTMLInputElement).value;
          imageSeed = val ? parseInt(val, 10) : null;
        }}
      />
    </div>

    <div class="field">
      <label class="field-label">Response Format</label>
      <select
        .value=${imageResponseFormat}
        @change=${(e: Event) => {
          imageResponseFormat = (e.target as HTMLSelectElement).value as typeof imageResponseFormat;
          requestUpdate();
        }}
      >
        <option value="b64_json">Base64 JSON (Embedded)</option>
        <option value="url">URL (External Link)</option>
      </select>
    </div>
  `;
}

function renderEmbeddingHyperparams(): TemplateResult {
  return html`
    <div class="field">
      <label class="field-label">Dimensions (Optional)</label>
      <input
        type="number"
        placeholder="e.g. 512, 1536"
        .value=${embeddingDimensions !== null ? String(embeddingDimensions) : ''}
        @input=${(e: Event) => {
          const val = (e.target as HTMLInputElement).value;
          embeddingDimensions = val ? parseInt(val, 10) : null;
        }}
      />
    </div>

    <div class="field">
      <label class="field-label">Encoding Format</label>
      <select
        .value=${embeddingEncodingFormat}
        @change=${(e: Event) => {
          embeddingEncodingFormat = (e.target as HTMLSelectElement).value as typeof embeddingEncodingFormat;
        }}
      >
        <option value="float">Float Array (Default)</option>
        <option value="base64">Base64 Encoded</option>
      </select>
    </div>
  `;
}

function renderAudioHyperparams(): TemplateResult {
  return html`
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
      <div class="field-header-row">
        <label class="field-label">Temperature</label>
        <input
          type="number"
          class="compact-number-input"
          min="0"
          max="1"
          step="0.05"
          .value=${String(audioTemperature)}
          @input=${(e: Event) => {
            audioTemperature = parseFloat((e.target as HTMLInputElement).value) || 0;
            requestUpdate();
          }}
        />
      </div>
      <input
        type="range"
        min="0"
        max="1"
        step="0.05"
        .value=${String(audioTemperature)}
        @input=${(e: Event) => {
          audioTemperature = parseFloat((e.target as HTMLInputElement).value);
          requestUpdate();
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
  `;
}

// ---------------------------------------------------------------------------
// MAIN PLAYGROUND VIEW
// ---------------------------------------------------------------------------

function renderPlayground(): TemplateResult {
  if (loadError) {
    return html`
      <div class="page-header"><h2>Playground</h2></div>
      <div class="banner banner-error">${loadError}</div>
    `;
  }

  return html`
    <div class="playground-studio-wrapper">
      <!-- Studio Header -->
      ${renderStudioHeader()}

      <!-- 2-Column Main Workspace + Inspector Layout -->
      <div class="playground-studio-grid">
        <div class="playground-main-area">
          ${modality === 'chat'
            ? renderChatWorkspace()
            : modality === 'image'
            ? renderImageStudioWorkspace()
            : modality === 'embedding'
            ? renderEmbeddingWorkspace()
            : renderAudioWorkspace()}
        </div>

        ${renderInspectorSidebar()}
      </div>
    </div>
  `;
}

// Global keyboard shortcut handler
function handleGlobalKeydown(e: KeyboardEvent): void {
  if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') {
    const active = document.activeElement;
    // If inside a text area or composer, trigger run
    if (active && (active.tagName === 'TEXTAREA' || active.tagName === 'INPUT' || active.tagName === 'BODY')) {
      e.preventDefault();
      if (!isLoading) {
        void executeRequest();
      }
    }
  }
}

// Mount function
export async function mountPlayground(): Promise<(() => void) | void> {
  loadError = null;
  window.addEventListener('keydown', handleGlobalKeydown);

  const cleanup = await createView(
    renderPlayground,
    async () => {
      const [models, providers, combos, keys, accounts] = await Promise.all([
        api('/models') as Promise<Model[]>,
        (state.providers.length === 0 ? api('/providers') : Promise.resolve(state.providers)) as Promise<Provider[]>,
        (state.combos.length === 0 ? api('/combos') : Promise.resolve(state.combos)) as Promise<Combo[]>,
        (state.apiKeys.length === 0 ? api('/keys') : Promise.resolve(state.apiKeys)) as Promise<unknown[]>,
        (state.accounts.length === 0 ? api('/accounts') : Promise.resolve(state.accounts)) as Promise<Account[]>,
      ]);
      state.models = models;
      state.modelsComplete = true;
      state.providers = providers;
      state.combos = combos;
      state.apiKeys = keys;
      state.accounts = accounts;
      ensureDefaultModel();
    },
    (msg) => { loadError = msg; },
  );

  return () => {
    window.removeEventListener('keydown', handleGlobalKeydown);
    if (cleanup) cleanup();
  };
}
