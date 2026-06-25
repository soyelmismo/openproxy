// components/page-header.ts — small helper for the per-view
// `<div class="page-header">` wrapper.
//
// Migrated to lit-html: returns a `TemplateResult`. The
// `actions` slot is a raw HTML string coming from the caller
// (e.g. provider-grid.ts passes `<button>…</button>` markup);
// it is embedded via `unsafeHTML` so the buttons render as
// elements rather than being escaped to text. The `back.href`
// and `back.label` go through normal `${...}` interpolation,
// which lit-html auto-escapes.

import { html, type TemplateResult } from "lit-html";
import { unsafeHTML } from "lit-html/directives/unsafe-html.js";

export interface PageHeaderBack {
  href: string;
  label?: string;
}

export interface PageHeaderProps {
  title: string;
  back?: PageHeaderBack;
  /** Raw HTML string rendered into the actions slot. */
  actions?: string;
}

export function pageHeader(props: PageHeaderProps): TemplateResult {
  const back: TemplateResult = props.back
    ? html`<a href="${props.back.href}" class="back-link">${props.back.label || "← Back"}</a>`
    : html``;
  const actions: TemplateResult = props.actions
    ? html`<div class="actions">${unsafeHTML(props.actions)}</div>`
    : html``;
  return html`<div class="page-header">${back}<h2>${props.title}</h2>${actions}</div>`;
}
