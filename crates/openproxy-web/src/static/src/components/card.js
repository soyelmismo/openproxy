// components/card.js — simple section wrapper. Mirrors the
// `section.card` / `.detail-section` look from the original CSS.

import { escapeHtml } from "../lib/escape.js";

export function card(title, body, opts = {}) {
  const cls = opts.variant === "detail" ? "detail-section" : "card";
  const h = title ? `<div class="section-header"><h3>${escapeHtml(title)}</h3>${opts.actions || ""}</div>` : "";
  return `<section class="${cls}">${h}${body}</section>`;
}
