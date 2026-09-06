// views/combos.ts — combo grid + combo detail (with target table).
//
// MIGRATED to lit-html for atomic DOM updates. lit-html diffs the
// template against the previous render and only updates the DOM
// nodes that actually changed. This means:
//   - <select> dropdowns stay open when state updates
//   - <input> fields keep focus during re-renders
//   - scroll position is preserved
//   - no full innerHTML rebuild — only changed attributes/text are patched

import { html, type TemplateResult } from 'lit-html';
import { state, type ComboTestResult } from "../state/index.js";
import { api } from "../state/api.js";
import { requestUpdate } from "../state/reactive.js";
import { createView } from "../lib/view-utils.js";
import { showToast } from "../components/toast.js";
import { flashButton } from "../lib/ui-utils.js";
import { showConfirm } from "../lib/show-confirm.js";
import { showCreateCombo, testAllTargets } from "../handlers/combo-handlers.js";
import { showAddTarget } from "../handlers/combo-target-handlers/index.js";
import { icons } from "../lib/icons.js";
import { t } from "../i18n/index.js";
import { statusPillClass, PRIORITY_MODE_LABELS, PRIORITY_MODE_TOOLTIPS, COOLDOWN_MODE_TOOLTIPS } from "../lib/constants.js";
import type {
  Combo,
  ComboTargetWithModel,
  PriorityMode,
  CooldownMode,
} from "../lib/types/api.js";

const PARAM_TOOLTIPS = {
  exploration_rate: "Probability (0.0–1.0) of trying a different target instead of the best-known one. 0.1 = 10% exploration. The exploration is priority-weighted: targets positioned first in the combo are more likely to be explored. Higher exploration rates discover alternatives faster but may pick suboptimal targets.",
  base_secs: "Initial cooldown duration in seconds. For exponential mode, this is multiplied by factor^(failures-1).",
  factor: "Multiplier applied to the cooldown after each failure. 2 = doubling.",
  max_secs: "Maximum cooldown duration in seconds. The exponential growth is capped at this value.",
  window_secs: "How far back to look at usage data for the selection algorithm. 3600 = 1 hour.",
  weight: "Relative weight for weighted random selection. Higher = more likely to be selected. Default 1.",
};

function priorityModeOf(c: Combo): PriorityMode { return (c.priority_mode ?? "strict") as PriorityMode; }
function cooldownModeOf(c: Combo): CooldownMode { return (c.cooldown_mode ?? "flat") as CooldownMode; }

function formatTokens(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1).replace(/\.0$/, "") + "M";
  if (n >= 1_000) return (n / 1_000).toFixed(0) + "k";
  return String(n);
}

// ---- State ----

let detailComboId: number | null = null;
let detailCombo: Combo | null = null;
let detailTargets: ComboTargetWithModel[] = [];

// ---- API helpers ----

async function patchCombo(id: number, body: Record<string, unknown>): Promise<void> {
  try {
    await api("/combos/" + id, { method: "PATCH", body: JSON.stringify(body) });
    if (detailCombo) Object.assign(detailCombo, body);
    const combo = (state.combos || []).find((c) => c.id === id);
    if (combo) Object.assign(combo, body);
    requestUpdate();
  } catch (err: unknown) {
    showToast(t("combos.toast.error", { message: err instanceof Error ? err.message : String(err) }), "error");
  }
}

// ---- Handlers ----

async function onUpdatePriorityMode(e: Event): Promise<void> {
  const val = (e.target as HTMLSelectElement).value;
  await patchCombo(detailComboId!, { priority_mode: val });
}
async function onUpdateCooldownMode(e: Event): Promise<void> {
  const val = (e.target as HTMLSelectElement).value;
  await patchCombo(detailComboId!, { cooldown_mode: val });
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
async function onUpdateRaceSize(e: Event): Promise<void> {
  if (e.type === "input") return;
  const val = parseInt((e.target as HTMLInputElement).value, 10);
  if (!Number.isFinite(val) || val < 1 || val > 8) return;
  await patchCombo(detailComboId!, { race_size: val });
}
async function onUpdateContextWindow(e: Event): Promise<void> {
  if (e.type === "input") return;
  const raw = (e.target as HTMLInputElement).value.trim();
  const val = raw === "" ? null : parseInt(raw, 10);
  if (raw !== "" && !Number.isFinite(val)) return;
  await patchCombo(detailComboId!, { context_window: val });
}
async function onTogglePreventiveRateLimit(e: Event): Promise<void> {
  const checked = (e.target as HTMLInputElement).checked;
  await patchCombo(detailComboId!, { preventive_rate_limit: checked });
}
async function onUpdateTargetWeight(targetId: number, e: Event): Promise<void> {
  if (e.type === "input") return;
  const raw = (e.target as HTMLInputElement).value.trim();
  const val = raw === "" ? 1 : parseInt(raw, 10);
  if (!Number.isFinite(val) || val < 1) return;
  try {
    await api(`/combos/${detailComboId}/targets/${targetId}`, { method: "PATCH", body: JSON.stringify({ weight: val }) });
    const t2 = detailTargets.find((t2) => t2.id === targetId);
    if (t2) t2.weight = val;
    requestUpdate();
  } catch (err: unknown) { showToast(t("combos.toast.error", { message: err instanceof Error ? err.message : String(err) }), "error"); }
}
async function onDeleteCombo(): Promise<void> {
  if (!detailComboId || !(await showConfirm({
    title: t("combos.confirm.delete_title"),
    message: t("combos.confirm.delete_message", { name: detailCombo?.name ?? String(detailComboId) }),
    danger: true,
    confirmLabel: t("combos.confirm.delete_btn"),
  }))) return;
  try {
    await api(`/combos/${detailComboId}`, { method: "DELETE" });
    location.hash = "#/combos";
  } catch (err: unknown) { showToast(t("combos.toast.error", { message: err instanceof Error ? err.message : String(err) }), "error"); }
}

async function onToggleTargetActive(targetId: number, currentActive: boolean): Promise<void> {
  try {
    await api(`/combos/${detailComboId}/targets/${targetId}`, { method: "PATCH", body: JSON.stringify({ active: !currentActive }) });
    const tgt = detailTargets.find((tgt) => tgt.id === targetId);
    if (tgt) tgt.active = !currentActive;
    requestUpdate();
  } catch (err: unknown) { showToast(t("combos.toast.error", { message: err instanceof Error ? err.message : String(err) }), "error"); }
}
async function onUpdateStrategy(e: Event): Promise<void> {
  const val = (e.target as HTMLSelectElement).value;
  await patchCombo(detailComboId!, { strategy: val });
}
async function onDeleteTarget(targetId: number): Promise<void> {
  if (!(await showConfirm({
    title: t("combos.confirm.remove_title"),
    message: t("combos.confirm.remove_message"),
    danger: true,
    confirmLabel: t("combos.confirm.remove_btn"),
  }))) return;
  try {
    await api(`/combos/${detailComboId}/targets/${targetId}`, { method: "DELETE" });
    detailTargets = detailTargets.filter((t) => t.id !== targetId);
    requestUpdate();
  } catch (err: unknown) { showToast(t("combos.toast.error", { message: err instanceof Error ? err.message : String(err) }), "error"); }
}
async function onChangePriority(targetId: number, delta: number): Promise<void> {
  try {
    // Re-fetch current targets to avoid stale IDs
    const currentTargets = await api(`/combos/${detailComboId}/targets`) as ComboTargetWithModel[];
    const ordered = [...currentTargets].sort((a, b) => a.priority_order - b.priority_order);
    const idx = ordered.findIndex((t) => t.id === targetId);
    if (idx < 0) return;
    const newIdx = idx + delta;
    if (newIdx < 0 || newIdx >= ordered.length) return;
    const tmp = ordered[idx]!;
    ordered[idx] = ordered[newIdx]!;
    ordered[newIdx] = tmp;
    await api(`/combos/${detailComboId}/targets/reorder`, { method: "POST", body: JSON.stringify({ target_ids: ordered.map((t) => t.id) }) });
    detailTargets = await api(`/combos/${detailComboId}/targets`) as ComboTargetWithModel[];
    requestUpdate();
  } catch (err: unknown) { showToast(t("combos.toast.error", { message: err instanceof Error ? err.message : String(err) }), "error"); }
}
async function onResetCooldown(targetId: number): Promise<void> {
  try {
    await api(`/combos/${detailComboId}/targets/${targetId}/clear-cooldown`, { method: "POST" });
    const target = detailTargets.find((target) => target.id === targetId);
    if (target) { target.in_cooldown = false; target.cooldown_until = null; target.cooldown_reason = null; }
    requestUpdate();
  } catch (err: unknown) { showToast(t("combos.toast.error", { message: err instanceof Error ? err.message : String(err) }), "error"); }
}

async function onUpdateTargetCooldownMode(targetId: number, e: Event): Promise<void> {
  const select = e.target as HTMLSelectElement;
  const val = select.value || null;
  try {
    await api(`/combos/${detailComboId}/targets/${targetId}`, {
      method: "PATCH",
      body: JSON.stringify({ cooldown_mode: val }),
    });
    const target = detailTargets.find((x) => x.id === targetId);
    if (target) target.cooldown_mode = val as CooldownMode | null;
    requestUpdate();
  } catch (err: unknown) {
    showToast(t("combos.toast.error_updating_cooldown", { message: err instanceof Error ? err.message : String(err) }), "error");
  }
}

async function onUpdateTargetCooldownBase(targetId: number, e: Event): Promise<void> {
  const input = e.target as HTMLInputElement;
  const raw = input.value.trim();
  const val = raw === "" ? null : parseInt(raw, 10);
  if (val != null && (Number.isNaN(val) || val < 0)) return;
  try {
    await api(`/combos/${detailComboId}/targets/${targetId}`, {
      method: "PATCH",
      body: JSON.stringify({ cooldown_base_secs: val }),
    });
    const target = detailTargets.find((x) => x.id === targetId);
    if (target) target.cooldown_base_secs = val;
    requestUpdate();
  } catch (err: unknown) {
    showToast(t("combos.toast.error_updating_cooldown_base", { message: err instanceof Error ? err.message : String(err) }), "error");
  }
}

// `onTestAllTargets` is now a thin shim into `testAllTargets` from
// combo-handlers.ts. That handler:
//   1. Disables the button and sets text to "🧪 Testing..."
//   2. Calls `POST /combos/:id/test-all`
//   3. Stores results in `state.comboTestResults[comboId]`
//   4. Re-enables the button
//   5. Calls `requestUpdate()`
//
// We keep this wrapper so the lit-html `@click=${onTestAllTargets}` site
// (which fires with no arg) doesn't need to know about the `(comboId, e)`
// signature. The previous implementation here discarded the results and
// only showed a count toast — there was no "Last test" column, so the
// operator couldn't see per-target outcomes without opening devtools.
async function onTestAllTargets(e: Event): Promise<void> {
  if (!detailComboId) return;
  await testAllTargets(detailComboId, e);
}

// Per-row test button. Mirrors `onTestModel` in views/providers.ts:
// disable + "Testing..." text while in flight, then a ✓/✗ flash on
// completion. The previous implementation here only showed a toast,
// which gave no visible feedback on the row itself.
async function onTestTarget(targetId: number, modelRowId: number | null, e: Event): Promise<void> {
  const btn = (e && e.target instanceof HTMLButtonElement ? e.target : null) as HTMLButtonElement | null;
  if (!modelRowId) { showToast(t("combos.target.no_model_to_test"), "warning"); return; }
  const oldText = btn ? btn.textContent : null;
  if (btn) { btn.disabled = true; btn.textContent = t("combos.target.testing"); }
  try {
    const result = await api(`/models/${modelRowId}/test`, { method: "POST" }) as { status: number; elapsed_ms?: number };
    if (result.status >= 200 && result.status < 300) {
      if (btn) flashButton(btn, "✓", "#a6e3a1");
    } else if (result.status === 0) {
      if (btn) flashButton(btn, "✗ net", "#f38ba8");
    } else {
      if (btn) flashButton(btn, "✗ " + result.status, "#f38ba8");
    }
    // Also drop the result into the combo test-results cache so the
    // "Last test" column updates for this row immediately. We synthesise
    // a single-element array; the next full Test-all run will overwrite
    // it with the complete picture.
    if (detailComboId) {
      const prev = state.comboTestResults[detailComboId] || [];
      const next: ComboTestResult[] = prev.filter((r) => r.target_id !== targetId);
      next.push({
        target_id: targetId,
        provider_id: "",
        model_row_id: modelRowId,
        status: result.status,
        elapsed_ms: result.elapsed_ms ?? null,
        error_msg: null,
        skipped: false,
      });
      state.comboTestResults[detailComboId] = next;
    }
    requestUpdate();
  } catch (err: unknown) {
    if (btn) flashButton(btn, "✗", "#f38ba8");
    showToast(t("combos.target.test_failed", { message: err instanceof Error ? err.message : String(err) }), "error");
  } finally {
    if (btn) {
      setTimeout(() => {
        btn.disabled = false;
        btn.textContent = oldText || t("combos.target.test_btn");
      }, 1500);
    }
  }
}

// ---- Templates ----

function priorityModeOptions(selected: PriorityMode): TemplateResult {
  const modes: PriorityMode[] = ["strict", "lkgp", "weighted", "least_used", "p2c"];
  return html`${modes.map((m) => html`<option value=${m} ?selected=${m === selected}>${PRIORITY_MODE_LABELS[m]}</option>`)}`;
}
function cooldownModeOptions(selected: CooldownMode): TemplateResult {
  const modes: CooldownMode[] = ["flat", "exponential", "none"];
  return html`${modes.map((m) => html`<option value=${m} ?selected=${m === selected}>${m === "flat" ? t("combos.detail.mode.flat") : m === "exponential" ? t("combos.detail.mode.exponential") : t("combos.detail.mode.disabled")}</option>`)}`;
}

function renderPriorityModeBar(combo: Combo): TemplateResult {
  const pm = priorityModeOf(combo);
  let params: TemplateResult = html``;
  if (pm === "lkgp") {
    const rate = combo.lkgp_exploration_rate != null ? String(combo.lkgp_exploration_rate) : "";
    params = html`<details class="combo-mode-params" open><summary>${t("combos.detail.parameters")}</summary><div class="combo-mode-params-body">
      <label><abbr title=${PARAM_TOOLTIPS.exploration_rate}>Exploration Rate (0.0–1.0)</abbr>
        <input type="number" min="0" max="1" step="0.05" .value=${rate} placeholder="0.1" @change=${(e: Event) => onFloatInput("lkgp_exploration_rate", e)} @input=${(e: Event) => onFloatInput("lkgp_exploration_rate", e)} class="cw-input"></label></div></details>`;
  } else if (pm === "least_used" || pm === "p2c") {
    const win = combo.selection_window_secs != null ? String(combo.selection_window_secs) : "";
    params = html`<details class="combo-mode-params" open><summary>${t("combos.detail.parameters")}</summary><div class="combo-mode-params-body">
      <label><abbr title=${PARAM_TOOLTIPS.window_secs}>Window (s)</abbr>
        <input type="number" min="1" .value=${win} placeholder="3600" @change=${(e: Event) => onNumInput("selection_window_secs", e)} @input=${(e: Event) => onNumInput("selection_window_secs", e)} class="cw-input"></label></div></details>`;
  } else if (pm === "weighted") {
    params = html`<details class="combo-mode-params" open><summary>${t("combos.detail.parameters")}</summary><div class="combo-mode-params-body"><span class="muted">${t("combos.detail.weighted_hint")}</span></div></details>`;
  }
  return html`<div class="combo-settings-bar"><label><abbr title=${PRIORITY_MODE_TOOLTIPS[pm]}>${t("combos.detail.priority_mode")}</abbr><select @change=${onUpdatePriorityMode}>${priorityModeOptions(pm)}</select></label>${params}</div>`;
}

function renderCooldownBar(combo: Combo): TemplateResult {
  const cm = cooldownModeOf(combo);
  const base = combo.cooldown_base_secs != null ? String(combo.cooldown_base_secs) : "";
  const factor = combo.cooldown_factor != null ? String(combo.cooldown_factor) : "";
  const max = combo.cooldown_max_secs != null ? String(combo.cooldown_max_secs) : "";
  const params = cm === "exponential" ? html`<details class="combo-mode-params" open><summary>${t("combos.detail.parameters")}</summary><div class="combo-mode-params-body">
    <label><abbr title=${PARAM_TOOLTIPS.base_secs}>Base (s)</abbr><input type="number" min="1" .value=${base} placeholder="60" @change=${(e: Event) => onNumInput("cooldown_base_secs", e)} @input=${(e: Event) => onNumInput("cooldown_base_secs", e)} class="cw-input"></label>
    <label><abbr title=${PARAM_TOOLTIPS.factor}>Factor</abbr><input type="number" min="2" .value=${factor} placeholder="2" @change=${(e: Event) => onNumInput("cooldown_factor", e)} @input=${(e: Event) => onNumInput("cooldown_factor", e)} class="cw-input"></label>
    <label><abbr title=${PARAM_TOOLTIPS.max_secs}>Max (s)</abbr><input type="number" min="1" .value=${max} placeholder="3600" @change=${(e: Event) => onNumInput("cooldown_max_secs", e)} @input=${(e: Event) => onNumInput("cooldown_max_secs", e)} class="cw-input"></label>
  </div></details>` : html``;
  return html`<div class="combo-settings-bar"><label><abbr title=${COOLDOWN_MODE_TOOLTIPS[cm]}>${t("combos.detail.cooldown_mode")}</abbr><select @change=${onUpdateCooldownMode}>${cooldownModeOptions(cm)}</select></label>${params}</div>`;
}

let touchDragState: {
  draggedId: number;
  rowEl: HTMLElement;
  currentOverEl: HTMLElement | null;
} | null = null;

function onTouchStartHandle(targetId: number, e: TouchEvent): void {
  const handle = e.currentTarget as HTMLElement;
  const row = handle.closest(".combo-target-card-row") as HTMLElement | null;
  if (!row) return;

  touchDragState = {
    draggedId: targetId,
    rowEl: row,
    currentOverEl: null,
  };

  row.classList.add("touch-dragging");
  if (navigator.vibrate) {
    try { navigator.vibrate(15); } catch { /* ignore */ }
  }
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
    const dropTargetIdStr = currentOverEl.getAttribute("data-drag-id");
    const dropTargetId = dropTargetIdStr ? parseInt(dropTargetIdStr, 10) : 0;
    if (dropTargetId && dropTargetId !== draggedId) {
      await executeTargetReorder(draggedId, dropTargetId);
    }
  }

  touchDragState = null;
}

async function executeTargetReorder(draggedId: number, dropTargetId: number): Promise<void> {
  if (!detailComboId || draggedId === dropTargetId) return;
  let currentTargets: ComboTargetWithModel[];
  try {
    currentTargets = await api(`/combos/${detailComboId}/targets`) as ComboTargetWithModel[];
  } catch {
    showToast(t("combos.toast.reorder_failed", { message: "could not fetch current targets" }), "error");
    return;
  }
  const ordered = [...currentTargets].sort((a, b) => a.priority_order - b.priority_order);
  const fromIdx = ordered.findIndex((x) => x.id === draggedId);
  const toIdx = ordered.findIndex((x) => x.id === dropTargetId);
  if (fromIdx < 0 || toIdx < 0) {
    showToast(t("combos.toast.reorder_failed", { message: "target not found in current list" }), "error");
    return;
  }
  const [moved] = ordered.splice(fromIdx, 1);
  if (!moved) return;
  ordered.splice(toIdx, 0, moved);
  try {
    await api(`/combos/${detailComboId}/targets/reorder`, {
      method: "POST",
      body: JSON.stringify({ target_ids: ordered.map((x) => x.id) }),
    });
    detailTargets = await api(`/combos/${detailComboId}/targets`) as ComboTargetWithModel[];
    requestUpdate();
    showToast(t("combos.toast.target_priority_updated"), "success");
  } catch (err: unknown) {
    showToast(t("combos.toast.reorder_failed", { message: err instanceof Error ? err.message : String(err) }), "error");
  }
}

function renderTargetRow(target: ComboTargetWithModel, showWeight: boolean): TemplateResult {
  const isSub = target.sub_combo_id != null;
  const cdBadge = target.in_cooldown ? html` <span class="badge badge-cooldown" title=${t("combos.target.cooldown_badge_title", { reason: target.cooldown_reason ?? "", until: target.cooldown_until ?? "" })}>⏸</span>` : html``;
  const inactiveBadge = (target.provider_active === false)
    ? html` <span class="badge badge-inactive" title=${t("combos.target.provider_inactive_title")}>⚠ inactive</span>`
    : html``;
  const targetInactiveBadge = (target.active === false)
    ? html` <span class="badge badge-inactive" title=${t("combos.target.target_inactive_title")}>⏸ inactive</span>`
    : html``;
  const modelCell = isSub ? html`<span class="chip combo-chip">→ combo: ${target.sub_combo_name ?? "#" + target.sub_combo_id}</span>` : html`${target.model_display_name || target.model_id || "row #" + target.model_row_id}${cdBadge}${inactiveBadge}${targetInactiveBadge}`;
  const providerCell = isSub ? html`<span class="virtual-provider">${target.provider_id}</span>` : html`<a href="#/providers/${encodeURIComponent(target.provider_id)}">${target.provider_id}</a>`;
  const accountCell = isSub ? html`<em>${t("combos.target.na")}</em>` : (target.account_id ? html`#${target.account_id}` : html`<em>${t("combos.target.rotate")}</em>`);
  const contextCell = isSub ? html`<em>${t("combos.target.sub_combo")}</em>` : (target.context_length != null ? html`<span title=${String(target.context_length)}>${formatTokens(target.context_length)}</span>` : html`—`);
  const weightCell = showWeight ? (isSub ? html`<td class="col-target-weight" data-label=${t("combos.detail.col.weight")}><em>${t("combos.target.na")}</em></td>` : html`<td class="col-target-weight" data-label=${t("combos.detail.col.weight")}><input type="number" min="1" .value=${String(target.weight ?? 1)} @change=${(e: Event) => onUpdateTargetWeight(target.id, e)} @input=${(e: Event) => onUpdateTargetWeight(target.id, e)} class="cw-input weight-input" title=${PARAM_TOOLTIPS.weight}></td>`) : html``;
  const cooldownCell = isSub ? html`<td class="col-target-cooldown" data-label=${t("combos.detail.col.cooldown")}><em>${t("combos.target.sub_combo")}</em></td>` : html`<td class="col-target-cooldown" data-label=${t("combos.detail.col.cooldown")}>
    <div style="display:flex;align-items:center;gap:4px">
      <select class="cw-input" style="font-size:0.75rem;padding:2px 4px;max-width:95px" @change=${(e: Event) => onUpdateTargetCooldownMode(target.id, e)}>
        <option value="" ?selected=${!target.cooldown_mode && target.cooldown_base_secs == null}>${t("combos.target.inherit")}</option>
        <option value="none" ?selected=${target.cooldown_mode === "none" || target.cooldown_base_secs === 0}>${t("combos.target.disabled")}</option>
        <option value="flat" ?selected=${target.cooldown_mode === "flat" && target.cooldown_base_secs !== 0}>${t("combos.detail.mode.flat")}</option>
        <option value="exponential" ?selected=${target.cooldown_mode === "exponential" && target.cooldown_base_secs !== 0}>${t("combos.detail.mode.exponential")}</option>
      </select>
      ${(target.cooldown_mode === "flat" || target.cooldown_mode === "exponential") ? html`<input type="number" min="0" placeholder="sec" style="width:48px;font-size:0.75rem;padding:2px 4px" .value=${target.cooldown_base_secs != null ? String(target.cooldown_base_secs) : ""} @change=${(e: Event) => onUpdateTargetCooldownBase(target.id, e)} class="cw-input" title=${t("combos.target.cooldown_base_title")}>` : html``}
    </div>
  </td>`;
  const testResults: ComboTestResult[] | undefined = detailComboId != null
    ? state.comboTestResults[detailComboId]
    : undefined;
  const tr = testResults?.find((r) => r.target_id === target.id);
  let lastTestCell: TemplateResult;
  if (!tr) {
    lastTestCell = html`<span class="muted">—</span>`;
  } else if (tr.skipped) {
    const reason = tr.error_msg ?? "skipped";
    lastTestCell = html`<span class="status-pill off" title=${reason}>skipped</span> <small>${reason}</small>`;
  } else {
    const cls = statusPillClass(tr.status);
    const ms = tr.elapsed_ms != null ? html` <small>${tr.elapsed_ms}ms</small>` : html``;
    const err = tr.error_msg ? html` <small title=${tr.error_msg}>${tr.error_msg}</small>` : html``;
    lastTestCell = html`<span class=${"status-pill " + cls}>${String(tr.status)}</span>${ms}${err}`;
  }
  return html`<tr draggable="true" data-drag-id=${String(target.id)} class="combo-target-card-row"
    @dragstart=${(e: DragEvent) => { e.dataTransfer?.setData("text/plain", String(target.id)); (e.target as HTMLElement).classList.add("dragging"); }}
    @dragend=${(e: DragEvent) => { (e.target as HTMLElement).classList.remove("dragging"); }}
    @dragover=${(e: DragEvent) => { e.preventDefault(); const row = (e.currentTarget as HTMLElement); row.classList.add("drag-over"); }}
    @dragleave=${(e: DragEvent) => { (e.currentTarget as HTMLElement).classList.remove("drag-over"); }}
    @drop=${async (e: DragEvent) => {
      e.preventDefault();
      (e.currentTarget as HTMLElement).classList.remove("drag-over");
      const draggedId = parseInt(e.dataTransfer?.getData("text/plain") || "0", 10);
      if (!draggedId || draggedId === target.id || !detailComboId) return;
      await executeTargetReorder(draggedId, target.id);
    }}
  >
    <td class="drag-handle col-target-drag"
        title=${t("combos.detail.col.drag_title")}
        @touchstart=${(e: TouchEvent) => onTouchStartHandle(target.id, e)}
        @touchmove=${(e: TouchEvent) => onTouchMoveHandle(e)}
        @touchend=${() => void onTouchEndHandle()}
        @touchcancel=${() => void onTouchEndHandle()}>${icons.dragHandle()}</td>
    <td class="col-target-order" data-label=${t("combos.detail.col.priority")}>${target.priority_order}</td>
    <td class="col-target-provider" data-label=${t("combos.detail.col.provider")}>${providerCell}</td>
    <td class="col-target-account" data-label=${t("combos.detail.col.account")}>${accountCell}</td>
    <td class="col-target-model" data-label=${t("combos.detail.col.model")}><div class="target-model-title">${modelCell}</div></td>
    <td class="col-target-context" data-label=${t("combos.detail.col.context")}>${contextCell}</td>
    ${weightCell}
    ${cooldownCell}
    <td class="last-test-cell col-target-test-status" data-label=${t("combos.detail.col.last_test")}>${lastTestCell}</td>
    <td class="col-target-actions" data-label=${t("combos.detail.col.actions")}>
      <div class="target-actions-wrap">
        ${!isSub ? html`<button class="small primary" title=${t("combos.target.test_title")} @click=${(e: Event) => onTestTarget(target.id, target.model_row_id, e)}>${icons.flask()} ${t("combos.target.test_btn")}</button>` : html``}
        <button class="small" title=${target.active !== false ? t("combos.target.deactivate_title") : t("combos.target.activate_title")} @click=${() => onToggleTargetActive(target.id, target.active !== false)}>${target.active !== false ? html`${icons.pause()} ${t("combos.target.pause")}` : html`${icons.play()} ${t("combos.target.resume")}`}</button>
        <button class="small reorder-btn" title=${t("combos.target.move_up_title")} @click=${() => onChangePriority(target.id, -1)}>${icons.caretUp()}</button>
        <button class="small reorder-btn" title=${t("combos.target.move_down_title")} @click=${() => onChangePriority(target.id, 1)}>${icons.caretDown()}</button>
        ${target.in_cooldown && !isSub ? html`<button class="small" title=${t("combos.target.reset_cd_title")} @click=${() => onResetCooldown(target.id)}>${icons.refresh()} ${t("combos.target.reset_cd")}</button>` : html``}
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
          <select style="padding:3px 6px;font-size:0.8rem;border-radius:var(--radius-sm);" .value=${combo.strategy} @change=${onUpdateStrategy}>
            <option value="priority" ?selected=${combo.strategy === "priority"}>priority</option>
            <option value="round_robin" ?selected=${combo.strategy === "round_robin"}>round_robin</option>
            <option value="shuffle" ?selected=${combo.strategy === "shuffle"}>shuffle</option>
          </select>
        </label>
        <span class="chip">${PRIORITY_MODE_LABELS[pm]}</span>
        <label style="display:inline-flex;align-items:center;gap:5px;cursor:pointer;font-size:0.85rem;" title=${t("combos.detail.predictive_rl_hint")}>
          <input type="checkbox" ?checked=${combo.preventive_rate_limit ?? false} @change=${onTogglePreventiveRateLimit}>
          ${icons.lightning()} ${t("combos.detail.predictive_rl")}
        </label>
        <label>${t("combos.detail.race_size")} <input type="number" min="1" max="8" .value=${String(combo.race_size)} @change=${onUpdateRaceSize} @input=${onUpdateRaceSize} class="race-input"></label>
        <button class="danger" @click=${onDeleteCombo}>${t("combos.detail.delete")}</button></div></div>
    <div class="combo-context-window-bar"><label>${t("combos.detail.context_window")}
      <input type="number" min="1" placeholder="auto (${autoCwLabel})" .value=${overrideCw != null ? String(overrideCw) : ""} @change=${onUpdateContextWindow} @input=${onUpdateContextWindow} class="cw-input" title=${t("combos.detail.override_hint")}></label>
      <span class="cw-hint">${t("combos.detail.auto", { value: autoCwLabel })} · ${t("combos.detail.effective", { value: effectiveCwLabel })}</span></div>
    ${renderPriorityModeBar(combo)}${renderCooldownBar(combo)}
    ${cds.length > 0 ? html`<div class="cooldown-banner">${icons.pause()} ${t("combos.detail.cooldown_banner", { count: cds.length, total: targets.length })}</div>` : html``}
    <section class="detail-section"><div class="section-header"><h3>${t("combos.detail.targets_heading", { count: targets.length })}</h3>
      <div class="actions"><button @click=${onTestAllTargets}>${icons.flask()} ${t("combos.detail.test_all")}</button><button class="primary" @click=${() => showAddTarget(combo.id)}>${icons.plus()} ${t("combos.detail.add_target")}</button></div></div>
      ${targets.length === 0 ? html`<p class="empty">${t("combos.detail.targets_empty")}</p>` : html`<div class="table-wrap"><table class="combo-targets-table responsive-card-table">
        <thead><tr><th></th><th>${t("combos.detail.col.priority")}</th><th>${t("combos.detail.col.provider")}</th><th>${t("combos.detail.col.account")}</th><th>${t("combos.detail.col.model")}</th><th>${t("combos.detail.col.context")}</th>${weightTh}<th>${t("combos.detail.col.cooldown")}</th><th>${t("combos.detail.col.last_test")}</th><th>${t("combos.detail.col.actions")}</th></tr></thead>
        <tbody>${targets.map((t) => renderTargetRow(t, showWeight))}</tbody></table></div>`}
    </section>`;
}

function renderComboGrid(): TemplateResult {
  const list = state.combos || [];
  return html`<div class="page-header"><h2>${t("combos.grid.title")}</h2><div class="actions"><button class="primary" @click=${() => showCreateCombo()}>${icons.plus()} ${t("combos.grid.create")}</button></div></div>
    ${list.length === 0 ? html`<p class="empty">${t("combos.grid.empty")}</p>` : html`<div class="combo-grid">${list.map((c) => {
      const pm = priorityModeOf(c);
      const pmChip = pm === "strict" ? html`` : html` · <span class="chip">${PRIORITY_MODE_LABELS[pm]}</span>`;
      return html`<a class="combo-card" href="#/combos/${c.id}"><h3>${c.name}</h3><div class="provider-meta"><span class="chip">${c.strategy}</span>${pmChip} · race ${c.race_size}</div></a>`;
    })}</div>`}`;
}

// ---- Mount ----

interface ComboDetailData {
  combo: Combo | null;
  targets: ComboTargetWithModel[];
}

export async function mountCombos(opts: { detailId?: number } = {}): Promise<(() => void) | void> {
  if (opts.detailId) {
    detailComboId = opts.detailId;
    detailCombo = null;
    detailTargets = [];
    return createView<ComboDetailData>({
      loader: async () => {
        const [combo, targets] = await Promise.all([
          api("/combos/" + opts.detailId).catch(() => null) as Promise<Combo | null>,
          api("/combos/" + opts.detailId + "/targets") as Promise<ComboTargetWithModel[]>,
        ]);
        return { combo, targets: targets || [] };
      },
      render: (data) => {
        detailCombo = data.combo;
        detailTargets = data.targets;
        return renderComboDetail();
      },
      loading: () => html`<div class="loading">${t("common.loading")}</div>`,
      error: (err) => html`<div class="banner banner-error">${err instanceof Error ? err.message : String(err)}</div>`,
    });
  }

  return createView<Combo[]>({
    loader: () => api("/combos") as Promise<Combo[]>,
    render: (combos) => {
      state.combos = combos;
      return renderComboGrid();
    },
    loading: () => html`<div class="loading">${t("common.loading")}</div>`,
    empty: () => false, // renderComboGrid handles empty state internally
    error: (err) => html`<div class="banner banner-error">${err instanceof Error ? err.message : String(err)}</div>`,
  });
}
