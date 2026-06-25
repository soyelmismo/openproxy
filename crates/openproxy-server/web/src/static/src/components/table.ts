// components/table.ts — render a simple HTML table. Caller passes
// the column descriptors and rows. Keeps the views free of <th>
// /<td> noise.
//
// Migrated to lit-html: returns a `TemplateResult`. Column
// `render(r)` callbacks still return HTML strings (callers build
// them with `escapeHtml`); those strings are embedded via
// `unsafeHTML` so the markup is honoured. Cell values that come
// straight from the row go through normal `${...}` interpolation
// and are auto-escaped by lit-html. The `data-id` attribute uses
// lit-html's `nothing` sentinel so the attribute is omitted
// entirely when the row has no `id`.

import { html, nothing, type TemplateResult } from "lit-html";
import { unsafeHTML } from "lit-html/directives/unsafe-html.js";

export interface TableColumn {
  key: string;
  label?: string;
  render?: (row: Record<string, unknown>) => string;
}

export interface TableProps {
  columns: readonly TableColumn[];
  rows: readonly Record<string, unknown>[];
  empty?: string;
}

export function renderTable(props: TableProps): TemplateResult {
  const { columns, rows, empty } = props;
  if (!rows || rows.length === 0) {
    return empty ? html`<p class="empty">${empty}</p>` : html``;
  }
  return html`<table>
    <thead>
      <tr>${columns.map((c) => html`<th>${c.label || ""}</th>`)}</tr>
    </thead>
    <tbody>
      ${rows.map((r) => {
        const idVal: unknown = r["id"];
        return html`<tr
          data-id=${idVal != null ? String(idVal) : nothing}
        >${columns.map((c) => {
          // Column `render` callbacks return pre-built HTML strings
          // (callers use escapeHtml inside them); those need
          // `unsafeHTML` so the markup is honoured. Plain cell values
          // are interpolated directly so lit-html auto-escapes them.
          if (typeof c.render === "function") {
            return html`<td>${unsafeHTML(c.render(r))}</td>`;
          }
          const raw: unknown = r[c.key];
          return html`<td>${raw != null ? String(raw) : ""}</td>`;
        })}</tr>`;
      })}
    </tbody>
  </table>`;
}
