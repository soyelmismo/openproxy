// state/notifications-store.test.ts — unit tests for the unread-count
// store + notification fan-out bus.
//
// Verifies the ACTUAL contract of notifications-store.ts:
//   - setUnreadCount / getUnreadCount / decrementUnread counter logic
//     (clamped at 0, listener fan-out, optimistic dirty flag)
//   - onUnreadCountChange / onNotificationEvent subscription lifecycle
//   - markIdsSeen dedup: a WS rebroadcast for an already-seen id does
//     NOT increment the count (NOTIF-FIX bug B)
//   - A novel WS event increments optimistically, fans out to listeners,
//     schedules a debounced re-sync, and shows a toast
//   - setSuppressToasts suppresses the toast (DnD overlay) but still
//     fans out + increments
//   - refreshUnreadCount applies the server's `count` field and clears
//     the dirty flag
//   - The 30s poll skips when dirty (NOTIF-FIX bug A)
//   - notificationBody / formatRelativeAgo i18n helpers
//
// SPEC DEVIATION (playground-refactor.md §8.C): the brief described a
// "queue" (sequential display) and "priority" (critical interrupts
// queue) model. The real store has neither — every novel notification
// fires showToast immediately (no queueing, no priority levels). The
// tests below cover the real behaviour; the queue/priority criteria
// are not applicable and are reported as a deviation.
//
// External deps are spied on (oxlint no-module-mocking forbids
// vi.mock): ws.js (connect), ws-bus.js (subscribeWs), api.js (fetch),
// toast.js (showToast), auth.js (isLoggedIn), i18n (t). The captured
// ws-bus handler is invoked manually to simulate live WS events.
//
// Module-load order matters: we resetModules, import + spy each
// dependency, THEN import the store so its top-level imports resolve
// to the already-spied module instances.

import { describe, it, expect, vi } from "vitest";
import { setupTimers } from "../__test-utils__/index.js";
import type { NotificationEvent } from "../lib/types/notifications.js";
import type { WsEnvelope } from "../views/logs.js";

// --------------- Helpers ---------------

function makeEvent(id: number, kind: NotificationEvent["kind"] = "model_new"): NotificationEvent {
  return {
    id,
    kind,
    payload: { model_id: "gpt-4", provider_id: "openai" },
    created_at: "2025-01-01T00:00:00Z",
  };
}

interface StoreHarness {
  store: typeof import("./notifications-store.js");
  fire: (data: unknown) => void;
  showToast: ReturnType<typeof vi.fn>;
  api: ReturnType<typeof vi.fn>;
  connect: ReturnType<typeof vi.fn>;
  t: ReturnType<typeof vi.fn>;
  subscribeWs: ReturnType<typeof vi.spyOn>;
}

/** Reset the module graph, spy every dependency, then import + init
 *  the store. Returns a harness with the store and the captured WS
 *  notification handler. */
async function setupStore(opts: { init?: boolean } = {}): Promise<StoreHarness> {
  const init: boolean = opts.init !== false;
  // Import + spy dependencies BEFORE the store so its top-level
  // imports resolve to the spied instances.
  const toast = await import("../components/toast.js");
  const showToast = vi.fn();
  vi.spyOn(toast, "showToast").mockImplementation(showToast);

  const apiMod = await import("./api.js");
  const api = vi.fn<() => Promise<unknown>>().mockResolvedValue(null);
  vi.spyOn(apiMod, "api").mockImplementation(api);

  const ws = await import("./ws.js");
  const connect = vi.fn();
  vi.spyOn(ws, "connectLogsWebSocket").mockImplementation(connect);

  const auth = await import("./auth.js");
  vi.spyOn(auth, "isLoggedIn").mockReturnValue(true);

  const i18n = await import("../i18n/index.js");
  const t = vi.fn((key: string) => key);
  vi.spyOn(i18n, "t").mockImplementation(t);

  // Capture the notification handler the store registers on the bus.
  // mockImplementation receives the real subscribeWs signature, so the
  // callback is typed by the spy — no cast chain needed.
  let capturedHandler: ((msg: WsEnvelope) => void) | null = null;
  const bus = await import("./ws-bus.js");
  const subscribeWs = vi
    .spyOn(bus, "subscribeWs")
    .mockImplementation((type, fn) => {
      if (type === "notification") capturedHandler = fn;
      return () => {};
    });

  const store = await import("./notifications-store.js");
  if (init) store.initNotificationsStore();

  return {
    store,
    fire: (data: unknown) => {
      if (!capturedHandler) {
        throw new Error("notification handler was never registered");
      }
      // The handler expects a WsEnvelope; we deliver a minimal shape
      // because the store only reads `msg.data`. Single cast at the
      // test boundary — input here is trusted test data, not untrusted
      // network input.
      capturedHandler({ type: "notification", data } as WsEnvelope);
    },
    showToast,
    api,
    connect,
    t,
    subscribeWs,
  };
}

setupTimers();

// --------------- Counter logic ---------------

describe("notifications store — unread count", () => {
  it("starts at 0", async () => {
    const { store } = await setupStore({ init: false });
    expect(store.getUnreadCount()).toBe(0);
  });

  it("setUnreadCount updates the value", async () => {
    const { store } = await setupStore({ init: false });
    store.setUnreadCount(5);
    expect(store.getUnreadCount()).toBe(5);
  });

  it("setUnreadCount clamps negatives to 0", async () => {
    const { store } = await setupStore({ init: false });
    store.setUnreadCount(-3);
    expect(store.getUnreadCount()).toBe(0);
  });

  it("decrementUnread reduces the count and clamps at 0", async () => {
    const { store } = await setupStore({ init: false });
    store.setUnreadCount(3);
    store.decrementUnread();
    expect(store.getUnreadCount()).toBe(2);
    store.decrementUnread(5);
    expect(store.getUnreadCount()).toBe(0);
  });

  it("onUnreadCountChange fires only when the value changes", async () => {
    const { store } = await setupStore({ init: false });
    const seen: number[] = [];
    store.onUnreadCountChange((n) => seen.push(n));

    store.setUnreadCount(1);
    store.setUnreadCount(1); // no change → no fire
    store.setUnreadCount(2);

    expect(seen).toEqual([1, 2]);
  });

  it("unsubscribe stops count notifications", async () => {
    const { store } = await setupStore({ init: false });
    const seen: number[] = [];
    const unsub = store.onUnreadCountChange((n) => seen.push(n));

    store.setUnreadCount(1);
    unsub();
    store.setUnreadCount(2);

    expect(seen).toEqual([1]);
  });
});

// --------------- WS event handling ---------------

describe("notifications store — WS notification events", () => {
  it("a novel event increments the count optimistically", async () => {
    const { store, fire } = await setupStore();
    const before = store.getUnreadCount();

    fire(makeEvent(100));

    expect(store.getUnreadCount()).toBe(before + 1);
  });

  it("a rebroadcast for an already-seen id does NOT increment", async () => {
    const { store, fire } = await setupStore();

    fire(makeEvent(200));
    const afterFirst = store.getUnreadCount();

    // Same id again (server dedup-hit rebroadcast).
    fire(makeEvent(200));

    expect(store.getUnreadCount()).toBe(afterFirst);
  });

  it("markIdsSeen prevents a future WS event for that id from incrementing", async () => {
    const { store, fire } = await setupStore();
    const before = store.getUnreadCount();

    store.markIdsSeen([300]);
    fire(makeEvent(300));

    expect(store.getUnreadCount()).toBe(before);
  });

  it("fans out to event listeners regardless of novelty", async () => {
    const { store, fire } = await setupStore();
    const events: Array<{ id: number }> = [];
    store.onNotificationEvent((e) => events.push({ id: e.id }));

    fire(makeEvent(400));
    fire(makeEvent(400)); // rebroadcast

    // Both deliveries reach the listener (real-time signal preserved).
    expect(events.length).toBe(2);
  });

  it("shows a toast for a novel event", async () => {
    const { fire, showToast } = await setupStore();

    fire(makeEvent(500));

    expect(showToast).toHaveBeenCalledOnce();
    expect(showToast.mock.calls[0]![1]).toBe("info");
  });

  it("does NOT show a toast for a rebroadcast (non-novel) event", async () => {
    const { fire, showToast } = await setupStore();

    fire(makeEvent(600));
    showToast.mockClear();
    fire(makeEvent(600));

    expect(showToast).not.toHaveBeenCalled();
  });

  it("setSuppressToasts(true) suppresses the toast but keeps the increment", async () => {
    const { store, fire, showToast } = await setupStore();
    store.setSuppressToasts(true);

    const before = store.getUnreadCount();
    fire(makeEvent(700));

    expect(store.getUnreadCount()).toBe(before + 1);
    expect(showToast).not.toHaveBeenCalled();
  });

  it("ignores a malformed (non-object) WS payload", async () => {
    const { store, fire, showToast } = await setupStore();
    const before = store.getUnreadCount();

    fire(null);
    fire("not-an-object");

    expect(store.getUnreadCount()).toBe(before);
    expect(showToast).not.toHaveBeenCalled();
  });

  it("schedules a debounced re-sync 500ms after an event", async () => {
    const { fire, api } = await setupStore();
    api.mockResolvedValue({ count: 0 });

    fire(makeEvent(800));
    api.mockClear();

    // Before the debounce window elapses, no refresh call yet.
    await vi.advanceTimersByTimeAsync(400);
    expect(api).not.toHaveBeenCalled();

    // After 500ms, the debounced refresh fires once.
    await vi.advanceTimersByTimeAsync(200);
    expect(api).toHaveBeenCalledTimes(1);
    expect(api).toHaveBeenCalledWith("/notifications/unread-count");
  });

  it("coalesces multiple rapid events into a single re-sync", async () => {
    const { fire, api } = await setupStore();
    api.mockResolvedValue({ count: 0 });

    fire(makeEvent(901));
    fire(makeEvent(902));
    fire(makeEvent(903));
    api.mockClear();

    await vi.advanceTimersByTimeAsync(600);

    expect(api).toHaveBeenCalledTimes(1);
  });
});

// --------------- refreshUnreadCount + dirty flag ---------------

describe("notifications store — refresh + dirty flag", () => {
  it("refreshUnreadCount applies the server's count field", async () => {
    const { store, api } = await setupStore();
    api.mockResolvedValue({ count: 7 });

    await store.refreshUnreadCount();

    expect(store.getUnreadCount()).toBe(7);
  });

  it("the 30s poll skips while dirty and resumes after a refresh", async () => {
    const { store, api } = await setupStore();

    // Mark the local count as ahead of the server (optimistic).
    store.setUnreadCount(50, { optimistic: true });
    api.mockClear();

    // Advance to the 30s poll tick — dirty → the fetch is skipped.
    await vi.advanceTimersByTimeAsync(30_000);
    expect(api).not.toHaveBeenCalled();

    // A user-initiated refresh clears dirty and applies the server value.
    api.mockResolvedValue({ count: 3 });
    await store.refreshUnreadCount();
    expect(store.getUnreadCount()).toBe(3);

    // Now the poll runs again.
    api.mockClear();
    api.mockResolvedValue({ count: 4 });
    await vi.advanceTimersByTimeAsync(30_000);
    expect(api).toHaveBeenCalled();
    expect(store.getUnreadCount()).toBe(4);
  });

  it("refreshUnreadCount swallows API errors (badge keeps last value)", async () => {
    const { store, api } = await setupStore();
    store.setUnreadCount(9);
    api.mockRejectedValue(new Error("network down"));

    await store.refreshUnreadCount();

    expect(store.getUnreadCount()).toBe(9);
  });

  it("refreshUnreadCount ignores a response without a numeric count", async () => {
    const { store, api } = await setupStore();
    store.setUnreadCount(4);
    api.mockResolvedValue({ unexpected: "shape" });

    await store.refreshUnreadCount();

    expect(store.getUnreadCount()).toBe(4);
  });
});

// --------------- init lifecycle ---------------

describe("notifications store — init", () => {
  it("opens the live-logs WS at boot", async () => {
    const { connect } = await setupStore();
    expect(connect).toHaveBeenCalled();
  });

  it("is idempotent: a second init does not re-register the handler", async () => {
    const { store, subscribeWs } = await setupStore();
    const callsAfterFirst = subscribeWs.mock.calls.length;

    store.initNotificationsStore();

    expect(subscribeWs.mock.calls.length).toBe(callsAfterFirst);
  });

  it("primes the unread count from the server on init", async () => {
    const { api } = await setupStore();
    api.mockResolvedValue({ count: 11 });

    // The initial refreshUnreadCount() is async; flush microtasks.
    await vi.advanceTimersByTimeAsync(0);

    expect(api).toHaveBeenCalledWith("/notifications/unread-count");
  });
});

// --------------- i18n helpers ---------------

describe("notifications store — notificationBody", () => {
  it("renders model_new via the i18n key", async () => {
    const { store, t } = await setupStore({ init: false });
    const body = store.notificationBody({
      id: 1,
      kind: "model_new",
      payload: { model_id: "gpt-4", provider_id: "openai" },
      created_at: "2025-01-01T00:00:00Z",
    });
    expect(t).toHaveBeenCalledWith("notifications.body.model_new", {
      model_id: "gpt-4",
      provider_id: "openai",
    });
    expect(body).toBe("notifications.body.model_new");
  });

  it("renders model_auto_activated with keyword vs without", async () => {
    const { store, t } = await setupStore({ init: false });

    store.notificationBody({
      id: 1,
      kind: "model_auto_activated",
      payload: { model_id: "m", provider_id: "p", matched_keyword: "kw" },
      created_at: "2025-01-01T00:00:00Z",
    });
    expect(t).toHaveBeenCalledWith(
      "notifications.body.model_auto_activated",
      { model_id: "m", provider_id: "p", keyword: "kw" },
    );

    t.mockClear();
    store.notificationBody({
      id: 2,
      kind: "model_auto_activated",
      payload: { model_id: "m", provider_id: "p", matched_keyword: null },
      created_at: "2025-01-01T00:00:00Z",
    });
    expect(t).toHaveBeenCalledWith(
      "notifications.body.model_auto_activated_no_keyword",
      { model_id: "m", provider_id: "p" },
    );
  });

  it("returns empty string for an unknown kind", async () => {
    const { store } = await setupStore({ init: false });
    const body = store.notificationBody({
      id: 1,
      kind: "bogus" as never,
      payload: {},
      created_at: "2025-01-01T00:00:00Z",
    });
    expect(body).toBe("");
  });

  it("system body falls back to the generic template when the per-code key is missing", async () => {
    const { store } = await setupStore({ init: false });
    // t() returns the key itself (our default mock), which the store
    // interprets as "missing" → routes to notifications.body.system.
    const body = store.notificationBody({
      id: 1,
      kind: "system",
      payload: { code: "some_new_code", message: "boom" },
      created_at: "2025-01-01T00:00:00Z",
    });
    expect(body).toBe("notifications.body.system");
  });
});

describe("notifications store — formatRelativeAgo", () => {
  it("returns 'just now' for anything under a minute", async () => {
    const { store } = await setupStore({ init: false });
    const now = Date.parse("2025-01-01T00:00:30Z");
    const out = store.formatRelativeAgo("2025-01-01T00:00:00Z", now);
    expect(out).toBe("notifications.ago.just_now");
  });

  it("formats minutes", async () => {
    const { store, t } = await setupStore({ init: false });
    const now = Date.parse("2025-01-01T00:05:00Z");
    const out = store.formatRelativeAgo("2025-01-01T00:00:00Z", now);
    expect(t).toHaveBeenCalledWith("notifications.ago.minutes", { count: 5 });
    expect(out).toBe("notifications.ago.minutes");
  });

  it("formats hours", async () => {
    const { store, t } = await setupStore({ init: false });
    const now = Date.parse("2025-01-01T03:00:00Z");
    store.formatRelativeAgo("2025-01-01T00:00:00Z", now);
    expect(t).toHaveBeenCalledWith("notifications.ago.hours", { count: 3 });
  });

  it("formats days", async () => {
    const { store, t } = await setupStore({ init: false });
    const now = Date.parse("2025-01-04T00:00:00Z");
    store.formatRelativeAgo("2025-01-01T00:00:00Z", now);
    expect(t).toHaveBeenCalledWith("notifications.ago.days", { count: 3 });
  });

  it("normalises a space-separated SQL timestamp", async () => {
    const { store, t } = await setupStore({ init: false });
    const now = Date.parse("2025-01-01T00:02:00Z");
    // SQLite `datetime('now')` yields "YYYY-MM-DD HH:MM:SS" (19 chars).
    const out = store.formatRelativeAgo("2025-01-01 00:00:00", now);
    expect(t).toHaveBeenCalledWith("notifications.ago.minutes", { count: 2 });
    expect(out).toBe("notifications.ago.minutes");
  });

  it("returns empty string for an unparseable timestamp", async () => {
    const { store } = await setupStore({ init: false });
    expect(store.formatRelativeAgo("not-a-date", Date.now())).toBe("");
  });
});