// components/model-custom-form.ts — the "Add custom model" modal.
// Defaults the format selector to whatever the provider already
// speaks (anthropic for anthropic providers, openai for everything
// else) so the user only has to override it when the model speaks a
// different protocol.
//
// The submit handler (`createCustomModel`) lives in
// `handlers/model-handlers.ts` and is imported here directly —
// no more `data-action` registry dispatch. The cycle
// (model-handlers → model-custom-form → model-handlers) is safe
// because the imported binding is only referenced inside an
// `@submit` closure (runtime), not at module top-level.
//
// Migrated to lit-html: the modal is rendered into a fresh wrapper
// `<div>` under `#modal-root` via `render()`. Closing the modal
// removes the wrapper.

import { html, render, type TemplateResult } from "lit-html";
import { state } from "../state/index.js";
import { createCustomModel } from "../handlers/model-handlers.js";
import { ensureModalRoot } from "../lib/ui-utils.js";
import type { Provider } from "../lib/types/api.js";

function customModelFormTemplate(providerId: string): TemplateResult {
  const provider: Provider | undefined = state.providers.find((p) => p.id === providerId);
  const defaultFormat: "openai" | "anthropic" = provider && provider.format === "anthropic" ? "anthropic" : "openai";
  return html`
    <div class="modal-bg" id="custom-model-modal" @click=${(e: Event) => { if (e.target === e.currentTarget) closeCustomModelForm(); }}>
      <div class="modal">
        <div class="modal-header">
          <h2>Custom model for ${providerId}</h2>
          <button type="button" class="close-btn" @click=${closeCustomModelForm} aria-label="Close">&times;</button>
        </div>
        <form @submit=${(e: Event) => { e.preventDefault(); createCustomModel(providerId, e); }}>
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
                <option value="openai" ?selected=${defaultFormat === "openai"}>openai</option>
                <option value="anthropic" ?selected=${defaultFormat === "anthropic"}>anthropic</option>
              </select>
            </div>
            <div class="field">
              <label for="custom-model-ttl">TTL (seconds, 0 = never expires)</label>
              <input id="custom-model-ttl" name="ttl_seconds" type="number" value="0">
            </div>
          </div>
          <div class="modal-footer">
            <button type="button" @click=${closeCustomModelForm}>Cancel</button>
            <button type="submit" class="primary">Create</button>
          </div>
        </form>
      </div>
    </div>
  `;
}

export function showCustomModelForm(providerId: string): void {
  const root = ensureModalRoot();
  // Render into a fresh wrapper div so lit-html can diff efficiently
  // if we ever re-render the same modal. The wrapper is removed by
  // closeCustomModelForm via the closest .modal-bg lookup.
  const wrapper = document.createElement("div");
  root.appendChild(wrapper);
  render(customModelFormTemplate(providerId), wrapper);
}

export function closeCustomModelForm(): void {
  const m: HTMLElement | null = document.getElementById("custom-model-modal");
  if (!m) return;
  // The modal lives inside a wrapper div we created in
  // showCustomModelForm; remove the wrapper too so #modal-root
  // stays clean.
  const wrapper = m.parentElement;
  m.remove();
  if (wrapper && wrapper.children.length === 0 && wrapper.parentElement?.id === "modal-root") {
    wrapper.remove();
  }
}
