import { defineConfig } from "oxlint";

export default defineConfig({
  ignorePatterns: [
    "**/dist/**",
    "**/node_modules/**",
    "tools/oxlint/**",
    "playwright.config.js",
    "tests/**",
  ],
  jsPlugins: [
    {
      name: "anti-slop",
      specifier: "./tools/oxlint/anti-slop/index.ts",
    },
  ],
  rules: {
    // Alta señal (bugs reales / hacks de tipado)
    "anti-slop/no-chained-type-assertions": "error",
    "anti-slop/no-conditional-empty-object-spread": "error",
    "anti-slop/no-known-value-widening": "error",
    "anti-slop/no-module-mocking": "error",
  },
});
