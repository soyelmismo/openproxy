// components/card.ts — simple section wrapper. Mirrors the
// `section.card` / `.detail-section` look from the original CSS.
//
// Migrated from .js to .ts as part of G5 (views migration). The
// shim that briefly lived at `card.d.ts` is gone — this is the
// real source now. The three call sites in `views/` (analytics,
// combos, config, home, provider-detail) import this through
// `../components/card.js` (the .js emitted by tsc) and tsc
// resolves the import to this file.

import { escapeHtml } from "../lib/escape.js";

/**
 * Render a section card. `opts.variant === "detail"` switches to
 * the wider `.detail-section` look (used by the provider-detail
 * and logs view). `opts.actions` is a raw HTML string that goes
 * into the right side of the section header; the caller is
 * responsible for escaping any interpolated data.
 */
export function card(title: string, body: string, opts: { variant?: "detail"; actions?: string } = {}): string {
  const cls = opts.variant === "detail" ? "detail-section" : "card";
  const h = title
    ? `<div class="section-header"><h3>${escapeHtml(title)}</h3>${opts.actions || ""}</div>`
    : "";
  return `<section class="${cls}">${h}${body}</section>`;
}
