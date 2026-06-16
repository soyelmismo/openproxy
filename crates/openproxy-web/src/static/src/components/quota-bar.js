// components/quota-bar.js — horizontal progress bar used in the
// account-detail quota view.

import { escapeHtml } from "../lib/escape.js";

export function quotaBar({ label, used, total, warn = 0.8, error = 0.95 }) {
  if (!total || total <= 0) return "";
  const pct = Math.max(0, Math.min(1, used / total));
  const cls = pct >= error ? "error" : pct >= warn ? "warn" : "";
  return `
    <div class="quota-bar" title="${escapeHtml(label || "")}">
      <div class="quota-bar-track">
        <div class="quota-bar-fill ${cls}" style="width:${(pct * 100).toFixed(1)}%"></div>
      </div>
      <div class="quota-bar-label">${escapeHtml(label || "")}</div>
    </div>
  `;
}
