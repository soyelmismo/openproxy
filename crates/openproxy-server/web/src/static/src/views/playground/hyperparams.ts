// views/playground/hyperparams.ts — Hyperparameter UI renderers.
import { html, type TemplateResult } from 'lit-html';
import { requestUpdate } from '../../state/reactive.js';
import type { PlaygroundState } from './shared.js';

function slider(label: string, val: number, min: number, max: number, step: number, onInput: (v: number) => void): TemplateResult {
  return html`
    <div class="field">
      <div class="field-header-row">
        <label class="field-label">${label}</label>
        <input type="number" class="compact-number-input" min="${min}" max="${max}" step="${step}" .value=${String(val)}
          @change=${(e: Event) => { onInput(parseFloat((e.target as HTMLInputElement).value) || 0); requestUpdate(); }} />
      </div>
      <input type="range" min="${min}" max="${max}" step="${step}" .value=${String(val)}
        @input=${(e: Event) => { onInput(parseFloat((e.target as HTMLInputElement).value)); requestUpdate(); }} />
    </div>`;
}

function select<T extends string>(label: string, val: T, opts: Array<[T, string]>, onChange: (v: T) => void): TemplateResult {
  return html`
    <div class="field">
      <label class="field-label">${label}</label>
      <select .value=${val} @change=${(e: Event) => { onChange((e.target as HTMLSelectElement).value as T); requestUpdate(); }}>
        ${opts.map(([v, l]) => html`<option value=${v}>${l}</option>`)}
      </select>
    </div>`;
}

export function renderChatHyperparams(st: PlaygroundState): TemplateResult {
  return html`
    ${slider('Temperature', st.chatTemperature, 0, 2, 0.05, (v) => { st.chatTemperature = Math.max(0, Math.min(2, v)); })}
    <div class="field">
      <div class="field-header-row">
        <label class="field-label">Max Output Tokens</label>
        <input type="number" class="compact-number-input" placeholder="2048" .value=${st.chatMaxTokens !== null ? String(st.chatMaxTokens) : ''}
          @input=${(e: Event) => { const v = (e.target as HTMLInputElement).value; st.chatMaxTokens = v ? parseInt(v, 10) : null; requestUpdate(); }} />
      </div>
      <div class="token-presets-row">
        ${([512, 2048, 4096, 8192] as const).map((t) => html`
          <button class="preset-pill ${st.chatMaxTokens === t ? 'active' : ''}" @click=${() => { st.chatMaxTokens = t; requestUpdate(); }}>${t >= 1024 ? `${t / 1024}k` : t}</button>
        `)}
      </div>
    </div>
    ${slider('Top P', st.chatTopP ?? 1, 0, 1, 0.05, (v) => { st.chatTopP = v >= 1 ? null : Math.max(0, v); })}
    <div class="field">
      <label class="field-label">Streaming Response</label>
      <label class="playground-switch-label">
        <input type="checkbox" ?checked=${st.chatStream} @change=${(e: Event) => { st.chatStream = (e.target as HTMLInputElement).checked; requestUpdate(); }} />
        <span>${st.chatStream ? 'SSE Stream Enabled' : 'Sync Single JSON'}</span>
      </label>
    </div>
    ${slider('Frequency Penalty', st.chatFrequencyPenalty, -2, 2, 0.1, (v) => { st.chatFrequencyPenalty = v; })}
    ${slider('Presence Penalty', st.chatPresencePenalty, -2, 2, 0.1, (v) => { st.chatPresencePenalty = v; })}
    ${select('Response Format', st.chatResponseFormat, [['text', 'Text (Default)'], ['json_object', 'JSON Object']], (v) => { st.chatResponseFormat = v; })}
    <div class="field">
      <label class="field-label">Seed (Optional)</label>
      <input type="number" placeholder="e.g. 42" .value=${st.chatSeed !== null ? String(st.chatSeed) : ''}
        @input=${(e: Event) => { const v = (e.target as HTMLInputElement).value; st.chatSeed = v ? parseInt(v, 10) : null; }} />
    </div>
    <div class="field">
      <label class="field-label">Stop Sequences</label>
      <input type="text" placeholder="e.g. \\n, END, ###" .value=${st.chatStop} @input=${(e: Event) => { st.chatStop = (e.target as HTMLInputElement).value; }} />
    </div>`;
}

export function renderImageHyperparams(st: PlaygroundState): TemplateResult {
  return html`
    ${select('Resolution & Aspect Ratio', st.imageSize, [
      ['1024x1024', '1024x1024 (1:1 Square)'], ['1792x1024', '1792x1024 (16:9 Cinema)'], ['1024x1792', '1024x1792 (9:16 Portrait)'],
      ['1024x680', '1024x680 (3:2 35mm)'], ['680x1024', '680x1024 (2:3 Portrait)'], ['1024x768', '1024x768 (4:3 Standard)'],
      ['768x1024', '768x1024 (3:4 Document)'], ['512x512', '512x512 (Fast)'],
    ], (v) => { st.imageSize = v; })}
    ${select('Aspect Ratio', st.imageAspectRatio, [
      ['1:1', '1:1 (Square)'], ['16:9', '16:9 (Landscape)'], ['9:16', '9:16 (Portrait)'], ['3:2', '3:2 (Photo)'],
      ['2:3', '2:3 (Photo)'], ['4:3', '4:3 (Display)'], ['3:4', '3:4 (Display)'],
    ], (v) => { st.imageAspectRatio = v; })}
    ${select('Quality', st.imageQuality, [['standard', 'Standard'], ['hd', 'HD / High Detail']], (v) => { st.imageQuality = v; })}
    ${select('Image Count (n)', String(st.imageN), [['1', '1 image'], ['2', '2 images'], ['4', '4 images']], (v) => { st.imageN = parseInt(v, 10); })}
    ${st.imageMode === 'edit' || st.imageMode === 'variation' ? html`
      ${slider('Denoising Strength', st.imageDenoisingStrength, 0, 1, 0.05, (v) => { st.imageDenoisingStrength = Math.max(0, Math.min(1, v)); })}
      ${select('Source Processing', st.imageSourceProcessing, [
        ['', '(Auto: Inpaint if mask, img2img otherwise)'], ['img2img', 'img2img (Guided variation)'],
        ['inpainting', 'inpainting (Masked replacement)'], ['outpainting', 'outpainting (Canvas extension)'],
      ], (v) => { st.imageSourceProcessing = v; })}
    ` : html``}
    <div class="field">
      <div style="display: flex; justify-content: space-between; align-items: center; margin-bottom: 4px;">
        <label class="field-label" style="margin-bottom: 0;">Post-Processing & Upscalers ${st.imagePostProcessing.length > 0 ? `(${st.imagePostProcessing.length} active)` : ''}</label>
        ${st.imagePostProcessing.length > 0 ? html`<button type="button" class="btn-text-action" @click=${() => { st.imagePostProcessing = []; requestUpdate(); }}>Clear</button>` : html``}
      </div>
      <div style="display: flex; flex-direction: column; gap: 4px; margin-top: 4px;">
        ${([
          { id: 'RealESRGAN_x4plus', label: 'RealESRGAN 4x', desc: '4x Upscaler' },
          { id: 'GFPGAN', label: 'GFPGAN', desc: 'Face Restoration' },
          { id: 'CodeFormers', label: 'CodeFormers', desc: 'Face Quality Fix' },
          { id: 'NMKD_Siax', label: 'NMKD Siax', desc: 'Detail Enhancement' },
          { id: '4x_AnimeSharp', label: '4x AnimeSharp', desc: '2D / Anime Upscaler' },
        ]).map((pp) => {
          const isSelected = st.imagePostProcessing.includes(pp.id);
          return html`
            <label style="display: flex; align-items: center; justify-content: space-between; gap: 8px; font-size: 0.8rem; padding: 4px 8px; border-radius: var(--radius-sm); background: ${isSelected ? 'var(--color-surface-hover, rgba(56,189,248,0.1))' : 'transparent'}; border: 1px solid ${isSelected ? 'var(--color-primary)' : 'var(--color-border)'}; cursor: pointer; user-select: none;">
              <div style="display: flex; align-items: center; gap: 8px;">
                <input type="checkbox" .checked=${isSelected} @change=${(e: Event) => {
                  const chk = (e.target as HTMLInputElement).checked;
                  st.imagePostProcessing = chk ? (st.imagePostProcessing.includes(pp.id) ? st.imagePostProcessing : [...st.imagePostProcessing, pp.id]) : st.imagePostProcessing.filter((id) => id !== pp.id);
                  requestUpdate();
                }} />
                <span style="font-weight: ${isSelected ? '600' : '400'}; color: ${isSelected ? 'var(--color-text-emphasis)' : 'var(--color-text)'};">${pp.label}</span>
              </div>
              <span style="font-size: 0.72rem; color: var(--color-text-muted);">${pp.desc}</span>
            </label>`;
        })}
      </div>
    </div>
    <div class="field">
      <label class="field-label">Deterministic Seed</label>
      <input type="number" placeholder="Random if empty" .value=${st.imageSeed !== null ? String(st.imageSeed) : ''}
        @input=${(e: Event) => { const v = (e.target as HTMLInputElement).value; st.imageSeed = v ? parseInt(v, 10) : null; }} />
    </div>
    ${select('Response Format', st.imageResponseFormat, [['b64_json', 'Base64 JSON (Embedded)'], ['url', 'URL (External Link)']], (v) => { st.imageResponseFormat = v; })}`;
}

export function renderEmbeddingHyperparams(st: PlaygroundState): TemplateResult {
  return html`
    <div class="field">
      <label class="field-label">Dimensions (Optional)</label>
      <input type="number" placeholder="e.g. 512, 1536" .value=${st.embeddingDimensions !== null ? String(st.embeddingDimensions) : ''}
        @input=${(e: Event) => { const v = (e.target as HTMLInputElement).value; st.embeddingDimensions = v ? parseInt(v, 10) : null; }} />
    </div>
    ${select('Encoding Format', st.embeddingEncodingFormat, [['float', 'Float Array (Default)'], ['base64', 'Base64 Encoded']], (v) => { st.embeddingEncodingFormat = v; })}`;
}

export function renderAudioHyperparams(st: PlaygroundState): TemplateResult {
  return html`
    <div class="field">
      <label class="field-label">Language (ISO-639-1)</label>
      <input type="text" placeholder="e.g. en, es, fr..." .value=${st.audioLanguage} @input=${(e: Event) => { st.audioLanguage = (e.target as HTMLInputElement).value; }} />
    </div>
    ${slider('Temperature', st.audioTemperature, 0, 1, 0.05, (v) => { st.audioTemperature = v; })}
    ${select('Response Format', st.audioResponseFormat, [
      ['json', 'json'], ['text', 'text'], ['verbose_json', 'verbose_json'], ['srt', 'srt'], ['vtt', 'vtt'],
    ], (v) => { st.audioResponseFormat = v; })}`;
}