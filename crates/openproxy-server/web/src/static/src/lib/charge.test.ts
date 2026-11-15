// lib/charge.test.ts — the Cost column must report the money the ledger
// actually moved, and must never dress the calculated price up as a charge.

import { describe, it, expect } from "vitest";
import { chargeStateOf, chargeView, formatChargedUsd } from "./charge.js";

describe("chargeStateOf", () => {
  it("treats a positive recorded amount as charged", () => {
    expect(chargeStateOf({ charged_units: 14_600, credits_debited: true })).toBe("charged");
  });

  it("treats a recorded zero as not charged, even with the debit marker set", () => {
    // A key with no prepaid credits runs the debit path, charges 0 and is still
    // marked: the marker alone proves nothing.
    expect(chargeStateOf({ charged_units: 0, credits_debited: true })).toBe("not_charged");
  });

  it("treats a marker without a recorded amount as unknown", () => {
    expect(chargeStateOf({ charged_units: null, credits_debited: true })).toBe("unknown");
  });

  it("treats a clear marker as not charged", () => {
    expect(chargeStateOf({ charged_units: null, credits_debited: false })).toBe("not_charged");
  });

  it("falls back to the server verdict only when the raw columns are absent", () => {
    expect(chargeStateOf({ charge_state: "unknown" })).toBe("unknown");
    expect(chargeStateOf({ charge_state: "not_charged" })).toBe("not_charged");
    // An unrecognised payload cannot prove anything.
    expect(chargeStateOf({})).toBe("unknown");
  });
});

describe("chargeView", () => {
  it("shows the charged amount and its verdict", () => {
    const view = chargeView({ charged_units: 14_600, credits_debited: true, cost_usd: 0.0146004 });
    expect(view.state).toBe("charged");
    expect(view.chargedUsd).toBeCloseTo(0.0146, 9);
    expect(view.amountText).toBe("$0.0146");
    expect(view.label).toBe("Cobrado");
    expect(view.isCharged).toBe(true);
    // The calculated price is still available, as secondary data, at full
    // micro precision.
    expect(view.calculatedText).toBe("$0.014600");
    expect(view.exactAmountText).toBe("$0.014600");
    expect(view.title).toContain("Cobrado: $0.014600");
    expect(view.title).toContain("Coste calculado");
  });

  it("shows 0 for a priced row that charged nothing — never the price", () => {
    const view = chargeView({ charged_units: 0, credits_debited: true, cost_usd: 0.0146004 });
    expect(view.state).toBe("not_charged");
    expect(view.chargedUsd).toBe(0);
    expect(view.amountText).toBe("$0.0000");
    expect(view.label).toBe("No cobrado");
    expect(view.isCharged).toBe(false);
    expect(view.exactAmountText).toBe("$0.000000");
    expect(view.title).toContain("No cobrado: $0.000000");
  });

  it("reports unknown for a historical row instead of inventing the price", () => {
    const view = chargeView({ charged_units: null, credits_debited: true, cost_usd: 0.0146004 });
    expect(view.state).toBe("unknown");
    expect(view.chargedUsd).toBeNull();
    expect(view.amountText).toBe("—");
    expect(view.label).toBe("Desconocido");
    expect(view.amountText).not.toBe(view.calculatedText);
    expect(view.title).toContain("importe desconocido");
  });

  it("keeps an old payload without durable columns honest", () => {
    const view = chargeView({ cost_usd: 0.0146004, charge_state: "unknown" });
    expect(view.state).toBe("unknown");
    expect(view.amountText).toBe("—");
    expect(view.chargedUsd).toBeNull();
  });

  it("does not fabricate $0.0000 when a 'charged' verdict carries no amount", () => {
    const view = chargeView({ charge_state: "charged", cost_usd: 0.0146004 });
    expect(view.state).toBe("unknown");
    expect(view.amountText).toBe("—");
  });

  it("shows the clamped amount of an overrun, not the full price", () => {
    const view = chargeView({ charged_units: 1_000_000, credits_debited: true, cost_usd: 2 });
    expect(view.amountText).toBe("$1.0000");
    expect(view.calculatedText).toBe("$2.000000");
  });

  it("keeps micro-amounts visible, and exact on demand", () => {
    expect(formatChargedUsd(0.000045)).toBe("$0.000045");
    expect(formatChargedUsd(0.034721, true)).toBe("$0.034721");
    expect(formatChargedUsd(0.034721)).toBe("$0.0347");
    expect(formatChargedUsd(0.0146)).toBe("$0.0146");
    expect(formatChargedUsd(null)).toBe("—");
    expect(chargeView({ charged_units: 45, credits_debited: true }).amountText).toBe("$0.000045");
  });
});
