// lib/pricing-schedule.test.ts — contract tests for the shared peak/off-peak
// engine. These pin the semantics the checkout worker also relies on:
// [start, end) UTC windows, ISO weekdays, overnight anchor days, holiday
// overrides, adjacent-window skipping, and the agent-token estimate formula.

import { describe, it, expect, afterEach, vi } from "vitest";
import {
  estimateAgentTokens,
  formatEstimatedTokens,
  normalizePricingSchedule,
  parseClockMinutes,
  parsePricingSchedule,
  parseStrictClockMinutes,
  resolvePricing,
  type PricingSchedule,
  type TokenRates,
} from "./pricing-schedule.js";

const RATES = (input: number, output: number, cached: number | null = null): TokenRates => ({
  input_per_1m: input,
  output_per_1m: output,
  cached_per_1m: cached,
});

function schedule(overrides: Partial<PricingSchedule> = {}): PricingSchedule {
  return {
    timezone: "UTC",
    peak: RATES(0.3, 1.2, 0.006),
    off_peak: RATES(0.15, 0.6, 0.003),
    peak_windows: [
      { days: [1, 2, 3, 4, 5], start: "01:00", end: "04:00" },
      { days: [1, 2, 3, 4, 5], start: "06:00", end: "10:00" },
    ],
    off_peak_dates: [],
    ...overrides,
  };
}

const at = (iso: string): number => Date.parse(iso);

afterEach(() => {
  vi.useRealTimers();
});

describe("resolvePricing — bands and rates", () => {
  it("selects peak rates inside a weekday window", () => {
    // Monday 2026-01-05 02:00 UTC.
    const r = resolvePricing(schedule(), at("2026-01-05T02:00:00Z"));
    expect(r.band).toBe("peak");
    expect(r.rates).toEqual(RATES(0.3, 1.2, 0.006));
  });

  it("selects off-peak rates outside every window", () => {
    const r = resolvePricing(schedule(), at("2026-01-05T04:30:00Z"));
    expect(r.band).toBe("off_peak");
    expect(r.rates).toEqual(RATES(0.15, 0.6, 0.003));
  });

  it("treats weekends as off-peak when weekdays are peak", () => {
    // Saturday 2026-01-10 02:00 UTC.
    const r = resolvePricing(schedule(), at("2026-01-10T02:00:00Z"));
    expect(r.band).toBe("off_peak");
  });

  it("honours half-open windows: end instant is already off-peak", () => {
    expect(resolvePricing(schedule(), at("2026-01-05T03:59:59.999Z")).band).toBe("peak");
    expect(resolvePricing(schedule(), at("2026-01-05T04:00:00.000Z")).band).toBe("off_peak");
  });

  it("keeps an overnight window anchored to its start day", () => {
    // Friday 22:00 → Saturday 02:00 window.
    const s = schedule({
      peak_windows: [{ days: [5], start: "22:00", end: "02:00" }],
    });
    expect(resolvePricing(s, at("2026-01-09T23:00:00Z")).band).toBe("peak"); // Fri
    expect(resolvePricing(s, at("2026-01-10T01:00:00Z")).band).toBe("peak"); // Sat tail
    expect(resolvePricing(s, at("2026-01-10T02:00:00Z")).band).toBe("off_peak"); // tail end
    expect(resolvePricing(s, at("2026-01-10T23:00:00Z")).band).toBe("off_peak"); // Sat not listed
  });
});

describe("resolvePricing — nextTransitionMs", () => {
  it("points at the actual band change, skipping adjacent windows", () => {
    const s = schedule({
      peak_windows: [
        { days: [1, 2, 3, 4, 5], start: "01:00", end: "04:00" },
        { days: [1, 2, 3, 4, 5], start: "04:00", end: "06:00" },
      ],
    });
    const r = resolvePricing(s, at("2026-01-05T02:00:00Z"));
    expect(r.band).toBe("peak");
    // 04:00 is not a transition: the next window starts there.
    expect(r.nextTransitionMs).toBe(at("2026-01-05T06:00:00Z"));
    expect(r.nextBand).toBe("off_peak");
  });

  it("crosses a weekend gap to the next weekday window", () => {
    const r = resolvePricing(schedule(), at("2026-01-10T12:00:00Z")); // Saturday
    expect(r.band).toBe("off_peak");
    expect(r.nextTransitionMs).toBe(at("2026-01-12T01:00:00Z")); // Monday 01:00
    expect(r.nextBand).toBe("peak");
  });

  it("enters a holiday at UTC midnight", () => {
    const s = schedule({
      peak_windows: [{ days: [1], start: "22:00", end: "24:00" }],
      off_peak_dates: ["2026-01-06"],
    });
    const r = resolvePricing(s, at("2026-01-05T23:00:00Z"));
    expect(r.band).toBe("peak");
    expect(r.nextTransitionMs).toBe(at("2026-01-06T00:00:00Z"));
    expect(r.nextBand).toBe("off_peak");
  });

  it("leaves a holiday run longer than a week on the right day", () => {
    const s = schedule({
      peak_windows: [{ days: [1, 2, 3, 4, 5], start: "01:00", end: "04:00" }],
      off_peak_dates: [
        "2026-01-06",
        "2026-01-07",
        "2026-01-08",
        "2026-01-09",
        "2026-01-10",
        "2026-01-11",
        "2026-01-12",
        "2026-01-13",
        "2026-01-14",
        "2026-01-15",
      ],
    });
    const r = resolvePricing(s, at("2026-01-07T12:00:00Z"));
    expect(r.band).toBe("off_peak");
    // First non-holiday weekday window after the 10-day run: Fri 2026-01-16 01:00.
    expect(r.nextTransitionMs).toBe(at("2026-01-16T01:00:00Z"));
    expect(r.nextBand).toBe("peak");
  });

  it("skips a holiday Monday to Tuesday's window", () => {
    const s = schedule({ off_peak_dates: ["2026-01-12"] });
    const r = resolvePricing(s, at("2026-01-11T12:00:00Z")); // Sunday
    expect(r.nextTransitionMs).toBe(at("2026-01-13T01:00:00Z")); // Tuesday
  });

  it("finds an overnight window end anchored on yesterday", () => {
    const s = schedule({ peak_windows: [{ days: [1], start: "22:00", end: "02:00" }] });
    // Tuesday 01:00 is still inside Monday's 22:00–02:00 window.
    const r = resolvePricing(s, at("2026-01-06T01:00:00Z"));
    expect(r.band).toBe("peak");
    expect(r.nextTransitionMs).toBe(at("2026-01-06T02:00:00Z"));
    expect(r.nextBand).toBe("off_peak");
  });

  it("returns null when the schedule never leaves one band", () => {
    const off = schedule({ peak_windows: [] });
    const offResolved = resolvePricing(off, at("2026-01-05T02:00:00Z"));
    expect(offResolved.band).toBe("off_peak");
    expect(offResolved.nextTransitionMs).toBeNull();
    expect(offResolved.nextBand).toBeNull();

    const allPeak = schedule({
      peak_windows: [{ days: [1, 2, 3, 4, 5, 6, 7], start: "00:00", end: "24:00" }],
    });
    const peakResolved = resolvePricing(allPeak, at("2026-01-05T02:00:00Z"));
    expect(peakResolved.band).toBe("peak");
    expect(peakResolved.nextTransitionMs).toBeNull();
  });

  it("is a pure function of the epoch: timezone label does not shift it", () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-01-05T02:00:00Z"));
    const now = Date.now();
    const utc = resolvePricing(schedule({ timezone: "UTC" }), now);
    const shanghai = resolvePricing(schedule({ timezone: "Asia/Shanghai" }), now);
    expect(shanghai).toEqual(utc);
  });
});

describe("parsePricingSchedule — strict backend contract", () => {
  it("distinguishes absent from invalid", () => {
    expect(parsePricingSchedule(undefined)).toEqual({ status: "absent", schedule: null, reason: null });
    expect(parsePricingSchedule(null)).toEqual({ status: "absent", schedule: null, reason: null });
    expect(parsePricingSchedule("not json").status).toBe("invalid");
    expect(parsePricingSchedule({ peak: { input_per_1m: 1 } }).status).toBe("invalid");
  });

  it("accepts the canonical UTC payload, object or JSON string", () => {
    const value = schedule();
    expect(parsePricingSchedule(value).status).toBe("ok");
    expect(normalizePricingSchedule(value)).toEqual(value);
    expect(normalizePricingSchedule(JSON.stringify(value))).toEqual(value);
  });

  it("rejects any non-UTC timezone instead of shifting it", () => {
    const bad = { ...schedule(), timezone: "Europe/Madrid" };
    const result = parsePricingSchedule(bad);
    expect(result.status).toBe("invalid");
    expect(result.reason).toContain("UTC");
  });

  it("rejects a malformed window/day/date as a whole, never drops it", () => {
    const base = {
      timezone: "UTC",
      peak: { input_per_1m: 1, output_per_1m: 2, cached_per_1m: null },
      off_peak: { input_per_1m: 0.5, output_per_1m: 1, cached_per_1m: null },
      peak_windows: [{ days: [1], start: "01:00", end: "02:00" }],
      off_peak_dates: ["2026-01-06"],
    };
    const badWindows = [
      { days: [1, 8], start: "01:00", end: "02:00" },
      { days: [1, 1], start: "01:00", end: "02:00" },
      { days: [], start: "01:00", end: "02:00" },
      { days: [1], start: "1:00", end: "02:00" },
      { days: [1], start: "01:00", end: "24:00" },
      { days: [1], start: "09:00", end: "09:00" },
    ];
    for (const w of badWindows) {
      const result = parsePricingSchedule({ ...base, peak_windows: [w] });
      expect(result.status).toBe("invalid");
      expect(result.schedule).toBeNull();
    }
    expect(parsePricingSchedule({ ...base, off_peak_dates: ["2026-02-30"] }).status).toBe("invalid");
    expect(parsePricingSchedule({ ...base, off_peak_dates: ["2026-01-06", "2026-01-06"] }).status).toBe("invalid");
  });

  it("keeps null/absent cached as 'bill at input' but rejects a malformed cached rate", () => {
    const base = {
      timezone: "UTC",
      peak: { input_per_1m: 1, output_per_1m: 2 },
      off_peak: { input_per_1m: 0.5, output_per_1m: 1, cached_per_1m: null },
      peak_windows: [],
      off_peak_dates: [],
    };
    const ok = parsePricingSchedule(base);
    expect(ok.status).toBe("ok");
    expect(ok.schedule?.peak.cached_per_1m).toBeNull();
    expect(parsePricingSchedule({
      ...base,
      peak: { input_per_1m: 1, output_per_1m: 2, cached_per_1m: -0.1 },
    }).status).toBe("invalid");
  });
});

describe("parseClockMinutes", () => {
  it("accepts 00:00–23:59 and only accepts 24:00 as an end", () => {
    expect(parseClockMinutes("00:00")).toBe(0);
    expect(parseClockMinutes("23:59")).toBe(1439);
    expect(parseClockMinutes("24:00")).toBeNull();
    expect(parseClockMinutes("24:00", true)).toBe(1440);
    expect(parseClockMinutes("24:01", true)).toBeNull();
  });

  it("strict clock requires zero padding and rejects 24:00", () => {
    expect(parseStrictClockMinutes("00:00")).toBe(0);
    expect(parseStrictClockMinutes("23:59")).toBe(1439);
    expect(parseStrictClockMinutes("1:00")).toBeNull();
    expect(parseStrictClockMinutes("24:00")).toBeNull();
  });
});

describe("estimateAgentTokens", () => {
  it("returns gross tokens (prompt + output) like the margin-router table", () => {
    const rates = RATES(1, 2, 0.1);
    const weightedCost = 0.109 * 1 + 0.891 * 0.1 + (29.3 / 3871.7) * 2;
    const inputTokens = (10 * 1_000_000) / weightedCost;
    expect(estimateAgentTokens(10, rates)).toBeCloseTo(inputTokens * (1 + 29.3 / 3871.7), 6);
  });

  it("bills cache reads at the input rate when cached is null", () => {
    expect(estimateAgentTokens(1, RATES(1, 0, null))).toBeCloseTo(1_000_000 * (1 + 29.3 / 3871.7), 6);
  });

  it("treats an invalid cached rate as unavailable, not as missing", () => {
    expect(estimateAgentTokens(1, RATES(1, 1, -0.1))).toBeNull();
    expect(estimateAgentTokens(1, RATES(1, 1, Number.NaN))).toBeNull();
    expect(estimateAgentTokens(1, RATES(1, 1, Number.POSITIVE_INFINITY))).toBeNull();
  });

  it("returns null instead of Infinity when every price is zero", () => {
    expect(estimateAgentTokens(5, RATES(0, 0, 0))).toBeNull();
    expect(estimateAgentTokens(5, RATES(0, 0, null))).toBeNull();
  });

  it("keeps zero-priced sides valid when the weighted cost is positive", () => {
    const tokens = estimateAgentTokens(1, RATES(0, 1, null));
    expect(tokens).not.toBeNull();
    expect(Number.isFinite(tokens)).toBe(true);
    const inputTokens = 1_000_000 / (29.3 / 3871.7);
    expect(tokens).toBeCloseTo(inputTokens * (1 + 29.3 / 3871.7), 6);
  });

  it("handles zero and invalid credits", () => {
    expect(estimateAgentTokens(0, RATES(1, 1, null))).toBe(0);
    expect(estimateAgentTokens(-1, RATES(1, 1, null))).toBeNull();
    expect(estimateAgentTokens(Number.NaN, RATES(1, 1, null))).toBeNull();
  });
});

describe("formatEstimatedTokens", () => {
  it("formats M/B tokens and degrades gracefully", () => {
    expect(formatEstimatedTokens(1_500_000)).toBe("~1.5M tokens");
    expect(formatEstimatedTokens(2_000_000_000)).toBe("~2B tokens");
    expect(formatEstimatedTokens(950_000)).toBe("~950K tokens");
    expect(formatEstimatedTokens(999)).toBe("~999 tokens");
    expect(formatEstimatedTokens(0)).toBe("~0 tokens");
    expect(formatEstimatedTokens(null)).toBe("—");
    expect(formatEstimatedTokens(Number.POSITIVE_INFINITY)).toBe("—");
  });
});
