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
//      novel live WS event, then re-synced 500ms later).
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
// NOTIF-FIX (bugs A, B, D): the count is now guarded by a `dirty`
// flag that prevents the 30s poll from overwriting optimistic local
// changes (decrements after dismiss, increments after a WS event)
// until a user-initiated `refreshUnreadCount()` confirms them. The
// WS handler also deduplicates by notification id — the server
// rebroadcasts the same id for dedup-hit inserts (e.g. a flapping
// `discovery_failed` code within 24h), and without dedup the badge
// would inflate by +1 per rebroadcast even though the underlying
// row (and therefore the server's unread count) hasn't changed.
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
// `{ "count": <number> }`. (NOTIF-FIX: previously this code read
// `unread_count` which never matched the server's response field,
// so the count was never actually synced from the server — only
// optimistic WS increments accumulated, producing the inflated
// "99" badge with an empty list.) We narrow defensively via
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

/** NOTIF-FIX (bug A): dirty flag set whenever the local count is
 *  "ahead" of the server (after an optimistic increment on a WS
 *  event or an optimistic decrement on a dismiss/mark-read). While
 *  dirty, the 30s background poll skips applying its fetched count
 *  — otherwise a poll that races with an in-flight dismiss API
 *  call would clobber the optimistic decrement with the server's
 *  stale "still unread" value. The flag is cleared by the next
 *  successful user-initiated `refreshUnreadCount()` (which always
 *  applies the server's response). */
let dirty: boolean = false;

/** NOTIF-FIX (bug B): set of notification ids we've already counted
 *  via a WS event. The server rebroadcasts the same id for dedup-hit
 *  inserts (e.g. `record_system("discovery_failed", ...)` twice in
 *  24h), so without dedup the badge would inflate by +1 per
 *  rebroadcast even though the underlying row hasn't changed.
 *  Populated from WS events and from `markIdsSeen()` (called by the
 *  notifications view after its initial list fetch). Capped at
 *  `SEEN_IDS_CAP` to bound memory; when the cap is exceeded we
 *  clear the set and start fresh (the 500ms debounced refresh
 *  will re-sync the count from the server, so a brief overcount
 *  window after the clear is acceptable). */
const seenIds: Set<number> = new Set<number>();
const SEEN_IDS_CAP: number = 1000;

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
 *  defensively in case a server bug returns -1.
 *
 *  NOTIF-FIX: the `opts.optimistic` flag marks the local count as
 *  "ahead of the server" (sets the dirty flag). Pass `optimistic: true`
 *  for local changes that haven't yet been confirmed by a server
 *  fetch — e.g. an optimistic decrement after dismiss, or an
 *  optimistic increment on a novel WS event. Pass `optimistic: false`
 *  (the default) when applying a server-confirmed value (e.g. inside
 *  `refreshUnreadCount` after a successful fetch, or restoring a
 *  reverted optimistic change after an API failure). */
export function setUnreadCount(n: number, opts: { optimistic?: boolean } = {}): void {
  const next: number = Math.max(0, n | 0);
  const changed: boolean = next !== unreadCount;
  if (changed) {
    unreadCount = next;
    for (const fn of countListeners) {
      try { fn(unreadCount); } catch (e: unknown) {
        console.error("[notifications-store] count listener threw", e);
      }
    }
  }
  if (opts.optimistic && changed) {
    dirty = true;
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
 *  notifications view after a mark-as-read / archive call, by the WS
 *  event handler's debounced re-sync (500ms after each event), and by
 *  the 30s background poll.
 *
 *  NOTIF-FIX: always applies the fetched count (regardless of the
 *  `dirty` flag) and clears `dirty` on success — this is the
 *  "confirmation" half of the dirty-flag protocol. The 30s poll
 *  goes through `pollRefreshUnreadCount()` which skips when dirty.
 *  Also fixed the response field name from `unread_count` (never
 *  matched the server's `count` field) to `count`. */
export async function refreshUnreadCount(): Promise<void> {
  try {
    const raw: unknown = await api("/notifications/unread-count");
    if (raw && typeof raw === "object" && "count" in raw) {
      const n: unknown = (raw as Record<string, unknown>)["count"];
      if (typeof n === "number") {
        setUnreadCount(n);
        dirty = false;
      }
    }
  } catch (_e: unknown) {
    // Swallow — the 30s poll will try again. The badge just stays
    // at its last-known value rather than flickering to 0.
  }
}

/** NOTIF-FIX: 30s background poll. Skips the fetch entirely when
 *  `dirty` is set — the local count is ahead of the server (an
 *  optimistic increment or decrement hasn't yet been confirmed by
 *  a user-initiated refresh), so applying the server's stale value
 *  would clobber the optimistic change. The next user action (or
 *  the WS handler's 500ms debounced refresh) will clear dirty and
 *  re-enable normal polling. */
async function pollRefreshUnreadCount(): Promise<void> {
  if (dirty) return;
  await refreshUnreadCount();
}

/** Decrement the unread count locally (e.g. after the user marks a
 *  single notification as read). Clamped at 0. Marks the local
 *  count as optimistic (dirty) so the 30s poll doesn't overwrite it
 *  before the next `refreshUnreadCount()` confirms. */
export function decrementUnread(by: number = 1): void {
  setUnreadCount(unreadCount - by, { optimistic: true });
}

/** NOTIF-FIX (bug B): mark a set of notification ids as "already
 *  seen" so a subsequent WS rebroadcast for the same id (dedup-hit
 *  on the server) doesn't cause a spurious optimistic increment.
 *  Called by the notifications view after its initial list fetch.
 *  The set is capped at `SEEN_IDS_CAP`; when the cap is exceeded we
 *  clear it and start fresh. */
export function markIdsSeen(ids: Iterable<number>): void {
  for (const id of ids) {
    seenIds.add(id);
  }
  if (seenIds.size > SEEN_IDS_CAP) {
    seenIds.clear();
  }
}

// ----------------------------------------------------------------------------
// i18n helpers — shared by the store (toast body) and the view (card
// body). Keeps the per-kind payload-narrowing logic in one place.
// ----------------------------------------------------------------------------

/** Pull the per-kind body text via `t()`. Accepts either a live
 *  `NotificationEvent` or a persisted `NotificationRow` — both have
 *  `kind` + `payload`.
 *
 *  For `system` notifications (G2), dispatches on `payload.code` to
 *  pick a per-code body template (`notifications.body.{code}`). The
 *  template receives the per-payload `details` fields as
 *  interpolation params, so the server-side `details` shape is the
 *  contract for what placeholders are available. If the per-code
 *  template is missing (older i18n pack, or a brand-new code the
 *  pack hasn't been updated for), falls back to the generic
 *  `notifications.body.system` template that just echoes `message`.
 */
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
      return systemBody(p, message);
    default:
      return "";
  }
}

/** Per-code body template lookup for `system` notifications. Mirrors
 *  the per-code constants on the Rust side
 *  (`notifications::CODE_*`). Falls back to `notifications.body.system`
 *  (which just echoes `{{message}}`) when the per-code key isn't in
 *  the i18n pack — `t()` returns the key itself when missing, so we
 *  detect that case explicitly and route to the generic template
 *  rather than showing the raw key string to the user. */
function systemBody(p: Record<string, unknown>, message: string): string {
  const code: string = typeof p["code"] === "string" ? p["code"] : "";
  if (!code) {
    return t("notifications.body.system", { message });
  }
  // Pull the per-code template. `t()` returns the key itself if the
  // string isn't loaded, so we detect that fallback and route to the
  // generic system template instead.
  const perCodeKey: string = `notifications.body.${code}`;
  const details: Record<string, unknown> =
    (p["details"] && typeof p["details"] === "object" && !Array.isArray(p["details"]))
      ? p["details"] as Record<string, unknown>
      : {};
  // Interpolation params: merge top-level payload fields + `details`
  // so templates can use either `{{account_id}}` (top-level on
  // SystemPayload? no — `account_id` lives inside `details`) or
  // `{{provider_id}}` (top-level on SystemPayload). Both shapes are
  // available to the template.
  const params: Record<string, string | number> = { message };
  for (const [k, v] of Object.entries(p)) {
    if (typeof v === "string") params[k] = v;
    else if (typeof v === "number") params[k] = v;
  }
  for (const [k, v] of Object.entries(details)) {
    if (typeof v === "string") params[k] = v;
    else if (typeof v === "number") params[k] = v;
    else if (typeof v === "boolean") params[k] = v ? "true" : "false";
  }
  const rendered: string = t(perCodeKey, params);
  if (rendered === perCodeKey) {
    // Missing i18n key — fall back to the generic system body so
    // the user sees the server-provided `message` instead of the
    // raw key string.
    return t("notifications.body.system", { message });
  }
  return rendered;
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
  //
  // NOTIF-FIX (bug B): only increment the count for NOVEL ids. The
  // server rebroadcasts the same id on dedup-hit inserts (e.g. a
  // flapping `discovery_failed` code within 24h, or a `model_new`
  // for the same provider:model that re-discovery sees again). The
  // underlying row already exists in those cases, so the server's
  // unread count is unchanged — incrementing the badge for each
  // rebroadcast produced the inflated "99" badge with an empty
  // list (the view's WS handler refuses to prepend a row whose id
  // is already in the list, so the rebroadcast was invisible in
  // the list but still +1 on the badge).
  subscribeWs("notification", (msg) => {
    const data: unknown = msg.data;
    if (!data || typeof data !== "object") return;
    const evt: NotificationEvent = data as NotificationEvent;
    const isNovel: boolean = !seenIds.has(evt.id);
    if (isNovel) {
      seenIds.add(evt.id);
      if (seenIds.size > SEEN_IDS_CAP) seenIds.clear();
      // Optimistic increment so the badge reacts instantly. The
      // server is the source of truth — we re-sync 500ms later
      // (debounced) and clear the dirty flag then. The dirty flag
      // also protects this increment from being clobbered by a
      // racing 30s poll.
      setUnreadCount(unreadCount + 1, { optimistic: true });
    }
    // Fan out to listeners (sidebar, view) regardless of novelty —
    // a rebroadcast for an already-known id still carries real-time
    // signal (the event is happening again right now), so listeners
    // may want to e.g. move the row to the top of the list. Each
    // listener is responsible for its own error handling.
    for (const fn of eventListeners) {
      try { fn(evt); } catch (e: unknown) {
        console.error("[notifications-store] event listener threw", e);
      }
    }
    // Debounced re-sync. Multiple events arriving in quick succession
    // coalesce into a single server call. This is the "confirm"
    // half of the dirty-flag protocol: it always applies the server's
    // count and clears dirty.
    if (refreshDebounce !== null) clearTimeout(refreshDebounce);
    refreshDebounce = setTimeout(() => {
      refreshDebounce = null;
      void refreshUnreadCount();
    }, 500);
    // Transient toast. Suppressed during DnD so a fresh notification
    // mid-drag doesn't yank focus. We show the toast for rebroadcasts
    // too — the user-perceived event ("discovery failed again") is
    // new even if the underlying row isn't.
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
      // NOTIF-FIX: poll goes through `pollRefreshUnreadCount` which
      // skips when `dirty` is set, so an in-flight optimistic change
      // can't be clobbered by a racing poll. The next user-initiated
      // `refreshUnreadCount()` clears dirty and re-enables polling.
      try { await pollRefreshUnreadCount(); }
      finally { schedulePoll(); }
    })();
  }, 30_000);
}
