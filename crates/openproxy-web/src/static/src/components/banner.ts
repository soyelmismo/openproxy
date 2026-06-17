// components/banner.ts — small inline banner used by the
// Config view (info + success variants) and the error fallback.

import { escapeHtml } from "../lib/escape.js";

export function banner(title: string, body: string, variant: string = "info"): string {
  return `<div class="banner banner-${variant}"><strong>${escapeHtml(title)}</strong> ${escapeHtml(body)}</div>`;
}
