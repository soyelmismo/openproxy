// components/chip.ts — inline chip (monospace text in a 1px
// bordered pill). Used for strategy names, virtual provider
// markers, and similar.
//
// Migrated to lit-html: returns a `TemplateResult`; lit-html
// auto-escapes the interpolated text.

import { html, type TemplateResult } from "lit-html";

export function chip(text: unknown, variant: string = ""): TemplateResult {
  const cls: string = variant ? `chip ${variant}` : "chip";
  return html`<span class="${cls}">${text}</span>`;
}
