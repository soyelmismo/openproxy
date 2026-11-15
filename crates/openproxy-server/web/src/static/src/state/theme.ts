// state/theme.ts — theme bootstrap, toggle, and persistence.
// Light/dark is driven by data-theme on <html>. We persist the
// user choice in localStorage.openproxy-theme. If no choice is
// recorded we fall back to prefers-color-scheme.

import { THEME_STORAGE_KEY } from "../lib/constants.js";

export type Theme = "light" | "dark";

let current: Theme = "light";

function readStored(): string | null {
  try { return localStorage.getItem(THEME_STORAGE_KEY); }
  catch { return null; }
}

function writeStored(value: string): void {
  try { localStorage.setItem(THEME_STORAGE_KEY, value); }
  catch { /* private mode etc. */ }
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
  document.documentElement.style.colorScheme = current;
  document.documentElement.style.backgroundColor = current === "dark" ? "#151413" : "#f5f7fa";
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

let mediaSubscribed = false;

// Bootstrap must be called before the first render to avoid a
// flash of the wrong theme. Idempotent.
export function bootstrapTheme(): void {
  const stored = readStored();
  if (stored === "light" || stored === "dark") {
    applyTheme(stored);
  } else {
    current = detectInitial();
    document.documentElement.setAttribute("data-theme", current);
    document.documentElement.style.colorScheme = current;
    document.documentElement.style.backgroundColor = current === "dark" ? "#151413" : "#f5f7fa";
  }
  if (!mediaSubscribed && typeof window !== "undefined" && window.matchMedia) {
    mediaSubscribed = true;
    const mql = window.matchMedia("(prefers-color-scheme: dark)");
    mql.addEventListener?.("change", (e: MediaQueryListEvent) => {
      const explicit = readStored();
      if (explicit === "light" || explicit === "dark") return;
      current = e.matches ? "dark" : "light";
      document.documentElement.setAttribute("data-theme", current);
      document.documentElement.style.colorScheme = current;
      document.documentElement.style.backgroundColor = current === "dark" ? "#151413" : "#f5f7fa";
      document.dispatchEvent(new CustomEvent("themechange", { detail: { theme: current } }));
    });
  }
}
