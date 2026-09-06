// views/playground/hyperparams.ts — Hyperparameter UI renderers.
//
// One lit-html TemplateResult per modality. Each renderer takes the
// PlaygroundState as parameter and emits the parameter card body used by
// the inspector sidebar (`inspector.ts`). State mutation goes through the
// shared mutable reference; `requestUpdate()` schedules the next render.

import { html, type TemplateResult } from 'lit-html';
import { requestUpdate } from '../../state/reactive.js';
import type { PlaygroundState } from './shared.js';

export function renderChatHyperparams(st: PlaygroundState): TemplateResult {
  return html`
    <!-- Temperature Slider + Input -->
    <div class="field">
      <div class="field-header-row">
        <label class="field-label">Temperature</label>
        <input
          type="number"
          class="compact-number-input"
          min="0"
          max="2"
          step="0.05"
          .value=${String(st.chatTemperature)}
          @input=${(e: Event) => {
            st.chatTemperature = Math.max(0, Math.min(2, parseFloat((e.target as HTMLInputElement).value) || 0));
            requestUpdate();
          }}
        />
      </div>
      <input
        type="range"
        min="0"
        max="2"
        step="0.05"
        .value=${String(st.chatTemperature)}
        @input=${(e: Event) => {
          st.chatTemperature = parseFloat((e.target as HTMLInputElement).value);
          requestUpdate();
        }}
      />
    </div>

    <!-- Max Output Tokens + Presets -->
    <div class="field">
      <div class="field-header-row">
        <label class="field-label">Max Output Tokens</label>
        <input
          type="number"
          class="compact-number-input"
          placeholder="2048"
          .value=${st.chatMaxTokens !== null ? String(st.chatMaxTokens) : ''}
          @input=${(e: Event) => {
            const val = (e.target as HTMLInputElement).value;
            st.chatMaxTokens = val ? parseInt(val, 10) : null;
            requestUpdate();
          }}
        />
      </div>
      <div class="token-presets-row">
        <button class="preset-pill ${st.chatMaxTokens === 512 ? 'active' : ''}" @click=${() => { st.chatMaxTokens = 512; requestUpdate(); }}>512</button>
        <button class="preset-pill ${st.chatMaxTokens === 2048 ? 'active' : ''}" @click=${() => { st.chatMaxTokens = 2048; requestUpdate(); }}>2k</button>
        <button class="preset-pill ${st.chatMaxTokens === 4096 ? 'active' : ''}" @click=${() => { st.chatMaxTokens = 4096; requestUpdate(); }}>4k</button>
        <button class="preset-pill ${st.chatMaxTokens === 8192 ? 'active' : ''}" @click=${() => { st.chatMaxTokens = 8192; requestUpdate(); }}>8k</button>
      </div>
    </div>

    <!-- Top P Slider + Input -->
    <div class="field">
      <div class="field-header-row">
        <label class="field-label">Top P</label>
        <input
          type="number"
          class="compact-number-input"
          min="0"
          max="1"
          step="0.05"
          .value=${String(st.chatTopP ?? 1)}
          @input=${(e: Event) => {
            const v = parseFloat((e.target as HTMLInputElement).value);
            st.chatTopP = isNaN(v) || v >= 1 ? null : Math.max(0, v);
            requestUpdate();
          }}
        />
      </div>
      <input
        type="range"
        min="0"
        max="1"
        step="0.05"
        .value=${String(st.chatTopP ?? 1)}
        @input=${(e: Event) => {
          const v = parseFloat((e.target as HTMLInputElement).value);
          st.chatTopP = v === 1 ? null : v;
          requestUpdate();
        }}
      />
    </div>

    <!-- SSE Stream Toggle -->
    <div class="field">
      <label class="field-label">Streaming Response</label>
      <label class="playground-switch-label">
        <input
          type="checkbox"
          ?checked=${st.chatStream}
          @change=${(e: Event) => {
            st.chatStream = (e.target as HTMLInputElement).checked;
            requestUpdate();
          }}
        />
        <span>${st.chatStream ? 'SSE Stream Enabled' : 'Sync Single JSON'}</span>
      </label>
    </div>

    <!-- Frequency Penalty -->
    <div class="field">
      <div class="field-header-row">
        <label class="field-label">Frequency Penalty</label>
        <input
          type="number"
          class="compact-number-input"
          min="-2"
          max="2"
          step="0.1"
          .value=${String(st.chatFrequencyPenalty)}
          @input=${(e: Event) => {
            st.chatFrequencyPenalty = parseFloat((e.target as HTMLInputElement).value) || 0;
            requestUpdate();
          }}
        />
      </div>
      <input
        type="range"
        min="-2"
        max="2"
        step="0.1"
        .value=${String(st.chatFrequencyPenalty)}
        @input=${(e: Event) => {
          st.chatFrequencyPenalty = parseFloat((e.target as HTMLInputElement).value);
          requestUpdate();
        }}
      />
    </div>

    <!-- Presence Penalty -->
    <div class="field">
      <div class="field-header-row">
        <label class="field-label">Presence Penalty</label>
        <input
          type="number"
          class="compact-number-input"
          min="-2"
          max="2"
          step="0.1"
          .value=${String(st.chatPresencePenalty)}
          @input=${(e: Event) => {
            st.chatPresencePenalty = parseFloat((e.target as HTMLInputElement).value) || 0;
            requestUpdate();
          }}
        />
      </div>
      <input
        type="range"
        min="-2"
        max="2"
        step="0.1"
        .value=${String(st.chatPresencePenalty)}
        @input=${(e: Event) => {
          st.chatPresencePenalty = parseFloat((e.target as HTMLInputElement).value);
          requestUpdate();
        }}
      />
    </div>

    <!-- Response Format -->
    <div class="field">
      <label class="field-label">Response Format</label>
      <select
        .value=${st.chatResponseFormat}
        @change=${(e: Event) => {
          st.chatResponseFormat = (e.target as HTMLSelectElement).value as typeof st.chatResponseFormat;
          requestUpdate();
        }}
      >
        <option value="text">Text (Default)</option>
        <option value="json_object">JSON Object</option>
      </select>
    </div>

    <!-- Seed & Stop -->
    <div class="field">
      <label class="field-label">Seed (Optional)</label>
      <input
        type="number"
        placeholder="e.g. 42"
        .value=${st.chatSeed !== null ? String(st.chatSeed) : ''}
        @input=${(e: Event) => {
          const val = (e.target as HTMLInputElement).value;
          st.chatSeed = val ? parseInt(val, 10) : null;
        }}
      />
    </div>

    <div class="field">
      <label class="field-label">Stop Sequences</label>
      <input
        type="text"
        placeholder="e.g. \n, END, ###"
        .value=${st.chatStop}
        @input=${(e: Event) => {
          st.chatStop = (e.target as HTMLInputElement).value;
        }}
      />
    </div>
  `;
}

export function renderImageHyperparams(st: PlaygroundState): TemplateResult {
  return html`
    <!-- Size / Resolution -->
    <div class="field">
      <label class="field-label">Resolution & Aspect Ratio</label>
      <select
        .value=${st.imageSize}
        @change=${(e: Event) => {
          st.imageSize = (e.target as HTMLSelectElement).value;
          requestUpdate();
        }}
      >
        <option value="1024x1024">1024x1024 (1:1 Square)</option>
        <option value="1792x1024">1792x1024 (16:9 Cinema)</option>
        <option value="1024x1792">1024x1792 (9:16 Portrait)</option>
        <option value="1024x680">1024x680 (3:2 35mm)</option>
        <option value="680x1024">680x1024 (2:3 Portrait)</option>
        <option value="1024x768">1024x768 (4:3 Standard)</option>
        <option value="768x1024">768x1024 (3:4 Document)</option>
        <option value="512x512">512x512 (Fast)</option>
      </select>
    </div>

    <!-- Aspect Ratio Parameter -->
    <div class="field">
      <label class="field-label">Aspect Ratio</label>
      <select
        .value=${st.imageAspectRatio}
        @change=${(e: Event) => {
          st.imageAspectRatio = (e.target as HTMLSelectElement).value;
          requestUpdate();
        }}
      >
        <option value="1:1">1:1 (Square)</option>
        <option value="16:9">16:9 (Landscape)</option>
        <option value="9:16">9:16 (Portrait)</option>
        <option value="3:2">3:2 (Photo)</option>
        <option value="2:3">2:3 (Photo)</option>
        <option value="4:3">4:3 (Display)</option>
        <option value="3:4">3:4 (Display)</option>
      </select>
    </div>

    <!-- Quality & Count -->
    <div class="field">
      <label class="field-label">Quality</label>
      <select
        .value=${st.imageQuality}
        @change=${(e: Event) => {
          st.imageQuality = (e.target as HTMLSelectElement).value;
          requestUpdate();
        }}
      >
        <option value="standard">Standard</option>
        <option value="hd">HD / High Detail</option>
      </select>
    </div>

    <div class="field">
      <label class="field-label">Image Count (n)</label>
      <select
        .value=${String(st.imageN)}
        @change=${(e: Event) => {
          st.imageN = parseInt((e.target as HTMLSelectElement).value, 10);
          requestUpdate();
        }}
      >
        <option value="1">1 image</option>
        <option value="2">2 images</option>
        <option value="4">4 images</option>
      </select>
    </div>

    <!-- Denoising Strength for Inpainting / Edit -->
    ${st.imageMode === 'edit' || st.imageMode === 'variation'
      ? html`
          <div class="field">
            <div class="field-header-row">
              <label class="field-label">Denoising Strength</label>
              <input
                type="number"
                class="compact-number-input"
                min="0"
                max="1"
                step="0.05"
                .value=${String(st.imageDenoisingStrength)}
                @input=${(e: Event) => {
                  st.imageDenoisingStrength = Math.max(0, Math.min(1, parseFloat((e.target as HTMLInputElement).value) || 0));
                  requestUpdate();
                }}
              />
            </div>
            <input
              type="range"
              min="0"
              max="1"
              step="0.05"
              .value=${String(st.imageDenoisingStrength)}
              @input=${(e: Event) => {
                st.imageDenoisingStrength = parseFloat((e.target as HTMLInputElement).value);
                requestUpdate();
              }}
            />
          </div>

          <div class="field">
            <label class="field-label">Source Processing</label>
            <select
              .value=${st.imageSourceProcessing}
              @change=${(e: Event) => {
                st.imageSourceProcessing = (e.target as HTMLSelectElement).value;
                requestUpdate();
              }}
            >
              <option value="">(Auto: Inpaint if mask, img2img otherwise)</option>
              <option value="img2img">img2img (Guided variation)</option>
              <option value="inpainting">inpainting (Masked replacement)</option>
              <option value="outpainting">outpainting (Canvas extension)</option>
            </select>
          </div>
        `
      : html``}

    <!-- Post-Processing / Upscalers (Cumulative Array) -->
    <div class="field">
      <div style="display: flex; justify-content: space-between; align-items: center; margin-bottom: 4px;">
        <label class="field-label" style="margin-bottom: 0;">
          Post-Processing & Upscalers ${st.imagePostProcessing.length > 0 ? `(${st.imagePostProcessing.length} active)` : ''}
        </label>
        ${st.imagePostProcessing.length > 0
          ? html`<button
              type="button"
              class="btn-text-action"
              @click=${() => {
                st.imagePostProcessing = [];
                requestUpdate();
              }}
            >
              Clear
            </button>`
          : html``}
      </div>
      <div style="display: flex; flex-direction: column; gap: 4px; margin-top: 4px;">
        ${[
          { id: 'RealESRGAN_x4plus', label: 'RealESRGAN 4x', desc: '4x Upscaler' },
          { id: 'GFPGAN', label: 'GFPGAN', desc: 'Face Restoration' },
          { id: 'CodeFormers', label: 'CodeFormers', desc: 'Face Quality Fix' },
          { id: 'NMKD_Siax', label: 'NMKD Siax', desc: 'Detail Enhancement' },
          { id: '4x_AnimeSharp', label: '4x AnimeSharp', desc: '2D / Anime Upscaler' },
        ].map((pp) => {
          const isSelected = st.imagePostProcessing.includes(pp.id);
          return html`
            <label
              style="display: flex; align-items: center; justify-content: space-between; gap: 8px; font-size: 0.8rem; padding: 4px 8px; border-radius: var(--radius-sm); background: ${isSelected ? 'var(--color-surface-hover, rgba(56,189,248,0.1))' : 'transparent'}; border: 1px solid ${isSelected ? 'var(--color-primary)' : 'var(--color-border)'}; cursor: pointer; user-select: none;"
            >
              <div style="display: flex; align-items: center; gap: 8px;">
                <input
                  type="checkbox"
                  .checked=${isSelected}
                  @change=${(e: Event) => {
                    const checked = (e.target as HTMLInputElement).checked;
                    if (checked) {
                      if (!st.imagePostProcessing.includes(pp.id)) {
                        st.imagePostProcessing = [...st.imagePostProcessing, pp.id];
                      }
                    } else {
                      st.imagePostProcessing = st.imagePostProcessing.filter((id) => id !== pp.id);
                    }
                    requestUpdate();
                  }}
                />
                <span style="font-weight: ${isSelected ? '600' : '400'}; color: ${isSelected ? 'var(--color-text-emphasis)' : 'var(--color-text)'};">
                  ${pp.label}
                </span>
              </div>
              <span style="font-size: 0.72rem; color: var(--color-text-muted);">${pp.desc}</span>
            </label>
          `;
        })}
      </div>
    </div>

    <!-- Seed & Format -->
    <div class="field">
      <label class="field-label">Deterministic Seed</label>
      <input
        type="number"
        placeholder="Random if empty"
        .value=${st.imageSeed !== null ? String(st.imageSeed) : ''}
        @input=${(e: Event) => {
          const val = (e.target as HTMLInputElement).value;
          st.imageSeed = val ? parseInt(val, 10) : null;
        }}
      />
    </div>

    <div class="field">
      <label class="field-label">Response Format</label>
      <select
        .value=${st.imageResponseFormat}
        @change=${(e: Event) => {
          st.imageResponseFormat = (e.target as HTMLSelectElement).value as typeof st.imageResponseFormat;
          requestUpdate();
        }}
      >
        <option value="b64_json">Base64 JSON (Embedded)</option>
        <option value="url">URL (External Link)</option>
      </select>
    </div>
  `;
}

export function renderEmbeddingHyperparams(st: PlaygroundState): TemplateResult {
  return html`
    <div class="field">
      <label class="field-label">Dimensions (Optional)</label>
      <input
        type="number"
        placeholder="e.g. 512, 1536"
        .value=${st.embeddingDimensions !== null ? String(st.embeddingDimensions) : ''}
        @input=${(e: Event) => {
          const val = (e.target as HTMLInputElement).value;
          st.embeddingDimensions = val ? parseInt(val, 10) : null;
        }}
      />
    </div>

    <div class="field">
      <label class="field-label">Encoding Format</label>
      <select
        .value=${st.embeddingEncodingFormat}
        @change=${(e: Event) => {
          st.embeddingEncodingFormat = (e.target as HTMLSelectElement).value as typeof st.embeddingEncodingFormat;
        }}
      >
        <option value="float">Float Array (Default)</option>
        <option value="base64">Base64 Encoded</option>
      </select>
    </div>
  `;
}

export function renderAudioHyperparams(st: PlaygroundState): TemplateResult {
  return html`
    <div class="field">
      <label class="field-label">Language (ISO-639-1)</label>
      <input
        type="text"
        placeholder="e.g. en, es, fr..."
        .value=${st.audioLanguage}
        @input=${(e: Event) => {
          st.audioLanguage = (e.target as HTMLInputElement).value;
        }}
      />
    </div>

    <div class="field">
      <div class="field-header-row">
        <label class="field-label">Temperature</label>
        <input
          type="number"
          class="compact-number-input"
          min="0"
          max="1"
          step="0.05"
          .value=${String(st.audioTemperature)}
          @input=${(e: Event) => {
            st.audioTemperature = parseFloat((e.target as HTMLInputElement).value) || 0;
            requestUpdate();
          }}
        />
      </div>
      <input
        type="range"
        min="0"
        max="1"
        step="0.05"
        .value=${String(st.audioTemperature)}
        @input=${(e: Event) => {
          st.audioTemperature = parseFloat((e.target as HTMLInputElement).value);
          requestUpdate();
        }}
      />
    </div>

    <div class="field">
      <label class="field-label">Response Format</label>
      <select
        .value=${st.audioResponseFormat}
        @change=${(e: Event) => {
          st.audioResponseFormat = (e.target as HTMLSelectElement).value;
        }}
      >
        <option value="json">json</option>
        <option value="text">text</option>
        <option value="verbose_json">verbose_json</option>
        <option value="srt">srt</option>
        <option value="vtt">vtt</option>
      </select>
    </div>
  `;
}