// views/playground/shared.ts — Shared types, state model, markdown/math
// rendering, model-type inference, cURL generation, and hyperparameter UI
// for the Playground view.
//
// This module centralizes everything needed by more than one of the
// playground sub-modules (chat / image / inspector / index).
//
// NOTE (LOC exceedance): this file is intentionally the largest of the five.
// It bundles the self-contained markdown+LaTeX rendering pipeline
// (`renderMarkdownAndMath` + table/list/blockquote parsers + LaTeX symbol
// map, ~600 LOC) and the four hyperparameter UI renderers (~370 LOC).
// **Suggested next extraction**: move the markdown pipeline and its
// helpers into `src/static/src/lib/markdown.ts` (per REFACTOR_SPEC Q20).
// That would bring this module below 700 LOC.

import { html, type TemplateResult } from 'lit-html';
import { state } from '../../state/index.js';
import { getToken } from '../../state/auth.js';
import { requestUpdate } from '../../state/reactive.js';
import { copyToClipboard } from '../../lib/clipboard.js';
import { showToast } from '../../components/toast.js';

// =========================================================================
// Public types
// =========================================================================

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

// =========================================================================
// Shared mutable state model
//
// Every module-local owned by the original monolithic view lives in this
// single object. The orchestrator (`index.ts`) creates it, `bindPlaygroundState`
// exposes it to presentation sub-modules, and the lifecycle hooks mutate it
// directly — mirroring the original module-locals 1:1.
// =========================================================================

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

// =========================================================================
// Helpers
// =========================================================================

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

// =========================================================================
// Model type inference
// =========================================================================

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

// =========================================================================
// Markdown / LaTeX pipeline
// =========================================================================

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
  text = text.replace(
    /\[([^\]]+)\]\(([^)"]+)\)/g,
    '<a href="$2" target="_blank" rel="noopener noreferrer" class="md-link">$1</a>',
  );

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

// =========================================================================
// cURL command generator
// =========================================================================

function getEffectiveChatModel(
  st: PlaygroundState,
): string {
  return st.selectedModelId || st.customModelInput.trim() || 'gpt-4o';
}

export function generateCurlCommand(st: PlaygroundState): string {
  const key = getEffectiveApiKeyFromState(st) || '<YOUR_API_KEY>';
  const host = window.location.origin;
  const model = getEffectiveChatModel(st);
  const accountHeader = st.selectedAccountId
    ? ` \\\n  -H "x-openproxy-account: ${st.selectedAccountId}"`
    : '';

  if (st.modality === 'chat') {
    const messages: Array<{ role: string; content: string }> = [];
    if (st.systemInstruction.trim()) {
      messages.push({ role: 'system', content: st.systemInstruction.trim() });
    }
    for (const m of st.chatMessages) {
      if (m.content.trim()) messages.push({ role: m.role, content: m.content });
    }
    const payload: Record<string, unknown> = {
      model,
      messages,
      temperature: st.chatTemperature,
      stream: st.chatStream,
    };
    if (st.chatTopP !== null) payload['top_p'] = st.chatTopP;
    if (st.chatMaxTokens !== null && st.chatMaxTokens > 0) {
      payload['max_tokens'] = st.chatMaxTokens;
    }
    if (st.chatFrequencyPenalty !== 0) payload['frequency_penalty'] = st.chatFrequencyPenalty;
    if (st.chatPresencePenalty !== 0) payload['presence_penalty'] = st.chatPresencePenalty;
    if (st.chatSeed !== null) payload['seed'] = st.chatSeed;
    if (st.chatStop.trim()) {
      const stops = st.chatStop.split(',').map((x) => x.trim()).filter((x) => x.length > 0);
      if (stops.length > 0) payload['stop'] = stops.length === 1 ? stops[0] : stops;
    }
    if (st.chatResponseFormat === 'json_object') {
      payload['response_format'] = { type: 'json_object' };
    }
    const body = JSON.stringify(payload, null, 2);
    return `curl -X POST "${host}/v1/chat/completions" \\\n  -H "Authorization: Bearer ${key}" \\\n  -H "Content-Type: application/json"${accountHeader} \\\n  -d '${body.replace(/'/g, "'\\''")}'`;
  } else if (st.modality === 'image') {
    if (st.imageMode === 'generation') {
      const payload: Record<string, unknown> = {
        model,
        prompt: st.imagePrompt,
        size: st.imageSize,
        quality: st.imageQuality,
        n: st.imageN,
        response_format: st.imageResponseFormat,
      };
      if (st.imageNegativePrompt.trim()) payload['negative_prompt'] = st.imageNegativePrompt.trim();
      if (st.imageSeed !== null && !isNaN(st.imageSeed)) payload['seed'] = st.imageSeed;
      if (st.imageAspectRatio) payload['aspect_ratio'] = st.imageAspectRatio;
      if (st.imagePostProcessing.length > 0) payload['post_processing'] = st.imagePostProcessing;
      const body = JSON.stringify(payload, null, 2);
      return `curl -X POST "${host}/v1/images/generations" \\\n  -H "Authorization: Bearer ${key}" \\\n  -H "Content-Type: application/json"${accountHeader} \\\n  -d '${body.replace(/'/g, "'\\''")}'`;
    } else if (st.imageMode === 'edit') {
      let cmd = `curl -X POST "${host}/v1/images/edits" \\\n  -H "Authorization: Bearer ${key}"${accountHeader} \\\n  -F "image=@${st.imageSourceFile ? st.imageSourceFile.name : 'image.png'}" \\\n  -F "prompt=${st.imagePrompt}" \\\n  -F "model=${model}" \\\n  -F "size=${st.imageSize}" \\\n  -F "quality=${st.imageQuality}" \\\n  -F "n=${st.imageN}" \\\n  -F "denoising_strength=${st.imageDenoisingStrength}"`;
      if (st.imageMaskFile) cmd += ` \\\n  -F "mask=@${st.imageMaskFile.name}"`;
      if (st.imageSourceProcessing) cmd += ` \\\n  -F "source_processing=${st.imageSourceProcessing}"`;
      for (const pp of st.imagePostProcessing) cmd += ` \\\n  -F "post_processing=${pp}"`;
      if (st.imageNegativePrompt.trim()) {
        cmd += ` \\\n  -F "negative_prompt=${st.imageNegativePrompt.trim()}"`;
      }
      if (st.imageSeed !== null && !isNaN(st.imageSeed)) cmd += ` \\\n  -F "seed=${st.imageSeed}"`;
      return cmd;
    } else {
      let cmd = `curl -X POST "${host}/v1/images/variations" \\\n  -H "Authorization: Bearer ${key}"${accountHeader} \\\n  -F "image=@${st.imageSourceFile ? st.imageSourceFile.name : 'image.png'}" \\\n  -F "model=${model}" \\\n  -F "size=${st.imageSize}" \\\n  -F "quality=${st.imageQuality}" \\\n  -F "n=${st.imageN}" \\\n  -F "denoising_strength=${st.imageDenoisingStrength}"`;
      if (st.imageMaskFile) cmd += ` \\\n  -F "mask=@${st.imageMaskFile.name}"`;
      if (st.imagePrompt.trim()) cmd += ` \\\n  -F "prompt=${st.imagePrompt.trim()}"`;
      if (st.imageSourceProcessing) cmd += ` \\\n  -F "source_processing=${st.imageSourceProcessing}"`;
      for (const pp of st.imagePostProcessing) cmd += ` \\\n  -F "post_processing=${pp}"`;
      if (st.imageNegativePrompt.trim()) {
        cmd += ` \\\n  -F "negative_prompt=${st.imageNegativePrompt.trim()}"`;
      }
      if (st.imageSeed !== null && !isNaN(st.imageSeed)) cmd += ` \\\n  -F "seed=${st.imageSeed}"`;
      return cmd;
    }
  } else if (st.modality === 'embedding') {
    const payload: Record<string, unknown> = {
      model,
      input: st.embeddingInput,
      encoding_format: st.embeddingEncodingFormat,
    };
    if (st.embeddingDimensions !== null && st.embeddingDimensions > 0) {
      payload['dimensions'] = st.embeddingDimensions;
    }
    const body = JSON.stringify(payload, null, 2);
    return `curl -X POST "${host}/v1/embeddings" \\\n  -H "Authorization: Bearer ${key}" \\\n  -H "Content-Type: application/json"${accountHeader} \\\n  -d '${body.replace(/'/g, "'\\''")}'`;
  } else {
    return `curl -X POST "${host}/v1/audio/transcriptions" \\\n  -H "Authorization: Bearer ${key}"${accountHeader} \\\n  -F "file=@${st.audioFile ? st.audioFile.name : 'audio.mp3'}" \\\n  -F "model=${model}"`;
  }
}

export function getEffectiveApiKeyFromState(st: PlaygroundState): string {
  if (st.keySource === 'session') {
    return getToken() || '';
  }
  if (st.keySource === 'custom') {
    return st.customApiKey.trim();
  }
  const keys = (state.apiKeys as Array<{ key_prefix?: string; id?: number; label?: string }>) || [];
  const found = keys.find((k) => k.key_prefix === st.selectedApiKeyPrefix);
  if (found && found.key_prefix) {
    return st.customApiKey.trim() || getToken() || '';
  }
  return getToken() || '';
}

// =========================================================================
// Hyperparameter UI renderers
// =========================================================================

export function renderChatHyperparams(st: PlaygroundState): TemplateResult {
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
          .value=${String(st.chatTemperature)}
          @input=${(e: Event) => {
            st.chatTemperature = Math.max(0, Math.min(2, parseFloat((e.target as HTMLInputElement).value) || 0));
            requestUpdate();
          }}
        />
      </div>
      <input
        type="range"
        min="0"
        max="2"
        step="0.05"
        .value=${String(st.chatTemperature)}
        @input=${(e: Event) => {
          st.chatTemperature = parseFloat((e.target as HTMLInputElement).value);
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
          .value=${st.chatMaxTokens !== null ? String(st.chatMaxTokens) : ''}
          @input=${(e: Event) => {
            const val = (e.target as HTMLInputElement).value;
            st.chatMaxTokens = val ? parseInt(val, 10) : null;
            requestUpdate();
          }}
        />
      </div>
      <div class="token-presets-row">
        <button class="preset-pill ${st.chatMaxTokens === 512 ? 'active' : ''}" @click=${() => { st.chatMaxTokens = 512; requestUpdate(); }}>512</button>
        <button class="preset-pill ${st.chatMaxTokens === 2048 ? 'active' : ''}" @click=${() => { st.chatMaxTokens = 2048; requestUpdate(); }}>2k</button>
        <button class="preset-pill ${st.chatMaxTokens === 4096 ? 'active' : ''}" @click=${() => { st.chatMaxTokens = 4096; requestUpdate(); }}>4k</button>
        <button class="preset-pill ${st.chatMaxTokens === 8192 ? 'active' : ''}" @click=${() => { st.chatMaxTokens = 8192; requestUpdate(); }}>8k</button>
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
          .value=${String(st.chatTopP ?? 1)}
          @input=${(e: Event) => {
            const v = parseFloat((e.target as HTMLInputElement).value);
            st.chatTopP = isNaN(v) || v >= 1 ? null : Math.max(0, v);
            requestUpdate();
          }}
        />
      </div>
      <input
        type="range"
        min="0"
        max="1"
        step="0.05"
        .value=${String(st.chatTopP ?? 1)}
        @input=${(e: Event) => {
          const v = parseFloat((e.target as HTMLInputElement).value);
          st.chatTopP = v === 1 ? null : v;
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
          ?checked=${st.chatStream}
          @change=${(e: Event) => {
            st.chatStream = (e.target as HTMLInputElement).checked;
            requestUpdate();
          }}
        />
        <span>${st.chatStream ? 'SSE Stream Enabled' : 'Sync Single JSON'}</span>
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
          .value=${String(st.chatFrequencyPenalty)}
          @input=${(e: Event) => {
            st.chatFrequencyPenalty = parseFloat((e.target as HTMLInputElement).value) || 0;
            requestUpdate();
          }}
        />
      </div>
      <input
        type="range"
        min="-2"
        max="2"
        step="0.1"
        .value=${String(st.chatFrequencyPenalty)}
        @input=${(e: Event) => {
          st.chatFrequencyPenalty = parseFloat((e.target as HTMLInputElement).value);
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
          .value=${String(st.chatPresencePenalty)}
          @input=${(e: Event) => {
            st.chatPresencePenalty = parseFloat((e.target as HTMLInputElement).value) || 0;
            requestUpdate();
          }}
        />
      </div>
      <input
        type="range"
        min="-2"
        max="2"
        step="0.1"
        .value=${String(st.chatPresencePenalty)}
        @input=${(e: Event) => {
          st.chatPresencePenalty = parseFloat((e.target as HTMLInputElement).value);
          requestUpdate();
        }}
      />
    </div>

    <!-- Response Format -->
    <div class="field">
      <label class="field-label">Response Format</label>
      <select
        .value=${st.chatResponseFormat}
        @change=${(e: Event) => {
          st.chatResponseFormat = (e.target as HTMLSelectElement).value as typeof st.chatResponseFormat;
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
        .value=${st.chatSeed !== null ? String(st.chatSeed) : ''}
        @input=${(e: Event) => {
          const val = (e.target as HTMLInputElement).value;
          st.chatSeed = val ? parseInt(val, 10) : null;
        }}
      />
    </div>

    <div class="field">
      <label class="field-label">Stop Sequences</label>
      <input
        type="text"
        placeholder="e.g. \\n, END, ###"
        .value=${st.chatStop}
        @input=${(e: Event) => {
          st.chatStop = (e.target as HTMLInputElement).value;
        }}
      />
    </div>
  `;
}

export function renderImageHyperparams(st: PlaygroundState): TemplateResult {
  return html`
    <!-- Size / Resolution -->
    <div class="field">
      <label class="field-label">Resolution & Aspect Ratio</label>
      <select
        .value=${st.imageSize}
        @change=${(e: Event) => {
          st.imageSize = (e.target as HTMLSelectElement).value;
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
        .value=${st.imageAspectRatio}
        @change=${(e: Event) => {
          st.imageAspectRatio = (e.target as HTMLSelectElement).value;
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
        .value=${st.imageQuality}
        @change=${(e: Event) => {
          st.imageQuality = (e.target as HTMLSelectElement).value;
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
        .value=${String(st.imageN)}
        @change=${(e: Event) => {
          st.imageN = parseInt((e.target as HTMLSelectElement).value, 10);
          requestUpdate();
        }}
      >
        <option value="1">1 image</option>
        <option value="2">2 images</option>
        <option value="4">4 images</option>
      </select>
    </div>

    <!-- Denoising Strength for Inpainting / Edit -->
    ${st.imageMode === 'edit' || st.imageMode === 'variation'
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
                .value=${String(st.imageDenoisingStrength)}
                @input=${(e: Event) => {
                  st.imageDenoisingStrength = Math.max(0, Math.min(1, parseFloat((e.target as HTMLInputElement).value) || 0));
                  requestUpdate();
                }}
              />
            </div>
            <input
              type="range"
              min="0"
              max="1"
              step="0.05"
              .value=${String(st.imageDenoisingStrength)}
              @input=${(e: Event) => {
                st.imageDenoisingStrength = parseFloat((e.target as HTMLInputElement).value);
                requestUpdate();
              }}
            />
          </div>

          <div class="field">
            <label class="field-label">Source Processing</label>
            <select
              .value=${st.imageSourceProcessing}
              @change=${(e: Event) => {
                st.imageSourceProcessing = (e.target as HTMLSelectElement).value;
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
          Post-Processing & Upscalers ${st.imagePostProcessing.length > 0 ? `(${st.imagePostProcessing.length} active)` : ''}
        </label>
        ${st.imagePostProcessing.length > 0
          ? html`<button
              type="button"
              class="btn-text-action"
              @click=${() => {
                st.imagePostProcessing = [];
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
          const isSelected = st.imagePostProcessing.includes(pp.id);
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
                      if (!st.imagePostProcessing.includes(pp.id)) {
                        st.imagePostProcessing = [...st.imagePostProcessing, pp.id];
                      }
                    } else {
                      st.imagePostProcessing = st.imagePostProcessing.filter((id) => id !== pp.id);
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
        .value=${st.imageSeed !== null ? String(st.imageSeed) : ''}
        @input=${(e: Event) => {
          const val = (e.target as HTMLInputElement).value;
          st.imageSeed = val ? parseInt(val, 10) : null;
        }}
      />
    </div>

    <div class="field">
      <label class="field-label">Response Format</label>
      <select
        .value=${st.imageResponseFormat}
        @change=${(e: Event) => {
          st.imageResponseFormat = (e.target as HTMLSelectElement).value as typeof st.imageResponseFormat;
          requestUpdate();
        }}
      >
        <option value="b64_json">Base64 JSON (Embedded)</option>
        <option value="url">URL (External Link)</option>
      </select>
    </div>
  `;
}

export function renderEmbeddingHyperparams(st: PlaygroundState): TemplateResult {
  return html`
    <div class="field">
      <label class="field-label">Dimensions (Optional)</label>
      <input
        type="number"
        placeholder="e.g. 512, 1536"
        .value=${st.embeddingDimensions !== null ? String(st.embeddingDimensions) : ''}
        @input=${(e: Event) => {
          const val = (e.target as HTMLInputElement).value;
          st.embeddingDimensions = val ? parseInt(val, 10) : null;
        }}
      />
    </div>

    <div class="field">
      <label class="field-label">Encoding Format</label>
      <select
        .value=${st.embeddingEncodingFormat}
        @change=${(e: Event) => {
          st.embeddingEncodingFormat = (e.target as HTMLSelectElement).value as typeof st.embeddingEncodingFormat;
        }}
      >
        <option value="float">Float Array (Default)</option>
        <option value="base64">Base64 Encoded</option>
      </select>
    </div>
  `;
}

export function renderAudioHyperparams(st: PlaygroundState): TemplateResult {
  return html`
    <div class="field">
      <label class="field-label">Language (ISO-639-1)</label>
      <input
        type="text"
        placeholder="e.g. en, es, fr..."
        .value=${st.audioLanguage}
        @input=${(e: Event) => {
          st.audioLanguage = (e.target as HTMLInputElement).value;
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
          .value=${String(st.audioTemperature)}
          @input=${(e: Event) => {
            st.audioTemperature = parseFloat((e.target as HTMLInputElement).value) || 0;
            requestUpdate();
          }}
        />
      </div>
      <input
        type="range"
        min="0"
        max="1"
        step="0.05"
        .value=${String(st.audioTemperature)}
        @input=${(e: Event) => {
          st.audioTemperature = parseFloat((e.target as HTMLInputElement).value);
          requestUpdate();
        }}
      />
    </div>

    <div class="field">
      <label class="field-label">Response Format</label>
      <select
        .value=${st.audioResponseFormat}
        @change=${(e: Event) => {
          st.audioResponseFormat = (e.target as HTMLSelectElement).value;
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
