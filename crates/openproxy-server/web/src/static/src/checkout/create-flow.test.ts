// create-flow.test.ts — order creation wiring of the public checkout.
//
// Two rules that lose money when they break:
//   * the "creation in flight" marker is settled ONLY after the order snapshot
//     is durable, so a reload can still resume the purchase instead of
//     orphaning a payable order;
//   * one Idempotency-Key identifies one purchase for ever: a retry can come
//     back with the ORIGINAL order already `expired`. `expired` is the price
//     clock, not the matcher giving up: the snapshot and the receipt MUST be
//     retained (a late block can still mint the key, and only that receipt can
//     claim it), no payment instruction may be painted, and the page must not
//     claim nothing was charged nor push a second payment.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { CheckoutOrder } from "./api.js";
import { initBuy } from "./buy.js";
import { initMyKey } from "./mykey.js";
import { stopPolling } from "./pay-step.js";
import {
  clearKey,
  clearOrder,
  endCreate,
  loadOrder,
  pendingReceipt,
} from "./session.js";

const ORDER: CheckoutOrder = {
  order_code: "OP-TEST-TEST",
  status: "pending",
  usd: 5,
  base_usd: 5,
  amount_units: 5_000_123,
  amount_display: "5.000123",
  chain_id: 8453,
  usdc_address: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
  collector_address: "0x0000000000000000000000000000000000000001",
  qr_uri: "ethereum:0x0@8453/transfer",
  qr_svg: "<svg></svg>",
  expires_at: "2099-01-01 00:00:00",
  payment_window_secs: 900,
  confirmation_grace_secs: 600,
};

interface Reply {
  status: number;
  order?: Partial<CheckoutOrder>;
  error?: string;
}

let postQueue: Reply[] = [];
let postHeaders: Array<Record<string, string>> = [];

const PAY_STEP_DOM = `
  <div id="step-pay" class="hidden"></div>
  <div id="step-key" class="hidden"><div id="key" class="hidden"></div></div>
  <div id="step-done" class="hidden"></div>
  <div id="qr"></div>
  <div><div id="amount"></div></div>
  <div id="addr"></div>
  <div id="timer"></div>
  <div id="pay-status"></div>
  <div id="recovery" class="hidden"><a id="recovery-link"></a><button id="recovery-copy"></button></div>
  <div id="cf-notice" class="hidden"><button id="cf-reload"></button></div>
  <button id="reveal"></button>
  <div id="page-err" class="err hidden"></div>
`;

function mountBuy(): void {
  document.body.innerHTML = `
    <div id="step-buy"></div>
    <div id="usd">
      <label><input type="radio" name="plan-buy" value="tier:3" checked></label>
    </div>
    <!-- The terms checkbox is a buy precondition: the flow fixtures accept it. -->
    <label><input type="checkbox" id="terms-check" checked></label>
    <button id="create"></button>
    <div id="buy-err" class="err hidden"></div>
    <button id="new-order"></button>
    ${PAY_STEP_DOM}
  `;
  initBuy();
}

function mountMyKey(): void {
  document.body.innerHTML = `
    <div id="step-mykey"></div>
    <input id="mykey" value="op_live_owner" />
    <button id="mykey-load"></button>
    <div id="mykey-data" class="hidden">
      <div id="my-plan-name"></div>
      <div id="my-plan-credit"></div>
      <div id="my-plan-expiry"></div>
    </div>
    <div id="my-usd">
      <label><input type="radio" name="plan-renew" value="tier:5" checked></label>
    </div>
    <!-- The terms checkbox is a buy precondition: the flow fixtures accept it. -->
    <label><input type="checkbox" id="terms-check-my" checked></label>
    <button id="my-buy"></button>
    <div id="mykey-err" class="err hidden"></div>
    ${PAY_STEP_DOM}
  `;
  initMyKey();
}

function click(id: string): void {
  document.getElementById(id)?.click();
}

function text(id: string): string {
  return document.getElementById(id)?.textContent ?? "";
}

function isHidden(id: string): boolean {
  return document.getElementById(id)?.classList.contains("hidden") ?? false;
}

const flush = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

async function settle(): Promise<void> {
  for (let i = 0; i < 5; i += 1) await flush();
}

beforeEach(() => {
  postQueue = [];
  postHeaders = [];
  clearOrder();
  clearKey();
  endCreate();
  sessionStorage.clear();
  vi.stubGlobal("fetch", (_url: string, init?: RequestInit) => {
    if (init?.method === "POST") {
      postHeaders.push({ ...(init.headers as Record<string, string>) });
      const reply = postQueue.shift() ?? { status: 200 };
      return Promise.resolve({
        ok: reply.status < 400,
        status: reply.status,
        json: async () =>
          reply.error
            ? { error: { message: reply.error } }
            : { ...ORDER, ...reply.order },
      });
    }
    return Promise.resolve({
      ok: true,
      status: 200,
      json: async () => ({ ...ORDER, status: "pending" }),
    });
  });
});

afterEach(() => {
  stopPolling();
  vi.unstubAllGlobals();
  clearOrder();
  clearKey();
  endCreate();
  sessionStorage.clear();
});

describe("anonymous buy creation", () => {
  it("settles the creation marker only after the snapshot is durable", async () => {
    mountBuy();
    click("create");
    await settle();
    expect(postHeaders).toHaveLength(1);
    expect(postHeaders[0]?.["Idempotency-Key"]).toBeDefined();
    expect(pendingReceipt()).toBeNull(); // settled
    expect(loadOrder()?.order_code).toBe(ORDER.order_code);
    expect(isHidden("step-pay")).toBe(false);
    expect(isHidden("step-buy")).toBe(true);
    expect(isHidden("page-err")).toBe(true);
  });

  it("keeps the resume marker when the order snapshot cannot be stored", async () => {
    mountBuy();
    const rawSetItem = Storage.prototype.setItem;
    const spy = vi
      .spyOn(Storage.prototype, "setItem")
      .mockImplementation((key: string, value: string) => {
        if (key === "op.checkout.order") throw new Error("QuotaExceededError");
        rawSetItem.call(sessionStorage, key, value);
      });
    try {
      click("create");
      await settle();
    } finally {
      spy.mockRestore();
    }
    // The marker keeps the receipt, so the next Buy replays the SAME order…
    expect(pendingReceipt()).toBe(postHeaders[0]?.["Idempotency-Key"]);
    // …while the fresh snapshot drives this tab from memory, never from the
    // stale stored one.
    expect(loadOrder()?.order_code).toBe(ORDER.order_code);
    expect(isHidden("step-pay")).toBe(false);
    expect(text("page-err")).toContain("could not save");
  });

  it("keeps the receipt and the snapshot when the replay comes back expired", async () => {
    mountBuy();
    // 1st attempt: the answer never reached the browser (5xx keeps the marker).
    postQueue.push({ status: 502, error: "upstream unavailable" });
    click("create");
    await settle();
    const firstReceipt = postHeaders[0]?.["Idempotency-Key"];
    expect(firstReceipt).toBeDefined();
    expect(pendingReceipt()).toBe(firstReceipt);
    expect(loadOrder()).toBeNull();

    // 2nd attempt: same receipt → the server replays ITS order, window closed.
    postQueue.push({
      status: 200,
      order: { status: "expired", expires_at: "2020-01-01 00:00:00" },
    });
    click("create");
    await settle();
    expect(postHeaders[1]?.["Idempotency-Key"]).toBe(firstReceipt);
    // Retained on purpose: the receipt bound to this order is the only way to
    // claim a key minted by a late confirmation.
    expect(pendingReceipt()).toBe(firstReceipt);
    expect(loadOrder()?.order_code).toBe(ORDER.order_code);
    // The buyer stays on a watch-only payment step: no QR, no instructions, no
    // claim that nothing was charged, no push for a second payment.
    expect(isHidden("step-buy")).toBe(true);
    expect(isHidden("step-pay")).toBe(false);
    expect(document.getElementById("qr")?.innerHTML).toBe("");
    expect(isHidden("qr")).toBe(true);
    expect(text("pay-status")).toContain("Still checking");
    expect(document.body.textContent).not.toMatch(/nothing was charged/i);
    expect(text("page-err")).toBe("");

    // 3rd attempt: the SAME purchase is replayed, never a second payable one.
    postQueue.push({
      status: 200,
      order: { status: "expired", expires_at: "2020-01-01 00:00:00" },
    });
    click("create");
    await settle();
    expect(postHeaders[2]?.["Idempotency-Key"]).toBe(firstReceipt);
    expect(loadOrder()?.order_code).toBe(ORDER.order_code);
    expect(pendingReceipt()).toBe(firstReceipt);
  });
});

describe("renewal creation (my key)", () => {
  it("keeps an expired renewal watchable instead of asking for a second payment", async () => {
    mountMyKey();
    postQueue.push({ status: 502, error: "upstream unavailable" });
    click("my-buy");
    await settle();
    const firstReceipt = postHeaders[0]?.["Idempotency-Key"];
    expect(pendingReceipt()).toBe(firstReceipt);

    postQueue.push({
      status: 200,
      order: { status: "expired", expires_at: "2020-01-01 00:00:00" },
    });
    click("my-buy");
    await settle();
    expect(postHeaders[1]?.["Idempotency-Key"]).toBe(firstReceipt);
    // Same retention rule: the receipt is the claim capability.
    expect(pendingReceipt()).toBe(firstReceipt);
    expect(loadOrder()?.order_code).toBe(ORDER.order_code);
    expect(isHidden("step-mykey")).toBe(true);
    expect(isHidden("step-pay")).toBe(false);
    expect(document.getElementById("qr")?.innerHTML).toBe("");
    expect(text("pay-status")).toContain("Still checking");
    expect(document.body.textContent).not.toMatch(/nothing was charged/i);
  });

  it("does not offer a payment step for a renewal the server already credited", async () => {
    mountMyKey();
    postQueue.push({
      status: 200,
      order: { status: "paid", api_key_id: 7, order_code: "OP-RNEW-RNEW" },
    });
    click("my-buy");
    await settle();
    expect(isHidden("step-done")).toBe(false);
    expect(isHidden("step-pay")).toBe(true);
    expect(pendingReceipt()).toBeNull();
  });
});
