// components/card.ts — simple section wrapper. Mirrors the
// `section.card` / `.detail-section` look from the original CSS.
//
// Migrated from .js to .ts as part of G5 (views migration). The
// shim that briefly lived at `card.d.ts` is gone — this is the
// real source now. The call sites in `views/` (analytics, combos,
// config, home, provider-detail) import this through
// `../components/card.js` (the .js emitted by tsc) and tsc
// resolves the import to this file.
//
// Migrated to lit-html: returns a `TemplateResult`. `body` and
// `opts.actions` are raw HTML strings built by callers, so they
// are embedded via `unsafeHTML` (otherwise lit-html would escape
// the `<` `>` and render them as visible text). The `title` is
// plain text and goes through normal `${...}` interpolation.

import { html, type TemplateResult } from "lit-html";
import { unsafeHTML } from "lit-html/directives/unsafe-html.js";

/**
 * Render a section card. `opts.variant === "detail"` switches to
 * the wider `.detail-section` look (used by the provider-detail
 * and logs view). `opts.actions` is a raw HTML string that goes
 * into the right side of the section header; the caller is
 * responsible for escaping any interpolated data.
 */
export function card(
  title: string,
  body: string,
  opts: { variant?: "detail"; actions?: string } = {},
): TemplateResult {
  const cls = opts.variant === "detail" ? "detail-section" : "card";
  const header: TemplateResult = title
    ? html`<div class="section-header"><h3>${title}</h3>${opts.actions ? unsafeHTML(opts.actions) : null}</div>`
    : html``;
  return html`<section class="${cls}">${header}${unsafeHTML(body)}</section>`;
}
