// components/filter-bar.ts — small search/filter bar with a
// result count. The caller owns the actual filtering logic; we
// only render the input + count slot.

import { escapeHtml } from "../lib/escape.js";

export interface FilterBarProps {
  placeholder?: string;
  count?: number | null;
  extra?: string;
}

export function filterBar(props: FilterBarProps): string {
  const placeholder: string = props.placeholder ?? "Filter...";
  return `
    <div class="filter-bar">
      <input type="text" id="filter-bar-input" placeholder="${escapeHtml(placeholder)}" oninput="window.__filterBar && window.__filterBar(this.value)">
      <span class="filter-info">${props.count != null ? escapeHtml(String(props.count)) + " results" : ""}</span>
      ${props.extra || ""}
    </div>
  `;
}
