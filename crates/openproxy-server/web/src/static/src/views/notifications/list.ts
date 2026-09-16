import { html, type TemplateResult, nothing } from "lit-html";
import { api } from "../../state/api.js";
import { requestUpdate } from "../../state/reactive.js";
import { showToast } from "../../components/toast.js";
import { t } from "../../i18n/index.js";
import { icons } from "../../lib/icons.js";
import {
  getUnreadCount, setUnreadCount, decrementUnread, refreshUnreadCount,
  markIdsSeen, onNotificationEvent, notificationBody, formatRelativeAgo,
} from "../../state/notifications-store.js";
import type { NotificationRow, NotificationKind } from "../../lib/types/notifications.js";
import {
  KIND_COLOR_VAR, SYSTEM_CODE_CARD_COLOR, SYSTEM_CODE_COLOR_VAR,
  DRAGGABLE_KINDS, DND_MIME, PAGE_LIMIT, isUnread, payloadString,
  payloadProviderId, payloadModelId, type DragPayload,
} from "./shared.js";
import { openOverlay, closeOverlay } from "./dnd-overlay.js";

const FILTER_OPTIONS: ReadonlyArray<{ value: "all" | "unread" | NotificationKind; key: string }> = [
  { value: "all", key: "notifications.filter.all" },
  { value: "unread", key: "notifications.filter.unread" },
  { value: "model_new", key: "notifications.filter.model_new" },
  { value: "model_gone", key: "notifications.filter.model_gone" },
  { value: "model_auto_activated", key: "notifications.filter.model_auto_activated" },
  { value: "system", key: "notifications.filter.system" },
];

let rows: NotificationRow[] = [];
let hasMore = false;
let isLoadingMore = false;
let oldestLoadedId: number | null = null;
let loadError: string | null = null;
let filter: "all" | "unread" | NotificationKind = "all";

function matchesFilter(r: NotificationRow): boolean {
  if (filter === "all") return true;
  return filter === "unread" ? isUnread(r) : r.kind === filter;
}

function notificationCardColor(r: NotificationRow): string {
  if (r.kind === "system") {
    const code = payloadString(r.payload, "code");
    if (code && code in SYSTEM_CODE_CARD_COLOR) return SYSTEM_CODE_CARD_COLOR[code] ?? "var(--color-text-muted, #6b7280)";
  }
  return KIND_COLOR_VAR[r.kind] ?? "var(--color-text-muted, #6b7280)";
}

function notificationIcon(r: NotificationRow): TemplateResult {
  if (r.kind === "system") {
    const code = payloadString(r.payload, "code");
    if (["discovery_failed", "circuit_open", "account_invalid"].includes(code)) return icons.warning();
    if (["account_key_decrypt_failed", "oauth_expired"].includes(code)) return icons.key();
    return code === "quota_low" ? icons.caretDown() : icons.tag();
  }
  if (r.kind === "model_new") return icons.plus();
  if (r.kind === "model_gone") return icons.close();
  return r.kind === "model_auto_activated" ? icons.lightning() : icons.tag();
}

function notificationIconColorVar(r: NotificationRow): string | null {
  if (r.kind !== "system") return null;
  const code = payloadString(r.payload, "code");
  return code && code in SYSTEM_CODE_COLOR_VAR ? (SYSTEM_CODE_COLOR_VAR[code] ?? null) : null;
}

function notificationKindLabel(r: NotificationRow): string {
  if (r.kind !== "system") return t("notifications.kind." + r.kind);
  const code = payloadString(r.payload, "code");
  if (!code) return t("notifications.kind.system");
  const key = `notifications.code.${code}`;
  const rendered = t(key);
  return rendered === key ? t("notifications.kind.system") : rendered;
}

async function fetchInitial(): Promise<void> {
  isLoadingMore = false; oldestLoadedId = null; hasMore = false;
  try {
    const raw = await api(`/notifications?limit=${PAGE_LIMIT}`);
    if (Array.isArray(raw)) {
      rows = raw as NotificationRow[];
      if (rows.length > 0 && rows[0]) oldestLoadedId = rows.reduce((min, r) => Math.min(min, r.id), rows[0].id);
      hasMore = rows.length >= PAGE_LIMIT;
    } else { rows = []; hasMore = false; }
    loadError = null;
  } catch (e: unknown) {
    loadError = e instanceof Error ? e.message : String(e); rows = []; hasMore = false;
  }
  markIdsSeen(rows.map((r) => r.id));
  requestUpdate();
}

async function loadMore(): Promise<void> {
  if (isLoadingMore || !hasMore) return;
  isLoadingMore = true; requestUpdate();
  try {
    const q = oldestLoadedId !== null ? `/notifications?limit=${PAGE_LIMIT}&before_id=${oldestLoadedId}` : `/notifications?limit=${PAGE_LIMIT}`;
    const raw = await api(q);
    if (Array.isArray(raw)) {
      const next = raw as NotificationRow[];
      if (next.length > 0 && next[0]) {
        const nextMin = next.reduce((min, r) => Math.min(min, r.id), next[0].id);
        oldestLoadedId = oldestLoadedId !== null ? Math.min(oldestLoadedId, nextMin) : nextMin;
        const ids = new Set(rows.map((r) => r.id));
        const toAdd = next.filter((r) => !ids.has(r.id));
        rows = [...rows, ...toAdd];
        markIdsSeen(toAdd.map((r) => r.id));
      }
      hasMore = next.length >= PAGE_LIMIT;
    } else { hasMore = false; }
  } catch { /* retain */ } finally { isLoadingMore = false; requestUpdate(); }
}

export async function markAsRead(id: number): Promise<void> {
  try {
    await api(`/notifications/${id}/read`, { method: "POST" });
    const r = rows.find((x) => x.id === id);
    if (r && r.read_at === null) { r.read_at = new Date().toISOString(); decrementUnread(1); requestUpdate(); }
    void refreshUnreadCount();
  } catch (e: unknown) { showToast("Error: " + (e instanceof Error ? e.message : String(e)), "error"); }
}

async function markAllRead(): Promise<void> {
  try {
    await api("/notifications/read-all", { method: "POST" });
    const now = new Date().toISOString();
    for (const r of rows) { if (r.read_at === null) r.read_at = now; }
    setUnreadCount(0); requestUpdate(); void refreshUnreadCount();
  } catch (e: unknown) { showToast("Error: " + (e instanceof Error ? e.message : String(e)), "error"); }
}

async function archiveAll(): Promise<void> {
  const snapshot = rows; const unreadBefore = getUnreadCount();
  rows = []; setUnreadCount(0); requestUpdate();
  try { await api("/notifications/archive-all", { method: "POST" }); void refreshUnreadCount(); }
  catch (e: unknown) { rows = snapshot; setUnreadCount(unreadBefore); requestUpdate(); void refreshUnreadCount(); showToast("Error: " + (e instanceof Error ? e.message : String(e)), "error"); }
}

async function archive(id: number): Promise<void> {
  const snapshot = rows; const wasUnread = rows.find((x) => x.id === id)?.read_at === null;
  rows = rows.filter((x) => x.id !== id);
  if (wasUnread) decrementUnread(1);
  requestUpdate();
  if (hasMore && !isLoadingMore && (rows.length < 10 || (filter !== "all" && rows.filter(matchesFilter).length === 0))) void loadMore();
  try { await api(`/notifications/${id}/archive`, { method: "POST" }); void refreshUnreadCount(); }
  catch (e: unknown) { rows = snapshot; if (wasUnread) setUnreadCount(getUnreadCount() + 1); requestUpdate(); void refreshUnreadCount(); showToast("Error: " + (e instanceof Error ? e.message : String(e)), "error"); }
}

let pendingDndOpen: ReturnType<typeof setTimeout> | null = null;
function scheduleDndOverlay(payload: DragPayload): void {
  if (pendingDndOpen) clearTimeout(pendingDndOpen);
  pendingDndOpen = setTimeout(() => { pendingDndOpen = null; openOverlay(payload, false); }, 0);
}
function cancelPendingDndOverlay(): void {
  if (pendingDndOpen) { clearTimeout(pendingDndOpen); pendingDndOpen = null; }
}

export function isRowDraggable(r: NotificationRow): boolean {
  return DRAGGABLE_KINDS.has(r.kind) && !!payloadModelId(r) && !!payloadProviderId(r);
}

function isDeletable(r: NotificationRow): boolean {
  return r.kind !== "model_new" && r.kind !== "model_gone" && r.kind !== "model_auto_activated";
}

async function onDelete(r: NotificationRow): Promise<void> {
  if (!isDeletable(r)) return;
  const snapshot = rows; const wasUnread = rows.find((x) => x.id === r.id)?.read_at === null;
  rows = rows.filter((x) => x.id !== r.id);
  if (wasUnread) decrementUnread(1);
  requestUpdate();
  if (hasMore && !isLoadingMore && (rows.length < 10 || (filter !== "all" && rows.filter(matchesFilter).length === 0))) void loadMore();
  try { await api(`/notifications/${r.id}`, { method: "DELETE" }); void refreshUnreadCount(); }
  catch { rows = snapshot; if (wasUnread) setUnreadCount(getUnreadCount() + 1); requestUpdate(); void refreshUnreadCount(); showToast(t("notifications.error.delete_failed"), "error"); }
}

function renderRow(r: NotificationRow): TemplateResult {
  const icon = notificationIcon(r);
  const iconColorVar = notificationIconColorVar(r);
  const cardColor = notificationCardColor(r);
  const body = notificationBody(r);
  const ago = formatRelativeAgo(r.created_at);
  const unread = isUnread(r);
  const draggable = isRowDraggable(r);
  const deletable = isDeletable(r);
  const rowClasses = "notification-card" + (unread ? " unread" : "") + (draggable ? " draggable" : "");
  const rowStyle = `--card-accent: ${cardColor};${iconColorVar ? ` --icon-color: ${iconColorVar};` : ""}`;
  const pId = payloadProviderId(r);
  const mId = payloadModelId(r);

  const dragStart = draggable ? (e: DragEvent) => {
    if (!pId || !mId) return;
    const payload: DragPayload = { notification_id: r.id, provider_id: pId, model_id: mId };
    if (e.dataTransfer) { e.dataTransfer.setData(DND_MIME, JSON.stringify(payload)); e.dataTransfer.setData("text/plain", mId); e.dataTransfer.effectAllowed = "copy"; }
    scheduleDndOverlay(payload);
  } : null;

  return html`<div class=${rowClasses} data-id=${String(r.id)} style=${rowStyle} draggable=${draggable ? "true" : "false"}
      @dragstart=${dragStart} @dragend=${draggable ? () => { cancelPendingDndOverlay(); closeOverlay(); } : null}>
    <div class="notification-card-icon" style=${rowStyle} aria-hidden="true">${icon}</div>
    <div class="notification-card-body">
      <div class="notification-card-text">${body}</div>
      <div class="notification-card-meta">
        <span class="notification-card-kind">${notificationKindLabel(r)}</span>
        ${ago ? html`<span class="notification-card-ago">${ago}</span>` : nothing}
        ${unread ? html`<span class="notification-card-unread-dot" title=${t("common.unread")} aria-label=${t("common.unread")}></span>` : nothing}
      </div>
      <div class="notification-card-actions">
        ${pId ? html`<button class="small" @click=${() => { void markAsRead(r.id); location.hash = "#/providers/" + encodeURIComponent(pId); }}>${t("notifications.action.view_provider")}</button>` : nothing}
        ${DRAGGABLE_KINDS.has(r.kind) && pId && mId ? html`<button class="small" @click=${() => openOverlay({ notification_id: r.id, provider_id: pId, model_id: mId }, true)}>${t("notifications.action.add_to_combo")}</button>` : nothing}
        ${unread ? html`<button class="small" @click=${() => { void markAsRead(r.id); }}>${t("notifications.action.mark_read")}</button>` : nothing}
        <button class="small danger" @click=${() => { void archive(r.id); }}>${t("notifications.action.dismiss")}</button>
        ${deletable ? html`<button class="small danger" @click=${() => { void onDelete(r); }}>${t("notifications.action.delete")}</button>` : nothing}
      </div>
    </div>
  </div>`;
}

function renderHeader(): TemplateResult {
  const unread = getUnreadCount();
  return html`<div class="page-header">
    <div class="page-header-title">
      <h2>${t("notifications.title")}</h2>
      <span class="badge ${unread > 0 ? "badge-error" : "badge-info"}">${unread > 0 ? t("notifications.unread_count", { count: unread }) : t("notifications.no_unread")}</span>
    </div>
    <div class="actions">
      <select class="notification-filter" @change=${(e: Event) => { filter = (e.target as HTMLSelectElement).value as typeof filter; requestUpdate(); if (hasMore && !isLoadingMore && rows.filter(matchesFilter).length === 0) void loadMore(); }}>
        ${FILTER_OPTIONS.map((o) => html`<option value=${o.value} ?selected=${o.value === filter}>${t(o.key)}</option>`)}
      </select>
      <button class="small" ?disabled=${unread === 0} @click=${() => { void markAllRead(); }}>${t("notifications.mark_all_read")}</button>
      <button class="small danger" ?disabled=${rows.length === 0} @click=${() => { void archiveAll(); }}>${t("notifications.clear_all")}</button>
    </div>
  </div>`;
}

function renderList(): TemplateResult {
  if (loadError) return html`<div class="banner banner-error">${loadError}</div>`;
  const filtered = rows.filter(matchesFilter);
  if (rows.length === 0) {
    return html`<div class="notification-empty"><div class="notification-empty-icon" aria-hidden="true">${isLoadingMore ? "" : "🔔"}</div><p>${isLoadingMore ? t("common.loading") : t("notifications.no_notifications")}</p></div>`;
  }
  const anyUnread = rows.some(isUnread);
  const noUnread = (!anyUnread && filter !== "all") ? html`<div class="notification-no-unread-hint">${t("notifications.no_unread")}</div>` : nothing;
  if (filtered.length === 0) {
    return html`${noUnread}<div class="notification-empty"><div class="notification-empty-icon" aria-hidden="true">${icons.search()}</div><p>${t("common.empty")}</p>${hasMore ? html`<button class="small" ?disabled=${isLoadingMore} @click=${() => { void loadMore(); }}>${isLoadingMore ? t("common.loading") : t("notifications.load_more")}</button>` : nothing}</div>`;
  }
  return html`${noUnread}<div class="notification-list">${filtered.map(renderRow)}</div>${hasMore ? html`<div style="display:flex;justify-content:center;margin:var(--space-4, 1rem) 0;"><button class="small" ?disabled=${isLoadingMore} @click=${() => { void loadMore(); }}>${isLoadingMore ? t("common.loading") : t("notifications.load_more")}</button></div>` : nothing}`;
}

export function renderView(): TemplateResult { return html`${renderHeader()}${renderList()}`; }
export function resetListState(): void { rows = []; loadError = null; filter = "all"; hasMore = false; isLoadingMore = false; oldestLoadedId = null; }
export function hasUnread(): boolean { return rows.some((r) => r.read_at === null && r.archived_at === null); }

export function subscribeNotificationEvents(): () => void {
  return onNotificationEvent((evt: import("../../lib/types/notifications.js").NotificationEvent) => {
    const row: NotificationRow = { id: evt.id, kind: evt.kind, payload: evt.payload, read_at: null, archived_at: null, created_at: evt.created_at, dedup_key: null, provider_id: null };
    if (!rows.some((r) => r.id === row.id)) { rows = [row, ...rows]; requestUpdate(); }
  });
}

export { fetchInitial };

export async function markAllReadOnClose(): Promise<void> {
  try { await api("/notifications/read-all", { method: "POST" }); setUnreadCount(0); void refreshUnreadCount(); } catch { /* swallow */ }
}
