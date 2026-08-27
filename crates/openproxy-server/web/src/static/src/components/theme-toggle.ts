// components/theme-toggle.ts — circular button anchored at the
// bottom-left of the sidebar. Click toggles data-theme between
// light and dark. Exposes a `mountThemeToggle()` symbol the
// sidebar can call after rendering.
//
// Migrated to lit-html: the button markup is now produced by a
// lit-html template and rendered into the slot with `render()`.
// The click handler is wired via `@click` instead of
// `addEventListener`. The `mountThemeToggle()` signature is
// unchanged (still `void`).

import { html, render } from "lit-html";
import { toggleTheme, getTheme } from "../state/theme.js";
import { icons } from "../lib/icons.js";

function renderThemeToggle(slot: HTMLElement): void {
  const isDark = getTheme() === "dark";
  render(
    html`<button
      id="theme-toggle"
      class="theme-toggle"
      type="button"
      title="Toggle light / dark theme"
      aria-label="Toggle theme"
      @click=${(): void => {
        toggleTheme();
        // Re-render so the icon swaps to the new theme.
        renderThemeToggle(slot);
      }}
    >
      ${isDark ? icons.sun() : icons.moon()}
    </button>`,
    slot,
  );
}

export function mountThemeToggle(): void {
  const slot: HTMLElement | null = document.getElementById("theme-toggle-slot");
  if (!slot) return;
  // lit-html's `render()` diffs against the existing children, so
  // calling this on a slot that already holds a previously-mounted
  // button simply updates its content rather than appending a
  // duplicate. (The old imperative code had to explicitly remove
  // the previous button; we no longer need to.)
  renderThemeToggle(slot);
}
