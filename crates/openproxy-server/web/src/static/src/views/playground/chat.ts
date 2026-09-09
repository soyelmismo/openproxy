// views/playground/chat.ts — Chat modality workspace.
//
// Renders the chat-specific main area: system instructions card, interactive
// message thread, directive chips, bottom composer with Ctrl+Enter, and
// delegates the inline response inspector to the inspector module.
//
// Owns `executeChatRequest` which performs the actual /v1/chat/completions
// call, including SSE streaming and reasoning-content extraction.
//
// All persistent state lives in the PlaygroundState object (shared.ts);
// this module reads and mutates it via the single mutable reference.

import { html, type TemplateResult } from 'lit-html';
import { requestUpdate } from '../../state/reactive.js';
import { icons } from '../../lib/icons.js';
import { t } from '../../i18n/index.js';
import type { PlaygroundState } from './shared.js';
import {
  copyText,
  estimateTokens,
  generateId,
} from './shared.js';
import { renderResponseInspector } from './inspector.js';

// For Ctrl+Enter in composer — dispatch a custom event that index.ts listens to.
const FIRE_RUN_EVENT = (): void => {
  window.dispatchEvent(new CustomEvent('playground:run'));
};

export function renderChatWorkspace(st: PlaygroundState): TemplateResult {
  return html`
    <div class="playground-workspace-column">
      <!-- Collapsible System Instructions -->
      <div class="playground-system-card ${st.systemInstructionsExpanded ? 'expanded' : 'collapsed'}">
        <div
          class="playground-system-header"
          @click=${() => {
            st.systemInstructionsExpanded = !st.systemInstructionsExpanded;
            requestUpdate();
          }}
        >
          <div class="header-left">
            <span class="system-tag">${t('playground.chat.system')}</span>
            <span class="system-title">${t('playground.chat.system_instructions')}</span>
          </div>
          <button class="system-toggle-btn" type="button">
            ${st.systemInstructionsExpanded ? icons.caretUp() : icons.caretDown()}
          </button>
        </div>
        ${st.systemInstructionsExpanded
          ? html`
              <div class="playground-system-body">
                <textarea
                  class="playground-system-textarea"
                  rows="3"
                  placeholder=${t('playground.chat.system_placeholder')}
                  .value=${st.systemInstruction}
                  @input=${(e: Event) => {
                    st.systemInstruction = (e.target as HTMLTextAreaElement).value;
                  }}
                ></textarea>
              </div>
            `
          : html``}
      </div>

      <!-- Interactive Messages Thread -->
      <div class="playground-messages-thread">
        ${st.chatMessages.map((msg, index) => {
          const approxTokens = estimateTokens(msg.content);
          return html`
            <div class="playground-msg-card role-${msg.role}">
              <div class="playground-msg-card-header">
                <div class="msg-card-meta">
                  <select
                    class="playground-role-badge-select select-${msg.role}"
                    .value=${msg.role}
                    @change=${(e: Event) => {
                      msg.role = (e.target as HTMLSelectElement).value as 'system' | 'user' | 'assistant';
                      requestUpdate();
                    }}
                  >
                     <option value="user">${t('playground.chat.user')}</option>
                     <option value="assistant">${t('playground.chat.assistant')}</option>
                     <option value="system">${t('playground.chat.system')}</option>
                  </select>
                  <span class="msg-token-badge">~${approxTokens} tokens</span>
                  <span class="msg-index-num">#${index + 1}</span>
                </div>
                <div class="msg-card-actions">
                  <button
                    class="icon-btn"
                     title=${t('playground.chat.copy_message')}
                     @click=${() => copyText(msg.content, 'Message')}
                  >
                    ${icons.copy()}
                  </button>
                  <button
                    class="icon-btn danger"
                     title=${t('playground.chat.delete_message')}
                    @click=${() => {
                      st.chatMessages = st.chatMessages.filter((m) => m.id !== msg.id);
                      if (st.chatMessages.length === 0) {
                        st.chatMessages.push({ id: generateId(), role: 'user', content: '' });
                      }
                      requestUpdate();
                    }}
                  >
                    ${icons.close()}
                  </button>
                </div>
              </div>
              <textarea
                class="playground-msg-card-textarea"
                rows=${Math.max(2, Math.min(10, Math.ceil(msg.content.length / 80)))}
                 placeholder=${t('playground.chat.message_placeholder')}
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
         <span class="directives-label">${t('playground.chat.directives_label')}</span>
        <button class="directive-chip" @click=${() => insertChatDirective(st, 'Respond exclusively in valid, parseable JSON.')}>
           ${icons.plus()} ${t('playground.chat.directive_json')}
        </button>
        <button class="directive-chip" @click=${() => insertChatDirective(st, 'Please format all code inside fenced markdown blocks with syntax highlighting.')}>
           ${icons.plus()} ${t('playground.chat.directive_code')}
        </button>
        <button class="directive-chip" @click=${() => insertChatDirective(st, 'Use clear Markdown headers, bold highlights, and clean bullet points.')}>
           ${icons.plus()} ${t('playground.chat.directive_markdown')}
        </button>
        <button class="directive-chip" @click=${() => insertChatDirective(st, 'Be direct, concise, and eliminate conversational filler.')}>
           ${icons.plus()} ${t('playground.chat.directive_concise')}
        </button>
        <button class="directive-chip" @click=${() => insertChatDirective(st, 'Think step-by-step and provide detailed reasoning.')}>
           ${icons.plus()} ${t('playground.chat.directive_step_by_step')}
        </button>
        <button class="directive-chip-add" @click=${() => {
          st.chatMessages.push({ id: generateId(), role: 'user', content: '' });
          requestUpdate();
        }}>
           ${icons.plus()} ${t('playground.chat.add_message')}
        </button>
      </div>

      <!-- Modern Bottom Composer -->
      <div class="playground-composer-card">
        <textarea
          class="playground-composer-textarea"
          rows="3"
           placeholder=${t('playground.chat.composer_placeholder')}
          .value=${st.composerContent}
          @input=${(e: Event) => {
            st.composerContent = (e.target as HTMLTextAreaElement).value;
          }}
          @keydown=${(e: KeyboardEvent) => {
            if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') {
              e.preventDefault();
              FIRE_RUN_EVENT();
            }
          }}
        ></textarea>
        <div class="playground-composer-footer">
             <span class="composer-shortcut-hint">
             <kbd>Ctrl</kbd> + <kbd>Enter</kbd> ${t('playground.chat.composer_hint')}
           </span>
          <div class="composer-actions">
            <button
              class="button small"
              @click=${() => {
                if (st.composerContent.trim()) {
                  st.chatMessages.push({ id: generateId(), role: 'user', content: st.composerContent.trim() });
                  st.composerContent = '';
                  requestUpdate();
                }
              }}
            >
               ${icons.plus()} ${t('playground.chat.add_to_thread')}
            </button>
            <button class="button primary small" @click=${FIRE_RUN_EVENT}>
               ${t('playground.chat.send_prompt')} ${icons.send()}
            </button>
          </div>
        </div>
      </div>

      <!-- Integrated Tabbed Inspector & Response Viewer -->
      ${renderResponseInspector(st)}
    </div>
  `;
}

function insertChatDirective(st: PlaygroundState, directive: string): void {
  if (st.composerContent.trim().length > 0) {
    st.composerContent = `${st.composerContent.trim()}\n${directive}`;
  } else if (st.chatMessages.length > 0) {
    const last = st.chatMessages[st.chatMessages.length - 1];
    if (last && last.role === 'user') {
      last.content = last.content ? `${last.content.trim()}\n${directive}` : directive;
    } else {
      st.composerContent = directive;
    }
  } else {
    st.composerContent = directive;
  }
  requestUpdate();
}

// =====================================================================
// executeChatRequest — chat completion call with optional SSE streaming.
// =====================================================================

export async function executeChatRequest(
  st: PlaygroundState,
  key: string,
  model: string,
  startTime: number,
): Promise<void> {
  const messages: Array<{ role: string; content: string }> = [];

  if (st.systemInstruction.trim().length > 0) {
    messages.push({ role: 'system', content: st.systemInstruction.trim() });
  }

  for (const m of st.chatMessages) {
    if (m.content.trim().length > 0) {
      messages.push({ role: m.role, content: m.content });
    }
  }

  if (messages.length === 0) {
    throw new Error(t('playground.chat.no_messages'));
  }

  const payload: Record<string, unknown> = {
    model,
    messages,
    temperature: st.chatTemperature,
    stream: st.chatStream,
  };
  if (st.chatTopP !== null) {
    payload['top_p'] = st.chatTopP;
  }
  if (st.chatMaxTokens !== null && st.chatMaxTokens > 0) {
    payload['max_tokens'] = st.chatMaxTokens;
  }
  if (st.chatFrequencyPenalty !== 0) {
    payload['frequency_penalty'] = st.chatFrequencyPenalty;
  }
  if (st.chatPresencePenalty !== 0) {
    payload['presence_penalty'] = st.chatPresencePenalty;
  }
  if (st.chatSeed !== null) {
    payload['seed'] = st.chatSeed;
  }
  if (st.chatStop.trim()) {
    const stops = st.chatStop.split(',').map((s) => s.trim()).filter((s) => s.length > 0);
    if (stops.length > 0) payload['stop'] = stops.length === 1 ? stops[0] : stops;
  }
  if (st.chatResponseFormat === 'json_object') {
    payload['response_format'] = { type: 'json_object' };
  }

  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
  };
  if (key) {
    headers['Authorization'] = `Bearer ${key}`;
  }
  if (st.selectedAccountId) {
    headers['x-openproxy-account'] = st.selectedAccountId;
  }

  const reqInit: RequestInit = {
    method: 'POST',
    headers,
    body: JSON.stringify(payload),
  };
  if (st.abortController) {
    reqInit.signal = st.abortController.signal;
  }

  const response = await fetch('/v1/chat/completions', reqInit);

  st.currentMetrics.statusCode = response.status;
  st.currentMetrics.statusText = response.statusText;
  response.headers.forEach((v, k) => {
    st.responseHeaders[k] = v;
  });

  if (!response.ok) {
    const errorText = await response.text();
    st.rawResponseText = errorText;
    try {
      st.parsedResponseJson = JSON.parse(errorText);
    } catch {
      st.parsedResponseJson = null;
    }
    throw new Error(`HTTP ${response.status}: ${errorText}`);
  }

  if (st.chatStream && response.body) {
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
      st.currentMetrics.payloadSizeBytes =
        (st.currentMetrics.payloadSizeBytes || 0) + value.byteLength;

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
                st.currentMetrics.ttftMs = Math.round(firstTokenTime - startTime);
              }
              st.streamedReasoningContent += reasoningDelta;
            }

            if (delta) {
              if (firstTokenTime === null) {
                firstTokenTime = performance.now();
                st.currentMetrics.ttftMs = Math.round(firstTokenTime - startTime);
              }
              st.streamedChatContent += delta;
            }

            if (parsed?.usage) {
              st.currentMetrics.promptTokens = parsed.usage.prompt_tokens ?? st.currentMetrics.promptTokens;
              st.currentMetrics.completionTokens = parsed.usage.completion_tokens ?? st.currentMetrics.completionTokens;
              st.currentMetrics.totalTokens = parsed.usage.total_tokens ?? st.currentMetrics.totalTokens;
            }

            st.streamChunks.push({
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
    st.rawResponseText = st.streamedChatContent;
  } else {
    const text = await response.text();
    st.rawResponseText = text;
    st.currentMetrics.payloadSizeBytes = new Blob([text]).size;

    try {
      const json = JSON.parse(text);
      st.parsedResponseJson = json;
      const content = json?.choices?.[0]?.message?.content || '';
      const reasoning =
        json?.choices?.[0]?.message?.reasoning_content ||
        json?.choices?.[0]?.message?.reasoning ||
        json?.choices?.[0]?.message?.thought ||
        json?.choices?.[0]?.message?.thinking ||
        json?.choices?.[0]?.reasoning_content ||
        '';
      st.streamedChatContent = content;
      st.streamedReasoningContent = reasoning;

      if (json?.usage) {
        st.currentMetrics.promptTokens = json.usage.prompt_tokens ?? null;
        st.currentMetrics.completionTokens = json.usage.completion_tokens ?? null;
        st.currentMetrics.totalTokens = json.usage.total_tokens ?? null;
      }
    } catch {
      st.streamedChatContent = text;
    }
  }
}
