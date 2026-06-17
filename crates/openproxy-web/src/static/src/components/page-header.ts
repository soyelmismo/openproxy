// components/page-header.ts — small helper for the per-view
// <div class="page-header"> wrapper.

import { escapeHtml } from "../lib/escape.js";

export interface PageHeaderBack {
  href: string;
  label?: string;
}

export interface PageHeaderProps {
  title: string;
  back?: PageHeaderBack;
  actions?: string;
}

export function pageHeader(props: PageHeaderProps): string {
  const backHtml: string = props.back
    ? `<a href="${escapeHtml(props.back.href)}" class="back-link">${escapeHtml(props.back.label || "← Back")}</a>`
    : "";
  const actionsHtml: string = props.actions ? `<div class="actions">${props.actions}</div>` : "";
  return `<div class="page-header">${backHtml}<h2>${escapeHtml(props.title)}</h2>${actionsHtml}</div>`;
}
