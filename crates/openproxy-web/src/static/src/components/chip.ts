// components/chip.ts — inline chip (monospace text in a 1px
// bordered pill). Used for strategy names, virtual provider
// markers, and similar.

import { escapeHtml } from "../lib/escape.js";

export function chip(text: unknown, variant: string = ""): string {
  const cls: string = variant ? `chip ${variant}` : "chip";
  return `<span class="${cls}">${escapeHtml(text)}</span>`;
}
