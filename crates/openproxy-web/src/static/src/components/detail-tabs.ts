// components/detail-tabs.ts — render the tab strip used by the
// log detail modal. Returns the markup; the caller wires the
// click handler.
//
// Migrated to lit-html: returns a `TemplateResult`. The tab id
// is interpolated into a `data-tab` attribute (lit-html
// auto-escapes attribute values), and `?class`-style active
// state is replaced with a conditional class string. The
// `attachDetailTabHandlers` helper still walks the rendered DOM
// — it works the same whether the markup came from a string or
// from a lit-html template.

import { html, type TemplateResult } from "lit-html";

export interface DetailTab {
  id: string;
  label: string;
}

export function renderDetailTabs(tabs: readonly DetailTab[]): TemplateResult {
  return html`<div class="detail-tabs" role="tablist">
    ${tabs.map(
      (t, i) =>
        html`<button
          class="detail-tab ${i === 0 ? "active" : ""}"
          data-tab="${t.id}"
          role="tab"
        >
          ${t.label}
        </button>`,
    )}
  </div>`;
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
