// state/theme.ts — theme bootstrap, toggle, and persistence.
// Light/dark is driven by data-theme on <html>. We persist the
// user choice in localStorage.openproxy-theme. If no choice is
// recorded we fall back to prefers-color-scheme.

import { THEME_STORAGE_KEY } from "../lib/constants.js";

export type Theme = "light" | "dark";

let current: Theme = "light";

function readStored(): string | null {
  try { return localStorage.getItem(THEME_STORAGE_KEY); }
  catch (_e: unknown) { return null; }
}

function writeStored(value: string): void {
  try { localStorage.setItem(THEME_STORAGE_KEY, value); }
  catch (_e: unknown) { /* private mode etc. */ }
}

function detectInitial(): Theme {
  const stored: string | null = readStored();
  if (stored === "light" || stored === "dark") return stored;
  if (typeof window !== "undefined" && window.matchMedia) {
    return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }
  return "light";
}

export function applyTheme(theme: Theme): void {
  current = theme === "dark" ? "dark" : "light";
  document.documentElement.setAttribute("data-theme", current);
  writeStored(current);
}

export function getTheme(): Theme { return current; }

// Toggle the theme, persist it, and re-emit a custom event so
// components (e.g. the theme toggle button) can repaint.
export function toggleTheme(): Theme {
  applyTheme(current === "dark" ? "light" : "dark");
  document.dispatchEvent(new CustomEvent("themechange", { detail: { theme: current } }));
  return current;
}

// Bootstrap must be called before the first render to avoid a
// flash of the wrong theme. Idempotent.
export function bootstrapTheme(): void {
  applyTheme(detectInitial());
}
