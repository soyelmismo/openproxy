// handlers/combo-handlers.ts — combo CRUD: create, delete, race
// size, test-all, priority/cooldown settings. Target-specific
// actions live in combo-target-handlers.ts.
//
// Per spec §3 + §13.8 we do not attach to `window.*`.

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { appendModal } from "../lib/dom.js";
import { rerenderCurrentView } from "../state/router.js";
import type { Combo, CreateComboInput, PriorityMode, CooldownMode } from "../lib/types/api.js";

// ---- Tooltips (English explanations, kept in one place so they
// can be reused between the create form and the detail view). ----

export const PRIORITY_MODE_TOOLTIPS: Record<PriorityMode, string> = {
  strict: "Walk targets in manual priority order. The first healthy target is always tried first.",
  lkgp: "Least Known Good Provider — prefer the target with the most recent successful request. Falls back to priority order for never-tried targets. An exploration rate adds priority-weighted randomness: earlier targets (which the operator positioned first for speed/intelligence) are more likely to be explored than later fallback targets.",
  weighted: "Weighted random selection — each target's probability is proportional to its weight. Set weights in the targets table below.",
  least_used: "Prefer the target with the fewest total requests in the selection window. Useful for distributing load evenly.",
  p2c: "Power of Two Choices — pick two random targets, choose the one with fewer recent failures. Good balance of simplicity and load distribution.",
};

export const COOLDOWN_MODE_TOOLTIPS: Record<CooldownMode, string> = {
  flat: "Fixed cooldown duration after each failure. The target is parked for the same amount of time regardless of how many times it has failed.",
  exponential: "Cooldown grows with each failure: base × factor^(failures-1), capped at max. A flapping target gets progressively longer cooldowns, giving it time to recover.",
};

export const PARAM_TOOLTIPS = {
  exploration_rate: "Probability (0.0–1.0) of trying a different target instead of the best-known one. 0.1 = 10% exploration. The exploration is priority-weighted: targets positioned first in the combo (lower priority order) are more likely to be explored, respecting the operator's intent that earlier = preferred for speed/intelligence, later = fallback with less desired concurrency. Higher exploration rates discover alternatives faster but may pick suboptimal targets.",
  base_secs: "Initial cooldown duration in seconds. For exponential mode, this is multiplied by factor^(failures-1).",
  factor: "Multiplier applied to the cooldown after each failure. 2 = doubling. The cooldown grows as base × factor^(failures-1).",
  max_secs: "Maximum cooldown duration in seconds. The exponential growth is capped at this value to prevent permanently parking a target.",
  window_secs: "How far back to look at usage data for the selection algorithm. 3600 = 1 hour.",
  weight: "Relative weight for weighted random selection. Higher = more likely to be selected. Default 1.",
} as const;

const PRIORITY_MODE_LABELS: Record<PriorityMode, string> = {
  strict: "Strict",
  lkgp: "LKGP",
  weighted: "Weighted",
  least_used: "Least Used",
  p2c: "P2C",
};

const COOLDOWN_MODE_LABELS: Record<CooldownMode, string> = {
  flat: "Flat",
  exponential: "Exponential",
};

/** Render the priority-mode `<option>`s with the given value preselected. */
export function priorityModeOptions(selected: PriorityMode): string {
  const modes: PriorityMode[] = ["strict", "lkgp", "weighted", "least_used", "p2c"];
  return modes.map((m) =>
    `<option value="${m}"${selected === m ? " selected" : ""}>${PRIORITY_MODE_LABELS[m]}</option>`
  ).join("");
}

/** Render the cooldown-mode `<option>`s with the given value preselected. */
export function cooldownModeOptions(selected: CooldownMode): string {
  const modes: CooldownMode[] = ["flat", "exponential"];
  return modes.map((m) =>
    `<option value="${m}"${selected === m ? " selected" : ""}>${COOLDOWN_MODE_LABELS[m]}</option>`
  ).join("");
}

export function showCreateCombo(): void {
  const html = `
    <div class="modal-bg" id="create-combo-modal" data-action="closeCreateCombo" data-arg1="self">
      <div class="modal">
        <div class="modal-header">
          <h2>New combo</h2>
          <button type="button" class="close-btn" data-action="closeCreateCombo" aria-label="Close">&times;</button>
        </div>
        <form data-action="createCombo">
          <div class="modal-body">
            <div class="field"><label for="combo-name">Name</label><input id="combo-name" name="name" type="text" required></div>
            <div class="field">
              <label for="combo-strategy">Strategy</label>
              <select id="combo-strategy" name="strategy">
                <option value="priority">priority</option>
                <option value="round_robin">round_robin</option>
                <option value="shuffle">shuffle</option>
              </select>
            </div>
            <div class="field"><label for="combo-race-size">Race size</label><input id="combo-race-size" name="race_size" type="number" min="1" max="8" value="1"></div>
            <div class="field">
              <label for="combo-priority-mode"><abbr title="${PRIORITY_MODE_TOOLTIPS.strict}">Priority mode</abbr></label>
              <select id="combo-priority-mode" name="priority_mode" data-action="onCreatePriorityModeChange">
                ${priorityModeOptions("strict")}
              </select>
            </div>
            <div class="field" id="create-lkgp-fields" style="display: none;">
              <label for="combo-lkgp-rate"><abbr title="${PARAM_TOOLTIPS.exploration_rate}">Exploration Rate (0.0–1.0)</abbr></label>
              <input id="combo-lkgp-rate" name="lkgp_exploration_rate" type="number" min="0" max="1" step="0.05" value="0.1">
            </div>
            <div class="field" id="create-window-fields" style="display: none;">
              <label for="combo-window"><abbr title="${PARAM_TOOLTIPS.window_secs}">Window (s)</abbr></label>
              <input id="combo-window" name="selection_window_secs" type="number" min="1" value="3600">
            </div>
            <div class="field">
              <label for="combo-cooldown-mode"><abbr title="${COOLDOWN_MODE_TOOLTIPS.flat}">Cooldown mode</abbr></label>
              <select id="combo-cooldown-mode" name="cooldown_mode" data-action="onCreateCooldownModeChange">
                ${cooldownModeOptions("flat")}
              </select>
            </div>
            <div id="create-cooldown-fields" style="display: none;">
              <div class="field">
                <label for="combo-cd-base"><abbr title="${PARAM_TOOLTIPS.base_secs}">Base (s)</abbr></label>
                <input id="combo-cd-base" name="cooldown_base_secs" type="number" min="1" value="60">
              </div>
              <div class="field">
                <label for="combo-cd-factor"><abbr title="${PARAM_TOOLTIPS.factor}">Factor</abbr></label>
                <input id="combo-cd-factor" name="cooldown_factor" type="number" min="2" value="2">
              </div>
              <div class="field">
                <label for="combo-cd-max"><abbr title="${PARAM_TOOLTIPS.max_secs}">Max (s)</abbr></label>
                <input id="combo-cd-max" name="cooldown_max_secs" type="number" min="1" value="3600">
              </div>
            </div>
          </div>
          <div class="modal-footer">
            <button type="button" data-action="closeCreateCombo">Cancel</button>
            <button type="submit" class="primary">Create</button>
          </div>
        </form>
      </div>
    </div>
  `;
  appendModal(html);
}

export function closeCreateCombo(): void {
  const m = document.getElementById("create-combo-modal");
  if (m) m.remove();
}

/** Show/hide the conditional priority-mode parameter fields in the
 *  create-combo modal based on the currently selected option. Mirrors
 *  the existing `onTargetKindChange` pattern in combo-target-handlers. */
export function onCreatePriorityModeChange(): void {
  const sel = document.getElementById("combo-priority-mode") as HTMLSelectElement | null;
  if (!sel) return;
  const mode = sel.value as PriorityMode;
  const lkgpFields = document.getElementById("create-lkgp-fields");
  const windowFields = document.getElementById("create-window-fields");
  const label = document.querySelector('label[for="combo-priority-mode"] abbr');
  if (lkgpFields) lkgpFields.style.display = mode === "lkgp" ? "" : "none";
  if (windowFields) windowFields.style.display = (mode === "least_used" || mode === "p2c") ? "" : "none";
  if (label) label.setAttribute("title", PRIORITY_MODE_TOOLTIPS[mode]);
}

/** Show/hide the conditional cooldown parameter fields in the
 *  create-combo modal based on the currently selected option. */
export function onCreateCooldownModeChange(): void {
  const sel = document.getElementById("combo-cooldown-mode") as HTMLSelectElement | null;
  if (!sel) return;
  const mode = sel.value as CooldownMode;
  const cdFields = document.getElementById("create-cooldown-fields");
  const label = document.querySelector('label[for="combo-cooldown-mode"] abbr');
  if (cdFields) cdFields.style.display = mode === "exponential" ? "" : "none";
  if (label) label.setAttribute("title", COOLDOWN_MODE_TOOLTIPS[mode]);
}

export async function createCombo(e: Event): Promise<void> {
  const target = e.target;
  if (!(target instanceof HTMLFormElement)) return;
  const f = new FormData(target);
  const priorityMode = String(f.get("priority_mode") || "strict");
  const cooldownMode = String(f.get("cooldown_mode") || "flat");
  const body: CreateComboInput = {
    name: String(f.get("name") || ""),
    strategy: String(f.get("strategy") || "priority"),
    race_size: parseInt(String(f.get("race_size") || "1"), 10),
    priority_mode: priorityMode,
    cooldown_mode: cooldownMode,
  };
  // Parameter fields are only sent when their mode is selected AND
  // the user entered a value. Empty strings would fail u64 / f64
  // parsing on the backend, so we skip them.
  if (priorityMode === "lkgp") {
    const rateRaw = String(f.get("lkgp_exploration_rate") || "").trim();
    if (rateRaw !== "") {
      const rate = parseFloat(rateRaw);
      if (!Number.isNaN(rate)) body.lkgp_exploration_rate = rate;
    }
  }
  if (priorityMode === "least_used" || priorityMode === "p2c") {
    const winRaw = String(f.get("selection_window_secs") || "").trim();
    if (winRaw !== "") {
      const win = parseInt(winRaw, 10);
      if (!Number.isNaN(win)) body.selection_window_secs = win;
    }
  }
  if (cooldownMode === "exponential") {
    const baseRaw = String(f.get("cooldown_base_secs") || "").trim();
    if (baseRaw !== "") {
      const base = parseInt(baseRaw, 10);
      if (!Number.isNaN(base)) body.cooldown_base_secs = base;
    }
    const maxRaw = String(f.get("cooldown_max_secs") || "").trim();
    if (maxRaw !== "") {
      const max = parseInt(maxRaw, 10);
      if (!Number.isNaN(max)) body.cooldown_max_secs = max;
    }
    const factorRaw = String(f.get("cooldown_factor") || "").trim();
    if (factorRaw !== "") {
      const factor = parseInt(factorRaw, 10);
      if (!Number.isNaN(factor)) body.cooldown_factor = factor;
    }
  }
  try {
    await api("/combos", { method: "POST", body: JSON.stringify(body) });
    closeCreateCombo();
    rerenderCurrentView();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    alert("Error: " + msg);
  }
}

export async function deleteCombo(id: number): Promise<void> {
  if (!confirm("Delete combo " + id + "?")) return;
  try {
    await api("/combos/" + id, { method: "DELETE" });
    rerenderCurrentView();
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    alert("Error: " + msg);
  }
}

export async function updateRaceSize(id: number, e: Event | null): Promise<void> {
  const val = e && e.target ? parseInt((e.target as HTMLInputElement).value, 10) : NaN;
  if (!Number.isFinite(val) || val < 1 || val > 8) { alert("Must be 1-8"); rerenderCurrentView(); return; }
  try {
    await api("/combos/" + id, { method: "PATCH", body: JSON.stringify({ race_size: val }) });
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    alert("Error: " + msg);
    rerenderCurrentView();
  }
}

export async function updateContextWindow(id: number, e: Event | null): Promise<void> {
  // Only fire on "change" (blur/enter), not on every "input" keystroke.
  if (e && e.type === "input") return;
  const raw = e && e.target ? (e.target as HTMLInputElement).value.trim() : "";
  const body = raw === "" ? { context_window: null } : { context_window: parseInt(raw, 10) };
  if (raw !== "" && !Number.isFinite(body.context_window)) {
    console.error("[openproxy] context_window must be a number or empty");
    return;
  }
  try {
    await api("/combos/" + id, { method: "PATCH", body: JSON.stringify(body) });
    // Update state WITHOUT re-rendering — see patchComboField for
    // the rationale (avoid closing dropdowns / stealing focus).
    const combo = (state.combos || []).find((c) => c.id === id);
    if (combo) combo.context_window = body.context_window;
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    console.error("[openproxy] context_window PATCH failed:", msg);
  }
}

// ---- Priority mode + cooldown PATCH handlers ----
//
// Each handler is a thin wrapper around `patchComboField` so the
// boilerplate (state update, error toast, re-render) lives in one
// place. Selects fire only on "change"; number inputs fire on both
// "input" and "change", so the wrappers for the latter pass
// `onlyOnChange: true` to filter out per-keystroke PATCHes.

async function patchComboField(
  id: number,
  field: string,
  value: unknown,
): Promise<void> {
  try {
    await api("/combos/" + id, {
      method: "PATCH",
      body: JSON.stringify({ [field]: value }),
    });
    // Optimistically update the in-memory combo so the value
    // persists across any future re-render. We do NOT call
    // rerenderCurrentView() here — a full DOM rebuild would
    // close any open dropdowns and steal focus from inputs,
    // making the UI feel broken (the exact "me cierra el
    // dropdown" bug the user reported). The state update is
    // enough; the next natural re-render (page nav, bg-poll)
    // will pick up the new value.
    const combo = (state.combos || []).find((c) => c.id === id);
    if (combo) {
      (combo as unknown as Record<string, unknown>)[field] = value;
    }
    // For select elements, the DOM already reflects the user's
    // choice. For number inputs, the value is already in the
    // input. No re-render needed.
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    // Show error but DON'T re-render — the user might be in the
    // middle of editing another field. A re-render would lose
    // their focus and unsaved changes.
    console.error("[openproxy] combo PATCH failed:", msg);
    // Use a toast instead of alert() so the user can dismiss it
    // without losing focus.
    const toast = document.createElement("div");
    toast.className = "toast toast-error";
    toast.textContent = "Error: " + msg;
    document.body.appendChild(toast);
    setTimeout(() => toast.classList.add("show"), 10);
    setTimeout(() => { toast.classList.remove("show"); setTimeout(() => toast.remove(), 300); }, 4000);
  }
}

export async function updatePriorityMode(id: number, e: Event | null): Promise<void> {
  const val = e && e.target ? (e.target as HTMLSelectElement).value : "strict";
  await patchComboField(id, "priority_mode", val);
}

export async function updateCooldownMode(id: number, e: Event | null): Promise<void> {
  const val = e && e.target ? (e.target as HTMLSelectElement).value : "flat";
  await patchComboField(id, "cooldown_mode", val);
}

export async function updateCooldownBase(id: number, e: Event | null): Promise<void> {
  if (e && e.type === "input") return;
  const raw = e && e.target ? (e.target as HTMLInputElement).value.trim() : "";
  const val: number | null = raw === "" ? null : parseInt(raw, 10);
  if (raw !== "" && !Number.isFinite(val)) {
    alert("Base must be a number or empty");
    rerenderCurrentView();
    return;
  }
  await patchComboField(id, "cooldown_base_secs", val);
}

export async function updateCooldownFactor(id: number, e: Event | null): Promise<void> {
  if (e && e.type === "input") return;
  const raw = e && e.target ? (e.target as HTMLInputElement).value.trim() : "";
  const val: number | null = raw === "" ? null : parseInt(raw, 10);
  if (raw !== "" && !Number.isFinite(val)) {
    alert("Factor must be a number or empty");
    rerenderCurrentView();
    return;
  }
  await patchComboField(id, "cooldown_factor", val);
}

export async function updateCooldownMax(id: number, e: Event | null): Promise<void> {
  if (e && e.type === "input") return;
  const raw = e && e.target ? (e.target as HTMLInputElement).value.trim() : "";
  const val: number | null = raw === "" ? null : parseInt(raw, 10);
  if (raw !== "" && !Number.isFinite(val)) {
    alert("Max must be a number or empty");
    rerenderCurrentView();
    return;
  }
  await patchComboField(id, "cooldown_max_secs", val);
}

export async function updateLkgpExplorationRate(id: number, e: Event | null): Promise<void> {
  if (e && e.type === "input") return;
  const raw = e && e.target ? (e.target as HTMLInputElement).value.trim() : "";
  const val: number | null = raw === "" ? null : parseFloat(raw);
  if (raw !== "" && !Number.isFinite(val)) {
    alert("Exploration rate must be a number 0.0–1.0 or empty");
    rerenderCurrentView();
    return;
  }
  if (val != null && (val < 0 || val > 1)) {
    alert("Exploration rate must be between 0.0 and 1.0");
    rerenderCurrentView();
    return;
  }
  await patchComboField(id, "lkgp_exploration_rate", val);
}

export async function updateSelectionWindow(id: number, e: Event | null): Promise<void> {
  if (e && e.type === "input") return;
  const raw = e && e.target ? (e.target as HTMLInputElement).value.trim() : "";
  const val: number | null = raw === "" ? null : parseInt(raw, 10);
  if (raw !== "" && !Number.isFinite(val)) {
    alert("Window must be a number or empty");
    rerenderCurrentView();
    return;
  }
  await patchComboField(id, "selection_window_secs", val);
}

// testAllTargets: receives (comboId, e). The e.target is the button
// that was clicked; we toggle a "Testing…" state on it. The
// original code used `window.event` to find the button, which is
// non-standard; the e.target path is more reliable.
export async function testAllTargets(comboId: number, e: Event | null): Promise<void> {
  const btn = e && e.target ? (e.target as HTMLElement).closest("button") : null;
  const oldText = btn ? btn.textContent : null;
  if (btn) { btn.disabled = true; btn.textContent = "🧪 Testing..."; }
  try {
    const results = await api(`/combos/${comboId}/test-all`, { method: "POST" });
    state.comboTestResults[comboId] = Array.isArray(results) ? results : [];
    rerenderCurrentView();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    alert("Test all failed: " + msg);
  } finally {
    if (btn) { btn.disabled = false; btn.textContent = oldText || "🧪 Test all"; }
  }
}

// Re-exported for the detail view; it reads `Combo.priority_mode` /
// `cooldown_mode` as `PriorityMode | null` and needs a typed default.
export function priorityModeOf(c: Combo): PriorityMode {
  return (c.priority_mode ?? "strict") as PriorityMode;
}

export function cooldownModeOf(c: Combo): CooldownMode {
  return (c.cooldown_mode ?? "flat") as CooldownMode;
}
