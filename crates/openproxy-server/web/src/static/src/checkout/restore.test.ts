// restore.test.ts — what a reload does with the order snapshot.
//
// `expired` on the snapshot is the price clock, not the matcher's verdict: a
// transfer sent before the deadline can still confirm and mint the key, and
// only the receipt persisted with this browser can claim it. Restore must
// therefore RESUME the watch (no QR, no payment instructions, slow cadence)
// instead of dropping the records and asking for a second purchase.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { CheckoutOrder } from "./api.js";

const EXPIRED: CheckoutOrder = {
  order_code: "OP-LATE-LATE",
  status: "expired",
  usd: 5,
  base_usd: 5,
  amount_units: 5_000_123,
  amount_display: "5.000123",
  chain_id: 8453,
  usdc_address: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
  collector_address: "0x0000000000000000000000000000000000000001",
  qr_uri: "ethereum:0x0@8453/transfer",
  qr_svg: "<svg></svg>",
  expires_at: "2020-01-01 00:00:00", // long past: the price window closed
  payment_window_secs: 900,
  confirmation_grace_secs: 600,
};

const RECEIPT = "r".repeat(32);

/** The ids main.ts + the steps touch (mirrors src/static/checkout.html). */
const CHECKOUT_DOM = `
  <button id="tab-buy"></button>
  <button id="tab-mykey"></button>
  <div id="step-buy">
    <div id="usd"></div>
    <button id="create"></button>
    <div id="buy-err" class="err hidden"></div>
  </div>
  <div id="step-pay" class="hidden">
    <div class="qr" id="qr"></div>
    <div class="amount" id="amount"></div>
    <div class="addr" id="addr"></div>
    <div class="timer" id="timer"></div>
    <div class="status" id="pay-status"></div>
    <div id="recovery" class="hidden"><a id="recovery-link"></a><button id="recovery-copy"></button></div>
    <div id="cf-notice" class="hidden"><button id="cf-reload"></button></div>
    <div class="field" id="send-note"><small>Send the exact amount shown.</small></div>
  </div>
  <div id="step-key" class="hidden"><div id="key" class="key hidden"></div><button id="reveal"></button></div>
  <div id="step-mykey" class="hidden">
    <input id="mykey" />
    <button id="mykey-load"></button>
    <div id="mykey-data" class="hidden">
      <div id="my-plan-name"></div><div id="my-plan-credit"></div><div id="my-plan-expiry"></div>
    </div>
    <div id="my-usd"></div>
    <button id="my-buy"></button>
    <div id="mykey-err" class="err hidden"></div>
  </div>
  <div id="step-done" class="hidden"><button id="new-order"></button></div>
  <div id="page-err" class="err hidden"></div>
`;

let polls = 0;

function isHidden(id: string): boolean {
  return document.getElementById(id)?.classList.contains("hidden") ?? false;
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date("2026-01-01T12:00:00Z"));
  sessionStorage.clear();
  polls = 0;
  document.body.innerHTML = CHECKOUT_DOM;
  vi.stubGlobal("fetch", (url: string) => {
    const path = String(url);
    if (path.endsWith("/plans")) {
      return Promise.resolve({
        ok: true,
        status: 200,
        json: async () => ({
          enabled: true,
          plans: [
            { tier_id: 1, name: "Starter", usd: 5, ttl_days: 30, models: [], combos: [] },
          ],
        }),
      });
    }
    if (path.includes("v1/credits/order/")) {
      polls += 1;
      return Promise.resolve({
        ok: true,
        status: 200,
        json: async () => ({ ...EXPIRED, status: "expired", block_number: null }),
      });
    }
    return Promise.resolve({ ok: true, status: 200, json: async () => ({}) });
  });
});

afterEach(async () => {
  // The module under test owns a live poll chain; stop it before the fake
  // timers go away.
  const { stopPolling } = await import("./pay-step.js");
  stopPolling();
  vi.unstubAllGlobals();
  vi.useRealTimers();
  sessionStorage.clear();
  document.body.innerHTML = "";
});

describe("reload with an expired order snapshot", () => {
  it("resumes the late watch with the records intact and no payment instructions", async () => {
    sessionStorage.setItem("op.checkout.order", JSON.stringify(EXPIRED));
    sessionStorage.setItem("op.checkout.receipt", RECEIPT);
    sessionStorage.setItem("op.checkout.creating", "1");
    vi.resetModules();

    await import("./main.js");
    await Promise.resolve();
    await Promise.resolve();

    // The snapshot, receipt and in-flight marker survive the reload: the
    // receipt is the only capability that can claim a late-minted key.
    expect(sessionStorage.getItem("op.checkout.order")).toContain(EXPIRED.order_code);
    expect(sessionStorage.getItem("op.checkout.receipt")).toBe(RECEIPT);
    expect(sessionStorage.getItem("op.checkout.creating")).toBe("1");

    // Watch-only payment step: no QR, no amount/address, no "send the exact
    // amount" note, and no false claim about the charge.
    expect(isHidden("step-buy")).toBe(true);
    expect(isHidden("step-pay")).toBe(false);
    expect(document.getElementById("qr")?.innerHTML).toBe("");
    expect(isHidden("qr")).toBe(true);
    expect(isHidden("amount")).toBe(true);
    expect(isHidden("addr")).toBe(true);
    expect(isHidden("send-note")).toBe(true);
    expect(document.getElementById("pay-status")?.textContent).toContain("Still checking");
    expect(document.body.textContent).not.toMatch(/nothing was charged/i);
    expect(document.body.textContent).not.toMatch(/press buy again/i);

    // Slow cadence: one request now, the next one a minute later.
    expect(polls).toBe(1);
    await vi.advanceTimersByTimeAsync(5_000);
    expect(polls).toBe(1);
    await vi.advanceTimersByTimeAsync(55_000);
    expect(polls).toBe(2);
  });
});
