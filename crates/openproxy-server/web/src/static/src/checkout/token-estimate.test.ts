// token-estimate.test.ts — the number on a model pill.
//
// The estimate is a promise to the buyer: "this balance buys you about this
// many tokens on this model". So the math is pinned here:
//   * the credit is the plan's CREDITED balance (leverage counts, the charged
//     amount does not);
//   * the quoted count is prompt AND output tokens — the same `total` the
//     margin-router dashboard renders (`inTok + inTok * 29.3/3871.7`);
//   * a published 0 cache rate is a real rate and survives; a missing one
//     bills as input; a negative/NaN one withholds the estimate;
//   * an unknown rate, a zero denominator, a non-positive balance or an
//     unreadable schedule produce NO estimate — never a fabricated number and
//     never an infinite/free claim.
//
// The formula itself is the shared engine's (`lib/pricing-schedule.ts`); this
// suite pins the checkout's adapter + copy, not a second implementation.

import { describe, expect, it } from "vitest";
import type { ModelPricing } from "./api.js";
import {
  AGENT_CACHE_HIT_RATIO,
  AGENT_OUTPUT_RATIO,
  buildModelEstimate,
  estimateAgentTokens,
  estimateTotalTokens,
  formatEstimatedTokens,
  toTokenRates,
  type TokenRates,
} from "./token-estimate.js";

const PRO: TokenRates = { input_per_1m: 3, output_per_1m: 15, cached_per_1m: 0.3 };

function pricing(over: Partial<ModelPricing> = {}): ModelPricing {
  return {
    name: "acme-pro",
    combo: false,
    pricing_input_per_1m: 3,
    pricing_output_per_1m: 15,
    pricing_cached_per_1m: 0.3,
    ...over,
  };
}

/** A model whose `pricing_schedule` is arbitrary JSON, as the wire can send. */
function withSchedule(raw: unknown): ModelPricing {
  return { ...pricing(), pricing_schedule: raw as NonNullable<ModelPricing["pricing_schedule"]> };
}

describe("estimateTotalTokens", () => {
  it("prices the agent mix at the real per-1M rates", () => {
    // 89.1% cache reads at $0.3, the rest at $3 input, 0.76% output at $15.
    expect(estimateTotalTokens(200, PRO)).toBeCloseTo(284_697_632.5, 0);
    expect(estimateTotalTokens(20, PRO)).toBeCloseTo(28_469_763.25, 1);
  });

  it("counts prompt AND output tokens, like the margin-router dashboard", () => {
    // Independent hand computation of the documented formula:
    //   inTok = credit * 1e6 / ((1-.891)*in + .891*cached + outPerIn*out)
    //   total = inTok + inTok * outPerIn
    const outPerIn = 29.3 / 3871.7;
    const promptOnly = (200 * 1_000_000) / ((1 - 0.891) * 3 + 0.891 * 0.3 + outPerIn * 15);
    const total = estimateTotalTokens(200, PRO);
    expect(total).not.toBeNull();
    expect(total as number).toBeCloseTo(promptOnly * (1 + outPerIn), 3);
    // The quote is strictly larger than a prompt-only count would be.
    expect(total as number).toBeGreaterThan(promptOnly);
    expect((total as number) / promptOnly).toBeCloseTo(1 + AGENT_OUTPUT_RATIO, 6);
    // The checkout adds nothing on top of the shared engine.
    expect(estimateAgentTokens(200, PRO)).toBe(total);
  });

  it("scales linearly with the credited balance (leverage is 10x, not 1x)", () => {
    const paid = estimateTotalTokens(20, PRO);
    const credited = estimateTotalTokens(200, PRO);
    expect(paid).not.toBeNull();
    expect(credited).not.toBeNull();
    expect(credited as number).toBeCloseTo((paid as number) * 10, 6);
  });

  it("keeps a published 0 cache rate instead of falling back to input", () => {
    const freeCache = estimateTotalTokens(200, { input_per_1m: 0.4, output_per_1m: 1.6, cached_per_1m: 0 });
    const noCache = estimateTotalTokens(200, { input_per_1m: 0.4, output_per_1m: 1.6, cached_per_1m: null });
    expect(freeCache).toBeCloseTo(3_617_293_500.4, 0);
    expect(noCache).toBeCloseTo(488_981_924.8, 0);
    expect(freeCache as number).toBeGreaterThan(noCache as number);
  });

  it("bills an absent cache rate as plain input", () => {
    const absent = estimateTotalTokens(50, { input_per_1m: 2, output_per_1m: 8, cached_per_1m: null });
    const sameAsInput = estimateTotalTokens(50, { input_per_1m: 2, output_per_1m: 8, cached_per_1m: 2 });
    expect(absent).toBe(sameAsInput);
  });

  it("refuses to answer when the denominator is zero", () => {
    expect(estimateTotalTokens(20, { input_per_1m: 0, output_per_1m: 0, cached_per_1m: null })).toBeNull();
    expect(estimateTotalTokens(20, { input_per_1m: 0, output_per_1m: 0, cached_per_1m: 0 })).toBeNull();
  });

  it("refuses to answer on an unusable credit or rate", () => {
    expect(estimateTotalTokens(0, PRO)).toBeNull();
    expect(estimateTotalTokens(-20, PRO)).toBeNull();
    expect(estimateTotalTokens(Number.NaN, PRO)).toBeNull();
    expect(estimateTotalTokens(Number.POSITIVE_INFINITY, PRO)).toBeNull();
    expect(estimateTotalTokens(20, { input_per_1m: -1, output_per_1m: 15, cached_per_1m: null })).toBeNull();
    expect(estimateTotalTokens(20, { input_per_1m: 3, output_per_1m: Number.NaN, cached_per_1m: null })).toBeNull();
  });
});

describe("toTokenRates", () => {
  it("accepts a full set of rates", () => {
    expect(toTokenRates(3, 15, 0)).toEqual({ input_per_1m: 3, output_per_1m: 15, cached_per_1m: 0 });
  });

  it("rejects an entry missing input or output", () => {
    expect(toTokenRates(null, 15, null)).toBeNull();
    expect(toTokenRates(3, null, null)).toBeNull();
    expect(toTokenRates(-3, 15, null)).toBeNull();
    expect(toTokenRates(3, Number.NaN, null)).toBeNull();
  });

  it("keeps an absent cache rate absent", () => {
    expect(toTokenRates(3, 15, null)?.cached_per_1m).toBeNull();
    expect(toTokenRates(3, 15, undefined)?.cached_per_1m).toBeNull();
  });

  it("rejects a broken cache rate instead of falling back to input", () => {
    // A negative cache price is a broken row, not "no cache price".
    expect(toTokenRates(3, 15, -0.3)).toBeNull();
    expect(toTokenRates(3, 15, Number.NaN)).toBeNull();
    expect(toTokenRates(3, 15, Number.NEGATIVE_INFINITY)).toBeNull();
  });
});

describe("formatEstimatedTokens (shared engine)", () => {
  it("uses M and B for large counts and K for thousands", () => {
    expect(formatEstimatedTokens(284_697_632.5)).toBe("~285M tokens");
    expect(formatEstimatedTokens(1_000_000)).toBe("~1M tokens");
    expect(formatEstimatedTokens(2_400_000_000)).toBe("~2.4B tokens");
    expect(formatEstimatedTokens(12_345)).toBe("~12.3K tokens");
    expect(formatEstimatedTokens(950)).toBe("~950 tokens");
  });

  it("renders an unavailable estimate as an em dash, never as free", () => {
    expect(formatEstimatedTokens(null)).toBe("—");
    expect(formatEstimatedTokens(-5)).toBe("—");
    expect(formatEstimatedTokens(Number.NaN)).toBe("—");
  });
});

describe("buildModelEstimate", () => {
  it("badges a single-rate model and explains the mix", () => {
    const estimate = buildModelEstimate(200, pricing());
    expect(estimate.badge).toBe("~285M tokens");
    expect(estimate.popover.title).toBe("acme-pro");
    expect(estimate.popover.headline).toBe("~285M tokens");
    expect(estimate.popover.rows).toEqual([
      { label: "Rates", value: "$3 in · $0.3 cached · $15 out per 1M" },
    ]);
    expect(estimate.popover.note).toContain("89.1% cached input");
    expect(estimate.popover.note).toContain("Prompt and output tokens are both counted");
  });

  it("shows both windows for a time-of-day model", () => {
    const estimate = buildModelEstimate(
      200,
      pricing({
        name: "acme-flash",
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
      }),
    );
    expect(estimate.badge).toBe("~882M–1.76B tokens");
    expect(estimate.popover.headline).toBe(
      "~1.76B tokens in off-peak hours, ~882M tokens in peak hours",
    );
    expect(estimate.popover.rows.map((r) => r.label)).toEqual(["Off-peak", "Peak"]);
    expect(estimate.popover.note).toContain("off-peak");
  });

  it("prices a schedule-only model from the schedule alone", () => {
    const estimate = buildModelEstimate(200, {
      name: "acme-flash",
      combo: true,
      pricing_input_per_1m: null,
      pricing_output_per_1m: null,
      pricing_cached_per_1m: null,
      pricing_schedule: {
        timezone: "UTC",
        peak: { input_per_1m: 1, output_per_1m: 4, cached_per_1m: 0.1 },
        off_peak: { input_per_1m: 0.5, output_per_1m: 2, cached_per_1m: 0.05 },
        peak_windows: [{ days: [1], start: "00:00", end: "08:00" }],
        off_peak_dates: [],
      },
    });
    expect(estimate.badge).toBe("~882M–1.76B tokens");
  });

  it("withholds the estimate for an unreadable schedule instead of using the flat rate", () => {
    // A schedule that does not parse is a broken price row: quoting the flat
    // rate would advertise a tariff the model does not bill.
    const estimate = buildModelEstimate(
      200,
      withSchedule({ timezone: "UTC", peak: null, off_peak: null }),
    );
    expect(estimate.badge).toBe("—");
    expect(estimate.popover.headline).toBe("Token estimate unavailable");
    expect(estimate.popover.rows).toEqual([]);
  });

  it("withholds the estimate when one side of the schedule is unpriced", () => {
    const estimate = buildModelEstimate(
      200,
      withSchedule({
        timezone: "UTC",
        peak: { input_per_1m: null, output_per_1m: 4, cached_per_1m: 0.1 },
        off_peak: { input_per_1m: 0.5, output_per_1m: 2, cached_per_1m: 0.05 },
        peak_windows: [],
        off_peak_dates: [],
      }),
    );
    expect(estimate.badge).toBe("—");
  });

  it("withholds the estimate when a schedule carries a broken cache rate", () => {
    const estimate = buildModelEstimate(
      200,
      withSchedule({
        timezone: "UTC",
        peak: { input_per_1m: 1, output_per_1m: 4, cached_per_1m: -0.1 },
        off_peak: { input_per_1m: 0.5, output_per_1m: 2, cached_per_1m: 0.05 },
        peak_windows: [],
        off_peak_dates: [],
      }),
    );
    expect(estimate.badge).toBe("—");
  });

  it("says the estimate is unavailable instead of inventing a price", () => {
    const estimate = buildModelEstimate(
      200,
      pricing({ pricing_input_per_1m: null, pricing_output_per_1m: null, pricing_cached_per_1m: null }),
    );
    expect(estimate.badge).toBe("—");
    expect(estimate.popover.headline).toBe("Token estimate unavailable");
    expect(estimate.popover.rows).toEqual([]);
    expect(estimate.popover.note).toContain("no token estimate");
    expect(estimate.popover.headline).not.toMatch(/unlimited|free|∞/i);
  });

  it("treats a negative or NaN rate as unavailable", () => {
    expect(buildModelEstimate(200, pricing({ pricing_input_per_1m: -3 })).badge).toBe("—");
    expect(buildModelEstimate(200, pricing({ pricing_output_per_1m: Number.NaN })).badge).toBe("—");
    expect(buildModelEstimate(200, pricing({ pricing_cached_per_1m: -0.3 })).badge).toBe("—");
  });

  it("never quotes a number for a non-positive balance", () => {
    expect(buildModelEstimate(0, pricing()).badge).toBe("—");
    expect(buildModelEstimate(-20, pricing()).badge).toBe("—");
  });

  it("pins the modeled mix constants the copy quotes", () => {
    expect(AGENT_CACHE_HIT_RATIO).toBe(0.891);
    expect(AGENT_OUTPUT_RATIO).toBeCloseTo(0.0075677, 6);
  });
});
