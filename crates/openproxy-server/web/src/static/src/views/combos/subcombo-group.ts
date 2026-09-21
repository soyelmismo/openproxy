// views/combos/subcombo-group.ts — Sub-combo accordion, nested targets, and in-place editor.
// Provides a distinct, collapsible grouped visual abstraction for combos embedded in combos.

import { html, render, type TemplateResult } from 'lit-html';
import { api } from "../../state/api.js";
import { state } from "../../state/index.js";
import { showToast } from "../../components/toast.js";
import { showConfirm } from "../../lib/show-confirm.js";
import { flashButton, ensureModalRoot, showApiError } from "../../lib/ui-utils.js";
import { icons } from "../../lib/icons.js";
import { t } from "../../i18n/index.js";
import {
  statusPillClass,
  PRIORITY_MODE_LABELS,
  PRIORITY_MODE_TOOLTIPS,
  COOLDOWN_MODE_TOOLTIPS,
} from "../../lib/constants.js";
import type { Combo, ComboTargetWithModel, PriorityMode, CooldownMode } from "../../lib/types/api.js";
import { showAddTarget } from "../../handlers/combo-target-handlers/index.js";
import {
  priorityModeOptions,
  cooldownModeOptions,
  PARAM_TOOLTIPS,
} from "../../handlers/combo-handlers.js";

function formatTokens(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1).replace(/\.0$/, "") + "M";
  return n >= 1_000 ? (n / 1_000).toFixed(0) + "k" : String(n);
}

export interface SubComboCacheEntry {
  combo: Combo | null;
  targets: ComboTargetWithModel[];
  loading: boolean;
  error: string | null;
}

const subComboCache = new Map<number, SubComboCacheEntry>();
const expandedSubCombos = new Set<number>();

export function isSubComboExpanded(targetId: number): boolean {
  return expandedSubCombos.has(targetId);
}

export function getSubComboData(subComboId: number): SubComboCacheEntry | undefined {
  return subComboCache.get(subComboId);
}

export async function loadSubComboData(subComboId: number, onUpdate: () => void): Promise<void> {
  const current = subComboCache.get(subComboId);
  if (current && !current.error && current.combo) {
    return;
  }

  subComboCache.set(subComboId, {
    combo: current?.combo ?? null,
    targets: current?.targets ?? [],
    loading: true,
    error: null,
  });
  onUpdate();

  try {
    const [combo, targets] = await Promise.all([
      api(`/combos/${subComboId}`) as Promise<Combo>,
      api(`/combos/${subComboId}/targets`) as Promise<ComboTargetWithModel[]>,
    ]);
    subComboCache.set(subComboId, {
      combo,
      targets: targets || [],
      loading: false,
      error: null,
    });
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    subComboCache.set(subComboId, {
      combo: null,
      targets: [],
      loading: false,
      error: msg,
    });
  }
  onUpdate();
}

export async function toggleSubCombo(
  targetId: number,
  subComboId: number,
  onUpdate: () => void,
): Promise<void> {
  if (expandedSubCombos.has(targetId)) {
    expandedSubCombos.delete(targetId);
    onUpdate();
  } else {
    expandedSubCombos.add(targetId);
    await loadSubComboData(subComboId, onUpdate);
  }
}

export async function refreshSubCombo(subComboId: number, onUpdate: () => void): Promise<void> {
  subComboCache.delete(subComboId);
  await loadSubComboData(subComboId, onUpdate);
}

// ---- Sub-Combo Inline Target Actions ----

async function onToggleNestedTargetActive(
  subComboId: number,
  targetId: number,
  currentActive: boolean,
  onUpdate: () => void,
): Promise<void> {
  try {
    await api(`/combos/${subComboId}/targets/${targetId}`, {
      method: "PATCH",
      body: JSON.stringify({ active: !currentActive }),
    });
    const entry = subComboCache.get(subComboId);
    if (entry) {
      const tgt = entry.targets.find((t) => t.id === targetId);
      if (tgt) tgt.active = !currentActive;
    }
    onUpdate();
  } catch (err: unknown) {
    showApiError(err, "Failed to toggle target");
  }
}

async function onDeleteNestedTarget(
  subComboId: number,
  targetId: number,
  onUpdate: () => void,
): Promise<void> {
  const confirmed = await showConfirm({
    title: t("combos.confirm.remove_title"),
    message: t("combos.confirm.remove_message"),
    danger: true,
    confirmLabel: t("combos.confirm.remove_btn"),
  });
  if (!confirmed) return;

  try {
    await api(`/combos/${subComboId}/targets/${targetId}`, { method: "DELETE" });
    const entry = subComboCache.get(subComboId);
    if (entry) {
      entry.targets = entry.targets.filter((t) => t.id !== targetId);
    }
    onUpdate();
    showToast(t("combos.toast.target_removed"), "success");
  } catch (err: unknown) {
    showApiError(err, "Failed to delete target");
  }
}

async function onTestNestedTarget(
  subComboId: number,
  targetId: number,
  modelRowId: number | null,
  e: Event,
  onUpdate: () => void,
): Promise<void> {
  const btn = (e?.target instanceof HTMLButtonElement ? e.target : null) as HTMLButtonElement | null;
  if (!modelRowId) {
    showToast(t("combos.target.no_model_to_test"), "warning");
    return;
  }
  const oldText = btn?.textContent ?? null;
  if (btn) {
    btn.disabled = true;
    btn.textContent = t("combos.target.testing");
  }

  try {
    const res = await api(`/models/${modelRowId}/test`, { method: "POST" }) as {
      status: number;
      elapsed_ms?: number;
    };
    if (btn) {
      flashButton(
        btn,
        res.status >= 200 && res.status < 300 ? "✓" : `✗ ${res.status || "net"}`,
        res.status >= 200 && res.status < 300 ? "#a6e3a1" : "#f38ba8",
      );
    }
    const prev = (state.comboTestResults[subComboId] || []).filter((r) => r.target_id !== targetId);
    prev.push({
      target_id: targetId,
      provider_id: "",
      model_row_id: modelRowId,
      status: res.status,
      elapsed_ms: res.elapsed_ms ?? null,
      error_msg: null,
      skipped: false,
    });
    state.comboTestResults[subComboId] = prev;
    onUpdate();
  } catch (err: unknown) {
    if (btn) flashButton(btn, "✗", "#f38ba8");
    showToast(
      t("combos.target.test_failed", { message: err instanceof Error ? err.message : String(err) }),
      "error",
    );
  } finally {
    if (btn) {
      setTimeout(() => {
        btn.disabled = false;
        btn.textContent = oldText || t("combos.target.test_btn");
      }, 1500);
    }
  }
}

// ---- Edit Sub-Combo Modal ----

export async function showEditSubComboModal(
  subComboId: number,
  onSaved: () => void,
): Promise<void> {
  const root = ensureModalRoot();
  const wrapper = document.createElement("div");
  root.appendChild(wrapper);

  let combo: Combo;
  try {
    combo = (await api(`/combos/${subComboId}`)) as Combo;
  } catch (err: unknown) {
    wrapper.remove();
    showApiError(err, "Failed to load sub-combo details");
    return;
  }

  function renderModal(): void {
    const pm = (combo.priority_mode ?? "strict") as PriorityMode;
    const cm = (combo.cooldown_mode ?? "flat") as CooldownMode;

    const onSubmit = async (e: Event): Promise<void> => {
      e.preventDefault();
      const form = e.target as HTMLFormElement;
      const data = new FormData(form);

      const name = String(data.get("name") || "").trim();
      const strategy = String(data.get("strategy") || "priority");
      const raceSize = parseInt(String(data.get("race_size") || "1"), 10);
      const prevRl = data.get("preventive_rate_limit") === "on";
      const priorityMode = String(data.get("priority_mode") || "strict");
      const cooldownMode = String(data.get("cooldown_mode") || "flat");

      const rawCw = String(data.get("context_window") || "").trim();
      const contextWindow = rawCw === "" ? null : parseInt(rawCw, 10);

      const decisionModel = String(data.get("decision_model") || "").trim() || null;
      const rawTimeout = String(data.get("decision_timeout_ms") || "").trim();
      const decisionTimeoutMs = rawTimeout === "" ? null : parseInt(rawTimeout, 10);

      const rawLkgp = String(data.get("lkgp_exploration_rate") || "").trim();
      const lkgpRate = rawLkgp === "" ? null : parseFloat(rawLkgp);

      const rawWin = String(data.get("selection_window_secs") || "").trim();
      const winSecs = rawWin === "" ? null : parseInt(rawWin, 10);

      const rawCdBase = String(data.get("cooldown_base_secs") || "").trim();
      const cdBase = rawCdBase === "" ? null : parseInt(rawCdBase, 10);

      const rawCdFactor = String(data.get("cooldown_factor") || "").trim();
      const cdFactor = rawCdFactor === "" ? null : parseInt(rawCdFactor, 10);

      const rawCdMax = String(data.get("cooldown_max_secs") || "").trim();
      const cdMax = rawCdMax === "" ? null : parseInt(rawCdMax, 10);

      const payload: Record<string, unknown> = {
        name: name || combo.name,
        strategy,
        race_size: Number.isFinite(raceSize) && raceSize >= 1 ? raceSize : 1,
        preventive_rate_limit: prevRl,
        priority_mode: priorityMode,
        context_window: contextWindow,
        decision_model: decisionModel,
        decision_timeout_ms: decisionTimeoutMs,
        lkgp_exploration_rate: lkgpRate,
        selection_window_secs: winSecs,
        cooldown_mode: cooldownMode,
        cooldown_base_secs: cdBase,
        cooldown_factor: cdFactor,
        cooldown_max_secs: cdMax,
      };

      try {
        await api(`/combos/${subComboId}`, {
          method: "PATCH",
          body: JSON.stringify(payload),
        });
        wrapper.remove();
        showToast(`Sub-combo "${name || combo.name}" updated`, "success");
        await refreshSubCombo(subComboId, onSaved);
      } catch (err: unknown) {
        showApiError(err, "Failed to update sub-combo");
      }
    };

    render(
      html`
        <div class="modal-bg" id="edit-subcombo-modal" @click=${(e: Event) => {
          if (e.target === e.currentTarget) wrapper.remove();
        }}>
          <div class="modal subcombo-modal">
            <div class="modal-header">
              <h2>${icons.navCombos()} Edit Sub-Combo: ${combo.name} (#${combo.id})</h2>
              <button type="button" class="close-btn" @click=${() => wrapper.remove()} aria-label="Close">&times;</button>
            </div>
            <form @submit=${onSubmit}>
              <div class="modal-body">
                <div class="field">
                  <label for="subcombo-name">Name</label>
                  <input id="subcombo-name" name="name" type="text" .value=${combo.name} required>
                </div>
                <div class="form-row-2">
                  <div class="field">
                    <label for="subcombo-strategy">Strategy</label>
                    <select id="subcombo-strategy" name="strategy">
                      <option value="priority" ?selected=${combo.strategy === "priority"}>priority</option>
                      <option value="round_robin" ?selected=${combo.strategy === "round_robin"}>round_robin</option>
                      <option value="shuffle" ?selected=${combo.strategy === "shuffle"}>shuffle</option>
                    </select>
                  </div>
                  <div class="field">
                    <label for="subcombo-race-size">Race size (1–8)</label>
                    <input id="subcombo-race-size" name="race_size" type="number" min="1" max="8" .value=${String(combo.race_size || 1)}>
                  </div>
                </div>
                <div class="field">
                  <label style="display:inline-flex;align-items:center;gap:6px;cursor:pointer;">
                    <input id="subcombo-preventive-rl" name="preventive_rate_limit" type="checkbox" ?checked=${combo.preventive_rate_limit ?? false}>
                    <span>${icons.lightning()} Preventive Rate Limit (Predict 429s)</span>
                  </label>
                </div>
                <div class="field">
                  <label for="subcombo-context-window">Context Window Override (tokens)</label>
                  <input id="subcombo-context-window" name="context_window" type="number" min="1" placeholder="auto" .value=${combo.context_window != null ? String(combo.context_window) : ""}>
                </div>

                <div class="field">
                  <label for="subcombo-priority-mode">
                    <abbr title=${PRIORITY_MODE_TOOLTIPS[pm]}>Priority mode</abbr>
                  </label>
                  <select
                    id="subcombo-priority-mode"
                    name="priority_mode"
                    @change=${(e: Event) => {
                      combo.priority_mode = (e.target as HTMLSelectElement).value as PriorityMode;
                      renderModal();
                    }}>
                    ${priorityModeOptions(pm)}
                  </select>
                </div>

                ${pm === "decision"
                  ? html`
                      <div class="subcombo-conditional-fields">
                        <div class="field">
                          <label for="subcombo-decision-model">
                            <abbr title="System One model (e.g. jev-1.13, jev-latest, laya)">Decision Model</abbr>
                          </label>
                          <input
                            id="subcombo-decision-model"
                            name="decision_model"
                            type="text"
                            placeholder="jev-1.13"
                            .value=${combo.decision_model || ""}>
                        </div>
                        <div class="field">
                          <label for="subcombo-decision-timeout">
                            <abbr title="Classification timeout in milliseconds">Decision Timeout (ms)</abbr>
                          </label>
                          <input
                            id="subcombo-decision-timeout"
                            name="decision_timeout_ms"
                            type="number"
                            min="10"
                            placeholder="150"
                            .value=${combo.decision_timeout_ms != null ? String(combo.decision_timeout_ms) : ""}>
                        </div>
                      </div>
                    `
                  : html``}

                ${pm === "lkgp"
                  ? html`
                      <div class="subcombo-conditional-fields">
                        <div class="field">
                          <label for="subcombo-lkgp-rate">
                            <abbr title=${PARAM_TOOLTIPS.exploration_rate}>Exploration Rate (0.0–1.0)</abbr>
                          </label>
                          <input
                            id="subcombo-lkgp-rate"
                            name="lkgp_exploration_rate"
                            type="number"
                            min="0"
                            max="1"
                            step="0.05"
                            placeholder="0.1"
                            .value=${combo.lkgp_exploration_rate != null ? String(combo.lkgp_exploration_rate) : ""}>
                        </div>
                      </div>
                    `
                  : html``}

                ${pm === "least_used" || pm === "p2c"
                  ? html`
                      <div class="subcombo-conditional-fields">
                        <div class="field">
                          <label for="subcombo-window">
                            <abbr title=${PARAM_TOOLTIPS.window_secs}>Window (s)</abbr>
                          </label>
                          <input
                            id="subcombo-window"
                            name="selection_window_secs"
                            type="number"
                            min="1"
                            placeholder="3600"
                            .value=${combo.selection_window_secs != null ? String(combo.selection_window_secs) : ""}>
                        </div>
                      </div>
                    `
                  : html``}

                <div class="field">
                  <label for="subcombo-cooldown-mode">
                    <abbr title=${COOLDOWN_MODE_TOOLTIPS[cm]}>Cooldown mode</abbr>
                  </label>
                  <select
                    id="subcombo-cooldown-mode"
                    name="cooldown_mode"
                    @change=${(e: Event) => {
                      combo.cooldown_mode = (e.target as HTMLSelectElement).value as CooldownMode;
                      renderModal();
                    }}>
                    ${cooldownModeOptions(cm)}
                  </select>
                </div>

                ${cm === "exponential"
                  ? html`
                      <div class="subcombo-conditional-fields form-row-3">
                        <div class="field">
                          <label for="subcombo-cd-base"><abbr title=${PARAM_TOOLTIPS.base_secs}>Base (s)</abbr></label>
                          <input id="subcombo-cd-base" name="cooldown_base_secs" type="number" min="1" placeholder="60" .value=${combo.cooldown_base_secs != null ? String(combo.cooldown_base_secs) : ""}>
                        </div>
                        <div class="field">
                          <label for="subcombo-cd-factor"><abbr title=${PARAM_TOOLTIPS.factor}>Factor</abbr></label>
                          <input id="subcombo-cd-factor" name="cooldown_factor" type="number" min="2" placeholder="2" .value=${combo.cooldown_factor != null ? String(combo.cooldown_factor) : ""}>
                        </div>
                        <div class="field">
                          <label for="subcombo-cd-max"><abbr title=${PARAM_TOOLTIPS.max_secs}>Max (s)</abbr></label>
                          <input id="subcombo-cd-max" name="cooldown_max_secs" type="number" min="1" placeholder="3600" .value=${combo.cooldown_max_secs != null ? String(combo.cooldown_max_secs) : ""}>
                        </div>
                      </div>
                    `
                  : html``}
              </div>
              <div class="modal-footer">
                <a href="#/combos/${subComboId}" class="button-link subcombo-open-full" @click=${() => wrapper.remove()}>
                  ${icons.desktop()} Open Full View
                </a>
                <div class="modal-footer-actions">
                  <button type="button" class="secondary" @click=${() => wrapper.remove()}>Cancel</button>
                  <button type="submit" class="primary">Save Changes</button>
                </div>
              </div>
            </form>
          </div>
        </div>
      `,
      wrapper,
    );
  }

  renderModal();
}

// ---- Sub-Combo Accordion Panel Renderer ----

export function renderSubComboAccordion(
  target: ComboTargetWithModel,
  onUpdate: () => void,
): TemplateResult {
  const subComboId = target.sub_combo_id;
  if (!subComboId) return html``;

  const entry = subComboCache.get(subComboId);
  if (!entry || entry.loading) {
    return html`
      <tr class="subcombo-accordion-row">
        <td colspan="11" class="subcombo-accordion-cell">
          <div class="subcombo-nested-container loading">
            <span class="subcombo-loading-spinner">${icons.refresh()}</span>
            <span>Loading sub-combo details & models...</span>
          </div>
        </td>
      </tr>
    `;
  }

  if (entry.error || !entry.combo) {
    return html`
      <tr class="subcombo-accordion-row">
        <td colspan="11" class="subcombo-accordion-cell">
          <div class="subcombo-nested-container error">
            <p class="error-msg">Failed to load sub-combo: ${entry.error || "Unknown error"}</p>
            <button class="small" @click=${() => refreshSubCombo(subComboId, onUpdate)}>Retry</button>
          </div>
        </td>
      </tr>
    `;
  }

  const sub = entry.combo;
  const subTargets = entry.targets;
  const pm = (sub.priority_mode ?? "strict") as PriorityMode;

  return html`
    <tr class="subcombo-accordion-row">
      <td colspan="11" class="subcombo-accordion-cell">
        <div class="subcombo-nested-container">
          <div class="subcombo-nested-header">
            <div class="subcombo-header-info">
              <span class="chip chip-subcombo-accent">${icons.navCombos()} Sub-Combo: ${sub.name}</span>
              <span class="subcombo-meta-pill" title="Strategy: ${sub.strategy}">Strategy: <strong>${sub.strategy}</strong></span>
              <span class="subcombo-meta-pill" title="Priority Mode: ${PRIORITY_MODE_LABELS[pm] || pm}">
                Mode: <strong>${PRIORITY_MODE_LABELS[pm] || pm}</strong>
              </span>
              ${pm === "decision" && sub.decision_model
                ? html`<span class="subcombo-meta-pill decision" title="System One Decision Model">Jev: <strong>${sub.decision_model}</strong></span>`
                : html``}
              <span class="subcombo-meta-pill" title="Race lanes">Race: <strong>${sub.race_size || 1}</strong></span>
              <span class="subcombo-meta-pill" title="Target count">Models: <strong>${subTargets.length}</strong></span>
            </div>
            <div class="subcombo-header-actions">
              <button
                class="small primary"
                title="Add a model target directly to this sub-combo"
                @click=${() => showAddTarget(subComboId)}>
                ${icons.plus()} Add Model
              </button>
              <button
                class="small"
                title="Edit this sub-combo's configuration"
                @click=${() => showEditSubComboModal(subComboId, onUpdate)}>
                ${icons.pencil()} Edit Sub-Combo
              </button>
              <a
                class="small button-link"
                href="#/combos/${subComboId}"
                title="Navigate to dedicated sub-combo dashboard page">
                ${icons.desktop()} Open View
              </a>
            </div>
          </div>

          <div class="subcombo-nested-body">
            ${subTargets.length === 0
              ? html`
                  <div class="subcombo-empty-state">
                    <p>No models configured in this sub-combo yet.</p>
                    <button class="small primary" @click=${() => showAddTarget(subComboId)}>
                      ${icons.plus()} Add Model Target
                    </button>
                  </div>
                `
              : html`
                  <table class="subcombo-nested-table">
                    <thead>
                      <tr>
                        <th>#</th>
                        <th>Provider</th>
                        <th>Account</th>
                        <th>Model</th>
                        <th>Context</th>
                        <th>Thinking</th>
                        <th>Cooldown</th>
                        <th>Test Status</th>
                        <th>Actions</th>
                      </tr>
                    </thead>
                    <tbody>
                      ${subTargets.map((st) => {
                        const inactBadge = st.provider_active === false || st.active === false
                          ? html` <span class="badge badge-inactive">⏸ off</span>`
                          : html``;
                        const cdBadge = st.in_cooldown
                          ? html` <span class="badge badge-cooldown">⏸ CD</span>`
                          : html``;
                        const tr = state.comboTestResults[subComboId]?.find((r) => r.target_id === st.id);
                        const testStatus = !tr
                          ? html`<span class="muted">—</span>`
                          : tr.skipped
                          ? html`<span class="status-pill off">skipped</span>`
                          : html`<span class=${"status-pill " + statusPillClass(tr.status)}>${String(tr.status)}</span>${tr.elapsed_ms != null ? html` <small>${tr.elapsed_ms}ms</small>` : html``}`;

                        return html`
                          <tr class="subcombo-child-row">
                            <td class="sub-col-order">${st.priority_order}</td>
                            <td class="sub-col-provider">
                              <a href="#/providers/${encodeURIComponent(st.provider_id)}">${st.provider_id}</a>
                            </td>
                            <td class="sub-col-account">
                              ${st.account_id ? html`#${st.account_id}` : html`<em>rotate</em>`}
                            </td>
                            <td class="sub-col-model">
                              <span class="sub-model-name">${st.model_display_name || st.model_id || "row #" + st.model_row_id}</span>
                              ${cdBadge}${inactBadge}
                            </td>
                            <td class="sub-col-ctx">
                              ${st.context_length != null ? formatTokens(st.context_length) : "—"}
                            </td>
                            <td class="sub-col-thinking">
                              ${st.thinking_effort ? html`<span class="chip chip-sm">${st.thinking_effort}</span>` : html`—`}
                            </td>
                            <td class="sub-col-cooldown">
                              <span class="sub-cd-text">${st.cooldown_mode ?? "inherit"}</span>
                            </td>
                            <td class="sub-col-test">${testStatus}</td>
                            <td class="sub-col-actions">
                              <div class="sub-actions-wrap">
                                <button
                                  class="small primary"
                                  title="Test model"
                                  @click=${(e: Event) => onTestNestedTarget(subComboId, st.id, st.model_row_id, e, onUpdate)}>
                                  ${icons.flask()}
                                </button>
                                <button
                                  class="small"
                                  title=${st.active !== false ? "Deactivate" : "Activate"}
                                  @click=${() => onToggleNestedTargetActive(subComboId, st.id, st.active !== false, onUpdate)}>
                                  ${st.active !== false ? icons.pause() : icons.play()}
                                </button>
                                <button
                                  class="small danger"
                                  title="Remove model from sub-combo"
                                  @click=${() => onDeleteNestedTarget(subComboId, st.id, onUpdate)}>
                                  ${icons.trash()}
                                </button>
                              </div>
                            </td>
                          </tr>
                        `;
                      })}
                    </tbody>
                  </table>
                `}
          </div>
        </div>
      </td>
    </tr>
  `;
}
