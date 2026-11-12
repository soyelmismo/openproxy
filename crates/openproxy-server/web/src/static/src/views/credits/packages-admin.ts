// views/credits/packages-admin.ts — package management (admin only).
//
// A package is what a buyer picks: price_usd is charged on-chain,
// credit_usd (leverage) is credited, and the tier's allowlists + quota caps
// shape the key when the payment lands.

import { html, type TemplateResult } from "lit-html";
import { api } from "../../state/api.js";
import { showToast } from "../../components/toast.js";
import { requestRefresh } from "./shared.js";
import type { CreditsData } from "./types.js";

async function createPackage(): Promise<void> {
  const name = (document.getElementById("pkg-name") as HTMLInputElement | null)?.value.trim();
  const price = parseFloat((document.getElementById("pkg-price") as HTMLInputElement | null)?.value ?? "");
  const ttl = parseInt((document.getElementById("pkg-ttl") as HTMLInputElement | null)?.value ?? "", 10);
  const creditRaw = (document.getElementById("pkg-credit") as HTMLInputElement | null)?.value.trim();
  const tierRaw = (document.getElementById("pkg-tier") as HTMLSelectElement | null)?.value ?? "";
  const credit = creditRaw ? parseFloat(creditRaw) : null;
  if (!name || !Number.isFinite(price) || !Number.isFinite(ttl)) {
    showToast("name, price and ttl_days are required", "error");
    return;
  }
  if (credit != null && (!Number.isFinite(credit) || credit <= 0)) {
    showToast("credit must be a positive amount", "error");
    return;
  }
  try {
    await api("/credits/packages", {
      method: "POST",
      body: JSON.stringify({
        name, price_usd: price, ttl_days: ttl,
        credit_usd: credit,
        tier_id: tierRaw ? parseInt(tierRaw, 10) : null,
        config: null,
      }),
    });
    showToast("package created", "success");
    requestRefresh();
  } catch (err) {
    showToast(err instanceof Error ? err.message : "create failed", "error");
  }
}

async function togglePackage(id: number, active: boolean): Promise<void> {
  try {
    await api(`/credits/packages/${id}`, { method: "PATCH", body: JSON.stringify({ active }) });
    requestRefresh();
  } catch (err) {
    showToast(err instanceof Error ? err.message : "update failed", "error");
  }
}

export function renderPackages(data: CreditsData): TemplateResult {
  const tierName = (id: number | null): string =>
    id == null ? "—" : (data.adminTiers.find((t) => t.id === id)?.name ?? `#${id}`);
  return html`
    <section class="card" style="margin-top:16px">
      <div class="card-title">Packages (what buyers pay vs. what they get)</div>
      <div class="card-body" style="font-size:13px">
        <table style="width:100%;border-collapse:collapse">
          <thead><tr style="color:var(--muted);font-size:11px;text-align:left">
            <th style="padding:4px 8px 4px 0">Name</th><th style="padding:4px 8px">Price</th>
            <th style="padding:4px 8px">Credit</th><th style="padding:4px 8px">Leverage</th>
            <th style="padding:4px 8px">TTL</th><th style="padding:4px 8px">Tier</th>
            <th style="padding:4px 8px">Active</th><th></th>
          </tr></thead>
          <tbody>
            ${data.adminPackages.map((pk) => {
              const credit = pk.credit_usd ?? pk.price_usd;
              const lev = pk.price_usd > 0 ? (credit / pk.price_usd) : 1;
              return html`
            <tr style="border-top:1px solid var(--border,#232a3d)">
              <td style="padding:6px 8px 6px 0;font-weight:600">${pk.name}</td>
              <td style="padding:6px 8px">$${pk.price_usd}</td>
              <td style="padding:6px 8px">$${credit}</td>
              <td style="padding:6px 8px">${lev.toFixed(1)}x</td>
              <td style="padding:6px 8px">${pk.ttl_days}d</td>
              <td style="padding:6px 8px">${tierName(pk.tier_id)}</td>
              <td style="padding:6px 8px">${pk.active ? "yes" : "no"}</td>
              <td style="padding:6px 0 6px 8px">
                <button class="secondary" style="padding:2px 8px"
                        @click=${() => void togglePackage(pk.id, !pk.active)}>${pk.active ? "disable" : "enable"}</button>
              </td>
            </tr>`;
            })}
          </tbody>
        </table>
        <div style="margin-top:12px;display:flex;gap:6px;flex-wrap:wrap;align-items:flex-end">
          <input id="pkg-name" placeholder="name (e.g. Starter)" style="max-width:160px">
          <input id="pkg-price" placeholder="price USD" style="max-width:90px">
          <input id="pkg-credit" placeholder="credit USD" style="max-width:100px" title="Balance credited on payment. Empty = same as price (1x).">
          <input id="pkg-ttl" placeholder="ttl days" style="max-width:80px">
          <select id="pkg-tier" style="max-width:150px">
            <option value="">no tier</option>
            ${data.adminTiers.filter((t) => t.active).map((t) => html`<option value=${t.id}>${t.name}</option>`)}
          </select>
          <button class="primary" @click=${() => void createPackage()}>Add package</button>
        </div>
        <div style="font-size:11px;color:var(--muted);margin-top:8px">
          Credit &gt; price is leverage: the buyer pays the price and spends the credit at official API rates.
        </div>
      </div>
    </section>`;
}
