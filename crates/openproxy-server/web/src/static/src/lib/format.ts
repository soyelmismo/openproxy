// lib/format.ts — small pure formatters.

import { html, type TemplateResult } from "lit-html";

export function formatContext(n: unknown): string {
  if (n == null) return "—";
  const num = Number(n);
  if (!Number.isFinite(num)) return "—";
  if (num < 1000) return String(num);
  if (num < 10000) return (num / 1000).toFixed(1) + "k";
  if (num < 1_000_000) return Math.round(num / 1000) + "k";
  return (num / 1_000_000).toFixed(1) + "M";
}

// TemplateResult variant of `formatContext`. Used inside `<td>`s
// where the null case must render as a muted `<span>` (em-dash
// inside an element, not escaped text) so the column width stays
// constant across rows. Mirrors the legacy inlined copy that used
// to live in `components/model-table.ts` and `views/providers.ts`.
export function formatContextBadge(tokens: number | null | undefined): TemplateResult {
  if (tokens == null) return html`<span class="muted">—</span>`;
  if (tokens >= 1_000_000) return html`${(tokens / 1_000_000).toFixed(1)}M`;
  if (tokens >= 1000) return html`${Math.round(tokens / 1000)}k`;
  return html`${String(tokens)}`;
}

export function formatCost(usd: unknown): string {
  return "$" + (Number(usd) || 0).toFixed(4);
}

export function formatMs(ms: unknown): string {
  if (ms == null) return "—";
  return Math.round(Number(ms)) + "ms";
}

// Localised-friendly number for currency / counts. Not a full i18n
// helper — just a one-liner we use in a few places.
export function formatNumber(n: number, opts: Intl.NumberFormatOptions = {}): string {
  return new Intl.NumberFormat(undefined, opts).format(n);
}
