// views/playground/executors.ts — Image / embedding / audio request executors.
//
// Each executor builds the upstream fetch request for its modality, pipes the
// response back into the shared PlaygroundState (status, headers, raw body,
// metrics), and throws on non-2xx. The chat executor lives in chat.ts.
//
// Shared shape: status → headers → text body → parse JSON → throw on error.

import type { PlaygroundState } from './shared.js';

// ==========
// Shared response ingestors
// ==========

function buildAuthHeaders(
  key: string,
  accountId: string,
  extra?: Record<string, string>,
): Record<string, string> {
  const headers: Record<string, string> = { ...extra };
  if (key) headers['Authorization'] = `Bearer ${key}`;
  if (accountId) headers['x-openproxy-account'] = accountId;
  return headers;
}

async function ingestResponse(response: Response, st: PlaygroundState): Promise<string> {
  st.currentMetrics.statusCode = response.status;
  st.currentMetrics.statusText = response.statusText;
  response.headers.forEach((v, k) => {
    st.responseHeaders[k] = v;
  });
  const text = await response.text();
  st.rawResponseText = text;
  st.currentMetrics.payloadSizeBytes = new Blob([text]).size;
  return text;
}

function safeJsonParse(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

// ==========
// Image executor (generation / edit / variation)
// ==========

export async function executeImageRequest(
  st: PlaygroundState,
  key: string,
  model: string,
): Promise<void> {
  let endpoint = '/v1/images/generations';
  let reqInit: RequestInit;

  const headers: Record<string, string> = {};
  if (key) headers['Authorization'] = `Bearer ${key}`;
  if (st.selectedAccountId) headers['x-openproxy-account'] = st.selectedAccountId;

  if (st.imageMode === 'generation') {
    if (!st.imagePrompt.trim()) {
      throw new Error('Please enter a prompt for image generation.');
    }
    const payload: Record<string, unknown> = {
      model: model || 'dall-e-3',
      prompt: st.imagePrompt.trim(),
      n: st.imageN,
      size: st.imageSize,
      quality: st.imageQuality,
      response_format: st.imageResponseFormat,
    };
    if (st.imageNegativePrompt.trim()) {
      payload['negative_prompt'] = st.imageNegativePrompt.trim();
    }
    if (st.imageSeed !== null && !isNaN(st.imageSeed)) {
      payload['seed'] = st.imageSeed;
    }
    if (st.imageAspectRatio) {
      payload['aspect_ratio'] = st.imageAspectRatio;
    }
    if (st.imagePostProcessing.length > 0) {
      payload['post_processing'] = st.imagePostProcessing;
    }
    headers['Content-Type'] = 'application/json';
    reqInit = {
      method: 'POST',
      headers,
      body: JSON.stringify(payload),
    };
  } else if (st.imageMode === 'edit') {
    if (!st.imageSourceFile) {
      throw new Error('Please select a source image file to edit.');
    }
    if (!st.imagePrompt.trim()) {
      throw new Error('Please enter a prompt describing the edits.');
    }
    endpoint = '/v1/images/edits';
    const formData = new FormData();
    formData.append('image', st.imageSourceFile, st.imageSourceFile.name);
    if (st.imageMaskFile) {
      formData.append('mask', st.imageMaskFile, st.imageMaskFile.name);
    }
    formData.append('prompt', st.imagePrompt.trim());
    formData.append('model', model || 'dall-e-2');
    formData.append('n', String(st.imageN));
    formData.append('size', st.imageSize);
    formData.append('quality', st.imageQuality);
    formData.append('response_format', st.imageResponseFormat);
    formData.append('denoising_strength', String(st.imageDenoisingStrength));
    if (st.imageSourceProcessing) {
      formData.append('source_processing', st.imageSourceProcessing);
    }
    for (const pp of st.imagePostProcessing) {
      formData.append('post_processing', pp);
    }
    if (st.imageNegativePrompt.trim()) {
      formData.append('negative_prompt', st.imageNegativePrompt.trim());
    }
    if (st.imageSeed !== null && !isNaN(st.imageSeed)) {
      formData.append('seed', String(st.imageSeed));
    }
    reqInit = {
      method: 'POST',
      headers,
      body: formData,
    };
  } else {
    if (!st.imageSourceFile) {
      throw new Error('Please select a source image file to create variations.');
    }
    endpoint = '/v1/images/variations';
    const formData = new FormData();
    formData.append('image', st.imageSourceFile, st.imageSourceFile.name);
    if (st.imageMaskFile) {
      formData.append('mask', st.imageMaskFile, st.imageMaskFile.name);
    }
    if (st.imagePrompt.trim()) {
      formData.append('prompt', st.imagePrompt.trim());
    }
    formData.append('model', model || 'dall-e-2');
    formData.append('n', String(st.imageN));
    formData.append('size', st.imageSize);
    formData.append('quality', st.imageQuality);
    formData.append('response_format', st.imageResponseFormat);
    formData.append('denoising_strength', String(st.imageDenoisingStrength));
    if (st.imageSourceProcessing) {
      formData.append('source_processing', st.imageSourceProcessing);
    }
    for (const pp of st.imagePostProcessing) {
      formData.append('post_processing', pp);
    }
    if (st.imageNegativePrompt.trim()) {
      formData.append('negative_prompt', st.imageNegativePrompt.trim());
    }
    if (st.imageSeed !== null && !isNaN(st.imageSeed)) {
      formData.append('seed', String(st.imageSeed));
    }
    reqInit = {
      method: 'POST',
      headers,
      body: formData,
    };
  }

  if (st.abortController) {
    reqInit.signal = st.abortController.signal;
  }

  const response = await fetch(endpoint, reqInit);
  const text = await ingestResponse(response, st);
  st.parsedResponseJson = safeJsonParse(text);

  if (!response.ok) {
    throw new Error(`HTTP ${response.status}: ${text}`);
  }
}

// ==========
// Embedding executor
// ==========

export async function executeEmbeddingRequest(
  st: PlaygroundState,
  key: string,
  model: string,
): Promise<void> {
  if (!st.embeddingInput.trim()) {
    throw new Error('Please enter text to generate embeddings for.');
  }

  let inputData: string | string[] = st.embeddingInput.trim();
  if (st.embeddingIsArray) {
    inputData = st.embeddingInput
      .split('\n')
      .map((s) => s.trim())
      .filter((s) => s.length > 0);
  }

  const payload: Record<string, unknown> = {
    model: model || 'text-embedding-3-small',
    input: inputData,
    encoding_format: st.embeddingEncodingFormat,
  };
  if (st.embeddingDimensions !== null && st.embeddingDimensions > 0) {
    payload['dimensions'] = st.embeddingDimensions;
  }

  const headers = buildAuthHeaders(key, st.selectedAccountId, {
    'Content-Type': 'application/json',
  });

  const reqInit: RequestInit = {
    method: 'POST',
    headers,
    body: JSON.stringify(payload),
  };
  if (st.abortController) {
    reqInit.signal = st.abortController.signal;
  }

  const response = await fetch('/v1/embeddings', reqInit);
  const text = await ingestResponse(response, st);

  const json = safeJsonParse(text);
  st.parsedResponseJson = json;
  if (json && typeof json === 'object' && 'usage' in json) {
    const usage = (json as { usage: { prompt_tokens?: number; total_tokens?: number } }).usage;
    st.currentMetrics.promptTokens = usage.prompt_tokens ?? null;
    st.currentMetrics.totalTokens = usage.total_tokens ?? null;
  }

  if (!response.ok) {
    throw new Error(`HTTP ${response.status}: ${text}`);
  }
}

// ==========
// Audio transcription executor
// ==========

export async function executeAudioRequest(
  st: PlaygroundState,
  key: string,
  model: string,
): Promise<void> {
  if (!st.audioFile) {
    throw new Error('Please select an audio file to transcribe.');
  }

  const formData = new FormData();
  formData.append('file', st.audioFile, st.audioFile.name);
  formData.append('model', model || 'whisper-1');
  if (st.audioPrompt.trim()) formData.append('prompt', st.audioPrompt.trim());
  if (st.audioLanguage.trim()) formData.append('language', st.audioLanguage.trim());
  formData.append('temperature', String(st.audioTemperature));
  formData.append('response_format', st.audioResponseFormat);

  const headers = buildAuthHeaders(key, st.selectedAccountId);

  const reqInit: RequestInit = {
    method: 'POST',
    headers,
    body: formData,
  };
  if (st.abortController) {
    reqInit.signal = st.abortController.signal;
  }

  const response = await fetch('/v1/audio/transcriptions', reqInit);
  const text = await ingestResponse(response, st);

  const json = safeJsonParse(text);
  st.parsedResponseJson = json !== null ? json : text;

  if (!response.ok) {
    throw new Error(`HTTP ${response.status}: ${text}`);
  }
}