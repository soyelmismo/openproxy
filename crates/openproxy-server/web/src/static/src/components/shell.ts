// components/shell.ts — renders the top-level #app shell.

import { html, render } from 'lit-html';
import { renderSidebar } from "./sidebar.js";

export function mountShell(): void {
  const app: HTMLElement | null = document.getElementById("app");
  if (!app) return;
  render(html`<aside class="sidebar" id="sidebar"></aside><main id="main"></main><div id="workspace" hidden></div>`, app);
  renderSidebar();
}
