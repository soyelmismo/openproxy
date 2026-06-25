// state/notifications-store.ts — module-local store for the unread
// notifications count + a fan-out bus for `notification` WS events.
//
// F4 introduced this store because both the sidebar badge (F4.2) and
// the notifications view header (F4.1) need to know the unread count,
// and multiple consumers (sidebar, view, DnD overlay) want to react
// to live `notification` WS events without each subscribing to ws-bus
// independently and risking duplicate toasts.
//
// Responsibilities:
//   1. Hold the authoritative unread count (server-fetched on init
//      and on every 30s tick; incremented optimistically on each
//      live WS event, then re-synced 500ms later).
//   2. Subscribe to ws-bus `'notification'` events once at boot and
//      fan them out to registered listeners (sidebar, view).
//   3. Open the live-logs WebSocket at boot so `notification` events
//      arrive even when no view that owns the WS is mounted. The WS
//      is shared with `views/logs.ts` and `state/live-store.ts` —
//      `connectLogsWebSocket()` is idempotent, so re-opening on top
//      of an existing connection is a no-op.
//   4. Show a transient toast per live notification (unless a drag
//      is in progress — the `suppressToasts` flag is toggled by the
//      DnD overlay so a fresh notification mid-drag doesn't yank
//      focus).
//
// The store is process-global and never tears down. The 30s poll
// handles the case where the WS is closed (e.g. the user navigated
// away from the logs view, which calls `disconnectLogsWebSocket()`)
// — the badge still updates, just at 30s granularity instead of
// real-time. A 5s keepalive re-opens the WS if it has been closed
// so the sidebar can resume real-time delivery.

import { api } from "./api.js";
import { subscribeWs } from "./ws-bus.js";
import { connectLogsWebSocket } from "./ws.js";
import { state } from "./index.js";
import { showToast } from "../components/toast.js";
import { t } from "../i18n/index.js";
import type {
  NotificationEvent,
  NotificationRow,
  NotificationKind,
} from "../lib/types/notifications.js";

// ----------------------------------------------------------------------------
// Types
// ----------------------------------------------------------------------------

// `GET /admin/api/notifications/unread-count` returns
// `{ "unread_count": <number> }`. We narrow defensively via
// `Record<string, unknown>` rather than a dedicated interface — the
// server contract is small enough that an inline narrowing is
// clearer than a one-field type alias.

type CountListener = (count: number) => void;
type EventListener = (evt: NotificationEvent) => void;

// ----------------------------------------------------------------------------
// Module-local state
// ----------------------------------------------------------------------------

let unreadCount: number = 0;
let initialized: boolean = false;
let suppressToasts: boolean = false;

/** 30s poll handle for `GET /notifications/unread-count`. Cleared on
 *  every tick and rescheduled inside the tick's `finally` so a slow
 *  request can't stack up two concurrent ticks. */
let pollHandle: ReturnType<typeof setTimeout> | null = null;

/** Debounce timer for the post-WS-event `refreshUnreadCount()` call.
 *  Multiple events arriving in quick succession coalesce into a
 *  single network call. */
let refreshDebounce: ReturnType<typeof setTimeout> | null = null;

const countListeners: Set<CountListener> = new Set();
const eventListeners: Set<EventListener> = new Set();

// ----------------------------------------------------------------------------
// Public API
// ----------------------------------------------------------------------------

/** Current unread count. Reads are cheap (no allocation). */
export function getUnreadCount(): number {
  return unreadCount;
}

/** Replace the unread count and notify every subscriber (sidebar,
 *  view header). Callers should pass a non-negative number; we clamp
 *  defensively in case a server bug returns -1. */
export function setUnreadCount(n: number): void {
  const next: number = Math.max(0, n | 0);
  if (next === unreadCount) return;
  unreadCount = next;
  for (const fn of countListeners) {
    try { fn(unreadCount); } catch (e: unknown) {
      console.error("[notifications-store] count listener threw", e);
    }
  }
}

/** Subscribe to unread-count changes. Returns an unsubscribe fn. */
export function onUnreadCountChange(fn: CountListener): () => void {
  countListeners.add(fn);
  return () => { countListeners.delete(fn); };
}

/** Subscribe to live `notification` WS events. Returns an unsubscribe
 *  fn. Listeners receive the parsed `NotificationEvent` (already
 *  narrowed by the ws-bus dispatcher). */
export function onNotificationEvent(fn: EventListener): () => void {
  eventListeners.add(fn);
  return () => { eventListeners.delete(fn); };
}

/** Toggle whether live notifications surface a toast. Used by the DnD
 *  overlay so a fresh notification mid-drag doesn't yank focus. */
export function setSuppressToasts(b: boolean): void {
  suppressToasts = b;
}

/** Force a refetch of the unread count from the server. Used by the
 *  notifications view after a mark-as-read / archive call. */
export async function refreshUnreadCount(): Promise<void> {
  try {
    const raw: unknown = await api("/notifications/unread-count");
    if (raw && typeof raw === "object" && "unread_count" in raw) {
      const n: unknown = (raw as Record<string, unknown>)["unread_count"];
      if (typeof n === "number") setUnreadCount(n);
    }
  } catch (_e: unknown) {
    // Swallow — the 30s poll will try again. The badge just stays
    // at its last-known value rather than flickering to 0.
  }
}

/** Decrement the unread count locally (e.g. after the user marks a
 *  single notification as read). Clamped at 0. */
export function decrementUnread(by: number = 1): void {
  setUnreadCount(unreadCount - by);
}

// ----------------------------------------------------------------------------
// i18n helpers — shared by the store (toast body) and the view (card
// body). Keeps the per-kind payload-narrowing logic in one place.
// ----------------------------------------------------------------------------

/** Pull the per-kind body text via `t()`. Accepts either a live
 *  `NotificationEvent` or a persisted `NotificationRow` — both have
 *  `kind` + `payload`. */
export function notificationBody(evt: NotificationEvent | NotificationRow): string {
  const p: Record<string, unknown> = evt.payload || {};
  const modelId: string = typeof p["model_id"] === "string" ? p["model_id"] : "";
  const providerId: string = typeof p["provider_id"] === "string" ? p["provider_id"] : "";
  const keyword: string = typeof p["matched_keyword"] === "string" ? p["matched_keyword"] : "";
  const message: string = typeof p["message"] === "string" ? p["message"] : "";
  switch (evt.kind) {
    case "model_new":
      return t("notifications.body.model_new", { model_id: modelId, provider_id: providerId });
    case "model_gone":
      return t("notifications.body.model_gone", { model_id: modelId, provider_id: providerId });
    case "model_auto_activated":
      // The "matched {{keyword}}" variant only fires when the
      // provider had an `auto_activate_keyword` configured. A null
      // keyword means "all new models auto-activate" — that gets the
      // shorter "_no_keyword" template.
      return keyword
        ? t("notifications.body.model_auto_activated", { model_id: modelId, provider_id: providerId, keyword })
        : t("notifications.body.model_auto_activated_no_keyword", { model_id: modelId, provider_id: providerId });
    case "system":
      return t("notifications.body.system", { message });
    default:
      return "";
  }
}

/** Format a `created_at` RFC-3339 timestamp as a relative "X ago"
 *  string via the i18n pluralised keys. Returns "just now" for
 *  anything within the last minute. */
export function formatRelativeAgo(iso: string, nowMs: number = Date.now()): string {
  let createdMs: number;
  try {
    createdMs = Date.parse(iso);
    if (!Number.isFinite(createdMs)) return "";
  } catch (_e: unknown) {
    return "";
  }
  const deltaSec: number = Math.max(0, Math.floor((nowMs - createdMs) / 1000));
  if (deltaSec < 60) return t("notifications.ago.just_now");
  const deltaMin: number = Math.floor(deltaSec / 60);
  if (deltaMin < 60) {
    return t("notifications.ago.minutes", { count: deltaMin });
  }
  const deltaHr: number = Math.floor(deltaMin / 60);
  if (deltaHr < 24) {
    return t("notifications.ago.hours", { count: deltaHr });
  }
  const deltaDay: number = Math.floor(deltaHr / 24);
  return t("notifications.ago.days", { count: deltaDay });
}

// ----------------------------------------------------------------------------
// Boot + lifecycle
// ----------------------------------------------------------------------------

/** Initialise the store at app boot. Idempotent — safe to call more
 *  than once. Opens the WS, subscribes to ws-bus, starts the 30s
 *  poll, and primes the unread count from the server. */
export function initNotificationsStore(): void {
  if (initialized) return;
  initialized = true;

  // Open the live-logs WS at boot so `notification` events arrive
  // even when no view that owns the WS is mounted. `connectLogsWebSocket`
  // is idempotent — a later call from `views/logs.ts` is a no-op.
  connectLogsWebSocket();

  // Keepalive: re-open the WS if anything closed it. The interval is
  // generous (5s) so we don't fight with the logs view's own reconnect
  // cadence — the logs view schedules a reconnect with 1–30s backoff,
  // and our keepalive acts as a safety net once that backoff window
  // has elapsed. The handle is intentionally not stored — the store
  // is process-global, so we never cancel the interval.
  void setInterval(() => {
    const ws: WebSocket | null = state.logs.ws;
    if (!ws || ws.readyState === WebSocket.CLOSED || ws.readyState === WebSocket.CLOSING) {
      try { connectLogsWebSocket(); } catch (_e: unknown) { /* swallow — next tick */ }
    }
  }, 5000);

  // Subscribe to ws-bus `notification` events. The bus is independent
  // of the WS connection state — when the WS is closed, no events
  // arrive and the 30s poll is the only source of truth.
  subscribeWs("notification", (msg) => {
    const data: unknown = msg.data;
    if (!data || typeof data !== "object") return;
    const evt: NotificationEvent = data as NotificationEvent;
    // Optimistic increment so the badge reacts instantly. The server
    // is the source of truth — we re-sync 500ms later (debounced).
    setUnreadCount(unreadCount + 1);
    // Fan out to listeners (sidebar, view). Each listener is
    // responsible for its own error handling.
    for (const fn of eventListeners) {
      try { fn(evt); } catch (e: unknown) {
        console.error("[notifications-store] event listener threw", e);
      }
    }
    // Debounced re-sync. Multiple events arriving in quick succession
    // coalesce into a single server call.
    if (refreshDebounce !== null) clearTimeout(refreshDebounce);
    refreshDebounce = setTimeout(() => {
      refreshDebounce = null;
      void refreshUnreadCount();
    }, 500);
    // Transient toast. Suppressed during DnD so a fresh notification
    // mid-drag doesn't yank focus.
    if (!suppressToasts) {
      const title: string = t("notifications.kind." + (evt.kind as NotificationKind));
      const body: string = notificationBody(evt);
      const text: string = body ? (title + " — " + body) : title;
      showToast(text, "info");
    }
  });

  // Prime the count + start the 30s poll.
  void refreshUnreadCount();
  schedulePoll();
}

/** Schedule the next 30s poll tick. We use `setTimeout` (not
 *  `setInterval`) so a slow request can't stack up two concurrent
 *  ticks — the next tick is scheduled inside the previous tick's
 *  `finally` AFTER the await resolves. */
function schedulePoll(): void {
  if (pollHandle !== null) return;
  pollHandle = setTimeout(() => {
    pollHandle = null;
    void (async () => {
      try { await refreshUnreadCount(); }
      finally { schedulePoll(); }
    })();
  }, 30_000);
}
