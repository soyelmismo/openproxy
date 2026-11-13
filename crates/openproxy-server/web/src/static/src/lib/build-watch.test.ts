// Unit tests for the build fingerprint watchdog, under the jsdom localStorage
// and fetch stubs vitest provides.
import { describe, it, expect, beforeEach, vi, afterEach } from "vitest";

const STORAGE_KEY = "openproxy_build_id";

function stubBuild(id: string): void {
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => ({ ok: true, json: async () => ({ id }) })),
  );
}

describe("build watch", () => {
  let dispose: (() => void) | null = null;

  beforeEach(() => {
    localStorage.clear();
    vi.resetModules();
  });

  afterEach(() => {
    dispose?.();
    dispose = null;
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it("records the build id on the first boot without reloading", async () => {
    stubBuild("build-a");
    const reload = vi.fn();

    const { installBuildWatch } = await import("./build-watch.js");
    dispose = installBuildWatch(reload);
    await vi.waitFor(() => {
      expect(localStorage.getItem(STORAGE_KEY)).toBe("build-a");
    });
    expect(reload).not.toHaveBeenCalled();
  });

  it("reloads once when the served build changed under an open tab", async () => {
    localStorage.setItem(STORAGE_KEY, "build-old");
    stubBuild("build-new");
    const reload = vi.fn();

    const { installBuildWatch } = await import("./build-watch.js");
    dispose = installBuildWatch(reload);
    await vi.waitFor(() => {
      expect(reload).toHaveBeenCalledTimes(1);
    });
    // The new id is recorded before reloading, so the reload cannot loop.
    expect(localStorage.getItem(STORAGE_KEY)).toBe("build-new");
  });

  it("does nothing when the endpoint is unreachable", async () => {
    localStorage.setItem(STORAGE_KEY, "build-old");
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new Error("offline");
      }),
    );
    const reload = vi.fn();

    const { installBuildWatch } = await import("./build-watch.js");
    dispose = installBuildWatch(reload);
    await Promise.resolve();
    await Promise.resolve();
    expect(reload).not.toHaveBeenCalled();
    expect(localStorage.getItem(STORAGE_KEY)).toBe("build-old");
  });
});
