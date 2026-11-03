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

/** Stub `fetch` so `POST /admin/api/ws-ticket` answers with a ticket (or a failure) and
 *  records every call for assertions. */
interface RecordedFetch { url: string; init: RequestInit | undefined }
interface TicketFetchStub { calls: RecordedFetch[] }

function stubTicketFetch(opts: { ticket?: string; status?: number } = {}): TicketFetchStub {
  const calls: RecordedFetch[] = [];
  const status = opts.status ?? 200;
  const ticket = opts.ticket ?? "tkt-abc123";
  vi.stubGlobal("fetch", async (url: string, init?: RequestInit) => {
    calls.push({ url, init });
    const body = status === 200 ? JSON.stringify({ ticket, expires_in_secs: 30 }) : "unauthorized";
    return new Response(body, {
      status,
      headers: { "content-type": status === 200 ? "application/json" : "text/plain" },
    });
  });
  return { calls };
}

/** Kick off a connect and let the async ticket round-trip settle so the MockWebSocket exists. */
async function connectAndSettle(): Promise<void> {
  const { connectLogsWebSocket } = await import("./ws.js");
  connectLogsWebSocket();
  await vi.advanceTimersByTimeAsync(0);
}

async function openWs(): Promise<MockWebSocket> {
  await connectAndSettle();
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
  stubTicketFetch();
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

  it("never places the API key in the WebSocket URL", async () => {
    const { logsWsUrl, logsWsUrlWithTicket } = await import("./ws.js");
    expect(logsWsUrl()).toMatch(/^wss?:\/\/[^/]+\/admin\/ws$/);
    expect(logsWsUrl()).not.toContain("token=");
    const withTicket = logsWsUrlWithTicket("t/k+t=1");
    expect(withTicket).toContain("/admin/ws?ticket=t%2Fk%2Bt%3D1");
    expect(withTicket).not.toContain("test-token");
  });
});

describe("ws store — connection and cursors", () => {
  it("requests a ticket with the Bearer header, then connects with ?ticket=", async () => {
    const { calls } = stubTicketFetch({ ticket: "tkt-xyz" });
    const statuses = await trackStatuses();
    await connectAndSettle();
    expect(calls.length).toBe(1);
    expect(calls[0]!.url).toBe("/admin/api/ws-ticket");
    expect(calls[0]!.init?.method).toBe("POST");
    expect(new Headers(calls[0]!.init?.headers).get("Authorization")).toBe("Bearer test-token");
    expect(MockWebSocket.instances.length).toBe(1);
    expect(MockWebSocket.instances[0]!.url).toContain("/admin/ws?ticket=tkt-xyz");
    expect(MockWebSocket.instances[0]!.url).not.toContain("test-token");
    expect(statuses).toContain("connecting");
  });

  it("does not connect when token is missing (no ticket request either)", async () => {
    const { calls } = stubTicketFetch();
    await setupAuthMock({ token: null });
    await connectAndSettle();
    expect(calls.length).toBe(0);
    expect(MockWebSocket.instances.length).toBe(0);
  });

  it("schedules a reconnect when the ticket request fails", async () => {
    stubTicketFetch({ status: 401 });
    const statuses = await trackStatuses();
    await connectAndSettle();
    expect(MockWebSocket.instances.length).toBe(0);
    expect(statuses).toContain("disconnected");
    // Backoff elapsed → a fresh ticket attempt (still failing → still no WS).
    await vi.advanceTimersByTimeAsync(300);
    expect(MockWebSocket.instances.length).toBe(0);
    expect(statuses).toContain("reconnecting");
  });

  it("handles cursor subscribe messaging", async () => {
    const { liveLogsStore } = await import("./live-logs-store.js");
    const { disconnectLogsWebSocket } = await import("./ws.js");
    liveLogsStore.lastAppliedCursor = 42;
    await connectAndSettle();
    const ws1 = MockWebSocket.instances[0]!;
    ws1.simulateOpen();
    expect(ws1.sent).toEqual([JSON.stringify({ type: "subscribe", cursor: 42 })]);

    disconnectLogsWebSocket();
    MockWebSocket.instances = [];
    liveLogsStore.lastAppliedCursor = 0;
    await connectAndSettle();
    const ws2 = MockWebSocket.instances[0]!;
    ws2.simulateOpen();
    expect(ws2.sent.length).toBe(0);
  });

  it("is idempotent: does not create a second WS while first is open", async () => {
    await openWs();
    await connectAndSettle();
    expect(MockWebSocket.instances.length).toBe(1);
  });

  it("is idempotent: a second connect during the ticket round-trip is coalesced", async () => {
    const { connectLogsWebSocket } = await import("./ws.js");
    connectLogsWebSocket();
    connectLogsWebSocket();
    await vi.advanceTimersByTimeAsync(0);
    expect(MockWebSocket.instances.length).toBe(1);
  });

  it("discards a ticket that resolves after disconnect", async () => {
    const { connectLogsWebSocket, disconnectLogsWebSocket } = await import("./ws.js");
    connectLogsWebSocket();
    disconnectLogsWebSocket();
    await vi.advanceTimersByTimeAsync(0);
    expect(MockWebSocket.instances.length).toBe(0);
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
