// components/theme-toggle.ts — circular button anchored at the
// bottom-left of the sidebar. Click toggles data-theme between
// light and dark. Exposes a `mountThemeToggle()` symbol the
// sidebar can call after rendering.

import { toggleTheme, getTheme } from "../state/theme.js";

export function mountThemeToggle(): void {
  const sb: HTMLElement | null = document.querySelector(".sidebar");
  if (!sb) return;
  // Remove any previous toggle to keep the sidebar idempotent.
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
  // Theme transitions are owned by the CSS; we just append the node.
  sb.appendChild(btn);
}
