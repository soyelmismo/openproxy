// components/badge.ts — small inline status pill/badge/chip.
//
// Unified `pill()` is the core; `badge()` and `chip()` are
// backwards-compatible wrappers that set the default base class.
//
// Migrated to lit-html: the function now returns a `TemplateResult`
// instead of an HTML string. lit-html auto-escapes `${...}`
// interpolations, so the explicit `escapeHtml()` call is gone.

import { html, type TemplateResult } from "lit-html";

/**
 * Reusable inline pill that renders a `<span>` with an optional
 * base class and variant modifier.
 *
 * @param label    - content inside the pill
 * @param variant  - optional modifier class (e.g. "badge-error", "protocol")
 * @param baseClass - CSS base class ("badge" or "chip")
 */
export function pill(
  label: unknown,
  variant: string = "",
  baseClass: string = "badge",
): TemplateResult {
  const cls: string = variant ? `${baseClass} ${variant}` : baseClass;
  return html`<span class="${cls}">${label}</span>`;
}

// Backwards-compatible aliases

/** Inline status badge (base class "badge"). */
export function badge(label: string, variant: string = ""): TemplateResult {
  return pill(label, variant ? `badge-${variant}` : "", "badge");
}

/** Inline chip (base class "chip"). */
export function chip(text: unknown, variant: string = ""): TemplateResult {
  return pill(text, variant, "chip");
}
