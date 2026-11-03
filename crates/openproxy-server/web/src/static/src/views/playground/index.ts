// views/playground/index.ts — orchestrator. Owns the single PlaygroundState
// instance, the cross-cutting lifecycle (`mountPlayground`, imported by
// router.ts, and `renderPlayground`), the Ctrl+Enter shortcut, and the
// per-modality workspace dispatch. Executors live in executors.ts / chat.ts;
// inspector sub-views in formatted-response.ts and metrics-bar.ts.

import { html, type TemplateResult } from 'lit-html';
import { state } from '../../state/index.js';
import { api } from '../../state/api.js';
import { requestUpdate } from '../../state/reactive.js';
import { createView } from '../../lib/view-utils.js';
import { icons } from '../../lib/icons.js';
import { t } from '../../i18n/index.js';
import type { Model, Provider, Account, Combo } from '../../lib/types/api.js';
import type { PlaygroundState } from './shared.js';
import {
  createInitialPlaygroundState,
  bindPlaygroundState,
} from './shared.js';
import { executeRequest } from './dispatcher.js';
import { renderStudioHeader } from './studio-header.js';
import { renderChatWorkspace } from './chat.js';
import { renderImageStudioWorkspace } from './image.js';
import { renderDecisionWorkspace } from './decision.js';
import {
  renderResponseInspector,
  renderInspectorSidebar,
  ensureDefaultModel,
} from './inspector.js';
import {
  consumeHandoffIntoState,
  hasPlaygroundHandoff,
} from '../../lib/playground-loader.js';

export type { ModalityType, ChatMessageRole } from './shared.js';
export type { ResponseTab, ChatMessage, RequestMetrics, StreamChunkItem } from './shared.js';


const stateInternal: PlaygroundState = createInitialPlaygroundState();
bindPlaygroundState(stateInternal);

export { stateInternal as playgroundState };


function renderEmbeddingWorkspace(st: PlaygroundState): TemplateResult {
  return html`
    <div class="playground-workspace-column">
      <div class="playground-card">
        <div class="playground-card-header">
          <h3>${t('playground.embedding.title')}</h3>
          <button
            class="button small"
            @click=${() => {
              st.embeddingInput =
                'Vector databases allow semantic similarity search across embeddings.';
              requestUpdate();
            }}
          >
            ${t('playground.embedding.sample_text')}
          </button>
        </div>

        <div class="field">
          <div style="display: flex; justify-content: space-between; align-items: center;">
            <label class="field-label">${t('playground.embedding.input_text')}</label>
            <label class="playground-switch-label" style="font-size: var(--fs-xs);">
              <input
                type="checkbox"
                ?checked=${st.embeddingIsArray}
                @change=${(e: Event) => {
                  st.embeddingIsArray = (e.target as HTMLInputElement).checked;
                  requestUpdate();
                }}
              />
              <span>${st.embeddingIsArray ? t('playground.embedding.batch_mode') : t('playground.embedding.single_string')}</span>
            </label>
          </div>
          <textarea
            rows="6"
            placeholder=${t('playground.embedding.placeholder')}
            .value=${st.embeddingInput}
            @input=${(e: Event) => {
              st.embeddingInput = (e.target as HTMLTextAreaElement).value;
            }}
          ></textarea>
        </div>
      </div>

      ${renderResponseInspector(st)}
    </div>
  `;
}

function renderAudioWorkspace(st: PlaygroundState): TemplateResult {
  return html`
    <div class="playground-workspace-column">
      <div class="playground-card">
        <div class="playground-card-header">
          <h3>${t('playground.audio.title')}</h3>
        </div>

        <div class="field">
          <label class="field-label">${t('playground.audio.upload_label')}</label>
          <div class="playground-file-dropzone">
            <input
              type="file"
              accept="audio/*"
              @change=${(e: Event) => {
                const input = e.target as HTMLInputElement;
                if (input.files && input.files[0]) {
                  st.audioFile = input.files[0];
                  requestUpdate();
                }
              }}
            />
            ${st.audioFile
              ? html`
                  <div class="playground-file-info">
                    <strong>${t('playground.audio.selected')}</strong> ${st.audioFile.name} (${Math.round(st.audioFile.size / 1024)} KB)
                    <audio controls src=${URL.createObjectURL(st.audioFile)} style="margin-top: var(--space-2); width: 100%;"></audio>
                  </div>
                `
              : html`<p class="text-muted">${icons.audio()} ${t('playground.audio.dropzone')}</p>`}
          </div>
        </div>

        <div class="field" style="margin-top: var(--space-2);">
          <label class="field-label">${t('playground.audio.prompt_label')}</label>
          <input
            type="text"
            placeholder=${t('playground.audio.prompt_placeholder')}
            .value=${st.audioPrompt}
            @input=${(e: Event) => {
              st.audioPrompt = (e.target as HTMLInputElement).value;
            }}
          />
        </div>
      </div>

      ${renderResponseInspector(st)}
    </div>
  `;
}

function renderPlayground(): TemplateResult {
  const st = stateInternal;

  if (st.loadError) {
    return html`
      <div class="page-header"><h2>${t('playground.title')}</h2></div>
      <div class="banner banner-error">${st.loadError}</div>
    `;
  }

  let workspace: TemplateResult;
  const modality = st.modality;
  if (modality === 'chat') {
    workspace = renderChatWorkspace(st);
  } else if (modality === 'image') {
    workspace = renderImageStudioWorkspace(st);
  } else if (modality === 'embedding') {
    workspace = renderEmbeddingWorkspace(st);
  } else if (modality === 'audio') {
    workspace = renderAudioWorkspace(st);
  } else {
    workspace = renderDecisionWorkspace(st);
  }

  return html`
    <div class="playground-studio-wrapper">
      ${renderStudioHeader(st, {
        executeRequest: () => executeRequest(st, ensureDefaultModel),
        ensureDefaultModel,
      })}

      <div class="playground-studio-grid">
        <div class="playground-main-area">
          ${workspace}
        </div>

        ${renderInspectorSidebar(st)}
      </div>
    </div>
  `;
}

// Ctrl+Enter (or Cmd+Enter) runs the request.

function handleGlobalKeydown(e: KeyboardEvent): void {
  if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') {
    const active = document.activeElement;
    if (active && (active.tagName === 'TEXTAREA' || active.tagName === 'INPUT' || active.tagName === 'BODY')) {
      e.preventDefault();
      if (!stateInternal.isLoading) {
        void executeRequest(stateInternal, ensureDefaultModel);
      }
    }
  }
}

function handleRunShortcut(): void {
  if (!stateInternal.isLoading) {
    void executeRequest(stateInternal, ensureDefaultModel);
  }
}


export async function mountPlayground(): Promise<(() => void) | void> {
  stateInternal.loadError = null;
  window.addEventListener('keydown', handleGlobalKeydown);
  window.addEventListener('playground:run', handleRunShortcut);

  // Consume a handoff payload from the log-detail modal ("Send to playground")
  // before the first render, so the restored request paints on the first frame.
  let handoffApplied = false;
  if (hasPlaygroundHandoff()) {
    const providerId = stateInternal.selectedProviderId || undefined;
    handoffApplied = consumeHandoffIntoState(stateInternal, providerId);
  }

  const cleanup = await createView(
    renderPlayground,
    async () => {
      const [models, providers, combos, keys, accounts] = await Promise.all([
        api('/models') as Promise<Model[]>,
        (state.providers.length === 0 ? api('/providers') : Promise.resolve(state.providers)) as Promise<Provider[]>,
        (state.combos.length === 0 ? api('/combos') : Promise.resolve(state.combos)) as Promise<Combo[]>,
        (state.apiKeys.length === 0 ? api('/keys') : Promise.resolve(state.apiKeys)) as Promise<typeof state.apiKeys>,
        (state.accounts.length === 0 ? api('/accounts') : Promise.resolve(state.accounts)) as Promise<Account[]>,
      ]);
      state.models = models;
      state.modelsComplete = true;
      state.providers = providers;
      state.combos = combos;
      state.apiKeys = keys;
      state.accounts = accounts;

      if (handoffApplied) {
        // The payload's model must survive ensureDefaultModel, which would
        // otherwise snap the selection to the first entry of the filtered list.
        const restored = stateInternal.selectedModelId;
        ensureDefaultModel(stateInternal);
        if (restored) stateInternal.selectedModelId = restored;
      } else {
        ensureDefaultModel(stateInternal);
      }
    },
    (msg) => {
      stateInternal.loadError = msg;
    },
  );

  return () => {
    window.removeEventListener('keydown', handleGlobalKeydown);
    window.removeEventListener('playground:run', handleRunShortcut);
    if (cleanup) cleanup();
  };
}