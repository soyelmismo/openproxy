import { describe, it, expect } from 'vitest';
import type { DecisionQuestion } from './shared.js';
import {
  questionsToWireMap,
  wireMapToQuestions,
  syncDecisionJson,
  applyJsonToList,
} from './decision.js';
import { createInitialPlaygroundState } from './shared.js';

const q = (over: Partial<DecisionQuestion>): DecisionQuestion => ({
  id: 'dq-t',
  name: 'q',
  type: 'choice',
  instructions: '',
  options: [],
  falseLabel: '',
  trueLabel: '',
  ...over,
});

describe('questionsToWireMap', () => {
  it('serializes a choice question to options[]', () => {
    const map = questionsToWireMap([
      q({ name: 'route', instructions: 'pick', options: ['a', 'b'] }),
    ]);
    expect(map['route']!).toEqual({ type: 'choice', instructions: 'pick', options: ['a', 'b'] });
  });

  it('serializes a score question to a criteria[] array', () => {
    const map = questionsToWireMap([
      q({ name: 'complexity', type: 'score', options: ['low', 'high'] }),
    ]);
    expect(map['complexity']!).toEqual({
      type: 'score',
      instructions: '',
      criteria: ['low', 'high'],
    });
  });

  it('serializes noul with optional false/true criteria text', () => {
    const map = questionsToWireMap([
      q({ name: 'is_code', type: 'noul', falseLabel: 'no code', trueLabel: 'has code' }),
    ]);
    expect(map['is_code']!).toEqual({
      type: 'noul',
      instructions: '',
      criteria: { false: 'no code', true: 'has code' },
    });
  });

  it('omits noul criteria when both labels are blank', () => {
    const map = questionsToWireMap([q({ name: 'ok', type: 'noul' })]);
    expect(map['ok']!).toEqual({ type: 'noul', instructions: '' });
  });

  it('skips questions with a blank name', () => {
    const map = questionsToWireMap([q({ name: '  ', options: ['a'] })]);
    expect(Object.keys(map)).toHaveLength(0);
  });
});

describe('wireMapToQuestions round-trip', () => {
  it('parses choice options[]', () => {
    const [c] = wireMapToQuestions({ route: { type: 'choice', instructions: 'x', options: ['a', 'b'] } });
    expect(c!.type).toBe('choice');
    expect(c!.options).toEqual(['a', 'b']);
  });

  it('parses choice criteria{} keys into options', () => {
    const [c] = wireMapToQuestions({ s: { type: 'choice', criteria: { ok: 'fine', err: 'bad' } } });
    expect(c!.options).toEqual(['ok', 'err']);
  });

  it('parses score criteria[] into options', () => {
    const [c] = wireMapToQuestions({ u: { type: 'score', criteria: ['low', 'med', 'high'] } });
    expect(c!.type).toBe('score');
    expect(c!.options).toEqual(['low', 'med', 'high']);
  });

  it('parses noul false/true labels', () => {
    const [c] = wireMapToQuestions({ b: { type: 'noul', criteria: { false: 'no', true: 'yes' } } });
    expect(c!.falseLabel).toBe('no');
    expect(c!.trueLabel).toBe('yes');
  });

  it('round-trips choice/score/noul through both directions', () => {
    const list = [
      q({ id: 'a', name: 'route', options: ['x', 'y'] }),
      q({ id: 'b', name: 'lvl', type: 'score', options: ['l', 'h'] }),
      q({ id: 'c', name: 'bool', type: 'noul', falseLabel: 'F', trueLabel: 'T' }),
    ];
    const back = wireMapToQuestions(questionsToWireMap(list));
    expect(back.map((x) => [x.name, x.type])).toEqual([
      ['route', 'choice'],
      ['lvl', 'score'],
      ['bool', 'noul'],
    ]);
    expect(back[1]!.options).toEqual(['l', 'h']);
  });
});

describe('sync + apply via state', () => {
  it('syncDecisionJson writes a valid object', () => {
    const st = createInitialPlaygroundState();
    st.decisionQuestionsList = [q({ name: 'r', options: ['a'] })];
    syncDecisionJson(st);
    expect(JSON.parse(st.decisionQuestions)).toEqual({ r: { type: 'choice', instructions: '', options: ['a'] } });
  });

  it('applyJsonToList repopulates the builder from JSON', () => {
    const st = createInitialPlaygroundState();
    st.decisionQuestions = '{"u":{"type":"score","criteria":["a","b"]}}';
    expect(applyJsonToList(st)).toBeNull();
    expect(st.decisionQuestionsList[0]!.options).toEqual(['a', 'b']);
  });

  it('applyJsonToList returns an error on invalid JSON', () => {
    const st = createInitialPlaygroundState();
    st.decisionQuestions = '{bad json';
    expect(applyJsonToList(st)).toBeTruthy();
  });

  it('applyJsonToList rejects a non-object root', () => {
    const st = createInitialPlaygroundState();
    st.decisionQuestions = '[1,2,3]';
    expect(applyJsonToList(st)).toBeTruthy();
  });

  it('the shipped default builds a valid wire map', () => {
    const st = createInitialPlaygroundState();
    const map = questionsToWireMap(st.decisionQuestionsList);
    // every question must resolve ≥1 option (engine contract 1..=128)
    for (const [name, raw] of Object.entries(map)) {
      const q = raw as Record<string, unknown>;
      const opts = (q['options'] as string[]) ?? (q['criteria'] as string[]);
      expect(opts && opts.length, `question ${name}`).toBeGreaterThan(0);
    }
    // and the serialized default must parse cleanly
    expect(JSON.parse(st.decisionQuestions)).toBeTruthy();
  });
});
