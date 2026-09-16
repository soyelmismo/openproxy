import { describe, it, expect, vi } from "vitest";
import { setupTimers } from "../__test-utils__/index.js";
import type { NotificationEvent } from "../lib/types/notifications.js";
import type { WsEnvelope } from "../views/logs.js";

function makeEvent(id: number, kind: NotificationEvent["kind"] = "model_new"): NotificationEvent {
  return { id, kind, payload: { model_id: "gpt-4", provider_id: "openai" }, created_at: "2025-01-01T00:00:00Z" };
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

async function setupStore(opts: { init?: boolean } = {}): Promise<StoreHarness> {
  const init = opts.init !== false;
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

  let capturedHandler: ((msg: WsEnvelope) => void) | null = null;
  const bus = await import("./ws-bus.js");
  const subscribeWs = vi.spyOn(bus, "subscribeWs").mockImplementation((type, fn) => {
    if (type === "notification") capturedHandler = fn;
    return () => {};
  });

  const store = await import("./notifications-store.js");
  if (init) store.initNotificationsStore();

  return {
    store,
    fire: (data: unknown) => {
      if (!capturedHandler) throw new Error("no handler");
      capturedHandler({ type: "notification", data } as WsEnvelope);
    },
    showToast, api, connect, t, subscribeWs,
  };
}

setupTimers();

describe("notifications store — unread count", () => {
  it("manages and clamps count with listener notifications", async () => {
    const { store } = await setupStore({ init: false });
    expect(store.getUnreadCount()).toBe(0);
    store.setUnreadCount(5);
    expect(store.getUnreadCount()).toBe(5);
    store.setUnreadCount(-3);
    expect(store.getUnreadCount()).toBe(0);

    store.setUnreadCount(3);
    store.decrementUnread();
    expect(store.getUnreadCount()).toBe(2);
    store.decrementUnread(5);
    expect(store.getUnreadCount()).toBe(0);

    const seen: number[] = [];
    const unsub = store.onUnreadCountChange((n) => seen.push(n));
    store.setUnreadCount(1);
    store.setUnreadCount(1);
    store.setUnreadCount(2);
    unsub();
    store.setUnreadCount(3);
    expect(seen).toEqual([1, 2]);
  });
});

describe("notifications store — WS notification events", () => {
  it("increments on novel event, deduplicates rebroadcasts and markIdsSeen", async () => {
    const { store, fire } = await setupStore();
    const b0 = store.getUnreadCount();
    fire(makeEvent(100));
    expect(store.getUnreadCount()).toBe(b0 + 1);

    // rebroadcast does not increment
    fire(makeEvent(100));
    expect(store.getUnreadCount()).toBe(b0 + 1);

    // markIdsSeen prevents increment
    store.markIdsSeen([300]);
    fire(makeEvent(300));
    expect(store.getUnreadCount()).toBe(b0 + 1);

    // malformed payload ignored
    fire(null);
    fire("not-an-object");
    expect(store.getUnreadCount()).toBe(b0 + 1);
  });

  it("handles event listener fan-out, toasts, and suppression", async () => {
    const { store, fire, showToast } = await setupStore();
    const events: number[] = [];
    store.onNotificationEvent((e) => events.push(e.id));
    fire(makeEvent(400));
    fire(makeEvent(400));
    expect(events).toEqual([400, 400]);
    expect(showToast).toHaveBeenCalledTimes(1);
    expect(showToast.mock.calls[0]![1]).toBe("info");

    showToast.mockClear();
    store.setSuppressToasts(true);
    const before = store.getUnreadCount();
    fire(makeEvent(700));
    expect(store.getUnreadCount()).toBe(before + 1);
    expect(showToast).not.toHaveBeenCalled();
  });

  it("debounces and coalesces re-sync after events", async () => {
    const { fire, api } = await setupStore();
    api.mockResolvedValue({ count: 0 });
    fire(makeEvent(800));
    api.mockClear();

    await vi.advanceTimersByTimeAsync(400);
    expect(api).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(200);
    expect(api).toHaveBeenCalledTimes(1);
    expect(api).toHaveBeenCalledWith("/notifications/unread-count");

    fire(makeEvent(901));
    fire(makeEvent(902));
    fire(makeEvent(903));
    api.mockClear();
    await vi.advanceTimersByTimeAsync(600);
    expect(api).toHaveBeenCalledTimes(1);
  });
});

describe("notifications store — refresh + dirty flag", () => {
  it("applies server count and manages 30s poll dirty state", async () => {
    const { store, api } = await setupStore();
    api.mockResolvedValue({ count: 7 });
    await store.refreshUnreadCount();
    expect(store.getUnreadCount()).toBe(7);

    store.setUnreadCount(50, { optimistic: true });
    api.mockClear();
    await vi.advanceTimersByTimeAsync(30_000);
    expect(api).not.toHaveBeenCalled();

    api.mockResolvedValue({ count: 3 });
    await store.refreshUnreadCount();
    expect(store.getUnreadCount()).toBe(3);

    api.mockClear();
    api.mockResolvedValue({ count: 4 });
    await vi.advanceTimersByTimeAsync(30_000);
    expect(api).toHaveBeenCalled();
    expect(store.getUnreadCount()).toBe(4);
  });

  it("handles api errors, invalid count shape, and delete rollbacks", async () => {
    const { store, api } = await setupStore({ init: false });
    store.setUnreadCount(9);
    api.mockRejectedValue(new Error("network down"));
    await store.refreshUnreadCount();
    expect(store.getUnreadCount()).toBe(9);

    api.mockResolvedValue({ unexpected: "shape" });
    await store.refreshUnreadCount();
    expect(store.getUnreadCount()).toBe(9);

    // optimistic delete and re-sync
    store.setUnreadCount(2);
    store.decrementUnread(1);
    expect(store.getUnreadCount()).toBe(1);
    api.mockResolvedValue({ count: 1 });
    await store.refreshUnreadCount();
    expect(store.getUnreadCount()).toBe(1);

    // already read: no decrement
    store.setUnreadCount(3);
    api.mockResolvedValue({ count: 3 });
    await store.refreshUnreadCount();
    expect(store.getUnreadCount()).toBe(3);

    // rollback on rejection
    store.setUnreadCount(2);
    store.decrementUnread(1);
    expect(store.getUnreadCount()).toBe(1);
    store.setUnreadCount(store.getUnreadCount() + 1);
    expect(store.getUnreadCount()).toBe(2);
  });
});

describe("notifications store — init and i18n helpers", () => {
  it("initializes WS connection and primes unread count idempotently", async () => {
    const { store, connect, subscribeWs, api } = await setupStore();
    expect(connect).toHaveBeenCalled();
    const calls = subscribeWs.mock.calls.length;
    store.initNotificationsStore();
    expect(subscribeWs.mock.calls.length).toBe(calls);
    api.mockResolvedValue({ count: 11 });
    await vi.advanceTimersByTimeAsync(0);
    expect(api).toHaveBeenCalledWith("/notifications/unread-count");
  });

  it("renders notification bodies correctly", async () => {
    const { store, t } = await setupStore({ init: false });
    expect(store.notificationBody({ id: 1, kind: "model_new", payload: { model_id: "gpt-4", provider_id: "openai" }, created_at: "2025-01-01T00:00:00Z" }))
      .toBe("notifications.body.model_new");
    expect(t).toHaveBeenCalledWith("notifications.body.model_new", { model_id: "gpt-4", provider_id: "openai" });

    store.notificationBody({ id: 1, kind: "model_auto_activated", payload: { model_id: "m", provider_id: "p", matched_keyword: "kw" }, created_at: "2025-01-01T00:00:00Z" });
    expect(t).toHaveBeenCalledWith("notifications.body.model_auto_activated", { model_id: "m", provider_id: "p", keyword: "kw" });

    store.notificationBody({ id: 2, kind: "model_auto_activated", payload: { model_id: "m", provider_id: "p", matched_keyword: null }, created_at: "2025-01-01T00:00:00Z" });
    expect(t).toHaveBeenCalledWith("notifications.body.model_auto_activated_no_keyword", { model_id: "m", provider_id: "p" });

    expect(store.notificationBody({ id: 1, kind: "bogus" as never, payload: {}, created_at: "2025-01-01T00:00:00Z" })).toBe("");
    expect(store.notificationBody({ id: 1, kind: "system", payload: { code: "some_code", message: "m" }, created_at: "2025-01-01T00:00:00Z" }))
      .toBe("notifications.body.system");
  });

  it("formats relative timestamps", async () => {
    const { store, t } = await setupStore({ init: false });
    expect(store.formatRelativeAgo("2025-01-01T00:00:00Z", Date.parse("2025-01-01T00:00:30Z"))).toBe("notifications.ago.just_now");
    expect(store.formatRelativeAgo("2025-01-01T00:00:00Z", Date.parse("2025-01-01T00:05:00Z"))).toBe("notifications.ago.minutes");
    expect(t).toHaveBeenCalledWith("notifications.ago.minutes", { count: 5 });

    store.formatRelativeAgo("2025-01-01T00:00:00Z", Date.parse("2025-01-01T03:00:00Z"));
    expect(t).toHaveBeenCalledWith("notifications.ago.hours", { count: 3 });

    store.formatRelativeAgo("2025-01-01T00:00:00Z", Date.parse("2025-01-04T00:00:00Z"));
    expect(t).toHaveBeenCalledWith("notifications.ago.days", { count: 3 });

    expect(store.formatRelativeAgo("2025-01-01 00:00:00", Date.parse("2025-01-01T00:02:00Z"))).toBe("notifications.ago.minutes");
    expect(store.formatRelativeAgo("not-a-date", Date.now())).toBe("");
  });
});