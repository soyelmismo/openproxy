// lib/playground-loader.ts — 1:1 handoff from the live-log detail modal to the
// Playground. The modal writes the captured `request_body_json` here; the
// playground reads it on mount (and on a re-navigation) to restore messages,
// model, target and every chat hyperparameter.
//
// The bridge is a plain localStorage key, not a module singleton: the modal and
// the playground are different route views, so the value has to survive a
// full teardown/mount cycle. Key prefix matches the other openproxy keys.

import type { PlaygroundState, ChatMessageRole } from '../views/playground/shared.js';

const PLAYGROUND_PAYLOAD_KEY = 'openproxy:playground:pendingPayload';

/** Payload shape persisted by the log-detail modal. Mirrors the OpenAI
 *  `/v1/chat/completions` request body, with the fields the playground knows
 *  how to edit. Unknown keys are ignored on load. */
export interface PlaygroundHandoffPayload {
  model: string;
  messages: Array<{
    role: string;
    content: string | unknown;
    tool_call_id?: string | undefined;
    name?: string | undefined;
  }>;
  temperature?: number | undefined;
  top_p?: number | null;
  max_tokens?: number | null;
  max_completion_tokens?: number | null;
  frequency_penalty?: number | undefined;
  presence_penalty?: number | undefined;
  seed?: number | null;
  stop?: string | string[];
  response_format?: { type?: string } | null;
  stream?: boolean;
  tools?: unknown[];
  tool_choice?: unknown;
}

/** Write a handoff payload, replacing any previous one. Called by the
 *  log-detail modal's "Send to playground" button. */
export function storePlaygroundHandoff(payload: PlaygroundHandoffPayload): void {
  try {
    localStorage.setItem(PLAYGROUND_PAYLOAD_KEY, JSON.stringify(payload));
  } catch {
    // Private mode / quota exceeded: the button is a convenience, not a
    // critical path. The caller surfaces a toast on failure.
    throw new Error('Could not store the payload for the playground');
  }
}

/** Read and clear the pending handoff payload. Returns null when absent. */
export function takePlaygroundHandoff(): PlaygroundHandoffPayload | null {
  let raw: string | null = null;
  try {
    raw = localStorage.getItem(PLAYGROUND_PAYLOAD_KEY);
  } catch {
    return null;
  }
  if (!raw) return null;
  try {
    localStorage.removeItem(PLAYGROUND_PAYLOAD_KEY);
  } catch {
    // ignore
  }
  try {
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== 'object') return null;
    const p = parsed as Record<string, unknown>;
    if (typeof p['model'] !== 'string') return null;
    if (!Array.isArray(p['messages'])) return null;
    return parsed as PlaygroundHandoffPayload;
  } catch {
    return null;
  }
}

/** True when a handoff payload is waiting to be consumed. */
export function hasPlaygroundHandoff(): boolean {
  try {
    return localStorage.getItem(PLAYGROUND_PAYLOAD_KEY) !== null;
  } catch {
    return false;
  }
}

type Numeric = number | null;

function numOrNull(v: unknown): Numeric {
  return typeof v === 'number' && Number.isFinite(v) ? v : null;
}

function numOr(v: unknown, fallback: number): number {
  return typeof v === 'number' && Number.isFinite(v) ? v : fallback;
}

function strOr(v: unknown, fallback: string): string {
  return typeof v === 'string' ? v : fallback;
}

/** Flatten any message content representation (plain string, OpenAI/Anthropic
 *  multimodal parts array `[{ type: "text", text: "..." }]`, etc.) into a
 *  clean text string suitable for the playground textarea. */
export function normalizeMessageContent(raw: unknown): string {
  if (typeof raw === 'string') return raw;
  if (Array.isArray(raw)) {
    return raw
      .map((part) => {
        if (typeof part === 'string') return part;
        if (part && typeof part === 'object') {
          const p = part as Record<string, unknown>;
          if (typeof p['text'] === 'string') return p['text'];
          if (typeof p['input_text'] === 'string') return p['input_text'];
          if (typeof p['output_text'] === 'string') return p['output_text'];
          if (typeof p['content'] === 'string') return p['content'];
          if (p['type'] === 'image_url' || p['image_url'] || p['type'] === 'image') return '[Image]';
        }
        return '';
      })
      .filter((s) => s.length > 0)
      .join('\n\n');
  }
  if (raw == null) return '';
  return String(raw);
}

/** Apply a captured payload onto the playground state, preserving 1:1 the
 *  request as it went upstream. Only known-editable fields are restored;
 *  everything else keeps the playground default so the user can tweak it.
 *
 *  Model handling: the playground model field is `provider/model`, so a bare
 *  model id is prefixed with `providerId` when the caller supplies one. */
export function applyHandoffToPlaygroundState(
  st: PlaygroundState,
  payload: PlaygroundHandoffPayload,
  providerId?: string,
): void {
  // --- Messages: flatten to the playground's {role, content} shape. ---
  const validRoles: readonly ChatMessageRole[] = ['system', 'user', 'assistant', 'tool', 'developer'];

  const normalized = (payload.messages || [])
    .filter((m) => !!m && typeof m === 'object')
    .map((m) => {
      const roleStr = typeof m.role === 'string' ? m.role : 'user';
      const role: ChatMessageRole = validRoles.includes(roleStr as ChatMessageRole)
        ? (roleStr as ChatMessageRole)
        : 'user';
      const content = normalizeMessageContent(m.content);
      const tool_call_id = typeof m.tool_call_id === 'string' ? m.tool_call_id : undefined;
      const name = typeof m.name === 'string' ? m.name : undefined;
      return {
        id: `msg-${Math.random().toString(36).slice(2, 9)}`,
        role,
        content,
        tool_call_id,
        name,
      };
    });

  // Hoist the first system message to the dedicated systemInstruction field so the
  // collapsible System card renders it cleanly and it is not duplicated in the thread.
  const firstSystemIdx = normalized.findIndex((m) => m.role === 'system');
  if (firstSystemIdx !== -1 && normalized[firstSystemIdx]) {
    st.systemInstruction = normalized[firstSystemIdx].content;
    st.systemInstructionsExpanded = true;
    normalized.splice(firstSystemIdx, 1);
  } else {
    st.systemInstruction = '';
    st.systemInstructionsExpanded = false;
  }

  // If after hoisting system prompt no messages remain, give the user an empty user card
  if (normalized.length === 0) {
    normalized.push({
      id: `msg-${Math.random().toString(36).slice(2, 9)}`,
      role: 'user',
      content: '',
      tool_call_id: undefined,
      name: undefined,
    });
  }

  st.chatMessages = normalized;
  // The composer is cleared so the user does not accidentally re-send stale text.
  st.composerContent = '';

  // --- Model & provider ---
  const model = strOr(payload.model, st.selectedModelId);
  if (model.startsWith('combo:')) {
    // Combos are global targets: leave provider unselected so the combo is
    // visible in the selector list and survives ensureDefaultModel.
    st.selectedProviderId = '';
    st.selectedModelId = model;
    st.customModelInput = '';
  } else if (model.includes('/')) {
    const slashIdx = model.indexOf('/');
    const inferredProvider = model.slice(0, slashIdx);
    st.selectedProviderId = providerId || inferredProvider;
    st.selectedModelId = model;
    st.customModelInput = '';
  } else {
    const validProvider = providerId && providerId !== '—' && providerId !== 'combo' ? providerId : '';
    st.selectedProviderId = validProvider;
    st.selectedModelId = validProvider ? `${validProvider}/${model}` : model;
    st.customModelInput = model;
  }

  // --- Hyperparameters (only when present in the captured body) ---
  if (typeof payload.temperature === 'number') {
    st.chatTemperature = payload.temperature;
  }
  st.chatTopP = payload.top_p != null ? numOrNull(payload.top_p) : st.chatTopP;
  const maxTokens = payload.max_tokens ?? payload.max_completion_tokens;
  st.chatMaxTokens = maxTokens != null ? numOrNull(maxTokens) : st.chatMaxTokens;
  if (typeof payload.frequency_penalty === 'number') {
    st.chatFrequencyPenalty = payload.frequency_penalty;
  }
  if (typeof payload.presence_penalty === 'number') {
    st.chatPresencePenalty = payload.presence_penalty;
  }
  st.chatSeed = payload.seed != null ? numOrNull(payload.seed) : st.chatSeed;

  if (payload.stop != null) {
    st.chatStop = Array.isArray(payload.stop) ? payload.stop.join(', ') : String(payload.stop);
  }

  const rfType = payload.response_format?.type;
  st.chatResponseFormat = rfType === 'json_object' ? 'json_object' : 'text';

  if (typeof payload.stream === 'boolean') {
    st.chatStream = payload.stream;
  }

  // Modality is always chat: only /v1/chat/completions bodies are handed off
  // (image/audio/embedding logs have no `messages` array to restore).
  st.modality = 'chat';

  // Force a fresh response area: the previous run's output must not be shown
  // alongside the restored request.
  st.rawResponseText = '';
  st.parsedResponseJson = null;
  st.streamChunks = [];
  st.streamedChatContent = '';
  st.streamedReasoningContent = '';
  st.responseHeaders = {};
  st.responseError = null;
}

/** Convenience used by tests and the playground mount: reads the stored
 *  payload, applies it, and reports whether anything was restored. */
export function consumeHandoffIntoState(
  st: PlaygroundState,
  providerId?: string,
): boolean {
  const payload = takePlaygroundHandoff();
  if (!payload) return false;
  applyHandoffToPlaygroundState(st, payload, providerId);
  return true;
}

// Referenced by the type-only import guard: keeps the import from being elided
// in builds that tree-shake unused type imports.
export type { PlaygroundState };

// numOr is kept exported for callers that clamp restored values.
export { numOr };
