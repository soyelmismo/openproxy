import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createVisibilityAwareInterval } from "./visibility-aware-interval";

function setDocumentHidden(hidden: boolean): void {
  Object.defineProperty(document, "hidden", {
    configurable: true,
    value: hidden,
  });
}

describe("createVisibilityAwareInterval", () => {
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
    // Drop the own `hidden` property (value or getter) so the jsdom
    // prototype getter is visible again for the next test.
    Reflect.deleteProperty(document, "hidden");
  });

  it("does not invoke the callback before the first interval elapses", () => {
    const callback = vi.fn();
    const handle = createVisibilityAwareInterval(callback, 1_000);

    expect(vi.getTimerCount()).toBe(1);

    vi.advanceTimersByTime(999);
    expect(callback).not.toHaveBeenCalled();

    vi.advanceTimersByTime(1);
    expect(callback).toHaveBeenCalledTimes(1);

    handle.stop();
  });

  it("resumes the cadence after an async callback settles (advanceTimersByTimeAsync)", async () => {
    const callback = vi.fn(async () => {
      await Promise.resolve();
    });
    const handle = createVisibilityAwareInterval(callback, 1_000);

    await vi.advanceTimersByTimeAsync(1_000);
    expect(callback).toHaveBeenCalledTimes(1);

    vi.advanceTimersByTime(999);
    expect(callback).toHaveBeenCalledTimes(1);
    vi.advanceTimersByTime(1);
    expect(callback).toHaveBeenCalledTimes(2);

    handle.stop();
  });

  it("keeps no pending timer while the async callback promise is pending", async () => {
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const callback = vi.fn(() => gate);
    const handle = createVisibilityAwareInterval(callback, 1_000);

    await vi.advanceTimersByTimeAsync(1_000);
    expect(callback).toHaveBeenCalledTimes(1);
    expect(vi.getTimerCount()).toBe(0);

    release();
    await Promise.resolve();
    await Promise.resolve();
    expect(vi.getTimerCount()).toBe(1);

    vi.advanceTimersByTime(999);
    expect(callback).toHaveBeenCalledTimes(1);
    vi.advanceTimersByTime(1);
    expect(callback).toHaveBeenCalledTimes(2);

    handle.stop();
  });

  it("does not arm a timer when created while the document is hidden", () => {
    Object.defineProperty(document, "hidden", {
      configurable: true,
      get: () => true,
    });
    const callback = vi.fn();
    const handle = createVisibilityAwareInterval(callback, 1_000);

    expect(vi.getTimerCount()).toBe(0);

    vi.advanceTimersByTime(10_000);
    expect(callback).not.toHaveBeenCalled();

    handle.stop();
  });

  it("runs exactly one immediate catch-up tick when becoming visible by default", async () => {
    setDocumentHidden(true);
    const callback = vi.fn();
    const handle = createVisibilityAwareInterval(callback, 1_000);

    expect(vi.getTimerCount()).toBe(0);

    Object.defineProperty(document, "hidden", { value: false, configurable: true });
    document.dispatchEvent(new Event("visibilitychange"));
    expect(callback).toHaveBeenCalledTimes(1);

    await Promise.resolve();
    vi.advanceTimersByTime(1_000);
    expect(callback).toHaveBeenCalledTimes(2);

    handle.stop();
  });

  it("resumes cadence without an immediate catch-up when suppression is disabled", () => {
    setDocumentHidden(true);
    const callback = vi.fn();
    const handle = createVisibilityAwareInterval(callback, 1_000, {
      suppressMissedTicks: false,
    });

    Object.defineProperty(document, "hidden", { value: false, configurable: true });
    document.dispatchEvent(new Event("visibilitychange"));
    expect(callback).not.toHaveBeenCalled();
    expect(vi.getTimerCount()).toBe(1);

    vi.advanceTimersByTime(999);
    expect(callback).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(callback).toHaveBeenCalledTimes(1);

    handle.stop();
  });

  it("survives a callback that throws and keeps the cadence", async () => {
    const callback = vi.fn(() => {
      throw new Error("poll failed");
    });
    const handle = createVisibilityAwareInterval(callback, 1_000);

    vi.advanceTimersByTime(1_000);
    await Promise.resolve();
    expect(vi.getTimerCount()).toBe(1);

    vi.advanceTimersByTime(1_000);
    expect(callback).toHaveBeenCalledTimes(2);

    handle.stop();
  });

  it("makes stop idempotent and prevents timers or visibility from reactivating it", () => {
    const callback = vi.fn();
    const handle = createVisibilityAwareInterval(callback, 1_000);
    expect(vi.getTimerCount()).toBe(1);

    handle.stop();
    handle.stop();
    expect(vi.getTimerCount()).toBe(0);

    vi.advanceTimersByTime(10_000);
    document.dispatchEvent(new Event("visibilitychange"));
    expect(callback).not.toHaveBeenCalled();
  });

  it("removes its visibilitychange listener on stop with no leftovers across cycles", () => {
    const addSpy = vi.spyOn(document, "addEventListener");
    const removeSpy = vi.spyOn(document, "removeEventListener");
    const callback = vi.fn();

    for (let cycle = 0; cycle < 5; cycle += 1) {
      createVisibilityAwareInterval(callback, 1_000).stop();
    }

    const added = addSpy.mock.calls.filter((call) => call[0] === "visibilitychange").length;
    const removed = removeSpy.mock.calls.filter((call) => call[0] === "visibilitychange").length;
    expect(added).toBe(5);
    expect(removed).toBe(5);

    document.dispatchEvent(new Event("visibilitychange"));
    expect(callback).not.toHaveBeenCalled();
  });
});
