// views/notifications/dnd-overlay.ts — drag-and-drop overlay for adding
// notification models to combos. Self-contained module with its own
// mount/unmount lifecycle, combos/targets cache, and overlay rendering.

import { html, render, type TemplateResult, nothing } from "lit-html";
import { api } from "../../state/api.js";
import { showToast } from "../../components/toast.js";
import { t } from "../../i18n/index.js";
import { state } from "../../state/index.js";
import { icons } from "../../lib/icons.js";
import { setSuppressToasts } from "../../state/notifications-store.js";
import type { Combo, ComboTargetWithModel, Model } from "../../lib/types/api.js";
import {
  TARGETS_CACHE_TTL_MS,
  type DragPayload,
  type CachedTargets,
} from "./shared.js";

// ==========
// Hook — registered by list.ts on mount
// ==========

/** Called by drop handlers after a successful add-to-combo. Set by
 *  `list.ts` via `setDndOnMarkAsRead()`. Accepts a notification id;
 *  implementation should fire-and-forget the async read call. */
let onMarkAsRead: ((notificationId: number) => void) | null = null;

export function setDndOnMarkAsRead(cb: (notificationId: number) => void): void {
  onMarkAsRead = cb;
}

// ==========
// DnD — state
// ==========

/** The overlay container. Created lazily on the first drag / "Add to
 *  combo" click; removed on cleanup. */
let overlayEl: HTMLDivElement | null = null;

/** Combos list, lazily fetched on the first overlay open. Cached for
 *  the session — the combos list doesn't change often. */
let combosCache: Combo[] | null = null;
let combosFetchPromise: Promise<void> | null = null;
let combosFetchError: string | null = null;

/** Per-combo targets cache. Keyed by combo id. Entries expire after
 *  `TARGETS_CACHE_TTL_MS`. */
const targetsCache: Map<number, CachedTargets> = new Map<number, CachedTargets>();

/** Which combo is currently expanded in the overlay. Only one combo
 *  can be expanded at a time. */
let expandedComboId: number | null = null;

/** The active drag payload. Set on `dragstart` and on "Add to combo"
 *  click; cleared on `dragend` / overlay close. */
let dragPayload: DragPayload | null = null;

/** True when the overlay was opened via the "Add to combo" button
 *  (click-mode) rather than a drag. */
let overlayClickMode: boolean = false;

// ==========
// DnD — model row lookup
// ==========

/** Look up the `model_row_id` for a (provider_id, model_id) pair. The
 *  `POST /admin/combos/:id/targets` endpoint requires the row id, not
 *  the upstream model id. We check `state.models` first (cached from
 *  the providers view) and fall back to fetching `/admin/models`. */
async function lookupModelRowId(providerId: string, modelId: string): Promise<number | null> {
  const fromCache: Model | undefined = (state.models as Model[]).find(
    (m) => m.provider_id === providerId && m.model_id === modelId,
  );
  if (fromCache) return fromCache.row_id;
  try {
    const raw: unknown = await api("/models");
    if (Array.isArray(raw)) {
      state.models = raw as Model[];
      const m: Model | undefined = (state.models as Model[]).find(
        (x) => x.provider_id === providerId && x.model_id === modelId,
      );
      return m ? m.row_id : null;
    }
  } catch (_e: unknown) {
    // Fall through to return null.
  }
  return null;
}

// ==========
// DnD — combo + targets cache
// ==========

/** Fetch the combos list. Cached for the session. Concurrent calls
 *  coalesce into the same promise. */
async function ensureCombos(): Promise<void> {
  if (combosCache || combosFetchPromise) {
    return combosFetchPromise ?? Promise.resolve();
  }
  combosFetchError = null;
  combosFetchPromise = (async () => {
    try {
      const raw: unknown = await api("/combos");
      if (Array.isArray(raw)) {
        combosCache = raw as Combo[];
      } else {
        combosCache = [];
      }
    } catch (e: unknown) {
      combosFetchError = e instanceof Error ? e.message : String(e);
      combosCache = [];
    } finally {
      combosFetchPromise = null;
    }
    renderDndOverlay();
  })();
  return combosFetchPromise;
}

/** Fetch a combo's targets, using the cache if fresh. */
function ensureTargets(comboId: number): void {
  const cached: CachedTargets | undefined = targetsCache.get(comboId);
  if (cached && Date.now() - cached.fetchedAt < TARGETS_CACHE_TTL_MS) {
    return;
  }
  void (async () => {
    try {
      const raw: unknown = await api(`/combos/${comboId}/targets`);
      if (Array.isArray(raw)) {
        targetsCache.set(comboId, {
          targets: raw as ComboTargetWithModel[],
          fetchedAt: Date.now(),
        });
      } else {
        targetsCache.set(comboId, { targets: [], fetchedAt: Date.now() });
      }
    } catch (_e: unknown) {
      targetsCache.set(comboId, { targets: [], fetchedAt: Date.now() });
    }
    renderDndOverlay();
  })();
}

/** Invalidate the targets cache for a combo. Called after adding a
 *  target so the next hover shows the fresh list. */
function invalidateTargets(comboId: number): void {
  targetsCache.delete(comboId);
}

// ==========
// DnD — overlay rendering
// ==========

function onOverlayKeydown(e: KeyboardEvent): void {
  if (e.key === "Escape" && overlayEl) {
    e.preventDefault();
    closeOverlay();
  }
}

export function openOverlay(payload: DragPayload, clickMode: boolean): void {
  if (!overlayEl) {
    overlayEl = document.createElement("div");
    overlayEl.id = "notification-dnd-overlay";
    overlayEl.className = "dnd-overlay";
    overlayEl.addEventListener("click", (e: MouseEvent) => {
      if (e.target === overlayEl) closeOverlay();
    });
    document.body.appendChild(overlayEl);
  }
  dragPayload = payload;
  overlayClickMode = clickMode;
  expandedComboId = null;
  setSuppressToasts(true);
  window.addEventListener("keydown", onOverlayKeydown);
  renderDndOverlay();
  void ensureCombos();
}

export function closeOverlay(): void {
  window.removeEventListener("keydown", onOverlayKeydown);
  if (overlayEl) {
    render(nothing, overlayEl);
    overlayEl.remove();
    overlayEl = null;
  }
  dragPayload = null;
  overlayClickMode = false;
  expandedComboId = null;
  setSuppressToasts(false);
}

/** Reset the DnD module state on view re-mount. The original
 *  `mountNotifications()` reset `expandedComboId` directly; since the
 *  state is now module-local, this function provides the same effect. */
export function resetDndState(): void {
  expandedComboId = null;
  dragPayload = null;
  overlayClickMode = false;
}

function renderDndOverlay(): void {
  if (!overlayEl || !dragPayload) return;
  const payload: DragPayload = dragPayload;
  const hint: string = t("notifications.dnd.hint", { model_id: payload.model_id });
  let body: TemplateResult | typeof nothing;
  if (combosFetchPromise) {
    body = html`<div class="dnd-loading">${t("notifications.dnd.fetching_combos")}</div>`;
  } else if (combosFetchError) {
    body = html`<div class="dnd-error">${combosFetchError}</div>`;
  } else if (!combosCache || combosCache.length === 0) {
    body = html`<div class="dnd-empty">${t("notifications.dnd.no_combos")}</div>`;
  } else {
    body = html`<div class="dnd-combos-list">
      ${combosCache.map((c) => renderComboRow(c, payload))}
    </div>`;
  }
  render(html`
    <div class="dnd-overlay-card">
      <div class="dnd-overlay-header">
        <h3>${hint}</h3>
        <p class="muted">${t("notifications.dnd.overlay_subhead")}</p>
      </div>
      ${body}
      <div class="dnd-overlay-footer">
        <button type="button" @click=${closeOverlay}>${t("common.close")}</button>
      </div>
    </div>
  `, overlayEl);
}

function renderComboRow(combo: Combo, payload: DragPayload): TemplateResult {
  const isExpanded: boolean = expandedComboId === combo.id;
  const cached: CachedTargets | undefined = targetsCache.get(combo.id);
  const targets: ComboTargetWithModel[] = cached?.targets ?? [];
  const fetchingTargets: boolean = isExpanded && !cached;
  const hint: string = isExpanded
    ? t("notifications.dnd.expand_combo", { combo_name: combo.name })
    : t("notifications.dnd.drop_here", { combo_name: combo.name });
  return html`<div class="dnd-combo${isExpanded ? " expanded" : ""}" data-combo-id=${String(combo.id)}>
    <div class="dnd-combo-header"
         @dragover=${(e: DragEvent) => { e.preventDefault(); }}
         @dragenter=${(e: DragEvent) => {
           e.preventDefault();
           if (expandedComboId !== combo.id) {
             expandedComboId = combo.id;
             ensureTargets(combo.id);
             renderDndOverlay();
           }
         }}
         @drop=${(e: DragEvent) => {
           e.preventDefault();
           void onDropAppend(combo, payload);
         }}
         @click=${overlayClickMode ? () => { void onDropAppend(combo, payload); } : null}
      >
      <span class="dnd-combo-name">${combo.name || ("Combo #" + String(combo.id))}</span>
      <span class="dnd-combo-hint">${hint}</span>
    </div>
    ${isExpanded ? html`<div class="dnd-combo-targets">
      ${fetchingTargets
        ? html`<div class="dnd-loading">${t("notifications.dnd.fetching_combos")}</div>`
        : renderTargetsWithIndicators(combo, targets, payload)}
    </div>` : nothing}
  </div>`;
}

function renderTargetsWithIndicators(
  combo: Combo,
  targets: ComboTargetWithModel[],
  payload: DragPayload,
): TemplateResult {
  const sorted: ComboTargetWithModel[] = [...targets].sort(
    (a, b) => a.priority_order - b.priority_order,
  );
  const items: TemplateResult[] = [];
  for (let i = 0; i < sorted.length; i++) {
    const position: number = i;
    items.push(renderDropIndicator(combo, payload, position));
    items.push(renderTargetRow(sorted[i]!));
  }
  items.push(renderDropIndicator(combo, payload, sorted.length));
  return html`${items}`;
}

function renderDropIndicator(
  combo: Combo,
  payload: DragPayload,
  position: number,
): TemplateResult {
  const hint: string = t("notifications.dnd.release_to_position", { combo_name: combo.name });
  return html`<div class="dnd-drop-indicator"
    data-position=${String(position)}
    title=${hint}
    @dragover=${(e: DragEvent) => { e.preventDefault(); (e.currentTarget as HTMLElement).classList.add("over"); }}
    @dragleave=${(e: DragEvent) => { (e.currentTarget as HTMLElement).classList.remove("over"); }}
    @drop=${(e: DragEvent) => {
      e.preventDefault();
      (e.currentTarget as HTMLElement).classList.remove("over");
      void onDropAtPosition(combo, payload, position);
    }}
    @click=${overlayClickMode ? () => { void onDropAtPosition(combo, payload, position); } : null}
  ><span class="dnd-drop-indicator-line"></span><span class="dnd-drop-indicator-hint">${hint}</span></div>`;
}

function renderTargetRow(tgt: ComboTargetWithModel): TemplateResult {
  const isSub: boolean = tgt.sub_combo_id != null;
  const name: string = isSub
    ? "→ " + (tgt.sub_combo_name ?? "#" + String(tgt.sub_combo_id))
    : (tgt.model_display_name || tgt.model_id || "row #" + String(tgt.model_row_id));
  const cdBadge: TemplateResult = tgt.in_cooldown
    ? html` <span class="badge badge-cooldown" title=${tgt.cooldown_reason ?? ""}>${icons.pause()}</span>`
    : html``;
  return html`<div class="dnd-target">
    <span class="dnd-target-pos">${String(tgt.priority_order)}</span>
    <span class="dnd-target-name">${name}${cdBadge}</span>
    <span class="dnd-target-provider">${tgt.provider_id}</span>
  </div>`;
}

// ==========
// DnD — drop actions
// ==========

/** Drop on a combo header → append at end. */
async function onDropAppend(combo: Combo, payload: DragPayload): Promise<void> {
  const modelRowId: number | null = await lookupModelRowId(payload.provider_id, payload.model_id);
  if (modelRowId == null) {
    showToast(
      t("notifications.dnd.failed", {
        model_id: payload.model_id,
        combo_name: combo.name,
        error: "model row not found",
      }),
      "error",
    );
    return;
  }
  const cached: CachedTargets | undefined = targetsCache.get(combo.id);
  const maxOrder: number = cached
    ? cached.targets.reduce((m, x) => Math.max(m, x.priority_order), 0)
    : 0;
  try {
    await api(`/combos/${combo.id}/targets`, {
      method: "POST",
      body: JSON.stringify({
        provider_id: payload.provider_id,
        account_id: null,
        model_row_id: modelRowId,
        sub_combo_id: null,
        priority_order: maxOrder + 1,
      }),
    });
    invalidateTargets(combo.id);
    showToast(
      t("notifications.dnd.added_success", { model_id: payload.model_id, combo_name: combo.name }),
      "success",
    );
    onMarkAsRead?.(payload.notification_id);
    closeOverlay();
    location.hash = `#/combos/${combo.id}`;
  } catch (e: unknown) {
    showToast(
      t("notifications.dnd.failed", {
        model_id: payload.model_id,
        combo_name: combo.name,
        error: e instanceof Error ? e.message : String(e),
      }),
      "error",
    );
  }
}

/** Drop on a position indicator → insert at that position. We append
 *  first (to get the new target id back), then reorder so the new
 *  target lands at the desired position. */
async function onDropAtPosition(
  combo: Combo,
  payload: DragPayload,
  position: number,
): Promise<void> {
  const modelRowId: number | null = await lookupModelRowId(payload.provider_id, payload.model_id);
  if (modelRowId == null) {
    showToast(
      t("notifications.dnd.failed", {
        model_id: payload.model_id,
        combo_name: combo.name,
        error: "model row not found",
      }),
      "error",
    );
    return;
  }
  let cached: CachedTargets | undefined = targetsCache.get(combo.id);
  if (!cached) {
    try {
      const raw: unknown = await api(`/combos/${combo.id}/targets`);
      cached = {
        targets: Array.isArray(raw) ? (raw as ComboTargetWithModel[]) : [],
        fetchedAt: Date.now(),
      };
      targetsCache.set(combo.id, cached);
    } catch (e: unknown) {
      showToast(
        t("notifications.dnd.failed", {
          model_id: payload.model_id,
          combo_name: combo.name,
          error: e instanceof Error ? e.message : String(e),
        }),
        "error",
      );
      return;
    }
  }
  const sorted: ComboTargetWithModel[] = [...cached.targets].sort(
    (a, b) => a.priority_order - b.priority_order,
  );
  const maxOrder: number = sorted.reduce((m, x) => Math.max(m, x.priority_order), 0);
  let newTargetId: number;
  try {
    const res: unknown = await api(`/combos/${combo.id}/targets`, {
      method: "POST",
      body: JSON.stringify({
        provider_id: payload.provider_id,
        account_id: null,
        model_row_id: modelRowId,
        sub_combo_id: null,
        priority_order: maxOrder + 1,
      }),
    });
    if (res && typeof res === "object" && "id" in res) {
      const id: unknown = (res as Record<string, unknown>)["id"];
      if (typeof id === "number") {
        newTargetId = id;
      } else {
        throw new Error("unexpected response from add-target: missing id");
      }
    } else {
      throw new Error("unexpected response from add-target");
    }
  } catch (e: unknown) {
    showToast(
      t("notifications.dnd.failed", {
        model_id: payload.model_id,
        combo_name: combo.name,
        error: e instanceof Error ? e.message : String(e),
      }),
      "error",
    );
    return;
  }
  const existingIds: number[] = sorted.map((t) => t.id);
  const clampedPos: number = Math.max(0, Math.min(position, existingIds.length));
  const newOrder: number[] = [
    ...existingIds.slice(0, clampedPos),
    newTargetId,
    ...existingIds.slice(clampedPos),
  ];
  try {
    await api(`/combos/${combo.id}/targets/reorder`, {
      method: "POST",
      body: JSON.stringify({ target_ids: newOrder }),
    });
    invalidateTargets(combo.id);
    showToast(
      t("notifications.dnd.added_at_position", {
        model_id: payload.model_id,
        combo_name: combo.name,
        position: clampedPos + 1,
      }),
      "success",
    );
    onMarkAsRead?.(payload.notification_id);
    closeOverlay();
    location.hash = `#/combos/${combo.id}`;
  } catch (e: unknown) {
    showToast(
      t("notifications.dnd.failed", {
        model_id: payload.model_id,
        combo_name: combo.name,
        error: e instanceof Error ? e.message : String(e),
      }),
      "error",
    );
  }
}
