// checkout/mykey.ts — the "My key" tab: read the key's plan and buy/renew
// one for it, authenticated with the buyer's own key (Bearer, `chat` scope).

import { $, err, hide, show } from "./dom.js";
import { getAuth, postJson, type CheckoutOrder, type MyPlanResponse } from "./api.js";
import { renderPayStep, startCountdown, startPolling } from "./pay-step.js";
import { parseSelection, selectedPlanValue } from "./catalog.js";

function keyOf(): string {
  return ($<HTMLInputElement>("mykey")).value.trim();
}

function timeLeft(iso: string | null): string {
  if (!iso) return "no expiry";
  const left = new Date(iso).getTime() - Date.now();
  if (left <= 0) return "expired";
  const d = Math.floor(left / 86_400_000);
  const h = Math.floor((left % 86_400_000) / 3_600_000);
  return d > 0 ? `expires in ${d}d ${h}h` : `expires in ${h}h`;
}

export function initMyKey(): void {
  $("mykey-load").addEventListener("click", () => {
    const key = keyOf();
    const errEl = $("mykey-err");
    errEl.classList.add("hidden");
    if (!key) return;
    getAuth<MyPlanResponse>("v1/credits/me/plan", key)
      .then((res) => {
        $("mykey-data").classList.remove("hidden");
        const plan = res.plan;
        if (!plan) {
          $("my-plan-name").textContent = "No plan on this key";
          $("my-plan-credit").textContent = "Pick a plan below to activate it.";
          $("my-plan-expiry").textContent = "";
          return;
        }
        $("my-plan-name").textContent = plan.name;
        $("my-plan-credit").textContent =
          "USDC " + plan.balance_usd.toFixed(2) + " of " +
          plan.credit_usd.toFixed(2) + " credit left";
        $("my-plan-expiry").textContent = timeLeft(plan.expires_at);
      })
      .catch((e: Error) => {
        errEl.textContent = e.message;
        errEl.classList.remove("hidden");
      });
  });

  $("my-buy").addEventListener("click", () => {
    const key = keyOf();
    const errEl = $("mykey-err");
    errEl.classList.add("hidden");
    const selection = selectedPlanValue("my-usd");
    if (!selection) {
      err("mykey-err", "Pick a plan first.");
      return;
    }
    const payload = parseSelection(selection);
    postJson<CheckoutOrder>("v1/credits/me/plan", payload, {
      Authorization: "Bearer " + key,
    })
      .then((res) => {
        if (!res.ok) throw new Error(res.body.error?.message || "plan purchase failed");
        hide("step-mykey");
        show("step-pay");
        renderPayStep(res.body);
        startCountdown();
        startPolling();
      })
      .catch((e: Error) => {
        errEl.textContent = e.message;
        errEl.classList.remove("hidden");
      });
  });
}
