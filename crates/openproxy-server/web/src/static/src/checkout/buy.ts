// checkout/buy.ts — anonymous buy flow: order → pay step → key reveal.
// The minted plaintext is shown exactly once (server wipes it on reveal).

import { $, err, hide, show } from "./dom.js";
import { postJson, type CheckoutOrder } from "./api.js";
import { renderPayStep, startCountdown, startPolling, currentOrder } from "./pay-step.js";
import { parseSelection } from "./catalog.js";

export function initBuy(): void {
  $("create").addEventListener("click", function (this: HTMLButtonElement) {
    const btn = this;
    btn.disabled = true;
    const body = {
      ...parseSelection(String($<HTMLSelectElement>("usd").value)),
      mint_new_key: true,
    };
    postJson<CheckoutOrder>("v1/credits/order", body)
      .then((res) => {
        if (!res.ok) throw new Error(res.body.error?.message || "order failed");
        hide("step-buy");
        show("step-pay");
        renderPayStep(res.body);
        startCountdown();
        startPolling();
      })
      .catch((e: Error) => {
        btn.disabled = false;
        err("buy-err", e.message);
      });
  });

  $("reveal").addEventListener("click", function (this: HTMLButtonElement) {
    const btn = this;
    const o = currentOrder();
    if (!o) return;
    btn.disabled = true;
    fetch("v1/credits/order/" + o.order_code + "/reveal", { method: "POST" })
      .then((r) => r.json())
      .then((b: { key?: string; error?: { message?: string } }) => {
        if (b.error) throw new Error(b.error.message || "reveal failed");
        const el = $<HTMLInputElement>("key");
        el.textContent = b.key ?? "";
        el.classList.remove("hidden");
        btn.classList.add("hidden");
      })
      .catch((e: Error) => {
        btn.disabled = false;
        err("page-err", e.message);
      });
  });
}
