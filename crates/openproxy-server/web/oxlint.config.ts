import { defineConfig } from "oxlint";

export default defineConfig({
  ignorePatterns: [
    "**/dist/**",
    "**/node_modules/**",
    "tools/oxlint/**",
    "playwright.config.ts",
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

    // Desactivadas por ruido y fricción innecesaria
    "anti-slop/no-runtime-typeof": "off",
    "anti-slop/no-unsafe-dictionary-type": "off",
    "anti-slop/no-unknown-parameters": "off",
    "anti-slop/no-unknown-returns": "off",
    "anti-slop/no-unknown-type-aliases": "off",
    "anti-slop/no-object-parameters": "off",
    "anti-slop/no-shape-in-symbol-names": "off",
    "anti-slop/no-reflect-apply": "off",
    "anti-slop/no-reflect-get": "off",
    "anti-slop/no-widen-then-assert": "off",
    "anti-slop/require-safety-comment-for-type-assertion": "off",
  },
});
