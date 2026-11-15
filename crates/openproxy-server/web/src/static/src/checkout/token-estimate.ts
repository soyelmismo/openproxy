// checkout/token-estimate.ts — "how many tokens does this plan's credited
// balance buy on this model", the number every model pill carries.
//
// The math itself lives in `lib/pricing-schedule.ts` (the shared engine, a pure
// module with no imports, so esbuild tree-shakes it into the checkout bundle
// without dragging a dashboard chunk in). This file only adapts the wire shape
// and writes the copy — there is exactly ONE formula in the tree, and it is
// the same one the margin-router dashboard renders:
//
//   inTok = credit * 1e6 / ( (1 - .891) * input
//                          + .891 * (cached ?? input)
//                          + (29.3 / 3871.7) * output )
//   total = inTok + inTok * (29.3 / 3871.7)      ← prompt AND output tokens
//
// `estimateAgentTokens` in the shared module already returns `total`; the
// checkout only adds the credit guard and the copy below.
//
// The buyer is quoted the TOTAL: the prompt tokens the balance buys plus the
// output tokens the same agent mix produces (29.3k output per 3871.7k prompt).
// Quoting prompt-only would understate the volume the dashboard shows for the
// same money.
//
// Rules that keep the number honest:
//   * the credit is the plan's CREDITED balance (`credit_usd`, else `usd`) —
//     never the amount charged, never a payment-adjusted figure;
//   * `cached_per_1m === 0` is a real rate and survives; only an ABSENT (null)
//     cache rate falls back to the input rate. A negative or non-finite cache
//     rate is a broken price, not "no cache rate": the estimate is withheld;
//   * an unknown, negative or non-finite input/output rate, a zero denominator
//     or a non-positive balance produce NO estimate;
//   * an unreadable time-of-day schedule withholds the estimate too — it is
//     never silently replaced by the flat rates, which would quote a number
//     the model does not actually bill.

import {
  AGENT_CACHE_HIT_RATIO,
  AGENT_OUTPUT_RATIO,
  estimateAgentTokens,
  formatEstimatedTokens,
  normalizePricingSchedule,
  type TokenRates,
} from "../lib/pricing-schedule.js";
import type { ModelPricing } from "./api.js";

export { AGENT_CACHE_HIT_RATIO, AGENT_OUTPUT_RATIO, estimateAgentTokens, formatEstimatedTokens };
export type { TokenRates };

export interface EstimateRow {
  label: string;
  value: string;
}

/** Everything the popover renders. Text only — the caller never builds HTML. */
export interface EstimatePopover {
  title: string;
  headline: string;
  rows: EstimateRow[];
  note: string;
}

export interface ModelEstimate {
  /** Pill suffix, e.g. "~285M tokens" or "—". */
  badge: string;
  popover: EstimatePopover;
}

const NOTE_SINGLE =
  "Estimated tokens the credited balance covers on this model, at its real " +
  "per-1M rates, for a typical agent mix: 89.1% cached input and 0.76% output. " +
  "Prompt and output tokens are both counted.";

const NOTE_SCHEDULE =
  "Two estimates of the same credited balance, one at this model's off-peak " +
  "rates and one at its peak rates, for a typical agent mix: 89.1% cached " +
  "input and 0.76% output. Prompt and output tokens are both counted.";

const NOTE_UNAVAILABLE =
  "This model publishes no usable price, so no token estimate is shown. The " +
  "plan still credits the balance on the card.";

/** A price we can bill with: a real, finite, non-negative number. */
function isRate(v: unknown): v is number {
  return typeof v === "number" && Number.isFinite(v) && v >= 0;
}

/**
 * Wire rates → engine rates. `null` when the entry cannot be priced.
 *
 * `cached` is deliberately strict: `null`/absent means "no cache rate, bill
 * cache reads as input", but a present-yet-invalid value (negative, NaN) means
 * the price row is broken and must not be papered over.
 */
export function toTokenRates(
  input: number | null | undefined,
  output: number | null | undefined,
  cached: number | null | undefined,
): TokenRates | null {
  if (!isRate(input) || !isRate(output)) return null;
  if (cached == null) return { input_per_1m: input, output_per_1m: output, cached_per_1m: null };
  if (!isRate(cached)) return null;
  return { input_per_1m: input, output_per_1m: output, cached_per_1m: cached };
}

/**
 * Tokens `creditUsd` buys at `rates`, or `null` when the answer is not
 * knowable. The shared engine already returns the TOTAL count (prompt plus the
 * output tokens the same mix produces); the only thing added here is the
 * checkout's rule that a zero/negative balance has no estimate at all instead
 * of "~0 tokens".
 */
export function estimateTotalTokens(creditUsd: number, rates: TokenRates): number | null {
  if (!(creditUsd > 0)) return null; // a zero/negative balance buys nothing
  const tokens = estimateAgentTokens(creditUsd, rates);
  if (tokens == null || tokens <= 0) return null;
  return Number.isFinite(tokens) ? tokens : null;
}

/** "$2.5" / "$0.15" / "$0.0005" — a per-1M rate, trailing zeros trimmed. */
function formatRate(v: number): string {
  if (!Number.isFinite(v) || v < 0) return "—";
  const fixed = v >= 1 ? v.toFixed(2) : v >= 0.01 ? v.toFixed(3) : v.toFixed(6);
  return "$" + fixed.replace(/\.?0+$/, "");
}

/** "$2.5 in · $1.25 cached · $10 out per 1M". */
function ratesSummary(rates: TokenRates): string {
  const cached = rates.cached_per_1m ?? rates.input_per_1m;
  return (
    formatRate(rates.input_per_1m) +
    " in · " +
    formatRate(cached) +
    " cached · " +
    formatRate(rates.output_per_1m) +
    " out per 1M"
  );
}

/** "~882M tokens" → "882M" (the badge range carries one "~" and one unit). */
function magnitude(phrase: string): string {
  return phrase.replace(/^~/, "").replace(/ tokens$/, "");
}

function unavailable(pricing: ModelPricing, note: string): ModelEstimate {
  return {
    badge: "—",
    popover: { title: pricing.name, headline: "Token estimate unavailable", rows: [], note },
  };
}

/**
 * Badge + popover copy for one model of one plan. `creditUsd` is the plan's
 * credited balance, NOT what the buyer pays.
 */
export function buildModelEstimate(creditUsd: number, pricing: ModelPricing): ModelEstimate {
  const rawSchedule = pricing.pricing_schedule;
  if (rawSchedule != null) {
    const schedule = normalizePricingSchedule(rawSchedule);
    if (!schedule) {
      return unavailable(pricing, NOTE_UNAVAILABLE);
    }
    const peakRates = toTokenRates(
      schedule.peak.input_per_1m,
      schedule.peak.output_per_1m,
      schedule.peak.cached_per_1m,
    );
    const offRates = toTokenRates(
      schedule.off_peak.input_per_1m,
      schedule.off_peak.output_per_1m,
      schedule.off_peak.cached_per_1m,
    );
    const peak = peakRates ? estimateTotalTokens(creditUsd, peakRates) : null;
    const off = offRates ? estimateTotalTokens(creditUsd, offRates) : null;
    if (peak == null || off == null) {
      return unavailable(pricing, NOTE_UNAVAILABLE);
    }
    const peakPhrase = formatEstimatedTokens(peak);
    const offPhrase = formatEstimatedTokens(off);
    return {
      // Ascending range: the cheap window is the lower bound of the spend.
      badge: "~" + magnitude(peakPhrase) + "–" + magnitude(offPhrase) + " tokens",
      popover: {
        title: pricing.name,
        headline: offPhrase + " in off-peak hours, " + peakPhrase + " in peak hours",
        rows: [
          { label: "Off-peak", value: offRates ? ratesSummary(offRates) : "—" },
          { label: "Peak", value: peakRates ? ratesSummary(peakRates) : "—" },
        ],
        note: NOTE_SCHEDULE,
      },
    };
  }

  const rates = toTokenRates(
    pricing.pricing_input_per_1m,
    pricing.pricing_output_per_1m,
    pricing.pricing_cached_per_1m,
  );
  const tokens = rates ? estimateTotalTokens(creditUsd, rates) : null;
  if (tokens == null || !rates) {
    return unavailable(pricing, NOTE_UNAVAILABLE);
  }
  return {
    badge: formatEstimatedTokens(tokens),
    popover: {
      title: pricing.name,
      headline: formatEstimatedTokens(tokens),
      rows: [{ label: "Rates", value: ratesSummary(rates) }],
      note: NOTE_SINGLE,
    },
  };
}
