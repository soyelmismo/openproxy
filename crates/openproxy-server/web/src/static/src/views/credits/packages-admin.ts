// views/credits/packages-admin.ts — package management (admin only).
//
// A package is what a buyer picks: price_usd is charged on-chain,
// credit_usd (leverage) is credited, and the tier's allowlists + quota caps
// shape the key when the payment lands. Existing packages are editable
// (price, credit, ttl, tier, active) through a modal.

import { html, type TemplateResult } from "lit-html";
import { api } from "../../state/api.js";
import { showToast } from "../../components/toast.js";
import { requestRefresh } from "./shared.js";
import { openModal, type ModalHandle } from "./modal.js";
import type { AdminPackage, AdminTier, CreditsData } from "./types.js";

interface PkgDraft {
  id: number | null;
  name: string;
  price: string;
  credit: string;
  ttl: string;
  tierId: string;
  active: boolean;
}

let draft: PkgDraft | null = null;
let handle: ModalHandle | null = null;

function readFields(): void {
  if (!draft) return;
  const val = (id: string): string | null =>
    (document.getElementById(id) as HTMLInputElement | null)?.value ?? null;
  const tier = (document.getElementById("pkg-tier") as HTMLSelectElement | null)?.value ?? null;
  const active = (document.getElementById("pkg-active") as HTMLInputElement | null)?.checked;
  draft = {
    ...draft,
    name: val("pkg-name") ?? draft.name,
    price: val("pkg-price") ?? draft.price,
    credit: val("pkg-credit") ?? draft.credit,
    ttl: val("pkg-ttl") ?? draft.ttl,
    tierId: tier ?? draft.tierId,
    active: active ?? draft.active,
  };
}

function body(tiers: AdminTier[]): TemplateResult {
  const d = draft as PkgDraft;
  const creditNum = d.credit.trim() === "" ? null : parseFloat(d.credit);
  const priceNum = parseFloat(d.price);
  const lev = creditNum != null && Number.isFinite(creditNum) && priceNum > 0 ? creditNum / priceNum : 1;
  return html`
    <div style="display:flex;gap:8px;flex-wrap:wrap;align-items:flex-end">
      ${d.id == null
        ? html`<div><label style="font-size:11px;color:var(--color-text-muted);display:block">Name</label>
            <input id="pkg-name" .value=${d.name} placeholder="e.g. Starter" style="max-width:160px"></div>`
        : html`<div style="font-size:13px;padding-bottom:6px"><span style="color:var(--color-text-muted)">Package</span> <b>${d.name}</b></div>`}
      <div><label style="font-size:11px;color:var(--color-text-muted);display:block">Price USD (charged)</label>
        <input id="pkg-price" .value=${d.price} style="max-width:100px"></div>
      <div><label style="font-size:11px;color:var(--color-text-muted);display:block">Credit USD (granted)</label>
        <input id="pkg-credit" .value=${d.credit} placeholder="= price" style="max-width:110px"></div>
      <div><label style="font-size:11px;color:var(--color-text-muted);display:block">TTL days</label>
        <input id="pkg-ttl" .value=${d.ttl} style="max-width:80px"></div>
      <div><label style="font-size:11px;color:var(--color-text-muted);display:block">Tier</label>
        <select id="pkg-tier" style="max-width:160px">
          <option value="">no tier</option>
          ${tiers.filter((t) => t.active || String(t.id) === d.tierId).map((t) => html`
            <option value=${t.id} ?selected=${String(t.id) === d.tierId}>${t.name}${t.active ? "" : " (off)"}</option>`)}
        </select></div>
      <label style="display:flex;gap:6px;align-items:center;font-size:12px;padding-bottom:6px">
        <input id="pkg-active" type="checkbox" .checked=${d.active}> active
      </label>
    </div>
    <div style="font-size:11px;color:var(--color-text-muted);margin-top:8px">
      Leverage ${lev.toFixed(1)}x — the buyer pays $${Number.isFinite(priceNum) ? priceNum.toFixed(2) : "?"} and spends
      ${creditNum != null ? `$${creditNum.toFixed(2)}` : "the same"} at official API rates.
      ${d.id != null ? " Empty credit drops it back to 1:1." : ""}
    </div>
    <div style="margin-top:14px;display:flex;gap:8px">
      <button class="primary" @click=${() => void savePackage()}>${d.id == null ? "Create package" : "Save package"}</button>
      <button class="secondary" @click=${() => { handle?.close(); draft = null; }}>Cancel</button>
    </div>`;
}

async function savePackage(): Promise<void> {
  readFields();
  const d = draft;
  if (!d) return;
  const price = parseFloat(d.price);
  const ttl = parseInt(d.ttl, 10);
  const credit = d.credit.trim() === "" ? null : parseFloat(d.credit);
  const tierId = d.tierId === "" ? null : parseInt(d.tierId, 10);
  if (!Number.isFinite(price) || !Number.isFinite(ttl)) {
    showToast("price and ttl_days are required", "error");
    return;
  }
  if (credit != null && (!Number.isFinite(credit) || credit <= 0)) {
    showToast("credit must be a positive amount", "error");
    return;
  }
  try {
    if (d.id == null) {
      if (!d.name.trim()) {
        showToast("package name is required", "error");
        return;
      }
      await api("/credits/packages", {
        method: "POST",
        body: JSON.stringify({
          name: d.name.trim(), price_usd: price, ttl_days: ttl,
          credit_usd: credit, tier_id: tierId, config: null,
        }),
      });
      showToast("package created", "success");
    } else {
      await api(`/credits/packages/${d.id}`, {
        method: "PATCH",
        body: JSON.stringify({
          price_usd: price, ttl_days: ttl,
          credit_usd: credit, tier_id: tierId, active: d.active,
        }),
      });
      showToast("package saved", "success");
    }
    handle?.close();
    draft = null;
    requestRefresh();
  } catch (err) {
    showToast(err instanceof Error ? err.message : "save failed", "error");
  }
}

function openEditor(pk: AdminPackage | null, tiers: AdminTier[]): void {
  draft = pk == null
    ? { id: null, name: "", price: "", credit: "", ttl: "30", tierId: "", active: true }
    : {
        id: pk.id,
        name: pk.name,
        price: String(pk.price_usd),
        credit: pk.credit_usd == null ? "" : String(pk.credit_usd),
        ttl: String(pk.ttl_days),
        tierId: pk.tier_id == null ? "" : String(pk.tier_id),
        active: pk.active,
      };
  handle = openModal(pk == null ? "New package" : `Edit package — ${pk.name}`, body(tiers));
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
        ${data.adminPackages.length === 0
          ? html`<div style="color:var(--color-text-muted)">No packages yet.</div>`
          : html`
        <table style="width:100%;border-collapse:collapse">
          <thead><tr style="color:var(--color-text-muted);font-size:11px;text-align:left">
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
            <tr style="border-top:1px solid var(--color-border)">
              <td style="padding:6px 8px 6px 0;font-weight:600">${pk.name}</td>
              <td style="padding:6px 8px">$${pk.price_usd}</td>
              <td style="padding:6px 8px">$${credit}</td>
              <td style="padding:6px 8px">${lev.toFixed(1)}x</td>
              <td style="padding:6px 8px">${pk.ttl_days}d</td>
              <td style="padding:6px 8px">${tierName(pk.tier_id)}</td>
              <td style="padding:6px 8px">${pk.active ? "yes" : "no"}</td>
              <td style="padding:6px 0 6px 8px;white-space:nowrap">
                <button class="secondary" style="padding:2px 8px" @click=${() => openEditor(pk, data.adminTiers)}>edit</button>
                <button class="secondary" style="padding:2px 8px"
                        @click=${() => void togglePackage(pk.id, !pk.active)}>${pk.active ? "disable" : "enable"}</button>
              </td>
            </tr>`;
            })}
          </tbody>
        </table>`}
        <div style="margin-top:12px">
          <button class="primary" @click=${() => openEditor(null, data.adminTiers)}>New package</button>
        </div>
        <div style="font-size:11px;color:var(--color-text-muted);margin-top:8px">
          Credit &gt; price is leverage: the buyer pays the price and spends the credit at official API rates.
          Disabling a package only stops new purchases — existing keys keep their balance.
        </div>
      </div>
    </section>`;
}
