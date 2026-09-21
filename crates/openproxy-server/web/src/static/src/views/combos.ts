import { html, type TemplateResult } from 'lit-html';
import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { requestUpdate } from "../state/reactive.js";
import { createView } from "../lib/view-utils.js";
import { showToast } from "../components/toast.js";
import { flashButton } from "../lib/ui-utils.js";
import { showConfirm } from "../lib/show-confirm.js";
import { testAllTargets } from "../handlers/combo-handlers.js";
import { showAddTarget } from "../handlers/combo-target-handlers/index.js";
import { icons } from "../lib/icons.js";
import { t } from "../i18n/index.js";
import { statusPillClass, PRIORITY_MODE_LABELS, PRIORITY_MODE_TOOLTIPS, COOLDOWN_MODE_TOOLTIPS } from "../lib/constants.js";
import type { Combo, ComboTargetWithModel, PriorityMode, CooldownMode } from "../lib/types/api.js";
import {
  isSubComboExpanded,
  toggleSubCombo,
  showEditSubComboModal,
  renderSubComboAccordion,
  getSubComboData,
  loadSubComboData,
} from "./combos/subcombo-group.js";
import { renderCombosList } from "./combos/combos-list.js";

const PARAM_TOOLTIPS = {
  exploration_rate: "Probability (0.0–1.0) of trying a different target instead of the best-known one. 0.1 = 10% exploration. The exploration is priority-weighted: targets positioned first in the combo are more likely to be explored. Higher exploration rates discover alternatives faster but may pick suboptimal targets.",
  base_secs: "Initial cooldown duration in seconds. For exponential mode, this is multiplied by factor^(failures-1).",
  factor: "Multiplier applied to the cooldown after each failure. 2 = doubling.",
  max_secs: "Maximum cooldown duration in seconds. The exponential growth is capped at this value.",
  window_secs: "How far back to look at usage data for the selection algorithm. 3600 = 1 hour.",
  weight: "Relative weight for weighted random selection. Higher = more likely to be selected. Default 1.",
};

const priorityModeOf = (c: Combo): PriorityMode => (c.priority_mode ?? "strict") as PriorityMode;
const cooldownModeOf = (c: Combo): CooldownMode => (c.cooldown_mode ?? "flat") as CooldownMode;

function formatTokens(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1).replace(/\.0$/, "") + "M";
  return n >= 1_000 ? (n / 1_000).toFixed(0) + "k" : String(n);
}

let detailComboId: number | null = null;
let detailCombo: Combo | null = null;
let detailTargets: ComboTargetWithModel[] = [];

async function patchCombo(id: number, body: Record<string, unknown>): Promise<void> {
  try {
    await api("/combos/" + id, { method: "PATCH", body: JSON.stringify(body) });
    if (detailCombo) Object.assign(detailCombo, body);
    const combo = (state.combos || []).find((c) => c.id === id);
    if (combo) Object.assign(combo, body);
    requestUpdate();
  } catch (err: unknown) { showToast(t("combos.toast.error", { message: err instanceof Error ? err.message : String(err) }), "error"); }
}

async function onNumInput(field: string, e: Event): Promise<void> {
  if (e.type === "input") return;
  const raw = (e.target as HTMLInputElement).value.trim();
  const val = raw === "" ? null : parseInt(raw, 10);
  if (raw !== "" && !Number.isFinite(val)) return;
  await patchCombo(detailComboId!, { [field]: val });
}

async function onFloatInput(field: string, e: Event): Promise<void> {
  if (e.type === "input") return;
  const raw = (e.target as HTMLInputElement).value.trim();
  const val = raw === "" ? null : parseFloat(raw);
  if (raw !== "" && !Number.isFinite(val)) return;
  if (field === "lkgp_exploration_rate" && val != null && (val < 0 || val > 1)) return;
  await patchCombo(detailComboId!, { [field]: val });
}

async function onDeleteCombo(): Promise<void> {
  if (!detailComboId || !(await showConfirm({ title: t("combos.confirm.delete_title"), message: t("combos.confirm.delete_message", { name: detailCombo?.name ?? String(detailComboId) }), danger: true, confirmLabel: t("combos.confirm.delete_btn") }))) return;
  try { await api(`/combos/${detailComboId}`, { method: "DELETE" }); location.hash = "#/combos"; }
  catch (err: unknown) { showToast(t("combos.toast.error", { message: err instanceof Error ? err.message : String(err) }), "error"); }
}

async function onToggleTargetActive(targetId: number, currentActive: boolean): Promise<void> {
  try {
    await api(`/combos/${detailComboId}/targets/${targetId}`, { method: "PATCH", body: JSON.stringify({ active: !currentActive }) });
    const tgt = detailTargets.find((t) => t.id === targetId);
    if (tgt) tgt.active = !currentActive;
    requestUpdate();
  } catch (err: unknown) { showToast(t("combos.toast.error", { message: err instanceof Error ? err.message : String(err) }), "error"); }
}

async function onDeleteTarget(targetId: number): Promise<void> {
  if (!(await showConfirm({ title: t("combos.confirm.remove_title"), message: t("combos.confirm.remove_message"), danger: true, confirmLabel: t("combos.confirm.remove_btn") }))) return;
  try {
    await api(`/combos/${detailComboId}/targets/${targetId}`, { method: "DELETE" });
    detailTargets = detailTargets.filter((t) => t.id !== targetId);
    requestUpdate();
  } catch (err: unknown) { showToast(t("combos.toast.error", { message: err instanceof Error ? err.message : String(err) }), "error"); }
}

async function onChangePriority(targetId: number, delta: number): Promise<void> {
  try {
    const currentTargets = await api(`/combos/${detailComboId}/targets`) as ComboTargetWithModel[];
    const ordered = [...currentTargets].sort((a, b) => a.priority_order - b.priority_order);
    const idx = ordered.findIndex((t) => t.id === targetId);
    const newIdx = idx + delta;
    if (idx < 0 || newIdx < 0 || newIdx >= ordered.length) return;
    const tmp = ordered[idx]!; ordered[idx] = ordered[newIdx]!; ordered[newIdx] = tmp;
    await api(`/combos/${detailComboId}/targets/reorder`, { method: "POST", body: JSON.stringify({ target_ids: ordered.map((t) => t.id) }) });
    detailTargets = await api(`/combos/${detailComboId}/targets`) as ComboTargetWithModel[];
    requestUpdate();
  } catch (err: unknown) { showToast(t("combos.toast.error", { message: err instanceof Error ? err.message : String(err) }), "error"); }
}

async function onResetCooldown(targetId: number): Promise<void> {
  try {
    await api(`/combos/${detailComboId}/targets/${targetId}/clear-cooldown`, { method: "POST" });
    const t = detailTargets.find((x) => x.id === targetId);
    if (t) { t.in_cooldown = false; t.cooldown_until = null; t.cooldown_reason = null; }
    requestUpdate();
  } catch (err: unknown) { showToast(t("combos.toast.error", { message: err instanceof Error ? err.message : String(err) }), "error"); }
}

async function onTestTarget(targetId: number, modelRowId: number | null, e: Event): Promise<void> {
  const btn = (e && e.target instanceof HTMLButtonElement ? e.target : null) as HTMLButtonElement | null;
  if (!modelRowId) { showToast(t("combos.target.no_model_to_test"), "warning"); return; }
  const oldText = btn?.textContent ?? null;
  if (btn) { btn.disabled = true; btn.textContent = t("combos.target.testing"); }
  try {
    const res = await api(`/models/${modelRowId}/test`, { method: "POST" }) as { status: number; elapsed_ms?: number };
    if (btn) flashButton(btn, res.status >= 200 && res.status < 300 ? "✓" : `✗ ${res.status || "net"}`, res.status >= 200 && res.status < 300 ? "#a6e3a1" : "#f38ba8");
    if (detailComboId) {
      const prev = (state.comboTestResults[detailComboId] || []).filter((r) => r.target_id !== targetId);
      prev.push({ target_id: targetId, provider_id: "", model_row_id: modelRowId, status: res.status, elapsed_ms: res.elapsed_ms ?? null, error_msg: null, skipped: false });
      state.comboTestResults[detailComboId] = prev;
    }
    requestUpdate();
  } catch (err: unknown) {
    if (btn) flashButton(btn, "✗", "#f38ba8");
    showToast(t("combos.target.test_failed", { message: err instanceof Error ? err.message : String(err) }), "error");
  } finally {
    if (btn) setTimeout(() => { btn.disabled = false; btn.textContent = oldText || t("combos.target.test_btn"); }, 1500);
  }
}

function priorityModeOptions(selected: PriorityMode): TemplateResult {
  const modes: PriorityMode[] = ["strict", "lkgp", "weighted", "least_used", "p2c", "decision"];
  return html`${modes.map((m) => html`<option value=${m} ?selected=${m === selected}>${PRIORITY_MODE_LABELS[m]}</option>`)}`;
}

function cooldownModeOptions(selected: CooldownMode): TemplateResult {
  const modes: CooldownMode[] = ["flat", "exponential", "none"];
  return html`${modes.map((m) => html`<option value=${m} ?selected=${m === selected}>${m === "flat" ? t("combos.detail.mode.flat") : m === "exponential" ? t("combos.detail.mode.exponential") : t("combos.detail.mode.disabled")}</option>`)}`;
}

function renderPriorityModeBar(combo: Combo): TemplateResult {
  const pm = priorityModeOf(combo);
  let params = html``;
  if (pm === "lkgp") {
    params = html`<details class="combo-mode-params" open><summary>${t("combos.detail.parameters")}</summary><div class="combo-mode-params-body"><label><abbr title=${PARAM_TOOLTIPS.exploration_rate}>Exploration Rate</abbr><input type="number" min="0" max="1" step="0.05" .value=${combo.lkgp_exploration_rate != null ? String(combo.lkgp_exploration_rate) : ""} placeholder="0.1" @change=${(e: Event) => onFloatInput("lkgp_exploration_rate", e)} class="cw-input"></label></div></details>`;
  } else if (pm === "least_used" || pm === "p2c") {
    params = html`<details class="combo-mode-params" open><summary>${t("combos.detail.parameters")}</summary><div class="combo-mode-params-body"><label><abbr title=${PARAM_TOOLTIPS.window_secs}>Window (s)</abbr><input type="number" min="1" .value=${combo.selection_window_secs != null ? String(combo.selection_window_secs) : ""} placeholder="3600" @change=${(e: Event) => onNumInput("selection_window_secs", e)} class="cw-input"></label></div></details>`;
  } else if (pm === "weighted") {
    params = html`<details class="combo-mode-params" open><summary>${t("combos.detail.parameters")}</summary><div class="combo-mode-params-body"><span class="muted">${t("combos.detail.weighted_hint")}</span></div></details>`;
  } else if (pm === "decision") {
    params = html`<details class="combo-mode-params" open><summary>${t("combos.detail.parameters")}</summary><div class="combo-mode-params-body">
      <label><abbr title="System One model used for real-time prompt classification and routing (e.g., jev-latest, laya)">Decision Model</abbr><input type="text" .value=${combo.decision_model || ""} placeholder="jev-latest" @change=${(e: Event) => patchCombo(detailComboId!, { decision_model: (e.target as HTMLInputElement).value.trim() || null })} class="cw-input"></label>
      <label><abbr title="Timeout in milliseconds for the System One routing call before falling back to default priority">Timeout (ms)</abbr><input type="number" min="10" .value=${combo.decision_timeout_ms != null ? String(combo.decision_timeout_ms) : ""} placeholder="150" @change=${(e: Event) => onNumInput("decision_timeout_ms", e)} class="cw-input"></label>
    </div></details>`;
  }
  return html`<div class="combo-settings-bar"><label><abbr title=${PRIORITY_MODE_TOOLTIPS[pm]}>${t("combos.detail.priority_mode")}</abbr><select @change=${(e: Event) => patchCombo(detailComboId!, { priority_mode: (e.target as HTMLSelectElement).value })}>${priorityModeOptions(pm)}</select></label>${params}</div>`;
}

function renderCooldownBar(combo: Combo): TemplateResult {
  const cm = cooldownModeOf(combo);
  const params = cm === "exponential" ? html`<details class="combo-mode-params" open><summary>${t("combos.detail.parameters")}</summary><div class="combo-mode-params-body">
    <label><abbr title=${PARAM_TOOLTIPS.base_secs}>Base (s)</abbr><input type="number" min="1" .value=${combo.cooldown_base_secs != null ? String(combo.cooldown_base_secs) : ""} placeholder="60" @change=${(e: Event) => onNumInput("cooldown_base_secs", e)} class="cw-input"></label>
    <label><abbr title=${PARAM_TOOLTIPS.factor}>Factor</abbr><input type="number" min="2" .value=${combo.cooldown_factor != null ? String(combo.cooldown_factor) : ""} placeholder="2" @change=${(e: Event) => onNumInput("cooldown_factor", e)} class="cw-input"></label>
    <label><abbr title=${PARAM_TOOLTIPS.max_secs}>Max (s)</abbr><input type="number" min="1" .value=${combo.cooldown_max_secs != null ? String(combo.cooldown_max_secs) : ""} placeholder="3600" @change=${(e: Event) => onNumInput("cooldown_max_secs", e)} class="cw-input"></label>
  </div></details>` : html``;
  return html`<div class="combo-settings-bar"><label><abbr title=${COOLDOWN_MODE_TOOLTIPS[cm]}>${t("combos.detail.cooldown_mode")}</abbr><select @change=${(e: Event) => patchCombo(detailComboId!, { cooldown_mode: (e.target as HTMLSelectElement).value })}>${cooldownModeOptions(cm)}</select></label>${params}</div>`;
}

let touchDragState: { draggedId: number; rowEl: HTMLElement; currentOverEl: HTMLElement | null } | null = null;

function onTouchStartHandle(targetId: number, e: TouchEvent): void {
  const row = (e.currentTarget as HTMLElement).closest(".combo-target-card-row") as HTMLElement | null;
  if (!row) return;
  touchDragState = { draggedId: targetId, rowEl: row, currentOverEl: null };
  row.classList.add("touch-dragging");
}

function onTouchMoveHandle(e: TouchEvent): void {
  if (!touchDragState) return;
  const touch = e.touches[0];
  if (!touch) return;
  if (e.cancelable) e.preventDefault();
  const el = document.elementFromPoint(touch.clientX, touch.clientY);
  const overRow = el?.closest(".combo-target-card-row") as HTMLElement | null;
  if (touchDragState.currentOverEl && touchDragState.currentOverEl !== overRow) {
    touchDragState.currentOverEl.classList.remove("drag-over");
    touchDragState.currentOverEl = null;
  }
  if (overRow && overRow !== touchDragState.rowEl) {
    overRow.classList.add("drag-over");
    touchDragState.currentOverEl = overRow;
  }
}

async function onTouchEndHandle(): Promise<void> {
  if (!touchDragState) return;
  const { draggedId, rowEl, currentOverEl } = touchDragState;
  rowEl.classList.remove("touch-dragging");
  if (currentOverEl) {
    currentOverEl.classList.remove("drag-over");
    const dropTargetId = parseInt(currentOverEl.getAttribute("data-drag-id") || "0", 10);
    if (dropTargetId && dropTargetId !== draggedId) await executeTargetReorder(draggedId, dropTargetId);
  }
  touchDragState = null;
}

async function executeTargetReorder(draggedId: number, dropTargetId: number): Promise<void> {
  if (!detailComboId || draggedId === dropTargetId) return;
  try {
    const currentTargets = await api(`/combos/${detailComboId}/targets`) as ComboTargetWithModel[];
    const ordered = [...currentTargets].sort((a, b) => a.priority_order - b.priority_order);
    const fromIdx = ordered.findIndex((x) => x.id === draggedId);
    const toIdx = ordered.findIndex((x) => x.id === dropTargetId);
    if (fromIdx < 0 || toIdx < 0) return;
    const [moved] = ordered.splice(fromIdx, 1);
    if (!moved) return;
    ordered.splice(toIdx, 0, moved);
    await api(`/combos/${detailComboId}/targets/reorder`, { method: "POST", body: JSON.stringify({ target_ids: ordered.map((x) => x.id) }) });
    detailTargets = await api(`/combos/${detailComboId}/targets`) as ComboTargetWithModel[];
    requestUpdate();
    showToast(t("combos.toast.target_priority_updated"), "success");
  } catch (err: unknown) { showToast(t("combos.toast.reorder_failed", { message: err instanceof Error ? err.message : String(err) }), "error"); }
}

function onTargetDragStart(targetId: number, e: DragEvent): void {
  const targetEl = e.target as HTMLElement | null;
  if (
    targetEl &&
    (targetEl.closest("input, select, textarea, button, a, .target-desc-row, .cw-input, .weight-input") ||
     targetEl.tagName === "INPUT" ||
     targetEl.tagName === "SELECT" ||
     targetEl.tagName === "BUTTON")
  ) {
    e.preventDefault();
    return;
  }
  e.dataTransfer?.setData("text/plain", String(targetId));
  (e.currentTarget as HTMLElement).classList.add("dragging");
}

function onTargetDragEnd(e: DragEvent): void {
  (e.currentTarget as HTMLElement).classList.remove("dragging");
}

function renderTargetRow(target: ComboTargetWithModel, showWeight: boolean): TemplateResult {
  const isSub = target.sub_combo_id != null;
  const cdBadge = target.in_cooldown ? html` <span class="badge badge-cooldown">⏸</span>` : html``;
  const inactBadge = (target.provider_active === false || target.active === false) ? html` <span class="badge badge-inactive">⏸ inactive</span>` : html``;

  if (isSub) {
    const isExpanded = isSubComboExpanded(target.id);
    const subData = getSubComboData(target.sub_combo_id!);
    const subCombo = subData?.combo;
    const subTargetsCount = subData?.targets?.length;
    const pm = subCombo?.priority_mode ? String(subCombo.priority_mode) : null;

    const descTag = html`
      <div class="target-desc-row" style="margin-top:4px;">
        <input type="text"
               draggable="false"
               class="cw-input"
               style="font-size:0.75rem;padding:2px 6px;width:100%;max-width:280px;"
               placeholder="Routing description for Decision router..."
               title="Semantic description / routing criteria for System One decision routing"
               .value=${target.description ?? ""}
               @dragstart=${(e: DragEvent) => e.stopPropagation()}
               @change=${async (e: Event) => {
                 const val = (e.target as HTMLInputElement).value.trim() || null;
                 await api(`/combos/${detailComboId}/targets/${target.id}`, { method: "PATCH", body: JSON.stringify({ description: val }) });
                 target.description = val;
                 requestUpdate();
               }}>
      </div>`;

    const modelCell = html`
      <div class="subcombo-title-wrap">
        <button
          type="button"
          class="subcombo-toggle-btn"
          title=${isExpanded ? "Collapse sub-combo" : "Expand sub-combo models"}
          @click=${() => void toggleSubCombo(target.id, target.sub_combo_id!, () => requestUpdate())}>
          ${isExpanded ? icons.caretDown() : icons.chevronRight()}
        </button>
        <span class="chip chip-subcombo">${icons.navCombos()} Sub-Combo</span>
        <strong class="subcombo-name">${target.sub_combo_name ?? "#" + target.sub_combo_id}</strong>
        ${subCombo ? html`<span class="chip chip-strategy" title="Strategy: ${subCombo.strategy}">${subCombo.strategy}</span>` : html``}
        ${pm ? html`<span class="chip chip-pm" title="Priority mode: ${pm}">${pm}</span>` : html``}
        ${subTargetsCount != null ? html`<span class="chip chip-targets-count" title="Configured models in sub-combo">${subTargetsCount} models</span>` : html``}
      </div>
      ${descTag}
    `;

    const actionsCell = html`
      <div class="target-actions-wrap">
        <button
          class="small primary"
          title="Configure sub-combo settings"
          @click=${() => void showEditSubComboModal(target.sub_combo_id!, () => requestUpdate())}>
          ${icons.pencil()} Edit
        </button>
        <button
          class="small"
          title="Add a model target into this sub-combo"
          @click=${() => showAddTarget(target.sub_combo_id!)}>
          ${icons.plus()}
        </button>
        <button
          class="small"
          title=${target.active !== false ? t("combos.target.deactivate_title") : t("combos.target.activate_title")}
          @click=${() => onToggleTargetActive(target.id, target.active !== false)}>
          ${target.active !== false ? html`${icons.pause()}` : html`${icons.play()}`}
        </button>
        <button
          class="small reorder-btn"
          title=${t("combos.target.move_up_title")}
          @click=${() => onChangePriority(target.id, -1)}>
          ${icons.caretUp()}
        </button>
        <button
          class="small reorder-btn"
          title=${t("combos.target.move_down_title")}
          @click=${() => onChangePriority(target.id, 1)}>
          ${icons.caretDown()}
        </button>
        <button
          class="small danger"
          title=${t("combos.target.remove_title")}
          @click=${() => onDeleteTarget(target.id)}>
          ${icons.close()}
        </button>
      </div>
    `;

    return html`
      <tr draggable="true" data-drag-id=${String(target.id)} class="combo-target-card-row subcombo-group-row ${isExpanded ? "subcombo-expanded" : ""}"
        @dragstart=${(e: DragEvent) => onTargetDragStart(target.id, e)}
        @dragend=${onTargetDragEnd}
        @dragover=${(e: DragEvent) => { e.preventDefault(); (e.currentTarget as HTMLElement).classList.add("drag-over"); }}
        @dragleave=${(e: DragEvent) => (e.currentTarget as HTMLElement).classList.remove("drag-over")}
        @drop=${async (e: DragEvent) => {
          e.preventDefault(); (e.currentTarget as HTMLElement).classList.remove("drag-over");
          const draggedId = parseInt(e.dataTransfer?.getData("text/plain") || "0", 10);
          if (draggedId && draggedId !== target.id && detailComboId) await executeTargetReorder(draggedId, target.id);
        }}>
        <td class="drag-handle col-target-drag" @touchstart=${(e: TouchEvent) => onTouchStartHandle(target.id, e)} @touchmove=${(e: TouchEvent) => onTouchMoveHandle(e)} @touchend=${() => void onTouchEndHandle()}>${icons.dragHandle()}</td>
        <td class="col-target-order">${target.priority_order}</td>
        <td class="col-target-provider"><span class="virtual-provider">${target.provider_id}</span></td>
        <td class="col-target-account"><em>${t("combos.target.na")}</em></td>
        <td class="col-target-model"><div class="target-model-title">${modelCell}</div></td>
        <td class="col-target-context"><em>${t("combos.target.sub_combo")}</em></td>
        ${showWeight ? html`<td class="col-target-weight"><em>${t("combos.target.na")}</em></td>` : html``}
        <td class="col-target-thinking"><em>${t("combos.target.na")}</em></td>
        <td class="col-target-cooldown"><em>${t("combos.target.sub_combo")}</em></td>
        <td class="last-test-cell col-target-test-status"><span class="muted">—</span></td>
        <td class="col-target-actions">${actionsCell}</td>
      </tr>
      ${isExpanded ? renderSubComboAccordion(target, () => requestUpdate()) : html``}
    `;
  }

  const descTag = html`
    <div class="target-desc-row" style="margin-top:3px;">
      <input type="text"
             draggable="false"
             class="cw-input"
             style="font-size:0.75rem;padding:2px 5px;width:100%;max-width:220px;"
             placeholder="Routing description..."
             title="Semantic description / routing criteria for System One decision routing"
             .value=${target.description ?? ""}
             @dragstart=${(e: DragEvent) => e.stopPropagation()}
             @change=${async (e: Event) => {
               const val = (e.target as HTMLInputElement).value.trim() || null;
               await api(`/combos/${detailComboId}/targets/${target.id}`, { method: "PATCH", body: JSON.stringify({ description: val }) });
               target.description = val;
               requestUpdate();
             }}>
    </div>`;
  const modelCell = html`${target.model_display_name || target.model_id || "row #" + target.model_row_id}${cdBadge}${inactBadge}${descTag}`;
  const providerCell = html`<a href="#/providers/${encodeURIComponent(target.provider_id)}">${target.provider_id}</a>`;
  const accountCell = target.account_id ? html`#${target.account_id}` : html`<em>${t("combos.target.rotate")}</em>`;
  const contextCell = target.context_length != null ? html`<span title=${String(target.context_length)}>${formatTokens(target.context_length)}</span>` : html`—`;
  const weightCell = showWeight ? html`<td class="col-target-weight"><input type="number" min="1" draggable="false" @dragstart=${(e: DragEvent) => e.stopPropagation()} .value=${String(target.weight ?? 1)} @change=${async (e: Event) => {
    const val = parseInt((e.target as HTMLInputElement).value, 10) || 1;
    await api(`/combos/${detailComboId}/targets/${target.id}`, { method: "PATCH", body: JSON.stringify({ weight: val }) });
    target.weight = val; requestUpdate();
  }} class="cw-input weight-input" title=${PARAM_TOOLTIPS.weight}></td>` : html``;

  const thinkingCell = html`<td class="col-target-thinking">
    <select class="cw-input" style="font-size:0.75rem;padding:2px 4px;max-width:110px" .value=${target.thinking_effort ?? ""} @change=${async (e: Event) => {
      const val = (e.target as HTMLSelectElement).value || null;
      await api(`/combos/${detailComboId}/targets/${target.id}`, { method: "PATCH", body: JSON.stringify({ thinking_effort: val }) });
      target.thinking_effort = val; requestUpdate();
    }}>
      <option value="" ?selected=${!target.thinking_effort}>${t("combos.target.thinking_passthrough")}</option>
      ${["none", "low", "medium", "high", "max"].map((ef) => html`<option value=${ef} ?selected=${target.thinking_effort === ef}>${ef}</option>`)}
    </select></td>`;

  const cooldownCell = html`<td class="col-target-cooldown">
    <div style="display:flex;align-items:center;gap:4px">
      <select class="cw-input" style="font-size:0.75rem;padding:2px 4px;max-width:95px" @change=${async (e: Event) => {
        const val = (e.target as HTMLSelectElement).value || null;
        await api(`/combos/${detailComboId}/targets/${target.id}`, { method: "PATCH", body: JSON.stringify({ cooldown_mode: val }) });
        target.cooldown_mode = val as CooldownMode | null; requestUpdate();
      }}>
        <option value="" ?selected=${!target.cooldown_mode && target.cooldown_base_secs == null}>${t("combos.target.inherit")}</option>
        <option value="none" ?selected=${target.cooldown_mode === "none" || target.cooldown_base_secs === 0}>${t("combos.target.disabled")}</option>
        <option value="flat" ?selected=${target.cooldown_mode === "flat" && target.cooldown_base_secs !== 0}>${t("combos.detail.mode.flat")}</option>
        <option value="exponential" ?selected=${target.cooldown_mode === "exponential" && target.cooldown_base_secs !== 0}>${t("combos.detail.mode.exponential")}</option>
      </select>
    </div></td>`;

  const tr = detailComboId != null ? state.comboTestResults[detailComboId]?.find((r) => r.target_id === target.id) : undefined;
  const lastTestCell = !tr ? html`<span class="muted">—</span>` : (tr.skipped ? html`<span class="status-pill off">skipped</span>` : html`<span class=${"status-pill " + statusPillClass(tr.status)}>${String(tr.status)}</span>${tr.elapsed_ms != null ? html` <small>${tr.elapsed_ms}ms</small>` : html``}`);

  return html`<tr draggable="true" data-drag-id=${String(target.id)} class="combo-target-card-row"
    @dragstart=${(e: DragEvent) => onTargetDragStart(target.id, e)}
    @dragend=${onTargetDragEnd}
    @dragover=${(e: DragEvent) => { e.preventDefault(); (e.currentTarget as HTMLElement).classList.add("drag-over"); }}
    @dragleave=${(e: DragEvent) => (e.currentTarget as HTMLElement).classList.remove("drag-over")}
    @drop=${async (e: DragEvent) => {
      e.preventDefault(); (e.currentTarget as HTMLElement).classList.remove("drag-over");
      const draggedId = parseInt(e.dataTransfer?.getData("text/plain") || "0", 10);
      if (draggedId && draggedId !== target.id && detailComboId) await executeTargetReorder(draggedId, target.id);
    }}>
    <td class="drag-handle col-target-drag" @touchstart=${(e: TouchEvent) => onTouchStartHandle(target.id, e)} @touchmove=${(e: TouchEvent) => onTouchMoveHandle(e)} @touchend=${() => void onTouchEndHandle()}>${icons.dragHandle()}</td>
    <td class="col-target-order">${target.priority_order}</td>
    <td class="col-target-provider">${providerCell}</td>
    <td class="col-target-account">${accountCell}</td>
    <td class="col-target-model"><div class="target-model-title">${modelCell}</div></td>
    <td class="col-target-context">${contextCell}</td>
    ${weightCell}${thinkingCell}${cooldownCell}
    <td class="last-test-cell col-target-test-status">${lastTestCell}</td>
    <td class="col-target-actions">
      <div class="target-actions-wrap">
        <button class="small primary" title=${t("combos.target.test_title")} @click=${(e: Event) => onTestTarget(target.id, target.model_row_id, e)}>${icons.flask()} ${t("combos.target.test_btn")}</button>
        <button class="small" title=${target.active !== false ? t("combos.target.deactivate_title") : t("combos.target.activate_title")} @click=${() => onToggleTargetActive(target.id, target.active !== false)}>${target.active !== false ? html`${icons.pause()}` : html`${icons.play()}`}</button>
        <button class="small reorder-btn" title=${t("combos.target.move_up_title")} @click=${() => onChangePriority(target.id, -1)}>${icons.caretUp()}</button>
        <button class="small reorder-btn" title=${t("combos.target.move_down_title")} @click=${() => onChangePriority(target.id, 1)}>${icons.caretDown()}</button>
        ${target.in_cooldown ? html`<button class="small" title=${t("combos.target.reset_cd_title")} @click=${() => onResetCooldown(target.id)}>${icons.refresh()}</button>` : html``}
        <button class="small danger" title=${t("combos.target.remove_title")} @click=${() => onDeleteTarget(target.id)}>${icons.close()}</button>
      </div>
    </td>
  </tr>`;
}

function renderComboDetail(): TemplateResult {
  if (!detailCombo) return html`<div class="loading">${t("combos.target.load_error")}</div>`;
  const combo = detailCombo;
  const pm = priorityModeOf(combo);
  const showWeight = pm === "weighted";
  const targets = [...detailTargets].sort((a, b) => a.priority_order - b.priority_order);
  const knownCtx = targets.map((t) => t.context_length).filter((c): c is number => c != null && c > 0);
  const autoCw = knownCtx.length > 0 ? Math.min(...knownCtx) : null;
  const autoCwLabel = autoCw != null ? formatTokens(autoCw) : "—";
  const overrideCw = combo.context_window ?? null;
  const effectiveCw = overrideCw ?? autoCw;
  const effectiveCwLabel = effectiveCw != null ? formatTokens(effectiveCw) : "—";
  const cds = targets.filter((t) => t.in_cooldown);
  const weightTh = showWeight ? html`<th><abbr title=${PARAM_TOOLTIPS.weight}>${t("combos.detail.col.weight")}</abbr></th>` : html``;

  return html`
    <div class="page-header"><a href="#/combos" class="back-link">${icons.arrowLeft()} ${t("combos.detail.back")}</a><h2>${combo.name}</h2>
      <div class="actions">
        <label style="display:inline-flex;align-items:center;gap:5px;font-size:0.85rem;">
          ${t("combos.detail.strategy")}
          <select style="padding:3px 6px;font-size:0.8rem;" .value=${combo.strategy} @change=${(e: Event) => patchCombo(detailComboId!, { strategy: (e.target as HTMLSelectElement).value })}>
            ${["priority", "round_robin", "shuffle"].map((s) => html`<option value=${s} ?selected=${combo.strategy === s}>${s}</option>`)}
          </select>
        </label>
        <span class="chip">${PRIORITY_MODE_LABELS[pm]}</span>
        <label style="display:inline-flex;align-items:center;gap:5px;cursor:pointer;font-size:0.85rem;" title=${t("combos.detail.predictive_rl_hint")}>
          <input type="checkbox" ?checked=${combo.preventive_rate_limit ?? false} @change=${(e: Event) => patchCombo(detailComboId!, { preventive_rate_limit: (e.target as HTMLInputElement).checked })}>
          ${icons.lightning()} ${t("combos.detail.predictive_rl")}
        </label>
        <label>${t("combos.detail.race_size")} <input type="number" min="1" max="8" .value=${String(combo.race_size)} @change=${(e: Event) => {
          const val = parseInt((e.target as HTMLInputElement).value, 10);
          if (Number.isFinite(val) && val >= 1 && val <= 8) patchCombo(detailComboId!, { race_size: val });
        }} class="race-input"></label>
        <button class="danger" @click=${onDeleteCombo}>${t("combos.detail.delete")}</button></div></div>
    <div class="combo-context-window-bar"><label>${t("combos.detail.context_window")}
      <input type="number" min="1" placeholder="auto (${autoCwLabel})" .value=${overrideCw != null ? String(overrideCw) : ""} @change=${(e: Event) => onNumInput("context_window", e)} class="cw-input" title=${t("combos.detail.override_hint")}></label>
      <span class="cw-hint">${t("combos.detail.auto", { value: autoCwLabel })} · ${t("combos.detail.effective", { value: effectiveCwLabel })}</span></div>
    ${renderPriorityModeBar(combo)}${renderCooldownBar(combo)}
    ${cds.length > 0 ? html`<div class="cooldown-banner">${icons.pause()} ${t("combos.detail.cooldown_banner", { count: cds.length, total: targets.length })}</div>` : html``}
    <section class="detail-section"><div class="section-header"><h3>${t("combos.detail.targets_heading", { count: targets.length })}</h3>
      <div class="actions"><button @click=${(e: Event) => detailComboId && testAllTargets(detailComboId, e)}>${icons.flask()} ${t("combos.detail.test_all")}</button><button class="primary" @click=${() => showAddTarget(combo.id)}>${icons.plus()} ${t("combos.detail.add_target")}</button></div></div>
      ${targets.length === 0 ? html`<p class="empty">${t("combos.detail.targets_empty")}</p>` : html`<div class="table-wrap"><table class="combo-targets-table responsive-card-table">
        <thead><tr><th></th><th>${t("combos.detail.col.priority")}</th><th>${t("combos.detail.col.provider")}</th><th>${t("combos.detail.col.account")}</th><th>${t("combos.detail.col.model")}</th><th>${t("combos.detail.col.context")}</th>${weightTh}<th>${t("combos.detail.col.thinking")}</th><th>${t("combos.detail.col.cooldown")}</th><th>${t("combos.detail.col.last_test")}</th><th>${t("combos.detail.col.actions")}</th></tr></thead>
        <tbody>${targets.map((t) => renderTargetRow(t, showWeight))}</tbody></table></div>`}
    </section>`;
}

export async function mountCombos(opts: { detailId?: number } = {}): Promise<(() => void) | void> {
  if (opts.detailId) {
    detailComboId = opts.detailId; detailCombo = null; detailTargets = [];
    return createView<{ combo: Combo | null; targets: ComboTargetWithModel[] }>({
      loader: async () => {
        const [combo, targets] = await Promise.all([
          api("/combos/" + opts.detailId).catch(() => null) as Promise<Combo | null>,
          api("/combos/" + opts.detailId + "/targets") as Promise<ComboTargetWithModel[]>,
        ]);
        return { combo, targets: targets || [] };
      },
      onLoaded: (data) => {
        detailCombo = data.combo;
        detailTargets = data.targets;
        for (const tgt of data.targets) {
          if (tgt.sub_combo_id != null) {
            void loadSubComboData(tgt.sub_combo_id, () => requestUpdate());
          }
        }
      },
      render: () => renderComboDetail(),
      loading: () => html`<div class="loading">${t("common.loading")}</div>`,
      error: (err) => html`<div class="banner banner-error">${err instanceof Error ? err.message : String(err)}</div>`,
    });
  }
  return createView<Combo[]>({
    loader: () => api("/combos") as Promise<Combo[]>,
    onLoaded: (combos) => { state.combos = combos; },
    render: () => renderCombosList(),
    loading: () => html`<div class="loading">${t("common.loading")}</div>`,
    empty: () => false,
    error: (err) => html`<div class="banner banner-error">${err instanceof Error ? err.message : String(err)}</div>`,
  });
}
