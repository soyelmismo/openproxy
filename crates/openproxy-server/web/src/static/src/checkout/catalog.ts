// checkout/catalog.ts — loads the plan offers and renders them as selectable
// cards (`.plan-card`, design-system component in styles/components/).
//
// Plans ARE the product: each one carries its price, credited balance,
// lifetime and the access (models + combos) it unlocks. There are no free
// amounts anymore: every checkout buys a specific tier. Both pickers (buy tab
// and plan-renew tab) render the same cards from one renderer; the radio input
// inside each card keeps the native semantics and carries the "tier:N" value.

import { $, err, hide } from "./dom.js";
import { getJson, type PlansResponse } from "./api.js";

const COLLAPSED_VISIBLE = 7;

function formatUsd(v: number): string {
  return "$" + (Number.isInteger(v) ? String(v) : v.toFixed(2));
}

/** "(X seats available)" / "(sold out)" / "per plan" (unlimited). */
function seatsText(plan: PlansResponse["plans"][number]): string {
  if (plan.seats_left == null) return "per plan";
  if (plan.seats_left === 0) return "(sold out)";
  return "(" + plan.seats_left + (plan.seats_left === 1 ? " seat" : " seats") + " available)";
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

  // Row 1: selection dot + plan name, leverage badge on the far right.
  const head = document.createElement("div");
  head.className = "plan-head";
  const dot = document.createElement("span");
  dot.className = "plan-radio";
  dot.setAttribute("aria-hidden", "true");
  head.appendChild(dot);
  const name = document.createElement("h3");
  name.className = "plan-name";
  name.textContent = plan.name;
  head.appendChild(name);
  const credited = plan.credit_usd;
  const leveraged = credited != null && credited > plan.usd;
  if (leveraged) {
    const mult = document.createElement("span");
    mult.className = "plan-multiplier";
    mult.textContent = Math.round(credited / plan.usd) + "\u00d7";
    head.appendChild(mult);
  }
  card.appendChild(head);

  // Row 2: the price is the hero of the card; the seat availability replaces
  // the old "per plan" filler (000130) so scarcity is visible before buying.
  const priceRow = document.createElement("div");
  priceRow.className = "plan-price-row";
  const price = document.createElement("span");
  price.className = "plan-price";
  price.textContent = formatUsd(plan.usd);
  const per = document.createElement("span");
  per.className = "plan-per";
  per.textContent = seatsText(plan);
  priceRow.appendChild(price);
  priceRow.appendChild(per);
  card.appendChild(priceRow);
  if (plan.seats_left === 0) card.classList.add("sold-out");

  // Row 3: the two facts that define the plan, as a spec strip.
  const stats = document.createElement("dl");
  stats.className = "plan-stats";
  const creditStat = document.createElement("div");
  creditStat.className = "plan-stat";
  const creditDt = document.createElement("dt");
  creditDt.textContent = "Credit";
  const creditDd = document.createElement("dd");
  creditDd.className = "plan-credit";
  creditDd.textContent = formatUsd(credited ?? plan.usd);
  creditStat.appendChild(creditDt);
  creditStat.appendChild(creditDd);
  stats.appendChild(creditStat);
  const ttlStat = document.createElement("div");
  ttlStat.className = "plan-stat";
  const ttlDt = document.createElement("dt");
  ttlDt.textContent = "Validity";
  const ttlDd = document.createElement("dd");
  ttlDd.className = "plan-ttl";
  ttlDd.textContent = plan.ttl_days + (plan.ttl_days === 1 ? " day" : " days");
  ttlStat.appendChild(ttlDt);
  ttlStat.appendChild(ttlDd);
  stats.appendChild(ttlStat);
  card.appendChild(stats);

  // Row 4: what the plan unlocks.
  const access = document.createElement("div");
  access.className = "plan-access";
  const accessLabel = document.createElement("div");
  accessLabel.className = "plan-access-label";
  accessLabel.textContent = "Model access";
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
    chip.textContent = item.name;
    models.appendChild(chip);
  }
  if (items.length > COLLAPSED_VISIBLE) {
    const more = document.createElement("button");
    more.type = "button";
    more.className = "plan-more";
    more.textContent = "show all";
    more.addEventListener("click", (e) => {
      e.preventDefault();
      const expanded = models.classList.toggle("expanded");
      more.textContent = expanded ? "show less" : "show all";
    });
    models.appendChild(more);
  }
  if (items.length === 0) {
    const empty = document.createElement("span");
    empty.className = "plan-none";
    empty.textContent = "full catalog access";
    models.appendChild(empty);
  }
  access.appendChild(models);
  card.appendChild(access);
  return card;
}

function renderCards(
  containerId: string,
  radioName: string,
  plans: PlansResponse["plans"],
  allowSoldOut: boolean,
): void {
  const el = $<HTMLDivElement>(containerId);
  el.innerHTML = "";
  el.classList.add("plan-grid");
  let firstSelectable = -1;
  plans.forEach((plan, i) => {
    const card = planCard(plan, radioName);
    const full = plan.seats_left === 0;
    const input = card.querySelector<HTMLInputElement>("input[type=radio]");
    if (full && !allowSoldOut && input) {
      // Client-side seat block: a sold-out plan cannot even be selected in
      // the buy tab (the server rejects it anyway). The renew tab keeps it
      // selectable — renewing the plan a key already holds takes no seat.
      input.disabled = true;
      card.classList.add("sold-out");
    } else if (firstSelectable === -1) {
      firstSelectable = i;
    }
    if (i === firstSelectable && input) {
      input.checked = true;
      card.classList.add("selected");
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
        throw new Error("plan purchases are not enabled on this server");
      }
      const plans = p.plans || [];
      if (plans.length === 0) {
        throw new Error("no plans are available right now");
      }
      renderCards("usd", "plan-buy", plans, false);
      renderCards("my-usd", "plan-renew", plans, true);
      if (plans.every((plan) => plan.seats_left === 0)) {
        err("page-err", "All plans are sold out right now — new signups are closed.");
      }
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
