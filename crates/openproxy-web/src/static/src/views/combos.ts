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
import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { mountView, requestUpdate } from "../state/reactive.js";
import { showToast } from "../components/toast.js";
import type {
  Combo,
  ComboTargetWithModel,
  PriorityMode,
  CooldownMode,
} from "../lib/types/api.js";

// ---- Constants ----

const PRIORITY_MODE_LABELS: Record<PriorityMode, string> = {
  strict: "Strict", lkgp: "LKGP", weighted: "Weighted",
  least_used: "Least Used", p2c: "P2C",
};

const PRIORITY_MODE_TOOLTIPS: Record<PriorityMode, string> = {
  strict: "Walk targets in manual priority order. The first healthy target is always tried first.",
  lkgp: "Least Known Good Provider — prefer the target with the most recent successful request. Falls back to priority order for never-tried targets. An exploration rate adds priority-weighted randomness: earlier targets (which the operator positioned first for speed/intelligence) are more likely to be explored than later fallback targets.",
  weighted: "Weighted random selection — each target's probability is proportional to its weight. Set weights in the targets table below.",
  least_used: "Prefer the target with the fewest total requests in the selection window. Useful for distributing load evenly.",
  p2c: "Power of Two Choices — pick two random targets, choose the one with fewer recent failures. Good balance of simplicity and load distribution.",
};

const COOLDOWN_MODE_TOOLTIPS: Record<CooldownMode, string> = {
  flat: "Fixed cooldown duration after each failure. The target is parked for the same amount of time regardless of how many times it has failed.",
  exponential: "Cooldown grows with each failure: base × factor^(failures-1), capped at max. A flapping target gets progressively longer cooldowns, giving it time to recover.",
};

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
async function onDeleteTarget(targetId: number): Promise<void> {
  try {
    await api(`/combos/${detailComboId}/targets/${targetId}`, { method: "DELETE" });
    detailTargets = detailTargets.filter((t) => t.id !== targetId);
    requestUpdate();
  } catch (err: unknown) { showToast("Error: " + (err instanceof Error ? err.message : String(err)), "error"); }
}
async function onChangePriority(targetId: number, delta: number): Promise<void> {
  try {
    const ordered = [...detailTargets].sort((a, b) => a.priority_order - b.priority_order);
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
    await api(`/combos/${detailComboId}/targets/${targetId}/cooldown`, { method: "DELETE" });
    const t = detailTargets.find((t) => t.id === targetId);
    if (t) { t.in_cooldown = false; t.cooldown_until = null; t.cooldown_reason = null; }
    requestUpdate();
  } catch (err: unknown) { showToast("Error: " + (err instanceof Error ? err.message : String(err)), "error"); }
}

// ---- Templates ----

function priorityModeOptions(selected: PriorityMode): TemplateResult {
  const modes: PriorityMode[] = ["strict", "lkgp", "weighted", "least_used", "p2c"];
  return html`${modes.map((m) => html`<option value=${m} ?selected=${m === selected}>${PRIORITY_MODE_LABELS[m]}</option>`)}`;
}
function cooldownModeOptions(selected: CooldownMode): TemplateResult {
  const modes: CooldownMode[] = ["flat", "exponential"];
  return html`${modes.map((m) => html`<option value=${m} ?selected=${m === selected}>${m === "flat" ? "Flat" : "Exponential"}</option>`)}`;
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

function renderTargetRow(t: ComboTargetWithModel, showWeight: boolean): TemplateResult {
  const isSub = t.sub_combo_id != null;
  const cdBadge = t.in_cooldown ? html` <span class="badge badge-cooldown" title="Cooldown — ${t.cooldown_reason ?? ""} until ${t.cooldown_until ?? ""}">⏸</span>` : html``;
  const modelCell = isSub ? html`<span class="chip combo-chip">→ combo: ${t.sub_combo_name ?? "#" + t.sub_combo_id}</span>` : html`${t.model_display_name || t.model_id || "row #" + t.model_row_id}${cdBadge}`;
  const providerCell = isSub ? html`<span class="virtual-provider">${t.provider_id}</span>` : html`<a href="#/providers/${encodeURIComponent(t.provider_id)}">${t.provider_id}</a>`;
  const accountCell = isSub ? html`<em>n/a</em>` : (t.account_id ? html`#${t.account_id}` : html`<em>rotate</em>`);
  const contextCell = isSub ? html`<em>sub-combo</em>` : (t.context_length != null ? html`<span title=${String(t.context_length)}>${formatTokens(t.context_length)}</span>` : html`—`);
  const weightCell = showWeight ? (isSub ? html`<td><em>n/a</em></td>` : html`<td><input type="number" min="1" .value=${String(t.weight ?? 1)} @change=${(e: Event) => onUpdateTargetWeight(t.id, e)} @input=${(e: Event) => onUpdateTargetWeight(t.id, e)} class="cw-input weight-input" title=${PARAM_TOOLTIPS.weight}></td>`) : html``;
  return html`<tr draggable="true" data-drag-id=${String(t.id)}>
    <td class="drag-handle" title="Drag to reorder">⠿</td><td>${t.priority_order}</td><td>${providerCell}</td><td>${accountCell}</td><td>${modelCell}</td><td>${contextCell}</td>${weightCell}
    <td><button class="small" @click=${() => onChangePriority(t.id, -1)}>↑</button><button class="small" @click=${() => onChangePriority(t.id, 1)}>↓</button>${t.in_cooldown && !isSub ? html`<button class="small" title="Clear cooldown" @click=${() => onResetCooldown(t.id)}>🔄</button>` : html``}<button class="small danger" @click=${() => onDeleteTarget(t.id)}>×</button></td>
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
      <div class="actions"><span class="chip">${combo.strategy}</span><span class="chip">${PRIORITY_MODE_LABELS[pm]}</span>
        <label>Race size: <input type="number" min="1" max="8" .value=${String(combo.race_size)} @change=${onUpdateRaceSize} @input=${onUpdateRaceSize} class="race-input"></label>
        <button class="danger" @click=${onDeleteCombo}>Delete</button></div></div>
    <div class="combo-context-window-bar"><label>Context window:
      <input type="number" min="1" placeholder="auto (${autoCwLabel})" .value=${overrideCw != null ? String(overrideCw) : ""} @change=${onUpdateContextWindow} @input=${onUpdateContextWindow} class="cw-input" title="Override context window (tokens). Empty = auto-compute."></label>
      <span class="cw-hint">Auto: <strong>${autoCwLabel}</strong> · Effective: <strong>${effectiveCwLabel}</strong></span></div>
    ${renderPriorityModeBar(combo)}${renderCooldownBar(combo)}
    ${cds.length > 0 ? html`<div class="cooldown-banner">⏸ ${cds.length} of ${targets.length} target(s) in cooldown — engine will skip them.</div>` : html``}
    <section class="detail-section"><div class="section-header"><h3>Targets (${targets.length})</h3>
      <button class="primary" @click=${() => { location.hash = "#/combos/" + combo.id + "/add-target"; }}>+ Add target</button></div>
      ${targets.length === 0 ? html`<p class="empty">No targets. Add a target to start routing.</p>` : html`<table>
        <thead><tr><th></th><th>#</th><th>Provider</th><th>Account</th><th>Model</th><th>Context</th>${weightTh}<th>Actions</th></tr></thead>
        <tbody>${targets.map((t) => renderTargetRow(t, showWeight))}</tbody></table>`}
    </section>`;
}

function renderComboGrid(): TemplateResult {
  const list = state.combos || [];
  return html`<div class="page-header"><h2>Combos</h2><div class="actions"><button class="primary" @click=${() => { location.hash = "#/combos/new"; }}>+ Create combo</button></div></div>
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
