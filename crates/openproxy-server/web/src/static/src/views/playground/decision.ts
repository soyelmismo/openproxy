// views/playground/decision.ts — Decision (SystemOne / Laya) workspace.
//
// Two editors over the same source of truth, `st.decisionQuestions` (a JSON
// string consumed by executors.ts and curl.ts):
//   · Builder — structured cards: name, type, instructions, options with
//     add/remove controls. Edits re-serialize into the JSON string.
//   · JSON — the raw `questions` map for advanced shapes (e.g. criteria
//     objects for `choice`). Parses back into the builder on valid input.
//
// The wire contract is openproxy's SystemOneQuestion: `type` ∈ {choice, score,
// noul}; `choice` resolves options from `options[]` or `criteria{}` keys;
// `score` resolves levels from a `criteria[]` array; `noul` needs no options
// (its criteria is `{false, true}` text). A choice or score question that
// resolves zero options is rejected by the engine — the builder keeps options
// non-empty to avoid that 400.

import { html, type TemplateResult } from 'lit-html';
import { requestUpdate } from '../../state/reactive.js';
import { t } from '../../i18n/index.js';
import { icons } from '../../lib/icons.js';
import type { PlaygroundState, DecisionQuestion, DecisionQuestionType } from './shared.js';
import { renderResponseInspector } from './inspector.js';

// ── pure model ↔ wire ────────────────────────────────────────────────────────

export function questionsToWireMap(
  list: DecisionQuestion[],
): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const q of list) {
    const name = q.name.trim();
    if (!name) continue;
    if (q.type === 'choice') {
      out[name] = { type: 'choice', instructions: q.instructions, options: q.options };
    } else if (q.type === 'score') {
      out[name] = { type: 'score', instructions: q.instructions, criteria: q.options };
    } else {
      const noul: Record<string, unknown> = { type: 'noul', instructions: q.instructions };
      if (q.falseLabel.trim() || q.trueLabel.trim()) {
        noul['criteria'] = { false: q.falseLabel, true: q.trueLabel };
      }
      out[name] = noul;
    }
  }
  return out;
}

export function wireMapToQuestions(
  map: Record<string, unknown>,
): DecisionQuestion[] {
  return Object.entries(map).map(([name, raw], i) => {
    const q = (raw && typeof raw === 'object' ? raw : {}) as Record<string, unknown>;
    const type: DecisionQuestionType =
      q['type'] === 'score' || q['type'] === 'noul' ? (q['type'] as DecisionQuestionType) : 'choice';
    const instructions = typeof q['instructions'] === 'string' ? q['instructions'] : '';

    let options: string[] = [];
    if (type === 'choice') {
      if (Array.isArray(q['options'])) {
        options = q['options'].filter((o): o is string => typeof o === 'string');
      } else if (q['criteria'] && typeof q['criteria'] === 'object' && !Array.isArray(q['criteria'])) {
        options = Object.keys(q['criteria']);
      }
    } else if (type === 'score') {
      if (Array.isArray(q['criteria'])) {
        options = q['criteria'].map((c) => (typeof c === 'string' ? c : JSON.stringify(c)));
      } else if (Array.isArray(q['options'])) {
        options = q['options'].filter((o): o is string => typeof o === 'string');
      }
    }

    let falseLabel = '';
    let trueLabel = '';
    if (type === 'noul' && q['criteria'] && typeof q['criteria'] === 'object' && !Array.isArray(q['criteria'])) {
      const c = q['criteria'] as Record<string, unknown>;
      const f = c['false'];
      const tr = c['true'];
      falseLabel = typeof f === 'string' ? f : f !== undefined ? JSON.stringify(f) : '';
      trueLabel = typeof tr === 'string' ? tr : tr !== undefined ? JSON.stringify(tr) : '';
    }

    return {
      id: `dq-${i}-${name}`,
      name,
      type,
      instructions,
      options,
      falseLabel,
      trueLabel,
    };
  });
}

// Rebuild the JSON string the executor sends from the builder list.
export function syncDecisionJson(st: PlaygroundState): void {
  st.decisionQuestions = JSON.stringify(questionsToWireMap(st.decisionQuestionsList), null, 2);
}

// Parse the raw JSON back into the builder list; returns an error message or
// null on success.
export function applyJsonToList(st: PlaygroundState): string | null {
  const text = st.decisionQuestions.trim();
  if (!text) {
    st.decisionQuestionsList = [];
    return null;
  }
  try {
    const parsed = JSON.parse(text) as unknown;
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
      return t('playground.decision.json_not_object');
    }
    st.decisionQuestionsList = wireMapToQuestions(parsed as Record<string, unknown>);
    return null;
  } catch (e) {
    return e instanceof Error ? e.message : String(e);
  }
}

// A choice/score question with zero options fails engine validation. Surface it
// before run so the user fixes the card instead of reading a 400.
function questionWarning(q: DecisionQuestion): string | null {
  if (!q.name.trim()) return t('playground.decision.warn_no_name');
  if (q.type !== 'noul' && q.options.filter((o) => o.trim()).length === 0) {
    return t('playground.decision.warn_no_options');
  }
  return null;
}

let nextQid = 100;
function newQuestion(type: DecisionQuestionType): DecisionQuestion {
  return {
    id: `dq-${nextQid++}`,
    name: '',
    type,
    instructions: '',
    options: type === 'choice' ? ['option-a', 'option-b'] : type === 'score' ? ['low', 'medium', 'high'] : [],
    falseLabel: '',
    trueLabel: '',
  };
}

// ── render ───────────────────────────────────────────────────────────────────

function mutateQuestion(st: PlaygroundState, id: string, fn: (q: DecisionQuestion) => void): void {
  const q = st.decisionQuestionsList.find((x) => x.id === id);
  if (!q) return;
  fn(q);
  syncDecisionJson(st);
  requestUpdate();
}

function renderOptionRow(st: PlaygroundState, q: DecisionQuestion, oi: number): TemplateResult {
  const placeholder = q.type === 'score' ? `level ${oi}` : `option ${oi + 1}`;
  return html`
    <div class="decision-option-row">
      ${q.type === 'score' ? html`<span class="decision-option-index">${oi}</span>` : ''}
      <input
        type="text"
        placeholder=${placeholder}
        .value=${q.options[oi] ?? ''}
        @input=${(e: Event) => {
          const v = (e.target as HTMLInputElement).value;
          mutateQuestion(st, q.id, (x) => {
            x.options[oi] = v;
          });
        }}
      />
      <button
        class="icon-btn danger"
        title=${t('playground.decision.remove_option')}
        @click=${() => {
          mutateQuestion(st, q.id, (x) => {
            x.options.splice(oi, 1);
          });
        }}
      >${icons.close()}</button>
    </div>
  `;
}

function renderQuestionCard(st: PlaygroundState, q: DecisionQuestion): TemplateResult {
  const warning = questionWarning(q);
  return html`
    <div class="decision-question-card">
      <div class="decision-question-head">
        <input
          class="decision-question-name"
          type="text"
          placeholder=${t('playground.decision.question_name')}
          .value=${q.name}
          @input=${(e: Event) => {
            const v = (e.target as HTMLInputElement).value;
            mutateQuestion(st, q.id, (x) => {
              x.name = v;
            });
          }}
        />
        <select
          class="decision-question-type"
          .value=${q.type}
          @change=${(e: Event) => {
            const v = (e.target as HTMLSelectElement).value as DecisionQuestionType;
            mutateQuestion(st, q.id, (x) => {
              x.type = v;
              if (v !== 'noul' && x.options.length === 0) {
                x.options = v === 'choice' ? ['option-a', 'option-b'] : ['low', 'medium', 'high'];
              }
            });
          }}
        >
          <option value="choice">choice</option>
          <option value="score">score</option>
          <option value="noul">noul</option>
        </select>
        <button
          class="icon-btn danger"
          title=${t('playground.decision.remove_question')}
          @click=${() => {
            st.decisionQuestionsList = st.decisionQuestionsList.filter((x) => x.id !== q.id);
            syncDecisionJson(st);
            requestUpdate();
          }}
        >${icons.trash()}</button>
      </div>

      <input
        class="decision-question-instructions"
        type="text"
        placeholder=${t('playground.decision.instructions_placeholder')}
        .value=${q.instructions}
        @input=${(e: Event) => {
          const v = (e.target as HTMLInputElement).value;
          mutateQuestion(st, q.id, (x) => {
            x.instructions = v;
          });
        }}
      />

      ${q.type === 'noul'
        ? html`
            <div class="decision-noul-row">
              <div class="decision-noul-arm">
                <span class="decision-arm-badge">false</span>
                <input
                  type="text"
                  placeholder=${t('playground.decision.false_placeholder')}
                  .value=${q.falseLabel}
                  @input=${(e: Event) => {
                    const v = (e.target as HTMLInputElement).value;
                    mutateQuestion(st, q.id, (x) => {
                      x.falseLabel = v;
                    });
                  }}
                />
              </div>
              <div class="decision-noul-arm">
                <span class="decision-arm-badge true">true</span>
                <input
                  type="text"
                  placeholder=${t('playground.decision.true_placeholder')}
                  .value=${q.trueLabel}
                  @input=${(e: Event) => {
                    const v = (e.target as HTMLInputElement).value;
                    mutateQuestion(st, q.id, (x) => {
                      x.trueLabel = v;
                    });
                  }}
                />
              </div>
            </div>
          `
        : html`
            <div class="decision-options-block">
              <div class="decision-options-label">
                ${q.type === 'score'
                  ? t('playground.decision.score_levels')
                  : t('playground.decision.choice_options')}
              </div>
              ${q.options.map((_, oi) => renderOptionRow(st, q, oi))}
              <button
                class="button small decision-add-option"
                @click=${() => {
                  mutateQuestion(st, q.id, (x) => {
                    x.options.push(q.type === 'score' ? `level-${x.options.length}` : `option-${x.options.length + 1}`);
                  });
                }}
              >${icons.plus()} ${t('playground.decision.add_option')}</button>
            </div>
          `}

      ${warning ? html`<div class="decision-question-warning">${warning}</div>` : ''}
    </div>
  `;
}

function renderDecisionWorkspace(st: PlaygroundState): TemplateResult {
  const applySample = (): void => {
    st.decisionState =
      'User request: Can you help me optimize this database query for high throughput?\\nCombo routing targets: [coding-expert, general-chat]';
    st.decisionQuestionsList = [
      {
        id: `dq-${nextQid++}`,
        name: 'route',
        type: 'choice',
        instructions: 'Select the optimal model category for this request',
        options: ['coding-expert', 'general-chat'],
        falseLabel: '',
        trueLabel: '',
      },
      {
        id: `dq-${nextQid++}`,
        name: 'complexity',
        type: 'score',
        instructions: 'Rate the complexity of this request',
        options: ['trivial', 'moderate', 'highly complex'],
        falseLabel: '',
        trueLabel: '',
      },
    ];
    syncDecisionJson(st);
    st.decisionJsonError = null;
    requestUpdate();
  };

  return html`
    <div class="playground-workspace-column">
      <div class="playground-card">
        <div class="playground-card-header">
          <h3>${t('playground.decision.title')}</h3>
          <button class="button small" @click=${applySample}>
            ${t('playground.decision.sample')}
          </button>
        </div>

        <div class="field">
          <label class="field-label">${t('playground.decision.state_label')}</label>
          <textarea
            rows="4"
            placeholder=${t('playground.decision.state_placeholder')}
            .value=${st.decisionState}
            @input=${(e: Event) => {
              st.decisionState = (e.target as HTMLTextAreaElement).value;
            }}
          ></textarea>
        </div>

        <div class="field" style="margin-top: var(--space-2);">
          <div class="decision-questions-toolbar">
            <label class="field-label" style="margin:0;">${t('playground.decision.questions_label')}</label>
            <div class="decision-toolbar-actions">
              <div class="decision-mode-toggle">
                <button
                  class="submode-btn ${!st.decisionJsonMode ? 'active' : ''}"
                  @click=${() => {
                    st.decisionJsonMode = false;
                    requestUpdate();
                  }}
                >${t('playground.decision.mode_builder')}</button>
                <button
                  class="submode-btn ${st.decisionJsonMode ? 'active' : ''}"
                  @click=${() => {
                    st.decisionJsonMode = true;
                    st.decisionJsonError = null;
                    requestUpdate();
                  }}
                >JSON</button>
              </div>
              ${!st.decisionJsonMode
                ? html`
                    <button
                      class="button small"
                      @click=${() => {
                        st.decisionQuestionsList.push(newQuestion('choice'));
                        syncDecisionJson(st);
                        requestUpdate();
                      }}
                    >${icons.plus()} ${t('playground.decision.add_question')}</button>
                  `
                : ''}
            </div>
          </div>

          ${st.decisionJsonMode
            ? html`
                <textarea
                  rows="10"
                  style="font-family: var(--font-mono); font-size: var(--fs-xs);"
                  .value=${st.decisionQuestions}
                  @input=${(e: Event) => {
                    st.decisionQuestions = (e.target as HTMLTextAreaElement).value;
                  }}
                  @change=${() => {
                    st.decisionJsonError = applyJsonToList(st);
                    requestUpdate();
                  }}
                ></textarea>
                ${st.decisionJsonError
                  ? html`<div class="decision-question-warning">${st.decisionJsonError}</div>`
                  : ''}
                <small class="text-muted">${t('playground.decision.json_hint')}</small>
              `
            : html`
                ${st.decisionQuestionsList.length === 0
                  ? html`<div class="decision-empty">${t('playground.decision.empty')}</div>`
                  : st.decisionQuestionsList.map((q) => renderQuestionCard(st, q))}
              `}
        </div>
      </div>

      ${renderResponseInspector(st)}
    </div>
  `;
}

export { renderDecisionWorkspace };
