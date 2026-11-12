// views/credits/topup.ts — the buyer's top-up panel: package cards
// (pay $X → spend $Y) + the QR checkout that polls until paid.

import { html, type TemplateResult } from "lit-html";
import { api } from "../../state/api.js";
import { showToast } from "../../components/toast.js";
import {
  activeOrder, requestRefresh, selectedPackageId,
  setActiveOrder, setSelectedPackageId, setTopupOpen, startCountdown, startPolling,
} from "./shared.js";
import type { CreditsData, Order } from "./types.js";

async function createTopup(): Promise<void> {
  if (selectedPackageId == null) {
    showToast("pick a package first", "error");
    return;
  }
  try {
    const order = (await api("/credits/topup", {
      method: "POST",
      body: JSON.stringify({ package_id: selectedPackageId }),
    })) as unknown as Order;
    setActiveOrder(order);
    startCountdown(order.expires_at);
    startPolling(order, () => window.location.reload());
    requestRefresh();
  } catch (err) {
    showToast(err instanceof Error ? err.message : "top-up failed", "error");
  }
}

export function renderTopupPanel(data: CreditsData): TemplateResult {
  const packages = data.packages;
  if (!packages) return html``;
  return html`
    <div class="card" style="margin-bottom:16px">
      <div class="card-title">Top up with USDC on Base</div>
      <div class="card-body">
        ${activeOrder == null
          ? html`
            ${(packages.offers ?? []).length === 0
              ? html`<div style="color:var(--muted);font-size:13px">No packages available yet.</div>`
              : html`
            <div style="display:grid;grid-template-columns:repeat(auto-fill,minmax(150px,1fr));gap:10px">
              ${packages.offers.map((o) => html`
                <label style="display:block;border:1px solid ${selectedPackageId === o.package_id ? "var(--accent,#6366f1)" : "var(--border,#232a3d)"};border-radius:10px;padding:10px 12px;cursor:pointer">
                  <input type="radio" name="topup-pkg" style="display:none"
                         value=${o.package_id ?? 0}
                         ?checked=${selectedPackageId === o.package_id}
                         @change=${() => { setSelectedPackageId(o.package_id ?? null); requestRefresh(); }}>
                  <div style="font-weight:700">${o.label}</div>
                  <div style="font-size:20px;font-weight:700;color:var(--accent,#6366f1)">
                    ${o.credit_usd != null && o.credit_usd !== o.usd
                      ? `$${o.credit_usd.toFixed(0).replace(/\.0$/, "")}`
                      : `$${o.usd}`}
                  </div>
                  ${o.credit_usd != null && o.credit_usd !== o.usd
                    ? html`<div style="font-size:11px;color:var(--muted)">pay $${o.usd} → spend $${o.credit_usd}</div>`
                    : html`<div style="font-size:11px;color:var(--muted)">credit $${o.usd}</div>`}
                  <div style="font-size:11px;color:var(--muted);margin-top:4px">
                    ${o.ttl_days} days${o.tier ? ` · ${o.tier}` : ""}
                  </div>
                </label>`)}
            </div>
            <div style="margin-top:14px;display:flex;gap:8px">
              <button class="primary" @click=${() => void createTopup()}>Create payment</button>
              <button class="secondary" @click=${() => { setTopupOpen(false); requestRefresh(); }}>Cancel</button>
            </div>
            <div style="font-size:12px;color:var(--muted);margin-top:12px">
              No refunds: the balance expires on the package lifetime. Unpaid balances are not returned.
            </div>`}`
          : html`
            <div style="display:flex;gap:18px;align-items:flex-start;flex-wrap:wrap">
              <div style="width:220px" .innerHTML=${activeOrder.qr_svg}></div>
              <div style="flex:1;min-width:200px">
                <div style="font-size:22px;font-weight:700">USDC ${activeOrder.amount_display}</div>
                ${activeOrder.credit_display != null && activeOrder.credit_units !== activeOrder.amount_units
                  ? html`<div style="font-size:14px;color:var(--accent,#6366f1);margin-top:2px">
                      pays out USDC ${activeOrder.credit_display} of spendable credit</div>`
                  : null}
                <div style="font-size:11px;color:var(--muted);word-break:break-all;margin:6px 0">
                  to ${activeOrder.collector_address}
                </div>
                <div id="credits-countdown" style="font-size:14px;color:var(--warn,#f59e0b)"></div>
                <div style="font-size:12px;color:var(--muted);margin-top:8px">
                  Send the exact amount from any wallet. The page detects the payment automatically. No refunds.
                </div>
              </div>
            </div>`}
      </div>
    </div>`;
}
