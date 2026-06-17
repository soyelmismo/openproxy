// components/key-display.ts — render the one-shot "save this key"
// modal that shows the plaintext. The user must copy it; the
// "I've saved it" button refetches the key list and re-renders.
//
// Per spec §3 + §13.8 we do not use inline `onclick="window.X()"`
// handlers. The "I've saved it" button uses
// data-action="closeAndNavigate" which closes the closest
// modal-bg and re-navigates.

import { escapeHtml } from "../lib/escape.js";
import { appendModal } from "../lib/dom.js";

export interface KeyMetadata {
  label?: string | null;
  key_prefix?: string | null;
}

export function showPlaintextKey(plaintext: string, metadata: KeyMetadata | null): void {
  const html: string = `
    <div class="modal-bg" data-action="closeModalBg" data-arg1="self">
      <div class="modal">
        <div class="modal-header">
          <h2>Save this key now</h2>
          <button type="button" class="close-btn" data-action="closeAndNavigate" aria-label="Close">&times;</button>
        </div>
        <div class="modal-body">
          <p>This is the <strong>only time</strong> you'll see this key. Copy it now and store it securely.</p>
          <div class="key-display">
            <code id="plaintext-key">${escapeHtml(plaintext)}</code>
            <button id="copy-key-btn" type="button">Copy</button>
          </div>
          <p><small>Label: ${escapeHtml(metadata && metadata.label ? metadata.label : "—")} · Prefix: <code>${escapeHtml(metadata && metadata.key_prefix ? metadata.key_prefix : "—")}</code></small></p>
        </div>
        <div class="modal-footer">
          <button type="button" class="primary" data-action="closeAndNavigate">I've saved it</button>
        </div>
      </div>
    </div>
  `;
  appendModal(html);
  const copyBtn: HTMLElement | null = document.getElementById("copy-key-btn");
  if (copyBtn) {
    copyBtn.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(plaintext);
        copyBtn.textContent = "Copied!";
      } catch (_e: unknown) {
        const ta: HTMLTextAreaElement = document.createElement("textarea");
        ta.value = plaintext;
        document.body.appendChild(ta);
        ta.select();
        try { document.execCommand("copy"); copyBtn.textContent = "Copied!"; }
        catch (__err: unknown) { copyBtn.textContent = "Copy failed"; }
        finally { document.body.removeChild(ta); }
      }
    });
  }
}
