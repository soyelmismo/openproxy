// Unit tests for the admin-token store, under the jsdom localStorage that
// vitest.config.ts provides.

import { describe, it, expect, beforeEach, vi } from "vitest";

const STORAGE_KEY = "openproxy_admin_token";

describe("auth store", () => {
  beforeEach(() => {
    localStorage.clear();
    // The module-local `currentToken` cache must not bleed between cases.
    vi.resetModules();
  });

  it("login persists the token to localStorage", async () => {
    const { setToken, getToken } = await import("./auth.js");
    setToken("test-token");
    expect(getToken()).toBe("test-token");
    expect(localStorage.getItem(STORAGE_KEY)).toBe("test-token");
  });

  it("logout clears the token from localStorage and the in-memory cache", async () => {
    const { setToken, clearToken, getToken } = await import("./auth.js");
    setToken("to-be-cleared");
    expect(getToken()).toBe("to-be-cleared");

    clearToken();

    expect(getToken()).toBeNull();
    expect(localStorage.getItem(STORAGE_KEY)).toBeNull();
  });

  it("getToken returns the stored token when one exists", async () => {
    localStorage.setItem(STORAGE_KEY, "preexisting-token");
    const { getToken } = await import("./auth.js");
    expect(getToken()).toBe("preexisting-token");
  });

  it("getToken returns null when no token is stored", async () => {
    const { getToken } = await import("./auth.js");
    expect(getToken()).toBeNull();
  });

  it("getToken returns null when the stored value is empty", async () => {
    // An empty string counts as "no token stored".
    localStorage.setItem(STORAGE_KEY, "");
    const { getToken } = await import("./auth.js");
    expect(getToken()).toBeNull();
  });

  it("token survives a simulated reload via the localStorage round-trip", async () => {
    const { setToken } = await import("./auth.js");
    setToken("persistent-token");

    // After the reset `currentToken` is null, so getToken() must re-read localStorage.
    vi.resetModules();
    const { getToken } = await import("./auth.js");
    expect(getToken()).toBe("persistent-token");
  });

  it("setToken trims surrounding whitespace from pasted tokens", async () => {
    const { setToken, getToken } = await import("./auth.js");
    setToken("  paste-with-newlines\n");
    expect(getToken()).toBe("paste-with-newlines");
    expect(localStorage.getItem(STORAGE_KEY)).toBe("paste-with-newlines");
  });

  it("isLoggedIn mirrors getToken", async () => {
    const { setToken, isLoggedIn, clearToken } = await import("./auth.js");
    expect(isLoggedIn()).toBe(false);
    setToken("now-logged-in");
    expect(isLoggedIn()).toBe(true);
    clearToken();
    expect(isLoggedIn()).toBe(false);
  });

  it("getToken caches the result so the second call does not re-read localStorage", async () => {
    localStorage.setItem(STORAGE_KEY, "cached-token");
    const { getToken } = await import("./auth.js");

    expect(getToken()).toBe("cached-token");

    // A non-null `currentToken` means the new value is ignored.
    localStorage.setItem(STORAGE_KEY, "tampered-token");

    expect(getToken()).toBe("cached-token");
  });

  it("falls back to null when localStorage.getItem throws (SecurityError)", async () => {
    const spy = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("SecurityError: storage disabled");
    });

    try {
      const { getToken } = await import("./auth.js");
      expect(getToken()).toBeNull();
    } finally {
      spy.mockRestore();
    }
  });

  it("setToken keeps the in-memory token when localStorage.setItem throws", async () => {
    const spy = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("QuotaExceededError");
    });
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});

    try {
      const { setToken, getToken } = await import("./auth.js");
      setToken("session-only-token");

      // Usable for this session despite the refused write.
      expect(getToken()).toBe("session-only-token");
      expect(warn).toHaveBeenCalled();
    } finally {
      spy.mockRestore();
      warn.mockRestore();
    }
  });

  it("clearToken tolerates localStorage.removeItem throwing", async () => {
    const { setToken, clearToken } = await import("./auth.js");
    setToken("about-to-clear");

    const spy = vi.spyOn(Storage.prototype, "removeItem").mockImplementation(() => {
      throw new Error("SecurityError");
    });

    try {
      // The in-memory cache is already null, so localStorage is moot.
      expect(() => clearToken()).not.toThrow();
    } finally {
      spy.mockRestore();
    }
  });

  it("clearToken removes the value from localStorage on the happy path", async () => {
    const { setToken, clearToken } = await import("./auth.js");
    setToken("about-to-clear");
    expect(localStorage.getItem(STORAGE_KEY)).toBe("about-to-clear");

    clearToken();

    expect(localStorage.getItem(STORAGE_KEY)).toBeNull();
  });

  it("isCurrentSessionKey detects current key by id or prefix", async () => {
    const { setToken, isCurrentSessionKey } = await import("./auth.js");
    const { state } = await import("./index.js");

    setToken("op_live_MYKEY1234567890abcdef");
    state.apiKeys = [
      { id: 42, key_prefix: "op_live_MYKE", label: "My Key" } as any,
      { id: 99, key_prefix: "op_live_OTHER", label: "Other Key" } as any,
    ];

    expect(isCurrentSessionKey(42)).toBe(true);
    expect(isCurrentSessionKey(99)).toBe(false);
    expect(isCurrentSessionKey({ id: 42 })).toBe(true);
    expect(isCurrentSessionKey({ id: 99 })).toBe(false);
    expect(isCurrentSessionKey({ key_prefix: "op_live_MYKE" })).toBe(true);
    expect(isCurrentSessionKey({ key_prefix: "op_live_OTHER" })).toBe(false);
  });

  it("invalidateSession clears token and updates hash to #/login", async () => {
    const { setToken, getToken, invalidateSession } = await import("./auth.js");
    setToken("op_live_MYKEY");
    location.hash = "#/keys";

    invalidateSession();

    expect(getToken()).toBeNull();
    expect(location.hash).toBe("#/login");
  });

  it("isFullAdmin fails closed while the profile is unknown", async () => {
    const { isFullAdmin } = await import("./auth.js");
    // A stored token with no profile yet: the boot hydration is what proves
    // the session, so the shell must not assume operator rights.
    localStorage.setItem("openproxy_admin_token", "op_live_UNKNOWN");
    expect(isFullAdmin()).toBe(false);
  });

  it("isFullAdmin follows the persisted profile", async () => {
    const { isFullAdmin, setProfile } = await import("./auth.js");
    setProfile({ key_id: 1, key_prefix: "op_live_oper", label: "Ops", scopes: ["manage"], is_admin: true });
    expect(isFullAdmin()).toBe(true);

    setProfile({ key_id: 2, key_prefix: "op_live_cust", label: "Customer", scopes: ["chat"], is_admin: false });
    expect(isFullAdmin()).toBe(false);
  });
});