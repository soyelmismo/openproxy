// views/notifications.ts — notifications tray view (F4).
//
// Renders the list of notifications with filters, real-time updates via
// the shared notifications store, and the centerpiece drag-and-drop
// interaction: the user drags a `model_new` / `model_auto_activated`
// card onto a combo (or a specific position within a combo) to add the
// model as a target. The DnD overlay is rendered into a separate
// `<div id="notification-dnd-overlay">` under <body> so it can sit
// above the rest of the UI with `position: fixed`.
//
// Architecture:
//   - `mountNotifications()` is called by the router when the user
//     navigates to `#/notifications`. It fetches the initial list,
//     subscribes to the notifications store for live events, and
//     returns a cleanup function that unsubscribes.
//   - The list itself is rendered via `mountView(main, renderView)`
//     so `requestUpdate()` triggers a microtask-coalesced re-render
//     (same pattern as the other views).
//   - The DnD overlay is rendered into a separate container that we
//     create lazily on the first `dragstart` (or "Add to combo"
//     click). The overlay re-renders via `renderDndOverlay()` whenever
//     its state changes (combos loaded, combo expanded, targets
//     fetched).
//
// i18n: every user-facing string goes through `t()`. Plural forms use
// the `_one` / `_other` suffix convention from `i18n/index.ts` — pass
// `{count: n}` and the helper picks the right variant.
//
// Quirks:
//   - HTML5 DnD requires `e.preventDefault()` on `dragover` for a
//     target to accept a drop. Forgetting it shows the "no drop"
//     cursor and silently rejects the drop.
//   - `dragend` fires on the SOURCE element (the card), not on the
//     drop target. We use it to close the overlay whether the drop
//     succeeded or was cancelled.
//   - Some browsers fire `dragenter` on every child element of a drop
//     target. We attach handlers to the specific drop zones (combo
//     headers, position indicators) rather than the overlay root to
//     avoid the resulting noise.
//   - Touch devices don't fire `dragstart` reliably (the API was
//     designed for mouse + keyboard). The "Add to combo" button opens
//     the same overlay in click-mode so mobile users can still add a
//     model to a combo.

import { html, render, type TemplateResult, nothing } from 'lit-html';
import { api } from "../state/api.js";
import { mountView, requestUpdate } from "../state/reactive.js";
import { showToast } from "../components/toast.js";
import { t } from "../i18n/index.js";
import { state } from "../state/index.js";
import {
  getUnreadCount,
  setUnreadCount,
  decrementUnread,
  refreshUnreadCount,
  onUnreadCountChange,
  onNotificationEvent,
  setSuppressToasts,
  notificationBody,
  formatRelativeAgo,
} from "../state/notifications-store.js";
import type {
  NotificationEvent,
  NotificationRow,
  NotificationKind,
} from "../lib/types/notifications.js";
import type {
  Combo,
  ComboTargetWithModel,
  Model,
} from "../lib/types/api.js";

// ============================================================================
// Constants
// ============================================================================

/** Filter dropdown options. The `key` is the i18n key for the label. */
const FILTER_OPTIONS: ReadonlyArray<{
  value: "all" | "unread" | NotificationKind;
  key: string;
}> = [
  { value: "all", key: "notifications.filter.all" },
  { value: "unread", key: "notifications.filter.unread" },
  { value: "model_new", key: "notifications.filter.model_new" },
  { value: "model_gone", key: "notifications.filter.model_gone" },
  { value: "model_auto_activated", key: "notifications.filter.model_auto_activated" },
  { value: "system", key: "notifications.filter.system" },
];

/** Per-kind icon glyph. Kept as a unicode string so it inherits the
 *  text color of its container (vs. an SVG that would need its own
 *  stroke color management). */
const KIND_ICON: Record<NotificationKind, string> = {
  model_new: "⊕",
  model_gone: "⊖",
  model_auto_activated: "⚡",
  system: "ℹ",
};

/** Kinds that are draggable — i.e. that carry a `model_id` we can add
 *  to a combo. `model_gone` and `system` are informational only. */
const DRAGGABLE_KINDS: ReadonlySet<NotificationKind> = new Set<NotificationKind>([
  "model_new",
  "model_auto_activated",
]);

/** Custom MIME type for the drag transfer. We also stash the payload
 *  in a module-local variable because some browsers don't expose
 *  `dataTransfer.getData()` outside of the `drop` handler (the
 *  `dragover` handler can read it in some browsers but not others).
 *  Reading from the module-local variable is the portable path. */
const DND_MIME: string = "application/x-openproxy-notification";

/** TTL for the per-combo targets cache. The combo targets view can
 *  change while the overlay is open (the user might add a target
 *  mid-drag) — 30s is short enough that stale data is unlikely to
 *  matter, long enough that we don't re-fetch on every dragenter. */
const TARGETS_CACHE_TTL_MS: number = 30_000;

// ============================================================================
// Module-local state
// ============================================================================

/** The current list of notifications, newest first. Prepend on every
 *  live WS event; refetch the whole list on filter change is NOT
 *  needed — the filter is applied client-side. */
let rows: NotificationRow[] = [];

/** Error from the initial fetch. Renders as a banner above the list. */
let loadError: string | null = null;

/** Active filter. Defaults to "all". The dropdown reads/writes this
 *  via the `@change` handler. */
let filter: "all" | "unread" | NotificationKind = "all";

/** Unsubscribe handles for the store subscriptions. Captured so the
 *  cleanup function returned by `mountNotifications()` can release
 *  them — otherwise navigating away would leak the listeners (and
 *  the closures they capture). */
let unsubCount: (() => void) | null = null;
let unsubEvents: (() => void) | null = null;

/** The overlay container. Created lazily on the first drag / "Add to
 *  combo" click; removed on cleanup. Kept module-local so a re-mount
 *  of the view (e.g. after navigating away and back) doesn't strand
 *  a stale overlay. */
let overlayEl: HTMLDivElement | null = null;

/** Combos list, lazily fetched on the first overlay open. Cached for
 *  the session — the combos list doesn't change often, and re-fetching
 *  on every drag would be wasteful. */
let combosCache: Combo[] | null = null;
let combosFetchPromise: Promise<void> | null = null;
let combosFetchError: string | null = null;

/** Per-combo targets cache. Keyed by combo id. Entries expire after
 *  `TARGETS_CACHE_TTL_MS`. The cache is invalidated (entry removed)
 *  whenever we add a target to that combo so the next hover re-fetches
 *  the fresh list. */
interface CachedTargets {
  targets: ComboTargetWithModel[];
  fetchedAt: number;
}
const targetsCache: Map<number, CachedTargets> = new Map<number, CachedTargets>();

/** Which combo is currently expanded in the overlay. Only one combo
 *  can be expanded at a time — `dragenter` on a different combo
 *  header collapses the previous one. `null` means no combo is
 *  expanded (the overlay just shows the headers). */
let expandedComboId: number | null = null;

/** The active drag payload. Set on `dragstart` and on "Add to combo"
 *  click; cleared on `dragend` / overlay close. The drop handler
 *  reads from this rather than `dataTransfer.getData()` for browser
 *  portability (see DND_MIME comment). */
let dragPayload: DragPayload | null = null;

/** True when the overlay was opened via the "Add to combo" button
 *  (click-mode) rather than a drag. In click-mode, combo headers
 *  respond to click (in addition to drop). */
let overlayClickMode: boolean = false;

interface DragPayload {
  notification_id: number;
  provider_id: string;
  model_id: string;
}

// ============================================================================
// Helpers — payload narrowing
// ============================================================================

function isUnread(r: NotificationRow): boolean {
  return r.read_at === null && r.archived_at === null;
}

function payloadString(p: Record<string, unknown>, key: string): string {
  const v: unknown = p[key];
  return typeof v === "string" ? v : "";
}

function payloadProviderId(r: NotificationRow): string {
  // `r.provider_id` is the denormalized column on the row (set by F1
  // for grouping); the per-payload `provider_id` is the source of
  // truth. Prefer the payload, fall back to the row column.
  const fromPayload: string = payloadString(r.payload, "provider_id");
  return fromPayload || (r.provider_id ?? "");
}

function payloadModelId(r: NotificationRow): string {
  return payloadString(r.payload, "model_id");
}

function matchesFilter(r: NotificationRow): boolean {
  if (filter === "all") return true;
  if (filter === "unread") return isUnread(r);
  return r.kind === filter;
}

// ============================================================================
// API helpers
// ============================================================================

/** Fetch the initial list. The endpoint returns the 50 most-recent
 *  rows (read + unread). The view filters client-side. */
async function fetchInitial(): Promise<void> {
  try {
    const raw: unknown = await api("/notifications?limit=50");
    if (Array.isArray(raw)) {
      rows = raw as NotificationRow[];
    } else {
      rows = [];
    }
    loadError = null;
  } catch (e: unknown) {
    loadError = e instanceof Error ? e.message : String(e);
    rows = [];
  }
  requestUpdate();
}

/** Mark a single notification as read. Idempotent — the server returns
 *  200 even if the row was already read. Updates the local row + the
 *  unread count optimistically, then re-syncs from the server. */
async function markAsRead(id: number): Promise<void> {
  try {
    await api(`/notifications/${id}/read`, { method: "POST" });
    const r: NotificationRow | undefined = rows.find((x) => x.id === id);
    if (r && r.read_at === null) {
      r.read_at = new Date().toISOString();
      decrementUnread(1);
      requestUpdate();
    }
    // Re-sync in case the optimistic update was wrong (e.g. the row
    // was already read on the server but our local copy was stale).
    void refreshUnreadCount();
  } catch (e: unknown) {
    showToast("Error: " + (e instanceof Error ? e.message : String(e)), "error");
  }
}

/** Mark all notifications as read. Updates local state + count. */
async function markAllRead(): Promise<void> {
  try {
    await api("/notifications/read-all", { method: "POST" });
    const nowIso: string = new Date().toISOString();
    for (const r of rows) {
      if (r.read_at === null) r.read_at = nowIso;
    }
    setUnreadCount(0);
    requestUpdate();
  } catch (e: unknown) {
    showToast("Error: " + (e instanceof Error ? e.message : String(e)), "error");
  }
}

/** Archive (dismiss) a notification. The card is removed from the
 *  list optimistically; the server call follows. On error, we
 *  refetch the list to restore the row. */
async function archive(id: number): Promise<void> {
  const snapshot: NotificationRow[] = rows;
  const wasUnread: boolean = rows.find((x) => x.id === id)?.read_at === null;
  rows = rows.filter((x) => x.id !== id);
  if (wasUnread) decrementUnread(1);
  requestUpdate();
  try {
    await api(`/notifications/${id}/archive`, { method: "POST" });
    void refreshUnreadCount();
  } catch (e: unknown) {
    // Restore the row + count. The user sees a brief flash.
    rows = snapshot;
    if (wasUnread) {
      // decrementUnread above reduced the count; restore it.
      setUnreadCount(getUnreadCount() + 1);
    }
    requestUpdate();
    showToast("Error: " + (e instanceof Error ? e.message : String(e)), "error");
  }
}

// ============================================================================
// DnD — combo + targets cache
// ============================================================================

/** Fetch the combos list. Cached for the session — the list rarely
 *  changes, and the operator can refresh by closing + re-opening the
 *  overlay. Concurrent calls coalesce into the same promise. */
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

/** Fetch a combo's targets, using the cache if fresh. Returns the
 *  cached list immediately if available; otherwise kicks off a fetch
 *  and re-renders the overlay when it resolves. */
function ensureTargets(comboId: number): void {
  const cached: CachedTargets | undefined = targetsCache.get(comboId);
  if (cached && Date.now() - cached.fetchedAt < TARGETS_CACHE_TTL_MS) {
    return;
  }
  // Fire-and-forget. The overlay re-renders with the new targets
  // when the fetch resolves.
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

/** Invalidate the targets cache for a combo. Called after we add a
 *  target to that combo so the next hover shows the fresh list. */
function invalidateTargets(comboId: number): void {
  targetsCache.delete(comboId);
}

/** Look up the `model_row_id` for a (provider_id, model_id) pair. The
 *  `POST /admin/combos/:id/targets` endpoint requires the row id, not
 *  the upstream model id. We check `state.models` first (cached from
 *  the providers view) and fall back to fetching `/admin/models`. */
async function lookupModelRowId(providerId: string, modelId: string): Promise<number | null> {
  const fromCache: Model | undefined = (state.models as Model[]).find(
    (m) => m.provider_id === providerId && m.model_id === modelId,
  );
  if (fromCache) return fromCache.row_id;
  // Cache miss — fetch all models and try again. The endpoint has no
  // filter, but the dashboard's model list is small enough that this
  // is fine.
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

// ============================================================================
// DnD — overlay rendering
// ============================================================================

function openOverlay(payload: DragPayload, clickMode: boolean): void {
  if (!overlayEl) {
    overlayEl = document.createElement("div");
    overlayEl.id = "notification-dnd-overlay";
    overlayEl.className = "dnd-overlay";
    // Backdrop click closes the overlay (only fires in click-mode —
    // during an HTML5 drag, no `click` event is emitted, only drag
    // events). The inner `.dnd-overlay-card` stops propagation so
    // clicks inside the card don't bubble to the backdrop.
    overlayEl.addEventListener("click", (e: MouseEvent) => {
      if (e.target === overlayEl) closeOverlay();
    });
    document.body.appendChild(overlayEl);
  }
  dragPayload = payload;
  overlayClickMode = clickMode;
  expandedComboId = null;
  // Suppress toasts for live notifications while the overlay is open
  // so a fresh notification mid-drag doesn't yank focus.
  setSuppressToasts(true);
  // Escape closes the overlay. The browser already cancels an in-
  // progress HTML5 drag on Escape (firing `dragend` on the source
  // card, which calls `closeOverlay()` via the card's dragend
  // handler), so this listener is mainly for click-mode (touch
  // fallback) where there is no drag to cancel. We attach it on
  // `window` so it fires regardless of focus.
  window.addEventListener("keydown", onOverlayKeydown);
  renderDndOverlay();
  void ensureCombos();
}

function onOverlayKeydown(e: KeyboardEvent): void {
  if (e.key === "Escape" && overlayEl) {
    e.preventDefault();
    closeOverlay();
  }
}

function closeOverlay(): void {
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
      <span class="dnd-combo-name">${combo.name}</span>
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
  // Sort by priority_order to match the server's ordering.
  const sorted: ComboTargetWithModel[] = [...targets].sort(
    (a, b) => a.priority_order - b.priority_order,
  );
  const items: TemplateResult[] = [];
  // Drop indicator BEFORE each target (position 0, 1, ..., n).
  for (let i = 0; i < sorted.length; i++) {
    const position: number = i;
    items.push(renderDropIndicator(combo, payload, position));
    items.push(renderTargetRow(sorted[i]!));
  }
  // Trailing indicator (position = length) — same as "append".
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
    ? html` <span class="badge badge-cooldown" title=${tgt.cooldown_reason ?? ""}>⏸</span>`
    : html``;
  return html`<div class="dnd-target">
    <span class="dnd-target-pos">${String(tgt.priority_order)}</span>
    <span class="dnd-target-name">${name}${cdBadge}</span>
    <span class="dnd-target-provider">${tgt.provider_id}</span>
  </div>`;
}

// ============================================================================
// DnD — drop actions
// ============================================================================

/** Drop on a combo header → append at end. The new target gets the
 *  next `priority_order` (one more than the current max); the reorder
 *  endpoint normalises to 1, 2, ... so the exact value doesn't
 *  matter as long as it's >= max. */
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
  // Compute the next priority_order. Use the cached targets if
  // available; otherwise default to 1 (the server accepts any
  // non-negative integer; the reorder step normalises).
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
    void markAsRead(payload.notification_id);
    closeOverlay();
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
  // We need the current target list to build the reorder payload. If
  // the cache is empty (e.g. the user dropped on a trailing indicator
  // before the targets fetch resolved), fetch synchronously first.
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
    // Response shape: `{ "id": <new_target_id> }`.
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
  // Build the desired order: insert newTargetId at `position`.
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
    void markAsRead(payload.notification_id);
    closeOverlay();
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

// ============================================================================
// Action handlers — view
// ============================================================================

async function onMarkAllRead(): Promise<void> {
  await markAllRead();
}

function onFilterChange(e: Event): void {
  const sel: HTMLSelectElement = e.target as HTMLSelectElement;
  const v: string = sel.value;
  if (v === "all" || v === "unread" || v === "model_new" || v === "model_gone" || v === "model_auto_activated" || v === "system") {
    filter = v;
    requestUpdate();
  }
}

async function onViewProvider(r: NotificationRow): Promise<void> {
  const providerId: string = payloadProviderId(r);
  if (!providerId) {
    showToast("Provider not found in notification payload", "error");
    return;
  }
  // Mark as read first (non-blocking) — the navigation will unmount
  // this view, but the API call goes through.
  void markAsRead(r.id);
  // Navigate to the provider detail view. The router's
  // `provider-detail` route calls `mountProviders({ detailId })`,
  // which renders the provider detail page (the closest thing to a
  // "provider modal" — the dashboard doesn't have a separate modal
  // for provider details, just the dedicated route).
  location.hash = "#/providers/" + encodeURIComponent(providerId);
}

function onAddToComboClick(r: NotificationRow): void {
  const providerId: string = payloadProviderId(r);
  const modelId: string = payloadModelId(r);
  if (!providerId || !modelId) {
    showToast("Notification payload missing provider_id / model_id", "error");
    return;
  }
  openOverlay(
    { notification_id: r.id, provider_id: providerId, model_id: modelId },
    true, // click-mode (touch fallback)
  );
}

async function onDismiss(r: NotificationRow): Promise<void> {
  await archive(r.id);
}

// ============================================================================
// Card rendering
// ============================================================================

function renderCard(r: NotificationRow): TemplateResult {
  const icon: string = KIND_ICON[r.kind] ?? "•";
  const body: string = notificationBody(r);
  const ago: string = formatRelativeAgo(r.created_at);
  const unread: boolean = isUnread(r);
  const draggable: boolean = DRAGGABLE_KINDS.has(r.kind) && !!payloadModelId(r) && !!payloadProviderId(r);
  const showAddToCombo: boolean = DRAGGABLE_KINDS.has(r.kind);
  const cardClasses: string = "notification-card" + (unread ? " unread" : "") + (draggable ? " draggable" : "");
  // Drag handlers — only attached when `draggable` is true. lit-html
  // happily accepts `null` for an event handler and skips it.
  const dragStartHandler: ((e: DragEvent) => void) | null = draggable
    ? (e: DragEvent) => {
        const providerId: string = payloadProviderId(r);
        const modelId: string = payloadModelId(r);
        if (!providerId || !modelId) return;
        const payload: DragPayload = {
          notification_id: r.id,
          provider_id: providerId,
          model_id: modelId,
        };
        // Set the dataTransfer so the browser actually starts a drag
        // (some browsers refuse to start a drag without
        // `setData`). The drop handler reads from `dragPayload` for
        // portability — `dataTransfer.getData()` is not reliably
        // available in `dragover` across browsers.
        if (e.dataTransfer) {
          e.dataTransfer.setData(DND_MIME, JSON.stringify(payload));
          e.dataTransfer.setData("text/plain", modelId);
          // `copy` lets the user drop on either the overlay (our app)
          // or external drop targets (e.g. a text editor). `move`
          // would be more semantically correct but disables external
          // drops.
          e.dataTransfer.effectAllowed = "copy";
        }
        openOverlay(payload, false);
      }
    : null;
  const dragEndHandler: ((e: DragEvent) => void) | null = draggable
    ? (_e: DragEvent) => {
        // `dragend` fires on the source card after a drop OR a
        // cancel (Escape, drop outside the overlay). We always close
        // the overlay — if a drop succeeded, `onDropAppend` /
        // `onDropAtPosition` already called `closeOverlay()` and
        // this is a no-op; if the drag was cancelled, this is the
        // only cleanup path.
        closeOverlay();
      }
    : null;
  return html`<div class=${cardClasses} data-id=${String(r.id)}
      draggable=${draggable ? "true" : "false"}
      @dragstart=${dragStartHandler}
      @dragend=${dragEndHandler}
    >
    <div class="notification-card-icon" aria-hidden="true">${icon}</div>
    <div class="notification-card-body">
      <div class="notification-card-text">${body}</div>
      <div class="notification-card-meta">
        <span class="notification-card-kind">${t("notifications.kind." + r.kind)}</span>
        ${ago ? html`<span class="notification-card-ago">${ago}</span>` : nothing}
        ${unread ? html`<span class="notification-card-unread-dot" title=${t("common.unread")}></span>` : nothing}
      </div>
    </div>
    <div class="notification-card-actions">
      ${payloadProviderId(r)
        ? html`<button class="small" @click=${() => { void onViewProvider(r); }}>${t("notifications.action.view_provider")}</button>`
        : nothing}
      ${showAddToCombo
        ? html`<button class="small" @click=${() => onAddToComboClick(r)}>${t("notifications.action.add_to_combo")}</button>`
        : nothing}
      <button class="small danger" @click=${() => { void onDismiss(r); }}>${t("notifications.action.dismiss")}</button>
    </div>
  </div>`;
}

// ============================================================================
// View rendering
// ============================================================================

function renderFilterDropdown(): TemplateResult {
  return html`<select class="notification-filter" @change=${onFilterChange}>
    ${FILTER_OPTIONS.map((o) => html`<option value=${o.value} ?selected=${o.value === filter}>${t(o.key)}</option>`)}
  </select>`;
}

function renderHeader(): TemplateResult {
  const unread: number = getUnreadCount();
  const unreadLabel: string = unread > 0
    ? t("notifications.unread_count", { count: unread })
    : t("notifications.no_unread");
  return html`<div class="page-header">
    <div class="page-header-title">
      <h2>${t("notifications.title")}</h2>
      <span class="badge ${unread > 0 ? "badge-error" : "badge-info"}">${unreadLabel}</span>
    </div>
    <div class="actions">
      ${renderFilterDropdown()}
      <button class="small" ?disabled=${unread === 0} @click=${() => { void onMarkAllRead(); }}>${t("notifications.mark_all_read")}</button>
    </div>
  </div>`;
}

function renderList(): TemplateResult {
  if (loadError) {
    return html`<div class="banner banner-error">${loadError}</div>`;
  }
  const filtered: NotificationRow[] = rows.filter(matchesFilter);
  if (rows.length === 0) {
    return html`<div class="notification-empty">
      <div class="notification-empty-icon" aria-hidden="true">🔔</div>
      <p>${t("notifications.no_notifications")}</p>
    </div>`;
  }
  // "No unread" hint: shown at the top of the list when there are
  // rows but none match the "unread" filter (or the user is on the
  // "all" filter and everything is read).
  const anyUnread: boolean = rows.some(isUnread);
  const noUnreadHint: TemplateResult | typeof nothing = (!anyUnread && filter !== "all")
    ? html`<div class="notification-no-unread-hint">${t("notifications.no_unread")}</div>`
    : nothing;
  if (filtered.length === 0) {
    return html`${noUnreadHint}<div class="notification-empty">
      <div class="notification-empty-icon" aria-hidden="true">🔍</div>
      <p>${t("common.empty")}</p>
    </div>`;
  }
  return html`${noUnreadHint}<div class="notification-list">${filtered.map(renderCard)}</div>`;
}

function renderView(): TemplateResult {
  return html`${renderHeader()}${renderList()}`;
}

// ============================================================================
// Mount
// ============================================================================

export async function mountNotifications(): Promise<(() => void) | void> {
  const main: HTMLElement | null = document.getElementById("main");
  if (!main) return;

  // Reset view-local state on every mount.
  rows = [];
  loadError = null;
  filter = "all";
  expandedComboId = null;

  // Mount the lit-html view. `mountView` registers the render
  // function with the reactive system so `requestUpdate()` (called
  // from the WS event handler, the mark-as-read handlers, etc.)
  // triggers a microtask-coalesced re-render.
  const cleanupReactive: () => void = mountView(main, renderView);

  // Subscribe to live notification events. The store fans out the
  // parsed `NotificationEvent` to every subscriber. We prepend the
  // new row to the local list (the store already incremented the
  // unread count + showed a toast).
  unsubEvents = onNotificationEvent((evt: NotificationEvent) => {
    // The WS event has the same shape as a `NotificationRow` minus
    // the read_at / archived_at / dedup_key / provider_id columns
    // (the server fills those in on insert). We synthesise a row
    // with nulls so the local list stays shape-compatible.
    const row: NotificationRow = {
      id: evt.id,
      kind: evt.kind,
      payload: evt.payload,
      read_at: null,
      archived_at: null,
      created_at: evt.created_at,
      dedup_key: null,
      provider_id: null,
    };
    // Prepend only if the row isn't already in the list (defensive
    // against duplicate WS events).
    if (!rows.some((r) => r.id === row.id)) {
      rows = [row, ...rows];
      requestUpdate();
    }
  });

  // Subscribe to unread-count changes so the header badge re-renders
  // when the count drops (e.g. the user marked a notification as read
  // from another tab, or the 30s poll fetched a fresh count).
  unsubCount = onUnreadCountChange(() => {
    requestUpdate();
  });

  // Fetch the initial list.
  void fetchInitial();

  // Cleanup: unsubscribe from the store + release the lit-html
  // container. Also close the DnD overlay if it was open.
  return () => {
    if (unsubEvents) { unsubEvents(); unsubEvents = null; }
    if (unsubCount) { unsubCount(); unsubCount = null; }
    closeOverlay();
    cleanupReactive();
  };
}
