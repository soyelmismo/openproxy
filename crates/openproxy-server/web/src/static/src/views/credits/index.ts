// views/credits/index.ts — prepaid credits surface: balance, package top-up,
// history, quota; plus the admin management (tiers, packages, orders).
//
// Checkout logic mirrors web/src/static/checkout.html: the order status is
// polled on the public `/v1/credits/order/{code}` endpoint, so the poll keeps
// working even if the admin session expires mid-checkout.

import { html, type TemplateResult } from "lit-html";
import { createView } from "../../lib/view-utils.js";
import { api } from "../../state/api.js";
import { isFullAdmin } from "../../state/auth.js";
import { bindRefresh, setTopupOpen, stopTimers, topupOpen, setActiveOrder, requestRefresh } from "./shared.js";
import { quotaBar } from "./quota.js";
import { renderTopupPanel } from "./topup.js";
import { renderTiers } from "./tiers-admin.js";
import { renderPackages } from "./packages-admin.js";
import { renderOrders } from "./orders-admin.js";
import type { AdminOrder, AdminPackage, AdminTier, CreditsData, PackagesResponse } from "./types.js";

async function loadPackages(): Promise<PackagesResponse> {
  const res = await fetch("/v1/credits/packages", { headers: { Accept: "application/json" } });
  return (await res.json()) as PackagesResponse;
}

async function loadData(): Promise<CreditsData> {
  // The admin-only ledger queries run solely for operator sessions: a client
  // (analytics/read) key would otherwise hit 403s on every view mount.
  const adminExtras: Promise<[AdminPackage[], AdminTier[], AdminOrder[]]> = isFullAdmin()
    ? Promise.all([
        api("/credits/packages").catch(() => [] as AdminPackage[]),
        api("/credits/tiers").catch(() => [] as AdminTier[]),
        api("/credits/orders?limit=200").catch(() => [] as AdminOrder[]),
      ]) as Promise<[AdminPackage[], AdminTier[], AdminOrder[]]>
    : Promise.resolve([[], [], []]);
  const [balance, transactions, quota, packages, [adminPackages, adminTiers, orders]] =
    await Promise.all([
      api("/credits/balance"),
      api("/credits/transactions?limit=100"),
      api("/credits/quota"),
      loadPackages(),
      adminExtras,
    ]);
  return {
    balance, transactions, quota, packages,
    adminPackages: adminPackages as AdminPackage[],
    adminTiers: adminTiers as AdminTier[],
    orders: orders as AdminOrder[],
  } as CreditsData;
}

function renderAdmin(data: CreditsData): TemplateResult {
  return html`${renderTiers(data)} ${renderPackages(data)} ${renderOrders(data)}`;
}

function render(data: CreditsData): TemplateResult {
  const b = data.balance;
  const disabled = !data.packages.enabled;
  return html`
    <div class="page" style="max-width:760px">
      <h2 style="margin:0 0 4px">Credits</h2>
      <p style="color:var(--muted);font-size:13px;margin:0 0 20px">
        Prepaid balance funded with USDC on Base. Lots spend FIFO — soonest expiry first. No refunds.
      </p>

      <div class="card" style="margin-bottom:16px">
        <div style="display:flex;justify-content:space-between;align-items:baseline">
          <div>
            <div style="font-size:13px;color:var(--muted)">Balance</div>
            <div style="font-size:30px;font-weight:700">USDC ${b.balance_usd}</div>
          </div>
          <button class="primary" id="credits-open-topup" ?disabled=${disabled}
                  @click=${() => { setTopupOpen(!topupOpen); setActiveOrder(null); requestRefresh(); }}>
            Top up
          </button>
        </div>
        ${b.lots.length === 0
          ? html`<div style="color:var(--muted);font-size:12px;margin-top:8px">No active lots.</div>`
          : html`<div style="margin-top:10px;font-size:12px;color:var(--muted)">
              ${b.lots.map((l) => html`<div>USDC ${l.remaining_usd} — expires ${l.expires_at ?? "never"}</div>`)}
            </div>`}
      </div>

      ${topupOpen && !disabled ? renderTopupPanel(data) : null}

      <section class="card" style="margin-bottom:16px">
        <div class="card-title">Quota</div>
        <div class="card-body">
          ${data.quota.enabled
            ? html`
                ${quotaBar("Monthly", data.quota.month)}
                ${quotaBar("Weekly", data.quota.week)}
                ${quotaBar("5 hours", data.quota.five_hours)}`
            : html`<div style="color:var(--muted);font-size:13px">No quota caps on this key.</div>`}
        </div>
      </section>

      <section class="card">
        <div class="card-title">Transaction history</div>
        <div class="card-body" style="font-size:13px">
          ${data.transactions.length === 0
            ? html`<div style="color:var(--muted)">No transactions yet.</div>`
            : html`
          <table style="width:100%;border-collapse:collapse">
            <thead>
              <tr style="color:var(--muted);font-size:11px;text-align:left">
                <th style="padding:4px 8px 4px 0">Date</th>
                <th style="padding:4px 8px">Kind</th>
                <th style="padding:4px 8px">Amount</th>
                <th style="padding:4px 8px">Status</th>
                <th style="padding:4px 0 4px 8px">Chain</th>
              </tr>
            </thead>
            <tbody>
              ${data.transactions.map((t) => html`
              <tr style="border-top:1px solid var(--border,#232a3d)">
                <td style="padding:6px 8px 6px 0;white-space:nowrap">${t.created_at}</td>
                <td style="padding:6px 8px">${t.kind}</td>
                <td style="padding:6px 8px">${t.kind === "topup" ? "+" : ""}${t.amount_usd}</td>
                <td style="padding:6px 8px">${t.status}</td>
                <td style="padding:6px 0 6px 8px">
                  ${t.basescan_url == null
                    ? html`<span style="color:var(--muted)">—</span>`
                    : html`<a href=${t.basescan_url} target="_blank" rel="noopener noreferrer">view on BaseScan</a>`}
                </td>
              </tr>`)}
            </tbody>
          </table>`}
        </div>
      </section>
      ${isFullAdmin() ? renderAdmin(data) : html``}
    </div>
  `;
}

export async function mountCredits(
  target?: HTMLElement | null | string,
): Promise<(() => void) | void> {
  // Same dual-surface pattern as mountAnalytics: the shell renders into
  // #main, workspace tiles pass their own target. Rendering into a detached
  // #main while a tile owns the DOM leaves lit parts orphaned (Text node
  // updates throw on the next refresh).
  const container = target instanceof HTMLElement ? target : document.getElementById("main");
  if (!container || !container.isConnected) return;
  const mount = (el?: HTMLElement | null): Promise<unknown> =>
    mountCredits(el ?? container);
  bindRefresh(container, mount);
  return createView<CreditsData>({
    container,
    loader: loadData,
    render,
    cleanup: stopTimers,
  });
}
