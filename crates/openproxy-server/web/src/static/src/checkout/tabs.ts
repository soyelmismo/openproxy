// checkout/tabs.ts — Buy / My key tab switcher of the public checkout.

import { $ } from "./dom.js";

function tab(name: string): void {
  ["step-buy", "step-mykey"].forEach((id) => $(id).classList.add("hidden"));
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
