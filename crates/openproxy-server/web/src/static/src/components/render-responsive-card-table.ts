// components/render-responsive-card-table.ts — generic responsive
// table → cards render function.
//
// Desktop (≥768px): renders a `<table>` whose columns come from the
// `columns` prop. Each column has an optional `render(row)` for
// custom cells, or falls back to `String(row[key])`.
//
// Mobile (<768px): renders a stack of cards, one per row, with each
// column shown as `label: value` pairs. The card layout is pure CSS
// (`@media (max-width: 768px)` in components.css) — the markup is
// the same `<table>` with class `responsive-card-table` so existing
// stylesheets continue to apply, but the cell sequence inside a row
// uses a `data-label` attribute that the mobile rules pick up.
//
// Empty state: a single full-width row with `emptyMessage` (defaults
// to "No items.").
//
// CAUTION (Phase 2 deviation): the codebase already has a rich
// per-view responsive card layout (e.g. `tr.proxy-card-row > td
// .p-card-line-1`, `tr.api-key-card-row > td.mobile-key-card-cell`,
// `tr.model-card-row > td.mobile-model-card-cell`, etc.) that gives
// each table a much more polished mobile view than this generic
// helper can produce from column metadata alone. Until a view is
// deliberately migrated, prefer the per-view markup — this helper
// is intended for new tables (e.g. ad-hoc admin pages) or for
// callers that do not need the bespoke mobile card design.
//
// MIGRATED in Phase 2: views/proxies.ts, views/keys.ts.
// FOLLOW-UP (Phase 3+): views/proxy-sources.ts (drag/touch handlers
// require bespoke per-row markup), views/analytics.ts (3 distinct
// table shapes), views/providers.ts (imperative DOM patching on
// `model-row-*` rows, which would lose the focus-preserving
// in-place updates).

import { html, nothing, type TemplateResult } from "lit-html";
import { unsafeHTML } from "lit-html/directives/unsafe-html.js";

export interface ResponsiveColumn<T> {
  /** Column key — used for the `data-label` and as the fallback
   *  extractor when `render` is not provided. */
  key: string;
  /** Header text shown on desktop. Used as the `label` prefix on
   *  mobile card cells. */
  label: string;
  /** Optional cell renderer. Receives the row and returns either a
   *  `TemplateResult` (preferred) or a plain HTML string. Plain
   *  strings are embedded via `unsafeHTML` so callers can opt into
   *  raw markup (e.g. pre-styled status pills) without writing a
   *  full TemplateResult. */
  render?: (row: T) => TemplateResult | string;
  /** If true, this column is hidden on mobile (its `<td>` gets a
   *  `display: none` in the card view via CSS). */
  hiddenMobile?: boolean;
}

export interface ResponsiveTableProps<T> {
  columns: ResponsiveColumn<T>[];
  rows: readonly T[];
  /** Unique key per row. Used as the `<tr>` id and the `data-row-key`
   *  attribute. Required so lit-html can keep `<tr>` identity across
   *  partial re-renders. */
  rowKey: (row: T) => string | number;
  /** Shown when `rows` is empty. Default: "No items." */
  emptyMessage?: string;
  /** Optional class added to the wrapping `<table>`. */
  className?: string;
  /** Optional per-row class (e.g. status colouring: `alive`/`dead`).
   *  The helper always adds `responsive-card-row`; this is appended. */
  rowClass?: (row: T) => string;
}

function renderCell<T>(col: ResponsiveColumn<T>, row: T): TemplateResult {
  if (col.render) {
    const v: TemplateResult | string = col.render(row);
    return typeof v === "string" ? html`${unsafeHTML(v)}` : v;
  }
  const value: unknown = (row as Record<string, unknown>)[col.key];
  return html`${value == null ? "" : String(value)}`;
}

export function renderResponsiveCardTable<T>(props: ResponsiveTableProps<T>): TemplateResult {
  const { columns, rows, rowKey, emptyMessage, className, rowClass } = props;
  const cls: string = "responsive-card-table" + (className ? " " + className : "");
  const empty: string = emptyMessage ?? "No items.";

  if (!rows || rows.length === 0) {
    return html`
      <div class="table-wrap">
        <table class=${cls}>
          <tbody>
            <tr class="empty-row">
              <td colspan=${columns.length} class="empty-row">${empty}</td>
            </tr>
          </tbody>
        </table>
      </div>
    `;
  }

  return html`
    <div class="table-wrap">
      <table class=${cls}>
        <thead>
          <tr>
            ${columns.map((c) => html`<th>${c.label}</th>`)}
          </tr>
        </thead>
        <tbody>
          ${rows.map((row) => html`
            <tr data-row-key=${String(rowKey(row))} class=${("responsive-card-row" + (rowClass ? " " + rowClass(row) : "")).trimEnd()}>
              ${columns.map((c) => html`
                <td data-label=${c.label} class=${c.hiddenMobile ? "hidden-mobile" : ""}>
                  ${renderCell(c, row)}
                </td>
              `)}
            </tr>
          `)}
        </tbody>
      </table>
    </div>
  `;
}

// Re-export `nothing` for callers that want to omit a column value
// in their `render(row)` callback.
export { nothing };
