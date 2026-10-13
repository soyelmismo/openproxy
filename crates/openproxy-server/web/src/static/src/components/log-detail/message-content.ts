// Presentation only: never mutate recorded payloads or format JSON/tool schemas as Markdown.
import { html, type TemplateResult } from 'lit-html';
import { renderMarkdownContent } from '../markdown-content.js';
import { formatJson } from './json-prune.js';
import { getLogDetailMarkdown } from './state.js';

function renderRaw(value: unknown): TemplateResult {
  return html`<pre class="json-viewer log-detail-collapsible-body">${formatJson(value)}</pre>`;
}

export function renderLogMessageText(text: string): TemplateResult {
  return getLogDetailMarkdown()
    ? html`<div class="log-detail-markdown">${renderMarkdownContent(text)}</div>`
    : html`<pre class="json-viewer log-detail-collapsible-body">${text}</pre>`;
}

/** Text parts in OpenAI/Anthropic multimodal messages. Non-text blocks stay JSON. */
export function renderLogMessageContent(value: unknown): TemplateResult {
  if (!getLogDetailMarkdown()) return renderRaw(value);
  if (typeof value === 'string') return renderLogMessageText(value);
  if (Array.isArray(value)) {
    return html`${value.map((block: unknown) => {
      if (!block || typeof block !== 'object' || Array.isArray(block)) return renderRaw(block);
      const part = block as Record<string, unknown>;
      const field = part['type'] === 'thinking' ? 'thinking'
        : part['type'] === 'tool_result' ? 'content'
        : ['text', 'input_text', 'output_text'].includes(String(part['type'])) ? 'text' : null;
      if (!field || part[field] == null) return renderRaw(block);
      const metadata = { ...part };
      delete metadata[field];
      return html`${renderRaw(metadata)}${renderLogMessageContent(part[field])}`;
    })}`;
  }
  return renderRaw(value);
}

export function renderLogMessagePayload(msg: Record<string, unknown>): TemplateResult {
  if (!getLogDetailMarkdown()) return renderRaw(msg);
  return html`${Object.entries(msg).map(([key, value]) => html`
    <div class="log-detail-message-field">
      <span class="log-detail-key">${key}</span>
      ${['content', 'reasoning', 'reasoning_content', 'reasoning_text'].includes(key)
        ? renderLogMessageContent(value) : renderRaw(value)}
    </div>`)} `;
}
