// lib/pricing-schedule.ts — shared peak/off-peak pricing engine.
//
// Contract shared with the checkout worker (do not change these signatures
// without coordinating):
//
//   TokenRates        { input_per_1m, output_per_1m, cached_per_1m }
//   PricingSchedule   { timezone, peak, off_peak, peak_windows, off_peak_dates }
//   resolvePricing(schedule, nowMs) ->
//     { band: 'peak' | 'off_peak', rates: TokenRates,
//       nextTransitionMs: number | null, nextBand: 'peak' | 'off_peak' | null }
//   estimateAgentTokens(creditUsd, rates) -> number | null
//   formatEstimatedTokens(tokens) -> string
//
// Semantics (mirrors the backend `pricing_schedule` JSON column):
//   - `pricing_schedule: null` disables the schedule (flat price);
//     omitting the field from a PATCH preserves the stored schedule.
//   - `peak_windows[].days` use ISO weekdays: 1 = Monday … 7 = Sunday.
//   - Windows are half-open `[start, end)` and evaluated in UTC. A window
//     whose `end` is before its `start` wraps past midnight and stays
//     anchored to its start day ("overnight anchor start day").
//   - `off_peak_dates` ("YYYY-MM-DD", UTC) force the whole calendar day to
//     off-peak and override any peak window that day.
//   - `timezone` is carried for forward compatibility; the engine always
//     evaluates in UTC, so the same epoch resolves to the same band no
//     matter what `timezone` says.
//
// Pure module: no timers, no fetch, no server tick. Callers pass `nowMs`
// and are responsible for re-resolving (e.g. once per second) and for
// rendering the result.

export type PricingBand = "peak" | "off_peak";

/** Per-1M-token rates in USD. `cached_per_1m: null` bills cache reads at
 *  the input rate, exactly like the combo/model pricing fields. */
export interface TokenRates {
  input_per_1m: number;
  output_per_1m: number;
  cached_per_1m: number | null;
}

/** One recurring peak window. `start`/`end` are "HH:MM" UTC strings. */
export interface PricingWindow {
  days: number[];
  start: string;
  end: string;
}

export interface PricingSchedule {
  timezone: string;
  peak: TokenRates;
  off_peak: TokenRates;
  peak_windows: PricingWindow[];
  off_peak_dates: string[];
}

export interface ResolvedPricing {
  band: PricingBand;
  rates: TokenRates;
  /** Epoch ms of the next instant where the band actually changes. */
  nextTransitionMs: number | null;
  /** Band that starts at `nextTransitionMs`, or null when it never changes. */
  nextBand: PricingBand | null;
}

const MINUTE_MS = 60_000;
const DAY_MS = 86_400_000;

/** Agent token mix used by the margin router (antseed-margin-router
 *  dashboard): 89.1% of input tokens are cache hits, and the router spends
 *  29.3 / 3871.7 output tokens per input token. */
export const AGENT_CACHE_HIT_RATIO = 0.891;
export const AGENT_OUTPUT_RATIO = 29.3 / 3871.7;

function isNonNegativeFinite(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0;
}

interface RateParse {
  rates: TokenRates | null;
  reason: string | null;
}

/** Strict rate-pair parse. Missing is never accepted; `cached_per_1m` may be
 *  null/absent (bill at input) but a present value must be a valid rate. */
function strictRates(raw: unknown, label: string): RateParse {
  if (raw == null || typeof raw !== "object" || Array.isArray(raw)) {
    return { rates: null, reason: `${label} must be an object` };
  }
  const o = raw as Record<string, unknown>;
  const input = o["input_per_1m"];
  const output = o["output_per_1m"];
  if (!isNonNegativeFinite(input)) {
    return { rates: null, reason: `${label}.input_per_1m must be a non-negative number` };
  }
  if (!isNonNegativeFinite(output)) {
    return { rates: null, reason: `${label}.output_per_1m must be a non-negative number` };
  }
  const cachedRaw = o["cached_per_1m"];
  let cached: number | null = null;
  if (cachedRaw != null) {
    if (!isNonNegativeFinite(cachedRaw)) {
      return { rates: null, reason: `${label}.cached_per_1m must be null or a non-negative number` };
    }
    cached = cachedRaw;
  }
  return {
    rates: { input_per_1m: input, output_per_1m: output, cached_per_1m: cached },
    reason: null,
  };
}

/** Parse "HH:MM" into minutes since midnight. `24:00` is accepted only for
 *  an end-of-window value when `allowEndOfDay` is true (1440). Used by the
 *  resolver, which must also read schedules stored by older backends. */
export function parseClockMinutes(value: string, allowEndOfDay = false): number | null {
  const m = /^(\d{1,2}):(\d{2})$/.exec(value.trim());
  if (!m) return null;
  const hours = Number(m[1]);
  const minutes = Number(m[2]);
  if (!Number.isInteger(hours) || !Number.isInteger(minutes)) return null;
  if (minutes > 59 || hours > 24) return null;
  if (hours === 24) return allowEndOfDay && minutes === 0 ? 1440 : null;
  return hours * 60 + minutes;
}

const STRICT_CLOCK_RE = /^([01]\d|2[0-3]):[0-5]\d$/;

/** Strict backend-compatible "HH:MM" (zero-padded, 00:00–23:59, no 24:00). */
export function parseStrictClockMinutes(value: string): number | null {
  if (!STRICT_CLOCK_RE.test(value)) return null;
  return Number(value.slice(0, 2)) * 60 + Number(value.slice(3, 5));
}

/** True for a real calendar day in "YYYY-MM-DD" form. */
export function isPricingDateKey(value: string): boolean {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(value)) return false;
  const ms = Date.parse(value + "T00:00:00.000Z");
  if (!Number.isFinite(ms)) return false;
  return new Date(ms).toISOString().slice(0, 10) === value;
}

function strictWindow(raw: unknown, index: number): { window: PricingWindow | null; reason: string | null } {
  const label = `peak_windows[${index}]`;
  if (raw == null || typeof raw !== "object" || Array.isArray(raw)) {
    return { window: null, reason: `${label} must be an object` };
  }
  const o = raw as Record<string, unknown>;
  const daysRaw = o["days"];
  if (!Array.isArray(daysRaw) || daysRaw.length === 0) {
    return { window: null, reason: `${label}.days must be a non-empty array` };
  }
  const days: number[] = [];
  for (const day of daysRaw) {
    if (typeof day !== "number" || !Number.isInteger(day) || day < 1 || day > 7) {
      return { window: null, reason: `${label}.days must contain ISO weekdays 1–7` };
    }
    if (days.includes(day)) {
      return { window: null, reason: `${label}.days contains a duplicate day` };
    }
    days.push(day);
  }
  const start = o["start"];
  const end = o["end"];
  if (typeof start !== "string" || parseStrictClockMinutes(start) == null) {
    return { window: null, reason: `${label}.start must be a zero-padded HH:MM UTC time` };
  }
  if (typeof end !== "string" || parseStrictClockMinutes(end) == null) {
    return { window: null, reason: `${label}.end must be a zero-padded HH:MM UTC time` };
  }
  if (start === end) {
    return { window: null, reason: `${label} has an empty start/end range` };
  }
  return { window: { days: [...days].sort((a, b) => a - b), start, end }, reason: null };
}

export type PricingScheduleStatus = "absent" | "invalid" | "ok";

export interface ParsedPricingSchedule {
  status: PricingScheduleStatus;
  schedule: PricingSchedule | null;
  /** Why a present value is unusable; null for absent/ok. */
  reason: string | null;
}

function invalid(reason: string): ParsedPricingSchedule {
  return { status: "invalid", schedule: null, reason };
}

/**
 * Strict parse of the API/DB `pricing_schedule` value.
 *
 * Mirrors the backend's UTC contract: `timezone` must be exactly "UTC",
 * times are zero-padded `HH:MM` (00:00–23:59, no 24:00), weekdays are unique
 * ISO 1–7 values, and every holiday date must be a real `YYYY-MM-DD` day.
 * Any malformed window/day/date rejects the WHOLE schedule — invalid entries
 * are not dropped or de-duplicated into something savable.
 *
 * Absent (`undefined`) or `null` means "no schedule" (flat price). A value
 * that is present but malformed is `invalid`, which callers must surface as
 * *unavailable* — never silently fall back to the flat price.
 */
export function parsePricingSchedule(raw: unknown): ParsedPricingSchedule {
  if (raw === undefined || raw === null) return { status: "absent", schedule: null, reason: null };

  let value: unknown = raw;
  if (typeof value === "string") {
    try {
      value = JSON.parse(value);
    } catch {
      return invalid("pricing_schedule is not valid JSON");
    }
    if (value === null) return { status: "absent", schedule: null, reason: null };
  }
  if (typeof value !== "object" || Array.isArray(value)) return invalid("pricing_schedule must be an object");
  const o = value as Record<string, unknown>;

  if (o["timezone"] !== "UTC") return invalid('pricing_schedule.timezone must be "UTC"');

  const peak = strictRates(o["peak"], "peak");
  if (!peak.rates) return invalid(peak.reason ?? "peak is invalid");
  const offPeak = strictRates(o["off_peak"], "off_peak");
  if (!offPeak.rates) return invalid(offPeak.reason ?? "off_peak is invalid");

  const windowsRaw = o["peak_windows"];
  if (!Array.isArray(windowsRaw)) return invalid("pricing_schedule.peak_windows must be an array");
  const peakWindows: PricingWindow[] = [];
  for (let i = 0; i < windowsRaw.length; i++) {
    const parsed = strictWindow(windowsRaw[i], i);
    if (!parsed.window) return invalid(parsed.reason ?? `peak_windows[${i}] is invalid`);
    peakWindows.push(parsed.window);
  }

  const datesRaw = o["off_peak_dates"];
  if (!Array.isArray(datesRaw)) return invalid("pricing_schedule.off_peak_dates must be an array");
  const dates = new Set<string>();
  for (let i = 0; i < datesRaw.length; i++) {
    const date = datesRaw[i];
    if (typeof date !== "string" || !isPricingDateKey(date)) {
      return invalid(`off_peak_dates[${i}] must be a real YYYY-MM-DD date`);
    }
    if (dates.has(date)) return invalid(`off_peak_dates[${i}] duplicates ${date}`);
    dates.add(date);
  }

  return {
    status: "ok",
    schedule: {
      timezone: "UTC",
      peak: peak.rates,
      off_peak: offPeak.rates,
      peak_windows: peakWindows,
      off_peak_dates: [...dates].sort(),
    },
    reason: null,
  };
}

/** Convenience wrapper: the schedule when it parses, otherwise null. Use
 *  `parsePricingSchedule` when you must distinguish absent from invalid. */
export function normalizePricingSchedule(raw: unknown): PricingSchedule | null {
  return parsePricingSchedule(raw).schedule;
}

interface ParsedWindow {
  days: number[];
  start: number;
  end: number;
}

function parseWindows(schedule: PricingSchedule): ParsedWindow[] {
  const out: ParsedWindow[] = [];
  const windows = Array.isArray(schedule.peak_windows) ? schedule.peak_windows : [];
  for (const w of windows) {
    const start = parseClockMinutes(w.start, false);
    const end = parseClockMinutes(w.end, true);
    if (start == null || end == null || start >= 1440 || start === end) continue;
    out.push({ days: w.days, start, end });
  }
  return out;
}

function utcDayStart(ms: number): number {
  return Math.floor(ms / DAY_MS) * DAY_MS;
}

function utcDateKey(ms: number): string {
  return new Date(ms).toISOString().slice(0, 10);
}

/** ISO weekday: 1 = Monday … 7 = Sunday. 1970-01-01 was a Thursday. */
function isoWeekday(ms: number): number {
  const dayIndex = Math.floor(ms / DAY_MS);
  return (((dayIndex + 3) % 7) + 7) % 7 + 1;
}

function bandAt(
  windows: ParsedWindow[],
  holidays: Set<string>,
  ms: number,
): PricingBand {
  if (holidays.has(utcDateKey(ms))) return "off_peak";
  const dayStart = utcDayStart(ms);
  // Check today's windows and yesterday's overnight tails. A window anchored
  // on day D with end < start runs from D+start to D+1day+end.
  for (const offset of [-1, 0]) {
    const anchor = dayStart + offset * DAY_MS;
    const weekday = isoWeekday(anchor);
    for (const w of windows) {
      if (!w.days.includes(weekday)) continue;
      const from = anchor + w.start * MINUTE_MS;
      const to = anchor + (w.end > w.start ? w.end : w.end + 1440) * MINUTE_MS;
      if (ms >= from && ms < to) return "peak";
    }
  }
  return "off_peak";
}

interface Transition {
  ms: number;
  band: PricingBand;
}

/** Earliest instant strictly after `nowMs` whose band differs from `current`.
 *
 *  Candidate boundaries are UTC midnights plus every window start/end. The
 *  band can only change on one of them, so the first candidate with a new
 *  band is the real transition — adjacent windows (end == next start) are
 *  skipped because the band there is unchanged. Days are scanned one by one
 *  so multi-day holiday runs (however long) are handled exactly; the scan
 *  stops 7 days past the last listed holiday because the weekly pattern is
 *  periodic after that. */
function nextTransition(
  windows: ParsedWindow[],
  holidays: Set<string>,
  nowMs: number,
  current: PricingBand,
): Transition | null {
  const today = utcDayStart(nowMs);
  const todayIndex = Math.floor(today / DAY_MS);

  let lastHolidayIndex = todayIndex;
  for (const key of holidays) {
    const ms = Date.parse(key + "T00:00:00.000Z");
    if (!Number.isFinite(ms)) continue;
    const index = Math.floor(ms / DAY_MS);
    if (index > lastHolidayIndex) lastHolidayIndex = index;
  }
  // Cap the horizon so a pathological date list cannot stall a render.
  const scanDays = Math.min(Math.max(0, lastHolidayIndex - todayIndex) + 8, 3660);

  const pending: number[] = [];
  // Start at yesterday: an overnight window anchored on the previous day can
  // end after `now` (e.g. Mon 22:00–Tue 02:00 at Tue 01:00), and that end is
  // the real next transition. Its start/day-start candidates are already in
  // the past and get skipped below.
  for (let d = -1; d <= scanDays; d++) {
    const dayStart = today + d * DAY_MS;
    pending.push(dayStart);
    const weekday = isoWeekday(dayStart);
    for (const w of windows) {
      if (!w.days.includes(weekday)) continue;
      pending.push(dayStart + w.start * MINUTE_MS);
      pending.push(dayStart + (w.end > w.start ? w.end : w.end + 1440) * MINUTE_MS);
    }
    pending.sort((a, b) => a - b);

    // Boundaries before the next midnight can be settled now; overnight ends
    // stay pending so global ordering across days is preserved.
    const limit = dayStart + DAY_MS;
    while (pending.length > 0 && pending[0]! < limit) {
      const candidate = pending.shift()!;
      if (candidate <= nowMs) continue;
      const band = bandAt(windows, holidays, candidate);
      if (band !== current) return { ms: candidate, band };
    }
  }
  return null;
}

/**
 * Resolve the active band, its rates, and the next real transition.
 *
 * Pure: the only time input is `nowMs`. Re-call it on your own cadence (the
 * Models view does once per second) to drive countdowns and band flips.
 */
export function resolvePricing(schedule: PricingSchedule, nowMs: number): ResolvedPricing {
  const now = Number.isFinite(nowMs) ? nowMs : 0;
  const windows = parseWindows(schedule);
  const holidays = new Set(schedule.off_peak_dates ?? []);
  const band = bandAt(windows, holidays, now);
  const next = nextTransition(windows, holidays, now, band);
  return {
    band,
    rates: band === "peak" ? schedule.peak : schedule.off_peak,
    nextTransitionMs: next ? next.ms : null,
    nextBand: next ? next.band : null,
  };
}

/**
 * Gross tokens (prompt + output) a prepaid credit buys at `rates`, matching
 * the margin router's token table:
 *
 *   inputTokens = credit * 1e6 / (0.109 * input + 0.891 * cached + (29.3/3871.7) * output)
 *   total       = inputTokens * (1 + 29.3/3871.7)
 *
 * Input is the basis; output is weighted into it, then the gross total adds
 * that output volume on top — it is NOT a separate denominator term.
 *
 * `cached_per_1m: null`/missing bills cache reads at the input rate. A cached
 * rate that is present but negative or non-finite makes the estimate
 * unavailable (`null`) — it is not silently treated as missing. Zero prices
 * are valid, but a zero (or negative) weighted denominator also yields `null`
 * rather than Infinity.
 */
export function estimateAgentTokens(creditUsd: number, rates: TokenRates): number | null {
  if (!Number.isFinite(creditUsd) || creditUsd < 0) return null;
  const input = rates.input_per_1m;
  const output = rates.output_per_1m;
  const cachedRaw = rates.cached_per_1m;
  if (cachedRaw != null && (!Number.isFinite(cachedRaw) || cachedRaw < 0)) return null;
  const cached = cachedRaw ?? input;
  if (!Number.isFinite(input) || !Number.isFinite(output) || !Number.isFinite(cached)) {
    return null;
  }
  const weightedCost =
    (1 - AGENT_CACHE_HIT_RATIO) * input +
    AGENT_CACHE_HIT_RATIO * cached +
    AGENT_OUTPUT_RATIO * output;
  if (!Number.isFinite(weightedCost) || weightedCost <= 0) return null;
  const inputTokens = (creditUsd * 1_000_000) / weightedCost;
  return inputTokens * (1 + AGENT_OUTPUT_RATIO);
}

function trimAmount(value: number): string {
  if (value >= 100) return String(Math.round(value));
  if (value >= 10) return String(Math.round(value * 10) / 10);
  return String(Math.round(value * 100) / 100);
}

/** Human token count: `~1.2M tokens`, `~950K tokens`, `~3B tokens`. */
export function formatEstimatedTokens(tokens: number | null | undefined): string {
  if (tokens == null || !Number.isFinite(tokens) || tokens < 0) return "—";
  if (tokens >= 1_000_000_000) return `~${trimAmount(tokens / 1_000_000_000)}B tokens`;
  if (tokens >= 1_000_000) return `~${trimAmount(tokens / 1_000_000)}M tokens`;
  if (tokens >= 1_000) {
    const value = tokens / 1_000;
    const label = trimAmount(value);
    return label === "1000" ? "~1M tokens" : `~${label}K tokens`;
  }
  return `~${Math.round(tokens)} tokens`;
}
