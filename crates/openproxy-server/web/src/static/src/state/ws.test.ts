import { describe, it, expect, vi, beforeEach } from "vitest";
import { setupTimers, setupAuthMock } from "../__test-utils__/index.js";

class MockWebSocket {
  static instances: MockWebSocket[] = [];
  url: string;
  readyState = 0;
  sent: unknown[] = [];
  private listeners: Record<string, Set<(ev: any) => void>> = {};

  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;

  constructor(url: string) {
    this.url = url;
    MockWebSocket.instances.push(this);
  }
  send(data: unknown) { this.sent.push(data); }
  close() {
    this.readyState = MockWebSocket.CLOSED;
    this.fire("close", new CloseEvent("close"));
  }
  addEventListener(type: string, fn: (ev: any) => void) {
    (this.listeners[type] ??= new Set()).add(fn);
  }
  removeEventListener(type: string, fn: (ev: any) => void) {
    this.listeners[type]?.delete(fn);
  }
  private fire(type: string, ev: any) {
    this.listeners[type]?.forEach((fn) => fn(ev));
  }
  simulateOpen() { this.readyState = MockWebSocket.OPEN; this.fire("open", new Event("open")); }
  simulateMessage(data: string) { this.fire("message", new MessageEvent("message", { data })); }
  simulateError() { this.fire("error", new Event("error")); }
}

async function openWs(): Promise<MockWebSocket> {
  const { connectLogsWebSocket } = await import("./ws.js");
  connectLogsWebSocket();
  const ws = MockWebSocket.instances[MockWebSocket.instances.length - 1]!;
  ws.simulateOpen();
  return ws;
}

async function trackStatuses(): Promise<string[]> {
  const { subscribeLogsStatus } = await import("./ws.js");
  const statuses: string[] = [];
  subscribeLogsStatus((s) => statuses.push(s));
  return statuses;
}

setupTimers();

beforeEach(async () => {
  const { disconnectLogsWebSocket } = await import("./ws.js");
  disconnectLogsWebSocket();
  MockWebSocket.instances = [];
  vi.stubGlobal("WebSocket", MockWebSocket);
  await setupAuthMock({ token: "test-token" });
});

describe("ws store — validation and urls", () => {
  it("validates StageEvent correctly", async () => {
    const { isStageEvent } = await import("./ws.js");
    const valid = {
      request_id: "req-1", trace_id: "tr-1", provider_id: "p1",
      upstream_model_id: "m1", stage: "request_start", elapsed_ms: 10,
      status_code: 200, timestamp: "2026-01-01T00:00:00Z",
    };
    expect(isStageEvent(valid)).toBe(true);
    expect(isStageEvent({ ...valid, request_id: 123 })).toBe(false);
    expect(isStageEvent(null)).toBe(false);
    expect(isStageEvent("string")).toBe(false);
  });

  it("builds correct URL with token and protocol", async () => {
    const { logsWsUrl } = await import("./ws.js");
    const url = logsWsUrl();
    expect(url).toContain("/admin/ws?token=test-token");
  });
});

describe("ws store — connection and cursors", () => {
  it("connects with token and triggers connecting status", async () => {
    const statuses = await trackStatuses();
    const { connectLogsWebSocket } = await import("./ws.js");
    connectLogsWebSocket();
    expect(MockWebSocket.instances.length).toBe(1);
    expect(statuses).toContain("connecting");
  });

  it("does not connect when token is missing", async () => {
    await setupAuthMock({ token: null });
    const { connectLogsWebSocket } = await import("./ws.js");
    connectLogsWebSocket();
    expect(MockWebSocket.instances.length).toBe(0);
  });

  it("handles cursor subscribe messaging", async () => {
    const { liveLogsStore } = await import("./live-logs-store.js");
    const { connectLogsWebSocket, disconnectLogsWebSocket } = await import("./ws.js");
    liveLogsStore.lastAppliedCursor = 42;
    connectLogsWebSocket();
    const ws1 = MockWebSocket.instances[0]!;
    ws1.simulateOpen();
    expect(ws1.sent).toEqual([JSON.stringify({ type: "subscribe", cursor: 42 })]);

    disconnectLogsWebSocket();
    MockWebSocket.instances = [];
    liveLogsStore.lastAppliedCursor = 0;
    connectLogsWebSocket();
    const ws2 = MockWebSocket.instances[0]!;
    ws2.simulateOpen();
    expect(ws2.sent.length).toBe(0);
  });

  it("is idempotent: does not create a second WS while first is open", async () => {
    await openWs();
    const { connectLogsWebSocket } = await import("./ws.js");
    connectLogsWebSocket();
    expect(MockWebSocket.instances.length).toBe(1);
  });
});

describe("ws store — reconnect and heartbeat", () => {
  it("reconnects with backoff on close", async () => {
    const statuses = await trackStatuses();
    const ws = await openWs();
    ws.close();
    expect(statuses).toContain("disconnected");

    await vi.advanceTimersByTimeAsync(300);
    expect(MockWebSocket.instances.length).toBe(2);
    expect(statuses).toContain("reconnecting");

    MockWebSocket.instances[1]!.close();
    await vi.advanceTimersByTimeAsync(600);
    expect(MockWebSocket.instances.length).toBe(3);

    MockWebSocket.instances[2]!.simulateOpen();
    MockWebSocket.instances[2]!.close();
    await vi.advanceTimersByTimeAsync(300);
    expect(MockWebSocket.instances.length).toBe(4);
  });

  it("sends ping every 15s and stops on close", async () => {
    const ws = await openWs();
    expect(ws.sent.length).toBe(0);
    await vi.advanceTimersByTimeAsync(15_000);
    expect(ws.sent).toEqual([JSON.stringify({ type: "ping" })]);
    await vi.advanceTimersByTimeAsync(15_000);
    expect(ws.sent.length).toBe(2);

    ws.close();
    await vi.advanceTimersByTimeAsync(30_000);
    expect(ws.sent.length).toBe(2);
  });
});

describe("ws store — message dispatch and errors", () => {
  it("dispatches messages to ws-bus and ignores malformed inputs", async () => {
    const bus = await import("./ws-bus.js");
    const dispatchSpy = vi.spyOn(bus, "dispatchWs");
    const ws = await openWs();

    ws.simulateMessage(JSON.stringify({ type: "notification", data: { id: 1 } }));
    expect(dispatchSpy).toHaveBeenCalledWith({ type: "notification", data: { id: 1 } });

    expect(() => ws.simulateMessage("bad json {{{")).not.toThrow();
    ws.simulateMessage(JSON.stringify({ foo: "bar" }));
    expect(dispatchSpy).toHaveBeenCalledTimes(1);
  });

  it("disconnectLogsWebSocket cleans up and error triggers close", async () => {
    const statuses = await trackStatuses();
    await openWs();
    statuses.length = 0;
    const { disconnectLogsWebSocket } = await import("./ws.js");
    disconnectLogsWebSocket();
    expect(statuses).toContain("disconnected");

    const ws = await openWs();
    ws.simulateError();
    expect(ws.readyState).toBe(MockWebSocket.CLOSED);
  });
});
