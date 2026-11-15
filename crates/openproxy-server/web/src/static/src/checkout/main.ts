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
import { initCheckoutModals } from "./modals.js";
import { err, hide, show } from "./dom.js";
import { loadKey, loadOrder, pendingReceipt, seedReceipt } from "./session.js";
import { enterOrder, showKey } from "./pay-step.js";

/**
 * A recovery link (?receipt=…) seeds the receipt of a purchase created on
 * another device/tab: the buyer picks the plan and presses Buy, and the POST
 * reuses THIS receipt — the server replays the original order (any state)
 * instead of opening a second payable amount. A local snapshot always wins:
 * its own receipt is the capability for ITS reveal and must not be overwritten
 * by a pasted link.
 */
function seedFromRecoveryLink(): void {
  // ROOT FIX (audit A5-01): read the receipt from the URL FRAGMENT. The
  // fragment never reaches the server, proxies or Referer — unlike a query
  // string, which used to deposit the full reveal capability in access
  // logs and browser history.
  const m = /[#&]receipt=([^&]+)/.exec(location.hash);
  const receipt = m ? decodeURIComponent(m[1]) : null;
  if (!receipt) return;
  // Strip it from the address bar: a refresh must not keep re-seeding after
  // the buyer moved on (the live order keeps painting its own recovery link).
  history.replaceState(null, "", location.pathname);
  if (loadKey() || loadOrder()) return;
  if (!seedReceipt(receipt)) {
    err(
      "buy-err",
      "This browser could not store the recovery receipt (private mode or " +
        "storage disabled). Enable site storage and reopen the link.",
    );
    return;
  }
  err(
    "buy-err",
    "Recovery link detected. Pick the plan you were buying and press Buy — " +
      "the original order is restored, no second payment is required.",
  );
}

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
initCheckoutModals();
seedFromRecoveryLink();
restore();
