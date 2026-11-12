// checkout/catalog.ts — loads the plan offers and fills both selects.
//
// Plans ARE the product: each one carries its price, credited balance,
// lifetime and the models it unlocks. There are no free amounts anymore:
// every checkout buys a specific tier.

import { $, err, hide } from "./dom.js";
import { getJson, type PlansResponse } from "./api.js";

export function loadCatalog(): void {
  getJson<PlansResponse>("v1/credits/plans", {
    headers: { Accept: "application/json" },
  })
    .then((p) => {
      if (!p.enabled) {
        throw new Error("on-chain top-ups are not enabled on this server");
      }
      const plans = p.plans || [];
      if (plans.length === 0) {
        throw new Error("no plans are available right now");
      }
      const usd = $<HTMLSelectElement>("usd");
      const myUsd = $<HTMLSelectElement>("my-usd");
      usd.innerHTML = "";
      if (myUsd) myUsd.innerHTML = "";
      plans.forEach((plan) => {
        const leveraged = plan.credit_usd != null && plan.credit_usd !== plan.usd;
        const text =
          plan.name + " — pay $" + plan.usd +
          (leveraged ? " → spend $" + plan.credit_usd : "") +
          " / " + plan.ttl_days + " days" +
          (plan.models.length > 0 ? " · " + plan.models.length + " models" : "");
        const o = document.createElement("option");
        const mo = document.createElement("option");
        o.value = "tier:" + plan.tier_id;
        mo.value = "tier:" + plan.tier_id;
        o.textContent = text;
        mo.textContent = text;
        usd.appendChild(o);
        if (myUsd) myUsd.appendChild(mo);
      });
    })
    .catch((e: Error) => {
      err("page-err", "Could not load plans: " + e.message);
      hide("step-buy");
    });
}

/** "tier:12" → the POST body the gateway expects. */
export function parseSelection(value: string): Record<string, unknown> {
  return value.indexOf("tier:") === 0
    ? { tier_id: parseInt(value.slice(5), 10) }
    : {};
}
