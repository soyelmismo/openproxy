// views/credits/quota.ts — quota bars of the logged-in key.
// Percentage + time-to-reset only, never hard dates (product rule).

import { html, type TemplateResult } from "lit-html";
import type { PlanStatus, QuotaWindow } from "./types.js";

function pct(win: QuotaWindow): number | null {
  if (win.cap_usd == null || win.cap_usd <= 0) return null;
  return Math.min(100, Math.round((win.spent_usd / win.cap_usd) * 100));
}

function leftText(left: number): string {
  const d = Math.floor(left / 86_400_000);
  const h = Math.floor((left % 86_400_000) / 3_600_000);
  const m = Math.floor((left % 3_600_000) / 60_000);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  return `${m}m`;
}

/** "resets in 2d 3h" style time-to-reset; never a hard date (product rule). */
function timeToReset(iso: string | null): string {
  if (!iso) return "";
  const left = new Date(iso).getTime() - Date.now();
  if (left <= 0) return "now";
  return `resets in ${leftText(left)}`;
}

/** Plan lifetime countdown; never a hard date (product rule). */
export function expiryText(iso: string | null): string {
  if (!iso) return "no expiry";
  const left = new Date(iso).getTime() - Date.now();
  if (left <= 0) return "expired";
  return `expires in ${leftText(left)}`;
}

/** The plan itself: credited balance as the limit, its TTL as the countdown. */
export function planCard(plan: PlanStatus): TemplateResult {
  const used = Math.max(0, plan.credit_usd - plan.balance_usd);
  const p = plan.credit_usd > 0 ? Math.min(100, Math.round((used / plan.credit_usd) * 100)) : 0;
  const tone = p > 80 ? "danger" : p > 50 ? "warn" : "ok";
  return html`
    <section class="card" style="margin-bottom:16px">
      <div class="card-title">Plan</div>
      <div class="card-body">
        <div style="display:flex;justify-content:space-between;align-items:baseline;font-size:13px">
          <span>${plan.name} — USDC ${plan.credit_usd.toFixed(2)} credit</span>
          <span style="font-weight:600">${p}% used</span>
        </div>
        <div class="quota-bar ${tone}" style="margin-top:8px">
          <div class="quota-bar-track" role="meter" aria-label="${plan.name} balance"
               aria-valuenow=${p} aria-valuemin="0" aria-valuemax="100">
            <div class="quota-bar-fill" style="width:${p}%"></div>
          </div>
        </div>
        <div style="font-size:11px;color:var(--color-text-muted);margin-top:4px">
          USDC ${plan.balance_usd.toFixed(2)} left · ${expiryText(plan.expires_at)}
        </div>
      </div>
    </section>`;
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
