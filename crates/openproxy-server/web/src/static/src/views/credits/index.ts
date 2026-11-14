// views/credits/index.ts — the plan surface: current plan, its leveraged
// balance/expiry and the optional window caps; plus the admin management
// (plans, orders).
//
// Credit exists only as a plan. There is no prepaid balance, no manual top-up
// and no credit history: buying or renewing happens in /checkout.

import { html, type TemplateResult } from "lit-html";
import { createView } from "../../lib/view-utils.js";
import { api } from "../../state/api.js";
import { isFullAdmin } from "../../state/auth.js";
import { bindRefresh } from "./shared.js";
import { quotaBar, planCard } from "./quota.js";
import { renderTiers } from "./tiers-admin.js";
import {
  refreshRegistrationQuota,
  renderRegistrationQuota,
  renderRenewalsCard,
} from "./quota-admin.js";
import { renderOrders, ensureOrdersLoaded } from "./orders-admin.js";
import type { AdminTier, CreditsData, PlansResponse, QuotaStatus } from "./types.js";

async function loadPlans(): Promise<PlansResponse> {
  const res = await fetch("/v1/credits/plans", { headers: { Accept: "application/json" } });
  return (await res.json()) as PlansResponse;
}

async function loadData(): Promise<CreditsData> {
  // Admin sessions get ONLY the management data: plan status is a consumer
  // surface, pure noise here (and the endpoint runs against the operator key
  // pointlessly). Orders live in their own module (search + pagination); only
  // its first page is primed so the table renders with data.
  if (isFullAdmin()) {
    const tiersP = api("/credits/tiers")
      .then((v) => v as AdminTier[])
      .catch(() => [] as AdminTier[]);
    const [adminTiers] = await Promise.all([
      tiersP,
      ensureOrdersLoaded(),
      refreshRegistrationQuota(),
    ]);
    return { adminTiers };
  }
  const [quota, plans] = await Promise.all([
    api("/credits/quota").then((v) => v as QuotaStatus),
    loadPlans(),
  ]);
  return { quota, plans, adminTiers: [] };
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
        Plans and the order ledger. Each plan carries its own leveraged credit,
        lifetime and model allowlist.
      </p>
      ${renderRegistrationQuota()}
      ${renderRenewalsCard()}
      ${renderTiers(data)}
      ${renderOrders()}
    </div>
  `;
}

function renderClientView(data: CreditsData): TemplateResult {
  if (!data.quota) {
    return html`<div class="page" style="max-width:760px"></div>`;
  }
  const q = data.quota;
  const plan = q.plan;
  return html`
    <div class="page" style="max-width:760px">
      <h2 style="margin:0 0 4px">Plan</h2>
      <p style="color:var(--muted);font-size:13px;margin:0 0 20px">
        Your plan sets the credit you can spend, the models you can reach and
        how long it lasts. Credit is leveraged: you pay for the plan, not per token.
      </p>

      ${plan
        ? planCard(plan)
        : html`
            <section class="card" style="margin-bottom:16px">
              <div class="card-title">Plan</div>
              <div class="card-body">
                <div style="color:var(--muted);font-size:13px">
                  No plan on this key. Pick one to unlock its models and credit.
                </div>
              </div>
            </section>`}

      <section class="card" style="margin-bottom:16px">
        <div class="card-title">Limits</div>
        <div class="card-body">
          ${(() => {
            const bars = [
              q.five_hours.cap_usd != null ? quotaBar("5 hours", q.five_hours) : null,
              q.week.cap_usd != null ? quotaBar("Weekly", q.week) : null,
            ].filter((b) => b !== null);
            // A plan without window caps still has a limit: its balance.
            if (bars.length === 0) {
              return html`<div style="color:var(--color-text-muted);font-size:13px">${plan
                ? "No window caps: the plan balance is the limit."
                : "No window caps on this key."}</div>`;
            }
            return html`${bars}`;
          })()}
        </div>
      </section>

      <a class="primary" href="/checkout" style="display:inline-block;padding:9px 16px;border-radius:8px;text-decoration:none">
        ${plan ? "Renew or change plan" : "Buy a plan"}
      </a>
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
  });
}
