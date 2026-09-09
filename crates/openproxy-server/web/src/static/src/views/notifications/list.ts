// views/notifications/list.ts — notification list with filters, card
// rendering, and pagination. Exports the list state mutators (markAsRead,
// markAllReadOnClose, etc.) for the DnD overlay and mount entry point.

import { html, type TemplateResult, nothing } from "lit-html";
import { api } from "../../state/api.js";
import { requestUpdate } from "../../state/reactive.js";
import { showToast } from "../../components/toast.js";
import { t } from "../../i18n/index.js";
import { icons } from "../../lib/icons.js";
import {
  getUnreadCount,
  setUnreadCount,
  decrementUnread,
  refreshUnreadCount,
  markIdsSeen,
  onNotificationEvent,
  notificationBody,
  formatRelativeAgo,
} from "../../state/notifications-store.js";
import type {
  NotificationRow,
  NotificationKind,
} from "../../lib/types/notifications.js";
import {
  KIND_COLOR_VAR,
  SYSTEM_CODE_CARD_COLOR,
  SYSTEM_CODE_COLOR_VAR,
  DRAGGABLE_KINDS,
  DND_MIME,
  PAGE_LIMIT,
  isUnread,
  payloadString,
  payloadProviderId,
  payloadModelId,
  type DragPayload,
} from "./shared.js";
import {
  openOverlay,
  closeOverlay,
} from "./dnd-overlay.js";

// ==========
// Filter options
// ==========

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

// ==========
// Module-local state
// ==========

let rows: NotificationRow[] = [];
let hasMore: boolean = false;
let isLoadingMore: boolean = false;
let oldestLoadedId: number | null = null;
let loadError: string | null = null;
let filter: "all" | "unread" | NotificationKind = "all";

// ==========
// Filter matching
// ==========

function matchesFilter(r: NotificationRow): boolean {
  if (filter === "all") return true;
  if (filter === "unread") return isUnread(r);
  return r.kind === filter;
}

// ==========
// Icon + color resolution
// ==========

/** Resolve the card accent color for a notification row. */
function notificationCardColor(r: NotificationRow): string {
  if (r.kind === "system") {
    const code: string = payloadString(r.payload, "code");
    if (code && (code in SYSTEM_CODE_CARD_COLOR)) {
      return SYSTEM_CODE_CARD_COLOR[code] ?? "var(--color-text-muted, #6b7280)";
    }
  }
  return KIND_COLOR_VAR[r.kind] ?? "var(--color-text-muted, #6b7280)";
}

/** Resolve the icon glyph for a row. */
function notificationIcon(r: NotificationRow): TemplateResult {
  if (r.kind === "system") {
    const code: string = payloadString(r.payload, "code");
    switch (code) {
      case "discovery_failed":
      case "circuit_open":
      case "account_invalid":
        return icons.warning();
      case "account_key_decrypt_failed":
      case "oauth_expired":
        return icons.key();
      case "quota_low":
        return icons.caretDown();
      default:
        return icons.tag();
    }
  }
  switch (r.kind) {
    case "model_new":
      return icons.plus();
    case "model_gone":
      return icons.close();
    case "model_auto_activated":
      return icons.lightning();
    default:
      return icons.tag();
  }
}

/** Resolve the CSS color variable for a system notification's icon. */
function notificationIconColorVar(r: NotificationRow): string | null {
  if (r.kind !== "system") return null;
  const code: string = payloadString(r.payload, "code");
  if (!code) return null;
  if (code in SYSTEM_CODE_COLOR_VAR) {
    return SYSTEM_CODE_COLOR_VAR[code] ?? null;
  }
  return null;
}

/** Resolve the kind label for the card's meta row. */
function notificationKindLabel(r: NotificationRow): string {
  if (r.kind !== "system") {
    return t("notifications.kind." + r.kind);
  }
  const code: string = payloadString(r.payload, "code");
  if (!code) {
    return t("notifications.kind.system");
  }
  const perCodeKey: string = `notifications.code.${code}`;
  const rendered: string = t(perCodeKey);
  if (rendered === perCodeKey) {
    return t("notifications.kind.system");
  }
  return rendered;
}

// ==========
// API helpers
// ==========

async function fetchInitial(): Promise<void> {
  isLoadingMore = false;
  oldestLoadedId = null;
  hasMore = false;
  try {
    const raw: unknown = await api(`/notifications?limit=${PAGE_LIMIT}`);
    if (Array.isArray(raw)) {
      rows = raw as NotificationRow[];
      if (rows.length > 0 && rows[0]) {
        oldestLoadedId = rows.reduce((min, r) => Math.min(min, r.id), rows[0].id);
      }
      hasMore = rows.length >= PAGE_LIMIT;
    } else {
      rows = [];
      hasMore = false;
    }
    loadError = null;
  } catch (e: unknown) {
    loadError = e instanceof Error ? e.message : String(e);
    rows = [];
    hasMore = false;
  }
  markIdsSeen(rows.map((r) => r.id));
  requestUpdate();
}

async function loadMore(): Promise<void> {
  if (isLoadingMore || !hasMore) return;
  isLoadingMore = true;
  requestUpdate();
  try {
    const query: string = oldestLoadedId !== null
      ? `/notifications?limit=${PAGE_LIMIT}&before_id=${oldestLoadedId}`
      : `/notifications?limit=${PAGE_LIMIT}`;
    const raw: unknown = await api(query);
    if (Array.isArray(raw)) {
      const nextRows: NotificationRow[] = raw as NotificationRow[];
      if (nextRows.length > 0 && nextRows[0]) {
        const nextMinId: number = nextRows.reduce((min, r) => Math.min(min, r.id), nextRows[0].id);
        oldestLoadedId = oldestLoadedId !== null ? Math.min(oldestLoadedId, nextMinId) : nextMinId;
        const existingIds: Set<number> = new Set<number>(rows.map((r) => r.id));
        const toAdd: NotificationRow[] = nextRows.filter((r) => !existingIds.has(r.id));
        rows = [...rows, ...toAdd];
        markIdsSeen(toAdd.map((r) => r.id));
      }
      hasMore = nextRows.length >= PAGE_LIMIT;
    } else {
      hasMore = false;
    }
  } catch (_e: unknown) {
    // Retain current rows on network error
  } finally {
    isLoadingMore = false;
    requestUpdate();
  }
}

/** Mark a single notification as read. */
export async function markAsRead(id: number): Promise<void> {
  try {
    await api(`/notifications/${id}/read`, { method: "POST" });
    const r: NotificationRow | undefined = rows.find((x) => x.id === id);
    if (r && r.read_at === null) {
      r.read_at = new Date().toISOString();
      decrementUnread(1);
      requestUpdate();
    }
    void refreshUnreadCount();
  } catch (e: unknown) {
    showToast("Error: " + (e instanceof Error ? e.message : String(e)), "error");
  }
}

async function markAllRead(): Promise<void> {
  try {
    await api("/notifications/read-all", { method: "POST" });
    const nowIso: string = new Date().toISOString();
    for (const r of rows) {
      if (r.read_at === null) r.read_at = nowIso;
    }
    setUnreadCount(0);
    requestUpdate();
    void refreshUnreadCount();
  } catch (e: unknown) {
    showToast("Error: " + (e instanceof Error ? e.message : String(e)), "error");
  }
}

async function archiveAll(): Promise<void> {
  const snapshot: NotificationRow[] = rows;
  const unreadBefore: number = getUnreadCount();
  rows = [];
  setUnreadCount(0);
  requestUpdate();
  try {
    await api("/notifications/archive-all", { method: "POST" });
    void refreshUnreadCount();
  } catch (e: unknown) {
    rows = snapshot;
    setUnreadCount(unreadBefore);
    requestUpdate();
    void refreshUnreadCount();
    showToast("Error: " + (e instanceof Error ? e.message : String(e)), "error");
  }
}

async function archive(id: number): Promise<void> {
  const snapshot: NotificationRow[] = rows;
  const wasUnread: boolean = rows.find((x) => x.id === id)?.read_at === null;
  rows = rows.filter((x) => x.id !== id);
  if (wasUnread) decrementUnread(1);
  requestUpdate();

  if (hasMore && !isLoadingMore && (rows.length < 10 || (filter !== "all" && rows.filter(matchesFilter).length === 0))) {
    void loadMore();
  }

  try {
    await api(`/notifications/${id}/archive`, { method: "POST" });
    void refreshUnreadCount();
  } catch (e: unknown) {
    rows = snapshot;
    if (wasUnread) {
      setUnreadCount(getUnreadCount() + 1);
    }
    requestUpdate();
    void refreshUnreadCount();
    showToast("Error: " + (e instanceof Error ? e.message : String(e)), "error");
  }
}

// ==========
// Action handlers — view
// ==========

async function onMarkAllRead(): Promise<void> {
  await markAllRead();
}

async function onClearAll(): Promise<void> {
  await archiveAll();
}

function onFilterChange(e: Event): void {
  const sel: HTMLSelectElement = e.target as HTMLSelectElement;
  const v: string = sel.value;
  if (v === "all" || v === "unread" || v === "model_new" || v === "model_gone" || v === "model_auto_activated" || v === "system") {
    filter = v;
    requestUpdate();
    if (hasMore && !isLoadingMore && rows.filter(matchesFilter).length === 0) {
      void loadMore();
    }
  }
}

async function onViewProvider(r: NotificationRow): Promise<void> {
  const providerId: string = payloadProviderId(r);
  if (!providerId) {
    showToast("Provider not found in notification payload", "error");
    return;
  }
  void markAsRead(r.id);
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
    true,
  );
}

async function onDismiss(r: NotificationRow): Promise<void> {
  await archive(r.id);
}

// ==========
// Card rendering
// ==========

function renderCard(r: NotificationRow): TemplateResult {
  const icon: TemplateResult = notificationIcon(r);
  const iconColorVar: string | null = notificationIconColorVar(r);
  const cardColor: string = notificationCardColor(r);
  const body: string = notificationBody(r);
  const ago: string = formatRelativeAgo(r.created_at);
  const unread: boolean = isUnread(r);
  const draggable: boolean = DRAGGABLE_KINDS.has(r.kind) && !!payloadModelId(r) && !!payloadProviderId(r);
  const showAddToCombo: boolean = DRAGGABLE_KINDS.has(r.kind);
  const cardClasses: string = "notification-card" + (unread ? " unread" : "") + (draggable ? " draggable" : "");
  const cardStyle: string = `--card-accent: ${cardColor};${iconColorVar ? ` --icon-color: ${iconColorVar};` : ""}`;
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
        if (e.dataTransfer) {
          e.dataTransfer.setData(DND_MIME, JSON.stringify(payload));
          e.dataTransfer.setData("text/plain", modelId);
          e.dataTransfer.effectAllowed = "copy";
        }
        openOverlay(payload, false);
      }
    : null;
  const dragEndHandler: ((e: DragEvent) => void) | null = draggable
    ? (_e: DragEvent) => {
        closeOverlay();
      }
    : null;
  return html`<div class=${cardClasses} data-id=${String(r.id)}
      style=${cardStyle}
      draggable=${draggable ? "true" : "false"}
      @dragstart=${dragStartHandler}
      @dragend=${dragEndHandler}
    >
    <div class="notification-card-icon" style=${cardStyle} aria-hidden="true">${icon}</div>
    <div class="notification-card-body">
      <div class="notification-card-text">${body}</div>
      <div class="notification-card-meta">
        <span class="notification-card-kind">${notificationKindLabel(r)}</span>
        ${ago ? html`<span class="notification-card-ago">${ago}</span>` : nothing}
        ${unread ? html`<span class="notification-card-unread-dot" title=${t("common.unread")}></span>` : nothing}
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
    </div>
  </div>`;
}

// ==========
// View rendering
// ==========

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
      <button class="small danger" ?disabled=${rows.length === 0} @click=${() => { void onClearAll(); }}>${t("notifications.clear_all")}</button>
    </div>
  </div>`;
}

function renderLoadMoreButton(): TemplateResult {
  return html`<button class="small" ?disabled=${isLoadingMore} @click=${() => { void loadMore(); }}>
    ${isLoadingMore ? t("common.loading") : t("notifications.load_more")}
  </button>`;
}

function renderList(): TemplateResult {
  if (loadError) {
    return html`<div class="banner banner-error">${loadError}</div>`;
  }
  const filtered: NotificationRow[] = rows.filter(matchesFilter);
  if (rows.length === 0) {
    if (isLoadingMore) {
      return html`<div class="notification-empty">
        <p>${t("common.loading")}</p>
      </div>`;
    }
    return html`<div class="notification-empty">
      <div class="notification-empty-icon" aria-hidden="true">🔔</div>
      <p>${t("notifications.no_notifications")}</p>
    </div>`;
  }
  const anyUnread: boolean = rows.some(isUnread);
  const noUnreadHint: TemplateResult | typeof nothing = (!anyUnread && filter !== "all")
    ? html`<div class="notification-no-unread-hint">${t("notifications.no_unread")}</div>`
    : nothing;
  if (filtered.length === 0) {
    return html`${noUnreadHint}<div class="notification-empty">
      <div class="notification-empty-icon" aria-hidden="true">${icons.search()}</div>
      <p>${t("common.empty")}</p>
      ${hasMore ? renderLoadMoreButton() : nothing}
    </div>`;
  }
  return html`${noUnreadHint}<div class="notification-list">${filtered.map(renderCard)}</div>${hasMore ? html`
    <div style="display:flex;justify-content:center;margin:var(--space-4, 1rem) 0;">
      ${renderLoadMoreButton()}
    </div>
  ` : nothing}`;
}

function renderView(): TemplateResult {
  return html`${renderHeader()}${renderList()}`;
}

// ==========
// State reset (called by index.ts on mount)
// ==========

export function resetListState(): void {
  rows = [];
  loadError = null;
  filter = "all";
  hasMore = false;
  isLoadingMore = false;
  oldestLoadedId = null;
}

/** Check if there are unread notifications. Used by mount cleanup. */
export function hasUnread(): boolean {
  return rows.some((r) => r.read_at === null && r.archived_at === null);
}

/** Subscribe to live WS notification events. The store fans out the
 *  parsed `NotificationEvent` to every subscriber. We prepend the
 *  new row to the local list (the store already incremented the
 *  unread count + showed a toast). */
export function subscribeNotificationEvents(): () => void {
  return onNotificationEvent((evt: import("../../lib/types/notifications.js").NotificationEvent) => {
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
    if (!rows.some((r) => r.id === row.id)) {
      rows = [row, ...rows];
      requestUpdate();
    }
  });
}

// ==========
// Fetch initial + renderView export
// ==========

export { fetchInitial, renderView };

/** NOTIF-FIX (task 4): best-effort `mark_all_read` fired from the
 *  view's cleanup path. Silent on error; skips local-row mutation. */
export async function markAllReadOnClose(): Promise<void> {
  try {
    await api("/notifications/read-all", { method: "POST" });
    setUnreadCount(0);
    void refreshUnreadCount();
  } catch (_e: unknown) {
    // Swallow — best-effort sync.
  }
}
