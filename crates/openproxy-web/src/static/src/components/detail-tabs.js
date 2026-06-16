// components/detail-tabs.js — render the tab strip used by the
// log detail modal. Returns the markup; the caller wires the
// click handler.

import { escapeHtml, escapeAttr } from "../lib/escape.js";

export function renderDetailTabs(tabs) {
  const buttons = tabs.map((t, i) =>
    `<button class="detail-tab ${i === 0 ? "active" : ""}" data-tab="${escapeAttr(t.id)}" role="tab">${escapeHtml(t.label)}</button>`
  ).join("");
  return `<div class="detail-tabs" role="tablist">${buttons}</div>`;
}

export function attachDetailTabHandlers(root) {
  const tabs = root.querySelectorAll(".detail-tab");
  tabs.forEach((tab) => {
    tab.addEventListener("click", () => {
      const target = tab.dataset.tab;
      root.querySelectorAll(".detail-tab").forEach((t) => t.classList.toggle("active", t === tab));
      root.querySelectorAll(".detail-tab-panel").forEach((panel) => {
        panel.classList.toggle("active", panel.dataset.panel === target);
      });
    });
  });
}
