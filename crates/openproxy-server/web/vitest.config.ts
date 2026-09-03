// vitest.config.ts — Vitest configuration for the dashboard unit tests.
//
// Defaults match the existing test runner (auto-discover
// src/static/src/**/*.test.ts, run with `vitest run`). We pin the
// environment to jsdom because the auth and ws stores interact with
// localStorage and the WebSocket API respectively — Node's native
// globals lack both. The live-logs-store test does not touch the DOM,
// but jsdom is harmless for it and avoids per-file environment
// overrides.
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    environment: "jsdom",
    include: ["src/static/src/**/*.test.ts"],
    // The auth store mutates module-local state (the cached token).
    // Run each file in isolation so file ordering can't bleed tokens
    // between cases.
    isolate: true,
  },
});