import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { state } from "../state/index.js";
import * as apiModule from "../state/api.js";
import * as wsModule from "../state/ws.js";
import * as logHandlersModule from "../handlers/log-handlers.js";
import * as logDetailModule from "../components/log-detail/index.js";
import { clockStore } from "../state/clock-store.js";
import { mountLogs } from "./logs.js";

describe("mountLogs native mount target and lifecycle", () => {
  let container: HTMLElement;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    state.apiKeys = [];
    vi.spyOn(apiModule, "api").mockResolvedValue([]);
    vi.spyOn(wsModule, "connectLogsWebSocket").mockImplementation(() => {});
    vi.spyOn(wsModule, "disconnectLogsWebSocket").mockImplementation(() => {});
    vi.spyOn(logHandlersModule, "fetchRecordingState").mockImplementation(async () => {});
  });

  afterEach(() => {
    container.remove();
    document.body.innerHTML = "";
    vi.restoreAllMocks();
  });

  it("mounts into an explicit target HTMLElement and cleans up correctly", async () => {
    const addEventListenerSpy = vi.spyOn(document, "addEventListener");
    const removeEventListenerSpy = vi.spyOn(document, "removeEventListener");
    const clockUnsubSpy = vi.spyOn(clockStore, "unsubscribe");

    const cleanup = await mountLogs(container);
    expect(typeof cleanup).toBe("function");
    expect(container.innerHTML).not.toBe("");
    expect(wsModule.connectLogsWebSocket).toHaveBeenCalled();

    // Check that document click listener was bound
    const docClickCalls = addEventListenerSpy.mock.calls.filter(c => c[0] === "click");
    expect(docClickCalls.length).toBeGreaterThanOrEqual(1);
    const registeredListener = docClickCalls[docClickCalls.length - 1]![1];

    if (typeof cleanup === "function") {
      cleanup();
    }

    // Verify cleanup
    expect(removeEventListenerSpy).toHaveBeenCalledWith("click", registeredListener);
    expect(wsModule.disconnectLogsWebSocket).toHaveBeenCalled();
    expect(clockUnsubSpy).toHaveBeenCalled();
    // The host owns its container, cleanup must not remove #main or a tile target.
    expect(container.isConnected).toBe(true);
  });

  it("parses query parameters from the route argument rather than location.hash", async () => {
    const openLogDetailSpy = vi.spyOn(logDetailModule, "openLogDetail").mockResolvedValue(undefined);
    window.location.hash = "#/logs?trace_id=wrong_trace&request_id=wrong_req";

    const cleanup = await mountLogs(container, "#/logs?trace_id=custom_trace&request_id=custom_req");
    expect(openLogDetailSpy).toHaveBeenCalledWith("", "custom_req", "custom_trace");

    if (typeof cleanup === "function") {
      cleanup();
    }
  });

  it("falls back to #main when target is omitted", async () => {
    const main = document.createElement("main");
    main.id = "main";
    document.body.appendChild(main);

    const cleanup = await mountLogs();
    expect(typeof cleanup).toBe("function");
    expect(main.innerHTML).not.toBe("");

    if (typeof cleanup === "function") {
      cleanup();
    }
    main.remove();
  });
});
