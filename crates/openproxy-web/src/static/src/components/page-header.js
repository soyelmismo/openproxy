// components/page-header.js — small helper for the per-view
// <div class="page-header"> wrapper.

import { escapeHtml } from "../lib/escape.js";

export function pageHeader({ title, back, actions }) {
  const backHtml = back ? `<a href="${escapeHtml(back.href)}" class="back-link">${escapeHtml(back.label || "← Back")}</a>` : "";
  const actionsHtml = actions ? `<div class="actions">${actions}</div>` : "";
  return `<div class="page-header">${backHtml}<h2>${escapeHtml(title)}</h2>${actionsHtml}</div>`;
}
