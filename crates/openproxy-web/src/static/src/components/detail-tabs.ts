// components/detail-tabs.ts — render the tab strip used by the
// log detail modal. Returns the markup; the caller wires the
// click handler.

import { escapeHtml, escapeAttr } from "../lib/escape.js";

export interface DetailTab {
  id: string;
  label: string;
}

export function renderDetailTabs(tabs: readonly DetailTab[]): string {
  const buttons: string = tabs.map((t, i) =>
    `<button class="detail-tab ${i === 0 ? "active" : ""}" data-tab="${escapeAttr(t.id)}" role="tab">${escapeHtml(t.label)}</button>`
  ).join("");
  return `<div class="detail-tabs" role="tablist">${buttons}</div>`;
}

export function attachDetailTabHandlers(root: HTMLElement): void {
  const tabs: NodeListOf<Element> = root.querySelectorAll(".detail-tab");
  tabs.forEach((tab) => {
    const tabEl: HTMLElement = tab as HTMLElement;
    tabEl.addEventListener("click", () => {
      const target: string | undefined = tabEl.dataset["tab"];
      root.querySelectorAll(".detail-tab").forEach((t) => t.classList.toggle("active", t === tabEl));
      root.querySelectorAll(".detail-tab-panel").forEach((panel) => {
        const panelEl: HTMLElement = panel as HTMLElement;
        panelEl.classList.toggle("active", panelEl.dataset["panel"] === target);
      });
    });
  });
}
