// checkout/buy.ts — anonymous buy flow: order → pay step → key reveal.
// The minted plaintext is revealed through the retryable receipt flow, so a
// dropped connection never costs the buyer the key.

import { $, err, hide, show } from "./dom.js";
import { postJson, type CheckoutOrder } from "./api.js";
import {
  currentOrder,
  enterOrder,
  revealOnce,
  showKey,
} from "./pay-step.js";
import { clearKey, clearOrder, endCreate, prepareCreate } from "./session.js";
import { parseSelection, selectedPlanValue } from "./catalog.js";

export function initBuy(): void {
  $("create").addEventListener("click", function (this: HTMLButtonElement) {
    const btn = this;
    const selection = selectedPlanValue("usd");
    if (!selection) {
      err("buy-err", "Pick a plan first.");
      return;
    }
    // The terms acceptance is a PRECONDITION of buying: the plan is consumable
    // software delivered on confirmation, so the buyer must have read (or at
    // least reached) the terms before any payable order exists.
    if (!$<HTMLInputElement>("terms-check").checked) {
      err("buy-err", "You must accept the Terms of Service to buy.");
      return;
    }
    btn.disabled = true;
    // The receipt is a PRECONDITION of the order: the server hashes it at
    // creation, so it must exist and be DURABLE before the POST. If storage
    // fails we do not create the order at all — no payment instructions may
    // be shown for an order whose key could never be claimed.
    const prep = prepareCreate();
    if (!prep.ok) {
      err("buy-err", prep.message);
      btn.disabled = false;
      return;
    }
    const body = {
      ...parseSelection(selection),
      mint_new_key: true,
    };
    postJson<CheckoutOrder>("v1/credits/order", body, {
      "Idempotency-Key": prep.receipt,
    })
      .then((res) => {
        if (!res.ok) {
          // A 4xx rejected the request without creating an order: allow a
          // fresh receipt on the next attempt. A 5xx keeps the marker so the
          // retry reuses the same key and stays idempotent server-side.
          if (res.status < 500) endCreate();
          throw new Error(res.body.error?.message || "order failed");
        }
        const entry = enterOrder(res.body);
        // The previous order's delivered key must not be restored later.
        clearKey();
        hide("step-buy");
        if (entry.kind === "settled") return; // enterOrder painted the done step
        show("step-pay");
        if (entry.kind === "late") {
          // Expired: the price window closed, not the purchase. KEEP the
          // creation marker (the receipt is bound to this order for ever and is
          // the only way to claim a key minted by a late confirmation). A new
          // receipt here would lose that key and invite a second payment for
          // the same intent.
          if (!entry.durable) {
            err(
              "page-err",
              "This browser could not save the checkout state (storage full or " +
                "restricted). Keep this tab open: a transfer sent before the " +
                "deadline can still be credited and deliver this order.",
            );
          }
          return;
        }
        // Persist FIRST, settle second: `endCreate` declares the attempt
        // finished. A memory-only snapshot that settled the marker would
        // orphan a payable order on reload.
        const durable = entry.durable;
        if (durable) {
          endCreate();
        } else {
          err(
            "page-err",
            "This browser could not save the checkout state (storage full or " +
              "restricted). Keep this tab open until the key is shown; after a " +
              "reload, pick the same plan and press Buy to resume this order.",
          );
        }
      })
      .catch((e: Error) => {
        err("buy-err", e.message);
      })
      .finally(() => {
        // Re-enable: a second order must be possible without a reload.
        btn.disabled = false;
      });
  });

  $("reveal").addEventListener("click", function (this: HTMLButtonElement) {
    const btn = this;
    const o = currentOrder();
    if (!o) return;
    btn.disabled = true;
    revealOnce()
      .then((key) => {
        if (key) showKey(key);
      })
      .catch((e: Error) => {
        err("page-err", e.message);
      })
      .finally(() => {
        btn.disabled = false;
      });
  });

  $("new-order").addEventListener("click", () => {
    clearOrder();
    clearKey();
    endCreate();
    hide("step-done");
    show("step-buy");
  });
}
