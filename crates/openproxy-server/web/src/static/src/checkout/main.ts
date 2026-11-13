// checkout/main.ts — entrypoint of the public checkout page. Wires the
// modules; each one owns exactly one responsibility:
//   tabs.ts    → Buy / My key switcher
//   catalog.ts → plan cards (price, credited balance, lifetime, access)
//   mykey.ts   → read the key's plan, buy/renew one for it
//   buy.ts     → anonymous order + key reveal
//   session.ts → sessionStorage persistence (order / receipt / key)
//   pay-step.ts → QR, countdown, on-chain poll (shared payment state)
//
// build.mjs compiles this to an IIFE at web/src/static/checkout.js, which
// the server embeds and serves at /checkout.js with `no-store`.

import { initTabs } from "./tabs.js";
import { loadCatalog } from "./catalog.js";
import { initMyKey } from "./mykey.js";
import { initBuy } from "./buy.js";
import { err, hide, show } from "./dom.js";
import { loadKey, loadOrder, pendingReceipt } from "./session.js";
import { enterOrder, showKey } from "./pay-step.js";

/** A refresh must resume where the buyer was, not restart at step 1. */
function restore(): boolean {
  const key = loadKey();
  if (key) {
    hide("step-buy");
    showKey(key);
    return true;
  }
  const order = loadOrder();
  if (!order) {
    // A creation whose response never arrived: the receipt (and the order it
    // already created server-side) is still pending. Re-selecting the plan and
    // pressing Buy reuses the SAME receipt, so the retry resumes that order
    // instead of opening a second payable amount.
    if (pendingReceipt()) {
      err(
        "buy-err",
        "Your previous order attempt was interrupted. Pick the same plan and " +
          "press Buy again to resume it — no second payment will be required.",
      );
    }
    return false;
  }
  hide("step-buy");
  // `enterOrder` owns the whole lifecycle, including the expired case: the
  // price window closing is NOT the matcher giving up, so a restored expired
  // order keeps its snapshot + receipt, hides the payment instructions and
  // resumes the slow watch instead of asking for a second purchase.
  const entry = enterOrder(order);
  if (entry.kind !== "settled") show("step-pay");
  return true;
}

initTabs();
loadCatalog();
initMyKey();
initBuy();
restore();
