// checkout/pay-step.ts — the payment step: QR, leverage line, countdown and
// the 5s on-chain poll that flips the page to the key-reveal step.

import { $, hide, show } from "./dom.js";
import type { CheckoutOrder, OrderStatus } from "./api.js";
import { getJson } from "./api.js";

let order: CheckoutOrder | null = null;
let pollTimer: ReturnType<typeof setInterval> | null = null;

/** Current order (null until a buy or a plan renewal creates one). */
export function currentOrder(): CheckoutOrder | null {
  return order;
}

/** Paint the payment step (QR, amount, leverage line, address). */
export function renderPayStep(o: CheckoutOrder): void {
  order = o;
  $("qr").innerHTML = o.qr_svg;
  $("amount").textContent = o.amount_display + " USDC";
  if (o.credit_usd != null && o.credit_usd !== o.base_usd) {
    let cred = document.getElementById("amount-credit");
    if (!cred) {
      cred = document.createElement("div");
      cred.id = "amount-credit";
      cred.style.cssText = "color:#6366f1;font-weight:600;margin-top:4px";
      $("amount").parentElement?.appendChild(cred);
    }
    cred.textContent = "pays out $" + o.credit_usd + " of spendable credit";
  }
  $("addr").textContent = o.collector_address;
}

export function startCountdown(): void {
  if (!order) return;
  const deadline = new Date(order.expires_at).getTime();
  const tick = (): void => {
    const left = Math.max(0, Math.floor((deadline - Date.now()) / 1000));
    const m = Math.floor(left / 60);
    const s = left % 60;
    $("timer").textContent =
      left > 0
        ? "Price holds for " + m + ":" + String(s).padStart(2, "0")
        : "Expired";
    if (left > 0) {
      setTimeout(tick, 1000);
    } else {
      stopPolling();
      $("pay-status").textContent = "Order expired. Create a new one.";
    }
  };
  tick();
}

export function startPolling(): void {
  pollTimer = setInterval(() => {
    const o = order;
    if (!o) return;
    getJson<OrderStatus>("v1/credits/order/" + o.order_code, {
      headers: { Accept: "application/json" },
    })
      .then((st) => {
        if (st.status === "paid" || st.status === "minted") {
          stopPolling();
          $("pay-status").textContent =
            "Paid. Confirmed on-chain (block " + st.block_number + ").";
          hide("step-pay");
          show("step-key");
        }
      })
      .catch(() => { /* transient: keep polling */ });
  }, 5000);
}

export function stopPolling(): void {
  if (pollTimer) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
}
