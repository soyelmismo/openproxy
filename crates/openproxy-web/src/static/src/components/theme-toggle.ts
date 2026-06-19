// components/theme-toggle.ts — circular button anchored at the
// bottom-left of the sidebar. Click toggles data-theme between
// light and dark. Exposes a `mountThemeToggle()` symbol the
// sidebar can call after rendering.

import { toggleTheme, getTheme } from "../state/theme.js";

export function mountThemeToggle(): void {
  const slot: HTMLElement | null = document.getElementById("theme-toggle-slot");
  if (!slot) return;
  const existing: HTMLElement | null = document.getElementById("theme-toggle");
  if (existing) existing.remove();
  const btn: HTMLButtonElement = document.createElement("button");
  btn.id = "theme-toggle";
  btn.className = "theme-toggle";
  btn.type = "button";
  btn.title = "Toggle light / dark theme";
  btn.setAttribute("aria-label", "Toggle theme");
  btn.textContent = getTheme() === "dark" ? "☀" : "🌙";
  btn.addEventListener("click", () => {
    const next: "light" | "dark" = toggleTheme();
    btn.textContent = next === "dark" ? "☀" : "🌙";
  });
  slot.appendChild(btn);
}
