// state/auth.test.ts — unit tests for the admin-token store.
//
// Verifies the spec contract from playground-refactor.md §8.A:
//   - login(token) persists the token to localStorage
//   - logout() clears the token
//   - getToken() returns the stored token if present, null otherwise
//   - getToken() caches the in-memory copy so the second call does
//     NOT hit localStorage again
//   - The token survives a simulated reload (localStorage round-trip)
//   - Trimming: pasted tokens with surrounding whitespace are cleaned
//   - Defensive: when localStorage throws (SecurityError, QuotaExceeded),
//     the in-memory copy is still updated so the current session keeps
//     working; logout tolerates the same failure
//
// All cases run under jsdom (vitest.config.ts) which provides a real
// localStorage global. We use vi.resetModules() per case so the
// module-local `currentToken` cache doesn't bleed between tests —
// every import re-runs the top-level state initialiser.

import { describe, it, expect, beforeEach, vi } from "vitest";

const STORAGE_KEY = "openproxy_admin_token";

describe("auth store", () => {
  beforeEach(() => {
    localStorage.clear();
    // Force a re-evaluation of the module so the module-local
    // `currentToken` cache is reset between cases.
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
    // An empty string is treated as "no token stored" by the loader.
    localStorage.setItem(STORAGE_KEY, "");
    const { getToken } = await import("./auth.js");
    expect(getToken()).toBeNull();
  });

  it("token survives a simulated reload via the localStorage round-trip", async () => {
    const { setToken } = await import("./auth.js");
    setToken("persistent-token");

    // Simulate a full page reload by resetting the module cache.
    // After the reset, the in-memory `currentToken` is null again —
    // the next getToken() must pull from localStorage.
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

    // Mutate localStorage behind the module's back. Because the
    // module-local `currentToken` is now non-null, getToken() must
    // return the cached value, not the new one.
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

      // The token is still usable for the current session even though
      // localStorage refused the write.
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
      // Must not throw — the in-memory cache is already null after
      // clearToken() runs, so the in-session behaviour is correct
      // regardless of localStorage availability.
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
});