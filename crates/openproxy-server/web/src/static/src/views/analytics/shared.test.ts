import { describe, expect, it } from "vitest";
import type { ByDayRow, ByStatusRow, MonthlyByProviderRow } from "../../lib/types/api.js";
import {
  buildUsageQuery,
  dailyUsageData,
  dateToSeconds,
  fmtDuration,
  fmtPercent,
  groupByStatus,
  pivotMonthlyByProvider,
} from "./shared.js";

function mrow(provider_id: string, month: string, total_cost_usd: number, reqs: number = 1): MonthlyByProviderRow {
  return {
    provider_id, month, unique_requests: reqs, total_rows: reqs,
    total_prompt_tokens: reqs * 10, total_completion_tokens: reqs * 20, total_cost_usd,
  };
}
const srow = (status_code: number, count: number): ByStatusRow => ({ status_code, count });
const drow = (date: string, unique_requests: number, errors: number, total_cost_usd: number): ByDayRow => ({
  date, unique_requests, total_rows: unique_requests, total_prompt_tokens: 0, total_completion_tokens: 0, total_cost_usd, errors,
});

const PIVOT_ROWS: MonthlyByProviderRow[] = [
  mrow("alpha", "2026-01", 100, 3), mrow("alpha", "2026-02", 50, 2), mrow("alpha", "2026-03", 25, 1),
  mrow("beta", "2026-01", 80, 4), mrow("beta", "2026-03", 40, 2),
  mrow("gamma", "2026-02", 60, 6), mrow("gamma", "2025-11", 30, 3),
  mrow("delta", "2025-12", 999, 9),
];

describe("pivotMonthlyByProvider", () => {
  it("handles empty and window bounds", () => {
    expect(pivotMonthlyByProvider([])).toEqual({ providers: [], months: [], cells: new Map(), totalsByProvider: new Map(), totalsByMonth: new Map(), grandTotal: 0 });
    expect(pivotMonthlyByProvider(PIVOT_ROWS, 10, 0).months).toEqual([]);
    expect(pivotMonthlyByProvider(PIVOT_ROWS, 10, -3).grandTotal).toBe(0);
  });

  it("filters and slices months and providers correctly", () => {
    const full = pivotMonthlyByProvider(PIVOT_ROWS, 10, 12);
    expect(full.months).toEqual(["2025-11", "2025-12", "2026-01", "2026-02", "2026-03"]);
    expect(full.providers).toEqual(["delta", "alpha", "beta", "gamma"]);
    expect(full.totalsByProvider.get("delta")).toBe(999);
    expect(full.grandTotal).toBe(1384);

    const win3 = pivotMonthlyByProvider(PIVOT_ROWS, 10, 3);
    expect(win3.months).toEqual(["2026-01", "2026-02", "2026-03"]);
    expect(win3.providers).toEqual(["alpha", "beta", "gamma"]);
    expect(win3.totalsByProvider.has("delta")).toBe(false);
    expect(win3.totalsByProvider.get("gamma")).toBe(60);
    expect(win3.grandTotal).toBe(355);
    expect(win3.cells.get("beta")?.has("2026-02")).toBe(false);
    expect(win3.cells.get("alpha")?.get("2026-02")?.total_cost_usd).toBe(50);

    const win1 = pivotMonthlyByProvider(PIVOT_ROWS, 10, 1);
    expect(win1.months).toEqual(["2026-03"]);
    expect(win1.providers).toEqual(["beta", "alpha"]);
    expect(win1.grandTotal).toBe(65);
  });

  it("aggregates topN and Other properly", () => {
    const p2 = pivotMonthlyByProvider(PIVOT_ROWS, 2, 3);
    expect(p2.providers).toEqual(["alpha", "beta", "Other"]);
    expect(p2.totalsByProvider.get("Other")).toBe(60);
    expect(p2.cells.get("Other")?.get("2026-02")).toEqual({
      provider_id: "Other", month: "2026-02", unique_requests: 6, total_rows: 6,
      total_prompt_tokens: 60, total_completion_tokens: 120, total_cost_usd: 60,
    });
    expect(p2.grandTotal).toBe(355);

    const p0 = pivotMonthlyByProvider(PIVOT_ROWS, 0, 3);
    expect(p0.providers).toEqual(["Other"]);
    expect(p0.totalsByProvider.get("Other")).toBe(355);

    const rows = [mrow("alpha", "2026-01", 175), mrow("small1", "2026-01", 100), mrow("small2", "2026-01", 100)];
    expect(pivotMonthlyByProvider(rows, 1, 3).providers).toEqual(["alpha", "Other"]);

    const sumRows = [mrow("alpha", "2026-01", 100, 1), mrow("small1", "2026-01", 7, 2), mrow("small2", "2026-01", 3, 5)];
    const pSum = pivotMonthlyByProvider(sumRows, 1, 6);
    expect(pSum.cells.get("Other")?.get("2026-01")).toEqual({
      provider_id: "Other", month: "2026-01", unique_requests: 7, total_rows: 7,
      total_prompt_tokens: 70, total_completion_tokens: 140, total_cost_usd: 10,
    });
  });

  it("handles ranking ties, zero costs, and overwrites", () => {
    expect(pivotMonthlyByProvider([mrow("p1", "2026-01", 5), mrow("p2", "2026-01", 5)], 10, 6).providers).toEqual(["p1", "p2"]);
    expect(pivotMonthlyByProvider([mrow("p2", "2026-01", 5), mrow("p1", "2026-01", 5)], 10, 6).providers).toEqual(["p2", "p1"]);
    const pZero = pivotMonthlyByProvider([mrow("alpha", "2026-01", 100), mrow("zed", "2026-01", 0)], 10, 6);
    expect(pZero.providers).toEqual(["alpha", "zed"]);
    expect(pZero.totalsByProvider.get("zed")).toBe(0);

    const pDup = pivotMonthlyByProvider([mrow("alpha", "2026-01", 100), mrow("alpha", "2026-01", 7)], 10, 6);
    expect(pDup.totalsByProvider.get("alpha")).toBe(7);
    expect(pDup.grandTotal).toBe(7);
  });
});

describe("groupByStatus", () => {
  it("handles empty and zero counts", () => {
    expect(groupByStatus([])).toEqual({ s2xx: 0, s4xx: 0, s5xx: 0, other: 0 });
    expect(groupByStatus([srow(500, 0)])).toEqual({ s2xx: 0, s4xx: 0, s5xx: 0, other: 0 });
  });

  it("classifies half-open bucket boundaries correctly", () => {
    const cases: Array<[number, keyof ReturnType<typeof groupByStatus>]> = [
      [199, "other"], [200, "s2xx"], [201, "s2xx"], [299, "s2xx"], [300, "other"],
      [301, "other"], [399, "other"], [400, "s4xx"], [404, "s4xx"], [499, "s4xx"],
      [500, "s5xx"], [502, "s5xx"], [599, "s5xx"], [600, "other"], [100, "other"], [0, "other"],
    ];
    for (const [code, bucket] of cases) {
      expect(groupByStatus([srow(code, 1)])).toEqual({
        s2xx: bucket === "s2xx" ? 1 : 0, s4xx: bucket === "s4xx" ? 1 : 0,
        s5xx: bucket === "s5xx" ? 1 : 0, other: bucket === "other" ? 1 : 0,
      });
    }
  });

  it("sums counts within buckets and across rows", () => {
    expect(groupByStatus([
      srow(200, 3), srow(204, 2), srow(404, 7), srow(500, 1), srow(503, 4), srow(302, 6), srow(200, 1),
    ])).toEqual({ s2xx: 6, s4xx: 7, s5xx: 5, other: 6 });
  });
});

describe("buildUsageQuery", () => {
  it("builds query with presets, filters and extras in order", () => {
    expect(buildUsageQuery("custom", "", "")).toBe("");
    expect(buildUsageQuery("custom", "p1", "")).toBe("?provider_id=p1");
    expect(buildUsageQuery("custom", "", "k1")).toBe("?api_key_id=k1");
    expect(buildUsageQuery("30d", "", "")).toBe("?preset=30d");
    expect(buildUsageQuery("30d", "p1", "k1")).toBe("?preset=30d&provider_id=p1&api_key_id=k1");
    expect(buildUsageQuery("7d", "p1", "k1", { limit: "10" })).toBe("?preset=7d&provider_id=p1&api_key_id=k1&limit=10");
    expect(buildUsageQuery("custom", "", "", { limit: "10" })).toBe("?limit=10");
    expect(buildUsageQuery("custom", "", "", { from: "" })).toBe("");
    expect(buildUsageQuery("30d", "", "", { from: "", limit: "5" })).toBe("?preset=30d&limit=5");
    expect(buildUsageQuery("custom", "", "", { limit: "0" })).toBe("?limit=0");
    const to: string = undefined as never;
    expect(buildUsageQuery("custom", "", "", { limit: "10", to })).toBe("?limit=10");
  });

  it("encodes special characters safely", () => {
    const qs = buildUsageQuery("custom", "prov 1", "key/1");
    expect(qs).toBe("?provider_id=prov+1&api_key_id=key%2F1");
    const parsed = new URLSearchParams(qs);
    expect(parsed.get("provider_id")).toBe("prov 1");
    expect(parsed.get("api_key_id")).toBe("key/1");
  });
});

describe("dailyUsageData", () => {
  it("handles empty and maps data to uPlot series", () => {
    expect(dailyUsageData([])).toEqual([[], [], [], []]);
    const data = dailyUsageData([drow("2026-06-21", 5, 1, 1.5), drow("2026-06-22", 0, 0, 0)]);
    expect(data).toEqual([
      [Date.UTC(2026, 5, 21) / 1000, Date.UTC(2026, 5, 22) / 1000],
      [5, 0], [1, 0], [1.5, 0],
    ]);
    const pthru = dailyUsageData([drow("2026-06-22", 2, 0, 2), drow("2026-06-21", 1, 0, 1)]);
    expect(pthru[0]).toEqual([Date.UTC(2026, 5, 22) / 1000, Date.UTC(2026, 5, 21) / 1000]);
    const gap = dailyUsageData([drow("2026-06-21", 1, 0, 1), drow("2026-06-23", 3, 1, 3)]);
    expect(gap[0]).toHaveLength(2);
  });
});

describe("formatters", () => {
  it("fmtPercent formats non-finite, standard and small percentages", () => {
    for (const val of [NaN, Infinity, -Infinity]) expect(fmtPercent(val)).toBe("—");
    expect(fmtPercent(0)).toBe("0.0%");
    expect(fmtPercent(0.5)).toBe("50.0%");
    expect(fmtPercent(0.1)).toBe("10.0%");
    expect(fmtPercent(1)).toBe("100.0%");
    expect(fmtPercent(0.999)).toBe("99.9%");
    expect(fmtPercent(0.099)).toBe("9.90%");
    expect(fmtPercent(0.0999)).toBe("9.99%");
    expect(fmtPercent(0.05)).toBe("5.00%");
    expect(fmtPercent(0.001)).toBe("0.10%");
    expect(fmtPercent(-0.05)).toBe("-5.00%");
  });

  it("fmtDuration formats nullish, milliseconds and seconds", () => {
    for (const val of [null, undefined, NaN, Infinity, -Infinity]) expect(fmtDuration(val as any)).toBe("—");
    expect(fmtDuration(0)).toBe("0ms");
    expect(fmtDuration(0.4)).toBe("0ms");
    expect(fmtDuration(0.5)).toBe("1ms");
    expect(fmtDuration(999)).toBe("999ms");
    expect(fmtDuration(999.4)).toBe("999ms");
    expect(fmtDuration(1000)).toBe("1.00s");
    expect(fmtDuration(1500)).toBe("1.50s");
    expect(fmtDuration(1234567)).toBe("1234.57s");
    expect(fmtDuration(999.6)).toBe("1000ms");
  });

  it("dateToSeconds parses dates and handles fallbacks", () => {
    expect(dateToSeconds("2026-06-21")).toBe(Date.UTC(2026, 5, 21) / 1000);
    expect(dateToSeconds("2026-12-31")).toBe(Date.UTC(2026, 11, 31) / 1000);
    expect(dateToSeconds("2024-02-29")).toBe(Date.UTC(2024, 1, 29) / 1000);
    expect(dateToSeconds("1970-01-01")).toBe(0);
    expect(dateToSeconds("garbage")).toBe(0);
    expect(dateToSeconds("")).toBe(0);
    expect(dateToSeconds("2026-13-01")).toBe(0);
  });
});
