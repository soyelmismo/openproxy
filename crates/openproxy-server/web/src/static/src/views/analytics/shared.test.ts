// shared.test.ts — unit tests for `views/analytics/shared.ts`.
//
// Scope: only pure, exported, regression-prone helpers. Deliberately
// NOT tested here (documented gaps, not oversights):
// - `parseHashParams` / `setHashParams`: impure — read/write
//   `location.hash`; would need DOM URL plumbing, not value-unit
//   material.
// - `fmtCost` / `fmtNumber` / `fmtDateTime`: locale-dependent via
//   `Intl` — assertions would be fragile across environments.
// - `card` / `metric`: thin lit-html template wrappers.
// - `dailyUsageData`: NOTE — it does NOT fill temporal gaps with zero
//   rows; it is a strict 1:1 mapper (the server `by_day` aggregation
//   also `GROUP BY date`, so missing days stay missing). Tests below
//   pin that pass-through contract and flag it.

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

// ── Fixture helpers ─────────────────────────────────────────────────

/** Build a `MonthlyByProviderRow` with count fields derived from
 *  `reqs` (×1 rows, ×10 prompt tokens, ×20 completion tokens) so
 *  aggregation tests can hand-verify sums. */
function mrow(
  provider_id: string,
  month: string,
  total_cost_usd: number,
  reqs: number = 1,
): MonthlyByProviderRow {
  return {
    provider_id,
    month,
    unique_requests: reqs,
    total_rows: reqs,
    total_prompt_tokens: reqs * 10,
    total_completion_tokens: reqs * 20,
    total_cost_usd,
  };
}

function srow(status_code: number, count: number): ByStatusRow {
  return { status_code, count };
}

function drow(date: string, unique_requests: number, errors: number, total_cost_usd: number): ByDayRow {
  return {
    date,
    unique_requests,
    total_rows: unique_requests,
    total_prompt_tokens: 0,
    total_completion_tokens: 0,
    total_cost_usd,
    errors,
  };
}

/**
 * Shared pivot fixture spanning 5 months (2 outside a Jan–Mar window):
 * - alpha: 175 in-window (jan 100 / feb 50 / mar 25)
 * - beta:  120 in-window (jan 80 / mar 40; no feb cell)
 * - gamma: 60 in-window (feb 60) + 30 in 2025-11 (outside any ≤4 window)
 * - delta: 999 ONLY in 2025-12 (outside a Jan–Mar window)
 */
const PIVOT_ROWS: MonthlyByProviderRow[] = [
  mrow("alpha", "2026-01", 100, 3),
  mrow("alpha", "2026-02", 50, 2),
  mrow("alpha", "2026-03", 25, 1),
  mrow("beta", "2026-01", 80, 4),
  mrow("beta", "2026-03", 40, 2),
  mrow("gamma", "2026-02", 60, 6),
  mrow("gamma", "2025-11", 30, 3),
  mrow("delta", "2025-12", 999, 9),
];

// ── pivotMonthlyByProvider ──────────────────────────────────────────

describe("pivotMonthlyByProvider", () => {
  it("returns an empty matrix for empty input", () => {
    const pivot = pivotMonthlyByProvider([]);
    expect(pivot.providers).toEqual([]);
    expect(pivot.months).toEqual([]);
    expect(pivot.cells.size).toBe(0);
    expect(pivot.totalsByProvider.size).toBe(0);
    expect(pivot.totalsByMonth.size).toBe(0);
    expect(pivot.grandTotal).toBe(0);
  });

  it("sorts months ascending and keeps every month when maxMonths exceeds the data", () => {
    const pivot = pivotMonthlyByProvider(PIVOT_ROWS, 10, 12);
    expect(pivot.months).toEqual([
      "2025-11", "2025-12", "2026-01", "2026-02", "2026-03",
    ]);
    // With the full window, delta becomes visible and tops the ranking.
    expect(pivot.providers).toEqual(["delta", "alpha", "beta", "gamma"]);
    expect(pivot.totalsByProvider.get("delta")).toBe(999);
    expect(pivot.grandTotal).toBe(1384); // 999 + 175 + 120 + 90
  });

  it("maxMonths keeps only the most recent months of the window", () => {
    const pivot = pivotMonthlyByProvider(PIVOT_ROWS, 10, 3);
    expect(pivot.months).toEqual(["2026-01", "2026-02", "2026-03"]);
  });

  it("maxMonths=1 collapses to the single most recent month", () => {
    const pivot = pivotMonthlyByProvider(PIVOT_ROWS, 10, 1);
    expect(pivot.months).toEqual(["2026-03"]);
    expect(pivot.providers).toEqual(["beta", "alpha"]); // 40 vs 25
    expect(pivot.grandTotal).toBe(65);
  });

  it("maxMonths=0 returns an empty matrix instead of falling through 'all months'", () => {
    // Regression guard for the `slice(-0) === slice(0)` footgun: the
    // caller explicitly asked for zero months, so we honour that with
    // an empty window instead of silently returning the full timeline.
    const pivot = pivotMonthlyByProvider(PIVOT_ROWS, 10, 0);
    expect(pivot.months).toEqual([]);
    expect(pivot.providers).toEqual([]);
    expect(pivot.cells.size).toBe(0);
    expect(pivot.totalsByProvider.size).toBe(0);
    expect(pivot.totalsByMonth.size).toBe(0);
    expect(pivot.grandTotal).toBe(0);
  });

  it("negative maxMonths is also clamped to zero (defensive)", () => {
    // `slice(-(-3))` would be `slice(3)` and return the first 3 months
    // ascending — also nonsensical. We clamp to 0 and return empty.
    const pivot = pivotMonthlyByProvider(PIVOT_ROWS, 10, -3);
    expect(pivot.months).toEqual([]);
    expect(pivot.grandTotal).toBe(0);
  });

  it("omits providers whose data lies entirely outside the visible window", () => {
    const pivot = pivotMonthlyByProvider(PIVOT_ROWS, 10, 3);
    expect(pivot.providers).toEqual(["alpha", "beta", "gamma"]);
    expect(pivot.providers).not.toContain("delta");
    // Not listed at all — not listed with a zero total either.
    expect(pivot.totalsByProvider.has("delta")).toBe(false);
  });

  it("excludes out-of-window costs from totals, cells and grandTotal", () => {
    const pivot = pivotMonthlyByProvider(PIVOT_ROWS, 10, 3);
    // gamma's 2025-11 row (30) and delta's 2025-12 row (999) are outside.
    expect(pivot.totalsByProvider.get("gamma")).toBe(60);
    expect(pivot.cells.get("gamma")?.has("2025-11")).toBe(false);
    expect(pivot.totalsByMonth.get("2026-01")).toBe(180); // 100 + 80
    expect(pivot.totalsByMonth.get("2026-02")).toBe(110); // 50 + 60
    expect(pivot.totalsByMonth.get("2026-03")).toBe(65); // 25 + 40
    expect(pivot.grandTotal).toBe(355);
  });

  it("keeps per-provider cells sparse: missing provider×month pairs stay absent", () => {
    const pivot = pivotMonthlyByProvider(PIVOT_ROWS, 10, 3);
    // beta has no february row — the UI renders "—" for the hole.
    expect(pivot.cells.get("beta")?.has("2026-02")).toBe(false);
    expect(pivot.cells.get("alpha")?.get("2026-02")?.total_cost_usd).toBe(50);
  });

  it("aggregates providers beyond topN into an 'Other' row that is always last", () => {
    const pivot = pivotMonthlyByProvider(PIVOT_ROWS, 2, 3);
    expect(pivot.providers).toEqual(["alpha", "beta", "Other"]);
    expect(pivot.totalsByProvider.get("Other")).toBe(60); // gamma in-window
    const otherRow = pivot.cells.get("Other")?.get("2026-02");
    expect(otherRow).toEqual({
      provider_id: "Other",
      month: "2026-02",
      unique_requests: 6,
      total_rows: 6,
      total_prompt_tokens: 60,
      total_completion_tokens: 120,
      total_cost_usd: 60,
    });
    // Other only exists in months where its providers had data.
    expect(pivot.cells.get("Other")?.has("2026-01")).toBe(false);
    // Aggregation preserves the grand total.
    expect(pivot.grandTotal).toBe(355);
  });

  it("keeps 'Other' in last position even when it outcosts every top provider", () => {
    const rows = [
      mrow("alpha", "2026-01", 175),
      mrow("small1", "2026-01", 100),
      mrow("small2", "2026-01", 100),
    ];
    const pivot = pivotMonthlyByProvider(rows, 1, 3);
    expect(pivot.providers).toEqual(["alpha", "Other"]);
    expect(pivot.totalsByProvider.get("Other")).toBe(200);
    expect(pivot.grandTotal).toBe(375);
  });

  it("sums every count field across providers merged into 'Other'", () => {
    const rows = [
      mrow("alpha", "2026-01", 100, 1), // 1 req, 1 row, 10 prompt, 20 completion
      mrow("small1", "2026-01", 7, 2), // 2 reqs, 2 rows, 20 prompt, 40 completion
      mrow("small2", "2026-01", 3, 5), // 5 reqs, 5 rows, 50 prompt, 100 completion
    ];
    const pivot = pivotMonthlyByProvider(rows, 1, 6);
    expect(pivot.cells.get("Other")?.get("2026-01")).toEqual({
      provider_id: "Other",
      month: "2026-01",
      unique_requests: 7,
      total_rows: 7,
      total_prompt_tokens: 70,
      total_completion_tokens: 140,
      total_cost_usd: 10,
    });
    expect(pivot.totalsByMonth.get("2026-01")).toBe(110);
    expect(pivot.grandTotal).toBe(110);
  });

  it("adds no 'Other' row when the provider count does not exceed topN", () => {
    const pivot = pivotMonthlyByProvider(PIVOT_ROWS, 10, 3);
    expect(pivot.providers).toEqual(["alpha", "beta", "gamma"]);
    expect(pivot.cells.has("Other")).toBe(false);
    expect(pivot.totalsByProvider.has("Other")).toBe(false);
  });

  it("topN=0 sends every provider to 'Other' (boundary)", () => {
    const pivot = pivotMonthlyByProvider(PIVOT_ROWS, 0, 3);
    expect(pivot.providers).toEqual(["Other"]);
    expect(pivot.totalsByProvider.get("Other")).toBe(355);
    expect(pivot.grandTotal).toBe(355);
  });

  it("ranks providers by in-window cost descending, ties broken by first appearance", () => {
    const rowsA = [mrow("p1", "2026-01", 5), mrow("p2", "2026-01", 5)];
    expect(pivotMonthlyByProvider(rowsA, 10, 6).providers).toEqual(["p1", "p2"]);
    const rowsB = [mrow("p2", "2026-01", 5), mrow("p1", "2026-01", 5)];
    expect(pivotMonthlyByProvider(rowsB, 10, 6).providers).toEqual(["p2", "p1"]);
  });

  it("keeps zero-cost providers that have data in the window", () => {
    const rows = [mrow("alpha", "2026-01", 100), mrow("zed", "2026-01", 0)];
    const pivot = pivotMonthlyByProvider(rows, 10, 6);
    expect(pivot.providers).toEqual(["alpha", "zed"]);
    expect(pivot.totalsByProvider.get("zed")).toBe(0);
  });

  it("overwrites duplicate (provider, month) rows — last row wins, no double-count", () => {
    // The API contract guarantees one row per (provider, month); this
    // pins the defensive behavior if that invariant ever breaks.
    const rows = [mrow("alpha", "2026-01", 100), mrow("alpha", "2026-01", 7)];
    const pivot = pivotMonthlyByProvider(rows, 10, 6);
    expect(pivot.totalsByProvider.get("alpha")).toBe(7);
    expect(pivot.cells.get("alpha")?.get("2026-01")?.total_cost_usd).toBe(7);
    expect(pivot.grandTotal).toBe(7);
  });
});

// ── groupByStatus ───────────────────────────────────────────────────

describe("groupByStatus", () => {
  it("returns all-zero buckets for empty input", () => {
    expect(groupByStatus([])).toEqual({ s2xx: 0, s4xx: 0, s5xx: 0, other: 0 });
  });

  it("classifies half-open bucket boundaries correctly", () => {
    const cases: Array<[number, keyof ReturnType<typeof groupByStatus>]> = [
      [199, "other"],
      [200, "s2xx"],
      [201, "s2xx"],
      [299, "s2xx"],
      [300, "other"], // 3xx is NOT 2xx
      [301, "other"],
      [399, "other"],
      [400, "s4xx"], // exactly 400 is 4xx
      [404, "s4xx"],
      [499, "s4xx"],
      [500, "s5xx"], // exactly 500 is 5xx
      [502, "s5xx"],
      [599, "s5xx"], // 5xx bucket is closed at 599
      [600, "other"],
      [100, "other"],
      [0, "other"],
    ];
    for (const [code, bucket] of cases) {
      const buckets = groupByStatus([srow(code, 1)]);
      expect(buckets, `status ${code}`).toEqual({
        s2xx: bucket === "s2xx" ? 1 : 0,
        s4xx: bucket === "s4xx" ? 1 : 0,
        s5xx: bucket === "s5xx" ? 1 : 0,
        other: bucket === "other" ? 1 : 0,
      });
    }
  });

  it("sums counts within buckets and across rows", () => {
    const buckets = groupByStatus([
      srow(200, 3),
      srow(204, 2),
      srow(404, 7),
      srow(500, 1),
      srow(503, 4),
      srow(302, 6), // → other
      srow(200, 1), // same bucket twice
    ]);
    expect(buckets).toEqual({ s2xx: 6, s4xx: 7, s5xx: 5, other: 6 });
  });

  it("ignores zero-count rows", () => {
    expect(groupByStatus([srow(500, 0)])).toEqual({ s2xx: 0, s4xx: 0, s5xx: 0, other: 0 });
  });
});

// ── buildUsageQuery ─────────────────────────────────────────────────

describe("buildUsageQuery", () => {
  it("returns an empty string when everything is filtered out", () => {
    expect(buildUsageQuery("custom", "", "")).toBe("");
  });

  it("omits the preset for 'custom' but keeps explicit ids", () => {
    expect(buildUsageQuery("custom", "p1", "")).toBe("?provider_id=p1");
    expect(buildUsageQuery("custom", "", "k1")).toBe("?api_key_id=k1");
  });

  it("omits empty provider/key filters", () => {
    expect(buildUsageQuery("30d", "", "")).toBe("?preset=30d");
    expect(buildUsageQuery("30d", "p1", "k1")).toBe(
      "?preset=30d&provider_id=p1&api_key_id=k1",
    );
  });

  it("emits params in a deterministic order: preset, provider_id, api_key_id, extras", () => {
    expect(buildUsageQuery("7d", "p1", "k1", { limit: "10" })).toBe(
      "?preset=7d&provider_id=p1&api_key_id=k1&limit=10",
    );
  });

  it("serializes extras with values but skips empty-string extras", () => {
    expect(buildUsageQuery("custom", "", "", { limit: "10" })).toBe("?limit=10");
    expect(buildUsageQuery("custom", "", "", { from: "" })).toBe("");
    expect(buildUsageQuery("30d", "", "", { from: "", limit: "5" })).toBe(
      "?preset=30d&limit=5",
    );
  });

  it("keeps falsy-string '0' values from extras (truthy string)", () => {
    expect(buildUsageQuery("custom", "", "", { limit: "0" })).toBe("?limit=0");
  });

  it("does not serialize undefined extra values injected by JS callers", () => {
    // Simulate a JS caller whose TS signature only declares `string`
    // values but who passes `undefined` at runtime: the implementation
    // must skip it, not emit `?to=undefined`. The cast is intentionally
    // single-level — we only need to widen `undefined` to satisfy the
    // declared `string` parameter type at the call site.
    const limit = "10";
    const to: string = undefined as never;
    expect(buildUsageQuery("custom", "", "", { limit, to })).toBe("?limit=10");
  });

  it("form-encodes special characters so the query round-trips through URLSearchParams", () => {
    const qs = buildUsageQuery("custom", "prov 1", "key/1");
    expect(qs).toBe("?provider_id=prov+1&api_key_id=key%2F1");
    const parsed = new URLSearchParams(qs);
    expect(parsed.get("provider_id")).toBe("prov 1");
    expect(parsed.get("api_key_id")).toBe("key/1");
  });
});

// ── dailyUsageData ──────────────────────────────────────────────────

describe("dailyUsageData", () => {
  it("returns four empty series for empty input", () => {
    expect(dailyUsageData([])).toEqual([[], [], [], []]);
  });

  it("maps date/requests/errors/cost into aligned uPlot series at UTC midnight", () => {
    const data = dailyUsageData([
      drow("2026-06-21", 5, 1, 1.5),
      drow("2026-06-22", 0, 0, 0),
    ]);
    expect(data).toEqual([
      [Date.UTC(2026, 5, 21) / 1000, Date.UTC(2026, 5, 22) / 1000],
      [5, 0],
      [1, 0],
      [1.5, 0],
    ]);
  });

  it("preserves input row order as-is (pass-through; no sorting)", () => {
    // The server orders by-day rows ASC (`ORDER BY date ASC`); the
    // mapper must not reorder.
    const data = dailyUsageData([drow("2026-06-22", 2, 0, 2), drow("2026-06-21", 1, 0, 1)]);
    expect(data[0]).toEqual([Date.UTC(2026, 5, 22) / 1000, Date.UTC(2026, 5, 21) / 1000]);
    expect(data[1]).toEqual([2, 1]);
  });

  it("does NOT synthesize zero rows for missing days (documented limitation)", () => {
    // GAP: the request expected temporal gaps to be backfilled with 0.
    // Neither this mapper nor the server `by_day` GROUP BY does that —
    // a day with no usage rows simply doesn't exist in the series.
    // This test pins the actual 1:1 pass-through contract.
    const data = dailyUsageData([drow("2026-06-21", 1, 0, 1), drow("2026-06-23", 3, 1, 3)]);
    expect(data[0]).toHaveLength(2); // no 2026-06-22 filler
    expect(data[0]).toEqual([Date.UTC(2026, 5, 21) / 1000, Date.UTC(2026, 5, 23) / 1000]);
  });
});

// ── Formatters ──────────────────────────────────────────────────────

describe("fmtPercent", () => {
  it("renders non-finite ratios as an em dash", () => {
    expect(fmtPercent(NaN)).toBe("—");
    expect(fmtPercent(Infinity)).toBe("—");
    expect(fmtPercent(-Infinity)).toBe("—");
  });

  it("uses one decimal for zero and ratios >= 0.1", () => {
    expect(fmtPercent(0)).toBe("0.0%");
    expect(fmtPercent(0.5)).toBe("50.0%");
    expect(fmtPercent(0.1)).toBe("10.0%"); // threshold: exactly 0.1
    expect(fmtPercent(1)).toBe("100.0%");
    expect(fmtPercent(0.999)).toBe("99.9%");
  });

  it("uses two decimals for small non-zero ratios below 0.1", () => {
    expect(fmtPercent(0.099)).toBe("9.90%");
    expect(fmtPercent(0.0999)).toBe("9.99%");
    expect(fmtPercent(0.05)).toBe("5.00%");
    expect(fmtPercent(0.001)).toBe("0.10%");
    expect(fmtPercent(-0.05)).toBe("-5.00%"); // negatives take the small branch too
  });
});

describe("fmtDuration", () => {
  it("renders nullish and non-finite values as an em dash", () => {
    expect(fmtDuration(null)).toBe("—");
    expect(fmtDuration(undefined)).toBe("—");
    expect(fmtDuration(NaN)).toBe("—");
    expect(fmtDuration(Infinity)).toBe("—");
    expect(fmtDuration(-Infinity)).toBe("—");
  });

  it("renders sub-second values as rounded milliseconds", () => {
    expect(fmtDuration(0)).toBe("0ms");
    expect(fmtDuration(0.4)).toBe("0ms");
    expect(fmtDuration(0.5)).toBe("1ms"); // Math.round boundary
    expect(fmtDuration(999)).toBe("999ms");
    expect(fmtDuration(999.4)).toBe("999ms");
  });

  it("switches to seconds at exactly 1000ms with two decimals", () => {
    expect(fmtDuration(1000)).toBe("1.00s");
    expect(fmtDuration(1500)).toBe("1.50s");
    expect(fmtDuration(1234567)).toBe("1234.57s");
  });

  it("checks the seconds threshold before rounding (999.6 → '1000ms', not '1.00s')", () => {
    // Pins the pre-rounding threshold: values are rounded to ms and
    // only rescaled if the RAW value reached 1000.
    expect(fmtDuration(999.6)).toBe("1000ms");
  });
});

describe("dateToSeconds", () => {
  it("parses YYYY-MM-DD as UTC midnight seconds", () => {
    expect(dateToSeconds("2026-06-21")).toBe(Date.UTC(2026, 5, 21) / 1000);
    expect(dateToSeconds("2026-12-31")).toBe(Date.UTC(2026, 11, 31) / 1000);
    expect(dateToSeconds("2024-02-29")).toBe(Date.UTC(2024, 1, 29) / 1000); // leap day
  });

  it("maps the epoch day to 0", () => {
    expect(dateToSeconds("1970-01-01")).toBe(0);
  });

  it("falls back to 0 for malformed input", () => {
    expect(dateToSeconds("garbage")).toBe(0);
    expect(dateToSeconds("")).toBe(0);
    expect(dateToSeconds("2026-13-01")).toBe(0); // month 13 does not parse
  });
});
