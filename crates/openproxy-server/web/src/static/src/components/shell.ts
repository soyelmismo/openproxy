// components/shell.ts — renders the top-level #app shell.

import { html, render } from 'lit-html';
import { renderSidebar } from "./sidebar.js";

export function mountShell(): void {
  const app: HTMLElement | null = document.getElementById("app");
  if (!app) return;
  const skipToMain = (e: Event): void => {
    e.preventDefault();
    const main = document.getElementById("main");
    if (main) main.focus();
  };
  render(
    html`<a class="skip-link" href="#main" @click=${skipToMain}>Skip to main content</a><aside class="sidebar" id="sidebar" aria-label="Sidebar"></aside><main id="main" tabindex="-1"></main><div id="workspace" hidden></div>`,
    app,
  );
  renderSidebar();
}
