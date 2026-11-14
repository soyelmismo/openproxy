// checkout/mykey.ts — the "My key" tab: read the key's plan and buy/renew
// one for it, authenticated with the buyer's own key (Bearer).

import { $, err, hide, show } from "./dom.js";
import { getAuth, postJson, type CheckoutOrder, type MyPlanResponse, type RotationResponse } from "./api.js";
import { enterOrder } from "./pay-step.js";
import { parseSelection, selectedPlanValue } from "./catalog.js";
import { clearKey, endCreate, parseUtc, prepareCreate } from "./session.js";

function keyOf(): string {
  return ($<HTMLInputElement>("mykey")).value.trim();
}

function timeLeft(iso: string | null): string {
  if (!iso) return "no expiry";
  // SQLite UTC -> absolute instant (never a local-time guess).
  const left = parseUtc(iso) - Date.now();
  if (!Number.isFinite(left)) return "unknown expiry";
  if (left <= 0) return "expired";
  const d = Math.floor(left / 86_400_000);
  const h = Math.floor((left % 86_400_000) / 3_600_000);
  return d > 0 ? `expires in ${d}d ${h}h` : `expires in ${h}h`;
}

export function initMyKey(): void {
  $("mykey-load").addEventListener("click", function (this: HTMLButtonElement) {
    const btn = this;
    const key = keyOf();
    const errEl = $("mykey-err");
    errEl.classList.add("hidden");
    if (!key) return;
    btn.disabled = true;
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
          "Plan credit: USDC " + plan.credit_usd.toFixed(2) +
          " · balance left: USDC " + plan.balance_usd.toFixed(2);
        $("my-plan-expiry").textContent = timeLeft(plan.expires_at);
      })
      .catch((e: Error) => {
        errEl.textContent = e.message;
        errEl.classList.remove("hidden");
      })
      .finally(() => {
        btn.disabled = false;
      });
  });

  // Self-service rotation (POST /v1/credits/me/rotate): for a leaked key.
  // The old plaintext dies server-side the instant the new one is generated;
  // the id stays, so plan, balance and quotas all survive. The new plaintext
  // is shown once in this tab — there is no way to see it again.
  $("my-rotate").addEventListener("click", function (this: HTMLButtonElement) {
    const btn = this;
    const key = keyOf();
    err("mykey-err", "");
    if (
      !window.confirm(
        "Rotate this key? The current key stops working immediately and the " +
          "new key is shown only once. Do this only if the key leaked or was " +
          "shared.",
      )
    ) {
      return;
    }
    btn.disabled = true;
    postJson<RotationResponse>("v1/credits/me/rotate", {}, {
      Authorization: "Bearer " + key,
    })
      .then((res) => {
        btn.disabled = false;
        if (!res.ok) {
          throw new Error(res.body.error?.message || "rotation failed");
        }
        const el = $("my-rotated-key");
        el.textContent = res.body.key;
        el.classList.remove("hidden");
        btn.classList.add("hidden");
      })
      .catch((e: unknown) =>
        err("mykey-err", e instanceof Error ? e.message : String(e)),
      );
  });

  $("my-buy").addEventListener("click", function (this: HTMLButtonElement) {
    const btn = this;
    const key = keyOf();
    const errEl = $("mykey-err");
    errEl.classList.add("hidden");
    const selection = selectedPlanValue("my-usd");
    if (!selection) {
      err("mykey-err", "Pick a plan first.");
      return;
    }
    // Same terms precondition as the mint-new-key flow: a renewal buys
    // consumable software credit too.
    if (!$<HTMLInputElement>("terms-check-my").checked) {
      err("mykey-err", "You must accept the Terms of Service to buy.");
      return;
    }
    btn.disabled = true; // double click must not create two payable orders
    // A renewal is authenticated by the owner's Bearer key, but the same
    // receipt-before-POST rule applies: a lost response must retry the SAME
    // order (the server treats the header as idempotent), and a browser that
    // cannot persist it must not start a payment it cannot resume.
    const prep = prepareCreate();
    if (!prep.ok) {
      err("mykey-err", prep.message);
      btn.disabled = false;
      return;
    }
    const payload = parseSelection(selection);
    postJson<CheckoutOrder>("v1/credits/me/plan", payload, {
      Authorization: "Bearer " + key,
      "Idempotency-Key": prep.receipt,
    })
      .then((res) => {
        if (!res.ok) {
          if (res.status < 500) endCreate();
          throw new Error(res.body.error?.message || "plan purchase failed");
        }
        const entry = enterOrder(res.body);
        // A renewal never reveals; a stale delivered key must not shadow the
        // payment step after a refresh.
        clearKey();
        hide("step-mykey");
        if (entry.kind === "settled") return; // enterOrder painted the done step
        show("step-pay");
        if (entry.kind === "late") {
          // Expired: the price window closed, not the purchase. KEEP the
          // creation marker: the receipt is bound to this order and is the only
          // way to claim a key minted by a late confirmation. Rotating it would
          // lose that key and allow a second payment for the same intent.
          if (!entry.durable) {
            err(
              "page-err",
              "This browser could not save the checkout state (storage full or " +
                "restricted). Keep this tab open: a transfer sent before the " +
                "deadline can still be credited and apply this plan.",
            );
          }
          return;
        }
        // Persist FIRST, settle second: a memory-only snapshot that settled the
        // marker would leave a paid renewal unrecoverable after a reload.
        const durable = entry.durable;
        if (durable) {
          endCreate();
        } else {
          err(
            "page-err",
            "This browser could not save the checkout state (storage full or " +
              "restricted). Keep this tab open until the payment confirms; " +
              "after a reload, pick the same plan and press Buy to resume it.",
          );
        }
      })
      .catch((e: Error) => {
        errEl.textContent = e.message;
        errEl.classList.remove("hidden");
      })
      .finally(() => {
        btn.disabled = false;
      });
  });
}
