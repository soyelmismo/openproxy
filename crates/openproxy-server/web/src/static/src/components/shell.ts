// components/shell.ts — renders the top-level #app shell.
// Migrated to lit-html.

import { html, render } from 'lit-html';
import { renderSidebar } from "./sidebar.js";

export function mountShell(): void {
  const app: HTMLElement | null = document.getElementById("app");
  if (!app) return;
  render(html`<aside class="sidebar" id="sidebar"></aside><main id="main"></main>`, app);
  renderSidebar();
}
