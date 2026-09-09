// views/playground/formatted-response.ts — Per-modality formatted response.
//
// Renders the "Formatted" tab of the response inspector: error banner with
// cURL replay, chat reasoning/content bubbles, image gallery, embedding
// vectors, and audio transcription. Dispatched by modality from the inspector.

import { html, type TemplateResult } from 'lit-html';
import { unsafeHTML } from 'lit-html/directives/unsafe-html.js';
import { showToast } from '../../components/toast.js';
import { requestUpdate } from '../../state/reactive.js';
import { icons } from '../../lib/icons.js';
import { t } from '../../i18n/index.js';
import type { PlaygroundState } from './shared.js';
import { extractThinkingProcess, copyText } from './shared.js';
import { renderMarkdownAndMath } from '../../lib/markdown.js';
import { generateCurlCommand } from './curl.js';

// ==========
// Error banner
// ==========

function renderErrorBanner(st: PlaygroundState): TemplateResult {
  const errorDetails = (typeof st.parsedResponseJson === 'object' && st.parsedResponseJson !== null)
    ? st.parsedResponseJson
    : null;
  const curl = generateCurlCommand(st);

  return html`
    <div class="banner banner-error" style="display: flex; flex-direction: column; gap: var(--space-3); padding: var(--space-3); border-radius: var(--radius-md);">
      <div style="display: flex; align-items: center; justify-content: space-between; gap: var(--space-2);">
        <h4 style="margin: 0; color: var(--color-error); display: flex; align-items: center; gap: var(--space-2);">
          <span>⚠️ ${t('playground.inspector.request_failed')}</span>
          ${st.currentMetrics.statusCode
            ? html`<span class="badge" style="background: var(--color-error); color: #fff;">HTTP ${st.currentMetrics.statusCode}</span>`
            : html``}
        </h4>
        <button class="button small" @click=${() => { st.activeResponseTab = 'raw'; requestUpdate(); }}>
          ${t('playground.inspector.view_raw_error')}
        </button>
      </div>

      <p style="margin: 0; font-family: var(--font-mono); font-size: var(--fs-sm); word-break: break-word;">${st.responseError}</p>

      ${errorDetails
        ? html`
            <div style="padding: var(--space-2); background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-sm);">
              <div style="font-weight: 600; font-size: var(--fs-xs); margin-bottom: var(--space-1); color: var(--color-text-muted);">
                ${t('playground.inspector.header_title')}
              </div>
              <pre class="playground-code-view" style="margin: 0; max-height: 180px; font-size: var(--fs-xs);"><code>${JSON.stringify(errorDetails, null, 2)}</code></pre>
            </div>
          `
        : html``}

      <div style="padding: var(--space-2); background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-sm);">
        <div style="display: flex; align-items: center; justify-content: space-between; margin-bottom: var(--space-1);">
          <span style="font-weight: 600; font-size: var(--fs-xs); color: var(--color-text-muted);">${t('playground.inspector.sent_request')}</span>
          <button class="button small" @click=${() => copyText(curl, 'cURL command')}>${t('playground.header.copy_curl')}</button>
        </div>
        <pre class="playground-code-view" style="margin: 0; max-height: 180px; font-size: var(--fs-xs);"><code>${curl}</code></pre>
      </div>
    </div>
  `;
}

// ==========
// Chat response (reasoning + content bubbles)
// ==========

function renderChatResponse(st: PlaygroundState): TemplateResult {
  const { reasoning, content, isThinking } = extractThinkingProcess(
    st.streamedReasoningContent,
    st.streamedChatContent,
    st.isLoading,
  );

  if (!reasoning && !content && !st.isLoading) {
    return html`<div class="playground-empty-response">${t('playground.chat.ready_to_send')}</div>`;
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
                  <span class="reasoning-title">${t('playground.chat.thinking_process')}</span>
                  ${isThinking
                    ? html`
                        <span class="thinking-pulse-dot" title="Thinking in progress…"></span>
                        <span class="thinking-status-text">${t('playground.chat.thinking')}</span>
                      `
                    : html`<span class="thinking-done-badge">${t('playground.chat.completed')}</span>`}
                </div>
                <button class="reasoning-toggle-btn" type="button">
                  ${st.reasoningExpanded ? t('playground.chat.hide') : t('playground.chat.show')}
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
          <span class="role-badge">${t('playground.chat.assistant')}</span>
          ${st.isLoading && !isThinking
            ? html`<span class="badge badge-info"><span class="pulse-dot"></span> ${t('playground.chat.generating')}</span>`
            : (st.isLoading && isThinking
                ? html`<span class="badge badge-info"><span class="pulse-dot"></span> ${t('playground.chat.reasoning')}</span>`
                : html``)}
        </div>
        <div class="bubble-content md-formatted-content">
          ${content
            ? unsafeHTML(renderMarkdownAndMath(content))
            : (st.isLoading
                ? html`<span class="bubble-placeholder-text">${t('playground.chat.drafting')}</span>`
                : html``)}
        </div>
      </div>
    </div>
  `;
}

// ==========
// Image gallery
// ==========

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
    showToast(t('playground.image.inpaint_success'), 'success');
    requestUpdate();
  } catch (err) {
    showToast(t('playground.image.inpaint_fail', { message: String(err) }), 'error');
  }
}

function renderImageResponse(st: PlaygroundState): TemplateResult {
  const data = (st.parsedResponseJson as { data?: Array<{ url?: string; b64_json?: string; revised_prompt?: string }> })?.data;
  if (!data || data.length === 0) {
    return html`<div class="playground-empty-response">${t('playground.image.empty')}</div>`;
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
              <span class="zoom-hint">${icons.search()} ${t('playground.image.zoom_hint')}</span>
            </div>
            ${item.revised_prompt
              ? html`<p class="image-revised-prompt"><small>${item.revised_prompt}</small></p>`
              : html``}
            <div class="image-actions">
              <button
                class="button small"
                @click=${() => sendImageToInpainting(st, item.b64_json || item.url || '', isB64)}
                title=${t('playground.image.inpaint_title')}
              >
                ${t('playground.image.send_to_inpaint')}
              </button>
              <button
                class="button small"
                @click=${() => copyText(item.b64_json ? item.b64_json : (item.url || ''), 'Image data')}
                title=${t('playground.image.copy_data_title')}
              >
                 ${icons.copy()} ${t('playground.image.copy_data')}
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

// ==========
// Embedding vectors
// ==========

function renderEmbeddingResponse(st: PlaygroundState): TemplateResult {
  const data = (st.parsedResponseJson as { data?: Array<{ embedding?: number[]; index?: number }> })?.data;
  if (!data || data.length === 0) {
    return html`<div class="playground-empty-response">${t('playground.embedding.empty')}</div>`;
  }

  return html`
    <div class="playground-embedding-results">
      ${data.map((emb, idx) => {
        const vector = emb.embedding || [];
        const preview = vector.slice(0, 8);
        return html`
          <div class="playground-vector-card">
            <h4>${t('playground.embedding.dimensions', { index: idx + 1, count: vector.length })}</h4>
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

// ==========
// Audio transcription
// ==========

function renderAudioResponse(st: PlaygroundState): TemplateResult {
  if (!st.rawResponseText && !st.parsedResponseJson) {
    return html`<div class="playground-empty-response">${t('playground.audio.empty')}</div>`;
  }

  const transcribedText = typeof st.parsedResponseJson === 'object' && st.parsedResponseJson !== null && 'text' in st.parsedResponseJson
    ? (st.parsedResponseJson as { text: string }).text
    : st.rawResponseText;

  return html`
    <div class="playground-audio-transcription">
      <div class="transcription-box">
        <div style="display: flex; justify-content: space-between; align-items: center; margin-bottom: var(--space-2);">
           <h4 style="margin: 0;">${t('playground.audio.transcribed_text')}</h4>
           <button class="button small" @click=${() => copyText(transcribedText, 'Transcription')}>
             ${t('playground.audio.copy_text')}
          </button>
        </div>
        <p style="white-space: pre-wrap; font-size: var(--fs-md); line-height: 1.6; margin: 0;">${transcribedText}</p>
      </div>
    </div>
  `;
}

// ==========
// Public dispatcher
// ==========

export function renderFormattedResponse(st: PlaygroundState): TemplateResult {
  if (st.responseError) {
    return renderErrorBanner(st);
  }

  if (st.modality === 'chat') {
    return renderChatResponse(st);
  }

  if (st.modality === 'image') {
    return renderImageResponse(st);
  }

  if (st.modality === 'embedding') {
    return renderEmbeddingResponse(st);
  }

  if (st.modality === 'audio') {
    return renderAudioResponse(st);
  }

  return html`<div class="playground-empty-response">${t('playground.inspector.no_display')}</div>`;
}