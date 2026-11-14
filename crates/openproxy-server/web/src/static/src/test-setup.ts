import { vi } from "vitest";

// esbuild injects `__OP_PAYMENTS__` via `define` at bundle time (build.mjs,
// from OP_PAYMENTS). Vitest transforms sources itself, so it must supply the
// global here: the tests run the default (payments-enabled) build, matching
// what the production bundle carries unless the operator builds with
// OP_PAYMENTS=0.
(globalThis as unknown as Record<string, unknown>)["__OP_PAYMENTS__"] = true;

if (typeof window !== "undefined") {
  if (typeof window.matchMedia !== "function") {
    Object.defineProperty(window, "matchMedia", {
      configurable: true,
      writable: true,
      value: vi.fn().mockImplementation((q: string) => ({
        matches: false,
        media: q,
        onchange: null,
        addListener: (): void => {},
        removeListener: (): void => {},
        addEventListener: (): void => {},
        removeEventListener: (): void => {},
        dispatchEvent: (): boolean => false,
      })),
    });
  }
}
