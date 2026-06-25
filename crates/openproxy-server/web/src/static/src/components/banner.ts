// components/banner.ts — small inline banner used by the
// Config view (info + success variants) and the error fallback.
//
// Migrated to lit-html: returns a `TemplateResult` and relies on
// lit-html's automatic escaping for `title` / `body`.

import { html, type TemplateResult } from "lit-html";

export function banner(title: string, body: string, variant: string = "info"): TemplateResult {
  return html`<div class="banner banner-${variant}"><strong>${title}</strong> ${body}</div>`;
}
