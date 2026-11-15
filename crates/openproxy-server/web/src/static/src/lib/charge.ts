// lib/charge.ts — what the credit ledger ACTUALLY charged for a log row.
//
// The Cost column must never present `cost_usd` as money collected: that field
// is the calculated price. `credits_debited` is only an exactly-once marker —
// it is set even when a key with no prepaid balance is debited 0, and a real
// debit is clamped to the remaining balance — so neither of them can answer
// "how much was charged?".
//
// The backend records the answer in `charged_units` (integer micro-USD, in the
// same transaction as the debit) and derives `charge_state` from it. `null`
// means "no durable proof of an amount": historical rows and rows whose debit
// was never attempted. Unknown must stay unknown — inferring the amount from
// the price would turn a guess into a billing claim.

export type ChargeState = "charged" | "not_charged" | "unknown";

/** The durable charge evidence carried by RecentUsageRow / UsageDetailRow. */
export interface ChargeFacts {
  /** Micro-USD actually debited; null/absent = no recorded amount. */
  charged_units?: number | null;
  /** Raw exactly-once debit marker (true does NOT imply a positive amount). */
  credits_debited?: boolean;
  /** Verdict computed by the server, used only when the raw columns are absent. */
  charge_state?: string | null;
  /** The calculated price. Secondary data: never rendered as the charge. */
  cost_usd?: number | null;
}

export interface ChargeView {
  state: ChargeState;
  /** USD actually charged; null when the amount is unknown. */
  chargedUsd: number | null;
  /** Amount text for the Cost column ("—" when unknown). */
  amountText: string;
  /** Exact charged amount, 6 decimals: the tooltip never rounds a micro-amount away. */
  exactAmountText: string;
  /** Verdict text: Cobrado / No cobrado / Desconocido. */
  label: string;
  /** The calculated price, always 6 decimals, for the tooltip/detail line. */
  calculatedText: string;
  /** True only when a positive charge is proven. */
  isCharged: boolean;
  /** Tooltip spelling out both numbers so they are never confused. */
  title: string;
}

const LABELS: Record<ChargeState, string> = {
  charged: "Cobrado",
  not_charged: "No cobrado",
  unknown: "Desconocido",
};

function numberOrNull(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

/**
 * USD with enough decimals to keep micro-amounts visible.
 *
 * `exact` keeps the ledger's own precision (6 decimals = 1 micro-USD) for
 * tooltips and detail lines; without it the cell uses the table's 4-decimal
 * format, widening only when a real amount would otherwise print as $0.0000.
 */
export function formatChargedUsd(usd: number | null | undefined, exact = false): string {
  if (usd == null || !Number.isFinite(usd)) return "—";
  const abs = Math.abs(usd);
  const digits = exact ? 6 : abs > 0 && abs < 0.0001 ? 6 : 4;
  return "$" + usd.toFixed(digits);
}

/**
 * Derive the verdict from the durable columns.
 *
 * The server also ships `charge_state`; it is only a fallback for payloads
 * produced before the raw columns existed, so a client can never invent a
 * state the database does not support.
 */
export function chargeStateOf(facts: ChargeFacts): ChargeState {
  const hasDurable = facts.charged_units !== undefined || facts.credits_debited !== undefined;
  if (hasDurable) {
    const units = numberOrNull(facts.charged_units);
    if (units != null) return units > 0 ? "charged" : "not_charged";
    // Marker set without an amount: the debit ran but its size is unrecoverable.
    return facts.credits_debited === true ? "unknown" : "not_charged";
  }
  const wire = facts.charge_state;
  if (wire === "charged" || wire === "not_charged" || wire === "unknown") return wire;
  return "unknown";
}

/** Everything the Cost column needs, from the durable evidence only. */
export function chargeView(facts: ChargeFacts): ChargeView {
  let state = chargeStateOf(facts);
  const units = numberOrNull(facts.charged_units);
  // A "charged" verdict without a recorded amount is not a proof of an amount:
  // keep the verdict honest rather than printing a fabricated $0.0000.
  if (state === "charged" && units == null) state = "unknown";
  const chargedUsd = state === "unknown" ? null : state === "charged" && units != null ? units / 1_000_000 : 0;
  const calculatedUsd = numberOrNull(facts.cost_usd);
  // The calculated price always keeps full micro precision: it is the number a
  // human audits when a charge looks wrong.
  const calculatedText = formatChargedUsd(calculatedUsd, true);
  const amountText = chargedUsd == null ? "—" : formatChargedUsd(chargedUsd);
  const exactAmountText = chargedUsd == null ? "—" : formatChargedUsd(chargedUsd, true);
  const label = LABELS[state];

  let title: string;
  if (state === "unknown") {
    title =
      "Cobrado: importe desconocido — la fila es anterior al registro del importe cobrado. " +
      `Coste calculado (no cobrado necesariamente): ${calculatedText}`;
  } else if (state === "charged") {
    title = `Cobrado: ${exactAmountText} · Coste calculado: ${calculatedText}`;
  } else {
    title = `No cobrado: $0.000000 · Coste calculado: ${calculatedText}`;
  }

  return {
    state,
    chargedUsd,
    amountText,
    exactAmountText,
    label,
    calculatedText,
    isCharged: state === "charged",
    title,
  };
}
