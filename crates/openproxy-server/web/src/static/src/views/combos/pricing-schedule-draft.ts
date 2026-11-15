// views/combos/pricing-schedule-draft.ts — pure draft model behind the combo
// pricing-schedule editor.
//
// The editor keeps every field as a string so a half-typed value survives a
// re-render. These functions are the only place that turns that draft into the
// `pricing_schedule` JSON the backend stores, and back again. Keeping them
// pure makes the validation/serialization testable without a DOM.

import {
  isPricingDateKey,
  parseStrictClockMinutes,
  type PricingSchedule,
  type TokenRates,
} from "../../lib/pricing-schedule.js";

export interface RatesDraft {
  input: string;
  output: string;
  cached: string;
}

export interface WindowDraft {
  days: number[];
  start: string;
  end: string;
}

export interface ScheduleDraft {
  enabled: boolean;
  peak: RatesDraft;
  off_peak: RatesDraft;
  windows: WindowDraft[];
  off_peak_dates: string[];
}

/** One validation problem, keyed to the control that owns it. */
export interface DraftIssue {
  field: string;
  message: string;
}

const EMPTY_RATES: RatesDraft = { input: "", output: "", cached: "" };

function rateToDraft(value: number | null | undefined): string {
  return value == null ? "" : String(value);
}

function ratesToDraft(rates: TokenRates | null | undefined): RatesDraft {
  if (!rates) return { ...EMPTY_RATES };
  return {
    input: rateToDraft(rates.input_per_1m),
    output: rateToDraft(rates.output_per_1m),
    cached: rateToDraft(rates.cached_per_1m),
  };
}

/** A fresh draft. When the combo has a flat price, both bands start from it
 *  so enabling the schedule never silently introduces free ($0) rates. */
export function emptyDraft(fallback?: {
  input_per_1m?: number | null;
  output_per_1m?: number | null;
  cached_per_1m?: number | null;
}): ScheduleDraft {
  const seed = ratesToDraft(
    fallback && (fallback.input_per_1m != null || fallback.output_per_1m != null)
      ? {
          input_per_1m: fallback.input_per_1m ?? 0,
          output_per_1m: fallback.output_per_1m ?? 0,
          cached_per_1m: fallback.cached_per_1m ?? null,
        }
      : null,
  );
  return {
    enabled: false,
    peak: { ...seed },
    off_peak: { ...seed },
    windows: [],
    off_peak_dates: [],
  };
}

/** Hydrate the editor from the stored schedule (or from the flat price when
 *  the combo has none). Always returns a detached copy. */
export function draftFromSchedule(
  schedule: PricingSchedule | null | undefined,
  fallback?: {
    input_per_1m?: number | null;
    output_per_1m?: number | null;
    cached_per_1m?: number | null;
  },
): ScheduleDraft {
  if (!schedule) return emptyDraft(fallback);
  return {
    enabled: true,
    peak: ratesToDraft(schedule.peak),
    off_peak: ratesToDraft(schedule.off_peak),
    windows: (schedule.peak_windows ?? []).map((w) => ({
      days: [...w.days],
      start: w.start,
      end: w.end,
    })),
    off_peak_dates: [...(schedule.off_peak_dates ?? [])],
  };
}

function parseRate(
  raw: string,
  field: string,
  label: string,
  required: boolean,
  issues: DraftIssue[],
): number | null {
  const value = raw.trim();
  if (value === "") {
    if (required) issues.push({ field, message: `${label} rate is required.` });
    return null;
  }
  const parsed = Number(value);
  if (!Number.isFinite(parsed) || parsed < 0) {
    issues.push({ field, message: `${label} rate must be zero or a positive number.` });
    return null;
  }
  return parsed;
}

function parseRates(
  draft: RatesDraft,
  band: "peak" | "off_peak",
  label: string,
  issues: DraftIssue[],
): TokenRates {
  return {
    input_per_1m: parseRate(draft.input, `${band}.input`, `${label} input`, true, issues) ?? 0,
    output_per_1m: parseRate(draft.output, `${band}.output`, `${label} output`, true, issues) ?? 0,
    cached_per_1m: parseRate(draft.cached, `${band}.cached`, `${label} cached`, false, issues),
  };
}

/** Turn the editor draft into the PATCH payload value.
 *
 *  - disabled → `null` (explicitly clears the stored schedule)
 *  - enabled  → a `PricingSchedule` in UTC
 *  - any issue → `schedule: null` + the issue list; the caller must not save.
 *
 *  Blank holiday rows are ignored (a freshly added, still-empty row is not an
 *  error); non-empty malformed dates are.
 */
export interface DraftScheduleResult {
  schedule: PricingSchedule | null;
  issues: DraftIssue[];
}

export function scheduleFromDraft(draft: ScheduleDraft): DraftScheduleResult {
  if (!draft.enabled) return { schedule: null, issues: [] };

  const issues: DraftIssue[] = [];
  const peak = parseRates(draft.peak, "peak", "Peak", issues);
  const offPeak = parseRates(draft.off_peak, "off_peak", "Off-peak", issues);

  const windows: PricingSchedule["peak_windows"] = [];
  draft.windows.forEach((w, index) => {
    const days: number[] = [];
    let daysOk = w.days.length > 0;
    if (!daysOk) {
      issues.push({ field: `windows.${index}.days`, message: `Window ${index + 1}: pick at least one weekday.` });
    }
    for (const day of w.days) {
      if (!Number.isInteger(day) || day < 1 || day > 7) {
        issues.push({ field: `windows.${index}.days`, message: `Window ${index + 1}: weekdays must be ISO 1–7.` });
        daysOk = false;
        break;
      }
      if (days.includes(day)) {
        issues.push({ field: `windows.${index}.days`, message: `Window ${index + 1}: duplicate weekday.` });
        daysOk = false;
        break;
      }
      days.push(day);
    }
    days.sort((a, b) => a - b);
    const startRaw = w.start.trim();
    const endRaw = w.end.trim();
    const start = parseStrictClockMinutes(startRaw);
    const end = parseStrictClockMinutes(endRaw);
    if (start == null) {
      issues.push({
        field: `windows.${index}.start`,
        message: `Window ${index + 1}: start must be a zero-padded 00:00–23:59 UTC time.`,
      });
    }
    if (end == null) {
      issues.push({
        field: `windows.${index}.end`,
        message: `Window ${index + 1}: end must be a zero-padded 00:00–23:59 UTC time (use an overnight window instead of 24:00).`,
      });
    }
    if (start != null && end != null && start === end) {
      issues.push({ field: `windows.${index}.end`, message: `Window ${index + 1}: start and end cannot be equal.` });
    }
    if (daysOk && start != null && end != null && start !== end) {
      windows.push({ days, start: startRaw, end: endRaw });
    }
  });

  const dates = new Set<string>();
  draft.off_peak_dates.forEach((raw, index) => {
    const value = raw.trim();
    if (value === "") return;
    if (!isPricingDateKey(value)) {
      issues.push({ field: `dates.${index}`, message: `Holiday ${index + 1}: use a real YYYY-MM-DD date.` });
      return;
    }
    if (dates.has(value)) {
      issues.push({ field: `dates.${index}`, message: `Holiday ${index + 1}: ${value} is listed twice.` });
      return;
    }
    dates.add(value);
  });

  if (issues.length > 0) return { schedule: null, issues };

  return {
    schedule: {
      timezone: "UTC",
      peak,
      off_peak: offPeak,
      peak_windows: windows,
      off_peak_dates: [...dates].sort(),
    },
    issues,
  };
}

/** DeepSeek's published peak/off-peak pair (USD per 1M):
 *  peak 0.3 in / 0.006 cached / 1.2 out; off-peak half that.
 *
 *  Loading the preset only fills the form — it never saves and never touches
 *  any other combo. Holidays stay manual (`off_peak_dates: []`). */
export const DEEPSEEK_PRESET: ScheduleDraft = {
  enabled: true,
  peak: { input: "0.3", output: "1.2", cached: "0.006" },
  off_peak: { input: "0.15", output: "0.6", cached: "0.003" },
  windows: [
    { days: [1, 2, 3, 4, 5], start: "01:00", end: "04:00" },
    { days: [1, 2, 3, 4, 5], start: "06:00", end: "10:00" },
  ],
  off_peak_dates: [],
};

/** Detached copy of the preset, safe to mutate in the editor. */
export function presetDraft(): ScheduleDraft {
  return {
    enabled: true,
    peak: { ...DEEPSEEK_PRESET.peak },
    off_peak: { ...DEEPSEEK_PRESET.off_peak },
    windows: DEEPSEEK_PRESET.windows.map((w) => ({ days: [...w.days], start: w.start, end: w.end })),
    off_peak_dates: [],
  };
}
