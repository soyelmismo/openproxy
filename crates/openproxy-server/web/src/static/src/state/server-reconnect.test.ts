// state/server-reconnect.test.ts — unit tests for the server reconnect workflow.

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  triggerServerReconnect,
  isServerReconnecting,
  _resetServerReconnectForTest,
} from "./server-reconnect.js";

describe("server-reconnect", () => {
  beforeEach(() => {
    _resetServerReconnectForTest();
    vi.useFakeTimers();
  });

  afterEach(() => {
    _resetServerReconnectForTest();
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it("activates reconnecting state and attaches overlay", () => {
    expect(isServerReconnecting()).toBe(false);
    triggerServerReconnect();
    expect(isServerReconnecting()).toBe(true);
    const overlay = document.getElementById("server-reconnect-overlay");
    expect(overlay).not.toBeNull();
    expect(overlay?.classList.contains("visible")).toBe(true);
  });

  it("is idempotent when triggered multiple times", () => {
    triggerServerReconnect();
    triggerServerReconnect();
    const overlays = document.querySelectorAll("#server-reconnect-overlay");
    expect(overlays.length).toBe(1);
    expect(isServerReconnecting()).toBe(true);
  });

  it("reloads the page when liveness probe answers 200 OK", async () => {
    const reloadMock = vi.fn();
    Object.defineProperty(window, "location", {
      configurable: true,
      value: { reload: reloadMock },
    });

    vi.spyOn(globalThis, "fetch").mockResolvedValue(new Response("ok", { status: 200 }));

    triggerServerReconnect();
    expect(isServerReconnecting()).toBe(true);

    // Fast-forward initial poll timer
    await vi.advanceTimersByTimeAsync(1100);

    // Fast-forward reload timeout (300ms)
    await vi.advanceTimersByTimeAsync(350);

    expect(reloadMock).toHaveBeenCalled();
  });

  it("is never triggered by regular API errors (500, 502, network failure in api())", async () => {
    const { api } = await import("../lib/api.js");
    const fetchSpy = vi.spyOn(window, "fetch");

    // 502 from an upstream endpoint (e.g. sync models)
    fetchSpy.mockResolvedValue(new Response("Upstream model sync failed: 502", { status: 502 }));
    await expect(api("/providers/1/sync")).rejects.toThrow("502: Upstream model sync failed: 502");
    expect(isServerReconnecting()).toBe(false);

    // Network error in api()
    fetchSpy.mockRejectedValue(new TypeError("Failed to fetch"));
    await expect(api("/models")).rejects.toThrow("Failed to fetch");
    expect(isServerReconnecting()).toBe(false);

    const overlay = document.getElementById("server-reconnect-overlay");
    expect(overlay).toBeNull();
  });
});
