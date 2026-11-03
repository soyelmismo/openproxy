// lib/playground-loader.test.ts — unit tests for the modal ↔ playground
// handoff bridge. Verifies payload serialization, message flattening, model
// prefixing, hyperparameter preservation, and take-once semantics.

import { describe, it, expect, beforeEach } from 'vitest';
import {
  storePlaygroundHandoff,
  takePlaygroundHandoff,
  hasPlaygroundHandoff,
  applyHandoffToPlaygroundState,
  consumeHandoffIntoState,
  type PlaygroundHandoffPayload,
} from './playground-loader.js';
import { createInitialPlaygroundState, type PlaygroundState } from '../views/playground/shared.js';

describe('playground-loader handoff store', () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it('reports false when no handoff is stored', () => {
    expect(hasPlaygroundHandoff()).toBe(false);
    expect(takePlaygroundHandoff()).toBeNull();
  });

  it('stores and takes a payload once (clears on read)', () => {
    const payload: PlaygroundHandoffPayload = {
      model: 'gpt-4o',
      messages: [{ role: 'user', content: 'hello' }],
      temperature: 0.5,
    };
    storePlaygroundHandoff(payload);
    expect(hasPlaygroundHandoff()).toBe(true);

    const taken = takePlaygroundHandoff();
    expect(taken).toEqual(payload);
    expect(hasPlaygroundHandoff()).toBe(false);
    expect(takePlaygroundHandoff()).toBeNull();
  });

  it('ignores malformed stored JSON', () => {
    localStorage.setItem('openproxy:playground:pendingPayload', 'not-json');
    expect(takePlaygroundHandoff()).toBeNull();
    // Clears the corrupted entry on read
    expect(hasPlaygroundHandoff()).toBe(false);
  });
});

describe('applyHandoffToPlaygroundState', () => {
  let st: PlaygroundState;

  beforeEach(() => {
    localStorage.clear();
    st = createInitialPlaygroundState();
  });

  it('restores chat messages with all roles 1:1 and clears composer', () => {
    st.composerContent = 'stale composer text';
    const payload: PlaygroundHandoffPayload = {
      model: 'claude-3-7-sonnet',
      messages: [
        { role: 'developer', content: 'developer instruction' },
        { role: 'user', content: 'first prompt' },
        { role: 'assistant', content: 'first answer' },
        { role: 'tool', content: '{"result": 42}', tool_call_id: 'call_123' },
        { role: 'user', content: 'follow-up' },
      ],
    };
    applyHandoffToPlaygroundState(st, payload);

    expect(st.chatMessages.length).toBe(5);
    expect(st.chatMessages[0]?.role).toBe('developer');
    expect(st.chatMessages[0]?.content).toBe('developer instruction');
    expect(st.chatMessages[1]?.role).toBe('user');
    expect(st.chatMessages[1]?.content).toBe('first prompt');
    expect(st.chatMessages[2]?.role).toBe('assistant');
    expect(st.chatMessages[2]?.content).toBe('first answer');
    expect(st.chatMessages[3]?.role).toBe('tool');
    expect(st.chatMessages[3]?.content).toBe('{"result": 42}');
    expect(st.chatMessages[3]?.tool_call_id).toBe('call_123');
    expect(st.chatMessages[4]?.role).toBe('user');
    expect(st.chatMessages[4]?.content).toBe('follow-up');
    expect(st.composerContent).toBe('');
  });

  it('hoists the first system message to the systemInstruction field', () => {
    const payload: PlaygroundHandoffPayload = {
      model: 'llama-3.3-70b',
      messages: [
        { role: 'system', content: 'You are an SQL expert.' },
        { role: 'user', content: 'write a select' },
      ],
    };
    applyHandoffToPlaygroundState(st, payload);

    expect(st.systemInstruction).toBe('You are an SQL expert.');
    expect(st.systemInstructionsExpanded).toBe(true);
  });

  it('prefixes bare model with providerId when supplied', () => {
    const payload: PlaygroundHandoffPayload = {
      model: 'glm-4.7-flash',
      messages: [{ role: 'user', content: 'hi' }],
    };
    applyHandoffToPlaygroundState(st, payload, 'scriptsnsenses');

    expect(st.selectedModelId).toBe('scriptsnsenses/glm-4.7-flash');
    expect(st.customModelInput).toBe('glm-4.7-flash');
  });

  it('infers providerId from already-qualified model strings', () => {
    const payload: PlaygroundHandoffPayload = {
      model: 'openrouter/anthropic/claude-3',
      messages: [{ role: 'user', content: 'hi' }],
    };
    applyHandoffToPlaygroundState(st, payload);

    expect(st.selectedModelId).toBe('openrouter/anthropic/claude-3');
    expect(st.selectedProviderId).toBe('openrouter');
  });

  it('restores all chat hyperparameters 1:1', () => {
    const payload: PlaygroundHandoffPayload = {
      model: 'gpt-4o',
      messages: [{ role: 'user', content: 'test' }],
      temperature: 0.25,
      top_p: 0.9,
      max_tokens: 4096,
      frequency_penalty: 0.5,
      presence_penalty: -0.2,
      seed: 42,
      stop: ['END', 'STOP'],
      response_format: { type: 'json_object' },
      stream: false,
    };
    applyHandoffToPlaygroundState(st, payload);

    expect(st.chatTemperature).toBe(0.25);
    expect(st.chatTopP).toBe(0.9);
    expect(st.chatMaxTokens).toBe(4096);
    expect(st.chatFrequencyPenalty).toBe(0.5);
    expect(st.chatPresencePenalty).toBe(-0.2);
    expect(st.chatSeed).toBe(42);
    expect(st.chatStop).toBe('END, STOP');
    expect(st.chatResponseFormat).toBe('json_object');
    expect(st.chatStream).toBe(false);
  });

  it('resets old execution responses to avoid displaying stale outputs', () => {
    st.rawResponseText = 'old output';
    st.parsedResponseJson = { answer: 'old' };
    st.streamChunks = [{ index: 0, delta: 'old', timestampMs: 0, raw: '' }];

    const payload: PlaygroundHandoffPayload = {
      model: 'gpt-4o',
      messages: [{ role: 'user', content: 'new' }],
    };
    applyHandoffToPlaygroundState(st, payload);

    expect(st.rawResponseText).toBe('');
    expect(st.parsedResponseJson).toBeNull();
    expect(st.streamChunks).toEqual([]);
  });

  it('consumeHandoffIntoState runs the full lifecycle', () => {
    storePlaygroundHandoff({
      model: 'mistral-large',
      messages: [{ role: 'user', content: 'hello from log' }],
      temperature: 0.1,
    });

    const applied = consumeHandoffIntoState(st, 'mistral');
    expect(applied).toBe(true);
    expect(st.selectedModelId).toBe('mistral/mistral-large');
    expect(st.chatTemperature).toBe(0.1);
    expect(st.chatMessages[0]?.content).toBe('hello from log');

    // Second call finds nothing
    expect(consumeHandoffIntoState(st)).toBe(false);
  });
});
