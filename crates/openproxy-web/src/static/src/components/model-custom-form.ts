// components/model-custom-form.ts — the "Add custom model" modal.
// Defaults the format selector to whatever the provider already
// speaks (anthropic for anthropic providers, openai for everything
// else) so the user only has to override it when the model speaks a
// different protocol.
//
// The submit is handled by `createCustomModel` in
// handlers/model-handlers.js via the central data-action shim.

import { state } from "../state/index.js";
import { escapeHtml, escapeAttr } from "../lib/escape.js";
import { appendModal } from "../lib/dom.js";
import type { Provider } from "../lib/types/api.js";

export function showCustomModelForm(providerId: string): void {
  const provider: Provider | undefined = state.providers.find((p) => p.id === providerId);
  const defaultFormat: "openai" | "anthropic" = provider && provider.format === "anthropic" ? "anthropic" : "openai";
  const html: string = `
    <div class="modal-bg" id="custom-model-modal" data-action="closeCustomModelForm" data-arg1="self">
      <div class="modal">
        <div class="modal-header">
          <h2>Custom model for ${escapeHtml(providerId)}</h2>
          <button type="button" class="close-btn" data-action="closeCustomModelForm" aria-label="Close">&times;</button>
        </div>
        <form data-action="createCustomModel" data-arg1="${escapeAttr(providerId)}">
          <div class="modal-body">
            <div class="field">
              <label for="custom-model-id">Model ID</label>
              <input id="custom-model-id" name="model_id" type="text" required placeholder="my-custom-model">
            </div>
            <div class="field">
              <label for="custom-model-display">Display name</label>
              <input id="custom-model-display" name="display_name" type="text" placeholder="My custom model">
            </div>
            <div class="field">
              <label for="custom-model-format">Target format</label>
              <select id="custom-model-format" name="target_format">
                <option value="openai" ${defaultFormat === "openai" ? "selected" : ""}>openai</option>
                <option value="anthropic" ${defaultFormat === "anthropic" ? "selected" : ""}>anthropic</option>
              </select>
            </div>
            <div class="field">
              <label for="custom-model-ttl">TTL (seconds, 0 = never expires)</label>
              <input id="custom-model-ttl" name="ttl_seconds" type="number" value="0">
            </div>
          </div>
          <div class="modal-footer">
            <button type="button" data-action="closeCustomModelForm">Cancel</button>
            <button type="submit" class="primary">Create</button>
          </div>
        </form>
      </div>
    </div>
  `;
  appendModal(html);
}

export function closeCustomModelForm(): void {
  const m: HTMLElement | null = document.getElementById("custom-model-modal");
  if (m) m.remove();
}
