// components/badge.js — small inline status badge. Reuses the
// `status-pill` look but exposes a slightly different vocabulary
// for static labels (cooldown, virtual provider, etc.).

import { escapeHtml } from "../lib/escape.js";

export function badge(label, variant = "") {
  const cls = variant ? `badge badge-${variant}` : "badge";
  return `<span class="${cls}">${escapeHtml(label)}</span>`;
}
