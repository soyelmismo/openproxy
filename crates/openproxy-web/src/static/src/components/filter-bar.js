// components/filter-bar.js — small search/filter bar with a
// result count. The caller owns the actual filtering logic; we
// only render the input + count slot.

import { escapeHtml } from "../lib/escape.js";

export function filterBar({ placeholder = "Filter...", count, extra }) {
  return `
    <div class="filter-bar">
      <input type="text" id="filter-bar-input" placeholder="${escapeHtml(placeholder)}" oninput="window.__filterBar && window.__filterBar(this.value)">
      <span class="filter-info">${count != null ? escapeHtml(String(count)) + " results" : ""}</span>
      ${extra || ""}
    </div>
  `;
}
