// components/table.js — render a simple HTML table. Caller passes
// the column descriptors and rows. Keeps the views free of <th>
// /<td> noise.

import { escapeHtml, escapeAttr } from "../lib/escape.js";

export function renderTable({ columns, rows, empty }) {
  if (!rows || rows.length === 0) {
    return empty ? `<p class="empty">${escapeHtml(empty)}</p>` : "";
  }
  const thead = `<thead><tr>${columns.map(c => `<th>${escapeHtml(c.label || "")}</th>`).join("")}</tr></thead>`;
  const tbody = `<tbody>${rows.map((r) => {
    return `<tr${r.id != null ? ` data-id="${escapeAttr(r.id)}"` : ""}>${columns.map((c) => {
      const v = typeof c.render === "function" ? c.render(r) : (r[c.key] != null ? escapeHtml(String(r[c.key])) : "");
      return `<td>${v}</td>`;
    }).join("")}</tr>`;
  }).join("")}</tbody>`;
  return `<table>${thead}${tbody}</table>`;
}
