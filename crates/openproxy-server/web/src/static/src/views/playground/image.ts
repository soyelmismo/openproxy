// views/playground/image.ts — Image Studio modality workspace.
//
// Renders the image-specific main area: sub-mode segmented control (generation,
// edit, variation), file dropzones for source and mask images, prompt editor
// with diffusion directive chips, negative prompt, and the lightbox overlay.
// The response inspector (gallery) is shared via inspector.ts.

import { html, type TemplateResult } from 'lit-html';
import { requestUpdate } from '../../state/reactive.js';
import type { PlaygroundState } from './shared.js';
import { renderResponseInspector } from './inspector.js';

export function renderImageStudioWorkspace(st: PlaygroundState): TemplateResult {
  return html`
    <div class="playground-workspace-column">
      <!-- Mode Segmented Control -->
      <div class="playground-submode-bar">
        <button
          class="submode-btn ${st.imageMode === 'generation' ? 'active' : ''}"
          @click=${() => { st.imageMode = 'generation'; requestUpdate(); }}
        >
          Text-to-Image (/generations)
        </button>
        <button
          class="submode-btn ${st.imageMode === 'edit' ? 'active' : ''}"
          @click=${() => { st.imageMode = 'edit'; requestUpdate(); }}
        >
          Inpainting & Img2Img (/edits)
        </button>
        <button
          class="submode-btn ${st.imageMode === 'variation' ? 'active' : ''}"
          @click=${() => { st.imageMode = 'variation'; requestUpdate(); }}
        >
          Image Variations (/variations)
        </button>
      </div>

      <!-- File Dropzones for Inpainting / Variations -->
      ${st.imageMode === 'edit' || st.imageMode === 'variation'
        ? html`
            <div class="playground-grid-2" style="margin-bottom: var(--space-3);">
              <div class="field">
                <label class="field-label">Source Base Image (Required)</label>
                <div class="playground-file-dropzone">
                  <input
                    type="file"
                    accept="image/png, image/jpeg, image/webp"
                    @change=${(e: Event) => {
                      const input = e.target as HTMLInputElement;
                      if (input.files && input.files[0]) {
                        st.imageSourceFile = input.files[0];
                        requestUpdate();
                      }
                    }}
                  />
                  ${st.imageSourceFile
                    ? html`
                        <div class="file-loaded-info">
                          <small><strong>Loaded:</strong> ${st.imageSourceFile.name} (${Math.round(st.imageSourceFile.size / 1024)} KB)</small>
                          <button
                            class="button small danger"
                            @click=${(e: Event) => {
                              e.stopPropagation();
                              st.imageSourceFile = null;
                              requestUpdate();
                            }}
                          >
                            Remove
                          </button>
                        </div>
                      `
                    : html`<p class="text-muted" style="margin:0;">📁 Click or drop base image (PNG/JPEG)</p>`}
                </div>
              </div>

              <div class="field">
                <label class="field-label">Alpha Mask (Optional for Inpainting)</label>
                <div class="playground-file-dropzone">
                  <input
                    type="file"
                    accept="image/png, image/webp"
                    @change=${(e: Event) => {
                      const input = e.target as HTMLInputElement;
                      if (input.files && input.files[0]) {
                        st.imageMaskFile = input.files[0];
                        requestUpdate();
                      }
                    }}
                  />
                  ${st.imageMaskFile
                    ? html`
                        <div class="file-loaded-info">
                          <small><strong>Mask:</strong> ${st.imageMaskFile.name} (${Math.round(st.imageMaskFile.size / 1024)} KB)</small>
                          <button
                            class="button small danger"
                            @click=${(e: Event) => {
                              e.stopPropagation();
                              st.imageMaskFile = null;
                              requestUpdate();
                            }}
                          >
                            Remove
                          </button>
                        </div>
                      `
                    : html`<p class="text-muted" style="margin:0;">🎭 Click or drop transparency mask (PNG)</p>`}
                </div>
              </div>
            </div>
          `
        : html``}

      <!-- Prompt Editor -->
      <div class="playground-image-prompt-card">
        <label class="field-label">
          Prompt ${st.imageMode === 'variation' ? '(Optional Guidance)' : '(Required)'}
        </label>
        <textarea
          class="playground-image-prompt-textarea"
          rows="3"
          placeholder="A majestic cinematic dragon perched atop a neon-lit cyber tower, hyperdetailed, 8k…"
          .value=${st.imagePrompt}
          @input=${(e: Event) => {
            st.imagePrompt = (e.target as HTMLTextAreaElement).value;
          }}
        ></textarea>

        <!-- Diffusion / Horde Directive Chips -->
        <div class="playground-directives-bar" style="margin-top: var(--space-2); margin-bottom: 0;">
          <span class="directives-label">Directives:</span>
          <button class="directive-chip" @click=${() => insertImageDirective(st, '<lora:name:1.0>')}>
            + &lt;lora:…&gt;
          </button>
          <button class="directive-chip" @click=${() => insertImageDirective(st, '--sampler k_euler_a')}>
            + --sampler
          </button>
          <button class="directive-chip" @click=${() => insertImageDirective(st, '--steps 30')}>
            + --steps
          </button>
          <button class="directive-chip" @click=${() => insertImageDirective(st, '--cfg 7.5')}>
            + --cfg
          </button>
          <button class="directive-chip" @click=${() => insertImageDirective(st, '--hires')}>
            + --hires
          </button>
          <button class="directive-chip" @click=${() => insertImageDirective(st, '--no blurry, distorted, lowres')}>
            + --no
          </button>
          <button class="directive-chip" @click=${() => insertImageDirective(st, '--post RealESRGAN_x4plus')}>
            + --post
          </button>
        </div>

        <div class="field" style="margin-top: var(--space-3);">
          <label class="field-label">Negative Prompt (Elements to avoid)</label>
          <input
            type="text"
            placeholder="blurry, distorted, artifacts, lowres, text, watermark…"
            .value=${st.imageNegativePrompt}
            @input=${(e: Event) => {
              st.imageNegativePrompt = (e.target as HTMLInputElement).value;
            }}
          />
        </div>
      </div>

      <!-- Lightbox Modal Viewer -->
      ${st.lightboxImageUrl
        ? html`
            <div class="playground-lightbox-overlay" @click=${() => { st.lightboxImageUrl = null; requestUpdate(); }}>
              <div class="playground-lightbox-modal" @click=${(e: Event) => e.stopPropagation()}>
                <div class="lightbox-header">
                  <span>High Resolution View</span>
                  <button class="icon-btn" @click=${() => { st.lightboxImageUrl = null; requestUpdate(); }}>✕</button>
                </div>
                <img src=${st.lightboxImageUrl} alt="High resolution preview" class="lightbox-img" />
                <div class="lightbox-footer">
                  <a class="button small primary" href=${st.lightboxImageUrl} download="image.png" target="_blank">Download High-Res</a>
                </div>
              </div>
            </div>
          `
        : html``}

      <!-- Integrated Response / Gallery Inspector -->
      ${renderResponseInspector(st)}
    </div>
  `;
}

function insertImageDirective(st: PlaygroundState, directive: string): void {
  st.imagePrompt = st.imagePrompt.trim() ? `${st.imagePrompt.trim()} ${directive}` : directive;
  requestUpdate();
}
