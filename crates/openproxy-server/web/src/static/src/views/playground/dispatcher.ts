// views/playground/dispatcher.ts — Request execution dispatcher.
//
// Owns `executeRequest`: validates auth/model, resets execution state,
// dispatches to the per-modality executor (chat / image / embedding / audio),
// and handles abort, error, and latency finalization.

import { requestUpdate } from '../../state/reactive.js';
import { showToast } from '../../components/toast.js';
import { getToken } from '../../state/auth.js';
import type { PlaygroundState } from './shared.js';
import { generateId } from './shared.js';
import { getEffectiveApiKeyFromState } from './curl.js';
import { executeChatRequest } from './chat.js';
import {
  executeImageRequest,
  executeEmbeddingRequest,
  executeAudioRequest,
} from './executors.js';

function beginExecution(st: PlaygroundState): void {
  st.isLoading = true;
  st.responseError = null;
  st.rawResponseText = '';
  st.parsedResponseJson = null;
  st.streamChunks = [];
  st.streamedChatContent = '';
  st.streamedReasoningContent = '';
  st.responseHeaders = {};
  st.currentMetrics = {
    statusCode: null,
    statusText: null,
    ttftMs: null,
    totalLatencyMs: null,
    promptTokens: null,
    completionTokens: null,
    totalTokens: null,
    payloadSizeBytes: null,
  };
  requestUpdate();
}

function commitComposerContent(st: PlaygroundState): void {
  if (st.modality === 'chat' && st.composerContent.trim().length > 0) {
    st.chatMessages.push({
      id: generateId(),
      role: 'user',
      content: st.composerContent.trim(),
    });
    st.composerContent = '';
  }
}

export async function executeRequest(st: PlaygroundState, ensureDefaultModel: (s: PlaygroundState) => void): Promise<void> {
  if (st.isLoading) return;

  const key = getEffectiveApiKeyFromState(st);
  if (!key && !getToken()) {
    showToast('Please provide an API Key or log in to send requests', 'error');
    return;
  }

  ensureDefaultModel(st);
  const effectiveModel = st.selectedModelId || st.customModelInput.trim();
  if (!effectiveModel) {
    showToast('Please select or enter a Model Target', 'error');
    return;
  }

  commitComposerContent(st);
  beginExecution(st);

  st.abortController = new AbortController();
  const startTime = performance.now();

  try {
    if (st.modality === 'chat') {
      await executeChatRequest(st, key, effectiveModel, startTime);
    } else if (st.modality === 'image') {
      await executeImageRequest(st, key, effectiveModel);
    } else if (st.modality === 'embedding') {
      await executeEmbeddingRequest(st, key, effectiveModel);
    } else if (st.modality === 'audio') {
      await executeAudioRequest(st, key, effectiveModel);
    }
  } catch (err: unknown) {
    if (st.abortController?.signal.aborted) {
      st.responseError = 'Request stopped by user.';
    } else {
      const msg = err instanceof Error ? err.message : String(err);
      st.responseError = msg;
      showToast(`Error: ${msg}`, 'error');
    }
  } finally {
    st.currentMetrics.totalLatencyMs = Math.round(performance.now() - startTime);
    st.isLoading = false;
    st.abortController = null;
    requestUpdate();
  }
}
