// state/notifications-store.ts — module-local store for the unread notifications count plus a
// fan-out bus for `notification` WS events. Shared by the sidebar badge and the notifications
// view header so multiple consumers react to live events without each subscribing to ws-bus
// (which would risk duplicate toasts).
//
// Responsibilities:
//   1. Authoritative unread count: server-fetched on init and every 30s tick, incremented
//      optimistically per novel WS event, re-synced 500ms later.
//   2. One ws-bus `'notification'` subscription at boot, fanned out to listeners.
//   3. Open the live-logs WebSocket at boot so events arrive even with no WS-owning view
//      mounted. Shared with views/logs.ts and state/live-store.ts; `connectLogsWebSocket()`
//      is idempotent, so re-opening over an existing connection is a no-op.
//   4. One transient toast per live notification, suppressed during a drag (the
//      `suppressToasts` flag, toggled by the DnD overlay, so a mid-drag notification
//      doesn't yank focus).
//
// NOTIF-FIX (bugs A, B, D): a `dirty` flag stops the 30s poll from overwriting optimistic local
// changes (decrement after dismiss, increment after a WS event) until a user-initiated
// `refreshUnreadCount()` confirms them. The WS handler also dedupes by notification id: the
// server rebroadcasts the same id for dedup-hit inserts (e.g. a flapping `discovery_failed`
// within 24h), and without dedup the badge inflates by +1 per rebroadcast even though the
// underlying row — and the server's count — hasn't changed.
//
// The store is process-global and never tears down. The 30s poll covers a closed WS (e.g. the
// user left the logs view, which calls `disconnectLogsWebSocket()`) at 30s granularity instead
// of real time; a 5s keepalive re-opens the WS so the sidebar resumes real-time delivery.

import { api } from "./api.js";
import { subscribeWs } from "./ws-bus.js";
import { connectLogsWebSocket } from "./ws.js";
import { state } from "./index.js";
import { isLoggedIn } from "./auth.js";
import { showToast } from "../components/toast.js";
import { t } from "../i18n/index.js";
import {
  createVisibilityAwareInterval,
  type VisibilityAwareHandle,
} from "../lib/visibility-aware-interval.js";
import type {
  NotificationEvent,
  NotificationRow,
  NotificationKind,
} from "../lib/types/notifications.js";

// ── Types ──────────────────────────────────────────────────────────────────

// `GET /admin/api/notifications/unread-count` returns `{ "count": <number> }`. (NOTIF-FIX: this
// code previously read `unread_count`, which never matched the server's field, so the count was
// never synced — only optimistic WS increments accumulated, producing the inflated "99" badge
// with an empty list.) Narrowed via `Record<string, unknown>` rather than a dedicated interface:
// the contract is small enough that inline narrowing beats a one-field type alias.

type CountListener = (count: number) => void;
type EventListener = (evt: NotificationEvent) => void;

// ── Module-local state ─────────────────────────────────────────────────────

let unreadCount: number = 0;
let initialized: boolean = false;
let suppressToasts: boolean = false;

/** NOTIF-FIX (bug A): set when the local count is "ahead" of the server (optimistic increment
 *  or decrement). While dirty the 30s poll skips its fetched count — a poll racing an in-flight
 *  dismiss would otherwise clobber the optimistic decrement with the server's stale "still
 *  unread" value. Cleared by the next successful `refreshUnreadCount()`. */
let dirty: boolean = false;

/** NOTIF-FIX (bug B): ids already counted via a WS event. The server rebroadcasts the same id on
 *  dedup-hit inserts (e.g. `record_system("discovery_failed", …)` twice in 24h), so without dedup
 *  the badge inflates by +1 per rebroadcast though the row hasn't changed. Populated from WS
 *  events and from `markIdsSeen()` (called by the notifications view after its initial list
 *  fetch). Capped at `SEEN_IDS_CAP`; on overflow the set is cleared and refilled (the 500ms
 *  debounced refresh re-syncs, so a brief overcount window is acceptable). */
const seenIds: Set<number> = new Set<number>();
const SEEN_IDS_CAP: number = 1000;

/** 30s visibility-aware poll for `GET /notifications/unread-count`. Async-aware (the next tick
 *  is scheduled only after the previous settles, so a slow request can't stack ticks) and
 *  paused while the tab is hidden — it refreshes once on resume. */
let pollHandle: VisibilityAwareHandle | null = null;

/** Debounce timer for the post-WS-event `refreshUnreadCount()`: events arriving in quick
 *  succession coalesce into one network call. */
let refreshDebounce: ReturnType<typeof setTimeout> | null = null;

const countListeners: Set<CountListener> = new Set();
const eventListeners: Set<EventListener> = new Set();

// ── Public API ─────────────────────────────────────────────────────────────

/** Current unread count. Reads are cheap (no allocation). */
export function getUnreadCount(): number {
  return unreadCount;
}

/** Replace the unread count and notify every subscriber (sidebar, view header). Clamped
 *  defensively against a negative from a server bug.
 *
 *  NOTIF-FIX: `opts.optimistic` marks the local count as "ahead of the server" (sets the dirty
 *  flag). Pass `true` for unconfirmed local changes (optimistic decrement after dismiss,
 *  increment on a novel WS event) and `false` (default) when applying a server-confirmed value
 *  (inside `refreshUnreadCount`, or reverting an optimistic change after an API failure). */
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

/** Subscribe to live `notification` WS events; returns an unsubscribe fn. Listeners receive
 *  the parsed `NotificationEvent`, already narrowed by the ws-bus dispatcher. */
export function onNotificationEvent(fn: EventListener): () => void {
  eventListeners.add(fn);
  return () => { eventListeners.delete(fn); };
}

/** Toggle toasts for live notifications. Used by the DnD overlay. */
export function setSuppressToasts(b: boolean): void {
  suppressToasts = b;
}

/** Force-refetch the unread count. Used by the notifications view after mark-as-read/archive,
 *  by the WS handler's debounced re-sync (500ms after each event) and by the 30s poll.
 *
 *  NOTIF-FIX: always applies the fetched count regardless of `dirty` and clears it on success —
 *  the confirmation half of the dirty-flag protocol. The 30s poll goes through
 *  `pollRefreshUnreadCount()`, which skips while dirty. Also fixed the response field name from
 *  `unread_count` to the server's actual `count`. */
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
  } catch {
    // Swallow: the 30s poll retries. The badge stays at its last-known value rather than
    // flickering to 0.
  }
}

/** NOTIF-FIX: 30s background poll. Skips the fetch entirely while `dirty` — the local count is
 *  ahead of the server, so applying the server's stale value would clobber the optimistic
 *  change. The next user action (or the WS handler's 500ms debounced refresh) clears dirty and
 *  re-enables polling. */
async function pollRefreshUnreadCount(): Promise<void> {
  if (dirty) return;
  await refreshUnreadCount();
}

/** Decrement the unread count locally (e.g. after marking one notification read). Clamped at 0
 *  and marked optimistic (dirty) so the 30s poll can't overwrite it before the next
 *  `refreshUnreadCount()` confirms. */
export function decrementUnread(by: number = 1): void {
  setUnreadCount(unreadCount - by, { optimistic: true });
}

/** NOTIF-FIX (bug B): mark ids as "already seen" so a WS rebroadcast of the same id (server
 *  dedup-hit) doesn't cause a spurious optimistic increment. Called by the notifications view
 *  after its initial list fetch. Capped at `SEEN_IDS_CAP`; on overflow the set is cleared and
 *  refilled. */
export function markIdsSeen(ids: Iterable<number>): void {
  for (const id of ids) {
    seenIds.add(id);
  }
  if (seenIds.size > SEEN_IDS_CAP) {
    seenIds.clear();
  }
}

// ── i18n helpers (store toast body + view card body) ───────────────────────

/** Per-kind body text via `t()`. Accepts a live `NotificationEvent` or a persisted
 *  `NotificationRow` — both have `kind` + `payload`.
 *
 *  For `system` notifications (G2) it dispatches on `payload.code` to pick a per-code template
 *  (`notifications.body.{code}`), whose placeholders come from the server-side `details` shape.
 *  A missing template (older i18n pack, or a brand-new code) falls back to the generic
 *  `notifications.body.system` that echoes `message`. */
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
      // The "matched {{keyword}}" variant only applies when the provider had an
      // `auto_activate_keyword`; a null keyword means "all new models auto-activate", which uses
      // the shorter `_no_keyword` template.
      return keyword
        ? t("notifications.body.model_auto_activated", { model_id: modelId, provider_id: providerId, keyword })
        : t("notifications.body.model_auto_activated_no_keyword", { model_id: modelId, provider_id: providerId });
    case "system":
      return systemBody(p, message);
    default:
      return "";
  }
}

/** Per-code body template for `system` notifications, mirroring the `notifications::CODE_*`
 *  constants on the Rust side. Falls back to `notifications.body.system` when the per-code key
 *  isn't in the pack: `t()` returns the key itself when missing, so that case is detected
 *  explicitly to avoid showing the raw key string. */
function systemBody(p: Record<string, unknown>, message: string): string {
  const code: string = typeof p["code"] === "string" ? p["code"] : "";
  if (!code) {
    return t("notifications.body.system", { message });
  }
  // `t()` returns the key itself when the string isn't loaded, so detect that fallback and
  // route to the generic system template.
  const perCodeKey: string = `notifications.body.${code}`;
  const details: Record<string, unknown> =
    (p["details"] && typeof p["details"] === "object" && !Array.isArray(p["details"]))
      ? p["details"] as Record<string, unknown>
      : {};
  // Interpolation params merge top-level payload fields with `details`, so templates can use
  // either `{{provider_id}}` (top-level) or `{{account_id}}` (inside `details`).
  const params: Record<string, string | number> = Object.assign({}, { message });
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
    // Missing i18n key — fall back to the generic system body so the user sees the
    // server-provided `message` instead of the raw key string.
    return t("notifications.body.system", { message });
  }
  return rendered;
}

/** Format an RFC-3339 `created_at` as a relative "X ago" string through the i18n pluralised
 *  keys; anything within the last minute is "just now". */
export function formatRelativeAgo(iso: string, nowMs: number = Date.now()): string {
  let createdMs: number;
  try {
    let normalizedIso = iso;
    if (normalizedIso.length === 19 && normalizedIso.charAt(10) === ' ') {
      normalizedIso = normalizedIso.replace(' ', 'T') + 'Z';
    }
    createdMs = Date.parse(normalizedIso);
    if (!Number.isFinite(createdMs)) return "";
  } catch {
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

// ── Boot + lifecycle ───────────────────────────────────────────────────────

/** Initialise the store at app boot. Idempotent. Opens the WS, subscribes to ws-bus, starts the
 *  30s poll and primes the unread count from the server. */
export function initNotificationsStore(): void {
  if (initialized) return;
  initialized = true;

  // Open the live-logs WS at boot so `notification` events arrive with no WS-owning view
  // mounted. `connectLogsWebSocket` is idempotent — a later call from views/logs.ts is a no-op.
  connectLogsWebSocket();

  // Keepalive re-opens the WS if anything closed it. 5s is generous enough not to fight the
  // logs view's own 1–30s reconnect backoff, acting as a safety net once that window elapses.
  // The handle is intentionally not stored: the store is process-global and never cancels it.
  void setInterval(() => {
    if (!isLoggedIn()) return;
    const ws: WebSocket | null = state.logs.ws;
    if (!ws || ws.readyState === WebSocket.CLOSED || ws.readyState === WebSocket.CLOSING) {
      try { connectLogsWebSocket(); } catch { /* swallow — next tick */ }
    }
  }, 5000);

  // The ws-bus is independent of WS connection state — with the WS closed no events arrive
  // and the 30s poll is the only source of truth.
  //
  // NOTIF-FIX (bug B): increment only for NOVEL ids. The server rebroadcasts the same id on
  // dedup-hit inserts (e.g. a flapping `discovery_failed` within 24h, or a `model_new` for a
  // provider:model re-discovery sees again). The underlying row already exists, so the server's
  // count is unchanged — incrementing per rebroadcast produced the inflated "99" badge with an
  // empty list (the view's WS handler refuses to prepend a row whose id is already listed, so
  // the rebroadcast was invisible in the list but still +1 on the badge).
  subscribeWs("notification", (msg) => {
    const data: unknown = msg.data;
    if (!data || typeof data !== "object") return;
    const evt: NotificationEvent = data as NotificationEvent;
    const isNovel: boolean = !seenIds.has(evt.id);
    if (isNovel) {
      seenIds.add(evt.id);
      if (seenIds.size > SEEN_IDS_CAP) seenIds.clear();
      // Optimistic increment so the badge reacts instantly; the server stays the source of truth
      // (debounced re-sync 500ms later clears `dirty`). The flag also shields this increment
      // from a racing 30s poll.
      setUnreadCount(unreadCount + 1, { optimistic: true });
    }
    // Fan out regardless of novelty: a rebroadcast of a known id is still real-time signal
    // (the event is happening again now), so listeners may want to move the row to the top.
    // Each listener handles its own errors.
    for (const fn of eventListeners) {
      try { fn(evt); } catch (e: unknown) {
        console.error("[notifications-store] event listener threw", e);
      }
    }
    // Debounced re-sync: events in quick succession coalesce into one server call. This is
    // the confirmation half of the dirty-flag protocol — always applies the server's count and
    // clears dirty.
    if (refreshDebounce !== null) clearTimeout(refreshDebounce);
    refreshDebounce = setTimeout(() => {
      refreshDebounce = null;
      void refreshUnreadCount();
    }, 500);
    // Transient toast, suppressed during DnD so a notification mid-drag doesn't yank focus.
    // Novel events only.
    if (!suppressToasts && isNovel) {
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

/** Start the 30s visibility-aware poll. Its callback goes through `pollRefreshUnreadCount`,
 *  which skips while `dirty`, so an in-flight optimistic change can't be clobbered. The next
 *  user-initiated `refreshUnreadCount()` clears dirty and re-enables polling. */
function schedulePoll(): void {
  if (pollHandle !== null) return;
  pollHandle = createVisibilityAwareInterval(() => pollRefreshUnreadCount(), 30_000);
}
