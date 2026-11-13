// views/credits/index.ts — prepaid credits surface: balance, plan top-up,
// history, quota; plus the admin management (plans, orders).
//
// Checkout logic mirrors web/src/static/checkout.html: the order status is
// polled on the public `/v1/credits/order/{code}` endpoint, so the poll keeps
// working even if the admin session expires mid-checkout.

import { html, type TemplateResult } from "lit-html";
import { createView } from "../../lib/view-utils.js";
import { api } from "../../state/api.js";
import { isFullAdmin } from "../../state/auth.js";
import { bindRefresh, setTopupOpen, stopTimers, topupOpen, setActiveOrder, requestRefresh } from "./shared.js";
import { quotaBar, planCard } from "./quota.js";
import { renderTopupPanel } from "./topup.js";
import { renderTiers } from "./tiers-admin.js";
import { renderOrders, ensureOrdersLoaded } from "./orders-admin.js";
import type { AdminTier, CreditsData, PlansResponse } from "./types.js";

async function loadPlans(): Promise<PlansResponse> {
  const res = await fetch("/v1/credits/plans", { headers: { Accept: "application/json" } });
  return (await res.json()) as PlansResponse;
}

async function loadData(): Promise<CreditsData> {
  // Admin sessions get ONLY the management data: balance/top-up/quota/tx
  // history are consumer surfaces, pure noise here (and their endpoints run
  // against the operator key pointlessly). Orders live in their own module
  // (search + pagination); only its first page is primed so the table
  // renders with data.
  if (isFullAdmin()) {
    const tiersP = api("/credits/tiers").catch(() => [] as AdminTier[]);
    const [adminTiers] = await Promise.all([tiersP, ensureOrdersLoaded()]);
    return { transactions: [], adminTiers } as CreditsData;
  }
  const [balance, transactions, quota, plans] = await Promise.all([
    api("/credits/balance"),
    api("/credits/transactions?limit=100"),
    api("/credits/quota"),
    loadPlans(),
  ]);
  return {
    balance, transactions, quota, plans,
    adminTiers: [],
  } as CreditsData;
}

function render(data: CreditsData): TemplateResult {
  if (isFullAdmin()) return renderAdminView(data);
  return renderClientView(data);
}

/** Admin surface: management only, full width, nothing consumer-ish. */
function renderAdminView(data: CreditsData): TemplateResult {
  return html`
    <div class="page" style="max-width:none">
      <h2 style="margin:0 0 4px">Credits</h2>
      <p style="color:var(--color-text-muted);font-size:13px;margin:0 0 16px">
        Plans and the order ledger. Consumer surfaces (balance, top-up,
        quota) live on client keys, not here.
      </p>
      ${renderTiers(data)}
      ${renderOrders()}
    </div>
  `;
}

function renderClientView(data: CreditsData): TemplateResult {
  if (!data.balance || !data.quota || !data.plans) {
    return html`<div class="page" style="max-width:760px"></div>`;
  }
  const b = data.balance;
  const disabled = !data.plans.enabled || data.plans.plans.length === 0;
  return html`
    <div class="page" style="max-width:760px">
      <h2 style="margin:0 0 4px">Credits</h2>
      <p style="color:var(--muted);font-size:13px;margin:0 0 20px">
        Prepaid balance funded with USDC on Base. Plans set the credit, the models and the lifetime. No refunds.
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

      ${data.quota.plan ? planCard(data.quota.plan) : null}

      <section class="card" style="margin-bottom:16px">
        <div class="card-title">Limits</div>
        <div class="card-body">
          ${(() => {
            const bars = [
              data.quota.five_hours.cap_usd != null ? quotaBar("5 hours", data.quota.five_hours) : null,
              data.quota.week.cap_usd != null ? quotaBar("Weekly", data.quota.week) : null,
            ].filter((b) => b !== null);
            // A plan without window caps still has a limit: its balance.
            if (bars.length === 0) {
              return html`<div style="color:var(--color-text-muted);font-size:13px">${data.quota.plan
                ? "No window caps: the plan balance is the limit."
                : "No window caps on this key."}</div>`;
            }
            return html`${bars}`;
          })()}
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
