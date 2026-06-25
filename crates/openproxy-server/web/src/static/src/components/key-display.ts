// components/key-display.ts — shows plaintext key modal.
// Migrated to lit-html.

import { html, render } from 'lit-html';
import { showToast } from "./toast.js";

export interface KeyMetadata {
  label?: string | null;
  key_prefix?: string | null;
}

export function showPlaintextKey(plaintext: string, metadata: KeyMetadata | null): void {
  const wrapper = document.createElement("div");
  document.body.appendChild(wrapper);
  render(html`
    <div class="modal-bg" @click=${(e: Event) => { if (e.target === wrapper?.firstChild) { wrapper.remove(); location.hash = location.hash; } }}>
      <div class="modal">
        <div class="modal-header">
          <h2>Save this key now</h2>
          <button type="button" class="close-btn" @click=${() => { wrapper.remove(); location.hash = location.hash; }} aria-label="Close">&times;</button>
        </div>
        <div class="modal-body">
          <p>This is the <strong>only time</strong> you'll see this key. Copy it now and store it securely.</p>
          <div class="key-display">
            <code id="plaintext-key">${plaintext}</code>
            <button id="copy-key-btn" type="button" @click=${async (e: Event) => {
              const btn = e.target as HTMLButtonElement;
              try {
                await navigator.clipboard.writeText(plaintext);
                btn.textContent = "Copied!";
              } catch (_e: unknown) {
                const ta: HTMLTextAreaElement = document.createElement("textarea");
                ta.value = plaintext;
                document.body.appendChild(ta);
                ta.select();
                try { document.execCommand("copy"); btn.textContent = "Copied!"; }
                catch (__err: unknown) { showToast("Copy failed", "error"); }
                finally { document.body.removeChild(ta); }
              }
            }}>Copy</button>
          </div>
          <p><small>Label: ${metadata?.label ?? "—"} · Prefix: <code>${metadata?.key_prefix ?? "—"}</code></small></p>
        </div>
        <div class="modal-footer">
          <button type="button" class="primary" @click=${() => { wrapper.remove(); location.hash = location.hash; }}>I've saved it</button>
        </div>
      </div>
    </div>
  `, wrapper);
}
