// Shared Markdown presentation and code-copy interaction for Playground and Live Logs.
import { html, type TemplateResult } from 'lit-html';
import { unsafeHTML } from 'lit-html/directives/unsafe-html.js';
import { renderMarkdownAndMath } from '../lib/markdown.js';
import { copyToClipboard } from '../lib/clipboard.js';
import { showToast } from './toast.js';

export function copyMarkdownCode(btn: HTMLElement): void {
  let code: string;
  try {
    code = decodeURIComponent(btn.getAttribute('data-code') || '');
  } catch {
    showToast('Copy failed — please copy manually', 'error');
    return;
  }
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
}

function onMarkdownClick(ev: MouseEvent): void {
  if (!(ev.target instanceof Element)) return;
  const btn = ev.target.closest<HTMLElement>('button.md-copy-btn');
  if (!btn) return;
  ev.preventDefault();
  copyMarkdownCode(btn);
}

export function renderMarkdownContent(text: string): TemplateResult {
  return html`<div class="md-formatted-content" @click=${onMarkdownClick}>${unsafeHTML(renderMarkdownAndMath(text))}</div>`;
}
