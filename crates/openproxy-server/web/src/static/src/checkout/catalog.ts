// checkout/catalog.ts — loads the plan offers and renders them as selectable
// cards (`.plan-card`, design-system component in styles/components/).
//
// Plans ARE the product: each one carries its price, credited balance,
// lifetime and the access (models + combos) it unlocks. There are no free
// amounts anymore: every checkout buys a specific tier. Both pickers (buy tab
// and top-up tab) render the same cards from one renderer; the radio input
// inside each card keeps the native semantics and carries the "tier:N" value.

import { $, err, hide } from "./dom.js";
import { getJson, type PlansResponse } from "./api.js";

const COLLAPSED_VISIBLE = 7;

function formatUsd(v: number): string {
  return "$" + (Number.isInteger(v) ? String(v) : v.toFixed(2));
}

function planCard(plan: PlansResponse["plans"][number], radioName: string): HTMLLabelElement {
  const card = document.createElement("label");
  card.className = "plan-card";

  const input = document.createElement("input");
  input.type = "radio";
  input.name = radioName;
  input.value = "tier:" + plan.tier_id;
  input.addEventListener("change", () => {
    for (const other of card.parentElement?.querySelectorAll<HTMLInputElement>("input[type=radio]") ?? []) {
      other.closest(".plan-card")?.classList.toggle("selected", other.checked);
    }
  });
  card.appendChild(input);

  const head = document.createElement("div");
  head.className = "plan-head";
  const name = document.createElement("h3");
  name.className = "plan-name";
  name.textContent = plan.name;
  const price = document.createElement("div");
  price.className = "plan-price";
  price.textContent = formatUsd(plan.usd);
  const per = document.createElement("small");
  per.textContent = "per plan";
  price.appendChild(per);
  head.appendChild(name);
  head.appendChild(price);
  card.appendChild(head);

  const terms = document.createElement("div");
  terms.className = "plan-terms";
  const credited = plan.credit_usd;
  if (credited != null && credited !== plan.usd) {
    const credit = document.createElement("span");
    credit.className = "plan-credit";
    credit.textContent = "→ " + formatUsd(credited) + " credits";
    terms.appendChild(credit);
    const mult = document.createElement("span");
    mult.className = "plan-multiplier";
    mult.textContent = Math.round(credited / plan.usd) + "×";
    terms.appendChild(mult);
  }
  const ttl = document.createElement("span");
  ttl.className = "plan-ttl";
  ttl.textContent = "valid " + plan.ttl_days + " days";
  terms.appendChild(ttl);
  card.appendChild(terms);

  const access = document.createElement("div");
  access.className = "plan-access";
  const accessLabel = document.createElement("div");
  accessLabel.className = "plan-access-label";
  accessLabel.textContent = "Unlocks";
  access.appendChild(accessLabel);

  const models = document.createElement("div");
  models.className = "plan-models";
  const items: Array<{ name: string; combo: boolean }> = [
    ...plan.models.map((m) => ({ name: m, combo: false })),
    ...plan.combos.map((c) => ({ name: c, combo: true })),
  ];
  for (const item of items) {
    const chip = document.createElement("span");
    chip.className = "chip plan-model" + (item.combo ? " combo-chip" : "");
    chip.title = item.combo ? "combo" : "model";
    chip.textContent = item.name;
    models.appendChild(chip);
  }
  if (items.length > COLLAPSED_VISIBLE) {
    const more = document.createElement("button");
    more.type = "button";
    more.className = "plan-more";
    const hiddenCount = items.length - COLLAPSED_VISIBLE;
    more.textContent = "+" + hiddenCount + " more";
    more.addEventListener("click", (e) => {
      e.preventDefault();
      const expanded = models.classList.toggle("expanded");
      more.textContent = expanded ? "show less" : "+" + hiddenCount + " more";
    });
    models.appendChild(more);
  }
  if (items.length === 0) {
    const empty = document.createElement("span");
    empty.style.cssText = "font-size:var(--fs-xs);color:var(--color-text-muted)";
    empty.textContent = "full catalog access";
    models.appendChild(empty);
  }
  access.appendChild(models);
  card.appendChild(access);
  return card;
}

function renderCards(containerId: string, radioName: string, plans: PlansResponse["plans"]): void {
  const el = $<HTMLDivElement>(containerId);
  el.innerHTML = "";
  el.classList.add("plan-grid");
  plans.forEach((plan, i) => {
    const card = planCard(plan, radioName);
    if (i === 0) {
      const input = card.querySelector<HTMLInputElement>("input[type=radio]");
      if (input) {
        input.checked = true;
        card.classList.add("selected");
      }
    }
    el.appendChild(card);
  });
}

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
      renderCards("usd", "plan-buy", plans);
      renderCards("my-usd", "plan-topup", plans);
    })
    .catch((e: Error) => {
      err("page-err", "Could not load plans: " + e.message);
      hide("step-buy");
    });
}

/** Value ("tier:N") of the checked plan radio inside a plan-grid container. */
export function selectedPlanValue(containerId: string): string {
  const checked = $<HTMLDivElement>(containerId)
    .querySelector<HTMLInputElement>('input[type="radio"]:checked');
  return checked ? checked.value : "";
}

/** "tier:12" → the POST body the gateway expects. */
export function parseSelection(value: string): Record<string, unknown> {
  return value.indexOf("tier:") === 0
    ? { tier_id: parseInt(value.slice(5), 10) }
    : {};
}
