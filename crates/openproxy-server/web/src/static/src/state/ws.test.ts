// state/ws.test.ts — unit tests for the WebSocket lifecycle manager.
//
// Verifies the spec contract from playground-refactor.md §8.B:
//   - Initial connection: connectLogsWebSocket() creates a WS with
//     the correct URL (including auth token), triggers status updates,
//     and sends a subscribe message if a cursor is set
//   - Reconnect with exponential backoff after disconnect
//   - Heartbeat ping is sent every 15s on an open connection
//   - DisconnectLogsWebSocket() cleans up (closes, clears timer)
//   - Error handling: WS error triggers close → reconnect
//
// External dependencies are spied on (oxlint no-module-mocking):
//   - auth.js  → controls getToken() for URL auth
//   - ws-bus.js → captures dispatchWs calls
//
// The WebSocket constructor is stubbed via vi.stubGlobal so
// connectLogsWebSocket() creates a MockWebSocket instead of
// trying to reach a live server.

import { describe, it, expect, vi, beforeEach } from "vitest";
import { setupTimers, setupAuthMock } from "../__test-utils__/index.js";

// ---------- MockWebSocket ----------

// Implements addEventListener/removeEventListener because ws.ts uses
// the EventTarget API, not the legacy on* properties.
class MockWebSocket {
  static instances: MockWebSocket[] = [];
  url: string;
  readyState = 0; // CONNECTING
  sent: unknown[] = [];
  onopen: ((ev: Event) => void) | null = null;
  onclose: ((ev: CloseEvent) => void) | null = null;
  onmessage: ((ev: MessageEvent) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;
  private listeners: { [type: string]: Set<(ev: Event) => void> } = {};

  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;

  constructor(url: string) {
    this.url = url;
    MockWebSocket.instances.push(this);
  }

  send(data: unknown) {
    this.sent.push(data);
  }

  close() {
    this.readyState = MockWebSocket.CLOSED;
    this.fire("close", new CloseEvent("close"));
  }

  addEventListener(type: string, fn: (ev: Event) => void) {
    let set = this.listeners[type];
    if (!set) {
      set = new Set();
      this.listeners[type] = set;
    }
    set.add(fn);
  }

  removeEventListener(type: string, fn: (ev: Event) => void) {
    this.listeners[type]?.delete(fn);
  }

  private fire(type: string, ev: Event) {
    const set = this.listeners[type];
    if (set) for (const fn of set) fn(ev);
  }

  /** Test helper: simulate the server accepting the connection. */
  simulateOpen() {
    this.readyState = MockWebSocket.OPEN;
    this.fire("open", new Event("open"));
  }

  /** Test helper: simulate a message from the server. */
  simulateMessage(data: string) {
    this.fire("message", new MessageEvent("message", { data }));
  }

  /** Test helper: simulate a network error. */
  simulateError() {
    this.fire("error", new Event("error"));
  }
}

// ---------- Local helpers ----------

/** Open a fresh WS via the production code and simulate the server
 *  accepting the connection. Returns the MockWebSocket for assertions. */
async function openWs(): Promise<MockWebSocket> {
  const { connectLogsWebSocket } = await import("./ws.js");
  connectLogsWebSocket();
  const ws = MockWebSocket.instances[0]!;
  ws.simulateOpen();
  return ws;
}

/** Subscribe to status changes and return the array that captures
 *  every emitted value (initial state included). */
async function trackStatuses(): Promise<string[]> {
  const { subscribeLogsStatus } = await import("./ws.js");
  const statuses: string[] = [];
  subscribeLogsStatus((s) => statuses.push(s));
  return statuses;
}

// ---------- Setup ----------

setupTimers();

beforeEach(async () => {
  MockWebSocket.instances = [];
  vi.stubGlobal("WebSocket", MockWebSocket);

  // Default auth mock — every test gets a logged-in user unless it
  // overrides getToken explicitly. Each test that needs a different
  // token calls setupAuthMock() which re-imports on a fresh module
  // (resetModules runs in setupTimers' beforeEach, so imports are
  // guaranteed fresh here).
  await setupAuthMock();
});

// ---------- Tests ----------

describe("ws store — isStageEvent", () => {
  it("accepts a valid StageEvent object", async () => {
    const { isStageEvent } = await import("./ws.js");
    expect(
      isStageEvent({
        request_id: "req-1",
        trace_id: "tr-1",
        provider_id: "openai",
        upstream_model_id: "gpt-4",
        stage: "streaming",
        elapsed_ms: 100,
        status_code: 200,
        timestamp: "2025-01-01T00:00:00Z",
      }),
    ).toBe(true);
  });

  it("rejects null", async () => {
    const { isStageEvent } = await import("./ws.js");
    expect(isStageEvent(null)).toBe(false);
  });

  it("rejects an object missing required string fields", async () => {
    const { isStageEvent } = await import("./ws.js");
    expect(isStageEvent({ request_id: "req-1", stage: "streaming" })).toBe(
      false,
    );
  });

  it("rejects an object with wrong types for numeric fields", async () => {
    const { isStageEvent } = await import("./ws.js");
    expect(
      isStageEvent({
        request_id: "req-1",
        trace_id: "tr-1",
        provider_id: "p",
        upstream_model_id: "m",
        stage: "streaming",
        elapsed_ms: "not-a-number",
        status_code: 200,
        timestamp: "2025-01-01T00:00:00Z",
      }),
    ).toBe(false);
  });
});

describe("ws store — logsWsUrl", () => {
  it("appends the token as a query param on ws://", async () => {
    await setupAuthMock({ token: "my-api-key" });

    const { logsWsUrl } = await import("./ws.js");
    expect(logsWsUrl()).toBe(
      "ws://" + location.host + "/admin/ws?token=my-api-key",
    );
  });

  it("omits the token param when getToken returns null", async () => {
    await setupAuthMock({ token: null });

    const { logsWsUrl } = await import("./ws.js");
    const url: string = logsWsUrl();
    expect(url).toContain("ws://");
    expect(url).not.toContain("token=");
  });

  it("percent-encodes tokens with special characters", async () => {
    await setupAuthMock({ token: "abc+/==123" });

    const { logsWsUrl } = await import("./ws.js");
    const url: string = logsWsUrl();
    expect(url).toContain("token=abc%2B%2F%3D%3D123");
  });
});

describe("ws store — subscribeLogsStatus", () => {
  it("returns current status on first call", async () => {
    const { subscribeLogsStatus } = await import("./ws.js");
    const statuses: string[] = [];
    subscribeLogsStatus((s) => statuses.push(s));
    expect(statuses.length).toBe(1);
    expect(statuses[0]).toBe("disconnected");
  });

  it("notifies on status change", async () => {
    const { subscribeLogsStatus, setLogsStatus } = await import("./ws.js");
    const statuses: string[] = [];
    subscribeLogsStatus((s) => statuses.push(s));

    setLogsStatus("connected");
    setLogsStatus("disconnected");

    expect(statuses).toEqual(["disconnected", "connected", "disconnected"]);
  });

  it("unsubscribe stops further notifications", async () => {
    const { subscribeLogsStatus, setLogsStatus } = await import("./ws.js");
    const statuses: string[] = [];
    const unsub = subscribeLogsStatus((s) => statuses.push(s));

    setLogsStatus("connected");
    unsub();
    setLogsStatus("disconnected");

    expect(statuses).toEqual(["disconnected", "connected"]);
  });

  it("multiple subscribers all receive the same notification", async () => {
    const { subscribeLogsStatus, setLogsStatus } = await import("./ws.js");
    const a: string[] = [];
    const b: string[] = [];
    subscribeLogsStatus((s) => a.push(s));
    subscribeLogsStatus((s) => b.push(s));

    setLogsStatus("connected");

    expect(a).toEqual(["disconnected", "connected"]);
    expect(b).toEqual(["disconnected", "connected"]);
  });
});

describe("ws store — connectLogsWebSocket", () => {
  it("creates a WebSocket with the correct URL including token", async () => {
    await setupAuthMock({ token: "mock-token-123" });

    const { connectLogsWebSocket } = await import("./ws.js");
    connectLogsWebSocket();

    expect(MockWebSocket.instances.length).toBe(1);
    const ws = MockWebSocket.instances[0]!;
    expect(ws.url).toBe(
      "ws://" + location.host + "/admin/ws?token=mock-token-123",
    );
  });

  it("does not create a WS when getToken returns null", async () => {
    await setupAuthMock({ token: null });

    const { connectLogsWebSocket } = await import("./ws.js");
    connectLogsWebSocket();

    expect(MockWebSocket.instances.length).toBe(0);
  });

  it("sets status to 'connecting' for the initial attempt", async () => {
    const statuses = await trackStatuses();
    const { connectLogsWebSocket } = await import("./ws.js");
    connectLogsWebSocket();
    expect(statuses).toContain("connecting");
  });

  it("sends a subscribe message when lastAppliedCursor > 0", async () => {
    const { liveLogsStore } = await import("./live-logs-store.js");
    liveLogsStore.lastAppliedCursor = 42;

    const { connectLogsWebSocket } = await import("./ws.js");
    connectLogsWebSocket();

    const ws = MockWebSocket.instances[0]!;
    ws.simulateOpen();

    expect(ws.sent.length).toBe(1);
    expect(JSON.parse(ws.sent[0] as string)).toEqual({
      type: "subscribe",
      cursor: 42,
    });
  });

  it("does not send subscribe when lastAppliedCursor is 0", async () => {
    const { liveLogsStore } = await import("./live-logs-store.js");
    liveLogsStore.lastAppliedCursor = 0;

    const { connectLogsWebSocket } = await import("./ws.js");
    connectLogsWebSocket();

    const ws = MockWebSocket.instances[0]!;
    ws.simulateOpen();

    expect(ws.sent.length).toBe(0);
  });

  it("is idempotent: does not create a second WS while the first is open", async () => {
    await openWs();
    const { connectLogsWebSocket } = await import("./ws.js");
    connectLogsWebSocket();

    expect(MockWebSocket.instances.length).toBe(1);
  });
});

describe("ws store — reconnect on close", () => {
  it("schedules a reconnect after the WS closes", async () => {
    const statuses = await trackStatuses();
    const ws = await openWs();

    ws.close();

    expect(statuses).toContain("disconnected");

    // Advance timers past the first reconnect delay (250ms).
    await vi.advanceTimersByTimeAsync(300);

    expect(MockWebSocket.instances.length).toBe(2);
    expect(statuses).toContain("reconnecting");
  });

  it("uses exponential backoff on successive failures", async () => {
    const { connectLogsWebSocket } = await import("./ws.js");
    connectLogsWebSocket();
    MockWebSocket.instances[0]!.close();

    // First reconnect delay is 250ms (LOGS_WS_RECONNECT_DELAYS[0]).
    await vi.advanceTimersByTimeAsync(300);
    expect(MockWebSocket.instances.length).toBe(2);

    MockWebSocket.instances[1]!.close();

    // Second reconnect delay is 500ms (LOGS_WS_RECONNECT_DELAYS[1]).
    await vi.advanceTimersByTimeAsync(600);
    expect(MockWebSocket.instances.length).toBe(3);
  });

  it("resets reconnectAttempt on successful open", async () => {
    const { connectLogsWebSocket } = await import("./ws.js");
    connectLogsWebSocket();
    MockWebSocket.instances[0]!.close();
    await vi.advanceTimersByTimeAsync(300);

    MockWebSocket.instances[1]!.simulateOpen();

    MockWebSocket.instances[1]!.close();
    await vi.advanceTimersByTimeAsync(300);
    expect(MockWebSocket.instances.length).toBe(3);
  });
});

describe("ws store — heartbeat", () => {
  it("sends a ping every 15s", async () => {
    const ws = await openWs();

    expect(ws.sent.length).toBe(0);

    await vi.advanceTimersByTimeAsync(15_000);
    expect(ws.sent.length).toBe(1);
    expect(JSON.parse(ws.sent[0] as string)).toEqual({ type: "ping" });

    await vi.advanceTimersByTimeAsync(15_000);
    expect(ws.sent.length).toBe(2);
  });

  it("stops heartbeat when WS is closed", async () => {
    const ws = await openWs();

    ws.close();

    await vi.advanceTimersByTimeAsync(30_000);
    expect(ws.sent.length).toBe(0);
  });
});

describe("ws store — message dispatch", () => {
  it("dispatches valid JSON messages to ws-bus", async () => {
    const bus = await import("./ws-bus.js");
    const dispatchSpy = vi.spyOn(bus, "dispatchWs");

    const { setMessageHandler } = await import("./ws.js");
    setMessageHandler(() => {});

    const ws = await openWs();
    ws.simulateMessage(JSON.stringify({ type: "notification", data: { id: 1 } }));

    expect(dispatchSpy).toHaveBeenCalledOnce();
    expect(dispatchSpy).toHaveBeenCalledWith({
      type: "notification",
      data: { id: 1 },
    });
  });

  it("ignores malformed JSON without throwing", async () => {
    const bus = await import("./ws-bus.js");
    const dispatchSpy = vi.spyOn(bus, "dispatchWs");

    const { setMessageHandler } = await import("./ws.js");
    setMessageHandler(() => {});

    const ws = await openWs();

    expect(() => {
      ws.simulateMessage("not valid json {{{");
    }).not.toThrow();

    expect(dispatchSpy).not.toHaveBeenCalled();
  });

  it("ignores objects without a 'type' field", async () => {
    const bus = await import("./ws-bus.js");
    const dispatchSpy = vi.spyOn(bus, "dispatchWs");

    const { setMessageHandler } = await import("./ws.js");
    setMessageHandler(() => {});

    const ws = await openWs();
    ws.simulateMessage(JSON.stringify({ foo: "bar" }));

    expect(dispatchSpy).not.toHaveBeenCalled();
  });
});

describe("ws store — disconnectLogsWebSocket", () => {
  it("closes the WS, clears the timer, and sets status to disconnected", async () => {
    const statuses = await trackStatuses();
    await openWs();
    statuses.length = 0;

    const { disconnectLogsWebSocket, connectLogsWebSocket } = await import("./ws.js");
    disconnectLogsWebSocket();

    expect(statuses[statuses.length - 1]).toBe("disconnected");
    expect(statuses).toContain("disconnected");

    const before = MockWebSocket.instances.length;
    connectLogsWebSocket();
    expect(MockWebSocket.instances.length).toBe(before + 1);
  });

  it("is a no-op when no WS is active", async () => {
    const { disconnectLogsWebSocket } = await import("./ws.js");
    expect(() => disconnectLogsWebSocket()).not.toThrow();
  });
});

describe("ws store — error handling", () => {
  it("closes the WS when an error event fires", async () => {
    const ws = await openWs();

    ws.simulateError();

    expect(ws.readyState).toBe(MockWebSocket.CLOSED);
  });
});
