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
import { mountView, requestUpdate } from "../state/reactive.js";
import { showToast } from "../components/toast.js";
import { flashButton } from "../lib/ui-utils.js";
import { showCreateCombo, testAllTargets } from "../handlers/combo-handlers.js";
import { showAddTarget } from "../handlers/combo-target-handlers.js";
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
    showToast("Error: " + (err instanceof Error ? err.message : String(err)), "error");
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
    const t = detailTargets.find((t) => t.id === targetId);
    if (t) t.weight = val;
    requestUpdate();
  } catch (err: unknown) { showToast("Error: " + (err instanceof Error ? err.message : String(err)), "error"); }
}
async function onDeleteCombo(): Promise<void> {
  if (!detailComboId || !confirm(`Delete combo "${detailCombo?.name ?? detailComboId}"?`)) return;
  try {
    await api(`/combos/${detailComboId}`, { method: "DELETE" });
    location.hash = "#/combos";
  } catch (err: unknown) { showToast("Error: " + (err instanceof Error ? err.message : String(err)), "error"); }
}

async function onToggleTargetActive(targetId: number, currentActive: boolean): Promise<void> {
  try {
    await api(`/combos/${detailComboId}/targets/${targetId}`, { method: "PATCH", body: JSON.stringify({ active: !currentActive }) });
    const t = detailTargets.find((t) => t.id === targetId);
    if (t) t.active = !currentActive;
    requestUpdate();
  } catch (err: unknown) { showToast("Error: " + (err instanceof Error ? err.message : String(err)), "error"); }
}
async function onUpdateStrategy(e: Event): Promise<void> {
  const val = (e.target as HTMLSelectElement).value;
  await patchCombo(detailComboId!, { strategy: val });
}
async function onDeleteTarget(targetId: number): Promise<void> {
  if (!confirm("Are you sure you want to remove this target from the combo?")) return;
  try {
    await api(`/combos/${detailComboId}/targets/${targetId}`, { method: "DELETE" });
    detailTargets = detailTargets.filter((t) => t.id !== targetId);
    requestUpdate();
  } catch (err: unknown) { showToast("Error: " + (err instanceof Error ? err.message : String(err)), "error"); }
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
  } catch (err: unknown) { showToast("Error: " + (err instanceof Error ? err.message : String(err)), "error"); }
}
async function onResetCooldown(targetId: number): Promise<void> {
  try {
    await api(`/combos/${detailComboId}/targets/${targetId}/clear-cooldown`, { method: "POST" });
    const t = detailTargets.find((t) => t.id === targetId);
    if (t) { t.in_cooldown = false; t.cooldown_until = null; t.cooldown_reason = null; }
    requestUpdate();
  } catch (err: unknown) { showToast("Error: " + (err instanceof Error ? err.message : String(err)), "error"); }
}

async function onUpdateTargetCooldownMode(targetId: number, e: Event): Promise<void> {
  const select = e.target as HTMLSelectElement;
  const val = select.value || null;
  try {
    await api(`/combos/${detailComboId}/targets/${targetId}`, {
      method: "PATCH",
      body: JSON.stringify({ cooldown_mode: val }),
    });
    const t = detailTargets.find((x) => x.id === targetId);
    if (t) t.cooldown_mode = val as CooldownMode | null;
    requestUpdate();
  } catch (err: unknown) {
    showToast("Error updating target cooldown: " + (err instanceof Error ? err.message : String(err)), "error");
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
    const t = detailTargets.find((x) => x.id === targetId);
    if (t) t.cooldown_base_secs = val;
    requestUpdate();
  } catch (err: unknown) {
    showToast("Error updating target cooldown base: " + (err instanceof Error ? err.message : String(err)), "error");
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
  if (!modelRowId) { showToast("No model to test for this target", "warning"); return; }
  const oldText = btn ? btn.textContent : null;
  if (btn) { btn.disabled = true; btn.textContent = "Testing..."; }
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
    showToast("Test failed: " + (err instanceof Error ? err.message : String(err)), "error");
  } finally {
    if (btn) {
      setTimeout(() => {
        btn.disabled = false;
        btn.textContent = oldText || "🧪";
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
  return html`${modes.map((m) => html`<option value=${m} ?selected=${m === selected}>${m === "flat" ? "Flat" : m === "exponential" ? "Exponential" : "Disabled (None)"}</option>`)}`;
}

function renderPriorityModeBar(combo: Combo): TemplateResult {
  const pm = priorityModeOf(combo);
  let params: TemplateResult = html``;
  if (pm === "lkgp") {
    const rate = combo.lkgp_exploration_rate != null ? String(combo.lkgp_exploration_rate) : "";
    params = html`<details class="combo-mode-params" open><summary>Parameters</summary><div class="combo-mode-params-body">
      <label><abbr title=${PARAM_TOOLTIPS.exploration_rate}>Exploration Rate (0.0–1.0)</abbr>
        <input type="number" min="0" max="1" step="0.05" .value=${rate} placeholder="0.1" @change=${(e: Event) => onFloatInput("lkgp_exploration_rate", e)} @input=${(e: Event) => onFloatInput("lkgp_exploration_rate", e)} class="cw-input"></label></div></details>`;
  } else if (pm === "least_used" || pm === "p2c") {
    const win = combo.selection_window_secs != null ? String(combo.selection_window_secs) : "";
    params = html`<details class="combo-mode-params" open><summary>Parameters</summary><div class="combo-mode-params-body">
      <label><abbr title=${PARAM_TOOLTIPS.window_secs}>Window (s)</abbr>
        <input type="number" min="1" .value=${win} placeholder="3600" @change=${(e: Event) => onNumInput("selection_window_secs", e)} @input=${(e: Event) => onNumInput("selection_window_secs", e)} class="cw-input"></label></div></details>`;
  } else if (pm === "weighted") {
    params = html`<details class="combo-mode-params" open><summary>Parameters</summary><div class="combo-mode-params-body"><span class="muted">Set per-target weights in the targets table below.</span></div></details>`;
  }
  return html`<div class="combo-settings-bar"><label><abbr title=${PRIORITY_MODE_TOOLTIPS[pm]}>Priority mode</abbr><select @change=${onUpdatePriorityMode}>${priorityModeOptions(pm)}</select></label>${params}</div>`;
}

function renderCooldownBar(combo: Combo): TemplateResult {
  const cm = cooldownModeOf(combo);
  const base = combo.cooldown_base_secs != null ? String(combo.cooldown_base_secs) : "";
  const factor = combo.cooldown_factor != null ? String(combo.cooldown_factor) : "";
  const max = combo.cooldown_max_secs != null ? String(combo.cooldown_max_secs) : "";
  const params = cm === "exponential" ? html`<details class="combo-mode-params" open><summary>Parameters</summary><div class="combo-mode-params-body">
    <label><abbr title=${PARAM_TOOLTIPS.base_secs}>Base (s)</abbr><input type="number" min="1" .value=${base} placeholder="60" @change=${(e: Event) => onNumInput("cooldown_base_secs", e)} @input=${(e: Event) => onNumInput("cooldown_base_secs", e)} class="cw-input"></label>
    <label><abbr title=${PARAM_TOOLTIPS.factor}>Factor</abbr><input type="number" min="2" .value=${factor} placeholder="2" @change=${(e: Event) => onNumInput("cooldown_factor", e)} @input=${(e: Event) => onNumInput("cooldown_factor", e)} class="cw-input"></label>
    <label><abbr title=${PARAM_TOOLTIPS.max_secs}>Max (s)</abbr><input type="number" min="1" .value=${max} placeholder="3600" @change=${(e: Event) => onNumInput("cooldown_max_secs", e)} @input=${(e: Event) => onNumInput("cooldown_max_secs", e)} class="cw-input"></label>
  </div></details>` : html``;
  return html`<div class="combo-settings-bar"><label><abbr title=${COOLDOWN_MODE_TOOLTIPS[cm]}>Cooldown mode</abbr><select @change=${onUpdateCooldownMode}>${cooldownModeOptions(cm)}</select></label>${params}</div>`;
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
    showToast("Reorder failed: could not fetch current targets", "error");
    return;
  }
  const ordered = [...currentTargets].sort((a, b) => a.priority_order - b.priority_order);
  const fromIdx = ordered.findIndex((x) => x.id === draggedId);
  const toIdx = ordered.findIndex((x) => x.id === dropTargetId);
  if (fromIdx < 0 || toIdx < 0) {
    showToast("Reorder failed: target not found in current list", "error");
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
    showToast("Target priority updated", "success");
  } catch (err: unknown) {
    showToast("Reorder failed: " + (err instanceof Error ? err.message : String(err)), "error");
  }
}

function renderTargetRow(t: ComboTargetWithModel, showWeight: boolean): TemplateResult {
  const isSub = t.sub_combo_id != null;
  const cdBadge = t.in_cooldown ? html` <span class="badge badge-cooldown" title="Cooldown — ${t.cooldown_reason ?? ""} until ${t.cooldown_until ?? ""}">⏸</span>` : html``;
  const inactiveBadge = (t.provider_active === false)
    ? html` <span class="badge badge-inactive" title="Provider is inactive — this target is not used for routing. Reactivate the provider in the Providers page to enable it.">⚠ inactive</span>`
    : html``;
  const targetInactiveBadge = (t.active === false)
    ? html` <span class="badge badge-inactive" title="Target is inactive — not used for routing.">⏸ inactive</span>`
    : html``;
  const modelCell = isSub ? html`<span class="chip combo-chip">→ combo: ${t.sub_combo_name ?? "#" + t.sub_combo_id}</span>` : html`${t.model_display_name || t.model_id || "row #" + t.model_row_id}${cdBadge}${inactiveBadge}${targetInactiveBadge}`;
  const providerCell = isSub ? html`<span class="virtual-provider">${t.provider_id}</span>` : html`<a href="#/providers/${encodeURIComponent(t.provider_id)}">${t.provider_id}</a>`;
  const accountCell = isSub ? html`<em>n/a</em>` : (t.account_id ? html`#${t.account_id}` : html`<em>rotate</em>`);
  const contextCell = isSub ? html`<em>sub-combo</em>` : (t.context_length != null ? html`<span title=${String(t.context_length)}>${formatTokens(t.context_length)}</span>` : html`—`);
  const weightCell = showWeight ? (isSub ? html`<td class="col-target-weight" data-label="Weight"><em>n/a</em></td>` : html`<td class="col-target-weight" data-label="Weight"><input type="number" min="1" .value=${String(t.weight ?? 1)} @change=${(e: Event) => onUpdateTargetWeight(t.id, e)} @input=${(e: Event) => onUpdateTargetWeight(t.id, e)} class="cw-input weight-input" title=${PARAM_TOOLTIPS.weight}></td>`) : html``;
  const cooldownCell = isSub ? html`<td class="col-target-cooldown" data-label="Cooldown"><em>sub-combo</em></td>` : html`<td class="col-target-cooldown" data-label="Cooldown">
    <div style="display:flex;align-items:center;gap:4px">
      <select class="cw-input" style="font-size:0.75rem;padding:2px 4px;max-width:95px" @change=${(e: Event) => onUpdateTargetCooldownMode(t.id, e)}>
        <option value="" ?selected=${!t.cooldown_mode && t.cooldown_base_secs == null}>Inherit</option>
        <option value="none" ?selected=${t.cooldown_mode === "none" || t.cooldown_base_secs === 0}>Disabled</option>
        <option value="flat" ?selected=${t.cooldown_mode === "flat" && t.cooldown_base_secs !== 0}>Flat</option>
        <option value="exponential" ?selected=${t.cooldown_mode === "exponential" && t.cooldown_base_secs !== 0}>Exponential</option>
      </select>
      ${(t.cooldown_mode === "flat" || t.cooldown_mode === "exponential") ? html`<input type="number" min="0" placeholder="sec" style="width:48px;font-size:0.75rem;padding:2px 4px" .value=${t.cooldown_base_secs != null ? String(t.cooldown_base_secs) : ""} @change=${(e: Event) => onUpdateTargetCooldownBase(t.id, e)} class="cw-input" title="Base seconds (0 = disabled)">` : html``}
    </div>
  </td>`;
  const testResults: ComboTestResult[] | undefined = detailComboId != null
    ? state.comboTestResults[detailComboId]
    : undefined;
  const tr = testResults?.find((r) => r.target_id === t.id);
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
  return html`<tr draggable="true" data-drag-id=${String(t.id)} class="combo-target-card-row"
    @dragstart=${(e: DragEvent) => { e.dataTransfer?.setData("text/plain", String(t.id)); (e.target as HTMLElement).classList.add("dragging"); }}
    @dragend=${(e: DragEvent) => { (e.target as HTMLElement).classList.remove("dragging"); }}
    @dragover=${(e: DragEvent) => { e.preventDefault(); const row = (e.currentTarget as HTMLElement); row.classList.add("drag-over"); }}
    @dragleave=${(e: DragEvent) => { (e.currentTarget as HTMLElement).classList.remove("drag-over"); }}
    @drop=${async (e: DragEvent) => {
      e.preventDefault();
      (e.currentTarget as HTMLElement).classList.remove("drag-over");
      const draggedId = parseInt(e.dataTransfer?.getData("text/plain") || "0", 10);
      if (!draggedId || draggedId === t.id || !detailComboId) return;
      await executeTargetReorder(draggedId, t.id);
    }}
  >
    <td class="drag-handle col-target-drag"
        title="Drag to reorder (touch & hold or mouse drag)"
        @touchstart=${(e: TouchEvent) => onTouchStartHandle(t.id, e)}
        @touchmove=${(e: TouchEvent) => onTouchMoveHandle(e)}
        @touchend=${() => void onTouchEndHandle()}
        @touchcancel=${() => void onTouchEndHandle()}>⠿</td>
    <td class="col-target-order" data-label="#">${t.priority_order}</td>
    <td class="col-target-provider" data-label="Provider">${providerCell}</td>
    <td class="col-target-account" data-label="Account">${accountCell}</td>
    <td class="col-target-model" data-label="Model"><div class="target-model-title">${modelCell}</div></td>
    <td class="col-target-context" data-label="Context">${contextCell}</td>
    ${weightCell}
    ${cooldownCell}
    <td class="last-test-cell col-target-test-status" data-label="Last test">${lastTestCell}</td>
    <td class="col-target-actions" data-label="Actions">
      <div class="target-actions-wrap">
        ${!isSub ? html`<button class="small primary" title="Test this model" @click=${(e: Event) => onTestTarget(t.id, t.model_row_id, e)}>🧪 Test</button>` : html``}
        <button class="small" title=${t.active !== false ? "Deactivate target" : "Activate target"} @click=${() => onToggleTargetActive(t.id, t.active !== false)}>${t.active !== false ? "⏸ Pause" : "▶ Resume"}</button>
        <button class="small reorder-btn" title="Move Up" @click=${() => onChangePriority(t.id, -1)}>▲</button>
        <button class="small reorder-btn" title="Move Down" @click=${() => onChangePriority(t.id, 1)}>▼</button>
        ${t.in_cooldown && !isSub ? html`<button class="small" title="Clear cooldown" @click=${() => onResetCooldown(t.id)}>🔄 Reset CD</button>` : html``}
        <button class="small danger" title="Remove target" @click=${() => onDeleteTarget(t.id)}>×</button>
      </div>
    </td>
  </tr>`;
}

function renderComboDetail(): TemplateResult {
  if (!detailCombo) return html`<div class="loading">Loading...</div>`;
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
  const weightTh = showWeight ? html`<th><abbr title=${PARAM_TOOLTIPS.weight}>Weight</abbr></th>` : html``;
  return html`
    <div class="page-header"><a href="#/combos" class="back-link">← All combos</a><h2>${combo.name}</h2>
      <div class="actions">
        <label style="display:inline-flex;align-items:center;gap:5px;font-size:0.85rem;">
          Strategy:
          <select style="padding:3px 6px;font-size:0.8rem;border-radius:var(--radius-sm);" .value=${combo.strategy} @change=${onUpdateStrategy}>
            <option value="priority" ?selected=${combo.strategy === "priority"}>priority</option>
            <option value="round_robin" ?selected=${combo.strategy === "round_robin"}>round_robin</option>
            <option value="shuffle" ?selected=${combo.strategy === "shuffle"}>shuffle</option>
          </select>
        </label>
        <span class="chip">${PRIORITY_MODE_LABELS[pm]}</span>
        <label style="display:inline-flex;align-items:center;gap:5px;cursor:pointer;font-size:0.85rem;" title="Rate Limit Preventivo: Predice 429s por target y salta proactivamente al siguiente target del combo sin consumir intentos.">
          <input type="checkbox" ?checked=${combo.preventive_rate_limit ?? false} @change=${onTogglePreventiveRateLimit}>
          ⚡ Predictive RL
        </label>
        <label>Race size: <input type="number" min="1" max="8" .value=${String(combo.race_size)} @change=${onUpdateRaceSize} @input=${onUpdateRaceSize} class="race-input"></label>
        <button class="danger" @click=${onDeleteCombo}>Delete</button></div></div>
    <div class="combo-context-window-bar"><label>Context window:
      <input type="number" min="1" placeholder="auto (${autoCwLabel})" .value=${overrideCw != null ? String(overrideCw) : ""} @change=${onUpdateContextWindow} @input=${onUpdateContextWindow} class="cw-input" title="Override context window (tokens). Empty = auto-compute."></label>
      <span class="cw-hint">Auto: <strong>${autoCwLabel}</strong> · Effective: <strong>${effectiveCwLabel}</strong></span></div>
    ${renderPriorityModeBar(combo)}${renderCooldownBar(combo)}
    ${cds.length > 0 ? html`<div class="cooldown-banner">⏸ ${cds.length} of ${targets.length} target(s) in cooldown — engine will skip them.</div>` : html``}
    <section class="detail-section"><div class="section-header"><h3>Targets (${targets.length})</h3>
      <div class="actions"><button @click=${onTestAllTargets}>🧪 Test all</button><button class="primary" @click=${() => showAddTarget(combo.id)}>+ Add target</button></div></div>
      ${targets.length === 0 ? html`<p class="empty">No targets. Add a target to start routing.</p>` : html`<div class="table-wrap"><table class="combo-targets-table responsive-card-table">
        <thead><tr><th></th><th>#</th><th>Provider</th><th>Account</th><th>Model</th><th>Context</th>${weightTh}<th>Cooldown</th><th>Last test</th><th>Actions</th></tr></thead>
        <tbody>${targets.map((t) => renderTargetRow(t, showWeight))}</tbody></table></div>`}
    </section>`;
}

function renderComboGrid(): TemplateResult {
  const list = state.combos || [];
  return html`<div class="page-header"><h2>Combos</h2><div class="actions"><button class="primary" @click=${() => showCreateCombo()}>+ Create combo</button></div></div>
    ${list.length === 0 ? html`<p class="empty">No combos yet. Create one to start routing.</p>` : html`<div class="combo-grid">${list.map((c) => {
      const pm = priorityModeOf(c);
      const pmChip = pm === "strict" ? html`` : html` · <span class="chip">${PRIORITY_MODE_LABELS[pm]}</span>`;
      return html`<a class="combo-card" href="#/combos/${c.id}"><h3>${c.name}</h3><div class="provider-meta"><span class="chip">${c.strategy}</span>${pmChip} · race ${c.race_size}</div></a>`;
    })}</div>`}`;
}

// ---- Mount ----

export async function mountCombos(opts: { detailId?: number } = {}): Promise<(() => void) | void> {
  const el = document.getElementById("main");
  if (!el) return;

  if (opts.detailId) {
    detailComboId = opts.detailId;
    detailCombo = null;
    detailTargets = [];
    const cleanup = mountView(el, renderComboDetail);
    try {
      const [combo, targets] = await Promise.all([
        api("/combos/" + opts.detailId).catch(() => null) as Promise<Combo | null>,
        api("/combos/" + opts.detailId + "/targets") as Promise<ComboTargetWithModel[]>,
      ]);
      detailCombo = combo;
      detailTargets = targets || [];
      requestUpdate();
    } catch { detailCombo = null; requestUpdate(); }
    return cleanup;
  }

  state.combos = await api("/combos") as Combo[];
  const cleanup = mountView(el, renderComboGrid);
  return cleanup;
}
