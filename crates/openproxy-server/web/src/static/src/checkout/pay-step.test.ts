// pay-step.test.ts — the payment step must poll exactly as long as the server
// can still credit the order.
//
// Rules that cost real money when they break:
//   * expiry does NOT stop polling and the page never claims the order became
//     uncreditable: the matcher compares BLOCK time and the watcher re-scans
//     recent blocks, so a late `safe` confirmation is still credited;
//   * past `expires_at + grace` the poll only SPACES OUT (a forgotten tab must
//     not hammer the API), it never declares the order dead;
//   * an `expired` replay keeps the snapshot AND the receipt (the only way to
//     claim a key minted by a late block), shows no payment instructions and
//     never claims nothing was charged;
//   * one poll chain at a time: an answer from a replaced order (or from an
//     abandoned request) must not paint over the current payment step;
//   * a reveal that answers after the order changed is dropped before it is
//     stored, acked or painted;
//   * every terminal transition clears every timer, watchdog included.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { CheckoutOrder } from "./api.js";
import {
  enterOrder,
  qrImageDataUri,
  renderPayStep,
  showKey,
  startCountdown,
  startPolling,
  stopPolling,
} from "./pay-step.js";
import {
  beginCreate,
  clearKey,
  clearOrder,
  endCreate,
  loadKey,
  loadOrder,
  loadReceipt,
  pendingReceipt,
  saveReceipt,
} from "./session.js";

/** Deadline is 60 s after the fake "now"; the grace window is 120 s. */
const NOW = new Date("2026-01-01T10:00:00Z");

function makeOrder(patch: Partial<CheckoutOrder> = {}): CheckoutOrder {
  return {
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
    expires_at: "2026-01-01 10:01:00",
    payment_window_secs: 60,
    confirmation_grace_secs: 120,
    ...patch,
  };
}

const ORDER = makeOrder();
const RENEWAL = makeOrder({ order_code: "OP-RENW-RENW", api_key_id: 7 });
const NEXT = makeOrder({ order_code: "OP-NEXT-NEXT" });
/** Receipt bound to the order at creation (the claim capability). */
const RECEIPT = "r".repeat(32);

let polls = 0;
let acks = 0;
/** When true, `/reveal` hangs until `releaseReveal` is called. */
let holdReveal = false;
let releaseReveal: (() => void) | null = null;
/** Per-order answer the mocked gateway gives to GET /order/:code. */
const replies = new Map<string, { status: string; block?: number; key_id?: number | null }>();
/** Order codes whose request is held open until released (stale/late races). */
const releases = new Map<string, () => void>();

function statusText(): string {
  return document.getElementById("pay-status")?.textContent ?? "";
}

function isHidden(id: string): boolean {
  return document.getElementById(id)?.classList.contains("hidden") ?? false;
}

function mount(): void {
  document.body.innerHTML = `
    <div id="step-buy"></div>
    <div id="step-pay"></div>
    <div id="step-done" class="hidden"></div>
    <div id="step-key" class="hidden"></div>
    <div id="qr"></div>
    <div><div id="amount"></div></div>
    <div id="addr"></div>
    <div id="timer"></div>
    <div id="pay-status"></div>
    <div id="recovery" class="hidden"><a id="recovery-link"></a><button id="recovery-copy"></button></div>
    <div id="cf-notice" class="hidden"><button id="cf-reload"></button></div>
    <div id="key" class="key hidden"></div>
    <button id="reveal"></button>
  `;
}

function answer(
  code: string,
  payload: { status: string; block_number: number | null; api_key_id: number | null },
) {
  return {
    ok: true,
    status: 200,
    json: async () => ({ ...ORDER, order_code: code, ...payload }),
  };
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(NOW);
  polls = 0;
  acks = 0;
  holdReveal = false;
  releaseReveal = null;
  replies.clear();
  releases.clear();
  // The in-memory mirror outlives sessionStorage.clear() in one test file.
  clearOrder();
  clearKey();
  endCreate();
  mount();
  vi.stubGlobal("fetch", (url: string) => {
    const path = String(url);
    if (path.endsWith("/reveal")) {
      if (holdReveal) {
        return new Promise((resolve) => {
          releaseReveal = () =>
            resolve({
              ok: true,
              status: 200,
              json: async () => ({ key: "op_live_stale" }),
            });
        });
      }
      return Promise.resolve({
        ok: true,
        status: 200,
        json: async () => ({ key: "op_live_replay" }),
      });
    }
    if (path.endsWith("/ack")) {
      acks += 1;
      return Promise.resolve({ ok: true, status: 200, json: async () => ({ ok: true }) });
    }
    polls += 1;
    const code = path.split("/").pop() ?? "";
    const reply = replies.get(code);
    if (reply?.status === "deferred") {
      return new Promise((resolve) => {
        releases.set(code, () =>
          resolve(
            answer(code, {
              status: "paid",
              block_number: reply.block ?? 42,
              api_key_id: reply.key_id ?? null,
            }),
          ),
        );
      });
    }
    return Promise.resolve(
      answer(code, {
        status: reply?.status ?? "pending",
        block_number: reply?.block ?? null,
        api_key_id: reply?.key_id ?? null,
      }),
    );
  });
});

afterEach(() => {
  stopPolling();
  sessionStorage.clear();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("QR rendering (audit H-6: no innerHTML sink on the money surface)", () => {
  it("renders the server SVG through an inert <img> data URI, never as markup", () => {
    const hostile = makeOrder({
      qr_svg:
        '<svg xmlns="http://www.w3.org/2000/svg"><script>alert(1)</script>' +
        '<path d="M0 0h10v10z" fill="#000"/></svg>',
    });
    renderPayStep(hostile);
    const qr = document.getElementById("qr")!;
    // Exactly one child, an IMG whose src is the wrapped data URI: in an
    // image context SVG scripts never execute, whatever the payload holds.
    const img = qr.querySelector("img");
    expect(img).not.toBeNull();
    expect(img!.getAttribute("src")).toBe(qrImageDataUri(hostile.qr_svg));
    expect(qr.querySelectorAll("svg,script,iframe,foreignObject").length).toBe(0);
  });

  it("falls back to the plain EIP-681 text when there is no SVG", () => {
    renderPayStep(makeOrder({ qr_svg: "" }));
    const qr = document.getElementById("qr")!;
    expect(qr.querySelector("img")).toBeNull();
    expect(qr.textContent).toContain("ethereum:0x0@8453/transfer");
  });
});

describe("payment window polling", () => {
  it("keeps polling after expiry while the grace window is open", async () => {
    renderPayStep(ORDER);
    startCountdown();
    startPolling();
    await vi.advanceTimersByTimeAsync(70_000); // 10 s past the deadline
    expect(statusText()).toContain("Still checking");
    expect(statusText()).not.toContain("can no longer be credited");
    const before = polls;
    await vi.advanceTimersByTimeAsync(10_000);
    expect(polls).toBeGreaterThan(before);
  });

  it("after the window keeps watching on a slow cadence and never says it is dead", async () => {
    renderPayStep(ORDER);
    startCountdown();
    startPolling();
    // 181 s: deadline (60 s) + grace (120 s) + 1 s.
    await vi.advanceTimersByTimeAsync(181_000);
    expect(statusText()).toContain("Still checking");
    expect(statusText()).not.toContain("no longer");
    expect(statusText()).not.toContain("can no longer be credited");
    expect(statusText()).not.toContain("cannot be credited");
    const settled = polls;
    await vi.advanceTimersByTimeAsync(120_000);
    const slow = polls - settled;
    // Still watched (the watcher can credit a late block)…
    expect(slow).toBeGreaterThan(0);
    // …but spaced out: 120 s can no longer mean ~24 requests.
    expect(slow).toBeLessThanOrEqual(3);
  });

  it("credits an order whose confirmation lands minutes after the window", async () => {
    replies.set(RENEWAL.order_code, { status: "pending" });
    renderPayStep(RENEWAL);
    startCountdown();
    startPolling();
    await vi.advanceTimersByTimeAsync(300_000); // five minutes past the window
    expect(isHidden("step-pay")).toBe(false);
    replies.set(RENEWAL.order_code, { status: "paid", block: 777 });
    await vi.advanceTimersByTimeAsync(60_000);
    expect(statusText()).toContain("Paid");
    expect(isHidden("step-pay")).toBe(true);
    expect(isHidden("step-done")).toBe(false);
    // Terminal state: nothing keeps polling the API.
    const settled = polls;
    await vi.advanceTimersByTimeAsync(600_000);
    expect(polls).toBe(settled);
  });

  it("keeps the chain alive when a request never settles", async () => {
    replies.set(ORDER.order_code, { status: "deferred" });
    renderPayStep(ORDER);
    startPolling();
    expect(polls).toBe(1);
    // 15 s watchdog abandons the stuck request and schedules the next one.
    await vi.advanceTimersByTimeAsync(16_000);
    expect(polls).toBeGreaterThan(1);
  });
});

describe("stale answers", () => {
  it("drops the answer of an order that was replaced while its request was in flight", async () => {
    replies.set(ORDER.order_code, { status: "deferred" });
    renderPayStep(ORDER);
    startPolling(); // first request for ORDER is now held open
    await vi.advanceTimersByTimeAsync(1_000);

    // The buyer starts another order: the old chain must be gone.
    renderPayStep(NEXT);
    startPolling();
    await vi.advanceTimersByTimeAsync(10_000);
    const whileNextPolls = polls;

    // The old order answers PAID after it stopped being the current one.
    releases.get(ORDER.order_code)?.();
    await vi.advanceTimersByTimeAsync(0);

    expect(statusText()).not.toContain("Paid");
    expect(isHidden("step-done")).toBe(true);
    expect(isHidden("step-key")).toBe(true);
    expect(document.getElementById("amount")?.textContent).toBe("5.000123 USDC");

    // And the new order keeps being watched.
    await vi.advanceTimersByTimeAsync(10_000);
    expect(polls).toBeGreaterThan(whileNextPolls);
  });

  it("clears the previous order's timers as soon as a new payment step renders", async () => {
    renderPayStep(ORDER);
    startCountdown();
    startPolling();
    await vi.advanceTimersByTimeAsync(10_000);
    renderPayStep(NEXT);
    const settled = polls;
    await vi.advanceTimersByTimeAsync(120_000);
    expect(polls).toBe(settled);
  });

  it("does not store, ack or paint a key that arrives after the order was replaced", async () => {
    saveReceipt(RECEIPT);
    holdReveal = true;
    replies.set(ORDER.order_code, { status: "paid", block: 11 });
    renderPayStep(ORDER);
    startPolling();
    // The paid answer launched /reveal, which is now held open.
    await vi.advanceTimersByTimeAsync(10);
    expect(releaseReveal).not.toBeNull();

    // The buyer starts another purchase while the old reveal is in flight.
    renderPayStep(NEXT);
    startPolling();
    await vi.advanceTimersByTimeAsync(10);

    // The old key answers now: it belongs to a purchase this page no longer
    // shows, so it must be dropped before storage, ack and paint.
    releaseReveal?.();
    await vi.advanceTimersByTimeAsync(10);

    expect(loadKey()).toBeNull();
    expect(acks).toBe(0);
    expect(document.getElementById("key")?.textContent).toBe("");
    expect(isHidden("step-key")).toBe(true);
    expect(isHidden("step-pay")).toBe(false);
    expect(document.getElementById("amount")?.textContent).toBe("5.000123 USDC");
  });

  it("stopPolling cancels the pending watchdog so no obsolete poll starts", async () => {
    replies.set(ORDER.order_code, { status: "deferred" });
    renderPayStep(ORDER);
    startPolling();
    expect(polls).toBe(1);
    stopPolling();
    // The watchdog of the abandoned request is gone, not merely neutered.
    expect(vi.getTimerCount()).toBe(0);
    await vi.advanceTimersByTimeAsync(120_000);
    expect(polls).toBe(1);
  });
});

describe("timer hygiene", () => {
  it("stops polling and the countdown once the key is shown", async () => {
    renderPayStep(ORDER);
    startCountdown();
    startPolling();
    await vi.advanceTimersByTimeAsync(10_000);
    expect(document.getElementById("timer")?.textContent).toBe("Price holds for 0:50");
    const settled = polls;
    showKey("op_live_test");
    await vi.advanceTimersByTimeAsync(600_000);
    expect(polls).toBe(settled);
    // Frozen at the moment of reveal: no countdown timeout survived.
    expect(document.getElementById("timer")?.textContent).toBe("Price holds for 0:50");
    expect(isHidden("step-key")).toBe(false);
    expect(isHidden("step-pay")).toBe(true);
  });
});

describe("replayed purchase (one Idempotency-Key = one order)", () => {
  it("keeps an expired replay watchable: snapshot + receipt retained, no payment instructions", async () => {
    // A real replay still carries the marker: the creation never settled.
    expect(beginCreate(RECEIPT)).toBe(true);
    const entry = enterOrder({ ...ORDER, status: "expired" });
    expect(entry).toEqual({ kind: "late", durable: true });
    // Retained: only this receipt can claim a key minted by a late block.
    expect(loadOrder()?.order_code).toBe(ORDER.order_code);
    expect(loadReceipt()).toBe(RECEIPT);
    expect(pendingReceipt()).toBe(RECEIPT);
    // No QR, amount, address or "send the exact amount" instruction…
    expect(document.getElementById("qr")?.innerHTML).toBe("");
    expect(isHidden("qr")).toBe(true);
    expect(isHidden("amount")).toBe(true);
    expect(isHidden("addr")).toBe(true);
    // …and no false claim about the charge, no push for a second payment.
    expect(statusText()).toContain("Still checking");
    expect(document.body.textContent).not.toMatch(/nothing was charged/i);
    expect(document.body.textContent).not.toMatch(/press buy again/i);
    expect(document.body.textContent).not.toMatch(/no longer be credited/i);
    // Watched on the slow beat from the first scheduled poll (5 s would hammer
    // an order whose price window is already closed).
    expect(polls).toBe(1);
    await vi.advanceTimersByTimeAsync(5_000);
    expect(polls).toBe(1);
    await vi.advanceTimersByTimeAsync(55_000);
    expect(polls).toBe(2);
  });

  it("delivers the key when an expired replay confirms late, receipt untouched", async () => {
    beginCreate(RECEIPT);
    replies.set(ORDER.order_code, { status: "expired" });
    const entry = enterOrder({ ...ORDER, status: "expired" });
    expect(entry.kind).toBe("late");
    await vi.advanceTimersByTimeAsync(60_000); // still watching
    // The block confirms after the window: the matcher mints the key.
    replies.set(ORDER.order_code, { status: "minted", block: 55 });
    await vi.advanceTimersByTimeAsync(60_000);
    expect(isHidden("step-key")).toBe(false);
    expect(document.getElementById("key")?.textContent).toBe("op_live_replay");
    expect(isHidden("step-pay")).toBe(true);
    // The claim capability was never rotated; the creation is settled now that
    // the purchase is delivered.
    expect(loadReceipt()).toBe(RECEIPT);
    expect(pendingReceipt()).toBeNull();
    // No QR was ever painted for this order.
    expect(document.getElementById("qr")?.innerHTML).toBe("");
  });

  it("lands a replayed renewal that was already credited on the done step", () => {
    const entry = enterOrder({ ...RENEWAL, status: "paid" });
    expect(entry).toEqual({ kind: "settled" });
    expect(isHidden("step-done")).toBe(false);
    expect(isHidden("step-pay")).toBe(true);
    expect(polls).toBe(0);
  });

  it("reveals a replayed mint without a second payment (never reads it as a renewal)", async () => {
    saveReceipt(RECEIPT);
    // The STATUS response carries the key id the matcher assigned; the order
    // snapshot does not. A mint must never be classified as a renewal.
    replies.set("OP-MINT-MINT", { status: "minted", block: 9, key_id: 99 });
    const entry = enterOrder(makeOrder({ order_code: "OP-MINT-MINT" }));
    expect(entry.kind).toBe("pay");
    await vi.advanceTimersByTimeAsync(10);
    expect(isHidden("step-key")).toBe(false);
    expect(document.getElementById("key")?.textContent).toBe("op_live_replay");
  });
});
