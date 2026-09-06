// views/playground/shared.ts — Shared types, state model, model-type
// inference, and playground utilities for the Playground view.
//
// Core functions that are consumed by chat.ts / image.ts / inspector.ts / index.ts.
// Heavy sub-modules have been extracted:
//   - `src/static/src/lib/markdown.ts` — pure markdown + LaTeX rendering
//   - `./hyperparams.ts` — hyperparameter UI renderers
//   - `./curl.ts` — cURL command generator and API key resolution

import { copyToClipboard } from '../../lib/clipboard.js';
import { showToast } from '../../components/toast.js';

// ==========
// Public types
// ==========

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

export interface ExtractedThinking {
  reasoning: string;
  content: string;
  isThinking: boolean;
}

// ==========
// Shared mutable state model
//
// Every module-local owned by the original monolithic view lives in this
// single object. The orchestrator (`index.ts`) creates it, `bindPlaygroundState`
// exposes it to presentation sub-modules, and the lifecycle hooks mutate it
// directly — mirroring the original module-locals 1:1.
// ==========

export interface PlaygroundState {
  modality: ModalityType;
  activeResponseTab: ResponseTab;
  selectedProviderId: string;
  selectedAccountId: string;
  selectedModelId: string;
  customModelInput: string;
  modelSearchQuery: string;
  keySource: 'session' | 'key' | 'custom';
  selectedApiKeyPrefix: string;
  customApiKey: string;

  systemInstructionsExpanded: boolean;
  systemInstruction: string;
  chatMessages: ChatMessage[];
  composerContent: string;
  chatTemperature: number;
  chatTopP: number | null;
  chatMaxTokens: number | null;
  chatFrequencyPenalty: number;
  chatPresencePenalty: number;
  chatSeed: number | null;
  chatStop: string;
  chatStream: boolean;
  chatResponseFormat: 'text' | 'json_object';

  imageMode: 'generation' | 'edit' | 'variation';
  imagePrompt: string;
  imageNegativePrompt: string;
  imageSize: string;
  imageQuality: string;
  imageN: number;
  imageSeed: number | null;
  imageAspectRatio: string;
  imageDenoisingStrength: number;
  imageSourceProcessing: string;
  imagePostProcessing: string[];
  imageResponseFormat: 'url' | 'b64_json';
  imageSourceFile: File | null;
  imageMaskFile: File | null;
  lightboxImageUrl: string | null;

  embeddingInput: string;
  embeddingIsArray: boolean;
  embeddingDimensions: number | null;
  embeddingEncodingFormat: 'float' | 'base64';

  audioFile: File | null;
  audioPrompt: string;
  audioLanguage: string;
  audioTemperature: number;
  audioResponseFormat: string;

  isLoading: boolean;
  abortController: AbortController | null;
  currentMetrics: RequestMetrics;
  responseHeaders: Record<string, string>;
  rawResponseText: string;
  parsedResponseJson: unknown;
  responseError: string | null;
  streamChunks: StreamChunkItem[];
  streamedChatContent: string;
  streamedReasoningContent: string;
  reasoningExpanded: boolean;
  loadError: string | null;
}

export function createInitialPlaygroundState(): PlaygroundState {
  return {
    modality: 'chat',
    activeResponseTab: 'formatted',
    selectedProviderId: '',
    selectedAccountId: '',
    selectedModelId: '',
    customModelInput: '',
    modelSearchQuery: '',
    keySource: 'session',
    selectedApiKeyPrefix: '',
    customApiKey: '',

    systemInstructionsExpanded: true,
    systemInstruction:
      'You are a helpful, expert AI assistant with direct, clear responses.',
    chatMessages: [
      {
        id: 'msg-1',
        role: 'user',
        content: 'Hello! Please summarize what capabilities and endpoints you provide.',
      },
    ],
    composerContent: '',
    chatTemperature: 0.7,
    chatTopP: null,
    chatMaxTokens: 2048,
    chatFrequencyPenalty: 0,
    chatPresencePenalty: 0,
    chatSeed: null,
    chatStop: '',
    chatStream: true,
    chatResponseFormat: 'text',

    imageMode: 'generation',
    imagePrompt:
      'A serene futuristic digital city with neon reflections and lush trees, cinematic lighting',
    imageNegativePrompt: 'blurry, low quality, distorted, artifacts',
    imageSize: '1024x1024',
    imageQuality: 'standard',
    imageN: 1,
    imageSeed: null,
    imageAspectRatio: '1:1',
    imageDenoisingStrength: 0.6,
    imageSourceProcessing: '',
    imagePostProcessing: [],
    imageResponseFormat: 'b64_json',
    imageSourceFile: null,
    imageMaskFile: null,
    lightboxImageUrl: null,

    embeddingInput:
      'Vector databases allow high-dimensional semantic similarity search across embeddings.',
    embeddingIsArray: false,
    embeddingDimensions: null,
    embeddingEncodingFormat: 'float',

    audioFile: null,
    audioPrompt: '',
    audioLanguage: '',
    audioTemperature: 0.0,
    audioResponseFormat: 'json',

    isLoading: false,
    abortController: null,
    currentMetrics: {
      statusCode: null,
      statusText: null,
      ttftMs: null,
      totalLatencyMs: null,
      promptTokens: null,
      completionTokens: null,
      totalTokens: null,
      payloadSizeBytes: null,
    },
    responseHeaders: {},
    rawResponseText: '',
    parsedResponseJson: null,
    responseError: null,
    streamChunks: [],
    streamedChatContent: '',
    streamedReasoningContent: '',
    reasoningExpanded: true,
    loadError: null,
  };
}

let _state: PlaygroundState | null = null;

export function bindPlaygroundState(state: PlaygroundState): void {
  _state = state;
}

export function getPlaygroundState(): PlaygroundState {
  if (!_state) {
    throw new Error('playground state not bound — call bindPlaygroundState() at module init');
  }
  return _state;
}

// ==========
// Helpers
// ==========

export function estimateTokens(text: string): number {
  if (!text) return 0;
  return Math.max(1, Math.ceil(text.trim().length / 4));
}

export function generateId(): string {
  return 'msg-' + Math.random().toString(36).substring(2, 9);
}

export function copyText(text: string, label = 'Content'): void {
  const notify = (): void => {
    showToast(`${label} copied to clipboard!`, 'info');
  };

  copyToClipboard(text)
    .then(notify)
    .catch(() => {
      showToast('Copy failed — please copy manually', 'error');
    });
}

// ==========
// Model type inference
// ==========

export function inferModelTypeFrontend(
  modelId: string,
  rawType?: string | null,
): ModalityType | 'rerank' {
  const idLower = modelId.toLowerCase();
  const isChatGuard =
    idLower.includes('gemini') ||
    idLower.includes('gpt-') ||
    idLower.includes('claude') ||
    idLower.includes('qwen') ||
    idLower.includes('llama') ||
    idLower.includes('mistral') ||
    idLower.includes('mixtral') ||
    idLower.includes('deepseek') ||
    idLower.includes('gemma') ||
    idLower.includes('inkling') ||
    idLower.includes('mimo') ||
    idLower.includes('muse-spark');
  const isAudioExplicit =
    idLower.includes('whisper') ||
    idLower.includes('tts') ||
    idLower.includes('-asr') ||
    idLower.includes('_asr') ||
    idLower.includes('/asr') ||
    idLower.includes('deepgram') ||
    idLower.includes('speechmatics') ||
    idLower.includes('elevenlabs') ||
    idLower.includes('fish-audio');
  const isImageExplicit =
    idLower.includes('dall-e') ||
    idLower.includes('imagen') ||
    idLower.includes('flux') ||
    idLower.includes('midjourney') ||
    idLower.includes('sdxl') ||
    idLower.includes('seedream') ||
    idLower.includes('nano-banana') ||
    idLower.includes('lucid-origin') ||
    idLower.includes('grok-imagine');

  if (isChatGuard && !isAudioExplicit && !isImageExplicit) {
    return 'chat';
  }

  const t = (rawType || '').toLowerCase();
  if (t === 'image' || t === 'embedding' || t === 'audio' || t === 'rerank') {
    return t as ModalityType | 'rerank';
  }
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

// ==========
// Reasoning / Thinking extraction
// ==========

export function extractThinkingProcess(
  explicitReasoning: string,
  chatContent: string,
  isLoadingChat: boolean,
): ExtractedThinking {
  let reasoning = explicitReasoning || '';
  let content = chatContent || '';

  // Extract completed  thinking... response and <thought>...</thought> tags
  const thinkTagRegex = /<(?:think|thought)>([\s\S]*?)<\/(?:think|thought)>/gi;
  let match: RegExpExecArray | null;
  while ((match = thinkTagRegex.exec(content)) !== null) {
    const matchedReasoning = (match[1] ?? '').trim();
    if (matchedReasoning) {
      reasoning = reasoning ? `${reasoning}\n\n${matchedReasoning}` : matchedReasoning;
    }
  }
  content = content.replace(thinkTagRegex, '').trim();

  // Check for unclosed  thinking or <thought> tag (during streaming)
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

// ==========
// Global code block copy handler (installed once; consumed by lib/markdown.ts
// rendered HTML in fenced code blocks).
// ==========

if (typeof window !== 'undefined') {
  const win = window as Window & typeof globalThis & { __copyPlaygroundCode?: (btn: HTMLElement) => void };
  if (!win.__copyPlaygroundCode) {
    win.__copyPlaygroundCode = (btn: HTMLElement) => {
      const encoded = btn.getAttribute('data-code') || '';
      const code = decodeURIComponent(encoded);
      const onCopied = (): void => {
        const originalText = btn.textContent;
        btn.textContent = 'Copied!';
        btn.classList.add('copied');
        setTimeout(() => {
          btn.textContent = originalText;
          btn.classList.remove('copied');
        }, 1600);
      };

      copyToClipboard(code).then(onCopied).catch(() => {
        showToast('Copy failed — please copy manually', 'error');
      });
    };
  }
}