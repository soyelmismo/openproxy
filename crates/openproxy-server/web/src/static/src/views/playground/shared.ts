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

export type ModalityType = 'chat' | 'image' | 'embedding' | 'audio' | 'decision';
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

  decisionState: string;
  decisionQuestions: string;

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

    decisionState:
      'User request: Can you help me optimize this database query for high throughput?\nCombo routing targets: [coding-expert, general-chat]',
    decisionQuestions: JSON.stringify(
      {
        route: {
          type: 'choice',
          instructions: 'Select the optimal model category for this request',
          options: ['coding-expert', 'general-chat'],
        },
        complexity: {
          type: 'score',
          instructions: 'Score the complexity from 0 (trivial) to 1 (highly complex)',
        },
      },
      null,
      2,
    ),

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

const CHAT_GUARDS = ['gemini', 'gpt-', 'claude', 'qwen', 'llama', 'mistral', 'mixtral', 'deepseek', 'gemma', 'inkling', 'mimo', 'muse-spark'];
const AUDIO_EXPLICIT = ['whisper', 'tts', '-asr', '_asr', '/asr', 'deepgram', 'speechmatics', 'elevenlabs', 'fish-audio'];
const IMAGE_EXPLICIT = ['dall-e', 'imagen', 'flux', 'midjourney', 'sdxl', 'seedream', 'nano-banana', 'lucid-origin', 'grok-imagine'];
const CHAT_EXCEPTIONS = ['gpt-4o-audio', 'gpt-4-audio', 'qwen-audio-chat', 'qwen2-audio-instruct', 'stepaudio-2.5-chat', 'stepaudio-2.5-realtime', 'diffusiongemma', 'sdft'];
const AUDIO_KEYWORDS = ['whisper', 'speechify', 'melotts', 'melo-tts', 'kokoro', 'fish-audio', 'fish-speech', 'chattts', 'cosyvoice', 'openvoice', 'parler-tts', 'speechmatics', 'tts-1', 'inworld-tts', 'elevenlabs', 'eleven-labs', 'eleven_multilingual', 'stable-audio', 'musicgen', 'audioldm', 'seamless-m4t', 'sensevoice', 'voxtral-mini-tts', 'xai-tts', '-tts', '_tts', '/tts', '-tts-', '_tts_', '/tts-', 'preview-tts', '-tts-preview', '-asr', '-asr-'];
const EMBED_KEYWORDS = ['text-embedding', 'embedding', 'embeddings', 'embedder', 'model2vec', 'bge-', '/bge-', 'bge_', 'bge.', 'embed-qa', 'embedcode', 'pplx-embed', 'mistral-embed', 'codestral-embed', 'arctic-embed', 'nomic-embed', 'voyage-embed', 'nv-embed', 'gte-', 'e5-', 'embed-v'];
const DECISION_KEYWORDS = ['jev', 'laya', 'systemone', 'system-one', 'decision'];
const IMAGE_KEYWORDS = ['dall-e', 'dalle', 'midjourney', 'ideogram', 'recraft', 'flux', 'sdxl', 'stable-diffusion', 'stable_diffusion', 'stablediffusion', 'stable-image', 'sd-turbo', 'sdxl-turbo', 'sd-1.5', 'sd-2.1', 'sd-3', 'sd-3.5', 'sd3', 'sd3.5', 'imagen', 'imagen-', 'imagen/', 'dreamshaper', 'pony', 'animagine', 'zavychroma', 'novafast', 'albedobase', 'edge of realism', 'zeipher female', 'mhxl', 'rag illustrious', 'mistoon anime', 'bb95 furry', 'camelliamix', 'anything v3', 'anything v5', 'perfect world', 'abyss orangemix', 'stable cascade', 'playbookxl', 'rundiffusion', 'playground-v2', 'kandinsky', 'kolors', 'auraflow', 'lumina-image', 'hunyuan-dit', 'pixart', 'cogview', 'gameart', 'art of mtg', 'duchaiten', 'duc haiten', 'nai-diffusion', 'diffusion'];

export function inferModelTypeFrontend(
  modelId: string,
  rawType?: string | null,
): ModalityType | 'rerank' {
  const s = modelId.toLowerCase();
  const t = (rawType || '').toLowerCase();
  if (t === 'image' || t === 'embedding' || t === 'audio' || t === 'rerank' || t === 'decision') return t as ModalityType | 'rerank';
  if (DECISION_KEYWORDS.some((k) => s.includes(k))) return 'decision';
  if (CHAT_GUARDS.some((k) => s.includes(k)) && !AUDIO_EXPLICIT.some((k) => s.includes(k)) && !IMAGE_EXPLICIT.some((k) => s.includes(k))) return 'chat';
  if (s.includes('deepgram')) return 'audio';
  if (CHAT_EXCEPTIONS.some((k) => s.includes(k))) return 'chat';
  if ((s.includes('telnyx-') && s.includes('tts')) || AUDIO_KEYWORDS.some((k) => s.includes(k))) return 'audio';
  if (s.includes('rerank')) return 'rerank';
  if (EMBED_KEYWORDS.some((k) => s.includes(k)) || (s.includes('embed') && !s.includes('embedded-') && !s.includes('embed_chat') && !s.includes('embeddable'))) return 'embedding';
  if (IMAGE_KEYWORDS.some((k) => s.includes(k))) return 'image';
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