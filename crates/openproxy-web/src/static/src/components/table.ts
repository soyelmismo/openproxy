// components/table.ts — render a simple HTML table. Caller passes
// the column descriptors and rows. Keeps the views free of <th>
// /<td> noise.

import { escapeHtml, escapeAttr } from "../lib/escape.js";

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

export function renderTable(props: TableProps): string {
  const { columns, rows, empty } = props;
  if (!rows || rows.length === 0) {
    return empty ? `<p class="empty">${escapeHtml(empty)}</p>` : "";
  }
  const thead: string = `<thead><tr>${columns.map((c) => `<th>${escapeHtml(c.label || "")}</th>`).join("")}</tr></thead>`;
  const tbody: string = `<tbody>${rows.map((r) => {
    const idVal: unknown = r["id"];
    const idAttr: string = idVal != null ? ` data-id="${escapeAttr(idVal)}"` : "";
    return `<tr${idAttr}>${columns.map((c) => {
      const v: string = typeof c.render === "function" ? c.render(r) : (r[c.key] != null ? escapeHtml(String(r[c.key])) : "");
      return `<td>${v}</td>`;
    }).join("")}</tr>`;
  }).join("")}</tbody>`;
  return `<table>${thead}${tbody}</table>`;
}
