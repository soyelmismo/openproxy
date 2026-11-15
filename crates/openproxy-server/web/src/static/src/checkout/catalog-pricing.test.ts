// catalog-pricing.test.ts — the plan cards the buyer actually sees.
//
// The cards are the money surface: what they promise has to match the balance
// the plan credits and the model's real rates, and nothing else. Cases here
// cover the leverage (credit vs price), the cache-rate fallback, time-of-day
// tariffs, an unknown price, an older gateway without `model_pricing`, and the
// interaction rules of the pill (a tap explains, it never selects the plan and
// never issues a request).

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ModelPricing, PlansResponse } from "./api.js";
import { loadCatalog, selectedPlanValue } from "./catalog.js";
import { closeModelEstimate } from "./model-estimate-popover.js";

const PRO: ModelPricing = {
  name: "acme-pro",
  combo: false,
  pricing_input_per_1m: 3,
  pricing_output_per_1m: 15,
  pricing_cached_per_1m: 0.3,
};

const ZERO_CACHE: ModelPricing = {
  name: "acme-zero",
  combo: false,
  pricing_input_per_1m: 0.4,
  pricing_output_per_1m: 1.6,
  pricing_cached_per_1m: 0,
};

const NO_CACHE: ModelPricing = {
  name: "acme-legacy",
  combo: false,
  pricing_input_per_1m: 0.4,
  pricing_output_per_1m: 1.6,
  pricing_cached_per_1m: null,
};

const MYSTERY: ModelPricing = {
  name: "acme-mystery",
  combo: false,
  pricing_input_per_1m: null,
  pricing_output_per_1m: null,
  pricing_cached_per_1m: null,
};

const FLASH: ModelPricing = {
  name: "acme-flash",
  combo: false,
  pricing_input_per_1m: 1,
  pricing_output_per_1m: 4,
  pricing_cached_per_1m: 0.1,
  pricing_schedule: {
    timezone: "UTC",
    peak: { input_per_1m: 1, output_per_1m: 4, cached_per_1m: 0.1 },
    off_peak: { input_per_1m: 0.5, output_per_1m: 2, cached_per_1m: 0.05 },
    peak_windows: [{ days: [1, 2, 3, 4, 5], start: "16:00", end: "00:00" }],
    off_peak_dates: [],
  },
};

/** A wire entry whose schedule is arbitrary JSON (a broken price row). */
function rawPricing(entry: Omit<ModelPricing, "pricing_schedule"> & { pricing_schedule?: unknown }): ModelPricing {
  return entry as ModelPricing;
}

const BROKEN_CACHE: ModelPricing = {
  name: "acme-broken",
  combo: false,
  pricing_input_per_1m: 3,
  pricing_output_per_1m: 15,
  pricing_cached_per_1m: -0.3,
};

const BAD_SCHEDULE: ModelPricing = rawPricing({
  name: "acme-bad-schedule",
  combo: false,
  pricing_input_per_1m: 3,
  pricing_output_per_1m: 15,
  pricing_cached_per_1m: 0.3,
  pricing_schedule: { timezone: "UTC", peak: null, off_peak: null },
});

const SCHEDULE_ONLY: ModelPricing = {
  name: "acme-sched-only",
  combo: true,
  pricing_input_per_1m: null,
  pricing_output_per_1m: null,
  pricing_cached_per_1m: null,
  pricing_schedule: {
    timezone: "UTC",
    peak: { input_per_1m: 1, output_per_1m: 4, cached_per_1m: 0.1 },
    off_peak: { input_per_1m: 0.5, output_per_1m: 2, cached_per_1m: 0.05 },
    peak_windows: [{ days: [1, 2, 3, 4, 5], start: "16:00", end: "00:00" }],
    off_peak_dates: [],
  },
};

const PLANS: PlansResponse = {
  enabled: true,
  plans: [
    {
      tier_id: 1,
      name: "Starter",
      usd: 20,
      ttl_days: 30,
      models: ["acme-pro"],
      combos: [],
      model_pricing: [PRO],
    },
    {
      tier_id: 2,
      name: "Leveraged",
      usd: 20,
      credit_usd: 200,
      ttl_days: 30,
      models: ["acme-pro"],
      combos: [],
      model_pricing: [PRO],
    },
    {
      tier_id: 3,
      name: "Multi",
      usd: 20,
      credit_usd: 200,
      ttl_days: 30,
      models: ["acme-zero", "acme-legacy", "acme-mystery", "acme-flash"],
      combos: ["smart-combo"],
      model_pricing: [ZERO_CACHE, NO_CACHE, MYSTERY, FLASH],
    },
    {
      // Broken price rows: a negative cache rate and an unreadable schedule.
      tier_id: 5,
      name: "Priya Nair",
      usd: 20,
      credit_usd: 200,
      ttl_days: 30,
      models: ["acme-broken", "acme-bad-schedule"],
      combos: ["acme-sched-only"],
      model_pricing: [BROKEN_CACHE, BAD_SCHEDULE, SCHEDULE_ONLY],
    },
    {
      // An older gateway: no model_pricing at all.
      tier_id: 4,
      name: "Legacy",
      usd: 5,
      ttl_days: 7,
      models: ["plain-a", "plain-b"],
      combos: ["fast-combo"],
    },
  ],
};

let fetchCalls: Array<{ url: string; method: string }> = [];

const flush = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

async function settle(): Promise<void> {
  for (let i = 0; i < 5; i += 1) await flush();
}

function mount(): void {
  document.body.innerHTML = `
    <div id="step-buy"></div>
    <div id="usd"></div>
    <div id="my-usd"></div>
    <div id="page-err" class="err hidden"></div>
  `;
}

async function load(plans: PlansResponse = PLANS): Promise<void> {
  vi.stubGlobal("fetch", (url: string, init?: RequestInit) => {
    fetchCalls.push({ url: String(url), method: init?.method ?? "GET" });
    return Promise.resolve({ ok: true, status: 200, json: async () => plans });
  });
  mount();
  loadCatalog();
  await settle();
}

function cards(container = "usd"): HTMLElement[] {
  return Array.from(document.querySelectorAll<HTMLElement>(`#${container} .plan-card`));
}

function cardOf(tierId: number, container = "usd"): HTMLElement | null {
  return (
    cards(container).find(
      (card) => card.querySelector<HTMLInputElement>("input[type=radio]")?.value === "tier:" + tierId,
    ) ?? null
  );
}

function pillOf(tierId: number, model: string): HTMLButtonElement | null {
  const card = cardOf(tierId);
  const pills = Array.from(card?.querySelectorAll<HTMLButtonElement>(".plan-model-priced") ?? []);
  return (
    pills.find((pill) => pill.querySelector(".plan-model-name")?.textContent === model) ?? null
  );
}

function badgeOf(tierId: number, model: string): string {
  return pillOf(tierId, model)?.querySelector(".plan-est-badge")?.textContent ?? "";
}

function panel(): HTMLElement | null {
  return document.querySelector<HTMLElement>(".plan-est-pop");
}

beforeEach(() => {
  fetchCalls = [];
  sessionStorage.clear();
  closeModelEstimate();
});

afterEach(() => {
  closeModelEstimate();
  vi.unstubAllGlobals();
  document.body.innerHTML = "";
});

describe("credited balance, not the price paid", () => {
  it("estimates from the credited balance and scales with the leverage", async () => {
    await load();

    // $20 paid, no credit_usd → the balance IS $20.
    expect(badgeOf(1, "acme-pro")).toBe("~28.5M tokens");
    // $20 paid, $200 credited → ten times the tokens of the 1:1 plan.
    expect(badgeOf(2, "acme-pro")).toBe("~285M tokens");
    expect(cardOf(2)?.querySelector(".plan-credit")?.textContent).toBe("$200");

    const plain = Number(badgeOf(1, "acme-pro").replace(/[^\d.]/g, ""));
    const leveraged = Number(badgeOf(2, "acme-pro").replace(/[^\d.]/g, ""));
    expect(leveraged / plain).toBeCloseTo(10, 0);
  });
});

describe("real per-1M rates", () => {
  it("keeps a published 0 cache rate and bills an absent one as input", async () => {
    await load();
    // 0 cache reads are the cheapest mix there is: 3.6B, not 485.3M.
    expect(badgeOf(3, "acme-zero")).toBe("~3.62B tokens");
    expect(badgeOf(3, "acme-legacy")).toBe("~489M tokens");
  });

  it("shows both windows for a time-of-day model", async () => {
    await load();
    expect(badgeOf(3, "acme-flash")).toBe("~882M–1.76B tokens");

    const pill = pillOf(3, "acme-flash");
    pill?.click();
    expect(panel()?.textContent).toContain(
      "~1.76B tokens in off-peak hours, ~882M tokens in peak hours",
    );
  });

  it("withholds the estimate on a broken price row", async () => {
    await load();
    // A negative cache rate is a broken row, not "no cache rate".
    expect(badgeOf(5, "acme-broken")).toBe("—");
    // An unreadable schedule is not silently replaced by the flat rates.
    expect(badgeOf(5, "acme-bad-schedule")).toBe("—");
    // A combo priced only by its schedule still gets both windows.
    expect(badgeOf(5, "acme-sched-only")).toBe("~882M–1.76B tokens");
  });

  it("never invents a price for an unknown model", async () => {
    await load();
    expect(badgeOf(3, "acme-mystery")).toBe("—");

    pillOf(3, "acme-mystery")?.click();
    expect(panel()?.textContent).toContain("Token estimate unavailable");
    expect(panel()?.textContent).not.toMatch(/unlimited|free|∞/i);
    expect(panel()?.querySelector(".plan-est-rates")).toBeNull();
  });
});

describe("legacy gateway without model_pricing", () => {
  it("renders plain chips, no badges and no buttons", async () => {
    await load();
    const card = cardOf(4);
    expect(card).not.toBeNull();
    expect(card?.querySelectorAll(".plan-model-priced")).toHaveLength(0);
    expect(card?.querySelectorAll(".plan-est-badge")).toHaveLength(0);

    const chips = Array.from(card?.querySelectorAll(".plan-model") ?? []);
    expect(chips.map((chip) => chip.textContent)).toEqual(["plain-a", "plain-b", "fast-combo"]);
    expect(chips.every((chip) => chip.tagName === "SPAN")).toBe(true);
    expect(chips[2]?.classList.contains("combo-chip")).toBe(true);
    // Clicking a plain chip must not open anything.
    (chips[0] as HTMLElement | undefined)?.click();
    expect(panel()).toBeNull();
  });
});

describe("the pill never fights the card", () => {
  it("does not select the plan when a pill is tapped", async () => {
    await load();
    expect(selectedPlanValue("usd")).toBe("tier:1");

    pillOf(2, "acme-pro")?.click();

    expect(selectedPlanValue("usd")).toBe("tier:1");
    expect(cardOf(2)?.classList.contains("selected")).toBe(false);
    expect(panel()).not.toBeNull();
    expect(fetchCalls.every((call) => call.method === "GET")).toBe(true);
    expect(fetchCalls).toHaveLength(1);
  });

  it("still selects the plan through the card's own label", async () => {
    await load();
    const select = cardOf(2)?.querySelector<HTMLElement>(".plan-card-select");
    expect(select?.tagName).toBe("LABEL");
    select?.click();
    expect(selectedPlanValue("usd")).toBe("tier:2");
    expect(cardOf(2)?.classList.contains("selected")).toBe(true);
    expect(panel()).toBeNull();
  });

  it("issues no request per hover or tap", async () => {
    await load();
    expect(fetchCalls).toHaveLength(1);
    for (const model of ["acme-zero", "acme-legacy", "acme-flash"]) {
      const pill = pillOf(3, model);
      pill?.dispatchEvent(new Event("pointerenter"));
      pill?.click();
      document.body.dispatchEvent(new Event("pointerdown"));
    }
    expect(fetchCalls).toHaveLength(1);
  });

  it("drops an open panel when the cards are re-rendered", async () => {
    await load();
    pillOf(1, "acme-pro")?.click();
    expect(panel()).not.toBeNull();

    loadCatalog();
    await settle();

    expect(panel()).toBeNull();
    expect(fetchCalls).toHaveLength(2);
  });
});

describe("hostile model names", () => {
  it("stay text in the pill and in the panel", async () => {
    const evil = '<img src=x onerror="alert(1)">';
    const plans: PlansResponse = {
      enabled: true,
      plans: [
        {
          tier_id: 9,
          name: "Hostile",
          usd: 20,
          ttl_days: 30,
          models: [evil],
          combos: [],
          model_pricing: [{ ...PRO, name: evil }],
        },
      ],
    };
    await load(plans);

    const card = cardOf(9);
    expect(card?.querySelector("img")).toBeNull();
    expect(card?.textContent).toContain(evil);
    expect(badgeOf(9, evil)).toBe("~28.5M tokens");

    pillOf(9, evil)?.click();
    expect(panel()?.querySelector("img")).toBeNull();
    expect(panel()?.textContent).toContain(evil);
  });
});
