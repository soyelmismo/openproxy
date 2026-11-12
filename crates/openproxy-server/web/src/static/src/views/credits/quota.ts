// views/credits/quota.ts — quota bars of the logged-in key.
// Percentage + time-to-reset only, never hard dates (product rule).

import { html, type TemplateResult } from "lit-html";
import type { QuotaWindow } from "./types.js";

function pct(win: QuotaWindow): number | null {
  if (win.cap_usd == null || win.cap_usd <= 0) return null;
  return Math.min(100, Math.round((win.spent_usd / win.cap_usd) * 100));
}

/** "resets in 2d 3h" style time-to-reset; never a hard date (product rule). */
function timeToReset(iso: string | null): string {
  if (!iso) return "";
  const left = new Date(iso).getTime() - Date.now();
  if (left <= 0) return "now";
  const d = Math.floor(left / 86_400_000);
  const h = Math.floor((left % 86_400_000) / 3_600_000);
  const m = Math.floor((left % 3_600_000) / 60_000);
  if (d > 0) return `resets in ${d}d ${h}h`;
  if (h > 0) return `resets in ${h}h ${m}m`;
  return `resets in ${m}m`;
}

export function quotaBar(label: string, win: QuotaWindow): TemplateResult {
  const p = pct(win);
  const cap = win.cap_usd == null ? "no cap" : `$${win.cap_usd.toFixed(2)} cap`;
  return html`
    <div style="margin-bottom:10px">
      <div style="display:flex;justify-content:space-between;font-size:12px;color:var(--muted)">
        <span>${label} — ${cap}</span>
        <span>${p == null ? "" : `${p}% used`}</span>
      </div>
      <div style="height:6px;border-radius:3px;background:var(--border,#232a3d);overflow:hidden">
        <div style="height:100%;width:${p == null ? 0 : p}%;background:var(--accent,#6366f1)"></div>
      </div>
      <div style="font-size:11px;color:var(--muted);margin-top:2px">
        $${win.spent_usd.toFixed(4)} spent ${timeToReset(win.resets_at)}
      </div>
    </div>`;
}
