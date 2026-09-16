import { html, render, type TemplateResult, nothing } from "lit-html";
import { api } from "../../state/api.js";
import { showToast } from "../../components/toast.js";
import { t } from "../../i18n/index.js";
import { state } from "../../state/index.js";
import { icons } from "../../lib/icons.js";
import { setSuppressToasts } from "../../state/notifications-store.js";
import type { Combo, ComboTargetWithModel, Model } from "../../lib/types/api.js";
import { TARGETS_CACHE_TTL_MS, type DragPayload, type CachedTargets } from "./shared.js";

let onMarkAsRead: ((id: number) => void) | null = null;
export function setDndOnMarkAsRead(cb: (id: number) => void): void { onMarkAsRead = cb; }

let overlayEl: HTMLDivElement | null = null;
let combosCache: Combo[] | null = null;
let combosFetchPromise: Promise<void> | null = null;
let combosFetchError: string | null = null;
const targetsCache = new Map<number, CachedTargets>();
let expandedComboId: number | null = null;
let dragPayload: DragPayload | null = null;
let overlayClickMode = false;

async function lookupModelRowId(providerId: string, modelId: string): Promise<number | null> {
  const fromCache = (state.models as Model[]).find((m) => m.provider_id === providerId && m.model_id === modelId);
  if (fromCache) return fromCache.row_id;
  try {
    const raw = await api("/models");
    if (Array.isArray(raw)) {
      state.models = raw as Model[];
      return (state.models as Model[]).find((x) => x.provider_id === providerId && x.model_id === modelId)?.row_id ?? null;
    }
  } catch { /* fall */ }
  return null;
}

async function ensureCombos(): Promise<void> {
  if (combosCache || combosFetchPromise) return combosFetchPromise ?? Promise.resolve();
  combosFetchError = null;
  combosFetchPromise = (async () => {
    try {
      const raw = await api("/combos");
      combosCache = Array.isArray(raw) ? (raw as Combo[]) : [];
    } catch (e: unknown) {
      combosFetchError = e instanceof Error ? e.message : String(e); combosCache = [];
    } finally { combosFetchPromise = null; }
    renderDndOverlay();
  })();
  return combosFetchPromise;
}

function ensureTargets(comboId: number): void {
  const cached = targetsCache.get(comboId);
  if (cached && Date.now() - cached.fetchedAt < TARGETS_CACHE_TTL_MS) return;
  void (async () => {
    try {
      const raw = await api(`/combos/${comboId}/targets`);
      targetsCache.set(comboId, { targets: Array.isArray(raw) ? (raw as ComboTargetWithModel[]) : [], fetchedAt: Date.now() });
    } catch { targetsCache.set(comboId, { targets: [], fetchedAt: Date.now() }); }
    renderDndOverlay();
  })();
}

function onOverlayKeydown(e: KeyboardEvent): void {
  if (e.key === "Escape" && overlayEl) { e.preventDefault(); closeOverlay(); }
}

export function openOverlay(payload: DragPayload, clickMode: boolean): void {
  if (!overlayEl) {
    overlayEl = document.createElement("div");
    overlayEl.id = "notification-dnd-overlay"; overlayEl.className = "dnd-overlay";
    overlayEl.addEventListener("click", (e) => { if (e.target === overlayEl) closeOverlay(); });
    document.body.appendChild(overlayEl);
  }
  dragPayload = payload; overlayClickMode = clickMode; expandedComboId = null;
  setSuppressToasts(true); window.addEventListener("keydown", onOverlayKeydown);
  renderDndOverlay(); void ensureCombos();
}

export function closeOverlay(): void {
  window.removeEventListener("keydown", onOverlayKeydown);
  if (overlayEl) { render(nothing, overlayEl); overlayEl.remove(); overlayEl = null; }
  dragPayload = null; overlayClickMode = false; expandedComboId = null; setSuppressToasts(false);
}

export function resetDndState(): void { expandedComboId = null; dragPayload = null; overlayClickMode = false; }

function renderDndOverlay(): void {
  if (!overlayEl || !dragPayload) return;
  const payload = dragPayload;
  let body: TemplateResult | typeof nothing;
  if (combosFetchPromise) body = html`<div class="dnd-loading">${t("notifications.dnd.fetching_combos")}</div>`;
  else if (combosFetchError) body = html`<div class="dnd-error">${combosFetchError}</div>`;
  else if (!combosCache || combosCache.length === 0) body = html`<div class="dnd-empty">${t("notifications.dnd.no_combos")}</div>`;
  else body = html`<div class="dnd-combos-list">${combosCache.map((c) => renderComboRow(c, payload))}</div>`;

  render(html`
    <div class="dnd-overlay-card">
      <div class="dnd-overlay-header">
        <h3>${t("notifications.dnd.hint", { model_id: payload.model_id })}</h3>
        <p class="muted">${t("notifications.dnd.overlay_subhead")}</p>
      </div>
      ${body}
      <div class="dnd-overlay-footer"><button type="button" @click=${closeOverlay}>${t("common.close")}</button></div>
    </div>`, overlayEl);
}

function renderComboRow(combo: Combo, payload: DragPayload): TemplateResult {
  const isExpanded = expandedComboId === combo.id;
  const cached = targetsCache.get(combo.id);
  const targets = cached?.targets ?? [];
  const hint = isExpanded ? t("notifications.dnd.expand_combo", { combo_name: combo.name }) : t("notifications.dnd.drop_here", { combo_name: combo.name });
  return html`<div class="dnd-combo${isExpanded ? " expanded" : ""}" data-combo-id=${String(combo.id)}>
    <div class="dnd-combo-header"
         @dragover=${(e: DragEvent) => e.preventDefault()}
         @dragenter=${(e: DragEvent) => { e.preventDefault(); if (expandedComboId !== combo.id) { expandedComboId = combo.id; ensureTargets(combo.id); renderDndOverlay(); } }}
         @drop=${(e: DragEvent) => { e.preventDefault(); void onDropAppend(combo, payload); }}
         @click=${overlayClickMode ? () => void onDropAppend(combo, payload) : null}>
      <span class="dnd-combo-name">${combo.name || ("Combo #" + String(combo.id))}</span>
      <span class="dnd-combo-hint">${hint}</span>
    </div>
    ${isExpanded ? html`<div class="dnd-combo-targets">${!cached ? html`<div class="dnd-loading">${t("notifications.dnd.fetching_combos")}</div>` : renderTargetsWithIndicators(combo, targets, payload)}</div>` : nothing}
  </div>`;
}

function renderTargetsWithIndicators(combo: Combo, targets: ComboTargetWithModel[], payload: DragPayload): TemplateResult {
  const sorted = [...targets].sort((a, b) => a.priority_order - b.priority_order);
  const items: TemplateResult[] = [];
  for (let i = 0; i < sorted.length; i++) {
    items.push(renderDropIndicator(combo, payload, i));
    const tgt = sorted[i]!;
    const isSub = tgt.sub_combo_id != null;
    const name = isSub ? "→ " + (tgt.sub_combo_name ?? "#" + String(tgt.sub_combo_id)) : (tgt.model_display_name || tgt.model_id || "row #" + String(tgt.model_row_id));
    items.push(html`<div class="dnd-target"><span class="dnd-target-pos">${String(tgt.priority_order)}</span><span class="dnd-target-name">${name}${tgt.in_cooldown ? html` <span class="badge badge-cooldown">${icons.pause()}</span>` : nothing}</span><span class="dnd-target-provider">${tgt.provider_id}</span></div>`);
  }
  items.push(renderDropIndicator(combo, payload, sorted.length));
  return html`${items}`;
}

function renderDropIndicator(combo: Combo, payload: DragPayload, position: number): TemplateResult {
  const hint = t("notifications.dnd.release_to_position", { combo_name: combo.name });
  return html`<div class="dnd-drop-indicator" data-position=${String(position)} title=${hint}
    @dragover=${(e: DragEvent) => { e.preventDefault(); (e.currentTarget as HTMLElement).classList.add("over"); }}
    @dragleave=${(e: DragEvent) => (e.currentTarget as HTMLElement).classList.remove("over")}
    @drop=${(e: DragEvent) => { e.preventDefault(); (e.currentTarget as HTMLElement).classList.remove("over"); void onDropAtPosition(combo, payload, position); }}
    @click=${overlayClickMode ? () => void onDropAtPosition(combo, payload, position) : null}>
    <span class="dnd-drop-indicator-line"></span><span class="dnd-drop-indicator-hint">${hint}</span>
  </div>`;
}

async function onDropAppend(combo: Combo, payload: DragPayload): Promise<void> {
  const modelRowId = await lookupModelRowId(payload.provider_id, payload.model_id);
  if (modelRowId == null) { showToast(t("notifications.dnd.failed", { model_id: payload.model_id, combo_name: combo.name, error: "model row not found" }), "error"); return; }
  const cached = targetsCache.get(combo.id);
  const maxOrder = cached ? cached.targets.reduce((m, x) => Math.max(m, x.priority_order), 0) : 0;
  try {
    await api(`/combos/${combo.id}/targets`, { method: "POST", body: JSON.stringify({ provider_id: payload.provider_id, account_id: null, model_row_id: modelRowId, sub_combo_id: null, priority_order: maxOrder + 1 }) });
    targetsCache.delete(combo.id);
    showToast(t("notifications.dnd.added_success", { model_id: payload.model_id, combo_name: combo.name }), "success");
    onMarkAsRead?.(payload.notification_id); closeOverlay(); location.hash = `#/combos/${combo.id}`;
  } catch (e: unknown) { showToast(t("notifications.dnd.failed", { model_id: payload.model_id, combo_name: combo.name, error: e instanceof Error ? e.message : String(e) }), "error"); }
}

async function onDropAtPosition(combo: Combo, payload: DragPayload, position: number): Promise<void> {
  const modelRowId = await lookupModelRowId(payload.provider_id, payload.model_id);
  if (modelRowId == null) { showToast(t("notifications.dnd.failed", { model_id: payload.model_id, combo_name: combo.name, error: "model row not found" }), "error"); return; }
  let cached = targetsCache.get(combo.id);
  if (!cached) {
    try {
      const raw = await api(`/combos/${combo.id}/targets`);
      cached = { targets: Array.isArray(raw) ? (raw as ComboTargetWithModel[]) : [], fetchedAt: Date.now() };
      targetsCache.set(combo.id, cached);
    } catch (e: unknown) { showToast(t("notifications.dnd.failed", { model_id: payload.model_id, combo_name: combo.name, error: e instanceof Error ? e.message : String(e) }), "error"); return; }
  }
  const sorted = [...cached.targets].sort((a, b) => a.priority_order - b.priority_order);
  const maxOrder = sorted.reduce((m, x) => Math.max(m, x.priority_order), 0);
  try {
    const res = await api(`/combos/${combo.id}/targets`, { method: "POST", body: JSON.stringify({ provider_id: payload.provider_id, account_id: null, model_row_id: modelRowId, sub_combo_id: null, priority_order: maxOrder + 1 }) }) as { id?: number };
    const newTargetId = res?.id;
    if (typeof newTargetId !== "number") throw new Error("missing id");
    const existingIds = sorted.map((t) => t.id);
    const clampedPos = Math.max(0, Math.min(position, existingIds.length));
    const newOrder = [...existingIds.slice(0, clampedPos), newTargetId, ...existingIds.slice(clampedPos)];
    await api(`/combos/${combo.id}/targets/reorder`, { method: "POST", body: JSON.stringify({ target_ids: newOrder }) });
    targetsCache.delete(combo.id);
    showToast(t("notifications.dnd.added_at_position", { model_id: payload.model_id, combo_name: combo.name, position: clampedPos + 1 }), "success");
    onMarkAsRead?.(payload.notification_id); closeOverlay(); location.hash = `#/combos/${combo.id}`;
  } catch (e: unknown) { showToast(t("notifications.dnd.failed", { model_id: payload.model_id, combo_name: combo.name, error: e instanceof Error ? e.message : String(e) }), "error"); }
}
