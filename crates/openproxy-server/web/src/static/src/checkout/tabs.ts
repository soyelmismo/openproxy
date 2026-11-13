// checkout/tabs.ts — Buy / My key tab switcher of the public checkout.
// Switching tabs hides EVERY step, not just the two tab panes: an in-flight
// QR/reveal panel must not stay on screen next to the other tab.

import { $ } from "./dom.js";

const STEPS = ["step-buy", "step-mykey", "step-pay", "step-key", "step-done"];

function tab(name: string): void {
  STEPS.forEach((id) => $(id).classList.add("hidden"));
  $(name).classList.remove("hidden");
  if (name === "step-buy") {
    $("tab-buy").className = "btn primary";
    $("tab-mykey").className = "btn btn-secondary";
  } else {
    $("tab-buy").className = "btn btn-secondary";
    $("tab-mykey").className = "btn primary";
  }
}

export function initTabs(): void {
  $("tab-buy").addEventListener("click", () => tab("step-buy"));
  $("tab-mykey").addEventListener("click", () => tab("step-mykey"));
}
