// components/shell.js — renders the top-level #app shell: a grid
// with a sidebar and a <main id="main">. Called once on boot.

import { renderSidebar } from "./sidebar.js";

export function mountShell() {
  const app = document.getElementById("app");
  if (!app) return;
  app.innerHTML = `
    <aside class="sidebar" id="sidebar"></aside>
    <main id="main"></main>
  `;
  renderSidebar();
}
