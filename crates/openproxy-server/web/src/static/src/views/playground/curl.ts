// views/playground/curl.ts — cURL command generator + API key resolution.
//
// `generateCurlCommand(st)` materializes a copyable shell snippet for the
// current request modality so users can reproduce the call from their
// terminal. `getEffectiveApiKeyFromState(st)` resolves the active key
// based on `keySource` (session token, saved API key prefix, or custom).

import { state } from '../../state/index.js';
import { getToken } from '../../state/auth.js';
import type { PlaygroundState } from './shared.js';

function getEffectiveChatModel(st: PlaygroundState): string {
  return st.selectedModelId || st.customModelInput.trim() || 'gpt-4o';
}

export function generateCurlCommand(st: PlaygroundState): string {
  const key = getEffectiveApiKeyFromState(st) || '<YOUR_API_KEY>';
  const host = window.location.origin;
  const model = getEffectiveChatModel(st);
  const accountHeader = st.selectedAccountId
    ? ` \\\n  -H "x-openproxy-account: ${st.selectedAccountId}"`
    : '';

  if (st.modality === 'chat') {
    const messages: Array<{ role: string; content: string }> = [];
    if (st.systemInstruction.trim()) {
      messages.push({ role: 'system', content: st.systemInstruction.trim() });
    }
    for (const m of st.chatMessages) {
      if (m.content.trim()) messages.push({ role: m.role, content: m.content });
    }
    const payload: Record<string, unknown> = {
      model,
      messages,
      temperature: st.chatTemperature,
      stream: st.chatStream,
    };
    if (st.chatTopP !== null) payload['top_p'] = st.chatTopP;
    if (st.chatMaxTokens !== null && st.chatMaxTokens > 0) {
      payload['max_tokens'] = st.chatMaxTokens;
    }
    if (st.chatFrequencyPenalty !== 0) payload['frequency_penalty'] = st.chatFrequencyPenalty;
    if (st.chatPresencePenalty !== 0) payload['presence_penalty'] = st.chatPresencePenalty;
    if (st.chatSeed !== null) payload['seed'] = st.chatSeed;
    if (st.chatStop.trim()) {
      const stops = st.chatStop.split(',').map((x) => x.trim()).filter((x) => x.length > 0);
      if (stops.length > 0) payload['stop'] = stops.length === 1 ? stops[0] : stops;
    }
    if (st.chatResponseFormat === 'json_object') {
      payload['response_format'] = { type: 'json_object' };
    }
    const body = JSON.stringify(payload, null, 2);
    return `curl -X POST "${host}/v1/chat/completions" \\\n  -H "Authorization: Bearer ${key}" \\\n  -H "Content-Type: application/json"${accountHeader} \\\n  -d '${body.replace(/'/g, "'\\''")}'`;
  } else if (st.modality === 'image') {
    if (st.imageMode === 'generation') {
      const payload: Record<string, unknown> = {
        model,
        prompt: st.imagePrompt,
        size: st.imageSize,
        quality: st.imageQuality,
        n: st.imageN,
        response_format: st.imageResponseFormat,
      };
      if (st.imageNegativePrompt.trim()) payload['negative_prompt'] = st.imageNegativePrompt.trim();
      if (st.imageSeed !== null && !isNaN(st.imageSeed)) payload['seed'] = st.imageSeed;
      if (st.imageAspectRatio) payload['aspect_ratio'] = st.imageAspectRatio;
      if (st.imagePostProcessing.length > 0) payload['post_processing'] = st.imagePostProcessing;
      const body = JSON.stringify(payload, null, 2);
      return `curl -X POST "${host}/v1/images/generations" \\\n  -H "Authorization: Bearer ${key}" \\\n  -H "Content-Type: application/json"${accountHeader} \\\n  -d '${body.replace(/'/g, "'\\''")}'`;
    } else if (st.imageMode === 'edit') {
      let cmd = `curl -X POST "${host}/v1/images/edits" \\\n  -H "Authorization: Bearer ${key}"${accountHeader} \\\n  -F "image=@${st.imageSourceFile ? st.imageSourceFile.name : 'image.png'}" \\\n  -F "prompt=${st.imagePrompt}" \\\n  -F "model=${model}" \\\n  -F "size=${st.imageSize}" \\\n  -F "quality=${st.imageQuality}" \\\n  -F "n=${st.imageN}" \\\n  -F "denoising_strength=${st.imageDenoisingStrength}"`;
      if (st.imageMaskFile) cmd += ` \\\n  -F "mask=@${st.imageMaskFile.name}"`;
      if (st.imageSourceProcessing) cmd += ` \\\n  -F "source_processing=${st.imageSourceProcessing}"`;
      for (const pp of st.imagePostProcessing) cmd += ` \\\n  -F "post_processing=${pp}"`;
      if (st.imageNegativePrompt.trim()) {
        cmd += ` \\\n  -F "negative_prompt=${st.imageNegativePrompt.trim()}"`;
      }
      if (st.imageSeed !== null && !isNaN(st.imageSeed)) cmd += ` \\\n  -F "seed=${st.imageSeed}"`;
      return cmd;
    } else {
      let cmd = `curl -X POST "${host}/v1/images/variations" \\\n  -H "Authorization: Bearer ${key}"${accountHeader} \\\n  -F "image=@${st.imageSourceFile ? st.imageSourceFile.name : 'image.png'}" \\\n  -F "model=${model}" \\\n  -F "size=${st.imageSize}" \\\n  -F "quality=${st.imageQuality}" \\\n  -F "n=${st.imageN}" \\\n  -F "denoising_strength=${st.imageDenoisingStrength}"`;
      if (st.imageMaskFile) cmd += ` \\\n  -F "mask=@${st.imageMaskFile.name}"`;
      if (st.imagePrompt.trim()) cmd += ` \\\n  -F "prompt=${st.imagePrompt.trim()}"`;
      if (st.imageSourceProcessing) cmd += ` \\\n  -F "source_processing=${st.imageSourceProcessing}"`;
      for (const pp of st.imagePostProcessing) cmd += ` \\\n  -F "post_processing=${pp}"`;
      if (st.imageNegativePrompt.trim()) {
        cmd += ` \\\n  -F "negative_prompt=${st.imageNegativePrompt.trim()}"`;
      }
      if (st.imageSeed !== null && !isNaN(st.imageSeed)) cmd += ` \\\n  -F "seed=${st.imageSeed}"`;
      return cmd;
    }
  } else if (st.modality === 'embedding') {
    const payload: Record<string, unknown> = {
      model,
      input: st.embeddingInput,
      encoding_format: st.embeddingEncodingFormat,
    };
    if (st.embeddingDimensions !== null && st.embeddingDimensions > 0) {
      payload['dimensions'] = st.embeddingDimensions;
    }
    const body = JSON.stringify(payload, null, 2);
    return `curl -X POST "${host}/v1/embeddings" \\\n  -H "Authorization: Bearer ${key}" \\\n  -H "Content-Type: application/json"${accountHeader} \\\n  -d '${body.replace(/'/g, "'\\''")}'`;
  } else {
    return `curl -X POST "${host}/v1/audio/transcriptions" \\\n  -H "Authorization: Bearer ${key}"${accountHeader} \\\n  -F "file=@${st.audioFile ? st.audioFile.name : 'audio.mp3'}" \\\n  -F "model=${model}"`;
  }
}

export function getEffectiveApiKeyFromState(st: PlaygroundState): string {
  if (st.keySource === 'session') {
    return getToken() || '';
  }
  if (st.keySource === 'custom') {
    return st.customApiKey.trim();
  }
  const keys = (state.apiKeys as Array<{ key_prefix?: string; id?: number; label?: string }>) || [];
  const found = keys.find((k) => k.key_prefix === st.selectedApiKeyPrefix);
  if (found && found.key_prefix) {
    return st.customApiKey.trim() || getToken() || '';
  }
  return getToken() || '';
}