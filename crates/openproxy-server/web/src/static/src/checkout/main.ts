// checkout/main.ts — entrypoint of the public checkout page. Wires the
// modules; each one owns exactly one responsibility:
//   tabs.ts    → Buy / My key switcher
//   catalog.ts → plan cards (price, credited balance, lifetime, access)
//   mykey.ts   → balance/history/top-up with the buyer's key
//   buy.ts     → anonymous order + key reveal
//   pay-step.ts → QR, countdown, on-chain poll (shared payment state)
//
// build.mjs compiles this to an IIFE at web/src/static/checkout.js, which
// the server embeds and serves at /checkout.js with `no-store`.

import { initTabs } from "./tabs.js";
import { loadCatalog } from "./catalog.js";
import { initMyKey } from "./mykey.js";
import { initBuy } from "./buy.js";

initTabs();
loadCatalog();
initMyKey();
initBuy();
