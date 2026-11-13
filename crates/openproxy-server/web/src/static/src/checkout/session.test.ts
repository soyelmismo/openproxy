import { describe, expect, it, beforeEach, vi } from "vitest";
import type { CheckoutOrder } from "./api.js";
import {
  endCreate,
  clearKey,
  clearOrder,
  isRenewal,
  loadKey,
  loadOrder,
  loadReceipt,
  newReceipt,
  parseUtc,
  pendingReceipt,
  prepareCreate,
  saveKey,
  saveOrder,
  saveReceipt,
} from "./session.js";

function order(patch: Partial<CheckoutOrder> = {}): CheckoutOrder {
  return {
    order_code: "OP-AAAA-BBBB",
    status: "pending",
    usd: 5,
    base_usd: 5,
    amount_units: 5_000_123,
    amount_display: "5.000123",
    chain_id: 8453,
    usdc_address: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
    collector_address: "0x12c7ab78f21ba0253a21d34a0151d44ddb824950",
    qr_uri: "ethereum:0x833589",
    qr_svg: "<svg></svg>",
    expires_at: "2026-11-13 10:15:00",
    payment_window_secs: 900,
    ...patch,
  };
}

beforeEach(() => {
  sessionStorage.clear();
  // The in-memory mirror outlives `sessionStorage.clear()` inside one test
  // file; dropping the in-flight marker keeps the tests isolated.
  endCreate();
});

describe("parseUtc", () => {
  it("reads a SQLite timestamp as UTC, not local time", () => {
    expect(parseUtc("2026-11-13 10:15:00")).toBe(Date.UTC(2026, 10, 13, 10, 15, 0));
  });

  it("accepts ISO strings with an explicit zone", () => {
    expect(parseUtc("2026-11-13T10:15:00Z")).toBe(Date.UTC(2026, 10, 13, 10, 15, 0));
  });

  it("treats a bare ISO string as UTC", () => {
    expect(parseUtc("2026-11-13T10:15:00")).toBe(Date.UTC(2026, 10, 13, 10, 15, 0));
  });
});

describe("order persistence", () => {
  it("round-trips the order snapshot so a refresh can resume", () => {
    const o = order();
    saveOrder(o);
    expect(loadOrder()).toEqual(o);
    clearOrder();
    expect(loadOrder()).toBeNull();
  });

  it("treats a key top-up as a renewal (never a reveal)", () => {
    expect(isRenewal(order({ api_key_id: 7 }))).toBe(true);
    expect(isRenewal(order())).toBe(false);
  });

  it("does not mistake an anonymous MINT for a renewal", () => {
    // After the matcher mints, the status response carries api_key_id. The
    // decision must come from the order snapshot taken at creation (which has
    // no api_key_id), or a freshly bought key would never be revealed.
    const minted = order({ status: "minted" });
    expect(isRenewal(minted)).toBe(false);
    expect(isRenewal(order({ status: "minted", api_key_id: 7 }))).toBe(true);
  });
});

describe("reveal receipt", () => {
  it("survives a reload and is long/url-safe enough for the server", () => {
    const r = newReceipt();
    expect(r.length).toBeGreaterThanOrEqual(32);
    expect(r).toMatch(/^[A-Za-z0-9_-]+$/);
    saveReceipt(r);
    expect(loadReceipt()).toBe(r);
  });

  it("keeps the revealed key across a refresh", () => {
    saveKey("op_live_test");
    expect(loadKey()).toBe("op_live_test");
    clearKey();
    expect(loadKey()).toBeNull();
  });

  it("keeps a non-durable key readable in memory so the reveal is not lost", () => {
    const spy = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("QuotaExceededError");
    });
    try {
      expect(saveKey("op_live_memory")).toBe(false);
      // Not durable (no ack may be sent) but the only copy is still readable.
      expect(loadKey()).toBe("op_live_memory");
    } finally {
      spy.mockRestore();
      clearKey();
    }
  });
});

describe("newest snapshot wins", () => {
  it("never resurrects a stale stored order when the write fails", () => {
    saveOrder(order({ order_code: "OP-OLD0-OLD0" }));
    const spy = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("QuotaExceededError");
    });
    try {
      expect(saveOrder(order({ order_code: "OP-NEW0-NEW0" }))).toBe(false);
      // Memory holds the newest value: reading storage first would drive the
      // payment step from the PREVIOUS order's code.
      expect(loadOrder()?.order_code).toBe("OP-NEW0-NEW0");
    } finally {
      spy.mockRestore();
    }
    // The durable record still holds the old snapshot — which is exactly why
    // saveOrder reports `false` and the caller keeps the resume marker.
    const stored = JSON.parse(sessionStorage.getItem("op.checkout.order") ?? "{}") as {
      order_code?: string;
    };
    expect(stored.order_code).toBe("OP-OLD0-OLD0");
    clearOrder();
  });
});

describe("receipt before order creation", () => {
  it("persists a fresh receipt and marks the creation in flight", () => {
    const prep = prepareCreate();
    expect(prep.ok).toBe(true);
    if (!prep.ok) return;
    expect(prep.reused).toBe(false);
    // The server hashes this exact value at creation: it must already be
    // durable when the POST leaves the browser.
    expect(loadReceipt()).toBe(prep.receipt);
    expect(pendingReceipt()).toBe(prep.receipt);

    // A retry (failed/unknown response) reuses it, so the server returns the
    // SAME order instead of opening a second payable amount.
    expect(prepareCreate()).toEqual({
      ok: true,
      receipt: prep.receipt,
      reused: true,
    });

    // Settling the creation keeps the receipt (the reveal needs it) but
    // stops treating it as a pending attempt.
    endCreate();
    expect(pendingReceipt()).toBeNull();
    expect(loadReceipt()).toBe(prep.receipt);
  });

  it("mints a new receipt once the previous creation is settled", () => {
    const first = prepareCreate();
    endCreate();
    const second = prepareCreate();
    expect(first.ok && second.ok).toBe(true);
    if (!first.ok || !second.ok) return;
    expect(second.receipt).not.toBe(first.receipt);
  });

  it("refuses the creation when storage cannot persist the receipt", () => {
    const spy = vi
      .spyOn(Storage.prototype, "setItem")
      .mockImplementation(() => {
        throw new Error("QuotaExceededError");
      });
    try {
      const prep = prepareCreate();
      expect(prep.ok).toBe(false);
      if (prep.ok) return;
      expect(prep.reason).toBe("no_storage");
      // No in-flight creation: the checkout must not show payment steps for
      // an order whose key could never be claimed.
      expect(pendingReceipt()).toBeNull();
    } finally {
      spy.mockRestore();
    }
  });
});
