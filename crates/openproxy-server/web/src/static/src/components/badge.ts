// components/badge.ts — small inline status badge. Reuses the
// `status-pill` look but exposes a slightly different vocabulary
// for static labels (cooldown, virtual provider, etc.).
//
// Migrated to lit-html: the function now returns a `TemplateResult`
// instead of an HTML string. lit-html auto-escapes `${...}`
// interpolations, so the explicit `escapeHtml()` call is gone.

import { html, type TemplateResult } from "lit-html";

export function badge(label: string, variant: string = ""): TemplateResult {
  const cls: string = variant ? `badge badge-${variant}` : "badge";
  return html`<span class="${cls}">${label}</span>`;
}
