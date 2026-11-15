// views/combos/pricing-schedule-draft.test.ts — pure draft ↔ schedule
// serialization for the combo editor. Locks the PATCH payload shape and the
// strict validation the backend enforces (UTC, zero-padded HH:MM, no 24:00).

import { describe, it, expect } from "vitest";
import {
  DEEPSEEK_PRESET,
  draftFromSchedule,
  emptyDraft,
  presetDraft,
  scheduleFromDraft,
  type ScheduleDraft,
} from "./pricing-schedule-draft.js";
import type { PricingSchedule } from "../../lib/pricing-schedule.js";

const STORED: PricingSchedule = {
  timezone: "UTC",
  peak: { input_per_1m: 0.3, output_per_1m: 1.2, cached_per_1m: 0.006 },
  off_peak: { input_per_1m: 0.15, output_per_1m: 0.6, cached_per_1m: 0.003 },
  peak_windows: [
    { days: [1, 2, 3, 4, 5], start: "01:00", end: "04:00" },
    { days: [1, 2, 3, 4, 5], start: "06:00", end: "10:00" },
  ],
  off_peak_dates: ["2026-02-17"],
};

function draft(overrides: Partial<ScheduleDraft> = {}): ScheduleDraft {
  return {
    enabled: true,
    peak: { input: "0.3", output: "1.2", cached: "0.006" },
    off_peak: { input: "0.15", output: "0.6", cached: "0.003" },
    windows: [{ days: [1, 2, 3, 4, 5], start: "01:00", end: "04:00" }],
    off_peak_dates: [],
    ...overrides,
  };
}

describe("draftFromSchedule", () => {
  it("round-trips a stored schedule without sharing references", () => {
    const d = draftFromSchedule(STORED);
    expect(d.enabled).toBe(true);
    expect(d.peak).toEqual({ input: "0.3", output: "1.2", cached: "0.006" });
    expect(d.windows).toEqual(STORED.peak_windows);
    expect(d.off_peak_dates).toEqual(["2026-02-17"]);

    d.windows[0]!.days.push(7);
    expect(STORED.peak_windows[0]!.days).toEqual([1, 2, 3, 4, 5]);
  });

  it("starts disabled and seeded from the flat combo price when absent", () => {
    const d = draftFromSchedule(null, { input_per_1m: 2, output_per_1m: 4, cached_per_1m: null });
    expect(d.enabled).toBe(false);
    expect(d.peak).toEqual({ input: "2", output: "4", cached: "" });
    expect(d.off_peak).toEqual({ input: "2", output: "4", cached: "" });
    expect(d.windows).toEqual([]);
  });

  it("emptyDraft without a fallback leaves the rates blank", () => {
    expect(emptyDraft().peak).toEqual({ input: "", output: "", cached: "" });
  });
});

describe("scheduleFromDraft", () => {
  it("serializes the canonical UTC payload", () => {
    const { schedule, issues } = scheduleFromDraft(draft());
    expect(issues).toEqual([]);
    expect(schedule).toEqual({
      timezone: "UTC",
      peak: { input_per_1m: 0.3, output_per_1m: 1.2, cached_per_1m: 0.006 },
      off_peak: { input_per_1m: 0.15, output_per_1m: 0.6, cached_per_1m: 0.003 },
      peak_windows: [{ days: [1, 2, 3, 4, 5], start: "01:00", end: "04:00" }],
      off_peak_dates: [],
    });
  });

  it("disabled serializes to null (explicitly clears the schedule)", () => {
    expect(scheduleFromDraft(draft({ enabled: false }))).toEqual({ schedule: null, issues: [] });
  });

  it("accepts zero rates and an optional cached rate", () => {
    const { schedule, issues } = scheduleFromDraft(
      draft({
        peak: { input: "0", output: "0", cached: "" },
        off_peak: { input: "0", output: "0", cached: "0" },
        windows: [],
      }),
    );
    expect(issues).toEqual([]);
    expect(schedule?.peak).toEqual({ input_per_1m: 0, output_per_1m: 0, cached_per_1m: null });
    expect(schedule?.off_peak.cached_per_1m).toBe(0);
  });

  it("accepts an overnight window anchored to its start day", () => {
    const { schedule, issues } = scheduleFromDraft(
      draft({ windows: [{ days: [5], start: "22:00", end: "02:00" }] }),
    );
    expect(issues).toEqual([]);
    expect(schedule?.peak_windows).toEqual([{ days: [5], start: "22:00", end: "02:00" }]);
  });

  it("sorts and de-duplicates nothing silently: duplicates are errors", () => {
    const dupDays = scheduleFromDraft(draft({ windows: [{ days: [2, 2], start: "01:00", end: "02:00" }] }));
    expect(dupDays.schedule).toBeNull();
    expect(dupDays.issues.some((i) => i.field === "windows.0.days")).toBe(true);

    const dupDates = scheduleFromDraft(draft({ off_peak_dates: ["2026-01-01", "2026-01-01"] }));
    expect(dupDates.schedule).toBeNull();
    expect(dupDates.issues.some((i) => i.field === "dates.1")).toBe(true);
  });

  it("rejects malformed times, empty weekdays and equal start/end", () => {
    const cases: Partial<ScheduleDraft>[] = [
      { windows: [{ days: [1], start: "1:00", end: "02:00" }] },
      { windows: [{ days: [1], start: "01:00", end: "24:00" }] },
      { windows: [{ days: [], start: "01:00", end: "02:00" }] },
      { windows: [{ days: [1], start: "09:00", end: "09:00" }] },
    ];
    for (const c of cases) {
      const result = scheduleFromDraft(draft(c));
      expect(result.schedule).toBeNull();
      expect(result.issues.length).toBeGreaterThan(0);
    }
  });

  it("rejects blank required rates and negative rates", () => {
    expect(scheduleFromDraft(draft({ peak: { input: "", output: "1", cached: "" } })).schedule).toBeNull();
    expect(scheduleFromDraft(draft({ off_peak: { input: "1", output: "-1", cached: "" } })).schedule).toBeNull();
  });

  it("ignores blank holiday rows but rejects invalid dates", () => {
    const blank = scheduleFromDraft(draft({ off_peak_dates: [""] }));
    expect(blank.issues).toEqual([]);
    expect(blank.schedule?.off_peak_dates).toEqual([]);

    const bad = scheduleFromDraft(draft({ off_peak_dates: ["2026-02-30"] }));
    expect(bad.schedule).toBeNull();
    expect(bad.issues[0]?.field).toBe("dates.0");
  });
});

describe("DeepSeek preset", () => {
  it("matches the published peak/off-peak pair and weekday windows", () => {
    expect(DEEPSEEK_PRESET.peak).toEqual({ input: "0.3", output: "1.2", cached: "0.006" });
    expect(DEEPSEEK_PRESET.off_peak).toEqual({ input: "0.15", output: "0.6", cached: "0.003" });
    expect(DEEPSEEK_PRESET.windows).toEqual([
      { days: [1, 2, 3, 4, 5], start: "01:00", end: "04:00" },
      { days: [1, 2, 3, 4, 5], start: "06:00", end: "10:00" },
    ]);
    expect(DEEPSEEK_PRESET.off_peak_dates).toEqual([]);
  });

  it("presetDraft is a detached copy: editing it cannot mutate the preset", () => {
    const copy = presetDraft();
    copy.windows[0]!.days.push(7);
    copy.off_peak_dates.push("2026-01-01");
    copy.peak.input = "9";
    expect(DEEPSEEK_PRESET.windows[0]!.days).toEqual([1, 2, 3, 4, 5]);
    expect(DEEPSEEK_PRESET.off_peak_dates).toEqual([]);
    expect(DEEPSEEK_PRESET.peak.input).toBe("0.3");
  });
});
